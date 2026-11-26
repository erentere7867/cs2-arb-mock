//! Historical mock replay and process operations.
//!
//! Everything here is simulation: strategies emit `Order` values that are filled
//! by an in-process [`FillModel`] against replayed quotes. No code in this crate
//! can place a real order or contact a marketplace.

pub mod fill;
pub mod fixtures;
pub mod integrated;
pub mod ops;
pub mod replay;
pub mod report;
pub mod strategies;

pub use fill::{FillModel, FillOutcome, SnapshotFillModel, DEFAULT_TRADE_HOLD};
pub use ops::{Secret, Secrets};
pub use replay::{
    default_strategies, replay, BacktestConfig, Backtester, MarketView, QuoteKey, StrategyAdapter,
};
pub use report::{PerformanceReport, ReplayWindow, StrategyStats, TradeRecord};
pub use strategies::{DipStrategy, ExitRule, SpreadStrategy};
