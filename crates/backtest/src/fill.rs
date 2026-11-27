//! Built-in fill model: fills at the replayed quote, applies configured fees,
//! and imposes the post-purchase trade hold. Purely in-process; nothing here can
//! reach a marketplace.

use std::collections::{BTreeSet, HashMap};

use chrono::{DateTime, Duration, Utc};
use cs2_arb_core::{FeeSchedule, Fill, Money, Order, OrderSide, PriceSnapshot, Result};
use cs2_arb_sim::venues::{VenueBalance, VenueTradeHold};

use crate::replay::{MarketView, QuoteKey};

/// Default trade hold applied to purchased items (CS2 7-day trade protection).
pub const DEFAULT_TRADE_HOLD: Duration = Duration::days(7);

/// Result of asking a fill model to execute a simulated order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FillOutcome {
    Filled(Fill),
    Rejected(String),
}

/// Pluggable simulated execution. The real `cs2-arb-sim` can be adapted to this
/// trait at integration time (for example by synthesizing a `Listing` from the quote).
pub trait FillModel {
    /// Schedule an order without changing inventory, cash, or risk.
    fn schedule_fill(&self, _strategy: &str, now: DateTime<Utc>) -> DateTime<Utc> {
        now
    }

    fn venue_trade_holds(&self) -> Vec<VenueTradeHold> {
        Vec::new()
    }

    fn preview_fill(&self, _strategy: &str, order: &Order) -> Option<(Money, u32)> {
        Some((order.limit_price, order.quantity))
    }

    fn venue_balances(&self, _strategy: &str) -> Result<Vec<VenueBalance>> {
        Ok(Vec::new())
    }

    fn transfer_fees_paid(&self, _strategy: &str) -> Money {
        Money(0)
    }

    fn available_cash(&self, _strategy: &str, _platform: cs2_arb_core::Platform) -> Option<Money> {
        None
    }

    fn add_quote_update_times(&self, _times: &mut BTreeSet<DateTime<Utc>>) {}

    fn invalidate_quotes(
        &self,
        _at: DateTime<Utc>,
        _quotes: &mut HashMap<QuoteKey, PriceSnapshot>,
    ) {
    }

    fn seller_proceeds(
        &self,
        unit_price: Money,
        quantity: u32,
        fees: FeeSchedule,
    ) -> Result<Money> {
        unit_price
            .checked_mul(quantity)?
            .checked_sub(fees.seller_order_fee(unit_price, quantity)?)
    }

    fn prepare_run(&self) {}

    fn prepare_step(&self, _strategy: &str, _view: &MarketView<'_>) -> Result<()> {
        Ok(())
    }

    fn after_fill(&self, _strategy: &str, _view: &MarketView<'_>, _fill: &Fill) -> Result<()> {
        Ok(())
    }

    fn fill_for(
        &self,
        _strategy: &str,
        order: &Order,
        quote: &PriceSnapshot,
        fees: FeeSchedule,
        now: DateTime<Utc>,
    ) -> Result<FillOutcome> {
        self.fill(order, quote, fees, now)
    }

    /// Simulate `order` against the most recent replayed `quote` for the same
    /// platform, item and float bucket. `fees` is the order platform's schedule.
    fn fill(
        &self,
        order: &Order,
        quote: &PriceSnapshot,
        fees: FeeSchedule,
        now: DateTime<Utc>,
    ) -> Result<FillOutcome>;

    /// When an item bought by `fill` becomes tradable again.
    fn locked_until(&self, fill: &Fill) -> DateTime<Utc>;
}

/// Fills the full order quantity at the quote price when the limit allows it.
/// Percentage fees round per unit; each fixed fee is charged once per order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotFillModel {
    pub trade_hold: Duration,
}

impl Default for SnapshotFillModel {
    fn default() -> Self {
        Self {
            trade_hold: DEFAULT_TRADE_HOLD,
        }
    }
}

