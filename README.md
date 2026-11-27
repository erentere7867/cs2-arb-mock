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

For unattended Linux operation and reading live mock results, see the [VPS operations guide](deploy/README.md).

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
cargo run --bin cs2-arb -- --config config/rust.toml backtest

# Online: poll CS2 App ID 730 and persist events. Omit --once to poll until Ctrl-C.
cargo run --bin cs2-arb -- watch --database data/events.db --once

# Online reads only: collect a bounded batch for the fixture item universe, then simulate it
cargo run --bin cs2-arb -- run-mock --live-market-data \
  --platform csfloat --samples 5 --interval-seconds 120 --database data/events.db
```

The CS2 live reader consumes watcher events from the same SQLite database and records panic/monitoring transitions. With the shipped config and no `--platform`, it reads Skinport and CSFloat and adds signed DMarket reads when both DMarket keys are set. Live samples and their price history are in memory for that run; a short run cannot establish a 30-day price baseline. Missing or insufficient sales cause orders to be rejected rather than filled on assumed live liquidity. Historical input formats are arrays of `PriceSnapshot`, `Sale`, and `MarketEvent` as defined in `crates/core`; custom history is not given synthetic sales.

## Configuration and credentials

Use the global `--config <path>` option to override [`config/default.toml`](config/default.toml). The top-level `markets` list defines the allowlist. For example, set `markets = ["skinport", "waxpeer"]` in a CS2 config to enable Waxpeer. In live-data mode, `--platform` can narrow an explicit list, not expand it. The shipped profiles set `dmarket_if_configured = true`: when no `--platform` is given, the engine skips DMarket unless it is allowlisted and both keys form a valid pair. Remove `"dmarket"` from the allowlist to disable it. Historical replay does not apply the credentials check.

Venues have independent buyer/seller platform fees and optional funding costs. Set `crypto_deposit`, `crypto_withdrawal`, and `bank_cashout` under `[fees.<venue>.funding]`; each accepts `{ pct = 0.0, fixed = 0 }` (percentage points and USD cents). Percentage charges round per filled unit; fixed charges apply once per filled order. Deposits apply on entry, and withdrawal and cash-out on exit. Use zero for unused legs. These are estimates, not payment quotes or an external wallet simulation. The legacy market fees are illustrative. Waxpeer uses the [6% standard seller fee](https://faq.waxpeer.com/faq/fees-sell), assumes no separate buyer fee when spending balance, and models [1% deposit for most coins](https://faq.waxpeer.com/faq/fees-deposit) and [1.5% standard crypto withdrawal](https://faq.waxpeer.com/faq/fees-withdraw). Network and bank costs are assumed zero until supplied; set them before relying on the simulated margins. Fee components appear in the JSON and Markdown for closed trades with funding enabled.

The top-level `game` setting selects a single game per run. Omit it or use `game = "cs2"` to keep the existing CS2 defaults. [`config/rust.toml`](config/rust.toml) selects Rust, with a separate bundled `Alien Red` replay using Skinport and DMarket. Rust uses exact market hash names with `"wear_tier": "not_applicable"` and no float bucket or float value. Both profiles use the same seven-day **mock** hold; it is not a claim that every marketplace applies that hold to Rust skins.

The Rust profile reads Skinport (`app_id=252490`) and Waxpeer (`game=rust`) by default, plus signed DMarket reads (`gameId=rust`) when both keys are set. CSFloat is not available in the Rust profile. Neither Skinport's aggregate history nor Waxpeer's public asks and top bids satisfy the verified-sale liquidity requirement; recorded sales can be supplied for historical simulations. The bundled Rust replay uses synthetic data. Steam Community Market prices are not included: Steam Wallet credit is not interchangeable with cash-market USD proceeds.

Spread candidates are ranked by expected net P&L after fees, hold cost and configured price drift; verification and depth checks are separate. The planned spread exit is not a guaranteed fill. With `[relist] cross_venue = true` (default in the Rust config), mock relisting considers other venues with sufficient observed sales and picks the highest net median-price proceeds. Without it, relisting stays on the venue of purchase. Peg bids still use a same-venue median to estimate their margin.

`backtest` writes to `reports/` for CS2 and `reports/rust/` for Rust by default. Use `--output-dir` to override either.

`watch` and its SQLite event queue remain CS2-only. Rust live reads do not consume CS2 events; a Rust historical replay can supply its own `--events` file if needed. Explicit panic-window records do not carry an app ID, so supply records for the selected game.

`[rate_limits]` configures quotas and sharing between paths. The bundled profiles use these limits for read-only data:

| Market | Local pacing | Published guidance |
| --- | --- | --- |
| CSFloat | 1 request per 10 seconds, shared across routes | [API reference](https://docs.csfloat.com/) does not publish a numeric quota. |
| DMarket | 1 request per second, shared across routes | [Signed requests](https://support.dmarket.com/hc/en-us/articles/25269064897297-What-are-the-rate-limits-for-requests): 10/s for market items and 6/s for last sales. |
| Skinport | 6 requests per 330 seconds, shared across both routes; five-minute cache per route | [8 per five minutes](https://docs.skinport.com/introduction/rate-limits) shared by items and sales history; 1,000/hour globally. |
| Waxpeer | 1 request per 6 seconds across both routes; ten-second cache per route | [Prices](https://docs.waxpeer.com/api/buy-items/prices-snapshot) and [buy orders](https://docs.waxpeer.com/api/buy-order/buy-orders-snapshot) snapshots refresh about every ten seconds; no numeric JSON snapshot quota is published. |

On HTTP 429 the client stops sending to that market for the server's `Retry-After` or the longer of the available body and reset hints, defaulting to 60 seconds without a usable hint. Skinport always gets at least five minutes of silence. Configured per-route penalties can last longer. When a successful response reports zero remaining quota, the next read waits for a reset of up to three seconds; for a longer reset or a 429, reads are skipped during the cooldown. Pacing and cooldowns are per running client; other processes or hosts sharing a provider quota can still consume it. 5xx and network failures use exponential backoff with full jitter (1 s base, 300 s cap), and every attempt is gated. `[events]`, `[spread]`, `[dip]`, and `[risk]` configure polling and panic windows, the strategies, and risk limits.

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
