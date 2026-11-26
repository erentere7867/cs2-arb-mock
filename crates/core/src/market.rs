use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{FeeSchedule, Money, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Skinport,
    CsFloat,
    DMarket,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ItemId {
    /// Canonical Steam market hash name, including variant and exterior.
    pub market_hash_name: String,
    pub wear_tier: WearTier,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WearTier {
    FactoryNew,
    MinimalWear,
    FieldTested,
    WellWorn,
    BattleScarred,
}

impl WearTier {
    /// Canonical wear intervals are lower-inclusive and upper-exclusive, except 1.0 is valid.
    pub const fn float_bounds(self) -> (f64, f64) {
        match self {
            Self::FactoryNew => (0.0, 0.07),
            Self::MinimalWear => (0.07, 0.15),
            Self::FieldTested => (0.15, 0.38),
            Self::WellWorn => (0.38, 0.45),
            Self::BattleScarred => (0.45, 1.0),
        }
    }

    pub fn from_float(value: f64) -> Option<Self> {
        FloatBucket::from_float(value).map(FloatBucket::wear_tier)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FloatBucket {
    FactoryNewLow,
    FactoryNewHigh,
    MinimalWear,
    FieldTestedLow,
    FieldTestedHigh,
    WellWorn,
    BattleScarredLow,
    BattleScarredHigh,
}

impl FloatBucket {
    pub const fn wear_tier(self) -> WearTier {
        match self {
            Self::FactoryNewLow | Self::FactoryNewHigh => WearTier::FactoryNew,
            Self::MinimalWear => WearTier::MinimalWear,
            Self::FieldTestedLow | Self::FieldTestedHigh => WearTier::FieldTested,
            Self::WellWorn => WearTier::WellWorn,
            Self::BattleScarredLow | Self::BattleScarredHigh => WearTier::BattleScarred,
        }
    }

    pub const fn float_bounds(self) -> (f64, f64) {
        match self {
            Self::FactoryNewLow => (0.0, 0.03),
            Self::FactoryNewHigh => (0.03, 0.07),
            Self::FieldTestedLow => (0.15, 0.25),
            Self::FieldTestedHigh => (0.25, 0.38),
            Self::BattleScarredLow => (0.45, 0.65),
            Self::BattleScarredHigh => (0.65, 1.0),
            _ => self.wear_tier().float_bounds(),
        }
    }

    pub fn from_float(value: f64) -> Option<Self> {
        if !(0.0..=1.0).contains(&value) {
            return None;
        }
        [
            Self::FactoryNewLow,
            Self::FactoryNewHigh,
            Self::MinimalWear,
            Self::FieldTestedLow,
            Self::FieldTestedHigh,
            Self::WellWorn,
            Self::BattleScarredLow,
            Self::BattleScarredHigh,
        ]
        .into_iter()
        .find(|bucket| {
            let (lower, upper) = bucket.float_bounds();
            value >= lower && (value < upper || upper == 1.0)
        })
    }
}

/// Only equal tiers and equal buckets are comparable. Two tier-only quotes may be
/// compared at tier granularity; a bucket-qualified quote cannot use a tier-only price.
pub fn comparable_wear(
    lhs_tier: WearTier,
    lhs_bucket: Option<FloatBucket>,
    rhs_tier: WearTier,
    rhs_bucket: Option<FloatBucket>,
) -> bool {
    if lhs_tier != rhs_tier {
        return false;
    }
    match (lhs_bucket, rhs_bucket) {
        (Some(lhs), Some(rhs)) => lhs == rhs && lhs.wear_tier() == lhs_tier,
        (None, None) => true,
        _ => false,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Listing {
    pub id: String,
    pub platform: Platform,
    pub item: ItemId,
    pub price: Money,
    pub quantity: u32,
    pub float_value: Option<f64>,
    pub observed_at: DateTime<Utc>,
    pub trade_locked_until: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sale {
    pub id: String,
    pub platform: Platform,
    pub item: ItemId,
    pub price: Money,
    pub float_value: Option<f64>,
    pub sold_at: DateTime<Utc>,
    /// Whether this individual sale is backed by a verified marketplace record.
    pub verified: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriceSnapshot {
    pub platform: Platform,
    pub item: ItemId,
    pub float_bucket: Option<FloatBucket>,
    pub price: Money,
    pub observed_at: DateTime<Utc>,
}

/// Read-only API. There are intentionally no methods to place or execute real orders.
#[async_trait]
pub trait MarketClient: Send + Sync {
    async fn list_listings(&self, item: &ItemId) -> Result<Vec<Listing>>;
    async fn recent_sales(&self, item: &ItemId, since: DateTime<Utc>) -> Result<Vec<Sale>>;
    async fn price_snapshot(&self, item: &ItemId) -> Result<PriceSnapshot>;
    async fn fees(&self) -> Result<FeeSchedule>;
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::{MarketEvent, CS2_APP_ID};

    #[test]
    fn validates_float_boundaries() {
        assert_eq!(WearTier::from_float(0.0), Some(WearTier::FactoryNew));
        assert_eq!(WearTier::from_float(0.07), Some(WearTier::MinimalWear));
        assert_eq!(WearTier::from_float(0.15), Some(WearTier::FieldTested));
        assert_eq!(WearTier::from_float(0.38), Some(WearTier::WellWorn));
        assert_eq!(WearTier::from_float(0.45), Some(WearTier::BattleScarred));
        assert_eq!(WearTier::from_float(1.0), Some(WearTier::BattleScarred));
        assert_eq!(WearTier::from_float(1.01), None);
        assert_eq!(WearTier::from_float(-0.01), None);
        assert_eq!(WearTier::from_float(f64::NAN), None);
    }

    #[test]
    fn compares_only_matching_wear_and_float_buckets() {
        use FloatBucket::{BattleScarredHigh, BattleScarredLow};
        use WearTier::{BattleScarred, FactoryNew};

        assert!(!comparable_wear(FactoryNew, None, BattleScarred, None));
        assert!(!comparable_wear(
            BattleScarred,
            Some(BattleScarredLow),
            BattleScarred,
            Some(BattleScarredHigh),
        ));
        assert!(comparable_wear(
            BattleScarred,
            Some(BattleScarredHigh),
            BattleScarred,
            Some(BattleScarredHigh),
        ));
        assert!(!comparable_wear(
            FactoryNew,
            Some(BattleScarredHigh),
            FactoryNew,
            Some(BattleScarredHigh),
        ));
    }

    #[test]
    fn fixture_matches_snapshot_and_event_contracts() {
        let quotes: Vec<PriceSnapshot> =
            serde_json::from_str(include_str!("../../../data/fixtures/price_snapshots.json"))
                .unwrap();
        let events: Vec<MarketEvent> =
            serde_json::from_str(include_str!("../../../data/fixtures/market_events.json"))
                .unwrap();
        assert_eq!(quotes.len(), 600);
        assert_eq!(
            quotes
                .iter()
                .map(|q| q.observed_at)
                .collect::<HashSet<_>>()
                .len(),
            50
        );
        assert_eq!(
            quotes
                .iter()
                .map(|q| q.platform)
                .collect::<HashSet<_>>()
                .len(),
            3
        );
        assert_eq!(
            quotes.iter().map(|q| &q.item).collect::<HashSet<_>>().len(),
            4
        );
        assert!(quotes
            .iter()
            .all(|q| q.float_bucket.unwrap().wear_tier() == q.item.wear_tier));

        let [MarketEvent::BranchUpdated { app_id, .. }, MarketEvent::PanicWindowOpened { opened_at, .. }, MarketEvent::PanicWindowClosed { closed_at, .. }] =
            events.as_slice()
        else {
            panic!("expected branch update and associated window");
        };
        assert_eq!(*app_id, CS2_APP_ID);
        assert!(quotes.iter().any(|q| {
            q.platform == Platform::DMarket
                && q.item.market_hash_name == "AK-47 | Redline (Field-Tested)"
                && q.price.0 < 2300
                && (*opened_at..*closed_at).contains(&q.observed_at)
        }));
    }
}
