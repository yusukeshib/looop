//! ONE BEAT — sense → diff → decide ONE move → act → log. The heart of the
//! AUTONOMOUS control loop (RULE 1: one tick = one move). Stateless and
//! disposable: all memory is the files in the data dir.
//!
//! looop is autonomous: each beat the pulse senses the world, and — when the
//! world changed — hands it to the configured `tick` runner for ONE move, which
//! looop (the sole executor) runs through the typed [`crate::executor`] actions.
//! The human is a peer, not the driver: they steer by editing goals/PLAYBOOK
//! (observed next beat) and answer worker questions via the ask/answer mailbox.
//!
//! This module provides:
//!   * [`sense`] — wipe + re-run every sensor, returning the fresh world hash.
//!   * [`tick`] — one full beat (sense → skip-if-unchanged → decide → execute).
//!   * [`state`] / [`cmd_state`] / [`cmd_wait`] — a read-only structured view of
//!     the world (sensor readings, pending asks, workers, goals, journal) a human
//!     or a helper agent can pull. No sensing, no side effects.

use crate::config::Config;
use crate::mailbox;
use crate::paths::Paths;
use crate::util::{self, Level};
use crate::{events, executor, prompt, runner, sensor, session};
use anyhow::Result;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

/// Exponential-backoff bounds for a repeatedly-failing world state (H1).
const BACKOFF_BASE_SECS: u64 = 60;
const BACKOFF_CAP_SECS: u64 = 3600;

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

// ---- backoff (H1) -------------------------------------------------------------

