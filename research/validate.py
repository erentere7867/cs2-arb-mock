#!/usr/bin/env python3
"""Test the engine's strategy rules on years of real CSFloat price history in minutes.

Live mock collection needs five or more weeks before ``backtest --history-db`` has enough
depth. This harness replays the same rules on the daily history that CSFloat already
publishes (see fetch_csfloat_history.py), charging the fees in config/default.toml.

It answers four questions:

1. Volatility: how far does a skin move over the seven-day trade lock? This calibrates
   ``[spread] daily_volatility_pct`` and shows what any edge must beat.
2. Dip: do prices that fall 2 sigma below their 30-day robust baseline (the engine's
   ``robust_log`` rule, with its volume-surge filter) recover after the lock? Signals are
   compared with a control that enters every day, so a "win rate" that the market
   produced on its own is not mistaken for an edge. Variants add the regime gate and a
   market-stress proxy for update panics (historical update windows are not available).
3. Peg bid: bidding at the engine's margin ceiling on CSFloat, what do fills earn after
   the lock? Fills are driven by the real dispersion of individual prints around
   CSFloat's float-adjusted reference price, so the bid is filled mostly by cheap prints,
   and a cheap print is often a cheap *float* that resells cheaply. That is the adverse
   selection the old exit model ignored.
4. Capacity: how many trades a month the rules find per item.

Limits, stated so the numbers are read correctly: daily averages include float and
sticker premiums (noise that inflates daily volatility; the 7-day measure is less
affected); single-print dispersion comes from the most recent 40 sales and is assumed
stable over time; exits assume the lot sells on the first day the rule allows, at that
day's market for its quality. Read the results as the rules' expected economics on real
price paths, not as a forecast.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import math
import pathlib
import statistics
import sys
from dataclasses import dataclass

from feeslib import Venue, load_section, load_venues

MAD_TO_SIGMA = 1.4826
DAY = dt.timedelta(days=1)


@dataclass
class Item:
    name: str
    days: dict[dt.date, tuple[float, int]]  # date -> (average price in cents, sales count)
    # Per recent sale: (price / median price, predicted / median predicted). The first is
    # where a print landed relative to the typical print; the second is the unit's own
    # float/sticker quality relative to a typical unit, from CSFloat's predicted price.
    prints: list[tuple[float, float]]
    level: float  # typical transaction price per unit of daily average (avg is outlier-heavy)

    @property
    def is_case(self) -> bool:
        return self.name.endswith(" Case")


def robust(values: list[float]) -> tuple[float, float]:
    """Median and scaled MAD."""
    center = statistics.median(values)
    return center, MAD_TO_SIGMA * statistics.median(abs(v - center) for v in values)


def quantile(values: list[float], q: float) -> float:
    """Lower nearest-rank quantile, matching the engine's quantile_cents."""
    ordered = sorted(values)
    return ordered[min(int((len(ordered) - 1) * q), len(ordered) - 1)]


def load(data_dir: pathlib.Path) -> list[Item]:
    items = []
    for graph_path in sorted(data_dir.glob("*.graph.json")):
        graph = json.loads(graph_path.read_text(encoding="utf-8"))
        days = {}
        for row in graph["data"]:
            if row.get("avg_price") and row.get("count"):
                days[dt.date.fromisoformat(row["day"][:10])] = (float(row["avg_price"]), int(row["count"]))
        sales_path = graph_path.with_name(graph_path.name.replace(".graph.", ".sales."))
        raw = []
        if sales_path.exists():
            for sale in json.loads(sales_path.read_text(encoding="utf-8"))["data"]:
                predicted = (sale.get("reference") or {}).get("predicted_price")
                if sale.get("price") and predicted:
                    raw.append((float(sale["price"]), float(predicted)))
        prints, level = [], 1.0
        if raw and days:
            typical_price = statistics.median(p for p, _ in raw)
            typical_quality = statistics.median(q for _, q in raw)
            prints = [(p / typical_price, q / typical_quality) for p, q in raw]
            # Daily averages include sticker crafts and rare floats; scale them to the
            # median print so entry and exit prices are typical transaction prices.
            recent = [days[d][0] for d in sorted(days)[-3:]]
            level = typical_price / statistics.fmean(recent)
        items.append(Item(graph["market_hash_name"], days, prints, level))
    return items


