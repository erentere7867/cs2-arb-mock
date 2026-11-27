use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use cs2_arb_core::{
    BuyOrder, Error, FeeSchedule, Game, ItemId, Listing, MarketClient, Money, Platform,
    PriceSnapshot, Result, Sale,
};
use reqwest::header::HeaderMap;
use serde::Deserialize;

use crate::cache::{Cached, TtlCache};
use crate::clock::Clock;
use crate::convert::{cents_from_i64, query_string, validate_item_for_game};
use crate::gate::{Endpoint, RequestGate};
use crate::http::{parse_json, HttpCore};

pub const PRICES: Endpoint = Endpoint {
    platform: Platform::Waxpeer,
    name: "waxpeer.prices",
    path: "/v1/prices/snapshot",
};
pub const BUY_ORDERS: Endpoint = Endpoint {
    platform: Platform::Waxpeer,
    name: "waxpeer.buy_orders",
    path: "/v1/buy-orders/snapshot",
};

const DETAILED_BUY_ORDERS: Endpoint = Endpoint {
    platform: Platform::Waxpeer,
    name: "waxpeer.detailed_buy_orders",
    path: "/v2/buy-orders",
};

fn parse_detailed_bids(
    body: &[u8],
    item: &ItemId,
    game: &str,
    at: DateTime<Utc>,
) -> Result<Vec<BuyOrder>> {
    #[derive(Deserialize)]
    struct Response {
        success: bool,
        #[serde(default)]
        orders: Vec<Row>,
    }
    #[derive(Deserialize)]
    struct Row {
        name: String,
        game: String,
        price: Minor,
        amount: u32,
        filled: u32,
        filters: serde_json::Value,
        by: String,
    }
    let response: Response = parse_json("Waxpeer detailed buy orders", body)?;
    if !response.success {
        return Err(Error::InvalidData(
            "Waxpeer detailed buy orders unavailable".into(),
        ));
    }
    let mut owners = HashSet::new();
    let mut depth = BTreeMap::<Money, u32>::new();
    for bid in response.orders {
        if bid.name != item.market_hash_name
            || bid.game != game
            || !bid.filters.is_null()
            || bid.amount <= bid.filled
            || bid.by.is_empty()
        {
            continue;
        }
        let price = bid.price.cents(false)?;
        if price.0 > 0 && owners.insert(bid.by) {
            *depth.entry(price).or_default() += 1;
        }
    }
    Ok(depth
        .into_iter()
        .rev()
        .map(|(price, quantity)| BuyOrder {
            platform: Platform::Waxpeer,
            item: item.clone(),
            float_bucket: None,
            price,
            quantity,
            observed_at: at,
        })
        .collect())
}

type Listings = Arc<HashMap<String, Vec<Ask>>>;
type BidsByName = Arc<HashMap<String, Money>>;

#[derive(Deserialize)]
#[serde(untagged)]
enum Minor {
    Integer(i64),
    String(String),
}

impl Minor {
    fn cents(&self, ask: bool) -> Result<Money> {
        let minor = match self {
            Self::Integer(value) => *value,
            Self::String(value) => value
                .parse()
                .map_err(|_| Error::InvalidData("invalid Waxpeer price integer".into()))?,
        };
        if minor < 0 {
            return Err(Error::InvalidData("negative Waxpeer price".into()));
        }
        cents_from_i64(minor / 10 + i64::from(ask && minor % 10 != 0))
    }
}

#[derive(Deserialize)]
struct Ask {
    name: String,
    item_id: String,
    price: Minor,
    unlock_at: Option<DateTime<Utc>>,
}

#[derive(Deserialize)]
struct Bid {
    name: String,
    max: Minor,
}

#[derive(Deserialize)]
struct Asks {
    success: bool,
    timestamp: Option<i64>,
    #[serde(default)]
    items: Vec<Ask>,
}

#[derive(Deserialize)]
struct Bids {
    success: bool,
    timestamp: Option<i64>,
    #[serde(default)]
    offers: Vec<Bid>,
}

