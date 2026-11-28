//! Automated peg-bidding strategy (mock simulation only).
//!
//! Ingests read-only top-of-book buy orders from supported marketplaces (CSFloat /
//! Waxpeer), computes `target_bid = max_current_bid + bid_increment`, enforces a
//! strict margin ceiling targeting at least `target_margin_pct` (7% default)
//! net profit after platform fees and the 7-day trade hold, and applies cooldown
//! and debounce controls to prevent 1-cent bot-war churn.
//!
//! The exit value behind that ceiling comes from verified sales, because a lot is sold
//! at the price buyers pay, not at the price sellers ask. Asks can only lower it: an ask
//! median under recent prints means sellers are already undercutting them. A tier-only
//! bid can be filled by any float in its wear tier, and sellers route their worst floats
//! to it, so it is valued at a lower quantile of the tier's sales instead of the median.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use cs2_arb_core::{
    BuyOrder, Error, FeeSchedule, FeeSchedules, FloatBucket, ItemId, Money, Order, OrderSide,
    Platform, PriceSnapshot, Result, Sale,
};
use serde::{Deserialize, Serialize};

use crate::{cents_ceil, order_id, platform_slug};

/// Tunables for [`PegBidStrategy`]. Deserializable from TOML (all fields optional).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PegBidConfig {
    /// Minimum net profit margin percentage after platform fees and 7-day hold.
    /// Default 7.0 (7%).
    pub target_margin_pct: f64,
    /// Increment above `max_current_bid` in USD cents. Default 1 ($0.01).
    pub bid_increment: Money,
    /// Valve trade hold duration in days. Default 7.
    pub hold_days: u32,
    /// Annual opportunity-cost rate in percentage points for capital locked during
    /// the 7-day hold. Default 10.0 (10%/year).
    pub annual_opportunity_rate_pct: f64,
    /// Rolling window in days used to compute the median market price. Default 7.
    pub lookback_days: i64,
    /// Minimum observations required before computing a rolling median. Default 3.
    pub min_median_samples: usize,
    /// Minimum seconds between bid reprices for the same item/platform. Default 300.
    pub cooldown_secs: i64,
    /// Minimum absolute price change (USD cents) required to reprice an already-active
    /// bid once cooldown expires. Suppresses 1-cent bot-war jitter. Default 2 ($0.02).
    pub min_reprice_delta: Money,
    /// Maximum age of top-of-book buy orders in seconds (`None` disables age check).
    pub max_order_age_secs: Option<i64>,
    /// Optional simulated lifetime of a resting bid. Expiration cancels remaining units.
    pub resting_bid_ttl_secs: Option<u32>,
    /// When `target_bid` exceeds the strict margin ceiling:
    /// - `false` (default): discard the bid;
    /// - `true`: clamp the bid down to the exact margin ceiling.
    pub lower_to_ceiling: bool,
    /// Units per mock buy order. Default 1.
    pub quantity: u32,
    /// Platforms permitted for peg-bidding. Default `[CsFloat, Waxpeer]`.
    pub allowed_platforms: Vec<Platform>,
    /// Quantile of the tier's verified sales used as the exit value of a tier-only bid
    /// (`float_bucket: None`). Default 0.25; 0.5 restores the median.
    pub tier_only_exit_quantile: f64,
    /// When set, a venue without enough verified sales is valued at its ask median less
    /// this percentage. `None` (default) declines to bid without a sales tape, which is
    /// the case for Waxpeer, whose adapter has no verified sale feed.
    pub ask_only_exit_haircut_pct: Option<f64>,
}

impl Default for PegBidConfig {
    fn default() -> Self {
        Self {
            target_margin_pct: 7.0,
            bid_increment: Money(1),
            hold_days: 7,
            annual_opportunity_rate_pct: 10.0,
            lookback_days: 7,
            min_median_samples: 3,
            cooldown_secs: 300,
            min_reprice_delta: Money(2),
            max_order_age_secs: Some(3600),
            resting_bid_ttl_secs: None,
            lower_to_ceiling: false,
            quantity: 1,
            allowed_platforms: vec![Platform::CsFloat, Platform::Waxpeer],
            tier_only_exit_quantile: 0.25,
            ask_only_exit_haircut_pct: None,
        }
    }
}

impl PegBidConfig {
    pub fn validate(&self) -> Result<()> {
        let bad = |msg: &str| Err(Error::InvalidConfig(format!("peg_bid: {msg}")));
        if !self.target_margin_pct.is_finite()
            || self.target_margin_pct < 7.0
            || self.target_margin_pct >= 100.0
        {
            return bad("target_margin_pct must be finite and in [7, 100)");
        }
        if self.bid_increment != Money(1) {
            return bad("bid_increment must be 1 cent");
        }
        if self.hold_days < 7 {
            return bad("hold_days must be >= 7 to match the mandatory 7-day CS2 trade hold");
        }
        if !self.annual_opportunity_rate_pct.is_finite() || self.annual_opportunity_rate_pct < 0.0 {
            return bad("annual_opportunity_rate_pct must be finite and >= 0");
        }
        if self.lookback_days <= 0 {
            return bad("lookback_days must be > 0");
        }
        if self.min_median_samples == 0 {
            return bad("min_median_samples must be >= 1");
        }
        if self.cooldown_secs <= 0 {
            return bad("cooldown_secs must be > 0");
        }
        if self.min_reprice_delta.0 < 0 {
            return bad("min_reprice_delta must be >= 0");
        }
        if matches!(self.max_order_age_secs, Some(age) if age < 0) {
            return bad("max_order_age_secs must be >= 0");
        }
        if self.resting_bid_ttl_secs == Some(0) {
            return bad("resting_bid_ttl_secs must be positive");
        }
        if self.quantity == 0 {
            return bad("quantity must be >= 1");
        }
        if !(self.tier_only_exit_quantile > 0.0 && self.tier_only_exit_quantile <= 0.5) {
            return bad("tier_only_exit_quantile must be in (0, 0.5]");
        }
        if matches!(self.ask_only_exit_haircut_pct, Some(pct) if !pct.is_finite() || !(0.0..100.0).contains(&pct))
        {
            return bad("ask_only_exit_haircut_pct must be finite and in [0, 100)");
        }
        if self.allowed_platforms.is_empty()
            || self
                .allowed_platforms
                .iter()
                .any(|platform| !matches!(platform, Platform::CsFloat | Platform::Waxpeer))
        {
            return bad("allowed_platforms must contain only CSFloat or Waxpeer");
        }
        Ok(())
    }

