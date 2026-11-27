//! Passive buy-order fill simulator (mock simulation only).
//!
//! Unlike aggressive market/limit buys in [`crate::MockExecutor`], resting peg
//! bids are passive:
//!
//! 1. Placing a bid never fills immediately.
//! 2. A resting buy order only fills when a verified historical or live
//!    transaction prints AT or BELOW the bid price after placement.
//! 3. Minimum trailing liquidity is enforced (`>= 15` recorded sales in 7 days
//!    by default).
//! 4. On exact price ties (`sale.price == bid`), queue priority is simulated
//!    via deterministic queue latency and ahead-in-queue prints rather than
//!    assuming instant first-in-line fill.
//! 5. On fill, cost (including buyer fees) is deducted from mock capital and
//!    the item is registered in simulated inventory with `locked_until = now + 7d`.

use std::collections::HashSet;

use chrono::{DateTime, TimeDelta, Utc};
use cs2_arb_core::{
    Error, FeeSchedules, Fill, FloatBucket, ItemId, Money, Order, OrderSide, Platform, Position,
    Result, Sale, SimulationMode,
};
use serde::Serialize;

use crate::config::{valve_trade_hold, LiquidityConfig, LiquidityScope};
use crate::executor::ExecutionReport;
use crate::fees::{PlatformFees, RoundingMode};
use crate::liquidity::LiquidityStats;
use crate::portfolio::{proportional_cost, LedgerEntry, Portfolio};
use crate::rejection::{Rejection, RejectionRecord};
use crate::rng::SplitMix64;
use crate::wear::order_matches_float;

/// Configuration for [`BuyOrderFillSimulator`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuyOrderFillConfig {
    /// Minimum recorded sales in the trailing 7-day window. Default 15.
    pub min_recorded_sales_7d: u32,
    /// Liquidity lookback window in days. Default 7.
    pub liquidity_window_days: u32,
    /// Whether only `sale.verified` transactions can fill bids and count toward
    /// the liquidity gate. Default true.
    pub require_verified_sales: bool,
    /// Minimum simulated queue latency before a price-tied bid can fill.
    /// Default 15 minutes (never instant first-in-line).
    pub tie_queue_latency_min: TimeDelta,
    /// Maximum simulated queue latency before a price-tied bid can fill.
    /// Default 2 hours.
    pub tie_queue_latency_max: TimeDelta,
    /// Minimum number of prints at the tied price required before our resting
    /// bid reaches the front of the queue. Default 2.
    pub tie_min_prints_ahead: u32,
    /// Optional starting mock capital balance in USD cents.
    pub starting_cash: Option<Money>,
    /// Fee rounding mode. Default `HalfUp`.
    pub fee_rounding: RoundingMode,
    /// Whether a tier-only buy order (`float_bucket: None`) matches sales of any
    /// float within the same `WearTier`. Default true (marketplace buy orders
    /// like CSFloat/Skinport are commonly tier-level).
    pub tier_only_matches_any_float: bool,
}

impl Default for BuyOrderFillConfig {
    fn default() -> Self {
        Self {
            min_recorded_sales_7d: 15,
            liquidity_window_days: 7,
            require_verified_sales: true,
            tie_queue_latency_min: TimeDelta::minutes(15),
            tie_queue_latency_max: TimeDelta::hours(2),
            tie_min_prints_ahead: 2,
            starting_cash: None,
            fee_rounding: RoundingMode::HalfUp,
            tier_only_matches_any_float: true,
        }
    }
}

impl BuyOrderFillConfig {
    pub fn validate(&self) -> Result<()> {
        let bad = |msg: &str| Err(Error::InvalidConfig(format!("buy_order_fill: {msg}")));
        if self.liquidity_window_days == 0 {
            return bad("liquidity_window_days must be positive");
        }
        if self.tie_queue_latency_min < TimeDelta::zero()
            || self.tie_queue_latency_max < self.tie_queue_latency_min
        {
            return bad("tie_queue_latency must satisfy 0 <= min <= max");
        }
        if self.tie_min_prints_ahead == 0 {
            return bad("tie_min_prints_ahead must be >= 1");
        }
        if self.starting_cash.is_some_and(|cash| cash.0 < 0) {
            return bad("starting_cash must be nonnegative");
        }
        Ok(())
    }
}

