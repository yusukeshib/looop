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

/// Removes the lock dir on drop (covers normal exit / panics; a hard Ctrl-C
/// relies on the stale-lock reclaim in the next start, which checks pid liveness).
struct LockGuard {
    path: PathBuf,
}
impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Acquire the single-instance lock (atomic mkdir + PID file). Returns the guard
/// (released on drop) on success, or `None` if a LIVE pulse already holds it. A
/// stale lock left by a dead process is reclaimed (mkdir is the atomic arbiter of
/// the reclaim race), so a crashed pulse never wedges the next start. The pulse
/// is the sole beat runner, so holding this for its lifetime guarantees no two
/// beats ever wipe/regenerate the shared snapshots/ dir under each other (H4).
fn acquire_lock(paths: &Paths) -> Option<LockGuard> {
    let lock = paths.lock();
    if fs::create_dir(&lock).is_err() {
        let oldpid = fs::read_to_string(lock.join("pid")).unwrap_or_default();
        if util::pid_alive(oldpid.trim()) {
            return None;
        }
        // Stale lock: reclaim it (mkdir is the atomic arbiter on the race).
        let _ = fs::remove_dir_all(&lock);
        if fs::create_dir(&lock).is_err() {
            return None;
        }
    }
    let _ = fs::write(lock.join("pid"), format!("{}\n", std::process::id()));
    Some(LockGuard { path: lock })
}

pub fn cmd_run(paths: &Paths) -> Result<ExitCode> {
    seed::ensure_dirs(paths)?;
    let cfg = Config::load(paths)?;
    let idle = interval("LOOOP_INTERVAL", &cfg, "interval", 60);
    let busy = interval("LOOOP_BUSY_INTERVAL", &cfg, "busy_interval", idle);
    let active = interval("LOOOP_ACTIVE_INTERVAL", &cfg, "active_interval", idle);

    // Single-instance lock (mkdir-based; macOS has no flock).
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

    loop {
        let acted = tick::tick(paths);

        // Pick the base cadence ONCE: a beat that moved is "busy"; otherwise a
        // live worker keeps us "active"; an idle world waits the longest. Both
        // the interval and its label come from this single classification, so
        // any_worker_alive() is probed at most once per beat (not twice).
        let (mut want, mut reason) = if acted {
            (busy, "busy")
        } else if session::any_worker_alive(paths) {
            (active, "active")
        } else {
            (idle, "idle")
        };

        // One-shot AI cadence override via .next-interval (clamped 5..3600).
        let reqf = paths.data_dir.join(".next-interval");
        if let Ok(raw) = fs::read_to_string(&reqf) {
            let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
            let _ = fs::remove_file(&reqf);
            if let Ok(mut req) = digits.parse::<u64>() {
                req = req.clamp(5, 3600);
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
            }
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
