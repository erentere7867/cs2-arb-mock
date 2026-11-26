//! Read-only HTTP executor that applies the endpoint limiter, 429 cooldown and
//! transient-failure backoff around reqwest (rustls).
//!
//! Only `GET` and `HEAD` requests are accepted: this crate can never be used
//! to place orders or mutate marketplace state.

use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cs2_arb_core::{Error, Platform, Result};
use rand::rngs::StdRng;
use rand::{RngCore, SeedableRng};
use reqwest::{IntoUrl, Method, Request, RequestBuilder, Response, StatusCode};
use serde::de::DeserializeOwned;

use crate::backoff::{retry_with_backoff, RetryError, RetryPolicy};
use crate::limiter::{CooldownState, RateLimiter, ResponseClass};
use crate::retry_after::retry_after_from_headers;

pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const USER_AGENT: &str = concat!("cs2-arb-mock/", env!("CARGO_PKG_VERSION"), " (read-only)");

/// What `execute` does when the endpoint is in a 429 cooldown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CooldownBehavior {
    /// Block until the cooldown ends, then send (default).
    #[default]
    Wait,
    /// Return [`Error::RateLimited`] immediately without sending.
    FailFast,
}

type BoxRng = Box<dyn RngCore + Send>;

/// RNG adapter so the shared, mutex-guarded RNG can be lent to the retry loop
/// without holding the lock across an await point.
struct SharedRng<'a>(&'a Mutex<BoxRng>);

impl SharedRng<'_> {
    fn with<T>(&mut self, f: impl FnOnce(&mut BoxRng) -> T) -> T {
        let mut guard = self.0.lock().unwrap_or_else(|p| p.into_inner());
        f(&mut guard)
    }
}

impl RngCore for SharedRng<'_> {
    fn next_u32(&mut self) -> u32 {
        self.with(|rng| rng.next_u32())
    }
    fn next_u64(&mut self) -> u64 {
        self.with(|rng| rng.next_u64())
    }
    fn fill_bytes(&mut self, dst: &mut [u8]) {
        self.with(|rng| rng.fill_bytes(dst))
    }
}

/// Per-platform HTTP front door. Typically one per marketplace client, all
/// sharing one `Arc<RateLimiter>`.
pub struct HttpExecutor {
    platform: Platform,
    client: reqwest::Client,
    limiter: Arc<RateLimiter>,
    retry: RetryPolicy,
    rng: Mutex<BoxRng>,
    cooldown: CooldownBehavior,
}

impl fmt::Debug for HttpExecutor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Deliberately omits the client (it may carry default auth headers).
        f.debug_struct("HttpExecutor")
            .field("platform", &self.platform)
            .field("retry", &self.retry)
            .field("cooldown", &self.cooldown)
            .field("limiter", &self.limiter)
            .finish_non_exhaustive()
    }
}

/// Default production client: rustls, HTTPS only, connect/request timeouts.
pub fn default_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .use_rustls_tls()
        .https_only(true)
        .connect_timeout(DEFAULT_CONNECT_TIMEOUT)
        .timeout(DEFAULT_REQUEST_TIMEOUT)
        .user_agent(USER_AGENT)
        .build()
        .map_err(|err| Error::Network(redact(err)))
}

fn redact(err: reqwest::Error) -> String {
    // URLs may carry credentials in query strings; never surface them.
    err.without_url().to_string()
}

fn ensure_read_only(method: &Method) -> Result<()> {
    if *method == Method::GET || *method == Method::HEAD {
        Ok(())
    } else {
        Err(Error::InvalidConfig(format!(
            "HttpExecutor is read-only (mock simulation mode); refusing {method} request"
        )))
    }
}

impl HttpExecutor {
    /// Executor with [`default_client`] and an OS-seeded RNG.
    pub fn new(platform: Platform, limiter: Arc<RateLimiter>, retry: RetryPolicy) -> Result<Self> {
        Ok(Self::with_client(
            platform,
            limiter,
            retry,
            default_client()?,
        ))
    }

