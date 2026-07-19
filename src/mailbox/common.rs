//! Shared mailbox record mechanics: schema versioning (write-side stamp +
//! read-side forward-compat warning), process-deduplicated stderr warnings,
//! and collision-safe sequential id allocation.

use crate::store::{Collection, Key, StateStore};
use anyhow::{Result, bail};

/// The newest mailbox record schema this binary knows how to interpret.
/// Bump when a record's meaning changes; [`warn_future_v`] flags records
/// stamped by a NEWER binary on the read side.
const KNOWN_V: u64 = 1;

/// Schema version stamped into serialized mailbox records. Records written
/// before versioning carry no `v` and deserialize as v1 (serde ignores unknown
/// fields on read, so `v` is also transparently ACCEPTED on Ask, whose struct
/// deliberately carries no `v` field — other modules construct Ask literals).
pub(super) fn default_v() -> u32 {
    1
}

/// Stamp `"v": 1` into a serialized record body (see [`default_v`]).
///
/// TWO STAMPING STYLES exist on purpose: Tell (and schedule.rs's Schedule)
/// carry `v` as a struct field — they are constructed in exactly one place,
/// so the field is cheap. Ask is constructed as a literal by OTHER modules
/// (seed.rs plants the starter ask), so adding a `v` field would force every
/// literal — including ones this module doesn't own — to pick a version;
/// injecting the stamp into the serialized JSON here keeps the version an
/// implementation detail of the ONE write path instead. Unify to a struct
/// field only if Ask construction is ever centralized.
pub(super) fn stamp_v1(body: &str) -> Result<String> {
    let mut val: serde_json::Value = serde_json::from_str(body)?;
    if let Some(obj) = val.as_object_mut() {
        obj.insert("v".into(), serde_json::json!(1));
    }
    Ok(serde_json::to_string_pretty(&val)?)
}

/// The session/worker id a CLI verb should act as: the explicit argument
/// when non-empty, else the worker's exported `$LOOOP_SESSION_ID`. Empty when
/// neither is set — callers decide whether that is an error. Shared by the
/// claim verbs (gate.rs) and the worker self-callbacks (`ask`/`told`), which
/// each used to carry their own copy of this fallback — three implementations
/// of one rule invited them to drift apart.
pub(crate) fn session_or_env(explicit: Option<&str>) -> String {
    match explicit {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => std::env::var("LOOOP_SESSION_ID").unwrap_or_default(),
    }
}

