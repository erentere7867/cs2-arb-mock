//! Event-driven dip strategy (mock only).
//!
//! Per price series (platform + item + float bucket by default, see
//! [`DipConfig::per_platform`]) keep a rolling 30-day window of observed prices. A buy is
//! signalled when **all** of:
//!
//! 1. `price < mean - k_sigma * std_dev` of the *prior* window (the triggering observation
//!    is excluded from its own baseline), with at least `min_samples` prior observations;
//! 2. the item is liquid: at least `min_sales_in_window` sales of the item (any platform)
//!    in the trailing `liquidity_window_days` (7 by default);
//! 3. an update/panic window derived from [`MarketEvent`]s is active at the observation time.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use cs2_arb_core::{
    Error, FloatBucket, ItemId, MarketEvent, Money, Order, OrderSide, Platform, PriceSnapshot,
    Result, Sale,
};
use serde::Deserialize;

use crate::{order_id, platform_slug};

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DipConfig {
    /// Rolling baseline length in days. Default 30.
    pub lookback_days: i64,
    /// Standard deviations below the mean required to trigger. Default 2.0.
    pub k_sigma: f64,
    /// Minimum prior observations in the baseline window (>= 2). Default 10.
    pub min_samples: usize,
    /// Liquidity window in days. Default 7.
    pub liquidity_window_days: i64,
    /// Minimum sales in the liquidity window. Default 10.
    pub min_sales_in_window: usize,
    /// Count only sales flagged `verified`. Default false.
    pub require_verified_sales: bool,
    /// Units per signal. Default 1.
    pub quantity: u32,
    /// If set, a `BranchUpdated` event opens an update window of this many hours. `None`
    /// (default) means only explicit `PanicWindowOpened`/`Closed` events define windows.
    pub branch_update_window_hours: Option<i64>,
    /// If set, only events on these branches open windows. Default: all branches.
    pub branches: Option<Vec<String>>,
    /// Keep a separate baseline per platform (default true). When false, quotes for the
    /// same item and bucket on all platforms share one series.
    pub per_platform: bool,
}

impl Default for DipConfig {
    fn default() -> Self {
        Self {
            lookback_days: 30,
            k_sigma: 2.0,
            min_samples: 10,
            liquidity_window_days: 7,
            min_sales_in_window: 10,
            require_verified_sales: false,
            quantity: 1,
            branch_update_window_hours: None,
            branches: None,
            per_platform: true,
        }
    }
}

impl DipConfig {
    pub fn validate(&self) -> Result<()> {
        let bad = |msg: &str| Err(Error::InvalidConfig(format!("dip: {msg}")));
        if self.lookback_days <= 0 || self.liquidity_window_days <= 0 {
            return bad("lookback_days and liquidity_window_days must be > 0");
        }
        if !self.k_sigma.is_finite() || self.k_sigma < 0.0 {
            return bad("k_sigma must be finite and >= 0");
        }
        if self.min_samples < 2 {
            return bad("min_samples must be >= 2");
        }
        if self.quantity == 0 {
            return bad("quantity must be >= 1");
        }
        if matches!(self.branch_update_window_hours, Some(h) if h <= 0) {
            return bad("branch_update_window_hours must be > 0");
        }
        Ok(())
    }
}

/// Sample statistics (n - 1 denominator) of a price window, in cents.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RollingStats {
    pub count: usize,
    pub mean: f64,
    pub std_dev: f64,
}

impl RollingStats {
    /// `mean - k * std_dev`.
    pub fn lower_band(&self, k: f64) -> f64 {
        self.mean - k * self.std_dev
    }

    /// Standard score of `price`; `None` when the window has zero variance.
    pub fn z_score(&self, price: f64) -> Option<f64> {
        (self.std_dev > 0.0).then(|| (price - self.mean) / self.std_dev)
    }
}

