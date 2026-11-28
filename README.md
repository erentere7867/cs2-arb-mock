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

Historical snapshots plus sales, or explicitly enabled read-only listing samples, feed a time-ordered replay. Spread compares identical items and float buckets across markets, net of both sides' fees and seven days of opportunity cost. Inventory is eligible for exit at its trade-lock expiry (seven days by default), provided the target venue can execute the mock exit; there is no additional 14-day wait. The risk manager checks exposure, UTC-day drawdown and the kill switch before a mock order reaches the simulator.

During an update window, dip compares the current quote with the mean (μ) and **sample** standard deviation (σ, denominator *n − 1*) of prior prices in the last 30 days. By default it requires at least 10 prior observations and a price at or below μ − 2σ, and rejects prices more than 30% below μ. Live batches seed price history with completed daily medians of verified public sales so the baseline does not depend on collecting weeks of fresh asks in one batch. Dip also requires at least 10 verified sales of that item on the quoting venue in the last seven days and at least 1.5× the sales rate of the preceding 30 days (normalized to seven days). A venue with no usable baseline or current sales cannot trigger: live CSFloat and DMarket can supply individual prints; Waxpeer has no sales tape in this adapter and Skinport's unverified aggregates do not qualify. Custom offline history must explicitly include a sales tape. The maximum-drop check is an entry guard, not an executable stop-loss during a lock.

The maximum entry drop is separate from the held-position stop-loss. After a dip lot unlocks, it is marked for liquidation when the median of fresh quotes for the same item and float bucket is **more than 25%** below its per-unit cost basis (including buy fees), or when the lot reaches **21 days** from purchase. Set `stop_loss_pct` and `max_hold_days` under `[dip]` to change these defaults. A marked lot waits for the best available mock exit after seller fees; no lock, liquidity, or execution check is bypassed.

The backtest library's snapshot-only `replay` helper has no sales tape: it warns and dip cannot open trades. To replay dip, pass a tape to `DipStrategy::with_sales` and run it with `Backtester`, or use the CLI's `backtest --sales` option for custom history.

The CLI mock engine checks the broad market and a separate index of items whose names end in ` Case` before placing or filling peg bids and before entering dip trades. Each index takes the median of per-item returns over 24 hours and seven days. Returns are computed within the same venue, float bucket, and source (ask snapshots or verified sales). The engine takes the median across sources within each bucket, across buckets within each venue, then across venues for the item. A venue with more observed float buckets gets no extra weight. The default thresholds are **−3% over 24 hours** or **−8% over seven days** on either available index. A broad index needs at least three comparable items per window; if either broad return is unavailable, the engine fails closed. A case return is used when at least two cases are comparable. Observations must be less than 24 hours old at each comparison point. The thresholds and coverage are configurable under `[market_regime]`; the latest batch's state and returns appear in the dashboard's peg summary.

Even in a risk-on market, the engine blocks peg placements, rejects matching fills, cancels resting peg bids, and blocks dip entries for items without a comparable 24-hour return or whose 24-hour return trails the broad index by more than `[market_regime].item_underperformance_pct` (default **5 percentage points**). A rising item can still be blocked: **+4%** trails a **+10%** index by six points, while **+5%** is exactly at the default limit and passes this check. Dip exits and peg relists continue. This is a mock-only safeguard, not a signal to trade with a real balance.

The mock peg reads CSFloat and Waxpeer buy books and targets one cent over the best bid. It stands down if the target exceeds the exit value × (1 − margin) or a tighter ceiling needed to retain at least 7% net return after venue fees and seven-day carry. The exit value comes from verified sales over the last seven days, because lots sell at what buyers pay, not at what sellers ask; an ask median can only lower it. A tier-only bid can be filled by any float in its wear tier, and sellers route their worst floats to it, so it is valued at the 25th percentile of the tier's prints (`tier_only_exit_quantile`). Waxpeer has no verified sales feed, so it places no peg bids unless `ask_only_exit_haircut_pct` explicitly values its exits from asks. Repricing defaults to a five-minute cooldown and a two-cent minimum change. Bids rest until a verified print on the same venue crosses the limit; at the limit, the simulator models queue delay. A fill debits mock cash including purchase fees and creates a seven-day locked lot. At expiry, the separate peg inventory relister posts at the rolling median ask (falling back to the median of eligible sale prints if there are too few asks), or one cent under the current market when that is lower. The current market is the newest lowest ask no older than six hours, else the median of the last 24 hours of verified sales. A listing fills on a verified print at or above its ask, or on trailing volume only while the current market is at or above the ask; with no fresh evidence it does not fill. After a day unfilled it reprices to one cent under the current market, even below cost, rather than resting above a falling market. The read-only dashboard on port 8787 shows the latest completed batch's active mock bids and locked inventory, including unlock times.

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

