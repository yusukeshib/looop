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
//!
//! The guard mechanisms the beat leans on (failure backoff, noop TTL,
//! flapping-sensor detection, the hourly decide cap) live in
//! [`crate::tick_guards`]. The read-only observation surface (`looop state` /
//! `looop wait`) lives in [`crate::observe`].

use crate::config::Config;
use crate::paths::Paths;
use crate::tick_guards::{
    backoff_delay, clear_backoff, decide_budget, decide_cap_per_hour, noop_revisit_due,
    read_backoff, read_decide_ledger, record_backoff, record_decide, record_noop, update_flap,
};
use crate::util::{self, Level};
use crate::{events, executor, prompt, runner, sensor, session};
use anyhow::Result;
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

// Re-exported so consumers keep addressing the beat's public surface
// (`tick::flapping_sensors`, used by the decide prompt) even though the guard
// itself now lives in `tick_guards`.
pub(crate) use crate::tick_guards::flapping_sensors;

/// Re-sense the world: reap aged corpses + stale claims, surface any interrupted
/// non-idempotent action from a crashed beat, refresh `snapshots/` (pruning
/// what no live sensor owns; a fresh interval-cadenced snapshot survives), and
/// return the resulting world hash. The pulse owns this.
pub fn sense(paths: &Paths) -> String {
    let _ = crate::seed::ensure_dirs(paths);
    events::emit(paths, "tick_start", serde_json::json!({}));

    session::prune_aged(
        paths,
        std::time::Duration::from_secs(crate::run::session_ttl_secs(paths)),
    );
    crate::gate::reap_stale_claims(paths);
    crate::executor::warn_if_interrupted(paths);

    // Snapshots are NOT wiped wholesale: a snapshot still fresh under a
    // declared `# looop:interval=N` cadence survives the beat; run_all prunes
    // anything a live sensor doesn't own and rewrites the rest.
    let snap = paths.snapshots_dir();
    let _ = fs::create_dir_all(&snap);
    sensor::run_all(paths, &snap, true);
    events::emit(paths, "sense_done", serde_json::json!({}));

    crate::worldhash::world_hash(paths)
}

/// Whether this beat may skip the AI: the world is unchanged since last beat AND
/// the decider did NOT request a forced re-decide (`force`, set when the previous
/// beat emitted a `next_interval_s` nudge for a time-based follow-up).
fn can_skip(hash: &str, last: &str, force: bool) -> bool {
    hash == last && !force
}

// ---- last-failure feedback ------------------------------------------------------

/// Persist WHY this beat failed, so the NEXT decide prompt can surface it
/// (`LAST FAILURE` section) instead of letting the decider re-emit the same
/// failing move blind. Cleared by the next usable decision.
fn record_failure(paths: &Paths, run_id: &str, code: &str, error: &str, fails: u32) {
    let body = serde_json::json!({
        "v": 1,
        "ts": util::now_unix(),
        "run_id": run_id,
        "code": code,
        "error": error,
        "fails": fails,
    })
    .to_string();
    if let Err(e) = util::write_atomic(&paths.last_failure(), body.as_bytes()) {
        util::event(
            Level::Warn,
            "tick.guard_degraded",
            &format!("failed to persist the last-failure feedback (next prompt flies blind): {e}"),
            &[],
        );
    }
}

/// What one beat produced: whether the AI acted (drives cadence) and the
/// decider's optional one-shot cadence nudge, handed back to the run loop
/// in-memory (no `.next-interval` file round-trip).
pub struct TickOutcome {
    pub acted: bool,
    pub next_interval_s: Option<u64>,
    /// True when a decide was actually ATTEMPTED this beat (the runner was
    /// launched) — success or failure alike. False when the beat idled out
    /// before deciding (skip, backoff, budget, config error). The pulse uses
    /// this to know when a durable next-wake nudge has been honored (see
    /// `run.rs`): a nudge must survive idle beats and be consumed only once a
    /// decide actually ran.
    pub decided_or_failed: bool,
}

impl TickOutcome {
    fn idle() -> Self {
        TickOutcome {
            acted: false,
            next_interval_s: None,
            decided_or_failed: false,
        }
    }
}

/// The committed last-beat world hash lives here (single definition — the
/// literal used to appear at both the read and the commit site).
fn last_tick_hash_path(paths: &Paths) -> PathBuf {
    paths.data_dir.join(".last-tick-hash")
}

