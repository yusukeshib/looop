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
  looop version | help           print version / show this help

  STEER (the contract — driven by you or any client; looop does NOT need these to act):
  looop state [--json] | wait [--json] [--only-asks|--actionable]  read state
  looop asks [--json]                      pending asks only (a client's narrow view)
  looop answer <ask_id> "<text>"|- [--force]  resolve a worker's ask (`-`/empty = stdin; --force to re-answer)
  looop goal write <id> [body|-] | goal archive <id>     (`-`/omit = stdin/heredoc)
  looop sensor write <name> [script|-]                   (`-`/omit = stdin/heredoc)
  looop playbook write [body|-]                          (`-`/omit = stdin/heredoc)
  looop screenshot <id> [--ansi|--json] [--no-trim]   capture a session's screen
  looop worker list [--json|--all|--watch [--interval N]]   fleet + health (busy/waiting-ask/stuck/dead), idle/uptime/ask age

  WORKER self-callbacks (auto-injected CONTRACT — not for humans):
  looop ask <id> --prompt "…" [--ref P] [--options a,b]   ask + block for answer
  looop kill <id> | claim <name> | unclaim <name>

Paths (override via env LOOOP_CONFIG / LOOOP_DATA_DIR):
  config    {config}
  data      {data}
  sessions  {fleet}

looop is a single self-contained binary: session management (babysit) is linked
as a LIBRARY and driven entirely in-process — no `babysit` executable required.
looop decides autonomously each beat and drives itself through the typed actions;
the STEER verbs above are the contract YOU (or any client) drive to steer +
answer asks; the worker self-callbacks (ask / kill / claim / unclaim) are
auto-injected.

looop launches each worker in the data dir; a worker that touches code provisions
its OWN sandbox (a git worktree). looop itself has no notion
of repos. Steer it by editing goals / the PLAYBOOK (`looop goal write` /
`playbook write`) — it takes effect next beat. (looop does not version the data
dir; `git init` it yourself for history.)"#,
        config = paths.config.display(),
        data = paths.data_dir.display(),
        fleet = paths.data_dir.join("sessions").display(),
    );
}