/// Print `msg` to stderr the FIRST time `key` is seen in this process, and
/// report whether it printed. The mailbox read paths run every beat — and
/// `ask()` polls every second — so an unconditional warning about the same
/// broken record repeats hundreds of times (log spam that buries real
/// signals). One line per condition per process is enough: the condition is
/// durable (a corrupt file stays corrupt until a human acts).
///
/// The HashSet grows without an eviction path, but it is bounded in practice:
/// keys derive from live record ids (one entry per broken record per kind),
/// so its size tracks the mailbox dirs — which are themselves swept/archived.
/// Unbounded growth would need a pathological writer minting endless distinct
/// broken ids inside ONE process lifetime; not worth an LRU.
pub(super) fn warn_once(key: String, msg: &str) -> bool {
    use std::sync::{Mutex, OnceLock};
    static SEEN: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    let mut seen = SEEN
        .get_or_init(|| Mutex::new(std::collections::HashSet::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let first = seen.insert(key);
    if first {
        eprintln!("{msg}");
    }
    first
}

/// Forward-compat signal on the READ side: when a record carries `v` GREATER
/// than [`KNOWN_V`] it was written by a newer binary, and silently reading it
/// with this schema may misinterpret fields. One stderr line (per record per
/// process, via [`warn_once`]) makes that visible; the record is still read —
/// v1 fields remain the best available interpretation. Returns whether the
/// warning was emitted (first sighting of a future-v record); testable.
pub(super) fn warn_future_v(kind: &str, id: &str, raw: &str) -> bool {
    let v = serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|val| val.get("v").and_then(serde_json::Value::as_u64))
        .unwrap_or(1); // absent `v` ⇒ v1 (pre-versioning record)
    if v <= KNOWN_V {
        return false;
    }
    warn_once(
        format!("future-v:{kind}:{id}"),
        &format!(
            "{kind}/{id}.json carries schema v{v} but this binary knows v{KNOWN_V} — \
             written by a newer looop; fields may be misread"
        ),
    )
}

/// Allocate the next sequential id for a worker: `<worker>-<n>` where `n` is
/// one past the highest existing index across `collections`. Shared by asks
/// (scans asks/ AND answers/, so an answered ask's id is never reused while its
/// record lingers) and tells (scans tells/ only). The scan-max+1 is inherently
/// racy across processes — callers must WRITE the record via `create_exclusive`
/// and re-scan on collision (see [`write_new_record`]).
pub(super) fn next_seq_id(
    store: &impl StateStore,
    collections: &[Collection],
    worker: &str,
) -> String {
    let mut max = 0u64;
    for coll in collections {
        for stem in store.list(coll) {
            if let Some(idx) = stem.strip_prefix(&format!("{worker}-"))
                && let Ok(n) = idx.parse::<u64>()
            {
                max = max.max(n);
            }
        }
    }
    format!("{worker}-{}", max + 1)
}

/// Allocate an id and durably create the record for it, retrying on collision:
/// scan-max+1 then EXCLUSIVE-create — when two issuers race to the same id,
/// exactly one create wins and the loser re-scans. `make` builds the record
/// body for a candidate id; `key` maps the id to its store key. Bounded (~20
/// attempts) so pathological contention errors out instead of spinning.
pub(super) fn write_new_record(
    store: &impl StateStore,
    collections: &[Collection],
    worker: &str,
    key: impl Fn(String) -> Key,
    make: impl Fn(&str) -> Result<String>,
) -> Result<String> {
    for _ in 0..20 {
        let id = next_seq_id(store, collections, worker);
        let body = make(&id)?;
        if store.create_exclusive(&key(id.clone()), &body)? {
            return Ok(id);
        }
        // Collision: another issuer took this id first — re-scan and retry.
    }
    bail!("mailbox: could not allocate an id for {worker:?} after 20 attempts (contention)")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Paths;
    use crate::store::FileStore;
    use std::fs;

    #[test]
    fn write_new_record_rescans_on_collision() {
        let p = Paths::temp();
        let store = FileStore::new(&p);
        fs::create_dir_all(p.asks_dir()).unwrap();
        // Simulate a racer: the FIRST time the body is built (i.e. after the
        // scan chose an id, before our exclusive create), another issuer lands
        // the same id. Our create must lose, re-scan, and take the next id.
        let raced = std::cell::Cell::new(false);
        let id = write_new_record(
            &store,
            &[Collection::Asks, Collection::Answers],
            "w",
            Key::Ask,
            |id| {
                if !raced.replace(true) {
                    fs::write(p.asks_dir().join(format!("{id}.json")), "{\"racer\":1}").unwrap();
                }
                Ok(format!("{{\"mine\":\"{id}\"}}"))
            },
        )
        .unwrap();
        assert_eq!(id, "w-2", "loser re-scans past the racer's id");
        assert_eq!(
            fs::read_to_string(p.asks_dir().join("w-1.json")).unwrap(),
            "{\"racer\":1}",
            "the racer's record is never overwritten"
        );
        assert!(p.asks_dir().join("w-2.json").is_file());
    }

    #[test]
    fn warn_once_prints_once_per_key_per_process() {
        // Regression for log spam: read_answer/pending warnings used to
        // repeat every beat / every poll second for the same broken record.
        assert!(warn_once("test-dedup-key-1".into(), "first sighting"));
        assert!(
            !warn_once("test-dedup-key-1".into(), "repeat"),
            "the same key must not warn twice in one process"
        );
        assert!(
            warn_once("test-dedup-key-2".into(), "different key"),
            "deduplication is per key, not global"
        );
    }

    #[test]
    fn future_schema_versions_warn_once_and_known_ones_stay_silent() {
        // v ≤ KNOWN_V (or absent ⇒ v1) is silent; v > KNOWN_V warns exactly
        // once per record per process — the forward-compat signal.
        assert!(!warn_future_v("asks", "fv-a", r#"{"v":1}"#));
        assert!(!warn_future_v(
            "asks",
            "fv-b",
            r#"{"prompt":"pre-versioning"}"#
        ));
        assert!(!warn_future_v("asks", "fv-c", "not json at all"));
        assert!(warn_future_v("asks", "fv-d", r#"{"v":2}"#));
        assert!(
            !warn_future_v("asks", "fv-d", r#"{"v":2}"#),
            "the same future-v record warns only once"
        );
    }
}
