//! Exponential backoff with full jitter for transient failures (5xx and
//! network/disconnect errors). 429s are *not* retried here; they go through
//! the limiter cooldown instead.
//!
//! `delay(attempt) = uniform[0, min(cap, base * 2^attempt))`, where `attempt`
//! is the zero-based retry index (0 = delay before the first retry).

use std::future::Future;
use std::time::Duration;

use cs2_arb_core::{Error, Result};
use rand::RngCore;

use crate::clock::Clock;

pub const DEFAULT_BASE: Duration = Duration::from_secs(1);
pub const DEFAULT_CAP: Duration = Duration::from_secs(300);
pub const DEFAULT_MAX_ATTEMPTS: u32 = 5;

/// Backoff parameters. `max_attempts` counts every attempt, including the first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    base: Duration,
    cap: Duration,
    max_attempts: u32,
}

impl Default for RetryPolicy {
    /// 1 s base, 5 min cap, 5 attempts.
    fn default() -> Self {
        Self {
            base: DEFAULT_BASE,
            cap: DEFAULT_CAP,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
        }
    }
}

impl RetryPolicy {
    pub fn new(base: Duration, cap: Duration, max_attempts: u32) -> Result<Self> {
        if base.is_zero() || cap < base {
            return Err(Error::InvalidConfig(
                "retry base must be non-zero and no larger than the cap".into(),
            ));
        }
        if max_attempts == 0 {
            return Err(Error::InvalidConfig(
                "retry max_attempts must be at least 1".into(),
            ));
        }
        Ok(Self {
            base,
            cap,
            max_attempts,
        })
    }

    pub fn with_max_attempts(self, max_attempts: u32) -> Result<Self> {
        Self::new(self.base, self.cap, max_attempts)
    }

    pub fn base(&self) -> Duration {
        self.base
    }

    pub fn cap(&self) -> Duration {
        self.cap
    }

    pub fn max_attempts(&self) -> u32 {
        self.max_attempts
    }

    /// Deterministic upper bound `min(cap, base * 2^attempt)`.
    pub fn ceiling(&self, attempt: u32) -> Duration {
        backoff_ceiling(self.base, self.cap, attempt)
    }

    /// Full-jitter delay for retry index `attempt`.
    pub fn delay<R: RngCore + ?Sized>(&self, attempt: u32, rng: &mut R) -> Duration {
        full_jitter(self.ceiling(attempt), rng)
    }
}

/// `min(cap, base * 2^attempt)`, saturating (never overflows, never exceeds `cap`).
pub fn backoff_ceiling(base: Duration, cap: Duration, attempt: u32) -> Duration {
    let factor = 1u32.checked_shl(attempt).unwrap_or(0);
    let grown = if factor == 0 {
        cap
    } else {
        base.checked_mul(factor).unwrap_or(cap)
    };
    grown.min(cap)
}

