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
use crate::util::{self, Level};
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

// Single-quoting for `bash -c` interpolation lives in `util::shell_quote`
// (the ONE shared implementation — see its doc).
use crate::util::shell_quote;

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
        let stdin = File::open(prompt_file)
            .map_err(|e| format!("cannot open the tick prompt {}: {e}", prompt_file.display()))?;
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
    // output, so a separate thread owns the timeout WHILE WE STREAM. It sleeps
    // on a channel; a send (after the stream ends below, BEFORE the child is
    // reaped) cancels it, and only an actual TIMEOUT (not a disconnect) kills
    // the group. Returns whether it fired. Once the stream ends, THIS thread
    // takes the deadline back (see the reap loop below) — the watchdog never
    // outlives the un-reaped child, so its group kill can never hit a
    // recycled pgid.
    let timeout = tick_timeout_secs();
    let started = std::time::Instant::now();
    let (cancel_tx, cancel_rx) = std::sync::mpsc::channel::<()>();
    let pgid = child.id() as i32;
    let watchdog = (timeout > 0).then(|| {
        std::thread::spawn(move || {
            match cancel_rx.recv_timeout(std::time::Duration::from_secs(timeout)) {
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    // Only Unix can actually kill the process group. On
                    // non-Unix the watchdog can't terminate the runner, so
                    // reporting a timeout would mislabel a runner that
                    // finishes on its own as killed — and the error message
                    // would falsely claim the group was killed. Return false
                    // there (effectively disabling the watchdog) instead of
                    // lying; the runner is left to complete naturally.
                    #[cfg(unix)]
                    {
                        kill_pgid(pgid);
                        true
                    }
                    #[cfg(not(unix))]
                    {
                        let _ = pgid;
                        false
                    }
                }
                _ => false, // cancelled: the runner finished in time
            }
        })
    });

    // File::create truncates — intentional: tick.log carries the LAST beat
    // only (see the module comment); runs/<id>/output.log is per-beat anyway.
    // A sink that fails to open degrades the REPLAY archive, not the beat —
    // but silently losing it would strand the operator when a beat needs
    // replaying, so name it (same discipline as the guard_degraded events).
    let mut sinks: Vec<File> = Vec::new();
    for p in tee {
        match File::create(p) {
            Ok(f) => sinks.push(f),
            Err(e) => util::event(
                Level::Warn,
                "tick.guard_degraded",
                &format!(
                    "cannot create the tee sink {} (this beat's replay archive is degraded): {e}",
                    p.display()
                ),
                &[],
            ),
        }
    }

    // A sink that OPENED fine can still fail per-write (disk full, quota) —
    // and `let _ = writeln!` alone would drop the replay archive in silence.
    // Warn ONCE per beat (not per line: a full disk would otherwise emit one
    // warning per output line), same degraded-not-fatal discipline as the
    // open failure above.
    let mut tee_write_warned = false;

    // Read RAW bytes per line, then lossy-decode: read_line() would return
    // Err(InvalidData) on invalid UTF-8 (LLM CLIs can emit partial/garbage
    // bytes), and treating that as EOF would drop the reader mid-stream —
    // SIGPIPE-ing a live child and mislabeling the beat as a runner failure.
    let mut reader = BufReader::new(out);
    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => break, // EOF: every writer closed the pipe
            Ok(_) => {
                let line = String::from_utf8_lossy(&buf);
                let line = line.trim_end_matches(['\n', '\r']);
                // Archive only the rendered progress (what the old `_ fmt` pipe wrote).
                if let Some(rendered) = fmt::format_line(line) {
                    let prefix = format!("{}[{}]{} ", util::dim(), util::hms(), util::rst());
                    tee_line(
                        &mut sinks,
                        &mut tee_write_warned,
                        &format!("{prefix}{rendered}"),
                    );
                }
            }
            Err(e) => {
                // A real I/O error on the pipe, not bad UTF-8 (that is
                // lossy-decoded above). Don't break in SILENCE: the rest of
                // the runner's chatter is lost to the replay archive, and an
                // operator replaying a confusing beat deserves to know the
                // log is truncated rather than trust it as complete. The beat
                // itself continues — the reap path below still bounds and
                // reports the runner's exit.
                util::event(
                    Level::Warn,
                    "tick.guard_degraded",
                    &format!(
                        "reading the tick runner's output failed mid-stream ({e}) — the rest \
                         of this beat's replay archive is lost; the runner is still reaped \
                         under the deadline"
                    ),
                    &[],
                );
                break;
            }
        }
    }
    // Drop our end of the pipe BEFORE waiting: after a read error above the
    // child may still be writing, and wait()-ing while holding the read end
    // would deadlock both sides on a full pipe buffer.
    drop(reader);

    // Take the deadline back from the watchdog BEFORE reaping: joining first
    // guarantees the watchdog's group kill can never fire AFTER wait() has
    // reaped the child — which could hit a recycled pgid belonging to someone
    // else. But the stream ending does NOT prove the child is exiting: EOF
    // only means every writer closed the pipe (a runner can close its stdout
    // and keep running), and after a read error the child is definitely still
    // alive — so a plain wait() here could hang the single-instance pulse
    // forever with the watchdog gone. Instead, reap with the ORIGINAL
    // deadline: poll try_wait() and kill the group ourselves if it expires.
    // Killing here is race-free where the watchdog was not: the child is
    // still un-reaped, so its pgid cannot have been recycled.
    let _ = cancel_tx.send(());
    // (cfg_attr: only the Unix arm below can set it — non-Unix would warn.)
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut timed_out = watchdog.is_some_and(|h| h.join().unwrap_or(false));
    let status = if timeout == 0 {
        child.wait() // deadline disabled by the operator — wait indefinitely
    } else {
        let deadline = started + std::time::Duration::from_secs(timeout);
        loop {
            match child.try_wait() {
                Ok(Some(s)) => break Ok(s),
                Ok(None) if std::time::Instant::now() >= deadline => {
                    // Same outcome as the watchdog firing mid-stream, minus
                    // the post-reap race: kill the group, then reap for real.
                    // (On non-Unix nothing can kill the group — fall through
                    // to a plain wait without claiming a kill happened,
                    // matching the watchdog's non-Unix behavior.)
                    #[cfg(unix)]
                    {
                        kill_pgid(pgid);
                        timed_out = true;
                    }
                    // Non-Unix: the deadline DEGRADES to an indefinite wait
                    // (this build cannot kill a process group), so a hung
                    // runner can still stall the single-instance pulse. Warn
                    // once at the moment the deadline lapses — this arm runs
                    // once, right before the unbounded wait — so the operator
                    // sees WHY the pulse is stuck instead of a silent hang.
                    // (macOS/Linux are the primary targets; a real kill path
                    // for other platforms isn't worth the complexity.)
                    #[cfg(not(unix))]
                    util::event(
                        Level::Warn,
                        "tick.guard_degraded",
                        &format!(
                            "LOOOP_TICK_TIMEOUT_SECS ({timeout}s) elapsed but this platform \
                             cannot kill the runner's process group — waiting indefinitely \
                             for it to exit"
                        ),
                        &[],
                    );
                    break child.wait();
                }
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
                Err(e) => break Err(e),
            }
        }
    };
    match status {
        // A clean zero exit WINS over a racing timeout flag: if the runner
        // finished its work, reporting "timed out · killed" would be a lie
        // (the kill hit an already-exiting group, or nothing at all).
        Ok(s) if s.success() => Ok(()),
        Ok(_) if timed_out => Err(format!(
            "timed out after {timeout}s (LOOOP_TICK_TIMEOUT_SECS) — process group killed"
        )),
        Ok(s) => Err(format!("the tick runner exited nonzero ({s})")),
        Err(_) if timed_out => Err(format!(
            "timed out after {timeout}s (LOOOP_TICK_TIMEOUT_SECS) — process group killed"
        )),
        Err(e) => Err(format!("failed to reap the tick runner: {e}")),
    }
}

