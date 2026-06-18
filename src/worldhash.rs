//! DIFF — a single content hash of everything that should trigger a move. If it
//! is unchanged since last tick, the beat skips the AI entirely (cheap,
//! level-triggered). What feeds the hash is deliberate:
//!   * PLAYBOOK + goals: hashed whole.
//!   * sensor snapshots: only the `.signal` (if present) — volatile detail under
//!     `.detail` never wakes the loop; keys are canonicalized so reordering is a
//!     no-op.
//!   * worker sessions: only the STABLE signal (id/state/exit_code/note), never
//!     the ever-incrementing age, so a tick fires on a real transition.

use crate::babysit;
use crate::paths::Paths;
use crate::util;
use std::fs;
use std::path::{Path, PathBuf};

fn rel(paths: &Paths, p: &Path) -> String {
    p.strip_prefix(&paths.data_dir)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

fn sorted_glob(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == ext).unwrap_or(false))
        .collect();
    v.sort();
    v
}

pub fn world_hash(paths: &Paths) -> String {
    let mut buf: Vec<u8> = Vec::new();

    // PLAYBOOK + goals/*.md, each behind an unambiguous path marker.
    let mut files = vec![paths.playbook()];
    files.extend(sorted_glob(&paths.goals_dir(), "md"));
    for f in files {
        if !f.is_file() {
            continue;
        }
        buf.extend_from_slice(format!("@@ {}\n", rel(paths, &f)).as_bytes());
        if let Ok(bytes) = fs::read(&f) {
            buf.extend_from_slice(&bytes);
        }
    }

    // Sensor snapshots: hash only the wake SIGNAL.
    for f in sorted_glob(&paths.snapshots_dir(), "json") {
        buf.extend_from_slice(format!("@@ {}\n", rel(paths, &f)).as_bytes());
        let raw = fs::read(&f).unwrap_or_default();
        match serde_json::from_slice::<serde_json::Value>(&raw) {
            Ok(v) => {
                // {object with "signal"} -> .signal, else the whole value.
                // serde_json::Value serializes objects with sorted keys by
                // default (BTreeMap), matching `jq -cS`'s canonical form.
                let signalled = match &v {
                    serde_json::Value::Object(m) if m.contains_key("signal") => {
                        m.get("signal").cloned().unwrap_or(serde_json::Value::Null)
                    }
                    _ => v,
                };
                buf.extend_from_slice(signalled.to_string().as_bytes());
                buf.push(b'\n');
            }
            Err(_) => buf.extend_from_slice(&raw), // non-JSON / error reading: raw bytes
        }
    }

    // Worker sessions: stable signal only (id state exit_code note), null-faithful.
    for s in babysit::list_looop() {
        let exit = s
            .exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "null".into());
        let note = s.note.clone().unwrap_or_else(|| "null".into());
        buf.extend_from_slice(format!("{} {} {} {}\n", s.id, s.state, exit, note).as_bytes());
    }

    util::content_hash(&buf)
}
