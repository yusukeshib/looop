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
/// `LOOOP_VERIFY_TIMEOUT_SECS` (0 disables the deadline, matching the
/// sensor timeout's semantics — see [`run_cmd`]).
const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// Keep only this much of the verify command's combined output tail.
const OUTPUT_TAIL_BYTES: usize = 2048;

fn timeout_secs() -> u64 {
    crate::util::env_knob("LOOOP_VERIFY_TIMEOUT_SECS").unwrap_or(DEFAULT_TIMEOUT_SECS)
}

/// Total wall-clock budget for ALL verifications in one beat (seconds). When
/// several workers die in the same beat, sequential verifies used to cost up
/// to count×timeout;
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
    // Rename-published: a torn plain write here would later EXECUTE a
    // truncated command (reconcile runs whatever bytes it finds).
    crate::util::write_atomic(&cmd_path(paths, id), cmd.as_bytes())?;
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
        // `None` = the .cmd file vanished between read_dir and the read (a
        // deliberate clear raced us) — nothing to judge, nothing to record.
        let Some((outcome, cmd)) = run_one(paths, &p) else {
            continue;
        };
        let json = serde_json::to_string(&outcome).unwrap_or_else(|_| "{}".into());
        // Rename-published: a torn result would be unparseable, look like "no
        // result yet", and re-run the (once-only) verify command next beat.
        let _ = crate::util::write_atomic(&result_path(paths, &id), json.as_bytes());
        // The event names WHAT ran, not just pass/fail: `verify` is AI-authored
        // shell executed automatically (constitution rule 2 constrains it to
        // read-only checks), so a human watching the pulse must be able to
        // audit the actual command text. One-lined + clipped so a runaway
        // command can't flood the event stream.
        let cmd_shown: String = cmd.trim().replace('\n', " ").chars().take(256).collect();
        crate::util::event(
            if outcome.ok {
                crate::util::Level::Info
            } else {
                crate::util::Level::Warn
            },
            "worker.verify",
            &format!(
                "{id}: postcondition {} — $ {cmd_shown}",
                if outcome.ok { "pass" } else { "FAIL" }
            ),
            &[("cmd", serde_json::json!(cmd_shown))],
        );
    }
}

/// Run one due verification from its persisted `.cmd` file. Returns the
/// verdict PLUS the command text that was judged (a placeholder when the file
/// was unreadable), so the caller's `worker.verify` event can show a human
/// WHAT ran — not just pass/fail.
///
/// The read itself can fail, and neither failure may ever degrade into
/// `bash -c ''` (exit 0) — that would record a FABRICATED pass, the exact lie
/// this module exists to catch. Two distinct failure shapes:
///
///   • NotFound — the file vanished between reconcile()'s read_dir and this
///     read: a TOCTOU with a deliberate [`clear`] (`looop kill`, reap). The
///     worker's verify state was intentionally dropped, so recording ANY
///     verdict here would resurrect state for a cleared worker. Skip (`None`)
///     — the next beat's read_dir simply won't see the file.
///   • Any other error (permissions, I/O) — the command is unknowable but the
///     obligation stands, and skipping would retry (and warn) forever on a
///     persistent error. Record a FAIL naming the unreadable file: the tick
///     sees "postcondition FAIL" instead of a clean corpse, and the
///     once-per-lifetime contract still holds.
fn run_one(paths: &Paths, cmd_file: &std::path::Path) -> Option<(VerifyResult, String)> {
    let cmd = match fs::read_to_string(cmd_file) {
        Ok(cmd) => cmd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            return Some((
                VerifyResult {
                    ok: false,
                    exit_code: None,
                    output: format!(
                        "verify command file {} is unreadable ({e}) — the postcondition could \
                         not be checked, recorded as FAIL (never a fabricated pass)",
                        cmd_file.display()
                    ),
                    ts: crate::util::now_unix(),
                },
                "(unreadable)".to_string(),
            ));
        }
    };
    let outcome = run_cmd(
        &paths.data_dir,
        &cmd,
        timeout_secs(),
        "LOOOP_VERIFY_TIMEOUT_SECS",
    );
    Some((outcome, cmd))
}

