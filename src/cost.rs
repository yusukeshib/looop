//! LLM cost accounting + the progressive output formatter.
//!
//! Spend reaches the ledger through two seams — INTENTIONALLY two, because the
//! two AI process models are different, not because the logic is duplicated:
//!   • tick / goal runs are one-shot, non-interactive, and looop owns their
//!     stdout: `runner::run_streamed` reads that NDJSON stream IN-PROCESS,
//!     rendering progress live (`format_line`) AND metering spend off the same
//!     stream (`CostMeter`). No external formatter process, no self-pipe.
//!   • worker sessions are long-lived, interactive, and self-supervising — looop
//!     never pipes their stdout, so the agent self-reports its own total via
//!     `looop _ cost` at end-of-session (an AI-facing callback, like `flag`).
//! Both append one JSON line to the cost ledger; `looop cost` reports over it.

use crate::config::{Config, CostMode, CostSpec};
use crate::paths::Paths;
use anyhow::Result;
use std::fs::OpenOptions;
use std::io::Write;
use std::process::ExitCode;

/// Total USD recorded in the ledger for the current LOCAL day. The ledger is the
/// single source of truth for spend (both the in-process tick meter and worker
/// `_ cost` append to it), so summing it survives pulse restarts (H2 — the daily cap must be
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

/// Safety-net daily ceiling used when neither the config key nor the env var is
/// set. The breaker is ON BY DEFAULT (a runaway loop must not bill forever);
/// raise it in config, or set `max_daily_usd: 0` / `LOOOP_MAX_DAILY_USD=0` to
/// turn the breaker off entirely.
pub const DEFAULT_DAILY_BUDGET_USD: f64 = 10.0;

/// The daily spend ceiling for the circuit breaker. Resolution order:
/// `LOOOP_MAX_DAILY_USD` env > config `max_daily_usd` > [`DEFAULT_DAILY_BUDGET_USD`].
/// A value `<= 0` (in env or config) explicitly DISABLES the breaker; an unset /
/// unparseable key falls through to the default (fail-closed: we'd rather cap by
/// default than spend without bound).
pub fn daily_budget(cfg: &Config) -> Option<f64> {
    // Env wins: an explicit 0 (or negative) disables, a positive value caps.
    if let Ok(v) = std::env::var("LOOOP_MAX_DAILY_USD")
        && let Ok(n) = v.trim().parse::<f64>()
    {
        return (n > 0.0).then_some(n);
    }
    match cfg
        .root
        .get("max_daily_usd")
        .and_then(|v| v.as_f64().or_else(|| v.as_u64().map(|n| n as f64)))
    {
        Some(x) if x > 0.0 => Some(x),          // explicit positive cap
        Some(_) => None,                        // explicit 0 / negative: breaker OFF
        None => Some(DEFAULT_DAILY_BUDGET_USD), // unset/unparseable: default cap
    }
}

/// Fail-closed budget breaker state. When a budget is set but a completed run
/// records NO cost, the breaker can't guarantee the cap. After this many
/// CONSECUTIVE unmetered runs at the same runner+spec signature, the breaker
/// opens (the pulse stops calling the AI) rather than fail open. It self-heals:
/// changing the runner or adding a cost spec changes the signature and resets the
/// count, giving the new config a fresh attempt.
pub const UNMETERED_LIMIT: u32 = 3;

fn unmetered_path(paths: &Paths) -> std::path::PathBuf {
    paths.data_dir.join(".cost-unmetered")
}

/// Read `(signature, consecutive_count)`; `None` when absent/unparseable.
fn read_unmetered(paths: &Paths) -> Option<(String, u32)> {
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(unmetered_path(paths)).ok()?).ok()?;
    let sig = v.get("sig")?.as_str()?.to_string();
    let count = v.get("count").and_then(|c| c.as_u64()).unwrap_or(0) as u32;
    Some((sig, count))
}

