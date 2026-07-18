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
use crate::store::{Collection, FileStore, Key, StateStore};
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
    let body = serde_json::json!({ "v": 1, "hash": hash, "fails": fails, "ts": util::now_unix() })
        .to_string();
    if let Err(e) = util::write_atomic(&backoff_path(paths), body.as_bytes()) {
        util::event(
            Level::Warn,
            "tick.guard_degraded",
            &format!("failed to persist backoff state (retry guard degraded): {e}"),
            &[],
        );
    }
    fails
}

/// Whether this beat may skip the AI: the world is unchanged since last beat AND
/// the decider did NOT request a forced re-decide (`force`, set when the previous
/// beat emitted a `next_interval_s` nudge for a time-based follow-up).
fn can_skip(hash: &str, last: &str, force: bool) -> bool {
    hash == last && !force
}

// ---- noop TTL (revisit) -------------------------------------------------------

/// How long an unchanged world may coast on a `noop` decision before the beat
/// re-decides anyway. A single wrong noop must not park a world state forever:
/// the skip gate is bypassed once the last decision was a noop older than this.
/// `LOOOP_NOOP_TTL` seconds; 0 disables; default 6h.
fn noop_ttl_secs() -> u64 {
    std::env::var("LOOOP_NOOP_TTL")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(6 * 3600)
}

/// Record that the latest decision was a noop at `hash` (or clear it for any
/// other action — a real move resets the revisit clock).
fn record_noop(paths: &Paths, kind: &str, hash: &str) {
    if kind == "noop" {
        let body = serde_json::json!({ "v": 1, "ts": util::now_unix(), "hash": hash }).to_string();
        let _ = util::write_atomic(&paths.noop_at(), body.as_bytes());
    } else {
        let _ = fs::remove_file(paths.noop_at());
    }
}

/// Whether the skip gate should be BYPASSED: the last decision at this same
/// world hash was a noop, and it has aged past the TTL. Consuming the record
/// (fresh one written after the re-decision) keeps this one-shot per TTL window.
fn noop_revisit_due(paths: &Paths, hash: &str) -> bool {
    let ttl = noop_ttl_secs();
    if ttl == 0 {
        return false;
    }
    let Ok(raw) = fs::read_to_string(paths.noop_at()) else {
        return false;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return false;
    };
    let same = v.get("hash").and_then(|h| h.as_str()) == Some(hash);
    let ts = v.get("ts").and_then(|t| t.as_u64()).unwrap_or(0);
    same && util::now_unix().saturating_sub(ts) >= ttl
}

// ---- flapping-sensor detection --------------------------------------------------

/// How many CONSECUTIVE beats a snapshot's wake signal must change before it is
/// flagged as flapping (`LOOOP_FLAP_STREAK`; 0 disables; default 5).
fn flap_streak_threshold() -> u32 {
    std::env::var("LOOOP_FLAP_STREAK")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(5)
}

/// Update the per-snapshot signal-change streaks after a sense, and return the
/// names currently at/over the flapping threshold.
///
/// WHY THIS EXISTS: the loop's entire cost model — "an unchanged world costs no
/// AI call" — hinges on sensor authors correctly splitting volatile fields into
/// `.detail`. A sensor that leaks a timestamp/counter into `.signal` silently
/// defeats BOTH the skip gate and the failure backoff (the world hash never
/// settles, and a moving hash clears backoff), turning a quiet loop into one
/// decide per beat forever. Nothing else in the system detects that mistake, so
/// the beat tracks it mechanically: a signal that has changed on N consecutive
/// beats is surfaced in the prompt (`FLAPPING SENSORS`) for the decider to fix
/// (move the volatile fields to `.detail`) and warned once when crossing the
/// threshold.
fn update_flap(paths: &Paths) -> Vec<String> {
    let threshold = flap_streak_threshold();
    if threshold == 0 {
        return Vec::new();
    }
    let prev: serde_json::Value = fs::read_to_string(paths.flap_state())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(serde_json::Value::Null);
    let prev_snaps = prev
        .get("snaps")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();

    let mut snaps = serde_json::Map::new();
    let mut flapping = Vec::new();
    for (name, signal) in crate::worldhash::world_items(paths) {
        let Some(name) = name.strip_prefix("snap:") else {
            continue; // policy files are the human's/decider's to edit — not flap
        };
        let streak = match prev_snaps.get(name) {
            Some(e) if e.get("last").and_then(|v| v.as_str()) == Some(signal.as_str()) => 0,
            Some(e) => e.get("streak").and_then(|v| v.as_u64()).unwrap_or(0) as u32 + 1,
            None => 0, // first sighting — nothing to compare against
        };
        if streak >= threshold {
            flapping.push(name.to_string());
            if streak == threshold {
                util::event(
                    Level::Warn,
                    "sense.flapping",
                    &format!(
                        "{name}: wake signal changed on {streak} consecutive beats — volatile \
                         data is likely leaking into .signal (belongs in .detail); every such \
                         beat costs a decide"
                    ),
                    &[
                        ("sensor", serde_json::json!(name)),
                        ("streak", serde_json::json!(streak)),
                    ],
                );
                events::emit(
                    paths,
                    "sensor_flapping",
                    serde_json::json!({ "sensor": name, "streak": streak }),
                );
            }
        }
        snaps.insert(
            name.to_string(),
            serde_json::json!({ "last": signal, "streak": streak }),
        );
    }
    let body = serde_json::json!({ "v": 1, "snaps": snaps }).to_string();
    let _ = fs::write(paths.flap_state(), body);
    flapping
}