/// Run one beat. `force` bypasses the unchanged-world skip once (see [`can_skip`]).
pub fn tick(paths: &Paths, force: bool) -> TickOutcome {
    // 0+1. housekeeping + sense (emits tick_start / sense_done, returns the hash).
    let hash = sense(paths);

    // 1b. flapping bookkeeping: track per-snapshot signal-change streaks and
    // warn when one crosses the threshold. Runs every beat (a skip resets the
    // streaks naturally — an unchanged world means unchanged signals).
    let _ = update_flap(paths);

    // 2..2c. gates that may idle the beat out before any AI spend.
    if let Some(idle) = should_decide(paths, &hash, force) {
        return idle;
    }

    // 3. hand everything to the AI for one move.
    let Some(run) = run_decider(paths) else {
        return TickOutcome::idle();
    };

    // 4. commit the outcome (or arm backoff) and prune the replay archive.
    let outcome = commit_outcome(paths, &hash, run);
    prune_runs(paths);
    outcome
}

/// Gate one beat BEFORE any AI spend: the unchanged-world skip (with the
/// noop-TTL revisit bypass), failure backoff, and the hourly decide budget.
/// Returns `Some(idle)` when the beat must idle out here without deciding;
/// `None` when the decide should run.
fn should_decide(paths: &Paths, hash: &str, force: bool) -> Option<TickOutcome> {
    // 2. skip if the world is unchanged (no AI call).
    let last = fs::read_to_string(last_tick_hash_path(paths))
        .unwrap_or_default()
        .trim()
        .to_string();
    if can_skip(hash, &last, force) {
        // Noop TTL: an unchanged world normally skips, but if the decision that
        // committed this hash was a NOOP and it has aged past the TTL, re-decide
        // — one wrong noop must not park this world state forever.
        if noop_revisit_due(paths, hash) {
            util::event(
                Level::Info,
                "tick.revisit",
                "world unchanged but the last noop aged past LOOOP_NOOP_TTL — re-deciding",
                &[],
            );
            events::emit(paths, "noop_revisit", serde_json::json!({}));
        } else {
            util::event(
                Level::Info,
                "tick.skip",
                "world unchanged — no AI call",
                &[],
            );
            events::emit(paths, "world_unchanged", serde_json::json!({}));
            return Some(TickOutcome::idle());
        }
    }
    if hash == last && force {
        util::event(
            Level::Info,
            "tick.forced",
            "world unchanged but re-deciding (forced: pulse start or a cadence nudge)",
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
            let elapsed = util::now_unix().saturating_sub(ts);
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
                return Some(TickOutcome::idle());
            }
        }
    }

    // 2c. global spend ceiling: the skip gate and backoff bound a quiet loop,
    // but a noisy one (flapping sensor, aggressive cadence nudges) can burn a
    // decide per beat forever. Cap ATTEMPTS per rolling hour; when exhausted,
    // idle out the beat — the world is level-triggered, so nothing is lost.
    if let Err(retry_in) = decide_budget(
        util::now_unix(),
        &read_decide_ledger(paths),
        decide_cap_per_hour(),
    ) {
        util::event(
            Level::Warn,
            "tick.capped",
            &format!(
                "hourly decide budget exhausted (LOOOP_MAX_DECIDES_PER_HOUR={}) — idling ~{retry_in}s; \
                 if this is unexpected, look for a flapping sensor or a runaway cadence",
                decide_cap_per_hour()
            ),
            &[("retry_in_s", serde_json::json!(retry_in))],
        );
        events::emit(
            paths,
            "tick_capped",
            serde_json::json!({ "retry_in_s": retry_in }),
        );
        return Some(TickOutcome::idle());
    }
    None
}

/// One decide attempt's raw result, handed from [`run_decider`] to
/// [`commit_outcome`].
struct DecideRun {
    run_id: String,
    run_dir: PathBuf,
    secs: u64,
    /// `Ok(())` when the runner exited cleanly; `Err` carries the CAUSE
    /// (unreadable prompt, spawn failure, deadline kill, nonzero exit) so the
    /// failure feedback names it instead of a generic "the runner failed".
    runner: Result<(), String>,
    outcome: Option<Result<executor::Decided>>,
}

