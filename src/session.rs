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
  an agent; only the pulse notifies.
- When you need a human decision / info / approval, do NOT guess — use ONLY this
  and then wait right there:
    "$LOOOP_BIN" flag __ID__ "<what you are waiting for / what you need to ask>"
  Once flagged, the human attaches over tmux to answer (the pulse turns the flag
  into a tmux window they can't miss).
- When the wait is resolved (you got your answer), unflag before continuing:
    "$LOOOP_BIN" unflag __ID__
- When the task is 100% complete and nothing is flagged, end your own session:
    "$LOOOP_BIN" kill __ID__
  (this lets the pulse prune the corpse). NEVER do this mid-task or while waiting
  on a human.
- LEASE (ONLY if the PLAYBOOK/goal tells you to claim this task) — announce
  ownership BEFORE any work so a tick or sibling can't duplicate/race you, and
  release it when done:
    mkdir -p claims && printf '{"session":"%s","name":"%s"}\n' "$LOOOP_SESSION_ID" "<name>" > "claims/<name>.json"
  (<name> and any extra fields are defined by the goal — e.g. one file per repo.)
  Delete claims/<name>.json the instant the task is fully done, right before the
  kill above. If you crash the pulse auto-reaps your claim; on a clean finish YOU
  delete it. NEVER sit/sleep/poll while holding a claim — act and move on.
- SINGLE-WRITER DATA DIR: the pulse (the tick AI) is the SOLE writer of the
  policy files — PLAYBOOK.md, goals/ and sensors/. By default you write ONLY to
  claims/ (your lease), reports/ (deliverables) and your own code sandbox. Do
  NOT edit PLAYBOOK/goals/sensors: a concurrent tick reads them every beat, so a
  racing writer tears the loop's state. If your task implies a policy change,
  write the proposal to reports/<id>.md and raise a flag — the human (or the
  next tick) applies it. EXCEPTION: if your task is explicitly a meta task (e.g.
  setup or playbook grooming), you MAY edit those files, but you MUST show the
  diff and `"$LOOOP_BIN" flag` for human approval BEFORE writing. When unsure whether
  your task is meta, treat the data dir as read-only and propose via reports/.
- WORKSPACE: you start in the loop data dir (read-only context for you, save the
  meta exception above). If your task touches a code repo, provision your OWN
  sandbox FIRST and cd into it — never edit code in the data dir:
    • if `box` is available:  box new __SESSION__ --repo <repo> && cd "$(box switch __SESSION__)"
    • otherwise (git):         git -C <local-clone> worktree add /tmp/__SESSION__ -b looop/__SESSION__ && cd /tmp/__SESSION__
  (the PLAYBOOK names the repos and which to prefer.)
- COST: when you end your session (right before `looop kill`), record this
  session's total LLM spend so the human can see it in `looop cost`. If you can
  determine your own USD cost for this run, log it:
    "$LOOOP_BIN" _cost session __ID__ __RUNNER__ <usd>
  (e.g. "$LOOOP_BIN" _cost session __ID__ __RUNNER__ 0.42). Skip only if you truly
  cannot determine the amount.
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
        eprintln!("usage: looop start-session <id> <prompt> [runner]");
        return Ok(ExitCode::from(1));
    };
    let Some(prompt) = args.get(1) else {
        eprintln!("missing prompt");
        return Ok(ExitCode::from(1));
    };

    let cfg = Config::load(paths)?;
    let runner = args
        .get(2)
        .cloned()
        .or_else(|| cfg.default_runner())
        .unwrap_or_default();

    let Some(tmpl) = cfg.runner_cmd(&runner, "interactive") else {
        eprintln!("start-session: unknown runner '{runner}'");
        return Ok(ExitCode::from(1));
    };

    // The worker's session id IS the goal id (no prefix — the fleet root is
    // looop-exclusive). `pulse` is reserved for the control loop, so a worker
    // can never collide with the 親玉.
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
        prune(paths); // reuse the id held by a dead corpse
    }

    // Prompt via file (avoids quoting hell; also a record of the ask), with the
    // contract prepended.
    let prompt_file = paths.prompts_dir().join(format!("{session}.md"));
    let contract = CONTRACT
        .replace("__SESSION__", &session)
        .replace("__ID__", id)
        .replace("__RUNNER__", &runner);
    fs::write(&prompt_file, format!("{contract}{prompt}\n"))?;

    let cmd = tmpl.replace("{{prompt_file}}", &prompt_file.to_string_lossy());

    // The worker runs in the DATA dir. The in-process spawner inherits the
    // current process cwd (babysit's Pane uses `std::env::current_dir`), so we
    // `cd` there inside the shell command instead of mutating looop's own cwd.
    // Export LOOOP_SESSION_ID so the worker knows its OWN session id (for its
    // lease claim, etc.) through a looop-branded var — looop never relies on
    // babysit's internal BABYSIT_SESSION_ID.
    let launch = format!(
        "export LOOOP_SESSION_ID={}; cd {} && {cmd}",
        shell_quote(&session),
        shell_quote(&paths.data_dir.to_string_lossy())
    );

    // Launch the worker detached, IN-PROCESS via the babysit library (no
    // `babysit` binary). babysit re-execs looop as the headless supervisor.
    spawn_detached(
        paths,
        vec!["bash".to_string(), "-lc".to_string(), launch],
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

/// `looop attach <id>` — attach the terminal to a worker session (in-process).
pub fn cmd_attach(paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let Some(id) = args.first() else {
        eprintln!("usage: looop attach <id>");
        return Ok(ExitCode::from(1));
    };
    let code = attach(paths, &full_session(id))?;
    Ok(ExitCode::from(code.clamp(0, 255) as u8))
}

/// `looop kill <id>` — terminate a worker session (in-process).
pub fn cmd_kill(paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let Some(id) = args.first() else {
        eprintln!("usage: looop kill <id>");
        return Ok(ExitCode::from(1));
    };
    kill(paths, &full_session(id))?;
    Ok(ExitCode::SUCCESS)
}

/// `looop flag <id> [message]` — raise a worker's attention flag (in-process).
pub fn cmd_flag(paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let Some(id) = args.first() else {
        eprintln!("usage: looop flag <id> [message]");
        return Ok(ExitCode::from(1));
    };
    let message = if args.len() > 1 {
        Some(args[1..].join(" "))
    } else {
        None
    };
    flag(paths, &full_session(id), message)?;
    Ok(ExitCode::SUCCESS)
}

/// `looop unflag <id>` — clear a worker's attention flag (in-process).
pub fn cmd_unflag(paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let Some(id) = args.first() else {
        eprintln!("usage: looop unflag <id>");
        return Ok(ExitCode::from(1));
    };
    unflag(paths, &full_session(id))?;
    Ok(ExitCode::SUCCESS)
}

/// `looop prune` — clear finished/dead worker corpses (in-process). The pulse
/// also does this every tick; this is the on-demand verb.
pub fn cmd_prune(paths: &Paths, _args: &[String]) -> Result<ExitCode> {
    prune(paths);
    println!("pruned finished worker sessions");
    Ok(ExitCode::SUCCESS)
}

// ---- thin argv helpers (looop parses argv by hand, no clap) ----------------

/// Is `flag` present anywhere in `args`?
fn has(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

/// Value of `--flag value` or `--flag=value`, if present.
fn val(args: &[String], flag: &str) -> Option<String> {
    let eq = format!("{flag}=");
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == flag {
            return it.next().cloned();
        }
        if let Some(v) = a.strip_prefix(&eq) {
            return Some(v.to_string());
        }
    }
    None
}

/// Positional (non-flag) args, skipping the value that follows each value-taking
/// flag in `value_flags`. `--flag=value` forms are dropped as flags too.
fn positionals(args: &[String], value_flags: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut skip = false;
    for a in args {
        if skip {
            skip = false;
            continue;
        }
        if a.starts_with('-') && a.len() > 1 {
            if value_flags.contains(&a.as_str()) {
                skip = true;
            }
            continue;
        }
        out.push(a.clone());
    }
    out
}

/// `looop watch <id>` — follow a session's output read-only (Ctrl-C to stop).
/// Accepts `pulse` to watch the loop itself.
pub fn cmd_watch(paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let Some(id) = positionals(args, &[]).first().cloned() else {
        eprintln!("usage: looop watch <id>   (e.g. looop watch pulse)");
        return Ok(ExitCode::from(1));
    };
    watch(paths, &full_session(&id))?;
    Ok(ExitCode::SUCCESS)
}

/// `looop log <id> [--tail N] [--grep RE] [--raw] [--since N] [--follow|-f] [--json]`
pub fn cmd_log(paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let pos = positionals(args, &["--tail", "--grep", "--since"]);
    let Some(id) = pos.first().cloned() else {
        eprintln!(
            "usage: looop log <id> [--tail N] [--grep RE] [--raw] [--since N] [--follow] [--json]"
        );
        return Ok(ExitCode::from(1));
    };
    let tail = val(args, "--tail").and_then(|v| v.parse().ok());
    let grep = val(args, "--grep");
    let since = val(args, "--since").and_then(|v| v.parse().ok());
    let raw = has(args, "--raw");
    let follow = has(args, "--follow") || has(args, "-f");
    let json = has(args, "--json");
    log(
        paths,
        &full_session(&id),
        tail,
        grep,
        raw,
        since,
        follow,
        json,
    )?;
    Ok(ExitCode::SUCCESS)
}

/// `looop shot <id> [--ansi|--json] [--trim]` — render the current screen.
pub fn cmd_screenshot(paths: &Paths, args: &[String]) -> Result<ExitCode> {
    use ::babysit::cli::ShotFormat;
    let Some(id) = positionals(args, &["--format"]).first().cloned() else {
        eprintln!("usage: looop shot <id> [--ansi|--json] [--trim]");
        return Ok(ExitCode::from(1));
    };
    let format = match val(args, "--format").as_deref() {
        Some("ansi") => ShotFormat::Ansi,
        Some("json") => ShotFormat::Json,
        Some("plain") | None => {
            if has(args, "--json") {
                ShotFormat::Json
            } else if has(args, "--ansi") {
                ShotFormat::Ansi
            } else {
                ShotFormat::Plain
            }
        }
        Some(other) => {
            eprintln!("looop shot: unknown --format '{other}' (plain|ansi|json)");
            return Ok(ExitCode::from(1));
        }
    };
    screenshot(paths, &full_session(&id), format, has(args, "--trim"))?;
    Ok(ExitCode::SUCCESS)
}

/// `looop send <id> <text...> [-n|--no-newline] [--json]`
pub fn cmd_send(paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let pos = positionals(args, &[]);
    let Some((id, text)) = pos.split_first() else {
        eprintln!("usage: looop send <id> <text...> [-n] [--json]");
        return Ok(ExitCode::from(1));
    };
    if text.is_empty() {
        eprintln!("usage: looop send <id> <text...> [-n] [--json]");
        return Ok(ExitCode::from(1));
    }
    let newline = !(has(args, "-n") || has(args, "--no-newline"));
    send(
        paths,
        &full_session(id),
        text.join(" "),
        newline,
        has(args, "--json"),
    )?;
    Ok(ExitCode::SUCCESS)
}

/// `looop key <id> <KEY...> [--json]` — send named keys (Enter, Up, C-c, …).
pub fn cmd_key(paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let pos = positionals(args, &[]);
    let Some((id, keys)) = pos.split_first() else {
        eprintln!("usage: looop key <id> <KEY...>   (e.g. looop key foo Enter C-c)");
        return Ok(ExitCode::from(1));
    };
    if keys.is_empty() {
        eprintln!("usage: looop key <id> <KEY...>   (e.g. looop key foo Enter C-c)");
        return Ok(ExitCode::from(1));
    }
    key(paths, &full_session(id), keys.to_vec(), has(args, "--json"))?;
    Ok(ExitCode::SUCCESS)
}

/// `looop expect <id> <REGEX> [--timeout DUR] [--from-now] [--raw] [--screen] [--json] [--since N]`
pub fn cmd_expect(paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let pos = positionals(args, &["--timeout", "--since"]);
    let (Some(id), Some(pattern)) = (pos.first(), pos.get(1)) else {
        eprintln!(
            "usage: looop expect <id> <REGEX> [--timeout DUR] [--from-now] [--raw] [--screen] [--json]"
        );
        return Ok(ExitCode::from(1));
    };
    let timeout = val(args, "--timeout").unwrap_or_else(|| "30s".into());
    let since = val(args, "--since").and_then(|v| v.parse().ok());
    let code = expect(
        paths,
        &full_session(id),
        pattern.clone(),
        timeout,
        since,
        has(args, "--from-now"),
        has(args, "--raw"),
        has(args, "--screen"),
        has(args, "--json"),
    )?;
    Ok(ExitCode::from(code.clamp(0, 255) as u8))
}

/// `looop wait <id> [--timeout DUR]` — block until exit; returns its exit code.
pub fn cmd_wait(paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let Some(id) = positionals(args, &["--timeout"]).first().cloned() else {
        eprintln!("usage: looop wait <id> [--timeout DUR]");
        return Ok(ExitCode::from(1));
    };
    let code = wait(paths, &full_session(&id), val(args, "--timeout"))?;
    Ok(ExitCode::from(code.clamp(0, 255) as u8))
}

/// `looop wait-idle <id> [--settle DUR] [--timeout DUR]` — block until quiet.
pub fn cmd_wait_idle(paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let Some(id) = positionals(args, &["--settle", "--timeout"])
        .first()
        .cloned()
    else {
        eprintln!("usage: looop wait-idle <id> [--settle DUR] [--timeout DUR]");
        return Ok(ExitCode::from(1));
    };
    let settle = val(args, "--settle").unwrap_or_else(|| "500ms".into());
    let timeout = val(args, "--timeout").unwrap_or_else(|| "30s".into());
    let code = wait_idle(paths, &full_session(&id), settle, timeout)?;
    Ok(ExitCode::from(code.clamp(0, 255) as u8))
}

/// `looop resize <id> <COLSxROWS> [--json]` — resize a session's terminal.
pub fn cmd_resize(paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let pos = positionals(args, &[]);
    let (Some(id), Some(size)) = (pos.first(), pos.get(1)) else {
        eprintln!("usage: looop resize <id> <COLSxROWS>   (e.g. looop resize foo 120x40)");
        return Ok(ExitCode::from(1));
    };
    resize(paths, &full_session(id), size.clone(), has(args, "--json"))?;
    Ok(ExitCode::SUCCESS)
}

/// `looop restart <id> [--json]` — restart the wrapped command in a session.
pub fn cmd_restart(paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let Some(id) = positionals(args, &[]).first().cloned() else {
        eprintln!("usage: looop restart <id>");
        return Ok(ExitCode::from(1));
    };
    restart(paths, &full_session(&id), has(args, "--json"))?;
    Ok(ExitCode::SUCCESS)
}

/// `looop detach <id> [--json]` — force-detach any other terminal from a session.
pub fn cmd_detach(paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let Some(id) = positionals(args, &[]).first().cloned() else {
        eprintln!("usage: looop detach <id>");
        return Ok(ExitCode::from(1));
    };
    detach(paths, &full_session(&id), has(args, "--json"))?;
    Ok(ExitCode::SUCCESS)
}

// ============================================================================
// Session fleet — the in-process adapter over the `babysit` library.
// looop hands the library an explicit `Babysit` context (`paths.sessions()`),
// so the fleet is self-contained per profile: no $BABYSIT_DIR, no shared
// ~/.babysit, and bare session ids (the pulse is `pulse`).
// ============================================================================

/// The session id the pulse (親玉) runs under when started as a service
/// (`looop up`). It is reserved: a worker can never take this id (see
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
    pub note: Option<String>,
}

