//! CSFloat read-only client: listings and per-item sales history.
//!
//! Endpoint verification (docs.csfloat.com did not resolve from this environment; checked
//! against public wrappers PaxxPatriot/csfloat.py, Bios-Marcel/csfloat_go and
//! EdlZitrone/csfloat-client):
//! - Base URL `https://csfloat.com/api/v1` — confirmed by all three wrappers.
//! - Auth header `Authorization: <api key>` (raw key, no scheme) — confirmed by all three.
//!   UNVERIFIED: whether listings / history work without a key (we send it when configured).
//! - `GET /api/v1/listings` with `limit` (max 50), `sort_by=lowest_price`, `type=buy_now`,
//!   `min_float`/`max_float`, `min_price`/`max_price` (USD cents) — confirmed by wrappers.
//!   UNVERIFIED: the `market_hash_name` filter param (not used by any wrapper; we send it
//!   AND filter client-side on `item.market_hash_name`, so a server that ignores it still
//!   yields correct, if sparser, results).
//! - Listings response shape: `{"data": [...], "cursor": ...}` per csfloat_go and
//!   csfloat-client; older csfloat.py treats it as a bare array. Both shapes are accepted.
//! - Listing fields `id`, `price` (integer USD cents), `type` (`buy_now`/`auction`),
//!   `state`, `created_at`, `item.market_hash_name`, `item.float_value` — confirmed.
//! - `GET /api/v1/history/<url-encoded market_hash_name>/sales` (optional `paint_index`)
//!   returns a bare JSON array of up to ~40 recent sales with `price` (cents),
//!   `item{market_hash_name,float_value}`, `sold_at` (RFC 3339) — confirmed by csfloat_go and
//!   csfloat-client. UNVERIFIED: presence of `id` on history rows (we synthesize one if
//!   absent).
//! - Rate limits: CSFloat returns `X-RateLimit-Limit/Remaining/Reset` (reset = unix seconds)
//!   per csfloat_go / csfloat.py; numeric quotas are UNVERIFIED and left to the injected
//!   `RequestGate`.
//! - UNVERIFIED: trade-hold information is not exposed by these endpoints, so
//!   `trade_locked_until` is always `None`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use cs2_arb_core::{
    Error, FeeSchedule, ItemId, Listing, MarketClient, Platform, PriceSnapshot, Result, Sale,
};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION};
use serde::Deserialize;

use crate::clock::{Clock, SystemClock};
use crate::convert::{
    cents_from_i64, float_matches_tier, percent_encode, query_string, validate_item,
};
use crate::gate::{Endpoint, NoopGate, RequestGate};
use crate::http::{header_value, parse_json, HttpCore};

pub const DEFAULT_BASE_URL: &str = "https://csfloat.com";
pub const API_KEY_ENV: &str = "CSFLOAT_API_KEY";

pub const LISTINGS_ENDPOINT: Endpoint = Endpoint {
    platform: Platform::CsFloat,
    name: "csfloat.listings",
    path: "/api/v1/listings",
};

pub const SALES_HISTORY_ENDPOINT: Endpoint = Endpoint {
    platform: Platform::CsFloat,
    name: "csfloat.sales_history",
    path: "/api/v1/history/{market_hash_name}/sales",
};

/// CSFloat API key. Never printed: `Debug` is redacted and there is no `Serialize`.
#[derive(Clone)]
pub struct CsFloatCredentials {
    api_key: String,
}

impl CsFloatCredentials {
    pub fn new(api_key: impl Into<String>) -> Result<Self> {
        let api_key = api_key.into();
        if api_key.trim().is_empty() {
            return Err(Error::InvalidConfig("empty CSFloat API key".into()));
        }
        HeaderValue::from_str(&api_key)
            .map_err(|_| Error::InvalidConfig("CSFloat API key is not a valid header".into()))?;
        Ok(Self { api_key })
    }

    /// Reads `CSFLOAT_API_KEY`; `Ok(None)` when unset or empty.
    pub fn from_env() -> Result<Option<Self>> {
        match std::env::var(API_KEY_ENV) {
            Ok(key) if !key.trim().is_empty() => Self::new(key).map(Some),
            _ => Ok(None),
        }
    }
}

impl std::fmt::Debug for CsFloatCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CsFloatCredentials")
            .field("api_key", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct CsFloatConfig {
    pub base_url: String,
    /// Listings page size (CSFloat maximum is 50).
    pub listings_limit: u32,
    pub request_timeout: Option<Duration>,
}

impl Default for CsFloatConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.into(),
            listings_limit: 50,
            request_timeout: Some(Duration::from_secs(30)),
        }
    }
}

// ---- raw wire types --------------------------------------------------------------------

#[derive(Deserialize)]
struct RawItem {
    market_hash_name: String,
    #[serde(default)]
    float_value: Option<f64>,
}

