//! Executor tests: every rejection path, fee/P&L worked examples, determinism.

use std::sync::Arc;

use chrono::{DateTime, TimeDelta, Utc};
use cs2_arb_core::{
    FeeSchedule, FeeSchedules, FloatBucket, ItemId, Listing, Money, Order, OrderSide, Platform,
    Sale, SimulationMode, WearTier,
};

use crate::*;

const LOW: Option<FloatBucket> = Some(FloatBucket::FieldTestedLow);
const HIGH: Option<FloatBucket> = Some(FloatBucket::FieldTestedHigh);
const LOW_FLOAT: f64 = 0.18;
const HIGH_FLOAT: f64 = 0.31;

fn t0() -> DateTime<Utc> {
    DateTime::from_timestamp(1_750_000_000, 0).unwrap()
}

fn item() -> ItemId {
    ItemId {
        market_hash_name: "AK-47 | Redline (Field-Tested)".into(),
        wear_tier: WearTier::FieldTested,
    }
}

fn other_item() -> ItemId {
    ItemId {
        market_hash_name: "AWP | Asiimov (Field-Tested)".into(),
        wear_tier: WearTier::FieldTested,
    }
}

fn schedule(buyer_pct: f64, seller_pct: f64, buyer_fixed: i64, seller_fixed: i64) -> FeeSchedule {
    FeeSchedule {
        buyer_fee_pct: buyer_pct,
        seller_fee_pct: seller_pct,
        buyer_fixed_fee: Money(buyer_fixed),
        seller_fixed_fee: Money(seller_fixed),
        ..FeeSchedule::default()
    }
}

/// Illustrative test fees: CSFloat charges the buyer 2.5% + 2c; Skinport charges
/// the seller 8%; DMarket charges the seller 7%.
fn test_fees() -> FeeSchedules {
    FeeSchedules {
        skinport: schedule(0.0, 8.0, 0, 0),
        csfloat: schedule(2.5, 2.0, 2, 0),
        dmarket: schedule(0.0, 7.0, 0, 0),
        waxpeer: schedule(0.0, 6.0, 0, 0),
    }
}

struct Harness {
    clock: Arc<ManualClock>,
    exec: MockExecutor,
}

fn harness_with(config: SimConfig, fees: FeeSchedules, seed: u64) -> Harness {
    let clock = Arc::new(ManualClock::new(t0()));
    let exec = MockExecutor::new(SimulationMode, config, &fees, clock.clone(), seed).unwrap();
    Harness { clock, exec }
}

fn harness() -> Harness {
    harness_with(SimConfig::default(), test_fees(), 7)
}

fn sale(id: &str, platform: Platform, float: Option<f64>, sold_at: DateTime<Utc>) -> Sale {
    Sale {
        id: id.into(),
        platform,
        item: item(),
        price: Money(1000),
        float_value: float,
        sold_at,
        verified: true,
    }
}

/// `n` verified sales of `item()` spread over the ~6 days before `end`.
fn sales_before(
    n: usize,
    platform: Platform,
    float: Option<f64>,
    tag: &str,
    end: DateTime<Utc>,
) -> Vec<Sale> {
    (0..n)
        .map(|i| {
            let at = end - TimeDelta::minutes(1 + (i as i64 * 7) % (6 * 24 * 60));
            sale(&format!("{tag}-{platform:?}-{i}"), platform, float, at)
        })
        .collect()
}

/// `n` verified sales of `item()` spread over the ~6 days before `t0()`.
fn sales(n: usize, platform: Platform, float: Option<f64>, tag: &str) -> Vec<Sale> {
    sales_before(n, platform, float, tag, t0())
}

fn listing(id: &str, platform: Platform, price: i64, quantity: u32, float: Option<f64>) -> Listing {
    Listing {
        id: id.into(),
        platform,
        item: item(),
        price: Money(price),
        quantity,
        float_value: float,
        observed_at: t0() - TimeDelta::minutes(1),
        trade_locked_until: None,
    }
}

fn order(id: &str, side: OrderSide, platform: Platform, quantity: u32, limit: i64) -> Order {
    Order {
        id: id.into(),
        side,
        platform,
        item: item(),
        float_bucket: LOW,
        quantity,
        limit_price: Money(limit),
        submitted_at: t0(),
    }
}

fn buy(id: &str, quantity: u32, limit: i64) -> Order {
    order(id, OrderSide::Buy, Platform::CsFloat, quantity, limit)
}

fn sell(id: &str, quantity: u32, limit: i64) -> Order {
    order(id, OrderSide::Sell, Platform::Skinport, quantity, limit)
}

impl Harness {
    /// 70 verified low-float sales on CSFloat: exactly enough for a cap of 1.
    fn liquid_csfloat(mut self, n: usize) -> Self {
        self.exec
            .book_mut()
            .record_sales(sales(n, Platform::CsFloat, Some(LOW_FLOAT), "liq"));
        self
    }

    fn liquid_skinport(mut self, n: usize) -> Self {
        self.exec
            .book_mut()
            .record_sales(sales(n, Platform::Skinport, Some(LOW_FLOAT), "liq"));
        self
    }

    /// Seed `n` fresh low-float sales ending at the current clock time.
    fn refresh_sales(&mut self, platform: Platform, n: usize, float: Option<f64>, tag: &str) {
        let now = self.clock.now();
        self.exec
            .book_mut()
            .record_sales(sales_before(n, platform, float, tag, now));
    }

    fn with_listing(mut self, listing: Listing) -> Self {
        self.exec.book_mut().upsert_listing(listing);
        self
    }

