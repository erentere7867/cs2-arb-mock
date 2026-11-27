//! Shared read-only HTTP plumbing. Only `GET` is implemented, by design.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use cs2_arb_core::{Error, Platform, Result};
use reqwest::header::{HeaderMap, HeaderValue};
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::clock::Clock;
use crate::gate::{Endpoint, RequestGate};

/// Delay assumed for a 429 that carries no usable retry hint.
const DEFAULT_429_RETRY: Duration = Duration::from_secs(60);
const RETRY_BODY_LIMIT: usize = 4096;

pub(crate) struct HttpCore {
    client: reqwest::Client,
    base_url: String,
    gate: Arc<dyn RequestGate>,
    clock: Arc<dyn Clock>,
    cooldown: Mutex<Option<Instant>>,
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
            cooldown: Mutex::new(None),
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
        check_cooldown(*self.cooldown.lock().await)?;
        self.gate.acquire(endpoint).await?;
        // Hold admissions until the response headers (and any 429 hint) are known.
        let mut cooldown = self.cooldown.lock().await;
        check_cooldown(*cooldown)?;
        let url = format!("{}{}", self.base_url, path_and_query);
        let mut response = self
            .client
            .get(&url)
            .headers(headers(self.clock.now())?)
            .send()
            .await
            .map_err(|e| Error::Network(format!("{}: {}", endpoint.name, e.without_url())))?;
        let status = response.status().as_u16();
        let retry_after =
            cs2_arb_net::retry_after_from_headers(response.headers(), self.clock.now());
        let reset = reset_from_headers(response.headers(), self.clock.now());

        if status == 429 {
            // Skinport asks for a full quiet window even if another hint is shorter:
            // https://docs.skinport.com/introduction/rate-limits
            let minimum = if endpoint.platform == Platform::Skinport {
                Duration::from_secs(300)
            } else {
                Duration::ZERO
            };
            let received_at = Instant::now();
            // Keep a safe cooldown even if the caller cancels while reading the error body.
            *cooldown = Some(received_at + DEFAULT_429_RETRY.max(minimum));
            let hint = match retry_after {
                Some(delay) => Some(delay),
                None => tokio::time::timeout(
                    Duration::from_secs(1),
                    retry_after_from_body(&mut response),
                )
                .await
                .ok()
                .flatten()
                .max(reset),
            };
            let retry_after = hint
                .unwrap_or(DEFAULT_429_RETRY)
                .max(minimum)
                .min(Duration::from_secs(u32::MAX.into()));
            *cooldown = Some(received_at + retry_after);
            self.gate.on_response(endpoint, status, Some(retry_after));
            tracing::warn!(
                endpoint = endpoint.name,
                ?retry_after,
                "HTTP 429 received; pausing market host requests"
            );
            return Err(Error::RateLimited { retry_after });
        }

        if (200..300).contains(&status) && quota_exhausted(response.headers()) {
            if let Some(delay) = retry_after.or(reset) {
                let delay = delay.min(Duration::from_secs(u32::MAX.into()));
                *cooldown = Some(Instant::now() + delay);
            }
        }
        self.gate
            .on_response(endpoint, status, retry_after.or(reset));
        drop(cooldown);
        tracing::debug!(endpoint = endpoint.name, status, "market request completed");
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

fn check_cooldown(until: Option<Instant>) -> Result<()> {
    if let Some(retry_after) = until.and_then(|until| until.checked_duration_since(Instant::now()))
    {
        if !retry_after.is_zero() {
            return Err(Error::RateLimited { retry_after });
        }
    }
    Ok(())
}

fn header_number(headers: &HeaderMap, name: &'static str) -> Option<i64> {
    headers.get(name)?.to_str().ok()?.trim().parse().ok()
}

fn quota_exhausted(headers: &HeaderMap) -> bool {
    [
        "ratelimit-remaining",
        "x-ratelimit-remaining",
        "x-ratelimit-remaining-second",
    ]
    .iter()
    .any(|name| header_number(headers, name) == Some(0))
}

fn reset_from_headers(headers: &HeaderMap, now: DateTime<Utc>) -> Option<Duration> {
    // DMarket uses relative RateLimit-Reset; legacy X-RateLimit-Reset can be Unix time.
    if let Some(secs) = header_number(headers, "ratelimit-reset") {
        return Some(Duration::from_secs(secs.max(0) as u64));
    }
    let reset = header_number(headers, "x-ratelimit-reset")?;
    let secs = if reset < 1_000_000_000 {
        reset
    } else {
        reset.saturating_sub(now.timestamp())
    };
    Some(Duration::from_secs(secs.max(0) as u64))
}

async fn retry_after_from_body(response: &mut reqwest::Response) -> Option<Duration> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        body.extend_from_slice(&chunk[..chunk.len().min(RETRY_BODY_LIMIT - body.len())]);
        if body.len() == RETRY_BODY_LIMIT {
            break;
        }
    }
    let text = String::from_utf8_lossy(&body).to_ascii_lowercase();
    text.match_indices("retry after")
        .find_map(|(start, phrase)| {
            let rest = text[start + phrase.len()..].trim_start();
            let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
            let seconds = rest[..digits].parse::<u64>().ok()?;
            rest[digits..]
                .trim_start()
                .starts_with('s')
                .then_some(Duration::from_secs(seconds))
        })
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
    use cs2_arb_net::retry_after_from_headers;
    use reqwest::header::RETRY_AFTER;

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
        assert_eq!(reset_from_headers(&h, now), Some(Duration::from_secs(30)));

        assert_eq!(retry_after_from_headers(&HeaderMap::new(), now), None);
    }
}