/// Lifecycle state of a simulated passive buy order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BuyOrderStatus {
    Resting,
    Filled,
    Cancelled,
}

/// A mock passive buy order resting on the simulated queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RestingBuyOrder {
    pub order: Order,
    pub placed_at: DateTime<Utc>,
    /// Earliest timestamp a price-tied print (`sale.price == limit_price`) may fill
    /// this order after simulated queue latency.
    pub tie_ready_at: DateTime<Utc>,
    /// Number of verified prints observed at the exact tied price since placement.
    pub tie_prints_seen: u32,
    /// Remaining unfilled units on this passive buy order.
    pub remaining_quantity: u32,
    pub status: BuyOrderStatus,
}

/// Deterministic passive buy-order fill and inventory simulator.
#[derive(Debug)]
pub struct BuyOrderFillSimulator {
    _mode: SimulationMode,
    config: BuyOrderFillConfig,
    fees: PlatformFees,
    rng: SplitMix64,
    portfolio: Portfolio,
    resting_bids: Vec<RestingBuyOrder>,
    sales_tape: Vec<Sale>,
    seen_sale_ids: HashSet<String>,
    filled_order_ids: HashSet<String>,
    rejections: Vec<RejectionRecord>,
}

impl BuyOrderFillSimulator {
    pub fn new(
        mode: SimulationMode,
        config: BuyOrderFillConfig,
        fees: &FeeSchedules,
        seed: u64,
    ) -> Result<Self> {
        config.validate()?;
        let fees = PlatformFees::from_schedules(fees, config.fee_rounding)?;
        Ok(Self {
            _mode: mode,
            portfolio: Portfolio::new(config.starting_cash),
            config,
            fees,
            rng: SplitMix64::new(seed),
            resting_bids: Vec::new(),
            sales_tape: Vec::new(),
            seen_sale_ids: HashSet::new(),
            filled_order_ids: HashSet::new(),
            rejections: Vec::new(),
        })
    }

    pub fn config(&self) -> &BuyOrderFillConfig {
        &self.config
    }

    pub fn portfolio(&self) -> &Portfolio {
        &self.portfolio
    }

    pub fn cash(&self) -> Option<Money> {
        self.portfolio.cash()
    }

    pub fn rejections(&self) -> &[RejectionRecord] {
        &self.rejections
    }

    /// Currently resting (unfilled, uncancelled) mock buy orders.
    pub fn active_bids(&self) -> Vec<&RestingBuyOrder> {
        self.resting_bids
            .iter()
            .filter(|b| b.status == BuyOrderStatus::Resting)
            .collect()
    }

    /// All recorded historical/live sales known to the simulator.
    pub fn sales_tape(&self) -> &[Sale] {
        &self.sales_tape
    }

    /// Add verified/unverified sales to the simulator's liquidity tape (deduplicated by `sale.id`).
    pub fn record_sales(&mut self, sales: impl IntoIterator<Item = Sale>) {
        for sale in sales {
            if self.seen_sale_ids.insert(sale.id.clone()) {
                self.sales_tape.push(sale);
            }
        }
    }

    /// Cancel any resting passive buy orders for the given `(platform, item, float_bucket)`.
    ///
    /// Returns the number of resting bids cancelled.
    pub fn cancel_bids_for_item(
        &mut self,
        platform: Platform,
        item: &ItemId,
        float_bucket: Option<FloatBucket>,
    ) -> usize {
        let mut cancelled = 0;
        for existing in &mut self.resting_bids {
            if existing.status == BuyOrderStatus::Resting
                && existing.order.platform == platform
                && &existing.order.item == item
                && existing.order.float_bucket == float_bucket
            {
                existing.status = BuyOrderStatus::Cancelled;
                cancelled += 1;
            }
        }
        cancelled
    }

