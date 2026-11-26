//! Runs the built-in spread + dip strategies over `data/fixtures` and checks the
//! report invariants. Fully offline.

use std::path::PathBuf;

use chrono::Duration;
use cs2_arb_backtest::fixtures::{load_events, load_fee_schedules, load_snapshots};
use cs2_arb_backtest::replay::event_time;
use cs2_arb_backtest::{replay, PerformanceReport, StrategyStats};
use cs2_arb_core::{MarketEvent, Money, Platform, PriceSnapshot, SimulationMode};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn inputs() -> (
    Vec<PriceSnapshot>,
    Vec<MarketEvent>,
    cs2_arb_core::FeeSchedules,
) {
    let root = root();
    (
        load_snapshots(root.join("data/fixtures/price_snapshots.json")).unwrap(),
        load_events(root.join("data/fixtures/market_events.json")).unwrap(),
        load_fee_schedules(root.join("config/default.toml")).unwrap(),
    )
}

fn run() -> PerformanceReport {
    let (snapshots, events, fees) = inputs();
    replay(SimulationMode, &snapshots, &events, fees).unwrap()
}

fn check_stats(s: &StrategyStats, hold_secs: i64) {
    assert_eq!(
        s.net_pnl.0,
        s.gross_pnl.0 - s.total_fees.0,
        "{}",
        s.strategy
    );
    assert_eq!(s.total_fees.0, s.buy_fees.0 + s.sell_fees.0);
    assert!(s.buy_fees.0 >= 0 && s.sell_fees.0 >= 0);
    assert_eq!(s.total_pnl.0, s.net_pnl.0 + s.unrealized_pnl.0);
    assert_eq!(s.final_equity, s.total_pnl, "{}", s.strategy);
    assert!(s.max_drawdown.0 >= 0);
    assert!(s.winning_trades + s.losing_trades <= s.trade_count);
    assert_eq!(s.orders_submitted, s.orders_filled + s.orders_rejected);
    match s.win_rate {
        Some(rate) => {
            assert!((0.0..=1.0).contains(&rate));
            assert!((rate - f64::from(s.winning_trades) / f64::from(s.trade_count)).abs() < 1e-12);
        }
        None => assert_eq!(s.trade_count, 0),
    }
    match s.fee_drag_pct_of_gross {
        Some(pct) => {
            assert!(s.gross_pnl.0 > 0);
            assert!((pct - s.total_fees.0 as f64 / s.gross_pnl.0 as f64 * 100.0).abs() < 1e-9);
        }
        None => assert!(s.gross_pnl.0 <= 0),
    }
    if s.trade_count > 0 {
        let avg = s.avg_trade_duration_secs.unwrap();
        let median = s.median_trade_duration_secs.unwrap();
        let min = s.min_trade_duration_secs.unwrap();
        let max = s.max_trade_duration_secs.unwrap();
        assert!(min >= hold_secs, "{}: trade shorter than hold", s.strategy);
        assert!(min as f64 <= avg && avg <= max as f64);
        assert!(min as f64 <= median && median <= max as f64);
    } else {
        assert!(s.avg_trade_duration_secs.is_none());
        assert!(s.median_trade_duration_secs.is_none());
    }
}

