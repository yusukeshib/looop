//! DIFF — a single content hash of everything that should trigger a move. If it
//! is unchanged since last tick, the beat skips the AI entirely (cheap,
//! level-triggered). What feeds the hash is deliberate:
//!   * PLAYBOOK + goals: hashed whole (the desired state).
//!   * sensor snapshots: only the `.signal` (if present) — volatile detail under
//!     `.detail` never wakes the loop; keys are canonicalized so reordering is a
//!     no-op. This is the WHOLE observed-state half: the live worker fleet and
//!     the worker leases are themselves system sensors (`sys-sessions` /
//!     `sys-claims`), so they flow through this same snapshot loop — there is no
//!     bespoke per-kind hashing. A worker's stable identity (id/state/exit_code)
//!     lives in that snapshot's `.signal`; volatile context (counts, ages) rides
//!     in `.detail` and never wakes the loop.
//!
//! ASYMMETRY (M3, deliberate): PLAYBOOK.md and goals/*.md are hashed whole, so
//! editing them wakes the loop next beat. The sensor SCRIPTS (sensors/*.sh) are
//! NOT hashed — only the snapshots they produce are. Editing a sensor script
//! therefore does NOT wake the loop on its own; the change only takes effect once
//! the next snapshot it emits differs in its `.signal`. Rationale: a sensor's job
//! is to observe the world, not to BE part of the world we react to — rehashing
//! the script would wake the loop on every cosmetic edit (comments, formatting)
//! that produces identical readings. If you need an edited sensor to take effect
//! immediately, run it once so its snapshot refreshes (the pulse regenerates
//! snapshots every beat anyway).

use crate::paths::Paths;
use crate::util;
use std::fs;
use std::path::Path;

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
pub(crate) fn wake_signal(v: serde_json::Value) -> serde_json::Value {
    match &v {
        serde_json::Value::Object(m) if m.contains_key("signal") => {
            m.get("signal").cloned().unwrap_or(serde_json::Value::Null)
        }
        _ => v,
    }
}

/// Hash the POLICY half only: PLAYBOOK + goals/*.md, each behind an unambiguous
/// path marker. Shared by [`world_hash`] and [`policy_hash`].
fn hash_policy_into(paths: &Paths, buf: &mut Vec<u8>) {
    let mut files = vec![paths.playbook()];
    files.extend(util::sorted_glob(&paths.goals_dir(), "md"));
    for f in files {
        if !f.is_file() {
            continue;
        }
        buf.extend_from_slice(format!("@@ {}\n", rel(paths, &f)).as_bytes());
        if let Ok(bytes) = fs::read(&f) {
            buf.extend_from_slice(&bytes);
        }
    }
}

/// The world broken into NAMED items, for the prompt's `WHAT CHANGED` diff:
///   * `playbook` / `goal:<id>` → a short content hash (policy files — the diff
///     names WHICH file moved; the decider re-reads the live body anyway),
///   * `snap:<name>` → the canonical wake-signal JSON itself (small by design —
///     the diff can show old → new inline).
///
/// Uses the SAME inputs and signal-reduction as [`world_hash`], so "the hash
/// moved" and "some item differs" can never disagree.
pub fn world_items(paths: &Paths) -> std::collections::BTreeMap<String, String> {
    let mut m = std::collections::BTreeMap::new();

    if paths.playbook().is_file()
        && let Ok(bytes) = fs::read(paths.playbook())
    {
        m.insert("playbook".to_string(), util::content_hash(&bytes));
    }
    for f in util::sorted_glob(&paths.goals_dir(), "md") {
        if let (Some(stem), Ok(bytes)) = (f.file_stem(), fs::read(&f)) {
            m.insert(
                format!("goal:{}", stem.to_string_lossy()),
                util::content_hash(&bytes),
            );
        }
    }
    for f in util::sorted_glob(&paths.snapshots_dir(), "json") {
        let Some(stem) = f.file_stem().map(|s| s.to_string_lossy().to_string()) else {
            continue;
        };
        let raw = fs::read(&f).unwrap_or_default();
        let val = match serde_json::from_slice::<serde_json::Value>(&raw) {
            Ok(v) => wake_signal(v).to_string(),
            Err(_) => format!("(non-JSON, {} bytes)", raw.len()),
        };
        m.insert(format!("snap:{stem}"), val);
    }
    m
}

pub fn world_hash(paths: &Paths) -> String {
    let mut buf: Vec<u8> = Vec::new();

    // PLAYBOOK + goals/*.md, each behind an unambiguous path marker.
    hash_policy_into(paths, &mut buf);

    // Sensor snapshots: hash only the wake SIGNAL. User sensors AND the virtual
    // system sensors (sys-sessions / sys-claims) all land here, so the fleet and
    // leases are diffed through this one loop — no bespoke per-kind hashing.
    for f in util::sorted_glob(&paths.snapshots_dir(), "json") {
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
