//! Post-trade-hold auto-relisting engine (mock simulation only).
//!
//! Periodically monitors simulated inventory:
//! 1. Items whose `locked_until` is still in the future stay `LOCKED` with an
//!    exact remaining lock countdown.
//! 2. Once `now >= locked_until`, the item transitions to `READY_TO_SELL`.
//! 3. `READY_TO_SELL` items are simulated as ask listings priced at the rolling
//!    median market price.
//! 4. Listed asks fill based on observed market volume, crediting proceeds net
//!    of platform seller fees and recording realized net P&L.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use cs2_arb_core::{
    Error, FeeSchedules, FloatBucket, ItemId, Money, Order, OrderSide, Platform, Position,
    PriceSnapshot, Result, Sale, TradeFees,
};
use serde::{Deserialize, Serialize};

use crate::peg_bid::median_cents;
use crate::{order_id, platform_slug};

/// Tunables for [`RelistEngine`]. Deserializable from TOML (all fields optional).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RelistConfig {
    /// Rolling window in days for computing the median ask price. Default 7.
    pub lookback_days: i64,
    /// Minimum price observations required to compute the rolling median. Default 1.
    pub min_median_samples: usize,
    /// Trailing window in days used to evaluate ask-fill volume. Default 7.
    pub volume_window_days: i64,
    /// Minimum sales in the trailing volume window required for a simulated ask
    /// listing at median price to fill. Default 15.
    pub min_sales_for_ask_fill: u32,
    /// Minimum time (seconds) a simulated ask must rest before filling on volume
    /// alone (unless a verified sale prints at or above the ask earlier). Default 3600.
    pub min_listing_age_secs: i64,
    /// Count only `sale.verified` transactions when measuring volume and median. Default true.
    pub require_verified_sales: bool,
    /// Consider additional venues that have enough observed sales to fill a mock ask.
    pub cross_venue: bool,
}

impl Default for RelistConfig {
    fn default() -> Self {
        Self {
            lookback_days: 7,
            min_median_samples: 1,
            volume_window_days: 7,
            min_sales_for_ask_fill: 15,
            min_listing_age_secs: 3600,
            require_verified_sales: true,
            cross_venue: false,
        }
    }
}

impl RelistConfig {
    pub fn validate(&self) -> Result<()> {
        let bad = |msg: &str| Err(Error::InvalidConfig(format!("relist: {msg}")));
        if self.lookback_days <= 0 {
            return bad("lookback_days must be > 0");
        }
        if self.min_median_samples == 0 {
            return bad("min_median_samples must be >= 1");
        }
        if self.volume_window_days <= 0 {
            return bad("volume_window_days must be > 0");
        }
        if self.min_listing_age_secs < 0 {
            return bad("min_listing_age_secs must be >= 0");
        }
        Ok(())
    }
}

/// Lifecycle state of an inventory position managed by [`RelistEngine`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InventoryStatus {
    #[serde(rename = "LOCKED")]
    Locked { locked_until: DateTime<Utc> },
    #[serde(rename = "READY_TO_SELL")]
    ReadyToSell,
    #[serde(rename = "LISTED")]
    Listed {
        order_id: String,
        ask_price: Money,
        listed_at: DateTime<Utc>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sell_platform: Option<Platform>,
    },
    #[serde(rename = "SOLD")]
    Sold {
        sold_at: DateTime<Utc>,
        ask_price: Money,
        net_proceeds: Money,
        realized_net_pnl: Money,
    },
}

/// Inventory item tracked across trade lock, relisting, and simulated ask fill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackedInventoryItem {
    pub position: Position,
    pub status: InventoryStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    entry: Option<(Money, Money)>,
}

impl TrackedInventoryItem {
    /// Remaining lock duration at `now` (zero once unlocked).
    pub fn remaining_lock(&self, now: DateTime<Utc>) -> Duration {
        if now < self.position.locked_until {
            self.position.locked_until - now
        } else {
            Duration::zero()
        }
    }
}

