//! Start a worker session — the hands. `looop start-session <id> "<prompt>"`.
//! The pulse only LAUNCHES the agent (in the data dir) under babysit, detached;
//! it does NOT provision a workspace. Every worker gets the same contract
//! prepended so the pulse can't forget it (workers never notify — they flag and
//! wait; they sandbox their own code; the data dir's policy files are read-only).

use crate::config::Config;
use crate::paths::Paths;
use crate::seed;
use anyhow::Result;
use std::process::ExitCode;

/// Single-quote a string for safe inclusion in a `bash -lc` command line
/// (wraps in `'…'`, escaping embedded single quotes as `'\''`).
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The outcome of resolving the worker launch command.
#[derive(Debug)]
struct WorkerCmd {
    /// The concrete launch command (`{{prompt_file}}` substituted).
    cmd: String,
    /// True when a per-worker `--command` override replaced the config
    /// template (reported in the banner/journal so the override is auditable).
    overridden: bool,
}

/// Resolve the concrete worker launch command.
///
/// Precedence: the per-worker `override_cmd` (from `--command` /
/// `start_worker.command`) replaces the config template WHOLESALE — looop does
/// no splicing, the override IS the full command. Without an override, the
/// config `worker_command` template is used as-is.
///
/// Either way, exactly ONE placeholder exists — `{{prompt_file}}` — and it is
/// REQUIRED (the prompt file is the worker's only brief channel; a worker has
/// no stdin). looop itself has NO runner vocabulary: how to launch a worker is
/// decided at `looop init` time, per-worker variation is the override above,
/// and the policy for WHEN to override lives in the PLAYBOOK, not in looop.
///
/// REMOVED: the `{{model}}`/`{{thinking}}` placeholders and their
/// `worker_model`/`worker_thinking` config keys. A template still carrying
/// them would silently launch a broken command if expanded empty (`--model
/// --thinking …` — the next flag becomes the value), so it is REFUSED with a
/// pointer to `looop init` instead.
fn build_worker_cmd(
    tmpl: &str,
    override_cmd: Option<&str>,
    prompt_file: &str,
) -> Result<WorkerCmd, String> {
    let (raw, overridden, label) = match override_cmd.map(str::trim).filter(|s| !s.is_empty()) {
        Some(over) => (over, true, "--command override"),
        None => (tmpl, false, "worker_command"),
    };
    for gone in ["{{model}}", "{{thinking}}"] {
        if raw.contains(gone) {
            let hint = if overridden {
                "bake the value directly into the --command string instead"
            } else {
                "bake the value into the command instead: re-run `looop init` or edit the config"
            };
            return Err(format!(
                "{label} still uses the removed {gone} placeholder (and its \
                 worker_model/worker_thinking config keys are gone) — {hint}"
            ));
        }
    }
    if !raw.contains("{{prompt_file}}") {
        return Err(format!(
            "{label} must contain the {{{{prompt_file}}}} placeholder \
             (the prompt file is the worker's brief): {raw:?}"
        ));
    }
    // Shared, quote-aware substitution (same as the tick path): the path is
    // shell-quoted, and a pre-quoted `"{{prompt_file}}"` / `'{{prompt_file}}'`
    // template doesn't end up double-quoted.
    Ok(WorkerCmd {
        cmd: crate::runner::substitute_prompt_file(raw, prompt_file),
        overridden,
    })
}

const CONTRACT: &str = r#"# ⚑ WORKER CONTRACT (auto-injected — must obey)
- Never send notifications (no terminal-notifier or any OS notification). You are
  an agent; surface anything a human must see by ASKing (below) — the human sees
  it through whatever client they run.
- When you need a human decision / info / approval, do NOT guess — ASK. Two modes:
  • QUICK question (answer likely within the hour) — BLOCK on it:
      answer=$("$LOOOP_BIN" ask __ID__ --prompt "<what you need to know>")
    (optionally --ref reports/x.md and/or --options a,b). Use $answer, continue.
    You do NOT need a terminal, stdin, or attach — just call it and read its
    output. Ask once per question; it returns only when answered.
  • LONG wait (the human may take hours or days) — CHECKPOINT and DETACH:
      1) write your FULL state (done / remaining / how to continue) to
         reports/__ID__-checkpoint.md
      2) "$LOOOP_BIN" ask __ID__ --detach --prompt "…" --ref reports/__ID__-checkpoint.md
      3) end your session: "$LOOOP_BIN" kill __ID__
    Do NOT sit idle waiting: when the human answers, looop dispatches a FRESH
    worker with the answer and your checkpoint. Your exit is by design, not a
    failure. When unsure which mode, prefer detach — idling is the expensive
    mistake.
- STEERING: the human can queue mid-task course corrections for you. Between
  major steps (and BEFORE any big/irreversible-adjacent step), run:
    "$LOOOP_BIN" told
  It prints and consumes any pending steering messages (nothing = no output);
  obey them immediately. Steering also rides along automatically on every ask
  answer, prefixed "[steering from the human …]".