    /// Place (or reprice) a passive mock buy order. Never fills immediately.
    pub fn place_bid(
        &mut self,
        order: Order,
        now: DateTime<Utc>,
    ) -> std::result::Result<RestingBuyOrder, Rejection> {
        if order.side != OrderSide::Buy {
            return Err(Rejection::InvalidLimitPrice {
                limit: order.limit_price,
            });
        }
        if self.filled_order_ids.contains(&order.id) {
            return Err(Rejection::DuplicateOrderId { order_id: order.id });
        }
        if order.quantity == 0 {
            return Err(Rejection::ZeroQuantity);
        }
        if order.limit_price.0 <= 0 {
            return Err(Rejection::InvalidLimitPrice {
                limit: order.limit_price,
            });
        }
        if let Some(bucket) = order.float_bucket {
            if bucket.wear_tier() != order.item.wear_tier {
                return Err(Rejection::OrderWearInconsistent {
                    tier: order.item.wear_tier,
                    bucket,
                });
            }
        }

        // Cancel any previous resting bid on the same (platform, item, float_bucket);
        // repricing resets queue priority.
        for existing in &mut self.resting_bids {
            if existing.status == BuyOrderStatus::Resting
                && existing.order.platform == order.platform
                && existing.order.item == order.item
                && existing.order.float_bucket == order.float_bucket
            {
                existing.status = BuyOrderStatus::Cancelled;
            }
        }

        let tie_ready_at = now + self.draw_tie_latency();
        let resting = RestingBuyOrder {
            remaining_quantity: order.quantity,
            order,
            placed_at: now,
            tie_ready_at,
            tie_prints_seen: 0,
            status: BuyOrderStatus::Resting,
        };
        self.resting_bids.push(resting.clone());
        Ok(resting)
    }

