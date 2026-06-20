//! The pulse (`looop`) — the control loop itself.
//!
//! The pulse is a single-instance, level-triggered reconcile loop: tick, choose
//! the next cadence, sleep, repeat. It is the SOLE writer of the policy files
//! (PLAYBOOK/goals/sensors) and the journal — there is no imperative override
//! that races it; humans steer it by editing the desired state it reconciles.

use crate::config::Config;
use crate::paths::Paths;
use crate::util::Level;
use crate::{seed, session, tick, util};
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

/// Whether a live pulse currently holds the single-instance lock. Used by
/// `looop status` from a SEPARATE process: open the lock file read-only and try
/// to take the flock; if we CAN take it, nobody holds it (we release it again on
/// drop) — so the pulse is NOT running. A crashed pulse that left the lock file
/// behind therefore reads as "not running", with no PID-reuse false positive.
pub(crate) fn pulse_running(paths: &Paths) -> bool {
    let Ok(f) = std::fs::File::open(paths.lock().join("lock")) else {
        return false; // never started, or already cleaned up
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
    let idle = interval("LOOOP_INTERVAL", &cfg, "interval", 60);
    let busy = interval("LOOOP_BUSY_INTERVAL", &cfg, "busy_interval", idle);
    let active = interval("LOOOP_ACTIVE_INTERVAL", &cfg, "active_interval", idle);

    // Single-instance lock (flock-based; released by the kernel on exit/crash).
    let Some(_guard) = acquire_lock(paths) else {
        let oldpid = fs::read_to_string(paths.lock().join("pid")).unwrap_or_default();
        eprintln!("looop: already running (pid {})", oldpid.trim());
        return Ok(ExitCode::from(1));
    };

    let runner_name = cfg.default_runner().unwrap_or_else(|| "?".into());
    util::event(
        Level::Ok,
        "pulse.start",
        &format!("pulse started · idle {idle}s / busy {busy}s · runner {runner_name}"),
        &[
            ("idle", serde_json::json!(idle)),
            ("busy", serde_json::json!(busy)),
            ("active", serde_json::json!(active)),
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

    // When the previous beat emitted a `next_interval_s` nudge, the next beat is
    // a FORCED re-decide: it bypasses the unchanged-world skip once, so a goal
    // can schedule a time-based follow-up instead of going silent until the world
    // changes on its own. Reset every beat; only a fresh override re-arms it.
    let mut force = false;
    loop {
        let outcome = tick::tick(paths, force);
        force = false;

        // Pick the base cadence ONCE: a beat that moved is "busy"; otherwise a
        // live worker keeps us "active"; an idle world waits the longest. Both
        // the interval and its label come from this single classification, so
        // any_worker_alive() is probed at most once per beat (not twice).
        let (mut want, mut reason) = if outcome.acted {
            (busy, "busy")
        } else if session::any_worker_alive(paths) {
            (active, "active")
        } else {
            (idle, "idle")
        };

        // One-shot AI cadence nudge, handed straight back from the beat
        // in-memory (no `.next-interval` file). Clamped 5..3600.
        if let Some(req) = outcome.next_interval_s {
            let req = req.clamp(5, 3600);
            util::event(
                Level::Info,
                "cadence",
                &format!("AI cadence override: next beat in {req}s (default {want}s)"),
                &[
                    ("secs", serde_json::json!(req)),
                    ("default", serde_json::json!(want)),
                ],
            );
            want = req;
            reason = "override";
            // The nudge also forces the next beat to re-decide even if the world
            // is unchanged (time-based follow-up, not just a sleep nudge).
            force = true;
        }
        util::event(
            Level::Info,
            "sleep",
            &format!("next beat in {want}s ({reason})"),
            &[
                ("secs", serde_json::json!(want)),
                ("reason", serde_json::json!(reason)),
            ],
        );
        std::thread::sleep(Duration::from_secs(want));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

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