    pub fn target_margin_fraction(&self) -> f64 {
        self.target_margin_pct / 100.0
    }

    pub fn opportunity_cost_fraction(&self) -> f64 {
        self.annual_opportunity_rate_pct / 100.0 * f64::from(self.hold_days) / 365.0
    }
}

/// Economics of a candidate peg bid, in USD cents for `quantity` units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PegBidEconomics {
    pub unit_bid: Money,
    pub rolling_median: Money,
    pub raw_margin_ceiling: Money,
    pub effective_ceiling: Money,
    pub buy_cost: Money,
    pub sell_proceeds: Money,
    pub opportunity_cost: Money,
    pub expected_net_profit: Money,
    pub expected_net_margin_pct: f64,
}

/// Currently resting mock peg bid tracked by [`PegBidStrategy`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActivePegBid {
    pub order: Order,
    pub max_observed_bid: Money,
    pub rolling_median: Money,
    pub effective_ceiling: Money,
    pub expected_net_profit: Money,
    pub expected_net_margin_pct: f64,
    pub updated_at: DateTime<Utc>,
}

/// Reason a candidate peg bid was not placed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PegBidSkipReason {
    PlatformNotAllowed(Platform),
    NoBuyOrderData,
    InsufficientMedianHistory { found: usize, required: usize },
    NoProfitableCeiling,
    ExceedsMarginCeiling { target_bid: Money, ceiling: Money },
    CooldownActive { remaining_secs: i64 },
    Debounced { current: Money, proposed: Money },
    Unchanged,
}

/// Outcome of evaluating one item/platform for peg bidding.
#[derive(Debug, Clone, PartialEq)]
pub enum PegBidDecision {
    Placed(ActivePegBid),
    Skipped(PegBidSkipReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PegBidPlacement {
    Accepted,
    KeepResting,
    CancelResting,
}

type BookKey = (Platform, ItemId, Option<FloatBucket>);

#[derive(Debug, Clone, Copy)]
struct PricePoint {
    observed_at: DateTime<Utc>,
    price: Money,
}

/// Automated peg-bidding engine producing mock `Order`s as plain data.
#[derive(Debug, Clone)]
pub struct PegBidStrategy {
    config: PegBidConfig,
    /// Observed top-of-book buy orders keyed by `(platform, item, float_bucket)`.
    top_bids: HashMap<BookKey, BuyOrder>,
    /// Verified sale prints: the primary exit-value evidence.
    price_history: HashMap<BookKey, Vec<PricePoint>>,
    /// Lowest-ask snapshots: they can only lower the exit value, or stand in for missing
    /// sales when `ask_only_exit_haircut_pct` is configured.
    ask_history: HashMap<BookKey, Vec<PricePoint>>,
    /// Active mock bids maintained by this strategy.
    active_bids: HashMap<BookKey, ActivePegBid>,
    /// Last bid update time per key for anti-jitter cooldown enforcement.
    last_updated: HashMap<BookKey, DateTime<Utc>>,
}

impl PegBidStrategy {
    pub fn new(config: PegBidConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            top_bids: HashMap::new(),
            price_history: HashMap::new(),
            ask_history: HashMap::new(),
            active_bids: HashMap::new(),
            last_updated: HashMap::new(),
        })
    }

    pub fn config(&self) -> &PegBidConfig {
        &self.config
    }

    /// Ingest a read-only top-of-book buy order from a marketplace client.
    /// Keeps the freshest / highest observed bid per `(platform, item, float_bucket)`.
    pub fn ingest_buy_order(&mut self, order: BuyOrder) {
        if !self.config.allowed_platforms.contains(&order.platform) || order.price.0 <= 0 {
            return;
        }
        let key = (order.platform, order.item.clone(), order.float_bucket);
        match self.top_bids.get_mut(&key) {
            Some(existing) => {
                if order.observed_at > existing.observed_at
                    || (order.observed_at == existing.observed_at && order.price > existing.price)
                {
                    *existing = order;
                }
            }
            None => {
                self.top_bids.insert(key, order);
            }
        }
    }

    /// Ingest a batch of read-only top-of-book buy orders.
    pub fn ingest_buy_orders(&mut self, orders: impl IntoIterator<Item = BuyOrder>) {
        for order in orders {
            self.ingest_buy_order(order);
        }
    }

    pub fn replace_buy_orders(
        &mut self,
        platform: Platform,
        item: &ItemId,
        orders: &[BuyOrder],
        now: DateTime<Utc>,
    ) {
        self.top_bids
            .retain(|(venue, bid_item, _), _| *venue != platform || bid_item != item);
        if !self.config.allowed_platforms.contains(&platform) {
            return;
        }
        let max_age = self.config.max_order_age_secs;
        let fresh = |observed_at: DateTime<Utc>| {
            observed_at <= now && max_age.is_none_or(|age| (now - observed_at).num_seconds() <= age)
        };
        for order in orders.iter().filter(|order| {
            order.platform == platform
                && &order.item == item
                && order.price.0 > 0
                && order.quantity > 0
                && fresh(order.observed_at)
        }) {
            self.top_bids
                .entry((platform, item.clone(), order.float_bucket))
                .and_modify(|best| {
                    if !fresh(best.observed_at)
                        || (order.price, order.observed_at) > (best.price, best.observed_at)
                    {
                        *best = order.clone();
                    }
                })
                .or_insert_with(|| order.clone());
        }
        self.active_bids.retain(|key @ (venue, bid_item, _), _| {
            *venue != platform || bid_item != item || self.top_bids.contains_key(key)
        });
    }

