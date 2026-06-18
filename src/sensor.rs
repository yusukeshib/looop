//! SENSE — run every `sensors/*.sh`, each printing one JSON snapshot of the
//! world. Two guardrails keep a misbehaving sensor from harming the pulse:
//!   * a portable timeout (LOOOP_SENSOR_TIMEOUT, default 60s) so a hung sensor
//!     can't freeze the beat;
//!   * a size cap (LOOOP_SENSOR_MAX_BYTES, default 8192) so an oversized blob
//!     can't silently inflate prompt context + LLM cost on every beat.

use crate::paths::Paths;
use crate::util;
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
    if rc == 0 && cap != 0 {
        if let Ok(meta) = fs::metadata(out) {
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
    if verbose && !scripts.is_empty() {
        util::log(&format!(
            "{}sensing the world{} {}({} sensors){}…",
            util::b(),
            util::rst(),
            util::dim(),
            scripts.len(),
            util::rst()
        ));
    }
    for s in scripts {
        let name = s.file_stem().unwrap_or_default().to_string_lossy().to_string();
        let out = snap_dir.join(format!("sensor-{name}.json"));
        let err = snap_dir.join(format!("sensor-{name}.err"));
        let t0 = Instant::now();
        let rc = exec_sensor(&s, &out, &err);
        let secs = t0.elapsed().as_secs();
        if verbose {
            if rc == 0 {
                util::log(&format!(
                    "  {}✓{} {} {}({}s){}",
                    util::grn(),
                    util::rst(),
                    name,
                    util::dim(),
                    secs,
                    util::rst()
                ));
            } else {
                if rc == 124 {
                    let to = env_num("LOOOP_SENSOR_TIMEOUT", 60);
                    let _ = fs::OpenOptions::new()
                        .append(true)
                        .open(&err)
                        .map(|mut f| {
                            use std::io::Write;
                            let _ = writeln!(
                                f,
                                "sensor timed out after {to}s (LOOOP_SENSOR_TIMEOUT)"
                            );
                        });
                }
                util::log(&format!(
                    "  {}✗ {} FAILED{} {}({}s) — see snapshots/sensor-{}.err{}",
                    util::red(),
                    name,
                    util::rst(),
                    util::dim(),
                    secs,
                    name,
                    util::rst()
                ));
            }
        } else if rc == 124 {
            let to = env_num("LOOOP_SENSOR_TIMEOUT", 60);
            let _ = fs::OpenOptions::new().append(true).open(&err).map(|mut f| {
                use std::io::Write;
                let _ = writeln!(f, "sensor timed out after {to}s (LOOOP_SENSOR_TIMEOUT)");
            });
        }
        // Drop the empty .err a successful sensor leaves behind.
        if fs::metadata(&err).map(|m| m.len() == 0).unwrap_or(false) {
            let _ = fs::remove_file(&err);
        }
    }
}
