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
    if let serde_json::Value::Object(m) = &v
        && let Some(signal) = m.get("signal")
    {
        return signal.clone();
    }
    v
}

/// Hash of the POLICY half only (PLAYBOOK + goals): the steering surface a
/// HUMAN edits. The backoff gate stores this alongside the fail counter so a
/// steering edit can cut the backoff wait short, while mere sensor movement
/// (which a failing action can cause every beat) cannot.
pub(crate) fn policy_hash(paths: &Paths) -> String {
    let mut buf: Vec<u8> = Vec::new();
    hash_policy_into(paths, &mut buf, None);
    util::content_hash(&buf)
}

/// Hash the POLICY half only: PLAYBOOK + goals/*.md, each behind an unambiguous
/// path marker. Shared by [`world_view`] and [`policy_hash`]. When `items` is
/// given, each file's ITEM view (`playbook` / `goal:<id>` → content hash or the
/// `!unreadable` sentinel) is derived from the SAME read that fed the hash
/// buffer — the one-pass guarantee [`world_view`] promises.
fn hash_policy_into(
    paths: &Paths,
    buf: &mut Vec<u8>,
    mut items: Option<&mut std::collections::BTreeMap<String, String>>,
) {
    let mut files = vec![(paths.playbook(), "playbook".to_string())];
    files.extend(
        util::sorted_glob(&paths.goals_dir(), "md")
            .into_iter()
            .map(|f| {
                let key = format!(
                    "goal:{}",
                    f.file_stem().unwrap_or_default().to_string_lossy()
                );
                (f, key)
            }),
    );
    for (f, key) in files {
        // fs::metadata, NOT `!f.is_file()`: is_file() maps EVERY stat error
        // (EACCES, EIO, …) to false, which made a present-but-unstat-able
        // policy file hash-invisible — identical to absent, so a goal
        // flipping unreadable never moved the world. Only a definitive
        // NotFound may skip (the PLAYBOOK legitimately may not exist yet);
        // any other stat failure falls through to the read below, whose
        // failure path emits the same `!unreadable` sentinel.
        match fs::metadata(&f) {
            // A directory squatting on a policy path is not policy — skip it
            // (same verdict is_file() used to give), and fs::read on a dir
            // would error into a misleading "unreadable" sentinel.
            Ok(m) if !m.is_file() => continue,
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {} // fall through: the read error path carries the sentinel
        }
        let bytes = match fs::read(&f) {
            Ok(b) => b,
            Err(_) => {
                // A present-but-UNREADABLE policy file must not be
                // hash-invisible (identical to absent): a goal flipping
                // unreadable would then never move the world. A sentinel
                // section keeps the transition readable↔unreadable visible.
                buf.extend_from_slice(format!("@@ {} !unreadable\n", rel(paths, &f)).as_bytes());
                // The item view carries the same sentinel (silently skipping
                // it would render as "gone" in the diff while the hash said
                // "changed").
                if let Some(items) = items.as_deref_mut() {
                    items.insert(key, "!unreadable".to_string());
                }
                continue;
            }
        };
        // Length-prefixed section marker: a bare "@@ path\n" separator is
        // ambiguous when a file's CONTENT contains "@@ " lines; prefixing the
        // payload length makes the framing injective. NOTE: this changed the
        // hash input format, so the first beat after upgrading sees one
        // (harmless) "world changed" and re-decides.
        buf.extend_from_slice(format!("@@ {} {}\n", rel(paths, &f), bytes.len()).as_bytes());
        buf.extend_from_slice(&bytes);
        if let Some(items) = items.as_deref_mut() {
            items.insert(key, util::content_hash(&bytes));
        }
    }
}

