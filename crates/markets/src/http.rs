//! Shared read-only HTTP plumbing. Only `GET` is implemented, by design.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use cs2_arb_core::{Error, Result};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, RETRY_AFTER};

use crate::clock::Clock;
use crate::gate::{Endpoint, RequestGate};

/// Delay assumed for a 429 that carries no usable retry hint.
pub(crate) const DEFAULT_429_RETRY: Duration = Duration::from_secs(60);

pub(crate) struct HttpCore {
    client: reqwest::Client,
    base_url: String,
    gate: Arc<dyn RequestGate>,
    clock: Arc<dyn Clock>,
}

impl HttpCore {
    pub(crate) fn new(
        base_url: &str,
        timeout: Option<Duration>,
        gate: Arc<dyn RequestGate>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self> {
        let base_url = base_url.trim_end_matches('/').to_owned();
        reqwest::Url::parse(&base_url)
            .map_err(|e| Error::InvalidConfig(format!("invalid base url: {e}")))?;
        let mut builder = reqwest::Client::builder()
            .user_agent(crate::USER_AGENT)
            .redirect(reqwest::redirect::Policy::none())
            // No idle-pool timers: keeps paused-time tests deterministic and costs nothing.
            .pool_idle_timeout(None);
        if let Some(timeout) = timeout {
            builder = builder.timeout(timeout);
        }
        let client = builder
            .build()
            .map_err(|e| Error::InvalidConfig(format!("http client: {e}")))?;
        Ok(Self {
            client,
            base_url,
            gate,
            clock,
        })
    }

    pub(crate) fn clock(&self) -> &dyn Clock {
        self.clock.as_ref()
    }

    /// `GET {base_url}{path_and_query}`. `path_and_query` must already be percent-encoded;
    /// it is sent byte-for-byte (important for DMarket signatures).
    pub(crate) async fn get(
        &self,
        endpoint: &Endpoint,
        path_and_query: &str,
        headers: HeaderMap,
    ) -> Result<Vec<u8>> {
        self.get_with_headers(endpoint, path_and_query, |_| Ok(headers))
            .await
    }

    pub(crate) async fn get_with_headers(
        &self,
        endpoint: &Endpoint,
        path_and_query: &str,
        headers: impl FnOnce(DateTime<Utc>) -> Result<HeaderMap>,
    ) -> Result<Vec<u8>> {
        self.gate.acquire(endpoint).await?;
        let url = format!("{}{}", self.base_url, path_and_query);
        let response = self
            .client
            .get(&url)
            .headers(headers(self.clock.now())?)
            .send()
            .await
            .map_err(|e| Error::Network(format!("{}: {}", endpoint.name, e.without_url())))?;
        let status = response.status().as_u16();
        let retry_after = retry_after_from_headers(response.headers(), self.clock.now())
            .or_else(|| (status == 429).then_some(DEFAULT_429_RETRY));
        self.gate.on_response(endpoint, status, retry_after);
        tracing::debug!(endpoint = endpoint.name, status, "market request completed");

        if status == 429 {
            return Err(Error::RateLimited {
                retry_after: retry_after.unwrap_or(DEFAULT_429_RETRY),
            });
        }
        if (500..600).contains(&status) {
            return Err(Error::Network(format!(
                "{} returned HTTP {status}",
                endpoint.name
            )));
        }
        if !(200..300).contains(&status) {
            return Err(Error::HttpStatus {
                endpoint: endpoint.name,
                status,
            });
        }
        let body = response
            .bytes()
            .await
            .map_err(|e| Error::Network(format!("{}: {}", endpoint.name, e.without_url())))?;
        Ok(body.to_vec())
    }
}

/// `Retry-After` (delta-seconds or HTTP-date), falling back to `X-RateLimit-Reset`
/// (unix seconds; small values are treated as delta-seconds).
pub(crate) fn retry_after_from_headers(
    headers: &HeaderMap,
    now: DateTime<Utc>,
) -> Option<Duration> {
    if let Some(value) = headers.get(RETRY_AFTER).and_then(|v| v.to_str().ok()) {
        let value = value.trim();
        if let Ok(secs) = value.parse::<u64>() {
            return Some(Duration::from_secs(secs));
        }
        if let Ok(date) = DateTime::parse_from_rfc2822(value) {
            return Some(
                (date.with_timezone(&Utc) - now)
                    .to_std()
                    .unwrap_or(Duration::ZERO),
            );
        }
    }
    let reset = headers
        .get(HeaderName::from_static("x-ratelimit-reset"))
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<i64>().ok())?;
    if reset < 1_000_000_000 {
        return Some(Duration::from_secs(reset.max(0) as u64));
    }
    Some(Duration::from_secs((reset - now.timestamp()).max(0) as u64))
}

pub(crate) fn header_value(value: &str) -> Result<HeaderValue> {
    HeaderValue::from_str(value).map_err(|_| Error::InvalidConfig("invalid header value".into()))
}

pub(crate) fn parse_json<T: serde::de::DeserializeOwned>(what: &str, body: &[u8]) -> Result<T> {
    serde_json::from_slice(body).map_err(|e| {
        Error::InvalidData(format!(
            "{what}: invalid JSON response at {}:{}",
            e.line(),
            e.column()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn parses_retry_hints() {
        let now = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        let mut h = HeaderMap::new();
        h.insert(RETRY_AFTER, HeaderValue::from_static("120"));
        assert_eq!(
            retry_after_from_headers(&h, now),
            Some(Duration::from_secs(120))
        );

        let mut h = HeaderMap::new();
        h.insert(
            RETRY_AFTER,
            HeaderValue::from_static("Wed, 01 Jan 2025 00:01:30 GMT"),
        );
        assert_eq!(
            retry_after_from_headers(&h, now),
            Some(Duration::from_secs(90))
        );

        let mut h = HeaderMap::new();
        let reset = (now.timestamp() + 30).to_string();
        h.insert("x-ratelimit-reset", HeaderValue::from_str(&reset).unwrap());
        assert_eq!(
            retry_after_from_headers(&h, now),
            Some(Duration::from_secs(30))
        );

        assert_eq!(retry_after_from_headers(&HeaderMap::new(), now), None);
    }
}
