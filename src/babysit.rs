//! Wrapper over the `babysit` worker fleet.
//!
//! Step 3: the hot, data-returning path (`list`) now calls the babysit LIBRARY
//! in-process (no `babysit ls --json` subprocess, no JSON re-parse). Action verbs
//! (prune/status/run) still shell out to the binary for now; migrating them is a
//! follow-on. The leading `::babysit` disambiguates the extern crate from this
//! same-named module.

use serde::Deserialize;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// A process-wide current-thread tokio runtime to drive babysit's async API.
/// looop is otherwise synchronous; async is confined to this boundary.
fn rt() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("looop: failed to build tokio runtime")
    })
}

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

/// List sessions via the babysit library, IN-PROCESS. Any failure yields an
/// empty list (matches the bash `2>/dev/null || true`): the pulse degrades
/// gracefully, never wedges. babysit's `paths::root()` reads `$BABYSIT_DIR`
/// fresh, so this honors looop's profile scoping just like the old shell-out.
pub fn list() -> Vec<Session> {
    match rt().block_on(::babysit::api::list_sessions()) {
        Ok(sessions) => sessions
            .into_iter()
            .map(|s| Session {
                id: s.id,
                cmd: None,
                state: s.state,
                alive: s.alive,
                exit_code: s.exit_code.map(|c| c as i64),
                note: s.note,
            })
            .collect(),
        Err(_) => Vec::new(),
    }
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
    list().iter().any(|s| s.id == session && s.alive)
}

/// Any looop worker currently in flight?
pub fn any_looop_alive() -> bool {
    list_looop().iter().any(|s| s.alive)
}