def series(item: Item, since: dt.date) -> list[tuple[dt.date, float, int]]:
    return [(d, *item.days[d]) for d in sorted(item.days) if d >= since]


# --------------------------------------------------------------------------- volatility
def noise_split(r1: list[float], r7: list[float]) -> tuple[float, float]:
    """Split daily-average volatility into a random-walk part and day-specific noise.

    If log price = random walk (daily sigma s) + independent noise per day (eta), then
    Var(1-day return) = s^2 + 2 eta^2 and Var(7-day return) = 7 s^2 + 2 eta^2. Robust
    variances of both returns give s and eta. Noise here is the mix of floats and sticker
    crafts that sold that day: it reverts by construction, so a rule that buys low-average
    days shows profits that no trader could capture when eta is not small next to s.
    """
    v1, v7 = robust(r1)[1] ** 2, robust(r7)[1] ** 2
    walk = max((v7 - v1) / 6, 0.0)
    return math.sqrt(walk), math.sqrt(max((v1 - walk) / 2, 0.0))


def volatility(items: list[Item], since: dt.date) -> dict[str, dict[str, float]]:
    out = {}
    for item in items:
        rows = series(item, since)
        by_day = {d: p for d, p, _ in rows}
        r1 = [math.log(by_day[d] / by_day[d - DAY]) for d in by_day if d - DAY in by_day]
        r7 = [math.log(by_day[d] / by_day[d - 7 * DAY]) for d in by_day if d - 7 * DAY in by_day]
        if len(r1) < 30 or len(r7) < 30:
            continue
        walk, noise = noise_split(r1, r7)
        out[item.name] = {
            "true_daily_pct": walk * 100,
            "noise_pct": noise * 100,
            "daily_robust_pct": robust(r1)[1] * 100,
            "weekly_robust_pct": robust(r7)[1] * 100,
            "weekly_std_pct": statistics.stdev(r7) * 100,
            "weekly_p05_pct": (math.exp(quantile(r7, 0.05)) - 1) * 100,
            "price_usd": rows[-1][1] / 100,
        }
    return out


# ------------------------------------------------------------------------ market index
def market_index(items: list[Item]) -> dict[dt.date, tuple[float | None, float | None]]:
    """Median 1-day and 7-day log return across items, like the engine's broad index."""
    dates = sorted({d for item in items for d in item.days})
    index = {}
    for d in dates:
        one = [math.log(i.days[d][0] / i.days[d - DAY][0]) for i in items if d in i.days and d - DAY in i.days]
        week = [math.log(i.days[d][0] / i.days[d - 7 * DAY][0]) for i in items if d in i.days and d - 7 * DAY in i.days]
        index[d] = (
            statistics.median(one) if len(one) >= 3 else None,
            statistics.median(week) if len(week) >= 3 else None,
        )
    return index


# ----------------------------------------------------------------------------- dip
@dataclass
class Trade:
    item: str
    entry: dt.date
    pnl: int  # cents per unit, after fees
    cost: int
    hold_days: int

    @property
    def pct(self) -> float:
        return self.pnl / self.cost * 100


def dip_exit(item: Item, entry: dt.date, cost: int, venue: Venue, dip_cfg: dict) -> Trade | None:
    """The engine's dip exit: after unlock, sell at the first profitable quote; liquidate at
    the best quote once the market is more than stop_loss_pct below cost or the lot reaches
    max_hold_days."""
    stop = float(dip_cfg.get("stop_loss_pct", 25.0)) / 100
    max_hold = int(dip_cfg.get("max_hold_days", 21))
    last = None
    for offset in range(7, max_hold + 15):
        d = entry + offset * DAY
        if d not in item.days:
            continue
        price = int(item.days[d][0])
        proceeds = venue.sell_proceeds(price)
        last = (offset, proceeds)
        if proceeds > cost or price < cost * (1 - stop) or offset >= max_hold:
            return Trade(item.name, entry, proceeds - cost, cost, offset)
    if last:  # No quote after max_hold within the data: mark at the last one seen.
        return Trade(item.name, entry, last[1] - cost, cost, last[0])
    return None