/// Record of a completed mock ask fill from [`RelistEngine`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RealizedRelistSale {
    pub position_id: String,
    pub order_id: String,
    pub item: ItemId,
    pub float_bucket: Option<FloatBucket>,
    pub platform: Platform,
    pub quantity: u32,
    pub cost_basis: Money,
    pub ask_price: Money,
    pub gross_proceeds: Money,
    pub seller_fee: Money,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fee_breakdown: Option<TradeFees>,
    pub net_proceeds: Money,
    pub realized_net_pnl: Money,
    pub listed_at: DateTime<Utc>,
    pub sold_at: DateTime<Utc>,
    pub trailing_volume: u32,
}

/// Outputs emitted by one monitoring tick of [`RelistEngine::step`].
#[derive(Debug, Clone, Default)]
pub struct RelistStepResult {
    /// Position IDs that transitioned from `LOCKED` to `READY_TO_SELL` this tick.
    pub unlocked: Vec<String>,
    /// Simulated sell `Order`s placed at rolling median market price this tick.
    pub listed_orders: Vec<Order>,
    /// Simulated ask fills settled this tick with net proceeds and realized P&L.
    pub realized_sales: Vec<RealizedRelistSale>,
}

/// Automated post-trade-hold relisting engine.
#[derive(Debug, Clone)]
pub struct RelistEngine {
    config: RelistConfig,
    items: HashMap<String, TrackedInventoryItem>,
    realized_sales: Vec<RealizedRelistSale>,
    total_realized_pnl: Money,
    total_net_proceeds: Money,
    total_seller_fees: Money,
}