    fn reject(&mut self, order: Order) -> Rejection {
        let before_ledger = self.exec.portfolio().ledger().len();
        let before_positions = self.exec.portfolio().positions().to_vec();
        let rejection = self.exec.submit(order).unwrap_err();
        assert_eq!(self.exec.portfolio().ledger().len(), before_ledger);
        assert_eq!(
            self.exec.portfolio().positions(),
            before_positions.as_slice()
        );
        rejection
    }
}

fn assert_accounting_identity(portfolio: &Portfolio) {
    // cash flow + cost basis still held == realized P&L, always.
    assert_eq!(
        portfolio.net_cash_flow().0 + portfolio.open_cost_basis().0,
        portfolio.realized_pnl().0
    );
}

// ---------------------------------------------------------------- happy path

#[test]
fn replay_charges_all_fixed_fees_once_per_order() {
    let mut fees = test_fees();
    fees.csfloat.funding.crypto_deposit.fixed = Money(11);
    fees.skinport.seller_fixed_fee = Money(5);
    fees.skinport.funding.crypto_withdrawal.fixed = Money(7);
    let mut h = harness_with(SimConfig::default(), fees.clone(), 1)
        .liquid_csfloat(140)
        .with_listing(listing(
            "replay",
            Platform::CsFloat,
            1000,
            2,
            Some(LOW_FLOAT),
        ));
    h.exec = h.exec.with_order_fees(fees);

    let buy = buy("replay-buy", 2, 1000);
    assert_eq!(h.exec.buy_cost_at_limit(&buy).unwrap(), Money(2063));
    assert_eq!(h.exec.submit(buy).unwrap().fee, Money(63));
    h.clock.advance(TimeDelta::days(7));
    h.refresh_sales(Platform::Skinport, 140, Some(LOW_FLOAT), "sale");
    assert_eq!(
        h.exec.submit(sell("replay-sell", 2, 1000)).unwrap().fee,
        Money(172)
    );
    assert_accounting_identity(h.exec.portfolio());
}

#[test]
fn buy_does_not_spend_cash_from_another_venue() {
    use crate::venues::{VenueAccounts, VenueAccountsConfig, VenueConfig};

    let mut h = harness().liquid_csfloat(70).with_listing(listing(
        "balance",
        Platform::CsFloat,
        1000,
        1,
        Some(LOW_FLOAT),
    ));
    let rules = VenueAccountsConfig {
        venues: [
            (Platform::CsFloat, VenueConfig::with_cash(1000)),
            (Platform::Skinport, VenueConfig::with_cash(5000)),
        ]
        .into(),
        ..VenueAccountsConfig::default()
    };
    h.exec
        .set_venue_accounts(VenueAccounts::new(rules, test_fees()));
    assert_eq!(
        h.reject(buy("balance", 1, 1000)),
        Rejection::InsufficientFunds {
            required: Money(1027),
            available: Money(1000),
        }
    );
}

/// Worked example (buy): CSFloat listing at $10.00, buyer fee 2.5% + 2c.
/// fee = round(1000 * 0.025) + 2 = 25 + 2 = 27c; cash out 1027c; basis 1027c.
#[test]
fn buy_fills_at_best_ask_with_buyer_fee_and_seven_day_hold() {
    let mut h = harness().liquid_csfloat(70).with_listing(listing(
        "l1",
        Platform::CsFloat,
        1000,
        1,
        Some(LOW_FLOAT),
    ));
    let report = h.exec.submit_detailed(buy("b1", 1, 1100)).unwrap();
    assert_eq!(report.fill.quantity, 1);
    assert_eq!(report.fill.unit_price, Money(1000));
    assert_eq!(report.fill.fee, Money(27));
    assert_eq!(report.fill.filled_at, t0());
    assert_eq!(report.fees.net, Money(1027));
    assert_eq!(report.liquidity.verified_sales, 70);
    assert_eq!(report.liquidity.max_order_quantity, 1);

    let position = report.position.unwrap();
    assert_eq!(position.cost_basis, Money(1027));
    assert_eq!(position.locked_until, t0() + TimeDelta::days(7));
    assert_eq!(position.float_bucket, LOW);
    assert_eq!(position.acquired_on, Platform::CsFloat);

    let p = h.exec.portfolio();
    assert_eq!(p.buyer_fees_paid(), Money(27));
    assert_eq!(p.seller_fees_paid(), Money(0));
    assert_eq!(p.net_cash_flow(), Money(-1027));
    assert_eq!(p.realized_pnl(), Money(0));
    assert_eq!(p.ledger()[0].listings, vec![("l1".to_string(), 1)]);
    assert_accounting_identity(p);
    // The listing was consumed.
    assert_eq!(
        h.exec
            .book()
            .listing(Platform::CsFloat, "l1")
            .unwrap()
            .quantity,
        0
    );
}

