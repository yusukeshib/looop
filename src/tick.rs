//! SENSE + STATE — the read side of looop, with the LLM decide phase removed.
//!
//! Judgment now lives in the ROOT AGENT (an external pi/claude session), not in
//! a looop-internal LLM call. So this module no longer "decides" anything. It
//! provides two things:
//!
//!   * [`sense`] — wipe and re-run every sensor, returning the fresh world hash.
//!     Called once per beat by the pulse ([`crate::run`]); the pulse is the SOLE
//!     senser, so two beats never wipe `snapshots/` under each other.
//!   * [`state`] / [`cmd_tick`] — a read-only structured snapshot of the world
//!     (sensor readings, pending asks, workers, goals, recent journal) the root
//!     agent pulls via `looop _ tick --json` whenever the pulse pokes it. No
//!     sensing, no side effects — safe to call anytime.

use crate::mailbox;
use crate::paths::Paths;
use crate::{events, sensor, session};
use anyhow::Result;
use std::fs;
use std::process::ExitCode;

/// Re-sense the world: reap aged corpses + stale claims, surface any interrupted
/// non-idempotent action from a crashed beat, wipe last beat's snapshots, run
/// every sensor fresh, and return the resulting world hash. The pulse owns this.
pub fn sense(paths: &Paths) -> String {
    let _ = crate::seed::ensure_dirs(paths);
    events::emit(paths, "tick_start", serde_json::json!({}));

    session::prune_aged(
        paths,
        std::time::Duration::from_secs(crate::run::session_ttl_secs(paths)),
    );
    crate::gate::reap_stale_claims(paths);
    crate::executor::warn_if_interrupted(paths);

    let snap = paths.snapshots_dir();
    let _ = fs::remove_dir_all(&snap);
    let _ = fs::create_dir_all(&snap);
    sensor::run_all(paths, &snap, true);
    events::emit(paths, "sense_done", serde_json::json!({}));

    crate::worldhash::world_hash(paths)
}

/// Read every `snapshots/*.json` into a name→value map (best-effort; unreadable
/// or non-JSON files are skipped). The pulse refreshes these each beat.
fn snapshots(paths: &Paths) -> serde_json::Map<String, serde_json::Value> {
    let mut out = serde_json::Map::new();
    for e in fs::read_dir(paths.snapshots_dir())
        .into_iter()
        .flatten()
        .flatten()
    {
        let p = e.path();
        if p.extension().map(|x| x == "json").unwrap_or(false)
            && let Some(stem) = p.file_stem().map(|s| s.to_string_lossy().to_string())
            && let Ok(raw) = fs::read_to_string(&p)
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw)
        {
            out.insert(stem, v);
        }
    }
    out
}

fn goal_ids(paths: &Paths) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(paths.goals_dir())
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "md").unwrap_or(false))
        .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().to_string()))
        .collect();
    v.sort();
    v
}

fn journal_tail(paths: &Paths, n: usize) -> Vec<String> {
    let text = fs::read_to_string(paths.journal()).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].iter().map(|s| s.to_string()).collect()
}

/// The read-only world state the root agent consumes. NO sensing, NO mutation:
/// it reads whatever the pulse last sensed plus the live mailbox / fleet.
pub fn state(paths: &Paths) -> serde_json::Value {
    let hash = crate::worldhash::world_hash(paths);

    let asks: Vec<serde_json::Value> = mailbox::pending(paths)
        .into_iter()
        .map(|a| serde_json::to_value(a).unwrap_or_default())
        .collect();

    let workers: Vec<serde_json::Value> = session::list_workers(paths)
        .into_iter()
        .map(|s| {
            serde_json::json!({
                "id": s.id,
                "state": s.state,
                "alive": s.alive,
                "exit_code": s.exit_code,
            })
        })
        .collect();

    serde_json::json!({
        "world_hash": hash,
        "snapshots": snapshots(paths),
        "asks": asks,
        "workers": workers,
        "goals": goal_ids(paths),
        "journal_tail": journal_tail(paths, 20),
        "data_dir": paths.data_dir.to_string_lossy(),
    })
}

/// Block until there is something to act on, then return. "Something" =
/// a pending ask (return immediately) OR the world hash moving from the value it
/// had when this call started (the pulse refreshes snapshots in the background,
/// so the hash tracks the real world). Pure read — it never senses, so it can't
/// race the pulse. Poll cadence: LOOOP_WAIT_POLL_MS (default 1000ms).
fn wait_for_change(paths: &Paths) {
    let poll = std::time::Duration::from_millis(
        std::env::var("LOOOP_WAIT_POLL_MS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(1000),
    );
    let baseline = crate::worldhash::world_hash(paths);
    loop {
        if !mailbox::pending(paths).is_empty() {
            return; // a worker is waiting — act now
        }
        if crate::worldhash::world_hash(paths) != baseline {
            return; // the world changed since we started waiting
        }
        std::thread::sleep(poll);
    }
}

/// Print the current state. `--json` = full structured object; else a summary.
fn print_state(paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let s = state(paths);
    if args.iter().any(|a| a == "--json") {
        println!("{}", serde_json::to_string_pretty(&s)?);
        return Ok(ExitCode::SUCCESS);
    }
    let asks = s["asks"].as_array().map(|a| a.len()).unwrap_or(0);
    let workers = s["workers"].as_array().map(|a| a.len()).unwrap_or(0);
    let goals = s["goals"].as_array().map(|a| a.len()).unwrap_or(0);
    println!("asks: {asks}  ·  workers: {workers}  ·  goals: {goals}");
    for a in s["asks"].as_array().cloned().unwrap_or_default() {
        println!(
            "  ⚑ {} ({}): {}",
            a["id"].as_str().unwrap_or("?"),
            a["worker"].as_str().unwrap_or("?"),
            a["prompt"].as_str().unwrap_or("")
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// `looop _ state [--json]` — read the current world state for the root agent.
/// Pure read: no sensing, no side effects (the pulse keeps snapshots fresh).
pub fn cmd_state(paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let _ = crate::seed::ensure_dirs(paths);
    print_state(paths, args)
}

/// `looop _ wait [--json]` — BLOCK until there is something to act on (a pending
/// ask, or the world changed), then print the fresh state. The root agent loops
/// `while: state=$(looop _ wait --json); act`. It does NOT run a tick — it waits
/// for one to matter; the pulse owns the sensing beats.
pub fn cmd_wait(paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let _ = crate::seed::ensure_dirs(paths);
    wait_for_change(paths);
    print_state(paths, args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_reports_goals_and_pending_asks() {
        let p = Paths::temp();
        let _ = crate::seed::ensure_dirs(&p);
        fs::write(p.goals_dir().join("triage.md"), b"triage\n").unwrap();
        fs::write(
            p.asks_dir().join("triage-1.json"),
            serde_json::json!({"id":"triage-1","worker":"triage","prompt":"merge?","ts":1})
                .to_string(),
        )
        .unwrap();

        let s = state(&p);
        let goals: Vec<String> = s["goals"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert!(goals.contains(&"triage".to_string()));
        assert_eq!(s["asks"].as_array().unwrap().len(), 1);
        assert_eq!(s["asks"][0]["id"], "triage-1");
    }
}