#[test]
fn fixture_backtest_report_invariants() {
    let report = run();
    let (snapshots, events, _) = inputs();
    println!("{}", report.to_markdown());

    assert_eq!(report.trade_hold_secs, Duration::days(7).num_seconds());
    assert_eq!(report.window.snapshots, 600);
    assert_eq!(report.window.steps, 50);
    assert_eq!(report.window.events_total, 3);
    assert_eq!(report.window.events_delivered, 3);
    let start = report.window.start.unwrap();
    let end = report.window.end.unwrap();

    check_stats(&report.totals, report.trade_hold_secs);
    let names: Vec<&str> = report
        .strategies
        .iter()
        .map(|s| s.strategy.as_str())
        .collect();
    assert_eq!(names, ["spread", "dip"]);
    for s in &report.strategies {
        check_stats(s, report.trade_hold_secs);
    }

    // Per-strategy breakdown adds up to the totals.
    let sum = |f: fn(&StrategyStats) -> i64| report.strategies.iter().map(f).sum::<i64>();
    assert_eq!(sum(|s| s.gross_pnl.0), report.totals.gross_pnl.0);
    assert_eq!(sum(|s| s.net_pnl.0), report.totals.net_pnl.0);
    assert_eq!(sum(|s| s.total_fees.0), report.totals.total_fees.0);
    assert_eq!(sum(|s| s.unrealized_pnl.0), report.totals.unrealized_pnl.0);
    assert_eq!(
        sum(|s| i64::from(s.trade_count)),
        i64::from(report.totals.trade_count)
    );
    assert_eq!(
        sum(|s| i64::from(s.open_positions)),
        i64::from(report.totals.open_positions)
    );
    // Combined drawdown cannot exceed the sum of the parts.
    assert!(report.totals.max_drawdown.0 <= sum(|s| s.max_drawdown.0));

    // The trade list reconciles with the aggregates and honours the hold.
    assert_eq!(report.trades.len() as u32, report.totals.trade_count);
    assert_eq!(
        report.trades.iter().map(|t| t.net_pnl.0).sum::<i64>(),
        report.totals.net_pnl.0
    );
    for t in &report.trades {
        assert_eq!(t.gross_pnl.0, t.exit_subtotal.0 - t.entry_subtotal.0);
        assert_eq!(t.fees.0, t.entry_fee.0 + t.exit_fee.0);
        assert_eq!(t.net_pnl.0, t.gross_pnl.0 - t.fees.0);
        assert_eq!(t.locked_until, t.opened_at + Duration::days(7));
        assert!(t.closed_at >= t.locked_until, "sold while trade-locked");
        assert!(start <= t.opened_at && t.closed_at <= end);
        assert_eq!(t.duration_secs, (t.closed_at - t.opened_at).num_seconds());
        assert!(t.quantity > 0);
    }
    assert!(report
        .trades
        .windows(2)
        .all(|w| w[0].closed_at <= w[1].closed_at));

    // The dip strategy only enters inside the panic window and profits from the DMarket dip.
    let opened = events.iter().find_map(|e| match e {
        MarketEvent::PanicWindowOpened { .. } => Some(event_time(e)),
        _ => None,
    });
    let closed = events.iter().find_map(|e| match e {
        MarketEvent::PanicWindowClosed { .. } => Some(event_time(e)),
        _ => None,
    });
    let window = opened.unwrap()..closed.unwrap();
    let dip = report.strategy("dip").unwrap();
    assert!(dip.trade_count >= 1);
    assert!(dip.net_pnl.0 > 0);
    for t in report.trades.iter().filter(|t| t.strategy == "dip") {
        assert!(window.contains(&t.opened_at));
        assert_eq!(t.buy_platform, Platform::DMarket);
        assert_eq!(t.item.market_hash_name, "AK-47 | Redline (Field-Tested)");
        assert!(t.net_pnl.0 > 0);
    }
    // Configured seller fees actually hit the P&L.
    assert!(report.totals.total_fees > Money(0));
    assert!(report.totals.net_pnl < report.totals.gross_pnl);

    // Input order does not matter here: fully reversed snapshots and events give
    // the identical report (the built-in strategies are per-item independent).
    let (_, _, fees) = inputs();
    let mut reversed = snapshots.clone();
    reversed.reverse();
    let mut events = events.clone();
    events.reverse();
    let again = replay(SimulationMode, &reversed, &events, fees).unwrap();
    assert_eq!(again, report);
}

#[test]
fn report_renders_as_json_and_markdown() {
    let report = run();
    let json = report.to_json().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["totals"]["strategy"], "all");
    assert_eq!(parsed["strategies"].as_array().unwrap().len(), 2);
    assert_eq!(
        parsed["totals"]["net_pnl"].as_i64().unwrap(),
        report.totals.net_pnl.0
    );
    assert_eq!(PerformanceReport::from_json(&json).unwrap(), report);

    let md = report.to_markdown();
    assert!(md.starts_with("# Backtest performance report"));
    assert!(md.contains("Mock simulation only"));
    assert!(md.contains("| Metric | all | spread | dip |"));
    assert!(md.contains("Fee drag (% of gross)"));
    assert!(md.contains("Median trade duration"));
    assert!(md.contains("Max drawdown"));
    assert!(md.contains("AK-47 \\| Redline (Field-Tested)"));
    // One table row per trade.
    let trade_rows = md
        .lines()
        .skip_while(|l| !l.starts_with("## Trades"))
        .filter(|l| l.starts_with("| spread |") || l.starts_with("| dip |"))
        .count();
    assert_eq!(trade_rows, report.trades.len());
}

#[test]
fn backtest_is_deterministic() {
    assert_eq!(run(), run());
    assert_eq!(run().to_json().unwrap(), run().to_json().unwrap());
}
