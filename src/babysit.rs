//! Wrapper over the `babysit` worker fleet.
//!
//! Step 4 (complete): looop is a SINGLE self-contained binary — the `babysit`
//! executable is no longer required at runtime. Every data + control path runs
//! against the babysit LIBRARY in-process: `list`/`ls`, `prune`, `status_exists`,
//! `kill`, `flag`, `unflag`, `attach`, AND detached worker spawn.
//!
//! Detached spawn is the subtle one: babysit's detacher re-execs
//! `std::env::current_exe()` (here = `looop`) as the headless supervisor with a
//! hidden `run --detached-id <id>` form. looop handles that form itself
//! (`run_detached_worker`) and hands it straight to babysit's `serve_worker`, so
//! the worker is owned by a looop process — no `babysit` binary in the loop.
//! The leading `::babysit` disambiguates the extern crate from this module.

use serde::Deserialize;
use std::sync::OnceLock;

/// The babysit session id the pulse (親玉) runs under when started as a service
/// (`looop up`). It shares the `looop-` prefix so `looop ls` lists it alongside
/// the workers, but it is NOT a worker: cadence selection, the world hash, the
/// tick prompt's worker list, status, and flag-surfacing all treat it
/// separately (see `list_workers`).
pub const PULSE_SESSION: &str = "looop-pulse";