/// Worked example (round trip): buy on CSFloat for 1027c all-in, wait out the
/// 7-day hold, sell on Skinport at $13.00 with an 8% seller fee:
/// fee = 104c, proceeds = 1196c, realized P&L = 1196 - 1027 = +169c.
#[test]
fn round_trip_realizes_net_of_fee_pnl() {
    let mut h = harness()
        .liquid_csfloat(70)
        .liquid_skinport(70)
        .with_listing(listing("l1", Platform::CsFloat, 1000, 1, Some(LOW_FLOAT)));
    h.exec.submit(buy("b1", 1, 1000)).unwrap();

    h.clock.advance(TimeDelta::days(7));
    // Fresh competing Skinport ask at 1350 and sales re-seeded inside the window.
    let mut competitor = listing("sp1", Platform::Skinport, 1350, 1, Some(LOW_FLOAT));
    competitor.observed_at = h.clock.now();
    h.exec.book_mut().upsert_listing(competitor);
    h.refresh_sales(Platform::Skinport, 70, Some(LOW_FLOAT), "later");

    let report = h.exec.submit_detailed(sell("s1", 1, 1300)).unwrap();
    assert_eq!(report.fill.unit_price, Money(1300));
    assert_eq!(report.fill.fee, Money(104));
    assert_eq!(report.fees.net, Money(1196));
    assert_eq!(report.ledger_entry.cost_basis_released, Some(Money(1027)));
    assert_eq!(report.ledger_entry.realized_pnl, Some(Money(169)));

    let p = h.exec.portfolio();
    assert!(p.positions().is_empty());
    assert_eq!(p.realized_pnl(), Money(169));
    assert_eq!(p.buyer_fees_paid(), Money(27));
    assert_eq!(p.seller_fees_paid(), Money(104));
    assert_eq!(p.total_fees_paid(), Money(131));
    assert_eq!(p.gross_bought(), Money(1000));
    assert_eq!(p.gross_sold(), Money(1300));
    assert_eq!(p.net_cash_flow(), Money(169));
    assert_accounting_identity(p);
    let summary = p.summary();
    assert_eq!(summary.fills, 2);
    assert_eq!(summary.open_quantity, 0);
}

// ---------------------------------------------------------- order validation

#[test]
fn rejects_duplicate_filled_order_id() {
    let mut h = harness().liquid_csfloat(140).with_listing(listing(
        "l1",
        Platform::CsFloat,
        1000,
        2,
        Some(LOW_FLOAT),
    ));
    h.exec.submit(buy("b1", 1, 1000)).unwrap();
    assert_eq!(
        h.reject(buy("b1", 1, 1000)),
        Rejection::DuplicateOrderId {
            order_id: "b1".into()
        }
    );
    // A rejected id may be retried.
    assert!(matches!(
        h.reject(buy("b2", 1, 1)),
        Rejection::LimitBelowBestAsk { .. }
    ));
    h.exec.submit(buy("b2", 1, 1000)).unwrap();
}

#[test]
fn rejects_zero_quantity_and_nonpositive_limits() {
    let mut h = harness().liquid_csfloat(70);
    assert_eq!(h.reject(buy("b", 0, 1000)), Rejection::ZeroQuantity);
    assert_eq!(
        h.reject(buy("b", 1, 0)),
        Rejection::InvalidLimitPrice { limit: Money(0) }
    );
    assert_eq!(
        h.reject(buy("b", 1, -5)),
        Rejection::InvalidLimitPrice { limit: Money(-5) }
    );
}

#[test]
fn rejects_order_bucket_outside_its_wear_tier() {
    let mut h = harness().liquid_csfloat(70);
    let mut o = buy("b", 1, 1000);
    o.float_bucket = Some(FloatBucket::MinimalWear);
    assert_eq!(
        h.reject(o),
        Rejection::OrderWearInconsistent {
            tier: WearTier::FieldTested,
            bucket: FloatBucket::MinimalWear
        }
    );
}

// ------------------------------------------------------------ liquidity gate

#[test]
fn liquidity_gate_counts_only_verified_comparable_trailing_sales() {
    let mut h = harness().with_listing(listing("l1", Platform::CsFloat, 1000, 1, Some(LOW_FLOAT)));
    let now = t0();
    let mut noise = vec![
        // unverified
        Sale {
            verified: false,
            ..sale("u", Platform::CsFloat, Some(LOW_FLOAT), now)
        },
        // exactly at the window start is excluded (window is (now-7d, now])
        sale(
            "old",
            Platform::CsFloat,
            Some(LOW_FLOAT),
            now - TimeDelta::days(7),
        ),
        // in the future (no look-ahead)
        sale(
            "future",
            Platform::CsFloat,
            Some(LOW_FLOAT),
            now + TimeDelta::seconds(1),
        ),
        // other platform
        sale("sp", Platform::Skinport, Some(LOW_FLOAT), now),
        // high-float bucket does not count for a low-float order
        sale("high", Platform::CsFloat, Some(HIGH_FLOAT), now),
        // no float does not count for a bucketed order
        sale("nofloat", Platform::CsFloat, None, now),
        // inconsistent float (FN float on an FT item)
        sale("bad", Platform::CsFloat, Some(0.01), now),
    ];
    let mut other = sale("other", Platform::CsFloat, Some(LOW_FLOAT), now);
    other.item = other_item();
    noise.push(other);
    h.exec.book_mut().record_sales(noise);
    h.exec
        .book_mut()
        .record_sales(sales(9, Platform::CsFloat, Some(LOW_FLOAT), "ok"));
    // Duplicates are ignored.
    assert_eq!(
        h.exec
            .book_mut()
            .record_sales(sales(9, Platform::CsFloat, Some(LOW_FLOAT), "ok")),
        0
    );

    assert_eq!(
        h.reject(buy("b", 1, 1000)),
        Rejection::InsufficientVerifiedSales {
            found: 9,
            required: 10,
            window_days: 7
        }
    );
    // The boundary sale at exactly now is included.
    h.exec
        .book_mut()
        .record_sale(sale("edge", Platform::CsFloat, Some(LOW_FLOAT), now));
    assert_eq!(
        h.reject(buy("b", 1, 1000)),
        Rejection::LiquidityCapZero {
            verified_sales: 10,
            window_days: 7
        }
    );
}

