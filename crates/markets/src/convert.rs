//! Normalization helpers: source prices to [`Money`] (USD cents), floats and market hash
//! names to [`WearTier`], and RFC 3986 percent-encoding.

use cs2_arb_core::{Error, Game, ItemId, Money, Result, WearTier};

/// Largest accepted price, in cents (USD 10 billion). Anything larger is treated as bad data.
const MAX_CENTS: i64 = 1_000_000_000_000;

fn check_range(cents: i64) -> Result<Money> {
    if !(0..=MAX_CENTS).contains(&cents) {
        return Err(Error::InvalidData(format!(
            "price {cents} cents out of range"
        )));
    }
    Ok(Money(cents))
}

/// USD amount given as a JSON number in currency units (Skinport) to cents, rounded half
/// away from zero.
pub fn usd_f64_to_cents(value: f64) -> Result<Money> {
    if !value.is_finite() || value < 0.0 {
        return Err(Error::InvalidData(format!("invalid price {value}")));
    }
    let cents = (value * 100.0).round();
    if cents > MAX_CENTS as f64 {
        return Err(Error::InvalidData(format!("price {value} out of range")));
    }
    check_range(cents as i64)
}

/// Integer cents (CSFloat `price`, DMarket `priceCents`).
pub fn cents_from_i64(value: i64) -> Result<Money> {
    check_range(value)
}

/// Integer cents serialized as a decimal string (DMarket int64-as-string fields).
pub fn cents_from_int_str(value: &str) -> Result<Money> {
    let cents: i64 = value
        .trim()
        .parse()
        .map_err(|_| Error::InvalidData(format!("invalid integer cents {value:?}")))?;
    check_range(cents)
}

