"""Fee arithmetic shared by the research scripts, read from the engine's own config.

The research numbers are only meaningful if they charge exactly what the Rust engine
charges, so this mirrors crates/core/src/fees.rs: percentage fees round half away from
zero per unit (f64::round), funding legs are added on top (deposit on entry, withdrawal
and bank cash-out on exit), and fixed fees are charged once per order.
"""

from __future__ import annotations

import math
import pathlib
import tomllib

ROOT = pathlib.Path(__file__).resolve().parent.parent
VENUES = ("skinport", "csfloat", "dmarket", "waxpeer")


def _round(value: float) -> int:
    """Rust's f64::round: half away from zero (Python's round() is half to even)."""
    return int(math.floor(abs(value) + 0.5)) * (1 if value >= 0 else -1)


def _fee(cents: int, pct: float, fixed: int = 0) -> int:
    # Same operation order as the engine's `subtotal as f64 * (pct / 100.0)`, so values
    # that land exactly on .5 round identically.
    return min(_round(cents * (pct / 100.0)), cents) + fixed


class Venue:
    def __init__(self, name: str, table: dict):
        self.name = name
        self.buyer_pct = float(table.get("buyer_fee_pct", 0.0))
        self.seller_pct = float(table.get("seller_fee_pct", 0.0))
        self.buyer_fixed = int(table.get("buyer_fixed_fee", 0))
        self.seller_fixed = int(table.get("seller_fixed_fee", 0))
        funding = table.get("funding", {})
        leg = lambda key: (float(funding.get(key, {}).get("pct", 0.0)), int(funding.get(key, {}).get("fixed", 0)))
        self.deposit = leg("crypto_deposit")
        self.withdrawal = leg("crypto_withdrawal")
        self.cashout = leg("bank_cashout")

    def buy_cost(self, price: int) -> int:
        """All-in cents to buy one unit at `price` (buyer fee plus deposit leg)."""
        return price + _fee(price, self.buyer_pct, self.buyer_fixed) + _fee(price, *self.deposit)

    def sell_proceeds(self, price: int) -> int:
        """Cents received for one unit sold at `price` after seller fee and cash-out legs."""
        return (
            price
            - _fee(price, self.seller_pct, self.seller_fixed)
            - _fee(price, *self.withdrawal)
            - _fee(price, *self.cashout)
        )

    def round_trip_pct(self) -> float:
        """Cost of buying and reselling one $100 unit on this venue, in percent."""
        return 100.0 - self.sell_proceeds(10_000) / self.buy_cost(10_000) * 100.0


def load_venues(config: pathlib.Path | None = None) -> dict[str, Venue]:
    path = config or ROOT / "config" / "default.toml"
    fees = tomllib.loads(path.read_text(encoding="utf-8"))["fees"]
    return {name: Venue(name, fees.get(name, {})) for name in VENUES}


def load_section(section: str, config: pathlib.Path | None = None) -> dict:
    path = config or ROOT / "config" / "default.toml"
    return tomllib.loads(path.read_text(encoding="utf-8")).get(section, {})
