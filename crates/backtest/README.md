# Workstream F — Operations and backtesting

Owns environment-only secrets with no logging or serialization, deterministic
historical replay through strategy and simulation, and reports for net P&L,
trade duration and fee drag. Fixture data lives in `data/fixtures/`. Unit and
integration tests for fee math, fills and backoff belong alongside the code
they exercise.