/// Record one unmetered run at `sig`; returns the new consecutive count. The
/// counter resets to 1 when `sig` differs from the previous record (a NEW
/// runner/spec deserves a fresh attempt).
pub fn record_unmetered(paths: &Paths, sig: &str) -> u32 {
    let count = match read_unmetered(paths) {
        Some((s, n)) if s == sig => n + 1,
        _ => 1,
    };
    let _ = std::fs::write(
        unmetered_path(paths),
        serde_json::json!({ "sig": sig, "count": count }).to_string(),
    );
    count
}

/// Clear the unmetered counter (a metered run proves the breaker can measure).
pub fn clear_unmetered(paths: &Paths) {
    let _ = std::fs::remove_file(unmetered_path(paths));
}

/// Whether the fail-closed breaker is OPEN for `sig`: at least [`UNMETERED_LIMIT`]
/// consecutive unmetered runs at this exact signature. A signature mismatch
/// (config changed) reads as closed, so the new config gets a fresh attempt.
pub fn unmetered_blocked(paths: &Paths, sig: &str) -> bool {
    matches!(read_unmetered(paths), Some((s, n)) if s == sig && n >= UNMETERED_LIMIT)
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

/// Accumulates LLM spend from a runner's NDJSON stream.
///
/// With no `spec`, the meter understands the two BUILT-IN runner shapes and
/// resolves them at the end (H3):
///   • pi (`--mode json`) emits per-message `usage.cost.total` — we SUM them.
///   • claude (`--output-format stream-json`) emits ONE `result.total_cost_usd`
///     that is already the CUMULATIVE run total — we take it verbatim.
/// claude's authoritative total wins when present, so the two readings are never
/// added together (no double counting); pi's running sum is the fallback.
///
/// With a `spec` (a CUSTOM runner declaring its cost shape in config), ONLY that
/// spec is applied — so the budget breaker (H2) can meter any runner instead of
/// failing open on an unrecognized stream.
#[derive(Default)]
pub(crate) struct CostMeter {
    pi_sum: f64,
    claude_total: Option<f64>,
    spec: Option<CostSpec>,
    spec_sum: f64,
    spec_total: Option<f64>,
}

impl CostMeter {
    /// A meter driven by a custom runner's [`CostSpec`]; `None` falls back to the
    /// built-in pi/claude shapes (identical to `CostMeter::default()`).
    pub(crate) fn new(spec: Option<CostSpec>) -> Self {
        CostMeter {
            spec,
            ..Default::default()
        }
    }

    /// Fold one NDJSON line into the running cost. Non-JSON lines and events
    /// without usage data are ignored.
    pub(crate) fn ingest(&mut self, line: &str) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            return;
        };
        // A custom spec takes over completely — the built-in shapes are not mixed
        // in, so a custom stream can't accidentally double-count.
        if let Some(spec) = &self.spec {
            if v.get("type").and_then(|t| t.as_str()) == Some(spec.type_tag.as_str())
                && let Some(c) = v.pointer(&spec.pointer).and_then(|c| c.as_f64())
            {
                match spec.mode {
                    CostMode::Sum => self.spec_sum += c,
                    CostMode::Total => self.spec_total = Some(c),
                }
            }
            return;
        }
        match v.get("type").and_then(|t| t.as_str()) {
            Some("message_end") => {
                self.pi_sum += v
                    .pointer("/message/usage/cost/total")
                    .and_then(|c| c.as_f64())
                    .unwrap_or(0.0);
            }
            Some("result") => {
                if let Some(c) = v.get("total_cost_usd").and_then(|c| c.as_f64()) {
                    self.claude_total = Some(c);
                }
            }
            _ => {}
        }
    }

    /// The resolved spend for the run. With a spec: the cumulative total (`total`
    /// mode) or the per-event sum (`sum` mode). Without: claude's authoritative
    /// cumulative total when present, else pi's per-message sum.
    pub(crate) fn total(&self) -> f64 {
        if self.spec.is_some() {
            return self.spec_total.unwrap_or(self.spec_sum);
        }
        self.claude_total.unwrap_or(self.pi_sum)
    }
}

