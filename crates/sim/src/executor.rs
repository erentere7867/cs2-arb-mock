//! The mock executor: deterministic fills against a [`MarketBook`].

use std::collections::HashSet;
use std::sync::Arc;

use chrono::{DateTime, TimeDelta, Utc};
use cs2_arb_core::{
    FeeSchedules, Fill, FloatBucket, ItemId, Listing, Money, Order, OrderSide, Platform, Position,
    Result, SimulationMode,
};
use serde::Serialize;

use crate::book::MarketBook;
use crate::clock::Clock;
use crate::config::{valve_trade_hold, SimConfig};
use crate::fees::{FeeBreakdown, PlatformFees};
use crate::liquidity::{self, LiquidityStats};
use crate::portfolio::{proportional_cost, LedgerEntry, Portfolio};
use crate::rejection::{Rejection, RejectionRecord};
use crate::rng::SplitMix64;
use crate::wear::{order_matches, order_matches_float};

/// Everything produced by an accepted order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExecutionReport {
    /// `fill.fee` is the **total** marketplace fee for the fill (buyer fee on a
    /// buy, seller fee on a sell), not a per-unit fee.
    pub fill: Fill,
    pub fees: FeeBreakdown,
    pub liquidity: LiquidityStats,
    pub ledger_entry: LedgerEntry,
    /// Buys only: the inventory position opened (locked for seven days).
    pub position: Option<Position>,
}

/// In-process, mock-only order executor. It has no network access and no way to
/// place a real order: `submit` only mutates this struct's simulated state.
///
/// Given the same seed, config, fees, clock readings, market data, and order
/// sequence, every result is identical.
#[derive(Debug)]
pub struct MockExecutor {
    _mode: SimulationMode,
    config: SimConfig,
    fees: PlatformFees,
    order_fees: Option<FeeSchedules>,
    clock: Arc<dyn Clock>,
    rng: SplitMix64,
    book: MarketBook,
    portfolio: Portfolio,
    filled_order_ids: HashSet<String>,
    rejections: Vec<RejectionRecord>,
}

impl MockExecutor {
    /// Build an executor from core fee schedules (as loaded from config).
    pub fn new(
        mode: SimulationMode,
        config: SimConfig,
        fees: &FeeSchedules,
        clock: Arc<dyn Clock>,
        seed: u64,
    ) -> Result<Self> {
        let fees = PlatformFees::from_schedules(fees, config.fee_rounding)?;
        Self::with_platform_fees(mode, config, fees, clock, seed)
    }

