//! SENSE — run every `sensors/*.sh`, each printing one JSON snapshot of the
//! world. Two guardrails keep a misbehaving sensor from harming the pulse:
//!   * a portable timeout (LOOOP_SENSOR_TIMEOUT, default 60s) so a hung sensor
//!     can't freeze the beat;
//!   * a size cap (LOOOP_SENSOR_MAX_BYTES, default 8192) so an oversized blob
//!     can't silently inflate prompt context + LLM cost on every beat.

use crate::paths::Paths;
use crate::session;
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

/// A virtual system sensor: an in-process probe of looop's OWN state that
/// returns one `{signal,detail}` snapshot value.
type Probe = fn(&Paths) -> serde_json::Value;

/// One observation source. The loop senses the world through a uniform set of
/// these; the User/System split is the ONLY place the distinction lives, and
/// everything downstream treats every sensor identically.
///
/// - `User` is a `sensors/*.sh` script: authored by the decider/human, shelled
///   out with a timeout + size cap, and MAY fail.
/// - `System` is a VIRTUAL in-process [`Probe`] of looop's OWN state (the fleet,
///   the leases): no source file, no shell, no timeout, never fails.
///
/// Both write ONE `{signal,detail}` JSON snapshot into snap_dir under a
/// kind-prefixed name (`sensor-…` / `sys-…`), so the world hash and the tick
/// prompt consume one uniform snapshot stream instead of bespoke per-kind code.
enum Sensor {
    User(PathBuf),
    System { name: &'static str, probe: Probe },
}

/// The fixed set of system sensors. Expose another slice of looop's internal
/// state to the decider by adding one row + a [`Probe`].
const SYSTEM_SENSORS: &[(&str, Probe)] = &[("sessions", sys_sessions), ("claims", sys_claims)];

/// One sensor's outcome, for the run summary.
struct Reading {
    name: String,
    ok: bool,
    secs: u64,
}

impl Sensor {
    /// Snapshot basename (no extension): `sensor-<stem>` or `sys-<name>`.
    fn name(&self) -> String {
        match self {
            Sensor::User(p) => format!(
                "sensor-{}",
                p.file_stem().unwrap_or_default().to_string_lossy()
            ),
            Sensor::System { name, .. } => format!("sys-{name}"),
        }
    }

