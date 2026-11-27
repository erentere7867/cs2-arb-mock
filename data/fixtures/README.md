# Synthetic historical fixtures

`price_snapshots.json` is a JSON array of `cs2_arb_core::PriceSnapshot`: 50
consecutive UTC days × 4 item/exterior pairs × 3 platforms, quoted in normalized
USD cents. The same bucket is used across platforms for each item. An update
on day 37 produces a sharp DMarket dip in the AK-47 | Redline (Field-Tested)
series, followed by a recovery. `market_events.json` holds matching simulated
branch and panic-window events. Prices and events are generated, not historical
observations or evidence of real sales.

Regenerate both files from the workspace root:

```sh
python3 data/fixtures/generate.py
```

`rust_price_snapshots.json` is a separate 16-day synthetic Skinport/DMarket
scenario for Alien Red, with no exterior or float bucket. It has no bundled
update events. The CLI models the same synthetic sales tape for each bundled
scenario; it does not synthesize sales for custom snapshot files.