The CS2 live reader consumes watcher events from the same SQLite database and records panic/monitoring transitions. With the shipped config and no `--platform`, it reads Skinport, CSFloat, and Waxpeer and adds signed DMarket reads when both DMarket keys are set. Missing or insufficient sales cause orders to be rejected rather than filled on assumed live liquidity. Historical input formats are arrays of `PriceSnapshot`, `Sale`, and `MarketEvent` as defined in `crates/core`; custom history is not given synthetic sales.

Live mock batches for both games store quotes and verified individual sales in the SQLite database named by `--database`, keeping `[market_history] retention_days` (40 in the shipped profiles) of history per game. The next batch reads that history for its tracked items and enabled venues. Live batches use persisted history only for the regime gate; `backtest --history-db` replays it as executable books and fills (see below), and the dip baseline and liquidity checks still use the batch's existing inputs. Historical replays do not read or populate the live store.

Timestamped verified sales already fetched from CSFloat and DMarket can seed the long and short anchors without extra requests, but only when enough comparable sale history exists for the tracked items. Skinport aggregates and synthetic fixture sales do not seed the live store. A fresh database without comparable sales needs recurring quote samples reaching back to the seven-day anchor and its lookback tolerance (up to eight days); missing anchors still block entries. Look for quote and verified-sale counts and the oldest loaded observation in the dashboard's peg summary and batch logs. The oldest observation alone does not establish comparable anchor coverage.

## Backtest on recorded market data

Live batches store every quote, verified individual sale, buy-book poll and empty listing poll they read in the `--database` SQLite file (`[market_history] retention_days`, 40 in the shipped profiles). `backtest --history-db` replays that recording through the same spread, dip, peg-bid, relist, regime and risk logic, fees and seven-day hold as a live batch, using only what was observed: no synthetic sales or bids are added. The database is opened read-only, so it is safe to point at the live store.

```sh
cs2-arb --config config/default.toml backtest --history-db /home/ubuntu/cs2-arb-mock-data/events.db \
  --from 2026-11-27T00:00:00Z --through 2026-12-05T00:00:00Z --output-dir reports/recorded
```

`--from` and `--through` default to the oldest record and now; only that window is traded, while everything recorded before it warms up the regime gate and the dip baseline. CS2 replays also use the branch updates and panic windows the watcher acknowledged unless `--events` is given. Results are only as deep as the recording: exits need quotes at least seven days after entry, and buy-book polls are only stored by builds that include this feature.

## Fast validation on real history

`research/` tests the strategy rules without waiting weeks for live recordings: it downloads years of public CSFloat daily history, replays the dip and peg rules with the shipped fees, measures how much cross-venue spread survives real costs today, and exports the history for the engine's own `backtest`. See [research/README.md](research/README.md).

## Configuration and credentials

Use the global `--config <path>` option to override [`config/default.toml`](config/default.toml). The top-level `markets` list defines the allowlist. For example, set `markets = ["skinport", "waxpeer"]` in a CS2 config to use only those two venues. In live-data mode, `--platform` can narrow an explicit list, not expand it. The shipped profiles set `dmarket_if_configured = true`: when no `--platform` is given, the engine skips DMarket unless it is allowlisted and both keys form a valid pair. Remove `"dmarket"` from the allowlist to disable it. Historical replay does not apply the credentials check.

The shipped fees were researched on 2026-11-28 (sources are linked in `config/default.toml`): CSFloat 2% plus a 2.5% entry-tier bank withdrawal and 1.5% ACH deposit, Skinport 8%, DMarket 5% (2–10% per item) plus 2% withdrawal, Waxpeer 6% plus 1% deposit and 1.5% withdrawal. Funding legs are charged on every trade, a deliberately conservative real-money round trip. Unit tests use the frozen `data/fixtures/test_fees.toml` instead.

