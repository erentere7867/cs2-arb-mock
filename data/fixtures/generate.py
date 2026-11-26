from datetime import datetime, timedelta, timezone
import json
from pathlib import Path
from random import Random


ROOT = Path(__file__).resolve().parent
START = datetime(2025, 1, 1, 12, tzinfo=timezone.utc)
DIP_DAY = 36
ITEMS = (
    ("AK-47 | Redline (Field-Tested)", "field_tested", "field_tested_low", 2940),
    ("AK-47 | Redline (Battle-Scarred)", "battle_scarred", "battle_scarred_high", 1360),
    ("M4A1-S | Decimator (Minimal Wear)", "minimal_wear", "minimal_wear", 2675),
    ("USP-S | Cortex (Factory New)", "factory_new", "factory_new_high", 1015),
)
PLATFORMS = (("skinport", 103), ("csfloat", 100), ("dmarket", 98))


def utc_string(timestamp):
    return timestamp.strftime("%Y-%m-%dT%H:%M:%SZ")


def generate_snapshots():
    rng = Random(730)
    for day in range(50):
        observed_at = utc_string(START + timedelta(days=day))
        for index, (name, wear, bucket, base_price) in enumerate(ITEMS):
            trend = (day // 3) * (index + 1)
            common_noise = rng.randint(-3, 3)
            for platform, multiplier in PLATFORMS:
                price = (
                    base_price * (multiplier + common_noise) // 100
                    + trend
                    + rng.randint(-12, 12)
                )
                if index == 0 and platform == "dmarket" and 0 <= day - DIP_DAY <= 3:
                    price = price * (74, 80, 90, 96)[day - DIP_DAY] // 100
                yield {
                    "platform": platform,
                    "item": {"market_hash_name": name, "wear_tier": wear},
                    "float_bucket": bucket,
                    "price": price,
                    "observed_at": observed_at,
                }


def generate_events():
    opened_at = START + timedelta(days=DIP_DAY, hours=-1)
    return [
        {
            "kind": "branch_updated",
            "app_id": 730,
            "branch": "staging",
            "change_number": 100_001,
            "observed_at": utc_string(opened_at),
        },
        {
            "kind": "panic_window_opened",
            "branch": "staging",
            "opened_at": utc_string(opened_at),
        },
        {
            "kind": "panic_window_closed",
            "branch": "staging",
            "closed_at": utc_string(opened_at + timedelta(days=4)),
        },
    ]


def write_fixture(name, entries):
    content = ",\n".join("  " + json.dumps(entry) for entry in entries)
    (ROOT / name).write_text(f"[\n{content}\n]\n", encoding="utf-8")


if __name__ == "__main__":
    write_fixture("price_snapshots.json", generate_snapshots())
    write_fixture("market_events.json", generate_events())
