//! DMarket read-only client with Ed25519 request signing.
//!
//! Endpoint verification (against the official spec served at
//! `https://docs.dmarket.com/v1/trading.swagger.json` and the official reference clients in
//! github.com/dmarket/dm-trading-tools):
//! - Base URL `https://api.dmarket.com` — confirmed.
//! - Signing — confirmed: headers `X-Api-Key` (lowercase hex public key), `X-Sign-Date`
//!   (unix seconds, must be within 2 minutes of server time) and
//!   `X-Request-Sign: dmar ed25519 <hex 64-byte signature>`, over the non-signed string
//!   `METHOD + path + ("?" + query) + body + timestamp`. The path is signed percent-DECODED
//!   and the query exactly as transmitted (percent-encoded). The secret key is the 64-byte
//!   NaCl secret key (`seed || public key`) in hex; a bare 32-byte seed is also accepted.
//! - Listings: the current spec documents `GET /marketplace-api/v2/offers`
//!   (`gameId=a8db` required, `title` = case-insensitive PREFIX filter, `orderBy=price`,
//!   `orderDir=asc`, `limit` 1..=100 required, `cursor`, `offerType`) returning
//!   `{items:[{offerId, priceCents (int64 string, USD cents), locked, createdAt,
//!   attributes{title, tradable, tradeLockDays, unlockDate, cs2{float (string), exterior}}}],
//!   total, cursor}` — confirmed. The legacy `/exchange/v1/market/items` route is NOT in the
//!   current spec, so it is not used.
//! - Sales: `GET /trade-aggregator/v1/last-sales` (`gameId`, `title` required, `limit`
//!   max 20, `offset`, `txOperationType`, `filters`) returning `{sales:[{price (USD decimal
//!   string "15.99"), date (unix seconds string), txOperationType, offerAttributes
//!   {floatValue,...}}]}` — confirmed. Rows carry no id; we synthesize one.
//! - Rate limits: numeric quotas are only referenced via the DMarket FAQ and are
//!   UNVERIFIED; left to the injected `RequestGate`.
//! - UNVERIFIED: whether `unlockDate` in the past with `tradeLockDays == 0` always means
//!   "tradable now" (we only set `trade_locked_until` for future unlock dates).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use cs2_arb_core::{
    Error, FeeSchedule, ItemId, Listing, MarketClient, Platform, PriceSnapshot, Result, Sale,
};
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT};
use serde::Deserialize;

use crate::clock::{Clock, SystemClock};
use crate::convert::{
    cents_from_i64, cents_from_int_str, float_matches_tier, query_string, usd_decimal_str_to_cents,
    usd_f64_to_cents, validate_item,
};
use crate::gate::{Endpoint, NoopGate, RequestGate};
use crate::http::{header_value, parse_json, HttpCore};

pub const DEFAULT_BASE_URL: &str = "https://api.dmarket.com";
pub const PUBLIC_KEY_ENV: &str = "DMARKET_PUBLIC_KEY";
pub const SECRET_KEY_ENV: &str = "DMARKET_SECRET_KEY";
pub const CS2_GAME_ID: &str = "a8db";
pub const SIGNATURE_PREFIX: &str = "dmar ed25519 ";

pub const OFFERS_ENDPOINT: Endpoint = Endpoint {
    platform: Platform::DMarket,
    name: "dmarket.offers",
    path: "/marketplace-api/v2/offers",
};

pub const LAST_SALES_ENDPOINT: Endpoint = Endpoint {
    platform: Platform::DMarket,
    name: "dmarket.last_sales",
    path: "/trade-aggregator/v1/last-sales",
};

// ---- signing ---------------------------------------------------------------------------

fn hex_decode(value: &str) -> Option<Vec<u8>> {
    let value = value.trim();
    if !value.len().is_multiple_of(2) {
        return None;
    }
    (0..value.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(value.get(i..i + 2)?, 16).ok())
        .collect()
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The DMarket non-signed string: `METHOD + path + ["?" + query] + body + timestamp`.
///
/// `decoded_path` is the route path with any path parameters percent-DECODED;
/// `raw_query` is the query string exactly as transmitted (no leading `?`).
pub fn string_to_sign(
    method: &str,
    decoded_path: &str,
    raw_query: &str,
    body: &str,
    timestamp: i64,
) -> String {
    let mut s = String::with_capacity(method.len() + decoded_path.len() + raw_query.len() + 32);
    s.push_str(&method.to_ascii_uppercase());
    s.push_str(decoded_path);
    if !raw_query.is_empty() {
        s.push('?');
        s.push_str(raw_query);
    }
    s.push_str(body);
    s.push_str(&timestamp.to_string());
    s
}

/// DMarket API key pair. The secret never leaves this struct: `Debug` is redacted, there is
/// no `Serialize`, and `ed25519_dalek::SigningKey` zeroizes on drop.
#[derive(Clone)]
pub struct DMarketCredentials {
    public_key_hex: String,
    signing_key: SigningKey,
}

impl std::fmt::Debug for DMarketCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DMarketCredentials")
            .field("public_key", &self.public_key_hex)
            .field("secret_key", &"<redacted>")
            .finish()
    }
}

