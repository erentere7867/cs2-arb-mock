//! Trailing-window verified-sales gate and average-daily-volume quantity cap.

use chrono::{DateTime, TimeDelta, Utc};
use cs2_arb_core::{FloatBucket, ItemId, Platform, Sale};
use serde::Serialize;

use crate::config::{LiquidityConfig, LiquidityScope};
use crate::wear::order_matches_float;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct LiquidityStats {
    /// Verified, wear-comparable sales in `(now - window, now]`.
    pub verified_sales: u32,
    pub window_days: u32,
    /// `floor(verified_sales / window_days * fraction)`; 0 means untradeable.
    pub max_order_quantity: u32,
    /// Whether `verified_sales >= min_verified_sales`.
    pub meets_minimum: bool,
}

/// Assess liquidity for an order on `platform` for `item`/`bucket` at `now`.
///
/// Counted sales must be verified, of the same item (name and tier), wear
/// comparable to the order, on the order platform (unless scope is
/// `AllPlatforms`), and sold in `(now - window, now]`. Sales after `now` are
/// ignored so replays cannot look ahead.
pub fn assess<'a>(
    config: &LiquidityConfig,
    sales: impl IntoIterator<Item = &'a Sale>,
    platform: Platform,
    item: &ItemId,
    bucket: Option<FloatBucket>,
    now: DateTime<Utc>,
    tier_only_any: bool,
) -> LiquidityStats {
    let window_start = now - TimeDelta::days(i64::from(config.window_days));
    let count = sales
        .into_iter()
        .filter(|sale| {
            sale.verified
                && &sale.item == item
                && (config.scope == LiquidityScope::AllPlatforms || sale.platform == platform)
                && sale.sold_at > window_start
                && sale.sold_at <= now
                && order_matches_float(
                    item.wear_tier,
                    bucket,
                    sale.item.wear_tier,
                    sale.float_value,
                    tier_only_any,
                )
        })
        .count();
    let verified_sales = u32::try_from(count).unwrap_or(u32::MAX);
    LiquidityStats {
        verified_sales,
        window_days: config.window_days,
        max_order_quantity: quantity_cap(config, verified_sales),
        meets_minimum: verified_sales >= config.min_verified_sales,
    }
}

/// `floor(sales * bps / (window_days * 10_000))`, i.e. floor(ADV * fraction).
pub fn quantity_cap(config: &LiquidityConfig, verified_sales: u32) -> u32 {
    let numerator = u64::from(verified_sales) * u64::from(config.max_adv_fraction_bps);
    let denominator = u64::from(config.window_days.max(1)) * 10_000;
    u32::try_from(numerator / denominator).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_is_floor_of_ten_percent_of_average_daily_volume() {
        let config = LiquidityConfig::default();
        // ADV = sales / 7; cap = floor(ADV / 10) = floor(sales / 70).
        assert_eq!(quantity_cap(&config, 0), 0);
        assert_eq!(quantity_cap(&config, 10), 0);
        assert_eq!(quantity_cap(&config, 69), 0);
        assert_eq!(quantity_cap(&config, 70), 1);
        assert_eq!(quantity_cap(&config, 139), 1);
        assert_eq!(quantity_cap(&config, 140), 2);
        assert_eq!(quantity_cap(&config, 700), 10);
        let half = LiquidityConfig {
            max_adv_fraction_bps: 5_000,
            window_days: 1,
            ..config
        };
        assert_eq!(quantity_cap(&half, 3), 1);
    }
}
