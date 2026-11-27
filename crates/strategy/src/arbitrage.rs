//! Cross-market spread arbitrage (mock only).
//!
//! For the same item identity and comparable wear (see [`comparable_wear`]), buy on
//! platform A at `ask * (1 + buyer_fee) + buyer_fixed` and sell on platform B for
//! `price * (1 - seller_fee) - seller_fixed`, after a hold (CS2 trade lock, 7 days by
//! default). The opportunity is accepted only if the expected net profit after
//!
//! * the opportunity cost of the locked capital: `buy_cost * annual_rate * hold_days / 365`,
//! * and an optional adverse price drift over the hold applied to the sale price,
//!
//! is strictly greater than [`SpreadConfig::min_net_profit`].

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use cs2_arb_core::{
    comparable_wear, Error, FeeSchedules, FloatBucket, ItemId, Money, Order, OrderSide, Platform,
    PriceSnapshot, Result,
};
use serde::Deserialize;

use crate::{cents_ceil, cents_floor, order_id, platform_slug};

/// Tunables for the spread strategy. Deserializable from TOML (all fields optional).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SpreadConfig {
    /// Days the purchased item is locked before it can be resold. Default 7.
    pub hold_days: u32,
    /// Annual opportunity-cost rate in percentage points (10.0 = 10 %/year). Default 10.0.
    pub annual_opportunity_rate_pct: f64,
    /// Expected adverse drift of the sale price in percentage points per day of hold.
    /// 0.0 (default) disables the haircut.
    pub price_drift_pct_per_day: f64,
    /// Accept only when expected net profit (for `quantity` units) is strictly above this.
    pub min_net_profit: Money,
    /// Cents to list below the sell-side reference price to get filled. Default 0.
    pub sell_undercut: Money,
    /// Units per opportunity. Default 1.
    pub quantity: u32,
    /// Ignore quotes older than this relative to `now` (and any quote from the future).
    /// `None` disables the age check (the future check always applies).
    pub max_quote_age_secs: Option<i64>,
}

impl Default for SpreadConfig {
    fn default() -> Self {
        Self {
            hold_days: 7,
            annual_opportunity_rate_pct: 10.0,
            price_drift_pct_per_day: 0.0,
            min_net_profit: Money(50),
            sell_undercut: Money(0),
            quantity: 1,
            max_quote_age_secs: Some(3600),
        }
    }
}

impl SpreadConfig {
    pub fn validate(&self) -> Result<()> {
        let bad = |msg: &str| Err(Error::InvalidConfig(format!("spread: {msg}")));
        if !self.annual_opportunity_rate_pct.is_finite() || self.annual_opportunity_rate_pct < 0.0 {
            return bad("annual_opportunity_rate_pct must be finite and >= 0");
        }
        if !self.price_drift_pct_per_day.is_finite() || self.price_drift_pct_per_day < 0.0 {
            return bad("price_drift_pct_per_day must be finite and >= 0");
        }
        if self.price_drift_pct_per_day * f64::from(self.hold_days) >= 100.0 {
            return bad("total drift over the hold must be below 100%");
        }
        if self.sell_undercut.0 < 0 {
            return bad("sell_undercut must be >= 0");
        }
        if self.quantity == 0 {
            return bad("quantity must be >= 1");
        }
        if matches!(self.max_quote_age_secs, Some(age) if age < 0) {
            return bad("max_quote_age_secs must be >= 0");
        }
        Ok(())
    }

    /// Fraction of capital charged over the hold: `rate/100 * hold_days / 365`.
    pub fn opportunity_cost_fraction(&self) -> f64 {
        self.annual_opportunity_rate_pct / 100.0 * f64::from(self.hold_days) / 365.0
    }

    /// Fraction of the sale price lost to drift over the hold.
    pub fn drift_fraction(&self) -> f64 {
        self.price_drift_pct_per_day / 100.0 * f64::from(self.hold_days)
    }
}

