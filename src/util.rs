//! Cross-cutting helpers: colors, timestamps, logging, content hashing.
//!
//! RULE 2 — the pulse is unbreakable code; these are the small deterministic
//! primitives it leans on. Timestamps that feed the AI prompt are taken from the
//! system `date` (parity with the bash version's TZ handling); everything else
//! uses chrono for speed.

use std::io::Write;
use std::process::{Command, Stdio};
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
    // Export so children (`looop _fmt`, sensors, workers) inherit the decision.
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

/// `[HH:MM:SS] <msg>` on stdout, the dim timestamp matching the bash `log()`.
/// In JSON mode it becomes a generic `{event:"log"}` object so legacy callers
/// still produce parseable output.
pub fn log(msg: &str) {
    if is_json() {
        event(Level::Info, "log", msg, &[]);
    } else {
        println!("{}[{}]{} {}", dim(), hms(), rst(), msg);
    }
}

/// Local wall-clock `HH:MM:SS` for log lines (chrono — fast, no subprocess).
pub fn hms() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

/// Run the system `date` with a `+`-format and return trimmed stdout. Used only
/// for the few TZ-sensitive strings embedded in the tick prompt, so they match
/// the bash version's libc formatting exactly (e.g. `%Z` => "EDT").
pub fn date_fmt(fmt: &str) -> String {
    Command::new("date")
        .arg(format!("+{fmt}"))
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim_end().to_string())
        .unwrap_or_default()
}

/// Portable content hash for `world_hash`: prefer `shasum`, then `sha1sum`,
/// then POSIX `cksum`. Feeds `input` on stdin and returns the first field of
/// the tool's output — byte-for-byte parity with the bash `_hash`.
pub fn content_hash(input: &[u8]) -> String {
    let tool = if on_path("shasum") {
        "shasum"
    } else if on_path("sha1sum") {
        "sha1sum"
    } else {
        "cksum"
    };
    let mut child = match Command::new(tool)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return String::new(),
    };
    if let Some(mut si) = child.stdin.take() {
        let _ = si.write_all(input);
    }
    let out = match child.wait_with_output() {
        Ok(o) => o,
        Err(_) => return String::new(),
    };
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_string()
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

/// `kill -0 <pid>` — is the process alive? Shelled out for portability parity.
pub fn pid_alive(pid: &str) -> bool {
    if pid.is_empty() {
        return false;
    }
    Command::new("kill")
        .args(["-0", pid])
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
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
}
