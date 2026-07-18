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
    println!("{}", usage_text(paths));
}

/// The live Usage/Paths tail of the manual, as a string (so tests can guard it
/// against drifting from the clap definitions in cli.rs).
fn usage_text(paths: &Paths) -> String {
    format!(
        r#"
Usage:
  HUMAN (looop runs itself — this is nearly all you touch):
  looop init                     interactive setup: choose the agent runner
                                (claude/codex/opencode/pi/custom) and write wiring
  looop up [--json]              start the pulse: the autonomous loop (sense +
                                decide + run workers), detached. --json logs NDJSON.
  looop down                     stop the pulse and all workers
  looop config zsh|bash          shell integration (completions); eval "$(looop config zsh)"
  looop version | help           print version / show this help

  STEER (the contract — driven by you or any client; looop does NOT need these to act):
  looop state [--json] | wait [--json] [--only-asks|--actionable]  read state
                                (`wait` exits 2 on a `pulse-down` wake — the loop
                                is not running — so a shell client can branch on
                                the exit code without parsing stdout)
  looop asks [--json]                      pending asks only (a client's narrow view)
  looop answer <ask_id> "<text>"|- [--force]  resolve a worker's ask (`-`/empty = stdin; --force to re-answer)
  looop goal write <id> [body|-] | goal archive <id>     (`-`/omit = stdin/heredoc)
  looop sensor write <name> [script|-]                   (`-`/omit = stdin/heredoc)
                                a script may declare `# looop:interval=<secs>` —
                                it is then re-run only when its snapshot is older
                                (rate-limited/expensive observers skip beats)
  looop playbook write [body|-]                          (`-`/omit = stdin/heredoc)
  looop schedule write <name> --in S | --every S [--note …]   durable time trigger:
                                one-shot (--in) or recurring (--every, min 60s);
                                when due it WAKES the loop (survives restarts —
                                unlike next_interval_s there is no 3600s cap)
  looop schedule rm <name> | schedule list [--json]
  looop run [--reason "…"] <cmd…>   one ad-hoc, REVERSIBLE shell command (recorded);
                                the command's own flags pass through verbatim — put
                                --reason/--journal BEFORE the command (or use `--`)
  looop tell <worker> "<msg>"   queue steering INTO a live worker — it picks the
                                message up at its next `told` check or along with
                                its next ask answer
  looop screenshot <id> [--ansi|--json|--plain] [--no-trim]   capture a session's screen
  looop worker start <id> [prompt|-] [--command CMD] [--verify CMD] [--resume <ask-id>]
                                spawn a worker; --verify = post-condition shell
                                command run ONCE after the worker dies (exit 0 =
                                verified done; fail is surfaced in sys-sessions
                                as verify:"fail" — exit status alone is never
                                trusted as "work done"); --resume <ask-id> =
                                consume an ANSWERED (detached) ask — injects its
                                question, the human's answer and the checkpoint
                                into the brief, then archives the ask/answer pair
  looop worker list [--json] [-a|--all] [-w|--watch [--interval N]]   fleet + health (busy/waiting-ask/stuck/dead), idle/uptime/ask age, verify verdict

  Shorthands: `worker`=`w`, `worker ls` / `w ls` = `worker list`, `screenshot`=`ss`,
  and `write`=`w` (`goal w` / `sensor w` / `playbook w` / `schedule w`).
  A body/script that STARTS with `-` needs a preceding `--` (e.g.
  `looop answer id-1 -- --yes`); only `tell` and `run` pass leading dashes through.

  WORKER self-callbacks (auto-injected CONTRACT — not for humans):
  looop ask <id> --prompt "…" [--ref P] [--options a,b] [--detach]   ask + block
                                for answer (--detach: write the ask, print its id
                                and return immediately — checkpoint + exit; the
                                answer arrives via `worker start --resume`)
  looop told [id]               print + consume pending steering messages
  looop kill <id> | claim <name> [--session ID] | unclaim <name> [--session ID]

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
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drift guard: every long flag clap defines on EVERY top-level subcommand
    /// (recursing into nested ops like `worker start` / `schedule write`) must
    /// appear in the hand-written usage text, so a future flag can't silently
    /// vanish from `looop help`. Exclusions are explicit and justified.
    #[test]
    fn usage_covers_every_clap_flag() {
        use clap::CommandFactory;
        let text = usage_text(&Paths::temp());

        // Flags intentionally NOT in the usage tail:
        //   help    — clap-injected on every verb;
        //   journal — the shared cross-verb note flag (JournalOpt), documented
        //             as a convention rather than repeated on every line.
        const EXCLUDED_FLAGS: &[&str] = &["help", "journal"];
        // Verbs with no human-facing usage line at all:
        //   pulse — looop's own detached spawn target, never typed by a human.
        const EXCLUDED_SUBS: &[&str] = &["pulse"];

        fn walk(cmd: &clap::Command, text: &str) {
            for a in cmd.get_arguments() {
                let Some(long) = a.get_long() else { continue };
                if EXCLUDED_FLAGS.contains(&long) {
                    continue;
                }
                assert!(
                    text.contains(&format!("--{long}")),
                    "help drift: --{long} (from `{}`) missing from usage",
                    cmd.get_name()
                );
            }
            for sub in cmd.get_subcommands() {
                walk(sub, text);
            }
        }

        for sub in crate::cli::Cli::command().get_subcommands() {
            if EXCLUDED_SUBS.contains(&sub.get_name()) {
                continue;
            }
            walk(sub, &text);
        }

        // The worker-list short flags are part of the documented surface.
        assert!(text.contains("-a|--all"), "-a short flag documented");
        assert!(text.contains("-w|--watch"), "-w short flag documented");
        // And the corrected list shorthand.
        assert!(text.contains("`worker ls` / `w ls`"));
    }
}
