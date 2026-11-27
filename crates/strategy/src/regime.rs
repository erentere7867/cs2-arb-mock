use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use cs2_arb_core::{Error, FloatBucket, ItemId, Platform, PriceSnapshot, Result, Sale};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RegimeConfig {
    pub min_index_items: usize,
    pub min_case_items: usize,
    pub max_price_age_hours: u32,
    pub short_loss_pct: f64,
    pub long_loss_pct: f64,
    pub item_underperformance_pct: f64,
}

impl Default for RegimeConfig {
    fn default() -> Self {
        Self {
            min_index_items: 3,
            min_case_items: 2,
            max_price_age_hours: 24,
            short_loss_pct: 3.0,
            long_loss_pct: 8.0,
            item_underperformance_pct: 5.0,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RegimeStatus {
    RiskOn,
    #[default]
    InsufficientHistory,
    MarketDowntrend,
    CaseDowntrend,
}

impl RegimeStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::RiskOn => "Risk-on",
            Self::InsufficientHistory => "Risk-off: insufficient history",
            Self::MarketDowntrend => "Risk-off: market downtrend",
            Self::CaseDowntrend => "Risk-off: case downtrend",
        }
    }
}

#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct IndexReturns {
    pub short_pct: Option<f64>,
    pub long_pct: Option<f64>,
}

#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct RegimeState {
    pub status: RegimeStatus,
    pub market: IndexReturns,
    pub cases: IndexReturns,
}

#[derive(Debug)]
pub struct MarketRegime {
    config: RegimeConfig,
    prices: HashMap<ItemId, HashMap<PriceSource, PriceHistory>>,
}

type PriceHistory = Vec<(DateTime<Utc>, i64)>;

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
enum PriceSource {
    Snapshot(Platform, Option<FloatBucket>),
    VerifiedSale(Platform, Option<FloatBucket>),
}

pub struct RegimeAt<'a> {
    pub state: RegimeState,
    short_returns: HashMap<&'a ItemId, f64>,
    item_underperformance_pct: f64,
}

impl RegimeAt<'_> {
    pub fn allows(&self, item: &ItemId) -> bool {
        self.state.status == RegimeStatus::RiskOn
            && self
                .short_returns
                .get(item)
                .zip(self.state.market.short_pct)
                .is_some_and(|(&change, index)| index - change <= self.item_underperformance_pct)
    }
}

impl MarketRegime {
    pub fn new(config: RegimeConfig, snapshots: &[PriceSnapshot], sales: &[Sale]) -> Result<Self> {
        if config.min_index_items == 0
            || config.min_case_items == 0
            || !(1..=24).contains(&config.max_price_age_hours)
            || [
                config.short_loss_pct,
                config.long_loss_pct,
                config.item_underperformance_pct,
            ]
            .into_iter()
            .any(|pct| !pct.is_finite() || pct <= 0.0 || pct > 100.0)
        {
            return Err(Error::InvalidConfig(
                "invalid market_regime configuration".into(),
            ));
        }
        let mut prices: HashMap<ItemId, HashMap<PriceSource, PriceHistory>> = HashMap::new();
        for snapshot in snapshots.iter().filter(|quote| quote.price.0 > 0) {
            prices
                .entry(snapshot.item.clone())
                .or_default()
                .entry(PriceSource::Snapshot(
                    snapshot.platform,
                    snapshot.float_bucket,
                ))
                .or_default()
                .push((snapshot.observed_at, snapshot.price.0));
        }
        let mut seen = HashSet::new();
        for sale in sales
            .iter()
            .filter(|sale| sale.verified && sale.price.0 > 0)
        {
            if seen.insert((sale.platform, &sale.id)) {
                prices
                    .entry(sale.item.clone())
                    .or_default()
                    .entry(PriceSource::VerifiedSale(
                        sale.platform,
                        sale.float_value.and_then(FloatBucket::from_float),
                    ))
                    .or_default()
                    .push((sale.sold_at, sale.price.0));
            }
        }
        for sources in prices.values_mut() {
            for (source, points) in sources {
                points.sort_unstable();
                if matches!(source, PriceSource::Snapshot(..)) {
                    points.dedup();
                }
            }
        }
        Ok(Self { config, prices })
    }