    /// Build an executor from already-exact fees. `fees`' own rounding is used.
    pub fn with_platform_fees(
        mode: SimulationMode,
        config: SimConfig,
        fees: PlatformFees,
        clock: Arc<dyn Clock>,
        seed: u64,
    ) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            _mode: mode,
            portfolio: Portfolio::new(config.starting_cash),
            config,
            fees,
            order_fees: None,
            clock,
            rng: SplitMix64::new(seed),
            book: MarketBook::new(),
            filled_order_ids: HashSet::new(),
            rejections: Vec::new(),
        })
    }

    pub fn config(&self) -> &SimConfig {
        &self.config
    }

    pub fn fees(&self) -> &PlatformFees {
        &self.fees
    }

    /// Charge platform fees once per order and funding fees per unit during replay.
    pub fn with_order_fees(mut self, schedules: FeeSchedules) -> Self {
        self.order_fees = Some(schedules);
        self
    }

    pub fn buy_cost_at_limit(&self, order: &Order) -> Result<Money> {
        Ok(self
            .execution_fees(order, order.limit_price, order.quantity)?
            .net)
    }

    fn execution_fees(
        &self,
        order: &Order,
        unit_price: Money,
        quantity: u32,
    ) -> Result<FeeBreakdown> {
        let Some(schedules) = &self.order_fees else {
            let schedule = self.fees.get(order.platform);
            return match order.side {
                OrderSide::Buy => schedule.buy(unit_price, quantity),
                OrderSide::Sell => schedule.sell(unit_price, quantity),
            };
        };
        let schedule = schedules.get(order.platform);
        let gross = unit_price.checked_mul(quantity)?;
        let (fee, net) = match order.side {
            OrderSide::Buy => {
                let fee = schedule.buyer_order_fee(unit_price, quantity)?;
                (fee, gross.checked_add(fee)?)
            }
            OrderSide::Sell => {
                let fee = schedule.seller_order_fee(unit_price, quantity)?;
                (fee, gross.checked_sub(fee)?)
            }
        };
        Ok(FeeBreakdown {
            quantity,
            unit_price,
            unit_fee: Money(fee.0.checked_div(i64::from(quantity)).unwrap_or(0)),
            gross,
            fee,
            net,
        })
    }

    pub fn now(&self) -> DateTime<Utc> {
        self.clock.now()
    }

    pub fn book(&self) -> &MarketBook {
        &self.book
    }

    /// Feed listings and sales here.
    pub fn book_mut(&mut self) -> &mut MarketBook {
        &mut self.book
    }

    pub fn portfolio(&self) -> &Portfolio {
        &self.portfolio
    }

    pub fn rejections(&self) -> &[RejectionRecord] {
        &self.rejections
    }

    /// Liquidity the gate would see for an order right now.
    pub fn liquidity(
        &self,
        platform: Platform,
        item: &ItemId,
        bucket: Option<FloatBucket>,
    ) -> LiquidityStats {
        liquidity::assess(
            &self.config.liquidity,
            self.book.sales_for(item),
            platform,
            item,
            bucket,
            self.clock.now(),
            self.config.tier_only_matches_any_float,
        )
    }

    /// Simulate an order. On rejection no portfolio or book state changes.
    pub fn submit(&mut self, order: Order) -> std::result::Result<Fill, Rejection> {
        self.submit_detailed(order).map(|report| report.fill)
    }

    /// As [`submit`](Self::submit) but returns fees, liquidity, ledger entry and position.
    pub fn submit_detailed(
        &mut self,
        order: Order,
    ) -> std::result::Result<ExecutionReport, Rejection> {
        let now = self.clock.now();
        let result = self.try_execute(&order, now);
        match &result {
            Ok(report) => {
                self.filled_order_ids.insert(order.id.clone());
                tracing::debug!(
                    order_id = %order.id,
                    side = ?order.side,
                    platform = ?order.platform,
                    quantity = report.fill.quantity,
                    unit_price = report.fill.unit_price.0,
                    fee = report.fill.fee.0,
                    "simulated fill"
                );
            }
            Err(rejection) => {
                tracing::debug!(order_id = %order.id, %rejection, "simulated rejection");
                self.rejections.push(RejectionRecord {
                    order_id: order.id.clone(),
                    rejected_at: now,
                    rejection: rejection.clone(),
                });
            }
        }
        result
    }

    fn try_execute(
        &mut self,
        order: &Order,
        now: DateTime<Utc>,
    ) -> std::result::Result<ExecutionReport, Rejection> {
        let (target, stats) = self.executable_target(order, now)?;
        match order.side {
            OrderSide::Buy => self.execute_buy(order, target, stats, now),
            OrderSide::Sell => self.execute_sell(order, target, stats, now),
        }
    }

    pub fn preview_fill(&self, order: &Order) -> std::result::Result<(Money, u32), Rejection> {
        let now = self.clock.now();
        let (target, _) = self.executable_target(order, now)?;
        match order.side {
            OrderSide::Buy => {
                let level = self.buy_level(order, target, now)?;
                Ok((level.price, level.quantity))
            }
            OrderSide::Sell => Ok((order.limit_price, target)),
        }
    }

    fn executable_target(
        &self,
        order: &Order,
        now: DateTime<Utc>,
    ) -> std::result::Result<(u32, LiquidityStats), Rejection> {
        self.validate_order(order)?;
        let stats = self.liquidity_at(order, now);
        if !stats.meets_minimum {
            return Err(Rejection::InsufficientVerifiedSales {
                found: stats.verified_sales,
                required: self.config.liquidity.min_verified_sales,
                window_days: stats.window_days,
            });
        }
        if stats.max_order_quantity == 0 {
            return Err(Rejection::LiquidityCapZero {
                verified_sales: stats.verified_sales,
                window_days: stats.window_days,
            });
        }
        Ok((order.quantity.min(stats.max_order_quantity), stats))
    }

    fn validate_order(&self, order: &Order) -> std::result::Result<(), Rejection> {
        if self.filled_order_ids.contains(&order.id) {
            return Err(Rejection::DuplicateOrderId {
                order_id: order.id.clone(),
            });
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
        Ok(())
    }

    fn liquidity_at(&self, order: &Order, now: DateTime<Utc>) -> LiquidityStats {
        liquidity::assess(
            &self.config.liquidity,
            self.book.sales_for(&order.item),
            order.platform,
            &order.item,
            order.float_bucket,
            now,
            self.config.tier_only_matches_any_float,
        )
    }

    fn wear_ok(&self, order: &Order, listing: &Listing) -> bool {
        order_matches_float(
            order.item.wear_tier,
            order.float_bucket,
            listing.item.wear_tier,
            listing.float_value,
            self.config.tier_only_matches_any_float,
        )
    }

    fn is_stale(&self, listing: &Listing, now: DateTime<Utc>) -> bool {
        now - listing.observed_at > self.config.max_listing_age
    }

    fn fill_time(&mut self, now: DateTime<Utc>) -> DateTime<Utc> {
        let latency = self.config.fill_latency;
        let span_ms = (latency.max - latency.min).num_milliseconds().max(0) as u64;
        let offset = self.rng.below(span_ms.saturating_add(1)) as i64;
        now + latency.min + TimeDelta::milliseconds(offset)
    }

    fn buy_level(
        &self,
        order: &Order,
        target: u32,
        now: DateTime<Utc>,
    ) -> std::result::Result<BuyLevel, Rejection> {
        // Visible listings: right platform/item, observed no later than now, with stock.
        let visible: Vec<&Listing> = self
            .book
            .listings_for(order.platform, &order.item)
            .filter(|l| l.observed_at <= now && l.quantity > 0)
            .collect();
        if visible.is_empty() {
            return Err(Rejection::NoListings);
        }
        let candidates = visible.len() as u32;
        let wear: Vec<&Listing> = visible
            .into_iter()
            .filter(|l| self.wear_ok(order, l))
            .collect();
        if wear.is_empty() {
            return Err(Rejection::WearMismatch { candidates });
        }
        let earliest_unlock = wear
            .iter()
            .filter_map(|l| l.trade_locked_until.filter(|until| *until > now))
            .min();
        let unlocked: Vec<&Listing> = wear
            .into_iter()
            .filter(|l| l.trade_locked_until.is_none_or(|until| until <= now))
            .collect();
        if unlocked.is_empty() {
            return Err(Rejection::ListingTradeLocked {
                earliest_unlock: earliest_unlock.unwrap_or(now),
            });
        }
        let newest_observed_at = unlocked.iter().map(|l| l.observed_at).max().unwrap_or(now);
        let fresh: Vec<&Listing> = unlocked
            .into_iter()
            .filter(|l| !self.is_stale(l, now))
            .collect();
        if fresh.is_empty() {
            return Err(Rejection::StaleListing {
                newest_observed_at,
                max_age_secs: self.config.max_listing_age.num_seconds(),
            });
        }
        let best_ask = fresh
            .iter()
            .map(|l| l.price)
            .min()
            .unwrap_or(Money(i64::MAX));
        if best_ask > order.limit_price {
            return Err(Rejection::LimitBelowBestAsk {
                limit: order.limit_price,
                best_ask,
            });
        }
        // Only the best price level fills, so the fill has one exact unit price.
        let listings: Vec<(String, u32)> = fresh
            .iter()
            .filter(|l| l.price == best_ask)
            .map(|l| (l.id.clone(), l.quantity))
            .collect();
        let depth: u64 = listings.iter().map(|(_, q)| u64::from(*q)).sum();
        let quantity = u32::try_from(depth.min(u64::from(target))).unwrap_or(target);
        Ok(BuyLevel {
            price: best_ask,
            quantity,
            listings,
        })
    }

    fn execute_buy(
        &mut self,
        order: &Order,
        target: u32,
        stats: LiquidityStats,
        now: DateTime<Utc>,
    ) -> std::result::Result<ExecutionReport, Rejection> {
        let BuyLevel {
            price: best_ask,
            quantity,
            listings: mut level,
        } = self.buy_level(order, target, now)?;
        let fees = self.execution_fees(order, best_ask, quantity)?;
        if let Some(cash) = self.portfolio.cash() {
            if fees.net > cash {
                return Err(Rejection::InsufficientFunds {
                    required: fees.net,
                    available: cash,
                });
            }
        }
        let cash_delta = Money(0).checked_sub(fees.net)?;

        // All checks passed: consume RNG, then mutate.
        level.sort();
        self.rng.shuffle(&mut level);
        let mut remaining = quantity;
        let mut taken = Vec::new();
        for (id, available) in level {
            if remaining == 0 {
                break;
            }
            let take = available.min(remaining);
            remaining -= take;
            taken.push((id, take));
        }
        let filled_at = self.fill_time(now);
        let position = Position {
            id: format!("{}#pos", order.id),
            item: order.item.clone(),
            float_bucket: order.float_bucket,
            acquired_on: order.platform,
            quantity,
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
            quantity,
            unit_price: best_ask,
            unit_fee: fees.unit_fee,
            gross: fees.gross,
            fee: fees.fee,
            cash_delta,
            cost_basis_released: None,
            realized_pnl: None,
            filled_at,
            listings: taken.clone(),
            positions: vec![(position.id.clone(), quantity)],
        };
        self.portfolio.record_buy(position.clone(), entry.clone())?;
        for (id, take) in &taken {
            self.book.consume(order.platform, id, *take);
        }
        Ok(ExecutionReport {
            fill: Fill {
                order: order.clone(),
                quantity,
                unit_price: best_ask,
                fee: fees.fee,
                filled_at,
            },
            fees,
            liquidity: stats,
            ledger_entry: entry,
            position: Some(position),
        })
    }

    /// Lowest fresh, wear-comparable competing ask (trade-locked listings still
    /// compete); otherwise the lower median of the gate's verified sales.
    fn sell_reference(&self, order: &Order, now: DateTime<Utc>) -> Option<Money> {
        let best_ask = self
            .book
            .listings_for(order.platform, &order.item)
            .filter(|l| {
                l.observed_at <= now
                    && l.quantity > 0
                    && !self.is_stale(l, now)
                    && self.wear_ok(order, l)
            })
            .map(|l| l.price)
            .min();
        if best_ask.is_some() {
            return best_ask;
        }
        let window_start = now - TimeDelta::days(i64::from(self.config.liquidity.window_days));
        let mut prices: Vec<Money> = self
            .book
            .sales_for(&order.item)
            .iter()
            .filter(|s| {
                s.verified
                    && s.platform == order.platform
                    && s.sold_at > window_start
                    && s.sold_at <= now
                    && order_matches_float(
                        order.item.wear_tier,
                        order.float_bucket,
                        s.item.wear_tier,
                        s.float_value,
                        self.config.tier_only_matches_any_float,
                    )
            })
            .map(|s| s.price)
            .collect();
        if prices.is_empty() {
            return None;
        }
        prices.sort();
        Some(prices[(prices.len() - 1) / 2])
    }

    fn execute_sell(
        &mut self,
        order: &Order,
        target: u32,
        stats: LiquidityStats,
        now: DateTime<Utc>,
    ) -> std::result::Result<ExecutionReport, Rejection> {
        let tier_only_any = self.config.tier_only_matches_any_float;
        let matching: Vec<(usize, &Position)> = self
            .portfolio
            .positions()
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                p.item == order.item
                    && p.quantity > 0
                    && order_matches(
                        order.item.wear_tier,
                        order.float_bucket,
                        p.item.wear_tier,
                        p.float_bucket,
                        tier_only_any,
                    )
            })
            .collect();
        let held: u64 = matching.iter().map(|(_, p)| u64::from(p.quantity)).sum();
        let unlocked: u64 = matching
            .iter()
            .filter(|(_, p)| !p.is_trade_locked_at(now))
            .map(|(_, p)| u64::from(p.quantity))
            .sum();
        if held < u64::from(target) {
            return Err(Rejection::InsufficientInventory {
                requested: target,
                available: u32::try_from(held).unwrap_or(u32::MAX),
            });
        }
        if unlocked < u64::from(target) {
            let earliest_unlock = matching
                .iter()
                .filter(|(_, p)| p.is_trade_locked_at(now))
                .map(|(_, p)| p.locked_until)
                .min()
                .unwrap_or(now);
            return Err(Rejection::PositionTradeLocked {
                requested: target,
                unlocked: u32::try_from(unlocked).unwrap_or(u32::MAX),
                earliest_unlock,
            });
        }

        // The liquidity gate guarantees verified sales exist, so a reference exists.
        let reference = self
            .sell_reference(order, now)
            .ok_or(Rejection::Arithmetic)?;
        if order.limit_price > reference {
            return Err(Rejection::SellLimitAboveMarket {
                limit: order.limit_price,
                reference,
            });
        }
        let fees = self.execution_fees(order, order.limit_price, target)?;
        if fees.net.0 <= 0 {
            return Err(Rejection::NonPositiveProceeds {
                unit_price: fees.unit_price,
                unit_fee: fees.unit_fee,
            });
        }

        // FIFO over tradable lots: oldest acquisition first, then position id.
        let mut lots: Vec<(usize, &Position)> = matching
            .into_iter()
            .filter(|(_, p)| !p.is_trade_locked_at(now))
            .collect();
        lots.sort_by(|(_, a), (_, b)| {
            a.acquired_at
                .cmp(&b.acquired_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        let mut remaining = target;
        let mut draws = Vec::new();
        let mut drawn_ids = Vec::new();
        let mut released = Money(0);
        for (index, lot) in lots {
            if remaining == 0 {
                break;
            }
            let take = lot.quantity.min(remaining);
            remaining -= take;
            let cost = proportional_cost(lot.cost_basis, take, lot.quantity);
            released = released.checked_add(cost)?;
            draws.push((index, take, cost));
            drawn_ids.push((lot.id.clone(), take));
        }
        let realized = fees.net.checked_sub(released)?;
        let filled_at = self.fill_time(now);
        let entry = LedgerEntry {
            seq: self.portfolio.next_seq(),
            order_id: order.id.clone(),
            side: OrderSide::Sell,
            platform: order.platform,
            item: order.item.clone(),
            float_bucket: order.float_bucket,
            quantity: target,
            unit_price: order.limit_price,
            unit_fee: fees.unit_fee,
            gross: fees.gross,
            fee: fees.fee,
            cash_delta: fees.net,
            cost_basis_released: Some(released),
            realized_pnl: Some(realized),
            filled_at,
            listings: Vec::new(),
            positions: drawn_ids,
        };
        self.portfolio.record_sell(entry.clone(), &draws)?;
        Ok(ExecutionReport {
            fill: Fill {
                order: order.clone(),
                quantity: target,
                unit_price: order.limit_price,
                fee: fees.fee,
                filled_at,
            },
            fees,
            liquidity: stats,
            ledger_entry: entry,
            position: None,
        })
    }
}

struct BuyLevel {
    price: Money,
    quantity: u32,
    listings: Vec<(String, u32)>,
}