    /// Executor with a caller-supplied client (tests: a plain-HTTP client
    /// without timeouts, so paused tokio time cannot fire them spuriously).
    pub fn with_client(
        platform: Platform,
        limiter: Arc<RateLimiter>,
        retry: RetryPolicy,
        client: reqwest::Client,
    ) -> Self {
        Self {
            platform,
            client,
            limiter,
            retry,
            rng: Mutex::new(Box::new(StdRng::from_os_rng())),
            cooldown: CooldownBehavior::Wait,
        }
    }

    /// Inject the jitter RNG (e.g. a seeded `StdRng` in tests).
    pub fn with_rng(self, rng: impl RngCore + Send + 'static) -> Self {
        Self {
            rng: Mutex::new(Box::new(rng)),
            ..self
        }
    }

    pub fn with_cooldown_behavior(self, cooldown: CooldownBehavior) -> Self {
        Self { cooldown, ..self }
    }

    pub fn platform(&self) -> Platform {
        self.platform
    }

    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    pub fn limiter(&self) -> &Arc<RateLimiter> {
        &self.limiter
    }

    pub fn retry_policy(&self) -> RetryPolicy {
        self.retry
    }

    /// `GET url` on this executor's client, ready for [`HttpExecutor::execute`].
    pub fn get(&self, url: impl IntoUrl) -> RequestBuilder {
        self.client.get(url)
    }

    /// Spend one token for `endpoint`, waiting through refills and cooldowns.
    /// Use with [`HttpExecutor::record_response`] when sending manually.
    pub async fn acquire(&self, endpoint: &str) -> Result<()> {
        self.limiter.acquire(self.platform, endpoint).await
    }

    /// Non-blocking variant of [`HttpExecutor::acquire`].
    pub fn try_acquire(&self, endpoint: &str) -> Result<()> {
        self.limiter.try_acquire(self.platform, endpoint)
    }

    pub fn record_response(
        &self,
        endpoint: &str,
        status: StatusCode,
        retry_after: Option<Duration>,
    ) -> Result<ResponseClass> {
        self.limiter
            .record_response(self.platform, endpoint, status, retry_after)
    }

    pub fn cooldown_state(&self, endpoint: &str) -> Result<CooldownState> {
        self.limiter.cooldown_state(self.platform, endpoint)
    }

    /// Send a read-only request, keyed by its URL path (falling back to the
    /// platform `default` limit). See [`HttpExecutor::execute_endpoint`].
    pub async fn execute(&self, request: RequestBuilder) -> Result<Response> {
        self.run(None, request, |response| async move { Ok(response) })
            .await
    }

    /// Send a read-only request under an explicit endpoint key.
    ///
    /// * Every attempt first acquires a token (and waits out any cooldown).
    /// * 429: the endpoint enters cooldown; returns [`Error::RateLimited`]
    ///   (no automatic retry).
    /// * 5xx / network errors: retried with full-jitter backoff up to
    ///   `max_attempts`, then [`Error::Network`].
    /// * Other statuses (2xx/3xx/4xx) are returned to the caller as-is.
    pub async fn execute_endpoint(
        &self,
        endpoint: &str,
        request: RequestBuilder,
    ) -> Result<Response> {
        self.run(
            Some(endpoint),
            request,
            |response| async move { Ok(response) },
        )
        .await
    }

    /// Like [`HttpExecutor::execute`], but also reads and decodes a JSON body.
    /// Disconnects while reading the body are retried; non-2xx statuses become
    /// [`Error::Network`]; undecodable bodies become [`Error::InvalidData`].
    pub async fn execute_json<T: DeserializeOwned>(&self, request: RequestBuilder) -> Result<T> {
        self.run(None, request, read_json).await
    }

    pub async fn execute_json_endpoint<T: DeserializeOwned>(
        &self,
        endpoint: &str,
        request: RequestBuilder,
    ) -> Result<T> {
        self.run(Some(endpoint), request, read_json).await
    }

