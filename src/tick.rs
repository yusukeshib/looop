//! ONE BEAT — sense → diff → decide ONE move → act → log. The heart of the
//! control loop (RULE 1: one tick = one move). Stateless and disposable: all
//! memory is the files in the data dir.

use crate::config::Config;
use crate::paths::Paths;
use crate::util::Level;
use crate::{events, executor, gate, prompt, runner, seed, sensor, session, util};
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

/// Exponential-backoff bounds for a repeatedly-failing world state (H1).
const BACKOFF_BASE_SECS: u64 = 60;
const BACKOFF_CAP_SECS: u64 = 3600;

/// Backoff window after `fails` consecutive failures at the SAME world state:
/// base·2^(fails-1), capped. `fails == 0` => no wait.
fn backoff_delay(fails: u32) -> u64 {
    if fails == 0 {
        return 0;
    }
    let shift = (fails - 1).min(20); // 1<<20 × 60s already far exceeds the cap
    BACKOFF_BASE_SECS
        .saturating_mul(1u64 << shift)
        .min(BACKOFF_CAP_SECS)
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn backoff_path(paths: &Paths) -> PathBuf {
    paths.data_dir.join(".tick-backoff")
}

/// Read backoff state as `(world_hash, consecutive_fails, last_fail_unix)`.
/// `None` when absent/unparseable (no backoff in effect).
fn read_backoff(paths: &Paths) -> Option<(String, u32, u64)> {
    let v: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(backoff_path(paths)).ok()?).ok()?;
    let hash = v.get("hash")?.as_str()?.to_string();
    let fails = v.get("fails").and_then(|f| f.as_u64()).unwrap_or(0) as u32;
    let ts = v.get("ts").and_then(|t| t.as_u64()).unwrap_or(0);
    Some((hash, fails, ts))
}

fn clear_backoff(paths: &Paths) {
    let _ = fs::remove_file(backoff_path(paths));
}

/// Record a failed attempt at `hash`; returns the new consecutive-fail count.
/// The counter resets when the world state differs from the previous failure
/// (a NEW situation deserves a fresh, immediate attempt).
fn record_backoff(paths: &Paths, hash: &str) -> u32 {
    let fails = match read_backoff(paths) {
        Some((h, n, _)) if h == hash => n + 1,
        _ => 1,
    };
    let body = serde_json::json!({ "hash": hash, "fails": fails, "ts": now_unix() }).to_string();
    let _ = fs::write(backoff_path(paths), body);
    fails
}