#[test]
fn liquidity_cap_is_floor_of_ten_percent_adv_and_zero_rejects() {
    // 69 sales: ADV 9.857/day, 10% = 0.98 -> floor 0 -> reject.
    let mut h = harness().liquid_csfloat(69).with_listing(listing(
        "l1",
        Platform::CsFloat,
        1000,
        5,
        Some(LOW_FLOAT),
    ));
    assert_eq!(
        h.reject(buy("b", 1, 1000)),
        Rejection::LiquidityCapZero {
            verified_sales: 69,
            window_days: 7
        }
    );
    // 70 sales: cap 1.
    h.exec
        .book_mut()
        .record_sales(sales(1, Platform::CsFloat, Some(LOW_FLOAT), "x"));
    assert_eq!(h.exec.submit(buy("b", 5, 1000)).unwrap().quantity, 1);

    // 140 sales: cap 2, so a 5-unit order partially fills 2.
    let mut h = harness().liquid_csfloat(140).with_listing(listing(
        "l1",
        Platform::CsFloat,
        1000,
        5,
        Some(LOW_FLOAT),
    ));
    let fill = h.exec.submit(buy("b", 5, 1000)).unwrap();
    assert_eq!(fill.quantity, 2);
    assert_eq!(fill.fee, Money(52));
    assert_eq!(h.exec.portfolio().positions()[0].cost_basis, Money(2052));
}

#[test]
fn liquidity_gate_expires_as_clock_moves() {
    let mut h = harness().liquid_csfloat(70).with_listing(listing(
        "l1",
        Platform::CsFloat,
        1000,
        5,
        Some(LOW_FLOAT),
    ));
    h.clock.advance(TimeDelta::days(8));
    assert!(matches!(
        h.reject(buy("b", 1, 1000)),
        Rejection::InsufficientVerifiedSales { found: 0, .. }
    ));
}

#[test]
fn liquidity_scope_all_platforms() {
    let config = SimConfig {
        liquidity: LiquidityConfig {
            scope: LiquidityScope::AllPlatforms,
            ..LiquidityConfig::default()
        },
        ..SimConfig::default()
    };
    let mut h = harness_with(config, test_fees(), 1)
        .liquid_csfloat(35)
        .liquid_skinport(35)
        .with_listing(listing("l1", Platform::CsFloat, 1000, 1, Some(LOW_FLOAT)));
    assert_eq!(
        h.exec
            .liquidity(Platform::CsFloat, &item(), LOW)
            .verified_sales,
        70
    );
    h.exec.submit(buy("b", 1, 1000)).unwrap();
    // Default scope would only see 35.
    let h2 = harness().liquid_csfloat(35).liquid_skinport(35);
    assert_eq!(
        h2.exec
            .liquidity(Platform::CsFloat, &item(), LOW)
            .verified_sales,
        35
    );
}

// ----------------------------------------------------- buy listing selection

#[test]
fn rejects_when_no_visible_listing() {
    let mut future = listing("future", Platform::CsFloat, 900, 1, Some(LOW_FLOAT));
    future.observed_at = t0() + TimeDelta::seconds(1);
    let mut other = listing("other", Platform::CsFloat, 900, 1, Some(LOW_FLOAT));
    other.item = other_item();
    let mut h = harness()
        .liquid_csfloat(70)
        .with_listing(listing("sp", Platform::Skinport, 900, 1, Some(LOW_FLOAT)))
        .with_listing(listing("empty", Platform::CsFloat, 900, 0, Some(LOW_FLOAT)))
        .with_listing(future)
        .with_listing(other);
    assert_eq!(h.reject(buy("b", 1, 1000)), Rejection::NoListings);
}

#[test]
fn rejects_high_low_float_mismatch_and_inconsistent_floats() {
    let mut h = harness()
        .liquid_csfloat(70)
        .with_listing(listing("high", Platform::CsFloat, 900, 1, Some(HIGH_FLOAT)))
        .with_listing(listing("nofloat", Platform::CsFloat, 900, 1, None))
        // FN float on an FT item: inconsistent data, never matched
        .with_listing(listing("bad", Platform::CsFloat, 900, 1, Some(0.02)));
    assert_eq!(
        h.reject(buy("b", 1, 1000)),
        Rejection::WearMismatch { candidates: 3 }
    );

    // A high-float order does match the high-float listing (with high-float liquidity).
    h.exec
        .book_mut()
        .record_sales(sales(70, Platform::CsFloat, Some(HIGH_FLOAT), "hi"));
    let mut o = buy("h", 1, 1000);
    o.float_bucket = HIGH;
    let report = h.exec.submit_detailed(o).unwrap();
    assert_eq!(report.ledger_entry.listings, vec![("high".to_string(), 1)]);
}

#[test]
fn tier_only_orders_follow_comparable_wear_unless_opted_in() {
    let tier_only = |id: &str| Order {
        float_bucket: None,
        ..buy(id, 1, 1000)
    };
    // Strict (default): tier-only order needs tier-only sales and listings.
    let mut h = harness().with_listing(listing("f", Platform::CsFloat, 900, 1, Some(LOW_FLOAT)));
    h.exec
        .book_mut()
        .record_sales(sales(70, Platform::CsFloat, None, "tier"));
    assert_eq!(
        h.reject(tier_only("t")),
        Rejection::WearMismatch { candidates: 1 }
    );
    h.exec
        .book_mut()
        .upsert_listing(listing("nf", Platform::CsFloat, 950, 1, None));
    let report = h.exec.submit_detailed(tier_only("t")).unwrap();
    assert_eq!(report.fill.unit_price, Money(950));

    // Opt-in: a tier-only order may take any in-tier float.
    let config = SimConfig {
        tier_only_matches_any_float: true,
        ..SimConfig::default()
    };
    let mut h = harness_with(config, test_fees(), 1)
        .liquid_csfloat(70)
        .with_listing(listing("f", Platform::CsFloat, 900, 1, Some(LOW_FLOAT)))
        .with_listing(listing("bad", Platform::CsFloat, 800, 1, Some(0.6)));
    let report = h.exec.submit_detailed(tier_only("t")).unwrap();
    assert_eq!(report.fill.unit_price, Money(900));
}