/// The world broken into NAMED items, for the prompt's `WHAT CHANGED` diff:
///   * `playbook` / `goal:<id>` → a short content hash (policy files — the diff
///     names WHICH file moved; the decider re-reads the live body anyway), or
///     the same `!unreadable` sentinel [`world_hash`] uses when the file is
///     present but unreadable (silently skipping it would render as "gone" in
///     the diff while the hash said "changed"),
///   * `snap:<name>` → the canonical wake-signal JSON itself (small by design —
///     the diff can show old → new inline). A non-JSON snapshot contributes a
///     hash of its raw bytes — the same bytes [`world_hash`] consumes — so two
///     same-length garbage snapshots still diff as distinct items (a
///     length-only representation could not).
///
/// Uses the SAME inputs and per-item reductions as [`world_hash`], so "the
/// hash moved" and "some item differs" stay in agreement for every item kind,
/// including the unreadable/non-JSON edge cases.
///
/// Test-only in practice since the prompt learned to take `Sensed.items` as a
/// parameter (the one-pass invariant now runs through the prompt boundary, so
/// no production caller re-derives the items) — kept for tests that simulate
/// the sense half of a beat.
#[cfg_attr(not(test), allow(dead_code))]
pub fn world_items(paths: &Paths) -> std::collections::BTreeMap<String, String> {
    world_view(paths).1
}

/// The world hash AND the named items, derived from ONE pass over the world:
/// every input file is read exactly once and BOTH views are built from those
/// same bytes. [`crate::tick::sense`] promises (see `Sensed`) that "the hash
/// moved" and "some item differs" describe the SAME observation — computing
/// them as two independent passes ([`world_hash`] then [`world_items`]) left a
/// window where a snapshot rewritten between the passes made hash and items
/// disagree. The single-view wrappers remain for callers that need only one
/// half and don't pair the two.
pub fn world_view(paths: &Paths) -> (String, std::collections::BTreeMap<String, String>) {
    let mut buf: Vec<u8> = Vec::new();
    let mut items = std::collections::BTreeMap::new();

    // PLAYBOOK + goals/*.md: one read per file feeds both the hash buffer and
    // the item map (readable → content hash, present-but-unreadable → the
    // sentinel — never silently skipped).
    hash_policy_into(paths, &mut buf, Some(&mut items));

    // Sensor snapshots: hash only the wake SIGNAL. User sensors AND the virtual
    // system sensors (sys-sessions / sys-claims) all land here, so the fleet and
    // leases are diffed through this one loop — no bespoke per-kind hashing.
    for f in util::sorted_glob(&paths.snapshots_dir(), "json") {
        let Some(stem) = f.file_stem().map(|s| s.to_string_lossy().to_string()) else {
            continue;
        };
        let raw = fs::read(&f).unwrap_or_default();
        // Both the hash payload and the item value derive from this ONE read.
        // Both hash branches end with a newline (consistent framing), and the
        // section marker is length-prefixed like the policy half (see
        // hash_policy_into) — injective even when a payload contains "@@ ".
        // We write straight into `buf` (length = payload.len()+1 for the
        // trailing newline) and move the item value into the map separately,
        // avoiding a clone of the payload bytes (or the serialized signal).
        match serde_json::from_slice::<serde_json::Value>(&raw) {
            Ok(v) => {
                let s = wake_signal(v).to_string();
                let len = s.len() + 1; // +1 for the trailing newline
                buf.extend_from_slice(format!("@@ {} {}\n", rel(paths, &f), len).as_bytes());
                buf.extend_from_slice(s.as_bytes());
                buf.push(b'\n');
                items.insert(format!("snap:{stem}"), s);
            }
            // Item: content hash, not just the length — world_hash consumes
            // the raw bytes, so the item must track the CONTENT too or
            // same-length garbage would show "hash moved" with a useless
            // no-op diff.
            Err(_) => {
                let len = raw.len() + 1; // +1 for the trailing newline
                buf.extend_from_slice(format!("@@ {} {}\n", rel(paths, &f), len).as_bytes());
                buf.extend_from_slice(&raw);
                buf.push(b'\n');
                let item = format!(
                    "(non-JSON, {} bytes, fnv {})",
                    raw.len(),
                    util::content_hash(&raw)
                );
                items.insert(format!("snap:{stem}"), item);
            }
        };
    }

    (util::content_hash(&buf), items)
}

