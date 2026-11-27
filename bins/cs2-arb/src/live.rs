use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use cs2_arb_backtest::{integrated::MarketDepth, Secrets};
use cs2_arb_core::{
    BuyOrder, Error, FeeSchedules, Game, ItemId, Listing, MarketClient, MarketEvent, Platform,
    PriceSnapshot, Result, SystemClock,
};
use cs2_arb_events::queue::MarketHistory;
use cs2_arb_events::EventQueue;
use cs2_arb_markets::{
    CsFloatClient, CsFloatConfig, CsFloatCredentials, DMarketClient, DMarketConfig,
    DMarketCredentials, NetGate, RequestGate, ResilientMarket, SkinportClient, SkinportConfig,
    WaxpeerClient,
};
use cs2_arb_net::{NetConfig, RateLimit, RateLimiter, TokioClock, DEFAULT_ENDPOINT};
use cs2_arb_sim::wear::quote_bucket;

use crate::peg_runner::BidRefresh;
use crate::{bundled_snapshots, watch, Inputs, Market};

type MarketClients = Vec<(Platform, Box<dyn MarketClient>)>;

pub struct MarketSources<'a> {
    pub secrets: &'a Secrets,
    pub base_url_override: Option<&'a str>,
}

impl MarketSources<'_> {
    fn override_base_url(&self, base_url: &mut String) {
        if let Some(url) = self.base_url_override {
            *base_url = url.into();
        }
    }
}

pub fn dmarket_credentials(secrets: &Secrets) -> Result<DMarketCredentials> {
    let (public, secret) = secrets.require_dmarket()?;
    DMarketCredentials::from_hex(public.expose_secret(), secret.expose_secret())
}

pub fn select_platforms(
    game: Game,
    requested: Vec<Market>,
    allowlist: &[Platform],
    explicit_allowlist: bool,
) -> Result<Vec<Platform>> {
    let platforms = if requested.is_empty() {
        if explicit_allowlist {
            allowlist.to_vec()
        } else {
            match game {
                Game::Cs2 => vec![Platform::Skinport, Platform::CsFloat],
                Game::Rust => vec![Platform::Skinport, Platform::Waxpeer],
            }
        }
    } else {
        requested.into_iter().map(Platform::from).collect()
    };
    for &platform in &platforms {
        if !game.supports_market(platform) {
            return Err(Error::InvalidConfig(format!(
                "{platform:?} is not supported for {game:?}"
            )));
        }
        if !allowlist.contains(&platform) {
            return Err(Error::InvalidConfig(format!(
                "{platform:?} is not enabled in markets"
            )));
        }
    }
    Ok(platforms)
}