#[test]
fn rejects_trade_locked_listings() {
    let unlock = t0() + TimeDelta::days(3);
    let mut locked = listing("locked", Platform::CsFloat, 900, 1, Some(LOW_FLOAT));
    locked.trade_locked_until = Some(unlock);
    let mut later = locked.clone();
    later.id = "later".into();
    later.trade_locked_until = Some(unlock + TimeDelta::days(1));
    let mut h = harness()
        .liquid_csfloat(70)
        .with_listing(locked)
        .with_listing(later);
    assert_eq!(
        h.reject(buy("b", 1, 1000)),
        Rejection::ListingTradeLocked {
            earliest_unlock: unlock
        }
    );

    // A lock that ends exactly now no longer blocks.
    let mut expired = listing("expired", Platform::CsFloat, 950, 1, Some(LOW_FLOAT));
    expired.trade_locked_until = Some(t0());
    h.exec.book_mut().upsert_listing(expired);
    let report = h.exec.submit_detailed(buy("b", 1, 1000)).unwrap();
    assert_eq!(
        report.ledger_entry.listings,
        vec![("expired".to_string(), 1)]
    );
}

#[test]
fn rejects_stale_listings_with_configurable_max_age() {
    let mut stale = listing("stale", Platform::CsFloat, 900, 1, Some(LOW_FLOAT));
    stale.observed_at = t0() - TimeDelta::minutes(10) - TimeDelta::seconds(1);
    let mut h = harness().liquid_csfloat(70).with_listing(stale.clone());
    assert_eq!(
        h.reject(buy("b", 1, 1000)),
        Rejection::StaleListing {
            newest_observed_at: stale.observed_at,
            max_age_secs: 600
        }
    );
    // Exactly max age is still fresh.
    let mut edge = listing("edge", Platform::CsFloat, 950, 1, Some(LOW_FLOAT));
    edge.observed_at = t0() - TimeDelta::minutes(10);
    h.exec.book_mut().upsert_listing(edge);
    assert_eq!(
        h.exec.submit(buy("b", 1, 1000)).unwrap().unit_price,
        Money(950)
    );

    // A longer configured max age accepts the older listing.
    let config = SimConfig {
        max_listing_age: TimeDelta::hours(1),
        ..SimConfig::default()
    };
    let mut h = harness_with(config, test_fees(), 1)
        .liquid_csfloat(70)
        .with_listing(stale);
    assert_eq!(
        h.exec.submit(buy("b", 1, 1000)).unwrap().unit_price,
        Money(900)
    );
}

#[test]
fn rejects_limit_below_best_eligible_ask() {
    let mut h = harness()
        .liquid_csfloat(70)
        .with_listing(listing("a", Platform::CsFloat, 1001, 1, Some(LOW_FLOAT)))
        .with_listing(listing("b", Platform::CsFloat, 1200, 1, Some(LOW_FLOAT)));
    assert_eq!(
        h.reject(buy("o", 1, 1000)),
        Rejection::LimitBelowBestAsk {
            limit: Money(1000),
            best_ask: Money(1001)
        }
    );
}

#[test]
fn skips_ineligible_cheaper_listings_and_fills_one_price_level() {
    let mut stale = listing("stale", Platform::CsFloat, 500, 5, Some(LOW_FLOAT));
    stale.observed_at = t0() - TimeDelta::hours(1);
    let mut locked = listing("locked", Platform::CsFloat, 600, 5, Some(LOW_FLOAT));
    locked.trade_locked_until = Some(t0() + TimeDelta::days(1));
    let mut h = harness()
        .liquid_csfloat(700) // cap 10
        .with_listing(stale)
        .with_listing(locked)
        .with_listing(listing("high", Platform::CsFloat, 700, 5, Some(HIGH_FLOAT)))
        .with_listing(listing("best", Platform::CsFloat, 1000, 1, Some(LOW_FLOAT)))
        .with_listing(listing("next", Platform::CsFloat, 1001, 5, Some(LOW_FLOAT)));
    let report = h.exec.submit_detailed(buy("o", 3, 1100)).unwrap();
    // Only the 1000c level fills, so the fill has one exact unit price.
    assert_eq!(report.fill.quantity, 1);
    assert_eq!(report.fill.unit_price, Money(1000));
    // Next order sweeps the following level.
    let fill = h.exec.submit(buy("o2", 3, 1100)).unwrap();
    assert_eq!((fill.quantity, fill.unit_price), (3, Money(1001)));
}

#[test]
fn rejects_insufficient_funds_when_cash_is_tracked() {
    let config = SimConfig {
        starting_cash: Some(Money(1026)),
        ..SimConfig::default()
    };
    let mut h = harness_with(config, test_fees(), 1)
        .liquid_csfloat(70)
        .with_listing(listing("l", Platform::CsFloat, 1000, 1, Some(LOW_FLOAT)));
    // 1000 + 27 fee = 1027 > 1026.
    assert_eq!(
        h.reject(buy("b", 1, 1000)),
        Rejection::InsufficientFunds {
            required: Money(1027),
            available: Money(1026)
        }
    );
    assert_eq!(
        h.exec
            .book()
            .listing(Platform::CsFloat, "l")
            .unwrap()
            .quantity,
        1
    );
}

