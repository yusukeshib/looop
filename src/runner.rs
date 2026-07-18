//! ACT — run the configured tick runner once, teeing its output to the per-beat
//! archive (runs/<id>/output.log) and tick.log so a beat is replayable. The
//! pulse keeps its own stream a clean structured-event log: the runner's
//! free-form chatter is archived to the tee files but never echoed live
//! (replay it from runs/<id>/output.log).
//!
//! NOTE: each tee sink is opened with `File::create`, which TRUNCATES it —
//! deliberately so for `tick.log`: it holds ONLY the LAST beat's rendered
//! output (a cheap "what just happened" probe). The durable per-beat history
//! lives in `runs/<id>/output.log` (one file per beat, pruned by
//! `LOOOP_RUNS_KEEP`).
//!
//! Formatting happens IN-PROCESS here: we read the runner's raw NDJSON stdout
//! line-by-line and render each line via `fmt::format_line` (the friendly
//! `→ bash:` progress). There is no external formatter and looop never re-execs
//! itself to post-process its own child — the old `| "$LOOOP_BIN" _ fmt` pipe
//! seam is gone.

use crate::fmt;
use crate::paths::Paths;
use crate::util;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Single-quote `s` for safe interpolation into a `bash -c` script: close the
/// quote, emit an escaped quote, reopen (`'\''` — the classic POSIX dance).
/// Without this a prompt-file path containing spaces/quotes/`$` would be
/// word-split or expanded by the shell.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Hard deadline for ONE tick runner invocation (seconds): the decide is an
/// external LLM call, and a hung runner would otherwise stall the whole
/// single-instance pulse silently forever (no other beat can run while this
/// one never returns). `LOOOP_TICK_TIMEOUT_SECS`; 0 disables; default 30min —
/// generous for a slow model, tiny next to "forever".
fn tick_timeout_secs() -> u64 {
    crate::util::env_knob("LOOOP_TICK_TIMEOUT_SECS").unwrap_or(1800)
}

/// SIGKILL a whole process GROUP by pgid (the runner is spawned with
/// `process_group(0)`, so its pid IS the pgid). Killing only the `bash`
/// leader would orphan grandchildren (the actual LLM CLI), which keep the
/// beat's resources busy past the deadline. Modeled on `verify::kill_group`;
/// libc-free: raw kill(2) via the same extern-"C" technique.
#[cfg(unix)]
fn kill_pgid(pgid: i32) {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    const SIGKILL: i32 = 9;
    unsafe {
        let _ = kill(-pgid, SIGKILL);
    }
}

/// Substitute `{{prompt_file}}` into a command template with shell quoting,
/// WITHOUT double-quoting configs that already wrapped the placeholder in
/// quotes (`"{{prompt_file}}"` / `'{{prompt_file}}'` worked before quoting was
/// added, and nesting quotes would hand the shell a literal-quote path).
/// Shared by the tick path (below) and the worker path (session.rs) so both
/// sides of the config behave identically.
pub(crate) fn substitute_prompt_file(template: &str, path: &str) -> String {
    let quoted = shell_quote(path);
    template
        .replace("\"{{prompt_file}}\"", &quoted)
        .replace("'{{prompt_file}}'", &quoted)
        .replace("{{prompt_file}}", &quoted)
}

