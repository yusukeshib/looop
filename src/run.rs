//! The pulse (`looop pulse`) — looop's AUTONOMOUS control loop.
//!
//! Each beat: sense the world, and — when it changed since last beat — hand it to
//! the configured `tick` runner for ONE move, which looop executes through the
//! typed [`crate::executor`] actions (RULE 1: one tick = one move). Judgment
//! lives HERE, in looop; the human is a peer who steers by editing goals/PLAYBOOK
//! and answers worker questions via the ask/answer mailbox (surfaced by a
//! client — the human-facing interface, not a decision-maker).
//!
//! It is a single-instance loop (flock) and the SOLE senser/decider, so two beats
//! never wipe `snapshots/` or decide under each other. An unchanged world skips
//! the AI entirely, so a quiet loop is nearly free.

use crate::config::Config;
use crate::paths::Paths;
use crate::util::Level;
use crate::{seed, tick, util};
use anyhow::Result;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

/// Retention for `sessions/<id>/` corpses (system scratch): env
/// `LOOOP_SESSION_TTL` (seconds) > config `session_ttl` > 3 days. looop owns
/// reaping its own scratch; a worker's durable output lives in reports/ + git +
/// its sandbox, never here, so this only bounds debug transcripts.
pub(crate) fn session_ttl_secs(paths: &Paths) -> u64 {
    const DEFAULT: u64 = 3 * 24 * 60 * 60; // 3 days
    if let Ok(v) = std::env::var("LOOOP_SESSION_TTL")
        && let Ok(n) = v.trim().parse::<u64>()
    {
        return n;
    }
    Config::load(paths)
        .ok()
        .and_then(|c| {
            c.root
                .get("session_ttl")
                .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64)))
        })
        .unwrap_or(DEFAULT)
}

/// Resolve a cadence knob: env var > config key > fallback.
fn interval(env: &str, cfg: &Config, key: &str, fallback: u64) -> u64 {
    if let Ok(v) = std::env::var(env)
        && let Ok(n) = v.trim().parse::<u64>()
    {
        return n;
    }
    cfg.root
        .get(key)
        .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64)))
        .unwrap_or(fallback)
}

// ---- durable cadence nudge (.next-wake.json) ---------------------------------

/// The pending next-wake deadline (unix secs), if any.
fn read_next_wake(paths: &Paths) -> Option<u64> {
    let raw = fs::read_to_string(paths.next_wake()).ok()?;
    serde_json::from_str::<serde_json::Value>(&raw)
        .ok()?
        .get("due")
        .and_then(|v| v.as_u64())
}

/// Persist a one-shot cadence nudge as a DEADLINE, not an in-memory flag: a
/// pulse crash during the sleep no longer loses the follow-up — the next pulse
/// (or the next loop iteration) sees the file and re-decides once it's due.
fn write_next_wake(paths: &Paths, due: u64) {
    let _ = fs::write(
        paths.next_wake(),
        serde_json::json!({ "v": 1, "due": due }).to_string(),
    );
}

/// Consume the nudge when due: returns true (and removes the file) when the
/// deadline has passed, arming a forced re-decide this beat.
fn consume_due_next_wake(paths: &Paths) -> bool {
    match read_next_wake(paths) {
        Some(due) if crate::util::now_unix() >= due => {
            let _ = fs::remove_file(paths.next_wake());
            true
        }
        _ => false,
    }
}

/// A non-blocking exclusive `flock(2)` on an open fd. `true` = we hold it now.
/// flock is the right primitive for single-instance: the kernel releases it when
/// the holding process dies for ANY reason (normal exit, panic, `kill -9`, crash),
/// so there is no stale lock to reclaim and no PID-liveness guessing that a reused
/// PID can fool. (macOS DOES have flock(2), despite the old comment here.)
#[cfg(unix)]
fn try_flock(f: &std::fs::File) -> bool {
    use std::os::unix::io::AsRawFd;
    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;
    unsafe extern "C" {
        fn flock(fd: i32, op: i32) -> i32;
    }
    unsafe { flock(f.as_raw_fd(), LOCK_EX | LOCK_NB) == 0 }
}
#[cfg(not(unix))]
fn try_flock(_f: &std::fs::File) -> bool {
    true // best-effort: single-instance enforcement is unix-only
}

