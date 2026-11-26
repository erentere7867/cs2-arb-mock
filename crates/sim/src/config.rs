use chrono::TimeDelta;
use cs2_arb_core::{Error, Money, Result};

use crate::fees::RoundingMode;

/// Valve's trade hold on newly acquired items. Not configurable on purpose.
pub const VALVE_TRADE_HOLD_DAYS: i64 = 7;

pub fn valve_trade_hold() -> TimeDelta {
    TimeDelta::days(VALVE_TRADE_HOLD_DAYS)
}

/// Which sales count toward an order's liquidity gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LiquidityScope {
    /// Only sales on the platform the order executes on (default).
    #[default]
    OrderPlatform,
    /// Sales on any platform.
    AllPlatforms,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiquidityConfig {
    /// Minimum verified, wear-comparable sales in the trailing window. Default 10.
    pub min_verified_sales: u32,
    /// Trailing window length in whole days. Default 7.
    pub window_days: u32,
    /// Max order quantity as a fraction of average daily volume, in basis points.
    /// Default 1000 (10%). The cap is floored; a zero cap rejects the order.
    pub max_adv_fraction_bps: u32,
    pub scope: LiquidityScope,
}

impl Default for LiquidityConfig {
    fn default() -> Self {
        Self {
            min_verified_sales: 10,
            window_days: 7,
            max_adv_fraction_bps: 1_000,
            scope: LiquidityScope::OrderPlatform,
        }
    }
}

/// Simulated time between submission and fill, drawn uniformly from the seeded RNG.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FillLatency {
    pub min: TimeDelta,
    pub max: TimeDelta,
}

impl Default for FillLatency {
    fn default() -> Self {
        Self {
            min: TimeDelta::zero(),
            max: TimeDelta::zero(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimConfig {
    /// Listings observed longer ago than this are stale and never filled. Default 10 min.
    pub max_listing_age: TimeDelta,
    pub liquidity: LiquidityConfig,
    pub fee_rounding: RoundingMode,
    pub fill_latency: FillLatency,
    /// `Some(cash)` enforces a cash balance on buys; `None` tracks cash flow only.
    pub starting_cash: Option<Money>,
    /// When false (default), matching uses core's `comparable_wear` verbatim: a
    /// tier-only order (no float bucket) only matches float-less listings, sales,
    /// and positions. When true, a tier-only order matches anything in its tier.
    pub tier_only_matches_any_float: bool,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            max_listing_age: TimeDelta::minutes(10),
            liquidity: LiquidityConfig::default(),
            fee_rounding: RoundingMode::HalfUp,
            fill_latency: FillLatency::default(),
            starting_cash: None,
            tier_only_matches_any_float: false,
        }
    }
}

impl SimConfig {
    pub fn validate(&self) -> Result<()> {
        let invalid = |msg: &str| Err(Error::InvalidConfig(msg.into()));
        if self.max_listing_age < TimeDelta::zero() {
            return invalid("max_listing_age must be nonnegative");
        }
        if self.liquidity.window_days == 0 {
            return invalid("liquidity window_days must be positive");
        }
        if self.liquidity.max_adv_fraction_bps > 10_000 {
            return invalid("max_adv_fraction_bps must be at most 10000");
        }
        if self.fill_latency.min < TimeDelta::zero()
            || self.fill_latency.max < self.fill_latency.min
        {
            return invalid("fill latency must satisfy 0 <= min <= max");
        }
        if self.starting_cash.is_some_and(|cash| cash.0 < 0) {
            return invalid("starting_cash must be nonnegative");
        }
        Ok(())
    }
}
