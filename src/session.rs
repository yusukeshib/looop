//! Start a worker session — the hands. `looop start-session <id> "<prompt>"`.
//! The pulse only LAUNCHES the agent (in the data dir) under babysit, detached;
//! it does NOT provision a workspace. Every worker gets the same contract
//! prepended so the pulse can't forget it (workers never notify — they flag and
//! wait; they sandbox their own code; the data dir's policy files are read-only).

use crate::config::Config;
use crate::paths::Paths;
use crate::seed;
use anyhow::Result;
use std::fs;
use std::process::ExitCode;

/// Single-quote a string for safe inclusion in a `bash -lc` command line
/// (wraps in `'…'`, escaping embedded single quotes as `'\''`).
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

const CONTRACT: &str = r#"# ⚑ WORKER CONTRACT (auto-injected — must obey)
- Never send notifications (no terminal-notifier or any OS notification). You are
  an agent; surface anything a human must see by ASKing (below) — the human sees
  it through whatever client they run.
- When you need a human decision / info / approval, do NOT guess — ASK and WAIT.
  This ONE command writes your question to the mailbox and BLOCKS until the root
  agent (or human) answers, printing the answer to stdout:
    answer=$("$LOOOP_BIN" _ ask __ID__ --prompt "<what you need to know>")
  (optionally --ref reports/x.md and/or --options a,b). Use $answer and continue.
  You do NOT need a terminal, stdin, or attach — just call it and read its output.
  Ask once per question; it returns only when answered.
- When the task is 100% complete and nothing is waiting, end your own session:
    "$LOOOP_BIN" _ kill __ID__
  (this lets the pulse prune the corpse). NEVER do this mid-task or while waiting
  on a human.
- LEASE (ONLY if the PLAYBOOK/goal tells you to claim this task) — announce
  ownership BEFORE any work so a tick or sibling can't duplicate/race you:
    "$LOOOP_BIN" _ claim <name>   # atomic test-and-set; <name> defined by the goal (e.g. one per repo)
  This EXITS NON-ZERO if a live session already holds <name> — if so, do NOT
  proceed: flag the human or pick other work, never race the holder. Release it
  the instant the task is fully done, right before the kill above:
    "$LOOOP_BIN" _ unclaim <name>
  If you crash the pulse auto-reaps your claim; on a clean finish YOU release it.
  NEVER sit/sleep/poll while holding a claim — act and move on.
- SINGLE-WRITER DATA DIR: the pulse (the tick AI) is the SOLE writer of the
  policy files — PLAYBOOK.md, goals/ and sensors/. By default you write ONLY to
  claims/ (your lease), reports/ (deliverables) and your own code sandbox. Do
  NOT edit PLAYBOOK/goals/sensors: a concurrent tick reads them every beat, so a
  racing writer tears the loop's state. If your task implies a policy change,
  write the proposal to reports/<id>.md and raise a flag — the human (or the
  next tick) applies it. EXCEPTION: if your task is explicitly a meta task (e.g.
  setup or playbook grooming), you MAY edit those files, but you MUST show the
  diff and `"$LOOOP_BIN" _ flag` for human approval BEFORE writing. When unsure whether
  your task is meta, treat the data dir as read-only and propose via reports/.
- WORKSPACE: you start in the loop data dir (read-only context for you, save the
  meta exception above). If your task touches a code repo, provision your OWN
  sandbox FIRST and cd into it — never edit code in the data dir:
    • if `box` is available:  box new __SESSION__ --repo <repo> && cd "$(box switch __SESSION__)"
    • otherwise (git):         git -C <local-clone> worktree add /tmp/__SESSION__ -b looop/__SESSION__ && cd /tmp/__SESSION__
  (the PLAYBOOK names the repos and which to prefer.)
- DELIVERABLES: write any report / artifact a human will read into the data dir's
  reports/ folder (e.g. reports/<id>.md). That dir PERSISTS across ticks. NEVER
  write deliverables to snapshots/ — the pulse wipes snapshots/ on EVERY beat, so
  anything you leave there vanishes before the human sees it. Reference the
  reports/ path in your flag note so I know where to look.

---

"#;

