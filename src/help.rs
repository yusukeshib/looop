//! `looop --help` / `looop help` — emits the FULL design manual (mechanism +
//! intent), not just a subcommand list. The static narrative lives in
//! `manual.txt` and is embedded at compile time; the Usage / Paths sections are
//! rendered here with live config/data paths (mirroring the bash heredoc).

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
  looop up [--json]              start the pulse: the autonomous loop (sense +
                                decide + run workers), detached. --json logs NDJSON.
  looop down                     stop the pulse and all workers
  looop watch [<id>] [--since <dur>] [--all]
                                observer TUI: live colored log + session selector
                                (read-only; <id> preselects, e.g. `looop watch pulse`)
                                hides dead sessions idle > 1d; --since 12h/30m to
                                widen, --all to show every session, `a` toggles live
  looop cost                     report LLM spend by day (per-beat + workers)
  looop config zsh|bash          print shell integration (completions)
  looop version | help           print version / show this help

  STEER (you, or a concierge acting for you — looop does NOT need these to act):
  looop _ state [--json] | _ wait [--json] [--only-asks|--actionable]  read state
  looop _ asks [--json]                      pending asks only (concierge's narrow view)
  looop _ answer <ask_id> "<text>"|- [--force]  resolve a worker's ask (`-`/empty = stdin; --force to re-answer)
  looop _ goal write <id> [body|stdin] | _ goal archive <id>
  looop _ sensor write <name> [script|stdin]
  looop _ playbook write [body|stdin]
  looop _ send <id> "<text>" [--no-newline]   type input into a worker's terminal
  looop _ screenshot <id> [--ansi|--json] [--no-trim]   capture a session's screen

  WORKER self-callbacks (auto-injected CONTRACT — not for humans):
  looop _ ask <id> --prompt "…" [--ref P] [--options a,b]   ask + block for answer
  looop _ kill <id> | _ claim <name> | _ unclaim <name> | _ cost <…>

Paths (override via env LOOOP_CONFIG / LOOOP_DATA_DIR):
  config    {config}
  data      {data}
  sessions  {fleet}

looop is a single self-contained binary: session management (babysit) is linked
as a LIBRARY and driven entirely in-process — no `babysit` executable required.
looop decides autonomously each beat and drives itself through the typed actions;
the `_ …` verbs above are for YOU (or a concierge) to steer + answer asks, and the
worker self-callbacks (ask / kill / claim / unclaim / cost) are auto-injected.

looop launches each worker in the data dir; a worker that touches code provisions
its OWN sandbox (box if available, else git worktree). looop itself has no notion
of repos. Steer it by editing goals / the PLAYBOOK (`looop _ goal write` /
`_ playbook write`) — it takes effect next beat. (looop does not version the data
dir; `git init` it yourself for history.)"#,
        config = paths.config.display(),
        data = paths.data_dir.display(),
        fleet = paths.data_dir.join("sessions").display(),
    );
}
