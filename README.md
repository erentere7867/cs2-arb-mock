# cs2-arb-mock

CS2 and Rust (Facepunch) skin arbitrage research in **mock simulation mode only**. The engine cannot send an order, accept a trade, or move inventory on a marketplace.

## Architecture

```text
cs2-arb CLI
├── markets ── net       read-only listings and individual sales; cache, quotas, retries
├── events              CS2 branch metadata → SQLite queue → panic-window events
└── backtest ── strategy ── sim
             └────────────── sim   local fills, inventory, fees and the seven-day hold

All library crates share core's market and simulation types; sim has no network dependency.
```

Historical snapshots plus sales, or explicitly enabled read-only listing samples, feed a time-ordered replay. The spread strategy compares identical items and float buckets across markets, net of both sides' fees and seven days of opportunity cost. The dip strategy looks for liquid items more than two standard deviations below their prior 30-day baseline during an update window. The risk manager checks position and portfolio exposure, UTC-day drawdown and the kill switch before a mock order reaches the simulator.

The simulator requires ten verified sales in the last seven days on the order's platform and caps order size at 10% of trailing average daily volume, rounded down (so one unit needs at least 70 sales). It rejects stale listings, trade-locked items and mismatched wear or float buckets. Bought positions cannot be sold for seven days. Spread and dip results use separate simulated portfolios and risk ledgers.

### Mock-only safeguards

- There is no live execution mode, order-placement API, or market-write feature. `MarketClient` reads listings, sales, snapshots and fee configuration only.
- Marketplace adapters only construct GET requests and do not follow redirects. The generic HTTP executor separately rejects methods other than GET and HEAD, with a test that verifies rejected methods send no request.
- `run-mock` and `backtest` are offline by default. `--live-market-data` authorizes market **reads**, never real execution. `watch` reads public branch metadata and writes only to local SQLite.
- `KILL_SWITCH=1` or a local file named `KILL_SWITCH` blocks new mock orders. The switch latches for the running process.

## Run

From the workspace root:

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings

# Offline: print the bundled simulation report
cargo run --bin cs2-arb -- run-mock

# Offline: write reports/backtest.json and reports/backtest.md
cargo run --bin cs2-arb -- backtest
cargo run --bin cs2-arb -- backtest --snapshots prices.json --sales sales.json \
  --events updates.json --output-dir reports/custom

# Offline: replay the bundled Rust market scenario
cargo run --bin cs2-arb -- --config config/rust.toml run-mock
cargo run --bin cs2-arb -- --config config/rust.toml backtest --output-dir reports/rust

# Online: poll CS2 App ID 730 and persist events. Omit --once to poll until Ctrl-C.
cargo run --bin cs2-arb -- watch --database data/events.db --once

# Online reads only: collect a bounded batch for the fixture item universe, then simulate it
cargo run --bin cs2-arb -- run-mock --live-market-data \
  --platform csfloat --samples 5 --interval-seconds 120 --database data/events.db
