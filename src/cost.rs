//! LLM cost accounting + the progressive output formatter (`looop _fmt`).
//!
//! Every AI call is metered through one of two seams: the tick/goal runner
//! (piped through `_fmt`, which sums per-message USD) and worker sessions (which
//! self-report via `looop _cost`). Both append one JSON line to the cost ledger.
//! `looop cost` reports over it.

use crate::config::Config;
use crate::paths::Paths;
use anyhow::Result;
use std::fs::OpenOptions;
use std::io::{BufRead, Write};
use std::process::ExitCode;

/// Total USD recorded in the ledger for the current LOCAL day. The ledger is the
/// single source of truth for spend (both tick `_fmt` and worker `_cost` append
/// to it), so summing it survives pulse restarts (H2 — the daily cap must be
/// process-independent state on disk, not in-memory loop state).
pub fn spent_today(paths: &Paths) -> f64 {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let Ok(text) = std::fs::read_to_string(paths.cost_ledger()) else {
        return 0.0;
    };
    text.lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|r| {
            r.get("ts")
                .and_then(|t| t.as_str())
                .map(|ts| local_day(ts) == today)
                .unwrap_or(false)
        })
        .filter_map(|r| r.get("cost_usd").and_then(|c| c.as_f64()))
        .sum()
}

/// The configured daily spend ceiling (`max_daily_usd`), if set to a positive
/// number. `None` disables the circuit breaker (the default).
pub fn daily_budget(cfg: &Config) -> Option<f64> {
    cfg.root
        .get("max_daily_usd")
        .and_then(|v| v.as_f64().or_else(|| v.as_u64().map(|n| n as f64)))
        .filter(|x| *x > 0.0)
}

/// Append one ledger line if `cost` parses to a positive amount.
pub fn record_cost(paths: &Paths, kind: &str, id: &str, runner: &str, cost: &str) {
    let Ok(amount) = cost.trim().parse::<f64>() else {
        return;
    };
    // Record only positive amounts; NaN and <= 0 are dropped (world-unchanged
    // ticks, runners that emit no usage data). Phrased as a positive branch so
    // NaN is excluded without a negated float comparison.
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

/// `looop _cost <kind> <id> <runner> <usd>` — a worker self-reporting its spend.
pub fn cmd_cost_record(paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let kind = args.first().map(String::as_str).unwrap_or("");
    let id = args.get(1).map(String::as_str).unwrap_or("");
    let runner = args.get(2).map(String::as_str).unwrap_or("");
    let cost = args.get(3).map(String::as_str).unwrap_or("");
    record_cost(paths, kind, id, runner, cost);
    Ok(ExitCode::SUCCESS)
}

/// `looop _fmt` — read a `pi --mode json` NDJSON stream on stdin, print friendly
/// progress live, and (when LOOOP_COST_* is set) meter spend into the ledger.
pub fn cmd_fmt(paths: &Paths) -> Result<ExitCode> {
    let metering = std::env::var("LOOOP_COST_KIND").is_ok();
    // pi emits per-message usage we SUM; claude (stream-json) emits one final
    // `result` event with a CUMULATIVE total_cost_usd we take as-is. Tracking
    // them separately lets one `_fmt` meter either runner (H3) — the claude
    // total, when present, wins so we never double-count.
    let mut pi_total = 0.0f64;
    let mut claude_total: Option<f64> = None;
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if let Some(rendered) = format_line(&line) {
            let _ = writeln!(stdout, "{rendered}");
            let _ = stdout.flush();
        }
        if metering && let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
            match v.get("type").and_then(|t| t.as_str()) {
                // pi --mode json: sum per-message cost.
                Some("message_end") => {
                    pi_total += v
                        .pointer("/usage/cost/total")
                        .and_then(|c| c.as_f64())
                        .unwrap_or(0.0);
                }
                // claude --output-format stream-json: single cumulative total.
                Some("result") => {
                    if let Some(c) = v.get("total_cost_usd").and_then(|c| c.as_f64()) {
                        claude_total = Some(c);
                    }
                }
                _ => {}
            }
        }
    }

    if metering {
        let total = claude_total.unwrap_or(pi_total);
        let kind = std::env::var("LOOOP_COST_KIND").unwrap_or_default();
        let id = std::env::var("LOOOP_COST_ID").unwrap_or_default();
        let runner = std::env::var("LOOOP_COST_RUNNER").unwrap_or_default();
        record_cost(paths, &kind, &id, &runner, &format!("{total:.6}"));
    }
    Ok(ExitCode::SUCCESS)
}

