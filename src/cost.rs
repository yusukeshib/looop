//! LLM cost accounting — the append-only spend ledger and its report.
//!
//! looop no longer runs its own LLM (judgment moved to the root agent), so the
//! in-process tick meter and the daily-budget breaker are gone. What remains is
//! a simple ledger: agents (workers, and the root agent) self-report their spend
//! via `looop _ cost <kind> <id> <runner> <usd>`, which appends one JSON line;
//! `looop cost` reports over it.

use crate::paths::Paths;
use anyhow::Result;
use std::fs::OpenOptions;
use std::io::Write;
use std::process::ExitCode;

/// Append one ledger line if `cost` parses to a positive amount.
pub fn record_cost(paths: &Paths, kind: &str, id: &str, runner: &str, cost: &str) {
    let Ok(amount) = cost.trim().parse::<f64>() else {
        return;
    };
    // Record only positive amounts; NaN and <= 0 are dropped. Phrased as a
    // positive branch so NaN is excluded without a negated float comparison.
    if amount > 0.0 {
        let line = serde_json::json!({
            "ts": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            "kind": kind,
            "id": id,
            "runner": runner,
            "cost_usd": amount,
        })
        .to_string();
        if let Ok(mut f) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(paths.cost_ledger())
        {
            let _ = writeln!(f, "{line}");
        }
    }
}

/// `looop _ cost <kind> <id> <runner> <usd>` — an agent self-reporting its spend.
pub fn cmd_cost_record(paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let kind = args.first().map(String::as_str).unwrap_or("");
    let id = args.get(1).map(String::as_str).unwrap_or("");
    let runner = args.get(2).map(String::as_str).unwrap_or("");
    let cost = args.get(3).map(String::as_str).unwrap_or("");
    record_cost(paths, kind, id, runner, cost);
    Ok(ExitCode::SUCCESS)
}

// ---- looop cost (report) ----------------------------------------------------

fn usd(amount: f64) -> String {
    // Round to 4 decimals, trim trailing zeros (parity with the jq `usd` def).
    let rounded = (amount * 10000.0).round() / 10000.0;
    let rounded = if rounded == 0.0 { 0.0 } else { rounded }; // kill -0.0
    let mut s = format!("{rounded:.4}");
    if s.contains('.') {
        s = s.trim_end_matches('0').trim_end_matches('.').to_string();
    }
    format!("${s}")
}

fn local_day(ts: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(ts)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%Y-%m-%d")
                .to_string()
        })
        .unwrap_or_default()
}

pub fn cmd_cost(paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let ledger = paths.cost_ledger();
    let mode = args.first().map(String::as_str).unwrap_or("all");

    if !ledger.is_file() {
        println!("looop: no LLM cost recorded yet.");
        println!(
            "  ledger: {}  (agents self-report via 'looop _ cost'; see 'looop help')",
            ledger.display()
        );
        return Ok(ExitCode::SUCCESS);
    }

    let text = std::fs::read_to_string(&ledger).unwrap_or_default();
    let rows: Vec<serde_json::Value> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v.is_object())
        .collect();

    if mode == "--json" {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(ExitCode::SUCCESS);
    }

    let today = match mode {
        "all" => String::new(),
        "today" => chrono::Local::now().format("%Y-%m-%d").to_string(),
        _ => {
            eprintln!("usage: looop cost [today|all|--json]");
            return Ok(ExitCode::from(1));
        }
    };

    let filtered: Vec<&serde_json::Value> = rows
        .iter()
        .filter(|r| {
            today.is_empty()
                || r.get("ts")
                    .and_then(|t| t.as_str())
                    .map(|ts| local_day(ts) == today)
                    .unwrap_or(false)
        })
        .collect();

    let cost_of = |r: &serde_json::Value| r.get("cost_usd").and_then(|c| c.as_f64()).unwrap_or(0.0);
    let total: f64 = filtered.iter().map(|r| cost_of(r)).sum();

    let scope = if today.is_empty() {
        "all time".to_string()
    } else {
        format!("today ({today} local)")
    };
    println!("looop cost — {scope}");
    println!("  total: {}  ({} calls)", usd(total), filtered.len());

    if !filtered.is_empty() {
        let group = |key: &str| -> Vec<(String, f64)> {
            let mut map: std::collections::BTreeMap<String, f64> =
                std::collections::BTreeMap::new();
            for r in &filtered {
                let k = r
                    .get(key)
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
                    .to_string();
                *map.entry(k).or_insert(0.0) += cost_of(r);
            }
            map.into_iter().collect()
        };
        println!("  by kind:");
        for (k, v) in group("kind") {
            println!("    {k}: {}", usd(v));
        }
        println!("  by runner:");
        for (k, v) in group("runner") {
            println!("    {k}: {}", usd(v));
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usd_formats_like_jq_def() {
        assert_eq!(usd(0.0), "$0");
        assert_eq!(usd(-0.0), "$0");
        assert_eq!(usd(1.5), "$1.5");
        assert_eq!(usd(0.12345), "$0.1235"); // 4-dp round
        assert_eq!(usd(2.0), "$2");
    }

    #[test]
    fn local_day_parses_valid_and_rejects_garbage() {
        assert_eq!(local_day("not-a-date"), "");
        let d = local_day("2024-01-02T03:04:05Z");
        assert_eq!(d.len(), 10, "YYYY-MM-DD");
    }

    #[test]
    fn record_cost_appends_only_positive_amounts() {
        let p = Paths::temp();
        record_cost(&p, "session", "triage", "pi", "0.42");
        record_cost(&p, "session", "noop", "pi", "0"); // dropped
        record_cost(&p, "session", "bad", "pi", "nope"); // dropped
        let text = std::fs::read_to_string(p.cost_ledger()).unwrap();
        let rows: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(rows.len(), 1, "only the positive amount is recorded");
        assert!(rows[0].contains("\"cost_usd\":0.42"));
    }
}
