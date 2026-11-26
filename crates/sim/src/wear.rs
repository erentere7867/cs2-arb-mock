//! Wear conditioning built on core's `comparable_wear`.

use cs2_arb_core::{comparable_wear, FloatBucket, WearTier};

/// A quote's float is not a valid float, or lies outside its item's wear tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("float value is invalid or outside the item's wear tier")]
pub struct InconsistentFloat;

/// Float bucket of a quote whose item has `tier`.
///
/// `Ok(None)`: no float reported. `Err(InconsistentFloat)`: inconsistent data
/// that must never be matched.
pub fn quote_bucket(
    tier: WearTier,
    float_value: Option<f64>,
) -> Result<Option<FloatBucket>, InconsistentFloat> {
    match float_value {
        None => Ok(None),
        Some(value) => match FloatBucket::from_float(value) {
            Some(bucket) if bucket.wear_tier() == tier => Ok(Some(bucket)),
            _ => Err(InconsistentFloat),
        },
    }
}

/// Whether an order (`order_tier`, `order_bucket`) may trade against a quote.
///
/// Strict mode (`tier_only_any == false`) is exactly `comparable_wear`: tiers and
/// buckets must be equal and a tier-only order only matches tier-only quotes.
/// Low/high float buckets inside one tier never match each other in any mode.
pub fn order_matches(
    order_tier: WearTier,
    order_bucket: Option<FloatBucket>,
    quote_tier: WearTier,
    quote_bucket: Option<FloatBucket>,
    tier_only_any: bool,
) -> bool {
    if order_bucket.is_none() && tier_only_any {
        return order_tier == quote_tier
            && quote_bucket.is_none_or(|bucket| bucket.wear_tier() == quote_tier);
    }
    comparable_wear(order_tier, order_bucket, quote_tier, quote_bucket)
}

/// `order_matches` for a quote that reports a raw float.
pub fn order_matches_float(
    order_tier: WearTier,
    order_bucket: Option<FloatBucket>,
    quote_tier: WearTier,
    quote_float: Option<f64>,
    tier_only_any: bool,
) -> bool {
    match quote_bucket(quote_tier, quote_float) {
        Ok(bucket) => order_matches(order_tier, order_bucket, quote_tier, bucket, tier_only_any),
        Err(InconsistentFloat) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use FloatBucket::{FieldTestedHigh, FieldTestedLow};
    use WearTier::{FactoryNew, FieldTested};

    #[test]
    fn derives_buckets_and_flags_inconsistent_floats() {
        assert_eq!(quote_bucket(FieldTested, None), Ok(None));
        assert_eq!(
            quote_bucket(FieldTested, Some(0.16)),
            Ok(Some(FieldTestedLow))
        );
        assert_eq!(
            quote_bucket(FieldTested, Some(0.30)),
            Ok(Some(FieldTestedHigh))
        );
        assert_eq!(
            quote_bucket(FieldTested, Some(0.05)),
            Err(InconsistentFloat)
        );
        assert_eq!(quote_bucket(FieldTested, Some(1.5)), Err(InconsistentFloat));
        assert_eq!(
            quote_bucket(FieldTested, Some(f64::NAN)),
            Err(InconsistentFloat)
        );
    }

    #[test]
    fn never_matches_low_against_high_float() {
        for any in [false, true] {
            assert!(!order_matches_float(
                FieldTested,
                Some(FieldTestedLow),
                FieldTested,
                Some(0.30),
                any
            ));
            assert!(!order_matches_float(
                FieldTested,
                Some(FieldTestedHigh),
                FieldTested,
                Some(0.16),
                any
            ));
            assert!(order_matches_float(
                FieldTested,
                Some(FieldTestedLow),
                FieldTested,
                Some(0.16),
                any
            ));
            assert!(!order_matches_float(
                FieldTested,
                Some(FieldTestedLow),
                FieldTested,
                None,
                any
            ));
            assert!(!order_matches_float(
                FactoryNew,
                None,
                WearTier::MinimalWear,
                None,
                any
            ));
        }
    }

    #[test]
    fn tier_only_policy() {
        assert!(order_matches_float(
            FieldTested,
            None,
            FieldTested,
            None,
            false
        ));
        assert!(!order_matches_float(
            FieldTested,
            None,
            FieldTested,
            Some(0.2),
            false
        ));
        assert!(order_matches_float(
            FieldTested,
            None,
            FieldTested,
            Some(0.2),
            true
        ));
        // inconsistent float is never matched
        assert!(!order_matches_float(
            FieldTested,
            None,
            FieldTested,
            Some(0.5),
            true
        ));
    }
}