impl FillModel for SnapshotFillModel {
    fn fill(
        &self,
        order: &Order,
        quote: &PriceSnapshot,
        fees: FeeSchedule,
        now: DateTime<Utc>,
    ) -> Result<FillOutcome> {
        if order.quantity == 0 {
            return Ok(FillOutcome::Rejected("zero quantity".into()));
        }
        let crosses = match order.side {
            OrderSide::Buy => quote.price <= order.limit_price,
            OrderSide::Sell => quote.price >= order.limit_price,
        };
        if !crosses {
            return Ok(FillOutcome::Rejected(format!(
                "limit {} not marketable against quote {}",
                order.limit_price.0, quote.price.0
            )));
        }
        let fee = match order.side {
            OrderSide::Buy => fees.buyer_order_fee(quote.price, order.quantity)?,
            OrderSide::Sell => fees.seller_order_fee(quote.price, order.quantity)?,
        };
        Ok(FillOutcome::Filled(Fill {
            order: order.clone(),
            quantity: order.quantity,
            unit_price: quote.price,
            fee,
            filled_at: now,
        }))
    }

    fn locked_until(&self, fill: &Fill) -> DateTime<Utc> {
        fill.filled_at + self.trade_hold
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use cs2_arb_core::{FloatBucket, ItemId, Money, Platform, WearTier};

    use super::*;

    fn quote(price: i64) -> PriceSnapshot {
        PriceSnapshot {
            platform: Platform::CsFloat,
            item: ItemId {
                market_hash_name: "AK-47 | Redline (Field-Tested)".into(),
                wear_tier: WearTier::FieldTested,
            },
            float_bucket: Some(FloatBucket::FieldTestedLow),
            price: Money(price),
            observed_at: Utc.with_ymd_and_hms(2025, 1, 1, 12, 0, 0).unwrap(),
        }
    }

    fn order(side: OrderSide, limit: i64, quantity: u32) -> Order {
        let q = quote(0);
        Order {
            id: "o".into(),
            side,
            platform: q.platform,
            item: q.item,
            float_bucket: q.float_bucket,
            quantity,
            limit_price: Money(limit),
            submitted_at: q.observed_at,
        }
    }

    const FEES: FeeSchedule = FeeSchedule {
        buyer_fee_pct: 1.0,
        seller_fee_pct: 2.0,
        buyer_fixed_fee: Money(5),
        seller_fixed_fee: Money(0),
        funding: cs2_arb_core::FundingFees::NONE,
    };

    #[test]
    fn applies_side_specific_fees_and_hold() {
        let model = SnapshotFillModel::default();
        let now = quote(0).observed_at;
        let FillOutcome::Filled(buy) = model
            .fill(&order(OrderSide::Buy, 1000, 2), &quote(1000), FEES, now)
            .unwrap()
        else {
            panic!("expected fill");
        };
        assert_eq!(buy.unit_price, Money(1000));
        assert_eq!(buy.fee, Money(25)); // 1% of 2000 + 5 fixed
        assert_eq!(model.locked_until(&buy), now + Duration::days(7));

        let FillOutcome::Filled(sell) = model
            .fill(&order(OrderSide::Sell, 900, 2), &quote(1000), FEES, now)
            .unwrap()
        else {
            panic!("expected fill");
        };
        assert_eq!(sell.fee, Money(40)); // 2% of 2000
    }

    #[test]
    fn funding_does_not_multiply_per_order_platform_fees() {
        let model = SnapshotFillModel::default();
        let mut fees = FEES;
        fees.seller_fixed_fee = Money(7);
        fees.funding.crypto_withdrawal.fixed = Money(3);
        let quote = quote(1000);
        let now = quote.observed_at;
        let FillOutcome::Filled(buy) = model
            .fill(&order(OrderSide::Buy, 1000, 2), &quote, fees, now)
            .unwrap()
        else {
            panic!("expected buy");
        };
        let FillOutcome::Filled(sell) = model
            .fill(&order(OrderSide::Sell, 1000, 2), &quote, fees, now)
            .unwrap()
        else {
            panic!("expected sell");
        };
        assert_eq!(buy.fee, Money(25));
        assert_eq!(sell.fee, Money(50));
        assert_eq!(
            model.seller_proceeds(Money(1000), 2, fees).unwrap(),
            Money(1950)
        );
    }

    #[test]
    fn rejects_unmarketable_and_empty_orders() {
        let model = SnapshotFillModel::default();
        let now = quote(0).observed_at;
        for (o, q) in [
            (order(OrderSide::Buy, 999, 1), quote(1000)),
            (order(OrderSide::Sell, 1001, 1), quote(1000)),
            (order(OrderSide::Buy, 1000, 0), quote(1000)),
        ] {
            assert!(matches!(
                model.fill(&o, &q, FEES, now).unwrap(),
                FillOutcome::Rejected(_)
            ));
        }
    }
}
