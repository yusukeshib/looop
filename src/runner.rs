//! ACT — run the configured tick runner once, teeing its output to the per-beat
//! archive (runs/<id>/output.log) and tick.log so a beat is replayable. The
//! pulse keeps its own stream a clean structured-event log: the runner's
//! free-form chatter is archived to the tee files but never echoed live (watch
//! it with `looop watch pulse` / replay it from runs/<id>/output.log).
//!
//! Formatting + cost metering happen IN-PROCESS here: we read the runner's raw
//! NDJSON stdout line-by-line, render each line via `cost::format_line` (the
//! friendly `→ bash:` progress), and fold every line into a `cost::CostMeter`,
//! recording the run's spend to the ledger when the stream ends. There is no
//! external formatter and looop never re-execs itself to post-process its own
//! child — the old `| "$LOOOP_BIN" _ fmt` pipe seam is gone.

use crate::cost::{self, CostMeter};
use crate::paths::Paths;
use crate::util;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Run `tick_cmd` (a shell pipeline) under `bash -lc`, with cwd at the data dir
/// and stdin from `prompt_file`. stdout+stderr are merged; each line is metered
/// into a `CostMeter`, rendered via `cost::format_line`, stamped, and written to
/// every `tee` file (the replay archive). The run's resolved spend is recorded
/// to the ledger under (`cost_kind`, `cost_id`, `cost_runner`) before returning.
/// Returns whether the runner exited successfully.
pub fn run_streamed(
    paths: &Paths,
    tick_cmd: &str,
    prompt_file: &Path,
    cost_kind: &str,
    cost_id: &str,
    cost_runner: &str,
    tee: &[PathBuf],
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

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return false,
    };
    let Some(out) = child.stdout.take() else {
        return false;
    };

    let mut sinks: Vec<File> = tee.iter().filter_map(|p| File::create(p).ok()).collect();
    let mut meter = CostMeter::default();

    for line in BufReader::new(out).lines() {
        let Ok(line) = line else { break };
        // Meter EVERY raw line (cost events emit nothing to format_line).
        meter.ingest(&line);
        // Archive only the rendered progress (what the old `_ fmt` pipe wrote).
        if let Some(rendered) = cost::format_line(&line) {
            let prefix = format!("{}[{}]{} ", util::dim(), util::hms(), util::rst());
            for f in &mut sinks {
                let _ = writeln!(f, "{prefix}{rendered}");
            }
        }
    }

    let ok = child.wait().map(|s| s.success()).unwrap_or(false);
    // Record the run's spend (no-op for non-positive / unmetered totals).
    cost::record_cost(
        paths,
        cost_kind,
        cost_id,
        cost_runner,
        &format!("{:.6}", meter.total()),
    );
    ok
}
