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

/// Read backoff state as `(world_hash, policy_hash, consecutive_fails,
/// last_fail_unix)`. `None` when absent/unparseable (no backoff in effect).
fn read_backoff(paths: &Paths) -> Option<(String, String, u32, u64)> {
    let v: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(backoff_path(paths)).ok()?).ok()?;
    let hash = v.get("hash")?.as_str()?.to_string();
    let phash = v
        .get("phash")
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string();
    let fails = v.get("fails").and_then(|f| f.as_u64()).unwrap_or(0) as u32;
    let ts = v.get("ts").and_then(|t| t.as_u64()).unwrap_or(0);
    Some((hash, phash, fails, ts))
}

fn clear_backoff(paths: &Paths) {
    let _ = fs::remove_file(backoff_path(paths));
}

/// Record a failed attempt; returns the new CONSECUTIVE-fail count. The counter
/// increments on EVERY failure regardless of how the world hash moved — a failing
/// action that mutates the world each beat would otherwise look "new" forever and
/// reset the count, defeating the backoff and billing without bound. Only a
/// SUCCESS ([`clear_backoff`]) — or a human policy edit (see the gate in [`tick`])
/// — resets it. `phash` (the policy half) is stored so the gate can spot a human
/// intervention and retry promptly.
fn record_backoff(paths: &Paths, hash: &str, phash: &str) -> u32 {
    let fails = read_backoff(paths).map(|(_, _, n, _)| n + 1).unwrap_or(1);
    let body =
        serde_json::json!({ "hash": hash, "phash": phash, "fails": fails, "ts": now_unix() })
            .to_string();
    let _ = fs::write(backoff_path(paths), body);
    fails
}

/// Whether this beat may skip the AI: the world is unchanged since last beat AND
/// the decider did NOT request a forced re-decide (`force`). `force` is set by
/// the pulse when the previous beat emitted a `next_interval_s` nudge, so a goal
/// that needs a time-based follow-up ("re-check in 5 min") can opt out of the
/// level-triggered skip exactly once instead of going silent until the world
/// changes on its own.
fn can_skip(hash: &str, last: &str, force: bool) -> bool {
    hash == last && !force
}

/// What one beat produced: whether the AI acted (drives cadence classification)
/// and the decider's optional one-shot cadence nudge. The nudge rides back to the
/// pulse loop IN-MEMORY here — there is no `.next-interval` file round-trip for
/// what is purely an in-process handoff between [`tick`] and the run loop.
pub struct TickOutcome {
    pub acted: bool,
    pub next_interval_s: Option<u64>,
}

impl TickOutcome {
    /// A beat that did not act and requested no cadence change (skips, backoff,
    /// config/budget gates).
    fn idle() -> Self {
        TickOutcome {
            acted: false,
            next_interval_s: None,
        }
    }
}

