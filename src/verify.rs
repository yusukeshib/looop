//! Worker post-condition verification — the machine check behind "did the
//! worker actually finish?".
//!
//! A worker's exit status is a LIE detector with no battery: an agent that
//! died mid-task (crashed runner, one-shot mode ending its turn, model quota)
//! exits 0 exactly like one that finished. The pulse then trusts the corpse,
//! and the only compensation is prose guards + respawn churn. This module
//! closes that gap with a mechanical contract:
//!
//!   • At spawn time the caller may declare `verify` — ONE shell command that
//!     must exit 0 once the work is truly done ("the PR exists", "the review
//!     thread count is 0", "HEAD moved"). Compose multiple conditions with
//!     `&&`; there is deliberately no DSL.
//!   • The command is persisted to `<data>/verify/<id>.cmd`.
//!   • On the first beat where the worker is DEAD (exited OR killed — self-kill
//!     is the normal completion path), the pulse runs the command once, with a
//!     hard timeout, and records `<data>/verify/<id>.json`.
//!   • The outcome ("pass"/"fail" + output tail) is surfaced through the
//!     sys-sessions snapshot, so a failed post-condition CHANGES THE WORLD and
//!     wakes the decide tick — which sees "exit 0 but POSTCONDITION FAILED"
//!     instead of a clean corpse.
//!
//! Workers without a declared `verify` behave exactly as before.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use crate::paths::Paths;

/// Hard cap on a verify command's runtime (seconds). Verification runs inside
/// the pulse beat, so it must be bounded — override with
/// `LOOOP_VERIFY_TIMEOUT_SECS`.
const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// Keep only this much of the verify command's combined output tail.
const OUTPUT_TAIL_BYTES: usize = 2048;

fn timeout_secs() -> u64 {
    std::env::var("LOOOP_VERIFY_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
}

fn dir(paths: &Paths) -> PathBuf {
    paths.data_dir.join("verify")
}

fn cmd_path(paths: &Paths, id: &str) -> PathBuf {
    dir(paths).join(format!("{id}.cmd"))
}

fn result_path(paths: &Paths, id: &str) -> PathBuf {
    dir(paths).join(format!("{id}.json"))
}

/// The recorded outcome of one verification run.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct VerifyResult {
    pub ok: bool,
    pub exit_code: Option<i32>,
    /// Tail of the command's combined stdout+stderr (diagnostic for the tick).
    pub output: String,
    pub ts: u64,
}

/// Persist a worker's verify command at spawn time (and clear any stale
/// result left by a previous corpse that reused the id).
pub fn store(paths: &Paths, id: &str, cmd: &str) -> anyhow::Result<()> {
    fs::create_dir_all(dir(paths))?;
    fs::write(cmd_path(paths, id), cmd)?;
    let _ = fs::remove_file(result_path(paths, id));
    Ok(())
}

/// Drop a worker's verify state (id reuse / reap).
pub fn clear(paths: &Paths, id: &str) {
    let _ = fs::remove_file(cmd_path(paths, id));
    let _ = fs::remove_file(result_path(paths, id));
}