    /// Record a verified sale into the rolling median price history.
    pub fn observe_sale(&mut self, sale: &Sale) {
        if !sale.verified || sale.price.0 <= 0 {
            return;
        }
        let bucket = sale.float_value.and_then(FloatBucket::from_float);
        let key = (sale.platform, sale.item.clone(), bucket);
        self.price_history.entry(key).or_default().push(PricePoint {
            observed_at: sale.sold_at,
            price: sale.price,
        });
    }

    /// Record a lowest-ask snapshot; see [`Self::rolling_median`] for how asks are used.
    pub fn observe_snapshot(&mut self, snapshot: &PriceSnapshot) {
        if snapshot.price.0 <= 0 {
            return;
        }
        let key = (
            snapshot.platform,
            snapshot.item.clone(),
            snapshot.float_bucket,
        );
        self.ask_history.entry(key).or_default().push(PricePoint {
            observed_at: snapshot.observed_at,
            price: snapshot.price,
        });
    }

    /// Exit value of a lot bought on `platform`, from observations in
    /// `(now - lookback_days, now]`, with the number of observations behind it.
    ///
    /// * With at least `min_median_samples` verified sales: the sales median for a float
    ///   bucket, or the `tier_only_exit_quantile` of all the tier's sales for a tier-only
    ///   query, lowered to the ask median when asks sit below it.
    /// * Without them: the ask median less `ask_only_exit_haircut_pct`, if configured.
    /// * Otherwise `None`; no bid is placed against an unknown exit.
    pub fn rolling_median(
        &self,
        platform: Platform,
        item: &ItemId,
        float_bucket: Option<FloatBucket>,
        now: DateTime<Utc>,
    ) -> Option<(Money, usize)> {
        let required = self.config.min_median_samples;
        let mut sales = self.window_prices(&self.price_history, platform, item, float_bucket, now);
        let mut asks = self.window_prices(&self.ask_history, platform, item, float_bucket, now);
        let ask_median = (asks.len() >= required).then(|| median_cents(&mut asks));
        if sales.len() >= required {
            let count = sales.len();
            let sales_value = match float_bucket {
                Some(_) => median_cents(&mut sales),
                None => quantile_cents(&mut sales, self.config.tier_only_exit_quantile),
            };
            return Some((
                ask_median.map_or(sales_value, |ask| ask.min(sales_value)),
                count,
            ));
        }
        let haircut = self.config.ask_only_exit_haircut_pct?;
        let ask = ask_median?;
        let value = Money((ask.0 as f64 * (1.0 - haircut / 100.0)).floor() as i64);
        Some((value, asks.len()))
    }

    /// Prices in the lookback window for one venue and item. A float bucket selects only
    /// that bucket; a tier-only query takes every bucket, because any float in the tier
    /// can fill a tier-only bid.
    fn window_prices(
        &self,
        history: &HashMap<BookKey, Vec<PricePoint>>,
        platform: Platform,
        item: &ItemId,
        float_bucket: Option<FloatBucket>,
        now: DateTime<Utc>,
    ) -> Vec<i64> {
        let cutoff = now - Duration::days(self.config.lookback_days);
        history
            .iter()
            .filter(|((p, it, bucket), _)| {
                *p == platform && it == item && (float_bucket.is_none() || *bucket == float_bucket)
            })
            .flat_map(|(_, points)| points.iter())
            .filter(|point| point.observed_at > cutoff && point.observed_at <= now)
            .map(|point| point.price.0)
            .collect()
    }

    /// Evaluate a single `(platform, item, float_bucket)` against its ingested
    /// top-of-book bid and rolling median price.
    pub fn evaluate_item(
        &mut self,
        platform: Platform,
        item: &ItemId,
        float_bucket: Option<FloatBucket>,
        fees: &FeeSchedules,
        now: DateTime<Utc>,
    ) -> Result<PegBidDecision> {
        let decision = self.propose_item(platform, item, float_bucket, fees, now)?;
        if let PegBidDecision::Placed(active) = &decision {
            self.record_placement(active.clone());
        }
        Ok(decision)
    }

