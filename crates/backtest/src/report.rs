//! Performance report produced by a backtest, rendered as JSON or Markdown.

use std::fmt::Write as _;

use chrono::{DateTime, Utc};
use cs2_arb_core::{Error, FloatBucket, ItemId, Money, Platform, Result};
use serde::{Deserialize, Serialize};

pub const REPORT_DISCLAIMER: &str = "Mock simulation only. Results come from replaying \
recorded or synthetic snapshots through an in-process fill model; no real orders were placed.";

/// One closed round trip (a purchased lot, or part of one, that was later sold).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TradeRecord {
    pub strategy: String,
    pub position_id: String,
    pub item: ItemId,
    pub float_bucket: Option<FloatBucket>,
    pub quantity: u32,
    pub buy_platform: Platform,
    pub sell_platform: Platform,
    /// Purchase subtotal before fees.
    pub entry_subtotal: Money,
    pub entry_fee: Money,
    /// Sale subtotal before fees.
    pub exit_subtotal: Money,
    pub exit_fee: Money,
    /// `exit_subtotal - entry_subtotal`.
    pub gross_pnl: Money,
    /// `entry_fee + exit_fee`.
    pub fees: Money,
    /// `gross_pnl - fees`.
    pub net_pnl: Money,
    pub opened_at: DateTime<Utc>,
    pub locked_until: DateTime<Utc>,
    pub closed_at: DateTime<Utc>,
    /// `closed_at - opened_at`, which includes the trade hold.
    pub duration_secs: i64,
}

/// Aggregate statistics for one strategy, or for all strategies combined.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StrategyStats {
    pub strategy: String,
    pub orders_submitted: u32,
    pub orders_filled: u32,
    pub orders_rejected: u32,
    /// Closed round trips.
    pub trade_count: u32,
    pub winning_trades: u32,
    pub losing_trades: u32,
    /// Winning trades / trade count (0..=1). `None` without trades.
    pub win_rate: Option<f64>,
    /// Realized P&L of closed trades before fees.
    pub gross_pnl: Money,
    pub buy_fees: Money,
    pub sell_fees: Money,
    /// `buy_fees + sell_fees` on closed trades.
    pub total_fees: Money,
    /// Realized P&L after fees: `gross_pnl - total_fees`.
    pub net_pnl: Money,
    /// `total_fees / gross_pnl * 100` when gross P&L is positive.
    pub fee_drag_pct_of_gross: Option<f64>,
    /// `total_fees / (entry + exit subtotals) * 100` for closed trades.
    pub fee_pct_of_turnover: Option<f64>,
    /// Entry and exit subtotals of closed trades.
    pub closed_turnover: Money,
    pub open_positions: u32,
    /// Cost basis (including entry fees) of positions still open at the end.
    pub open_cost_basis: Money,
    /// Open positions marked at the best net-of-seller-fee quote, minus cost basis.
    pub unrealized_pnl: Money,
    /// `net_pnl + unrealized_pnl`.
    pub total_pnl: Money,
    pub avg_trade_duration_secs: Option<f64>,
    pub median_trade_duration_secs: Option<f64>,
    pub min_trade_duration_secs: Option<i64>,
    pub max_trade_duration_secs: Option<i64>,
    /// Largest peak-to-trough fall of mark-to-market equity (realized net +
    /// unrealized), with equity starting at zero. Always nonnegative.
    pub max_drawdown: Money,
    /// Mark-to-market equity after the final replay step.
    pub final_equity: Money,
}

/// Replay coverage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayWindow {
    pub start: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
    pub steps: u32,
    pub snapshots: u32,
    pub events_delivered: u32,
    pub events_total: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PerformanceReport {
    pub disclaimer: String,
    pub window: ReplayWindow,
    pub trade_hold_secs: i64,
    /// All strategies combined. Drawdown is computed on the combined equity curve.
    pub totals: StrategyStats,
    /// Per-strategy breakdown, in the order the strategies were supplied.
    pub strategies: Vec<StrategyStats>,
    /// Closed trades in close order.
    pub trades: Vec<TradeRecord>,
}

