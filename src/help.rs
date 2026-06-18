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
    let bs = paths.bs_hint_env();
    println!(
        r#"
Usage:
  looop                          run the pulse (foreground; Ctrl-C to stop)
  looop run <goal-id>            run ONE goal NOW (manual override): a forced,
                                goal-focused move, ignoring priority order and
                                the world-unchanged skip; works while the pulse
                                runs. <goal-id> = goals/<id>.md basename.
                                e.g. looop run setup ; looop run morning-standup
  looop tick                     run a single beat and exit (debug / cron)
  looop status [--json]          structured snapshot of the loop's live state
                                (pulse, last tick, workers, proposals, cost) —
                                for an external observer / AI watching the loop
  looop ls [babysit ls opts]     list this profile's worker sessions (⚑ = waiting);
                                opts pass through to babysit ls, e.g.
                                looop ls --watch [--interval 2s]  (live, Ctrl-C to stop)
  looop start-session <id> "<prompt>" [runner]
                                start a worker session (used by the tick AI)
  looop cost [today|all|--json]   report LLM spend recorded in the cost ledger
                                (ticks + manual goal runs are metered
                                automatically; workers self-report via
                                'looop _cost')
  looop playbook [diff|approve|reject]
                                review a PLAYBOOK change the AI proposed. PLAYBOOK
                                edits never take effect until you approve them —
                                the loop keeps running on the current PLAYBOOK.
                                diff (default) shows it; approve applies it;
                                reject discards it. (To change judgment yourself,
                                just edit PLAYBOOK.md — that takes effect next tick.)
  looop version                  print the looop version
  looop help                     show this help

Paths (override via env LOOOP_CONFIG / LOOOP_DATA_DIR):
  config  {config}
  data    {data}

Worker sessions are managed by external tools (looop scopes BABYSIT_DIR to this
profile automatically; use 'looop ls' to skip the BABYSIT_DIR= prefix):
  looop ls                      list worker sessions (⚑ = waiting for you)
  looop ls --watch              watch the fleet live (= babysit ls --watch)
  {bs}babysit attach -s looop-<id>   enter a waiting session and talk to it

The pulse launches each worker in the data dir; if a worker needs to touch code
it provisions its OWN sandbox (box if available, else git worktree), as told by
the PLAYBOOK. looop itself has no notion of repos.

Fix judgment by editing PLAYBOOK.md (in the data dir) and committing — it takes
effect next tick. Drop a file at $data/paused to pause the loop."#,
        config = paths.config.display(),
        data = paths.data_dir.display(),
        bs = bs,
    );
}
