//! Decision logic for **mock** orders; owns no marketplace execution surface.
//!
//! Everything here produces [`cs2_arb_core::Order`] values as plain data. Orders only ever
//! reach an in-process simulator through the local [`executor::SimExecutor`] trait; this
//! crate has no network dependency and cannot place real trades.
//!
//! * [`arbitrage`]: cross-market net spread with a hold-period opportunity cost.
//! * [`dip`]: rolling 30-day mean / standard deviation dip detector gated on liquidity and
//!   update/panic windows derived from [`cs2_arb_core::MarketEvent`].
//! * [`risk`]: position, item and total exposure limits, a UTC-daily drawdown halt, and the
//!   emergency [`kill_switch::KillSwitch`].

pub mod arbitrage;
pub mod dip;
pub mod executor;
pub mod kill_switch;
pub mod risk;

pub use arbitrage::{evaluate_spread, spread_breakdown, SpreadBreakdown, SpreadConfig};
pub use dip::{
    evaluate_dip, rolling_stats, DipConfig, DipDetector, DipEvaluation, DipSignal, EventWindows,
    RollingStats,
};
pub use executor::{submit_with_risk, SimExecutor, SubmitError};
pub use kill_switch::KillSwitch;
pub use risk::{RiskLimits, RiskManager, RiskRejection};

use chrono::{DateTime, Utc};
use cs2_arb_core::{Error, Money, Platform, Result};

pub(crate) fn platform_slug(platform: Platform) -> &'static str {
    match platform {
        Platform::Skinport => "skinport",
        Platform::CsFloat => "csfloat",
        Platform::DMarket => "dmarket",
    }
}

pub(crate) fn order_id(prefix: &str, parts: &[&str], at: DateTime<Utc>) -> String {
    let mut id = String::from(prefix);
    for part in parts {
        id.push(':');
        id.push_str(part);
    }
    id.push(':');
    id.push_str(&at.timestamp_millis().to_string());
    id
}

/// Converts a finite, in-range f64 number of cents to [`Money`], rounding up (costs).
pub(crate) fn cents_ceil(value: f64) -> Result<Money> {
    to_money(value.ceil())
}

/// Converts a finite, in-range f64 number of cents to [`Money`], rounding down (revenues).
pub(crate) fn cents_floor(value: f64) -> Result<Money> {
    to_money(value.floor())
}

fn to_money(value: f64) -> Result<Money> {
    // i64::MAX is not exactly representable; stay strictly inside the range.
    if !value.is_finite() || value >= i64::MAX as f64 || value <= i64::MIN as f64 {
        return Err(Error::MoneyOverflow);
    }
    Ok(Money(value as i64))
}
