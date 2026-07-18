//! The structured event stream: color decision, level tags, the one `event`
//! logging primitive (human-pretty or NDJSON), and the env-knob reader that
//! warns on typos instead of silently ignoring them.

use std::sync::OnceLock;

static COLOR: OnceLock<bool> = OnceLock::new();
static JSON: OnceLock<bool> = OnceLock::new();

/// Decide once whether the loop's own log lines are emitted as NDJSON (one
/// structured object per line) instead of the human-pretty `[HH:MM:SS] …` form.
/// Driven by `$LOOOP_LOG_FORMAT=json`. Exported so the detached pulse worker and
/// any child inherit the decision (so a watcher of the pulse log sees a clean stream).
pub fn init_format() {
    let json = matches!(std::env::var("LOOOP_LOG_FORMAT").as_deref(), Ok("json"));
    let _ = JSON.set(json);
    unsafe { std::env::set_var("LOOOP_LOG_FORMAT", if json { "json" } else { "human" }) };
}

/// True when log lines should be NDJSON rather than human-pretty text.
pub fn is_json() -> bool {
    *JSON.get().unwrap_or(&false)
}

/// Decide once whether to emit ANSI: a tty on stdout with no `$NO_COLOR`, and
/// never in JSON mode (the machine stream stays free of escapes).
///
/// Each looop process decides from its OWN stdout — there is NO inherited
/// override. looop re-execs itself (the detached pulse supervisor, worker
/// self-callbacks), and a previous design exported the computed decision so the
/// tree shared one choice. That backfired: the detached supervisor runs with
/// stdout=/dev/null, so it computed "no color" and pushed that down onto the
/// PTY-backed pulse below it, leaving the pulse log uncolored. Self-detection
/// fixes it structurally — the pulse sees its real PTY and colors correctly;
/// sensors write JSON to files (never colored); workers are agents under their
/// own PTY (they self-color). `NO_COLOR` is the one honored opt-out.
pub fn init_color() {
    let enabled = !is_json() && super::is_stdout_tty() && std::env::var_os("NO_COLOR").is_none();
    let _ = COLOR.set(enabled);
}

/// Whether ANSI is on for this process — shared with the sibling `term` module
/// (spinner/countdown/clipping are no-ops or plain when color is off).
pub(super) fn color_on() -> bool {
    *COLOR.get().unwrap_or(&false)
}

macro_rules! code {
    ($name:ident, $seq:expr) => {
        pub fn $name() -> &'static str {
            if color_on() { $seq } else { "" }
        }
    };
}
code!(rst, "\x1b[0m");
code!(dim, "\x1b[2m");
code!(b, "\x1b[1m");
code!(cyan, "\x1b[36m");
code!(red, "\x1b[31m");
code!(yel, "\x1b[33m");
code!(wht, "\x1b[97m");

/// Severity of a structured log line — picks the human color and rides along as
/// the `level` field in JSON mode.
#[derive(Clone, Copy)]
pub enum Level {
    /// Neutral progress / context.
    Info,
    /// A step of the beat is starting (cyan).
    Step,
    /// Success / a decision (bright white).
    Ok,
    /// Non-fatal caution (yellow).
    Warn,
    /// Failure (red).
    Error,
}

impl Level {
    fn tag(self) -> &'static str {
        match self {
            Level::Info => "info",
            Level::Step => "step",
            Level::Ok => "ok",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }
    fn color(self) -> &'static str {
        match self {
            Level::Info => "",
            Level::Step => cyan(),
            Level::Ok => wht(),
            Level::Warn => yel(),
            Level::Error => red(),
        }
    }
}

/// The one structured log primitive the pulse uses. Human mode prints a single
/// concise line `[HH:MM:SS] <msg>` with the message tinted by level. JSON mode
/// prints one NDJSON object `{ts,level,event,msg,...fields}` — the same shape an
/// agent tailing the pulse log can parse line-by-line. `fields` carry the
/// machine-useful extras (runner, secs, run_id, journal, …).
///
/// STDOUT INTERLEAVING INVARIANT: `event` writes whole lines to stdout with no
/// coordination against [`super::Spinner`]'s repaints. The callers uphold the
/// rule that no events are emitted while a spinner is live — a spinner wraps
/// exactly one otherwise-silent step (the tick runner wait), and is dropped
/// (erasing its line) before the step's outcome event prints. Emitting an event
/// mid-spinner would splice it into the repaint line; if that ever becomes a
/// need, take a shared stdout lock in both paths instead.
pub fn event(level: Level, event: &str, msg: &str, fields: &[(&str, serde_json::Value)]) {
    if is_json() {
        let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        println!("{}", json_event_line(&ts, level, event, msg, fields));
        return;
    }
    // Human mode is a *rendering* of the structured event, not a dump of it.
    // Color encodes IMPORTANCE (no glyphs): the MESSAGE itself is tinted by
    // level, so decisions/failures pop and the heartbeat (sense summary, sleep,
    // skip, cadence) recedes. The machine `event` name is intentionally omitted
    // for a human — it lives in the JSON stream.
    if matches!(level, Level::Info | Level::Step) {
        // Heartbeat & transient "starting" steps: the whole line is dim so it
        // sits quietly in the background and lets the OUTCOME stand out.
        println!("{}[{}] {}{}", dim(), super::hms(), msg, rst());
        return;
    }
    // Outcomes (ok / warn / error): dim timestamp, then the message tinted by
    // the level color (no bold) so it carries the importance the glyph used to.
    let c = level.color();
    println!("{}[{}]{} {}{}{}", dim(), super::hms(), rst(), c, msg, rst());
}