def dip_study(items: list[Item], index, since: dt.date, venue: Venue, cfg: dict, regime_cfg: dict, min_price: int):
    k = float(cfg.get("k_sigma", 2.0))
    lookback = int(cfg.get("lookback_days", 30))
    max_drop = float(cfg.get("max_drop_pct", 30.0)) / 100
    min_days = 10
    surge = float(cfg.get("volume_multiplier", 1.5))
    short_loss = -float(regime_cfg.get("short_loss_pct", 3.0)) / 100
    long_loss = -float(regime_cfg.get("long_loss_pct", 8.0)) / 100
    lag = float(regime_cfg.get("item_underperformance_pct", 5.0)) / 100

    variants = {name: [] for name in ("control: buy every day", "dip rule", "dip + regime gate", "dip in market stress")}
    for item in items:
        busy_until = {name: dt.date.min for name in variants}
        for d in sorted(item.days):
            if d < since:
                continue
            price, _ = item.days[d]
            if price < min_price:
                continue
            cost = venue.buy_cost(int(price))
            # Daily log averages of the prior lookback window (the triggering day excluded).
            prior = [math.log(item.days[p][0]) for p in (d - n * DAY for n in range(1, lookback + 1)) if p in item.days]
            recent = sum(item.days[p][1] for p in (d - n * DAY for n in range(0, 7)) if p in item.days)
            base = sum(item.days[p][1] for p in (d - n * DAY for n in range(7, 7 + lookback)) if p in item.days)
            one, week = index.get(d, (None, None))
            item_one = math.log(price / item.days[d - DAY][0]) if d - DAY in item.days else None

            def enter(name: str):
                if d < busy_until[name]:
                    return  # One open lot per item, as the engine's occupied-key check.
                trade = dip_exit(item, d, cost, venue, cfg)
                if trade:
                    variants[name].append(trade)
                    busy_until[name] = d + trade.hold_days * DAY

            enter("control: buy every day")
            if len(prior) < min_days:
                continue
            center, sigma = robust(prior)
            signal = (
                sigma > 0
                and math.log(price) <= center - k * sigma
                and price >= math.exp(center) * (1 - max_drop)
                and base > 0
                and recent >= 10
                and recent >= surge * base * 7 / lookback
                # Entry check: selling at the baseline must clear the all-in cost.
                and venue.sell_proceeds(int(math.exp(center))) > cost
            )
            if not signal:
                continue
            enter("dip rule")
            risk_on = (
                one is not None
                and week is not None
                and one > short_loss
                and week > long_loss
                and item_one is not None
                and item_one >= one - lag
            )
            if risk_on:
                enter("dip + regime gate")
            stressed = any(
                (index.get(d - n * DAY, (None, None))[0] or 0) <= short_loss
                or (index.get(d - n * DAY, (None, None))[1] or 0) <= long_loss
                for n in range(0, 4)
            )
            if stressed:
                enter("dip in market stress")
    return variants


# --------------------------------------------------------------------------- peg bid
def ceiling(ref: int, venue: Venue, margin: float, carry: float) -> int:
    """Largest bid whose net margin at exit `ref` meets the target, as compute_effective_ceiling."""
    low, high = 0, int(ref * (1 - margin))
    while low < high:
        mid = high - (high - low) // 2
        cost = venue.buy_cost(mid)
        net = venue.sell_proceeds(ref) - cost - math.ceil(cost * carry)
        if net >= math.ceil(cost * margin):
            low = mid
        else:
            high = mid - 1
    return low


