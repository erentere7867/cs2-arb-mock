use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::Serialize;

const HISTORY_LIMIT: usize = 24;
const REPORT_LIMIT: u64 = 1024 * 1024;
const SUMMARY: &str = "## Summary";
const PEG: &str = "### Realized Buy-Order P&L";
const ACTIVE_BIDS: &str = "### Active Mock Bids";
const LOCKED_INVENTORY: &str = "### Locked Inventory";

#[derive(Default, Serialize)]
struct Pnl {
    total: Option<i64>,
    net: Option<i64>,
    gross: Option<i64>,
    unrealized: Option<i64>,
}

#[derive(Serialize)]
struct Strategy {
    name: String,
    pnl: Pnl,
}

#[derive(Serialize)]
struct Batch {
    at: DateTime<Utc>,
    total: Option<i64>,
}

#[derive(Serialize)]
struct Latest {
    strategies: Vec<Strategy>,
    peg: BTreeMap<String, String>,
    active_bids: Option<ReportTable>,
    locked_inventory: Option<ReportTable>,
    markdown: String,
}

#[derive(Serialize)]
struct ReportTable {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
}

#[derive(Default, Serialize)]
pub struct GameReports {
    history: Vec<Batch>,
    latest: Option<Latest>,
    pending_since: Option<DateTime<Utc>>,
    cadence_secs: Option<i64>,
    unavailable: bool,
}

pub fn read_game(path: &Path) -> GameReports {
    let mut output = GameReports::default();
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) => {
            output.unavailable = error.kind() != io::ErrorKind::NotFound;
            return output;
        }
    };
    output.pending_since = fs::symlink_metadata(path.join(".in-progress"))
        .ok()
        .filter(|metadata| metadata.is_file())
        .and_then(|metadata| metadata.created().or_else(|_| metadata.modified()).ok())
        .map(DateTime::from);

    let mut batches = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            output.unavailable = true;
            continue;
        };
        let Some(at) = entry.file_name().to_str().and_then(report_time) else {
            continue;
        };
        if entry.file_type().is_ok_and(|kind| kind.is_file()) {
            batches.push((at, entry.path()));
        }
    }
    batches.sort_unstable_by_key(|(at, _)| std::cmp::Reverse(*at));
    for (index, (at, path)) in batches.into_iter().take(HISTORY_LIMIT).enumerate() {
        let report = read_report(&path).ok();
        output.unavailable |= report.is_none();
        let strategies = report.as_deref().map(parse_summary).unwrap_or_default();
        let total = strategies.first().and_then(|strategy| strategy.pnl.total);
        output.history.push(Batch { at, total });
        if index == 0 {
            output.latest = report.map(|markdown| Latest {
                peg: parse_metrics(&markdown, PEG)
                    .map(|(name, data)| (name.to_owned(), data[0].to_owned()))
                    .collect(),
                active_bids: parse_detail(&markdown, ACTIVE_BIDS),
                locked_inventory: parse_detail(&markdown, LOCKED_INVENTORY),
                strategies,
                markdown,
            });
        }
    }

    if let [first, second, ..] = &output.history[..] {
        output.cadence_secs = Some((first.at - second.at).num_seconds());
    }
    output
}

fn report_time(name: &str) -> Option<DateTime<Utc>> {
    let stamp = name.strip_suffix("Z.md")?;
    if stamp.len() != 25 || stamp.as_bytes().get(8) != Some(&b'T') {
        return None;
    }
    chrono::NaiveDateTime::parse_from_str(stamp, "%Y%m%dT%H%M%S%.9f")
        .ok()
        .map(|time| time.and_utc())
}

fn read_report(path: &Path) -> io::Result<String> {
    let mut body = String::new();
    File::open(path)?
        .take(REPORT_LIMIT + 1)
        .read_to_string(&mut body)?;
    if body.len() as u64 > REPORT_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "report too large",
        ));
    }
    Ok(body)
}

fn table<'a>(report: &'a str, section: &'a str) -> impl Iterator<Item = Vec<&'a str>> + 'a {
    report
        .lines()
        .skip_while(move |line| *line != section)
        .skip(1)
        .skip_while(|line| line.trim().is_empty())
        .take_while(|line| line.starts_with('|'))
        .map(|line| {
            let body = line.strip_prefix('|').unwrap_or(line);
            let body = body.strip_suffix('|').unwrap_or(body);
            let mut start = 0;
            let mut fields = Vec::new();
            for (i, _) in body.match_indices('|') {
                if i == 0 || body.as_bytes()[i - 1] != b'\\' {
                    fields.push(body[start..i].trim());
                    start = i + 1;
                }
            }
            fields.push(body[start..].trim());
            fields
        })
}

