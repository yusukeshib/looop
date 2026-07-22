//! Session fleet — the in-process adapter over the `babysit` library.
//! looop hands the library an explicit `Babysit` context (`paths.sessions()`),
//! so the fleet is self-contained per profile: no $BABYSIT_DIR, no shared
//! ~/.babysit, and bare session ids (the pulse is `pulse`).
//!
//! The [`Fleet`] trait is the seam the launch gating (fleet cap, duplicate-id,
//! fail-closed enumeration — see [`super::launch`]) runs against, so that
//! lifecycle POLICY is unit-testable with an in-memory fake instead of real
//! process spawns. [`BabysitFleet`] is the only production impl.

use crate::paths::Paths;

use super::plumbing::suppress_stdout;

/// The session id the pulse runs under when started as a service
/// (a bare `looop`). It is reserved: a worker can never take this id (see
/// `session::cmd_start_session`), so the single control-plane session can't
/// collide with a goal-named worker.
pub const PULSE_SESSION: &str = "pulse";

/// A process-wide multi-thread tokio runtime to drive babysit's async API.
/// looop is otherwise synchronous; async is confined to this boundary.
pub(super) fn rt() -> &'static tokio::runtime::Runtime {
    use std::sync::OnceLock;
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        // Multi-thread + enable_all to match babysit's own `#[tokio::main]`:
        // the detached worker (serve_worker) owns a PTY read loop + a control
        // socket accept loop concurrently, and `attach` drives a socket + PTY.
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("looop: failed to build tokio runtime")
    })
}

/// One session in this profile's fleet — a thin projection of babysit's
/// `SessionInfo` onto just what looop reasons about.
#[derive(Debug, Default, Clone)]
pub struct Session {
    pub id: String,
    pub state: String,
    pub alive: bool,
    pub exit_code: Option<i64>,
    /// RFC3339 timestamp of the session's start (babysit's `started_at`).
    /// Empty when babysit didn't report one. Feeds the sys-sessions health
    /// reading's `uptime_s` detail.
    pub started_at: String,
    /// RFC3339 timestamp of the most recent session state change. This is the
    /// authoritative recency marker for retained corpses in `looop watch`.
    pub last_change: String,
}

impl Session {
    /// The pulse session is the control loop, not a worker.
    pub fn is_pulse(&self) -> bool {
        self.id == PULSE_SESSION
    }

    /// Seconds since this session started, if `started_at` parses.
    pub fn uptime_secs(&self) -> Option<u64> {
        let ts = chrono::DateTime::parse_from_rfc3339(self.started_at.trim()).ok()?;
        (chrono::Utc::now() - ts.with_timezone(&chrono::Utc))
            .to_std()
            .ok()
            .map(|d| d.as_secs())
    }

    /// Time since babysit last changed this session's state.
    pub fn idle_for(&self) -> Option<std::time::Duration> {
        let ts = chrono::DateTime::parse_from_rfc3339(self.last_change.trim()).ok()?;
        (chrono::Utc::now() - ts.with_timezone(&chrono::Utc))
            .to_std()
            .ok()
    }
}

fn project(info: ::babysit::SessionInfo) -> Session {
    Session {
        id: info.id,
        state: info.state,
        alive: info.alive,
        exit_code: info.exit_code.map(|c| c as i64),
        started_at: info.started_at,
        last_change: info.last_change,
    }
}

/// The fleet operations the worker-launch GATING logic needs, expressed as a
/// trait (mirroring [`crate::contract::Contract`]'s seam-over-backend shape)
/// so the policy in `cmd_start_session` — cap enforcement, duplicate-id
/// refusal, fail-closed on enumeration error, corpse reap before id reuse —
/// can be exercised against an in-memory fake instead of a real process
/// fleet. Deliberately MINIMAL: only what the gating path calls; read-only
/// sensor paths keep using the free functions below directly.
pub(crate) trait Fleet {
    /// Enumerate the fleet, SURFACING the enumeration error — the gating
    /// callers fail CLOSED on `Err` (see [`try_list`]'s doc for why an
    /// error-to-empty collapse would be a wrong decision).
    fn try_list(&self) -> anyhow::Result<Vec<Session>>;
    /// Targeted reap of `session` IF it is a dead corpse, freeing its id for
    /// reuse (see [`reap`]).
    fn reap(&self, session: &str);
    /// Spawn a detached worker under `session` (see [`spawn_detached`]).
    fn spawn(&self, cmd: Vec<String>, session: &str) -> anyhow::Result<()>;
}