    /// Produce this sensor's snapshot into snap_dir; report ok + duration.
    fn sense(&self, paths: &Paths, snap_dir: &Path) -> Reading {
        let name = self.name();
        let t0 = Instant::now();
        let ok = match self {
            Sensor::User(script) => {
                let out = snap_dir.join(format!("{name}.json"));
                let err = snap_dir.join(format!("{name}.err"));
                let rc = exec_sensor(script, &out, &err);
                if rc == 124 {
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
                rc == 0
            }
            // Virtual: a probe can't fail or hang, so there's no timeout/err path.
            Sensor::System { probe, .. } => {
                let body = probe(paths);
                let _ = fs::write(snap_dir.join(format!("{name}.json")), format!("{body}\n"));
                true
            }
        };
        Reading {
            name,
            ok,
            secs: t0.elapsed().as_secs(),
        }
    }
}

/// Every sensor for this beat: the user `sensors/*.sh` followed by the fixed
/// system probes.
fn all_sensors(paths: &Paths) -> Vec<Sensor> {
    let mut v: Vec<Sensor> = sensor_scripts(paths)
        .into_iter()
        .map(Sensor::User)
        .collect();
    v.extend(
        SYSTEM_SENSORS
            .iter()
            .map(|&(name, probe)| Sensor::System { name, probe }),
    );
    v
}

/// System sensor: the live worker fleet (the pulse excludes itself, so it never
/// feeds its own wake signal). The wake SIGNAL is each worker's stable identity
/// — id/state/exit_code, plus a ⚑note ONLY while the worker is alive (a note on
/// a corpse is stale, so it neither wakes the loop nor reaches the decider).
/// Sorted by id so the snapshot — and thus the hash — is order-stable.
fn sys_sessions(paths: &Paths) -> serde_json::Value {
    let mut workers = session::list_workers(paths);
    workers.sort_by(|a, b| a.id.cmp(&b.id));
    let signal: Vec<serde_json::Value> = workers
        .iter()
        .map(|s| {
            let mut o = serde_json::json!({
                "id": s.id,
                "state": s.state,
                "exit_code": s.exit_code,
            });
            if s.alive && s.note.is_some() {
                o["note"] = serde_json::json!(s.note);
            }
            o
        })
        .collect();
    serde_json::json!({ "signal": signal, "detail": { "count": workers.len() } })
}

/// System sensor: live worker leases (claims/*.json). Stale claims are reaped
/// deterministically BEFORE sense, so every lease here is owned by a live worker
/// — a name listed is OWNED; the decider must not act on it itself. The lease
/// set IS the wake SIGNAL: a worker taking or releasing a task is a real
/// transition the decider may react to.
fn sys_claims(paths: &Paths) -> serde_json::Value {
    let mut entries: Vec<PathBuf> = fs::read_dir(paths.claims_dir())
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "json").unwrap_or(false))
        .collect();
    entries.sort();
    let leases: Vec<serde_json::Value> = entries
        .iter()
        .map(|cf| {
            let name = cf
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            let claim: serde_json::Value = fs::read_to_string(cf)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or(serde_json::Value::Null);
            serde_json::json!({ "name": name, "claim": claim })
        })
        .collect();
    serde_json::json!({ "signal": leases })
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

/// Run every sensor — user `sensors/*.sh` AND the virtual system probes — into
/// `snap_dir`. Caller is responsible for wiping the dir first (level-triggered).
/// When `verbose`, log each sensor + duration like a tick; otherwise stay quiet
/// (manual goal runs).
pub fn run_all(paths: &Paths, snap_dir: &Path, verbose: bool) {
    let sensors = all_sensors(paths);
    let total = sensors.len();
    // Per-sensor lines are machine granularity — only the JSON stream gets them.
    // The human pulse stream gets ONE summary line (below), so a healthy fleet of
    // sensors doesn't drown the decisions a watcher actually cares about.
    let json = util::is_json();
    let t0_all = Instant::now();
    let mut ok = 0usize;
    let mut failed: Vec<String> = Vec::new();

    for s in &sensors {
        let r = s.sense(paths, snap_dir);
        if r.ok {
            ok += 1;
            if verbose && json {
                util::event(
                    Level::Ok,
                    "sense.ok",
                    &format!("{} ({}s)", r.name, r.secs),
                    &[
                        ("sensor", serde_json::json!(r.name)),
                        ("secs", serde_json::json!(r.secs)),
                    ],
                );
            }
        } else {
            if verbose && json {
                util::event(
                    Level::Error,
                    "sense.fail",
                    &format!(
                        "{} failed ({}s) — see snapshots/{}.err",
                        r.name, r.secs, r.name
                    ),
                    &[
                        ("sensor", serde_json::json!(r.name)),
                        ("secs", serde_json::json!(r.secs)),
                    ],
                );
            }
            failed.push(r.name);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensor_name_is_kind_prefixed() {
        let user = Sensor::User(PathBuf::from("/x/sensors/today.sh"));
        assert_eq!(user.name(), "sensor-today");
        let sys = Sensor::System {
            name: "sessions",
            probe: sys_sessions,
        };
        assert_eq!(sys.name(), "sys-sessions");
    }

    #[test]
    fn sys_claims_signal_lists_live_leases_sorted() {
        let p = Paths::temp();
        fs::create_dir_all(p.claims_dir()).unwrap();
        fs::write(
            p.claims_dir().join("repo-b.json"),
            br#"{"session":"w2","name":"repo-b"}"#,
        )
        .unwrap();
        fs::write(
            p.claims_dir().join("repo-a.json"),
            br#"{"session":"w1","name":"repo-a"}"#,
        )
        .unwrap();

        let v = sys_claims(&p);
        let leases = v.get("signal").and_then(|s| s.as_array()).unwrap();
        assert_eq!(leases.len(), 2);
        // Sorted by file name so the snapshot — and the world hash — is stable.
        assert_eq!(leases[0]["name"], "repo-a");
        assert_eq!(leases[1]["name"], "repo-b");
        assert_eq!(leases[0]["claim"]["session"], "w1");
    }

    #[test]
    fn sys_claims_empty_when_no_dir() {
        let p = Paths::temp();
        let v = sys_claims(&p);
        assert_eq!(v["signal"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn system_sensor_sense_writes_snapshot() {
        let p = Paths::temp();
        let snap = p.snapshots_dir();
        fs::create_dir_all(&snap).unwrap();
        let s = Sensor::System {
            name: "claims",
            probe: sys_claims,
        };
        let r = s.sense(&p, &snap);
        assert!(r.ok);
        assert_eq!(r.name, "sys-claims");
        assert!(snap.join("sys-claims.json").is_file());
    }
}
