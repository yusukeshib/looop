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
  looop up [--watch] [--json]    run the pulse as a DETACHED service (the 親玉
                                becomes a babysit session). --json makes the
                                pulse emit NDJSON to its log (agent-readable);
                                --watch follows that output after starting.
                                Idempotent: a live pulse is left running.
  looop down                     stop the detached pulse service
  looop watch <id>               follow a session's output read-only, like
                                tail -f (Ctrl-C to stop). `looop watch pulse`
                                watches the loop itself. No input — use attach
                                for that.
  looop run <goal-id>            run ONE goal NOW (manual override): a forced,
                                goal-focused move, ignoring priority order and
                                the world-unchanged skip; works while the pulse
                                runs. <goal-id> = goals/<id>.md basename.
                                e.g. looop run setup ; looop run morning-standup
                                (a bare `looop run` is gone — the pulse only runs
                                detached now; start it with `looop up`.)
  looop tick                     run a single beat and exit (debug / cron)
  looop status [--json]          structured snapshot of the loop's live state
                                (pulse, last tick, workers, cost) — for an
                                external observer / AI watching the loop
  looop ls [--json] [--watch] [--interval <dur>]
                                list this profile's worker sessions (⚑ = waiting),
                                in-process; --watch refreshes live (Ctrl-C to stop)
  looop start-session <id> "<prompt>" [runner]
                                start a worker session (used by the tick AI)
  looop attach <id>              attach your terminal to a worker (in-process;
                                detach with Ctrl-\ Ctrl-\)
  looop detach <id>              force-detach any other terminal from a session
  looop log <id> [--tail N] [--grep RE] [--since N] [--follow] [--raw] [--json]
                                show / tail / grep / follow a session's output
  looop shot <id> [--ansi|--json] [--trim]
                                render the session's current visible screen
  looop send <id> <text...> [-n] [--json]
                                type text into a session's stdin (-n: no newline)
  looop key <id> <KEY...> [--json]
                                send named keys (Enter, Up, Esc, C-c, F1, …)
  looop expect <id> <REGEX> [--timeout DUR] [--from-now] [--screen] [--json]
                                block until a regex appears (exit 124 on timeout)
  looop wait <id> [--timeout DUR]
                                block until the session exits; return its code
  looop wait-idle <id> [--settle DUR] [--timeout DUR]
                                block until output is quiet for --settle
  looop resize <id> <COLSxROWS>  resize a session's terminal (e.g. 120x40)
  looop restart <id>             restart the wrapped command in a session
  looop kill <id>                terminate a worker session
  looop flag <id> [message]      raise a worker's attention flag
  looop unflag <id>              clear a worker's attention flag
  looop prune                    clear finished worker corpses (looop-scoped;
                                the pulse also does this every tick)
  looop cost [today|all|--json]   report LLM spend recorded in the cost ledger
                                (ticks + manual goal runs are metered
                                automatically; workers self-report via
                                'looop _cost')
  looop config zsh|bash          print shell integration (completions);
                                add eval "$(looop config zsh)" to your ~/.zshrc
                                (or eval "$(looop config bash)" to ~/.bashrc)
  looop version                  print the looop version
  looop help                     show this help

Paths (override via env LOOOP_CONFIG / LOOOP_DATA_DIR):
  config  {config}
  data    {data}

looop is a single self-contained binary: the worker fleet (babysit) is linked
as a LIBRARY and driven entirely in-process — no `babysit` executable required.
looop scopes the fleet to this profile automatically.
  looop ls                      list worker sessions (⚑ = waiting for you)
  looop ls --watch              watch the fleet live, in place
  looop attach <id>             enter a waiting session and talk to it
  looop kill <id>               end a session ; looop flag/unflag <id> ; looop prune

The pulse launches each worker in the data dir; if a worker needs to touch code
it provisions its OWN sandbox (box if available, else git worktree), as told by
the PLAYBOOK. looop itself has no notion of repos.

Fix judgment by editing PLAYBOOK.md (in the data dir) and committing — it takes
effect next tick."#,
        config = paths.config.display(),
        data = paths.data_dir.display(),
    );
}
