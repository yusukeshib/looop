//! Cross-cutting helpers: colors, timestamps, logging, content hashing.
//!
//! RULE 2 — the pulse is unbreakable code; these are the small deterministic
//! primitives it leans on. Everything here is pure in-process Rust — timestamps
//! and TZ via chrono, hashing via FNV-1a, liveness via a direct `kill(pid, 0)`
//! syscall — so the pulse never depends on `date`/`shasum`/`kill` being on PATH.

use std::path::{Path, PathBuf};
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
    let enabled = !is_json() && is_stdout_tty() && std::env::var_os("NO_COLOR").is_none();
    let _ = COLOR.set(enabled);
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

/// Terminal width (columns) of stdout, if stdout is a tty. Used by
/// `worker list --watch` to clip rows so they never wrap — wrapping breaks the
/// cursor-up-N in-place repaint arithmetic (the residue piles up as repeated
/// header lines in scrollback).
pub fn term_cols() -> Option<usize> {
    if !is_stdout_tty() {
        return None;
    }
    ratatui::crossterm::terminal::size()
        .ok()
        .map(|(cols, _rows)| cols as usize)
}

/// Clip `s` to at most `max` visible columns, treating ANSI escape sequences
/// as zero-width (they are copied through, never split). If the cut happens
/// after any escape was emitted, a reset is appended so a clipped colored cell
/// can't bleed its color into the rest of the screen. Assumes 1 column per
/// char (our tables are ASCII).
pub fn clip_ansi(s: &str, max: usize) -> String {
    let mut out = String::with_capacity(s.len());
    let mut width = 0usize;
    let mut saw_esc = false;
    let mut truncated = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            saw_esc = true;
            out.push(c);
            // Copy the CSI sequence through to its final byte (a letter).
            for c2 in chars.by_ref() {
                out.push(c2);
                if c2.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        if width >= max {
            truncated = true;
            break;
        }
        out.push(c);
        width += 1;
    }
    if truncated && saw_esc {
        out.push_str("\x1b[0m");
    }
    out
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
        println!("{}[{}] {}{}", dim(), hms(), msg, rst());
        return;
    }
    // Outcomes (ok / warn / error): dim timestamp, then the message tinted by
    // the level color (no bold) so it carries the importance the glyph used to.
    let c = level.color();
    println!("{}[{}]{} {}{}{}", dim(), hms(), rst(), c, msg, rst());
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

/// Wall-clock seconds since the Unix epoch (0 if the clock is before it).
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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

/// A process-wide monotonic nonce for temp-file names. `now_unix()` alone is
/// second-precision, so two atomic writes to the SAME target within one second
/// (easy under test or a busy mailbox) could collide on the temp name; the
/// counter makes every temp name unique within the process, and the pid keeps
/// processes apart.
pub fn temp_nonce() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

/// fsync the DIRECTORY containing `path`, so the rename that just landed in it
/// is durable (a crash after rename can otherwise lose the directory entry).
/// Unix-only (opening a directory read-only works there); a failure is ignored
/// by callers that treat durability as best-effort.
#[cfg(unix)]
fn sync_parent_dir(path: &std::path::Path) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::File::open(dir)?.sync_all()?;
    }
    Ok(())
}
#[cfg(not(unix))]
fn sync_parent_dir(_path: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

/// Atomically write `contents` to `path`: write a sibling temp file, fsync, then
/// `rename` over the target. `rename(2)` on the same filesystem is atomic, so a
/// concurrent reader (the pulse re-sensing each beat) never sees a half-written
/// goal/PLAYBOOK/sensor — it sees either the old bytes or the new, never a torn
/// truncation. This is what lets the contract's STEER verbs promise atomic
/// writes that a raw `fs::write` (truncate-then-write) cannot. After the rename
/// the parent directory is fsync'd too, so the new entry survives a crash.
pub fn write_atomic(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    write_atomic_mode(path, contents, None)
}

/// [`write_atomic`] with an optional unix permission mode applied to the TEMP
/// file BEFORE the rename, so the target is never observable with the wrong
/// mode (e.g. a sensor script must never be visible non-executable).
pub fn write_atomic_mode(
    path: &std::path::Path,
    contents: &[u8],
    #[cfg_attr(not(unix), allow(unused_variables))] mode: Option<u32>,
) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    std::fs::create_dir_all(dir)?;
    // Unique temp name in the SAME dir (so rename stays on one filesystem).
    // pid + second + process-wide counter: unique across processes AND within
    // the same second in one process.
    let pid = std::process::id();
    let stem = path.file_name().and_then(|s| s.to_str()).unwrap_or("tmp");
    let tmp = dir.join(format!(".{stem}.{pid}.{}.{}.tmp", now_unix(), temp_nonce()));
    let res = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(contents)?;
        f.sync_all()?;
        #[cfg(unix)]
        if let Some(mode) = mode {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
        }
        std::fs::rename(&tmp, path)?;
        // Durability of the rename itself: fsync the parent dir so the entry
        // survives a crash. Best-effort — the data bytes are already synced.
        let _ = sync_parent_dir(path);
        Ok(())
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

/// Reject a file-name segment that could escape its directory or hit a dotfile.
/// The SINGLE source of truth for this security-relevant check — the mailbox
/// (ask ids), the executor (goal/sensor ids) and the gate (claim names) all
/// route here so the guard can never drift between call sites. `kind` names the
/// segment for the error (e.g. "ask id", "claim name", "goal id").
pub fn safe_segment(kind: &str, seg: &str) -> anyhow::Result<()> {
    if seg.is_empty()
        || seg.contains('/')
        || seg.contains('\\')
        || seg.starts_with('.')
        || seg == ".."
        || seg.chars().any(char::is_whitespace)
    {
        anyhow::bail!("invalid {kind} {seg:?}");
    }
    Ok(())
}

/// Sorted absolute paths of `dir/*.<ext>` (best-effort: an unreadable dir yields
/// an empty list). Sorting makes any derived hash / prompt order-stable.
pub fn sorted_glob(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == ext).unwrap_or(false))
        .collect();
    v.sort();
    v
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