#[test]
fn consumed_listings_are_not_resurrected_by_later_snapshots() {
    let snapshot = listing("l", Platform::CsFloat, 1000, 2, Some(LOW_FLOAT));
    let mut h = harness().liquid_csfloat(70).with_listing(snapshot.clone());
    h.exec.submit(buy("b1", 1, 1000)).unwrap();
    h.exec.book_mut().upsert_listing(snapshot.clone());
    assert_eq!(h.exec.book().consumed_quantity(Platform::CsFloat, "l"), 1);
    assert_eq!(
        h.exec
            .book()
            .listing(Platform::CsFloat, "l")
            .unwrap()
            .quantity,
        1
    );
    h.exec.submit(buy("b2", 1, 1000)).unwrap();
    h.exec
        .book_mut()
        .replace_listings(Platform::CsFloat, &item(), [snapshot]);
    assert_eq!(h.reject(buy("b3", 1, 1000)), Rejection::NoListings);
}

// ------------------------------------------------------------------ sell side

/// Buys one low-float unit on CSFloat at 1000 (+27 fee) at `t0()`.
fn holding() -> Harness {
    let mut h = harness()
        .liquid_csfloat(70)
        .liquid_skinport(70)
        .with_listing(listing("l1", Platform::CsFloat, 1000, 1, Some(LOW_FLOAT)));
    h.exec.submit(buy("b1", 1, 1000)).unwrap();
    h
}

#[test]
fn rejects_sell_without_inventory() {
    let mut h = harness().liquid_skinport(70);
    assert_eq!(
        h.reject(sell("s", 1, 1000)),
        Rejection::InsufficientInventory {
            requested: 1,
            available: 0
        }
    );
}

#[test]
fn rejects_sell_across_float_buckets() {
    let mut h = holding();
    h.clock.advance(TimeDelta::days(7));
    h.refresh_sales(Platform::Skinport, 70, Some(HIGH_FLOAT), "hi");
    let mut o = sell("s", 1, 900);
    o.float_bucket = HIGH;
    assert_eq!(
        h.reject(o),
        Rejection::InsufficientInventory {
            requested: 1,
            available: 0
        }
    );
}

#[test]
fn rejects_sell_of_trade_locked_inventory_until_hold_expires() {
    let mut h = holding();
    let unlock = t0() + TimeDelta::days(7);
    assert_eq!(h.exec.portfolio().positions()[0].locked_until, unlock);
    assert_eq!(h.exec.portfolio().sellable_quantity(&item(), LOW, t0()), 0);
    assert_eq!(
        h.reject(sell("s", 1, 900)),
        Rejection::PositionTradeLocked {
            requested: 1,
            unlocked: 0,
            earliest_unlock: unlock
        }
    );
    // One second before the hold ends: still locked (sales are still in window).
    h.clock.set(unlock - TimeDelta::seconds(1));
    h.refresh_sales(Platform::Skinport, 70, Some(LOW_FLOAT), "late");
    assert!(matches!(
        h.reject(sell("s", 1, 900)),
        Rejection::PositionTradeLocked { .. }
    ));
    // At the unlock instant it is tradable.
    h.clock.set(unlock);
    assert_eq!(
        h.exec.portfolio().sellable_quantity(&item(), LOW, unlock),
        1
    );
    let fill = h.exec.submit(sell("s", 1, 900)).unwrap();
    assert_eq!(fill.quantity, 1);
}

#[test]
fn partial_lock_rejects_rather_than_silently_selling_less() {
    let mut h = harness()
        .liquid_csfloat(140)
        .liquid_skinport(140)
        .with_listing(listing("l1", Platform::CsFloat, 1000, 1, Some(LOW_FLOAT)));
    h.exec.submit(buy("b1", 1, 1000)).unwrap();
    h.clock.advance(TimeDelta::days(1));
    let mut l2 = listing("l2", Platform::CsFloat, 1000, 1, Some(LOW_FLOAT));
    l2.observed_at = h.clock.now();
    h.exec.book_mut().upsert_listing(l2);
    h.exec.submit(buy("b2", 1, 1000)).unwrap();
    // Day 7: first lot unlocked, second still locked until day 8.
    h.clock.set(t0() + TimeDelta::days(7));
    h.refresh_sales(Platform::Skinport, 140, Some(LOW_FLOAT), "late");
    assert_eq!(
        h.reject(sell("s", 2, 900)),
        Rejection::PositionTradeLocked {
            requested: 2,
            unlocked: 1,
            earliest_unlock: t0() + TimeDelta::days(8)
        }
    );
}

