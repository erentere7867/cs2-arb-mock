#!/usr/bin/env python3
"""Measure cross-venue spreads available right now, after the engine's real fees.

Three public, read-only GETs cover every CS2 item on two venues:

* Skinport ``/v1/items``: lowest ask and listing count per item.
* Skinport ``/v1/sales/history``: 7-day median sale price and volume per item.
* Waxpeer ``/v1/prices``: lowest ask and listing count per item (milli-USD).

Two exits are compared for each item listed on both venues:

* ``ask`` exit: sell at the other venue's current lowest ask minus one cent. This is
  what the engine's spread strategy assumes, and it is optimistic: it treats the
  cheapest competing listing as a price buyers will pay us seven days from now.
* ``sold`` exit (Skinport only): sell at Skinport's 7-day median *sale* price, which is
  what buyers actually paid there recently.

Net profit charges the buy venue's all-in cost and the sell venue's seller fee and
cash-out legs from config/default.toml (see research/feeslib.py), plus the hold-period
hurdle from [spread] when it is configured. Skinport and Waxpeer prices are aggregates
across floats, patterns and stickers, so any surviving "opportunity" is an upper bound
that still needs a like-for-like listing check.
"""

from __future__ import annotations

import argparse
import gzip
import json
import math
import pathlib
import statistics
import subprocess
import sys
import urllib.error
import urllib.request

from feeslib import load_section, load_venues

USER_AGENT = "cs2-arb-mock-research/1.0"
SKINPORT_ITEMS = "https://api.skinport.com/v1/items?app_id=730&currency=USD&tradable=1"
SKINPORT_SALES = "https://api.skinport.com/v1/sales/history?app_id=730&currency=USD"
WAXPEER_PRICES = "https://api.waxpeer.com/v1/prices?game=csgo&min_price=0&max_price=100000000"


def get_json(url: str) -> object:
    request = urllib.request.Request(
        url, headers={"User-Agent": USER_AGENT, "Accept": "application/json", "Accept-Encoding": "gzip"}
    )
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            body = response.read()
            if response.headers.get("Content-Encoding") == "gzip":
                body = gzip.decompress(body)
            return json.loads(body)
    except urllib.error.HTTPError as error:
        if error.code != 406:
            raise
    # Skinport answers 406 unless the client accepts Brotli, which the standard library
    # cannot decode; curl's --compressed negotiates and decodes it.
    body = subprocess.run(
        ["curl", "-sS", "--fail", "--compressed", "-m", "60", "-A", USER_AGENT, url],
        check=True,
        capture_output=True,
    ).stdout
    return json.loads(body)


def cached(path: pathlib.Path, url: str) -> object:
    """Reuse a download from the same session; Skinport caches for five minutes anyway."""
    if path.exists():
        return json.loads(path.read_text(encoding="utf-8"))
    data = get_json(url)
    path.write_text(json.dumps(data), encoding="utf-8")
    return data