/// SIGKILL a child's whole process GROUP (the child was spawned with
/// `process_group(0)`, so its pid IS the pgid), then reap it. Killing only the
/// `bash` leader would orphan grandchildren (`bash -c 'slow | slower'`), which
/// keep the beat's resources busy past the deadline. libc-free: raw kill(2)
/// via the same extern-"C" technique the flock helper uses.
fn kill_group(child: &mut std::process::Child) {
    crate::util::kill_process_group(child.id());
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
/// `timeout_env` names the knob in the timeout message. `timeout == 0`
/// DISABLES the deadline (same semantics as LOOOP_SENSOR_TIMEOUT — it used to
/// mean "kill immediately" here, silently failing every verify), and an
/// absurd value that would overflow `Instant + Duration` also means "no
/// deadline" instead of panicking. Shared with the executor's run_shell path
/// (same bounded-shell semantics).
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
    // Opened with create_new (O_EXCL): the shared temp_dir + a predictable
    // name means a plain File::create would FOLLOW a pre-planted symlink and
    // overwrite whatever it points at (any file the looop user can write) —
    // O_EXCL refuses both the symlink and any pre-existing file. On collision
    // retry with a clock-derived randomized suffix; give up after a few tries
    // rather than loop forever against an adversary squatting the namespace.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let base = format!(
        "looop-verify-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let mut out_path = std::env::temp_dir().join(format!("{base}.out"));
    let out_file = 'open: {
        for _ in 0..8 {
            match fs::File::options()
                .write(true)
                .create_new(true)
                .open(&out_path)
            {
                Ok(f) => break 'open f,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let nonce = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| d.subsec_nanos());
                    out_path = std::env::temp_dir()
                        .join(format!("{base}-{nonce}-{}.out", crate::util::temp_nonce()));
                }
                Err(_) => {
                    return fail("verify: cannot create output capture file".into());
                }
            }
        }
        return fail("verify: cannot create output capture file (name collisions)".into());
    };
    let Ok(err_file) = out_file.try_clone() else {
        let _ = fs::remove_file(&out_path);
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

    // checked_add: an absurd knob value (u64::MAX) would overflow
    // Instant + Duration and panic — overflow means "no deadline", exactly
    // like sensor.rs. `timeout == 0` also disables the deadline.
    let deadline = if timeout == 0 {
        None
    } else {
        std::time::Instant::now().checked_add(std::time::Duration::from_secs(timeout))
    };
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if let Some(d) = deadline
                    && std::time::Instant::now() >= d
                {
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

/// The UTF-8-safe tail of a file, capped at `max` bytes — SEEK-based, so only
/// the tail is ever loaded (a whole-file read would let an unbounded capture
/// file balloon the pulse's memory). pub(crate): sensor.rs's stderr-tail read
/// delegates here for exactly that bound (its `.err` files have no size cap).
pub(crate) fn read_tail_file(path: &std::path::Path, max: usize) -> String {
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

    /// Fabricate a babysit session on disk in `state` — the same file layout
    /// the library reads back (meta.json + status.json) — so reconcile's
    /// alive/dead judgment can be exercised without spawning real processes.
    fn fake_worker(paths: &Paths, id: &str, state: &str) {
        let dir = paths.data_dir.join("sessions").join(id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("meta.json"),
            format!(
                r#"{{"id":"{id}","cmd":["true"],"babysit_pid":4194000,"started_at":"2020-01-01T00:00:00Z"}}"#
            ),
        )
        .unwrap();
        fs::write(
            dir.join("status.json"),
            format!(
                r#"{{"state":"{state}","child_pid":null,"exit_code":0,"last_change":"2020-01-01T00:00:01Z"}}"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn reconcile_judges_a_dead_worker_once() {
        let paths = Paths::temp();
        fake_worker(&paths, "w-dead", "exited");
        store(&paths, "w-dead", "echo verified-ok").unwrap();
        reconcile(&paths);
        let r = result(&paths, "w-dead").expect("a dead worker with a stored command is verified");
        assert!(r.ok, "the passing postcondition records ok");
        assert!(
            r.output.contains("verified-ok"),
            "the command's output is captured for diagnosis: {}",
            r.output
        );
        // Once per lifetime: a second beat must not re-run the command.
        let first_ts_file = fs::read_to_string(result_path(&paths, "w-dead")).unwrap();
        reconcile(&paths);
        assert_eq!(
            fs::read_to_string(result_path(&paths, "w-dead")).unwrap(),
            first_ts_file,
            "an already-recorded verdict is never re-judged"
        );
    }

    #[test]
    fn reconcile_skips_a_live_worker() {
        let paths = Paths::temp();
        // `running` state + a dead babysit pid reads as NOT alive — use our own
        // live pid so is_owner_alive holds and the worker counts as running.
        let dir = paths.data_dir.join("sessions").join("w-live");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("meta.json"),
            format!(
                r#"{{"id":"w-live","cmd":["true"],"babysit_pid":{},"started_at":"2020-01-01T00:00:00Z"}}"#,
                std::process::id()
            ),
        )
        .unwrap();
        fs::write(
            dir.join("status.json"),
            r#"{"state":"running","child_pid":null,"exit_code":null,"last_change":"2020-01-01T00:00:01Z"}"#,
        )
        .unwrap();
        store(&paths, "w-live", "echo too-early").unwrap();
        reconcile(&paths);
        assert!(
            result(&paths, "w-live").is_none(),
            "a still-running worker is never verified early"
        );
        assert!(
            cmd_path(&paths, "w-live").exists(),
            "its stored command is kept for the eventual death"
        );
    }

    #[test]
    fn reconcile_defers_when_the_beat_budget_is_exhausted() {
        // Serialize with other env-mutating tests; restore even on panic.
        let _env = crate::util::test_env_lock();
        struct Restore;
        impl Drop for Restore {
            fn drop(&mut self) {
                unsafe { std::env::remove_var("LOOOP_VERIFY_BEAT_BUDGET_SECS") };
            }
        }
        let _restore = Restore;
        let paths = Paths::temp();
        fake_worker(&paths, "w-deferred", "exited");
        store(&paths, "w-deferred", "echo should-not-run-yet").unwrap();
        // Budget 0: already exhausted before the first verify would run.
        unsafe { std::env::set_var("LOOOP_VERIFY_BEAT_BUDGET_SECS", "0") };
        reconcile(&paths);
        assert!(
            result(&paths, "w-deferred").is_none(),
            "a budget-deferred verify records no verdict"
        );
        assert!(
            cmd_path(&paths, "w-deferred").exists(),
            "deferred state is untouched so the next beat retries"
        );
        // With the budget restored, the next beat picks it up.
        unsafe { std::env::remove_var("LOOOP_VERIFY_BEAT_BUDGET_SECS") };
        reconcile(&paths);
        assert!(
            result(&paths, "w-deferred").is_some(),
            "the deferred verify runs on the next beat"
        );
    }

    #[test]
    fn reconcile_clears_a_vanished_sessions_state() {
        let paths = Paths::temp();
        // A stored command whose session was reaped (no sessions/<id>/ at all):
        // its verdict would be unattributable, so the state is dropped.
        store(&paths, "w-ghost", "echo unattributable").unwrap();
        reconcile(&paths);
        assert!(
            !cmd_path(&paths, "w-ghost").exists(),
            "a vanished session's verify command is cleared"
        );
        assert!(
            result(&paths, "w-ghost").is_none(),
            "no verdict is fabricated for a vanished session"
        );
    }

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
        let (r, cmd) = run_one(&paths, &f).expect("a readable command file is judged");
        assert!(!r.ok);
        assert_eq!(r.exit_code, Some(3));
        assert!(r.output.contains("missing-artifact"));
        // The command text rides along so the worker.verify event can show a
        // human WHAT ran — pass/fail alone hid the AI-authored shell entirely.
        assert_eq!(cmd, "echo missing-artifact >&2; exit 3");
    }

    #[test]
    fn run_one_vanished_cmd_file_skips_without_a_verdict() {
        // TOCTOU with `looop kill` → clear(): the .cmd file listed by
        // read_dir is gone by the time run_one reads it. The old
        // `unwrap_or_default()` turned this into `bash -c ''` → exit 0 → a
        // fabricated PASS; now it must record NOTHING.
        let paths = Paths::temp();
        let gone = dir(&paths).join("vanished.cmd");
        assert!(
            run_one(&paths, &gone).is_none(),
            "a vanished command file is skipped, not judged"
        );
        assert!(
            result(&paths, "vanished").is_none(),
            "no verdict is recorded for a cleared worker"
        );
    }

    #[test]
    fn run_one_unreadable_cmd_file_records_fail_never_pass() {
        // A persistent read error (here: the path is a DIRECTORY, so
        // read_to_string fails with a non-NotFound error) must record a FAIL
        // naming the file — never the old empty-command fabricated pass.
        let paths = Paths::temp();
        let d = paths.data_dir.join("unreadable.cmd");
        fs::create_dir_all(&d).unwrap();
        let (r, cmd) = run_one(&paths, &d).expect("a persistent read error is judged terminally");
        assert!(!r.ok, "an unreadable command file can never verify as pass");
        assert_eq!(
            cmd, "(unreadable)",
            "the event still gets a placeholder command, never fabricated text"
        );
        assert_eq!(
            r.exit_code, None,
            "no command ran, so there is no exit code"
        );
        assert!(
            r.output.contains("unreadable") && r.output.contains("unreadable.cmd"),
            "the FAIL names the unreadable file: {}",
            r.output
        );
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
    fn run_cmd_timeout_zero_disables_the_deadline() {
        // 0 used to mean "kill immediately" here while the sensor timeout
        // treats 0 as "no deadline" — the semantics are now aligned: the
        // command must run to completion.
        let paths = Paths::temp();
        let r = run_cmd(&paths.data_dir, "echo done", 0, "LOOOP_VERIFY_TIMEOUT_SECS");
        assert!(r.ok, "timeout 0 must not kill the command: {:?}", r.output);
        assert_eq!(r.exit_code, Some(0));
        assert!(r.output.contains("done"));
    }

    #[test]
    fn run_cmd_survives_an_absurd_timeout_without_panicking() {
        // u64::MAX seconds overflows Instant + Duration — checked_add must
        // turn that into "no deadline", not a panic mid-beat.
        let paths = Paths::temp();
        let r = run_cmd(
            &paths.data_dir,
            "true",
            u64::MAX,
            "LOOOP_VERIFY_TIMEOUT_SECS",
        );
        assert!(r.ok);
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