    /// Process a single verified historical/live transaction against resting bids.
    ///
    /// Also records `sale` into the liquidity tape. Returns any fills triggered
    /// by this transaction.
    pub fn process_transaction(&mut self, sale: &Sale, now: DateTime<Utc>) -> Vec<ExecutionReport> {
        self.record_sales([sale.clone()]);

        if sale.sold_at > now {
            return Vec::new();
        }
        if self.config.require_verified_sales && !sale.verified {
            return Vec::new();
        }

        let mut reports = Vec::new();
        let bid_count = self.resting_bids.len();

        for idx in 0..bid_count {
            let (order, placed_at, tie_ready_at, remaining_quantity) = {
                let bid = &self.resting_bids[idx];
                if bid.status != BuyOrderStatus::Resting || bid.remaining_quantity == 0 {
                    continue;
                }
                if bid.order.platform != sale.platform || bid.order.item != sale.item {
                    continue;
                }
                if sale.sold_at <= bid.placed_at {
                    continue;
                }
                if !order_matches_float(
                    bid.order.item.wear_tier,
                    bid.order.float_bucket,
                    sale.item.wear_tier,
                    sale.float_value,
                    self.config.tier_only_matches_any_float,
                ) {
                    continue;
                }
                if sale.price > bid.order.limit_price {
                    continue;
                }
                (
                    bid.order.clone(),
                    bid.placed_at,
                    bid.tie_ready_at,
                    bid.remaining_quantity,
                )
            };

            let _ = placed_at;
            let is_price_tie = sale.price == order.limit_price;
            if is_price_tie {
                self.resting_bids[idx].tie_prints_seen =
                    self.resting_bids[idx].tie_prints_seen.saturating_add(1);
                let prints = self.resting_bids[idx].tie_prints_seen;
                if sale.sold_at < tie_ready_at || prints < self.config.tie_min_prints_ahead {
                    continue;
                }
            }

            // Enforce minimum 7-day recorded sales liquidity requirement (>= 15 by default).
            let stats = self.assess_liquidity(&order, sale.sold_at);
            if !stats.meets_minimum {
                self.rejections.push(RejectionRecord {
                    order_id: order.id.clone(),
                    rejected_at: sale.sold_at,
                    rejection: Rejection::InsufficientVerifiedSales {
                        found: stats.verified_sales,
                        required: self.config.min_recorded_sales_7d,
                        window_days: stats.window_days,
                    },
                });
                continue;
            }

            // A single historical/live sale print represents one traded skin unit.
            let fill_qty = 1u32;
            let Ok(fees) = self
                .fees
                .get(order.platform)
                .buy(order.limit_price, fill_qty)
            else {
                continue;
            };

            if let Some(cash) = self.portfolio.cash() {
                if fees.net > cash {
                    self.rejections.push(RejectionRecord {
                        order_id: order.id.clone(),
                        rejected_at: sale.sold_at,
                        rejection: Rejection::InsufficientFunds {
                            required: fees.net,
                            available: cash,
                        },
                    });
                    continue;
                }
            }

            let Ok(cash_delta) = Money(0).checked_sub(fees.net) else {
                continue;
            };

            let filled_at = sale.sold_at;
            let fill_index = order.quantity - remaining_quantity + 1;
            let position_id = if order.quantity == 1 {
                format!("{}#pos", order.id)
            } else {
                format!("{}#pos-{fill_index}", order.id)
            };
            let position = Position {
                id: position_id,
                item: order.item.clone(),
                float_bucket: order.float_bucket,
                acquired_on: order.platform,
                quantity: fill_qty,
                cost_basis: fees.net,
                acquired_at: filled_at,
                locked_until: filled_at + valve_trade_hold(),
            };
            let entry = LedgerEntry {
                seq: self.portfolio.next_seq(),
                order_id: order.id.clone(),
                side: OrderSide::Buy,
                platform: order.platform,
                item: order.item.clone(),
                float_bucket: order.float_bucket,
                quantity: fill_qty,
                unit_price: order.limit_price,
                unit_fee: fees.unit_fee,
                gross: fees.gross,
                fee: fees.fee,
                cash_delta,
                cost_basis_released: None,
                realized_pnl: None,
                filled_at,
                listings: vec![(sale.id.clone(), fill_qty)],
                positions: vec![(position.id.clone(), fill_qty)],
            };

            if self
                .portfolio
                .record_buy(position.clone(), entry.clone())
                .is_ok()
            {
                let new_remaining = remaining_quantity - fill_qty;
                self.resting_bids[idx].remaining_quantity = new_remaining;
                if new_remaining == 0 {
                    self.resting_bids[idx].status = BuyOrderStatus::Filled;
                    self.filled_order_ids.insert(order.id.clone());
                }
                reports.push(ExecutionReport {
                    fill: Fill {
                        order,
                        quantity: fill_qty,
                        unit_price: entry.unit_price,
                        fee: fees.fee,
                        filled_at,
                    },
                    fees,
                    liquidity: stats,
                    ledger_entry: entry,
                    position: Some(position),
                });
                break;
            }
        }

        reports
    }

    /// Process a batch of transactions in chronological order.
    pub fn process_sales(&mut self, sales: &[Sale], now: DateTime<Utc>) -> Vec<ExecutionReport> {
        let mut sorted: Vec<&Sale> = sales.iter().filter(|s| s.sold_at <= now).collect();
        sorted.sort_by_key(|s| s.sold_at);
        let mut reports = Vec::new();
        for sale in sorted {
            reports.extend(self.process_transaction(sale, now));
        }
        reports
    }

