# Workstream F — Operations and backtesting

Owns environment-only secrets with no logging or serialization, deterministic
historical replay through strategy and simulation, and reports for net P&L,
trade duration and fee drag. Fixture data lives in `data/fixtures/`. Everything
is mock simulation: strategies emit `Order` values that an in-process
`FillModel` fills against replayed quotes. Nothing here can place a real order.

## Modules

| Module | Contents |
| --- | --- |
| `ops::secrets` | `Secret` (env-only, redacted `Debug`/`Display`, no `Serialize`/`Clone`, zeroized on drop) and `Secrets::from_env()` for `SKINPORT_CLIENT_ID`, `SKINPORT_CLIENT_SECRET`, `CSFLOAT_API_KEY`, `DMARKET_PUBLIC_KEY`, `DMARKET_SECRET_KEY` |
| `replay` | `Backtester`, `BacktestConfig`, `StrategyAdapter`, `MarketView`, `replay()` |
| `fill` | `FillModel` trait and the built-in `SnapshotFillModel` (fills at the quote, applies configured fees, 7-day hold) |
| `report` | `PerformanceReport` with `to_json()` / `to_markdown()` |
| `integrated` | CLI adapters for `cs2-arb-strategy` and `cs2-arb-sim`, including historical depth, per-strategy risk, and `MockExecutor` fills |
| `strategies` | Built-in `SpreadStrategy` and `DipStrategy` for the lightweight comparison replay |
| `fixtures` | JSON snapshot/event loaders and the fee-config loader |

## Replay semantics

- Snapshots are stably sorted by `observed_at` and grouped into one step per timestamp.
- Events are delivered at the first step at or after their timestamp.
- Buys fill on `order.platform` against the latest quote for the same item and
  float bucket (max age 1 day by default) and create a `Position` locked for 7 days.
- Sells consume the strategy's unlocked positions for that item and bucket in
  FIFO order on any platform; cost, fees and proceeds are prorated per lot.
- Equity is marked to market each step (realized net plus open positions at
  the best net-of-seller-fee quote). Max drawdown is taken on that curve, starting at zero.

## Lightweight model fixture baseline

For the CLI's results using real strategy and mock executor logic, see the
[workspace backtest results](../../README.md#bundled-fixture-backtest).

`cargo test -p cs2-arb-backtest --test fixtures_backtest -- --nocapture` prints
the full Markdown report. With `config/default.toml` fees and default strategy
parameters on the synthetic fixtures (not market evidence):

| Metric | all | spread | dip |
| --- | ---: | ---: | ---: |
| Net realized P&L | $25.46 | $10.93 | $14.53 |
| Gross realized P&L | $28.92 | $13.21 | $15.71 |
| Fees (fee drag % of gross) | $3.46 (11.96%) | $2.28 (17.26%) | $1.18 (7.51%) |
| Trades / win rate | 10 / 100% | 8 / 100% | 2 / 100% |
| Avg / median duration | 7d 9h 36m / 7d | 7d 12h / 7d | 7d / 7d |
| Max drawdown | $3.20 | $1.14 | $2.26 |
| Open positions (unrealized) | 1 (-$0.04) | 1 (-$0.04) | 0 |

The spread strategy's take-profit is at breakeven, so it only realizes a loss
at its 14-day time stop. That keeps its realized win rate high; the losing lot
that is still open shows up as unrealized P&L.