/// Run one beat. Returns whether the AI actually acted (drives cadence).
pub fn tick(paths: &Paths) -> bool {
    let _ = seed::ensure_dirs(paths);
    events::emit(paths, "tick_start", serde_json::json!({}));

    // 0. housekeeping (deterministic, no AI). Reap only AGED corpses — a worker
    // that just finished keeps its transcript for the retention window; sessions/
    // is looop-owned scratch, bounded here, never the user's deliverables.
    session::prune_aged(
        paths,
        std::time::Duration::from_secs(crate::run::session_ttl_secs(paths)),
    );
    gate::reap_stale_claims(paths);

    // 1. sense — level-triggered: wipe last beat's snapshots first.
    let snap = paths.snapshots_dir();
    let _ = fs::remove_dir_all(&snap);
    let _ = fs::create_dir_all(&snap);
    sensor::run_all(paths, &snap, true);
    events::emit(paths, "sense_done", serde_json::json!({}));

    // 2. skip if the world is unchanged (no AI call).
    let hash = crate::worldhash::world_hash(paths);
    let last = fs::read_to_string(paths.data_dir.join(".last-tick-hash"))
        .unwrap_or_default()
        .trim()
        .to_string();
    if hash == last {
        util::event(
            Level::Info,
            "tick.skip",
            "world unchanged — no AI call",
            &[],
        );
        events::emit(paths, "world_unchanged", serde_json::json!({}));
        return false;
    }

    // 2b. backoff (H1): if THIS exact world state has been failing, wait out an
    // exponential window before burning another AI call. Without this, a tick
    // that fails every time (bad runner, broken creds) never commits its hash,
    // so the world looks "changed" forever and retries every cadence — infinite
    // retries and infinite spend.
    if let Some((bhash, fails, ts)) = read_backoff(paths)
        && bhash == hash
    {
        let wait = backoff_delay(fails);
        let elapsed = now_unix().saturating_sub(ts);
        if elapsed < wait {
            let remain = wait - elapsed;
            util::event(
                Level::Warn,
                "tick.backoff",
                &format!(
                    "world changed but last {fails} attempt(s) failed — backing off ~{remain}s before retry"
                ),
                &[
                    ("fails", serde_json::json!(fails)),
                    ("retry_in_s", serde_json::json!(remain)),
                ],
            );
            events::emit(
                paths,
                "tick_backoff",
                serde_json::json!({ "fails": fails, "retry_in_s": remain }),
            );
            return false;
        }
    }

    // 3. hand everything to the AI for one move.
    let cfg = match Config::load(paths) {
        Ok(c) => c,
        Err(e) => {
            util::event(Level::Error, "tick.error", &format!("config: {e}"), &[]);
            return false;
        }
    };
    let runner_name = cfg.default_runner().unwrap_or_default();
    let Some(tick_cmd) = cfg.runner_cmd(&runner_name, "tick") else {
        util::event(
            Level::Error,
            "tick.error",
            &format!("no tick command for runner '{runner_name}'"),
            &[("runner", serde_json::json!(runner_name))],
        );
        return false;
    };

    // 3b. budget circuit breaker (H2): once today's ledger total reaches the
    // configured ceiling, skip the AI entirely so a runaway loop can't bill past
    // the cap. Off by default; clears at local midnight.
    if let Some(max) = crate::cost::daily_budget(&cfg) {
        let spent = crate::cost::spent_today(paths);
        if spent >= max {
            util::event(
                Level::Warn,
                "tick.budget",
                &format!(
                    "daily budget reached (${spent:.2} ≥ ${max:.2}) — skipping AI until local midnight"
                ),
                &[
                    ("spent_usd", serde_json::json!(spent)),
                    ("max_daily_usd", serde_json::json!(max)),
                ],
            );
            events::emit(
                paths,
                "budget_exceeded",
                serde_json::json!({ "spent_usd": spent, "max_daily_usd": max }),
            );
            return false;
        }
    }

    let cost_id = format!("tick-{}", chrono::Local::now().format("%Y%m%d-%H%M%S"));
    let run_dir = paths.runs_dir().join(&cost_id);
    let _ = fs::create_dir_all(&run_dir);
    let prompt_file = run_dir.join("prompt.md");
    let _ = fs::write(&prompt_file, prompt::build_prompt(paths, &snap));

    let t0 = Instant::now();
    util::event(
        Level::Step,
        "tick.start",
        &format!("{runner_name} is deciding the one move"),
        &[
            ("runner", serde_json::json!(runner_name)),
            ("run_id", serde_json::json!(cost_id)),
        ],
    );
    events::emit(
        paths,
        "decide_start",
        serde_json::json!({ "runner": runner_name, "run_id": cost_id }),
    );

    // The pulse stream stays a clean structured-event log: the runner's
    // free-form chatter (its `→ bash:` calls, blank lines, final text) is
    // archived to the tee files but NOT echoed live (live=false), so
    // `looop watch pulse` shows only `tick.*`/`sense.*` events. Replay the full
    // detail from runs/<id>/output.log or tick.log.
    let tee: Vec<PathBuf> = vec![run_dir.join("output.log"), paths.data_dir.join("tick.log")];
    let cost_env = [
        ("LOOOP_COST_KIND", "tick"),
        ("LOOOP_COST_RUNNER", runner_name.as_str()),
        ("LOOOP_COST_ID", cost_id.as_str()),
    ];

    // Typed-action path: looop is the SOLE executor of the decider's single move.
    // The runner emits ONE JSON action to .decision.json; we execute it, journal
    // it, and render one typed line. A beat "succeeds" (commits its world hash)
    // ONLY when a usable decision was produced — a runner crash, a malformed
    // decision, or no decision all count as failures that arm exponential
    // backoff (H1) and leave the hash uncommitted so a transient issue retries.
    let runner_ok = runner::run_streamed(paths, &tick_cmd, &prompt_file, &cost_env, &tee);
    let secs = t0.elapsed().as_secs();
    let outcome = if runner_ok {
        executor::consume_decision(paths)
    } else {
        None
    };

    let mut acted = false;
    match (runner_ok, outcome) {
        (true, Some(Ok(d))) => {
            let _ = fs::write(paths.data_dir.join(".last-tick-hash"), format!("{hash}\n"));
            clear_backoff(paths);
            acted = true;
            let cost = tick_cost(paths, &cost_id);
            let cost_str = cost.map(|c| format!(" · ${c:.4}")).unwrap_or_default();
            util::event(
                Level::Ok,
                "tick.decided",
                &format!("{} · {} · {secs}s{cost_str}", d.kind, d.journal),
                &[
                    ("action", serde_json::json!(d.kind)),
                    ("summary", serde_json::json!(d.summary)),
                    ("journal", serde_json::json!(d.journal)),
                    ("secs", serde_json::json!(secs)),
                    ("cost_usd", serde_json::json!(cost)),
                    ("run_id", serde_json::json!(cost_id)),
                ],
            );
            events::emit(
                paths,
                "decided",
                serde_json::json!({ "run_id": cost_id, "action": d.kind, "journal": d.journal }),
            );
        }
        (true, Some(Err(e))) => {
            let fails = record_backoff(paths, &hash);
            util::event(
                Level::Error,
                "tick.failed",
                &format!(
                    "decision failed after {secs}s (fail #{fails}): {e} · replay: {}",
                    run_dir.display()
                ),
                &[
                    ("secs", serde_json::json!(secs)),
                    ("run_id", serde_json::json!(cost_id)),
                    ("error", serde_json::json!(e.to_string())),
                    ("fails", serde_json::json!(fails)),
                ],
            );
            events::emit(
                paths,
                "tick_failed",
                serde_json::json!({ "run_id": cost_id }),
            );
        }
        (true, None) => {
            let fails = record_backoff(paths, &hash);
            util::event(
                Level::Warn,
                "tick.no_decision",
                &format!(
                    "ran {secs}s but emitted no .decision.json (no move, fail #{fails}) · replay: {}",
                    run_dir.display()
                ),
                &[
                    ("secs", serde_json::json!(secs)),
                    ("run_id", serde_json::json!(cost_id)),
                    ("fails", serde_json::json!(fails)),
                ],
            );
        }
        (false, _) => {
            let fails = record_backoff(paths, &hash);
            util::event(
                Level::Error,
                "tick.failed",
                &format!(
                    "tick failed after {secs}s (fail #{fails}) · replay: {}",
                    run_dir.display()
                ),
                &[
                    ("secs", serde_json::json!(secs)),
                    ("run_id", serde_json::json!(cost_id)),
                    ("replay", serde_json::json!(run_dir.display().to_string())),
                    ("fails", serde_json::json!(fails)),
                ],
            );
            events::emit(
                paths,
                "tick_failed",
                serde_json::json!({ "run_id": cost_id }),
            );
        }
    }

    prune_runs(paths);
    acted
}

