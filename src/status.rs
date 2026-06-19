//! `looop status [--json]` — a one-shot STRUCTURED probe of the loop's live
//! state (observability layer 2). An external observer (e.g. an AI watching the
//! loop) reads this instead of scraping the human-pretty stdout. Pure read over
//! the data dir + lock + `babysit ls`; no daemon.

use crate::config::Config;
use crate::paths::Paths;
use crate::{session, util};
use anyhow::Result;
use std::fs;
use std::process::ExitCode;

fn cost_today(paths: &Paths) -> f64 {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    fs::read_to_string(paths.cost_ledger())
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|r| {
            r.get("ts")
                .and_then(|t| t.as_str())
                .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
                .map(|dt| {
                    dt.with_timezone(&chrono::Local)
                        .format("%Y-%m-%d")
                        .to_string()
                        == today
                })
                .unwrap_or(false)
        })
        .filter_map(|r| r.get("cost_usd").and_then(|c| c.as_f64()))
        .sum::<f64>()
        + 0.0 // normalize -0.0 -> 0.0
}

fn build(paths: &Paths) -> serde_json::Value {
    let lock = paths.lock();
    let pid = fs::read_to_string(lock.join("pid")).unwrap_or_default();
    let pid = pid.trim().to_string();
    let running = lock.is_dir() && util::pid_alive(&pid);

    let idle = Config::load(paths)
        .ok()
        .and_then(|c| {
            c.root
                .get("interval")
                .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64)))
        })
        .unwrap_or(60);

    let hash_file = paths.data_dir.join(".last-tick-hash");
    let last_hash = fs::read_to_string(&hash_file)
        .unwrap_or_default()
        .trim()
        .to_string();
    let last_at = fs::metadata(&hash_file)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| {
            chrono::DateTime::<chrono::Utc>::from_timestamp(d.as_secs() as i64, 0)
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_default()
        });

    let workers: Vec<serde_json::Value> = session::list_workers(paths)
        .into_iter()
        .map(|s| {
            serde_json::json!({
                "id": s.id,
                "state": s.state,
                "alive": s.alive,
                "exit_code": s.exit_code,
                "flagged": s.flagged(),
                "note": s.note,
            })
        })
        .collect();

    let attention: Vec<String> = fs::read_to_string(paths.data_dir.join("attention.md"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .filter(|l| !l.is_empty())
        .collect();

    serde_json::json!({
        "pulse": { "running": running, "pid": pid, "interval_s": idle },
        "last_tick": { "at": last_at, "hash": last_hash },
        "workers": workers,
        "attention": attention,
        "cost_today_usd": cost_today(paths),
        "data_dir": paths.data_dir.to_string_lossy(),
    })
}

pub fn cmd_status(paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let s = build(paths);
    if args.iter().any(|a| a == "--json") {
        println!("{}", serde_json::to_string_pretty(&s)?);
        return Ok(ExitCode::SUCCESS);
    }

    // Human summary.
    let pulse = &s["pulse"];
    let running = pulse["running"].as_bool().unwrap_or(false);
    println!(
        "pulse:    {}",
        if running {
            format!("running (pid {})", pulse["pid"].as_str().unwrap_or("?"))
        } else {
            "stopped".into()
        }
    );
    println!(
        "last tick: {}  (hash {})",
        s["last_tick"]["at"].as_str().unwrap_or("never"),
        {
            let h = s["last_tick"]["hash"].as_str().unwrap_or("");
            if h.is_empty() { "—" } else { h }
        }
    );
    let workers = s["workers"].as_array().cloned().unwrap_or_default();
    println!("workers:  {}", workers.len());
    for w in &workers {
        let flag = if w["flagged"].as_bool().unwrap_or(false) {
            format!("  ⚑ {}", w["note"].as_str().unwrap_or(""))
        } else {
            String::new()
        };
        println!(
            "  - {} [{}]{}",
            w["id"].as_str().unwrap_or("?"),
            w["state"].as_str().unwrap_or("?"),
            flag
        );
    }
    let att = s["attention"].as_array().cloned().unwrap_or_default();
    if !att.is_empty() {
        println!("attention:");
        for a in att {
            println!("  {}", a.as_str().unwrap_or(""));
        }
    }
    println!(
        "cost today: ${:.4}",
        s["cost_today_usd"].as_f64().unwrap_or(0.0)
    );
    Ok(ExitCode::SUCCESS)
}
