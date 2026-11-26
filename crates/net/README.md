# Workstream B — Network resilience

Owns per-endpoint token buckets, complete request suppression during 429
cooldowns, and exponential retry backoff with jitter (1 s base, 5 min cap)
for 5xx and disconnects. Includes deterministic timing tests.

## Modules

| Module | Contents |
| --- | --- |
| `limiter` | `RateLimit`, pure `TokenBucket` state machine, async `EndpointLimiter`, `RateLimiter` registry keyed by `(Platform, endpoint)` |
| `backoff` | `RetryPolicy`, `backoff_ceiling`, `full_jitter`, `retry_with_backoff` (injected `Clock` + `RngCore`) |
| `executor` | `HttpExecutor`: GET/HEAD-only reqwest (rustls) wrapper applying limiter, cooldown and backoff |
| `config` | `NetConfig::from_toml` for `[rate_limits]` / `[retry]`, `build_limiter` |
| `retry_after` | `Retry-After` parsing (delay-seconds or HTTP-date) |
| `clock` | `Clock` trait, `TokioClock` (works with paused tokio time) |

## Semantics

* **Token bucket**: each spent token returns exactly `window (+ safety margin)`
  after it was spent, so at most `capacity` requests are admitted in *any*
  `window`-long interval (strict sliding-window variant).
* **429 cooldown**: `Normal -> Cooldown { until } -> Normal`;
  `until = now + max(window, Retry-After) + safety margin`. No token is issued
  during cooldown; `acquire` waits, `try_acquire` / `CooldownBehavior::FailFast`
  return `Error::RateLimited`. Later 429s only extend the cooldown.
* **Backoff**: full jitter, `uniform[0, min(cap, base * 2^attempt))`; 5xx and
  network/body-read errors are retried up to `max_attempts` (total attempts,
  default 5). A `Retry-After` on a 5xx acts as a floor (capped at `cap`).
  Every retry re-acquires a token. 429s are never auto-retried.
* **Read-only**: `HttpExecutor` rejects every method except GET and HEAD.
* **Fail closed**: unknown endpoints (no exact entry and no platform
  `default`) are rejected before anything is sent.
* Error strings use `reqwest::Error::without_url()` so query-string
  credentials never leak into errors or logs.

## Verification still needed

Whether Skinport's `/v1/items` and `/v1/sales/history` share one quota is
unverified. Set `quota_group = "<name>"` on both entries to make them share a
bucket and cooldown.