impl PerformanceReport {
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string_pretty(self).map_err(|err| Error::InvalidData(err.to_string()))
    }

    pub fn from_json(source: &str) -> Result<Self> {
        serde_json::from_str(source).map_err(|err| Error::InvalidData(err.to_string()))
    }

    pub fn strategy(&self, name: &str) -> Option<&StrategyStats> {
        self.strategies.iter().find(|s| s.strategy == name)
    }

    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "# Backtest performance report\n");
        let _ = writeln!(out, "> {}\n", self.disclaimer);
        let w = &self.window;
        let _ = writeln!(
            out,
            "Window: {} → {} · {} steps · {} snapshots · {}/{} events delivered · trade hold {}\n",
            w.start.map_or("n/a".into(), |t| t.to_rfc3339()),
            w.end.map_or("n/a".into(), |t| t.to_rfc3339()),
            w.steps,
            w.snapshots,
            w.events_delivered,
            w.events_total,
            fmt_duration(self.trade_hold_secs as f64),
        );

        let _ = writeln!(out, "## Summary\n");
        let mut columns: Vec<&StrategyStats> = vec![&self.totals];
        columns.extend(self.strategies.iter());
        let _ = write!(out, "| Metric |");
        for c in &columns {
            let _ = write!(out, " {} |", c.strategy);
        }
        let _ = write!(out, "\n| --- |");
        for _ in &columns {
            let _ = write!(out, " ---: |");
        }
        out.push('\n');
        type Row = (&'static str, fn(&StrategyStats) -> String);
        let rows: [Row; 21] = [
            ("Total P&L (net + unrealized)", |s| fmt_money(s.total_pnl)),
            ("Net realized P&L", |s| fmt_money(s.net_pnl)),
            ("Gross realized P&L", |s| fmt_money(s.gross_pnl)),
            ("Total fees", |s| fmt_money(s.total_fees)),
            ("Buy / sell fees", |s| {
                format!("{} / {}", fmt_money(s.buy_fees), fmt_money(s.sell_fees))
            }),
            ("Fee drag (% of gross)", |s| {
                fmt_pct(s.fee_drag_pct_of_gross)
            }),
            ("Fees (% of turnover)", |s| fmt_pct(s.fee_pct_of_turnover)),
            ("Closed turnover", |s| fmt_money(s.closed_turnover)),
            ("Trades", |s| s.trade_count.to_string()),
            ("Wins / losses", |s| {
                format!("{} / {}", s.winning_trades, s.losing_trades)
            }),
            ("Win rate", |s| fmt_pct(s.win_rate.map(|r| r * 100.0))),
            ("Avg trade duration", |s| {
                s.avg_trade_duration_secs.map_or("n/a".into(), fmt_duration)
            }),
            ("Median trade duration", |s| {
                s.median_trade_duration_secs
                    .map_or("n/a".into(), fmt_duration)
            }),
            ("Min trade duration", |s| {
                s.min_trade_duration_secs
                    .map_or("n/a".into(), |d| fmt_duration(d as f64))
            }),
            ("Max trade duration", |s| {
                s.max_trade_duration_secs
                    .map_or("n/a".into(), |d| fmt_duration(d as f64))
            }),
            ("Max drawdown", |s| fmt_money(s.max_drawdown)),
            ("Final equity", |s| fmt_money(s.final_equity)),
            ("Open positions", |s| s.open_positions.to_string()),
            ("Open cost basis", |s| fmt_money(s.open_cost_basis)),
            ("Unrealized P&L", |s| fmt_money(s.unrealized_pnl)),
            ("Orders submitted / filled / rejected", |s| {
                format!(
                    "{} / {} / {}",
                    s.orders_submitted, s.orders_filled, s.orders_rejected
                )
            }),
        ];
        for (label, value) in rows {
            let _ = write!(out, "| {label} |");
            for c in &columns {
                let _ = write!(out, " {} |", value(c));
            }
            out.push('\n');
        }

        let _ = writeln!(out, "\n## Trades\n");
        if self.trades.is_empty() {
            let _ = writeln!(out, "No closed trades.");
        } else {
            let _ = writeln!(
                out,
                "| Strategy | Item | Qty | Buy | Sell | Entry | Exit | Gross | Fees | Net | Opened | Closed | Duration |"
            );
            let _ = writeln!(
                out,
                "| --- | --- | ---: | --- | --- | ---: | ---: | ---: | ---: | ---: | --- | --- | ---: |"
            );
            for t in &self.trades {
                let _ = writeln!(
                    out,
                    "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                    t.strategy,
                    t.item.market_hash_name.replace('|', "\\|"),
                    t.quantity,
                    platform_name(t.buy_platform),
                    platform_name(t.sell_platform),
                    fmt_money(t.entry_subtotal),
                    fmt_money(t.exit_subtotal),
                    fmt_money(t.gross_pnl),
                    fmt_money(t.fees),
                    fmt_money(t.net_pnl),
                    t.opened_at.format("%Y-%m-%d %H:%M"),
                    t.closed_at.format("%Y-%m-%d %H:%M"),
                    fmt_duration(t.duration_secs as f64),
                );
            }
        }
        out
    }
}

pub(crate) fn platform_name(platform: Platform) -> &'static str {
    match platform {
        Platform::Skinport => "Skinport",
        Platform::CsFloat => "CSFloat",
        Platform::DMarket => "DMarket",
    }
}

/// `-$12.34` style formatting of USD cents.
pub fn fmt_money(money: Money) -> String {
    let sign = if money.0 < 0 { "-" } else { "" };
    let abs = money.0.unsigned_abs();
    format!("{sign}${}.{:02}", abs / 100, abs % 100)
}

fn fmt_pct(value: Option<f64>) -> String {
    value.map_or("n/a".into(), |v| format!("{v:.2}%"))
}

/// `7d 3h 0m` style formatting of a number of seconds.
pub fn fmt_duration(secs: f64) -> String {
    let total = secs.round() as i64;
    let sign = if total < 0 { "-" } else { "" };
    let total = total.unsigned_abs();
    format!(
        "{sign}{}d {}h {}m",
        total / 86_400,
        (total % 86_400) / 3_600,
        (total % 3_600) / 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_money_and_durations() {
        assert_eq!(fmt_money(Money(0)), "$0.00");
        assert_eq!(fmt_money(Money(12345)), "$123.45");
        assert_eq!(fmt_money(Money(-5)), "-$0.05");
        assert_eq!(fmt_money(Money(i64::MIN)), "-$92233720368547758.08");
        assert_eq!(fmt_duration(7.0 * 86_400.0), "7d 0h 0m");
        assert_eq!(fmt_duration(90_061.0), "1d 1h 1m");
    }
}
