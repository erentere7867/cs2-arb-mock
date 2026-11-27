use std::collections::{HashMap, VecDeque};

use chrono::Duration;
use cs2_arb_core::{Money, Order, Result};

use super::{buy_order, exit_orders, ExitRule};
use crate::replay::{MarketView, QuoteKey, StrategyAdapter};

/// Event dip: while a panic window is open, buy a platform quote trading at
/// least `dip_pct` below its own trailing mean, then sell after the hold once
/// the best net exit recovers past the take-profit level.
#[derive(Debug, Clone)]
pub struct DipStrategy {
    pub name: String,
    pub dip_pct: f64,
    /// Number of prior quotes in the trailing mean; no signal until it is full.
    pub lookback: usize,
    pub require_panic_window: bool,
    pub quantity: u32,
    pub max_open_per_item: usize,
    pub exit: ExitRule,
    history: HashMap<QuoteKey, VecDeque<i64>>,
    next_id: u64,
}

impl Default for DipStrategy {
    fn default() -> Self {
        Self {
            name: "dip".into(),
            dip_pct: 10.0,
            lookback: 7,
            require_panic_window: true,
            quantity: 1,
            max_open_per_item: 2,
            exit: ExitRule {
                take_profit_pct: 5.0,
                max_hold: Duration::days(21),
            },
            history: HashMap::new(),
            next_id: 0,
        }
    }
}

impl DipStrategy {
    fn next_id(name: &str, counter: &mut u64) -> String {
        *counter += 1;
        format!("{name}-order-{counter}")
    }
}

impl StrategyAdapter for DipStrategy {
    fn name(&self) -> &str {
        &self.name
    }

    fn on_step(&mut self, view: &MarketView<'_>) -> Result<Vec<Order>> {
        let name = self.name.clone();
        let counter = &mut self.next_id;
        let mut orders = exit_orders(view, self.exit, || Self::next_id(&name, counter))?;

        let window_open = !self.require_panic_window || view.panic_window_active();
        let mut planned: Vec<(&cs2_arb_core::ItemId, Option<cs2_arb_core::FloatBucket>)> =
            Vec::new();
        for snapshot in view.batch {
            let key = QuoteKey::of(snapshot);
            let series = self.history.entry(key).or_default();
            if window_open && series.len() >= self.lookback && self.lookback > 0 {
                let mean = series.iter().sum::<i64>() as f64 / series.len() as f64;
                let threshold = mean * (1.0 - self.dip_pct / 100.0);
                let item_key = (&snapshot.item, snapshot.float_bucket);
                let open = view
                    .open_positions_for(&snapshot.item, snapshot.float_bucket)
                    .count()
                    + planned.iter().filter(|k| **k == item_key).count();
                let fees = view.fees.get(snapshot.platform);
                if (snapshot.price.0 as f64) <= threshold
                    && open < self.max_open_per_item
                    && fees.seller_order_proceeds(Money(mean.floor() as i64), self.quantity)?
                        > fees.buyer_order_total(snapshot.price, self.quantity)?
                {
                    planned.push(item_key);
                    orders.push(buy_order(
                        Self::next_id(&name, counter),
                        snapshot,
                        self.quantity,
                    ));
                }
            }
            series.push_back(snapshot.price.0);
            while series.len() > self.lookback {
                series.pop_front();
            }
        }
        Ok(orders)
    }
}