impl DMarketCredentials {
    /// `secret_key_hex` is the 64-byte NaCl secret key (`seed || public`) or a 32-byte seed.
    /// The public key must match the one derived from the secret.
    pub fn from_hex(public_key_hex: &str, secret_key_hex: &str) -> Result<Self> {
        let mut secret = hex_decode(secret_key_hex)
            .ok_or_else(|| Error::InvalidConfig(format!("{SECRET_KEY_ENV} is not valid hex")))?;
        let seed: Option<[u8; 32]> = match secret.len() {
            32 | 64 => secret[..32].try_into().ok(),
            _ => None,
        };
        let embedded_public: Option<Vec<u8>> = (secret.len() == 64).then(|| secret[32..].to_vec());
        secret.iter_mut().for_each(|b| *b = 0);
        let mut seed = seed.ok_or_else(|| {
            Error::InvalidConfig(format!("{SECRET_KEY_ENV} must be 32 or 64 bytes of hex"))
        })?;
        let signing_key = SigningKey::from_bytes(&seed);
        seed.iter_mut().for_each(|b| *b = 0);
        let derived = signing_key.verifying_key().to_bytes();
        if embedded_public.is_some_and(|p| p != derived) {
            return Err(Error::InvalidConfig(format!(
                "{SECRET_KEY_ENV} is internally inconsistent"
            )));
        }
        let public = hex_decode(public_key_hex)
            .filter(|p| p.len() == 32)
            .ok_or_else(|| {
                Error::InvalidConfig(format!("{PUBLIC_KEY_ENV} must be 32 bytes of hex"))
            })?;
        if public != derived {
            return Err(Error::InvalidConfig(format!(
                "{PUBLIC_KEY_ENV} does not match {SECRET_KEY_ENV}"
            )));
        }
        Ok(Self {
            public_key_hex: hex_encode(&derived),
            signing_key,
        })
    }

    /// Reads `DMARKET_PUBLIC_KEY` and `DMARKET_SECRET_KEY`.
    pub fn from_env() -> Result<Self> {
        let public = std::env::var(PUBLIC_KEY_ENV)
            .map_err(|_| Error::InvalidConfig(format!("{PUBLIC_KEY_ENV} is not set")))?;
        let secret = std::env::var(SECRET_KEY_ENV)
            .map_err(|_| Error::InvalidConfig(format!("{SECRET_KEY_ENV} is not set")))?;
        Self::from_hex(&public, &secret)
    }

    /// Lowercase hex public key, as sent in `X-Api-Key`.
    pub fn public_key_hex(&self) -> &str {
        &self.public_key_hex
    }

    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing_key.verifying_key()
    }

    /// Hex-encoded 64-byte Ed25519 signature of `message`.
    pub fn sign(&self, message: &str) -> String {
        hex_encode(&self.signing_key.sign(message.as_bytes()).to_bytes())
    }

    /// The three DMarket auth headers for one request.
    pub fn auth_headers(
        &self,
        method: &str,
        decoded_path: &str,
        raw_query: &str,
        body: &str,
        timestamp: i64,
    ) -> Result<HeaderMap> {
        let signature = self.sign(&string_to_sign(
            method,
            decoded_path,
            raw_query,
            body,
            timestamp,
        ));
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("x-api-key"),
            header_value(&self.public_key_hex)?,
        );
        headers.insert(
            HeaderName::from_static("x-sign-date"),
            header_value(&timestamp.to_string())?,
        );
        let mut sign = header_value(&format!("{SIGNATURE_PREFIX}{signature}"))?;
        sign.set_sensitive(true);
        headers.insert(HeaderName::from_static("x-request-sign"), sign);
        Ok(headers)
    }
}

// ---- raw wire types --------------------------------------------------------------------