/// Build one NDJSON object line for a structured event. Always carries the
/// reserved keys `ts`, `level`, `event`, `msg` plus any caller `fields` (keys
/// are serialized in sorted order — serde_json's default Map). Pure + testable.
fn json_event_line(
    ts: &str,
    level: Level,
    event: &str,
    msg: &str,
    fields: &[(&str, serde_json::Value)],
) -> String {
    let mut obj = serde_json::Map::new();
    obj.insert("ts".into(), serde_json::Value::String(ts.into()));
    obj.insert(
        "level".into(),
        serde_json::Value::String(level.tag().into()),
    );
    obj.insert("event".into(), serde_json::Value::String(event.into()));
    obj.insert("msg".into(), serde_json::Value::String(msg.into()));
    for (k, v) in fields {
        obj.insert((*k).to_string(), v.clone());
    }
    serde_json::Value::Object(obj).to_string()
}

/// Read a numeric `LOOOP_*` tuning knob from the environment. `None` when the
/// variable is unset — the caller applies its default. An UNPARSEABLE value
/// also falls back to the default, but WARNS first: every knob used to be read
/// ad hoc with `.parse().ok()`, so a typo like `LOOOP_NOOP_TTL=6h` silently
/// became the default and the operator never learned their override was dead.
/// This is the ONE place env knobs are parsed — new knobs must go through it.
pub fn env_knob<T: std::str::FromStr>(name: &str) -> Option<T> {
    let raw = std::env::var(name).ok()?;
    match raw.trim().parse::<T>() {
        Ok(v) => Some(v),
        Err(_) => {
            // Once per key per process: env_knob is called on EVERY beat (poll
            // intervals, size caps, TTLs), so an unconditional warning about
            // the same dead override repeats forever — log spam that buries
            // real signals. The bad value is durable for the process's whole
            // life (it was set at spawn), so one line is the whole signal.
            if warn_knob_once(name) {
                event(
                    Level::Warn,
                    "env.invalid",
                    &format!("ignoring {name}={raw:?} (not a valid number) — using the default"),
                    &[("var", serde_json::json!(name))],
                );
            }
            None
        }
    }
}

/// True the FIRST time `name` is seen by this process — the dedup gate behind
/// [`env_knob`]'s invalid-value warning. Separate + return-value-based so the
/// once-only contract is directly testable without capturing stdout.
fn warn_knob_once(name: &str) -> bool {
    use std::sync::Mutex;
    static SEEN: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    SEEN.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_event_line_is_valid_and_ordered() {
        let line = json_event_line(
            "2026-01-02T03:04:05Z",
            Level::Ok,
            "tick.decided",
            "decided in 3s",
            &[
                ("secs", serde_json::json!(3)),
                ("runner", serde_json::json!("claude")),
            ],
        );
        // Parses back to the expected object.
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["ts"], "2026-01-02T03:04:05Z");
        assert_eq!(v["level"], "ok");
        assert_eq!(v["event"], "tick.decided");
        assert_eq!(v["msg"], "decided in 3s");
        assert_eq!(v["secs"], 3);
        assert_eq!(v["runner"], "claude");
    }

    #[test]
    fn invalid_env_knob_warns_once_per_key_per_process() {
        // Regression for log spam: the invalid-value warning used to fire on
        // every env_knob call — i.e. every beat — for the same dead override.
        assert!(warn_knob_once("LOOOP_TEST_KNOB_A"), "first sighting warns");
        assert!(
            !warn_knob_once("LOOOP_TEST_KNOB_A"),
            "the same key must not warn again in this process"
        );
        assert!(
            warn_knob_once("LOOOP_TEST_KNOB_B"),
            "deduplication is per key, not global"
        );
    }

    #[test]
    fn level_tags_are_stable() {
        assert_eq!(Level::Info.tag(), "info");
        assert_eq!(Level::Step.tag(), "step");
        assert_eq!(Level::Ok.tag(), "ok");
        assert_eq!(Level::Warn.tag(), "warn");
        assert_eq!(Level::Error.tag(), "error");
    }
}
