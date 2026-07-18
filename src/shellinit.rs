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

    #[test]
    fn both_scripts_resolve_data_dir_consistently() {
        // Both must honor $LOOOP_DATA_DIR then the XDG default, matching Paths.
        for s in [ZSH, BASH] {
            assert!(s.contains("LOOOP_DATA_DIR"));
            assert!(s.contains("XDG_STATE_HOME:-$HOME/.local/state"));
        }
    }
}