/// Full economics of buying on one platform and selling on another. All money is the total
/// for [`SpreadBreakdown::quantity`] units, in USD cents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpreadBreakdown {
    pub item: ItemId,
    pub float_bucket: Option<FloatBucket>,
    pub buy_platform: Platform,
    pub sell_platform: Platform,
    pub quantity: u32,
    /// Per-unit ask on the buy platform.
    pub buy_price: Money,
    /// Per-unit list price on the sell platform (reference price minus undercut).
    pub sell_price: Money,
    pub buy_observed_at: DateTime<Utc>,
    pub sell_observed_at: DateTime<Utc>,
    /// `(sell_price - buy_price) * quantity`, before any fees.
    pub gross_spread: Money,
    /// Buy price plus buyer fees.
    pub buy_cost: Money,
    /// Seller proceeds after the drift haircut and seller fees.
    pub sell_proceeds: Money,
    /// Reduction of seller proceeds caused by the drift haircut.
    pub drift_haircut: Money,
    /// Opportunity cost of `buy_cost` locked for `hold_days`.
    pub opportunity_cost: Money,
    /// `sell_proceeds - buy_cost - opportunity_cost`.
    pub expected_net_profit: Money,
    pub hold_days: u32,
}

impl SpreadBreakdown {
    /// Net profit over total cost (buy cost + opportunity cost), in percent.
    pub fn net_margin_pct(&self) -> f64 {
        let denom = self.buy_cost.0 + self.opportunity_cost.0;
        if denom <= 0 {
            return 0.0;
        }
        self.expected_net_profit.0 as f64 / denom as f64 * 100.0
    }

    /// Mock buy order on the buy platform at the observed ask.
    pub fn buy_order(&self, now: DateTime<Utc>) -> Order {
        Order {
            id: self.order_id("arb-buy", now),
            side: OrderSide::Buy,
            platform: self.buy_platform,
            item: self.item.clone(),
            float_bucket: self.float_bucket,
            quantity: self.quantity,
            limit_price: self.buy_price,
            submitted_at: now,
        }
    }

    /// Mock sell order on the sell platform; submit no earlier than [`Self::sell_not_before`].
    pub fn sell_order(&self, at: DateTime<Utc>) -> Order {
        Order {
            id: self.order_id("arb-sell", at),
            side: OrderSide::Sell,
            platform: self.sell_platform,
            item: self.item.clone(),
            float_bucket: self.float_bucket,
            quantity: self.quantity,
            limit_price: self.sell_price,
            submitted_at: at,
        }
    }

    /// Earliest time the planned sell can be submitted if bought at `bought_at`.
    pub fn sell_not_before(&self, bought_at: DateTime<Utc>) -> DateTime<Utc> {
        bought_at + Duration::days(i64::from(self.hold_days))
    }

    fn order_id(&self, prefix: &str, at: DateTime<Utc>) -> String {
        order_id(
            prefix,
            &[
                platform_slug(self.buy_platform),
                platform_slug(self.sell_platform),
                &self.item.market_hash_name,
            ],
            self.float_bucket,
            at,
        )
    }
}

/// Computes the spread economics for buying at `buy` and selling at `sell`, regardless of
/// profitability. Errors if the quotes are not comparable or on invalid money/config.
pub fn spread_breakdown(
    buy: &PriceSnapshot,
    sell: &PriceSnapshot,
    fees: &FeeSchedules,
    config: &SpreadConfig,
) -> Result<SpreadBreakdown> {
    config.validate()?;
    if buy.item != sell.item
        || !comparable_wear(
            buy.item.wear_tier,
            buy.float_bucket,
            sell.item.wear_tier,
            sell.float_bucket,
        )
    {
        return Err(Error::InvalidData(
            "spread quotes must share item identity and comparable wear".into(),
        ));
    }
    if buy.platform == sell.platform {
        return Err(Error::InvalidData("spread requires two platforms".into()));
    }
    if buy.price.0 <= 0 || sell.price.0 <= 0 {
        return Err(Error::InvalidData("quote prices must be positive".into()));
    }
    let sell_price = sell.price.checked_sub(config.sell_undercut)?;
    if sell_price.0 <= 0 {
        return Err(Error::InvalidData("undercut leaves no sale price".into()));
    }

    let buy_fees = fees.get(buy.platform);
    let sell_fees = fees.get(sell.platform);
    let drifted_price = cents_floor(sell_price.0 as f64 * (1.0 - config.drift_fraction()))?;
    let qty = config.quantity;
    let buy_cost = buy_fees.buyer_order_total(buy.price, qty)?;
    let sell_proceeds = sell_fees.seller_order_proceeds(drifted_price, qty)?;
    let drift_haircut = sell_fees
        .seller_order_proceeds(sell_price, qty)?
        .checked_sub(sell_proceeds)?;
    let opportunity_cost = cents_ceil(buy_cost.0 as f64 * config.opportunity_cost_fraction())?;
    let expected_net_profit = sell_proceeds
        .checked_sub(buy_cost)?
        .checked_sub(opportunity_cost)?;
    let gross_spread = sell_price.checked_sub(buy.price)?.checked_mul(qty)?;

    Ok(SpreadBreakdown {
        item: buy.item.clone(),
        float_bucket: buy.float_bucket,
        buy_platform: buy.platform,
        sell_platform: sell.platform,
        quantity: qty,
        buy_price: buy.price,
        sell_price,
        buy_observed_at: buy.observed_at,
        sell_observed_at: sell.observed_at,
        gross_spread,
        buy_cost,
        sell_proceeds,
        drift_haircut,
        opportunity_cost,
        expected_net_profit,
        hold_days: config.hold_days,
    })
}

