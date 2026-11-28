#!/usr/bin/env python3
"""Download public CSFloat price history for the validation harness.

For every item in a universe file this saves two read-only, unauthenticated GETs:

* ``/api/v1/history/<name>/graph``: one row per UTC day with the number of sales and
  their average price in cents, reaching back years. This is what lets the strategies be
  tested on real price paths today instead of after weeks of live collection.
* ``/api/v1/history/<name>/sales``: the most recent individual sales with float values
  and CSFloat's own float-adjusted reference price, used to measure how far single
  prints scatter around the daily average.

Requests are paced at one per ten seconds, the same pacing the engine uses for CSFloat
(config/default.toml), and files already on disk are skipped, so an interrupted run
resumes where it stopped. Nothing here can place an order: it only issues GETs.
"""

from __future__ import annotations

import argparse
import json
import pathlib
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

BASE = "https://csfloat.com/api/v1/history"
# A generic client name: no personal contact details go to the provider.
USER_AGENT = "cs2-arb-mock-research/1.0"
ENDPOINTS = ("graph", "sales")


def slug(name: str) -> str:
    """Filesystem-safe, stable file stem for a market_hash_name."""
    keep = [c.lower() if c.isalnum() else "-" for c in name]
    return "-".join(part for part in "".join(keep).split("-") if part)


def read_universe(path: pathlib.Path) -> list[str]:
    names = []
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if line and not line.startswith("#"):
            names.append(line)
    return names


def fetch(url: str, timeout: float) -> tuple[int, bytes, float | None]:
    """GET `url`; return (status, body, retry_after_seconds)."""
    request = urllib.request.Request(
        url, headers={"User-Agent": USER_AGENT, "Accept": "application/json"}
    )
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            return response.status, response.read(), None
    except urllib.error.HTTPError as error:
        retry = error.headers.get("Retry-After")
        try:
            retry_after = float(retry) if retry is not None else None
        except ValueError:
            retry_after = None
        return error.code, error.read(), retry_after


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--universe", type=pathlib.Path, default=pathlib.Path(__file__).with_name("universe.txt"))
    parser.add_argument("--out", type=pathlib.Path, default=pathlib.Path(__file__).with_name("data") / "csfloat")
    parser.add_argument("--interval", type=float, default=10.0, help="seconds between requests")
    parser.add_argument("--timeout", type=float, default=30.0)
    args = parser.parse_args()

    args.out.mkdir(parents=True, exist_ok=True)
    names = read_universe(args.universe)
    last_request = 0.0
    failures = 0
    for index, name in enumerate(names, 1):
        for endpoint in ENDPOINTS:
            target = args.out / f"{slug(name)}.{endpoint}.json"
            if target.exists():
                continue
            url = f"{BASE}/{urllib.parse.quote(name, safe='')}/{endpoint}"
            for attempt in range(4):
                wait = last_request + args.interval - time.monotonic()
                if wait > 0:
                    time.sleep(wait)
                last_request = time.monotonic()
                status, body, retry_after = fetch(url, args.timeout)
                if status == 200:
                    payload = json.loads(body)
                    # Keep the name next to the data so later steps never guess it from a slug.
                    target.write_text(json.dumps({"market_hash_name": name, "data": payload}), encoding="utf-8")
                    print(f"[{index}/{len(names)}] {endpoint:5} {name}: {len(payload)} rows", flush=True)
                    break
                if status == 429 or status >= 500:
                    # Back off for the server's hint, or exponentially, before retrying.
                    pause = retry_after if retry_after is not None else 60.0 * (2**attempt)
                    print(f"  HTTP {status} for {name} {endpoint}; pausing {pause:.0f}s", file=sys.stderr, flush=True)
                    time.sleep(pause)
                    continue
                print(f"  HTTP {status} for {name} {endpoint}; skipping", file=sys.stderr, flush=True)
                failures += 1
                break
            else:
                failures += 1
    print(f"done: {len(names)} items, {failures} failures", flush=True)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