impl Session {
    /// The pulse session is the 親玉, not a worker.
    pub fn is_pulse(&self) -> bool {
        self.id == PULSE_SESSION
    }
    /// True when the session has raised a flag (a non-empty note).
    pub fn flagged(&self) -> bool {
        self.note.as_deref().map(|n| !n.is_empty()).unwrap_or(false)
    }
}

fn project(info: ::babysit::SessionInfo) -> Session {
    Session {
        id: info.id,
        state: info.state,
        alive: info.alive,
        exit_code: info.exit_code.map(|c| c as i64),
        note: info.note,
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

/// Worker sessions only — the pulse (親玉) is excluded. Everything that reasons
/// about "the fleet the pulse manages" (cadence, world hash, tick prompt,
/// status, flag-surfacing) uses this so the pulse never counts itself.
pub fn list_workers(paths: &Paths) -> Vec<Session> {
    list(paths).into_iter().filter(|s| !s.is_pulse()).collect()
}

/// Clear exited/dead-owner corpses, IN-PROCESS and SILENTLY. babysit's own
/// `prune` prints (it is a CLI handler), which would pollute looop's pretty
/// output, so we drive the same decision over the library's lower-level
/// session/context API. The fleet root is looop-exclusive, so every corpse here
/// is ours. Best-effort: any error is ignored.
pub fn prune(paths: &Paths) {
    use ::babysit::session::{self, State};
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
            let dead = match status.as_ref().map(|s| s.state) {
                Some(State::Exited | State::Killed) => true,
                Some(State::Starting | State::Running) if !alive => true,
                None if !alive => true,
                _ => false,
            };
            if dead {
                let _ = tokio::fs::remove_dir_all(bs.session_dir(&id)).await;
            }
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

/// `looop flag <id> [msg]` — raise a session's attention flag.
pub fn flag(paths: &Paths, session: &str, message: Option<String>) -> anyhow::Result<()> {
    rt().block_on(
        paths
            .sessions()
            .flag(Some(session.to_string()), message, false),
    )
}

/// `looop unflag <id>` — clear a session's attention flag.
pub fn unflag(paths: &Paths, session: &str) -> anyhow::Result<()> {
    rt().block_on(paths.sessions().unflag(Some(session.to_string()), false))
}

/// `looop attach <id>` — attach the terminal to a session; returns its exit code.
pub fn attach(paths: &Paths, session: &str) -> anyhow::Result<i32> {
    let bs = paths.sessions();
    rt().block_on(::babysit::attach::attach(&bs, Some(session.to_string())))
}

/// `looop detach <id>` — force-detach any other terminal attached to a session.
pub fn detach(paths: &Paths, session: &str, json: bool) -> anyhow::Result<()> {
    let bs = paths.sessions();
    rt().block_on(::babysit::attach::detach(
        &bs,
        Some(session.to_string()),
        json,
    ))
}

/// `looop watch <id>` — follow a session's output read-only (tail -f): the full
/// log so far, then live output until the session exits. The non-interactive
/// twin of `attach`.
pub fn watch(paths: &Paths, session: &str) -> anyhow::Result<()> {
    rt().block_on(paths.sessions().log(
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
    paths: &Paths,
    session: &str,
    tail: Option<usize>,
    grep: Option<String>,
    raw: bool,
    since: Option<u64>,
    follow: bool,
    json: bool,
) -> anyhow::Result<()> {
    rt().block_on(paths.sessions().log(
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
    paths: &Paths,
    session: &str,
    format: ::babysit::cli::ShotFormat,
    trim: bool,
) -> anyhow::Result<()> {
    rt().block_on(
        paths
            .sessions()
            .screenshot(Some(session.to_string()), format, trim),
    )
}

/// `looop send <id> <text>` — type text into a session's stdin.
pub fn send(
    paths: &Paths,
    session: &str,
    text: String,
    newline: bool,
    json: bool,
) -> anyhow::Result<()> {
    rt().block_on(
        paths
            .sessions()
            .send(Some(session.to_string()), text, newline, json),
    )
}

/// `looop key <id> <KEY...>` — send named keys (Enter, Up, C-c, …) to a session.
pub fn key(paths: &Paths, session: &str, keys: Vec<String>, json: bool) -> anyhow::Result<()> {
    rt().block_on(paths.sessions().key(Some(session.to_string()), keys, json))
}

/// `looop expect <id> <REGEX>` — block until a regex appears; exit 124 on timeout.
#[allow(clippy::too_many_arguments)]
pub fn expect(
    paths: &Paths,
    session: &str,
    pattern: String,
    timeout: String,
    since: Option<u64>,
    from_now: bool,
    raw: bool,
    screen: bool,
    json: bool,
) -> anyhow::Result<i32> {
    rt().block_on(paths.sessions().expect(
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
pub fn wait(paths: &Paths, session: &str, timeout: Option<String>) -> anyhow::Result<i32> {
    rt().block_on(paths.sessions().wait(Some(session.to_string()), timeout))
}

/// `looop wait-idle <id>` — block until output is quiet for `settle`.
pub fn wait_idle(
    paths: &Paths,
    session: &str,
    settle: String,
    timeout: String,
) -> anyhow::Result<i32> {
    rt().block_on(
        paths
            .sessions()
            .wait_idle(Some(session.to_string()), settle, timeout),
    )
}

/// `looop resize <id> <COLSxROWS>` — resize a session's terminal.
pub fn resize(paths: &Paths, session: &str, size: String, json: bool) -> anyhow::Result<()> {
    rt().block_on(
        paths
            .sessions()
            .resize(Some(session.to_string()), size, json),
    )
}

/// `looop restart <id>` — restart the wrapped command in a session.
pub fn restart(paths: &Paths, session: &str, json: bool) -> anyhow::Result<()> {
    rt().block_on(paths.sessions().restart(Some(session.to_string()), json))
}

/// `looop ls [--json] [--watch] [--interval <dur>]` — render the fleet table
/// IN-PROCESS via babysit's own list renderer.
pub fn ls(paths: &Paths, json: bool, watch: bool, interval: String) -> anyhow::Result<()> {
    rt().block_on(paths.sessions().list(json, watch, interval))
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
pub fn is_alive(paths: &Paths, session: &str) -> bool {
    list(paths).iter().any(|s| s.id == session && s.alive)
}

/// Any looop worker currently in flight?
pub fn any_worker_alive(paths: &Paths) -> bool {
    list_workers(paths).iter().any(|s| s.alive)
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
    fn pulse_is_recognized() {
        assert!(sess(PULSE_SESSION, None).is_pulse());
        assert!(!sess("triage", None).is_pulse());
    }

    #[test]
    fn flagged_iff_nonempty_note() {
        assert!(sess("x", Some("help")).flagged());
        assert!(!sess("x", Some("")).flagged());
        assert!(!sess("x", None).flagged());
    }
}
