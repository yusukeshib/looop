//! Structured observability channel (layer 2): one NDJSON line per tick
//! lifecycle event, appended to `events.jsonl`. This is the MACHINE-facing
//! channel — an external AI (e.g. one watching the loop) tails it instead of
//! scraping the human-pretty stdout. stdout stays human; the two never tangle.
//!
//! File-as-channel keeps the "memory is files" property: events are replayable
//! and need no daemon or socket (those are layer 3, added only if this hurts).

use crate::paths::Paths;
use std::fs::OpenOptions;
use std::io::Write;

/// Append one event with a UTC timestamp and arbitrary structured fields.
/// Best-effort: observability must never fail a beat.
pub fn emit(paths: &Paths, event: &str, fields: serde_json::Value) {
    let mut obj = serde_json::Map::new();
    obj.insert(
        "ts".into(),
        serde_json::Value::String(chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()),
    );
    obj.insert("event".into(), serde_json::Value::String(event.to_string()));
    if let serde_json::Value::Object(extra) = fields {
        for (k, v) in extra {
            obj.insert(k, v);
        }
    }
    let line = serde_json::Value::Object(obj).to_string();
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.data_dir.join("events.jsonl"))
    {
        let _ = writeln!(f, "{line}");
    }
}