    fn propose_item(
        &mut self,
        platform: Platform,
        item: &ItemId,
        float_bucket: Option<FloatBucket>,
        fees: &FeeSchedules,
        now: DateTime<Utc>,
    ) -> Result<PegBidDecision> {
        if !self.config.allowed_platforms.contains(&platform) {
            return Ok(PegBidDecision::Skipped(
                PegBidSkipReason::PlatformNotAllowed(platform),
            ));
        }

        let key = (platform, item.clone(), float_bucket);
        let Some(top_bid) = self.fresh_top_bid(&key, now).cloned() else {
            return Ok(PegBidDecision::Skipped(PegBidSkipReason::NoBuyOrderData));
        };

        let Some((rolling_median, _)) = self.rolling_median(platform, item, float_bucket, now)
        else {
            self.active_bids.remove(&key);
            let found = self.sample_count(platform, item, float_bucket, now);
            return Ok(PegBidDecision::Skipped(
                PegBidSkipReason::InsufficientMedianHistory {
                    found,
                    required: self.config.min_median_samples,
                },
            ));
        };

        let schedule = fees.get(platform);
        let Some(effective_ceiling) =
            compute_effective_ceiling(rolling_median, schedule, &self.config)?
        else {
            self.active_bids.remove(&key);
            return Ok(PegBidDecision::Skipped(
                PegBidSkipReason::NoProfitableCeiling,
            ));
        };

        let target_bid = top_bid.price.checked_add(self.config.bid_increment)?;
        let proposed_bid = if target_bid <= effective_ceiling {
            target_bid
        } else if self.config.lower_to_ceiling {
            effective_ceiling
        } else {
            self.active_bids.remove(&key);
            return Ok(PegBidDecision::Skipped(
                PegBidSkipReason::ExceedsMarginCeiling {
                    target_bid,
                    ceiling: effective_ceiling,
                },
            ));
        };

        let economics = evaluate_bid_economics(
            proposed_bid,
            rolling_median,
            effective_ceiling,
            schedule,
            &self.config,
        )?;

        let mut violates_ceiling = false;
        if let Some(existing) = self.active_bids.get(&key) {
            let current = existing.order.limit_price;
            // Safety override: if the existing resting bid now violates the new margin
            // ceiling, bypass cooldown/debounce so we never stay exposed above ceiling.
            violates_ceiling = current > effective_ceiling;
            if !violates_ceiling && proposed_bid == current {
                return Ok(PegBidDecision::Skipped(PegBidSkipReason::Unchanged));
            }
        }
        if !violates_ceiling {
            if let Some(last) = self.last_updated.get(&key) {
                let elapsed = (now - *last).num_seconds();
                if elapsed < self.config.cooldown_secs {
                    return Ok(PegBidDecision::Skipped(PegBidSkipReason::CooldownActive {
                        remaining_secs: self.config.cooldown_secs - elapsed,
                    }));
                }
            }
            if let Some(existing) = self.active_bids.get(&key) {
                let current = existing.order.limit_price;
                if (proposed_bid.0 - current.0).abs() < self.config.min_reprice_delta.0 {
                    return Ok(PegBidDecision::Skipped(PegBidSkipReason::Debounced {
                        current,
                        proposed: proposed_bid,
                    }));
                }
            }
        }

        let order = Order {
            id: order_id(
                "peg-bid",
                &[platform_slug(platform), &item.market_hash_name],
                float_bucket,
                now,
            ),
            side: OrderSide::Buy,
            platform,
            item: item.clone(),
            float_bucket,
            quantity: self.config.quantity,
            limit_price: proposed_bid,
            submitted_at: now,
        };
        let active = ActivePegBid {
            order,
            max_observed_bid: top_bid.price,
            rolling_median,
            effective_ceiling,
            expected_net_profit: economics.expected_net_profit,
            expected_net_margin_pct: economics.expected_net_margin_pct,
            updated_at: now,
        };
        Ok(PegBidDecision::Placed(active))
    }

    fn record_placement(&mut self, active: ActivePegBid) {
        let order = &active.order;
        let key = (order.platform, order.item.clone(), order.float_bucket);
        self.last_updated.insert(key.clone(), active.updated_at);
        self.active_bids.insert(key, active);
    }

    /// Ingest a batch of top-of-book buy orders and emit mock `Order`s for every
    /// qualifying item (sorted deterministically by platform, item name, and bucket).
    pub fn evaluate(
        &mut self,
        buy_orders: &[BuyOrder],
        fees: &FeeSchedules,
        now: DateTime<Utc>,
    ) -> Result<Vec<Order>> {
        self.evaluate_with(buy_orders, fees, now, |_, _| Ok(PegBidPlacement::Accepted))
    }

    /// Evaluate bids and let the caller decide whether a rejected replacement
    /// should leave the current bid resting.
    pub fn evaluate_with(
        &mut self,
        buy_orders: &[BuyOrder],
        fees: &FeeSchedules,
        now: DateTime<Utc>,
        mut place: impl FnMut(&Order, Option<&Order>) -> Result<PegBidPlacement>,
    ) -> Result<Vec<Order>> {
        self.ingest_buy_orders(buy_orders.iter().cloned());
        let mut keys: Vec<BookKey> = self.top_bids.keys().cloned().collect();
        keys.sort_by(|a, b| {
            platform_slug(a.0)
                .cmp(platform_slug(b.0))
                .then_with(|| a.1.market_hash_name.cmp(&b.1.market_hash_name))
                .then_with(|| format!("{:?}", a.2).cmp(&format!("{:?}", b.2)))
        });
        let mut orders = Vec::new();
        for key in keys {
            let (platform, item, bucket) = &key;
            let PegBidDecision::Placed(active) =
                self.propose_item(*platform, item, *bucket, fees, now)?
            else {
                continue;
            };
            let resting = self.active_bids.get(&key).map(|bid| &bid.order);
            match place(&active.order, resting)? {
                PegBidPlacement::Accepted => {
                    orders.push(active.order.clone());
                    self.record_placement(active);
                }
                PegBidPlacement::KeepResting => {}
                PegBidPlacement::CancelResting => {
                    self.active_bids.remove(&key);
                }
            }
        }
        Ok(orders)
    }

    pub fn retain_bids(&mut self, mut keep: impl FnMut(&ItemId) -> bool) {
        self.active_bids.retain(|_, bid| keep(&bid.order.item));
    }

    /// Remove an active bid after it has been filled in the simulator.
    pub fn mark_filled(&mut self, order_id: &str) -> Option<ActivePegBid> {
        let key = self
            .active_bids
            .iter()
            .find(|(_, bid)| bid.order.id == order_id)
            .map(|(k, _)| k.clone())?;
        self.active_bids.remove(&key)
    }

    pub fn cancel_unprofitable_bids(
        &mut self,
        fees: &FeeSchedules,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let mut cancelled = Vec::new();
        for (key @ (platform, item, bucket), active) in &self.active_bids {
            let ceiling = match self.rolling_median(*platform, item, *bucket, now) {
                Some((median, _)) => {
                    compute_effective_ceiling(median, fees.get(*platform), &self.config)?
                }
                None => None,
            };
            let target_exceeds_ceiling =
                if let (Some(ceiling), Some(top)) = (ceiling, self.fresh_top_bid(key, now)) {
                    !self.config.lower_to_ceiling
                        && top.price.checked_add(self.config.bid_increment)? > ceiling
                } else {
                    false
                };
            if target_exceeds_ceiling
                || ceiling.is_none_or(|ceiling| active.order.limit_price > ceiling)
            {
                let bypass_cooldown = self.config.lower_to_ceiling
                    && ceiling.is_some_and(|ceiling| active.order.limit_price > ceiling);
                cancelled.push((key.clone(), bypass_cooldown));
            }
        }
        for (key, bypass_cooldown) in cancelled {
            self.active_bids.remove(&key);
            if bypass_cooldown {
                self.last_updated.remove(&key);
            }
        }
        Ok(())
    }

