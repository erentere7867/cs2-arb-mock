# Workstream A — Market clients

Implements `MarketClient` for Skinport, CSFloat and DMarket. Owns source-data
normalization, Skinport caching and 8-per-5-minute limits on `/v1/items` and
`/v1/sales/history`, the CSFloat API verification matrix, and Ed25519 signing for
read-only DMarket requests. Fees come from config, not client constants.
