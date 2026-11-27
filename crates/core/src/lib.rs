//! Shared contracts for a mock-only market simulator. No marketplace API can place orders.

mod clock;
mod error;
mod event;
mod execution;
mod fees;
mod game;
mod market;

pub use clock::{Clock, ManualClock, SystemClock};
pub use error::{Error, Result};
pub use event::{MarketEvent, CS2_APP_ID};
pub use execution::{Fill, Order, OrderSide, Position, SimulationMode};
pub use fees::{
    fee_for_order, FeeSchedule, FeeSchedules, FundingCharge, FundingFees, Money, TradeFees,
};
pub use game::{Game, RUST_APP_ID};
pub use market::{
    comparable_wear, BuyOrder, FloatBucket, ItemId, Listing, MarketClient, Platform, PriceSnapshot,
    Sale, WearTier,
};
