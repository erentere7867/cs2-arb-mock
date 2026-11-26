//! Local admission-control abstraction.
//!
//! Admission control for marketplace endpoints, backed by `cs2-arb-net` in the CLI.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use cs2_arb_core::{Platform, Result};
use cs2_arb_net::RateLimiter;
use reqwest::StatusCode;

/// Identifies a rate-limit bucket: one per platform route template.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Endpoint {
    pub platform: Platform,
    /// Stable bucket name, e.g. `"skinport.items"`.
    pub name: &'static str,
    /// Route template (path parameters shown as `{name}`), e.g. `"/v1/items"`.
    pub path: &'static str,
}

/// Admission control consulted around every outbound request.
#[async_trait]
pub trait RequestGate: Send + Sync {
    /// Wait for (or refuse, with `Error::RateLimited`) permission to send one request.
    async fn acquire(&self, endpoint: &Endpoint) -> Result<()>;

    /// Report the HTTP status of a completed request and any server-provided retry delay
    /// (`Retry-After`, or `X-RateLimit-Reset` converted to a delay). Called for every
    /// response that produced a status, including 429 and 5xx.
    fn on_response(&self, endpoint: &Endpoint, status: u16, retry_after: Option<Duration>);
}

/// Gate that admits everything and ignores responses.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopGate;

#[async_trait]
impl RequestGate for NoopGate {
    async fn acquire(&self, _endpoint: &Endpoint) -> Result<()> {
        Ok(())
    }

    fn on_response(&self, _endpoint: &Endpoint, _status: u16, _retry_after: Option<Duration>) {}
}

#[derive(Debug, Clone)]
pub struct NetGate(pub Arc<RateLimiter>);

#[async_trait]
impl RequestGate for NetGate {
    async fn acquire(&self, endpoint: &Endpoint) -> Result<()> {
        self.0.acquire(endpoint.platform, endpoint.path).await
    }

    fn on_response(&self, endpoint: &Endpoint, status: u16, retry_after: Option<Duration>) {
        if let Ok(status) = StatusCode::from_u16(status) {
            if let Err(error) =
                self.0
                    .record_response(endpoint.platform, endpoint.path, status, retry_after)
            {
                tracing::warn!(endpoint = endpoint.name, %error, "could not record HTTP status");
            }
        }
    }
}
