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
  HUMAN (that's nearly all you run — the rest you do through your agent):
  looop up [--json]              start the pulse (sensing loop, detached).
                                --json makes the pulse log NDJSON. Then start your
                                agent yourself and tell it to observe looop.
  looop down                     stop the pulse and all workers
  looop cost [today|all|--json]  report LLM spend (agents self-report via `_ cost`)
  looop config zsh|bash          print shell integration (completions)
  looop version | help           print version / show this help

  ROOT-AGENT VERBS (your agent session emits these in its loop — you rarely type
  them yourself; see the CONTRACT above):
  looop _ state [--json] | _ wait [--json]             read state (blocking with --wait)
  looop _ answer <ask_id> "<text>"           resolve a worker's pending ask
  looop _ goal write <id> [body|stdin] | _ goal archive <id>
  looop _ sensor write <name> [script|stdin]
  looop _ playbook write [body|stdin]
  looop _ run <cmd…> [--reason T]            ONE reversible shell command
  looop _ worker start <id> <prompt…> | _ worker kill <id>
  looop _ notify <message…>                  surface a notice to the human

  WORKER self-callbacks (auto-injected CONTRACT — not for humans):
  looop _ ask <id> --prompt "…" [--ref P] [--options a,b]   ask + block for answer
  looop _ kill <id> | _ claim <name> | _ unclaim <name> | _ cost <…>

Paths (override via env LOOOP_CONFIG / LOOOP_DATA_DIR):
  config    {config}
  data      {data}
  sessions  {fleet}

looop is a single self-contained binary: session management (babysit) is linked
as a LIBRARY and driven entirely in-process — no `babysit` executable required.
You run your own agent and tell it to observe looop (loop on `looop _ wait
--json`); it decides and drives looop through the `_ …` verbs. Worker self-control
verbs (ask / kill / claim / unclaim / cost) are auto-injected callbacks.

The root agent launches each worker in the data dir; a worker that touches code
provisions its OWN sandbox (box if available, else git worktree). looop itself
has no notion of repos. Change judgment with `looop _ playbook write` / `_ goal
write` — it takes effect next beat. (looop does not version the data dir; `git
init` it yourself for history.)"#,
        config = paths.config.display(),
        data = paths.data_dir.display(),
        fleet = paths.data_dir.join("sessions").display(),
    );
}