/// Render one NDJSON event line; `None` means "emit nothing" (mirrors jq empty).
fn format_line(line: &str) -> Option<String> {
    use crate::util::{cyan, dim, red, rst};
    let Ok(e) = serde_json::from_str::<serde_json::Value>(line) else {
        // Non-JSON: pass through unchanged, but swallow empty lines.
        return if line.is_empty() {
            None
        } else {
            Some(line.to_string())
        };
    };
    let ty = e.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match ty {
        "tool_execution_start" => {
            let name = e.get("toolName").and_then(|t| t.as_str()).unwrap_or("tool");
            let args = e.get("args");
            let raw = args
                .and_then(|a| a.get("command"))
                .or_else(|| args.and_then(|a| a.get("path")))
                .or_else(|| args.and_then(|a| a.get("file_path")))
                .and_then(|v| v.as_str().map(str::to_owned))
                .or_else(|| args.map(|a| a.to_string()))
                .unwrap_or_default();
            let collapsed: String = collapse_ws(&raw).chars().take(100).collect();
            let argpart = if collapsed.is_empty() {
                String::new()
            } else {
                format!("{}: {}{}", dim(), collapsed, rst())
            };
            Some(format!("  {}→ {}{}{}", cyan(), name, rst(), argpart))
        }
        "tool_execution_end" if e.get("isError").and_then(|b| b.as_bool()).unwrap_or(false) => {
            let name = e.get("toolName").and_then(|t| t.as_str()).unwrap_or("tool");
            Some(format!("  {}✗ {} failed{}", red(), name, rst()))
        }
        "message_end"
            if e.pointer("/message/role").and_then(|r| r.as_str()) == Some("assistant") =>
        {
            let text: String = e
                .pointer("/message/content")
                .and_then(|c| c.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("text"))
                        .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                        .collect::<String>()
                })
                .unwrap_or_default();
            if text.is_empty() {
                None
            } else {
                Some(format!("\n{text}"))
            }
        }
        _ => None,
    }
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
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
            "  ledger: {}  (written as the pulse/goals run; see 'looop help')",
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
    use serde_json::json;

    #[test]
    fn usd_formats_like_jq_def() {
        assert_eq!(usd(0.0), "$0");
        assert_eq!(usd(-0.0), "$0"); // no -0.0
        assert_eq!(usd(1.0), "$1");
        assert_eq!(usd(1.5), "$1.5");
        assert_eq!(usd(0.12345), "$0.1235"); // rounds at 4dp
        assert_eq!(usd(0.00004), "$0"); // rounds below 4dp to zero
        assert_eq!(usd(12.3400), "$12.34"); // trailing zeros trimmed
    }

    #[test]
    fn local_day_parses_valid_and_rejects_garbage() {
        let d = local_day("2026-06-18T12:00:00Z");
        assert_eq!(d.len(), 10, "yyyy-mm-dd is 10 chars");
        assert_eq!(d.matches('-').count(), 2);
        assert_eq!(local_day("not-a-date"), "");
    }

    #[test]
    fn collapse_ws_squeezes_all_whitespace() {
        assert_eq!(collapse_ws("  a\t b\n c "), "a b c");
        assert_eq!(collapse_ws(""), "");
    }

    #[test]
    fn format_line_passthrough_and_empty() {
        // Non-JSON passes through; blank lines are swallowed.
        assert_eq!(format_line("plain text"), Some("plain text".to_string()));
        assert_eq!(format_line(""), None);
    }

    #[test]
    fn format_line_assistant_text_and_skips() {
        let msg = json!({
            "type": "message_end",
            "message": { "role": "assistant", "content": [ { "type": "text", "text": "hi" } ] }
        })
        .to_string();
        assert_eq!(format_line(&msg), Some("\nhi".to_string()));

        // Empty assistant text emits nothing.
        let empty = json!({
            "type": "message_end",
            "message": { "role": "assistant", "content": [] }
        })
        .to_string();
        assert_eq!(format_line(&empty), None);

        // Unknown event types emit nothing.
        let other = json!({ "type": "session_start" }).to_string();
        assert_eq!(format_line(&other), None);
    }

    #[test]
    fn daily_budget_reads_positive_only() {
        let cfg = |v: serde_json::Value| Config { root: v };
        assert_eq!(daily_budget(&cfg(json!({"max_daily_usd": 5.0}))), Some(5.0));
        assert_eq!(daily_budget(&cfg(json!({"max_daily_usd": 10}))), Some(10.0));
        assert_eq!(daily_budget(&cfg(json!({"max_daily_usd": 0}))), None);
        assert_eq!(daily_budget(&cfg(json!({}))), None);
    }

    #[test]
    fn spent_today_sums_only_todays_rows() {
        let p = Paths::temp();
        let today = chrono::Local::now().to_rfc3339();
        let line = |ts: &str, c: f64| {
            format!(r#"{{"ts":"{ts}","kind":"tick","id":"x","runner":"pi","cost_usd":{c}}}"#)
        };
        let body = format!(
            "{}\n{}\n{}\n",
            line(&today, 0.5),
            line(&today, 1.25),
            line("2000-01-01T00:00:00Z", 9.0), // ancient row excluded
        );
        std::fs::write(p.cost_ledger(), body).unwrap();
        assert!((spent_today(&p) - 1.75).abs() < 1e-9);
    }

    #[test]
    fn record_cost_appends_only_positive_amounts() {
        let p = Paths::temp();
        record_cost(&p, "tick", "id1", "pi", "0.5");
        record_cost(&p, "tick", "id2", "pi", "0"); // dropped
        record_cost(&p, "tick", "id3", "pi", "not-a-number"); // dropped
        record_cost(&p, "goal", "id4", "pi", "1.25");

        let text = std::fs::read_to_string(p.cost_ledger()).unwrap();
        let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 2, "only the two positive amounts are recorded");
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["cost_usd"].as_f64(), Some(0.5));
        assert_eq!(first["kind"].as_str(), Some("tick"));
    }
}