/// The babysit-backed [`Fleet`]: a thin binding from the trait's operations to
/// the free functions this module already exposes, borrowing the resolved
/// [`Paths`] exactly like `contract::LocalContract` does.
pub(crate) struct BabysitFleet<'a> {
    paths: &'a Paths,
}

impl<'a> BabysitFleet<'a> {
    pub(crate) fn new(paths: &'a Paths) -> Self {
        BabysitFleet { paths }
    }
}

impl Fleet for BabysitFleet<'_> {
    fn try_list(&self) -> anyhow::Result<Vec<Session>> {
        try_list(self.paths)
    }
    fn reap(&self, session: &str) {
        reap(self.paths, session);
    }
    fn spawn(&self, cmd: Vec<String>, session: &str) -> anyhow::Result<()> {
        spawn_detached(self.paths, cmd, session)
    }
}

/// Normalize a user-supplied worker id to its full session id. Accepts both the
/// short goal id (`triage`) and the legacy full session id (`looop-triage`).
pub(super) fn full_session(paths: &Paths, id: &str) -> String {
    // The fleet root is looop-exclusive, so a session id is just the goal id
    // (or `pulse`). A literally-named session always wins: only when NO
    // session exists under the raw id do we strip a legacy `looop-` prefix
    // (back-compat with old muscle memory / scripts) — stripping first would
    // make a worker legitimately named `looop-x` unreachable (kill/screenshot
    // would target a nonexistent `x`).
    if status_exists(paths, id) {
        return id.to_string();
    }
    id.strip_prefix("looop-").unwrap_or(id).to_string()
}

/// Seconds since `id`'s terminal last produced OUTPUT — the mtime of the
/// session's PTY tee (`sessions/<id>/output.log`). This is the fleet's
/// last-stdout-time health signal: a live worker that is neither writing output
/// nor blocked on an ask has no other way to show progress (there is no input
/// channel to nudge it), so a long silence here means it is likely stuck.
/// `None` when the log is missing or undatable (bias: treat as fresh).
pub fn output_idle_secs(paths: &Paths, id: &str) -> Option<u64> {
    let log = paths
        .sessions()
        .session_dir(&full_session(paths, id))
        .join("output.log");
    std::fs::metadata(log)
        .ok()?
        .modified()
        .ok()?
        .elapsed()
        .ok()
        .map(|d| d.as_secs())
}

/// List every session in this profile's fleet, surfacing the enumeration
/// error. GATING callers — the fleet cap and duplicate-id checks in
/// `cmd_start_session`, `looop up`'s already-running probe, `looop down`'s
/// sweep — use THIS and fail CLOSED: an error-to-empty collapse there turns an
/// I/O failure into a wrong DECISION (cap bypass, a second pulse, a `down`
/// that claims a clean stop over a live fleet). Read-only/sensor callers use
/// the lenient [`list`] instead.
pub fn try_list(paths: &Paths) -> anyhow::Result<Vec<Session>> {
    Ok(rt()
        .block_on(paths.sessions().list_sessions())?
        .into_iter()
        .map(project)
        .collect())
}

/// List every session in this profile's fleet. Any failure yields an empty
/// list: the pulse's SENSING paths degrade gracefully, never wedge — but not
/// silently: the error is surfaced as a warn event, because a fleet read as
/// empty when enumeration actually FAILED is a degraded reading the operator
/// must be able to see. Paths whose decisions must not fail open use
/// [`try_list`].
pub fn list(paths: &Paths) -> Vec<Session> {
    try_list(paths).unwrap_or_else(|e| {
        crate::util::event(
            crate::util::Level::Warn,
            "fleet.list_degraded",
            &format!("cannot enumerate the fleet ({e}) — reading it as empty"),
            &[],
        );
        Vec::new()
    })
}

