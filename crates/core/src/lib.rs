//! Shared contracts for a mock-only market simulator. No marketplace API can place orders.

mod error;
mod event;
mod execution;
mod fees;
mod market;

pub use error::{Error, Result};
pub use event::{MarketEvent, CS2_APP_ID};
pub use execution::{Fill, Order, OrderSide, Position, SimulationMode};
pub use fees::{FeeSchedule, FeeSchedules, Money};
pub use market::{
    comparable_wear, FloatBucket, ItemId, Listing, MarketClient, Platform, PriceSnapshot, Sale,
    WearTier,
};
