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
    // Millisecond precision (RFC3339-compatible, so any consumer that parsed
    // the old second-precision form still parses this): events are appended by
    // CONCURRENT processes (pulse + CLI invocations), and at second precision
    // two near-simultaneous events sort non-deterministically for a tailing
    // reader. No in-repo consumer parses this field — it exists for external
    // watchers — so the widening is safe.
    obj.insert(
        "ts".into(),
        serde_json::Value::String(
            chrono::Utc::now()
                .format("%Y-%m-%dT%H:%M:%S%.3fZ")
                .to_string(),
        ),
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
    // current file rolls to `events.jsonl.1` (replacing any previous .1)
    // before this append, so the live file stays bounded at ~one generation.
    //
    // emit() IS reachable from concurrent processes (the pulse AND any CLI
    // invocation that journals / emits), so the check+rename must be
    // serialized. The mechanics — non-blocking flock on a sibling
    // `.<name>.rotlock` (previously `.events.rotlock` — a stale copy of the
    // old lock file is inert debris), best-effort skip on contention — are
    // shared with the journal cap in [`crate::store::rotate_at_cap`]; the
    // append below stays lock-free (O_APPEND + single write_all is already
    // interleave-safe).
    let max_bytes: u64 = crate::util::env_knob("LOOOP_EVENTS_MAX_BYTES").unwrap_or(5 * 1024 * 1024);
    crate::store::rotate_at_cap(&path, max_bytes);
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(line.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_lines(paths: &Paths) -> Vec<serde_json::Value> {
        std::fs::read_to_string(paths.data_dir.join("events.jsonl"))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).expect("every event line is valid JSON"))
            .collect()
    }

    #[test]
    fn emit_appends_one_parseable_ndjson_line_per_event() {
        // emit() READS `LOOOP_EVENTS_MAX_BYTES` (rotation knob), so this test
        // must serialize against the env-mutating rotation test below — a
        // leaked/mid-flight tiny cap would rotate the first line away.
        let _g = crate::util::test_env_lock();
        let p = Paths::temp();
        emit(
            &p,
            "tick.decided",
            serde_json::json!({"runner": "claude", "secs": 3}),
        );
        emit(&p, "claim_reaped", serde_json::json!({}));
        let lines = read_lines(&p);
        assert_eq!(lines.len(), 2, "one line per emit, newline-terminated");
        // Reserved keys ride on every event; caller fields are merged in.
        assert_eq!(lines[0]["event"], "tick.decided");
        assert_eq!(lines[0]["runner"], "claude");
        assert_eq!(lines[0]["secs"], 3);
        assert_eq!(lines[1]["event"], "claim_reaped");
        // ts is RFC3339 with millisecond precision (…T….mmmZ) — parseable by
        // the same consumers that read the old second-precision form.
        let ts = lines[0]["ts"].as_str().unwrap();
        assert!(
            chrono::DateTime::parse_from_rfc3339(ts).is_ok(),
            "ts must stay RFC3339-parseable, got {ts:?}"
        );
        assert!(ts.ends_with('Z') && ts.contains('.'), "got {ts:?}");
    }

    #[test]
    fn events_rotate_one_generation_at_the_size_cap() {
        // Mirrors store.rs's journal_rotates test: set_var is process-global,
        // so serialize against other env-mutating tests and restore the knob
        // even if an assert panics.
        let _g = crate::util::test_env_lock();
        let _r = crate::store::EnvRestore::set("LOOOP_EVENTS_MAX_BYTES", "64");
        let p = Paths::temp();
        for i in 0..8 {
            emit(
                &p,
                "padding",
                serde_json::json!({"i": i, "pad": "xxxxxxxxxxxxxxxx"}),
            );
        }
        let rotated = p.data_dir.join("events.jsonl.1");
        assert!(
            rotated.is_file(),
            "past the cap the events log must roll to events.jsonl.1"
        );
        let live = std::fs::read_to_string(p.data_dir.join("events.jsonl")).unwrap();
        assert!(
            live.len() <= 64 + 256,
            "the live events log stays bounded near the cap, got {} bytes",
            live.len()
        );
        // One-generation policy (matches journal.md): the newest event
        // survives across live + .1; the oldest generation is dropped.
        let all = format!("{}{live}", std::fs::read_to_string(&rotated).unwrap());
        assert!(all.contains("\"i\":7"), "the newest event must survive");
        assert!(
            !all.contains("\"i\":0"),
            "the oldest generation is dropped (one rotated generation kept)"
        );
    }
}
