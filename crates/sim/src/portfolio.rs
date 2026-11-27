//! Simulated inventory, cash and an append-only ledger with realized P&L.

use chrono::{DateTime, Utc};
use cs2_arb_core::{FloatBucket, ItemId, Money, OrderSide, Platform, Position};
use serde::Serialize;

/// One filled order. Buys have negative `cash_delta`; sells positive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LedgerEntry {
    pub seq: u64,
    pub order_id: String,
    pub side: OrderSide,
    pub platform: Platform,
    pub item: ItemId,
    pub float_bucket: Option<FloatBucket>,
    pub quantity: u32,
    pub unit_price: Money,
    pub unit_fee: Money,
    /// `quantity * unit_price`.
    pub gross: Money,
    /// Total marketplace fee: buyer fee on buys, seller fee on sells.
    pub fee: Money,
    /// Buy: `-(gross + fee)`. Sell: `gross - fee`.
    pub cash_delta: Money,
    /// Sells only: cost basis (including entry fees) of the units sold.
    pub cost_basis_released: Option<Money>,
    /// Sells only: `cash_delta - cost_basis_released`.
    pub realized_pnl: Option<Money>,
    pub filled_at: DateTime<Utc>,
    /// Buys: listings consumed, with quantity taken from each.
    pub listings: Vec<(String, u32)>,
    /// Buys: position opened. Sells: positions drawn down (FIFO), with quantity.
    pub positions: Vec<(String, u32)>,
}

/// Aggregate figures for reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PortfolioSummary {
    pub realized_pnl: Money,
    pub buyer_fees_paid: Money,
    pub seller_fees_paid: Money,
    pub total_fees_paid: Money,
    pub gross_bought: Money,
    pub gross_sold: Money,
    pub net_cash_flow: Money,
    pub cash: Option<Money>,
    pub open_cost_basis: Money,
    pub open_quantity: u64,
    pub fills: u64,
}

#[derive(Debug, Clone)]
pub struct Portfolio {
    positions: Vec<Position>,
    ledger: Vec<LedgerEntry>,
    starting_cash: Option<Money>,
    net_cash_flow: Money,
    realized_pnl: Money,
    buyer_fees_paid: Money,
    seller_fees_paid: Money,
    gross_bought: Money,
    gross_sold: Money,
}

impl Portfolio {
    pub fn new(starting_cash: Option<Money>) -> Self {
        Self {
            positions: Vec::new(),
            ledger: Vec::new(),
            starting_cash,
            net_cash_flow: Money(0),
            realized_pnl: Money(0),
            buyer_fees_paid: Money(0),
            seller_fees_paid: Money(0),
            gross_bought: Money(0),
            gross_sold: Money(0),
        }
    }

    /// Open positions, in acquisition (FIFO) order.
    pub fn positions(&self) -> &[Position] {
        &self.positions
    }

    pub fn ledger(&self) -> &[LedgerEntry] {
        &self.ledger
    }

    pub fn realized_pnl(&self) -> Money {
        self.realized_pnl
    }

    pub fn buyer_fees_paid(&self) -> Money {
        self.buyer_fees_paid
    }

    pub fn seller_fees_paid(&self) -> Money {
        self.seller_fees_paid
    }

    pub fn total_fees_paid(&self) -> Money {
        Money(
            self.buyer_fees_paid
                .0
                .saturating_add(self.seller_fees_paid.0),
        )
    }

    pub fn gross_bought(&self) -> Money {
        self.gross_bought
    }

    pub fn gross_sold(&self) -> Money {
        self.gross_sold
    }

    pub fn net_cash_flow(&self) -> Money {
        self.net_cash_flow
    }

    /// Current cash if a starting balance was configured.
    pub fn cash(&self) -> Option<Money> {
        self.starting_cash
            .map(|start| Money(start.0.saturating_add(self.net_cash_flow.0)))
    }

    pub fn open_cost_basis(&self) -> Money {
        Money(self.positions.iter().map(|p| p.cost_basis.0).sum())
    }

    /// Units held of an exact item and float bucket (locked or not).
    pub fn quantity(&self, item: &ItemId, bucket: Option<FloatBucket>) -> u32 {
        self.matching(item, bucket).map(|p| p.quantity).sum()
    }