/// DMarket serializes int64 as strings; accept either.
fn value_as_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn value_as_f64(value: &serde_json::Value) -> Option<f64> {
    match value {
        serde_json::Value::String(s) => s.trim().parse().ok(),
        serde_json::Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

#[derive(Deserialize, Default)]
struct RawCs2 {
    #[serde(default)]
    float: Option<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawAttributes {
    title: String,
    #[serde(default)]
    trade_lock_days: Option<i64>,
    #[serde(default)]
    unlock_date: Option<String>,
    #[serde(default)]
    cs2: Option<RawCs2>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawOffer {
    offer_id: String,
    price_cents: serde_json::Value,
    #[serde(default)]
    locked: bool,
    attributes: RawAttributes,
}

#[derive(Deserialize)]
struct RawOffers {
    #[serde(default)]
    items: Vec<RawOffer>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSale {
    price: serde_json::Value,
    date: serde_json::Value,
    #[serde(default)]
    tx_operation_type: Option<String>,
    #[serde(default)]
    offer_attributes: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Deserialize)]
struct RawSales {
    #[serde(default)]
    sales: Vec<RawSale>,
}

fn parse_float(value: Option<&serde_json::Value>) -> Option<f64> {
    value
        .and_then(value_as_f64)
        .filter(|f| f.is_finite() && (0.0..=1.0).contains(f))
}

/// Parses `/marketplace-api/v2/offers` into core listings for exactly `item` (the server
/// `title` filter is a prefix match), skipping locked offers and out-of-tier floats.
pub fn parse_offers(
    body: &[u8],
    item: &ItemId,
    observed_at: DateTime<Utc>,
) -> Result<Vec<Listing>> {
    let raw: RawOffers = parse_json("dmarket offers", body)?;
    let mut out = Vec::new();
    for offer in raw.items {
        if offer.locked || offer.attributes.title != item.market_hash_name {
            continue;
        }
        let float_value = parse_float(offer.attributes.cs2.as_ref().and_then(|c| c.float.as_ref()));
        if !float_matches_tier(item, float_value) {
            tracing::warn!(offer = %offer.offer_id, "dmarket offer float outside wear tier");
            continue;
        }
        let price = match &offer.price_cents {
            serde_json::Value::Number(n) => cents_from_i64(n.as_i64().ok_or_else(|| {
                Error::InvalidData("dmarket priceCents is not an integer".into())
            })?)?,
            other => cents_from_int_str(
                &value_as_string(other)
                    .ok_or_else(|| Error::InvalidData("dmarket priceCents missing".into()))?,
            )?,
        };
        let unlock = offer
            .attributes
            .unlock_date
            .as_deref()
            .and_then(|d| DateTime::parse_from_rfc3339(d).ok())
            .map(|d| d.with_timezone(&Utc));
        let trade_locked_until = match (unlock, offer.attributes.trade_lock_days) {
            (Some(u), _) if u > observed_at => Some(u),
            (None, Some(days)) if days > 0 => Some(observed_at + chrono::Duration::days(days)),
            _ => None,
        };
        out.push(Listing {
            id: format!("dmarket:{}", offer.offer_id),
            platform: Platform::DMarket,
            item: item.clone(),
            price,
            quantity: 1,
            float_value,
            observed_at,
            trade_locked_until,
        });
    }
    out.sort_by_key(|l| l.price);
    Ok(out)
}

/// Parses `/trade-aggregator/v1/last-sales` into verified sales at or after `since`.
pub fn parse_last_sales(body: &[u8], item: &ItemId, since: DateTime<Utc>) -> Result<Vec<Sale>> {
    let raw: RawSales = parse_json("dmarket last sales", body)?;
    let mut out = Vec::new();
    for (index, sale) in raw.sales.into_iter().enumerate() {
        let ts: i64 = value_as_string(&sale.date)
            .and_then(|d| d.trim().parse().ok())
            .ok_or_else(|| Error::InvalidData("dmarket sale date is not unix seconds".into()))?;
        let sold_at = DateTime::from_timestamp(ts, 0)
            .ok_or_else(|| Error::InvalidData("dmarket sale date out of range".into()))?;
        if sold_at < since {
            continue;
        }
        let price = match &sale.price {
            serde_json::Value::String(s) => usd_decimal_str_to_cents(s)?,
            serde_json::Value::Number(n) => usd_f64_to_cents(n.as_f64().unwrap_or(f64::NAN))?,
            _ => return Err(Error::InvalidData("dmarket sale price missing".into())),
        };
        let float_value = parse_float(
            sale.offer_attributes
                .as_ref()
                .and_then(|a| a.get("floatValue")),
        );
        if !float_matches_tier(item, float_value) {
            continue;
        }
        out.push(Sale {
            id: format!(
                "dmarket:{}:{ts}:{}:{index}",
                sale.tx_operation_type.as_deref().unwrap_or("sale"),
                price.0
            ),
            platform: Platform::DMarket,
            item: item.clone(),
            price,
            float_value,
            sold_at,
            verified: true,
        });
    }
    out.sort_by_key(|s| s.sold_at);
    Ok(out)
}

// ---- client ----------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct DMarketConfig {
    pub base_url: String,
    pub game_id: String,
    /// `limit` for offers (1..=100).
    pub offers_limit: u32,
    /// `limit` for last-sales (1..=20).
    pub sales_limit: u32,
    /// `offerType` (`dmarket`, `p2p`, `all`); `None` uses the server default (`dmarket`).
    pub offer_type: Option<String>,
    pub request_timeout: Option<Duration>,
}

impl Default for DMarketConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.into(),
            game_id: CS2_GAME_ID.into(),
            offers_limit: 100,
            sales_limit: 20,
            offer_type: None,
            request_timeout: Some(Duration::from_secs(30)),
        }
    }
}

