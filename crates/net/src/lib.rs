//! Endpoint admission control and retry policies for read-only market requests.
//!
//! * [`limiter`]: per-`(Platform, endpoint)` token buckets with a 429 cooldown
//!   state machine (`Normal -> Cooldown { until } -> Normal`).
//! * [`backoff`]: exponential backoff with full jitter (1 s base, 5 min cap)
//!   plus a generic retry helper with injected clock and RNG.
//! * [`executor`]: [`HttpExecutor`], a GET/HEAD-only reqwest wrapper applying
//!   all of the above.
//!
//! Nothing in this crate can place orders: non-GET/HEAD requests are rejected.

pub mod backoff;
pub mod clock;
pub mod config;
pub mod executor;
pub mod limiter;
pub mod retry_after;

pub use backoff::{
    backoff_ceiling, full_jitter, retry_delay, retry_with_backoff, RetryError, RetryPolicy,
};
pub use clock::{Clock, TokioClock};
pub use config::{EndpointLimitConfig, NetConfig};
pub use executor::{default_client, CooldownBehavior, HttpExecutor};
pub use limiter::{
    Admission, BlockReason, CooldownState, EndpointKey, EndpointLimiter, RateLimit, RateLimiter,
    ResponseClass, TokenBucket, DEFAULT_ENDPOINT,
};
pub use retry_after::{parse_retry_after, retry_after_from_headers};