pub fn cmd_start_session(paths: &Paths, args: &[String]) -> Result<ExitCode> {
    seed::ensure_dirs(paths)?;

    let Some(id) = args.first() else {
        eprintln!("usage: looop start-session <id> <prompt>");
        return Ok(ExitCode::from(1));
    };
    let Some(prompt) = args.get(1) else {
        eprintln!("missing prompt");
        return Ok(ExitCode::from(1));
    };

    let cfg = Config::load(paths)?;
    let runner = cfg.runner_label();
    let Some(tmpl) = cfg.runner_cmd("worker_command") else {
        eprintln!("start-session: no `worker_command` configured");
        return Ok(ExitCode::from(1));
    };

    // The worker's session id IS the goal id (no prefix — the fleet root is
    // looop-exclusive). `pulse` is reserved for the control loop, so a worker
    // can never collide with the pulse.
    if id.as_str() == PULSE_SESSION {
        eprintln!("start-session: '{id}' is reserved for the pulse; pick another id");
        return Ok(ExitCode::from(1));
    }
    let session = id.clone();

    if status_exists(paths, &session) {
        if is_alive(paths, &session) {
            eprintln!("start-session: session {session} is already running");
            return Ok(ExitCode::from(1));
        }
        reap(paths, &session); // reuse the id held by a dead corpse (targeted)
    }

    // Prompt via file (avoids quoting hell; also a record of the ask), with the
    // contract prepended.
    let prompt_file = paths.prompts_dir().join(format!("{session}.md"));
    let contract = CONTRACT
        .replace("__SESSION__", &session)
        .replace("__ID__", id);
    fs::write(&prompt_file, format!("{contract}{prompt}\n"))?;

    let cmd = tmpl.replace("{{prompt_file}}", &prompt_file.to_string_lossy());

    // The worker runs in the DATA dir. The in-process spawner inherits the
    // current process cwd (babysit's Pane uses `std::env::current_dir`), so we
    // `cd` there inside the shell command instead of mutating looop's own cwd.
    // Export LOOOP_SESSION_ID so the worker knows its OWN session id (for its
    // lease claim, etc.) through a looop-branded var.
    let launch = format!(
        "export LOOOP_SESSION_ID={}; cd {} && {cmd}",
        shell_quote(&session),
        shell_quote(&paths.data_dir.to_string_lossy())
    );

    // Launch the worker detached, IN-PROCESS via the babysit library (no
    // `babysit` binary). babysit re-execs looop as the headless supervisor.
    // `-c`, not `-lc`: a non-login shell sources no rc files, so the worker
    // launches against looop's inherited environment instead of re-running the
    // operator's login profile (hermetic + cheaper). The runner template itself
    // is still a shell string ($(cat ...), &&), so the shell stays.
    spawn_detached(
        paths,
        vec!["bash".to_string(), "-c".to_string(), launch],
        &session,
    )?;

    println!(
        "started {session} (runner: {runner}, cwd: {})",
        paths.data_dir.display()
    );
    println!("  watch: looop attach {id}");
    Ok(ExitCode::SUCCESS)
}

/// Normalize a user-supplied worker id to its full session id. Accepts both the
/// short goal id (`triage`) and the full session id (`looop-triage`).
fn full_session(id: &str) -> String {
    // The fleet root is looop-exclusive, so a session id is just the goal id
    // (or `pulse`). Strip a legacy `looop-` prefix for back-compat with old
    // muscle memory / scripts.
    id.strip_prefix("looop-").unwrap_or(id).to_string()
}

/// The pulse is the control loop, NOT a worker: refuse worker-management verbs
/// aimed at it so a stray `looop _ kill pulse` / `attach pulse` can't decapitate
/// or hijack the loop. Observe it with `looop watch`/`log`; control it with
/// a bare `looop`. Returns true (and prints guidance) when `session` is the
/// reserved pulse id — the caller should then bail with a non-zero code.
fn reject_pulse(session: &str, verb: &str) -> bool {
    if session == PULSE_SESSION {
        eprintln!(
            "looop {verb}: '{PULSE_SESSION}' is the control loop, not a worker — observe it with \
             `looop watch {PULSE_SESSION}` / `looop log {PULSE_SESSION}`, start it by running \
             `looop` (Ctrl-C stops it)"
        );
        true
    } else {
        false
    }
}

/// `looop _ kill <id>` — terminate a worker session (in-process). Internal
/// worker self-control callback (CONTRACT), not a human-facing verb.
pub fn cmd_kill(paths: &Paths, id: &str) -> Result<ExitCode> {
    let session = full_session(id);
    if reject_pulse(&session, "kill") {
        return Ok(ExitCode::from(1));
    }
    kill(paths, &session)?;
    Ok(ExitCode::SUCCESS)
}

/// `looop _ send <id> <text…> [--no-newline]` — type text into a worker's
/// terminal as if a human were at the keyboard. A STEER verb: the human (or any
/// client) nudges a stuck/interactive worker that's waiting on input. By
/// default a trailing Enter is sent (the common "answer the prompt" case);
/// `--no-newline` suppresses it (e.g. partial input). Refuses the pulse — the
/// control loop is driven by goals/PLAYBOOK + asks, never raw keystrokes.
pub fn cmd_send(paths: &Paths, args: &crate::cli::SendArgs) -> Result<ExitCode> {
    let newline = !args.no_newline;
    if args.text.is_empty() {
        eprintln!("usage: looop _ send <id> <text…> [--no-newline]");
        return Ok(ExitCode::from(1));
    }
    let session = full_session(&args.id);
    if reject_pulse(&session, "send") {
        return Ok(ExitCode::from(1));
    }
    let text = args.text.join(" ");
    rt().block_on(
        paths
            .sessions()
            .send(Some(session.clone()), text, newline, false),
    )?;
    println!("sent to {session}");
    Ok(ExitCode::SUCCESS)
}