/// Run `tick_cmd` (a shell pipeline) under `bash -c`, with cwd at the data dir.
/// The tick prompt reaches the runner one of two ways, mirroring the worker:
/// if `tick_cmd` contains the `{{prompt_file}}` placeholder it is substituted
/// with the prompt file's path (so the config can read it via `$(cat …)` /
/// `@file`, symmetric with `worker_command`); otherwise the file is piped in as
/// stdin (the original, zero-config path). stdout+stderr are merged; each line
/// is rendered via `fmt::format_line`, stamped, and written to every `tee` file
/// (the replay archive). `Ok(())` when the runner exited successfully; `Err`
/// carries the CAUSE (unreadable prompt, spawn failure, deadline kill, nonzero
/// exit) so the caller can record it into the failure feedback instead of
/// flying blind on a bare `false`.
///
/// Bounded: the runner is spawned in its own process GROUP and the whole group
/// is SIGKILLed once `LOOOP_TICK_TIMEOUT_SECS` elapses (a watchdog thread —
/// this thread is blocked streaming the runner's output, so it cannot poll).
/// Killing the group closes the pipe, the streaming loop sees EOF, and the
/// beat fails like any other runner crash (backoff arms, the failure is
/// recorded) instead of stalling the single-instance pulse forever.
pub fn run_streamed(
    paths: &Paths,
    tick_cmd: &str,
    prompt_file: &Path,
    tee: &[PathBuf],
) -> Result<(), String> {
    // When the operator references the prompt explicitly via `{{prompt_file}}`
    // (the same placeholder `worker_command` uses), substitute the path and
    // leave stdin alone. Otherwise fall back to feeding the file via stdin.
    let has_placeholder = tick_cmd.contains("{{prompt_file}}");
    let tick_cmd = substitute_prompt_file(tick_cmd, &prompt_file.to_string_lossy());

    // `{ …; } 2>&1` merges the whole pipeline's stderr into stdout in order, so
    // a single pipe carries everything (Rust can't easily interleave two pipes).
    let script = format!("{{ {tick_cmd} ; }} 2>&1");

    let mut cmd = Command::new("bash");
    // `-c`, not `-lc`: a non-login shell sources no rc files, so the runner
    // pipeline runs against looop's inherited environment instead of re-running
    // the operator's login profile on every beat (hermetic + cheaper).
    cmd.arg("-c")
        .arg(&script)
        .current_dir(&paths.data_dir)
        .stdout(Stdio::piped());

    if !has_placeholder {
        let stdin = File::open(prompt_file).map_err(|e| {
            format!(
                "cannot open the tick prompt {}: {e}",
                prompt_file.display()
            )
        })?;
        cmd.stdin(Stdio::from(stdin));
    }

    // Make the runner its own process-group leader so a deadline kill can take
    // out the WHOLE pipeline (grandchildren included), not just bash — the
    // same discipline as verify::run_cmd.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn the tick runner: {e}"))?;
    let Some(out) = child.stdout.take() else {
        // Never leak a zombie: kill + reap before reporting the (should-be
        // impossible — stdout was piped) missing pipe.
        let _ = child.kill();
        let _ = child.wait();
        return Err("no stdout pipe from the tick runner".into());
    };

    // Deadline watchdog: this thread is about to block streaming the runner's
    // output, so a separate thread owns the timeout. It sleeps on a channel;
    // a send (after wait() below) cancels it, and only an actual TIMEOUT (not
    // a disconnect) kills the group. Returns whether it fired.
    let timeout = tick_timeout_secs();
    let (cancel_tx, cancel_rx) = std::sync::mpsc::channel::<()>();
    let pgid = child.id() as i32;
    let watchdog = (timeout > 0).then(|| {
        std::thread::spawn(move || {
            match cancel_rx.recv_timeout(std::time::Duration::from_secs(timeout)) {
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    #[cfg(unix)]
                    kill_pgid(pgid);
                    #[cfg(not(unix))]
                    let _ = pgid;
                    true
                }
                _ => false, // cancelled: the runner finished in time
            }
        })
    });

    // File::create truncates — intentional: tick.log carries the LAST beat
    // only (see the module comment); runs/<id>/output.log is per-beat anyway.
    let mut sinks: Vec<File> = tee.iter().filter_map(|p| File::create(p).ok()).collect();

    let mut reader = BufReader::new(out);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break, // EOF
            Ok(_) => {
                let line = line.trim_end_matches(['\n', '\r']);
                // Archive only the rendered progress (what the old `_ fmt` pipe wrote).
                if let Some(rendered) = fmt::format_line(line) {
                    let prefix = format!("{}[{}]{} ", util::dim(), util::hms(), util::rst());
                    for f in &mut sinks {
                        let _ = writeln!(f, "{prefix}{rendered}");
                    }
                }
            }
            Err(_) => break,
        }
    }
    // Drop our end of the pipe BEFORE waiting: after a read error above the
    // child may still be writing, and wait()-ing while holding the read end
    // would deadlock both sides on a full pipe buffer.
    drop(reader);

    let status = child.wait();
    // Cancel the watchdog (harmless if it already fired) and learn whether the
    // deadline was the reason the stream ended — a group-killed runner exits
    // nonzero anyway, but the timeout message names the ACTUAL cause.
    let _ = cancel_tx.send(());
    let timed_out = watchdog.is_some_and(|h| h.join().unwrap_or(false));
    if timed_out {
        return Err(format!(
            "timed out after {timeout}s (LOOOP_TICK_TIMEOUT_SECS) — process group killed"
        ));
    }
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("the tick runner exited nonzero ({s})")),
        Err(e) => Err(format!("failed to reap the tick runner: {e}")),
    }
}
