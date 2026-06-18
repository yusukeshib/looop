//! Start a worker session — the hands. `looop start-session <id> "<prompt>"`.
//! The pulse only LAUNCHES the agent (in the data dir) under babysit, detached;
//! it does NOT provision a workspace. Every worker gets the same contract
//! prepended so the pulse can't forget it (workers never notify — they flag and
//! wait; they sandbox their own code; the data dir's policy files are read-only).

use crate::config::Config;
use crate::paths::Paths;
use crate::{babysit, seed};
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
    mkdir -p claims && printf '{"session":"%s","name":"%s"}\n' "$BABYSIT_SESSION_ID" "<name>" > "claims/<name>.json"
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

    let session = format!("looop-{id}");

    if babysit::status_exists(&session) {
        if babysit::is_alive(&session) {
            eprintln!("start-session: session {session} is already running");
            return Ok(ExitCode::from(1));
        }
        babysit::prune(); // reuse the id held by a dead corpse
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
    let launch = format!(
        "cd {} && {cmd}",
        shell_quote(&paths.data_dir.to_string_lossy())
    );

    // Launch the worker detached, IN-PROCESS via the babysit library (no
    // `babysit` binary). babysit re-execs looop as the headless supervisor.
    babysit::spawn_detached(
        vec!["bash".to_string(), "-lc".to_string(), launch],
        &session,
    )?;

    println!(
        "started {session} (runner: {runner}, cwd: {})",
        paths.data_dir.display()
    );
    // `looop attach` scopes BABYSIT_DIR itself, so no BABYSIT_DIR= prefix needed.
    println!("  watch: looop attach {id}");
    Ok(ExitCode::SUCCESS)
}

/// Normalize a user-supplied worker id to its full session id. Accepts both the
/// short goal id (`triage`) and the full session id (`looop-triage`).
fn full_session(id: &str) -> String {
    if id.starts_with("looop-") {
        id.to_string()
    } else {
        format!("looop-{id}")
    }
}

/// `looop attach <id>` — attach the terminal to a worker session (in-process).
pub fn cmd_attach(_paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let Some(id) = args.first() else {
        eprintln!("usage: looop attach <id>");
        return Ok(ExitCode::from(1));
    };
    let code = babysit::attach(&full_session(id))?;
    Ok(ExitCode::from(code.clamp(0, 255) as u8))
}

/// `looop kill <id>` — terminate a worker session (in-process).
pub fn cmd_kill(_paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let Some(id) = args.first() else {
        eprintln!("usage: looop kill <id>");
        return Ok(ExitCode::from(1));
    };
    babysit::kill(&full_session(id))?;
    Ok(ExitCode::SUCCESS)
}

/// `looop flag <id> [message]` — raise a worker's attention flag (in-process).
pub fn cmd_flag(_paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let Some(id) = args.first() else {
        eprintln!("usage: looop flag <id> [message]");
        return Ok(ExitCode::from(1));
    };
    let message = if args.len() > 1 {
        Some(args[1..].join(" "))
    } else {
        None
    };
    babysit::flag(&full_session(id), message)?;
    Ok(ExitCode::SUCCESS)
}

/// `looop unflag <id>` — clear a worker's attention flag (in-process).
pub fn cmd_unflag(_paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let Some(id) = args.first() else {
        eprintln!("usage: looop unflag <id>");
        return Ok(ExitCode::from(1));
    };
    babysit::unflag(&full_session(id))?;
    Ok(ExitCode::SUCCESS)
}

/// `looop prune` — clear finished/dead worker corpses (in-process). The pulse
/// also does this every tick; this is the on-demand verb.
pub fn cmd_prune(_paths: &Paths, _args: &[String]) -> Result<ExitCode> {
    babysit::prune();
    println!("pruned finished worker sessions");
    Ok(ExitCode::SUCCESS)
}