    /// Settle a simulated ask fill for an unlocked position in this simulator's portfolio.
    ///
    /// Enforces the 7-day trade lock (`now >= position.locked_until`), deducts seller fees,
    /// credits net proceeds to mock capital, and records realized net P&L.
    pub fn settle_relist_sale(
        &mut self,
        position_id: &str,
        platform: Platform,
        unit_price: Money,
        now: DateTime<Utc>,
    ) -> std::result::Result<ExecutionReport, Rejection> {
        if unit_price.0 <= 0 {
            return Err(Rejection::InvalidLimitPrice { limit: unit_price });
        }
        let Some((pos_idx, position)) = self
            .portfolio
            .positions()
            .iter()
            .enumerate()
            .find(|(_, p)| p.id == position_id)
            .map(|(i, p)| (i, p.clone()))
        else {
            return Err(Rejection::InsufficientInventory {
                requested: 1,
                available: 0,
            });
        };

        if position.is_trade_locked_at(now) {
            return Err(Rejection::PositionTradeLocked {
                requested: position.quantity,
                unlocked: 0,
                earliest_unlock: position.locked_until,
            });
        }

        let fees = self
            .fees
            .get(platform)
            .sell(unit_price, position.quantity)?;
        if fees.net.0 <= 0 {
            return Err(Rejection::NonPositiveProceeds {
                unit_price,
                unit_fee: fees.unit_fee,
            });
        }

        let released = proportional_cost(position.cost_basis, position.quantity, position.quantity);
        let realized_pnl = fees.net.checked_sub(released)?;
        let order_id = format!("relist-sell:{position_id}:{}", now.timestamp_millis());
        let order = Order {
            id: order_id.clone(),
            side: OrderSide::Sell,
            platform,
            item: position.item.clone(),
            float_bucket: position.float_bucket,
            quantity: position.quantity,
            limit_price: unit_price,
            submitted_at: now,
        };
        let stats = self.assess_liquidity(&order, now);
        let entry = LedgerEntry {
            seq: self.portfolio.next_seq(),
            order_id: order_id.clone(),
            side: OrderSide::Sell,
            platform,
            item: position.item.clone(),
            float_bucket: position.float_bucket,
            quantity: position.quantity,
            unit_price,
            unit_fee: fees.unit_fee,
            gross: fees.gross,
            fee: fees.fee,
            cash_delta: fees.net,
            cost_basis_released: Some(released),
            realized_pnl: Some(realized_pnl),
            filled_at: now,
            listings: Vec::new(),
            positions: vec![(position.id.clone(), position.quantity)],
        };
        self.portfolio
            .record_sell(entry.clone(), &[(pos_idx, position.quantity, released)])?;
        self.filled_order_ids.insert(order_id);

        Ok(ExecutionReport {
            fill: Fill {
                order,
                quantity: position.quantity,
                unit_price,
                fee: fees.fee,
                filled_at: now,
            },
            fees,
            liquidity: stats,
            ledger_entry: entry,
            position: None,
        })
    }

    fn assess_liquidity(&self, order: &Order, at: DateTime<Utc>) -> LiquidityStats {
        let window_start = at - TimeDelta::days(i64::from(self.config.liquidity_window_days));
        let count = self
            .sales_tape
            .iter()
            .filter(|sale| {
                (!self.config.require_verified_sales || sale.verified)
                    && sale.item == order.item
                    && sale.platform == order.platform
                    && sale.sold_at > window_start
                    && sale.sold_at <= at
                    && order_matches_float(
                        order.item.wear_tier,
                        order.float_bucket,
                        sale.item.wear_tier,
                        sale.float_value,
                        self.config.tier_only_matches_any_float,
                    )
            })
            .count();
        let verified_sales = u32::try_from(count).unwrap_or(u32::MAX);
        let liq_cfg = LiquidityConfig {
            min_verified_sales: self.config.min_recorded_sales_7d,
            window_days: self.config.liquidity_window_days,
            max_adv_fraction_bps: 10_000,
            scope: LiquidityScope::OrderPlatform,
        };
        LiquidityStats {
            verified_sales,
            window_days: liq_cfg.window_days,
            max_order_quantity: if verified_sales >= self.config.min_recorded_sales_7d {
                order.quantity
            } else {
                0
            },
            meets_minimum: verified_sales >= self.config.min_recorded_sales_7d,
        }
    }

