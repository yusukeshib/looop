//! ACT — run the configured tick runner once, teeing its output to the per-beat
//! archive (runs/<id>/output.log) and tick.log so a beat is replayable. The
//! pulse keeps its own stream a clean structured-event log: the runner's
//! free-form chatter is archived to the tee files but never echoed live (watch
//! it with `looop watch pulse` / replay it from runs/<id>/output.log).
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

/// Run `tick_cmd` (a shell pipeline) under `bash -c`, with cwd at the data dir
/// and stdin from `prompt_file`. stdout+stderr are merged; each line is rendered
/// via `fmt::format_line`, stamped, and written to every `tee` file (the replay
/// archive). Returns whether the runner exited successfully.
pub fn run_streamed(paths: &Paths, tick_cmd: &str, prompt_file: &Path, tee: &[PathBuf]) -> bool {
    let stdin = match File::open(prompt_file) {
        Ok(f) => f,
        Err(_) => return false,
    };

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
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return false,
    };
    let Some(out) = child.stdout.take() else {
        return false;
    };

    let mut sinks: Vec<File> = tee.iter().filter_map(|p| File::create(p).ok()).collect();

    for line in BufReader::new(out).lines() {
        let Ok(line) = line else { break };
        // Archive only the rendered progress (what the old `_ fmt` pipe wrote).
        if let Some(rendered) = fmt::format_line(&line) {
            let prefix = format!("{}[{}]{} ", util::dim(), util::hms(), util::rst());
            for f in &mut sinks {
                let _ = writeln!(f, "{prefix}{rendered}");
            }
        }
    }

    child.wait().map(|s| s.success()).unwrap_or(false)
}
