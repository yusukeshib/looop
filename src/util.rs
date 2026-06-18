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

/// Decide once whether to emit ANSI, mirroring the bash gate:
/// `$LOOOP_COLOR` wins; else a tty on stdout with no `$NO_COLOR`.
pub fn init_color() {
    let enabled = match std::env::var("LOOOP_COLOR") {
        Ok(v) if v == "1" => true,
        Ok(v) if v == "0" => false,
        _ => is_stdout_tty() && std::env::var_os("NO_COLOR").is_none(),
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

/// `[HH:MM:SS] <msg>` on stdout, the dim timestamp matching the bash `log()`.
pub fn log(msg: &str) {
    println!("{}[{}]{} {}", dim(), hms(), rst(), msg);
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