def peg_study(items: list[Item], since: dt.date, venue: Venue, cfg: dict, min_price: int):
    """Peg bids at the margin ceiling under two fill models that bracket reality.

    * engine: any print at or below the bid fills it, and the unit keeps the quality of
      that print. This is the engine's BuyOrderFillSimulator rule. It credits us with the
      underpriced listings that sniping bots usually take first, so it is an upper bound.
    * conservative: the bid fills only on days the typical print is at or below it, that
      is, when the market itself has fallen to the bid, with an average-quality unit.
    """
    margin = float(cfg.get("target_margin_pct", 7.0)) / 100
    carry = float(cfg.get("annual_opportunity_rate_pct", 10.0)) / 100 * int(cfg.get("hold_days", 7)) / 365
    q = float(cfg.get("tier_only_exit_quantile", 0.25))
    results = {}
    for item in items:
        if len(item.prints) < 20:
            continue
        q_rel = quantile([r for r, _ in item.prints], q)
        engine = {"fills": 0.0, "net": 0.0, "wins": 0.0, "pnl": []}
        conservative = []
        control = []
        days_seen = 0
        for d in sorted(item.days):
            if d < since or item.days[d][0] < min_price:
                continue
            week = [item.days[p][0] * item.level for p in (d - n * DAY for n in range(1, 8)) if p in item.days]
            if len(week) < 3:
                continue
            exit_day = next((d + n * DAY for n in range(7, 22) if d + n * DAY in item.days), None)
            if exit_day is None:
                continue
            days_seen += 1
            market = item.days[d][0] * item.level
            exit_market = item.days[exit_day][0] * item.level
            market_cost = venue.buy_cost(int(market))
            control.append((venue.sell_proceeds(int(exit_market) - 1) - market_cost) / market_cost)
            # Engine valuation of a tier-only bid: tier quantile of the last week's prints.
            bid = ceiling(int(statistics.median(week) * q_rel), venue, margin, carry)
            if bid <= 0:
                continue
            cost = venue.buy_cost(bid)
            if market <= bid:
                conservative.append((venue.sell_proceeds(int(exit_market) - 1) - cost) / cost)
            eligible = [quality for rel, quality in item.prints if rel * market <= bid]
            p_fill = 1 - (1 - len(eligible) / len(item.prints)) ** item.days[d][1]
            if p_fill <= 0:
                continue
            pnls = [(venue.sell_proceeds(int(exit_market * quality) - 1) - cost) / cost for quality in eligible]
            engine["fills"] += p_fill
            engine["net"] += p_fill * statistics.fmean(pnls)
            engine["wins"] += p_fill * sum(p > 0 for p in pnls) / len(pnls)
            engine["pnl"].extend(pnls)
        if days_seen == 0:
            continue
        row = {"days": days_seen, "control_pct": statistics.fmean(control) * 100}
        if engine["fills"] > 0:
            row.update(
                engine_fills_per_30d=engine["fills"] / days_seen * 30,
                engine_net_pct=engine["net"] / engine["fills"] * 100,
                engine_win_pct=engine["wins"] / engine["fills"] * 100,
                engine_p05_pct=quantile(engine["pnl"], 0.05) * 100,
            )
        if conservative:
            row.update(
                conservative_fills_per_30d=len(conservative) / days_seen * 30,
                conservative_net_pct=statistics.fmean(conservative) * 100,
                conservative_win_pct=sum(p > 0 for p in conservative) / len(conservative) * 100,
            )
        results[item.name] = row
    return results


# --------------------------------------------------------------------------- report
def bootstrap_ci(trades: list[Trade], draws: int = 4000) -> tuple[float, float]:
    """95% interval for the mean net %, resampling whole entry weeks rather than trades:
    signals in one market episode are not independent, and treating them as such would
    overstate the evidence."""
    import random

    rng = random.Random(7)  # Fixed seed: the report is reproducible.
    weeks: dict[tuple[int, int], list[float]] = {}
    for t in trades:
        weeks.setdefault(t.entry.isocalendar()[:2], []).append(t.pct)
    clusters = list(weeks.values())
    means = []
    for _ in range(draws):
        sample = [p for _ in clusters for p in rng.choice(clusters)]
        means.append(statistics.fmean(sample))
    means.sort()
    return means[int(0.025 * draws)], means[int(0.975 * draws)]


