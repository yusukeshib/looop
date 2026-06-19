//! SENSE — run every `sensors/*.sh`, each printing one JSON snapshot of the
//! world. Two guardrails keep a misbehaving sensor from harming the pulse:
//!   * a portable timeout (LOOOP_SENSOR_TIMEOUT, default 60s) so a hung sensor
//!     can't freeze the beat;
//!   * a size cap (LOOOP_SENSOR_MAX_BYTES, default 8192) so an oversized blob
//!     can't silently inflate prompt context + LLM cost on every beat.

use crate::paths::Paths;
use crate::util::{self, Level};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

fn env_num(var: &str, default: u64) -> u64 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(default)
}

/// Run ONE sensor with a portable timeout + size cap. Returns its exit status
/// (124 = timed out, per coreutils). `out`/`err` receive stdout/stderr.
fn exec_sensor(script: &Path, out: &Path, err: &Path) -> i32 {
    let to = env_num("LOOOP_SENSOR_TIMEOUT", 60);
    let tbin = if to != 0 {
        if util::on_path("timeout") {
            Some("timeout")
        } else if util::on_path("gtimeout") {
            Some("gtimeout")
        } else {
            None
        }
    } else {
        None
    };

    let (Ok(of), Ok(ef)) = (File::create(out), File::create(err)) else {
        return 1;
    };

    let mut cmd = match tbin {
        Some(t) => {
            let mut c = Command::new(t);
            c.arg(to.to_string()).arg(script);
            c
        }
        None => Command::new(script),
    };
    let status = cmd.stdout(of).stderr(ef).status();
    let rc = match status {
        Ok(s) => s.code().unwrap_or(1),
        Err(_) => 1,
    };

    // Context backpressure: a successful reading over the cap is replaced with a
    // tiny error object so the pulse stops paying for the blob and the AI sees
    // the misbehavior.
    let cap = env_num("LOOOP_SENSOR_MAX_BYTES", 8192);
    if rc == 0
        && cap != 0
        && let Ok(meta) = fs::metadata(out)
    {
        let sz = meta.len();
        if sz > cap {
            let blob = serde_json::json!({
                "error": "sensor output too large — emit a small normalized {signal,detail} snapshot, not a raw dump",
                "bytes": sz,
                "cap": cap,
            });
            let _ = fs::write(out, format!("{blob}\n"));
        }
    }
    rc
}

/// Sorted list of `sensors/*.sh`.
pub fn sensor_scripts(paths: &Paths) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(paths.sensors_dir())
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "sh").unwrap_or(false))
        .collect();
    v.sort();
    v
}

/// Run every sensor into `snap_dir`. Caller is responsible for wiping the dir
/// first (level-triggered). When `verbose`, log each sensor + duration like a
/// tick; otherwise stay quiet (manual goal runs).
pub fn run_all(paths: &Paths, snap_dir: &Path, verbose: bool) {
    let scripts = sensor_scripts(paths);
    let total = scripts.len();
    // Per-sensor lines are machine granularity — only the JSON stream gets them.
    // The human pulse stream gets ONE summary line (below), so a healthy fleet of
    // sensors doesn't drown the decisions a watcher actually cares about.
    let json = util::is_json();
    let t0_all = Instant::now();
    let mut ok = 0usize;
    let mut failed: Vec<String> = Vec::new();

    for s in scripts {
        let name = s
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let out = snap_dir.join(format!("sensor-{name}.json"));
        let err = snap_dir.join(format!("sensor-{name}.err"));
        let t0 = Instant::now();
        let rc = exec_sensor(&s, &out, &err);
        let secs = t0.elapsed().as_secs();
        if rc == 124 {
            let to = env_num("LOOOP_SENSOR_TIMEOUT", 60);
            let _ = fs::OpenOptions::new().append(true).open(&err).map(|mut f| {
                use std::io::Write;
                let _ = writeln!(f, "sensor timed out after {to}s (LOOOP_SENSOR_TIMEOUT)");
            });
        }
        if rc == 0 {
            ok += 1;
            if verbose && json {
                util::event(
                    Level::Ok,
                    "sense.ok",
                    &format!("{name} ({secs}s)"),
                    &[
                        ("sensor", serde_json::json!(name)),
                        ("secs", serde_json::json!(secs)),
                    ],
                );
            }
        } else {
            failed.push(name.clone());
            if verbose && json {
                util::event(
                    Level::Error,
                    "sense.fail",
                    &format!("{name} failed ({secs}s) — see snapshots/sensor-{name}.err"),
                    &[
                        ("sensor", serde_json::json!(name)),
                        ("secs", serde_json::json!(secs)),
                    ],
                );
            }
        }
        // Drop the empty .err a successful sensor leaves behind.
        if fs::metadata(&err).map(|m| m.len() == 0).unwrap_or(false) {
            let _ = fs::remove_file(&err);
        }
    }

    // The summary: a single dim heartbeat line when all is well, a red line that
    // names the offenders when not. (In JSON mode this rides alongside the
    // per-sensor events as a `sense` aggregate.)
    if verbose && total > 0 {
        let secs = t0_all.elapsed().as_secs();
        let fields = [
            ("ok", serde_json::json!(ok)),
            ("total", serde_json::json!(total)),
            ("failed", serde_json::json!(failed)),
            ("secs", serde_json::json!(secs)),
        ];
        if failed.is_empty() {
            util::event(
                Level::Info,
                "sense",
                &format!("{ok} sensors ok ({secs}s)"),
                &fields,
            );
        } else {
            util::event(
                Level::Error,
                "sense",
                &format!("{ok}/{total} sensors ok · failed: {}", failed.join(", ")),
                &fields,
            );
        }
    }
}
