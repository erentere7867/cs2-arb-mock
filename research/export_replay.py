#!/usr/bin/env python3
"""Turn CSFloat daily history into inputs for the Rust engine's own backtest.

validate.py mirrors the engine's rules in Python. This script lets the engine itself
(`cs2-arb backtest --snapshots --sales --events`) run the same years of real prices, so
the real dip, regime, risk and exit code is exercised and the two can be cross-checked.

What is real and what is reconstructed:

* snapshots: one CSFloat quote per item per day at 12:00 UTC, priced at that day's
  average sale price (a price path, not a historical lowest ask).
* sales: the day's real sale *count*, reconstructed as evenly spaced prints at the daily
  average (capped per day to keep files small). The engine's liquidity and volume-surge
  gates see real volume, but individual print prices are not historical. Their IDs start
  with "recon-" so they can never be mistaken for recorded sales.
* events: panic windows proxied from market stress, because historical CS2 update
  windows are not available: a window opens on any day the median item fell more than
  `--stress-1d` in a day or `--stress-7d` in a week, and lasts `[events]
  window_duration_secs` (four days in the shipped config).

Only wear-tiered skins whose daily average is at least `--min-price` cents are exported:
the engine's CS2 model rejects items without a wear tier (cases), and cheaper items round
percentage fees to zero cents and would show fee-free phantom profits. Cases still feed
the market-stress index.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import math
import pathlib
import re
import statistics
import sys
import zlib

from feeslib import ROOT, load_section

WEAR = {
    "Factory New": "factory_new",
    "Minimal Wear": "minimal_wear",
    "Field-Tested": "field_tested",
    "Well-Worn": "well_worn",
    "Battle-Scarred": "battle_scarred",
}
DAY = dt.timedelta(days=1)


def wear_tier(name: str) -> str:
    match = re.search(r"\(([^)]+)\)\s*$", name)
    return WEAR.get(match.group(1), "not_applicable") if match else "not_applicable"


def iso(moment: dt.datetime) -> str:
    return moment.strftime("%Y-%m-%dT%H:%M:%SZ")


def write_replay_config(path: pathlib.Path) -> None:
    """The shipped config, narrowed to CSFloat (the only venue with this history) and the
    robust dip baseline. Derived on every run so it never drifts from config/default.toml."""
    source = (ROOT / "config" / "default.toml").read_text(encoding="utf-8")
    markets = re.compile(r"^markets = .*$", re.MULTILINE)
    if not markets.search(source) or "\n[dip]\n" not in source:
        raise SystemExit("config/default.toml no longer has a markets line or [dip] table")
    config = markets.sub('markets = ["csfloat"] # research replay: CSFloat history only', source, count=1)
    if "baseline =" not in config.split("\n[dip]\n", 1)[1].split("\n[", 1)[0]:
        config = config.replace("\n[dip]\n", '\n[dip]\nbaseline = "robust_log"\n', 1)
    path.write_text(config, encoding="utf-8")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--data", type=pathlib.Path, default=pathlib.Path(__file__).with_name("data") / "csfloat")
    parser.add_argument("--out", type=pathlib.Path, default=pathlib.Path(__file__).with_name("data") / "replay")
    parser.add_argument("--years", type=float, default=2.0)
    parser.add_argument("--min-price", type=int, default=100)
    parser.add_argument("--max-sales-per-day", type=int, default=150)
    parser.add_argument("--stress-1d", type=float, default=3.0, help="percent fall of the median item in a day")
    parser.add_argument("--stress-7d", type=float, default=8.0, help="percent fall of the median item in a week")
    args = parser.parse_args()

    histories = {}
    for path in sorted(args.data.glob("*.graph.json")):
        graph = json.loads(path.read_text(encoding="utf-8"))
        days = {
            dt.date.fromisoformat(row["day"][:10]): (float(row["avg_price"]), int(row["count"]))
            for row in graph["data"]
            if row.get("avg_price") and row.get("count")
        }
        histories[graph["market_hash_name"]] = days
    if not histories:
        print(f"no history under {args.data}", file=sys.stderr)
        return 1
    latest = max(d for days in histories.values() for d in days)
    # Export the test window plus the 37 days of warm-up the dip baseline and volume need.
    since = latest - dt.timedelta(days=int(args.years * 365) + 37)

    snapshots, sales = [], []
    for name, days in histories.items():
        if wear_tier(name) == "not_applicable":
            continue  # The engine's CS2 model trades wear-tiered skins; cases are rejected.
        item = {"market_hash_name": name, "wear_tier": wear_tier(name)}
        # Stable across runs (str hash() is salted per process), so IDs are reproducible.
        item_key = zlib.crc32(name.encode("utf-8"))
        for day in sorted(d for d in days if d >= since):
            avg, count = days[day]
            if avg < args.min_price:
                continue
            noon = dt.datetime.combine(day, dt.time(12), dt.timezone.utc)
            price = int(avg)
            snapshots.append(
                {"platform": "csfloat", "item": item, "float_bucket": None, "price": price, "observed_at": iso(noon)}
            )
            n = min(count, args.max_sales_per_day)
            for i in range(n):
                # Spread prints across the day, all strictly before the next day's quote.
                at = dt.datetime.combine(day, dt.time(0), dt.timezone.utc) + dt.timedelta(seconds=(i + 0.5) * 86_400 / n)
                sales.append({
                    "id": f"recon-{item_key:08x}-{day.isoformat()}-{i}",
                    "platform": "csfloat",
                    "item": item,
                    "price": price,
                    "float_value": None,
                    "sold_at": iso(at),
                    "verified": True,
                })

    # Market stress proxy for update panics, from the median item return.
    window = dt.timedelta(seconds=int(load_section("events").get("window_duration_secs", 345_600)))
    events, open_until = [], None
    for day in sorted({d for days in histories.values() for d in days if d >= since}):
        one = [math.log(h[day][0] / h[day - DAY][0]) for h in histories.values() if day in h and day - DAY in h]
        week = [math.log(h[day][0] / h[day - 7 * DAY][0]) for h in histories.values() if day in h and day - 7 * DAY in h]
        stressed = (len(one) >= 3 and statistics.median(one) <= math.log(1 - args.stress_1d / 100)) or (
            len(week) >= 3 and statistics.median(week) <= math.log(1 - args.stress_7d / 100)
        )
        start = dt.datetime.combine(day, dt.time(0), dt.timezone.utc)
        if stressed and (open_until is None or start >= open_until):
            if open_until is not None:
                events.append({"kind": "panic_window_closed", "branch": "market-stress", "closed_at": iso(open_until)})
            events.append({"kind": "panic_window_opened", "branch": "market-stress", "opened_at": iso(start)})
            open_until = start + window
        elif stressed:
            open_until = max(open_until, start + window)  # Extend an open window.
    if open_until is not None:
        events.append({"kind": "panic_window_closed", "branch": "market-stress", "closed_at": iso(open_until)})

    args.out.mkdir(parents=True, exist_ok=True)
    write_replay_config(args.out / "replay.toml")
    snapshots.sort(key=lambda s: s["observed_at"])
    sales.sort(key=lambda s: s["sold_at"])
    for name, rows in (("snapshots", snapshots), ("sales", sales), ("events", events)):
        (args.out / f"{name}.json").write_text(json.dumps(rows), encoding="utf-8")
    items = len({s["item"]["market_hash_name"] for s in snapshots})
    windows = sum(e["kind"] == "panic_window_opened" for e in events)
    print(f"exported {items} items, {len(snapshots)} snapshots, {len(sales)} reconstructed sales, "
          f"{windows} market-stress windows to {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
