use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::{TimeDelta, Utc};
use cs2_arb_backtest::{integrated::MarketDepth, Secrets};
use cs2_arb_core::{
    Error, FeeSchedules, Game, ItemId, MarketClient, MarketEvent, Platform, PriceSnapshot, Result,
    SystemClock,
};
use cs2_arb_events::EventQueue;
use cs2_arb_markets::{
    CsFloatClient, CsFloatConfig, CsFloatCredentials, DMarketClient, DMarketConfig,
    DMarketCredentials, NetGate, RequestGate, ResilientMarket, SkinportClient, SkinportConfig,
    WaxpeerClient,
};
use cs2_arb_net::{NetConfig, TokioClock};
use cs2_arb_sim::wear::quote_bucket;

use crate::{bundled_snapshots, watch, Inputs, Market};

type MarketClients = Vec<(Platform, Box<dyn MarketClient>)>;

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
) -> Result<MarketClients> {
    let net = NetConfig::from_toml(source)?;
    let gate: Arc<dyn RequestGate> =
        Arc::new(NetGate(Arc::new(net.build_limiter(Arc::new(TokioClock))?)));
    let clock = Arc::new(SystemClock);
    let secrets = Secrets::from_env();
    let mut seen = HashSet::new();
    let mut clients: Vec<(Platform, Box<dyn MarketClient>)> = Vec::new();
    for platform in platforms.into_iter().filter(|p| seen.insert(*p)) {
        let schedule = fees.get(platform);
        let client: Box<dyn MarketClient> = match platform {
            Platform::Skinport => Box::new(ResilientMarket::new(
                SkinportClient::with_config(
                    schedule,
                    SkinportConfig {
                        game,
                        ..SkinportConfig::default()
                    },
                    gate.clone(),
                    clock.clone(),
                )?,
                net.retry,
            )),
            Platform::Waxpeer => Box::new(ResilientMarket::new(
                WaxpeerClient::new(schedule, game, gate.clone(), clock.clone())?,
                net.retry,
            )),
            Platform::CsFloat => {
                let credentials = secrets
                    .csfloat_api_key
                    .as_ref()
                    .map(|key| CsFloatCredentials::new(key.expose_secret()))
                    .transpose()?;
                Box::new(ResilientMarket::new(
                    CsFloatClient::with_config(
                        schedule,
                        CsFloatConfig::default(),
                        credentials,
                        gate.clone(),
                        clock.clone(),
                    )?,
                    net.retry,
                ))
            }
            Platform::DMarket => {
                let (public, secret) = secrets.require_dmarket()?;
                let credentials =
                    DMarketCredentials::from_hex(public.expose_secret(), secret.expose_secret())?;
                Box::new(ResilientMarket::new(
                    DMarketClient::with_config(
                        schedule,
                        DMarketConfig::for_game(game),
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

pub async fn collect(
    config: &str,
    fees: &FeeSchedules,
    platforms: Vec<Platform>,
    samples: u16,
    interval_seconds: u64,
    branch_updates: Option<(&str, Option<i64>)>,
    game: Game,
) -> Result<Inputs> {
    let assets = bundled_snapshots(game)?;
    let mut seen = HashSet::new();
    let items: Vec<ItemId> = assets
        .into_iter()
        .map(|quote| quote.item)
        .filter(|item| seen.insert(item.clone()))
        .collect();
    let clients = make_clients(config, fees, platforms, game)?;
    let mut events = Vec::new();
    let queue = if let Some((database, hours)) = branch_updates {
        let queue = EventQueue::open(database).await?;
        events = watch::recover_branch_updates(&queue, hours).await?;
        // Restore already-consumed opens after a process restart. Later closes come through the queue.
        for window in queue.open_windows_at(game.app_id(), Utc::now()).await? {
            events.push(MarketEvent::PanicWindowOpened {
                branch: window.branch,
                opened_at: window.opened_at,
            });
        }
        Some(queue)
    } else {
        None
    };
    let mut snapshots = Vec::new();
    let mut buy_orders = Vec::new();
    let mut depth = MarketDepth::default();
    for sample in 0..samples {
        if sample > 0 {
            tokio::time::sleep(Duration::from_secs(interval_seconds)).await;
        }
        let since = Utc::now() - TimeDelta::days(7);
        let mut books = Vec::new();
        for (platform, client) in &clients {
            for item in &items {
                let listings = match client.list_listings(item).await {
                    Ok(listings) => listings,
                    Err(error) => {
                        tracing::warn!(?platform, %error, "read-only listing request skipped");
                        continue;
                    }
                };
                match client.recent_sales(item, since).await {
                    Ok(sales) => depth.add_sales(sales),
                    Err(error) => {
                        tracing::warn!(?platform, %error, "read-only sales request skipped")
                    }
                }
                if matches!(
                    platform,
                    Platform::CsFloat | Platform::Skinport | Platform::Waxpeer
                ) {
                    match client.top_buy_orders(item).await {
                        Ok(orders) => buy_orders.extend(orders),
                        Err(error) => {
                            tracing::warn!(?platform, %error, "read-only buy-orders request skipped")
                        }
                    }
                }
                books.push((*platform, item.clone(), listings));
            }
        }
        let at = Utc::now();
        let max_age = cs2_arb_sim::SimConfig::default().max_listing_age;
        for (platform, item, listings) in books {
            let mut best = BTreeMap::new();
            for listing in &listings {
                if listing.price.0 <= 0
                    || listing.quantity == 0
                    || listing.trade_locked_until.is_some_and(|until| until > at)
                    || at - listing.observed_at > max_age
                {
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
                            observed_at: at,
                        });
                }
            }
            snapshots.extend(best.into_values());
            depth.replace_listings(at, platform, item, listings);
        }
        if let Some(queue) = &queue {
            watch::drain(queue, &mut events).await?;
        }
    }
    if snapshots.is_empty() {
        return Err(Error::InvalidData(
            "no usable quotes from the selected read-only markets".into(),
        ));
    }
    Ok(Inputs {
        snapshots,
        events,
        depth,
        buy_orders,
    })
}
