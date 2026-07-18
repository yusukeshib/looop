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
    // One write(2) for the whole line: `writeln!` can split the line and the
    // trailing newline into separate writes, which lets a concurrent appender
    // interleave mid-line. O_APPEND + a single write_all keeps each event line
    // intact.
    let mut line = serde_json::Value::Object(obj).to_string();
    line.push('\n');
    let path = paths.data_dir.join("events.jsonl");
    // Size-based rotation: past `LOOOP_EVENTS_MAX_BYTES` (default 5 MiB) the
    // current file rolls to `events.jsonl.1` (replacing any previous .1) before
    // this append, so the live file stays bounded at ~one generation.
    //
    // SINGLE-WRITER ASSUMPTION: the size-check + rename below is not atomic —
    // two concurrent emitters could both pass the check and the second rename
    // would clobber the freshly rotated .1 generation. Today the only writer
    // is the single locked pulse process (one beat at a time), so the race
    // cannot fire; if emit() ever gains concurrent callers, add locking here.
    let max_bytes: u64 = crate::util::env_knob("LOOOP_EVENTS_MAX_BYTES").unwrap_or(5 * 1024 * 1024);
    if max_bytes > 0 && std::fs::metadata(&path).is_ok_and(|m| m.len() > max_bytes) {
        let _ = std::fs::rename(&path, paths.data_dir.join("events.jsonl.1"));
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(line.as_bytes());
    }
}