/// One decide attempt: build the run dir + prompt, launch the runner (its
/// chatter teed to the replay archive, a spinner on the pulse's stdout), and
/// consume its decision. `None` when the beat idles out before the runner ever
/// launches (config error / no tick command).
fn run_decider(paths: &Paths) -> Option<DecideRun> {
    let cfg = match Config::load(paths) {
        Ok(c) => c,
        Err(e) => {
            util::event(Level::Error, "tick.error", &format!("config: {e}"), &[]);
            return None;
        }
    };
    let runner_name = cfg.runner_label();
    let Some(tick_cmd) = cfg.runner_cmd("tick_command") else {
        util::event(
            Level::Error,
            "tick.error",
            "no `tick` command configured",
            &[("runner", serde_json::json!(runner_name))],
        );
        return None;
    };

    // run_id is second-resolution: on a collision (two decides within the same
    // second — e.g. fast test beats) suffix -2, -3, … so a beat never shares
    // another beat's run dir.
    let base = format!("tick-{}", chrono::Local::now().format("%Y%m%d-%H%M%S"));
    let mut run_id = base.clone();
    let mut run_dir = paths.runs_dir().join(&run_id);
    let mut n = 2u32;
    while run_dir.exists() {
        run_id = format!("{base}-{n}");
        run_dir = paths.runs_dir().join(&run_id);
        n += 1;
    }
    let _ = fs::create_dir_all(&run_dir);
    let prompt_file = run_dir.join("prompt.md");
    let snap = paths.snapshots_dir();
    // FAIL-FAST: a prompt that fails to land (disk full, unwritable run dir)
    // must idle the beat out, NOT launch the runner — a decider reading an
    // empty/missing prompt would decide with zero context and could emit any
    // action. Idling here spends no budget (record_decide comes later) and the
    // next beat simply retries.
    if let Err(e) = fs::write(&prompt_file, prompt::build_prompt(paths, &snap)) {
        util::event(
            Level::Error,
            "tick.error",
            &format!(
                "failed to write the decide prompt ({}): {e}",
                prompt_file.display()
            ),
            &[],
        );
        return None;
    }

    let t0 = Instant::now();
    // In human mode the live spinner below IS the "deciding" indicator, so this
    // line would just duplicate it — emit the marker only to the JSON pulse
    // stream (whose watchers can't see the spinner).
    if util::is_json() {
        util::event(
            Level::Step,
            "tick.start",
            &format!("{runner_name} is deciding the one move"),
            &[
                ("runner", serde_json::json!(runner_name)),
                ("run_id", serde_json::json!(run_id)),
            ],
        );
    }
    events::emit(
        paths,
        "decide_start",
        serde_json::json!({ "runner": runner_name, "run_id": run_id }),
    );

    // The runner's free-form chatter is archived to the tee files (replay from
    // runs/<id>/output.log or tick.log) but NOT echoed live — the pulse stream
    // stays a clean structured-event log.
    let tee: Vec<PathBuf> = vec![run_dir.join("output.log"), paths.data_dir.join("tick.log")];

    // Spend is committed the moment the runner launches — attempts count
    // against the hourly budget whether or not they produce a decision.
    record_decide(paths);

    // Never execute a STALE decision: if a previous beat's runner wrote
    // .decision.json and then FAILED (exited nonzero), the file was never
    // consumed — left in place it would be executed as if THIS beat's runner
    // produced it. Clear it right before launching.
    let stale = paths.data_dir.join(executor::DECISION_FILE);
    if stale.exists() {
        let _ = fs::remove_file(&stale);
        util::event(
            Level::Warn,
            "tick.stale_decision",
            "removed a stale .decision.json left by a previous failed beat before deciding",
            &[],
        );
    }

    let runner = {
        // Show a live "working" indicator on the pulse's stdout while the runner
        // streams (its chatter is teed to the replay archive, not echoed here).
        // Dropped right after the run, which erases the spinner line so the
        // following structured outcome event prints clean.
        let _spin = util::Spinner::start(&format!("{runner_name} is deciding"));
        runner::run_streamed(paths, &tick_cmd, &prompt_file, &tee)
    };
    let secs = t0.elapsed().as_secs();
    let outcome = if runner.is_ok() {
        executor::consume_decision(paths)
    } else {
        None
    };
    Some(DecideRun {
        run_id,
        run_dir,
        secs,
        runner,
        outcome,
    })
}