/// Run one beat. `force` bypasses the unchanged-world skip once (see
/// [`can_skip`]).
pub fn tick(paths: &Paths, force: bool) -> TickOutcome {
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
    // Crash recovery: if the previous beat died mid non-idempotent side effect
    // (run_shell / send_notification) before committing its world hash, surface
    // it durably here. We do NOT auto-retry — a duplicate command is worse than
    // a missed one; a human verifies whether it half-ran (H: side-effect/commit
    // gap).
    executor::warn_if_interrupted(paths);

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

    // 2b. backoff (H1): after consecutive FAILED beats, wait out an exponential
    // window before burning another AI call. Without this, a tick that fails
    // every time (bad runner, broken creds) never commits its hash, so the world
    // looks "changed" forever and retries every cadence — infinite retries and
    // infinite spend. We back off on the consecutive-fail COUNT regardless of
    // world-hash churn (a failing run_shell can mutate the world every beat),
    // EXCEPT when the policy half (PLAYBOOK/goals) changed since the last failure
    // — that's a human steering the loop, so we drop the backoff and retry now.
    let policy = crate::worldhash::policy_hash(paths);
    if let Some((_bhash, bphash, fails, ts)) = read_backoff(paths)
        && fails > 0
    {
        if bphash != policy {
            // A human edited PLAYBOOK/goals since the failure — explicit "try now".
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
                        "last {fails} beat(s) failed — backing off ~{remain}s before retry (edit PLAYBOOK/goals to retry now)"
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
            &format!("no tick command for runner '{runner_name}'"),
            &[("runner", serde_json::json!(runner_name))],
        );
        return TickOutcome::idle();
    };

    // The runner+spec signature for fail-closed unmetered tracking: a change to
    // either (switching runners, adding a cost spec) resets the breaker so the
    // new config gets a fresh attempt.
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
        // Fail-closed: if a budget is set but this runner keeps producing no cost
        // (the breaker can't measure it), refuse to spend blindly rather than
        // fail open. Self-heals when the runner/spec signature changes.
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

    // Typed-action path: looop is the SOLE executor of the decider's single move.
    // The runner emits ONE JSON action to .decision.json; we execute it, journal
    // it, and render one typed line. A beat "succeeds" (commits its world hash)
    // ONLY when a usable decision was produced — a runner crash, a malformed
    // decision, or no decision all count as failures that arm exponential
    // backoff (H1) and leave the hash uncommitted so a transient issue retries.
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

    // Fail-closed accounting: the budget breaker (H2) can only enforce a cap if
    // runs are metered. If a budget is set, track whether THIS run recorded a
    // cost: a metered run clears the counter; an unmetered one increments it, and
    // once it reaches the limit the pre-run check above opens the breaker. So a
    // runner the meter can't read can't run away — it stalls after a bounded
    // number of unmetered runs instead of billing forever.
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
    // hash, clear backoff, journal the move. Every other outcome (bad decision /
    // no decision / runner crash) is a failure that arms exponential backoff
    // (H1) and leaves the hash uncommitted so a transient issue retries — the
    // three failure shapes share that handling and differ only in how the log
    // line reads.
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
            (true, next_interval_s)
        }
        failure => {
            let fails = record_backoff(paths, &hash, &policy);
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
            // Persist the failure shape to the DURABLE log too: events.jsonl is the
            // only post-mortem trail (tick.log is per-run scratch, util::event is the
            // transient pulse stream). Without code/reason/fails here, a beat that
            // "decided but failed" leaves no record of WHY — see H1 diagnosis.
            events::emit(
                paths,
                "tick_failed",
                serde_json::json!({
                    "run_id": cost_id,
                    "code": code,
                    "reason": msg,
                    "fails": fails,
                }),
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
/// (`run_streamed` meters the run in-process and writes the row before it
/// returns). `None` when the runner emitted no usage data or nothing was recorded.
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
    fn can_skip_only_when_unchanged_and_not_forced() {
        assert!(can_skip("h", "h", false), "unchanged + not forced => skip");
        assert!(!can_skip("h", "h", true), "forced re-decide overrides skip");
        assert!(!can_skip("h2", "h", false), "changed world never skips");
        assert!(!can_skip("h2", "h", true), "changed + forced never skips");
    }

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
    fn backoff_counts_consecutive_failures_regardless_of_hash() {
        let p = Paths::temp();
        assert!(read_backoff(&p).is_none());
        assert_eq!(record_backoff(&p, "h1", "pol"), 1);
        assert_eq!(record_backoff(&p, "h1", "pol"), 2);
        // A DIFFERENT world hash STILL increments — a failing action that churns
        // the world can't reset the backoff (that was the runaway-spend hole).
        assert_eq!(record_backoff(&p, "h2", "pol"), 3);
        let (h, ph, n, _) = read_backoff(&p).unwrap();
        assert_eq!((h.as_str(), ph.as_str(), n), ("h2", "pol", 3));
        // Only a success clears it.
        clear_backoff(&p);
        assert!(read_backoff(&p).is_none());
    }
}
