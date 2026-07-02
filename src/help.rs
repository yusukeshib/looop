//! `looop --help` / `looop help` — emits the FULL design manual (mechanism +
//! intent), not just a subcommand list. The static narrative lives in
//! `manual.txt` and is embedded at compile time; the Usage / Paths sections are
//! rendered here with live config/data paths (mirroring the bash heredoc).
//!
//! A bare `looop` does NOT land here — it shows clap's auto-generated short
//! command summary (see main.rs). This full manual is reserved for the explicit
//! `help` verb / `--help` front door, because it is a hand-written design
//! narrative clap cannot produce.

use crate::paths::Paths;

/// The mechanism + intent narrative (THE IDEA, THREE NOUNS, ONE BEAT, RULES,
/// CODE/CONFIG/DATA, BOOTSTRAP, DEPENDENCIES), embedded from manual.txt.
const MANUAL: &str = include_str!("manual.txt");

pub fn print(paths: &Paths) {
    print!("{MANUAL}");
    println!(
        r#"
Usage:
  HUMAN (looop runs itself — this is nearly all you touch):
  looop init                     interactive setup: choose the agent runner
                                (claude/codex/opencode/pi/custom) and write wiring
  looop up [--json]              start the pulse: the autonomous loop (sense +
                                decide + run workers), detached. --json logs NDJSON.
  looop down                     stop the pulse and all workers
  looop watch [<id>] [--since <dur>] [--all]
                                observer TUI: live colored log + session selector
                                (read-only; <id> preselects, e.g. `looop watch pulse`)
                                shows only active sessions; --since 1d/12h/30m to
                                also show recent dead, --all for every session,
                                `a` cycles active/recent/all live;
                                scroll up reaches the first line; shift+drag to copy
  looop client                   non-agent TUI: pending asks always on screen,
                                answer each by hand (the humble alternative to an
                                agent concierge — see the /looop skill)
  looop version | help           print version / show this help

  STEER (the contract — driven by you or any client; looop does NOT need these to act):
  looop _ state [--json] | _ wait [--json] [--only-asks|--actionable]  read state
  looop _ asks [--json]                      pending asks only (a client's narrow view)
  looop _ answer <ask_id> "<text>"|- [--force]  resolve a worker's ask (`-`/empty = stdin; --force to re-answer)
  looop _ goal write <id> [body|-] | _ goal archive <id>   (`-`/omit = stdin/heredoc)
  looop _ sensor write <name> [script|-]                   (`-`/omit = stdin/heredoc)
  looop _ playbook write [body|-]                          (`-`/omit = stdin/heredoc)
  looop _ send <id> "<text>" [--no-newline]   type input into a worker's terminal
  looop _ screenshot <id> [--ansi|--json] [--no-trim]   capture a session's screen

  WORKER self-callbacks (auto-injected CONTRACT — not for humans):
  looop _ ask <id> --prompt "…" [--ref P] [--options a,b]   ask + block for answer
  looop _ kill <id> | _ claim <name> | _ unclaim <name>

Paths (override via env LOOOP_CONFIG / LOOOP_DATA_DIR):
  config    {config}
  data      {data}
  sessions  {fleet}

looop is a single self-contained binary: session management (babysit) is linked
as a LIBRARY and driven entirely in-process — no `babysit` executable required.
looop decides autonomously each beat and drives itself through the typed actions;
the `_ …` verbs above are the contract YOU (or any client) drive to steer +
answer asks; the worker self-callbacks (ask / kill / claim / unclaim) are
auto-injected.

looop launches each worker in the data dir; a worker that touches code provisions
its OWN sandbox (a git worktree). looop itself has no notion
of repos. Steer it by editing goals / the PLAYBOOK (`looop _ goal write` /
`_ playbook write`) — it takes effect next beat. (looop does not version the data
dir; `git init` it yourself for history.)"#,
        config = paths.config.display(),
        data = paths.data_dir.display(),
        fleet = paths.data_dir.join("sessions").display(),
    );
}
