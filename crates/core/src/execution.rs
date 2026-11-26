use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{FloatBucket, ItemId, Money, Platform};

/// The only execution mode. Orders are data for the in-process simulator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SimulationMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderSide {
    Buy,
    Sell,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Order {
    pub id: String,
    pub side: OrderSide,
    pub platform: Platform,
    pub item: ItemId,
    pub float_bucket: Option<FloatBucket>,
    pub quantity: u32,
    pub limit_price: Money,
    pub submitted_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fill {
    pub order: Order,
    pub quantity: u32,
    pub unit_price: Money,
    pub fee: Money,
    pub filled_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub id: String,
    pub item: ItemId,
    pub float_bucket: Option<FloatBucket>,
    pub acquired_on: Platform,
    pub quantity: u32,
    /// Total cost for the remaining quantity, including entry fees.
    pub cost_basis: Money,
    pub acquired_at: DateTime<Utc>,
    pub locked_until: DateTime<Utc>,
}

impl Position {
    pub fn is_trade_locked_at(&self, now: DateTime<Utc>) -> bool {
        now < self.locked_until
    }
}