- When the task is 100% complete and nothing is waiting, end your own session:
    "$LOOOP_BIN" kill __ID__
  (this lets the pulse prune the corpse). NEVER do this mid-task or while waiting
  on a human.
- LEASE (ONLY if the PLAYBOOK/goal tells you to claim this task) — announce
  ownership BEFORE any work so a tick or sibling can't duplicate/race you:
    "$LOOOP_BIN" claim <name>   # atomic test-and-set; <name> defined by the goal (e.g. one per repo)
  This EXITS NON-ZERO if a live session already holds <name> — if so, do NOT
  proceed: flag the human or pick other work, never race the holder. Release it
  the instant the task is fully done, right before the kill above:
    "$LOOOP_BIN" unclaim <name>
  If you crash the pulse auto-reaps your claim; on a clean finish YOU release it.
  NEVER sit/sleep/poll while holding a claim — act and move on.
- SINGLE-WRITER DATA DIR: the pulse (the tick AI) is the SOLE writer of the
  policy files — PLAYBOOK.md, goals/ and sensors/. By default you write ONLY to
  claims/ (your lease), reports/ (deliverables) and your own code sandbox. Do
  NOT edit PLAYBOOK/goals/sensors: a concurrent tick reads them every beat, so a
  racing writer tears the loop's state. If your task implies a policy change,
  write the proposal to reports/<id>.md and raise an ask — the human (or the
  next tick) applies it. EXCEPTION: if your task is explicitly a meta task (e.g.
  setup or playbook grooming), you MAY edit those files, but you MUST show the
  diff and ASK (above) for human approval BEFORE writing. When unsure whether
  your task is meta, treat the data dir as read-only and propose via reports/.
- WORKSPACE: you start in the loop data dir (read-only context for you, save the
  meta exception above). If your task touches a code repo, provision your OWN
  sandbox FIRST and cd into it — never edit code in the data dir:
    git -C <local-clone> worktree add /tmp/__SESSION__ -b looop/__SESSION__ && cd /tmp/__SESSION__
  (the PLAYBOOK names the repos and which to prefer.)
- DELIVERABLES: write any report / artifact a human will read into the data dir's
  reports/ folder (e.g. reports/<id>.md). That dir PERSISTS across ticks. NEVER
  write deliverables to snapshots/ — the pulse OWNS that dir and prunes/rewrites
  its files on every beat, so anything you leave there is destroyed. Reference
  the reports/ path in your flag note so I know where to look.

---

"#;

/// The result of a worker launch: the process exit code plus whether a
/// per-worker `--command` override replaced the config template (reported in
/// the journal so overrides stay auditable).
pub struct StartOutcome {
    pub code: ExitCode,
    pub overridden: bool,
    /// WHY the start was refused, when it was (`code != SUCCESS`). Carried
    /// back to the executor so the failed move's LAST FAILURE feedback names
    /// the cause (fleet cap, duplicate id, bad template, …) instead of a
    /// generic "failed" — without it the decider repeats the same refused
    /// move blind. The CLI stderr line stays alongside for the human path.
    pub reason: Option<String>,
}

impl StartOutcome {
    fn refused(reason: String) -> Self {
        StartOutcome {
            code: ExitCode::from(1),
            overridden: false,
            reason: Some(reason),
        }
    }
}