/// A lightweight, in-place "something is happening" indicator for the pulse's
/// PTY stdout while a long, otherwise-silent step runs. The tick runner can take
/// minutes and its chatter is teed to the replay archive (NOT echoed live, to
/// keep the pulse a clean structured-event log) — so without this the stream
/// goes quiet between `… is deciding the one move` and the outcome line.
///
/// Repaints ONE line every second via `\r` (`[HH:MM:SS] label elapsed`), then
/// erases it on drop so the next structured event prints clean. It is a
/// no-op unless color (ANSI) is enabled: JSON mode and `NO_COLOR` streams stay
/// byte-clean, and a non-PTY consumer never sees stray carriage returns.
pub struct Spinner {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Spinner {
    /// Start the indicator (no-op when color is off). `label` is a short verb
    /// phrase, e.g. `"pi is deciding"`.
    pub fn start(label: &str) -> Self {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let stop = Arc::new(AtomicBool::new(false));
        let handle = if color_on() {
            let stop = stop.clone();
            let label = label.to_string();
            // Freeze the start timestamp so the line reads like a normal log
            // line (`[HH:MM:SS] <label> <elapsed>s`) — no spinner glyph; only
            // the elapsed counter advances.
            let ts = hms();
            Some(std::thread::spawn(move || {
                let t0 = std::time::Instant::now();
                // Repaint about once a second so the elapsed counter advances
                // visibly while keeping the PTY transcript small (~one short
                // line/sec). Poll `stop` in 100ms steps so drop() is responsive.
                while !stop.load(Ordering::Relaxed) {
                    let secs = t0.elapsed().as_secs();
                    print!("\r{}[{ts}] {label} {secs}s{}", dim(), rst());
                    let _ = std::io::Write::flush(&mut std::io::stdout());
                    for _ in 0..10 {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                }
            }))
        } else {
            None
        };
        Spinner { stop, handle }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
            // Erase the spinner line (CR + clear-to-end-of-line) so the next
            // structured event prints on a clean line.
            print!("\r\x1b[2K");
            let _ = std::io::Write::flush(&mut std::io::stdout());
        }
    }
}

/// Sleep `secs`, showing a live one-line COUNTDOWN on the pulse's PTY stdout
/// (`[HH:MM:SS] next beat in Ns (<suffix>)`) that repaints each second and is
/// erased when it reaches zero, so the next beat prints clean — the idle-wait
/// counterpart of [`Spinner`]. A no-op decoration unless color (ANSI) is on:
/// JSON / `NO_COLOR` / non-PTY streams just sleep silently (their structured
/// `sleep` event is emitted separately), never seeing stray carriage returns.
pub fn sleep_countdown(secs: u64, suffix: &str) {
    if !color_on() {
        std::thread::sleep(std::time::Duration::from_secs(secs));
        return;
    }
    // Freeze the timestamp at the start (like the spinner) so the line reads as
    // "the beat logged at [ts], next one in Ns".
    let ts = hms();
    for remaining in (1..=secs).rev() {
        // CR + clear-to-EOL so a shrinking count (60s → 9s) leaves no stale digit.
        print!(
            "\r\x1b[2K{}[{ts}] next beat in {remaining}s ({suffix}){}",
            dim(),
            rst()
        );
        let _ = std::io::Write::flush(&mut std::io::stdout());
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    print!("\r\x1b[2K");
    let _ = std::io::Write::flush(&mut std::io::stdout());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_ansi_counts_only_visible_columns() {
        // Plain text: clipped at the column budget.
        assert_eq!(clip_ansi("hello world", 5), "hello");
        // Shorter than the budget: untouched.
        assert_eq!(clip_ansi("hi", 5), "hi");
        // Escapes are zero-width and copied through whole.
        assert_eq!(
            clip_ansi("\x1b[31mred\x1b[0m ok", 6),
            "\x1b[31mred\x1b[0m ok"
        );
        // A cut after a color start appends a reset so color can't bleed.
        assert_eq!(clip_ansi("\x1b[31mredredred", 3), "\x1b[31mred\x1b[0m");
    }

    #[test]
    fn write_atomic_replaces_existing_and_leaves_no_temp() {
        let dir =
            std::env::temp_dir().join(format!("looop-wa-{}-{}", std::process::id(), now_unix()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("sub").join("goal.md");
        // Writes through a not-yet-existing parent dir.
        write_atomic(&target, b"first").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "first");
        // Overwrites in place.
        write_atomic(&target, b"second").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "second");
        // No leftover temp siblings.
        let leftovers: Vec<_> = std::fs::read_dir(target.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp file left behind: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

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
}