/// Finds every buy-on-A / sell-on-B opportunity whose expected net profit exceeds the
/// threshold. Uses the newest quote per (platform, item, float bucket) that is not in the
/// future of `now` and not older than `max_quote_age_secs`. Results are sorted by expected
/// net profit, highest first.
pub fn evaluate_spread(
    quotes: &[PriceSnapshot],
    fees: &FeeSchedules,
    config: &SpreadConfig,
    now: DateTime<Utc>,
) -> Result<Vec<SpreadBreakdown>> {
    config.validate()?;
    let oldest = config
        .max_quote_age_secs
        .map(|age| now - Duration::seconds(age));

    let mut latest: HashMap<(Platform, &ItemId, Option<FloatBucket>), &PriceSnapshot> =
        HashMap::new();
    for quote in quotes {
        if quote.observed_at > now || oldest.is_some_and(|oldest| quote.observed_at < oldest) {
            continue;
        }
        latest
            .entry((quote.platform, &quote.item, quote.float_bucket))
            .and_modify(|current| {
                if quote.observed_at > current.observed_at {
                    *current = quote;
                }
            })
            .or_insert(quote);
    }

    let mut groups: HashMap<(&ItemId, Option<FloatBucket>), Vec<&PriceSnapshot>> = HashMap::new();
    for ((_, item, bucket), quote) in latest {
        groups.entry((item, bucket)).or_default().push(quote);
    }

    let mut out = Vec::new();
    for group in groups.values() {
        for buy in group {
            for sell in group {
                if buy.platform == sell.platform {
                    continue;
                }
                match spread_breakdown(buy, sell, fees, config) {
                    Ok(breakdown)
                        if breakdown.expected_net_profit > config.min_net_profit
                            && (breakdown.expected_net_profit.0 > 0
                                || (!fees.get(buy.platform).funding.enabled()
                                    && !fees.get(sell.platform).funding.enabled())) =>
                    {
                        out.push(breakdown)
                    }
                    Ok(_) => {}
                    Err(err) => tracing::debug!(
                        item = %buy.item.market_hash_name,
                        buy = platform_slug(buy.platform),
                        sell = platform_slug(sell.platform),
                        error = %err,
                        "skipping spread pair"
                    ),
                }
            }
        }
    }
    out.sort_by(|a, b| {
        b.expected_net_profit
            .cmp(&a.expected_net_profit)
            .then_with(|| a.item.market_hash_name.cmp(&b.item.market_hash_name))
            .then_with(|| platform_slug(a.buy_platform).cmp(platform_slug(b.buy_platform)))
            .then_with(|| platform_slug(a.sell_platform).cmp(platform_slug(b.sell_platform)))
    });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use cs2_arb_core::{FeeSchedule, WearTier};

    use super::*;

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2025, 1, 1, 12, 0, 0).unwrap()
    }

    fn item() -> ItemId {
        ItemId {
            market_hash_name: "AK-47 | Redline (Field-Tested)".into(),
            wear_tier: WearTier::FieldTested,
        }
    }

    fn quote(platform: Platform, price: i64) -> PriceSnapshot {
        PriceSnapshot {
            platform,
            item: item(),
            float_bucket: Some(FloatBucket::FieldTestedLow),
            price: Money(price),
            observed_at: t0(),
        }
    }

    fn schedule(
        buyer_pct: f64,
        seller_pct: f64,
        buyer_fixed: i64,
        seller_fixed: i64,
    ) -> FeeSchedule {
        FeeSchedule {
            buyer_fee_pct: buyer_pct,
            seller_fee_pct: seller_pct,
            buyer_fixed_fee: Money(buyer_fixed),
            seller_fixed_fee: Money(seller_fixed),
            ..FeeSchedule::default()
        }
    }

    fn fees() -> FeeSchedules {
        FeeSchedules {
            skinport: schedule(0.0, 8.0, 0, 0),
            csfloat: schedule(2.5, 2.0, 10, 0),
            dmarket: schedule(0.0, 7.0, 0, 5),
            waxpeer: schedule(0.0, 6.0, 0, 0),
        }
    }

    fn cfg() -> SpreadConfig {
        SpreadConfig {
            hold_days: 7,
            annual_opportunity_rate_pct: 10.0,
            price_drift_pct_per_day: 0.0,
            min_net_profit: Money(0),
            sell_undercut: Money(0),
            quantity: 1,
            max_quote_age_secs: None,
        }
    }

    #[test]
    fn spread_math_includes_fees_fixed_fees_and_opportunity_cost() {
        // Buy on CSFloat at 10_000: 10_000 + 2.5% (250) + 10 fixed = 10_260.
        // Sell on DMarket at 11_000: 11_000 - 7% (770) - 5 fixed = 10_225.
        // Opportunity: 10_260 * 0.10 * 7 / 365 = 19.676.. -> ceil 20.
        let b = spread_breakdown(
            &quote(Platform::CsFloat, 10_000),
            &quote(Platform::DMarket, 11_000),
            &fees(),
            &cfg(),
        )
        .unwrap();
        assert_eq!(b.buy_cost, Money(10_260));
        assert_eq!(b.sell_proceeds, Money(10_225));
        assert_eq!(b.opportunity_cost, Money(20));
        assert_eq!(b.drift_haircut, Money(0));
        assert_eq!(b.gross_spread, Money(1_000));
        assert_eq!(b.expected_net_profit, Money(10_225 - 10_260 - 20));
        assert!(
            b.expected_net_profit.0 < 0,
            "fees eat a 10% gross spread here"
        );
    }

    #[test]
    fn opportunity_cost_scales_with_rate_hold_and_capital() {
        let mut config = cfg();
        config.annual_opportunity_rate_pct = 36.5;
        config.quantity = 3;
        // Skinport buy: no buyer fee, 3 * 20_000 = 60_000 capital.
        // 60_000 * 0.365 * 7 / 365 = 420 exactly.
        let b = spread_breakdown(
            &quote(Platform::Skinport, 20_000),
            &quote(Platform::CsFloat, 25_000),
            &fees(),
            &config,
        )
        .unwrap();
        assert_eq!(b.buy_cost, Money(60_000));
        assert_eq!(b.opportunity_cost, Money(420));
        // CSFloat sell: 25_000 - 2% = 24_500 per unit.
        assert_eq!(b.sell_proceeds, Money(73_500));
        assert_eq!(b.expected_net_profit, Money(73_500 - 60_000 - 420));

        config.hold_days = 14;
        let b14 = spread_breakdown(
            &quote(Platform::Skinport, 20_000),
            &quote(Platform::CsFloat, 25_000),
            &fees(),
            &config,
        )
        .unwrap();
        assert_eq!(b14.opportunity_cost, Money(840));
    }

    #[test]
    fn drift_haircut_reduces_sale_price_over_the_hold() {
        let mut config = cfg();
        config.annual_opportunity_rate_pct = 0.0;
        config.price_drift_pct_per_day = 0.5; // 3.5% over 7 days
        let b = spread_breakdown(
            &quote(Platform::Skinport, 10_000),
            &quote(Platform::CsFloat, 12_000),
            &fees(),
            &config,
        )
        .unwrap();
        // Drifted price 12_000 * 0.965 = 11_580; 2% fee = 231.6 -> 232; proceeds 11_348.
        // Undrifted proceeds 12_000 - 240 = 11_760; haircut 412.
        assert_eq!(b.sell_proceeds, Money(11_348));
        assert_eq!(b.drift_haircut, Money(412));
        assert_eq!(b.expected_net_profit, Money(1_348));
    }

    #[test]
    fn waxpeer_can_win_either_side_and_funding_can_erase_the_spread() {
        let mut config = cfg();
        config.annual_opportunity_rate_pct = 0.0;
        let mut fees = fees();
        let waxpeer_buy = quote(Platform::Waxpeer, 10_000);
        let skinport_sell = quote(Platform::Skinport, 11_000);
        let found = evaluate_spread(
            &[waxpeer_buy.clone(), skinport_sell.clone()],
            &fees,
            &config,
            t0(),
        )
        .unwrap();
        assert_eq!(found[0].buy_platform, Platform::Waxpeer);
        fees.waxpeer.funding.crypto_deposit.fixed = Money(120);
        assert!(
            evaluate_spread(&[waxpeer_buy, skinport_sell], &fees, &config, t0())
                .unwrap()
                .is_empty()
        );

        let waxpeer_sell = quote(Platform::Waxpeer, 11_000);
        let skinport_buy = quote(Platform::Skinport, 10_000);
        let found = evaluate_spread(
            &[waxpeer_sell.clone(), skinport_buy.clone()],
            &fees,
            &config,
            t0(),
        )
        .unwrap();
        assert_eq!(found[0].sell_platform, Platform::Waxpeer);
        fees.waxpeer.funding.crypto_withdrawal.fixed = Money(200);
        fees.waxpeer.funding.bank_cashout.fixed = Money(150);
        assert!(
            evaluate_spread(&[waxpeer_sell, skinport_buy], &fees, &config, t0())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn threshold_is_strict_and_applied_after_opportunity_cost() {
        let mut config = cfg();
        config.annual_opportunity_rate_pct = 36.5; // 0.7% over 7 days
                                                   // Skinport buy 10_000 (cost 10_000, opp cost 70), CSFloat sell 10_500 -> 10_290.
                                                   // Net = 10_290 - 10_000 - 70 = 220.
        let quotes = [
            quote(Platform::Skinport, 10_000),
            quote(Platform::CsFloat, 10_500),
        ];
        config.min_net_profit = Money(219);
        let found = evaluate_spread(&quotes, &fees(), &config, t0()).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].expected_net_profit, Money(220));
        assert_eq!(found[0].buy_platform, Platform::Skinport);
        assert_eq!(found[0].sell_platform, Platform::CsFloat);

        config.min_net_profit = Money(220);
        assert!(evaluate_spread(&quotes, &fees(), &config, t0())
            .unwrap()
            .is_empty());

        // Without the opportunity cost it would have cleared 220.
        config.annual_opportunity_rate_pct = 0.0;
        assert_eq!(
            evaluate_spread(&quotes, &fees(), &config, t0())
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn never_pairs_different_wear_buckets_items_or_stale_quotes() {
        let config = SpreadConfig {
            max_quote_age_secs: Some(60),
            ..cfg()
        };
        let mut other_bucket = quote(Platform::CsFloat, 20_000);
        other_bucket.float_bucket = Some(FloatBucket::FieldTestedHigh);
        let mut tier_only = quote(Platform::DMarket, 20_000);
        tier_only.float_bucket = None;
        let mut other_item = quote(Platform::DMarket, 20_000);
        other_item.item.market_hash_name = "AK-47 | Redline (Minimal Wear)".into();
        other_item.item.wear_tier = WearTier::MinimalWear;
        other_item.float_bucket = Some(FloatBucket::MinimalWear);
        let quotes = vec![
            quote(Platform::Skinport, 10_000),
            other_bucket,
            tier_only,
            other_item,
        ];
        assert!(evaluate_spread(&quotes, &fees(), &config, t0())
            .unwrap()
            .is_empty());

        let mut stale = quote(Platform::CsFloat, 20_000);
        stale.observed_at = t0() - Duration::seconds(61);
        let mut future = quote(Platform::DMarket, 20_000);
        future.observed_at = t0() + Duration::seconds(1);
        let quotes = vec![quote(Platform::Skinport, 10_000), stale, future];
        assert!(evaluate_spread(&quotes, &fees(), &config, t0())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn qualified_buckets_get_separate_buy_and_sell_order_ids() {
        let buy = quote(Platform::Skinport, 10_000);
        let sell = quote(Platform::CsFloat, 20_000);
        let mut high_buy = buy.clone();
        let mut high_sell = sell.clone();
        high_buy.float_bucket = Some(FloatBucket::FieldTestedHigh);
        high_sell.float_bucket = high_buy.float_bucket;
        let spreads =
            evaluate_spread(&[buy, sell, high_buy, high_sell], &fees(), &cfg(), t0()).unwrap();
        assert_eq!(spreads.len(), 2);
        assert_ne!(spreads[0].buy_order(t0()).id, spreads[1].buy_order(t0()).id);
        assert_ne!(
            spreads[0].sell_order(t0()).id,
            spreads[1].sell_order(t0()).id
        );
    }

    #[test]
    fn uses_newest_quote_and_sorts_by_profit() {
        let mut old = quote(Platform::CsFloat, 50_000);
        old.observed_at = t0() - Duration::seconds(10);
        let quotes = vec![
            old,
            quote(Platform::CsFloat, 12_000),
            quote(Platform::Skinport, 10_000),
            quote(Platform::DMarket, 13_000),
        ];
        let found = evaluate_spread(&quotes, &fees(), &cfg(), t0()).unwrap();
        assert!(!found.is_empty());
        assert!(found
            .windows(2)
            .all(|w| w[0].expected_net_profit >= w[1].expected_net_profit));
        // Best: buy Skinport 10_000, sell DMarket 13_000 -> 13_000 - 910 - 5 = 12_085.
        assert_eq!(found[0].buy_platform, Platform::Skinport);
        assert_eq!(found[0].sell_platform, Platform::DMarket);
        assert_eq!(found[0].sell_proceeds, Money(12_085));
        // The stale 50_000 CSFloat quote was superseded by the newer 12_000 one.
        assert!(found.iter().all(|b| b.sell_price != Money(50_000)));

        let buy = found[0].buy_order(t0());
        assert_eq!(buy.side, OrderSide::Buy);
        assert_eq!(buy.platform, Platform::Skinport);
        assert_eq!(buy.limit_price, Money(10_000));
        let sell_at = found[0].sell_not_before(t0());
        assert_eq!(sell_at, t0() + Duration::days(7));
        let sell = found[0].sell_order(sell_at);
        assert_eq!(sell.side, OrderSide::Sell);
        assert_eq!(sell.platform, Platform::DMarket);
        assert_eq!(sell.limit_price, Money(13_000));
    }

    #[test]
    fn validates_config_and_rejects_bad_quotes() {
        let bad = SpreadConfig {
            quantity: 0,
            ..cfg()
        };
        assert!(evaluate_spread(&[], &fees(), &bad, t0()).is_err());
        let bad = SpreadConfig {
            price_drift_pct_per_day: 20.0,
            ..cfg()
        };
        assert!(bad.validate().is_err());
        assert!(spread_breakdown(
            &quote(Platform::Skinport, 0),
            &quote(Platform::CsFloat, 100),
            &fees(),
            &cfg()
        )
        .is_err());
        assert!(spread_breakdown(
            &quote(Platform::Skinport, 100),
            &quote(Platform::Skinport, 200),
            &fees(),
            &cfg()
        )
        .is_err());
    }

    #[test]
    fn config_deserializes_with_defaults() {
        let config: SpreadConfig =
            toml::from_str("annual_opportunity_rate_pct = 5.0\nmin_net_profit = 100").unwrap();
        assert_eq!(config.annual_opportunity_rate_pct, 5.0);
        assert_eq!(config.min_net_profit, Money(100));
        assert_eq!(config.hold_days, 7);
        assert!(toml::from_str::<SpreadConfig>("unknown = 1").is_err());
    }

    #[test]
    fn fixture_spreads_are_consistent() {
        let quotes: Vec<PriceSnapshot> =
            serde_json::from_str(include_str!("../../../data/fixtures/price_snapshots.json"))
                .unwrap();
        let fees = FeeSchedules::from_toml(include_str!("../../../config/default.toml")).unwrap();
        let config = SpreadConfig {
            min_net_profit: Money(i64::MIN + 1),
            ..SpreadConfig::default()
        };
        let found = evaluate_spread(&quotes, &fees, &config, t0()).unwrap();
        // 4 items x 3 platforms at t0 -> 6 ordered pairs per item.
        assert_eq!(found.len(), 24);
        for b in &found {
            assert_eq!(
                b.expected_net_profit,
                Money(b.sell_proceeds.0 - b.buy_cost.0 - b.opportunity_cost.0)
            );
        }
    }
}