    fn draw_tie_latency(&mut self) -> TimeDelta {
        let min = self.config.tie_queue_latency_min;
        let max = self.config.tie_queue_latency_max;
        let span_ms = (max - min).num_milliseconds().max(0) as u64;
        let offset = self.rng.below(span_ms.saturating_add(1)) as i64;
        min + TimeDelta::milliseconds(offset)
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use cs2_arb_core::{ItemId, WearTier};

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

    fn t(day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 2, day, hour, minute, 0).unwrap()
    }

    fn bid_order(id: &str, price: i64, at: DateTime<Utc>) -> Order {
        Order {
            id: id.into(),
            side: OrderSide::Buy,
            platform: Platform::CsFloat,
            item: item(),
            float_bucket: None,
            quantity: 1,
            limit_price: Money(price),
            submitted_at: at,
        }
    }

    fn make_sale(id: &str, price: i64, sold_at: DateTime<Utc>, verified: bool) -> Sale {
        Sale {
            id: id.into(),
            platform: Platform::CsFloat,
            item: item(),
            price: Money(price),
            float_value: Some(0.20),
            sold_at,
            verified,
        }
    }

    fn seed_liquidity(sim: &mut BuyOrderFillSimulator, count: usize, before: DateTime<Utc>) {
        let sales = (1..=count).map(|i| {
            make_sale(
                &format!("hist-{i}"),
                9_500,
                before - TimeDelta::hours(i as i64),
                true,
            )
        });
        sim.record_sales(sales);
    }

    #[test]
    fn does_not_fill_immediately_and_requires_verified_print_at_or_below_bid() {
        let now = t(10, 12, 0);
        let config = BuyOrderFillConfig {
            starting_cash: Some(Money(50_000)),
            ..BuyOrderFillConfig::default()
        };
        let mut sim = BuyOrderFillSimulator::new(SimulationMode, config, &fees(), 42).unwrap();
        seed_liquidity(&mut sim, 15, now);

        // Placement never fills immediately.
        let resting = sim.place_bid(bid_order("bid-1", 8_500, now), now).unwrap();
        assert_eq!(resting.status, BuyOrderStatus::Resting);
        assert_eq!(sim.active_bids().len(), 1);
        assert_eq!(sim.portfolio().positions().len(), 0);
        assert_eq!(sim.cash(), Some(Money(50_000)));

        // Print above bid does not fill.
        let above = make_sale("s1", 8_600, t(10, 12, 5), true);
        assert!(sim.process_transaction(&above, t(10, 12, 5)).is_empty());

        // Unverified print below bid does not fill.
        let unverified = make_sale("s2", 8_400, t(10, 12, 10), false);
        assert!(sim
            .process_transaction(&unverified, t(10, 12, 10))
            .is_empty());

        // Verified print strictly below bid fills, deducts cash, and locks for 7 days.
        let below = make_sale("s3", 8_450, t(10, 12, 15), true);
        let fills = sim.process_transaction(&below, t(10, 12, 15));
        assert_eq!(fills.len(), 1);
        assert_eq!(sim.active_bids().len(), 0);
        assert_eq!(sim.cash(), Some(Money(50_000 - 8_500)));
        assert_eq!(sim.portfolio().positions().len(), 1);
        let pos = &sim.portfolio().positions()[0];
        assert_eq!(pos.acquired_at, t(10, 12, 15));
        assert_eq!(pos.locked_until, t(10, 12, 15) + TimeDelta::days(7));
    }