#[test]
fn rejects_sell_limit_above_market_reference() {
    let mut h = holding();
    h.clock.advance(TimeDelta::days(7));
    let now = h.clock.now();
    // Liquidity sales inside the new window, prices 1000..=1069 (lower median 1034).
    let fresh_sales: Vec<Sale> = (0..70)
        .map(|i| Sale {
            price: Money(1000 + i),
            ..sale(
                &format!("r{i}"),
                Platform::Skinport,
                Some(LOW_FLOAT),
                now - TimeDelta::hours(i),
            )
        })
        .collect();
    h.exec.book_mut().record_sales(fresh_sales);
    // No competing listings: the reference is the lower median of trailing sales.
    assert_eq!(
        h.reject(sell("s", 1, 1035)),
        Rejection::SellLimitAboveMarket {
            limit: Money(1035),
            reference: Money(1034)
        }
    );
    // A fresh competing ask at 990 becomes the reference (stale/mismatched ones do not).
    let mut competitor = listing("c", Platform::Skinport, 990, 1, Some(LOW_FLOAT));
    competitor.observed_at = now;
    let mut stale = listing("old", Platform::Skinport, 500, 1, Some(LOW_FLOAT));
    stale.observed_at = now - TimeDelta::hours(1);
    let mut high = listing("hi", Platform::Skinport, 400, 1, Some(HIGH_FLOAT));
    high.observed_at = now;
    h.exec.book_mut().upsert_listings([competitor, stale, high]);
    assert_eq!(
        h.reject(sell("s", 1, 1000)),
        Rejection::SellLimitAboveMarket {
            limit: Money(1000),
            reference: Money(990)
        }
    );
    // Matching the best ask fills.
    let report = h.exec.submit_detailed(sell("s", 1, 990)).unwrap();
    // 8% of 990 = 79.2 -> 79; proceeds 911; P&L 911 - 1027 = -116.
    assert_eq!(report.fill.fee, Money(79));
    assert_eq!(report.ledger_entry.realized_pnl, Some(Money(-116)));
    assert_eq!(h.exec.portfolio().realized_pnl(), Money(-116));
    assert_accounting_identity(h.exec.portfolio());
}

#[test]
fn rejects_sell_when_fees_consume_all_proceeds() {
    let mut fees = test_fees();
    fees.skinport = schedule(0.0, 0.0, 0, 50);
    let mut h = harness_with(SimConfig::default(), fees, 1)
        .liquid_csfloat(70)
        .liquid_skinport(70)
        .with_listing(listing("l1", Platform::CsFloat, 1000, 1, Some(LOW_FLOAT)));
    h.exec.submit(buy("b1", 1, 1000)).unwrap();
    h.clock.advance(TimeDelta::days(7));
    h.refresh_sales(Platform::Skinport, 70, Some(LOW_FLOAT), "late");
    assert_eq!(
        h.reject(sell("s", 1, 50)),
        Rejection::NonPositiveProceeds {
            unit_price: Money(50),
            unit_fee: Money(50)
        }
    );
    // 51c leaves 1c of proceeds and fills.
    assert_eq!(h.exec.submit(sell("s", 1, 51)).unwrap().fee, Money(50));
}

/// Worked FIFO example: lot A = 2 units at 1000 (+25 each + 2 fixed) = 2052; lot B = 1
/// unit at 1100 (+round(27.5)=28, +2 = 30) = 1130. After both unlock, selling 2
/// on DMarket at 1200 (7% = 84 each) releases lot A's 2052 basis:
/// proceeds 2 * (1200 - 84) = 2232; P&L = 2232 - 2052 = +180.
/// Then selling the last unit at 1200 releases 1130: P&L 1116 - 1130 = -14.
#[test]
fn sells_fifo_with_exact_cost_basis_release() {
    let mut h = harness().liquid_csfloat(140).with_listing(listing(
        "a",
        Platform::CsFloat,
        1000,
        2,
        Some(LOW_FLOAT),
    ));
    h.exec
        .book_mut()
        .record_sales(sales(140, Platform::DMarket, Some(LOW_FLOAT), "dm"));
    h.exec.submit(buy("A", 2, 1000)).unwrap();
    h.clock.advance(TimeDelta::hours(1));
    let mut b = listing("b", Platform::CsFloat, 1100, 1, Some(LOW_FLOAT));
    b.observed_at = h.clock.now();
    h.exec.book_mut().upsert_listing(b);
    h.exec.submit(buy("B", 1, 1100)).unwrap();
    assert_eq!(h.exec.portfolio().open_cost_basis(), Money(2052 + 1130));

    h.clock.set(t0() + TimeDelta::days(7) + TimeDelta::hours(1));
    let now = h.clock.now();
    h.exec.book_mut().record_sales(
        sales_before(140, Platform::DMarket, Some(LOW_FLOAT), "dm2", now)
            .into_iter()
            .map(|s| Sale {
                price: Money(1300),
                ..s
            }),
    );
    let dm_sell = |id: &str, q| order(id, OrderSide::Sell, Platform::DMarket, q, 1200);
    let first = h.exec.submit_detailed(dm_sell("s1", 2)).unwrap();
    assert_eq!(first.fill.fee, Money(168));
    assert_eq!(first.ledger_entry.cost_basis_released, Some(Money(2052)));
    assert_eq!(first.ledger_entry.realized_pnl, Some(Money(180)));
    assert_eq!(first.ledger_entry.positions, vec![("A#pos".to_string(), 2)]);
    let second = h.exec.submit_detailed(dm_sell("s2", 1)).unwrap();
    assert_eq!(second.ledger_entry.realized_pnl, Some(Money(-14)));
    let p = h.exec.portfolio();
    assert_eq!(p.realized_pnl(), Money(166));
    assert_eq!(p.buyer_fees_paid(), Money(52 + 30));
    assert_eq!(p.seller_fees_paid(), Money(252));
    assert!(p.positions().is_empty());
    assert_accounting_identity(p);
}

