//! Skinport read-only client: `GET /v1/items` and `GET /v1/sales/history`.
//!
//! See https://docs.skinport.com/items, https://docs.skinport.com/sales/history and
//! https://docs.skinport.com/introduction/rate-limits.
//! - Both endpoints require Brotli, are unauthenticated and share an 8 requests / 5 minutes
//!   quota; responses are cached server-side for 5 minutes.
//! - Query params `app_id` (730 for CS2 or 252490 for Rust), `currency` (we always send `USD` because `Money` is USD
//!   cents), `tradable` (items) and `market_hash_name` (sales history, comma-delimited,
//!   optional).
//! - `/v1/items` returns aggregate rows: `market_hash_name, currency, suggested_price,
//!   item_page, market_page, min_price, max_price, mean_price, median_price, quantity,
//!   created_at, updated_at`; prices are JSON numbers in currency units and may be `null`.
//! - `/v1/sales/history` returns aggregate rows with `last_24_hours`, `last_7_days`,
//!   `last_30_days`, `last_90_days` each `{min,max,avg,median,volume}`; prices nullable.
//! - Both endpoints can return `version` rows such as Doppler phases. The public item
//!   identity in this adapter is tier-level, not phase-specific.
//!
//! Skinport exposes no per-listing or per-sale records on these endpoints, so listings are
//! one aggregate row per item (price = `min_price`) and sales are an aggregate record per
//! window flagged `verified: false`.
//!
//! Both endpoints are fetched for the whole catalogue (no `market_hash_name` filter) so a
//! single request per TTL window serves every item.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use cs2_arb_core::{
    Error, FeeSchedule, Game, ItemId, Listing, MarketClient, Money, Platform, PriceSnapshot,
    Result, Sale,
};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, ACCEPT_ENCODING};
use serde::Deserialize;

use crate::cache::{Cached, TtlCache};
use crate::clock::{Clock, SystemClock};
use crate::convert::{query_string, usd_f64_to_cents, validate_item_for_game};
use crate::gate::{Endpoint, NoopGate, RequestGate};
use crate::http::{parse_json, HttpCore};
use crate::limiter::{LimitPolicy, SlidingWindowLimiter, WindowLimit};

pub const DEFAULT_BASE_URL: &str = "https://api.skinport.com";

pub const ITEMS_ENDPOINT: Endpoint = Endpoint {
    platform: Platform::Skinport,
    name: "skinport.items",
    path: "/v1/items",
};

pub const SALES_HISTORY_ENDPOINT: Endpoint = Endpoint {
    platform: Platform::Skinport,
    name: "skinport.sales_history",
    path: "/v1/sales/history",
};

#[derive(Debug, Clone)]
pub struct SkinportConfig {
    pub game: Game,
    /// Without trailing `/v1`; e.g. `https://api.skinport.com` or a mock server URL.
    pub base_url: String,
    /// Response cache TTL. Skinport caches these responses for 5 minutes server-side.
    pub cache_ttl: Duration,
    /// Optional directory for the on-disk JSON mirror of the cache.
    pub cache_dir: Option<PathBuf>,
    pub items_limit: WindowLimit,
    pub sales_history_limit: WindowLimit,
    pub on_limit: LimitPolicy,
    /// Serve an expired cached response instead of failing when locally or remotely rate
    /// limited. The returned data keeps its original `fetched_at`.
    pub serve_stale_on_limit: bool,
    /// Pass `tradable=1` to `/v1/items`.
    pub tradable_only: bool,
    pub request_timeout: Option<Duration>,
}

impl Default for SkinportConfig {
    fn default() -> Self {
        Self {
            game: Game::default(),
            base_url: DEFAULT_BASE_URL.into(),
            cache_ttl: Duration::from_secs(300),
            cache_dir: None,
            items_limit: WindowLimit::SKINPORT,
            sales_history_limit: WindowLimit::SKINPORT,
            on_limit: LimitPolicy::Reject,
            serve_stale_on_limit: true,
            tradable_only: false,
            request_timeout: Some(Duration::from_secs(60)),
        }
    }
}