/// `looop _ screenshot <id> [--ansi|--json] [--no-trim]` — capture a session's
/// current screen (the rendered terminal grid, not a frame-by-frame append).
/// A read-only STEER verb usable on any session, including the pulse: it's how
/// a human (or any client) peeks at what a worker is showing right now without
/// attaching. Falls back to the on-disk log render if the session isn't live.
/// Defaults to plain text (cheapest for an LLM to read) with trailing blank
/// rows trimmed.
pub fn cmd_screenshot(paths: &Paths, args: &crate::cli::ScreenshotArgs) -> Result<ExitCode> {
    use ::babysit::cli::ShotFormat;
    let format = if args.ansi {
        ShotFormat::Ansi
    } else if args.json {
        ShotFormat::Json
    } else {
        ShotFormat::Plain
    };
    let trim = !args.no_trim;
    let Some(id) = args.id.as_deref() else {
        eprintln!("usage: looop _ screenshot <id> [--ansi|--json] [--no-trim]");
        return Ok(ExitCode::from(1));
    };
    let session = full_session(id);
    rt().block_on(paths.sessions().screenshot(Some(session), format, trim))?;
    Ok(ExitCode::SUCCESS)
}

// ============================================================================
// Session fleet — the in-process adapter over the `babysit` library.
// looop hands the library an explicit `Babysit` context (`paths.sessions()`),
// so the fleet is self-contained per profile: no $BABYSIT_DIR, no shared
// ~/.babysit, and bare session ids (the pulse is `pulse`).
// ============================================================================

/// The session id the pulse runs under when started as a service
/// (a bare `looop`). It is reserved: a worker can never take this id (see
/// `session::cmd_start_session`), so the single control-plane session can't
/// collide with a goal-named worker.
pub const PULSE_SESSION: &str = "pulse";

/// A process-wide multi-thread tokio runtime to drive babysit's async API.
/// looop is otherwise synchronous; async is confined to this boundary.
fn rt() -> &'static tokio::runtime::Runtime {
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
#[derive(Debug, Default)]
pub struct Session {
    pub id: String,
    pub state: String,
    pub alive: bool,
    pub exit_code: Option<i64>,
    /// RFC3339 timestamp of the session's last state change (babysit's
    /// `last_change`). Empty when babysit didn't report one. `watch` parses
    /// this to filter stale corpses out of the selector.
    pub last_change: String,
}

impl Session {
    /// The pulse session is the control loop, not a worker.
    pub fn is_pulse(&self) -> bool {
        self.id == PULSE_SESSION
    }

    /// How long since this session last changed state, if its `last_change`
    /// timestamp parses. `None` ⇒ undatable (treat as fresh — bias toward
    /// keeping it visible).
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
        last_change: info.last_change,
    }
}

/// List every session in this profile's fleet. Any failure yields an empty
/// list: the pulse degrades gracefully, never wedges.
pub fn list(paths: &Paths) -> Vec<Session> {
    match rt().block_on(paths.sessions().list_sessions()) {
        Ok(sessions) => sessions.into_iter().map(project).collect(),
        Err(_) => Vec::new(),
    }
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
                    .map(|age| age >= max_age)
                    .unwrap_or(false);
            if old {
                let _ = tokio::fs::remove_dir_all(&dir).await;
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
        }
    });
}

/// Does a session with this id exist in the fleet?
pub fn status_exists(paths: &Paths, session: &str) -> bool {
    list(paths).iter().any(|s| s.id == session)
}

/// `looop _ kill <id>` — terminate a session.
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

/// The worker side of detached spawn: looop was re-exec'd by babysit's detacher
/// as `looop run --detached-id <id> --root <dir> [--no-tty] [--timeout <ms>]
/// [--idle-timeout <ms>] [--size <CxR>] -- <cmd…>`. Parse that argv and hand off
/// to the library's headless supervisor, which blocks until the wrapped command
/// exits. The state root comes from `--root`, so the worker reconstructs THIS
/// fleet's context without reading any environment.
pub fn run_detached_worker(args: &[String]) -> anyhow::Result<i32> {
    use anyhow::Context;
    let mut id = None;
    let mut root = None;
    let mut no_tty = false;
    let mut timeout = None;
    let mut idle_timeout = None;
    let mut size = None;
    let mut cmd: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--detached-id" => id = it.next().cloned(),
            "--root" => root = it.next().cloned(),
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
    let root = root.context("looop run --detached-id: missing --root")?;
    let bs = ::babysit::Babysit::new(root);
    rt().block_on(bs.run(
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
pub(crate) fn suppress_stdout<T>(f: impl FnOnce() -> T) -> T {
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
pub(crate) fn suppress_stdout<T>(f: impl FnOnce() -> T) -> T {
    f()
}

/// Is a session currently alive?
pub fn is_alive(paths: &Paths, session: &str) -> bool {
    list(paths).iter().any(|s| s.id == session && s.alive)
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
}
