//! Local-only, deterministic order simulation and inventory accounting.
//!
//! There is no code path here that can place a real order: the crate has no
//! network or marketplace-client dependency, and [`MockExecutor::submit`] only
//! mutates in-memory simulated state.
//!
//! Rules enforced on every order:
//! * Liquidity gate: at least `min_verified_sales` (10) verified, wear-comparable
//!   sales in the trailing `window_days` (7); quantity capped at
//!   `floor(ADV * 10%)`, and a zero cap rejects.
//! * Wear conditioning via core's `comparable_wear` (never across tiers or
//!   low/high float buckets).
//! * Stale (older than `max_listing_age`) and trade-locked listings never fill.
//! * Each buy opens a position locked for 7 days from the fill time; locked
//!   units cannot be sold.
//! * Buyer fees are added to cost basis; seller fees are deducted from proceeds.
//!   Percentage fees round per unit with an explicit [`RoundingMode`]; fixed fees apply once per order.

pub mod book;
pub mod buy_order_fill;
pub mod clock {
    pub use cs2_arb_core::{Clock, ManualClock, SystemClock};
}
pub mod config;
pub mod execution;
pub mod executor;
pub mod fees;
pub mod liquidity;
pub mod portfolio;
pub mod rejection;
pub mod rng;
pub mod venues;
pub mod wear;

pub use book::MarketBook;
pub use buy_order_fill::{
    BuyOrderFillConfig, BuyOrderFillSimulator, BuyOrderStatus, RestingBuyOrder,
};
pub use clock::{Clock, ManualClock, SystemClock};
pub use config::{
    valve_trade_hold, FillLatency, LiquidityConfig, LiquidityScope, SimConfig,
    VALVE_TRADE_HOLD_DAYS,
};
pub use execution::simulate_fill;
pub use executor::{ExecutionReport, MockExecutor};
pub use fees::{ExactFeeSchedule, FeeBreakdown, FeeRate, PlatformFees, RoundingMode, RATE_SCALE};
pub use liquidity::LiquidityStats;
pub use portfolio::{LedgerEntry, Portfolio, PortfolioSummary};
pub use rejection::{Rejection, RejectionRecord};
pub use rng::SplitMix64;

#[cfg(test)]
mod tests;