/// Start one worker session. Structured as VALIDATE-THEN-COMMIT: every check
/// (resume context, id, prompt, config, fleet cap, duplicate id, launch
/// command) runs BEFORE the first side effect, so a refusal never leaves a
/// half-started worker behind (in particular: no stale verify record from a
/// start that failed a later step — the old shape stored the verify before
/// resolving the launch command).
pub fn cmd_start_session(
    paths: &Paths,
    id: &str,
    prompt: &str,
    command: Option<&str>,
    verify: Option<&str>,
    resume: Option<&str>,
) -> Result<StartOutcome> {
    seed::ensure_dirs(paths)?;

    // One shape for every refusal: stderr for the human, the same text in the
    // outcome for the executor (whose bail! feeds record_failure).
    let refuse = |msg: String| {
        eprintln!("start-session: {msg}");
        Ok(StartOutcome::refused(msg))
    };

    // ---- VALIDATION PHASE (no side effects) --------------------------------

    // Resolve the RESUME context FIRST: an unknown / not-yet-answered ask id
    // is a decider mistake and must fail the move loudly (LAST FAILURE names
    // it) before anything is spawned. The pair is ARCHIVED just before the
    // spawn (archive-then-spawn, so a crash between the two can't re-dispatch
    // the same resume) and UN-ARCHIVED if the spawn fails, below.
    let resume_block = match resume {
        Some(ask_id) => match crate::mailbox::resume_context(paths, ask_id) {
            Ok(block) => Some(block),
            Err(e) => return refuse(e.to_string()),
        },
        None => None,
    };

    // The id becomes both a path segment (the prompt file) and the session id,
    // so reject traversal/dotfile/separator ids up front — the same guard the
    // executor applies to goal/sensor ids.
    if let Err(e) = crate::util::safe_segment("worker id", id) {
        return refuse(e.to_string());
    }
    if prompt.is_empty() {
        return refuse("missing prompt".to_string());
    }

    let cfg = Config::load(paths)?;
    let runner = cfg.runner_label();
    let Some(tmpl) = cfg.runner_cmd("worker_command") else {
        return refuse("no `worker_command` configured".to_string());
    };

    // The worker's session id IS the goal id (no prefix — the fleet root is
    // looop-exclusive). `pulse` is reserved for the control loop, so a worker
    // can never collide with the pulse.
    if id == PULSE_SESSION {
        return refuse(format!("'{id}' is reserved for the pulse; pick another id"));
    }
    let session = id.to_string();

    // Fleet-size ceiling (`LOOOP_MAX_WORKERS`, default 8; 0 disables): one move
    // per beat bounds the spawn RATE but not the standing fleet — without this,
    // a pathological goal/playbook can accumulate a heavy agent per beat
    // indefinitely. The refusal reaches the decider as a failed move (LAST
    // FAILURE names the cap via `StartOutcome.reason`), so it can kill or wait
    // instead of piling on.
    let cap: usize = crate::util::env_knob("LOOOP_MAX_WORKERS").unwrap_or(8);
    if cap != 0 {
        // Check-then-spawn is a TOCTOU race in principle (two concurrent
        // starts could both pass the count), but starts are issued by the
        // single decider one-move-per-beat, so the race is benign — not worth
        // a lock.
        let live = list_workers(paths).iter().filter(|w| w.alive).count();
        if live >= cap {
            return refuse(format!(
                "{live} live workers — at the fleet cap (LOOOP_MAX_WORKERS={cap}); \
                 kill or wait out an existing worker first"
            ));
        }
    }

    // Check-then-act (exists → alive → reap-in-commit-phase) races a
    // concurrent start of the same id in principle; the single-decider
    // dispatch model makes it benign (the spawn below would fail loudly on a
    // true collision).
    let id_taken = status_exists(paths, &session);
    if id_taken && is_alive(paths, &session) {
        return refuse(format!("session {session} is already running"));
    }

    // Resolve the launch command LAST among the checks (it needs only the
    // prompt file's PATH, not its contents): a per-worker `--command` override
    // replaces the template wholesale; otherwise the template. Only
    // `{{prompt_file}}` is substituted (looop has no runner vocabulary).
    let prompt_file = paths.prompts_dir().join(format!("{session}.md"));
    let expanded = match build_worker_cmd(&tmpl, command, &prompt_file.to_string_lossy()) {
        Ok(e) => e,
        Err(msg) => return refuse(msg),
    };
    let cmd = expanded.cmd;

    // ---- COMMIT PHASE (side effects; all checks passed) ---------------------

    if id_taken {
        reap(paths, &session); // reuse the id held by a dead corpse (targeted)
    }

    // GENERATION BOUNDARY for tells: a worker id is its goal id and gets
    // reused, and a tell can only be queued for a LIVE worker (cmd_tell
    // refuses corpses) — so any tell still pending here was addressed to the
    // PREVIOUS, now-dead worker under this id. Discard before the spawn:
    // delivering it to the new generation (via `told` or an ask-answer
    // piggyback) would apply stale steering to a worker with a different
    // brief. reap/prune/kill also discard (hygiene), but this pre-spawn point
    // holds even when the corpse was removed by some other path — and it is
    // race-safe: cmd_tell can't queue for this id until the worker is alive,
    // i.e. after the spawn below.
    crate::mailbox::discard_tells(paths, &session);

    // Persist the post-condition BEFORE the spawn: a verify declared for a
    // worker that dies instantly must still be checked on the next beat. No
    // verify ⇒ clear any stale one left by a prior corpse with this id.
    // Rolled back (cleared) below if a later commit step fails — a stale
    // verify record for a worker that never launched would fail the NEXT
    // start of this id for the wrong reason.
    match verify {
        Some(v) if !v.trim().is_empty() => crate::verify::store(paths, &session, v)?,
        _ => crate::verify::clear(paths, &session),
    }

    // Prompt via file (avoids quoting hell; also a record of the ask), with the
    // contract prepended.
    let contract = CONTRACT
        .replace("__SESSION__", &session)
        .replace("__ID__", id);
    let resume_part = resume_block.as_deref().unwrap_or("");
    // Atomic write (temp + fsync + rename), not fs::write: the worker command
    // reads this file via `$(cat {{prompt_file}})`, and on an id REUSE a
    // truncate-then-write could expose a torn brief to a concurrent reader —
    // rename-publish means the path only ever holds a complete prompt.
    if let Err(e) = crate::util::write_atomic(
        &prompt_file,
        format!("{contract}{resume_part}{prompt}\n").as_bytes(),
    ) {
        crate::verify::clear(paths, &session); // no stale verify for a no-show worker
        return Err(e.into());
    }

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

    // ARCHIVE-THEN-SPAWN: consume the answered resume pair BEFORE launching.
    // The old order (spawn, then archive) re-dispatched the same resume when a
    // crash landed between the two — the sys-asks resume signal stayed hot with
    // a worker already running. Archiving first makes a crash lose at most one
    // dispatch (recoverable: the record stays under asks/archive/); if the
    // spawn FAILS we un-archive so the resume signal returns.
    if let Some(ask_id) = resume {
        crate::mailbox::archive_pair(paths, ask_id);
    }

    // Launch the worker detached, IN-PROCESS via the babysit library (no
    // `babysit` binary). babysit re-execs looop as the headless supervisor.
    // `-c`, not `-lc`: a non-login shell sources no rc files, so the worker
    // launches against looop's inherited environment instead of re-running the
    // operator's login profile (hermetic + cheaper). The runner template itself
    // is still a shell string ($(cat ...), &&), so the shell stays.
    if let Err(e) = spawn_detached(
        paths,
        vec!["bash".to_string(), "-c".to_string(), launch],
        &session,
    ) {
        if let Some(ask_id) = resume {
            crate::mailbox::unarchive_pair(paths, ask_id);
        }
        crate::verify::clear(paths, &session); // no stale verify for a no-show worker
        return Err(e);
    }

    // Label the banner with what actually launched: the override's first
    // token when a per-worker `--command` replaced the template, else the
    // configured runner. Flag the override so it is visible at a glance.
    let (runner, override_note) = if expanded.overridden {
        let tok = cmd
            .split_whitespace()
            .next()
            .unwrap_or("runner")
            .to_string();
        (tok, ", command override")
    } else {
        (runner, "")
    };
    println!(
        "started {session} (runner: {runner}{override_note}, cwd: {})",
        paths.data_dir.display()
    );
    println!("  peek: looop screenshot {id}");
    Ok(StartOutcome {
        code: ExitCode::SUCCESS,
        overridden: expanded.overridden,
        reason: None,
    })
}

