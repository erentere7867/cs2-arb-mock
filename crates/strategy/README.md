# Workstream E — Strategies and risk

Owns cross-market net spread calculations including seven-day opportunity cost,
and panic-window dip detection (>2 standard deviations below a 30-day baseline
for liquid items). Gates simulated orders on position sizing, daily drawdown and
an emergency kill switch.

## API (mock only)

- `arbitrage::evaluate_spread(quotes, fees, &SpreadConfig, now)` returns `SpreadBreakdown`s
  with expected net profit > `min_net_profit` after buyer/seller fees, the opportunity
  cost `buy_cost * annual_rate * hold_days / 365` (7 days default) and an optional drift
  haircut. `spread_breakdown` exposes the math for one pair.
- `dip::DipDetector` keeps 30-day rolling mean/sample-sd per platform+item+bucket and
  signals a buy when `price < mean - 2 sd`, >= 10 sales in 7 days and an update/panic
  window (from `MarketEvent`) is active. `dip::evaluate_dip` is a batch helper.
- `risk::RiskManager` enforces position, item and total exposure limits, halts entries for
  the rest of the UTC day once the intraday drawdown reaches the cap, and consults
  `kill_switch::KillSwitch` (AtomicBool, `KILL_SWITCH` file, `KILL_SWITCH` env) to block
  every new order.
- `executor::SimExecutor` is a local trait the simulator is adapted to;
  `submit_with_risk` gates every order through the risk manager first.
