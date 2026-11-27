use std::collections::HashSet;

use chrono::Duration;
use cs2_arb_core::{Money, Order, Position, Result, Sale};
use cs2_arb_strategy::{DipConfig, DipDetector};

use super::{buy_order, exit_orders, ExitRule};
use crate::replay::{MarketView, StrategyAdapter};

#[derive(Debug, Clone, Default)]
pub(crate) struct DipLiquidations(HashSet<String>);

impl DipLiquidations {
    pub(crate) fn update(&mut self, view: &MarketView<'_>, stop_loss_pct: f64, max_hold: Duration) {
        self.0
            .retain(|id| view.positions.iter().any(|position| &position.id == id));
        for position in view.positions {
            if position.is_trade_locked_at(view.now) || self.contains(position) {
                continue;
            }
            let expired = view.now - position.acquired_at >= max_hold;
            let stopped = if expired {
                false
            } else {
                let mut prices: Vec<i64> = view
                    .quotes_for(&position.item, position.float_bucket)
                    .into_iter()
                    .filter(|quote| quote.price.0 > 0 && view.quote_is_fresh(quote))
                    .map(|quote| quote.price.0)
                    .collect();
                prices.sort_unstable();
                if prices.is_empty() {
                    false
                } else {
                    let (lower, upper) = (prices[(prices.len() - 1) / 2], prices[prices.len() / 2]);
                    let median = (lower as f64 + upper as f64) / 2.0;
                    median * f64::from(position.quantity)
                        < position.cost_basis.0 as f64 * (1.0 - stop_loss_pct / 100.0)
                }
            };
            if expired || stopped {
                self.0.insert(position.id.clone());
            }
        }
    }

    pub(crate) fn contains(&self, position: &Position) -> bool {
        self.0.contains(&position.id)
    }
}

/// Replay adapter for the shared rolling dip detector.
#[derive(Debug, Clone)]
pub struct DipStrategy {
    pub name: String,
    pub max_open_per_item: usize,
    pub exit: ExitRule,
    detector: DipDetector,
    liquidations: DipLiquidations,
    next_id: u64,
}

impl Default for DipStrategy {
    fn default() -> Self {
        Self::with_sales(DipConfig::default(), &[]).expect("default dip config is valid")
    }
}

impl DipStrategy {
    pub fn with_sales(config: DipConfig, sales: &[Sale]) -> Result<Self> {
        let max_hold_days = config.max_hold_days;
        let mut detector = DipDetector::new(config)?;
        detector.record_sales(sales);
        Ok(Self {
            name: "dip".into(),
            max_open_per_item: 2,
            exit: ExitRule {
                take_profit_pct: 5.0,
                max_hold: Duration::days(max_hold_days),
            },
            detector,
            liquidations: DipLiquidations::default(),
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
        self.liquidations.update(
            view,
            self.detector.config().stop_loss_pct,
            self.exit.max_hold,
        );
        let mut orders = exit_orders(
            view,
            self.exit,
            |position| self.liquidations.contains(position),
            || Self::next_id(&name, counter),
        )?;

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
