# CLI

Owns the `run-mock`, `backtest`, and `watch` entry points. Simulation runs
offline by default. `run-mock --live-market-data` explicitly enables read-only
quotes and sale records; neither that flag nor any other command can place
real orders.