    async fn run<T, F, Fut>(
        &self,
        endpoint: Option<&str>,
        request: RequestBuilder,
        finish: F,
    ) -> Result<T>
    where
        F: Fn(Response) -> Fut,
        Fut: Future<Output = std::result::Result<T, RetryError<Error>>>,
    {
        let (client, request) = request.build_split();
        let request = request.map_err(|err| Error::InvalidConfig(redact(err)))?;
        ensure_read_only(request.method())?;
        let path = request.url().path().to_owned();
        let endpoint = endpoint.unwrap_or(&path);
        // Fail closed before anything is sent if the endpoint is unknown.
        let limiter = self.limiter.resolve(self.platform, endpoint)?;
        let clock = self.limiter.clock();

        let platform = self.platform;
        let cooldown = self.cooldown;
        let (client, limiter, clock, finish) = (&client, &*limiter, &*clock, &finish);
        let mut original: Option<Request> = Some(request);
        let mut rng = SharedRng(&self.rng);

        retry_with_backoff(&self.retry, clock, &mut rng, move |attempt| {
            let request = match original.as_ref().and_then(Request::try_clone) {
                Some(clone) => Some(clone),
                None => original.take(),
            };
            async move {
                let request = request.ok_or_else(|| {
                    RetryError::Permanent(Error::Network(format!(
                        "{platform:?} {endpoint}: request body cannot be replayed for retry"
                    )))
                })?;

                if cooldown == CooldownBehavior::FailFast {
                    if let CooldownState::Cooldown { until } = limiter.cooldown_state() {
                        return Err(RetryError::Permanent(Error::RateLimited {
                            retry_after: until.saturating_duration_since(clock.now()),
                        }));
                    }
                }
                limiter.acquire().await;

                tracing::debug!(platform = ?platform, endpoint, attempt, "sending request");
                let response = match client.execute(request).await {
                    Ok(response) => response,
                    Err(err) if err.is_builder() => {
                        return Err(RetryError::Permanent(Error::InvalidConfig(redact(err))))
                    }
                    Err(err) => {
                        tracing::debug!(platform = ?platform, endpoint, attempt, "network error");
                        return Err(RetryError::transient(Error::Network(format!(
                            "{platform:?} {endpoint}: {}",
                            redact(err)
                        ))));
                    }
                };

                let status = response.status();
                let retry_after = retry_after_from_headers(response.headers(), clock.utc_now());
                match limiter.record_response(status, retry_after) {
                    ResponseClass::RateLimited { until } => {
                        Err(RetryError::Permanent(Error::RateLimited {
                            retry_after: until.saturating_duration_since(clock.now()),
                        }))
                    }
                    ResponseClass::ServerError => Err(RetryError::Transient {
                        error: Error::Network(format!(
                            "{platform:?} {endpoint}: HTTP {status} after {} attempt(s)",
                            attempt + 1
                        )),
                        retry_after,
                    }),
                    ResponseClass::Success | ResponseClass::ClientError => finish(response).await,
                }
            }
        })
        .await
    }
}

async fn read_json<T: DeserializeOwned>(
    response: Response,
) -> std::result::Result<T, RetryError<Error>> {
    let status = response.status();
    if !status.is_success() {
        return Err(RetryError::Permanent(Error::Network(format!(
            "unexpected HTTP {status}"
        ))));
    }
    let body = response.bytes().await.map_err(|err| {
        RetryError::transient(Error::Network(format!(
            "reading response body: {}",
            redact(err)
        )))
    })?;
    serde_json::from_slice(&body)
        .map_err(|err| RetryError::Permanent(Error::InvalidData(err.to_string())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_read_only_methods_are_allowed() {
        assert!(ensure_read_only(&Method::GET).is_ok());
        assert!(ensure_read_only(&Method::HEAD).is_ok());
        for method in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
            assert!(ensure_read_only(&method).is_err());
        }
    }

    #[test]
    fn debug_does_not_expose_client() {
        let executor = HttpExecutor::with_client(
            Platform::Skinport,
            Arc::new(RateLimiter::default()),
            RetryPolicy::default(),
            reqwest::Client::new(),
        );
        let debug = format!("{executor:?}");
        assert!(debug.contains("Skinport"));
        assert!(!debug.contains("client"));
    }
}
