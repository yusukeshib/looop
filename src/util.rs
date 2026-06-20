//! Cross-cutting helpers: colors, timestamps, logging, content hashing.
//!
//! RULE 2 — the pulse is unbreakable code; these are the small deterministic
//! primitives it leans on. Everything here is pure in-process Rust — timestamps
//! and TZ via chrono, hashing via FNV-1a, liveness via a direct `kill(pid, 0)`
//! syscall — so the pulse never depends on `date`/`shasum`/`kill` being on PATH.

use std::sync::OnceLock;

static COLOR: OnceLock<bool> = OnceLock::new();
static JSON: OnceLock<bool> = OnceLock::new();

/// Decide once whether the loop's own log lines are emitted as NDJSON (one
/// structured object per line) instead of the human-pretty `[HH:MM:SS] …` form.
/// Driven by `$LOOOP_LOG_FORMAT=json`. Exported so the detached pulse worker and
/// any child inherit the decision (so `looop watch pulse` sees a clean stream).
pub fn init_format() {
    let json = matches!(std::env::var("LOOOP_LOG_FORMAT").as_deref(), Ok("json"));
    let _ = JSON.set(json);
    unsafe { std::env::set_var("LOOOP_LOG_FORMAT", if json { "json" } else { "human" }) };
}

/// True when log lines should be NDJSON rather than human-pretty text.
pub fn is_json() -> bool {
    *JSON.get().unwrap_or(&false)
}

/// Decide once whether to emit ANSI, mirroring the bash gate:
/// `$LOOOP_COLOR` wins; else a tty on stdout with no `$NO_COLOR`.
/// JSON mode forces color OFF so the machine stream stays free of escapes.
pub fn init_color() {
    let enabled = if is_json() {
        false
    } else {
        match std::env::var("LOOOP_COLOR") {
            Ok(v) if v == "1" => true,
            Ok(v) if v == "0" => false,
            _ => is_stdout_tty() && std::env::var_os("NO_COLOR").is_none(),
        }
    };
    let _ = COLOR.set(enabled);
    // Export so children (the tick runner, sensors, workers) inherit the decision.
    unsafe { std::env::set_var("LOOOP_COLOR", if enabled { "1" } else { "0" }) };
}

fn color_on() -> bool {
    *COLOR.get().unwrap_or(&false)
}

#[cfg(unix)]
fn is_stdout_tty() -> bool {
    unsafe { libc_isatty(1) }
}
#[cfg(not(unix))]
fn is_stdout_tty() -> bool {
    false
}

#[cfg(unix)]
unsafe fn libc_isatty(fd: i32) -> bool {
    unsafe extern "C" {
        fn isatty(fd: i32) -> i32;
    }
    unsafe { isatty(fd) == 1 }
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
code!(grn, "\x1b[32m");
code!(red, "\x1b[31m");
code!(yel, "\x1b[33m");

/// Severity of a structured log line — picks the human color and rides along as
/// the `level` field in JSON mode.
#[derive(Clone, Copy)]
pub enum Level {
    /// Neutral progress / context.
    Info,
    /// A step of the beat is starting (cyan).
    Step,
    /// Success (green).
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
            Level::Ok => grn(),
            Level::Warn => yel(),
            Level::Error => red(),
        }
    }
    /// The human-facing sigil for this line. Carries the signal in human mode
    /// (the machine `event` name is JSON-only), so importance reads at a glance:
    /// `·` heartbeat, `→` a step starting, `✓` success/decision, `⚡`/`✗` trouble.
    fn glyph(self) -> &'static str {
        match self {
            Level::Info => "·",
            Level::Step => "→",
            Level::Ok => "✓",
            Level::Warn => "⚡",
            Level::Error => "✗",
        }
    }
}

