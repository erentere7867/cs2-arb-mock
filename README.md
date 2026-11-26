# cs2-arb-mock

A Rust workspace for CS2 skin arbitrage and market-dip research in **mock
simulation mode only**. Real order/trade placement is not implemented, compiled,
or executable. `MarketClient` is read-only, and the simulator has no network or
market-client dependency. Store future API credentials in process environment
variables only; never in config, fixtures, logs, or serialized state.

## Architecture

| Crate | Responsibility |
| --- | --- |
| `crates/core` | Shared market, fee, event and mock execution contracts |
| `crates/net` | Endpoint rate limits, 429 silence window and transient retries (B) |
| `crates/markets` | Skinport, CSFloat and DMarket read-only clients (A) |
| `crates/events` | CS2 branch watcher and SQLite event queue (C) |
| `crates/sim` | Mock fills, liquidity checks, fee accounting and trade holds (D) |
| `crates/strategy` | Spread and event-dip strategies, position and risk controls (E) |
| `crates/backtest` | Historical replay, reporting and secrets handling (F) |
| `bins/cs2-arb` | Mock-only CLI wiring |

```sh
cargo build
cargo test
cargo run --bin cs2-arb -- --help
```

The CLI parses `run-mock`, `backtest`, and `watch` but does not run any engine or
make any network requests in the scaffold.

## API assumptions

No live endpoints or responses have been verified in this scaffold. The
configured Skinport `/v1/items` and `/v1/sales/history` paths and 8 requests per
5 minutes are project requirements. Whether these endpoints share a quota
still needs verification. CSFloat listing/history routes
and payloads, the DMarket signing format, CS2 branch visibility and all platform
fee schedules still need verification. None of the illustrative fee schedules
should be treated as marketplace quotes.

## Backtest results

No results yet: replay is a stub. [`data/fixtures/`](data/fixtures/) contains a
deterministic synthetic history and update event for development, not measured
market performance.