/// Backoff window after `fails` consecutive failures at the SAME world state:
/// base·2^(fails-1), capped. `fails == 0` => no wait.
fn backoff_delay(fails: u32) -> u64 {
    if fails == 0 {
        return 0;
    }
    let shift = (fails - 1).min(20);
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

/// Record a failed attempt; returns the new CONSECUTIVE-fail count. The counter
/// increments on EVERY failure regardless of how the world hash moved — a failing
/// action that mutates the world each beat would otherwise look "new" forever and
/// reset the count, defeating the backoff. Only a SUCCESS ([`clear_backoff`]) — or
/// the world moving off the failing state (the gate in [`tick`]) — resets it.
fn record_backoff(paths: &Paths, hash: &str) -> u32 {
    let fails = read_backoff(paths).map(|(_, n, _)| n + 1).unwrap_or(1);
    let body = serde_json::json!({ "hash": hash, "fails": fails, "ts": now_unix() }).to_string();
    let _ = fs::write(backoff_path(paths), body);
    fails
}

/// Whether this beat may skip the AI: the world is unchanged since last beat AND
/// the decider did NOT request a forced re-decide (`force`, set when the previous
/// beat emitted a `next_interval_s` nudge for a time-based follow-up).
fn can_skip(hash: &str, last: &str, force: bool) -> bool {
    hash == last && !force
}

/// What one beat produced: whether the AI acted (drives cadence) and the
/// decider's optional one-shot cadence nudge, handed back to the run loop
/// in-memory (no `.next-interval` file round-trip).
pub struct TickOutcome {
    pub acted: bool,
    pub next_interval_s: Option<u64>,
}

impl TickOutcome {
    fn idle() -> Self {
        TickOutcome {
            acted: false,
            next_interval_s: None,
        }
    }
}

/// Run one beat. `force` bypasses the unchanged-world skip once (see [`can_skip`]).
pub fn tick(paths: &Paths, force: bool) -> TickOutcome {
    // 0+1. housekeeping + sense (emits tick_start / sense_done, returns the hash).
    let hash = sense(paths);

    // 2. skip if the world is unchanged (no AI call).
    let last = fs::read_to_string(paths.data_dir.join(".last-tick-hash"))
        .unwrap_or_default()
        .trim()
        .to_string();
    if can_skip(&hash, &last, force) {
        util::event(
            Level::Info,
            "tick.skip",
            "world unchanged — no AI call",
            &[],
        );
        events::emit(paths, "world_unchanged", serde_json::json!({}));
        return TickOutcome::idle();
    }
    if hash == last && force {
        util::event(
            Level::Info,
            "tick.forced",
            "world unchanged but re-deciding (cadence override from last beat)",
            &[],
        );
    }

    // 2b. backoff (H1): after consecutive FAILED beats at the same world state,
    // wait out an exponential window before burning another AI call. The world
    // moving off the failing state clears it (a human edit to goals/PLAYBOOK
    // changes the world hash, so steering retries promptly).
    if let Some((bhash, fails, ts)) = read_backoff(paths)
        && fails > 0
    {
        if bhash != hash {
            clear_backoff(paths);
        } else {
            let wait = backoff_delay(fails);
            let elapsed = now_unix().saturating_sub(ts);
            if elapsed < wait {
                let remain = wait - elapsed;
                util::event(
                    Level::Warn,
                    "tick.backoff",
                    &format!(
                        "last {fails} beat(s) failed — backing off ~{remain}s before retry (edit a goal/PLAYBOOK to retry now)"
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
                return TickOutcome::idle();
            }
        }
    }

    // 3. hand everything to the AI for one move.
    let cfg = match Config::load(paths) {
        Ok(c) => c,
        Err(e) => {
            util::event(Level::Error, "tick.error", &format!("config: {e}"), &[]);
            return TickOutcome::idle();
        }
    };
    let runner_name = cfg.default_runner().unwrap_or_default();
    let Some(tick_cmd) = cfg.runner_cmd(&runner_name, "tick") else {
        util::event(
            Level::Error,
            "tick.error",
            &format!("no tick command for runner '{runner_name}' (config a `tick` command)"),
            &[("runner", serde_json::json!(runner_name))],
        );
        return TickOutcome::idle();
    };

    // The runner+spec signature for fail-closed unmetered tracking: a change to
    // either (switching runners, adding a cost spec) resets the breaker.
    let cost_sig = format!(
        "{runner_name}|{}",
        cfg.runner_cost_spec(&runner_name).is_some()
    );

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
            return TickOutcome::idle();
        }
        if crate::cost::unmetered_blocked(paths, &cost_sig) {
            util::event(
                Level::Warn,
                "tick.budget_unmetered",
                &format!(
                    "runner '{runner_name}' produced no cost for {n} consecutive runs and a budget is set — skipping AI (declare a runner `cost` spec, or use pi/claude)",
                    n = crate::cost::UNMETERED_LIMIT
                ),
                &[("runner", serde_json::json!(runner_name))],
            );
            events::emit(
                paths,
                "budget_unmetered",
                serde_json::json!({ "runner": runner_name }),
            );
            return TickOutcome::idle();
        }
    }

    let cost_id = format!("tick-{}", chrono::Local::now().format("%Y%m%d-%H%M%S"));
    let run_dir = paths.runs_dir().join(&cost_id);
    let _ = fs::create_dir_all(&run_dir);
    let prompt_file = run_dir.join("prompt.md");
    let snap = paths.snapshots_dir();
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

    // The runner's free-form chatter is archived to the tee files (replay from
    // runs/<id>/output.log or tick.log) but NOT echoed live — the pulse stream
    // stays a clean structured-event log.
    let tee: Vec<PathBuf> = vec![run_dir.join("output.log"), paths.data_dir.join("tick.log")];

    let runner_ok = runner::run_streamed(
        paths,
        &tick_cmd,
        &prompt_file,
        "tick",
        &cost_id,
        &runner_name,
        &tee,
    );
    let secs = t0.elapsed().as_secs();
    let outcome = if runner_ok {
        executor::consume_decision(paths)
    } else {
        None
    };

    // Fail-closed accounting: a budget can only be enforced if runs are metered.
    if runner_ok && crate::cost::daily_budget(&cfg).is_some() {
        if tick_cost(paths, &cost_id).is_none() {
            let n = crate::cost::record_unmetered(paths, &cost_sig);
            let limit = crate::cost::UNMETERED_LIMIT;
            let tail = if n >= limit {
                "breaker now open".to_string()
            } else {
                format!("{n}/{limit} before the breaker opens")
            };
            util::event(
                Level::Warn,
                "tick.unmetered",
                &format!(
                    "max_daily_usd is set but runner '{runner_name}' produced no cost row ({tail}) — declare a runner `cost` spec, or use pi/claude"
                ),
                &[
                    ("runner", serde_json::json!(runner_name)),
                    ("count", serde_json::json!(n)),
                ],
            );
        } else {
            crate::cost::clear_unmetered(paths);
        }
    }

    // A beat SUCCEEDS only when a usable decision was produced: commit the world
    // hash, clear backoff, journal the move. Every other outcome arms backoff and
    // leaves the hash uncommitted so a transient issue retries.
    let (acted, next_interval_s) = match (runner_ok, outcome) {
        (true, Some(Ok(d))) => {
            let _ = fs::write(paths.data_dir.join(".last-tick-hash"), format!("{hash}\n"));
            clear_backoff(paths);
            let next_interval_s = d.next_interval_s;
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
            // noop is a real decision (the world is fine) — it does not count as
            // "acted" for cadence, but it DID commit the hash above.
            (d.kind != "noop", next_interval_s)
        }
        failure => {
            let fails = record_backoff(paths, &hash);
            let replay = run_dir.display().to_string();
            let mut fields = vec![
                ("secs", serde_json::json!(secs)),
                ("run_id", serde_json::json!(cost_id)),
                ("fails", serde_json::json!(fails)),
            ];
            let (level, code, msg) = match failure {
                (true, Some(Err(e))) => {
                    fields.push(("error", serde_json::json!(e.to_string())));
                    (
                        Level::Error,
                        "tick.failed",
                        format!(
                            "decision failed after {secs}s (fail #{fails}): {e} · replay: {replay}"
                        ),
                    )
                }
                (true, None) => (
                    Level::Warn,
                    "tick.no_decision",
                    format!(
                        "ran {secs}s but emitted no .decision.json (no move, fail #{fails}) · replay: {replay}"
                    ),
                ),
                _ => {
                    fields.push(("replay", serde_json::json!(replay.clone())));
                    (
                        Level::Error,
                        "tick.failed",
                        format!("tick failed after {secs}s (fail #{fails}) · replay: {replay}"),
                    )
                }
            };
            util::event(level, code, &msg, &fields);
            events::emit(
                paths,
                "tick_failed",
                serde_json::json!({ "run_id": cost_id, "fails": fails }),
            );
            (false, None)
        }
    };

    prune_runs(paths);
    TickOutcome {
        acted,
        next_interval_s,
    }
}

/// Best-effort: this tick's recorded spend, read back from the cost ledger
/// (`run_streamed` writes the row before it returns). `None` when the runner
/// emitted no usage data or nothing was recorded.
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
    runs.sort_by_key(|r| std::cmp::Reverse(r.0));
    for (_, p) in runs.into_iter().skip(keep) {
        let _ = fs::remove_dir_all(p);
    }
}