/// Worker sessions only — the pulse is excluded. Everything that reasons
/// about "the fleet the pulse manages" (cadence, world hash, tick prompt,
/// status, flag-surfacing) uses this so the pulse never counts itself.
pub fn list_workers(paths: &Paths) -> Vec<Session> {
    list(paths).into_iter().filter(|s| !s.is_pulse()).collect()
}

/// Is this session a reapable corpse? (exited/killed, or a dead owner with no
/// fresh status). Never reaps a session whose meta we couldn't parse — we don't
/// nuke blind.
fn corpse_dead(state: Option<::babysit::session::State>, alive: bool) -> bool {
    use ::babysit::session::State;
    match state {
        Some(State::Exited | State::Killed) => true,
        Some(State::Starting | State::Running) if !alive => true,
        None if !alive => true,
        _ => false,
    }
}

/// Reap dead corpses whose session dir is older than `max_age`, IN-PROCESS and
/// SILENTLY. sessions/ is system scratch (the durable artifacts a worker
/// produces live in reports/ + git + its sandbox — see the CONTRACT), so looop
/// owns its lifecycle. But a corpse's `output.log` is the only transcript of
/// what that agent did, so the per-tick housekeeping passes a RETENTION window
/// rather than nuking it the instant the worker finishes. The fleet root is
/// looop-exclusive, so every corpse here is ours. Best-effort: errors ignored.
pub fn prune_aged(paths: &Paths, max_age: std::time::Duration) {
    use ::babysit::session;
    let bs = paths.sessions();
    rt().block_on(async {
        let ids = match session::list_ids(&bs).await {
            Ok(ids) => ids,
            Err(_) => return,
        };
        for id in ids {
            let Ok(meta) = session::read_meta(&bs, &id).await else {
                continue; // unparseable meta — leave it alone, never nuke blind
            };
            let status = session::read_status(&bs, &id).await.ok();
            let alive = session::is_pid_alive(meta.babysit_pid);
            if !corpse_dead(status.as_ref().map(|s| s.state), alive) {
                continue;
            }
            let dir = bs.session_dir(&id);
            // Age ≈ time since the dir last changed (a dead session stops
            // writing). max_age == 0 ⇒ reap now; undeterminable age ⇒ KEEP (the
            // retention bias favors preserving a transcript we can't date —
            // explicit `looop prune` is the catch-all).
            let old = max_age.is_zero()
                || tokio::fs::metadata(&dir)
                    .await
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age >= max_age);
            if old {
                let _ = tokio::fs::remove_dir_all(&dir).await;
                // Shared generation-boundary hygiene (verify + tells) — see
                // session::on_generation_end for the full reasoning.
                super::on_generation_end(paths, &id);
            }
        }
    });
}

/// Targeted reap: remove just `session`'s dir IF it's a dead corpse, so its id
/// can be reused — without disturbing sibling sessions' retained transcripts.
/// Used when reclaiming one specific id (the pulse on `up`/`down`, a worker id
/// on restart).
pub fn reap(paths: &Paths, session: &str) {
    use ::babysit::session;
    let bs = paths.sessions();
    rt().block_on(async {
        let Ok(meta) = session::read_meta(&bs, session).await else {
            return;
        };
        let status = session::read_status(&bs, session).await.ok();
        let alive = session::is_pid_alive(meta.babysit_pid);
        if corpse_dead(status.as_ref().map(|s| s.state), alive) {
            let _ = tokio::fs::remove_dir_all(bs.session_dir(session)).await;
            // A removed corpse's id is about to be REUSED — shared generation-
            // boundary hygiene (verify + tells), see session::on_generation_end.
            super::on_generation_end(paths, session);
        }
    });
}

