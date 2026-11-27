//! Read-only marketplace adapters for Skinport, CSFloat, DMarket and Waxpeer.
//!
//! MOCK SIMULATION MODE ONLY: this crate exposes no marketplace execution interface. Every
//! HTTP request issued here is a `GET` against a listings / sales / price endpoint; there is
//! no code path that can buy, sell, list, delist, or otherwise place an order.
//!
//! Admission control is abstracted by the local [`RequestGate`] trait so this crate does not
//! depend on `cs2-arb-net` internals. Skinport additionally enforces its documented
//! 8-requests-per-5-minutes limit per endpoint locally and serves repeat calls from a TTL
//! cache (memory plus optional on-disk JSON).

pub mod cache;
pub mod clock {
    pub use cs2_arb_core::{Clock, ManualClock, SystemClock};
}
pub mod convert;
pub mod csfloat;
pub mod dmarket;
pub mod gate;
mod http;
pub mod limiter;
pub mod skinport;
pub mod waxpeer;

pub use cache::Cached;
pub use clock::{Clock, ManualClock, SystemClock};
pub use csfloat::{CsFloatClient, CsFloatConfig, CsFloatCredentials};
pub use dmarket::{DMarketClient, DMarketConfig, DMarketCredentials};
pub use gate::{Endpoint, NetGate, NoopGate, RequestGate};
mod resilient;
pub use limiter::{LimitPolicy, SlidingWindowLimiter, WindowLimit};
pub use resilient::ResilientMarket;
pub use skinport::{SkinportClient, SkinportConfig};
pub use waxpeer::WaxpeerClient;

/// User-Agent sent on every request.
pub const USER_AGENT: &str = concat!("cs2-arb-mock/", env!("CARGO_PKG_VERSION"), " (read-only)");
