//! Thin wrapper over the `babysit` CLI — the worker fleet.
//!
//! Step 2 of the port still SHELLS OUT to the babysit binary, exactly like the
//! bash version. Step 3 will replace these calls with in-process lib calls once
//! babysit grows a `lib.rs`; keeping them behind this module makes that swap a
//! one-file change.

use serde::Deserialize;
use std::process::{Command, Stdio};

/// One row of `babysit ls --json`. Tolerant of missing fields (a starting
/// session may not have an exit code or note yet).
#[derive(Debug, Deserialize, Default)]
pub struct Session {
    pub id: String,
    // `cmd` is an array of argv strings in `babysit ls --json`; keep it as a raw
    // Value so a type mismatch can never fail the WHOLE list parse (it is not
    // consumed here anyway). Tolerance over precision for an external schema.
    #[serde(default)]
    #[allow(dead_code)]
    pub cmd: Option<serde_json::Value>,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub alive: bool,
    #[serde(default)]
    pub exit_code: Option<i64>,
    #[serde(default)]
    pub note: Option<String>,
}

impl Session {
    pub fn is_looop(&self) -> bool {
        self.id.starts_with("looop-")
    }
    /// True when the worker has raised a flag (a non-empty note).
    pub fn flagged(&self) -> bool {
        self.note.as_deref().map(|n| !n.is_empty()).unwrap_or(false)
    }
}

/// `babysit ls --json`, parsed. Any failure yields an empty list (matches the
/// bash `2>/dev/null || true`): the pulse degrades gracefully, never wedges.
pub fn list() -> Vec<Session> {
    let out = Command::new("babysit")
        .args(["ls", "--json"])
        .stderr(Stdio::null())
        .output();
    let Ok(out) = out else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    // Parse element-by-element so one malformed/extended row can't drop the whole
    // fleet (resilience to babysit schema drift).
    serde_json::from_slice::<Vec<serde_json::Value>>(&out.stdout)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| serde_json::from_value::<Session>(v).ok())
        .collect()
}

/// looop-owned sessions only.
pub fn list_looop() -> Vec<Session> {
    list().into_iter().filter(Session::is_looop).collect()
}

/// `babysit prune` — clear exited corpses; best-effort.
pub fn prune() {
    let _ = Command::new("babysit")
        .arg("prune")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// `babysit status -s <id>` success — does a session with this id exist?
pub fn status_exists(session: &str) -> bool {
    Command::new("babysit")
        .args(["status", "-s", session])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Is a session currently alive?
pub fn is_alive(session: &str) -> bool {
    list()
        .iter()
        .any(|s| s.id == session && s.alive)
}

/// Any looop worker currently in flight?
pub fn any_looop_alive() -> bool {
    list_looop().iter().any(|s| s.alive)
}