/// One `/v1/items` row, normalized to USD cents.
#[derive(Debug, Clone, PartialEq)]
pub struct SkinportItem {
    pub market_hash_name: String,
    pub suggested_price: Option<Money>,
    pub min_price: Option<Money>,
    pub max_price: Option<Money>,
    pub mean_price: Option<Money>,
    pub median_price: Option<Money>,
    pub quantity: u32,
    pub updated_at: Option<DateTime<Utc>>,
}

/// Aggregate sale statistics for one window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SkinportSalesStats {
    pub min: Option<Money>,
    pub max: Option<Money>,
    pub avg: Option<Money>,
    pub median: Option<Money>,
    pub volume: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SalesWindow {
    Last24Hours,
    Last7Days,
    Last30Days,
    Last90Days,
}

impl SalesWindow {
    pub const ALL: [Self; 4] = [
        Self::Last24Hours,
        Self::Last7Days,
        Self::Last30Days,
        Self::Last90Days,
    ];

    pub fn duration(self) -> chrono::Duration {
        chrono::Duration::days(match self {
            Self::Last24Hours => 1,
            Self::Last7Days => 7,
            Self::Last30Days => 30,
            Self::Last90Days => 90,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Last24Hours => "24h",
            Self::Last7Days => "7d",
            Self::Last30Days => "30d",
            Self::Last90Days => "90d",
        }
    }
}

/// One `/v1/sales/history` row, normalized to USD cents.
#[derive(Debug, Clone, PartialEq)]
pub struct SkinportSalesHistory {
    pub market_hash_name: String,
    pub version: Option<String>,
    pub last_24_hours: Option<SkinportSalesStats>,
    pub last_7_days: Option<SkinportSalesStats>,
    pub last_30_days: Option<SkinportSalesStats>,
    pub last_90_days: Option<SkinportSalesStats>,
}

impl SkinportSalesHistory {
    pub fn window(&self, window: SalesWindow) -> Option<SkinportSalesStats> {
        match window {
            SalesWindow::Last24Hours => self.last_24_hours,
            SalesWindow::Last7Days => self.last_7_days,
            SalesWindow::Last30Days => self.last_30_days,
            SalesWindow::Last90Days => self.last_90_days,
        }
    }
}

pub type ItemsByName = Arc<HashMap<String, SkinportItem>>;
pub type SalesByName = Arc<HashMap<String, SkinportSalesHistory>>;

// ---- raw wire types --------------------------------------------------------------------

#[derive(Deserialize)]
struct RawItem {
    market_hash_name: String,
    version: Option<String>,
    #[serde(default)]
    currency: Option<String>,
    #[serde(default)]
    suggested_price: Option<f64>,
    #[serde(default)]
    min_price: Option<f64>,
    #[serde(default)]
    max_price: Option<f64>,
    #[serde(default)]
    mean_price: Option<f64>,
    #[serde(default)]
    median_price: Option<f64>,
    #[serde(default)]
    quantity: Option<u32>,
    #[serde(default)]
    updated_at: Option<i64>,
}

#[derive(Deserialize)]
struct RawStats {
    #[serde(default)]
    min: Option<f64>,
    #[serde(default)]
    max: Option<f64>,
    #[serde(default)]
    avg: Option<f64>,
    #[serde(default)]
    median: Option<f64>,
    #[serde(default)]
    volume: Option<u64>,
}

#[derive(Deserialize)]
struct RawSalesHistory {
    market_hash_name: String,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    currency: Option<String>,
    #[serde(default)]
    last_24_hours: Option<RawStats>,
    #[serde(default)]
    last_7_days: Option<RawStats>,
    #[serde(default)]
    last_30_days: Option<RawStats>,
    #[serde(default)]
    last_90_days: Option<RawStats>,
}

fn opt_cents(value: Option<f64>) -> Result<Option<Money>> {
    value.map(usd_f64_to_cents).transpose()
}

fn check_currency(currency: &Option<String>) -> Result<()> {
    match currency.as_deref() {
        None | Some("USD") => Ok(()),
        Some(other) => Err(Error::InvalidData(format!(
            "skinport returned currency {other}, expected USD"
        ))),
    }
}

fn convert_stats(raw: Option<RawStats>) -> Result<Option<SkinportSalesStats>> {
    raw.map(|s| {
        Ok(SkinportSalesStats {
            min: opt_cents(s.min)?,
            max: opt_cents(s.max)?,
            avg: opt_cents(s.avg)?,
            median: opt_cents(s.median)?,
            volume: s.volume.unwrap_or(0),
        })
    })
    .transpose()
}

pub fn parse_items(body: &[u8]) -> Result<ItemsByName> {
    let rows: Vec<RawItem> = parse_json("skinport items", body)?;
    let mut map = HashMap::with_capacity(rows.len());
    let mut aggregate = std::collections::HashSet::new();
    for row in rows {
        check_currency(&row.currency)?;
        let is_aggregate = row.version.is_none_or(|v| v.is_empty());
        let item = SkinportItem {
            suggested_price: opt_cents(row.suggested_price)?,
            min_price: opt_cents(row.min_price)?,
            max_price: opt_cents(row.max_price)?,
            mean_price: opt_cents(row.mean_price)?,
            median_price: opt_cents(row.median_price)?,
            quantity: row.quantity.unwrap_or(0),
            updated_at: row.updated_at.and_then(|t| DateTime::from_timestamp(t, 0)),
            market_hash_name: row.market_hash_name,
        };
        if is_aggregate {
            aggregate.insert(item.market_hash_name.clone());
            map.insert(item.market_hash_name.clone(), item);
        } else if !aggregate.contains(&item.market_hash_name) {
            let previous = map
                .entry(item.market_hash_name.clone())
                .or_insert_with(|| item.clone());
            if item.min_price.is_some()
                && (previous.min_price.is_none() || item.min_price < previous.min_price)
            {
                *previous = item;
            }
        }
    }
    Ok(Arc::new(map))
}

pub fn parse_sales_history(body: &[u8]) -> Result<SalesByName> {
    let rows: Vec<RawSalesHistory> = parse_json("skinport sales history", body)?;
    let mut map: HashMap<String, SkinportSalesHistory> = HashMap::with_capacity(rows.len());
    for row in rows {
        check_currency(&row.currency)?;
        let entry = SkinportSalesHistory {
            market_hash_name: row.market_hash_name,
            version: row.version.filter(|v| !v.is_empty()),
            last_24_hours: convert_stats(row.last_24_hours)?,
            last_7_days: convert_stats(row.last_7_days)?,
            last_30_days: convert_stats(row.last_30_days)?,
            last_90_days: convert_stats(row.last_90_days)?,
        };
        // Phase variants (e.g. Doppler `version`) share a market hash name; prefer the
        // unversioned aggregate row, otherwise keep the first variant seen.
        match map.get(&entry.market_hash_name) {
            Some(existing) if existing.version.is_none() || entry.version.is_some() => {}
            _ => {
                map.insert(entry.market_hash_name.clone(), entry);
            }
        }
    }
    Ok(Arc::new(map))
}

// ---- client ----------------------------------------------------------------------------

struct EndpointState<T> {
    endpoint: Endpoint,
    cache: TtlCache<T>,
    fetch_lock: tokio::sync::Mutex<()>,
}

impl<T: Clone> EndpointState<T> {
    fn new(endpoint: Endpoint, config: &SkinportConfig) -> Self {
        Self {
            endpoint,
            cache: TtlCache::new(endpoint.name, config.cache_ttl, config.cache_dir.clone()),
            fetch_lock: tokio::sync::Mutex::new(()),
        }
    }
}

/// Skinport listings and sale-history reads with local caching and rate limiting.
pub struct SkinportClient {
    fees: FeeSchedule,
    config: SkinportConfig,
    http: HttpCore,
    limiter: SlidingWindowLimiter,
    items: EndpointState<ItemsByName>,
    sales: EndpointState<SalesByName>,
}

impl std::fmt::Debug for SkinportClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SkinportClient")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl SkinportClient {
    /// Production defaults, no external gate, system clock.
    pub fn new(fees: FeeSchedule) -> Result<Self> {
        Self::with_config(
            fees,
            SkinportConfig::default(),
            Arc::new(NoopGate),
            Arc::new(SystemClock),
        )
    }

