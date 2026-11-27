use chrono::Duration;
use cs2_arb_core::{Order, Result};

use super::{buy_order, exit_orders, pct_of, ExitRule};
use crate::replay::{MarketView, StrategyAdapter};

/// Cross-market spread: buy where the all-in cost is lowest when the best
/// net-of-fee exit elsewhere beats it by `min_edge_pct`, then sell after the hold.
#[derive(Debug, Clone)]
pub struct SpreadStrategy {
    pub name: String,
    pub min_edge_pct: f64,
    pub quantity: u32,
    pub max_open_per_item: usize,
    pub exit: ExitRule,
    next_id: u64,
}

impl Default for SpreadStrategy {
    fn default() -> Self {
        Self {
            name: "spread".into(),
            min_edge_pct: 1.0,
            quantity: 1,
            max_open_per_item: 1,
            exit: ExitRule {
                take_profit_pct: 0.0,
                max_hold: Duration::days(14),
            },
            next_id: 0,
        }
    }
}

impl SpreadStrategy {
    fn next_id(name: &str, counter: &mut u64) -> String {
        *counter += 1;
        format!("{name}-order-{counter}")
    }
}

impl StrategyAdapter for SpreadStrategy {
    fn name(&self) -> &str {
        &self.name
    }

    fn on_step(&mut self, view: &MarketView<'_>) -> Result<Vec<Order>> {
        let name = self.name.clone();
        let counter = &mut self.next_id;
        let mut orders = exit_orders(view, self.exit, || Self::next_id(&name, counter))?;

        let mut seen = Vec::new();
        for snapshot in view.batch {
            let key = (&snapshot.item, snapshot.float_bucket);
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
            let open = view
                .open_positions_for(&snapshot.item, snapshot.float_bucket)
                .count();
            if open >= self.max_open_per_item {
                continue;
            }
            let quotes = view.quotes_for(&snapshot.item, snapshot.float_bucket);
            let mut best_buy = None;
            for quote in &quotes {
                let subtotal = quote.price.checked_mul(self.quantity)?;
                let fees = view.fees.get(quote.platform);
                let cost =
                    subtotal.checked_add(fees.buyer_order_fee(quote.price, self.quantity)?)?;
                if best_buy.as_ref().is_none_or(|(_, c)| cost < *c) {
                    best_buy = Some((*quote, cost));
                }
            }
            let Some((buy, cost)) = best_buy else {
                continue;
            };
            let mut best_exit = None;
            for quote in quotes.iter().filter(|q| q.platform != buy.platform) {
                let net = view.seller_proceeds(quote, self.quantity)?;
                best_exit = Some(best_exit.map_or(net, |b: cs2_arb_core::Money| b.max(net)));
            }
            let Some(exit) = best_exit else {
                continue;
            };
            if pct_of(exit.checked_sub(cost)?, cost) >= self.min_edge_pct
                && (exit > cost
                    || !quotes
                        .iter()
                        .any(|q| view.fees.get(q.platform).funding.enabled()))
            {
                orders.push(buy_order(Self::next_id(&name, counter), buy, self.quantity));
            }
        }
        Ok(orders)
    }
}