/// Whether a live pulse currently holds the single-instance flock. The
/// authoritative "is the loop actually running" probe (a babysit session can be
/// alive while its inner loop has crashed): open the lock file read-only and try
/// to take the flock; if we CAN, nobody holds it. Exercised by the lock tests.
pub(crate) fn pulse_running(paths: &Paths) -> bool {
    let Ok(f) = std::fs::File::open(paths.lock().join("lock")) else {
        return false;
    };
    !try_flock(&f)
}

/// Holds the lock file open for the pulse's lifetime; the flock is released by the
/// kernel when `_file` is dropped (or the process dies). The lock DIR is removed
/// on a clean exit for tidiness, but correctness no longer depends on that.
struct LockGuard {
    path: PathBuf,
    _file: std::fs::File,
}
impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Acquire the single-instance lock via `flock(2)` on `<data>/.lock/lock`.
/// Returns the guard (lock held for its lifetime) on success, or `None` if a LIVE
/// pulse already holds it. The pulse is the sole beat runner, so holding this for
/// its lifetime guarantees no two beats ever wipe/regenerate the shared
/// snapshots/ dir under each other (H4). A pid file is written alongside purely
/// for human-facing messages (`looop status`, the "already running" notice).
fn acquire_lock(paths: &Paths) -> Option<LockGuard> {
    let dir = paths.lock();
    let _ = fs::create_dir_all(&dir);
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("lock"))
        .ok()?;
    if !try_flock(&file) {
        return None; // a live pulse holds the flock (kernel-managed, no PID guess)
    }
    let _ = fs::write(dir.join("pid"), format!("{}\n", std::process::id()));
    Some(LockGuard {
        path: dir,
        _file: file,
    })
}

