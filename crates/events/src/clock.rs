//! Injectable wall clock so detection, window timing and queue leases are testable.

use std::fmt;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, TimeDelta, Utc};

/// Source of wall-clock time. Queue leases, delayed delivery and window
/// timestamps all read time through this trait.
pub trait Clock: Send + Sync + fmt::Debug {
    fn now(&self) -> DateTime<Utc>;
}

/// The real system clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// A manually advanced clock for tests and deterministic replays. Clones share
/// the same underlying instant.
#[derive(Debug, Clone)]
pub struct ManualClock {
    now: Arc<Mutex<DateTime<Utc>>>,
}

impl ManualClock {
    pub fn new(start: DateTime<Utc>) -> Self {
        Self {
            now: Arc::new(Mutex::new(start)),
        }
    }

    pub fn set(&self, instant: DateTime<Utc>) {
        *self.lock() = instant;
    }

    pub fn advance(&self, by: TimeDelta) {
        let mut guard = self.lock();
        *guard += by;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, DateTime<Utc>> {
        // A poisoned clock still holds a valid instant.
        self.now.lock().unwrap_or_else(|poison| poison.into_inner())
    }
}

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self.lock()
    }
}