def dollars_to_cents(value: float | None) -> int | None:
    return None if value is None else int(round(value * 100))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--out", type=pathlib.Path, default=pathlib.Path(__file__).with_name("data") / "spread")
    parser.add_argument("--min-price", type=int, default=100, help="ignore items cheaper than this many cents")
    parser.add_argument("--min-listings", type=int, default=5, help="listings required on each venue")
    parser.add_argument("--min-weekly-sales", type=int, default=10, help="Skinport 7-day sales for the sold exit")
    parser.add_argument(
        "--previous",
        type=pathlib.Path,
        help="spread_scan.json from an earlier run; reports how many confirmed pairs persisted",
    )
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)

    venues = load_venues()
    spread_cfg = load_section("spread")
    hold_days = float(spread_cfg.get("hold_days", 7))
    hurdle_pct = float(spread_cfg.get("min_net_margin_pct", 0.0)) + float(
        spread_cfg.get("risk_sigma_multiple", 0.0)
    ) * float(spread_cfg.get("daily_volatility_pct", 0.0)) * math.sqrt(hold_days)
    carry = float(spread_cfg.get("annual_opportunity_rate_pct", 10.0)) / 100.0 * hold_days / 365.0

    sp_items = cached(args.out / "skinport_items.json", SKINPORT_ITEMS)
    sp_sales = cached(args.out / "skinport_sales.json", SKINPORT_SALES)
    wax = cached(args.out / "waxpeer_prices.json", WAXPEER_PRICES)

    skinport = {}
    for row in sp_items:
        if row.get("version"):
            continue  # Phase-specific Doppler rows; the engine prefers the unversioned row.
        ask = dollars_to_cents(row.get("min_price"))
        if ask:
            skinport[row["market_hash_name"]] = (ask, int(row.get("quantity") or 0))
    sold = {}
    for row in sp_sales:
        week = row.get("last_7_days") or {}
        median = dollars_to_cents(week.get("median"))
        if median and not row.get("version"):
            sold[row["market_hash_name"]] = (median, int(week.get("volume") or 0))
    waxpeer = {}
    for row in wax.get("items", []):
        minor = row.get("min")
        if minor:
            # Milli-USD rounded up to a whole cent for asks, as the engine's adapter does.
            waxpeer[row["name"]] = (minor // 10 + (1 if minor % 10 else 0), int(row.get("count") or 0))

    rows = []
    for name in sorted(skinport.keys() & waxpeer.keys()):
        sp_ask, sp_qty = skinport[name]
        wx_ask, wx_qty = waxpeer[name]
        if min(sp_ask, wx_ask) < args.min_price or min(sp_qty, wx_qty) < args.min_listings:
            continue
        for buy_venue, buy_ask, sell_venue, sell_ask in (
            ("skinport", sp_ask, "waxpeer", wx_ask),
            ("waxpeer", wx_ask, "skinport", sp_ask),
        ):
            cost = venues[buy_venue].buy_cost(buy_ask)
            required = cost * (carry + hurdle_pct / 100.0)
            ask_net = venues[sell_venue].sell_proceeds(sell_ask - 1) - cost
            row = {
                "item": name,
                "buy": buy_venue,
                "sell": sell_venue,
                "buy_ask": buy_ask,
                "sell_ask": sell_ask,
                "ask_exit_net": ask_net,
                "ask_exit_pct": ask_net / cost * 100.0,
                "clears_hurdle_on_ask": ask_net > required,
            }
            if sell_venue == "skinport" and name in sold and sold[name][1] >= args.min_weekly_sales:
                sold_net = venues["skinport"].sell_proceeds(sold[name][0]) - cost
                row.update(
                    sold_median=sold[name][0],
                    sold_exit_net=sold_net,
                    sold_exit_pct=sold_net / cost * 100.0,
                    clears_hurdle_on_sold=sold_net > required,
                )
            rows.append(row)

    (args.out / "spread_scan.json").write_text(json.dumps(rows, indent=1), encoding="utf-8")
    pairs = len(rows)
    ask_positive = [r for r in rows if r["ask_exit_net"] > 0]
    ask_hurdle = [r for r in rows if r["clears_hurdle_on_ask"]]
    with_sold = [r for r in rows if "sold_exit_net" in r]
    print(f"items on both venues after filters: {pairs // 2}  (directed pairs: {pairs})")
    print(f"round-trip cost of one venue, buy+resell: "
          + ", ".join(f"{v}: {venues[v].round_trip_pct():.2f}%" for v in ("skinport", "waxpeer", "csfloat", "dmarket")))
    print(f"hold hurdle charged: carry {carry * 100:.2f}% + risk/margin {hurdle_pct:.2f}%")
    print(f"ask exit  > 0:      {len(ask_positive):5d} pairs ({len(ask_positive) / max(pairs, 1) * 100:.1f}%)")
    print(f"ask exit  > hurdle: {len(ask_hurdle):5d} pairs ({len(ask_hurdle) / max(pairs, 1) * 100:.1f}%)")
    if ask_positive:
        print(f"  median ask-exit margin of positive pairs: {statistics.median(r['ask_exit_pct'] for r in ask_positive):.2f}%")
    if with_sold:
        sold_hurdle = [r for r in with_sold if r["clears_hurdle_on_sold"]]
        confirmed = [r for r in with_sold if r["clears_hurdle_on_ask"] and r["clears_hurdle_on_sold"]]
        print(f"waxpeer->skinport pairs with >= {args.min_weekly_sales} weekly Skinport sales: {len(with_sold)}")
        print(f"  clear the hurdle at the 7-day median SALE price: {len(sold_hurdle)}")
        print(f"  clear it on both the ask and the sale price:     {len(confirmed)}")
        for r in sorted(confirmed, key=lambda r: -r["sold_exit_pct"])[:15]:
            print(f"    {r['item'][:58]:58} buy {r['buy_ask'] / 100:8.2f} ask-exit {r['ask_exit_pct']:6.2f}% sold-exit {r['sold_exit_pct']:6.2f}%")
        # Capacity: the simulator caps an order at 10% of trailing average daily volume,
        # so a pair supports floor(weekly_sales / 70) units per day.
        units = {r["item"]: sold[r["item"]][1] // 70 for r in confirmed}
        per_day = sum(r["sold_exit_net"] * units[r["item"]] for r in confirmed)
        print(f"  units/day allowed by the 10%-of-volume cap: {sum(units.values())}; "
              f"expected net if every one filled at the 7-day median: ${per_day / 100:.2f}/day")
        if args.previous:
            before = json.loads(args.previous.read_text(encoding="utf-8"))
            earlier = {(r["item"], r["buy"]) for r in before if r.get("clears_hurdle_on_ask") and r.get("clears_hurdle_on_sold")}
            now = {(r["item"], r["buy"]) for r in confirmed}
            kept = earlier & now
            print(f"  persistence vs {args.previous}: {len(kept)} of {len(earlier)} earlier confirmed pairs still confirmed "
                  f"({len(kept) / max(len(earlier), 1) * 100:.0f}%), {len(now - earlier)} new")
    return 0


if __name__ == "__main__":
    sys.exit(main())
