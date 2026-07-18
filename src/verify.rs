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
use std::io::{Read, Seek, SeekFrom};
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
    crate::util::env_knob("LOOOP_VERIFY_TIMEOUT_SECS").unwrap_or(DEFAULT_TIMEOUT_SECS)
}

/// Total wall-clock budget for ALL verifications in one beat (seconds). N
/// workers dying in the same beat used to cost up to N×timeout sequentially;
/// once the budget is spent the remaining verifies are DEFERRED (their state
/// untouched, so the next beat picks them up). `LOOOP_VERIFY_BEAT_BUDGET_SECS`,
/// default 120.
fn beat_budget_secs() -> u64 {
    crate::util::env_knob("LOOOP_VERIFY_BEAT_BUDGET_SECS").unwrap_or(120)
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
    let started = std::time::Instant::now();
    let budget = std::time::Duration::from_secs(beat_budget_secs());
    let mut budget_warned = false;
    for entry in entries.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("cmd") {
            continue;
        }
        let Some(id) = p.file_stem().and_then(|s| s.to_str()).map(str::to_owned) else {
            continue;
        };
        if result(paths, &id).is_some() {
            continue; // already verified once
        }
        // Only verify a session we still know about AND that is dead. A
        // vanished session (reaped corpse) is dropped — its verdict would be
        // unattributable anyway. This bookkeeping is free, so it runs even
        // when the budget below is spent.
        match workers.iter().find(|w| w.id == id) {
            Some(w) if !w.alive => {}
            Some(_) => continue, // still running
            None => {
                clear(paths, &id);
                continue;
            }
        }
        // Per-beat budget: N dead workers must not cost N×timeout in one beat.
        // Checked just before actually RUNNING a verify command — the free
        // bookkeeping above always runs, and the deferral warning only ever
        // names a worker that WOULD have been verified. Deferred state is
        // untouched, so the next beat retries.
        if started.elapsed() >= budget {
            if !budget_warned {
                budget_warned = true;
                crate::util::event(
                    crate::util::Level::Warn,
                    "worker.verify_deferred",
                    &format!(
                        "per-beat verify budget exhausted (LOOOP_VERIFY_BEAT_BUDGET_SECS={}s) — \
                         deferring {id} (and any remaining) to the next beat",
                        beat_budget_secs()
                    ),
                    &[("worker", serde_json::json!(id))],
                );
            }
            continue;
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
    run_cmd(
        &paths.data_dir,
        &cmd,
        timeout_secs(),
        "LOOOP_VERIFY_TIMEOUT_SECS",
    )
}

/// SIGKILL a child's whole process GROUP (the child was spawned with
/// `process_group(0)`, so its pid IS the pgid), then reap it. Killing only the
/// `bash` leader would orphan grandchildren (`bash -c 'slow | slower'`), which
/// keep the beat's resources busy past the deadline. libc-free: raw kill(2)
/// via the same extern-"C" technique the flock helper uses.
fn kill_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        const SIGKILL: i32 = 9;
        unsafe {
            let _ = kill(-(child.id() as i32), SIGKILL);
        }
    }
    let _ = child.kill(); // belt-and-braces (and the non-unix path)
    let _ = child.wait(); // reap; kill is racy but wait is not
}

