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
/// Accepts `pulse` to watch the loop itself (`looop-pulse`).
pub fn cmd_watch(_paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let Some(id) = positionals(args, &[]).first().cloned() else {
        eprintln!("usage: looop watch <id>   (e.g. looop watch pulse)");
        return Ok(ExitCode::from(1));
    };
    babysit::watch(&full_session(&id))?;
    Ok(ExitCode::SUCCESS)
}

/// `looop log <id> [--tail N] [--grep RE] [--raw] [--since N] [--follow|-f] [--json]`
pub fn cmd_log(_paths: &Paths, args: &[String]) -> Result<ExitCode> {
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
    babysit::log(&full_session(&id), tail, grep, raw, since, follow, json)?;
    Ok(ExitCode::SUCCESS)
}

/// `looop shot <id> [--ansi|--json] [--trim]` — render the current screen.
pub fn cmd_screenshot(_paths: &Paths, args: &[String]) -> Result<ExitCode> {
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
    babysit::screenshot(&full_session(&id), format, has(args, "--trim"))?;
    Ok(ExitCode::SUCCESS)
}

/// `looop send <id> <text...> [-n|--no-newline] [--json]`
pub fn cmd_send(_paths: &Paths, args: &[String]) -> Result<ExitCode> {
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
    babysit::send(
        &full_session(id),
        text.join(" "),
        newline,
        has(args, "--json"),
    )?;
    Ok(ExitCode::SUCCESS)
}

/// `looop key <id> <KEY...> [--json]` — send named keys (Enter, Up, C-c, …).
pub fn cmd_key(_paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let pos = positionals(args, &[]);
    let Some((id, keys)) = pos.split_first() else {
        eprintln!("usage: looop key <id> <KEY...>   (e.g. looop key foo Enter C-c)");
        return Ok(ExitCode::from(1));
    };
    if keys.is_empty() {
        eprintln!("usage: looop key <id> <KEY...>   (e.g. looop key foo Enter C-c)");
        return Ok(ExitCode::from(1));
    }
    babysit::key(&full_session(id), keys.to_vec(), has(args, "--json"))?;
    Ok(ExitCode::SUCCESS)
}

/// `looop expect <id> <REGEX> [--timeout DUR] [--from-now] [--raw] [--screen] [--json] [--since N]`
pub fn cmd_expect(_paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let pos = positionals(args, &["--timeout", "--since"]);
    let (Some(id), Some(pattern)) = (pos.first(), pos.get(1)) else {
        eprintln!(
            "usage: looop expect <id> <REGEX> [--timeout DUR] [--from-now] [--raw] [--screen] [--json]"
        );
        return Ok(ExitCode::from(1));
    };
    let timeout = val(args, "--timeout").unwrap_or_else(|| "30s".into());
    let since = val(args, "--since").and_then(|v| v.parse().ok());
    let code = babysit::expect(
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
pub fn cmd_wait(_paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let Some(id) = positionals(args, &["--timeout"]).first().cloned() else {
        eprintln!("usage: looop wait <id> [--timeout DUR]");
        return Ok(ExitCode::from(1));
    };
    let code = babysit::wait(&full_session(&id), val(args, "--timeout"))?;
    Ok(ExitCode::from(code.clamp(0, 255) as u8))
}

/// `looop wait-idle <id> [--settle DUR] [--timeout DUR]` — block until quiet.
pub fn cmd_wait_idle(_paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let Some(id) = positionals(args, &["--settle", "--timeout"])
        .first()
        .cloned()
    else {
        eprintln!("usage: looop wait-idle <id> [--settle DUR] [--timeout DUR]");
        return Ok(ExitCode::from(1));
    };
    let settle = val(args, "--settle").unwrap_or_else(|| "500ms".into());
    let timeout = val(args, "--timeout").unwrap_or_else(|| "30s".into());
    let code = babysit::wait_idle(&full_session(&id), settle, timeout)?;
    Ok(ExitCode::from(code.clamp(0, 255) as u8))
}

/// `looop resize <id> <COLSxROWS> [--json]` — resize a session's terminal.
pub fn cmd_resize(_paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let pos = positionals(args, &[]);
    let (Some(id), Some(size)) = (pos.first(), pos.get(1)) else {
        eprintln!("usage: looop resize <id> <COLSxROWS>   (e.g. looop resize foo 120x40)");
        return Ok(ExitCode::from(1));
    };
    babysit::resize(&full_session(id), size.clone(), has(args, "--json"))?;
    Ok(ExitCode::SUCCESS)
}

/// `looop restart <id> [--json]` — restart the wrapped command in a session.
pub fn cmd_restart(_paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let Some(id) = positionals(args, &[]).first().cloned() else {
        eprintln!("usage: looop restart <id>");
        return Ok(ExitCode::from(1));
    };
    babysit::restart(&full_session(&id), has(args, "--json"))?;
    Ok(ExitCode::SUCCESS)
}

/// `looop detach <id> [--json]` — force-detach any other terminal from a session.
pub fn cmd_detach(_paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let Some(id) = positionals(args, &[]).first().cloned() else {
        eprintln!("usage: looop detach <id>");
        return Ok(ExitCode::from(1));
    };
    babysit::detach(&full_session(&id), has(args, "--json"))?;
    Ok(ExitCode::SUCCESS)
}