fn make_clients(
    source: &str,
    fees: &FeeSchedules,
    platforms: Vec<Platform>,
    game: Game,
    instant_sale: bool,
    waxpeer_api_key: Option<String>,
    sources: &MarketSources<'_>,
) -> Result<MarketClients> {
    let net = NetConfig::from_toml(source)?;
    let gate: Arc<dyn RequestGate> = Arc::new(NetGate(Arc::new(build_limiter(&net, &platforms)?)));
    let clock = Arc::new(SystemClock);
    let mut seen = HashSet::new();
    let mut clients: Vec<(Platform, Box<dyn MarketClient>)> = Vec::new();
    for platform in platforms.into_iter().filter(|p| seen.insert(*p)) {
        let schedule = fees.get(platform);
        let client: Box<dyn MarketClient> = match platform {
            Platform::Skinport => {
                let mut config = SkinportConfig {
                    game,
                    tradable_only: true,
                    ..SkinportConfig::default()
                };
                sources.override_base_url(&mut config.base_url);
                Box::new(ResilientMarket::new(
                    SkinportClient::with_config(schedule, config, gate.clone(), clock.clone())?,
                    net.retry,
                ))
            }
            Platform::Waxpeer => {
                let mut client = if let Some(url) = sources.base_url_override {
                    WaxpeerClient::with_base_url(schedule, game, url, gate.clone(), clock.clone())?
                } else {
                    WaxpeerClient::new(schedule, game, gate.clone(), clock.clone())?
                };
                if instant_sale {
                    client = client.with_detailed_buy_orders(waxpeer_api_key.clone());
                }
                Box::new(ResilientMarket::new(client, net.retry))
            }
            Platform::CsFloat => {
                let credentials = sources
                    .secrets
                    .csfloat_api_key
                    .as_ref()
                    .map(|key| CsFloatCredentials::new(key.expose_secret()))
                    .transpose()?;
                let mut config = CsFloatConfig::default();
                sources.override_base_url(&mut config.base_url);
                Box::new(ResilientMarket::new(
                    CsFloatClient::with_config(
                        schedule,
                        config,
                        credentials,
                        gate.clone(),
                        clock.clone(),
                    )?,
                    net.retry,
                ))
            }
            Platform::DMarket => {
                let credentials = dmarket_credentials(sources.secrets)?;
                let mut config = DMarketConfig::for_game(game);
                sources.override_base_url(&mut config.base_url);
                Box::new(ResilientMarket::new(
                    DMarketClient::with_config(
                        schedule,
                        config,
                        credentials,
                        gate.clone(),
                        clock.clone(),
                    )?,
                    net.retry,
                ))
            }
        };
        clients.push((platform, client));
    }
    Ok(clients)
}

fn build_limiter(net: &NetConfig, platforms: &[Platform]) -> Result<RateLimiter> {
    let mut limiter = net.build_limiter(Arc::new(TokioClock))?;
    if platforms.contains(&Platform::Waxpeer)
        && !net.endpoints.iter().any(|entry| {
            entry.platform == Platform::Waxpeer
                && (entry.name == DEFAULT_ENDPOINT
                    || entry.path.as_deref() == Some(DEFAULT_ENDPOINT))
        })
    {
        limiter.register(
            Platform::Waxpeer,
            DEFAULT_ENDPOINT,
            RateLimit::new(1, Duration::from_secs(6))?,
        )?;
    }
    Ok(limiter)
}