fn parse_detail(report: &str, section: &str) -> Option<ReportTable> {
    if !report.lines().any(|line| line == section) {
        return None;
    }
    let decode = |fields: Vec<&str>| {
        fields
            .into_iter()
            .map(|field| field.replace("\\|", "|"))
            .collect()
    };
    let mut rows = table(report, section);
    let headers: Vec<String> = rows.next().map(decode).unwrap_or_default();
    let rows = rows
        .skip(1)
        .filter(|row| row.len() == headers.len())
        .map(decode)
        .collect();
    Some(ReportTable { headers, rows })
}

fn parse_metrics<'a>(
    report: &'a str,
    section: &'a str,
) -> impl Iterator<Item = (&'a str, Vec<&'a str>)> + 'a {
    table(report, section).skip(2).filter_map(|mut fields| {
        (fields.len() > 1).then(|| {
            let name = fields.remove(0);
            (name, fields)
        })
    })
}

fn parse_summary(report: &str) -> Vec<Strategy> {
    let mut strategies: Vec<_> = table(report, SUMMARY)
        .next()
        .into_iter()
        .flatten()
        .skip(1)
        .map(|name| Strategy {
            name: name.to_owned(),
            pnl: Pnl::default(),
        })
        .collect();
    for (label, values) in parse_metrics(report, SUMMARY) {
        for (strategy, value) in strategies.iter_mut().zip(values) {
            let field = match label {
                "Total P&L (net + unrealized)" => &mut strategy.pnl.total,
                "Net realized P&L" => &mut strategy.pnl.net,
                "Gross realized P&L" => &mut strategy.pnl.gross,
                "Unrealized P&L" => &mut strategy.pnl.unrealized,
                _ => continue,
            };
            *field = money(value);
        }
    }
    strategies
}

fn money(raw: &str) -> Option<i64> {
    let negative = raw.starts_with('-');
    let (dollars, cents) = raw
        .strip_prefix('-')
        .unwrap_or(raw)
        .strip_prefix('$')?
        .split_once('.')?;
    if dollars.is_empty()
        || cents.len() != 2
        || !dollars
            .bytes()
            .chain(cents.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    format!("{}{dollars}{cents}", if negative { "-" } else { "" })
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_peg_detail_tables_with_escaped_item_names() {
        let report = r"
### Active Mock Bids

| Platform | Item | Bid |
| --- | --- | ---: |
| CsFloat | AK-47 \| Redline | $8.50 |

### Locked Inventory

_None_
";
        let bids = parse_detail(report, ACTIVE_BIDS).unwrap();
        assert_eq!(bids.headers, ["Platform", "Item", "Bid"]);
        assert_eq!(bids.rows[0], ["CsFloat", "AK-47 | Redline", "$8.50"]);
        assert!(parse_detail(report, LOCKED_INVENTORY)
            .unwrap()
            .rows
            .is_empty());
        assert!(parse_detail("", LOCKED_INVENTORY).is_none());
    }

    #[test]
    fn parses_live_filename_and_signed_money() {
        let at = report_time("20261127T051211.123456789Z.md").unwrap();
        assert_eq!(at.timestamp_subsec_nanos(), 123_456_789);
        assert!(report_time(".in-progress").is_none());
        assert!(report_time("backtest.md").is_none());
        assert_eq!(money("-$12.34"), Some(-1234));
        assert_eq!(money("$0.00"), Some(0));
        assert_eq!(money("n/a"), None);
        assert_eq!(money("$1.1"), None);
    }

    #[test]
    fn parses_report_pnl_by_strategy_and_separate_peg_metrics() {
        let report = "\
## Summary

| Metric | total | spread | dip |
| --- | ---: | ---: | ---: |
| Total P&L (net + unrealized) | $1.00 | n/a | $1.00 |
| Net realized P&L | -$1.50 | $0.00 | -$1.50 |
| Gross realized P&L | $1.25 | $0.00 | $1.25 |
| Unrealized P&L | $2.50 | $0.00 | $2.50 |

### Realized Buy-Order P&L

| Metric | Value |
| --- | ---: |
| Mock Bids Placed | 10 |
| Realized Buy-Order P&L | $12.99 |
";
        let s = parse_summary(report);
        assert_eq!(s[1].name, "spread");
        assert_eq!(s[1].pnl.total, None);
        assert_eq!(s[2].pnl.total, Some(100));
        assert_eq!(s[0].pnl.net, Some(-150));
        assert_eq!(s[0].pnl.gross, Some(125));
        assert_eq!(s[0].pnl.unrealized, Some(250));
        let metrics: BTreeMap<_, _> = parse_metrics(report, PEG).collect();
        assert_eq!(metrics["Mock Bids Placed"], ["10"]);
        assert_eq!(metrics["Realized Buy-Order P&L"], ["$12.99"]);
    }
}
