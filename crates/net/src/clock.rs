//! Injectable time source.
//!
//! All rate-limit, cooldown and backoff timing goes through [`Clock`]. The
//! default [`TokioClock`] uses `tokio::time`, so tests can drive it
//! deterministically with `#[tokio::test(start_paused = true)]`, and tests that
//! want to observe sleeps can inject their own implementation.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::time::Instant;

#[async_trait]
pub trait Clock: Send + Sync + 'static {
    /// Monotonic "now" used for every limiter/cooldown decision.
    fn now(&self) -> Instant;

    /// Wall-clock time; only used to interpret HTTP-date `Retry-After` values.
    fn utc_now(&self) -> DateTime<Utc>;

    /// Resolve no earlier than `deadline`.
    async fn sleep_until(&self, deadline: Instant);

    async fn sleep(&self, duration: Duration) {
        let deadline = self.now() + duration;
        self.sleep_until(deadline).await;
    }
}

/// `tokio::time` backed clock (honours paused/auto-advanced test time).
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioClock;

#[async_trait]
impl Clock for TokioClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn utc_now(&self) -> DateTime<Utc> {
        Utc::now()
    }

    async fn sleep_until(&self, deadline: Instant) {
        tokio::time::sleep_until(deadline).await;
    }
}
