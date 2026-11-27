# Read-only platform audit

This simulator never places an order, cancels a real order, moves funds, or uses a non-GET marketplace request. Documentation evidence is not an authenticated production capture. Values marked **UNVERIFIED** must not be treated as trading or settlement guarantees.

| Venue / games | Official API contract and implementation | Fees, holds, funding and limits |
| --- | --- | --- |
| Skinport — CS2 `730`, Rust `252490` | [Items](https://docs.skinport.com/items) and [sales history](https://docs.skinport.com/sales/history) document unauthenticated GET, `app_id`, `currency=USD`, Brotli, and aggregate prices in currency units. The client rounds to USD cents, prefers an unversioned catalogue row over phase-specific rows, and never treats total stock as all available at the minimum price. The CLI requests tradable listings. Aggregate sales are not verified individual prints. The [FAQ](https://skinport.com/faq) says Skinport does not buy items directly; no instant-sale path. | [Shared rate limit](https://docs.skinport.com/introduction/rate-limits): 8 requests per 5 minutes across these endpoints together, with 5-minute server caching. **UNVERIFIED**: individual listing locks and exact price-level depth (not exposed), the effective seller fee per item/account, and effective payout and bank delays per game/account. The [FAQ](https://skinport.com/faq) describes CS2 trade protection and bank payouts; it is not a payout quote. |
| CSFloat — CS2 only | [Official API documentation](https://docs.csfloat.com/) could not be reached from this environment. **UNVERIFIED**: `GET /api/v1/listings` including exact filter support, `GET /api/v1/history/{name}/sales`, `GET /api/v1/buy-orders`, anonymous auth behavior, response variants, and USD-cent interpretation on a production response. The adapter uses client-side name filtering, supports documented-by-wrapper response variants and an optional raw authorization key. The existing unauthenticated buy-order probe returned HTTP 405; live buy-order eligibility is **UNVERIFIED**. Rust is excluded. | The official [fee announcement](https://blog.csfloat.com/changes-to-fees-on-csgofloat-market/) describes marketplace and tiered withdrawal fees; the [payment announcement](https://blog.csfloat.com/payment-changes/) describes Stripe, bank, cards and Polygon USDC. **UNVERIFIED**: current rates/tiers, payment-method and network costs, settlement timing, lock metadata, rate quotas and executable instant-sale depth. Config values are scenarios. |
| DMarket — CS2 `a8db`, Rust `rust` | Official [Trading OpenAPI](https://docs.dmarket.com/v1/trading.swagger.json): signed GET offers `/marketplace-api/v2/offers` (`gameId`, exact-title filtering after prefix query; USD-cent `priceCents`), last sales `/trade-aggregator/v1/last-sales` (USD decimal, Unix seconds), and `/marketplace-api/v1/targets-by-title/{game_id}/{title}` (USD-cent `price`, string `amount`; URL-encoded title is signed decoded). The simulator accepts only targets with no item-attribute restriction it cannot prove. [Official signing examples](https://github.com/dmarket/dm-trading-tools). | The [FAQ](https://dmarket.com/faq) describes Targets, escrow, fees and funding methods; config can model up to 15 days of escrow for a scenario. **UNVERIFIED**: signed production responses on either game in this audit, authenticated funding behind target depth, whether otherwise-identical sales are distinct (no API sale ID), exact numeric quota and effective fees/escrow/withdrawal delay per account, and edge cases combining `tradable`, unlock date and lock days. |
| Waxpeer — CS2 `csgo`, Rust `rust` | [Catalogue snapshot](https://docs.waxpeer.com/api/buy-items/prices-snapshot.md): GET, integer milli-USD (asks round up), excludes locked/manual rows by default. The [public bid maximum](https://docs.waxpeer.com/api/buy-order/buy-orders-snapshot.md) is indicative, not executable depth. Opt-in [detailed buy orders](https://docs.waxpeer.com/api/buy-order/buy-orders-v2.md) use signed-in API-key GET (no writes); it exposes remaining amount and filters and says owners can pay for **one** unit, so the sim includes one unit per distinct unrestricted order, not its full remaining amount. Whether orders from the same owner are simultaneously funded is **UNVERIFIED**. Bids round down. No verified public sale-print feed was established; the [instant sell UI](https://faq.waxpeer.com/faq/instant-sell) is not an executable API quote. | [Standard seller fee: 6%](https://faq.waxpeer.com/faq/fees-sell); [deposit: 1% for most coins](https://faq.waxpeer.com/faq/fees-deposit); [standard crypto withdrawal: 1.5% before network cost](https://faq.waxpeer.com/faq/fees-withdraw). [Trade lock](https://faq.waxpeer.com/faq/trade-lock) differs from [balance hold](https://faq.waxpeer.com/faq/hold-balance): the latter says eligible proceeds are spendable immediately but withdrawable after eight days. An eight-day **spendable** hold in the simulator is a conservative scenario, not that policy. **UNVERIFIED**: exact incoming balance/fee treatment by game and method, network/bank charges, numeric API quotas, authenticated detailed response on either game, and live executability even with detailed depth. |

## Simulation setup

Amounts are USD cents; percentage fees are percentage points and liquidity/time-cost settings use basis points. The following is a **scenario**, not a recommendation or live quote. Keep venue accounts off to preserve the original fee-per-trade funding model.

```toml
game = "rust"
markets = ["skinport", "waxpeer"]
platform_audit = true

[venue_accounts.venues.skinport]
starting_cash = 20000
trade_hold_seconds = 604800
sale_balance_hold_seconds = 0
transfer_in_seconds = 0
transfer_out_seconds = 86400

[venue_accounts.venues.waxpeer]
starting_cash = 20000
trade_hold_seconds = 604800
sale_balance_hold_seconds = 691200
transfer_in_seconds = 3600
transfer_out_seconds = 3600

[[venue_accounts.transfers]]
at = "2025-01-08T12:00:00Z"
from = "waxpeer"
to = "skinport"
amount = 1000

[relist]
cross_venue = true
instant_sale = true
maximum_bid_age_seconds = 60
time_cost_bps_per_day = 25

[simulation]
maximum_listing_age_seconds = 600
fill_latency_min_milliseconds = 50
fill_latency_max_milliseconds = 150
minimum_verified_sales = 10
liquidity_window_days = 7
maximum_adv_fraction_bps = 1000

[spread]
price_drift_pct_per_day = 0.1
```

Merge this example into the corresponding game config (retain the `fees`, `rate_limits` and other strategy settings). Accounts have independent balances in each strategy comparison, not a shared live portfolio. Successful sales credit the selling venue when its configured balance hold elapses; proceeds may still be pending at replay end. Transfers are explicit and charge the source's configured crypto withdrawal followed by the destination's configured deposit. Delays add together. Transferability of any particular funding rail between two venues is **UNVERIFIED**; the example does not imply Skinport accepts crypto. No per-trade funding conversion or fiat cashout is applied while using internally funded venue accounts. Include funding/rail costs in a realistic scenario before drawing conclusions about cash-out profitability.

Instant sales are opt-in for the peg-bidding relist path and use explicit historical `--buy-orders` observations or read-only live bids, never synthetic bundled bids. A fresh observation supplies finite depth; a single price level must cover a lot. Repeated identical time/price records do not refill consumed depth. Cached/filtered/unsupported bids and motion between observations are not guarantees; live Waxpeer detailed bids require `WAXPEER_API_KEY` already present in the environment, otherwise that route supplies no bids in instant mode. An unlocked lot compares net bid proceeds to a liquid median listing, adjusted by the configured listing wait proxy and both venues’ balance holds. Already listed lots are not repriced into future bids. Fill latency, market marks, bid age, price drift, sales caps and modeled waiting periods are **UNVERIFIED simulation assumptions**.

Live sales collection requests the longest configured liquidity or peg/relist lookback (at least seven days). Skinport supplies only aggregates up to 90 days; CSFloat coverage is limited to the sales returned by its history endpoint; DMarket requests up to 20 sales per page through its rate limiter; Waxpeer has no verified sales feed. Detailed Waxpeer bid reads stop at a short page, 100 distinct eligible orders, or three pages of 100 rows, whichever comes first. Depth outside that cap is not simulated.

The run report includes `UNVERIFIED` notes for live data and when `platform_audit`, venue accounts or instant sale are enabled. Default offline CS2 report behavior is unchanged.