    pub fn at(&self, now: DateTime<Utc>) -> RegimeAt<'_> {
        let age = Duration::hours(i64::from(self.config.max_price_age_hours));
        let mut market = [Vec::new(), Vec::new()];
        let mut cases = [Vec::new(), Vec::new()];
        let mut short_returns = HashMap::new();
        let anchors = [1, 7].map(|days| now.checked_sub_signed(Duration::days(days)));
        for (item, sources) in &self.prices {
            let mut changes = [Vec::new(), Vec::new()];
            for series in sources.values() {
                let Some(current) = price_at(series, now, age) else {
                    continue;
                };
                for (i, anchor) in anchors.iter().enumerate() {
                    if let Some(prior) = anchor.and_then(|anchor| price_at(series, anchor, age)) {
                        changes[i].push((current - prior) * 100.0 / prior);
                    }
                }
            }
            for (i, changes) in changes.into_iter().enumerate() {
                let Some(change) = index_median(changes, 1) else {
                    continue;
                };
                market[i].push(change);
                if item.market_hash_name.ends_with(" Case") {
                    cases[i].push(change);
                }
                if i == 0 {
                    short_returns.insert(item, change);
                }
            }
        }
        let returns = |[short, long]: [Vec<f64>; 2], min| IndexReturns {
            short_pct: index_median(short, min),
            long_pct: index_median(long, min),
        };
        let market = returns(market, self.config.min_index_items);
        let cases = returns(cases, self.config.min_case_items);
        let falling = |index: IndexReturns| {
            index
                .short_pct
                .is_some_and(|pct| pct <= -self.config.short_loss_pct)
                || index
                    .long_pct
                    .is_some_and(|pct| pct <= -self.config.long_loss_pct)
        };
        let status = if falling(market) {
            RegimeStatus::MarketDowntrend
        } else if falling(cases) {
            RegimeStatus::CaseDowntrend
        } else if market.short_pct.is_none() || market.long_pct.is_none() {
            RegimeStatus::InsufficientHistory
        } else {
            RegimeStatus::RiskOn
        };
        RegimeAt {
            state: RegimeState {
                status,
                market,
                cases,
            },
            short_returns,
            item_underperformance_pct: self.config.item_underperformance_pct,
        }
    }
}

fn price_at(series: &[(DateTime<Utc>, i64)], anchor: DateTime<Utc>, age: Duration) -> Option<f64> {
    let cutoff = anchor.checked_sub_signed(age)?;
    let start = series.partition_point(|(at, _)| *at <= cutoff);
    let end = series.partition_point(|(at, _)| *at <= anchor);
    index_median(
        series[start..end]
            .iter()
            .map(|(_, price)| *price as f64)
            .collect(),
        1,
    )
}