    #[test]
    fn enforces_fifteen_sales_seven_day_liquidity_gate() {
        let now = t(10, 12, 0);
        let mut sim =
            BuyOrderFillSimulator::new(SimulationMode, BuyOrderFillConfig::default(), &fees(), 7)
                .unwrap();
        // Only 13 historical sales + 1 triggering print = 14 < 15 -> must not fill.
        seed_liquidity(&mut sim, 13, now);
        sim.place_bid(bid_order("bid-low-liq", 8_500, now), now)
            .unwrap();

        let print1 = make_sale("p1", 8_400, t(10, 12, 5), true);
        assert!(sim.process_transaction(&print1, t(10, 12, 5)).is_empty());
        assert_eq!(sim.portfolio().positions().len(), 0);

        // Next verified print brings 7d recorded sales to 15 -> fills cleanly.
        let print2 = make_sale("p2", 8_400, t(10, 12, 10), true);
        let fills = sim.process_transaction(&print2, t(10, 12, 10));
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].liquidity.verified_sales, 15);
    }

    #[test]
    fn price_tie_simulates_queue_latency_instead_of_instant_fill() {
        let now = t(10, 12, 0);
        let config = BuyOrderFillConfig {
            tie_queue_latency_min: TimeDelta::minutes(30),
            tie_queue_latency_max: TimeDelta::minutes(30),
            tie_min_prints_ahead: 2,
            ..BuyOrderFillConfig::default()
        };
        let mut sim = BuyOrderFillSimulator::new(SimulationMode, config, &fees(), 99).unwrap();
        seed_liquidity(&mut sim, 20, now);

        let resting = sim
            .place_bid(bid_order("bid-tie", 8_500, now), now)
            .unwrap();
        assert_eq!(resting.tie_ready_at, t(10, 12, 30));

        // First tie print at +5 min: latency not elapsed -> does not fill.
        let tie1 = make_sale("t1", 8_500, t(10, 12, 5), true);
        assert!(sim.process_transaction(&tie1, t(10, 12, 5)).is_empty());

        // Second tie print at +20 min: still before tie_ready_at -> does not fill.
        let tie2 = make_sale("t2", 8_500, t(10, 12, 20), true);
        assert!(sim.process_transaction(&tie2, t(10, 12, 20)).is_empty());

        // Third tie print at +35 min (>= tie_ready_at and >= 2 prints) -> fills!
        let tie3 = make_sale("t3", 8_500, t(10, 12, 35), true);
        let fills = sim.process_transaction(&tie3, t(10, 12, 35));
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].fill.unit_price, Money(8_500));
    }

    #[test]
    fn settle_relist_sale_enforces_seven_day_lock_and_credits_net_proceeds() {
        let now = t(10, 12, 0);
        let config = BuyOrderFillConfig {
            starting_cash: Some(Money(20_000)),
            ..BuyOrderFillConfig::default()
        };
        let mut sim = BuyOrderFillSimulator::new(SimulationMode, config, &fees(), 1).unwrap();
        seed_liquidity(&mut sim, 20, now);
        sim.place_bid(bid_order("bid-lock", 8_500, now), now)
            .unwrap();
        let fill_time = t(10, 12, 10);
        let fills = sim.process_transaction(&make_sale("f1", 8_400, fill_time, true), fill_time);
        let pos_id = fills[0].position.as_ref().unwrap().id.clone();

        // Day 6: still trade locked -> rejected.
        let day6 = fill_time + TimeDelta::days(6);
        assert!(matches!(
            sim.settle_relist_sale(&pos_id, Platform::CsFloat, Money(10_000), day6),
            Err(Rejection::PositionTradeLocked { .. })
        ));

        // Day 7: unlocked -> sells at $100.00, 2% CSFloat seller fee = $2.00 -> net $98.00.
        // Realized net P&L = $98.00 - $85.00 = $13.00 (1_300c).
        let day7 = fill_time + TimeDelta::days(7);
        let report = sim
            .settle_relist_sale(&pos_id, Platform::CsFloat, Money(10_000), day7)
            .unwrap();
        assert_eq!(report.fees.fee, Money(200));
        assert_eq!(report.fees.net, Money(9_800));
        assert_eq!(report.ledger_entry.realized_pnl, Some(Money(1_300)));
        assert_eq!(sim.portfolio().realized_pnl(), Money(1_300));
        assert_eq!(sim.cash(), Some(Money(20_000 - 8_500 + 9_800)));
    }
}