/// Normalize a user-supplied worker id to its full session id. Accepts both the
/// short goal id (`triage`) and the legacy full session id (`looop-triage`).
fn full_session(paths: &Paths, id: &str) -> String {
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

/// The pulse is the control loop, NOT a worker: refuse worker-management verbs
/// aimed at it so a stray `looop kill pulse` can't decapitate
/// or hijack the loop. Observe it with `looop screenshot pulse`; control it
/// with `looop up`/`down`. Returns true (and prints guidance) when `session` is
/// the reserved pulse id — the caller should then bail with a non-zero code.
fn reject_pulse(session: &str, verb: &str) -> bool {
    if session == PULSE_SESSION {
        eprintln!(
            "looop {verb}: '{PULSE_SESSION}' is the control loop, not a worker — observe it with \
             `looop screenshot {PULSE_SESSION}`, control it with `looop up` / `looop down`"
        );
        true
    } else {
        false
    }
}

/// `looop kill <id>` — terminate a worker session (in-process). Internal
/// worker self-control callback (CONTRACT), not a human-facing verb.
/// `looop worker list [--json] [--all] [--watch [--interval N]]` — the fleet
/// with its health reading (the same projection the `sys-sessions` snapshot
/// feeds the decider): id, state, health, idle (since last PTY output), uptime,
/// and how long a pending ask has been waiting. Live workers only by default;
/// `--all` includes corpses. `--watch` re-renders every `--interval` seconds
/// (default 2) until Ctrl-C — the humble replacement for a fleet TUI.
pub fn cmd_worker_list(
    paths: &Paths,
    json: bool,
    all: bool,
    watch: bool,
    interval: u64,
) -> Result<ExitCode> {
    let mut prev_lines = 0usize;
    loop {
        let fleet: Vec<crate::sensor::WorkerHealth> = crate::sensor::fleet_health(paths)
            .into_iter()
            .filter(|w| all || w.alive)
            .collect();
        if json {
            let rows: Vec<serde_json::Value> = fleet
                .iter()
                .map(|w| {
                    serde_json::json!({
                        "id": w.id,
                        "state": w.state,
                        "alive": w.alive,
                        "exit_code": w.exit_code,
                        "health": w.health,
                        "idle_s": w.idle_s,
                        "uptime_s": w.uptime_s,
                        "ask_age_s": w.ask_age_s,
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&rows)?);
            return Ok(ExitCode::SUCCESS);
        }
        let mut lines = 0usize;
        // Clip every line to the terminal width in watch mode: the in-place
        // repaint below counts LOGICAL lines, so a row that wraps would throw
        // off the cursor-up arithmetic and leave stale header lines behind.
        // Re-read the width each tick so a resize self-heals.
        let clip = if watch {
            crate::util::term_cols()
        } else {
            None
        };
        if watch {
            // Repaint IN PLACE, not full-screen: move the cursor up over the
            // block we drew last tick and clear from there down (\x1b[J), so
            // everything above it (prompt, prior output, scrollback) stays
            // intact. No alternate screen buffer, no \x1b[2J whole-screen wipe.
            if prev_lines > 0 {
                print!("\x1b[{prev_lines}A\x1b[J");
            }
            print_clipped(
                &format!(
                    "fleet · {}  (refresh {interval}s — Ctrl-C to stop)",
                    crate::util::date_fmt("%H:%M:%S")
                ),
                clip,
            );
            println!();
            lines += 2; // the header line + its trailing blank line
        }
        lines += render_fleet(&fleet, clip);
        if !watch {
            return Ok(ExitCode::SUCCESS);
        }
        prev_lines = lines;
        std::thread::sleep(std::time::Duration::from_secs(interval.max(1)));
    }
}

/// Print one line, clipped to `clip` columns when set (ANSI-aware). Watch
/// mode clips so no row ever wraps; one-shot mode passes `None` and prints
/// full rows.
fn print_clipped(line: &str, clip: Option<usize>) {
    match clip {
        Some(w) => println!("{}", crate::util::clip_ansi(line, w)),
        None => println!("{line}"),
    }
}

/// The plain fleet table. Columns are fixed-name, width sized to content.
/// Render the fleet table and return the number of terminal lines printed (used
/// by `--watch` to repaint in place instead of clearing the whole screen).
fn render_fleet(fleet: &[crate::sensor::WorkerHealth], clip: Option<usize>) -> usize {
    use crate::util::{dim, red, rst, yel};
    if fleet.is_empty() {
        println!("no workers");
        return 1;
    }
    let idw = fleet.iter().map(|w| w.id.len()).max().unwrap_or(2).max(2);
    print_clipped(
        &format!(
            "{}{:idw$}  {:11}  {:8}  {:>6}  {:>6}  {:>7}  VERIFY{}",
            dim(),
            "ID",
            "HEALTH",
            "STATE",
            "IDLE",
            "UP",
            "ASK",
            rst()
        ),
        clip,
    );
    for w in fleet {
        let (hl, hr) = match w.health {
            "stuck" => (red(), rst()),
            "waiting-ask" => (yel(), rst()),
            "dead" => (dim(), rst()),
            _ => ("", ""),
        };
        let state = match (w.state.as_str(), w.exit_code) {
            ("exited", Some(c)) => format!("exit {c}"),
            (s, _) => s.to_string(),
        };
        let verify = match w.verify {
            Some(true) => "pass".to_string(),
            Some(false) => format!("{}FAIL{}", red(), rst()),
            None => "-".to_string(),
        };
        print_clipped(
            &format!(
                "{:idw$}  {hl}{:11}{hr}  {:8}  {:>6}  {:>6}  {:>7}  {verify}",
                w.id,
                w.health,
                state,
                fmt_dur(w.idle_s),
                fmt_dur(w.uptime_s),
                fmt_dur(w.ask_age_s),
            ),
            clip,
        );
    }
    // 1 header row + one row per worker.
    1 + fleet.len()
}

/// Compact duration: `-` (unknown), `42s`, `5m`, `2h`, `3d`.
fn fmt_dur(secs: Option<u64>) -> String {
    match secs {
        None => "-".to_string(),
        Some(s) if s < 60 => format!("{s}s"),
        Some(s) if s < 3600 => format!("{}m", s / 60),
        Some(s) if s < 86400 => format!("{}h", s / 3600),
        Some(s) => format!("{}d", s / 86400),
    }
}

pub fn cmd_kill(paths: &Paths, id: &str) -> Result<ExitCode> {
    let session = full_session(paths, id);
    if reject_pulse(&session, "kill") {
        return Ok(ExitCode::from(1));
    }
    kill(paths, &session)?;
    // The worker is dead by fiat: any tell it never drained is now addressed
    // to a corpse and must not linger for a future worker reusing the id.
    crate::mailbox::discard_tells(paths, &session);
    Ok(ExitCode::SUCCESS)
}

/// `looop screenshot <id> [--ansi|--json] [--no-trim]` — capture a session's
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
        eprintln!("usage: looop screenshot <id> [--ansi|--json] [--no-trim]");
        return Ok(ExitCode::from(1));
    };
    let session = full_session(paths, id);
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
    /// RFC3339 timestamp of the session's start (babysit's `started_at`).
    /// Empty when babysit didn't report one. Feeds the sys-sessions health
    /// reading's `uptime_s` detail.
    pub started_at: String,
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
}

fn project(info: ::babysit::SessionInfo) -> Session {
    Session {
        id: info.id,
        state: info.state,
        alive: info.alive,
        exit_code: info.exit_code.map(|c| c as i64),
        started_at: info.started_at,
    }
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
                    .is_some_and(|age| age >= max_age);
            if old {
                let _ = tokio::fs::remove_dir_all(&dir).await;
                crate::verify::clear(paths, &id);
                // Same generation hygiene as reap(): a pruned corpse must not
                // leave tells behind for a future worker reusing its id.
                crate::mailbox::discard_tells(paths, &id);
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
            crate::verify::clear(paths, session);
            // A removed corpse's id is about to be REUSED — drop any tell
            // still addressed to the dead generation (see cmd_start_session's
            // generation-boundary discard for the full reasoning).
            crate::mailbox::discard_tells(paths, session);
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
///
/// The `saved` copy of fd 1 is created close-on-exec: `f` spawns the detached
/// worker, so a plain `dup(1)` would leak our stdout into that child. When the
/// caller captured our stdout through a pipe (`$(looop worker start …)`), a
/// leaked write end keeps that pipe open for the worker's entire lifetime, so
/// the caller's read never sees EOF and the command *looks* hung even though we
/// returned immediately. `dup_cloexec` keeps the copy out of the child.
#[cfg(unix)]
pub(crate) fn suppress_stdout<T>(f: impl FnOnce() -> T) -> T {
    use std::cell::Cell;
    use std::io::Write;
    use std::os::unix::io::AsRawFd;
    unsafe extern "C" {
        fn dup2(a: i32, b: i32) -> i32;
    }
    if SUPPRESS_DEPTH.with(Cell::get) > 0 {
        return f();
    }
    let Ok(devnull) = std::fs::OpenOptions::new().write(true).open("/dev/null") else {
        return f();
    };
    let _swap = STDOUT_SWAP
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _ = std::io::stdout().flush();
    // Close-on-exec so the detached worker `f` spawns never inherits this
    // copy of our stdout (see the doc comment above).
    let saved = unsafe { dup_cloexec(1) };
    if saved < 0 {
        return f();
    }

    /// RAII restore: puts the saved fd back on 1 and decrements the depth on
    /// DROP, so even a PANICKING action unwinds with stdout restored — without
    /// this, a panic inside `f` would leave the whole process silenced on
    /// /dev/null (and the depth counter stuck). Declared AFTER `_swap`, so it
    /// drops FIRST: fd 1 is restored while the swap lock is still held.
    struct Restore(i32);
    impl Drop for Restore {
        fn drop(&mut self) {
            unsafe extern "C" {
                fn dup2(a: i32, b: i32) -> i32;
                fn close(fd: i32) -> i32;
            }
            let _ = std::io::Write::flush(&mut std::io::stdout());
            unsafe {
                dup2(self.0, 1);
                close(self.0);
            }
            SUPPRESS_DEPTH.with(|d| d.set(d.get() - 1));
        }
    }

    unsafe { dup2(devnull.as_raw_fd(), 1) };
    SUPPRESS_DEPTH.with(|d| d.set(d.get() + 1));
    let _restore = Restore(saved);
    f()
}

/// fd 1 is PROCESS-GLOBAL state: two threads swapping it concurrently could
/// each "save" the other's /dev/null and restore THAT as the real stdout,
/// leaving the whole process silenced forever (observed as parallel tests
/// losing the libtest results/summary mid-run). This mutex serializes the
/// whole swap window; [`SUPPRESS_DEPTH`] makes NESTED suppression on the same
/// thread (execute → start_worker → suppress_stdout) a plain pass-through
/// instead of a self-deadlock — fd 1 is already /dev/null inside the outer
/// window, so the inner swap was always redundant.
#[cfg(unix)]
static STDOUT_SWAP: std::sync::Mutex<()> = std::sync::Mutex::new(());
#[cfg(unix)]
thread_local! {
    static SUPPRESS_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// `dup(fd)` that returns a close-on-exec copy, so it is not inherited across a
/// later `exec`/spawn. Returns a negative value on failure (caller falls back).
///
/// Hand-written FFI is deliberate: replacing it would require the `libc` (or
/// `nix`) crate as a new dependency, which this project avoids. The constants
/// are safe to hardcode — F_SETFD / FD_CLOEXEC are POSIX-stable and are 2 / 1
/// on both macOS and Linux (verified by the `dup_cloexec_sets_close_on_exec`
/// test, which round-trips through F_GETFD).
#[cfg(unix)]
unsafe fn dup_cloexec(fd: i32) -> i32 {
    unsafe extern "C" {
        fn dup(fd: i32) -> i32;
        fn fcntl(fd: i32, cmd: i32, ...) -> i32;
        fn close(fd: i32) -> i32;
    }
    const F_SETFD: i32 = 2;
    const FD_CLOEXEC: i32 = 1;
    unsafe {
        let copy = dup(fd);
        if copy < 0 {
            return copy;
        }
        // If we can't set close-on-exec the copy would leak into the detached
        // worker (the very leak this guards against), so treat it as a hard
        // failure: close the copy and return a negative fd so the caller falls
        // back safely instead of running with a non-CLOEXEC descriptor.
        if fcntl(copy, F_SETFD, FD_CLOEXEC) < 0 {
            close(copy);
            return -1;
        }
        copy
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

    #[test]
    fn fmt_dur_is_compact_and_handles_unknown() {
        assert_eq!(fmt_dur(None), "-");
        assert_eq!(fmt_dur(Some(42)), "42s");
        assert_eq!(fmt_dur(Some(300)), "5m");
        assert_eq!(fmt_dur(Some(7200)), "2h");
        assert_eq!(fmt_dur(Some(259_200)), "3d");
    }

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

    // Regression: suppress_stdout swaps fd 1 — PROCESS-GLOBAL state. Without
    // serialization, two concurrent suppressions could each "save" the other's
    // /dev/null and restore it as the real stdout, permanently silencing the
    // process (this ate the libtest results/summary under parallel `cargo
    // test`). Also asserts nesting on one thread passes through (no deadlock).
    #[cfg(unix)]
    #[test]
    fn suppress_stdout_is_reentrant_and_race_free() {
        let before = stdout_ident();
        // Nested on one thread: pass-through, not a self-deadlock.
        assert_eq!(suppress_stdout(|| suppress_stdout(|| 42)), 42);
        // Hammer the swap from two threads at once.
        std::thread::scope(|s| {
            for _ in 0..2 {
                s.spawn(|| {
                    for _ in 0..200 {
                        suppress_stdout(|| std::hint::black_box(()));
                    }
                });
            }
        });
        assert_eq!(
            stdout_ident(),
            before,
            "stdout identity must survive concurrent suppression windows"
        );
    }

    /// Observe fd 1 while HOLDING the swap lock: another parallel test may
    /// legitimately be inside its own suppression window right now, and an
    /// unsynchronized peek would see its temporary /dev/null.
    #[cfg(unix)]
    fn stdout_ident() -> (u64, u64, u64) {
        use std::os::fd::BorrowedFd;
        use std::os::unix::fs::MetadataExt;
        let _swap = STDOUT_SWAP
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let fd = unsafe { BorrowedFd::borrow_raw(1) };
        let f = std::fs::File::from(fd.try_clone_to_owned().unwrap());
        let m = f.metadata().unwrap();
        (m.dev(), m.ino(), m.rdev())
    }

    // Regression: a PANICKING action must not leave the process silenced — the
    // RAII guard restores fd 1 and the depth counter during unwinding.
    #[cfg(unix)]
    #[test]
    fn suppress_stdout_restores_fd1_when_the_action_panics() {
        let before = stdout_ident();
        let unwound = std::panic::catch_unwind(|| suppress_stdout(|| panic!("boom")));
        assert!(unwound.is_err(), "the panic propagates");
        assert_eq!(
            stdout_ident(),
            before,
            "fd 1 restored during unwinding — the process is not left on /dev/null"
        );
        // Depth was decremented too: a later suppression still round-trips.
        assert_eq!(suppress_stdout(|| 7), 7);
        assert_eq!(stdout_ident(), before);
    }

    // Regression: the fd we dup inside suppress_stdout MUST be close-on-exec.
    // A plain dup(1) leaked our stdout into the detached worker; when the
    // caller captured our stdout via a pipe (`out=$(looop worker start …)`)
    // that leaked write end kept the pipe open for the worker's whole lifetime,
    // so the caller never saw EOF and `worker start` looked hung. Assert the
    // copy carries FD_CLOEXEC so no spawned child can inherit it.
    #[cfg(unix)]
    #[test]
    fn dup_cloexec_sets_close_on_exec() {
        unsafe extern "C" {
            fn fcntl(fd: i32, cmd: i32, ...) -> i32;
            fn close(fd: i32) -> i32;
        }
        const F_GETFD: i32 = 1;
        const FD_CLOEXEC: i32 = 1;
        unsafe {
            // dup fd 2 (stderr): always open under the test harness, and we
            // never touch its target so this stays side-effect free.
            let copy = dup_cloexec(2);
            assert!(copy >= 0, "dup_cloexec failed");
            let flags = fcntl(copy, F_GETFD);
            close(copy);
            assert!(flags >= 0, "F_GETFD failed");
            assert_eq!(
                flags & FD_CLOEXEC,
                FD_CLOEXEC,
                "dup_cloexec copy must be close-on-exec so detached workers don't \
                 inherit (and pin open) the caller's captured stdout pipe"
            );
        }
    }

    // No override: the template renders with only {{prompt_file}} substituted.
    #[test]
    fn start_session_refuses_a_resume_without_an_answer() {
        // The resume check runs BEFORE any spawn: an unknown ask id — or one
        // the human hasn't answered yet — must fail the move loudly.
        let p = crate::paths::Paths::temp();
        let out = cmd_start_session(&p, "w", "brief", None, None, Some("ghost-1")).unwrap();
        assert_eq!(out.code, ExitCode::from(1));

        // A pending (unanswered) detached ask is refused too.
        let id = crate::mailbox::ask_detached(&p, "w", "q?", "", &[]).unwrap();
        let out = cmd_start_session(&p, "w", "brief", None, None, Some(&id)).unwrap();
        assert_eq!(out.code, ExitCode::from(1));
    }

    #[test]
    fn build_worker_cmd_template_default() {
        let tmpl = "pi --model opus @{{prompt_file}}";
        let out = build_worker_cmd(tmpl, None, "/p/x.md").unwrap();
        assert_eq!(out.cmd, "pi --model opus @'/p/x.md'");
        assert!(!out.overridden);
    }

    // The worker path uses the SAME quote-aware substitution as the tick path:
    // a pre-quoted `"{{prompt_file}}"` template is not double-quoted, and a
    // path with shell metacharacters stays a single argument.
    #[test]
    fn build_worker_cmd_quotes_like_the_tick_path() {
        let out = build_worker_cmd("claude @\"{{prompt_file}}\"", None, "/p/x.md").unwrap();
        assert_eq!(out.cmd, "claude @'/p/x.md'");
        let out = build_worker_cmd("claude {{prompt_file}}", None, "/p/a b.md").unwrap();
        assert_eq!(out.cmd, "claude '/p/a b.md'");
    }

    // A --command override replaces the template WHOLESALE.
    #[test]
    fn build_worker_cmd_override_replaces_template() {
        let out = build_worker_cmd(
            "claude @{{prompt_file}}",
            Some("pi --model gpt-6 --no-tools @{{prompt_file}}"),
            "/p/x.md",
        )
        .unwrap();
        assert_eq!(out.cmd, "pi --model gpt-6 --no-tools @'/p/x.md'");
        assert!(out.overridden);
    }

    // A command WITHOUT {{prompt_file}} is refused — override or template:
    // the prompt file is the worker's only brief channel, so such a worker
    // would launch blind.
    #[test]
    fn build_worker_cmd_requires_prompt_placeholder() {
        let err =
            build_worker_cmd("claude @{{prompt_file}}", Some("pi -p"), "/p/x.md").unwrap_err();
        assert!(err.contains("{{prompt_file}}"));
        assert!(err.contains("--command"));

        let err = build_worker_cmd("claude -p", None, "/p/x.md").unwrap_err();
        assert!(err.contains("worker_command"));
    }

    // A blank override falls back to the template (treated as absent).
    #[test]
    fn build_worker_cmd_blank_override_is_ignored() {
        let out = build_worker_cmd("claude @{{prompt_file}}", Some("  "), "/p/x.md").unwrap();
        assert_eq!(out.cmd, "claude @'/p/x.md'");
        assert!(!out.overridden);
    }

    // REMOVED: a command still carrying {{model}}/{{thinking}} is
    // refused outright — expanding them empty would silently launch a broken
    // command (`--model --thinking …` parses the next flag as the value).
    #[test]
    fn build_worker_cmd_refuses_removed_placeholders() {
        let tmpl = "pi --model {{model}} --thinking {{thinking}} @{{prompt_file}}";
        let err = build_worker_cmd(tmpl, None, "/p/x.md").unwrap_err();
        assert!(err.contains("{{model}}"));
        assert!(err.contains("looop init"));

        let err = build_worker_cmd(
            "claude @{{prompt_file}}",
            Some("pi --thinking {{thinking}} @{{prompt_file}}"),
            "/p/x.md",
        )
        .unwrap_err();
        assert!(err.contains("{{thinking}}"));
        // Override case: hint points at the --command string, not `looop init`
        // (there's no config template to re-run init for).
        assert!(!err.contains("looop init"));
        assert!(err.contains("--command"));
    }

    // REGRESSION (#1): placeholder checks run against the ORIGINAL command,
    // BEFORE {{prompt_file}} substitution — a prompt path containing the
    // literal `{{model}}` (e.g. via a crafted session id) is substituted
    // verbatim and can never trip the removed-placeholder refusal.
    #[test]
    fn build_worker_cmd_prompt_path_with_literal_placeholder() {
        let out = build_worker_cmd("claude @{{prompt_file}}", None, "/p/{{model}}.md").unwrap();
        assert_eq!(out.cmd, "claude @'/p/{{model}}.md'");
    }
}
