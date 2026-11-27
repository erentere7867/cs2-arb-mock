use chrono::Duration;
use cs2_arb_core::{Money, Order, Result, Sale};
use cs2_arb_strategy::{DipConfig, DipDetector};

use super::{buy_order, exit_orders, ExitRule};
use crate::replay::{MarketView, StrategyAdapter};

/// Replay adapter for the shared rolling dip detector.
#[derive(Debug, Clone)]
pub struct DipStrategy {
    pub name: String,
    pub max_open_per_item: usize,
    pub exit: ExitRule,
    detector: DipDetector,
    next_id: u64,
}

impl Default for DipStrategy {
    fn default() -> Self {
        Self::with_sales(DipConfig::default(), &[]).expect("default dip config is valid")
    }
}

impl DipStrategy {
    pub fn with_sales(config: DipConfig, sales: &[Sale]) -> Result<Self> {
        let mut detector = DipDetector::new(config)?;
        detector.record_sales(sales);
        Ok(Self {
            name: "dip".into(),
            max_open_per_item: 2,
            exit: ExitRule {
                take_profit_pct: 5.0,
                max_hold: Duration::days(21),
            },
            detector,
            next_id: 0,
        })
    }

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

        for event in view.new_events {
            self.detector.apply_event(event);
        }
        let mut planned: Vec<(&cs2_arb_core::ItemId, Option<cs2_arb_core::FloatBucket>)> =
            Vec::new();
        for snapshot in view.batch {
            if let Some(signal) = self.detector.observe(snapshot)? {
                let quantity = signal.order.quantity;
                let item_key = (&snapshot.item, snapshot.float_bucket);
                let open = view
                    .open_positions_for(&snapshot.item, snapshot.float_bucket)
                    .count()
                    + planned.iter().filter(|k| **k == item_key).count();
                let fees = view.fees.get(snapshot.platform);
                if open < self.max_open_per_item
                    && fees
                        .seller_order_proceeds(Money(signal.stats.mean.floor() as i64), quantity)?
                        > fees.buyer_order_total(snapshot.price, quantity)?
                {
                    planned.push(item_key);
                    orders.push(buy_order(Self::next_id(&name, counter), snapshot, quantity));
                }
            }
        }
        Ok(orders)
    }
}
