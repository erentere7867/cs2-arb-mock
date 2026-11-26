//! Pre-trade risk gate for mock orders.
//!
//! * Per-position limit: a single buy's notional (`limit_price * quantity`).
//! * Per-item limits: open cost basis (and optionally quantity) per [`ItemId`].
//! * Total exposure: open cost basis across all items.
//! * Daily drawdown: intraday peak-to-current decline of the UTC day's P&L (realized from
//!   sells plus change in reported unrealized P&L). Reaching the cap halts **new entries**
//!   (buys) for the rest of that UTC day; exits (sells) remain allowed.
//! * Kill switch: blocks **all** new orders, buys and sells, immediately.

use std::collections::HashMap;

use chrono::{DateTime, NaiveDate, Utc};
use cs2_arb_core::{Error, Fill, ItemId, Money, Order, OrderSide, Result};
use serde::Deserialize;

use crate::kill_switch::KillSwitch;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RiskLimits {
    /// Max notional of a single buy order, in cents.
    pub max_position_notional: Money,
    /// Max open cost basis per item (including the new order's notional).
    pub max_item_exposure: Money,
    /// Optional max open quantity per item.
    pub max_item_quantity: Option<u32>,
    /// Max open cost basis across all items.
    pub max_total_exposure: Money,
    /// Intraday drawdown (positive cents) that halts entries for the rest of the UTC day.
    pub daily_drawdown_cap: Money,
}

impl Default for RiskLimits {
    fn default() -> Self {
        Self {
            max_position_notional: Money(10_000),
            max_item_exposure: Money(25_000),
            max_item_quantity: None,
            max_total_exposure: Money(100_000),
            daily_drawdown_cap: Money(10_000),
        }
    }
}