fn parse_asks(body: &[u8]) -> Result<(Listings, Option<i64>)> {
    let response: Asks = parse_json("Waxpeer listings", body)?;
    if !response.success {
        return Err(Error::InvalidData("Waxpeer listings unavailable".into()));
    }
    let mut asks = HashMap::<String, Vec<Ask>>::new();
    for mut row in response.items {
        let name = std::mem::take(&mut row.name);
        asks.entry(name).or_default().push(row);
    }
    Ok((Arc::new(asks), response.timestamp))
}

fn parse_bids(body: &[u8]) -> Result<(BidsByName, Option<i64>)> {
    let response: Bids = parse_json("Waxpeer bids", body)?;
    if !response.success {
        return Err(Error::InvalidData("Waxpeer bids unavailable".into()));
    }
    let mut bids = HashMap::<_, Money>::new();
    for row in response.offers {
        let price = row.max.cents(false)?;
        if price.0 > 0 {
            bids.entry(row.name)
                .and_modify(|old| *old = (*old).max(price))
                .or_insert(price);
        }
    }
    Ok((Arc::new(bids), response.timestamp))
}

pub struct WaxpeerClient {
    fees: FeeSchedule,
    game: Game,
    http: HttpCore,
    listings: TtlCache<Listings>,
    bids: TtlCache<BidsByName>,
    fetch_lock: tokio::sync::Mutex<()>,
    detailed_buy_orders: Option<Option<String>>,
}

impl WaxpeerClient {
    pub fn new(
        fees: FeeSchedule,
        game: Game,
        gate: Arc<dyn RequestGate>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self> {
        Self::with_base_url(fees, game, "https://api.waxpeer.com", gate, clock)
    }

    pub fn with_base_url(
        fees: FeeSchedule,
        game: Game,
        base_url: &str,
        gate: Arc<dyn RequestGate>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self> {
        fees.validate()?;
        Ok(Self {
            fees,
            game,
            http: HttpCore::new(base_url, Some(Duration::from_secs(30)), gate, clock)?,
            listings: TtlCache::new("waxpeer-asks", Duration::from_secs(10), None),
            bids: TtlCache::new("waxpeer-bids", Duration::from_secs(10), None),
            fetch_lock: tokio::sync::Mutex::new(()),
            detailed_buy_orders: None,
        })
    }

    pub fn with_detailed_buy_orders(mut self, api_key: Option<String>) -> Self {
        self.detailed_buy_orders = Some(api_key.filter(|key| !key.trim().is_empty()));
        self
    }

    fn game_code(&self) -> &'static str {
        match self.game {
            Game::Cs2 => "csgo",
            Game::Rust => "rust",
        }
    }

    async fn fetch<T: Clone>(
        &self,
        endpoint: &Endpoint,
        cache: &TtlCache<T>,
        parse: impl FnOnce(&[u8]) -> Result<(T, Option<i64>)>,
    ) -> Result<Cached<T>> {
        let game = self.game_code();
        let key = format!("{}?{}", endpoint.path, query_string(&[("game", game)]));
        if let Some(hit) = cache.get_fresh(&key) {
            return Ok(hit);
        }
        let _guard = self.fetch_lock.lock().await;
        if let Some(hit) = cache.get_fresh(&key) {
            return Ok(hit);
        }
        let body = self.http.get(endpoint, &key, HeaderMap::new()).await?;
        let (value, timestamp) = parse(&body)?;
        let now = self.http.clock().now();
        let observed_at = match timestamp {
            Some(ts) => DateTime::from_timestamp(ts, 0)
                .ok_or_else(|| Error::InvalidData("invalid Waxpeer snapshot timestamp".into()))?
                .min(now),
            None => now,
        };
        Ok(cache.insert(&key, value, observed_at, Duration::ZERO))
    }
}

#[async_trait]
impl MarketClient for WaxpeerClient {
    async fn list_listings(&self, item: &ItemId) -> Result<Vec<Listing>> {
        validate_item_for_game(item, self.game)?;
        let asks = self.fetch(&PRICES, &self.listings, parse_asks).await?;
        let mut listings = Vec::new();
        for ask in asks.value.get(&item.market_hash_name).into_iter().flatten() {
            let price = ask.price.cents(true)?;
            if price.0 > 0 {
                listings.push(Listing {
                    id: format!("waxpeer:{}", ask.item_id),
                    platform: Platform::Waxpeer,
                    item: item.clone(),
                    price,
                    quantity: 1,
                    float_value: None,
                    observed_at: asks.fetched_at,
                    trade_locked_until: ask.unlock_at,
                });
            }
        }
        Ok(listings)
    }