    fn fresh_top_bid(&self, key: &BookKey, now: DateTime<Utc>) -> Option<&BuyOrder> {
        self.top_bids.get(key).filter(|bid| {
            bid.observed_at <= now
                && self
                    .config
                    .max_order_age_secs
                    .is_none_or(|age| (now - bid.observed_at).num_seconds() <= age)
        })
    }

    /// Active mock bids, sorted deterministically for display and logging.
    pub fn active_bids(&self) -> Vec<&ActivePegBid> {
        let mut bids: Vec<&ActivePegBid> = self.active_bids.values().collect();
        bids.sort_by(|a, b| {
            platform_slug(a.order.platform)
                .cmp(platform_slug(b.order.platform))
                .then_with(|| {
                    a.order
                        .item
                        .market_hash_name
                        .cmp(&b.order.item.market_hash_name)
                })
        });
        bids
    }

    fn sample_count(
        &self,
        platform: Platform,
        item: &ItemId,
        float_bucket: Option<FloatBucket>,
        now: DateTime<Utc>,
    ) -> usize {
        let sales = self
            .window_prices(&self.price_history, platform, item, float_bucket, now)
            .len();
        if sales > 0 || self.config.ask_only_exit_haircut_pct.is_none() {
            return sales;
        }
        self.window_prices(&self.ask_history, platform, item, float_bucket, now)
            .len()
    }
}

/// Computes the exact integer-cent upper bound on a unit bid such that:
/// 1. `bid <= floor(rolling_median * (1.0 - target_margin))`
/// 2. Expected net profit after platform buy/sell fees and the 7-day hold
///    opportunity cost is `>= target_margin_pct` of total buy cost.
pub fn compute_effective_ceiling(
    rolling_median: Money,
    fees: FeeSchedule,
    config: &PegBidConfig,
) -> Result<Option<Money>> {
    config.validate()?;
    fees.validate()?;
    if rolling_median.0 <= 0 {
        return Ok(None);
    }

    let target_margin = config.target_margin_fraction();
    let raw_ceiling = Money((rolling_median.0 as f64 * (1.0 - target_margin)).floor() as i64);
    if raw_ceiling.0 <= 0 {
        return Ok(None);
    }

    let (mut low, mut high) = (0, raw_ceiling.0);
    while low < high {
        let mid = high - (high - low) / 2;
        if meets_net_margin(Money(mid), rolling_median, fees, config)? {
            low = mid;
        } else {
            high = mid - 1;
        }
    }

    Ok((low > 0).then_some(Money(low)))
}

/// Checks whether a candidate unit bid guarantees at least `config.target_margin_pct`
/// net profit after buyer fees, seller fees, and 7-day hold opportunity cost.
pub fn meets_net_margin(
    unit_bid: Money,
    rolling_median: Money,
    fees: FeeSchedule,
    config: &PegBidConfig,
) -> Result<bool> {
    if unit_bid.0 <= 0 || rolling_median.0 <= 0 || config.quantity == 0 {
        return Ok(false);
    }
    let econ = evaluate_bid_economics(unit_bid, rolling_median, unit_bid, fees, config)?;
    if fees.buyer_fixed_fee.0 > 0 || fees.funding.crypto_deposit.fixed.0 > 0 {
        let qty = i64::from(config.quantity);
        let largest_basis = Money(econ.buy_cost.0 / qty + i64::from(econ.buy_cost.0 % qty != 0));
        if fees.seller_order_proceeds(rolling_median, 1)? <= largest_basis {
            return Ok(false);
        }
    }
    let required_net_profit = cents_ceil(econ.buy_cost.0 as f64 * config.target_margin_fraction())?;
    Ok(econ.expected_net_profit >= required_net_profit
        && (!fees.funding.enabled() || econ.expected_net_profit.0 > 0))
}

/// Computes full integer-cent economics for a candidate unit bid.
pub fn evaluate_bid_economics(
    unit_bid: Money,
    rolling_median: Money,
    effective_ceiling: Money,
    fees: FeeSchedule,
    config: &PegBidConfig,
) -> Result<PegBidEconomics> {
    let raw_margin_ceiling =
        Money((rolling_median.0 as f64 * (1.0 - config.target_margin_fraction())).floor() as i64);
    let buy_cost = fees.buyer_order_total(unit_bid, config.quantity)?;
    let sell_proceeds = fees
        .seller_order_proceeds(rolling_median, 1)?
        .checked_mul(config.quantity)?;
    let opportunity_cost = cents_ceil(buy_cost.0 as f64 * config.opportunity_cost_fraction())?;
    let expected_net_profit = sell_proceeds
        .checked_sub(buy_cost)?
        .checked_sub(opportunity_cost)?;
    let expected_net_margin_pct = if buy_cost.0 > 0 {
        expected_net_profit.0 as f64 / buy_cost.0 as f64 * 100.0
    } else {
        0.0
    };
    Ok(PegBidEconomics {
        unit_bid,
        rolling_median,
        raw_margin_ceiling,
        effective_ceiling,
        buy_cost,
        sell_proceeds,
        opportunity_cost,
        expected_net_profit,
        expected_net_margin_pct,
    })
}