/// The one structured log primitive the pulse uses. Human mode prints a single
/// concise, consistently-colored line:  `[HH:MM:SS] <event> — <msg>`. JSON mode
/// prints one NDJSON object `{ts,level,event,msg,...fields}` — the same shape an
/// agent watching `looop watch pulse` can parse line-by-line. `fields` carry the
/// machine-useful extras (runner, secs, run_id, journal, …).
pub fn event(level: Level, event: &str, msg: &str, fields: &[(&str, serde_json::Value)]) {
    if is_json() {
        let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        println!("{}", json_event_line(&ts, level, event, msg, fields));
        return;
    }
    // Human mode is a *rendering* of the structured event, not a dump of it.
    // Color encodes IMPORTANCE so the lines a watcher cares about (decisions,
    // failures, flags) pop and the heartbeat (sense summary, sleep, skip,
    // cadence) recedes. The machine `event` name is intentionally omitted — the
    // glyph + msg say it for a human; the name lives in the JSON stream.
    let glyph = level.glyph();
    if matches!(level, Level::Info | Level::Step) {
        // Heartbeat & transient "starting" steps: the whole line is dim so it
        // sits quietly in the background and lets the OUTCOME (✓/✗) stand out.
        // The glyph still differs (`·` vs `→`) so a step still reads as a step.
        println!("{}[{}] {} {}{}", dim(), hms(), glyph, msg, rst());
        return;
    }
    let c = level.color();
    let bold = if matches!(level, Level::Ok | Level::Error) {
        b()
    } else {
        ""
    };
    // Warnings/errors tint the whole message; success/step keep the body in the
    // default fg (a long journal line stays readable) and let the colored glyph
    // carry the signal.
    let msg_c = if matches!(level, Level::Warn | Level::Error) {
        c
    } else {
        ""
    };
    let msg_rst = if msg_c.is_empty() { "" } else { rst() };
    println!(
        "{}[{}]{} {}{}{}{} {}{}{}",
        dim(),
        hms(),
        rst(),
        bold,
        c,
        glyph,
        rst(),
        msg_c,
        msg,
        msg_rst
    );
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

/// Local wall-clock `HH:MM:SS` for log lines (chrono — fast, no subprocess).
pub fn hms() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

/// Local wall-clock formatted with a chrono strftime pattern. Used for the
/// TZ-sensitive strings embedded in the tick prompt. The bash version shelled
/// out to `date` to render `%Z` as a libc abbreviation ("EDT"); chrono renders
/// `%Z` on `Local` as the numeric offset ("-04:00") instead, which is
/// unambiguous for the AI reading the prompt and needs no subprocess or PATH
/// dependency. Format strings are controlled constants, so `format` never sees
/// an invalid specifier.
pub fn date_fmt(fmt: &str) -> String {
    chrono::Local::now().format(fmt).to_string()
}

/// Content hash for `world_hash` — deterministic FNV-1a (128-bit), computed
/// in-process. The bash version shelled out to `shasum`/`sha1sum`/`cksum`; the
/// port carried that over, which (a) made hashing an UNDECLARED dependency and
/// (b) silently returned an empty string when none of those tools was on $PATH,
/// which collapses `world_hash` to a constant so the pulse never wakes. A native
/// hash removes the subprocess, the hidden dependency, and that silent-stall
/// failure mode. Only requirement: stable across runs (it is — fixed constants),
/// so `.last-tick-hash` stays comparable beat to beat. The exact digest differs
/// from the old shell tools, so the first beat after upgrading sees one
/// (harmless) "world changed".
pub fn content_hash(input: &[u8]) -> String {
    // FNV-1a, 128-bit (offset basis + prime per the FNV spec).
    const OFFSET: u128 = 0x6c62272e07bb014262b821756295c58d;
    const PRIME: u128 = 0x0000000001000000000000000000013b;
    let mut h = OFFSET;
    for &b in input {
        h ^= b as u128;
        h = h.wrapping_mul(PRIME);
    }
    format!("{h:032x}")
}

/// `command -v <cmd>` — true if found and executable on $PATH.
pub fn on_path(cmd: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let p = dir.join(cmd);
        p.is_file() && is_executable(&p)
    })
}

#[cfg(unix)]
fn is_executable(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}
#[cfg(not(unix))]
fn is_executable(_p: &std::path::Path) -> bool {
    true
}

/// Is the process alive? A direct `kill(pid, 0)` syscall (no subprocess) — the
/// FFI shape mirrors `main::restore_sigpipe`'s raw `signal` declaration, so we
/// stay crate-free. `kill(pid, 0)` sends no signal but performs the existence +
/// permission check: it returns 0 when the process exists and is signalable,
/// matching the old `kill -0` exit status exactly (EPERM/ESRCH both → not 0 →
/// treated as not-alive, as the shell did).
#[cfg(unix)]
pub fn pid_alive(pid: &str) -> bool {
    let Ok(pid) = pid.trim().parse::<i32>() else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe { kill(pid, 0) == 0 }
}
#[cfg(not(unix))]
pub fn pid_alive(_pid: &str) -> bool {
    false
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
    fn level_tags_are_stable() {
        assert_eq!(Level::Info.tag(), "info");
        assert_eq!(Level::Step.tag(), "step");
        assert_eq!(Level::Ok.tag(), "ok");
        assert_eq!(Level::Warn.tag(), "warn");
        assert_eq!(Level::Error.tag(), "error");
    }

    #[test]
    fn level_glyphs_map_importance() {
        // Heartbeat recedes; decision and trouble each get a distinct sigil.
        assert_eq!(Level::Info.glyph(), "·");
        assert_eq!(Level::Step.glyph(), "→");
        assert_eq!(Level::Ok.glyph(), "✓");
        assert_eq!(Level::Warn.glyph(), "⚡");
        assert_eq!(Level::Error.glyph(), "✗");
    }

    #[test]
    fn content_hash_is_deterministic_and_change_sensitive() {
        // Stable across calls (so `.last-tick-hash` stays comparable).
        assert_eq!(content_hash(b"hello world"), content_hash(b"hello world"));
        // Distinct inputs hash differently.
        assert_ne!(content_hash(b"hello world"), content_hash(b"hello worle"));
        // 128-bit digest is rendered as 32 lowercase hex chars, never empty.
        let h = content_hash(b"");
        assert_eq!(h.len(), 32);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[cfg(unix)]
    #[test]
    fn pid_alive_detects_self_and_rejects_garbage() {
        // Our own process is always signalable by ourselves.
        assert!(pid_alive(&std::process::id().to_string()));
        assert!(!pid_alive("not-a-pid"));
        assert!(!pid_alive(""));
        assert!(!pid_alive("0"));
        assert!(!pid_alive("-1"));
    }
}