    async fn recent_sales(&self, item: &ItemId, _since: DateTime<Utc>) -> Result<Vec<Sale>> {
        validate_item_for_game(item, self.game)?;
        Ok(Vec::new())
    }

    async fn price_snapshot(&self, item: &ItemId) -> Result<PriceSnapshot> {
        let now = self.http.clock().now();
        let ask = self
            .list_listings(item)
            .await?
            .into_iter()
            .filter(|listing| listing.trade_locked_until.is_none_or(|until| until <= now))
            .min_by_key(|listing| listing.price)
            .ok_or_else(|| {
                Error::InvalidData(format!("Waxpeer has no ask for {}", item.market_hash_name))
            })?;
        Ok(PriceSnapshot {
            platform: ask.platform,
            item: ask.item,
            price: ask.price,
            float_bucket: None,
            observed_at: ask.observed_at,
        })
    }

    async fn top_buy_orders(&self, item: &ItemId) -> Result<Vec<BuyOrder>> {
        validate_item_for_game(item, self.game)?;
        if let Some(api_key) = &self.detailed_buy_orders {
            let Some(api_key) = api_key else {
                return Ok(Vec::new());
            };
            let game = self.game_code();
            let query = query_string(&[
                ("api", api_key.as_str()),
                ("name", item.market_hash_name.as_str()),
                ("game", game),
            ]);
            let path = format!("{}?{query}", DETAILED_BUY_ORDERS.path);
            let body = self
                .http
                .get(&DETAILED_BUY_ORDERS, &path, HeaderMap::new())
                .await?;
            return parse_detailed_bids(&body, item, game, self.http.clock().now());
        }
        let bids = self.fetch(&BUY_ORDERS, &self.bids, parse_bids).await?;
        Ok(bids
            .value
            .get(&item.market_hash_name)
            .map(|&price| BuyOrder {
                platform: Platform::Waxpeer,
                item: item.clone(),
                price,
                quantity: 1,
                float_bucket: None,
                observed_at: bids.fetched_at,
            })
            .into_iter()
            .collect())
    }

    async fn fees(&self) -> Result<FeeSchedule> {
        Ok(self.fees)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detailed_depth_counts_one_funded_unit_per_distinct_unrestricted_owner() {
        let item = ItemId {
            market_hash_name: "Alien Red".into(),
            wear_tier: cs2_arb_core::WearTier::NotApplicable,
        };
        let body = br#"{"success":true,"orders":[
          {"name":"Alien Red","game":"rust","price":13500,"amount":5,"filled":0,"filters":{"paint_seeds":[1]},"by":"filtered"},
          {"name":"Alien Red","game":"rust","price":12349,"amount":5,"filled":1,"filters":null,"by":"one"},
          {"name":"Alien Red","game":"rust","price":12349,"amount":2,"filled":1,"filters":null,"by":"two"},
          {"name":"Alien Red","game":"rust","price":12000,"amount":4,"filled":1,"filters":null,"by":"one"}
        ]}"#;
        let bids = parse_detailed_bids(body, &item, "rust", DateTime::<Utc>::UNIX_EPOCH).unwrap();
        assert_eq!(bids.len(), 1);
        assert_eq!(bids[0].quantity, 2);
        assert_eq!(bids[0].price, Money(1234));
    }
}