fn index_median(mut values: Vec<f64>, min: usize) -> Option<f64> {
    if values.len() < min {
        return None;
    }
    let middle = values.len() / 2;
    let even = values.len().is_multiple_of(2);
    let (lower, upper, _) = values.select_nth_unstable_by(middle, f64::total_cmp);
    Some(if even {
        (lower.iter().copied().fold(f64::NEG_INFINITY, f64::max) + *upper) / 2.0
    } else {
        *upper
    })
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use cs2_arb_core::{Money, Platform, WearTier};

    use super::*;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 8, 12, 0, 0).unwrap()
    }

    fn item(name: &str) -> ItemId {
        ItemId {
            market_hash_name: name.into(),
            wear_tier: WearTier::NotApplicable,
        }
    }

    fn series(name: &str, old: i64, yesterday: i64, current: i64) -> [PriceSnapshot; 3] {
        [(7, old), (1, yesterday), (0, current)].map(|(days, price)| PriceSnapshot {
            item: item(name),
            platform: Platform::CsFloat,
            float_bucket: None,
            price: Money(price),
            observed_at: now() - Duration::days(days),
        })
    }

    #[test]
    fn normalizes_each_item_and_blocks_on_either_inclusive_market_threshold() {
        for (older, previous, current, short, long) in
            [(100, 100, 97, -3.0, -3.0), (100, 92, 92, 0.0, -8.0)]
        {
            let mut prices = Vec::new();
            for (name, scale) in [("Cheap", 1), ("Medium", 10), ("Expensive", 1000)] {
                prices.extend(series(
                    name,
                    older * scale,
                    previous * scale,
                    current * scale,
                ));
            }
            let regime = MarketRegime::new(RegimeConfig::default(), &prices, &[]).unwrap();
            let regime = regime.at(now());
            assert_eq!(regime.state.status, RegimeStatus::MarketDowntrend);
            assert_eq!(regime.state.market.short_pct, Some(short));
            assert_eq!(regime.state.market.long_pct, Some(long));
            assert!(!regime.allows(&item("Cheap")));
        }
    }

    #[test]
    fn case_decline_can_stop_a_flat_broad_market() {
        let mut prices = Vec::new();
        for name in ["One", "Two", "Three"] {
            prices.extend(series(name, 100, 100, 100));
        }
        prices.extend(series("First Case", 100, 100, 90));
        let regime = MarketRegime::new(RegimeConfig::default(), &prices, &[]).unwrap();
        assert_eq!(regime.at(now()).state.status, RegimeStatus::RiskOn);
        prices.extend(series("Second Case", 100, 100, 92));
        let regime = MarketRegime::new(RegimeConfig::default(), &prices, &[]).unwrap();
        let state = regime.at(now()).state;
        assert_eq!(state.market.short_pct, Some(0.0));
        assert_eq!(state.cases.short_pct, Some(-9.0));
        assert_eq!(state.status, RegimeStatus::CaseDowntrend);
    }

    #[test]
    fn blocks_underperformers_and_missing_item_history_with_market_risk_on() {
        let prices = [
            series("Weak", 100, 100, 90),
            series("Flat", 100, 100, 100),
            series("Positive", 100, 100, 103),
        ]
        .concat();
        let regime = MarketRegime::new(RegimeConfig::default(), &prices, &[]).unwrap();
        let regime = regime.at(now());
        assert_eq!(regime.state.status, RegimeStatus::RiskOn);
        assert!(!regime.allows(&item("Weak")));
        assert!(regime.allows(&item("Flat")));
        assert!(regime.allows(&item("Positive")));
        assert!(!regime.allows(&item("Missing")));
    }

    #[test]
    fn item_gate_compares_positive_returns_and_allows_exactly_the_configured_gap() {
        let mut prices = Vec::new();
        for name in ["One", "Two", "Three", "Four"] {
            prices.extend(series(name, 100, 100, 110));
        }
        prices.extend(series("Below margin", 100, 100, 104));
        prices.extend(series("At margin", 100, 100, 105));
        prices.push(series("No prior price", 100, 100, 110)[2].clone());

        let default = MarketRegime::new(RegimeConfig::default(), &prices, &[]).unwrap();
        let regime = default.at(now());
        assert_eq!(regime.state.status, RegimeStatus::RiskOn);
        assert_eq!(regime.state.market.short_pct, Some(10.0));
        assert!(!regime.allows(&item("Below margin"))); // +4% is six points behind +10%.
        assert!(regime.allows(&item("At margin")));
        assert!(!regime.allows(&item("No prior price")));

        let wider = MarketRegime::new(
            RegimeConfig {
                item_underperformance_pct: 6.0,
                ..RegimeConfig::default()
            },
            &prices,
            &[],
        )
        .unwrap();
        assert!(wider.at(now()).allows(&item("Below margin")));
    }

    #[test]
    fn distinct_sale_ids_keep_their_weight_at_identical_prices_and_times() {
        let sale = Sale {
            id: "yesterday".into(),
            platform: Platform::CsFloat,
            item: item("One"),
            price: Money(100),
            float_value: None,
            sold_at: now() - Duration::days(1),
            verified: true,
        };
        let current = Sale {
            id: "current-high".into(),
            price: Money(110),
            sold_at: now(),
            ..sale.clone()
        };
        let sales = [
            sale,
            current.clone(),
            current.clone(),
            Sale {
                id: "current-low-1".into(),
                price: Money(90),
                ..current.clone()
            },
            Sale {
                id: "current-low-2".into(),
                price: Money(90),
                ..current
            },
        ];
        let regime = MarketRegime::new(
            RegimeConfig {
                min_index_items: 1,
                ..RegimeConfig::default()
            },
            &[],
            &sales,
        )
        .unwrap();
        assert_eq!(regime.at(now()).state.market.short_pct, Some(-10.0));
    }

    #[test]
    fn a_shift_in_sale_float_mix_does_not_move_the_index() {
        let mut sales = Vec::new();
        for (days, low_count, high_count) in [(7, 3, 1), (1, 3, 1), (0, 1, 3)] {
            for (float_value, price, count) in [(0.18, 200, low_count), (0.30, 100, high_count)] {
                for i in 0..count {
                    sales.push(Sale {
                        id: format!("{days}-{float_value}-{i}"),
                        platform: Platform::CsFloat,
                        item: ItemId {
                            wear_tier: WearTier::FieldTested,
                            ..item("One")
                        },
                        price: Money(price),
                        float_value: Some(float_value),
                        sold_at: now() - Duration::days(days),
                        verified: true,
                    });
                }
            }
        }
        let regime = MarketRegime::new(
            RegimeConfig {
                min_index_items: 1,
                ..RegimeConfig::default()
            },
            &[],
            &sales,
        )
        .unwrap();
        let state = regime.at(now()).state;
        assert_eq!(state.market.short_pct, Some(0.0));
        assert_eq!(state.market.long_pct, Some(0.0));
        assert_eq!(state.status, RegimeStatus::RiskOn);
    }

    #[test]
    fn stale_future_and_unverified_observations_do_not_complete_the_index() {
        let mut prices = [series("One", 100, 100, 100), series("Two", 100, 100, 100)].concat();
        let [mut stale, yesterday, mut future] = series("Three", 100, 100, 100);
        stale.observed_at -= Duration::days(1);
        future.observed_at += Duration::seconds(1);
        prices.extend([stale, yesterday, future.clone()]);
        let sale = Sale {
            id: "third-now".into(),
            platform: future.platform,
            item: future.item,
            price: future.price,
            float_value: None,
            sold_at: now(),
            verified: false,
        };
        let historical = Sale {
            id: "third-yesterday".into(),
            sold_at: now() - Duration::days(1),
            verified: true,
            ..sale.clone()
        };
        let regime = MarketRegime::new(
            RegimeConfig::default(),
            &prices,
            &[historical.clone(), sale.clone()],
        )
        .unwrap();
        assert_eq!(
            regime.at(now()).state.status,
            RegimeStatus::InsufficientHistory
        );
        let verified = Sale {
            verified: true,
            ..sale
        };
        let regime =
            MarketRegime::new(RegimeConfig::default(), &prices, &[historical, verified]).unwrap();
        let state = regime.at(now()).state;
        assert_eq!(state.market.short_pct, Some(0.0));
        assert_eq!(state.market.long_pct, None);
        assert_eq!(state.status, RegimeStatus::InsufficientHistory);
    }
}