/// Does a session with this id exist in the fleet?
pub fn status_exists(paths: &Paths, session: &str) -> bool {
    list(paths).iter().any(|s| s.id == session)
}

/// `looop kill <id>` — terminate a session.
pub fn kill(paths: &Paths, session: &str) -> anyhow::Result<()> {
    rt().block_on(paths.sessions().kill(Some(session.to_string()), false))
}

/// Like `kill` but swallows babysit's "killed session …" stdout line, so a
/// caller that prints its own message (e.g. the foreground teardown) stays single-line.
pub fn kill_quiet(paths: &Paths, session: &str) -> anyhow::Result<()> {
    suppress_stdout(|| kill(paths, session))
}

/// Spawn a detached worker IN-PROCESS. babysit's parent path re-execs
/// `current_exe()` (= looop) as the headless supervisor, handing it the state
/// root via `--root` and the id via `--detached-id`; looop routes that back into
/// `serve_worker` via `run_detached_worker`. babysit prints a start banner on
/// the parent path; we suppress it so looop owns its own "started …" output.
pub fn spawn_detached(paths: &Paths, cmd: Vec<String>, session: &str) -> anyhow::Result<()> {
    let bs = paths.sessions();
    suppress_stdout(|| {
        rt().block_on(bs.run(
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

/// The parsed shape of babysit's re-exec argv (see [`run_detached_worker`]).
/// Split out of the runner purely so the parse is unit-testable without
/// spawning a real headless supervisor.
#[derive(Debug, Default, PartialEq)]
struct DetachedArgs {
    id: Option<String>,
    root: Option<String>,
    no_tty: bool,
    timeout: Option<String>,
    idle_timeout: Option<String>,
    size: Option<String>,
    cmd: Vec<String>,
}

/// Parse `--detached-id <id> --root <dir> [--no-tty] [--timeout <ms>]
/// [--idle-timeout <ms>] [--size <CxR>] -- <cmd…>`.
///
/// Every VALUE-TAKING flag is an explicit match arm that consumes its value
/// itself. FORWARD-COMPAT: babysit may re-exec us with flags a newer babysit
/// knows and this looop build does not — those must be skipped, not fatal.
/// An unknown flag is skipped together with its apparent value (the next
/// token, when that token is not itself flag-like), so `--future-knob 42`
/// can never leak `42` into the parse — and, more importantly, an unknown
/// flag's value can never be mistaken for one of OUR flags' trigger. The one
/// unguardable shape — an unknown flag whose value itself starts with `-` —
/// must be spelled `--flag=value` (one token, skipped whole); this parser
/// cannot know the arity of a flag it has never heard of.
fn parse_detached_args(args: &[String]) -> DetachedArgs {
    let mut out = DetachedArgs::default();
    let mut it = args.iter().peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--detached-id" => out.id = it.next().cloned(),
            "--root" => out.root = it.next().cloned(),
            "--no-tty" => out.no_tty = true,
            "--timeout" => out.timeout = it.next().cloned(),
            "--idle-timeout" => out.idle_timeout = it.next().cloned(),
            "--size" => out.size = it.next().cloned(),
            "--" => {
                out.cmd = it.cloned().collect();
                break;
            }
            // Unknown flag (forward-compat): skip it, and skip its apparent
            // value too — unless that next token is flag-like (`-…`), which
            // covers both a following real flag and the `--` separator.
            unknown => {
                if unknown.starts_with("--")
                    && !unknown.contains('=')
                    && it.peek().is_some_and(|next| !next.starts_with('-'))
                {
                    let _ = it.next();
                }
            }
        }
    }
    out
}

/// The worker side of detached spawn: looop was re-exec'd by babysit's detacher
/// as `looop run --detached-id <id> --root <dir> [--no-tty] [--timeout <ms>]
/// [--idle-timeout <ms>] [--size <CxR>] -- <cmd…>`. Parse that argv (see
/// [`parse_detached_args`]) and hand off to the library's headless supervisor,
/// which blocks until the wrapped command exits. The state root comes from
/// `--root`, so the worker reconstructs THIS fleet's context without reading
/// any environment.
pub fn run_detached_worker(args: &[String]) -> anyhow::Result<i32> {
    use anyhow::Context;
    let parsed = parse_detached_args(args);
    let id = parsed
        .id
        .context("looop run --detached-id: missing worker id")?;
    let root = parsed
        .root
        .context("looop run --detached-id: missing --root")?;
    let bs = ::babysit::Babysit::new(root);
    rt().block_on(bs.run(
        parsed.cmd,
        None,
        false,
        Some(id),
        parsed.no_tty,
        parsed.timeout,
        parsed.idle_timeout,
        parsed.size,
        false,
    ))
}

/// Is a session currently alive?
pub fn is_alive(paths: &Paths, session: &str) -> bool {
    list(paths).iter().any(|s| s.id == session && s.alive)
}

/// Is a session currently alive? — the fail-closed variant of [`is_alive`]
/// for GATING callers: `looop up` must not read an enumeration error as "no
/// pulse" and spawn a second one; `looop down` must not read it as "nothing
/// to stop" and exit 0 over a live fleet.
pub fn try_is_alive(paths: &Paths, session: &str) -> anyhow::Result<bool> {
    Ok(try_list(paths)?.iter().any(|s| s.id == session && s.alive))
}

/// Block (briefly) until a session is registered and alive. For callers that
/// spawn detached then immediately follow it (e.g. the foreground `looop`): the
/// supervisor needs a beat to register the session, so following it instantly
/// races the spawn (`no session matching …`). Returns true once alive, false if
/// it never came up within `timeout`.
pub fn await_alive(paths: &Paths, session: &str, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if is_alive(paths, session) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sess(id: &str) -> Session {
        Session {
            id: id.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn pulse_is_recognized() {
        assert!(sess(PULSE_SESSION).is_pulse());
        assert!(!sess("triage").is_pulse());
    }

    #[test]
    fn detached_argv_parse_is_explicit_about_value_taking_flags() {
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // The full known shape round-trips.
        let got = parse_detached_args(&s(&[
            "--detached-id",
            "w1",
            "--root",
            "/data/sessions",
            "--no-tty",
            "--timeout",
            "5000",
            "--",
            "bash",
            "-c",
            "true",
        ]));
        assert_eq!(got.id.as_deref(), Some("w1"));
        assert_eq!(got.root.as_deref(), Some("/data/sessions"));
        assert!(got.no_tty);
        assert_eq!(got.timeout.as_deref(), Some("5000"));
        assert_eq!(got.cmd, s(&["bash", "-c", "true"]));

        // FORWARD-COMPAT: an unknown flag's value is skipped WITH it — it can
        // neither leak into the parse nor shadow a known flag's slot.
        let got = parse_detached_args(&s(&[
            "--future-knob",
            "42",
            "--root",
            "/r",
            "--detached-id",
            "w2",
            "--",
            "true",
        ]));
        assert_eq!(got.id.as_deref(), Some("w2"));
        assert_eq!(got.root.as_deref(), Some("/r"));
        assert_eq!(got.cmd, s(&["true"]));

        // A value-LESS unknown followed by a known flag must not eat it…
        let got = parse_detached_args(&s(&["--future-bool", "--no-tty", "--root", "/r"]));
        assert!(got.no_tty, "a known flag after a bare unknown still parses");
        assert_eq!(got.root.as_deref(), Some("/r"));

        // …`--flag=value` unknowns are one token, skipped whole…
        let got = parse_detached_args(&s(&["--future-knob=42", "--detached-id", "w3"]));
        assert_eq!(got.id.as_deref(), Some("w3"));

        // …and an unknown just before the `--` separator never consumes it.
        let got = parse_detached_args(&s(&["--future-bool", "--", "echo", "hi"]));
        assert_eq!(got.cmd, s(&["echo", "hi"]));
    }
}