def summarize(trades: list[Trade]) -> str:
    if not trades:
        return "| 0 | – | – | – | – | – | – | – |"
    pcts = [t.pct for t in trades]
    weeks = len({t.entry.isocalendar()[:2] for t in trades})
    low, high = bootstrap_ci(trades) if weeks >= 2 else (float("nan"), float("nan"))
    return (
        f"| {len(trades)} | {weeks} | {sum(p > 0 for p in pcts) / len(pcts) * 100:.0f}% | {statistics.fmean(pcts):+.2f}% "
        f"| {low:+.1f}% to {high:+.1f}% | {statistics.median(pcts):+.2f}% | {quantile(pcts, 0.05):+.1f}% "
        f"| {statistics.fmean(t.hold_days for t in trades):.1f} |"
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--data", type=pathlib.Path, default=pathlib.Path(__file__).with_name("data") / "csfloat")
    parser.add_argument("--years", type=float, default=2.0, help="history window to test")
    parser.add_argument("--min-price", type=int, default=100, help="cents; cheaper items round fees to zero")
    parser.add_argument("--max-noise", type=float, default=1.5,
                        help="daily-average noise (percent) above which an item's results are not trusted")
    parser.add_argument("--output", type=pathlib.Path, default=pathlib.Path(__file__).resolve().parent.parent / "reports" / "validation.md")
    args = parser.parse_args()

    items = load(args.data)
    if not items:
        print(f"no history under {args.data}; run fetch_csfloat_history.py first", file=sys.stderr)
        return 1
    latest = max(d for item in items for d in item.days)
    since = latest - dt.timedelta(days=int(args.years * 365))
    venue = load_venues()["csfloat"]
    lines = [
        "# Strategy validation on CSFloat history",
        "",
        f"{len(items)} items, {since} to {latest}, fees from config/default.toml "
        f"(CSFloat buy+resell round trip {venue.round_trip_pct():.2f}%). "
        f"P&L uses items priced at least ${args.min_price / 100:.2f}.",
        "",
        "## 1. Price risk over the trade lock",
        "",
        "*True daily σ* is the random-walk part of the price; *noise* is the day-to-day mix of floats "
        f"and sticker crafts in the daily average. Items with noise above {args.max_noise:.1f}% are marked "
        "✗: rules that buy low-average days look profitable on them because the mix reverts, not the price.",
        "",
        "| Item | Price | True daily σ | Daily noise | Clean | 7-day σ (robust) | 7-day σ (std) | 7-day 5th pct |",
        "| --- | ---: | ---: | ---: | :---: | ---: | ---: | ---: |",
    ]
    vol = volatility(items, since)
    clean = {name for name, v in vol.items() if v["noise_pct"] <= args.max_noise}
    for name, v in sorted(vol.items(), key=lambda kv: kv[1]["noise_pct"]):
        lines.append(
            f"| {name} | ${v['price_usd']:.2f} | {v['true_daily_pct']:.2f}% | {v['noise_pct']:.2f}% "
            f"| {'✓' if name in clean else '✗'} | {v['weekly_robust_pct']:.2f}% | {v['weekly_std_pct']:.2f}% "
            f"| {v['weekly_p05_pct']:+.1f}% |"
        )
    tradable = [v for v in vol.values() if v["price_usd"] * 100 >= args.min_price]
    effective_daily = statistics.median(v["true_daily_pct"] for v in tradable) if tradable else float("nan")
    weekly = statistics.median(v["weekly_robust_pct"] for v in tradable) if tradable else float("nan")
    lines += [
        "",
        f"Tradable items (≥ ${args.min_price / 100:.2f}): median 7-day robust σ **{weekly:.2f}%**, median true "
        f"daily σ **{effective_daily:.2f}%**, the calibration for `[spread] daily_volatility_pct`. "
        "Robust σ understates the tails: compare the standard deviation and the 5th percentile.",
        "",
    ]

    index = market_index(items)
    dip_cfg, regime_cfg = load_section("dip"), load_section("market_regime")
    dips = dip_study(items, index, since, venue, dip_cfg, regime_cfg, args.min_price)
    clean_items = [item for item in items if item.name in clean]
    clean_dips = dip_study(clean_items, index, since, venue, dip_cfg, regime_cfg, args.min_price)
    lines += [
        "## 2. Dip rule (robust_log baseline, volume surge, engine exit)",
        "",
        "One unit per signal, bought at the day's average on CSFloat, sold by the engine's exit rule.",
        "",
        "The 95% interval resamples entry weeks, because signals in one episode move together. "
        "An interval that includes the control's mean is not evidence of an edge.",
        "",
        "| Variant | Trades | Distinct weeks | Win rate | Mean net | 95% CI of mean | Median net | 5th pct | Avg hold (days) |",
        "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
    ]
    for name, trades in dips.items():
        lines.append(f"| {name} {summarize(trades)}")
    for name, trades in clean_dips.items():
        lines.append(f"| {name}, clean series only {summarize(trades)}")
    lines.append("")

    pegs = peg_study(items, since, venue, load_section("peg_bid"), args.min_price)
    lines += [
        "## 3. Peg bid at the margin ceiling on CSFloat",
        "",
        "Two fill models bracket reality. **Engine**: any print at or below the bid fills it and the unit "
        "keeps that print's float/sticker quality (the engine's BuyOrderFillSimulator rule; it credits us "
        "with underpriced listings that sniping bots usually take first). **Conservative**: the bid fills "
        "only when the typical print has fallen to it, with an average unit. The control buys at the market "
        "every day and sells after the lock.",
        "",
        "| Item | Clean | Engine fills / 30 d | Engine net | Engine win | Engine 5th pct | Conservative fills / 30 d | Conservative net | Conservative win | Control |",
        "| --- | :---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
    ]
    fmt = lambda row, key, spec: format(row[key], spec) if key in row else "–"
    for name, p in sorted(pegs.items(), key=lambda kv: -kv[1].get("engine_net_pct", -1e9)):
        lines.append(
            f"| {name} | {'✓' if name in clean else '✗'} | {fmt(p, 'engine_fills_per_30d', '.1f')} | {fmt(p, 'engine_net_pct', '+.2f')}% "
            f"| {fmt(p, 'engine_win_pct', '.0f')}% | {fmt(p, 'engine_p05_pct', '+.1f')}% "
            f"| {fmt(p, 'conservative_fills_per_30d', '.2f')} | {fmt(p, 'conservative_net_pct', '+.2f')}% "
            f"| {fmt(p, 'conservative_win_pct', '.0f')}% | {p['control_pct']:+.2f}% |"
        )

    def weighted(prefix: str, names: set[str] | None = None) -> tuple[float, float]:
        rows = [p for n, p in pegs.items() if f"{prefix}_fills_per_30d" in p and (names is None or n in names)]
        fills = sum(p[f"{prefix}_fills_per_30d"] for p in rows)
        net = sum(p[f"{prefix}_fills_per_30d"] * p[f"{prefix}_net_pct"] for p in rows) / fills if fills else float("nan")
        return fills, net

    lines.append("")
    for label, names in (("All items", None), ("Clean series only", clean)):
        engine_fills, engine_net = weighted("engine", names)
        cons_fills, cons_net = weighted("conservative", names)
        lines.append(
            f"- **{label}**: engine fill model {engine_fills:.0f} fills / 30 days at {engine_net:+.2f}% "
            f"fill-weighted net; conservative model {cons_fills:.1f} fills / 30 days at {cons_net:+.2f}%."
        )
    lines += [
        "",
        "Real fills sit between the two models; only real orders can measure where.",
        "",
    ]

    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text("\n".join(lines) + "\n", encoding="utf-8")
    print("\n".join(lines))
    print(f"\nwrote {args.output}")
    (args.output.with_suffix(".json")).write_text(
        json.dumps({"volatility": vol, "effective_daily_volatility_pct": effective_daily, "clean": sorted(clean), "peg": pegs,
                    "dip": {k: [t.__dict__ | {"entry": t.entry.isoformat()} for t in v] for k, v in dips.items()}},
                   indent=1, default=str),
        encoding="utf-8",
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
