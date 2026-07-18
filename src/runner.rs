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
/// (the replay archive). Returns whether the runner exited successfully.
pub fn run_streamed(paths: &Paths, tick_cmd: &str, prompt_file: &Path, tee: &[PathBuf]) -> bool {
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
        let stdin = match File::open(prompt_file) {
            Ok(f) => f,
            Err(_) => return false,
        };
        cmd.stdin(Stdio::from(stdin));
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return false,
    };
    let Some(out) = child.stdout.take() else {
        return false;
    };

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

    child.wait().is_ok_and(|s| s.success())
}