/// Lower nearest-rank quantile (`q` in `(0, 1]`) of a non-empty price slice, in cents.
/// Rounding the rank down keeps the estimate on the conservative side.
pub fn quantile_cents(prices: &mut [i64], q: f64) -> Money {
    prices.sort_unstable();
    let rank = ((prices.len() - 1) as f64 * q).floor() as usize;
    Money(prices[rank.min(prices.len() - 1)])
}

/// Computes the conservative integer-cent median of a non-empty price slice.
pub fn median_cents(prices: &mut [i64]) -> Money {
    prices.sort_unstable();
    let mid = prices.len() / 2;
    if prices.len() % 2 == 1 {
        Money(prices[mid])
    } else {
        Money((prices[mid - 1] + prices[mid]) / 2)
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use cs2_arb_core::WearTier;

    use super::*;

    fn fees() -> FeeSchedules {
        FeeSchedules::from_toml(include_str!("../../../data/fixtures/test_fees.toml")).unwrap()
    }

    fn item() -> ItemId {
        ItemId {
            market_hash_name: "AK-47 | Redline (Field-Tested)".into(),
            wear_tier: WearTier::FieldTested,
        }
    }

    fn t(hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 10, hour, minute, 0).unwrap()
    }

    fn seed_median(
        strategy: &mut PegBidStrategy,
        platform: Platform,
        median: i64,
        now: DateTime<Utc>,
    ) {
        // Verified prints at one price make the tier quantile and the median coincide, so
        // tests can state ceilings in terms of `median`.
        for i in 0..3 {
            strategy.observe_sale(&Sale {
                id: format!("seed-{platform:?}-{i}"),
                platform,
                item: item(),
                price: Money(median),
                float_value: None,
                sold_at: now - Duration::hours(i as i64 + 1),
                verified: true,
            });
        }
    }

    #[test]
    fn cancelled_bid_above_a_lower_ceiling_can_be_replaced_during_cooldown() {
        let now = t(12, 0);
        let platform = Platform::CsFloat;
        let item = item();
        let fees = fees();
        let config = PegBidConfig {
            lower_to_ceiling: true,
            ..PegBidConfig::default()
        };
        let mut strategy = PegBidStrategy::new(config.clone()).unwrap();
        seed_median(&mut strategy, platform, 10_000, now);
        strategy.ingest_buy_order(BuyOrder {
            platform,
            item: item.clone(),
            float_bucket: None,
            price: Money(8_500),
            quantity: 1,
            observed_at: now,
        });
        assert!(matches!(
            strategy
                .evaluate_item(platform, &item, None, &fees, now)
                .unwrap(),
            PegBidDecision::Placed(_)
        ));

        let now = now + Duration::seconds(1);
        for _ in 0..4 {
            strategy.observe_snapshot(&PriceSnapshot {
                platform,
                item: item.clone(),
                float_bucket: None,
                price: Money(8_000),
                observed_at: now,
            });
        }
        strategy.cancel_unprofitable_bids(&fees, now).unwrap();
        assert!(strategy.active_bids().is_empty());
        let PegBidDecision::Placed(replacement) = strategy
            .evaluate_item(platform, &item, None, &fees, now)
            .unwrap()
        else {
            panic!("safety repricing should bypass cooldown");
        };
        let ceiling = compute_effective_ceiling(Money(8_000), fees.get(platform), &config)
            .unwrap()
            .unwrap();
        assert_eq!(replacement.order.limit_price, ceiling);
    }

    #[test]
    fn expired_competitor_bid_does_not_cancel_a_safe_resting_bid() {
        let now = t(12, 0);
        let platform = Platform::CsFloat;
        let item = item();
        let fees = fees();
        let mut strategy = PegBidStrategy::new(PegBidConfig {
            max_order_age_secs: Some(1),
            ..PegBidConfig::default()
        })
        .unwrap();
        seed_median(&mut strategy, platform, 10_000, now);
        let bid = BuyOrder {
            platform,
            item: item.clone(),
            float_bucket: None,
            price: Money(8_500),
            quantity: 1,
            observed_at: now,
        };
        strategy.ingest_buy_order(bid.clone());
        let PegBidDecision::Placed(resting) = strategy
            .evaluate_item(platform, &item, None, &fees, now)
            .unwrap()
        else {
            panic!("expected safe resting bid");
        };
        strategy.ingest_buy_order(BuyOrder {
            price: Money(9_500),
            ..bid
        });
        let later = now + Duration::seconds(2);
        assert!(matches!(
            strategy
                .evaluate_item(platform, &item, None, &fees, later)
                .unwrap(),
            PegBidDecision::Skipped(PegBidSkipReason::NoBuyOrderData)
        ));
        strategy.cancel_unprofitable_bids(&fees, later).unwrap();
        assert_eq!(strategy.active_bids()[0].order.id, resting.order.id);
        strategy.ingest_buy_order(BuyOrder {
            platform,
            item,
            float_bucket: None,
            price: Money(9_500),
            quantity: 1,
            observed_at: later,
        });
        strategy.cancel_unprofitable_bids(&fees, later).unwrap();
        assert!(strategy.active_bids().is_empty());
    }

    #[test]
    fn refresh_uses_the_highest_fresh_bid_across_pages() {
        let now = t(12, 0);
        let venue = Platform::CsFloat;
        let item = item();
        let mut strategy = PegBidStrategy::new(PegBidConfig {
            max_order_age_secs: Some(2),
            ..PegBidConfig::default()
        })
        .unwrap();
        let bid = |price, age| BuyOrder {
            platform: venue,
            item: item.clone(),
            float_bucket: None,
            price: Money(price),
            quantity: 1,
            observed_at: now - Duration::seconds(age),
        };
        strategy.replace_buy_orders(
            venue,
            &item,
            &[bid(9000, 3), bid(8500, 1), bid(8400, 0)],
            now,
        );
        assert_eq!(
            strategy.top_bids[&(venue, item.clone(), None)].price,
            Money(8500)
        );
        strategy.replace_buy_orders(venue, &item, &[], now);
        assert!(strategy.top_bids.is_empty());
    }

    #[test]
    fn calculates_optimal_bid_one_cent_above_max_bid_within_margin_ceiling() {
        let now = t(12, 0);
        let mut strat = PegBidStrategy::new(PegBidConfig::default()).unwrap();
        seed_median(&mut strat, Platform::CsFloat, 10_000, now);

        strat.ingest_buy_order(BuyOrder {
            platform: Platform::CsFloat,
            item: item(),
            float_bucket: None,
            price: Money(8_500),
            quantity: 3,
            observed_at: now,
        });

        let decision = strat
            .evaluate_item(Platform::CsFloat, &item(), None, &fees(), now)
            .unwrap();
        let PegBidDecision::Placed(active) = decision else {
            panic!("expected placed bid, got {decision:?}");
        };
        assert_eq!(active.order.limit_price, Money(8_501));
        assert_eq!(active.rolling_median, Money(10_000));
        assert!(active.expected_net_margin_pct >= 7.0);
        assert!(active.order.limit_price <= active.effective_ceiling);
    }

    #[test]
    fn enforces_strict_margin_ceiling_after_csfloat_seller_fee_and_seven_day_hold() {
        let now = t(12, 0);
        let config = PegBidConfig::default();
        let schedules = fees();
        let csfloat_fees = schedules.get(Platform::CsFloat);

        // Median $100.00 (10_000c), CSFloat 2% seller fee -> $98.00 proceeds.
        // Raw ceiling = 10_000 * 0.93 = 9_300c, but after 2% seller fee + 7-day hold
        // opportunity cost, max bid guaranteeing >= 7% net profit is strictly lower.
        let ceiling = compute_effective_ceiling(Money(10_000), csfloat_fees, &config)
            .unwrap()
            .unwrap();
        assert!(ceiling.0 < 9_300);
        assert!(meets_net_margin(ceiling, Money(10_000), csfloat_fees, &config).unwrap());
        assert!(
            !meets_net_margin(Money(ceiling.0 + 1), Money(10_000), csfloat_fees, &config).unwrap()
        );

        // Bid at ceiling + 1 is discarded when lower_to_ceiling is false.
        let mut strat = PegBidStrategy::new(config.clone()).unwrap();
        seed_median(&mut strat, Platform::CsFloat, 10_000, now);
        strat.ingest_buy_order(BuyOrder {
            platform: Platform::CsFloat,
            item: item(),
            float_bucket: None,
            price: ceiling, // target_bid = ceiling + 1 -> exceeds ceiling
            quantity: 1,
            observed_at: now,
        });
        let decision = strat
            .evaluate_item(Platform::CsFloat, &item(), None, &schedules, now)
            .unwrap();
        assert!(matches!(
            decision,
            PegBidDecision::Skipped(PegBidSkipReason::ExceedsMarginCeiling { .. })
        ));
        assert!(strat.active_bids().is_empty());

        // When lower_to_ceiling is true, the bid is lowered to the exact ceiling.
        let mut lowering = PegBidStrategy::new(PegBidConfig {
            lower_to_ceiling: true,
            ..config
        })
        .unwrap();
        seed_median(&mut lowering, Platform::CsFloat, 10_000, now);
        lowering.ingest_buy_order(BuyOrder {
            platform: Platform::CsFloat,
            item: item(),
            float_bucket: None,
            price: Money(9_500),
            quantity: 1,
            observed_at: now,
        });
        let PegBidDecision::Placed(lowered) = lowering
            .evaluate_item(Platform::CsFloat, &item(), None, &schedules, now)
            .unwrap()
        else {
            panic!("expected lowered bid");
        };
        assert_eq!(lowered.order.limit_price, ceiling);
        assert!(lowered.expected_net_margin_pct >= 7.0);
    }

    #[test]
    fn anti_jitter_cooldown_and_debounce_prevent_one_cent_bot_wars() {
        let schedules = fees();
        let mut strat = PegBidStrategy::new(PegBidConfig {
            cooldown_secs: 300,
            min_reprice_delta: Money(2),
            ..PegBidConfig::default()
        })
        .unwrap();
        let t0 = t(12, 0);
        seed_median(&mut strat, Platform::CsFloat, 10_000, t0);

        // Initial bid at $85.01
        strat.ingest_buy_order(BuyOrder {
            platform: Platform::CsFloat,
            item: item(),
            float_bucket: None,
            price: Money(8_500),
            quantity: 1,
            observed_at: t0,
        });
        assert!(matches!(
            strat
                .evaluate_item(Platform::CsFloat, &item(), None, &schedules, t0)
                .unwrap(),
            PegBidDecision::Placed(_)
        ));

        // 1 minute later, rival bot bids $85.01 -> target $85.02 blocked by cooldown.
        let t1 = t(12, 1);
        strat.ingest_buy_order(BuyOrder {
            platform: Platform::CsFloat,
            item: item(),
            float_bucket: None,
            price: Money(8_501),
            quantity: 1,
            observed_at: t1,
        });
        assert!(matches!(
            strat
                .evaluate_item(Platform::CsFloat, &item(), None, &schedules, t1)
                .unwrap(),
            PegBidDecision::Skipped(PegBidSkipReason::CooldownActive { .. })
        ));

        // 6 minutes later (after cooldown), 1-cent move ($85.01 -> $85.02) is debounced.
        let t2 = t(12, 6);
        strat.ingest_buy_order(BuyOrder {
            platform: Platform::CsFloat,
            item: item(),
            float_bucket: None,
            price: Money(8_501),
            quantity: 1,
            observed_at: t2,
        });
        assert!(matches!(
            strat
                .evaluate_item(Platform::CsFloat, &item(), None, &schedules, t2)
                .unwrap(),
            PegBidDecision::Skipped(PegBidSkipReason::Debounced { .. })
        ));

        // Meaningful move ($85.05 -> target $85.06, delta = 5c >= 2c) reprices cleanly.
        let t3 = t(12, 7);
        strat.ingest_buy_order(BuyOrder {
            platform: Platform::CsFloat,
            item: item(),
            float_bucket: None,
            price: Money(8_505),
            quantity: 1,
            observed_at: t3,
        });
        let PegBidDecision::Placed(updated) = strat
            .evaluate_item(Platform::CsFloat, &item(), None, &schedules, t3)
            .unwrap()
        else {
            panic!("expected reprice after cooldown and debounce threshold");
        };
        assert_eq!(updated.order.limit_price, Money(8_506));
    }

    #[test]
    fn rejects_non_allowed_platform_and_validates_config() {
        let now = t(12, 0);
        let mut strat = PegBidStrategy::new(PegBidConfig::default()).unwrap();
        let decision = strat
            .evaluate_item(Platform::DMarket, &item(), None, &fees(), now)
            .unwrap();
        assert!(matches!(
            decision,
            PegBidDecision::Skipped(PegBidSkipReason::PlatformNotAllowed(Platform::DMarket))
        ));

        assert!(PegBidStrategy::new(PegBidConfig {
            target_margin_pct: 0.0,
            ..PegBidConfig::default()
        })
        .is_err());
    }
    fn sale_at(id: &str, price: i64, float_value: Option<f64>, at: DateTime<Utc>) -> Sale {
        Sale {
            id: id.into(),
            platform: Platform::CsFloat,
            item: item(),
            price: Money(price),
            float_value,
            sold_at: at,
            verified: true,
        }
    }

    fn ask_at(price: i64, bucket: Option<FloatBucket>, at: DateTime<Utc>) -> PriceSnapshot {
        PriceSnapshot {
            platform: Platform::CsFloat,
            item: item(),
            float_bucket: bucket,
            price: Money(price),
            observed_at: at,
        }
    }

    #[test]
    fn asks_above_sales_cannot_raise_the_exit_value_but_asks_below_cap_it() {
        let now = t(12, 0);
        let low = Some(FloatBucket::FieldTestedLow);
        let mut strategy = PegBidStrategy::new(PegBidConfig::default()).unwrap();
        for i in 0..3 {
            let at = now - Duration::hours(i + 1);
            strategy.observe_sale(&sale_at(&format!("s{i}"), 9_000, Some(0.16), at));
            strategy.observe_snapshot(&ask_at(11_000, low, at));
        }
        // Sellers asking 11_000 do not mean buyers pay it; prints say 9_000.
        let (value, count) = strategy
            .rolling_median(Platform::CsFloat, &item(), low, now)
            .unwrap();
        assert_eq!((value, count), (Money(9_000), 3));

        for i in 0..4 {
            strategy.observe_snapshot(&ask_at(8_500, low, now - Duration::minutes(i + 1)));
        }
        // Seven asks: median 8_500 is under the prints, so sellers are already undercutting.
        let (value, _) = strategy
            .rolling_median(Platform::CsFloat, &item(), low, now)
            .unwrap();
        assert_eq!(value, Money(8_500));
    }

    #[test]
    fn tier_only_bids_are_valued_at_a_lower_quantile_of_every_float_in_the_tier() {
        let now = t(12, 0);
        let mut strategy = PegBidStrategy::new(PegBidConfig::default()).unwrap();
        let prints = [
            (8_000, 0.37),
            (9_000, 0.36),
            (10_000, 0.16),
            (11_000, 0.17),
            (12_000, 0.18),
        ];
        for (i, (price, float)) in prints.into_iter().enumerate() {
            let at = now - Duration::hours(i as i64 + 1);
            strategy.observe_sale(&sale_at(&format!("s{i}"), price, Some(float), at));
        }
        // A tier-only bid fills on any float, so its exit is the 25th percentile of all five.
        let tier = strategy.rolling_median(Platform::CsFloat, &item(), None, now);
        assert_eq!(tier, Some((Money(9_000), 5)));
        // A bucket-qualified bid only receives that bucket and keeps its own median.
        let low = strategy.rolling_median(
            Platform::CsFloat,
            &item(),
            Some(FloatBucket::FieldTestedLow),
            now,
        );
        assert_eq!(low, Some((Money(11_000), 3)));
    }

    #[test]
    fn asks_alone_value_an_exit_only_through_an_explicit_haircut() {
        let now = t(12, 0);
        let asks = |strategy: &mut PegBidStrategy| {
            for i in 0..3 {
                strategy.observe_snapshot(&ask_at(10_000, None, now - Duration::hours(i + 1)));
            }
        };
        let mut strict = PegBidStrategy::new(PegBidConfig::default()).unwrap();
        asks(&mut strict);
        assert_eq!(
            strict.rolling_median(Platform::CsFloat, &item(), None, now),
            None
        );

        let mut haircut = PegBidStrategy::new(PegBidConfig {
            ask_only_exit_haircut_pct: Some(10.0),
            ..PegBidConfig::default()
        })
        .unwrap();
        asks(&mut haircut);
        assert_eq!(
            haircut.rolling_median(Platform::CsFloat, &item(), None, now),
            Some((Money(9_000), 3))
        );
    }

    #[test]
    fn rejects_out_of_range_exit_valuation_settings() {
        for config in [
            PegBidConfig {
                tier_only_exit_quantile: 0.0,
                ..PegBidConfig::default()
            },
            PegBidConfig {
                tier_only_exit_quantile: 0.6,
                ..PegBidConfig::default()
            },
            PegBidConfig {
                ask_only_exit_haircut_pct: Some(100.0),
                ..PegBidConfig::default()
            },
            PegBidConfig {
                ask_only_exit_haircut_pct: Some(f64::NAN),
                ..PegBidConfig::default()
            },
        ] {
            assert!(config.validate().is_err(), "{config:?}");
        }
    }
}
