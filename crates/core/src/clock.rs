use std::fmt::Debug;
use std::sync::{Arc, Mutex, MutexGuard};

use chrono::{DateTime, TimeDelta, Utc};

pub trait Clock: Debug + Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

#[derive(Debug, Clone)]
pub struct ManualClock(Arc<Mutex<DateTime<Utc>>>);

impl ManualClock {
    pub fn new(now: DateTime<Utc>) -> Self {
        Self(Arc::new(Mutex::new(now)))
    }

    pub fn set(&self, now: DateTime<Utc>) {
        *self.lock() = now;
    }

    pub fn advance(&self, by: TimeDelta) {
        *self.lock() += by;
    }

    fn lock(&self) -> MutexGuard<'_, DateTime<Utc>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self.lock()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_clock_moves_only_when_told() {
        let start = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let clock = ManualClock::new(start);
        assert_eq!(clock.now(), start);
        clock.advance(TimeDelta::hours(2));
        assert_eq!(clock.now(), start + TimeDelta::hours(2));
        clock.set(start);
        assert_eq!(clock.now(), start);
    }
}