pub fn cmd_run(paths: &Paths) -> Result<ExitCode> {
    seed::ensure_dirs(paths)?;
    let cfg = Config::load(paths)?;
    let beat = interval("LOOOP_INTERVAL", &cfg, "interval", 60);

    // Single-instance lock (flock-based; released by the kernel on exit/crash).
    let Some(_guard) = acquire_lock(paths) else {
        let oldpid = fs::read_to_string(paths.lock().join("pid")).unwrap_or_default();
        eprintln!("looop: already running (pid {})", oldpid.trim());
        return Ok(ExitCode::from(1));
    };

    let runner_name = cfg.runner_label();
    util::event(
        Level::Ok,
        "pulse.start",
        &format!("pulse started · deciding every {beat}s · runner {runner_name}"),
        &[
            ("interval", serde_json::json!(beat)),
            ("runner", serde_json::json!(runner_name)),
        ],
    );
    if !paths.default_profile {
        util::event(
            Level::Info,
            "pulse.profile",
            &format!(
                "this profile's sessions live under {d} (LOOOP_DATA_DIR={d} looop ls)",
                d = paths.data_dir.display()
            ),
            &[(
                "data_dir",
                serde_json::json!(paths.data_dir.display().to_string()),
            )],
        );
    }

    // Decide forever. `force` makes a beat re-decide even if the world hash is
    // unchanged. It starts TRUE so the FIRST beat of every pulse process always
    // takes a move: `looop up` should act immediately, not sit idle for a full
    // interval because the world happens to match a `.last-tick-hash` left by a
    // previous run in this data dir. After that it is reset every beat and only
    // re-armed by a `next_interval_s` cadence nudge (a goal scheduling a
    // time-based follow-up). Steady-state beats stay gated by the world hash, so
    // a quiet loop is still nearly free. (Failure backoff still applies on the
    // forced beat, so a crash-restart loop can't burn unbounded AI calls.)
    let mut force = true;
    loop {
        // A due durable nudge (written below, survives crashes) forces this
        // beat to re-decide even over an unchanged world.
        if consume_due_next_wake(paths) {
            force = true;
        }
        let outcome = tick::tick(paths, force);
        force = false;

        // One-shot AI cadence nudge (clamped 5..3600), persisted as a DEADLINE
        // (`.next-wake.json`) rather than carried in memory: a crash during the
        // sleep no longer loses the follow-up, and the consume above re-arms the
        // forced re-decide once it's due.
        let mut want = beat;
        if let Some(req) = outcome.next_interval_s {
            let req = req.clamp(5, 3600);
            util::event(
                Level::Info,
                "cadence",
                &format!("AI cadence override: next beat in {req}s (default {beat}s)"),
                &[
                    ("secs", serde_json::json!(req)),
                    ("default", serde_json::json!(beat)),
                ],
            );
            write_next_wake(paths, util::now_unix() + req);
            want = req;
        }
        // Never sleep PAST a pending deadline (this beat's nudge or a leftover
        // from a previous pulse) — cap the sleep to it.
        if let Some(due) = read_next_wake(paths) {
            let remaining = due.saturating_sub(util::now_unix()).max(1);
            want = want.min(remaining);
        }
        let suffix = if outcome.acted { "acted" } else { "idle" };
        if util::is_json() {
            // JSON watchers can't see the live countdown — keep the structured
            // marker, then sleep plainly.
            util::event(
                Level::Info,
                "sleep",
                &format!("next beat in {want}s ({suffix})"),
                &[
                    ("secs", serde_json::json!(want)),
                    ("acted", serde_json::json!(outcome.acted)),
                ],
            );
            std::thread::sleep(Duration::from_secs(want));
        } else {
            // Human mode: a live countdown that IS the sleep.
            util::sleep_countdown(want, suffix);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn next_wake_is_durable_and_consumed_only_when_due() {
        let p = Paths::temp();
        assert!(!consume_due_next_wake(&p), "no file, nothing due");

        // A future deadline persists across reads and does not consume.
        let future = crate::util::now_unix() + 3600;
        write_next_wake(&p, future);
        assert_eq!(read_next_wake(&p), Some(future));
        assert!(!consume_due_next_wake(&p), "not due yet");
        assert!(p.next_wake().is_file(), "undue nudge survives (durable)");

        // A passed deadline is consumed exactly once and arms the force.
        write_next_wake(&p, crate::util::now_unix() - 1);
        assert!(consume_due_next_wake(&p), "due nudge forces a re-decide");
        assert!(!p.next_wake().is_file(), "consumed");
        assert!(!consume_due_next_wake(&p), "one-shot");
    }

    #[test]
    fn lock_is_exclusive_and_self_heals_after_release() {
        let p = Paths::temp();
        // Nobody holds it yet.
        assert!(!pulse_running(&p), "no pulse before any acquire");

        let g = acquire_lock(&p).expect("first acquire succeeds");
        // A second acquire (separate fd, even same process) is denied by flock.
        assert!(
            acquire_lock(&p).is_none(),
            "second acquire blocked while held"
        );
        // An outside observer (looop status) sees it as running.
        assert!(
            pulse_running(&p),
            "pulse_running true while the lock is held"
        );

        // Releasing the guard releases the flock; the next start re-acquires with
        // no stale-lock reclaim and no PID-liveness guessing.
        drop(g);
        assert!(!pulse_running(&p), "not running once released");
        let g2 = acquire_lock(&p).expect("re-acquire after release");
        drop(g2);
    }

    #[test]
    fn stale_lock_dir_is_not_mistaken_for_a_live_pulse() {
        let p = Paths::temp();
        // Simulate a crashed pulse: the lock dir + files exist, but no flock holder.
        let dir = p.lock();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("lock"), b"").unwrap();
        std::fs::write(dir.join("pid"), b"999999\n").unwrap();

        assert!(
            !pulse_running(&p),
            "a leftover lock dir is not a running pulse"
        );
        // And a fresh start reclaims it cleanly.
        let g = acquire_lock(&p).expect("acquire over a stale lock dir");
        drop(g);
    }
}
