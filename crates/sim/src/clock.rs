//! Injected clocks. The executor never reads wall-clock time unless it is given
//! a [`SystemClock`]; tests and backtests use [`ManualClock`].

use std::fmt;
use std::sync::Mutex;

use chrono::{DateTime, TimeDelta, Utc};

/// Source of "now" for the simulator.
pub trait Clock: Send + Sync + fmt::Debug {
    fn now(&self) -> DateTime<Utc>;
}

/// Wall-clock time. Only for interactive mock runs; never for deterministic tests.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// A clock that only moves when told to. Shareable through `Arc`.
#[derive(Debug)]
pub struct ManualClock {
    now: Mutex<DateTime<Utc>>,
}

impl ManualClock {
    pub fn new(start: DateTime<Utc>) -> Self {
        Self {
            now: Mutex::new(start),
        }
    }

    pub fn set(&self, now: DateTime<Utc>) {
        *self.lock() = now;
    }

    pub fn advance(&self, by: TimeDelta) {
        let mut guard = self.lock();
        *guard += by;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, DateTime<Utc>> {
        // A poisoned clock still holds a valid timestamp.
        self.now
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
