# Workstream D — Execution simulator

Owns simulated fills and inventory (including the seven-day Valve trade hold),
buyer and seller fee accounting, trailing-seven-day verified sales and daily
liquidity limits, float/wear matching, and rejection of stale or trade-locked
items. Does not depend on any network or marketplace crate, and cannot place
real orders: `MockExecutor::submit` only mutates in-memory simulated state.

## Usage

```rust,ignore
let clock = Arc::new(ManualClock::new(start));
let fees = FeeSchedules::from_toml(&config_text)?;
let mut exec = MockExecutor::new(SimulationMode, SimConfig::default(), &fees, clock.clone(), seed)?;
exec.book_mut().upsert_listings(listings);   // from read-only clients / fixtures
exec.book_mut().record_sales(sales);
match exec.submit(order) {
    Ok(fill) => { /* fill.fee is the total fee for fill.quantity units */ }
    Err(rejection) => { /* typed `Rejection` */ }
}
let summary = exec.portfolio().summary();     // realized P&L, fee totals, cash flow
```

## Rules

| Rule | Behaviour |
| --- | --- |
| Liquidity gate | >= 10 verified, same-item, wear-comparable sales on the order platform in `(now-7d, now]`; else `InsufficientVerifiedSales` |
| Quantity cap | `floor(sales/7 * 10%)` units; a zero cap is `LiquidityCapZero`; larger orders partially fill |
| Wear | core `comparable_wear` on derived float buckets; inconsistent floats never match; low/high buckets never cross |
| Listings | must be on the order platform, observed at or before now, not trade-locked, and at most `max_listing_age` (10 min) old |
| Buy price | best eligible ask only (one exact price level per fill); limit must be >= best ask |
| Trade hold | each buy opens a `Position` with `locked_until = filled_at + 7d`; locked units cannot be sold |
| Sell price | fills at the limit if it is <= the best fresh comparable competing ask, else <= lower median of trailing verified sales |
| Fees | per unit, integer cents, rate in ppm, explicit `RoundingMode` (default half-up, matching core); buyer fee into cost basis, seller fee out of proceeds |
| P&L | FIFO lots; realized P&L = net proceeds - released cost basis |
| Determinism | seeded SplitMix64 for tie-breaks and fill latency; injected `Clock` |