/// Uniform sample in `[0, ceiling)` (exactly 0 when `ceiling` is 0) using 53
/// random bits, computed with integer arithmetic so results are reproducible
/// for a given RNG output.
pub fn full_jitter<R: RngCore + ?Sized>(ceiling: Duration, rng: &mut R) -> Duration {
    const BITS: u32 = 53;
    let unit = u128::from(rng.next_u64() >> (64 - BITS));
    let nanos = (ceiling.as_nanos() * unit) >> BITS;
    Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

/// Default-policy full-jitter delay using the thread RNG. Prefer
/// [`RetryPolicy::delay`] with an injected RNG where determinism matters.
pub fn retry_delay(attempt: u32) -> Duration {
    RetryPolicy::default().delay(attempt, &mut rand::rng())
}

/// Classification returned by an operation passed to [`retry_with_backoff`].
#[derive(Debug)]
pub enum RetryError<E> {
    /// Retry with backoff. If `retry_after` is set (e.g. a 503 `Retry-After`),
    /// the wait is at least `min(retry_after, cap)`.
    Transient {
        error: E,
        retry_after: Option<Duration>,
    },
    /// Do not retry.
    Permanent(E),
}

impl<E> RetryError<E> {
    pub fn transient(error: E) -> Self {
        Self::Transient {
            error,
            retry_after: None,
        }
    }

    pub fn into_inner(self) -> E {
        match self {
            Self::Transient { error, .. } | Self::Permanent(error) => error,
        }
    }
}

/// Run `op` until it succeeds, fails permanently, or `max_attempts` is used up,
/// sleeping on `clock` between transient failures. `op` receives the zero-based
/// attempt number. Returns the last error on exhaustion.
pub async fn retry_with_backoff<T, E, Op, Fut, R>(
    policy: &RetryPolicy,
    clock: &dyn Clock,
    rng: &mut R,
    mut op: Op,
) -> std::result::Result<T, E>
where
    Op: FnMut(u32) -> Fut,
    Fut: Future<Output = std::result::Result<T, RetryError<E>>>,
    R: RngCore + ?Sized,
{
    let mut attempt = 0;
    loop {
        match op(attempt).await {
            Ok(value) => return Ok(value),
            Err(RetryError::Permanent(error)) => return Err(error),
            Err(RetryError::Transient { error, retry_after }) => {
                if attempt + 1 >= policy.max_attempts {
                    return Err(error);
                }
                let mut delay = policy.delay(attempt, rng);
                if let Some(hint) = retry_after {
                    delay = delay.max(hint.min(policy.cap));
                }
                tracing::debug!(
                    attempt = attempt + 1,
                    max_attempts = policy.max_attempts,
                    delay_ms = delay.as_millis() as u64,
                    "transient failure; backing off"
                );
                clock.sleep(delay).await;
                attempt += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use rand::rngs::StdRng;
    use rand::SeedableRng;
    use tokio::time::Instant;

    use super::*;

    /// RNG that always returns the same word (for exact extreme cases).
    struct Fixed(u64);

    impl RngCore for Fixed {
        fn next_u32(&mut self) -> u32 {
            self.0 as u32
        }
        fn next_u64(&mut self) -> u64 {
            self.0
        }
        fn fill_bytes(&mut self, dst: &mut [u8]) {
            dst.fill(self.0 as u8);
        }
    }

    #[test]
    fn ceiling_grows_exponentially_then_caps() {
        let policy = RetryPolicy::default();
        let expected = [1, 2, 4, 8, 16, 32, 64, 128, 256, 300, 300];
        for (attempt, secs) in expected.into_iter().enumerate() {
            assert_eq!(
                policy.ceiling(attempt as u32),
                Duration::from_secs(secs),
                "attempt {attempt}"
            );
        }
        for attempt in [31, 32, 33, 64, 1000, u32::MAX] {
            assert_eq!(policy.ceiling(attempt), DEFAULT_CAP);
        }
        // Huge base * factor overflow saturates to the cap too.
        let wide = Duration::from_secs(u64::MAX / 2);
        assert_eq!(backoff_ceiling(wide, Duration::MAX, 4), Duration::MAX);
    }

    #[test]
    fn delays_never_exceed_cap_and_stay_in_jitter_range() {
        let policy = RetryPolicy::default();
        let mut rng = StdRng::seed_from_u64(7);
        for attempt in 0..40 {
            let ceiling = policy.ceiling(attempt);
            let mut max_seen = Duration::ZERO;
            let mut min_seen = Duration::MAX;
            for _ in 0..2_000 {
                let delay = policy.delay(attempt, &mut rng);
                assert!(delay < ceiling, "{delay:?} >= {ceiling:?}");
                assert!(delay <= DEFAULT_CAP);
                max_seen = max_seen.max(delay);
                min_seen = min_seen.min(delay);
            }
            // Full jitter spans (nearly) the whole [0, ceiling) range.
            assert!(max_seen > ceiling.mul_f64(0.95), "attempt {attempt}");
            assert!(min_seen < ceiling.mul_f64(0.05), "attempt {attempt}");
        }
    }

    #[test]
    fn jitter_extremes_are_exact() {
        let ceiling = Duration::from_secs(8);
        assert_eq!(full_jitter(ceiling, &mut Fixed(0)), Duration::ZERO);
        let top = full_jitter(ceiling, &mut Fixed(u64::MAX));
        assert!(top < ceiling && ceiling - top < Duration::from_nanos(2));
        let half = full_jitter(ceiling, &mut Fixed(1 << 63));
        assert_eq!(half, Duration::from_secs(4));
        assert_eq!(
            full_jitter(Duration::ZERO, &mut Fixed(u64::MAX)),
            Duration::ZERO
        );
    }

    #[test]
    fn mean_delay_grows_exponentially() {
        let policy = RetryPolicy::default();
        let mut rng = StdRng::seed_from_u64(42);
        let mean = |attempt: u32, rng: &mut StdRng| {
            let total: f64 = (0..4_000)
                .map(|_| policy.delay(attempt, rng).as_secs_f64())
                .sum();
            total / 4_000.0
        };
        let means: Vec<f64> = (0..8).map(|a| mean(a, &mut rng)).collect();
        for pair in means.windows(2) {
            let ratio = pair[1] / pair[0];
            assert!((1.8..2.2).contains(&ratio), "ratio {ratio} in {means:?}");
        }
    }

    #[test]
    fn same_seed_same_delays() {
        let policy = RetryPolicy::default();
        let a: Vec<_> = {
            let mut rng = StdRng::seed_from_u64(1);
            (0..10).map(|i| policy.delay(i, &mut rng)).collect()
        };
        let b: Vec<_> = {
            let mut rng = StdRng::seed_from_u64(1);
            (0..10).map(|i| policy.delay(i, &mut rng)).collect()
        };
        assert_eq!(a, b);
        assert!(retry_delay(3) < Duration::from_secs(8));
    }

    #[test]
    fn rejects_invalid_policy() {
        let s = Duration::from_secs;
        assert!(RetryPolicy::new(Duration::ZERO, s(1), 1).is_err());
        assert!(RetryPolicy::new(s(2), s(1), 1).is_err());
        assert!(RetryPolicy::new(s(1), s(1), 0).is_err());
        assert!(RetryPolicy::new(s(1), s(300), 1).is_ok());
    }

    /// Virtual clock that records requested sleeps and advances instantly.
    struct RecordingClock {
        start: Instant,
        elapsed: Mutex<Duration>,
        sleeps: Mutex<Vec<Duration>>,
    }

    #[async_trait]
    impl Clock for RecordingClock {
        fn now(&self) -> Instant {
            self.start + *self.elapsed.lock().unwrap()
        }
        fn utc_now(&self) -> DateTime<Utc> {
            DateTime::UNIX_EPOCH
        }
        async fn sleep_until(&self, deadline: Instant) {
            let now = self.now();
            let step = deadline.saturating_duration_since(now);
            self.sleeps.lock().unwrap().push(step);
            *self.elapsed.lock().unwrap() += step;
        }
    }

    fn recording_clock() -> Arc<RecordingClock> {
        Arc::new(RecordingClock {
            start: Instant::now(),
            elapsed: Mutex::new(Duration::ZERO),
            sleeps: Mutex::new(Vec::new()),
        })
    }

    #[tokio::test]
    async fn retry_helper_sleeps_jittered_delays_until_success() {
        let clock = recording_clock();
        let policy = RetryPolicy::default();
        // Fixed(1<<63) => every delay is exactly half its ceiling.
        let mut rng = Fixed(1 << 63);
        let mut calls = Vec::new();
        let out: std::result::Result<&str, &str> =
            retry_with_backoff(&policy, &*clock, &mut rng, |attempt| {
                calls.push(attempt);
                async move {
                    if attempt < 3 {
                        Err(RetryError::transient("boom"))
                    } else {
                        Ok("ok")
                    }
                }
            })
            .await;
        assert_eq!(out, Ok("ok"));
        assert_eq!(calls, [0, 1, 2, 3]);
        let ms = Duration::from_millis;
        assert_eq!(*clock.sleeps.lock().unwrap(), [ms(500), ms(1000), ms(2000)]);
    }

    #[tokio::test]
    async fn retry_helper_stops_at_max_attempts_and_on_permanent() {
        let clock = recording_clock();
        let policy = RetryPolicy::default().with_max_attempts(3).unwrap();
        let mut rng = StdRng::seed_from_u64(3);
        let mut calls = 0;
        let out: std::result::Result<(), u32> =
            retry_with_backoff(&policy, &*clock, &mut rng, |attempt| {
                calls += 1;
                async move { Err(RetryError::transient(attempt)) }
            })
            .await;
        assert_eq!(out, Err(2));
        assert_eq!(calls, 3);
        assert_eq!(clock.sleeps.lock().unwrap().len(), 2);

        let mut calls = 0;
        let out: std::result::Result<(), &str> =
            retry_with_backoff(&policy, &*clock, &mut rng, |_| {
                calls += 1;
                async { Err(RetryError::Permanent("fatal")) }
            })
            .await;
        assert_eq!(out, Err("fatal"));
        assert_eq!(calls, 1);
    }

    #[tokio::test]
    async fn retry_after_hint_raises_delay_but_respects_cap() {
        let clock = recording_clock();
        let policy = RetryPolicy::default().with_max_attempts(3).unwrap();
        let mut rng = Fixed(0); // jitter alone would be 0
        let hints = [Duration::from_secs(20), Duration::from_secs(10_000)];
        let _: std::result::Result<(), ()> =
            retry_with_backoff(&policy, &*clock, &mut rng, |attempt| {
                let hint = hints.get(attempt as usize).copied();
                async move {
                    Err(RetryError::Transient {
                        error: (),
                        retry_after: hint,
                    })
                }
            })
            .await;
        assert_eq!(
            *clock.sleeps.lock().unwrap(),
            [Duration::from_secs(20), DEFAULT_CAP]
        );
    }
}