    /// Units of an exact item and float bucket that are past their trade hold at `now`.
    pub fn sellable_quantity(
        &self,
        item: &ItemId,
        bucket: Option<FloatBucket>,
        now: DateTime<Utc>,
    ) -> u32 {
        self.matching(item, bucket)
            .filter(|p| !p.is_trade_locked_at(now))
            .map(|p| p.quantity)
            .sum()
    }

    pub fn summary(&self) -> PortfolioSummary {
        PortfolioSummary {
            realized_pnl: self.realized_pnl,
            buyer_fees_paid: self.buyer_fees_paid,
            seller_fees_paid: self.seller_fees_paid,
            total_fees_paid: self.total_fees_paid(),
            gross_bought: self.gross_bought,
            gross_sold: self.gross_sold,
            net_cash_flow: self.net_cash_flow,
            cash: self.cash(),
            open_cost_basis: self.open_cost_basis(),
            open_quantity: self.positions.iter().map(|p| u64::from(p.quantity)).sum(),
            fills: self.ledger.len() as u64,
        }
    }

    fn matching<'a>(
        &'a self,
        item: &'a ItemId,
        bucket: Option<FloatBucket>,
    ) -> impl Iterator<Item = &'a Position> + 'a {
        self.positions
            .iter()
            .filter(move |p| &p.item == item && p.float_bucket == bucket)
    }

    pub(crate) fn next_seq(&self) -> u64 {
        self.ledger.len() as u64
    }

    pub(crate) fn realize_prepaid_fee_loss(&mut self, fee: Money) -> cs2_arb_core::Result<()> {
        self.realized_pnl = self.realized_pnl.checked_sub(fee)?;
        Ok(())
    }

    /// Apply a buy whose totals have been validated by the caller.
    pub(crate) fn record_buy(
        &mut self,
        position: Position,
        entry: LedgerEntry,
    ) -> cs2_arb_core::Result<()> {
        let net_cash_flow = self.net_cash_flow.checked_add(entry.cash_delta)?;
        let buyer_fees_paid = self.buyer_fees_paid.checked_add(entry.fee)?;
        let gross_bought = self.gross_bought.checked_add(entry.gross)?;
        self.net_cash_flow = net_cash_flow;
        self.buyer_fees_paid = buyer_fees_paid;
        self.gross_bought = gross_bought;
        self.positions.push(position);
        self.ledger.push(entry);
        Ok(())
    }

    /// Apply a sell: `draws` are `(position index, units taken, cost released)`.
    /// Totals are checked before any state changes.
    pub(crate) fn record_sell(
        &mut self,
        entry: LedgerEntry,
        draws: &[(usize, u32, Money)],
    ) -> cs2_arb_core::Result<()> {
        let pnl = entry.realized_pnl.unwrap_or(Money(0));
        let net_cash_flow = self.net_cash_flow.checked_add(entry.cash_delta)?;
        let seller_fees_paid = self.seller_fees_paid.checked_add(entry.fee)?;
        let gross_sold = self.gross_sold.checked_add(entry.gross)?;
        let realized_pnl = self.realized_pnl.checked_add(pnl)?;
        self.net_cash_flow = net_cash_flow;
        self.seller_fees_paid = seller_fees_paid;
        self.gross_sold = gross_sold;
        self.realized_pnl = realized_pnl;
        for &(index, take, released) in draws {
            let position = &mut self.positions[index];
            position.quantity -= take;
            position.cost_basis = Money(position.cost_basis.0 - released.0);
        }
        self.positions.retain(|p| p.quantity > 0);
        self.ledger.push(entry);
        Ok(())
    }
}

/// Cost basis attributable to `take` of `quantity` units with total `cost_basis`,
/// floored; taking the whole lot always releases exactly the remaining basis.
pub(crate) fn proportional_cost(cost_basis: Money, take: u32, quantity: u32) -> Money {
    if take >= quantity || quantity == 0 {
        return cost_basis;
    }
    let released = i128::from(cost_basis.0) * i128::from(take) / i128::from(quantity);
    Money(released as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proportional_cost_is_exact_on_whole_lots() {
        assert_eq!(proportional_cost(Money(3030), 1, 3), Money(1010));
        assert_eq!(proportional_cost(Money(1000), 1, 3), Money(333));
        assert_eq!(proportional_cost(Money(1000), 3, 3), Money(1000));
        assert_eq!(proportional_cost(Money(1000), 0, 3), Money(0));
    }
}