    pub fn with_config(
        fees: FeeSchedule,
        config: SkinportConfig,
        gate: Arc<dyn RequestGate>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self> {
        fees.validate()?;
        let http = HttpCore::new(&config.base_url, config.request_timeout, gate, clock)?;
        Ok(Self {
            fees,
            limiter: SlidingWindowLimiter::new(WindowLimit {
                max_requests: config
                    .items_limit
                    .max_requests
                    .min(config.sales_history_limit.max_requests),
                window: config
                    .items_limit
                    .window
                    .max(config.sales_history_limit.window),
            })?,
            items: EndpointState::new(ITEMS_ENDPOINT, &config),
            sales: EndpointState::new(SALES_HISTORY_ENDPOINT, &config),
            http,
            config,
        })
    }

    fn items_query(&self) -> String {
        let app_id = self.config.game.app_id().to_string();
        let mut pairs = vec![("app_id", app_id.as_str()), ("currency", "USD")];
        if self.config.tradable_only {
            pairs.push(("tradable", "1"));
        }
        query_string(&pairs)
    }

    fn sales_query(&self) -> String {
        let app_id = self.config.game.app_id().to_string();
        query_string(&[("app_id", app_id.as_str()), ("currency", "USD")])
    }

    /// Whole-catalogue `/v1/items`, cached.
    pub async fn items(&self) -> Result<Cached<ItemsByName>> {
        let query = self.items_query();
        self.fetch_cached(&self.items, &query, parse_items).await
    }

    /// Whole-catalogue `/v1/sales/history`, cached.
    pub async fn sales_history(&self) -> Result<Cached<SalesByName>> {
        let query = self.sales_query();
        self.fetch_cached(&self.sales, &query, parse_sales_history)
            .await
    }

    async fn fetch_cached<T: Clone>(
        &self,
        state: &EndpointState<T>,
        query: &str,
        parse: fn(&[u8]) -> Result<T>,
    ) -> Result<Cached<T>> {
        let path_and_query = format!("{}?{}", state.endpoint.path, query);
        let key = path_and_query.as_str();
        if let Some(hit) = state.cache.get_fresh(key) {
            return Ok(hit);
        }
        // Single-flight: concurrent misses on one endpoint share a single upstream request.
        let _guard = state.fetch_lock.lock().await;
        if let Some(hit) = state.cache.get_fresh(key) {
            return Ok(hit);
        }
        if state.cache.get_any(key).is_none() {
            if let Some(hit) = self.load_from_disk(state, key, parse).await {
                if hit.fresh {
                    return Ok(hit);
                }
            }
        }

        if let Err(err) = self.limiter.acquire(self.config.on_limit).await {
            return self.stale_or(state, key, err);
        }
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("br"));
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        let body = match self.http.get(&state.endpoint, key, headers).await {
            Ok(body) => body,
            Err(err @ Error::RateLimited { retry_after }) => {
                self.limiter.penalize(retry_after);
                return self.stale_or(state, key, err);
            }
            Err(err) => return Err(err),
        };
        let value = parse(&body)?;
        let fetched_at = self.http.clock().now();
        state.cache.store_disk(key, fetched_at, &body).await;
        Ok(state.cache.insert(key, value, fetched_at, Duration::ZERO))
    }

