//! Cross-cutting helpers: colors, timestamps, logging, content hashing.
//!
//! RULE 2 — the pulse is unbreakable code; these are the small deterministic
//! primitives it leans on. Everything here is pure in-process Rust — timestamps
//! and TZ via chrono, hashing via FNV-1a, liveness via a direct `kill(pid, 0)`
//! syscall — so the pulse never depends on `date`/`shasum`/`kill` being on PATH.
//!
//! Split by concern, re-exported flat (callers keep saying `util::X`):
//!   * [`term`] — terminal rendering: widths, ANSI clipping, spinner/countdown.
//!   * [`fsio`] — durable file I/O: atomic writes, flock, globbing.
//!   * [`log`]  — the structured event stream: colors, levels, JSON mode.
//!   * this module — the remaining small pure primitives (time, hashing,
//!     path-segment safety, PATH lookup, process-group kill).

mod fsio;
mod log;
mod term;

pub(crate) use fsio::flock_file;
pub use fsio::{sorted_glob, temp_nonce, write_atomic, write_atomic_mode};
// NB: `cyan`/`wht` exist in `log` too but are only consumed internally (by
// `Level::color`) — re-exporting them would trip unused_imports until a
// caller appears.
pub use log::{Level, b, dim, env_knob, event, init_color, init_format, is_json, red, rst, yel};
pub use term::{Spinner, clip_ansi, term_cols};
pub(crate) use term::{char_cols, display_width};

use std::path::Path;

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
        .map_or(0, |d| d.as_secs())
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
///
/// NB: FNV-1a is NOT cryptographic — collisions against it are CONSTRUCTIBLE
/// by anyone who controls the input bytes. And the inputs are not purely the
/// operator's own: sensors routinely ingest EXTERNAL data (GitHub issue
/// bodies, inbound email, API responses) into their signals, so an adversary
/// can in principle craft a colliding world. The trade is still acceptable
/// because the blast radius is tiny — a collision's worst case is one wrongly
/// skipped beat, which the noop TTL revisit later repairs. Do not reuse this
/// hash anywhere integrity against hostile input actually matters.
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
        // Control chars (NUL, ESC, …) are not traversal risks but poison file
        // names, log lines, and prompt interpolation (a `\x1b` in a goal id
        // could smuggle terminal escapes into `worker list` output).
        || seg.chars().any(char::is_control)
    {
        anyhow::bail!("invalid {kind} {seg:?}");
    }
    Ok(())
}

/// POSIX single-quote `s` so a shell reproduces the exact original bytes as
/// ONE word (close-quote, escaped quote, reopen for embedded `'`). The SINGLE
/// shared implementation — the tick runner, the worker session spawn, and the
/// `looop run` verb all route here so the quoting rule can never drift
/// between call sites.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// `command -v <cmd>` — true if found and executable on $PATH. A command
/// containing '/' bypasses PATH lookup entirely (shell semantics): it is
/// checked directly, resolved against the CWD when relative. Without this
/// explicit branch, absolute paths only worked by a `Path::join` replacement
/// quirk, and relative paths like `bin/claude` were wrongly joined onto every
/// PATH entry instead of the CWD.
pub fn on_path(cmd: &str) -> bool {
    if cmd.contains('/') {
        let p = Path::new(cmd);
        return p.is_file() && is_executable(p);
    }
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let p = dir.join(cmd);
        p.is_file() && is_executable(&p)
    })
}

/// SIGKILL an entire process GROUP by pgid (negative-pid `kill(2)`), libc-free
/// via the same extern-"C" technique as [`flock_file`]. The ONE shared group
/// killer: the sensor timeout and the verify/run_shell deadline both route
/// here, so neither depends on a `kill(1)` binary being on $PATH (RULE 2 — no
/// hidden PATH dependencies). Callers spawned the child with
/// `process_group(0)`, so its pid IS the pgid.
#[cfg(unix)]
pub(crate) fn kill_process_group(pgid: u32) {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    const SIGKILL: i32 = 9;
    unsafe {
        let _ = kill(-(pgid as i32), SIGKILL);
    }
}
#[cfg(not(unix))]
pub(crate) fn kill_process_group(_pgid: u32) {}

#[cfg(unix)]
fn is_executable(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}
#[cfg(not(unix))]
fn is_executable(_p: &std::path::Path) -> bool {
    true
}

/// Serializes tests that mutate process-global env vars (`std::env::set_var`
/// is unsafe under the default multi-threaded test harness: concurrent getenv
/// is UB on some platforms, and a leaked knob poisons sibling tests). Every
/// env-mutating test must hold this for its whole body and restore the var
/// before dropping the guard.
#[cfg(test)]
pub fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn on_path_handles_slash_commands_directly() {
        // Absolute path to a real executable: found without PATH scanning.
        assert!(on_path("/bin/sh"));
        // Absolute path to nothing: not found (and never PATH-joined).
        assert!(!on_path("/no/such/looop-binary"));
        // Relative path with '/': resolved against the CWD like a shell would,
        // not against PATH entries.
        assert!(!on_path("no-such-dir/looop-binary"));
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