impl RelistEngine {
    pub fn new(config: RelistConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            items: HashMap::new(),
            realized_sales: Vec::new(),
            total_realized_pnl: Money(0),
            total_net_proceeds: Money(0),
            total_seller_fees: Money(0),
        })
    }

    pub fn config(&self) -> &RelistConfig {
        &self.config
    }

    /// Register a newly filled simulated position. Starts `LOCKED` if `now < locked_until`,
    /// otherwise `READY_TO_SELL`.
    pub fn track_position(&mut self, position: Position, now: DateTime<Utc>) {
        self.track(position, None, now);
    }

    pub fn track_filled_position(&mut self, position: Position, fill: &cs2_arb_core::Fill) {
        self.track(position, Some((fill.unit_price, fill.fee)), fill.filled_at);
    }

    fn track(&mut self, position: Position, entry: Option<(Money, Money)>, now: DateTime<Utc>) {
        if self.items.contains_key(&position.id) {
            return;
        }
        let status = if position.is_trade_locked_at(now) {
            InventoryStatus::Locked {
                locked_until: position.locked_until,
            }
        } else {
            InventoryStatus::ReadyToSell
        };
        self.items.insert(
            position.id.clone(),
            TrackedInventoryItem {
                position,
                status,
                entry,
            },
        );
    }

    /// Sync against the simulator's open positions and transition any expired trade locks
    /// to `READY_TO_SELL`.
    pub fn sync_inventory(&mut self, positions: &[Position], now: DateTime<Utc>) -> Vec<String> {
        for pos in positions {
            self.track_position(pos.clone(), now);
        }
        self.transition_expired_locks(now)
    }

    /// Transition any `LOCKED` items whose `locked_until <= now` to `READY_TO_SELL`.
    pub fn transition_expired_locks(&mut self, now: DateTime<Utc>) -> Vec<String> {
        let mut unlocked = Vec::new();
        for item in self.items.values_mut() {
            if matches!(item.status, InventoryStatus::Locked { .. })
                && !item.position.is_trade_locked_at(now)
            {
                item.status = InventoryStatus::ReadyToSell;
                unlocked.push(item.position.id.clone());
            }
        }
        unlocked.sort();
        unlocked
    }

    /// Run one periodic inventory-monitoring tick:
    /// 1. Transition expired trade locks to `READY_TO_SELL`.
    /// 2. Place simulated ask listings at rolling median market price for `READY_TO_SELL` items.
    /// 3. Simulate volume-based ask fills for `LISTED` items, crediting net proceeds and
    ///    recording realized net P&L.
    pub fn step(
        &mut self,
        sales: &[Sale],
        snapshots: &[PriceSnapshot],
        fees: &FeeSchedules,
        now: DateTime<Utc>,
    ) -> Result<RelistStepResult> {
        let mut result = RelistStepResult {
            unlocked: self.transition_expired_locks(now),
            ..RelistStepResult::default()
        };

        let mut ids: Vec<String> = self.items.keys().cloned().collect();
        ids.sort();

        for id in ids {
            let status = self.items[&id].status.clone();
            match status {
                InventoryStatus::ReadyToSell => {
                    let pos = self.items[&id].position.clone();
                    if pos.is_trade_locked_at(now) {
                        continue;
                    }
                    let mut best = None;
                    for platform in std::iter::once(pos.acquired_on).chain(
                        Platform::ALL
                            .into_iter()
                            .filter(|p| self.config.cross_venue && *p != pos.acquired_on),
                    ) {
                        if platform != pos.acquired_on
                            && self.trailing_volume(
                                platform,
                                &pos.item,
                                pos.float_bucket,
                                sales,
                                now,
                            ) < self.config.min_sales_for_ask_fill.max(1)
                        {
                            continue;
                        }
                        let Some(ask) = self.rolling_median_price(
                            platform,
                            &pos.item,
                            pos.float_bucket,
                            sales,
                            snapshots,
                            now,
                        ) else {
                            continue;
                        };
                        if ask.0 <= 0 {
                            continue;
                        }
                        let net = fees
                            .get(platform)
                            .seller_proceeds(ask)?
                            .checked_mul(pos.quantity)?;
                        if (fees.get(pos.acquired_on).funding.enabled()
                            || fees.get(platform).funding.enabled())
                            && net <= pos.cost_basis
                        {
                            continue;
                        }
                        if best.is_none_or(|(_, _, best_net)| net > best_net) {
                            best = Some((platform, ask, net));
                        }
                    }
                    let Some((platform, ask_price, _)) = best else {
                        continue;
                    };
                    let oid = order_id(
                        "relist-ask",
                        &[platform_slug(platform), &pos.item.market_hash_name],
                        pos.float_bucket,
                        now,
                    );
                    let order = Order {
                        id: oid.clone(),
                        side: OrderSide::Sell,
                        platform,
                        item: pos.item.clone(),
                        float_bucket: pos.float_bucket,
                        quantity: pos.quantity,
                        limit_price: ask_price,
                        submitted_at: now,
                    };
                    if let Some(tracked) = self.items.get_mut(&id) {
                        tracked.status = InventoryStatus::Listed {
                            order_id: oid,
                            ask_price,
                            listed_at: now,
                            sell_platform: (platform != pos.acquired_on).then_some(platform),
                        };
                    }
                    result.listed_orders.push(order);
                }
                InventoryStatus::Listed {
                    order_id: oid,
                    ask_price,
                    listed_at,
                    sell_platform,
                } => {
                    let pos = self.items[&id].position.clone();
                    if pos.is_trade_locked_at(now) {
                        continue;
                    }
                    let platform = sell_platform.unwrap_or(pos.acquired_on);
                    let trailing_vol =
                        self.trailing_volume(platform, &pos.item, pos.float_bucket, sales, now);
                    let has_crossing_print = sales.iter().any(|s| {
                        (!self.config.require_verified_sales || s.verified)
                            && s.platform == platform
                            && s.item == pos.item
                            && s.sold_at > listed_at
                            && s.sold_at <= now
                            && s.price >= ask_price
                            && matches_bucket(pos.float_bucket, s.float_value)
                    });
                    let listing_age_ok =
                        (now - listed_at).num_seconds() >= self.config.min_listing_age_secs;
                    let volume_fill_ok =
                        trailing_vol >= self.config.min_sales_for_ask_fill && listing_age_ok;

                    if !has_crossing_print && !volume_fill_ok {
                        continue;
                    }

                    let schedule = fees.get(platform);
                    let gross_proceeds = ask_price.checked_mul(pos.quantity)?;
                    let unit_seller_fee = schedule.seller_fee(ask_price)?;
                    let seller_fee = unit_seller_fee.checked_mul(pos.quantity)?;
                    let net_proceeds = gross_proceeds.checked_sub(seller_fee)?;
                    if net_proceeds.0 <= 0 {
                        continue;
                    }
                    let realized_net_pnl = net_proceeds.checked_sub(pos.cost_basis)?;
                    if (fees.get(pos.acquired_on).funding.enabled() || schedule.funding.enabled())
                        && realized_net_pnl.0 <= 0
                    {
                        continue;
                    }
                    let (entry_schedule, entry_unit, entry_fee) = self.items[&id]
                        .entry
                        .map_or((Default::default(), Money(0), Money(0)), |(unit, fee)| {
                            (fees.get(pos.acquired_on), unit, fee)
                        });
                    let fee_breakdown = TradeFees::from_trade(
                        entry_schedule,
                        schedule,
                        entry_fee,
                        seller_fee,
                        entry_unit,
                        ask_price,
                        pos.quantity,
                    )?;

                    let record = RealizedRelistSale {
                        position_id: pos.id.clone(),
                        order_id: oid,
                        item: pos.item.clone(),
                        float_bucket: pos.float_bucket,
                        platform,
                        quantity: pos.quantity,
                        cost_basis: pos.cost_basis,
                        ask_price,
                        gross_proceeds,
                        seller_fee,
                        fee_breakdown,
                        net_proceeds,
                        realized_net_pnl,
                        listed_at,
                        sold_at: now,
                        trailing_volume: trailing_vol,
                    };

                    self.total_realized_pnl =
                        self.total_realized_pnl.checked_add(realized_net_pnl)?;
                    self.total_net_proceeds = self.total_net_proceeds.checked_add(net_proceeds)?;
                    self.total_seller_fees = self.total_seller_fees.checked_add(seller_fee)?;
                    self.realized_sales.push(record.clone());
                    if let Some(tracked) = self.items.get_mut(&id) {
                        tracked.status = InventoryStatus::Sold {
                            sold_at: now,
                            ask_price,
                            net_proceeds,
                            realized_net_pnl,
                        };
                    }
                    result.realized_sales.push(record);
                }
                InventoryStatus::Locked { .. } | InventoryStatus::Sold { .. } => {}
            }
        }

        Ok(result)
    }

    pub fn get(&self, position_id: &str) -> Option<&TrackedInventoryItem> {
        self.items.get(position_id)
    }

    /// Locked inventory positions with their remaining lock duration at `now`.
    pub fn locked_inventory(&self, now: DateTime<Utc>) -> Vec<(&Position, Duration)> {
        let mut locked: Vec<(&Position, Duration)> = self
            .items
            .values()
            .filter(|item| {
                matches!(item.status, InventoryStatus::Locked { .. })
                    && item.position.is_trade_locked_at(now)
            })
            .map(|item| (&item.position, item.remaining_lock(now)))
            .collect();
        locked.sort_by_key(|(pos, _)| (pos.locked_until, pos.id.clone()));
        locked
    }

    /// Sorted unique `locked_until` timestamps strictly before `now` for items
    /// still in [`InventoryStatus::Locked`].
    pub fn pending_unlocks_before(&self, now: DateTime<Utc>) -> Vec<DateTime<Utc>> {
        let mut times: std::collections::BTreeSet<DateTime<Utc>> =
            std::collections::BTreeSet::new();
        for item in self.items.values() {
            if let InventoryStatus::Locked { locked_until } = item.status {
                if locked_until < now {
                    times.insert(locked_until);
                }
            }
        }
        times.into_iter().collect()
    }

    pub fn ready_to_sell(&self) -> Vec<&Position> {
        let mut ready: Vec<&Position> = self
            .items
            .values()
            .filter(|item| matches!(item.status, InventoryStatus::ReadyToSell))
            .map(|item| &item.position)
            .collect();
        ready.sort_by(|a, b| a.id.cmp(&b.id));
        ready
    }

    pub fn active_asks(&self) -> Vec<(&Position, Money, DateTime<Utc>)> {
        let mut asks: Vec<(&Position, Money, DateTime<Utc>)> = self
            .items
            .values()
            .filter_map(|item| match item.status {
                InventoryStatus::Listed {
                    ask_price,
                    listed_at,
                    ..
                } => Some((&item.position, ask_price, listed_at)),
                _ => None,
            })
            .collect();
        asks.sort_by(|a, b| a.0.id.cmp(&b.0.id));
        asks
    }

    pub fn realized_sales(&self) -> &[RealizedRelistSale] {
        &self.realized_sales
    }

    pub fn total_realized_pnl(&self) -> Money {
        self.total_realized_pnl
    }

    pub fn total_net_proceeds(&self) -> Money {
        self.total_net_proceeds
    }

    pub fn total_seller_fees(&self) -> Money {
        self.total_seller_fees
    }

    fn rolling_median_price(
        &self,
        platform: Platform,
        item: &ItemId,
        bucket: Option<FloatBucket>,
        sales: &[Sale],
        snapshots: &[PriceSnapshot],
        now: DateTime<Utc>,
    ) -> Option<Money> {
        let cutoff = now - Duration::days(self.config.lookback_days);
        let mut seen_ids = std::collections::HashSet::new();
        let mut prices: Vec<i64> = sales
            .iter()
            .filter(|s| {
                (!self.config.require_verified_sales || s.verified)
                    && s.platform == platform
                    && &s.item == item
                    && s.sold_at >= cutoff
                    && s.sold_at <= now
                    && s.price.0 > 0
                    && matches_bucket(bucket, s.float_value)
                    && seen_ids.insert((s.platform, s.id.as_str()))
            })
            .map(|s| s.price.0)
            .collect();

        if prices.len() < self.config.min_median_samples {
            prices.extend(
                snapshots
                    .iter()
                    .filter(|q| {
                        q.platform == platform
                            && &q.item == item
                            && q.observed_at >= cutoff
                            && q.observed_at <= now
                            && q.price.0 > 0
                            && (bucket.is_none() || q.float_bucket == bucket)
                    })
                    .map(|q| q.price.0),
            );
        }

        if prices.len() < self.config.min_median_samples {
            return None;
        }
        Some(median_cents(&mut prices))
    }

    fn trailing_volume(
        &self,
        platform: Platform,
        item: &ItemId,
        bucket: Option<FloatBucket>,
        sales: &[Sale],
        now: DateTime<Utc>,
    ) -> u32 {
        let cutoff = now - Duration::days(self.config.volume_window_days);
        let mut seen_ids = std::collections::HashSet::new();
        let count = sales
            .iter()
            .filter(|s| {
                (!self.config.require_verified_sales || s.verified)
                    && s.platform == platform
                    && &s.item == item
                    && s.sold_at > cutoff
                    && s.sold_at <= now
                    && matches_bucket(bucket, s.float_value)
                    && seen_ids.insert((s.platform, s.id.as_str()))
            })
            .count();
        u32::try_from(count).unwrap_or(u32::MAX)
    }
}

