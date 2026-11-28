#!/usr/bin/env python3
"""Event study: how CS2 updates move skin prices, and whether trading them pays.

Uses every update since the CS2 launch (fetch_updates.py) and the CSFloat daily history
(fetch_csfloat_history.py). Two questions:

1. Do prices move around updates, and do they move before release? The pre-release
   window is what watching SteamDB's staging and prerelease branches would buy.
   For each update U (UTC posting date) and each clean series, log returns over:

   * pre:      U-4 -> U-1   (before the announcement)
   * reaction: U-1 -> U+1   (across the release)
   * lock:     U+1 -> U+8   (buy the day after, hold through the 7-day lock)
   * two weeks: U+1 -> U+15

   An event's move is the median across items (items co-move, so they are not
   independent). Each category is compared with the same window measured from every
   day, and its 95% interval resamples events.

2. Does the engine's update-aware dip rule make money inside real update windows?
   The dip rule from validate.py runs only inside [U, U + window_duration_secs) for each
   update, by category, with and without the regime gate, against a buy-every-day
   control. Exits follow the engine: first profitable quote after the lock, stop-loss and
   maximum hold.

Only clean series (daily-average noise since the CS2 launch at or below --max-noise) are
used, so returns are price moves rather than shifts in the day's float and sticker mix.
Cases and skins are reported separately: supply updates change case drops directly.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import math
import pathlib
import random
import statistics
import sys

from feeslib import load_section, load_venues
from validate import DAY, Book, dip_signal, load, market_index, quantile, regime_allows, summarize, volatility

WINDOWS = {"pre": (-4, -1), "reaction": (-1, 1), "lock": (1, 8), "two weeks": (1, 15)}
CATEGORIES = ("trading", "supply", "major", "maintenance")


def window_return(days: dict, anchor: dt.date, window: tuple[int, int]) -> float | None:
    start, end = anchor + window[0] * DAY, anchor + window[1] * DAY
    if start in days and end in days:
        return math.log(days[end][0] / days[start][0])
    return None


def event_move(items, anchor: dt.date, window) -> float | None:
    moves = [r for item in items if (r := window_return(item.days, anchor, window)) is not None]
    return statistics.median(moves) if len(moves) >= 3 else None


def mean_ci(values: list[float], draws: int = 4000) -> tuple[float, float, float]:
    rng = random.Random(11)  # Fixed seed: reproducible report.
    means = sorted(statistics.fmean(rng.choices(values, k=len(values))) for _ in range(draws))
    return statistics.fmean(values), means[int(0.025 * draws)], means[int(0.975 * draws)]


def pct(log_return: float) -> float:
    return (math.exp(log_return) - 1) * 100


def main() -> int:
    here = pathlib.Path(__file__).resolve().parent
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--data", type=pathlib.Path, default=here / "data" / "csfloat")
    parser.add_argument("--updates", type=pathlib.Path, default=here / "data" / "steam" / "updates.json")
    parser.add_argument("--min-price", type=int, default=100)
    # 2.2% admits the six steadiest skins since launch (Printstreams, Bloodsport, Neo-Noir,
    # Chromatic Aberration) and no noisier one; chosen on data quality, not on returns.
    parser.add_argument("--max-noise", type=float, default=2.2)
    parser.add_argument("--output", type=pathlib.Path, default=here.parent / "reports" / "update_study.md")
    args = parser.parse_args()

    items = load(args.data)
    launch = dt.date(2023, 9, 27)  # CS2 release: the update history starts here.
    vol = volatility(items, launch)
    clean = [item for item in items if item.name in vol and vol[item.name]["noise_pct"] <= args.max_noise]
    groups = {
        "cases": [item for item in clean if item.is_case],
        "skins": [item for item in clean if not item.is_case],
    }
    events: dict[dt.date, set[str]] = {}
    for update in json.loads(args.updates.read_text(encoding="utf-8")):
        events.setdefault(dt.date.fromisoformat(update["date"][:10]), set()).update(update["tags"])
    for tags in events.values():
        if len(tags) > 1:
            tags.discard("maintenance")  # A day with an item change is not a maintenance day.
    all_days = sorted({d for item in clean for d in item.days})

    lines = [
        "# CS2 update event study",
        "",
        f"{len(events)} update days ({min(events)} to {max(events)}); clean series (noise ≤ {args.max_noise}%): "
        f"{len(groups['cases'])} cases, {len(groups['skins'])} skins ({', '.join(i.name for i in groups['skins'])}). "
        "An event's move is the median clean item's return; intervals resample events.",
        "",
        "## 1. Price moves around updates",
    ]
    per_event: dict[dt.date, dict[str, list[float | None]]] = {}
    for group, members in groups.items():
        if len(members) < 3:
            continue  # event_move needs three items to take a median.
        lines += [
            "",
            f"### {group}",
            "",
            "| Category | Events | " + " | ".join(f"{w} (U{a:+d}→U{b:+d})" for w, (a, b) in WINDOWS.items()) + " |",
            "| --- | ---: | " + " | ".join("---:" for _ in WINDOWS) + " |",
        ]
        days = sorted({d for item in members for d in item.days if d >= launch})
        baseline = {}
        for name, window in WINDOWS.items():
            baseline[name] = statistics.fmean(m for d in days if (m := event_move(members, d, window)) is not None)
        lines.append(f"| every day (baseline) | {len(days)} | " + " | ".join(f"{pct(baseline[w]):+.2f}%" for w in WINDOWS) + " |")
        for category in CATEGORIES:
            dates = sorted(d for d, tags in events.items() if category in tags)
            cells = []
            for name, window in WINDOWS.items():
                moves = [m for d in dates if (m := event_move(members, d, window)) is not None]
                if len(moves) < 2:
                    cells.append("–")
                    continue
                mean, low, high = mean_ci(moves)
                # Mark intervals that exclude the baseline: the only evidence of an effect.
                flag = " **" if high < baseline[name] or low > baseline[name] else ""
                cells.append(f"{flag}{pct(mean):+.2f}% ({pct(low):+.1f} to {pct(high):+.1f}){flag.strip() and '**'}")
            lines.append(f"| {category} | {len(dates)} | " + " | ".join(cells) + " |")
            if category != "maintenance":
                for d in dates:
                    per_event.setdefault(d, {})[group] = [event_move(members, d, w) for w in WINDOWS.values()]
    lines += [
        "",
        "**Bold**: the interval excludes the every-day baseline, i.e. evidence that the category moves prices.",
        "",
        "### Item-relevant updates, one by one (median clean case / skin)",
        "",
        "| Date | Tags | " + " | ".join(WINDOWS) + " |",
        "| --- | --- | " + " | ".join("---:" for _ in WINDOWS) + " |",
    ]
    for d in sorted(per_event):
        cells = []
        for i in range(len(WINDOWS)):
            parts = [per_event[d].get(g, [None] * len(WINDOWS))[i] for g in groups]
            cells.append(" / ".join("–" if r is None else f"{pct(r):+.1f}%" for r in parts))
        lines.append(f"| {d} | {','.join(sorted(events[d]))} | " + " | ".join(cells) + " |")

    # ------------------------------------------------------------------ trading test
    venue = load_venues()["csfloat"]
    dip_cfg, regime_cfg = load_section("dip"), load_section("market_regime")
    duration = dt.timedelta(seconds=int(load_section("events").get("window_duration_secs", 345_600)))
    index = market_index(items)

    def in_window(d: dt.date, categories) -> bool:
        return any(u <= d < u + duration and tags & categories for u, tags in events.items())

    item_tags = {"trading", "supply", "major"}
    variants = (
        "control: buy every day",
        "buy the day after an item update",
        # The decisive control: the same signal on days with no update window. If it earns
        # as much there, updates add nothing and the result is the dip rule (or noise).
        "dip rule, outside update windows",
        "dip rule, any update window",
        "dip rule, any update window + regime gate",
        "dip rule, item-update window",
        "dip rule, item-update window + regime gate",
    )
    books = {group: Book(variants) for group in groups}
    for item in clean:  # Cases included: a case above min_price is as tradable as a skin.
        book = books["cases" if item.is_case else "skins"]
        for d in sorted(item.days):
            if item.days[d][0] < args.min_price:
                continue
            book.enter("control: buy every day", item, d, venue, dip_cfg)
            if d - DAY in events and events[d - DAY] & item_tags:
                book.enter("buy the day after an item update", item, d, venue, dip_cfg)
            if not dip_signal(item, d, venue, dip_cfg):
                continue
            any_window = in_window(d, set(CATEGORIES))
            item_window = in_window(d, item_tags)
            if not any_window:
                book.enter("dip rule, outside update windows", item, d, venue, dip_cfg)
                continue
            gate = regime_allows(item, d, index, regime_cfg)
            book.enter("dip rule, any update window", item, d, venue, dip_cfg)
            if gate:
                book.enter("dip rule, any update window + regime gate", item, d, venue, dip_cfg)
            if item_window:
                book.enter("dip rule, item-update window", item, d, venue, dip_cfg)
                if gate:
                    book.enter("dip rule, item-update window + regime gate", item, d, venue, dip_cfg)
    lines += [
        "",
        "## 2. Trading the updates (engine dip rule and exits, CSFloat fees)",
        "",
        f"Clean series priced at least ${args.min_price / 100:.2f}; windows last {duration.days} days from the post, "
        "as `[events] window_duration_secs`. Intervals resample entry weeks.",
        "",
        "| Variant | Trades | Distinct weeks | Win rate | Mean net | 95% CI of mean | Median net | 5th pct | Avg hold (days) |",
        "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
    ]
    for group, book in books.items():
        for name in variants:
            lines.append(f"| {group}: {name} {summarize(book.trades[name])}")
    lines += ["", "### Dip trades inside update windows", "", "| Group | Entry | Item | Net | Hold (days) | Update tags that week |", "| --- | --- | --- | ---: | ---: | --- |"]
    for group, book in books.items():
        for trade in sorted(book.trades["dip rule, any update window"], key=lambda t: t.entry):
            tags = sorted({t for u, ts in events.items() if u <= trade.entry < u + duration for t in ts})
            lines.append(f"| {group} | {trade.entry} | {trade.item} | {trade.pct:+.1f}% | {trade.hold_days} | {','.join(tags)} |")
    lines.append("")

    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text("\n".join(lines) + "\n", encoding="utf-8")
    print("\n".join(lines))
    print(f"\nwrote {args.output}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
