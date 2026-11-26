# Workstream A — Market clients (`cs2-arb-markets`)

Read-only `MarketClient` implementations for Skinport, CSFloat and DMarket. Every request
is a `GET`; no order/trade code exists. Fees come from the `FeeSchedule` passed in.

- `gate::RequestGate` (+ `NoopGate`): local admission-control trait; integration adapts
  `cs2-arb-net`'s limiter to it. This crate does not depend on `cs2-arb-net`.
- Skinport: `/v1/items`, `/v1/sales/history`, `Accept-Encoding: br`, whole-catalogue
  fetches, per-endpoint 8 req / 5 min local limiter, TTL cache (memory + optional disk
  JSON), stale fallback when rate limited.
- CSFloat: `/api/v1/listings`, `/api/v1/history/<name>/sales`, optional
  `CSFLOAT_API_KEY` sent as `Authorization`.
- DMarket: `/marketplace-api/v2/offers`, `/trade-aggregator/v1/last-sales`, Ed25519
  signing (`DMARKET_PUBLIC_KEY` / `DMARKET_SECRET_KEY`).

Endpoint verification status and UNVERIFIED assumptions are documented at the top of
each client module.