/// Render one NDJSON event line; `None` means "emit nothing" (mirrors jq empty).
/// Used in-process by `runner::run_streamed` to turn the tick runner's raw
/// stream into the friendly progress lines archived to runs/<id>/output.log.
pub(crate) fn format_line(line: &str) -> Option<String> {
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

/// Aggregate ledger rows by local calendar day.
///
/// Returns `(day, total_usd, calls)` tuples sorted ascending by day (BTreeMap
/// order). Rows whose `ts` cannot be parsed collapse to an empty-string day and
/// sort first; their cost still counts toward the grand total.
fn by_day(rows: &[serde_json::Value]) -> Vec<(String, f64, usize)> {
    let mut map: std::collections::BTreeMap<String, (f64, usize)> =
        std::collections::BTreeMap::new();
    for r in rows {
        let day = r
            .get("ts")
            .and_then(|t| t.as_str())
            .map(local_day)
            .unwrap_or_default();
        let cost = r.get("cost_usd").and_then(|c| c.as_f64()).unwrap_or(0.0);
        let e = map.entry(day).or_insert((0.0, 0));
        e.0 += cost;
        e.1 += 1;
    }
    map.into_iter().map(|(d, (c, n))| (d, c, n)).collect()
}

/// Render a fixed-width text table. `right` flags which columns are right
/// aligned (numbers). An optional `footer` row is printed below a second
/// separator. Every emitted line is prefixed with `indent`.
fn table(
    headers: &[&str],
    rows: &[Vec<String>],
    right: &[bool],
    footer: Option<&[String]>,
    indent: &str,
) -> String {
    let cols = headers.len();
    let mut w: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    let widen = |w: &mut [usize], row: &[String]| {
        for (i, c) in row.iter().enumerate() {
            w[i] = w[i].max(c.chars().count());
        }
    };
    for row in rows {
        widen(&mut w, row);
    }
    if let Some(f) = footer {
        widen(&mut w, f);
    }

    let pad = |s: &str, i: usize| -> String {
        let gap = " ".repeat(w[i].saturating_sub(s.chars().count()));
        if right[i] {
            format!("{gap}{s}")
        } else {
            format!("{s}{gap}")
        }
    };
    let line = |cells: &[String]| -> String {
        let rendered: Vec<String> = cells.iter().enumerate().map(|(i, c)| pad(c, i)).collect();
        format!("{indent}{}", rendered.join("   "))
    };
    let sep_w: usize = w.iter().sum::<usize>() + 3 * (cols.saturating_sub(1));
    let sep = format!("{indent}{}", "─".repeat(sep_w));

    let mut out = String::new();
    let hdr: Vec<String> = headers.iter().map(|h| h.to_string()).collect();
    out.push_str(&line(&hdr));
    out.push('\n');
    out.push_str(&sep);
    out.push('\n');
    for row in rows {
        out.push_str(&line(row));
        out.push('\n');
    }
    if let Some(f) = footer {
        out.push_str(&sep);
        out.push('\n');
        out.push_str(&line(f));
        out.push('\n');
    }
    out
}

pub fn cmd_cost(paths: &Paths) -> Result<ExitCode> {
    let ledger = paths.cost_ledger();

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

    let days = by_day(&rows);
    let grand_total: f64 = days.iter().map(|(_, c, _)| c).sum();
    let grand_calls: usize = days.iter().map(|(_, _, n)| n).sum();

    println!("looop cost — by day (local)");
    println!();

    if days.is_empty() {
        println!("  (no cost recorded yet)");
        return Ok(ExitCode::SUCCESS);
    }

    let body: Vec<Vec<String>> = days
        .iter()
        .map(|(day, cost, calls)| {
            let label = if day.is_empty() {
                "?".to_string()
            } else {
                day.clone()
            };
            vec![label, calls.to_string(), usd(*cost)]
        })
        .collect();
    let footer = vec![
        "Total".to_string(),
        grand_calls.to_string(),
        usd(grand_total),
    ];

    print!(
        "{}",
        table(
            &["Day", "Calls", "Cost"],
            &body,
            &[false, true, true],
            Some(&footer),
            "  ",
        )
    );
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
    fn cost_meter_sums_pi_per_message_usage() {
        let mut m = CostMeter::default();
        m.ingest(r#"{"type":"message_end","message":{"usage":{"cost":{"total":0.10}}}}"#);
        m.ingest(r#"{"type":"message_end","message":{"usage":{"cost":{"total":0.25}}}}"#);
        m.ingest(r#"{"type":"tool_execution_start"}"#); // no usage — ignored
        m.ingest("not json"); // ignored
        assert!((m.total() - 0.35).abs() < 1e-9);
    }

    #[test]
    fn cost_meter_takes_claude_cumulative_total_verbatim() {
        let mut m = CostMeter::default();
        m.ingest(r#"{"type":"result","total_cost_usd":1.23}"#);
        assert!((m.total() - 1.23).abs() < 1e-9);
    }

    #[test]
    fn cost_meter_claude_total_wins_and_never_adds_to_pi_sum() {
        // A stream carrying both shapes must NOT double-count: claude's
        // authoritative cumulative total wins, pi's per-message sum is dropped.
        let mut m = CostMeter::default();
        m.ingest(r#"{"type":"message_end","message":{"usage":{"cost":{"total":0.50}}}}"#);
        m.ingest(r#"{"type":"result","total_cost_usd":2.00}"#);
        assert!((m.total() - 2.00).abs() < 1e-9);
    }

    #[test]
    fn cost_meter_empty_stream_is_zero() {
        assert_eq!(CostMeter::default().total(), 0.0);
    }

    #[test]
    fn cost_meter_spec_sum_mode_adds_matching_events() {
        let spec = CostSpec {
            type_tag: "usage".into(),
            pointer: "/spend".into(),
            mode: CostMode::Sum,
        };
        let mut m = CostMeter::new(Some(spec));
        m.ingest(r#"{"type":"usage","spend":0.10}"#);
        m.ingest(r#"{"type":"usage","spend":0.05}"#);
        m.ingest(r#"{"type":"other","spend":9.0}"#); // wrong type — ignored
        // Built-in shapes are NOT mixed in when a spec is active.
        m.ingest(r#"{"type":"result","total_cost_usd":99.0}"#);
        assert!((m.total() - 0.15).abs() < 1e-9);
    }

    #[test]
    fn cost_meter_spec_total_mode_takes_last_value() {
        let spec = CostSpec {
            type_tag: "final".into(),
            pointer: "/cost/usd".into(),
            mode: CostMode::Total,
        };
        let mut m = CostMeter::new(Some(spec));
        m.ingest(r#"{"type":"final","cost":{"usd":1.0}}"#);
        m.ingest(r#"{"type":"final","cost":{"usd":2.5}}"#);
        assert!((m.total() - 2.5).abs() < 1e-9);
    }

    #[test]
    fn runner_cost_spec_parses_and_defaults_mode_to_sum() {
        let cfg = Config {
            root: json!({
                "runners": {
                    "custom": { "cost": { "type": "usage", "pointer": "/x" } },
                    "full": { "cost": { "type": "r", "pointer": "/y", "mode": "total" } },
                    "bare": { "tick": "echo hi" }
                }
            }),
        };
        assert_eq!(
            cfg.runner_cost_spec("custom"),
            Some(CostSpec {
                type_tag: "usage".into(),
                pointer: "/x".into(),
                mode: CostMode::Sum,
            })
        );
        assert_eq!(cfg.runner_cost_spec("full").unwrap().mode, CostMode::Total);
        assert_eq!(cfg.runner_cost_spec("bare"), None);
        assert_eq!(cfg.runner_cost_spec("missing"), None);
    }

    #[test]
    fn daily_budget_reads_positive_only() {
        let cfg = |v: serde_json::Value| Config { root: v };
        assert_eq!(daily_budget(&cfg(json!({"max_daily_usd": 5.0}))), Some(5.0));
        assert_eq!(daily_budget(&cfg(json!({"max_daily_usd": 10}))), Some(10.0));
        // 0 (or negative) explicitly turns the breaker OFF.
        assert_eq!(daily_budget(&cfg(json!({"max_daily_usd": 0}))), None);
        // Unset => the safety-net default is ON (breaker default-on).
        assert_eq!(
            daily_budget(&cfg(json!({}))),
            Some(DEFAULT_DAILY_BUDGET_USD)
        );
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
    fn by_day_groups_and_counts_per_local_day() {
        let row = |ts: &str, c: f64| {
            serde_json::from_str::<serde_json::Value>(&format!(
                r#"{{"ts":"{ts}","kind":"tick","id":"x","runner":"pi","cost_usd":{c}}}"#
            ))
            .unwrap()
        };
        // Two rows on the same UTC day, one on another; ts uses Z so local_day is
        // deterministic only when the local offset keeps them on the same date —
        // assert on counts/totals which are offset-stable within a single day.
        let rows = vec![
            row("2026-06-20T01:00:00Z", 0.5),
            row("2026-06-20T02:00:00Z", 1.25),
            row("2026-06-21T12:00:00Z", 2.0),
        ];
        let days = by_day(&rows);
        let total: f64 = days.iter().map(|(_, c, _)| c).sum();
        let calls: usize = days.iter().map(|(_, _, n)| n).sum();
        assert!((total - 3.75).abs() < 1e-9, "grand total sums every row");
        assert_eq!(calls, 3, "every row counted once");
        // BTreeMap order => ascending day labels.
        let labels: Vec<&str> = days.iter().map(|(d, _, _)| d.as_str()).collect();
        let mut sorted = labels.clone();
        sorted.sort_unstable();
        assert_eq!(labels, sorted, "days sorted ascending");
    }

    #[test]
    fn by_day_unparseable_ts_collapses_to_empty_day() {
        let bad = serde_json::from_str::<serde_json::Value>(
            r#"{"ts":"not-a-date","kind":"tick","id":"x","runner":"pi","cost_usd":1.0}"#,
        )
        .unwrap();
        let days = by_day(&[bad]);
        assert_eq!(days.len(), 1);
        assert_eq!(days[0].0, "", "garbage ts -> empty day key");
        assert!((days[0].1 - 1.0).abs() < 1e-9, "cost still counted");
    }

    #[test]
    fn unmetered_counts_per_signature_and_opens_at_limit() {
        let p = Paths::temp();
        assert!(
            !unmetered_blocked(&p, "pi|false"),
            "closed before any record"
        );

        // Consecutive unmetered runs at the same signature escalate.
        for i in 1..UNMETERED_LIMIT {
            assert_eq!(record_unmetered(&p, "custom|false"), i);
            assert!(
                !unmetered_blocked(&p, "custom|false"),
                "still closed below the limit"
            );
        }
        assert_eq!(record_unmetered(&p, "custom|false"), UNMETERED_LIMIT);
        assert!(
            unmetered_blocked(&p, "custom|false"),
            "breaker opens at the limit"
        );

        // A different signature (config changed) reads as closed and resets.
        assert!(!unmetered_blocked(&p, "custom|true"));
        assert_eq!(record_unmetered(&p, "custom|true"), 1);

        // A metered run clears the counter entirely.
        clear_unmetered(&p);
        assert!(!unmetered_blocked(&p, "custom|true"));
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