/// The snapshot names currently flagged as flapping (streak at/over the
/// threshold), read from the ledger [`update_flap`] maintains. Consumed by the
/// decide prompt's `FLAPPING SENSORS` section.
pub fn flapping_sensors(paths: &Paths) -> Vec<String> {
    let threshold = flap_streak_threshold();
    if threshold == 0 {
        return Vec::new();
    }
    let Ok(raw) = fs::read_to_string(paths.flap_state()) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return Vec::new();
    };
    let mut out: Vec<String> = v
        .get("snaps")
        .and_then(|s| s.as_object())
        .map(|m| {
            m.iter()
                .filter(|(_, e)| {
                    e.get("streak").and_then(|x| x.as_u64()).unwrap_or(0) >= threshold as u64
                })
                .map(|(k, _)| k.clone())
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

// ---- decide rate cap (global spend ceiling) --------------------------------------

/// Max decide ATTEMPTS per rolling hour (`LOOOP_MAX_DECIDES_PER_HOUR`; 0
/// disables; default 120). The skip gate and backoff bound a QUIET loop's
/// spend; nothing else bounds a noisy one — cadence nudges can legally reach
/// one decide per 5s (720/h), and a flapping sensor re-arms the beat forever.
/// This is the hard ceiling underneath both: attempts (not successes) count,
/// so failing beats spend budget too.
fn decide_cap_per_hour() -> u64 {
    std::env::var("LOOOP_MAX_DECIDES_PER_HOUR")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(120)
}

fn read_decide_ledger(paths: &Paths) -> Vec<u64> {
    fs::read_to_string(paths.decide_ledger())
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("ts").cloned())
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

/// Whether the hourly decide budget still has room. Returns `Err(retry_in_s)`
/// when exhausted (seconds until the oldest attempt ages out of the window).
fn decide_budget(now: u64, ledger: &[u64], cap: u64) -> Result<(), u64> {
    if cap == 0 {
        return Ok(());
    }
    let recent: Vec<u64> = ledger
        .iter()
        .copied()
        .filter(|t| now.saturating_sub(*t) < 3600)
        .collect();
    if (recent.len() as u64) < cap {
        return Ok(());
    }
    let oldest = recent.iter().copied().min().unwrap_or(now);
    Err((oldest + 3600).saturating_sub(now).max(1))
}

/// Record one decide attempt and prune the ledger to the rolling hour.
fn record_decide(paths: &Paths) {
    let now = util::now_unix();
    let mut ts = read_decide_ledger(paths);
    ts.retain(|t| now.saturating_sub(*t) < 3600);
    ts.push(now);
    let body = serde_json::json!({ "v": 1, "ts": ts }).to_string();
    if let Err(e) = util::write_atomic(&paths.decide_ledger(), body.as_bytes()) {
        util::event(
            Level::Warn,
            "tick.guard_degraded",
            &format!("failed to persist the decide ledger (spend guard degraded): {e}"),
            &[],
        );
    }
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
    let _ = fs::write(paths.last_failure(), body);
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
    runner_ok: bool,
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
    let _ = fs::write(&prompt_file, prompt::build_prompt(paths, &snap));

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

    let runner_ok = {
        // Show a live "working" indicator on the pulse's stdout while the runner
        // streams (its chatter is teed to the replay archive, not echoed here).
        // Dropped right after the run, which erases the spinner line so the
        // following structured outcome event prints clean.
        let _spin = util::Spinner::start(&format!("{runner_name} is deciding"));
        runner::run_streamed(paths, &tick_cmd, &prompt_file, &tee)
    };
    let secs = t0.elapsed().as_secs();
    let outcome = if runner_ok {
        executor::consume_decision(paths)
    } else {
        None
    };
    Some(DecideRun {
        run_id,
        run_dir,
        secs,
        runner_ok,
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
        runner_ok,
        outcome,
    } = run;
    let (acted, next_interval_s) = match (runner_ok, outcome) {
        (true, Some(Ok(d))) => {
            let _ = util::write_atomic(&last_tick_hash_path(paths), format!("{hash}\n").as_bytes());
            // Commit the WHAT-CHANGED baseline alongside the hash: the next
            // decide prompt diffs the live world against the world THIS decision
            // saw. A failed beat leaves both uncommitted, so the same diff is
            // re-reported until a decision lands.
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
                (true, Some(Err(e))) => {
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
                (true, None) => (
                    Level::Warn,
                    "tick.no_decision",
                    "the runner ran but emitted no .decision.json (it must write exactly one \
                     JSON action object to .decision.json, then stop)"
                        .to_string(),
                    format!(
                        "ran {secs}s but emitted no .decision.json (no move, fail #{fails}) · replay: {replay}"
                    ),
                ),
                _ => {
                    fields.push(("replay", serde_json::json!(replay.clone())));
                    (
                        Level::Error,
                        "tick.failed",
                        "the runner command itself failed (crashed / exited nonzero) before \
                         producing a decision"
                            .to_string(),
                        format!("tick failed after {secs}s (fail #{fails}) · replay: {replay}"),
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
    // store.list is already sorted.
    FileStore::new(paths).list(&Collection::Goals)
}

fn journal_tail(paths: &Paths, n: usize) -> Vec<String> {
    let text = FileStore::new(paths)
        .read(&Key::Journal)
        .unwrap_or_default();
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
        // Is the autonomous loop actually running? Without it the snapshots/fleet
        // below are frozen at the last beat, so a client must know the pulse is
        // down before trusting (or waiting on) this state.
        "pulse_alive": crate::run::pulse_running(paths),
        "snapshots": snapshots(paths),
        "asks": asks,
        "workers": workers,
        "goals": goal_ids(paths),
        "journal_tail": journal_tail(paths, 20),
        "data_dir": paths.data_dir.to_string_lossy(),
    })
}

/// Which kinds of change should make `wait` return. The diff is computed per
/// category (see [`fingerprints`]) so a noisy snapshot-only move can be filtered
/// out by a client that only cares about asks / journal progress.
#[derive(Clone, Copy)]
pub(crate) enum WaitFilter {
    /// Wake on ANY category change (default — the historical behavior).
    Any,
    /// Wake only when the pending-asks set changes (`--only-asks`).
    Asks,
    /// Wake only on asks or journal changes (`--actionable`).
    Actionable,
}

/// Per-category content fingerprints, so `wait` can report WHAT changed, not
/// just that the world hash moved. Categories: asks (the pending mailbox),
/// journal, playbook, goals, snapshots (sensors + the live worker fleet).
fn fingerprints(paths: &Paths) -> std::collections::BTreeMap<&'static str, String> {
    let mut m = std::collections::BTreeMap::new();

    let asks: Vec<serde_json::Value> = mailbox::pending(paths)
        .into_iter()
        .map(|a| serde_json::to_value(a).unwrap_or_default())
        .collect();
    m.insert(
        "asks",
        util::content_hash(serde_json::Value::Array(asks).to_string().as_bytes()),
    );
    m.insert(
        "journal",
        util::content_hash(&fs::read(paths.journal()).unwrap_or_default()),
    );
    m.insert(
        "playbook",
        util::content_hash(&fs::read(paths.playbook()).unwrap_or_default()),
    );

    let mut goals = Vec::new();
    for id in goal_ids(paths) {
        goals.extend_from_slice(id.as_bytes());
        goals.push(b'\n');
        goals.extend_from_slice(
            &fs::read(paths.goals_dir().join(format!("{id}.md"))).unwrap_or_default(),
        );
    }
    m.insert("goals", util::content_hash(&goals));

    // Snapshots: only the wake SIGNAL (matching world_hash) so volatile `.detail`
    // never registers as a change. `snapshots()` returns sorted keys.
    let mut snaps = Vec::new();
    for (k, v) in snapshots(paths) {
        snaps.extend_from_slice(k.as_bytes());
        snaps.push(b'\n');
        snaps.extend_from_slice(crate::worldhash::wake_signal(v).to_string().as_bytes());
        snaps.push(b'\n');
    }
    m.insert("snapshots", util::content_hash(&snaps));

    m
}

/// Categories whose fingerprint differs between two snapshots, sorted (BTreeMap).
fn changed_categories(
    base: &std::collections::BTreeMap<&'static str, String>,
    cur: &std::collections::BTreeMap<&'static str, String>,
) -> Vec<String> {
    base.iter()
        .filter(|(k, v)| cur.get(*k) != Some(*v))
        .map(|(k, _)| k.to_string())
        .collect()
}

/// Block until there is something to look at, then return the list of categories
/// that changed. "Something" = a pending ask (return immediately) OR a category
/// move that passes `filter`. Pure read — never senses, so it can't race the pulse.
pub(crate) fn wait_for_change(paths: &Paths, filter: WaitFilter) -> Vec<String> {
    let poll = std::time::Duration::from_millis(
        std::env::var("LOOOP_WAIT_POLL_MS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(1000),
    );
    // An ask already waiting is actionable for every filter: don't block.
    if !mailbox::pending(paths).is_empty() {
        return vec!["asks".to_string()];
    }
    let baseline = fingerprints(paths);
    loop {
        // Re-check the mailbox DIRECTLY every poll: an ask that lands between
        // the pre-loop pending check and the baseline snapshot above gets baked
        // into the baseline and would never register as a fingerprint diff.
        // A pending ask is actionable for every filter, so it is an absolute
        // wake condition, not a diff.
        if !mailbox::pending(paths).is_empty() {
            return vec!["asks".to_string()];
        }
        // The pulse is the only thing that drives autonomous change; if it isn't
        // running, these files will never move, so don't block forever — wake the
        // caller with a distinct `pulse-down` signal (filter-independent: a dead
        // loop is critical no matter what a client narrowed its wait to).
        if !crate::run::pulse_running(paths) {
            return vec!["pulse-down".to_string()];
        }
        let changed = changed_categories(&baseline, &fingerprints(paths));
        let hit = match filter {
            WaitFilter::Any => !changed.is_empty(),
            WaitFilter::Asks => changed.iter().any(|c| c == "asks"),
            WaitFilter::Actionable => changed.iter().any(|c| c == "asks" || c == "journal"),
        };
        if hit {
            return changed;
        }
        std::thread::sleep(poll);
    }
}

/// Render a unix-seconds age as a compact human delta ("just now", "4m", "2h",
/// "3d") so the plain `state` / `wait` output can show how long an ask has
/// been waiting without the caller doing clock math.
fn fmt_ago(ts: u64) -> String {
    let now = util::now_unix();
    let secs = now.saturating_sub(ts);
    if secs < 45 {
        "just now".to_string()
    } else if secs < 90 {
        "1m ago".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

/// First line of `s`, trimmed and clipped to `max` chars (… suffix when cut), so
/// a multi-line ask prompt collapses to a single readable summary line.
fn one_line(s: &str, max: usize) -> String {
    let first = s.lines().next().unwrap_or("").trim();
    if first.chars().count() > max {
        let head: String = first.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    } else {
        first.to_string()
    }
}

/// Print the current state. `--json` = full structured object; else a summary.
/// `changed` (set by `wait`) is surfaced as a `changed: […]` diff summary so a
/// caller knows WHICH categories moved without re-diffing the whole state.
///
/// The plain summary is intentionally rich enough to STAND ALONE: pending asks
/// (with age), the live worker fleet, each sensor's wake signal, and the last
/// few journal lines — so a client woken by `wait` never has to follow up
/// with `tail journal.md` / `state --json | jq` to see what actually moved.
/// Render a state value (from [`crate::contract::Contract::state`] / `wait`) to
/// stdout. A `"changed"` array on the value (present only for `wait`) prints the
/// `changed:` diff line; absent (plain `state`) skips it. PRESENTATION ONLY — the
/// data assembly lives behind the contract, so this is the CLI transport's job.
pub(crate) fn render_state(s: &serde_json::Value, json: bool) -> Result<ExitCode> {
    if json {
        println!("{}", serde_json::to_string_pretty(s)?);
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(ch) = s.get("changed").and_then(|c| c.as_array()) {
        println!(
            "changed: {}",
            if ch.is_empty() {
                "(none)".to_string()
            } else {
                ch.iter()
                    .filter_map(|c| c.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        );
    }
    let asks = s["asks"].as_array().cloned().unwrap_or_default();
    let workers = s["workers"].as_array().cloned().unwrap_or_default();
    let goals = s["goals"].as_array().map(|a| a.len()).unwrap_or(0);
    let live = workers
        .iter()
        .filter(|w| w["alive"].as_bool().unwrap_or(false))
        .count();
    let pulse_alive = s["pulse_alive"].as_bool().unwrap_or(false);
    println!(
        "pulse: {}  ·  asks: {}  ·  workers: {live} live / {}  ·  goals: {goals}",
        if pulse_alive { "live" } else { "DOWN" },
        asks.len(),
        workers.len()
    );
    if !pulse_alive {
        println!(
            "  ⚠ the autonomous loop is not running — run `looop up` (no beats, snapshots are stale)"
        );
    }

    // Pending asks, each with WHICH worker + HOW LONG it has been waiting, so the
    // freshness of a blocked decision is obvious at a glance.
    for a in &asks {
        let mut head = format!(
            "  ⚑ {} ({} · {}): {}",
            a["id"].as_str().unwrap_or("?"),
            a["worker"].as_str().unwrap_or("?"),
            fmt_ago(a["ts"].as_u64().unwrap_or(0)),
            one_line(a["prompt"].as_str().unwrap_or(""), 100),
        );
        if let Some(r) = a["reference"].as_str().filter(|r| !r.is_empty()) {
            head.push_str(&format!("\n      ref: {r}"));
        }
        if let Some(opts) = a["options"].as_array().filter(|o| !o.is_empty()) {
            let opts: Vec<&str> = opts.iter().filter_map(|o| o.as_str()).collect();
            head.push_str(&format!("\n      options: {}", opts.join(", ")));
        }
        println!("{head}");
    }

    // Sensor readings — one line per snapshot's wake SIGNAL. This is where a
    // user `gh`/PR-review sensor surfaces (e.g. a stale CHANGES_REQUESTED), so
    // a client sees PR state in `state` instead of shelling out to `gh`.
    let snaps = s["snapshots"].as_object().cloned().unwrap_or_default();
    if !snaps.is_empty() {
        println!("sensors:");
        for (k, v) in &snaps {
            let signal = crate::worldhash::wake_signal(v.clone());
            println!("  {k}: {}", one_line(&signal.to_string(), 100));
        }
    }

    // Live workers — id + state, so "who is running" needs no `--json | jq`.
    let alive: Vec<&serde_json::Value> = workers
        .iter()
        .filter(|w| w["alive"].as_bool().unwrap_or(false))
        .collect();
    if !alive.is_empty() {
        println!("workers (live):");
        for w in alive {
            println!(
                "  ● {}  {}",
                w["id"].as_str().unwrap_or("?"),
                w["state"].as_str().unwrap_or("?")
            );
        }
    }

    // Last few journal lines — so a `changed: journal` wake is self-explanatory
    // and the caller never has to `tail journal.md` to learn what looop just did.
    let jtail = s["journal_tail"].as_array().cloned().unwrap_or_default();
    let recent: Vec<&str> = jtail
        .iter()
        .rev()
        .take(3)
        .filter_map(|l| l.as_str())
        .collect();
    if !recent.is_empty() {
        println!("journal (latest):");
        for l in recent.into_iter().rev() {
            println!("  {l}");
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `looop state [--json]` — read the current world state. Pure read: no
/// sensing, no side effects (the autonomous pulse keeps snapshots fresh).
pub fn cmd_state(paths: &Paths, json: bool) -> Result<ExitCode> {
    use crate::contract::Contract;
    let s = crate::contract::LocalContract::new(paths).state()?;
    render_state(&s, json)
}

/// `looop wait [--json] [--only-asks|--actionable]` — BLOCK until there is
/// something to look at, then print the fresh state plus a `changed: […]` diff
/// summary. By default any category move (asks / journal / playbook / goals /
/// snapshots) wakes it; `--actionable` narrows to asks+journal and `--only-asks`
/// to asks alone, so a watching client can ignore noisy snapshot-only moves.
///
/// It also wakes — regardless of filter — with `changed: [pulse-down]` if the
/// autonomous loop isn't running, so a blocked client is never left hanging on a
/// dead pulse (nothing would ever change the files to wake it otherwise).
pub fn cmd_wait(paths: &Paths, args: &crate::cli::WaitArgs) -> Result<ExitCode> {
    let _ = crate::seed::ensure_dirs(paths);
    use crate::contract::Contract;
    let filter = if args.only_asks {
        WaitFilter::Asks
    } else if args.actionable {
        WaitFilter::Actionable
    } else {
        WaitFilter::Any
    };
    let s = crate::contract::LocalContract::new(paths).wait(filter)?;
    render_state(&s, args.json)
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
    fn decide_budget_blocks_at_cap_and_names_the_retry() {
        // Under cap: fine.
        assert!(decide_budget(1000, &[], 2).is_ok());
        assert!(decide_budget(1000, &[500], 2).is_ok());
        // At cap: blocked until the oldest attempt ages out of the hour.
        let err = decide_budget(1000, &[500, 900], 2).unwrap_err();
        assert_eq!(err, 500 + 3600 - 1000);
        // Old attempts age out of the window.
        assert!(decide_budget(5000, &[500, 900], 2).is_ok());
        // 0 disables.
        assert!(decide_budget(1000, &[1, 2, 3], 0).is_ok());
    }

    #[test]
    fn record_decide_appends_and_prunes_the_rolling_hour() {
        let p = Paths::temp();
        let old = util::now_unix() - 4000;
        fs::write(
            p.decide_ledger(),
            serde_json::json!({ "v": 1, "ts": [old] }).to_string(),
        )
        .unwrap();
        record_decide(&p);
        let ts = read_decide_ledger(&p);
        assert_eq!(ts.len(), 1, "the aged-out attempt was pruned");
        assert!(util::now_unix() - ts[0] < 5);
    }

    #[test]
    fn flapping_is_flagged_after_consecutive_signal_changes_and_resets() {
        let p = Paths::temp();
        fs::create_dir_all(p.snapshots_dir()).unwrap();
        let snap = p.snapshots_dir().join("sensor-noisy.json");
        let write = |n: u64| fs::write(&snap, format!(r#"{{"signal":{{"n":{n}}}}}"#)).unwrap();

        // First sighting establishes a baseline; each subsequent CHANGE bumps
        // the streak. Threshold 5 ⇒ flagged on the 5th consecutive change.
        write(0);
        assert!(update_flap(&p).is_empty());
        for i in 1..=4u64 {
            write(i);
            assert!(update_flap(&p).is_empty(), "streak {i} is below threshold");
        }
        write(5);
        assert_eq!(update_flap(&p), vec!["sensor-noisy".to_string()]);
        assert_eq!(
            flapping_sensors(&p),
            vec!["sensor-noisy".to_string()],
            "the prompt reads the same verdict from the ledger"
        );

        // An unchanged beat resets the streak — a settled sensor is forgiven.
        assert!(update_flap(&p).is_empty());
        assert!(flapping_sensors(&p).is_empty());
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
    fn backoff_delay_grows_then_caps() {
        assert_eq!(backoff_delay(0), 0);
        assert_eq!(backoff_delay(1), BACKOFF_BASE_SECS);
        assert_eq!(backoff_delay(2), BACKOFF_BASE_SECS * 2);
        assert_eq!(backoff_delay(99), BACKOFF_CAP_SECS);
    }

    #[test]
    fn noop_ttl_bypasses_skip_only_for_an_aged_noop_at_the_same_hash() {
        let p = Paths::temp();
        // No record: never revisit.
        assert!(!noop_revisit_due(&p, "h1"));

        // Fresh noop at h1: not due yet.
        record_noop(&p, "noop", "h1");
        assert!(!noop_revisit_due(&p, "h1"));

        // Age the record past the TTL: due at the SAME hash only.
        let old = util::now_unix() - noop_ttl_secs() - 1;
        fs::write(
            p.noop_at(),
            serde_json::json!({ "ts": old, "hash": "h1" }).to_string(),
        )
        .unwrap();
        assert!(
            noop_revisit_due(&p, "h1"),
            "aged noop at same hash re-decides"
        );
        assert!(
            !noop_revisit_due(&p, "h2"),
            "different world: normal skip rules"
        );

        // A real (non-noop) decision clears the record.
        record_noop(&p, "goal", "h1");
        assert!(!p.noop_at().is_file());
        assert!(!noop_revisit_due(&p, "h1"));
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
        let asks = s["asks"].as_array().unwrap();
        assert_eq!(asks.len(), 2);
        assert!(asks.iter().any(|a| a["id"] == "setup-1"));
        assert!(asks.iter().any(|a| a["id"] == "triage-1"));
    }

    #[test]
    fn fingerprints_pin_each_category_independently() {
        let p = Paths::temp();
        let _ = crate::seed::ensure_dirs(&p);
        let base = fingerprints(&p);

        // A goal edit moves only the goals category.
        fs::write(p.goals_dir().join("g.md"), b"do the thing\n").unwrap();
        assert_eq!(changed_categories(&base, &fingerprints(&p)), vec!["goals"]);

        // A new pending ask moves only the asks category.
        let after_goal = fingerprints(&p);
        fs::write(
            p.asks_dir().join("w-1.json"),
            serde_json::json!({"id":"w-1","worker":"w","prompt":"ok?","ts":1}).to_string(),
        )
        .unwrap();
        assert_eq!(
            changed_categories(&after_goal, &fingerprints(&p)),
            vec!["asks"]
        );

        // A journal append moves only the journal category.
        let after_ask = fingerprints(&p);
        fs::write(p.journal(), b"progress\n").unwrap();
        assert_eq!(
            changed_categories(&after_ask, &fingerprints(&p)),
            vec!["journal"]
        );
    }

    #[test]
    fn wait_returns_immediately_when_an_ask_is_already_pending() {
        let p = Paths::temp();
        let _ = crate::seed::ensure_dirs(&p);
        fs::write(
            p.asks_dir().join("w-1.json"),
            serde_json::json!({"id":"w-1","worker":"w","prompt":"ok?","ts":1}).to_string(),
        )
        .unwrap();
        // No blocking: a waiting ask is actionable for every filter.
        assert_eq!(wait_for_change(&p, WaitFilter::Asks), vec!["asks"]);
        assert_eq!(wait_for_change(&p, WaitFilter::Actionable), vec!["asks"]);
        assert_eq!(wait_for_change(&p, WaitFilter::Any), vec!["asks"]);
    }

    #[test]
    fn state_reports_pulse_down_when_nothing_holds_the_lock() {
        let p = Paths::temp();
        let _ = crate::seed::ensure_dirs(&p);
        // No pulse has ever acquired the flock in a fresh temp dir.
        assert_eq!(state(&p)["pulse_alive"], serde_json::json!(false));
    }

    #[test]
    fn wait_wakes_with_pulse_down_instead_of_blocking_when_the_loop_is_dead() {
        let p = Paths::temp();
        let _ = crate::seed::ensure_dirs(&p);
        crate::mailbox::answer(&p, "setup-1", "handled", false).unwrap();
        // No ask pending and no pulse running: every filter must wake (not hang)
        // with the distinct pulse-down signal.
        assert_eq!(wait_for_change(&p, WaitFilter::Any), vec!["pulse-down"]);
        assert_eq!(wait_for_change(&p, WaitFilter::Asks), vec!["pulse-down"]);
        assert_eq!(
            wait_for_change(&p, WaitFilter::Actionable),
            vec!["pulse-down"]
        );
    }
}