/// Commit one decide attempt: a beat SUCCEEDS only when a usable decision was
/// produced — commit the world hash, clear backoff, journal the move. Every
/// other outcome arms backoff and leaves the hash uncommitted so a transient
/// issue retries.
fn commit_outcome(paths: &Paths, hash: &str, run: DecideRun) -> TickOutcome {
    let DecideRun {
        run_id,
        run_dir,
        secs,
        runner,
        outcome,
    } = run;
    let (acted, next_interval_s) = match (runner, outcome) {
        (Ok(()), Some(Ok(d))) => {
            // This is the MOST important persistence of the whole beat: the
            // hash commit is what stops the next beat from re-deciding — and
            // possibly re-issuing the same non-idempotent run_shell. A failed
            // write must not be silent.
            if let Err(e) =
                util::write_atomic(&last_tick_hash_path(paths), format!("{hash}\n").as_bytes())
            {
                util::event(
                    Level::Warn,
                    "tick.guard_degraded",
                    &format!(
                        "failed to commit the world hash (the next beat will re-decide and may \
                         re-issue the same move): {e}"
                    ),
                    &[],
                );
            }
            // Commit the WHAT-CHANGED baseline alongside the hash: the next
            // decide prompt diffs the live world against the world THIS decision
            // saw. A failed beat leaves both uncommitted, so the same diff is
            // re-reported until a decision lands.
            // (These are two separate atomic writes, not one transaction: a
            // crash between them leaves the hash committed with a stale
            // baseline, so ONE beat's what-changed diff may over-report. That
            // is accepted — the next committed decision rewrites both, and the
            // hash, which gates spend, always lands first.)
            if let Ok(items) = serde_json::to_string(&crate::worldhash::world_items(paths)) {
                let _ = util::write_atomic(&paths.last_world(), items.as_bytes());
            }
            clear_backoff(paths);
            // This decision consumed the previous failure (if any): the decider
            // saw it in the prompt and moved past it.
            let _ = fs::remove_file(paths.last_failure());
            record_noop(paths, d.kind, hash);
            // A run_shell's output tail is persisted for the NEXT prompt
            // (`RUN_SHELL OUTPUT`), but the command's output alone never moves
            // the world hash — without a nudge the next beat would skip and the
            // decider would never see what its own query returned. Arm a short
            // follow-up unless the decision already scheduled one.
            let next_interval_s = match d.next_interval_s {
                None if d.kind == "shell" => Some(5),
                other => other,
            };
            util::event(
                Level::Ok,
                "tick.decided",
                &format!("{} · {} · {secs}s", d.kind, d.journal),
                &[
                    ("action", serde_json::json!(d.kind)),
                    ("summary", serde_json::json!(d.summary)),
                    ("journal", serde_json::json!(d.journal)),
                    ("secs", serde_json::json!(secs)),
                    ("run_id", serde_json::json!(run_id)),
                ],
            );
            events::emit(
                paths,
                "decided",
                serde_json::json!({ "run_id": run_id, "action": d.kind, "journal": d.journal }),
            );
            // noop is a real decision (the world is fine) — it does not count as
            // "acted" for cadence, but it DID commit the hash above.
            (d.kind != "noop", next_interval_s)
        }
        failure => {
            let fails = record_backoff(paths, hash);
            let replay = run_dir.display().to_string();
            let mut fields = vec![
                ("secs", serde_json::json!(secs)),
                ("run_id", serde_json::json!(run_id)),
                ("fails", serde_json::json!(fails)),
            ];
            let (level, code, err, msg) = match failure {
                (Ok(()), Some(Err(e))) => {
                    fields.push(("error", serde_json::json!(e.to_string())));
                    (
                        Level::Error,
                        "tick.failed",
                        e.to_string(),
                        format!(
                            "decision failed after {secs}s (fail #{fails}): {e} · replay: {replay}"
                        ),
                    )
                }
                (Ok(()), None) => (
                    Level::Warn,
                    "tick.no_decision",
                    "the runner ran but emitted no .decision.json (it must write exactly one \
                     JSON action object to .decision.json, then stop)"
                        .to_string(),
                    format!(
                        "ran {secs}s but emitted no .decision.json (no move, fail #{fails}) · replay: {replay}"
                    ),
                ),
                // The success shape was consumed by the outer match's first
                // arm; the compiler just can't see that through the binding.
                (Ok(()), Some(Ok(_))) => unreachable!("handled by the success arm above"),
                (Err(cause), _) => {
                    fields.push(("replay", serde_json::json!(replay.clone())));
                    fields.push(("error", serde_json::json!(cause.clone())));
                    (
                        Level::Error,
                        "tick.failed",
                        format!(
                            "the runner command failed before producing a decision: {cause}"
                        ),
                        format!(
                            "tick failed after {secs}s (fail #{fails}): {cause} · replay: {replay}"
                        ),
                    )
                }
            };
            // Durable feedback for the NEXT decide prompt (LAST FAILURE section).
            record_failure(paths, &run_id, code, &err, fails);
            util::event(level, code, &msg, &fields);
            events::emit(
                paths,
                "tick_failed",
                serde_json::json!({ "run_id": run_id, "fails": fails }),
            );
            (false, None)
        }
    };
    TickOutcome {
        acted,
        next_interval_s,
        decided_or_failed: true,
    }
}