/// The recorded verification outcome for a worker, if any.
pub fn result(paths: &Paths, id: &str) -> Option<VerifyResult> {
    let raw = fs::read_to_string(result_path(paths, id)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Run every due verification: a DEAD worker with a stored command and no
/// recorded result. Called once per beat, before sensing, so the outcome is
/// visible in the same beat's snapshots. Bounded: each command gets the hard
/// timeout, and each worker is verified at most once per lifetime.
pub fn reconcile(paths: &Paths) {
    let d = dir(paths);
    let Ok(entries) = fs::read_dir(&d) else {
        return;
    };
    let workers = crate::session::list_workers(paths);
    for entry in entries.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("cmd") {
            continue;
        }
        let Some(id) = p.file_stem().and_then(|s| s.to_str()).map(str::to_owned) else {
            continue;
        };
        if result_path(paths, &id).exists() {
            continue; // already verified once
        }
        // Only verify a session we still know about AND that is dead. A
        // vanished session (reaped corpse) is dropped — its verdict would be
        // unattributable anyway.
        match workers.iter().find(|w| w.id == id) {
            Some(w) if !w.alive => {}
            Some(_) => continue, // still running
            None => {
                clear(paths, &id);
                continue;
            }
        }
        let outcome = run_one(paths, &p);
        let json = serde_json::to_string(&outcome).unwrap_or_else(|_| "{}".into());
        let _ = fs::write(result_path(paths, &id), json);
        crate::util::event(
            if outcome.ok {
                crate::util::Level::Info
            } else {
                crate::util::Level::Warn
            },
            "worker.verify",
            &format!(
                "{id}: postcondition {}",
                if outcome.ok { "pass" } else { "FAIL" }
            ),
            &[],
        );
    }
}

fn run_one(paths: &Paths, cmd_file: &std::path::Path) -> VerifyResult {
    let cmd = fs::read_to_string(cmd_file).unwrap_or_default();
    // `timeout` guards the pulse beat; run in the data dir like workers do.
    let out = Command::new("timeout")
        .arg(format!("{}s", timeout_secs()))
        .arg("bash")
        .arg("-c")
        .arg(&cmd)
        .current_dir(&paths.data_dir)
        .output();
    match out {
        Ok(o) => {
            let mut combined = String::from_utf8_lossy(&o.stdout).into_owned();
            combined.push_str(&String::from_utf8_lossy(&o.stderr));
            let tail = if combined.len() > OUTPUT_TAIL_BYTES {
                combined[combined.len() - OUTPUT_TAIL_BYTES..].to_string()
            } else {
                combined
            };
            VerifyResult {
                ok: o.status.success(),
                exit_code: o.status.code(),
                output: tail,
                ts: crate::util::now_unix(),
            }
        }
        Err(e) => VerifyResult {
            ok: false,
            exit_code: None,
            output: format!("verify spawn failed: {e}"),
            ts: crate::util::now_unix(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_result_round_trip() {
        let paths = Paths::temp();
        store(&paths, "w1", "true").unwrap();
        assert!(cmd_path(&paths, "w1").exists());
        assert!(result(&paths, "w1").is_none());
        let r = VerifyResult {
            ok: false,
            exit_code: Some(1),
            output: "no PR".into(),
            ts: 42,
        };
        fs::write(
            result_path(&paths, "w1"),
            serde_json::to_string(&r).unwrap(),
        )
        .unwrap();
        let got = result(&paths, "w1").unwrap();
        assert!(!got.ok);
        assert_eq!(got.exit_code, Some(1));
    }

    #[test]
    fn store_clears_stale_result_on_id_reuse() {
        let paths = Paths::temp();
        store(&paths, "w1", "true").unwrap();
        fs::write(result_path(&paths, "w1"), "{\"ok\":true,\"exit_code\":0,\"output\":\"\",\"ts\":1}").unwrap();
        store(&paths, "w1", "false").unwrap();
        assert!(result(&paths, "w1").is_none());
    }

    #[test]
    fn run_one_captures_failure_and_output() {
        let paths = Paths::temp();
        let f = paths.data_dir.join("verify-test.cmd");
        fs::create_dir_all(&paths.data_dir).unwrap();
        fs::write(&f, "echo missing-artifact >&2; exit 3").unwrap();
        let r = run_one(&paths, &f);
        assert!(!r.ok);
        assert_eq!(r.exit_code, Some(3));
        assert!(r.output.contains("missing-artifact"));
    }

    #[test]
    fn clear_removes_both_files() {
        let paths = Paths::temp();
        store(&paths, "w1", "true").unwrap();
        clear(&paths, "w1");
        assert!(!cmd_path(&paths, "w1").exists());
        assert!(result(&paths, "w1").is_none());
    }
}
