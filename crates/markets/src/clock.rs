//! Injectable wall clock, used for `observed_at`, `since` filtering, DMarket `X-Sign-Date`
//! and on-disk cache freshness. In-memory TTLs and rate-limit windows use
//! `tokio::time::Instant`, so they follow paused tokio time in tests.

use std::sync::Mutex;

use chrono::{DateTime, Duration as ChronoDuration, Utc};

pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// Manually driven clock for tests and replay.
#[derive(Debug)]
pub struct ManualClock {
    now: Mutex<DateTime<Utc>>,
}

impl ManualClock {
    pub fn new(now: DateTime<Utc>) -> Self {
        Self {
            now: Mutex::new(now),
        }
    }

    pub fn set(&self, now: DateTime<Utc>) {
        *self.now.lock().unwrap_or_else(|e| e.into_inner()) = now;
    }

    pub fn advance(&self, by: ChronoDuration) {
        let mut guard = self.now.lock().unwrap_or_else(|e| e.into_inner());
        *guard += by;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self.now.lock().unwrap_or_else(|e| e.into_inner())
    }
}