/// DMarket reads with Ed25519 request signing.
pub struct DMarketClient {
    fees: FeeSchedule,
    config: DMarketConfig,
    credentials: DMarketCredentials,
    http: HttpCore,
}

impl std::fmt::Debug for DMarketClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DMarketClient")
            .field("config", &self.config)
            .field("credentials", &self.credentials)
            .finish_non_exhaustive()
    }
}

impl DMarketClient {
    pub fn new(fees: FeeSchedule, credentials: DMarketCredentials) -> Result<Self> {
        Self::with_config(
            fees,
            DMarketConfig::default(),
            credentials,
            Arc::new(NoopGate),
            Arc::new(SystemClock),
        )
    }

    /// Production defaults with keys from `DMARKET_PUBLIC_KEY` / `DMARKET_SECRET_KEY`.
    pub fn from_env(fees: FeeSchedule, gate: Arc<dyn RequestGate>) -> Result<Self> {
        Self::with_config(
            fees,
            DMarketConfig::default(),
            DMarketCredentials::from_env()?,
            gate,
            Arc::new(SystemClock),
        )
    }

    pub fn with_config(
        fees: FeeSchedule,
        config: DMarketConfig,
        credentials: DMarketCredentials,
        gate: Arc<dyn RequestGate>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self> {
        fees.validate()?;
        if !(1..=100).contains(&config.offers_limit) || !(1..=20).contains(&config.sales_limit) {
            return Err(Error::InvalidConfig(
                "dmarket offers_limit must be 1..=100 and sales_limit 1..=20".into(),
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

    /// Signed `GET path?query`; returns the body. Read-only by construction.
    async fn signed_get(&self, endpoint: &Endpoint, query: &str) -> Result<Vec<u8>> {
        let timestamp = self.http.clock().now().timestamp();
        let mut headers =
            self.credentials
                .auth_headers("GET", endpoint.path, query, "", timestamp)?;
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        let path_and_query = if query.is_empty() {
            endpoint.path.to_owned()
        } else {
            format!("{}?{query}", endpoint.path)
        };
        self.http.get(endpoint, &path_and_query, headers).await
    }
}

#[async_trait]
impl MarketClient for DMarketClient {
    async fn list_listings(&self, item: &ItemId) -> Result<Vec<Listing>> {
        validate_item(item)?;
        let limit = self.config.offers_limit.to_string();
        let mut pairs = vec![
            ("gameId", self.config.game_id.as_str()),
            ("title", item.market_hash_name.as_str()),
            ("orderBy", "price"),
            ("orderDir", "asc"),
            ("limit", limit.as_str()),
        ];
        if let Some(offer_type) = &self.config.offer_type {
            pairs.push(("offerType", offer_type.as_str()));
        }
        let body = self
            .signed_get(&OFFERS_ENDPOINT, &query_string(&pairs))
            .await?;
        parse_offers(&body, item, self.http.clock().now())
    }

    async fn recent_sales(&self, item: &ItemId, since: DateTime<Utc>) -> Result<Vec<Sale>> {
        validate_item(item)?;
        let limit = self.config.sales_limit.to_string();
        let query = query_string(&[
            ("gameId", self.config.game_id.as_str()),
            ("title", item.market_hash_name.as_str()),
            ("limit", limit.as_str()),
        ]);
        let body = self.signed_get(&LAST_SALES_ENDPOINT, &query).await?;
        parse_last_sales(&body, item, since)
    }

    /// Lowest unlocked ask across the wear tier (`float_bucket: None`).
    async fn price_snapshot(&self, item: &ItemId) -> Result<PriceSnapshot> {
        let listings = self.list_listings(item).await?;
        let best = listings
            .into_iter()
            .min_by_key(|l| l.price)
            .ok_or_else(|| {
                Error::InvalidData(format!(
                    "dmarket has no unlocked offer for {}",
                    item.market_hash_name
                ))
            })?;
        Ok(PriceSnapshot {
            platform: Platform::DMarket,
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

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signature, Verifier};

    // Fixed test-only seed (RFC 8032 test vector 1). Not a real DMarket key.
    const SEED_HEX: &str = "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60";
    const PUBLIC_HEX: &str = "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a";

    fn creds() -> DMarketCredentials {
        DMarketCredentials::from_hex(PUBLIC_HEX, &format!("{SEED_HEX}{PUBLIC_HEX}")).unwrap()
    }

    #[test]
    fn builds_documented_string_to_sign() {
        assert_eq!(
            string_to_sign(
                "get",
                "/trade-aggregator/v1/last-sales",
                "gameId=a8db&limit=20",
                "",
                1605619994
            ),
            "GET/trade-aggregator/v1/last-sales?gameId=a8db&limit=201605619994"
        );
        assert_eq!(
            string_to_sign("POST", "/get-item", "Amount=%220.25%22", "{}", 1605619994),
            "POST/get-item?Amount=%220.25%22{}1605619994"
        );
        assert_eq!(string_to_sign("GET", "/x", "", "", 7), "GET/x7");
    }

    #[test]
    fn rfc8032_vector_matches() {
        // RFC 8032 section 7.1, TEST 1: empty message.
        let c = creds();
        assert_eq!(
            c.sign(""),
            "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b"
        );
    }

    #[test]
    fn signatures_are_deterministic_and_verify() {
        let c = creds();
        let message = string_to_sign(
            "GET",
            OFFERS_ENDPOINT.path,
            "gameId=a8db&title=AK-47%20%7C%20Redline%20%28Field-Tested%29",
            "",
            1_700_000_000,
        );
        let a = c.sign(&message);
        let b = c.sign(&message);
        assert_eq!(a, b);
        assert_eq!(a.len(), 128);
        let sig_bytes: [u8; 64] = hex_decode(&a).unwrap().try_into().unwrap();
        let signature = Signature::from_bytes(&sig_bytes);
        let public: [u8; 32] = hex_decode(PUBLIC_HEX).unwrap().try_into().unwrap();
        let vk = VerifyingKey::from_bytes(&public).unwrap();
        vk.verify(message.as_bytes(), &signature).unwrap();
        assert!(vk.verify(b"GET/tampered1700000000", &signature).is_err());
        assert_ne!(c.sign(&format!("{message}1")), a);

        let headers = c
            .auth_headers("GET", "/x", "a=1", "", 1_700_000_000)
            .unwrap();
        assert_eq!(headers["x-api-key"], PUBLIC_HEX);
        assert_eq!(headers["x-sign-date"], "1700000000");
        let sign = headers["x-request-sign"].to_str().unwrap();
        assert_eq!(
            sign,
            format!("{SIGNATURE_PREFIX}{}", c.sign("GET/x?a=11700000000"))
        );
    }

    #[test]
    fn validates_and_redacts_keys() {
        assert!(DMarketCredentials::from_hex(PUBLIC_HEX, SEED_HEX).is_ok());
        assert!(DMarketCredentials::from_hex(PUBLIC_HEX, "zz").is_err());
        assert!(DMarketCredentials::from_hex(PUBLIC_HEX, &SEED_HEX[..30]).is_err());
        let other_public = "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c";
        assert!(DMarketCredentials::from_hex(other_public, SEED_HEX).is_err());
        assert!(
            DMarketCredentials::from_hex(PUBLIC_HEX, &format!("{SEED_HEX}{other_public}")).is_err()
        );
        let debug = format!("{:?}", creds());
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains(SEED_HEX));
        // Uppercase public key input is normalized to lowercase for X-Api-Key.
        let upper = DMarketCredentials::from_hex(&PUBLIC_HEX.to_uppercase(), SEED_HEX).unwrap();
        assert_eq!(upper.public_key_hex(), PUBLIC_HEX);
    }
}