/// A process-wide current-thread tokio runtime to drive babysit's async API.
/// looop is otherwise synchronous; async is confined to this boundary.
fn rt() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        // Multi-thread + enable_all to match babysit's own `#[tokio::main]`:
        // the detached worker (serve_worker) owns a PTY read loop + a control
        // socket accept loop concurrently, and `attach` drives a socket + PTY.
        // The light paths (list/prune/flag) are happy here too.
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
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
    /// The pulse session is `looop-`-prefixed but is the 親玉, not a worker.
    pub fn is_pulse(&self) -> bool {
        self.id == PULSE_SESSION
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

/// looop-owned WORKER sessions only — the pulse (親玉) is excluded. This is the
/// list everything that reasons about "the fleet the pulse manages" must use
/// (cadence, world hash, tick prompt, status, flag-surfacing), so the pulse
/// never counts itself as one of its own workers.
pub fn list_workers() -> Vec<Session> {
    list()
        .into_iter()
        .filter(|s| s.is_looop() && !s.is_pulse())
        .collect()
}

/// Clear exited/dead-owner corpses, IN-PROCESS and SILENTLY. babysit's own
/// `sub::prune` prints to stdout (it is a CLI handler), which would pollute
/// looop's pretty output, so we drive the same decision over babysit's lower
/// level `session`/`paths` modules instead. Best-effort: any error is ignored,
/// matching the old `2>/dev/null` shell-out.
pub fn prune() {
    use ::babysit::session::{self, State};
    rt().block_on(async {
        let ids = match session::list_ids().await {
            Ok(ids) => ids,
            Err(_) => return,
        };
        for id in ids {
            // Scope to OUR fleet only. On the default profile looop shares
            // ~/.babysit with the user's other babysit sessions, so pruning by
            // liveness alone would reap THEIR dead corpses too. Only ever touch
            // `looop-` ids (the pulse + workers we own).
            if !id.starts_with("looop-") {
                continue;
            }
            let Ok(meta) = session::read_meta(&id).await else {
                continue; // unparseable meta — leave it alone, never nuke blind
            };
            let status = session::read_status(&id).await.ok();
            let alive = session::is_pid_alive(meta.babysit_pid);
            let dead = match status.as_ref().map(|s| s.state) {
                Some(State::Exited | State::Killed) => true,
                Some(State::Starting | State::Running) if !alive => true,
                None if !alive => true,
                _ => false,
            };
            if dead && let Ok(dir) = ::babysit::paths::session_dir(&id) {
                let _ = tokio::fs::remove_dir_all(&dir).await;
            }
        }
    });
}

/// Does a session with this id exist? In-process: the id is present in the
/// fleet list (replaces `babysit status -s <id>`).
pub fn status_exists(session: &str) -> bool {
    list().iter().any(|s| s.id == session)
}

/// `looop kill <id>` — terminate a session. Prints babysit's own result line
/// (this is a user-facing verb, so its output is wanted). Returns Ok on success.
pub fn kill(session: &str) -> anyhow::Result<()> {
    rt().block_on(::babysit::sub::kill(Some(session.to_string()), false))
}

/// `looop flag <id> [msg]` — raise a worker's attention flag.
pub fn flag(session: &str, message: Option<String>) -> anyhow::Result<()> {
    rt().block_on(::babysit::sub::flag(
        Some(session.to_string()),
        message,
        false,
    ))
}

/// `looop unflag <id>` — clear a worker's attention flag.
pub fn unflag(session: &str) -> anyhow::Result<()> {
    rt().block_on(::babysit::sub::unflag(Some(session.to_string()), false))
}

/// `looop attach <id>` — attach the terminal to a session; returns its exit code.
pub fn attach(session: &str) -> anyhow::Result<i32> {
    rt().block_on(::babysit::attach::attach(Some(session.to_string())))
}

/// `looop detach <id>` — force-detach any other terminal attached to a session.
pub fn detach(session: &str, json: bool) -> anyhow::Result<()> {
    rt().block_on(::babysit::attach::detach(Some(session.to_string()), json))
}

/// `looop watch <id>` — follow a session's output read-only (tail -f), printing
/// the full log so far and then live output until the session exits (Ctrl-C to
/// stop). This is the non-interactive twin of `attach`: no input, just output.
pub fn watch(session: &str) -> anyhow::Result<()> {
    rt().block_on(::babysit::sub::log(
        Some(session.to_string()),
        None,  // tail: whole log, then follow
        None,  // grep
        false, // raw
        None,  // since: from the start
        true,  // follow
        false, // json
    ))
}

/// `looop log <id>` — show / tail / grep / follow a session's recorded output.
#[allow(clippy::too_many_arguments)]
pub fn log(
    session: &str,
    tail: Option<usize>,
    grep: Option<String>,
    raw: bool,
    since: Option<u64>,
    follow: bool,
    json: bool,
) -> anyhow::Result<()> {
    rt().block_on(::babysit::sub::log(
        Some(session.to_string()),
        tail,
        grep,
        raw,
        since,
        follow,
        json,
    ))
}

/// `looop shot <id>` — render the session's current visible screen.
pub fn screenshot(
    session: &str,
    format: ::babysit::cli::ShotFormat,
    trim: bool,
) -> anyhow::Result<()> {
    rt().block_on(::babysit::sub::screenshot(
        Some(session.to_string()),
        format,
        trim,
    ))
}

/// `looop send <id> <text>` — type text into a session's stdin.
pub fn send(session: &str, text: String, newline: bool, json: bool) -> anyhow::Result<()> {
    rt().block_on(::babysit::sub::send(
        Some(session.to_string()),
        text,
        newline,
        json,
    ))
}

/// `looop key <id> <KEY...>` — send named keys (Enter, Up, C-c, …) to a session.
pub fn key(session: &str, keys: Vec<String>, json: bool) -> anyhow::Result<()> {
    rt().block_on(::babysit::sub::key(Some(session.to_string()), keys, json))
}

/// `looop expect <id> <REGEX>` — block until a regex appears; exit 124 on timeout.
#[allow(clippy::too_many_arguments)]
pub fn expect(
    session: &str,
    pattern: String,
    timeout: String,
    since: Option<u64>,
    from_now: bool,
    raw: bool,
    screen: bool,
    json: bool,
) -> anyhow::Result<i32> {
    rt().block_on(::babysit::sub::expect(
        Some(session.to_string()),
        pattern,
        timeout,
        since,
        from_now,
        raw,
        screen,
        json,
    ))
}

/// `looop wait <id>` — block until the session exits; returns its exit code.
pub fn wait(session: &str, timeout: Option<String>) -> anyhow::Result<i32> {
    rt().block_on(::babysit::sub::wait(Some(session.to_string()), timeout))
}

/// `looop wait-idle <id>` — block until output is quiet for `settle`.
pub fn wait_idle(session: &str, settle: String, timeout: String) -> anyhow::Result<i32> {
    rt().block_on(::babysit::sub::wait_idle(
        Some(session.to_string()),
        settle,
        timeout,
    ))
}

/// `looop resize <id> <COLSxROWS>` — resize a session's terminal.
pub fn resize(session: &str, size: String, json: bool) -> anyhow::Result<()> {
    rt().block_on(::babysit::sub::resize(
        Some(session.to_string()),
        size,
        json,
    ))
}

/// `looop restart <id>` — restart the wrapped command in a session.
pub fn restart(session: &str, json: bool) -> anyhow::Result<()> {
    rt().block_on(::babysit::sub::restart(Some(session.to_string()), json))
}

/// `looop ls [--json] [--watch] [--interval <dur>]` — render the fleet table
/// IN-PROCESS via babysit's own list renderer (this is the printing CLI path,
/// so its stdout is exactly what we want). `--watch` refreshes in place, just
/// like `babysit ls --watch`, because the loop lives in the library.
pub fn ls(json: bool, watch: bool, interval: String) -> anyhow::Result<()> {
    rt().block_on(::babysit::sub::list(json, watch, interval))
}

/// Spawn a detached worker IN-PROCESS (no `babysit` binary). babysit's parent
/// path picks the id, then re-execs `current_exe()` (= looop) as the headless
/// supervisor — looop routes that back into `serve_worker` via
/// `run_detached_worker`. babysit prints a start banner on the parent path; we
/// suppress it so looop owns its own "started …" output.
pub fn spawn_detached(cmd: Vec<String>, session: &str) -> anyhow::Result<()> {
    suppress_stdout(|| {
        rt().block_on(::babysit::run::run(
            cmd,
            Some(session.to_string()),
            true,  // detach: spawn the worker and return immediately
            None,  // detached_id: we are the parent, not the worker
            false, // no_tty
            None,  // timeout
            None,  // idle_timeout
            None,  // size
            true,  // json (one suppressed line; we print our own message)
        ))
    })
    .map(|_code| ())
}

/// The worker side of detached spawn: looop was re-exec'd by babysit's detacher
/// as `looop run --detached-id <id> [--no-tty] [--timeout <ms>] [--idle-timeout
/// <ms>] [--size <CxR>] -- <cmd…>`. Parse that argv (babysit's own format, pinned
/// by the `^0.10` dep) and hand off to the library's headless supervisor, which
/// blocks until the wrapped command exits.
pub fn run_detached_worker(args: &[String]) -> anyhow::Result<i32> {
    use anyhow::Context;
    let mut id = None;
    let mut no_tty = false;
    let mut timeout = None;
    let mut idle_timeout = None;
    let mut size = None;
    let mut cmd: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--detached-id" => id = it.next().cloned(),
            "--no-tty" => no_tty = true,
            "--timeout" => timeout = it.next().cloned(),
            "--idle-timeout" => idle_timeout = it.next().cloned(),
            "--size" => size = it.next().cloned(),
            "--" => {
                cmd = it.by_ref().cloned().collect();
                break;
            }
            _ => {} // ignore unknown flags (forward-compat with babysit)
        }
    }
    let id = id.context("looop run --detached-id: missing worker id")?;
    rt().block_on(::babysit::run::run(
        cmd,
        None,
        false,
        Some(id),
        no_tty,
        timeout,
        idle_timeout,
        size,
        false,
    ))
}