#[derive(Deserialize)]
struct RawListing {
    id: serde_json::Value,
    price: i64,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    state: Option<String>,
    item: RawItem,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawListings {
    Wrapped { data: Vec<RawListing> },
    Bare(Vec<RawListing>),
}

#[derive(Deserialize)]
struct RawSale {
    #[serde(default)]
    id: Option<serde_json::Value>,
    price: i64,
    item: RawItem,
    sold_at: DateTime<Utc>,
}

fn id_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Parses a listings body into core listings for `item`, dropping auctions, non-listed
/// states, other items and floats outside the item's wear tier.
pub fn parse_listings(
    body: &[u8],
    item: &ItemId,
    observed_at: DateTime<Utc>,
) -> Result<Vec<Listing>> {
    let raw = match parse_json::<RawListings>("csfloat listings", body)? {
        RawListings::Wrapped { data } => data,
        RawListings::Bare(data) => data,
    };
    let mut out = Vec::new();
    for row in raw {
        if row.item.market_hash_name != item.market_hash_name
            || row.kind.as_deref().is_some_and(|k| k != "buy_now")
            || row.state.as_deref().is_some_and(|s| s != "listed")
        {
            continue;
        }
        if !float_matches_tier(item, row.item.float_value) {
            tracing::warn!(id = %id_string(&row.id), "csfloat listing float outside wear tier");
            continue;
        }
        out.push(Listing {
            id: format!("csfloat:{}", id_string(&row.id)),
            platform: Platform::CsFloat,
            item: item.clone(),
            price: cents_from_i64(row.price)?,
            quantity: 1,
            float_value: row.item.float_value,
            observed_at,
            trade_locked_until: None,
        });
    }
    out.sort_by_key(|l| l.price);
    Ok(out)
}

/// Parses a sales-history body into verified core sales sold at or after `since`.
pub fn parse_sales(body: &[u8], item: &ItemId, since: DateTime<Utc>) -> Result<Vec<Sale>> {
    let raw: Vec<RawSale> = parse_json("csfloat sales history", body)?;
    let mut out = Vec::new();
    for (index, row) in raw.into_iter().enumerate() {
        if row.item.market_hash_name != item.market_hash_name
            || row.sold_at < since
            || !float_matches_tier(item, row.item.float_value)
        {
            continue;
        }
        let id = match &row.id {
            Some(id) => format!("csfloat:{}", id_string(id)),
            None => format!("csfloat:sale:{}:{index}", row.sold_at.timestamp()),
        };
        out.push(Sale {
            id,
            platform: Platform::CsFloat,
            item: item.clone(),
            price: cents_from_i64(row.price)?,
            float_value: row.item.float_value,
            sold_at: row.sold_at,
            verified: true,
        });
    }
    out.sort_by_key(|s| s.sold_at);
    Ok(out)
}

// ---- client ----------------------------------------------------------------------------

/// CSFloat listings and sale-history reads.
pub struct CsFloatClient {
    fees: FeeSchedule,
    config: CsFloatConfig,
    credentials: Option<CsFloatCredentials>,
    http: HttpCore,
}

impl std::fmt::Debug for CsFloatClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CsFloatClient")
            .field("config", &self.config)
            .field("credentials", &self.credentials)
            .finish_non_exhaustive()
    }
}

impl CsFloatClient {
    /// Production defaults without an API key.
    pub fn new(fees: FeeSchedule) -> Result<Self> {
        Self::with_config(
            fees,
            CsFloatConfig::default(),
            None,
            Arc::new(NoopGate),
            Arc::new(SystemClock),
        )
    }

    /// Production defaults with the key from `CSFLOAT_API_KEY` (if set).
    pub fn from_env(fees: FeeSchedule, gate: Arc<dyn RequestGate>) -> Result<Self> {
        Self::with_config(
            fees,
            CsFloatConfig::default(),
            CsFloatCredentials::from_env()?,
            gate,
            Arc::new(SystemClock),
        )
    }

    pub fn with_config(
        fees: FeeSchedule,
        config: CsFloatConfig,
        credentials: Option<CsFloatCredentials>,
        gate: Arc<dyn RequestGate>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self> {
        fees.validate()?;
        if !(1..=50).contains(&config.listings_limit) {
            return Err(Error::InvalidConfig(
                "csfloat listings_limit must be 1..=50".into(),
            ));
        }
        let http = HttpCore::new(&config.base_url, config.request_timeout, gate, clock)?;
        Ok(Self {
            fees,
            config,
            credentials,
            http,
        })
    }

    fn headers(&self) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        if let Some(creds) = &self.credentials {
            let mut value = header_value(&creds.api_key)?;
            value.set_sensitive(true);
            headers.insert(AUTHORIZATION, value);
        }
        Ok(headers)
    }
}

#[async_trait]
impl MarketClient for CsFloatClient {
    async fn list_listings(&self, item: &ItemId) -> Result<Vec<Listing>> {
        validate_item(item)?;
        let limit = self.config.listings_limit.to_string();
        let query = query_string(&[
            // UNVERIFIED parameter; see module docs.
            ("market_hash_name", item.market_hash_name.as_str()),
            ("sort_by", "lowest_price"),
            ("type", "buy_now"),
            ("limit", limit.as_str()),
        ]);
        let path = format!("{}?{query}", LISTINGS_ENDPOINT.path);
        let body = self
            .http
            .get(&LISTINGS_ENDPOINT, &path, self.headers()?)
            .await?;
        parse_listings(&body, item, self.http.clock().now())
    }

    async fn recent_sales(&self, item: &ItemId, since: DateTime<Utc>) -> Result<Vec<Sale>> {
        validate_item(item)?;
        let path = format!(
            "/api/v1/history/{}/sales",
            percent_encode(&item.market_hash_name)
        );
        let body = self
            .http
            .get(&SALES_HISTORY_ENDPOINT, &path, self.headers()?)
            .await?;
        parse_sales(&body, item, since)
    }

    /// Lowest buy-now ask across the wear tier (`float_bucket: None`).
    async fn price_snapshot(&self, item: &ItemId) -> Result<PriceSnapshot> {
        let listings = self.list_listings(item).await?;
        let best = listings
            .into_iter()
            .min_by_key(|l| l.price)
            .ok_or_else(|| {
                Error::InvalidData(format!(
                    "csfloat has no buy-now listing for {}",
                    item.market_hash_name
                ))
            })?;
        Ok(PriceSnapshot {
            platform: Platform::CsFloat,
            item: item.clone(),
            float_bucket: None,
            price: best.price,
            observed_at: best.observed_at,
        })
    }

    async fn fees(&self) -> Result<FeeSchedule> {
        Ok(self.fees)
    }
}