/// Keep the newest LOOOP_RUNS_KEEP run dirs (default 50; 0 = keep all).
pub fn prune_runs(paths: &Paths) {
    let keep: usize = util::env_knob("LOOOP_RUNS_KEEP").unwrap_or(50);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Wire a FAKE runner into a temp profile: `tick_command` just copies a
    /// pre-staged decision file into `.decision.json` (cwd is the data dir; the
    /// prompt arrives on stdin and is ignored). This exercises the REAL beat —
    /// sense → hash → runner → consume → execute → commit — with no LLM.
    fn wire_fake_runner(p: &Paths, decision_json: &str) {
        fs::write(p.data_dir.join("fixture-decision.json"), decision_json).unwrap();
        fs::write(
            &p.config,
            serde_json::json!({
                "tick_command": "cat fixture-decision.json > .decision.json",
                "worker_command": "true {{prompt_file}}"
            })
            .to_string(),
        )
        .unwrap();
    }

    /// Rewire the runner to one that always fails — used to prove a later beat
    /// SKIPPED (the runner was never invoked) rather than succeeded again.
    fn wire_failing_runner(p: &Paths) {
        fs::write(
            &p.config,
            serde_json::json!({
                "tick_command": "false",
                "worker_command": "true {{prompt_file}}"
            })
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn full_beat_with_fake_runner_commits_then_skips_unchanged_world() {
        let p = Paths::temp();
        fs::write(p.data_dir.join("PLAYBOOK.md"), "be good\n").unwrap();
        wire_fake_runner(
            &p,
            r#"{"action":"noop","reason":"steady","journal":"steady state"}"#,
        );

        // Beat 1 (forced, like a pulse start): the fake runner's noop lands.
        let out = tick(&p, true);
        assert!(!out.acted, "noop is a decision but not an act");
        let committed = fs::read_to_string(p.data_dir.join(".last-tick-hash")).unwrap();
        assert!(!committed.trim().is_empty(), "hash committed on success");
        assert!(p.last_world().is_file(), "WHAT-CHANGED baseline committed");
        assert!(!p.data_dir.join(".tick-backoff").is_file(), "no backoff");
        let journal = fs::read_to_string(p.journal()).unwrap();
        assert!(journal.contains("steady state"), "journal line appended");

        // Beat 2: unchanged world must SKIP — prove it by wiring a runner that
        // would FAIL if invoked, then asserting no failure was recorded.
        wire_failing_runner(&p);
        let out2 = tick(&p, false);
        assert!(!out2.acted);
        assert!(
            !p.last_failure().is_file(),
            "skip means the failing runner was never invoked"
        );
    }

    #[test]
    fn full_beat_executes_a_write_goal_and_reports_acted() {
        let p = Paths::temp();
        fs::write(p.data_dir.join("PLAYBOOK.md"), "be good\n").unwrap();
        wire_fake_runner(
            &p,
            r#"{"action":"write_goal","id":"ship","body":"ship v2","journal":"opened ship goal"}"#,
        );
        let out = tick(&p, true);
        assert!(out.acted, "a real move counts as acted");
        let goal = fs::read_to_string(p.goals_dir().join("ship.md")).unwrap();
        assert_eq!(goal, "ship v2\n");
    }

    #[test]
    fn full_beat_with_failing_runner_arms_backoff_and_records_failure() {
        let p = Paths::temp();
        fs::write(p.data_dir.join("PLAYBOOK.md"), "be good\n").unwrap();
        wire_failing_runner(&p);
        let out = tick(&p, true);
        assert!(!out.acted);
        let (_, fails, _) = read_backoff(&p).expect("backoff armed");
        assert_eq!(fails, 1);
        assert!(p.last_failure().is_file(), "LAST FAILURE feedback recorded");
        assert!(
            !p.data_dir.join(".last-tick-hash").is_file(),
            "a failed beat commits nothing"
        );
    }

    #[test]
    fn run_shell_decision_arms_a_follow_up_nudge() {
        let p = Paths::temp();
        fs::write(p.data_dir.join("PLAYBOOK.md"), "be good\n").unwrap();
        wire_fake_runner(
            &p,
            r#"{"action":"run_shell","cmd":"echo hi","reason":"probe","journal":"probed"}"#,
        );
        let out = tick(&p, true);
        assert!(out.acted);
        assert_eq!(
            out.next_interval_s,
            Some(5),
            "run_shell schedules the follow-up beat that reads its output"
        );
        assert!(p.last_shell().is_file(), "output captured for the prompt");
    }

    #[test]
    fn stale_decision_from_a_failed_beat_is_never_executed() {
        let p = Paths::temp();
        fs::write(p.data_dir.join("PLAYBOOK.md"), "be good\n").unwrap();
        // A previous beat's runner wrote a decision, then FAILED: the file was
        // left behind. This beat's runner writes NOTHING — the stale decision
        // must be cleared before the runner launches, never executed.
        fs::write(
            p.data_dir.join(executor::DECISION_FILE),
            r#"{"action":"write_goal","id":"stale","body":"boom","journal":"stale"}"#,
        )
        .unwrap();
        fs::write(
            &p.config,
            serde_json::json!({
                "tick_command": "true",
                "worker_command": "true {{prompt_file}}"
            })
            .to_string(),
        )
        .unwrap();

        let out = tick(&p, true);
        assert!(out.decided_or_failed, "the runner was launched");
        assert!(
            !p.goals_dir().join("stale.md").exists(),
            "the stale decision must not execute"
        );
        assert!(
            !p.data_dir.join(executor::DECISION_FILE).exists(),
            "the stale file was cleared before the runner ran"
        );
    }

    #[test]
    fn can_skip_only_when_unchanged_and_not_forced() {
        assert!(can_skip("a", "a", false));
        assert!(!can_skip("a", "b", false));
        assert!(!can_skip("a", "a", true));
    }

    #[test]
    fn hanging_runner_is_killed_at_the_deadline_and_fails_the_beat() {
        // Serialize with other env-mutating tests, and restore the knob even
        // if an assert below panics (same discipline as the run_shell timeout
        // test in executor.rs).
        let _env = crate::util::test_env_lock();
        struct Restore;
        impl Drop for Restore {
            fn drop(&mut self) {
                unsafe { std::env::remove_var("LOOOP_TICK_TIMEOUT_SECS") };
            }
        }
        let _restore = Restore;
        let p = Paths::temp();
        fs::write(p.data_dir.join("PLAYBOOK.md"), "be good\n").unwrap();
        // A runner that would hang the whole single-instance pulse forever
        // without the deadline.
        fs::write(
            &p.config,
            serde_json::json!({
                "tick_command": "sleep 30",
                "worker_command": "true {{prompt_file}}"
            })
            .to_string(),
        )
        .unwrap();
        unsafe { std::env::set_var("LOOOP_TICK_TIMEOUT_SECS", "1") };

        let t0 = Instant::now();
        let out = tick(&p, true);
        assert!(
            t0.elapsed().as_secs() < 10,
            "must not wait out the 30s sleep"
        );
        assert!(!out.acted);
        assert!(out.decided_or_failed, "the runner was launched");
        // A timed-out runner is a FAILED beat: backoff arms, the failure is
        // recorded for the next prompt, and nothing is committed.
        let (_, fails, _) = read_backoff(&p).expect("backoff armed");
        assert_eq!(fails, 1);
        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(p.last_failure()).unwrap()).unwrap();
        assert!(
            v["error"]
                .as_str()
                .unwrap()
                .contains("timed out after 1s"),
            "the failure names the deadline: {v}"
        );
        assert!(
            !p.data_dir.join(".last-tick-hash").is_file(),
            "a timed-out beat commits nothing"
        );
    }

    #[test]
    fn record_failure_persists_the_feedback_for_the_next_prompt() {
        let p = Paths::temp();
        record_failure(
            &p,
            "tick-x",
            "tick.failed",
            "run_shell exited 2: gh not found",
            3,
        );
        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(p.last_failure()).unwrap()).unwrap();
        assert_eq!(v["code"], "tick.failed");
        assert_eq!(v["fails"], 3);
        assert!(v["error"].as_str().unwrap().contains("gh not found"));
    }
}
