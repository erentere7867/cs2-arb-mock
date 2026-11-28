#!/usr/bin/env python3
"""Download CS2 update announcements and classify which ones touch the item economy.

Steam's public news API (ISteamNews/GetNewsForApp, app 730, official community
announcements only) returns every "Counter-Strike 2 Update" post with its date and full
patch notes. Those dates are what the engine's SteamDB watcher would have reacted to,
so they let the update-aware strategy be tested on every update of the last few years
instead of on the two market crashes that a price-based proxy can find.

Announcements are paged back to the CS2 launch (27 Sep 2023). "Counter-Strike 2 Update"
posts are always kept; other announcements (season launches such as the one that
introduced Trade Protection) are kept when their text carries a tag.

Each line of the notes is tagged only if it names an item-economy subject *and*
announces a change (added, now available, removed, no longer, starting ...); trade-rule
terms are specific enough to count on their own. Lines that begin with "Fixed" are bug
fixes and are ignored, so "Fixed an Armory UI issue" or "Fixed a case where ..." do not
count. Tags:

* supply: a named case, collection, capsule or souvenir package appears, or the
  Armory / drop pool changes. These change how many items enter the market.
* trading: trade protection, trade holds, trade-up contracts or market rules change.
* major: a Major event is mentioned (sticker capsules, souvenirs, viewer passes).
* maintenance: none of the above; maps, gameplay and bug fixes.

Patterns are case-sensitive where patch notes use ordinary words in ordinary ways
("Fixed a case where ..." must not count as a weapon case).
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import pathlib
import re
import sys
import urllib.request

NEWS = (
    "https://api.steampowered.com/ISteamNews/GetNewsForApp/v2/"
    "?appid=730&count=100&feeds=steam_community_announcements&maxlength=0&format=json&enddate={enddate}"
)
CS2_LAUNCH = dt.datetime(2023, 9, 27, tzinfo=dt.timezone.utc)
USER_AGENT = "cs2-arb-mock-research/1.0"
# Update posts were titled "Release Notes for M/D/YYYY" until October 2024.
TITLE = re.compile(r"^(Counter-Strike 2 (Pre-Release )?Update|Release Notes for [\d/]+)$")
TAGS = {
    "supply": re.compile(
        r"[A-Z][\w&'.-]* (Weapon )?Case\b|[A-Z][\w&'.-]* Collection\b|Capsule\b|Souvenir Package"
        r"|\bArmory\b|drop (pool|list)|Care Package|Terminal|rare special items"
    ),
    "trading": re.compile(
        r"(?i)trade[- ]protect|trade hold|trade[- ]up|community market|trade lock"
        r"|trade reversal|reverse (all|your|any)[\w ]* trades"
    ),
    # Weak trading terms: items are "tradable" in many ordinary lines, so these also need
    # an announced change and are reported under the same tag.
    "trading_weak": re.compile(r"(?i)\b(tradable|marketable)\b"),
    "major": re.compile(r"\bMajor\b"),
}
CHANGE = re.compile(
    r"(?i)\b(added|adding|now available|now be|available (for|to|in)|removed|no longer|introduc|starting|"
    r"will (now|become|be)|can now|is now|are now|replaced|updated the|on sale|discontinu)"
)


def plain(text: str) -> str:
    """Strip Steam BBCode so patterns see the words."""
    return re.sub(r"\s+", " ", re.sub(r"\[/?[^\]]+\]", " ", text.replace("\\[", "[").replace("\\]", "]"))).strip()


def note_lines(text: str) -> list[str]:
    """One entry per bullet, paragraph or sentence of the patch notes."""
    text = text.replace("\\[", "[").replace("\\]", "]")
    parts = re.split(r"\[\*\]|\[/?p\]|\[/?list\]|\n|(?<=[.!?])\s+(?=[A-Z])", text)
    return [line for line in (plain(part) for part in parts) if line]


# Reviewed false positives: (UTC date, tag) -> why the matching line is not that kind of
# change. Kept small and explicit rather than bending the patterns around single lines.
OVERRIDES = {
    ("2024-02-08", "trading"): "sticker rendering fix when inspecting Market items",
    ("2025-09-03", "trading"): "UI action added for trade-protected items; no rule change",
}

# Trade-rule terms are specific enough on their own; supply and Major terms also appear
# in bug-fix and UI lines, so those need an announced change on the same line.
NEEDS_CHANGE = {"supply", "major", "trading_weak"}


def tags_for(text: str) -> list[str]:
    found = set()
    for line in note_lines(text):
        if line.startswith("Fixed"):
            continue
        changed = bool(CHANGE.search(line))
        found.update(
            name.removesuffix("_weak")
            for name, pattern in TAGS.items()
            if pattern.search(line) and (changed or name not in NEEDS_CHANGE)
        )
    return sorted(found)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--out", type=pathlib.Path, default=pathlib.Path(__file__).with_name("data") / "steam" / "updates.json")
    args = parser.parse_args()

    news, enddate, seen = [], int(dt.datetime.now(dt.timezone.utc).timestamp()), set()
    while True:
        request = urllib.request.Request(NEWS.format(enddate=enddate), headers={"User-Agent": USER_AGENT})
        with urllib.request.urlopen(request, timeout=60) as response:
            page = [p for p in json.load(response)["appnews"]["newsitems"] if p["gid"] not in seen]
        if not page:
            break
        seen.update(p["gid"] for p in page)
        news.extend(page)
        enddate = min(p["date"] for p in page) - 1
        if enddate < CS2_LAUNCH.timestamp():
            break

    updates = []
    for post in news:
        posted = dt.datetime.fromtimestamp(post["date"], dt.timezone.utc)
        if posted < CS2_LAUNCH:
            continue
        is_update = bool(TITLE.match(post["title"].strip()))
        tags = [t for t in tags_for(post.get("contents", "")) if (posted.date().isoformat(), t) not in OVERRIDES]
        if not is_update and not tags:
            continue  # Esports and community posts that change nothing about items.
        text = plain(post.get("contents", ""))
        updates.append({
            "date": posted.isoformat(),
            "title": post["title"].strip(),
            "tags": tags or ["maintenance"],
            "url": post.get("url"),
            "summary": text[:400],
        })
    updates.sort(key=lambda u: u["date"])
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(updates, indent=1), encoding="utf-8")

    counts: dict[str, int] = {}
    for update in updates:
        for tag in update["tags"]:
            counts[tag] = counts.get(tag, 0) + 1
    span = f"{updates[0]['date'][:10]} to {updates[-1]['date'][:10]}" if updates else "none"
    print(f"{len(updates)} updates ({span}): " + ", ".join(f"{k} {v}" for k, v in sorted(counts.items())))
    return 0


if __name__ == "__main__":
    sys.exit(main())
