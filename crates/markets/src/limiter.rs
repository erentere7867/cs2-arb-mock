//! Local sliding-window limiter (used for Skinport's documented 8 requests / 5 minutes per
//! endpoint). Uses `tokio::time::Instant`, so it follows paused tokio time in tests.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use cs2_arb_core::{Error, Result};
use tokio::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowLimit {
    pub max_requests: u32,
    pub window: Duration,
}

impl WindowLimit {
    /// Skinport `/v1/items` and `/v1/sales/history`: 8 requests per 5 minutes.
    pub const SKINPORT: Self = Self {
        max_requests: 8,
        window: Duration::from_secs(300),
    };
}

/// What to do when the local budget is exhausted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LimitPolicy {
    /// Fail fast with `Error::RateLimited { retry_after }`.
    #[default]
    Reject,
    /// Sleep until a slot frees up.
    Wait,
}

#[derive(Debug)]
struct State {
    sent: VecDeque<Instant>,
    blocked_until: Option<Instant>,
}

#[derive(Debug)]
pub struct SlidingWindowLimiter {
    limit: WindowLimit,
    state: Mutex<State>,
}

impl SlidingWindowLimiter {
    pub fn new(limit: WindowLimit) -> Result<Self> {
        if limit.max_requests == 0 || limit.window.is_zero() {
            return Err(Error::InvalidConfig(
                "rate limit needs max_requests > 0 and a nonzero window".into(),
            ));
        }
        Ok(Self {
            limit,
            state: Mutex::new(State {
                sent: VecDeque::new(),
                blocked_until: None,
            }),
        })
    }

    pub fn limit(&self) -> WindowLimit {
        self.limit
    }

    /// Takes a slot now, or returns how long until one is available.
    pub fn try_acquire(&self) -> std::result::Result<(), Duration> {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(until) = state.blocked_until {
            if now < until {
                return Err(until - now);
            }
            state.blocked_until = None;
        }
        while let Some(&oldest) = state.sent.front() {
            if now.duration_since(oldest) >= self.limit.window {
                state.sent.pop_front();
            } else {
                break;
            }
        }
        if state.sent.len() < self.limit.max_requests as usize {
            state.sent.push_back(now);
            return Ok(());
        }
        let oldest = *state.sent.front().expect("non-empty when full");
        Err((oldest + self.limit.window) - now)
    }

    pub async fn acquire(&self, policy: LimitPolicy) -> Result<()> {
        loop {
            match self.try_acquire() {
                Ok(()) => return Ok(()),
                Err(wait) => match policy {
                    LimitPolicy::Reject => return Err(Error::RateLimited { retry_after: wait }),
                    LimitPolicy::Wait => tokio::time::sleep(wait).await,
                },
            }
        }
    }

    /// Blocks all requests for `penalty` (e.g. after a server 429).
    pub fn penalize(&self, penalty: Duration) {
        let until = Instant::now() + penalty;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.blocked_until = Some(state.blocked_until.map_or(until, |u| u.max(until)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn enforces_window_and_penalty() {
        let limiter = SlidingWindowLimiter::new(WindowLimit::SKINPORT).unwrap();
        for _ in 0..8 {
            limiter.try_acquire().unwrap();
        }
        assert_eq!(limiter.try_acquire(), Err(Duration::from_secs(300)));
        tokio::time::advance(Duration::from_secs(100)).await;
        assert_eq!(limiter.try_acquire(), Err(Duration::from_secs(200)));
        tokio::time::advance(Duration::from_secs(200)).await;
        limiter.try_acquire().unwrap();

        limiter.penalize(Duration::from_secs(42));
        assert_eq!(limiter.try_acquire(), Err(Duration::from_secs(42)));

        let start = Instant::now();
        limiter.acquire(LimitPolicy::Wait).await.unwrap();
        assert!(start.elapsed() >= Duration::from_secs(42));
        assert!(SlidingWindowLimiter::new(WindowLimit {
            max_requests: 0,
            window: Duration::from_secs(1)
        })
        .is_err());
    }
}