/// Mean and sample standard deviation; `None` for fewer than two values.
pub fn rolling_stats(values: &[f64]) -> Option<RollingStats> {
    let n = values.len();
    if n < 2 {
        return None;
    }
    let mean = values.iter().sum::<f64>() / n as f64;
    let var = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1) as f64;
    Some(RollingStats {
        count: n,
        mean,
        std_dev: var.max(0.0).sqrt(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Window {
    branch: String,
    start: DateTime<Utc>,
    end: Option<DateTime<Utc>>,
}

/// Update/panic windows built from [`MarketEvent`]s. Windows are stored as time
/// intervals, so they can be queried at any historical time during a replay.
#[derive(Debug, Clone, Default)]
pub struct EventWindows {
    windows: Vec<Window>,
    branch_update_window: Option<Duration>,
    branches: Option<Vec<String>>,
}

impl EventWindows {
    pub fn new(config: &DipConfig) -> Self {
        Self {
            windows: Vec::new(),
            branch_update_window: config.branch_update_window_hours.map(Duration::hours),
            branches: config.branches.clone(),
        }
    }

    fn branch_allowed(&self, branch: &str) -> bool {
        self.branches
            .as_ref()
            .is_none_or(|allowed| allowed.iter().any(|b| b == branch))
    }

    pub fn apply(&mut self, event: &MarketEvent) {
        match event {
            MarketEvent::PanicWindowOpened { branch, opened_at } => {
                if !self.branch_allowed(branch) {
                    return;
                }
                let already_open = self
                    .windows
                    .iter()
                    .any(|w| &w.branch == branch && w.end.is_none() && w.start <= *opened_at);
                if !already_open {
                    self.windows.push(Window {
                        branch: branch.clone(),
                        start: *opened_at,
                        end: None,
                    });
                }
            }
            MarketEvent::PanicWindowClosed { branch, closed_at } => {
                if let Some(window) = self
                    .windows
                    .iter_mut()
                    .filter(|w| &w.branch == branch && w.end.is_none() && w.start <= *closed_at)
                    .max_by_key(|w| w.start)
                {
                    window.end = Some(*closed_at);
                }
            }
            MarketEvent::BranchUpdated {
                branch,
                observed_at,
                ..
            } => {
                if let Some(len) = self.branch_update_window {
                    if self.branch_allowed(branch) {
                        self.windows.push(Window {
                            branch: branch.clone(),
                            start: *observed_at,
                            end: Some(*observed_at + len),
                        });
                    }
                }
            }
        }
    }

    /// Whether any window covers `at` (start inclusive, end exclusive).
    pub fn is_active(&self, at: DateTime<Utc>) -> bool {
        self.windows
            .iter()
            .any(|w| w.start <= at && w.end.is_none_or(|end| at < end))
    }

    /// Drops windows that ended before `before`.
    pub fn prune(&mut self, before: DateTime<Utc>) {
        self.windows
            .retain(|w| w.end.is_none_or(|end| end >= before));
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SeriesKey {
    platform: Option<Platform>,
    item: ItemId,
    float_bucket: Option<FloatBucket>,
}

#[derive(Debug, Clone)]
struct SaleRecord {
    at: DateTime<Utc>,
    platform: Platform,
    id: String,
}

/// Diagnostic result of evaluating one snapshot against the dip rule.
#[derive(Debug, Clone, PartialEq)]
pub struct DipEvaluation {
    /// Baseline from prior observations in the lookback window; `None` if fewer than two.
    pub stats: Option<RollingStats>,
    /// `mean - k_sigma * std_dev` in cents, when stats exist.
    pub threshold: Option<f64>,
    pub enough_samples: bool,
    pub below_band: bool,
    pub sales_in_window: usize,
    pub liquid: bool,
    pub window_active: bool,
}

impl DipEvaluation {
    pub fn triggered(&self) -> bool {
        self.enough_samples && self.below_band && self.liquid && self.window_active
    }
}

/// A dip buy signal with the mock order to submit.
#[derive(Debug, Clone, PartialEq)]
pub struct DipSignal {
    pub snapshot: PriceSnapshot,
    pub stats: RollingStats,
    pub threshold: f64,
    pub z_score: Option<f64>,
    pub sales_in_window: usize,
    pub order: Order,
}

/// Stateful dip detector. Feed it snapshots in (roughly) chronological order via
/// [`DipDetector::observe`], sales via [`DipDetector::record_sale`] and events via
/// [`DipDetector::apply_event`].
#[derive(Debug, Clone)]
pub struct DipDetector {
    config: DipConfig,
    prices: HashMap<SeriesKey, Vec<(DateTime<Utc>, f64)>>,
    sales: HashMap<ItemId, Vec<SaleRecord>>,
    windows: EventWindows,
}

impl DipDetector {
    pub fn new(config: DipConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            windows: EventWindows::new(&config),
            config,
            prices: HashMap::new(),
            sales: HashMap::new(),
        })
    }

    pub fn config(&self) -> &DipConfig {
        &self.config
    }

    pub fn windows(&self) -> &EventWindows {
        &self.windows
    }

    pub fn apply_event(&mut self, event: &MarketEvent) {
        self.windows.apply(event);
    }

    /// Records a sale for liquidity. Duplicate (platform, id) pairs are ignored.
    pub fn record_sale(&mut self, sale: &Sale) {
        if self.config.require_verified_sales && !sale.verified {
            return;
        }
        let records = self.sales.entry(sale.item.clone()).or_default();
        if records
            .iter()
            .any(|r| r.platform == sale.platform && r.id == sale.id)
        {
            return;
        }
        let pos = records.partition_point(|r| r.at <= sale.sold_at);
        records.insert(
            pos,
            SaleRecord {
                at: sale.sold_at,
                platform: sale.platform,
                id: sale.id.clone(),
            },
        );
    }

    /// Sales of `item` in `(at - liquidity_window, at]`.
    pub fn sales_in_window(&self, item: &ItemId, at: DateTime<Utc>) -> usize {
        let from = at - Duration::days(self.config.liquidity_window_days);
        self.sales.get(item).map_or(0, |records| {
            let lo = records.partition_point(|r| r.at <= from);
            let hi = records.partition_point(|r| r.at <= at);
            hi.saturating_sub(lo)
        })
    }

    fn key(&self, snapshot: &PriceSnapshot) -> SeriesKey {
        SeriesKey {
            platform: self.config.per_platform.then_some(snapshot.platform),
            item: snapshot.item.clone(),
            float_bucket: snapshot.float_bucket,
        }
    }

    /// Evaluates `snapshot` without recording it.
    pub fn evaluate(&self, snapshot: &PriceSnapshot) -> Result<DipEvaluation> {
        let sales = self.sales_in_window(&snapshot.item, snapshot.observed_at);
        self.evaluate_with_liquidity(snapshot, sales)
    }

    /// Like [`Self::evaluate`] but with an externally supplied trailing sales count (e.g. a
    /// marketplace "sales last 7 days" aggregate).
    pub fn evaluate_with_liquidity(
        &self,
        snapshot: &PriceSnapshot,
        sales_in_window: usize,
    ) -> Result<DipEvaluation> {
        if snapshot.price.0 <= 0 {
            return Err(Error::InvalidData(
                "dip snapshot price must be positive".into(),
            ));
        }
        let at = snapshot.observed_at;
        let from = at - Duration::days(self.config.lookback_days);
        let prior: Vec<f64> = self
            .prices
            .get(&self.key(snapshot))
            .map(|series| {
                series
                    .iter()
                    .filter(|(t, _)| *t >= from && *t < at)
                    .map(|(_, p)| *p)
                    .collect()
            })
            .unwrap_or_default();
        let stats = rolling_stats(&prior);
        let threshold = stats.map(|s| s.lower_band(self.config.k_sigma));
        let price = snapshot.price.0 as f64;
        Ok(DipEvaluation {
            stats,
            threshold,
            enough_samples: prior.len() >= self.config.min_samples,
            below_band: threshold.is_some_and(|t| price < t),
            sales_in_window,
            liquid: sales_in_window >= self.config.min_sales_in_window,
            window_active: self.windows.is_active(at),
        })
    }

    /// Evaluates `snapshot`, then adds it to its series. Returns a signal if triggered.
    pub fn observe(&mut self, snapshot: &PriceSnapshot) -> Result<Option<DipSignal>> {
        let sales = self.sales_in_window(&snapshot.item, snapshot.observed_at);
        self.observe_with_liquidity(snapshot, sales)
    }

    /// [`Self::observe`] with an externally supplied trailing sales count.
    pub fn observe_with_liquidity(
        &mut self,
        snapshot: &PriceSnapshot,
        sales_in_window: usize,
    ) -> Result<Option<DipSignal>> {
        let eval = self.evaluate_with_liquidity(snapshot, sales_in_window)?;
        self.insert(snapshot);
        self.prune(snapshot.observed_at);
        if !eval.triggered() {
            return Ok(None);
        }
        let (Some(stats), Some(threshold)) = (eval.stats, eval.threshold) else {
            return Ok(None);
        };
        Ok(Some(DipSignal {
            z_score: stats.z_score(snapshot.price.0 as f64),
            order: self.buy_order(snapshot),
            snapshot: snapshot.clone(),
            stats,
            threshold,
            sales_in_window: eval.sales_in_window,
        }))
    }

    fn insert(&mut self, snapshot: &PriceSnapshot) {
        let series = self.prices.entry(self.key(snapshot)).or_default();
        let pos = series.partition_point(|(t, _)| *t <= snapshot.observed_at);
        series.insert(pos, (snapshot.observed_at, snapshot.price.0 as f64));
    }

    fn prune(&mut self, now: DateTime<Utc>) {
        let price_from = now - Duration::days(self.config.lookback_days);
        for series in self.prices.values_mut() {
            let cut = series.partition_point(|(t, _)| *t < price_from);
            series.drain(..cut);
        }
        let sales_from = now - Duration::days(self.config.liquidity_window_days);
        for records in self.sales.values_mut() {
            let cut = records.partition_point(|r| r.at <= sales_from);
            records.drain(..cut);
        }
    }

    fn buy_order(&self, snapshot: &PriceSnapshot) -> Order {
        Order {
            id: order_id(
                "dip-buy",
                &[
                    platform_slug(snapshot.platform),
                    &snapshot.item.market_hash_name,
                ],
                snapshot.observed_at,
            ),
            side: OrderSide::Buy,
            platform: snapshot.platform,
            item: snapshot.item.clone(),
            float_bucket: snapshot.float_bucket,
            quantity: self.config.quantity,
            limit_price: Money(snapshot.price.0),
            submitted_at: snapshot.observed_at,
        }
    }
}

/// Batch helper for replays: applies all `events` and `sales`, then observes `quotes` in
/// chronological order. Invalid snapshots are skipped.
pub fn evaluate_dip(
    quotes: &[PriceSnapshot],
    events: &[MarketEvent],
    sales: &[Sale],
    config: &DipConfig,
) -> Result<Vec<DipSignal>> {
    let mut detector = DipDetector::new(config.clone())?;
    for event in events {
        detector.apply_event(event);
    }
    for sale in sales {
        detector.record_sale(sale);
    }
    let mut ordered: Vec<&PriceSnapshot> = quotes.iter().collect();
    ordered.sort_by_key(|q| q.observed_at);
    let mut signals = Vec::new();
    for quote in ordered {
        match detector.observe(quote) {
            Ok(Some(signal)) => signals.push(signal),
            Ok(None) => {}
            Err(err) => tracing::debug!(error = %err, "skipping dip snapshot"),
        }
    }
    Ok(signals)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use cs2_arb_core::WearTier;

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

    fn snap(day: i64, price: i64) -> PriceSnapshot {
        PriceSnapshot {
            platform: Platform::DMarket,
            item: item(),
            float_bucket: Some(FloatBucket::FieldTestedLow),
            price: Money(price),
            observed_at: t0() + Duration::days(day),
        }
    }

    fn sale(n: usize, at: DateTime<Utc>) -> Sale {
        Sale {
            id: format!("s{n}"),
            platform: Platform::Skinport,
            item: item(),
            price: Money(3000),
            float_value: Some(0.2),
            sold_at: at,
            verified: n.is_multiple_of(2),
        }
    }

    fn panic_window(open_day: i64, close_day: i64) -> [MarketEvent; 2] {
        [
            MarketEvent::PanicWindowOpened {
                branch: "public".into(),
                opened_at: t0() + Duration::days(open_day),
            },
            MarketEvent::PanicWindowClosed {
                branch: "public".into(),
                closed_at: t0() + Duration::days(close_day),
            },
        ]
    }

    /// 30 days alternating 2900/3100 (mean 3000, sample sd ~101.7), then a dip on day 30.
    fn baseline() -> Vec<PriceSnapshot> {
        (0..30)
            .map(|d| snap(d, if d % 2 == 0 { 2900 } else { 3100 }))
            .collect()
    }

    fn liquid_sales(at: DateTime<Utc>, n: usize) -> Vec<Sale> {
        (0..n)
            .map(|i| sale(i, at - Duration::hours(i as i64 + 1)))
            .collect()
    }

    #[test]
    fn rolling_stats_matches_hand_computation() {
        let s = rolling_stats(&[2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0]).unwrap();
        assert_eq!(s.count, 8);
        assert!((s.mean - 5.0).abs() < 1e-12);
        // Sample variance = 32 / 7.
        assert!((s.std_dev - (32.0f64 / 7.0).sqrt()).abs() < 1e-12);
        assert!((s.lower_band(2.0) - (5.0 - 2.0 * s.std_dev)).abs() < 1e-12);
        assert!(rolling_stats(&[1.0]).is_none());
        assert_eq!(rolling_stats(&[3.0, 3.0]).unwrap().z_score(1.0), None);
    }

    #[test]
    fn triggers_only_below_two_sigma_with_liquidity_and_window() {
        let dip_day = t0() + Duration::days(30);
        let events = panic_window(29, 31);
        let sales = liquid_sales(dip_day, 10);
        let stats = rolling_stats(
            &baseline()
                .iter()
                .map(|s| s.price.0 as f64)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let band = stats.lower_band(2.0); // ~2796.6
        assert!(band > 2796.0 && band < 2797.0, "band {band}");

        // Just above the band: no trigger.
        let mut quotes = baseline();
        quotes.push(snap(30, 2797));
        assert!(
            evaluate_dip(&quotes, &events, &sales, &DipConfig::default())
                .unwrap()
                .is_empty()
        );

        // Just below the band: trigger.
        let mut quotes = baseline();
        quotes.push(snap(30, 2796));
        let signals = evaluate_dip(&quotes, &events, &sales, &DipConfig::default()).unwrap();
        assert_eq!(signals.len(), 1);
        let sig = &signals[0];
        assert_eq!(sig.stats.count, 30);
        assert!((sig.stats.mean - 3000.0).abs() < 1e-9);
        assert!((sig.threshold - band).abs() < 1e-9);
        assert!(sig.z_score.unwrap() < -2.0);
        assert_eq!(sig.sales_in_window, 10);
        assert_eq!(sig.order.side, OrderSide::Buy);
        assert_eq!(sig.order.platform, Platform::DMarket);
        assert_eq!(sig.order.limit_price, Money(2796));
        assert_eq!(sig.order.submitted_at, dip_day);

        // Illiquid (9 sales): no trigger.
        let few = liquid_sales(dip_day, 9);
        assert!(evaluate_dip(&quotes, &events, &few, &DipConfig::default())
            .unwrap()
            .is_empty());

        // No active window: no trigger (window closed before the dip).
        let closed = panic_window(10, 20);
        assert!(
            evaluate_dip(&quotes, &closed, &sales, &DipConfig::default())
                .unwrap()
                .is_empty()
        );
        assert!(evaluate_dip(&quotes, &[], &sales, &DipConfig::default())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn liquidity_counts_trailing_seven_days_and_verified_filter() {
        let mut det = DipDetector::new(DipConfig::default()).unwrap();
        let at = t0() + Duration::days(30);
        for i in 0..10 {
            det.record_sale(&sale(i, at - Duration::days(1)));
        }
        det.record_sale(&sale(0, at - Duration::days(1))); // duplicate id ignored
        det.record_sale(&sale(100, at - Duration::days(8))); // outside window
        det.record_sale(&sale(101, at + Duration::hours(1))); // future
        assert_eq!(det.sales_in_window(&item(), at), 10);
        assert_eq!(det.sales_in_window(&item(), at - Duration::days(7)), 1);

        let mut det = DipDetector::new(DipConfig {
            require_verified_sales: true,
            ..DipConfig::default()
        })
        .unwrap();
        for i in 0..10 {
            det.record_sale(&sale(i, at - Duration::days(1)));
        }
        assert_eq!(det.sales_in_window(&item(), at), 5);
    }

    #[test]
    fn baseline_excludes_old_and_current_observations() {
        let mut det = DipDetector::new(DipConfig {
            min_sales_in_window: 0,
            ..DipConfig::default()
        })
        .unwrap();
        // Very old high prices outside the 30-day window must not inflate the mean.
        for d in 0..5 {
            det.observe(&snap(d, 10_000)).unwrap();
        }
        for d in 40..70 {
            det.observe(&snap(d, if d % 2 == 0 { 2900 } else { 3100 }))
                .unwrap();
        }
        let eval = det.evaluate(&snap(70, 2000)).unwrap();
        let stats = eval.stats.unwrap();
        assert_eq!(stats.count, 30);
        assert!((stats.mean - 3000.0).abs() < 1e-9);
        assert!(eval.below_band && eval.liquid && eval.enough_samples);
        assert!(!eval.window_active);
        assert!(!eval.triggered());
    }

    #[test]
    fn requires_min_samples() {
        let mut det = DipDetector::new(DipConfig {
            min_sales_in_window: 0,
            ..DipConfig::default()
        })
        .unwrap();
        for e in panic_window(0, 100) {
            det.apply_event(&e);
        }
        for d in 0..9 {
            det.observe(&snap(d, if d % 2 == 0 { 2900 } else { 3100 }))
                .unwrap();
        }
        assert!(det.observe(&snap(9, 1000)).unwrap().is_none());
        // Day 9's 1000 is now part of the baseline; a later deeper dip still needs 2 sigma.
        let eval = det.evaluate(&snap(10, 1)).unwrap();
        assert!(eval.enough_samples);
    }

    #[test]
    fn event_windows_from_panic_and_branch_updates() {
        let mut w = EventWindows::new(&DipConfig::default());
        let [open, close] = panic_window(1, 3);
        w.apply(&open);
        assert!(
            w.is_active(t0() + Duration::days(10)),
            "open-ended until closed"
        );
        w.apply(&close);
        assert!(!w.is_active(t0()));
        assert!(w.is_active(t0() + Duration::days(1)));
        assert!(w.is_active(t0() + Duration::days(2)));
        assert!(!w.is_active(t0() + Duration::days(3)));

        let update = MarketEvent::BranchUpdated {
            app_id: cs2_arb_core::CS2_APP_ID,
            branch: "public".into(),
            change_number: 1,
            observed_at: t0() + Duration::days(20),
        };
        w.apply(&update);
        assert!(
            !w.is_active(t0() + Duration::days(20)),
            "disabled by default"
        );

        let mut w = EventWindows::new(&DipConfig {
            branch_update_window_hours: Some(24),
            branches: Some(vec!["public".into()]),
            ..DipConfig::default()
        });
        w.apply(&update);
        assert!(w.is_active(t0() + Duration::days(20) + Duration::hours(23)));
        assert!(!w.is_active(t0() + Duration::days(21)));
        w.apply(&MarketEvent::PanicWindowOpened {
            branch: "staging".into(),
            opened_at: t0(),
        });
        assert!(!w.is_active(t0() + Duration::days(1)), "branch not allowed");
        w.prune(t0() + Duration::days(30));
        assert!(!w.is_active(t0() + Duration::days(20)));
    }

    #[test]
    fn fixture_dip_inside_panic_window_triggers() {
        let quotes: Vec<PriceSnapshot> =
            serde_json::from_str(include_str!("../../../data/fixtures/price_snapshots.json"))
                .unwrap();
        let events: Vec<MarketEvent> =
            serde_json::from_str(include_str!("../../../data/fixtures/market_events.json"))
                .unwrap();
        // The fixture has no sales; treat the item as liquid via min_sales_in_window = 0.
        let config = DipConfig {
            min_sales_in_window: 0,
            ..DipConfig::default()
        };
        let signals = evaluate_dip(&quotes, &events, &[], &config).unwrap();
        let (
            MarketEvent::PanicWindowOpened { opened_at, .. },
            MarketEvent::PanicWindowClosed { closed_at, .. },
        ) = (&events[1], &events[2])
        else {
            panic!("fixture layout");
        };
        assert!(signals
            .iter()
            .any(|s| s.snapshot.platform == Platform::DMarket
                && s.snapshot.item.market_hash_name == "AK-47 | Redline (Field-Tested)"));
        assert!(signals
            .iter()
            .all(|s| (*opened_at..*closed_at).contains(&s.snapshot.observed_at)));
        // With the liquidity gate at its default and no sales, nothing triggers.
        assert!(evaluate_dip(&quotes, &events, &[], &DipConfig::default())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn config_validation() {
        assert!(DipDetector::new(DipConfig {
            min_samples: 1,
            ..DipConfig::default()
        })
        .is_err());
        let c: DipConfig =
            toml::from_str("k_sigma = 2.5\nbranch_update_window_hours = 48").unwrap();
        assert_eq!(c.k_sigma, 2.5);
        assert_eq!(c.lookback_days, 30);
        assert_eq!(c.branch_update_window_hours, Some(48));
    }
}