impl RiskLimits {
    pub fn validate(&self) -> Result<()> {
        if self.max_position_notional.0 <= 0
            || self.max_item_exposure.0 <= 0
            || self.max_total_exposure.0 <= 0
            || self.daily_drawdown_cap.0 <= 0
            || self.max_item_quantity == Some(0)
        {
            return Err(Error::InvalidConfig(
                "risk limits must all be positive".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RiskRejection {
    #[error("kill switch engaged")]
    KillSwitch,
    #[error("daily drawdown cap reached; entries halted until next UTC day ({day})")]
    DrawdownHalt { day: NaiveDate },
    #[error("invalid order: {0}")]
    InvalidOrder(&'static str),
    #[error("position notional {notional:?} exceeds limit {limit:?}")]
    PositionLimit { notional: Money, limit: Money },
    #[error("item exposure {projected:?} would exceed limit {limit:?}")]
    ItemExposureLimit { projected: Money, limit: Money },
    #[error("item quantity {projected} would exceed limit {limit}")]
    ItemQuantityLimit { projected: u64, limit: u32 },
    #[error("total exposure {projected:?} would exceed limit {limit:?}")]
    TotalExposureLimit { projected: Money, limit: Money },
    #[error("sell quantity {requested} exceeds held quantity {held}")]
    InsufficientPosition { requested: u32, held: u64 },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Holding {
    quantity: u64,
    cost_basis: i64,
}

#[derive(Debug, Clone)]
pub struct RiskManager {
    limits: RiskLimits,
    kill_switch: KillSwitch,
    holdings: HashMap<ItemId, Holding>,
    total_exposure: i64,
    day: Option<NaiveDate>,
    day_realized: i64,
    unrealized: i64,
    unrealized_at_day_start: i64,
    day_peak: i64,
    halted_on: Option<NaiveDate>,
}

impl RiskManager {
    pub fn new(limits: RiskLimits, kill_switch: KillSwitch) -> Result<Self> {
        limits.validate()?;
        Ok(Self {
            limits,
            kill_switch,
            holdings: HashMap::new(),
            total_exposure: 0,
            day: None,
            day_realized: 0,
            unrealized: 0,
            unrealized_at_day_start: 0,
            day_peak: 0,
            halted_on: None,
        })
    }

    pub fn limits(&self) -> &RiskLimits {
        &self.limits
    }

    pub fn kill_switch(&self) -> &KillSwitch {
        &self.kill_switch
    }

    pub fn total_exposure(&self) -> Money {
        Money(self.total_exposure)
    }

    pub fn item_exposure(&self, item: &ItemId) -> Money {
        Money(self.holdings.get(item).map_or(0, |h| h.cost_basis))
    }

    pub fn item_quantity(&self, item: &ItemId) -> u64 {
        self.holdings.get(item).map_or(0, |h| h.quantity)
    }

    /// Today's P&L: realized today plus the change in unrealized since the day began.
    pub fn day_pnl(&self) -> Money {
        Money(self.day_realized + self.unrealized - self.unrealized_at_day_start)
    }

    /// Current intraday drawdown from today's P&L high-water mark (>= 0).
    pub fn day_drawdown(&self) -> Money {
        Money((self.day_peak - self.day_pnl().0).max(0))
    }

    /// Whether new entries are halted at `now` (drawdown halt or kill switch).
    pub fn entries_halted(&mut self, now: DateTime<Utc>) -> bool {
        self.roll_day(now);
        self.kill_switch.is_engaged() || self.halted_on == Some(now.date_naive())
    }

    /// Pre-trade check. Kill switch blocks everything; drawdown halt blocks buys only.
    pub fn check(
        &mut self,
        order: &Order,
        now: DateTime<Utc>,
    ) -> std::result::Result<(), RiskRejection> {
        if self.kill_switch.is_engaged() {
            return Err(RiskRejection::KillSwitch);
        }
        self.roll_day(now);
        if order.quantity == 0 {
            return Err(RiskRejection::InvalidOrder("quantity must be positive"));
        }
        if order.limit_price.0 <= 0 {
            return Err(RiskRejection::InvalidOrder("limit price must be positive"));
        }
        match order.side {
            OrderSide::Sell => {
                let held = self.item_quantity(&order.item);
                if u64::from(order.quantity) > held {
                    return Err(RiskRejection::InsufficientPosition {
                        requested: order.quantity,
                        held,
                    });
                }
                Ok(())
            }
            OrderSide::Buy => self.check_entry(order, now),
        }
    }

    /// Boolean convenience over [`Self::check`].
    pub fn allows(&mut self, order: &Order, now: DateTime<Utc>) -> bool {
        self.check(order, now).is_ok()
    }

    fn check_entry(
        &self,
        order: &Order,
        now: DateTime<Utc>,
    ) -> std::result::Result<(), RiskRejection> {
        let today = now.date_naive();
        if self.halted_on == Some(today) {
            return Err(RiskRejection::DrawdownHalt { day: today });
        }
        let notional = order
            .limit_price
            .checked_mul(order.quantity)
            .map_err(|_| RiskRejection::InvalidOrder("notional overflow"))?;
        if notional > self.limits.max_position_notional {
            return Err(RiskRejection::PositionLimit {
                notional,
                limit: self.limits.max_position_notional,
            });
        }
        self.check_entry_exposure(order, notional)
    }

    pub(crate) fn check_entry_exposure(
        &self,
        order: &Order,
        buy_cost: Money,
    ) -> std::result::Result<(), RiskRejection> {
        let holding = self.holdings.get(&order.item).copied().unwrap_or_default();
        let item_projected = Money(holding.cost_basis.saturating_add(buy_cost.0));
        if item_projected > self.limits.max_item_exposure {
            return Err(RiskRejection::ItemExposureLimit {
                projected: item_projected,
                limit: self.limits.max_item_exposure,
            });
        }
        if let Some(limit) = self.limits.max_item_quantity {
            let projected = holding.quantity + u64::from(order.quantity);
            if projected > u64::from(limit) {
                return Err(RiskRejection::ItemQuantityLimit { projected, limit });
            }
        }
        let total_projected = Money(self.total_exposure.saturating_add(buy_cost.0));
        if total_projected > self.limits.max_total_exposure {
            return Err(RiskRejection::TotalExposureLimit {
                projected: total_projected,
                limit: self.limits.max_total_exposure,
            });
        }
        Ok(())
    }

    /// Updates exposure and P&L from a simulated fill. Buys add `qty * price + fee` to cost
    /// basis; sells release average cost and realize `qty * price - fee - released cost`.
    /// Returns the realized P&L of this fill (zero for buys).
    pub fn record_fill(&mut self, fill: &Fill) -> Result<Money> {
        self.record_fill_inner(fill, None, None)
    }

    /// Update drawdown using the post-fill mark and the simulator's actual sold-lot cost
    /// basis (`Money(0)` on buys).
    pub fn record_marked_fill(
        &mut self,
        fill: &Fill,
        unrealized: Money,
        cost_basis_released: Money,
    ) -> Result<Money> {
        self.record_fill_inner(fill, Some(unrealized), Some(cost_basis_released))
    }

    fn record_fill_inner(
        &mut self,
        fill: &Fill,
        unrealized: Option<Money>,
        cost_basis_released: Option<Money>,
    ) -> Result<Money> {
        self.roll_day(fill.filled_at);
        let gross = fill.unit_price.checked_mul(fill.quantity)?;
        let holding = self.holdings.entry(fill.order.item.clone()).or_default();
        let realized = match fill.order.side {
            OrderSide::Buy => {
                let cost = gross.checked_add(fill.fee)?;
                holding.quantity += u64::from(fill.quantity);
                holding.cost_basis = holding
                    .cost_basis
                    .checked_add(cost.0)
                    .ok_or(Error::MoneyOverflow)?;
                self.total_exposure = self
                    .total_exposure
                    .checked_add(cost.0)
                    .ok_or(Error::MoneyOverflow)?;
                0
            }
            OrderSide::Sell => {
                let qty = u64::from(fill.quantity).min(holding.quantity);
                if qty < u64::from(fill.quantity) {
                    tracing::warn!(
                        item = %fill.order.item.market_hash_name,
                        "sell fill exceeds tracked holding; clamping released cost"
                    );
                }
                let released = if let Some(cost) = cost_basis_released {
                    if qty != u64::from(fill.quantity)
                        || !(0..=holding.cost_basis).contains(&cost.0)
                    {
                        return Err(Error::InvalidData(
                            "sold-lot cost exceeds risk holdings".into(),
                        ));
                    }
                    cost.0
                } else if holding.quantity == 0 {
                    0
                } else {
                    // Pro-rata average cost (truncated); a full exit releases everything.
                    if qty == holding.quantity {
                        holding.cost_basis
                    } else {
                        ((i128::from(holding.cost_basis) * i128::from(qty))
                            / i128::from(holding.quantity)) as i64
                    }
                };
                holding.quantity -= qty;
                holding.cost_basis -= released;
                self.total_exposure -= released;
                gross.checked_sub(fill.fee)?.0 - released
            }
        };
        if holding.quantity == 0 {
            self.holdings.remove(&fill.order.item);
        }
        self.day_realized += realized;
        if let Some(mark) = unrealized {
            self.unrealized = mark.0;
        }
        self.refresh_drawdown(fill.filled_at);
        Ok(Money(realized))
    }

    /// Reports the current total unrealized P&L (marked to market) at `at`.
    pub fn set_unrealized_pnl(&mut self, total: Money, at: DateTime<Utc>) {
        self.roll_day(at);
        self.unrealized = total.0;
        self.refresh_drawdown(at);
    }

    /// Records an arbitrary realized P&L adjustment (e.g. fees outside fills) at `at`.
    pub fn record_realized_pnl(&mut self, delta: Money, at: DateTime<Utc>) {
        self.roll_day(at);
        self.day_realized += delta.0;
        self.refresh_drawdown(at);
    }

    fn refresh_drawdown(&mut self, at: DateTime<Utc>) {
        let pnl = self.day_pnl().0;
        self.day_peak = self.day_peak.max(pnl);
        if self.day_peak - pnl >= self.limits.daily_drawdown_cap.0 && self.halted_on.is_none() {
            let day = self.day.unwrap_or_else(|| at.date_naive());
            tracing::warn!(%day, drawdown = self.day_peak - pnl, "daily drawdown cap hit; entries halted");
            self.halted_on = Some(day);
        }
    }

    /// Starts a new UTC day when `now` is on a later date. Earlier timestamps never rewind.
    fn roll_day(&mut self, now: DateTime<Utc>) {
        let today = now.date_naive();
        match self.day {
            Some(day) if today <= day => {}
            _ => {
                self.day = Some(today);
                self.day_realized = 0;
                self.unrealized_at_day_start = self.unrealized;
                self.day_peak = 0;
                if self.halted_on.is_some_and(|d| d < today) {
                    self.halted_on = None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};
    use cs2_arb_core::{Platform, WearTier};

    use super::*;

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2025, 2, 6, 9, 0, 0).unwrap()
    }

    fn item(name: &str) -> ItemId {
        ItemId {
            market_hash_name: name.into(),
            wear_tier: WearTier::FieldTested,
        }
    }

    fn order(side: OrderSide, name: &str, qty: u32, price: i64, at: DateTime<Utc>) -> Order {
        Order {
            id: format!("{name}-{qty}-{price}"),
            side,
            platform: Platform::Skinport,
            item: item(name),
            float_bucket: None,
            quantity: qty,
            limit_price: Money(price),
            submitted_at: at,
        }
    }

    fn fill(order: Order, price: i64, fee: i64, at: DateTime<Utc>) -> Fill {
        Fill {
            quantity: order.quantity,
            order,
            unit_price: Money(price),
            fee: Money(fee),
            filled_at: at,
        }
    }

    fn limits() -> RiskLimits {
        RiskLimits {
            max_position_notional: Money(5_000),
            max_item_exposure: Money(8_000),
            max_item_quantity: Some(3),
            max_total_exposure: Money(12_000),
            daily_drawdown_cap: Money(1_000),
        }
    }

    fn manager() -> RiskManager {
        RiskManager::new(limits(), KillSwitch::new()).unwrap()
    }

    #[test]
    fn enforces_position_item_and_total_limits() {
        let mut rm = manager();
        let now = t0();
        assert_eq!(
            rm.check(&order(OrderSide::Buy, "a", 2, 2_501, now), now),
            Err(RiskRejection::PositionLimit {
                notional: Money(5_002),
                limit: Money(5_000)
            })
        );
        let a1 = order(OrderSide::Buy, "a", 1, 5_000, now);
        rm.check(&a1, now).unwrap();
        rm.record_fill(&fill(a1, 5_000, 0, now)).unwrap();
        assert_eq!(rm.item_exposure(&item("a")), Money(5_000));
        assert!(matches!(
            rm.check(&order(OrderSide::Buy, "a", 1, 3_001, now), now),
            Err(RiskRejection::ItemExposureLimit { .. })
        ));
        let a2 = order(OrderSide::Buy, "a", 1, 3_000, now);
        rm.check(&a2, now).unwrap();
        rm.record_fill(&fill(a2, 2_900, 100, now)).unwrap();
        assert_eq!(rm.item_exposure(&item("a")), Money(8_000));

        let b1 = order(OrderSide::Buy, "b", 1, 4_000, now);
        rm.record_fill(&fill(b1, 4_000, 0, now)).unwrap();
        assert_eq!(rm.total_exposure(), Money(12_000));
        assert!(matches!(
            rm.check(&order(OrderSide::Buy, "c", 1, 1, now), now),
            Err(RiskRejection::TotalExposureLimit { .. })
        ));
    }

    #[test]
    fn enforces_item_quantity_and_sell_holdings() {
        let mut rm = manager();
        let now = t0();
        let buy = order(OrderSide::Buy, "a", 3, 100, now);
        rm.record_fill(&fill(buy, 100, 0, now)).unwrap();
        assert!(matches!(
            rm.check(&order(OrderSide::Buy, "a", 1, 100, now), now),
            Err(RiskRejection::ItemQuantityLimit {
                projected: 4,
                limit: 3
            })
        ));
        assert!(matches!(
            rm.check(&order(OrderSide::Sell, "a", 4, 100, now), now),
            Err(RiskRejection::InsufficientPosition {
                requested: 4,
                held: 3
            })
        ));
        rm.check(&order(OrderSide::Sell, "a", 3, 100, now), now)
            .unwrap();
        assert!(matches!(
            rm.check(&order(OrderSide::Buy, "a", 0, 100, now), now),
            Err(RiskRejection::InvalidOrder(_))
        ));
    }

    #[test]
    fn sells_release_average_cost_and_realize_pnl() {
        let mut rm = manager();
        let now = t0();
        rm.record_fill(&fill(
            order(OrderSide::Buy, "a", 2, 3_000, now),
            3_000,
            60,
            now,
        ))
        .unwrap();
        assert_eq!(rm.item_exposure(&item("a")), Money(6_060));
        let realized = rm
            .record_fill(&fill(
                order(OrderSide::Sell, "a", 1, 3_500, now),
                3_500,
                175,
                now,
            ))
            .unwrap();
        // Released cost 3_030; proceeds 3_325; realized 295.
        assert_eq!(realized, Money(295));
        assert_eq!(rm.item_exposure(&item("a")), Money(3_030));
        assert_eq!(rm.total_exposure(), Money(3_030));
        assert_eq!(rm.day_pnl(), Money(295));
        rm.record_fill(&fill(
            order(OrderSide::Sell, "a", 1, 3_030, now),
            3_030,
            0,
            now,
        ))
        .unwrap();
        assert_eq!(rm.total_exposure(), Money(0));
        assert_eq!(rm.item_quantity(&item("a")), 0);
    }

    #[test]
    fn drawdown_halts_entries_for_rest_of_utc_day_only() {
        let mut rm = manager();
        let morning = t0();
        let buy = order(OrderSide::Buy, "a", 1, 1_000, morning);
        rm.check(&buy, morning).unwrap();

        // Gain first: peak 600. Then a mark-down of 1_000 from the peak hits the cap.
        rm.record_realized_pnl(Money(600), morning);
        rm.set_unrealized_pnl(Money(-399), morning + Duration::hours(1));
        assert_eq!(rm.day_drawdown(), Money(399));
        rm.check(&buy, morning + Duration::hours(1)).unwrap();
        rm.set_unrealized_pnl(Money(-999), morning + Duration::hours(2));
        assert_eq!(rm.day_drawdown(), Money(999));
        rm.check(&buy, morning + Duration::hours(2)).unwrap();
        rm.set_unrealized_pnl(Money(-1_000), morning + Duration::hours(2));
        assert_eq!(rm.day_pnl(), Money(-400));
        assert_eq!(rm.day_drawdown(), Money(1_000));
        let day = morning.date_naive();
        assert_eq!(
            rm.check(&buy, morning + Duration::hours(2)),
            Err(RiskRejection::DrawdownHalt { day })
        );

        // Recovery the same day does not lift the halt.
        rm.set_unrealized_pnl(Money(5_000), morning + Duration::hours(3));
        let late = Utc.with_ymd_and_hms(2025, 2, 6, 23, 59, 59).unwrap();
        assert!(rm.entries_halted(late));
        assert!(matches!(
            rm.check(&buy, late),
            Err(RiskRejection::DrawdownHalt { .. })
        ));

        // Exits are still allowed while halted.
        rm.record_fill(&fill(buy.clone(), 1_000, 0, morning))
            .unwrap();
        rm.check(&order(OrderSide::Sell, "a", 1, 900, late), late)
            .unwrap();

        // Next UTC day: entries resume and the day's P&L baseline resets.
        let next = Utc.with_ymd_and_hms(2025, 2, 7, 0, 0, 0).unwrap();
        assert!(!rm.entries_halted(next));
        assert_eq!(rm.day_pnl(), Money(0));
        rm.check(&order(OrderSide::Buy, "b", 1, 1_000, next), next)
            .unwrap();
    }

    #[test]
    fn realized_losses_from_fills_trigger_halt() {
        let mut rm = manager();
        let now = t0();
        rm.record_fill(&fill(
            order(OrderSide::Buy, "a", 1, 4_000, now),
            4_000,
            0,
            now,
        ))
        .unwrap();
        let realized = rm
            .record_fill(&fill(
                order(OrderSide::Sell, "a", 1, 3_100, now),
                3_100,
                100,
                now,
            ))
            .unwrap();
        assert_eq!(realized, Money(-1_000));
        assert!(rm.entries_halted(now));
    }

    #[test]
    fn kill_switch_blocks_all_orders_immediately() {
        let ks = KillSwitch::new();
        let mut rm = RiskManager::new(limits(), ks.clone()).unwrap();
        let now = t0();
        rm.record_fill(&fill(order(OrderSide::Buy, "a", 1, 100, now), 100, 0, now))
            .unwrap();
        let buy = order(OrderSide::Buy, "b", 1, 100, now);
        let sell = order(OrderSide::Sell, "a", 1, 100, now);
        assert!(rm.allows(&buy, now) && rm.allows(&sell, now));

        ks.flag().store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(rm.check(&buy, now), Err(RiskRejection::KillSwitch));
        assert_eq!(rm.check(&sell, now), Err(RiskRejection::KillSwitch));
        assert!(rm.entries_halted(now));
        // Not cleared by a new UTC day.
        let later = now + Duration::days(3);
        assert_eq!(rm.check(&buy, later), Err(RiskRejection::KillSwitch));
    }

    #[test]
    fn rejects_invalid_limits() {
        assert!(RiskManager::new(
            RiskLimits {
                daily_drawdown_cap: Money(0),
                ..RiskLimits::default()
            },
            KillSwitch::new()
        )
        .is_err());
        let l: RiskLimits = toml::from_str("max_total_exposure = 50000").unwrap();
        assert_eq!(l.max_total_exposure, Money(50_000));
        assert_eq!(
            l.max_position_notional,
            RiskLimits::default().max_position_notional
        );
    }
}
