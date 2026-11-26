//! Simple built-in strategies for exercising the replay engine. They are
//! stand-ins until `cs2-arb-strategy` is plugged in through [`StrategyAdapter`].
//!
//! [`StrategyAdapter`]: crate::replay::StrategyAdapter

mod dip;
mod spread;

pub use dip::DipStrategy;
pub use spread::SpreadStrategy;

use chrono::Duration;
use cs2_arb_core::{Money, Order, OrderSide, Result};

use crate::replay::MarketView;

/// Exit rules shared by the built-in strategies.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExitRule {
    /// Sell an unlocked position once the best net exit is at least this
    /// percentage above its cost basis.
    pub take_profit_pct: f64,
    /// Sell an unlocked position at the best net exit once it has been held this long.
    pub max_hold: Duration,
}

/// Sell orders for this strategy's unlocked positions that meet `rule`.
///
/// Walks positions in FIFO order per item and stops at the first one that does
/// not qualify, so the engine's FIFO matching sells exactly the chosen lots.
pub(crate) fn exit_orders(
    view: &MarketView<'_>,
    rule: ExitRule,
    mut next_id: impl FnMut() -> String,
) -> Result<Vec<Order>> {
    let mut orders = Vec::new();
    let mut blocked: Vec<(&cs2_arb_core::ItemId, Option<cs2_arb_core::FloatBucket>)> = Vec::new();
    for position in view.positions {
        let key = (&position.item, position.float_bucket);
        if blocked.contains(&key) {
            continue;
        }
        if position.is_trade_locked_at(view.now) {
            blocked.push(key);
            continue;
        }
        let Some((platform, quote, net)) =
            view.best_exit(&position.item, position.float_bucket, position.quantity)?
        else {
            blocked.push(key);
            continue;
        };
        let target = position.cost_basis.0 as f64 * (1.0 + rule.take_profit_pct / 100.0);
        let expired = view.now - position.acquired_at >= rule.max_hold;
        if net.0 as f64 >= target || expired {
            orders.push(Order {
                id: next_id(),
                side: OrderSide::Sell,
                platform,
                item: position.item.clone(),
                float_bucket: position.float_bucket,
                quantity: position.quantity,
                limit_price: quote.price,
                submitted_at: view.now,
            });
        } else {
            blocked.push(key);
        }
    }
    Ok(orders)
}

pub(crate) fn buy_order(id: String, quote: &cs2_arb_core::PriceSnapshot, quantity: u32) -> Order {
    Order {
        id,
        side: OrderSide::Buy,
        platform: quote.platform,
        item: quote.item.clone(),
        float_bucket: quote.float_bucket,
        quantity,
        limit_price: quote.price,
        submitted_at: quote.observed_at,
    }
}

pub(crate) fn pct_of(amount: Money, base: Money) -> f64 {
    if base.0 == 0 {
        0.0
    } else {
        amount.0 as f64 / base.0 as f64 * 100.0
    }
}