    async fn load_from_disk<T: Clone>(
        &self,
        state: &EndpointState<T>,
        key: &str,
        parse: fn(&[u8]) -> Result<T>,
    ) -> Option<Cached<T>> {
        let (fetched_at, body) = state.cache.load_disk(key).await?;
        let value = match parse(&body) {
            Ok(value) => value,
            Err(err) => {
                tracing::warn!(endpoint = state.endpoint.name, %err, "discarding cached body");
                return None;
            }
        };
        let age = (self.http.clock().now() - fetched_at)
            .to_std()
            .unwrap_or(Duration::ZERO);
        Some(state.cache.insert(key, value, fetched_at, age))
    }

    fn stale_or<T: Clone>(
        &self,
        state: &EndpointState<T>,
        key: &str,
        err: Error,
    ) -> Result<Cached<T>> {
        if self.config.serve_stale_on_limit {
            if let Some(stale) = state.cache.get_any(key) {
                tracing::warn!(
                    endpoint = state.endpoint.name,
                    fetched_at = %stale.fetched_at,
                    "rate limited; serving stale cached response"
                );
                return Ok(stale);
            }
        }
        Err(err)
    }
}

#[async_trait]
impl MarketClient for SkinportClient {
    async fn list_listings(&self, item: &ItemId) -> Result<Vec<Listing>> {
        validate_item_for_game(item, self.config.game)?;
        let items = self.items().await?;
        let Some(row) = items.value.get(&item.market_hash_name) else {
            return Ok(Vec::new());
        };
        let Some(price) = row.min_price.filter(|_| row.quantity > 0) else {
            return Ok(Vec::new());
        };
        Ok(vec![Listing {
            id: format!("skinport:{}:min", item.market_hash_name),
            platform: Platform::Skinport,
            item: item.clone(),
            price,
            quantity: 1,
            float_value: None,
            observed_at: items.fetched_at,
            trade_locked_until: None,
        }])
    }

