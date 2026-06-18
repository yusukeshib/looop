//! ACT — run the configured tick runner once, streaming its output live with a
//! timestamp gutter and teeing it to the per-beat archive (runs/<id>/output.log)
//! and tick.log, so a beat is replayable. A faithful port of the bash
//! `( cd DATA && eval "$tick_cmd" < prompt ) 2>&1 | ts_prefix | tee …` pipeline.

use crate::paths::Paths;
use crate::util;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Run `tick_cmd` (a shell pipeline) under `bash -lc`, with cwd at the data dir
/// and stdin from `prompt_file`. stdout+stderr are merged, each line is stamped
/// with `gutter`, and written to our stdout plus every `tee` file. Returns
/// whether the runner exited successfully.
pub fn run_streamed(
    paths: &Paths,
    tick_cmd: &str,
    prompt_file: &Path,
    cost_env: &[(&str, &str)],
    tee: &[PathBuf],
    gutter: &str,
) -> bool {
    let stdin = match File::open(prompt_file) {
        Ok(f) => f,
        Err(_) => return false,
    };

    // `{ …; } 2>&1` merges the whole pipeline's stderr into stdout in order, so
    // a single pipe carries everything (Rust can't easily interleave two pipes).
    let script = format!("{{ {tick_cmd} ; }} 2>&1");

    let mut cmd = Command::new("bash");
    cmd.arg("-lc")
        .arg(&script)
        .current_dir(&paths.data_dir)
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::piped());
    for (k, v) in cost_env {
        cmd.env(k, v);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return false,
    };
    let Some(out) = child.stdout.take() else {
        return false;
    };

    let mut sinks: Vec<File> = tee.iter().filter_map(|p| File::create(p).ok()).collect();
    let mut stdout = std::io::stdout();
    // In JSON mode stdout is a machine NDJSON stream; the runner's free-form
    // output would corrupt it, so we only tee it to the archive files there.
    let echo = !util::is_json();

    for line in BufReader::new(out).lines() {
        let Ok(line) = line else { break };
        let stamped = format!(
            "{}[{}]{} {}{}",
            util::dim(),
            util::hms(),
            util::rst(),
            gutter,
            line
        );
        if echo {
            let _ = writeln!(stdout, "{stamped}");
            let _ = stdout.flush();
        }
        for f in &mut sinks {
            let _ = writeln!(f, "{stamped}");
        }
    }

    child.wait().map(|s| s.success()).unwrap_or(false)
}