pub fn world_hash(paths: &Paths) -> String {
    world_view(paths).0
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
    fn world_items_distinguish_same_length_non_json_snapshots() {
        // world_hash consumes a non-JSON snapshot's raw bytes; the item view
        // must move with the CONTENT too, not just the length — otherwise the
        // hash says "changed" while WHAT CHANGED shows an identical value.
        let p = Paths::temp();
        fs::create_dir_all(p.snapshots_dir()).unwrap();
        let f = p.snapshots_dir().join("sensor-raw.json");

        fs::write(&f, b"not json AAAA").unwrap();
        let a = world_items(&p).get("snap:sensor-raw").cloned().unwrap();
        fs::write(&f, b"not json BBBB").unwrap();
        let b = world_items(&p).get("snap:sensor-raw").cloned().unwrap();
        assert_ne!(a, b, "same-length different-content must differ");

        // And identical content stays stable.
        fs::write(&f, b"not json AAAA").unwrap();
        let a2 = world_items(&p).get("snap:sensor-raw").cloned().unwrap();
        assert_eq!(a, a2);
    }

    #[test]
    fn world_view_agrees_with_the_single_fn_views() {
        // Regression for the two-pass Sensed race: world_view must produce
        // exactly the hash world_hash reports and the items world_items
        // reports (on a quiescent world the three are interchangeable — the
        // point of world_view is that hash AND items come from ONE read).
        let p = Paths::temp();
        fs::create_dir_all(p.goals_dir()).unwrap();
        fs::create_dir_all(p.snapshots_dir()).unwrap();
        fs::write(p.playbook(), b"rule one\n").unwrap();
        fs::write(p.goals_dir().join("a.md"), b"goal a\n").unwrap();
        fs::write(
            p.snapshots_dir().join("s.json"),
            br#"{"signal":{"n":1},"detail":{"ts":9}}"#,
        )
        .unwrap();
        fs::write(p.snapshots_dir().join("raw.json"), b"not json").unwrap();

        let (hash, items) = world_view(&p);
        assert_eq!(hash, world_hash(&p));
        assert_eq!(items, world_items(&p));
        assert!(items.contains_key("playbook"));
        assert!(items.contains_key("goal:a"));
        assert!(items.contains_key("snap:s"));
        assert!(items.contains_key("snap:raw"));
    }

    #[cfg(unix)]
    #[test]
    fn unstatable_goal_reads_as_the_unreadable_sentinel_not_absence() {
        // Regression: `!f.is_file()` squashed stat errors (EACCES, EIO) into
        // "absent", silently dropping the file from hash AND items — a goal
        // flipping unreadable never moved the world. A goals dir with read
        // permission but NO search bit (0o444) makes the glob still list the
        // file while every stat/read of it fails — exactly the squashed case.
        use std::os::unix::fs::PermissionsExt;
        let p = Paths::temp();
        fs::create_dir_all(p.goals_dir()).unwrap();
        let goal = p.goals_dir().join("locked.md");
        fs::write(&goal, b"goal body\n").unwrap();
        fs::set_permissions(p.goals_dir(), fs::Permissions::from_mode(0o444)).unwrap();
        // Running as root (some CI containers) the kernel does not enforce
        // the missing search bit — nothing to assert then.
        let enforced = fs::metadata(&goal).is_err();
        let view = if enforced { Some(world_view(&p)) } else { None };
        // Restore BEFORE asserting so a failed assert can't strand an
        // unsearchable temp dir (Paths::temp's Drop cleanup needs it).
        fs::set_permissions(p.goals_dir(), fs::Permissions::from_mode(0o755)).unwrap();
        let Some((_, items)) = view else { return };
        assert_eq!(
            items.get("goal:locked").map(String::as_str),
            Some("!unreadable"),
            "a stat failure must surface as the sentinel, never as absence"
        );
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
