//! `looop config zsh|bash` — emit shell integration (completions).
//!
//! Mirrors the pattern `box config <shell>` uses: the completion scripts live
//! as static files under `completions/` and are embedded at compile time, so a
//! single self-contained binary can print its own shell wiring. Enable with
//! `eval "$(looop config zsh)"` (or `bash`) in your shell rc.
//!
//! Unlike box there is no `cd` wrapper: looop never switches the shell's working
//! directory, so shell integration is purely dynamic completion of subcommands
//! and live ids (pending asks, goals, sensors, workers, leases) read straight
//! from the data dir.

use crate::cli::ConfigShell;
use anyhow::Result;
use std::process::ExitCode;

const ZSH: &str = include_str!("completions/looop.zsh");
const BASH: &str = include_str!("completions/looop.bash");

pub fn cmd_config(shell: &ConfigShell) -> Result<ExitCode> {
    match shell {
        ConfigShell::Zsh => print!("{ZSH}"),
        ConfigShell::Bash => print!("{BASH}"),
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zsh_script_registers_completion() {
        assert!(ZSH.contains("compdef _looop looop"));
        assert!(ZSH.contains("__looop_data_dir"));
    }

    #[test]
    fn bash_script_registers_completion() {
        assert!(BASH.contains("complete -F _looop looop"));
        assert!(BASH.contains("__looop_data_dir"));
    }

    /// Drift guard: every top-level clap subcommand must be offered by BOTH
    /// completion scripts — a past version shipped completions missing `tell`,
    /// `told`, `ask` and `schedule` entirely. Exclusions are explicit.
    #[test]
    fn completions_cover_every_top_level_subcommand() {
        use clap::CommandFactory;
        // `pulse` is looop's own detached spawn target — never typed by a
        // human, so completing it would only invite accidents.
        const EXCLUDED: &[&str] = &["pulse"];
        for sub in crate::cli::Cli::command().get_subcommands() {
            let name = sub.get_name();
            if EXCLUDED.contains(&name) {
                continue;
            }
            // zsh offers each verb as a `'name:desc'` entry in the subcmds array.
            assert!(
                ZSH.contains(&format!("'{name}:")),
                "zsh completion drift: `{name}` missing from the subcmds array"
            );
            // bash lists verbs space-separated in the `subcommands` string.
            let bash_list = BASH
                .lines()
                .find(|l| l.contains("local subcommands="))
                .expect("bash subcommands list");
            assert!(
                bash_list.split(['"', ' ']).any(|w| w == name),
                "bash completion drift: `{name}` missing from the subcommands string"
            );
        }
    }

    /// Drift guard, flag edition: every LONG FLAG clap defines on every
    /// subcommand (recursing into nested ops like `worker start`) must appear
    /// as a `--flag` string in BOTH completion scripts — a past version
    /// shipped completions missing `--session` on claim/unclaim entirely.
    #[test]
    fn completions_cover_every_long_flag() {
        use clap::CommandFactory;
        // `pulse` is never completed (see the test above); `help` is clap's
        // auto-injected per-verb flag, deliberately not offered.
        const EXCLUDED_SUBS: &[&str] = &["pulse"];
        const EXCLUDED_FLAGS: &[&str] = &["help"];

        fn walk(cmd: &clap::Command, check: &mut dyn FnMut(&str, &str)) {
            for a in cmd.get_arguments() {
                if let Some(long) = a.get_long()
                    && !EXCLUDED_FLAGS.contains(&long)
                {
                    check(cmd.get_name(), long);
                }
            }
            for sub in cmd.get_subcommands() {
                walk(sub, check);
            }
        }

        for sub in crate::cli::Cli::command().get_subcommands() {
            if EXCLUDED_SUBS.contains(&sub.get_name()) {
                continue;
            }
            walk(sub, &mut |verb, long| {
                let flag = format!("--{long}");
                assert!(
                    ZSH.contains(&flag),
                    "zsh completion drift: `{flag}` (from `{verb}`) missing"
                );
                assert!(
                    BASH.contains(&flag),
                    "bash completion drift: `{flag}` (from `{verb}`) missing"
                );
            });
        }
    }

    /// `looop help <topic>` completes verb names — both scripts must offer
    /// the subcommand list after the `help` verb (reusing the top-level list,
    /// which the test above already checks for coverage).
    #[test]
    fn help_topic_completes_subcommand_names() {
        // zsh routes `help` args back through the shared verb-list helper.
        assert!(ZSH.contains("__looop_subcommands"), "zsh shared verb list");
        assert!(
            ZSH.contains("help)"),
            "zsh: a `help` arm offering topics is wired"
        );
        // bash reuses the $subcommands string inside a `help` case arm.
        assert!(
            BASH.contains("help)"),
            "bash: a `help` arm offering topics is wired"
        );
    }

    #[test]
    fn both_scripts_resolve_data_dir_consistently() {
        // Both must honor $LOOOP_DATA_DIR then the XDG default, matching Paths.
        for s in [ZSH, BASH] {
            assert!(s.contains("LOOOP_DATA_DIR"));
            assert!(s.contains("XDG_STATE_HOME:-$HOME/.local/state"));
        }
    }
}