Venues have independent buyer/seller platform fees and optional funding costs. Set `crypto_deposit`, `crypto_withdrawal`, and `bank_cashout` under `[fees.<venue>.funding]`; each accepts `{ pct = 0.0, fixed = 0 }` (percentage points and USD cents). Percentage charges round per filled unit; fixed charges apply once per filled order. Deposits apply on entry, and withdrawal and cash-out on exit. Use zero for unused legs. These are estimates, not payment quotes or an external wallet simulation. Waxpeer uses the [6% standard seller fee](https://faq.waxpeer.com/faq/fees-sell), assumes no separate buyer fee when spending balance, and models [1% deposit for most coins](https://faq.waxpeer.com/faq/fees-deposit) and [1.5% standard crypto withdrawal](https://faq.waxpeer.com/faq/fees-withdraw). Blockchain network fees and your own bank's charges are not included; add them for the rails you actually use. Percentage fees round to whole cents, so items under about $1 can show a 0-cent fee where a venue would charge a minimum. Fee components appear in the JSON and Markdown for closed trades with funding enabled.

The top-level `game` setting selects a single game per run. Omit it or use `game = "cs2"` to keep the existing CS2 defaults. [`config/rust.toml`](config/rust.toml) selects Rust, with a separate bundled `Alien Red` replay using Skinport and DMarket. Rust uses exact market hash names with `"wear_tier": "not_applicable"` and no float bucket or float value. Both profiles use the same seven-day **mock** hold; it is not a claim that every marketplace applies that hold to Rust skins.

The Rust profile allows a one-item index for the default `Alien Red` universe. With one item, the broad trend still checks that item's returns and fails closed without enough history, but the relative gate adds no protection because the item is compared with itself.

The Rust profile reads Skinport (`app_id=252490`) and Waxpeer (`game=rust`) by default, plus signed DMarket reads (`gameId=rust`) when both keys are set. CSFloat is not available in the Rust profile. Neither Skinport's aggregate history nor Waxpeer's public asks and top bids satisfy the verified-sale liquidity requirement; recorded sales can be supplied for historical simulations. The bundled Rust replay uses synthetic data. Steam Community Market prices are not included: Steam Wallet credit is not interchangeable with cash-market USD proceeds.

Spread candidates are ranked by expected net P&L after fees, hold cost and configured price drift; verification and depth checks are separate. A candidate must clear the larger of `min_net_profit` and a percentage hurdle on its buy cost: `min_net_margin_pct` plus `risk_sigma_multiple` × `daily_volatility_pct` × √(hold days), which prices the risk that the exit moves during the lock. The planned spread exit is not a guaranteed fill. With `[relist] cross_venue = true` (default in the Rust config), mock relisting considers other venues with sufficient observed sales and picks the highest net median-price proceeds. Without it, relisting stays on the venue of purchase. Peg bids still use a same-venue median to estimate their margin.

Opt in to per-venue mock balances, sale holds, explicit transfers and finite-depth instant relisting in the config. See the [platform audit and example configuration](docs/platform-audit.md) for verified API contracts, UNVERIFIED assumptions, fee treatment and simulation limits.

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
| Closed trades | 1 | 1 | 0 |
| Gross realized P&L | $8.23 | $8.23 | $0.00 |
| Net realized and total P&L | $6.92 | $6.92 | $0.00 |
| Fees / fee drag (% of gross P&L) | $1.31 / 15.92% | $1.31 / 15.92% | $0.00 / n/a |
| Average / median trade duration | 7 days / 7 days | 7 days / 7 days | n/a |
| Max drawdown | $1.10 | $1.10 | $0.00 |
| Orders submitted / filled / rejected | 2 / 2 / 0 | 2 / 2 / 0 | 0 / 0 / 0 |

Fees include the shipped cash-out legs, so the same trade nets less than with the earlier illustrative schedules. Spread selected the synthetic AK-47 Redline; the dip strategy stood down because the uniform mock tape shows no relative volume surge. The combined result is **not** a joint book, earnings forecast, or evidence of a live trading opportunity. Historical asks stand in for book prices, and market transfers after the hold are not separately simulated.
