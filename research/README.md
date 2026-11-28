# Fast strategy validation

Live mock collection needs five or more weeks of recorded data before
`backtest --history-db` can say anything. These tools test the strategy rules
**today**, on years of real public price history and on the live cross-venue
book, using the same fees as the engine (`config/default.toml`).

Everything here only issues GET requests to public endpoints, at or below the
engine's own pacing. Downloads go to `research/data/` (git-ignored); reports go
to `reports/` (git-ignored). Python 3.11+ standard library only; the Skinport
scan also needs `curl`, because Skinport requires Brotli.

```sh
# 1. Download ~3.4 years of daily CSFloat history plus the latest 40 prints per item
#    (one request per 10 s, resumable; about 12 minutes for the bundled universe).
python3 research/fetch_csfloat_history.py

# 2. Volatility, dip and peg-bid economics on that history (seconds).
python3 research/validate.py --years 2

# 3. Today's Skinport <-> Waxpeer spreads after real fees and cash-out.
python3 research/spread_scan.py
#    Run it again an hour later against the first result to see what persists.
python3 research/spread_scan.py --out research/data/spread-later --previous research/data/spread/spread_scan.json

# 4. Run the Rust engine itself over the same history (dip, regime, risk, exits).
#    The exporter also writes replay.toml: the shipped config narrowed to CSFloat.
python3 research/export_replay.py
cargo run --release --bin cs2-arb -- --config research/data/replay/replay.toml backtest \
  --snapshots research/data/replay/snapshots.json --sales research/data/replay/sales.json \
  --events research/data/replay/events.json --output-dir reports/replay
```

## What each step measures

| Script | Question | Real data | Reconstructed or assumed |
| --- | --- | --- | --- |
| `validate.py` §1 | How far can a price move during the 7-day lock? Calibrates `[spread] daily_volatility_pct`. | Daily CSFloat averages | Daily averages include float and sticker premiums (noise). |
| `validate.py` §2 | Does the dip rule beat simply buying every day? Variants with the regime gate and a market-stress proxy. | Prices and daily sale counts | Historical update windows are unavailable; market stress stands in for them. |
| `validate.py` §3 | What does a peg bid at the margin ceiling earn, given who sells into it? | Dispersion of the latest 40 prints around CSFloat's float-adjusted price | That dispersion is assumed stable; a lot sells on the first day the lock allows. |
| `spread_scan.py` | How much cross-venue spread exists after fees, at ask prices and at actual sale prices? | Whole live catalogue of both venues | Aggregate prices across floats and stickers, so survivors are upper bounds. |
| `export_replay.py` + engine | Does the engine reproduce the harness on the same paths? | Prices, daily sale counts | Individual prints are evenly spaced at the daily average (`recon-` IDs). |

## Reading the results

Each number is the rule's expected economics on real price paths. None of them
forecasts profits. A strategy has an edge only if it beats its control (dip),
or clears the 7-day volatility (spread, peg) after every fee, on enough trades
to rule out luck. When live batches have recorded a few weeks, confirm with
`backtest --history-db`, which replays actual quotes, prints and buy books.