/// Write one rendered line to every tee sink. The FIRST failed write flips
/// `warned` and emits a single `tick.guard_degraded` event; subsequent
/// failures stay silent (the flag is per-beat — run_streamed resets it by
/// constructing a fresh one). Losing the replay archive is degraded, never
/// fatal, but it must not be INVISIBLE: a disk-full beat that silently
/// dropped every line stranded the operator exactly when a replay was needed.
fn tee_line(sinks: &mut [File], warned: &mut bool, line: &str) {
    for f in sinks.iter_mut() {
        if writeln!(f, "{line}").is_err() && !*warned {
            *warned = true;
            util::event(
                Level::Warn,
                "tick.guard_degraded",
                "a tee sink write failed (disk full?) — this beat's replay archive is \
                 degraded; further tee write failures this beat are not repeated",
                &[],
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tee_write_failure_warns_once_and_keeps_streaming() {
        let dir = std::env::temp_dir().join(format!("looop-tee-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sink.log");
        std::fs::write(&path, "").unwrap();
        // A READ-ONLY handle: every write fails (EBADF), the portable stand-in
        // for a disk-full sink.
        let broken = File::open(&path).unwrap();
        let mut sinks = vec![broken];
        let mut warned = false;
        tee_line(&mut sinks, &mut warned, "line one");
        assert!(
            warned,
            "the first failed tee write flips the warn-once flag"
        );
        // Later lines keep flowing (no panic, no early return) and the flag
        // guard keeps the warning from repeating — the old `let _ =` shape
        // never warned at all.
        tee_line(&mut sinks, &mut warned, "line two");
        assert!(warned);

        // A healthy sink never warns.
        let good_path = dir.join("good.log");
        let mut good = vec![File::create(&good_path).unwrap()];
        let mut warned_good = false;
        tee_line(&mut good, &mut warned_good, "payload");
        assert!(!warned_good, "a successful write must not warn");
        assert!(
            std::fs::read_to_string(&good_path)
                .unwrap()
                .contains("payload"),
            "the line actually landed in the sink"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shell_quote_survives_spaces_quotes_and_dollars() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("with space"), "'with space'");
        // `$` inside single quotes is literal — no expansion.
        assert_eq!(shell_quote("$HOME"), "'$HOME'");
        // The classic POSIX dance: close, escaped quote, reopen.
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        // Round-trip through a REAL shell: the quoted form must reproduce the
        // exact original bytes as one word.
        let tricky = "a b'c$d\"e";
        let out = std::process::Command::new("bash")
            .arg("-c")
            .arg(format!("printf %s {}", shell_quote(tricky)))
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            tricky,
            "the shell must hand the exact original string back"
        );
    }

    #[test]
    fn substitute_prompt_file_quotes_once_never_twice() {
        let path = "/tmp/prompt file's.md";
        let quoted = shell_quote(path);
        // A bare placeholder gets quoted.
        assert_eq!(
            substitute_prompt_file("cat {{prompt_file}}", path),
            format!("cat {quoted}")
        );
        // A config that already wrapped the placeholder in quotes (either
        // style) must NOT be double-quoted — nesting would hand the shell a
        // literal-quote path.
        assert_eq!(
            substitute_prompt_file("cat \"{{prompt_file}}\"", path),
            format!("cat {quoted}"),
            "pre-double-quoted placeholder is replaced whole"
        );
        assert_eq!(
            substitute_prompt_file("cat '{{prompt_file}}'", path),
            format!("cat {quoted}"),
            "pre-single-quoted placeholder is replaced whole"
        );
        // Every occurrence is substituted; unrelated text is untouched.
        assert_eq!(
            substitute_prompt_file("a {{prompt_file}} b '{{prompt_file}}' c", path),
            format!("a {quoted} b {quoted} c")
        );
    }

    #[test]
    fn substitute_prompt_file_path_roundtrips_through_bash() {
        // A path with a space, a single quote, and a `$` must reach the
        // command as ONE argument carrying the exact original bytes.
        let p = crate::paths::Paths::temp();
        let path = p.data_dir.join("we ird'$x.md");
        std::fs::write(&path, "hello").unwrap();
        let cmd = substitute_prompt_file("cat {{prompt_file}}", &path.to_string_lossy());
        let out = std::process::Command::new("bash")
            .arg("-c")
            .arg(&cmd)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "bash must resolve the tricky path: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout), "hello");
    }
}