// ---- read-only world state (for humans / helper agents) -----------------------

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

/// The read-only world state a human (or helper agent) consumes. NO sensing, NO
/// mutation: it reads whatever the pulse last sensed plus the live mailbox/fleet.
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

/// Block until there is something to look at, then return. "Something" = a
/// pending ask (return immediately) OR the world hash moving from its value when
/// this call started. Pure read — never senses, so it can't race the pulse.
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
            return;
        }
        if crate::worldhash::world_hash(paths) != baseline {
            return;
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

/// `looop _ state [--json]` — read the current world state. Pure read: no
/// sensing, no side effects (the autonomous pulse keeps snapshots fresh).
pub fn cmd_state(paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let _ = crate::seed::ensure_dirs(paths);
    print_state(paths, args)
}

/// `looop _ wait [--json]` — BLOCK until there is something to look at (a pending
/// ask, or the world changed), then print the fresh state. A convenience for a
/// human/helper watching the autonomous loop; looop itself does not need it.
pub fn cmd_wait(paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let _ = crate::seed::ensure_dirs(paths);
    wait_for_change(paths);
    print_state(paths, args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn can_skip_only_when_unchanged_and_not_forced() {
        assert!(can_skip("a", "a", false));
        assert!(!can_skip("a", "b", false));
        assert!(!can_skip("a", "a", true));
    }

    #[test]
    fn backoff_delay_grows_then_caps() {
        assert_eq!(backoff_delay(0), 0);
        assert_eq!(backoff_delay(1), BACKOFF_BASE_SECS);
        assert_eq!(backoff_delay(2), BACKOFF_BASE_SECS * 2);
        assert_eq!(backoff_delay(99), BACKOFF_CAP_SECS);
    }

    #[test]
    fn backoff_round_trips_and_clears() {
        let p = Paths::temp();
        let _ = crate::seed::ensure_dirs(&p);
        assert!(read_backoff(&p).is_none());
        assert_eq!(record_backoff(&p, "h"), 1);
        assert_eq!(record_backoff(&p, "h"), 2);
        let (h, n, _) = read_backoff(&p).unwrap();
        assert_eq!((h.as_str(), n), ("h", 2));
        clear_backoff(&p);
        assert!(read_backoff(&p).is_none());
    }

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
