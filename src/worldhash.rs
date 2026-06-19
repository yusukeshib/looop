//! DIFF — a single content hash of everything that should trigger a move. If it
//! is unchanged since last tick, the beat skips the AI entirely (cheap,
//! level-triggered). What feeds the hash is deliberate:
//!   * PLAYBOOK + goals: hashed whole.
//!   * sensor snapshots: only the `.signal` (if present) — volatile detail under
//!     `.detail` never wakes the loop; keys are canonicalized so reordering is a
//!     no-op.
//!   * worker sessions: only the STABLE signal (id/state/exit_code/note), never
//!     the ever-incrementing age, so a tick fires on a real transition.

use crate::paths::Paths;
use crate::session;
use crate::util;
use std::fs;
use std::path::{Path, PathBuf};

fn rel(paths: &Paths, p: &Path) -> String {
    p.strip_prefix(&paths.data_dir)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Reduce a sensor snapshot to the part that should WAKE the loop: an object
/// with a `signal` key contributes only `.signal`; anything else contributes
/// whole. Volatile `.detail` is dropped so it reaches the prompt but never the
/// change-detection hash. `serde_json::Value` serializes objects with sorted
/// keys (BTreeMap), matching `jq -cS`'s canonical form.
fn wake_signal(v: serde_json::Value) -> serde_json::Value {
    match &v {
        serde_json::Value::Object(m) if m.contains_key("signal") => {
            m.get("signal").cloned().unwrap_or(serde_json::Value::Null)
        }
        _ => v,
    }
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
                buf.extend_from_slice(wake_signal(v).to_string().as_bytes());
                buf.push(b'\n');
            }
            Err(_) => buf.extend_from_slice(&raw), // non-JSON / error reading: raw bytes
        }
    }

    // Worker sessions: stable signal only (id state exit_code note), null-faithful.
    // Workers only — the pulse's own session must not feed its own wake signal.
    for s in session::list_workers(paths) {
        let exit = s
            .exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "null".into());
        let note = s.note.clone().unwrap_or_else(|| "null".into());
        buf.extend_from_slice(format!("{} {} {} {}\n", s.id, s.state, exit, note).as_bytes());
    }

    util::content_hash(&buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn wake_signal_keeps_only_signal_when_present() {
        let v = json!({ "signal": { "open": 3 }, "detail": { "checked_at": "now" } });
        assert_eq!(wake_signal(v), json!({ "open": 3 }));
    }

    #[test]
    fn wake_signal_passes_through_objects_without_signal() {
        let v = json!({ "open": 3, "closed": 1 });
        assert_eq!(wake_signal(v.clone()), v);
    }

    #[test]
    fn wake_signal_passes_through_non_objects() {
        assert_eq!(wake_signal(json!(42)), json!(42));
        assert_eq!(wake_signal(json!([1, 2])), json!([1, 2]));
    }

    #[test]
    fn wake_signal_ignores_volatile_detail_changes() {
        // Same signal, different detail => identical wake signal (no false wake).
        let a = json!({ "signal": { "open": 3 }, "detail": { "ts": 1 } });
        let b = json!({ "signal": { "open": 3 }, "detail": { "ts": 999 } });
        assert_eq!(wake_signal(a), wake_signal(b));
    }

    #[test]
    fn world_hash_is_stable_and_change_sensitive() {
        let p = Paths::temp();
        fs::create_dir_all(p.goals_dir()).unwrap();
        fs::write(p.playbook(), b"rule one\n").unwrap();
        fs::write(p.goals_dir().join("a.md"), b"goal a\n").unwrap();

        let h1 = world_hash(&p);
        let h2 = world_hash(&p);
        assert_eq!(h1, h2, "same content must hash the same");

        fs::write(p.goals_dir().join("a.md"), b"goal a changed\n").unwrap();
        let h3 = world_hash(&p);
        assert_ne!(h1, h3, "a goal edit must change the world hash");
    }
}