#[test]
fn partial_sell_of_a_lot_releases_proportional_basis() {
    let mut h = harness().liquid_csfloat(140).with_listing(listing(
        "a",
        Platform::CsFloat,
        1000,
        2,
        Some(LOW_FLOAT),
    ));
    h.exec.submit(buy("A", 2, 1000)).unwrap();
    h.clock.advance(TimeDelta::days(7));
    h.refresh_sales(Platform::CsFloat, 140, Some(LOW_FLOAT), "later");
    let s = order("s", OrderSide::Sell, Platform::CsFloat, 1, 1000);
    let report = h.exec.submit_detailed(s).unwrap();
    // CSFloat seller 2% of 1000 = 20; proceeds 980; basis 2052/2 = 1026.
    assert_eq!(report.ledger_entry.cost_basis_released, Some(Money(1026)));
    assert_eq!(report.ledger_entry.realized_pnl, Some(Money(-46)));
    let p = h.exec.portfolio();
    assert_eq!(p.positions()[0].quantity, 1);
    assert_eq!(p.positions()[0].cost_basis, Money(1026));
    assert_accounting_identity(p);
}

// ------------------------------------------------------------- determinism

fn scenario(seed: u64) -> Vec<ExecutionReport> {
    let config = SimConfig {
        fill_latency: FillLatency {
            min: TimeDelta::seconds(1),
            max: TimeDelta::seconds(90),
        },
        ..SimConfig::default()
    };
    let mut h = harness_with(config, test_fees(), seed).liquid_csfloat(700);
    for i in 0..6 {
        h.exec.book_mut().upsert_listing(listing(
            &format!("tie{i}"),
            Platform::CsFloat,
            1000,
            1,
            Some(LOW_FLOAT),
        ));
    }
    (0..3)
        .map(|i| {
            h.exec
                .submit_detailed(buy(&format!("b{i}"), 1, 1000))
                .unwrap()
        })
        .collect()
}

#[test]
fn identical_seed_and_inputs_give_identical_results() {
    let a = scenario(99);
    assert_eq!(a, scenario(99));
    for report in &a {
        let fill_at = report.fill.filled_at;
        assert!(fill_at >= t0() + TimeDelta::seconds(1));
        assert!(fill_at <= t0() + TimeDelta::seconds(90));
        assert_eq!(
            report.position.as_ref().unwrap().locked_until,
            fill_at + TimeDelta::days(7)
        );
    }
    // Some other seed picks different tied listings or latencies.
    assert!((0..20).any(|seed| scenario(seed) != a));
}

// ------------------------------------------------------------------ misc

#[test]
fn rejections_are_logged_with_time_and_reason() {
    let mut h = harness();
    h.reject(buy("x", 0, 1000));
    h.clock.advance(TimeDelta::seconds(5));
    h.reject(sell("y", 1, 1000));
    let log = h.exec.rejections();
    assert_eq!(log.len(), 2);
    assert_eq!(log[0].order_id, "x");
    assert_eq!(log[0].rejected_at, t0());
    assert_eq!(log[1].rejected_at, t0() + TimeDelta::seconds(5));
    assert!(matches!(
        log[1].rejection,
        Rejection::InsufficientVerifiedSales { .. }
    ));
    let json = serde_json::to_value(&log[0].rejection).unwrap();
    assert_eq!(json["reason"], "zero_quantity");
}

#[test]
fn invalid_configs_are_refused() {
    let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(t0()));
    let bad = [
        SimConfig {
            max_listing_age: TimeDelta::seconds(-1),
            ..SimConfig::default()
        },
        SimConfig {
            liquidity: LiquidityConfig {
                window_days: 0,
                ..LiquidityConfig::default()
            },
            ..SimConfig::default()
        },
        SimConfig {
            liquidity: LiquidityConfig {
                max_adv_fraction_bps: 10_001,
                ..LiquidityConfig::default()
            },
            ..SimConfig::default()
        },
        SimConfig {
            fill_latency: FillLatency {
                min: TimeDelta::seconds(2),
                max: TimeDelta::seconds(1),
            },
            ..SimConfig::default()
        },
        SimConfig {
            starting_cash: Some(Money(-1)),
            ..SimConfig::default()
        },
    ];
    for config in bad {
        assert!(MockExecutor::new(SimulationMode, config, &test_fees(), clock.clone(), 0).is_err());
    }
    let mut fees = test_fees();
    fees.dmarket.seller_fee_pct = 1.23456;
    assert!(MockExecutor::new(SimulationMode, SimConfig::default(), &fees, clock, 0).is_err());
}

#[test]
fn default_config_fees_load_into_executor() {
    let fees =
        FeeSchedules::from_toml(include_str!("../../../data/fixtures/test_fees.toml")).unwrap();
    let clock = Arc::new(ManualClock::new(t0()));
    let exec = MockExecutor::new(SimulationMode, SimConfig::default(), &fees, clock, 0).unwrap();
    assert_eq!(exec.fees().get(Platform::Skinport).seller.rate_ppm, 80_000);
}

#[test]
fn stateless_simulate_fill_wrapper() {
    let fees = schedule(2.5, 0.0, 2, 0);
    let all_sales = sales(70, Platform::CsFloat, Some(LOW_FLOAT), "w");
    let l = listing("l", Platform::CsFloat, 1000, 1, Some(LOW_FLOAT));
    let fill = simulate_fill(
        SimulationMode,
        &buy("b", 1, 1000),
        &l,
        &all_sales,
        fees,
        t0(),
    )
    .unwrap();
    assert_eq!(fill.fee, Money(27));
    let err = simulate_fill(
        SimulationMode,
        &buy("b", 1, 999),
        &l,
        &all_sales,
        fees,
        t0(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("limit"));
}