#[allow(clippy::too_many_arguments)]
pub async fn collect(
    config: &str,
    fees: &FeeSchedules,
    platforms: Vec<Platform>,
    samples: u16,
    interval_seconds: u64,
    database: &str,
    branch_update_window_hours: Option<i64>,
    game: Game,
    instant_sale: bool,
    sales_lookback_days: i64,
    max_listing_age: TimeDelta,
    history_retention: TimeDelta,
    sources: &MarketSources<'_>,
) -> Result<(Inputs, MarketHistory)> {
    let sales_lookback = TimeDelta::try_days(sales_lookback_days)
        .ok_or_else(|| Error::InvalidConfig("live sales lookback is out of range".into()))?;
    let assets = bundled_snapshots(game)?;
    let mut seen = HashSet::new();
    let items: Vec<ItemId> = assets
        .into_iter()
        .map(|quote| quote.item)
        .filter(|item| seen.insert(item.clone()))
        .collect();
    let waxpeer_api_key = instant_sale
        .then(|| std::env::var("WAXPEER_API_KEY").ok())
        .flatten();
    let waxpeer_has_key = waxpeer_api_key.is_some();
    let clients = make_clients(
        config,
        fees,
        platforms,
        game,
        instant_sale,
        waxpeer_api_key,
        sources,
    )?;
    let queue = EventQueue::open(database).await?;
    let mut events = Vec::new();
    if game == Game::Cs2 {
        events = watch::recover_branch_updates(&queue, branch_update_window_hours).await?;
        // Restore already-consumed opens after a process restart. Later closes come through the queue.
        for window in queue.open_windows_at(game.app_id(), Utc::now()).await? {
            events.push(MarketEvent::PanicWindowOpened {
                branch: window.branch,
                opened_at: window.opened_at,
            });
        }
    }
    let mut snapshots = Vec::new();
    let mut regime_quotes = Vec::new();
    let mut bid_refreshes = Vec::new();
    let mut depth = MarketDepth::default();
    let started_at = Utc::now();
    for sample in 0..samples {
        if sample > 0 {
            tokio::time::sleep(Duration::from_secs(interval_seconds)).await;
        }
        let since = Utc::now()
            .checked_sub_signed(sales_lookback)
            .ok_or_else(|| Error::InvalidConfig("live sales cutoff is out of range".into()))?;
        let mut books = Vec::new();
        for (platform, client) in &clients {
            for item in &items {
                let listings = match client.list_listings(item).await {
                    Ok(listings) => listings,
                    Err(Error::RateLimited { .. }) => continue,
                    Err(error) => {
                        tracing::warn!(?platform, %error, "read-only listing request skipped");
                        continue;
                    }
                };
                match client.recent_sales(item, since).await {
                    Ok(sales) => depth.add_sales(sales),
                    Err(Error::RateLimited { .. }) => {}
                    Err(error) => {
                        tracing::warn!(?platform, %error, "read-only sales request skipped")
                    }
                }
                if polls_bids(*platform, instant_sale, waxpeer_has_key) {
                    match client.poll_buy_orders(item).await {
                        Ok((orders, observed_at)) => {
                            record_bid_poll(
                                &mut bid_refreshes,
                                *platform,
                                item,
                                orders,
                                observed_at.unwrap_or_else(Utc::now),
                            );
                        }
                        Err(Error::RateLimited { .. }) => {}
                        Err(error) => {
                            tracing::warn!(?platform, %error, "read-only buy-orders request skipped")
                        }
                    }
                }
                books.push((*platform, item.clone(), listings));
            }
        }
        let at = Utc::now();
        for (platform, item, listings) in books {
            let mut best = BTreeMap::new();
            for listing in &listings {
                if !quotable(listing, at, max_listing_age) {
                    continue;
                }
                if let Ok(bucket) = quote_bucket(item.wear_tier, listing.float_value) {
                    best.entry(bucket.map(|bucket| bucket as u8))
                        .and_modify(|quote: &mut PriceSnapshot| {
                            quote.price = quote.price.min(listing.price);
                        })
                        .or_insert_with(|| PriceSnapshot {
                            platform,
                            item: item.clone(),
                            float_bucket: bucket,
                            price: listing.price,
                            observed_at: if platform == Platform::Skinport {
                                listing.observed_at
                            } else {
                                at
                            },
                        });
                }
            }
            for mut quote in best.into_values() {
                regime_quotes.push(quote.clone());
                quote.observed_at = at;
                snapshots.push(quote);
            }
            depth.replace_listings(at, platform, item, listings);
        }
        if game == Game::Cs2 {
            watch::drain(&queue, &mut events).await?;
        }
    }
    let retain_since = started_at - history_retention;
    queue
        .record_market_history(
            game,
            &regime_quotes,
            depth.sales(),
            &bid_refreshes,
            retain_since,
        )
        .await?;
    if snapshots.is_empty() {
        return Err(Error::InvalidData(
            "no usable quotes from the selected read-only markets".into(),
        ));
    }
    let mut history = queue
        .load_market_history(game, retain_since, Utc::now())
        .await?;
    let in_scope = |platform, item: &ItemId| {
        seen.contains(item) && clients.iter().any(|(active, _)| *active == platform)
    };
    history
        .snapshots
        .retain(|quote| in_scope(quote.platform, &quote.item));
    history
        .sales
        .retain(|sale| in_scope(sale.platform, &sale.item));
    Ok((
        Inputs {
            snapshots,
            events,
            depth,
            buy_orders: Vec::new(),
            bid_refreshes,
            has_observed_bids: true,
        },
        history,
    ))
}

