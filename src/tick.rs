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
    let runner_name = cfg.runner_label();
    let Some(tick_cmd) = cfg.runner_cmd("tick_command") else {
        util::event(
            Level::Error,
            "tick.error",
            "no `tick` command configured",
            &[("runner", serde_json::json!(runner_name))],
        );
        return TickOutcome::idle();
    };

    let run_id = format!("tick-{}", chrono::Local::now().format("%Y%m%d-%H%M%S"));
    let run_dir = paths.runs_dir().join(&run_id);
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
            ("run_id", serde_json::json!(run_id)),
        ],
    );
    events::emit(
        paths,
        "decide_start",
        serde_json::json!({ "runner": runner_name, "run_id": run_id }),
    );

    // The runner's free-form chatter is archived to the tee files (replay from
    // runs/<id>/output.log or tick.log) but NOT echoed live — the pulse stream
    // stays a clean structured-event log.
    let tee: Vec<PathBuf> = vec![run_dir.join("output.log"), paths.data_dir.join("tick.log")];

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

    // A beat SUCCEEDS only when a usable decision was produced: commit the world
    // hash, clear backoff, journal the move. Every other outcome arms backoff and
    // leaves the hash uncommitted so a transient issue retries.
    let (acted, next_interval_s) = match (runner_ok, outcome) {
        (true, Some(Ok(d))) => {
            let _ = fs::write(paths.data_dir.join(".last-tick-hash"), format!("{hash}\n"));
            clear_backoff(paths);
            let next_interval_s = d.next_interval_s;
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
            let fails = record_backoff(paths, &hash);
            let replay = run_dir.display().to_string();
            let mut fields = vec![
                ("secs", serde_json::json!(secs)),
                ("run_id", serde_json::json!(run_id)),
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
                serde_json::json!({ "run_id": run_id, "fails": fails }),
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
        "snapshots": snapshots(paths),
        "asks": asks,
        "workers": workers,
        "goals": goal_ids(paths),
        "journal_tail": journal_tail(paths, 20),
        "data_dir": paths.data_dir.to_string_lossy(),
    })
}

/// Which kinds of change should make `_ wait` return. The diff is computed per
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

/// Per-category content fingerprints, so `_ wait` can report WHAT changed, not
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
/// "3d") so the plain `_ state` / `_ wait` output can show how long an ask has
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
/// `changed` (set by `_ wait`) is surfaced as a `changed: […]` diff summary so a
/// caller knows WHICH categories moved without re-diffing the whole state.
///
/// The plain summary is intentionally rich enough to STAND ALONE: pending asks
/// (with age), the live worker fleet, each sensor's wake signal, and the last
/// few journal lines — so a client woken by `_ wait` never has to follow up
/// with `tail journal.md` / `_ state --json | jq` to see what actually moved.
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
    println!(
        "asks: {}  ·  workers: {live} live / {}  ·  goals: {goals}",
        asks.len(),
        workers.len()
    );

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
    // a client sees PR state in `_ state` instead of shelling out to `gh`.
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

/// `looop _ state [--json]` — read the current world state. Pure read: no
/// sensing, no side effects (the autonomous pulse keeps snapshots fresh).
pub fn cmd_state(paths: &Paths, json: bool) -> Result<ExitCode> {
    use crate::contract::Contract;
    let s = crate::contract::LocalContract::new(paths).state()?;
    render_state(&s, json)
}

/// `looop _ wait [--json] [--only-asks|--actionable]` — BLOCK until there is
/// something to look at, then print the fresh state plus a `changed: […]` diff
/// summary. By default any category move (asks / journal / playbook / goals /
/// snapshots) wakes it; `--actionable` narrows to asks+journal and `--only-asks`
/// to asks alone, so a watching client can ignore noisy snapshot-only moves.
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
}