/// Run `f` with this process's stdout (fd 1) redirected to /dev/null, then
/// restore it. Used to swallow babysit's parent-path banner while keeping
/// looop's own output. Unix-only; a no-op redirect failure just runs `f`.
#[cfg(unix)]
fn suppress_stdout<T>(f: impl FnOnce() -> T) -> T {
    use std::io::Write;
    use std::os::unix::io::AsRawFd;
    unsafe extern "C" {
        fn dup(fd: i32) -> i32;
        fn dup2(a: i32, b: i32) -> i32;
        fn close(fd: i32) -> i32;
    }
    let Ok(devnull) = std::fs::OpenOptions::new().write(true).open("/dev/null") else {
        return f();
    };
    let _ = std::io::stdout().flush();
    unsafe {
        let saved = dup(1);
        if saved < 0 {
            return f();
        }
        dup2(devnull.as_raw_fd(), 1);
        let out = f();
        let _ = std::io::stdout().flush();
        dup2(saved, 1);
        close(saved);
        out
    }
}

#[cfg(not(unix))]
fn suppress_stdout<T>(f: impl FnOnce() -> T) -> T {
    f()
}

/// Is a session currently alive?
pub fn is_alive(session: &str) -> bool {
    list().iter().any(|s| s.id == session && s.alive)
}

/// Any looop worker currently in flight?
pub fn any_worker_alive() -> bool {
    list_workers().iter().any(|s| s.alive)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sess(id: &str, note: Option<&str>) -> Session {
        Session {
            id: id.to_string(),
            note: note.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn is_looop_matches_only_prefixed_ids() {
        assert!(sess("looop-triage", None).is_looop());
        assert!(!sess("other-job", None).is_looop());
        assert!(!sess("looop", None).is_looop()); // needs the trailing dash
    }

    #[test]
    fn pulse_is_looop_but_not_a_worker() {
        let p = sess(PULSE_SESSION, None);
        assert!(p.is_looop(), "pulse shares the looop- prefix");
        assert!(p.is_pulse(), "pulse is the 親玉");
        // A worker is looop-prefixed but is NOT the pulse.
        let w = sess("looop-triage", None);
        assert!(w.is_looop() && !w.is_pulse());
    }

    #[test]
    fn flagged_iff_nonempty_note() {
        assert!(sess("looop-x", Some("help")).flagged());
        assert!(!sess("looop-x", Some("")).flagged());
        assert!(!sess("looop-x", None).flagged());
    }
}