fn record_bid_poll(
    refreshes: &mut Vec<BidRefresh>,
    platform: Platform,
    item: &ItemId,
    bids: Vec<BuyOrder>,
    at: DateTime<Utc>,
) {
    refreshes.push(BidRefresh {
        at,
        platform,
        item: item.clone(),
        bids,
    });
}

fn polls_bids(platform: Platform, instant_sale: bool, waxpeer_has_key: bool) -> bool {
    match platform {
        Platform::CsFloat => true,
        Platform::DMarket => instant_sale,
        Platform::Waxpeer => !instant_sale || waxpeer_has_key,
        Platform::Skinport => false,
    }
}

fn quotable(listing: &Listing, at: DateTime<Utc>, max_age: TimeDelta) -> bool {
    listing.price.0 > 0
        && listing.quantity > 0
        && listing.trade_locked_until.is_none_or(|until| until <= at)
        && at - listing.observed_at <= max_age
}

#[cfg(test)]
mod tests {
    #[test]
    fn live_bid_poll_publishes_one_atomic_tick_and_empty_poll_clears() {
        let at = Utc::now();
        let later = at + TimeDelta::seconds(1);
        let completed_at = later + TimeDelta::milliseconds(1);
        let platform = Platform::Waxpeer;
        let item = ItemId {
            market_hash_name: "Alien Red".into(),
            wear_tier: cs2_arb_core::WearTier::NotApplicable,
        };
        let bid = |observed_at| BuyOrder {
            platform,
            item: item.clone(),
            float_bucket: None,
            price: cs2_arb_core::Money(100),
            quantity: 1,
            observed_at,
        };
        let mut refreshes = Vec::new();
        record_bid_poll(
            &mut refreshes,
            platform,
            &item,
            vec![bid(later), bid(at)],
            later,
        );
        record_bid_poll(&mut refreshes, platform, &item, Vec::new(), completed_at);
        assert_eq!(
            refreshes
                .iter()
                .map(|refresh| (refresh.at, refresh.bids.len()))
                .collect::<Vec<_>>(),
            [(later, 2), (completed_at, 0)]
        );
        assert_eq!(refreshes[0].bids[0].observed_at, refreshes[0].at);
        assert_eq!(refreshes[0].bids[1].observed_at, at);
    }

    #[test]
    fn unsupported_bid_polls_do_not_create_refreshes() {
        assert!(!polls_bids(Platform::Skinport, true, true));
        assert!(!polls_bids(Platform::Waxpeer, true, false));
        assert!(polls_bids(Platform::Waxpeer, true, true));
        assert!(polls_bids(Platform::Waxpeer, false, false));
        assert!(polls_bids(Platform::DMarket, true, false));
    }
    use super::*;

    #[test]
    fn live_quote_selection_respects_listing_age_overrides() {
        let now = Utc::now();
        let listing = Listing {
            id: "listing".into(),
            platform: Platform::Waxpeer,
            item: ItemId {
                market_hash_name: "Alien Red".into(),
                wear_tier: cs2_arb_core::WearTier::NotApplicable,
            },
            price: cs2_arb_core::Money(100),
            quantity: 1,
            float_value: None,
            observed_at: now - TimeDelta::minutes(15),
            trade_locked_until: None,
        };
        assert!(quotable(&listing, now, TimeDelta::minutes(20)));
        assert!(!quotable(&listing, now, TimeDelta::minutes(10)));
    }

    #[test]
    fn waxpeer_live_reads_get_a_limit_even_without_rate_limit_config() {
        let net = NetConfig::from_toml("markets = ['waxpeer']").unwrap();
        let limiter = build_limiter(&net, &[Platform::Waxpeer]).unwrap();
        for endpoint in [
            cs2_arb_markets::waxpeer::PRICES,
            cs2_arb_markets::waxpeer::BUY_ORDERS,
        ] {
            assert!(limiter.resolve(endpoint.platform, endpoint.path).is_ok());
        }
    }
}