/// USD decimal string in currency units (DMarket last-sales `"15.99"`) to cents, exactly
/// (no float round-trip). More than two fractional digits are rounded half-up.
pub fn usd_decimal_str_to_cents(value: &str) -> Result<Money> {
    let bad = || Error::InvalidData(format!("invalid decimal price {value:?}"));
    let s = value.trim();
    let (int_part, frac_part) = match s.split_once('.') {
        Some((i, f)) => (i, f),
        None => (s, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return Err(bad());
    }
    if !int_part.bytes().all(|b| b.is_ascii_digit())
        || !frac_part.bytes().all(|b| b.is_ascii_digit())
        || int_part.len() > 12
    {
        return Err(bad());
    }
    let units: i64 = if int_part.is_empty() {
        0
    } else {
        int_part.parse().map_err(|_| bad())?
    };
    let digits: Vec<i64> = frac_part.bytes().map(|b| i64::from(b - b'0')).collect();
    let d = |i: usize| digits.get(i).copied().unwrap_or(0);
    let mut cents = units * 100 + d(0) * 10 + d(1);
    if d(2) >= 5 {
        cents += 1;
    }
    check_range(cents)
}

/// Exterior suffix of a CS2 market hash name, e.g. `"AK-47 | Redline (Field-Tested)"`.
pub fn wear_from_market_hash_name(name: &str) -> Option<WearTier> {
    const SUFFIXES: [(&str, WearTier); 5] = [
        ("(Factory New)", WearTier::FactoryNew),
        ("(Minimal Wear)", WearTier::MinimalWear),
        ("(Field-Tested)", WearTier::FieldTested),
        ("(Well-Worn)", WearTier::WellWorn),
        ("(Battle-Scarred)", WearTier::BattleScarred),
    ];
    let name = name.trim_end();
    SUFFIXES
        .iter()
        .find(|(suffix, _)| name.ends_with(suffix))
        .map(|(_, tier)| *tier)
}

/// Rejects an [`ItemId`] whose market hash name names a different exterior than its tier.
pub fn validate_item(item: &ItemId) -> Result<()> {
    if item.market_hash_name.trim().is_empty() {
        return Err(Error::InvalidData("empty market hash name".into()));
    }
    match wear_from_market_hash_name(&item.market_hash_name) {
        Some(tier) if tier != item.wear_tier => Err(Error::InvalidData(format!(
            "market hash name {:?} contradicts wear tier {:?}",
            item.market_hash_name, item.wear_tier
        ))),
        _ => Ok(()),
    }
}

pub(crate) fn validate_item_for_game(item: &ItemId, game: Game) -> Result<()> {
    if !game.supports_wear(item.wear_tier) {
        return Err(Error::InvalidData(format!(
            "incompatible wear tier for {game:?}"
        )));
    }
    if game == Game::Cs2 {
        return validate_item(item);
    }
    if item.market_hash_name.trim().is_empty() {
        return Err(Error::InvalidData("empty market hash name".into()));
    }
    Ok(())
}

/// Whether an observed float (if any) falls in the item's wear tier.
pub fn float_matches_tier(item: &ItemId, float_value: Option<f64>) -> bool {
    match float_value {
        None => true,
        Some(f) => WearTier::from_float(f) == Some(item.wear_tier),
    }
}

/// Percent-encodes everything outside the RFC 3986 unreserved set (`A-Za-z0-9-._~`).
pub fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Builds `k1=v1&k2=v2` with both sides percent-encoded, preserving order.
pub fn query_string(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_prices_to_cents() {
        assert_eq!(usd_f64_to_cents(12.34).unwrap(), Money(1234));
        assert_eq!(usd_f64_to_cents(0.1 + 0.2).unwrap(), Money(30));
        assert_eq!(usd_f64_to_cents(19.995).unwrap(), Money(2000));
        assert!(usd_f64_to_cents(-1.0).is_err());
        assert!(usd_f64_to_cents(f64::NAN).is_err());
        assert!(usd_f64_to_cents(1e300).is_err());

        assert_eq!(usd_decimal_str_to_cents("15.99").unwrap(), Money(1599));
        assert_eq!(usd_decimal_str_to_cents("1550.00").unwrap(), Money(155000));
        assert_eq!(usd_decimal_str_to_cents("7").unwrap(), Money(700));
        assert_eq!(usd_decimal_str_to_cents("0.5").unwrap(), Money(50));
        assert_eq!(usd_decimal_str_to_cents("1.005").unwrap(), Money(101));
        assert!(usd_decimal_str_to_cents("-1.00").is_err());
        assert!(usd_decimal_str_to_cents("1e3").is_err());
        assert!(usd_decimal_str_to_cents("").is_err());

        assert_eq!(cents_from_int_str("1599").unwrap(), Money(1599));
        assert!(cents_from_int_str("-5").is_err());
        assert!(cents_from_i64(-5).is_err());
    }

    #[test]
    fn maps_names_and_floats_to_wear() {
        assert_eq!(
            wear_from_market_hash_name("AK-47 | Redline (Field-Tested)"),
            Some(WearTier::FieldTested)
        );
        assert_eq!(wear_from_market_hash_name("Operation Breakout Case"), None);
        let item = ItemId {
            market_hash_name: "AK-47 | Redline (Field-Tested)".into(),
            wear_tier: WearTier::FieldTested,
        };
        assert!(validate_item(&item).is_ok());
        assert!(float_matches_tier(&item, Some(0.2)));
        assert!(!float_matches_tier(&item, Some(0.05)));
        assert!(float_matches_tier(&item, None));
        let wrong = ItemId {
            wear_tier: WearTier::FactoryNew,
            ..item
        };
        assert!(validate_item(&wrong).is_err());
    }

    #[test]
    fn percent_encodes_unreserved_only() {
        assert_eq!(
            percent_encode("AK-47 | Redline (Field-Tested)"),
            "AK-47%20%7C%20Redline%20%28Field-Tested%29"
        );
        assert_eq!(percent_encode("a~b_c.d"), "a~b_c.d");
        assert_eq!(percent_encode("★ é/%"), "%E2%98%85%20%C3%A9%2F%25");
        assert_eq!(query_string(&[("a b", "1&2")]), "a%20b=1%262");
    }
}