/// Best-effort: this tick's recorded spend, read back from the cost ledger (the
/// runner's `_fmt` seam writes the row before `run_streamed` returns). `None`
/// when the runner doesn't meter (e.g. the claude tick) or nothing was recorded.
fn tick_cost(paths: &Paths, cost_id: &str) -> Option<f64> {
    let text = fs::read_to_string(paths.cost_ledger()).ok()?;
    text.lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|r| r.get("id").and_then(|x| x.as_str()) == Some(cost_id))
        .and_then(|r| r.get("cost_usd").and_then(|c| c.as_f64()))
}

/// Keep the newest LOOOP_RUNS_KEEP run dirs (default 50; 0 = keep all).
pub fn prune_runs(paths: &Paths) {
    let keep: usize = std::env::var("LOOOP_RUNS_KEEP")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(50);
    if keep == 0 {
        return;
    }
    let dir = paths.runs_dir();
    let mut runs: Vec<(std::time::SystemTime, PathBuf)> = fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let m = e.metadata().ok()?.modified().ok()?;
            Some((m, e.path()))
        })
        .collect();
    runs.sort_by_key(|r| std::cmp::Reverse(r.0)); // newest first
    for (_, p) in runs.into_iter().skip(keep) {
        let _ = fs::remove_dir_all(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_cost_reads_matching_ledger_row() {
        let p = Paths::temp();
        fs::write(
            p.cost_ledger(),
            concat!(
                r#"{"ts":"t","kind":"tick","id":"tick-A","runner":"pi","cost_usd":0.0123}"#,
                "\n",
                r#"{"ts":"t","kind":"tick","id":"tick-B","runner":"pi","cost_usd":0.0456}"#,
                "\n",
            ),
        )
        .unwrap();
        assert_eq!(tick_cost(&p, "tick-B"), Some(0.0456));
        assert_eq!(tick_cost(&p, "tick-A"), Some(0.0123));
        assert_eq!(tick_cost(&p, "tick-missing"), None);
    }

    #[test]
    fn tick_cost_none_without_ledger() {
        let p = Paths::temp();
        assert_eq!(tick_cost(&p, "tick-X"), None);
    }

    #[test]
    fn backoff_delay_grows_then_caps() {
        assert_eq!(backoff_delay(0), 0);
        assert_eq!(backoff_delay(1), 60);
        assert_eq!(backoff_delay(2), 120);
        assert_eq!(backoff_delay(3), 240);
        assert_eq!(backoff_delay(100), 3600); // capped, no overflow
    }

    #[test]
    fn backoff_counts_consecutive_same_hash_and_resets_on_change() {
        let p = Paths::temp();
        assert!(read_backoff(&p).is_none());
        assert_eq!(record_backoff(&p, "h1"), 1);
        assert_eq!(record_backoff(&p, "h1"), 2);
        let (h, n, _) = read_backoff(&p).unwrap();
        assert_eq!((h.as_str(), n), ("h1", 2));
        // a NEW world state restarts the counter (fresh, immediate attempt)
        assert_eq!(record_backoff(&p, "h2"), 1);
        clear_backoff(&p);
        assert!(read_backoff(&p).is_none());
    }
}