fn matches_bucket(bucket: Option<FloatBucket>, float_value: Option<f64>) -> bool {
    match bucket {
        None => true,
        Some(expected) => float_value
            .and_then(FloatBucket::from_float)
            .is_some_and(|actual| actual == expected),
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use cs2_arb_core::WearTier;

    use super::*;

    fn fees() -> FeeSchedules {
        FeeSchedules::from_toml(include_str!("../../../config/default.toml")).unwrap()
    }

    fn item() -> ItemId {
        ItemId {
            market_hash_name: "AK-47 | Redline (Field-Tested)".into(),
            wear_tier: WearTier::FieldTested,
        }
    }

    fn t(day: u32, hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, day, hour, 0, 0).unwrap()
    }

    fn make_position(acquired_at: DateTime<Utc>) -> Position {
        Position {
            id: "peg-bid-1#pos".into(),
            item: item(),
            float_bucket: None,
            acquired_on: Platform::CsFloat,
            quantity: 1,
            cost_basis: Money(8_500),
            acquired_at,
            locked_until: acquired_at + Duration::days(7),
        }
    }

    fn make_sales(count: usize, price: i64, end: DateTime<Utc>) -> Vec<Sale> {
        (1..=count)
            .map(|i| Sale {
                id: format!("s-{i}-{}", end.timestamp()),
                platform: Platform::CsFloat,
                item: item(),
                price: Money(price),
                float_value: Some(0.20),
                sold_at: end - Duration::hours(i as i64),
                verified: true,
            })
            .collect()
    }

    #[test]
    fn cross_venue_relist_requires_liquidity_and_ranks_net_proceeds() {
        let acquired = t(1, 12);
        let now = t(8, 12);
        let position = make_position(acquired);
        let fees = fees();
        let mut sales = make_sales(20, 10_000, now);
        let waxpeer_sales: Vec<_> = make_sales(20, 12_000, now)
            .into_iter()
            .map(|s| Sale {
                platform: Platform::Waxpeer,
                ..s
            })
            .collect();
        let mut engine = RelistEngine::new(RelistConfig {
            cross_venue: true,
            ..RelistConfig::default()
        })
        .unwrap();
        engine.track_position(position.clone(), acquired);
        sales.extend(waxpeer_sales.iter().take(1).cloned());
        let step = engine.step(&sales, &[], &fees, now).unwrap();
        assert_eq!(step.listed_orders[0].platform, Platform::CsFloat);

        let mut engine = RelistEngine::new(RelistConfig {
            cross_venue: true,
            ..RelistConfig::default()
        })
        .unwrap();
        engine.track_position(position, acquired);
        sales.extend(waxpeer_sales.into_iter().skip(1));
        let step = engine.step(&sales, &[], &fees, now).unwrap();
        assert_eq!(step.listed_orders[0].platform, Platform::Waxpeer);
        let step = engine.step(&sales, &[], &fees, t(8, 14)).unwrap();
        assert_eq!(step.realized_sales[0].platform, Platform::Waxpeer);
        assert_eq!(
            step.realized_sales[0]
                .fee_breakdown
                .unwrap()
                .crypto_withdrawal,
            Money(180)
        );
    }

    #[test]
    fn enforces_seven_day_lock_transitions_to_ready_to_sell_and_relists_at_median() {
        let mut engine = RelistEngine::new(RelistConfig::default()).unwrap();
        let acquired = t(1, 12);
        let pos = make_position(acquired);
        engine.track_position(pos.clone(), acquired);

        // Day 6: still locked, remaining lock = 1 day, no ask placed.
        let day6 = t(7, 12);
        let sales = make_sales(20, 10_000, day6);
        let step_day6 = engine.step(&sales, &[], &fees(), day6).unwrap();
        assert!(step_day6.unlocked.is_empty());
        assert!(step_day6.listed_orders.is_empty());
        assert_eq!(engine.locked_inventory(day6).len(), 1);
        assert_eq!(engine.locked_inventory(day6)[0].1, Duration::days(1));

        // Day 7 (exact expiry): transitions to READY_TO_SELL and lists at rolling median ($100.00).
        let day7 = t(8, 12);
        let sales_day7 = make_sales(20, 10_000, day7);
        let step_day7 = engine.step(&sales_day7, &[], &fees(), day7).unwrap();
        assert_eq!(step_day7.unlocked, vec![pos.id.clone()]);
        assert_eq!(step_day7.listed_orders.len(), 1);
        assert_eq!(step_day7.listed_orders[0].limit_price, Money(10_000));
        assert!(engine.locked_inventory(day7).is_empty());
        assert_eq!(engine.active_asks().len(), 1);

        // 2 hours later: sufficient trailing volume (20 >= 15) fills the ask.
        // Gross $100.00, CSFloat 2% seller fee = $2.00, net proceeds = $98.00, net P&L = +$13.00.
        let fill_time = t(8, 14);
        let sales_later = make_sales(20, 10_000, fill_time);
        let step_fill = engine.step(&sales_later, &[], &fees(), fill_time).unwrap();
        assert_eq!(step_fill.realized_sales.len(), 1);
        let sale = &step_fill.realized_sales[0];
        assert_eq!(sale.ask_price, Money(10_000));
        assert_eq!(sale.seller_fee, Money(200));
        assert_eq!(sale.net_proceeds, Money(9_800));
        assert_eq!(sale.realized_net_pnl, Money(1_300));
        assert_eq!(engine.total_realized_pnl(), Money(1_300));
        assert!(matches!(
            engine.get(&pos.id).unwrap().status,
            InventoryStatus::Sold { .. }
        ));
    }

    #[test]
    fn ready_to_sell_serializes_as_ready_to_sell() {
        let json = serde_json::to_string(&InventoryStatus::ReadyToSell).unwrap();
        assert_eq!(json, "\"READY_TO_SELL\"");
    }
}