/// Run `cmd` under `bash -c` in `cwd` with a NATIVE timeout. No `timeout(1)`
/// dependency: stock macOS ships without it (coreutils-only), which used to
/// make every declared verify fail with "spawn failed" — the exact silent
/// half-wiring the deps preflight exists to prevent. Output goes to a temp
/// file (not a pipe) so a chatty command can never dead-lock the beat on a
/// full pipe buffer; we poll `try_wait` and kill the whole process GROUP on
/// deadline (the child is its own group leader via `process_group(0)`).
/// `timeout_env` names the knob in the timeout message. Shared with the
/// executor's run_shell path (same bounded-shell semantics).
pub(crate) fn run_cmd(
    cwd: &std::path::Path,
    cmd: &str,
    timeout: u64,
    timeout_env: &str,
) -> VerifyResult {
    let now = || crate::util::now_unix();
    let fail = |output: String| VerifyResult {
        ok: false,
        exit_code: None,
        output,
        ts: now(),
    };

    // stdout+stderr merged into one temp file, in order. The name carries a
    // process-wide counter, not just a timestamp — two runs in the same second
    // (parallel tests, back-to-back verifies) must never share a capture file.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let out_path = std::env::temp_dir().join(format!(
        "looop-verify-{}-{}.out",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let Ok(out_file) = fs::File::create(&out_path) else {
        return fail("verify: cannot create output capture file".into());
    };
    let Ok(err_file) = out_file.try_clone() else {
        return fail("verify: cannot clone output capture file".into());
    };

    let mut command = Command::new("bash");
    command
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(out_file)
        .stderr(err_file);
    // Make the child its own process-group leader so a deadline kill can take
    // out the WHOLE pipeline (grandchildren included), not just bash.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = fs::remove_file(&out_path);
            return fail(format!("verify spawn failed: {e}"));
        }
    };

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    kill_group(&mut child);
                    break None;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(_) => {
                // A failed status probe leaves the child state unknown. Kill and
                // reap defensively so a verifier never escapes this deadline loop.
                kill_group(&mut child);
                break None;
            }
        }
    };

    let mut tail = read_tail_file(&out_path, OUTPUT_TAIL_BYTES);
    let _ = fs::remove_file(&out_path);
    match status {
        Some(s) => VerifyResult {
            ok: s.success(),
            exit_code: s.code(),
            output: tail,
            ts: now(),
        },
        None => {
            if !tail.is_empty() && !tail.ends_with('\n') {
                tail.push('\n');
            }
            tail.push_str(&format!(
                "timed out after {timeout}s ({timeout_env}) — process group killed"
            ));
            fail(tail)
        }
    }
}

/// The UTF-8-safe tail of a file, capped at `max` bytes.
fn read_tail_file(path: &std::path::Path, max: usize) -> String {
    let Ok(mut file) = fs::File::open(path) else {
        return String::new();
    };
    let Ok(len) = file.metadata().map(|meta| meta.len()) else {
        return String::new();
    };
    let take = len.min(max as u64) as usize;
    if file.seek(SeekFrom::End(-(take as i64))).is_err() {
        return String::new();
    }
    let mut tail = Vec::with_capacity(take);
    if file.read_to_end(&mut tail).is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&tail).into_owned()
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
        fs::write(
            result_path(&paths, "w1"),
            "{\"ok\":true,\"exit_code\":0,\"output\":\"\",\"ts\":1}",
        )
        .unwrap();
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
    fn run_cmd_kills_on_deadline_without_external_timeout_binary() {
        // The native timeout must work where coreutils `timeout` is absent
        // (stock macOS). A 1s deadline on a 30s sleep must come back promptly,
        // marked failed, with the timeout named in the output.
        let paths = Paths::temp();
        let t0 = std::time::Instant::now();
        let r = run_cmd(
            &paths.data_dir,
            "echo before; sleep 30",
            1,
            "LOOOP_VERIFY_TIMEOUT_SECS",
        );
        assert!(t0.elapsed().as_secs() < 10, "must not wait out the sleep");
        assert!(!r.ok);
        assert_eq!(r.exit_code, None, "a killed command has no exit code");
        assert!(r.output.contains("before"), "pre-kill output is kept");
        assert!(r.output.contains("timed out after 1s"));
    }

    #[test]
    fn run_cmd_merges_stdout_and_stderr_and_succeeds() {
        let paths = Paths::temp();
        let r = run_cmd(
            &paths.data_dir,
            "echo out; echo err >&2",
            10,
            "LOOOP_VERIFY_TIMEOUT_SECS",
        );
        assert!(r.ok);
        assert_eq!(r.exit_code, Some(0));
        assert!(r.output.contains("out") && r.output.contains("err"));
    }

    #[test]
    fn read_tail_file_is_bounded_and_lossy() {
        let paths = Paths::temp();
        fs::create_dir_all(&paths.data_dir).unwrap();
        let path = paths.data_dir.join("output");
        fs::write(&path, b"prefix\xfftail").unwrap();
        let tail = read_tail_file(&path, 5);
        assert_eq!(tail, "\u{fffd}tail");
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