```

The CS2 live reader consumes watcher events from the same SQLite database and records panic/monitoring transitions. Without `--platform` it reads Skinport and CSFloat; use `--platform dmarket` to opt in to signed DMarket reads. Live samples and their price history are in memory for that run; a short run cannot establish a 30-day price baseline. Missing or insufficient sales cause orders to be rejected rather than filled on assumed live liquidity. Historical input formats are arrays of `PriceSnapshot`, `Sale`, and `MarketEvent` as defined in `crates/core`; custom history is not given synthetic sales.

## Configuration and credentials

Use the global `--config <path>` option to override [`config/default.toml`](config/default.toml). All platforms have independent buyer and seller percentage fees (percentage points) and fixed fees (USD cents); all are **illustrative**, not current fee quotes. The included seller fees are Skinport 8%, CSFloat 2%, DMarket 7%; the illustrative buyer and fixed fees are zero. The simulator applies and rounds commissions per unit and reports total fees per fill.

The top-level `game` setting selects a single game per run. Omit it or use `game = "cs2"` to keep the existing CS2 defaults. [`config/rust.toml`](config/rust.toml) selects Rust, with a separate bundled `Alien Red` replay using Skinport and DMarket. Rust uses exact market hash names with `"wear_tier": "not_applicable"` and no float bucket or float value. Both profiles use the same seven-day **mock** hold; it is not a claim that every marketplace applies that hold to Rust skins.

With `--live-market-data`, Rust defaults to Skinport (`app_id=252490`). To include DMarket (`gameId=rust`), pass `--platform skinport --platform dmarket` and set the DMarket credentials below. CSFloat is not available in the Rust profile. Skinport's aggregate history does not satisfy the verified-sale liquidity requirement, so live mock orders on Skinport are rejected even when both markets are enabled. Steam Community Market prices are not included: Steam Wallet credit is not interchangeable with cash-market USD proceeds.

`watch` and its SQLite event queue remain CS2-only. Rust live reads do not consume CS2 events; a Rust historical replay can supply its own `--events` file if needed.

`[rate_limits]` configures quotas and optional sharing between paths. Skinport also has a local 8-request/300-second cap and five-minute cache per endpoint (optional on-disk cache in `SkinportConfig`). A 429 suppresses further admissions for at least the full configured window or the server's longer retry hint. 5xx and network failures use exponential backoff with full jitter (1 s base, 300 s cap), and every attempt is gated. `[events]`, `[spread]`, `[dip]`, and `[risk]` configure polling and panic windows, the strategies, and risk limits.

The application loads credentials from the process environment only. It does not log them, serialize them, load a dotenv file, or pass them to mock strategy/report code.

| Variable | Used for |
| --- | --- |
| `CSFLOAT_API_KEY` | Optional raw authorization key for read-only CSFloat data. |
| `DMARKET_PUBLIC_KEY`, `DMARKET_SECRET_KEY` | Required only for DMarket reads. Hex Ed25519 public key and hex seed or `seed || public` secret. |
| `SKINPORT_CLIENT_ID`, `SKINPORT_CLIENT_SECRET` | Recognized by the secrets helper, but **not used** for either public Skinport endpoint. |

## API verification and limits

“Checked” distinguishes a live observation from documentation or wrapper validation. Test servers exercise client behavior but do not validate real marketplace responses.

| Source | Checked | Unverified or limited |
| --- | --- | --- |
| Skinport | Cross-checked `/v1/items` and `/v1/sales/history`, auth-free reads, Brotli, response fields, five-minute cache and 8 requests per 5 minutes against `skinport.py`, `skinport.js` and `skinport-rs`. Official docs returned 403. | Shared quota and Doppler `version` coverage on `/v1/items` are unverified. Min asks are aggregate; sales history does not provide individual verified fills. |
| CSFloat | Cross-checked `/api/v1/listings`, `/api/v1/history/<encoded name>/sales`, integer cents, and the raw authorization header against `csfloat.py`, `csfloat_go` and `csfloat-client`. Official docs did not resolve during implementation. | `market_hash_name` listing filter, whether a key is required, history IDs, rate limits and hold data are unverified. Listings are filtered locally and an unknown trade lock is not populated. |
| DMarket | Official Trading Swagger and `dmarket/dm-trading-tools` agree on Ed25519 request signing and headers, `/marketplace-api/v2/offers`, and `/trade-aggregator/v1/last-sales` (up to 20 results per call). | Numeric rate limits and `unlockDate` semantics are unverified. No authenticated live requests were tested. |
| Rust | Public GETs returned the Rust Skinport catalogue and aggregate sales for `app_id=252490`; DMarket's public game catalogue identifies Rust as `rust`. Local test servers exercise both Rust adapters. | Rust DMarket offers and sales were not verified against the authenticated API. |
| Steam / CS2 | A live GET to `api.steamcmd.net/v1/info/730` on 2026-11-26 confirmed app changelist and public branch response shape; a captured SteamDB RSS sample confirmed public build IDs and dates. | Staging and prerelease were not visible in the captured response. Private branches may never appear in this feed. The `_changelist` heuristic is not a default panic trigger; proxy quota/uptime and SteamDB automation access are unverified. |

## Bundled fixture backtest

Actual output of `cs2-arb backtest` on the bundled synthetic scenario: **50 UTC days, 600 price snapshots, four item/exterior pairs, three marketplaces, three update events**. The bundled run models a separate tape of 10 settled mock sales per item/platform/day so it can exercise the strict liquidity check. These are simulated transactions, **not** real marketplace sales.

| Metric | Combined | Spread | Dip |
| --- | ---: | ---: | ---: |
| Closed trades | 2 | 1 | 1 |
| Gross realized P&L | $16.46 | $8.23 | $8.23 |
| Net realized and total P&L | $15.30 | $7.65 | $7.65 |
| Fees / fee drag (% of gross P&L) | $1.16 / 7.05% | $0.58 / 7.05% | $0.58 / 7.05% |
| Average / median trade duration | 7 days / 7 days | 7 days / 7 days | 7 days / 7 days |
| Max drawdown | $2.26 | $1.13 | $1.13 |
| Orders submitted / filled / rejected | 4 / 4 / 0 | 2 / 2 / 0 | 2 / 2 / 0 |

Both strategies selected the same synthetic AK-47 Redline dip in independent portfolios. The combined result is **not** a joint book, earnings forecast, or evidence of a live trading opportunity. Historical asks stand in for book prices, and market transfers after the hold are not separately simulated.
