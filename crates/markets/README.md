# Workstream A — Market clients (`cs2-arb-markets`)

Read-only `MarketClient` implementations for Skinport, CSFloat, DMarket and Waxpeer. Every request
is a `GET`; no order/trade code exists. Fees come from the `FeeSchedule` passed in.

- The shared HTTP client stops reads across routes during a host cooldown, even without
  a `RequestGate`. `NetGate` applies configured quotas and endpoint penalties.
  `ResilientMarket` retries transient network failures, not 429s.
- Skinport: `/v1/items`, `/v1/sales/history`, `Accept-Encoding: br`, whole-catalogue
  fetches, per-endpoint 8 req / 5 min local limiter, TTL cache (memory + optional disk
  JSON), stale fallback when rate limited.
- CSFloat: `/api/v1/listings`, `/api/v1/history/<name>/sales`, optional
  `CSFLOAT_API_KEY` sent as `Authorization`.
- DMarket: `/marketplace-api/v2/offers`, `/trade-aggregator/v1/last-sales`, Ed25519
  signing (`DMARKET_PUBLIC_KEY` / `DMARKET_SECRET_KEY`).
- Waxpeer: public `/v1/prices/snapshot` and `/v1/buy-orders/snapshot` using `game=csgo` or
  `game=rust` ([API docs](https://docs.waxpeer.com/llms.txt)). No API key. Cached in memory
  for ten seconds; snapshot observation times and listing unlock dates are retained.
  USD uses 1,000 source units per dollar; asks round up and bids round down to cents.
  Listings are matched by exact market hash name, with no CS2 float data in the adapter.
  The highest bid is a price signal with placeholder quantity one, not reliable available
  quantity or a completed sale. No public sales tape is used, and no order-management routes
  are called (even when documented as GET).

Endpoint verification status and UNVERIFIED assumptions are documented at the top of
each client module.