    /// Skinport only publishes aggregates. Returns one unverified record for the smallest
    /// window that covers `since` (median price, falling back to average), stamped with the
    /// fetch time.
    async fn recent_sales(&self, item: &ItemId, since: DateTime<Utc>) -> Result<Vec<Sale>> {
        validate_item_for_game(item, self.config.game)?;
        let history = self.sales_history().await?;
        let Some(row) = history.value.get(&item.market_hash_name) else {
            return Ok(Vec::new());
        };
        let now = history.fetched_at;
        if since > now {
            return Ok(Vec::new());
        }
        let lookback = now - since;
        let window = SalesWindow::ALL
            .into_iter()
            .find(|w| w.duration() >= lookback)
            .unwrap_or(SalesWindow::Last90Days);
        let Some(stats) = row.window(window) else {
            return Ok(Vec::new());
        };
        let Some(price) = stats.median.or(stats.avg).filter(|_| stats.volume > 0) else {
            return Ok(Vec::new());
        };
        Ok(vec![Sale {
            id: format!("skinport:agg:{}:{}", window.label(), item.market_hash_name),
            platform: Platform::Skinport,
            item: item.clone(),
            price,
            float_value: None,
            sold_at: now,
            verified: false,
        }])
    }

    async fn price_snapshot(&self, item: &ItemId) -> Result<PriceSnapshot> {
        validate_item_for_game(item, self.config.game)?;
        let items = self.items().await?;
        let price = items
            .value
            .get(&item.market_hash_name)
            .filter(|row| row.quantity > 0)
            .and_then(|row| row.min_price)
            .ok_or_else(|| {
                Error::InvalidData(format!(
                    "skinport has no active listing for {}",
                    item.market_hash_name
                ))
            })?;
        Ok(PriceSnapshot {
            platform: Platform::Skinport,
            item: item.clone(),
            float_bucket: None,
            price,
            observed_at: items.fetched_at,
        })
    }

    async fn fees(&self) -> Result<FeeSchedule> {
        Ok(self.fees)
    }
}
