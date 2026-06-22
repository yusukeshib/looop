//! Dependency preflight — the pulse must not limp along half-wired (a RULE).
//!
//! looop is glue: it orchestrates external tools. If a required command is
//! missing, fail fast with install instructions. Unlike the bash version, the
//! Rust port needs neither `jq` (JSON is handled in-process by serde_json) nor
//! the `babysit` binary (babysit is linked as a library and the whole worker
//! fleet — spawn / list / attach / kill / prune — runs in-process). The
//! single hard prerequisite is the configured runner (pi/claude) used for looop's
//! per-beat decide (`tick`) and to launch worker sessions.

use crate::config::Config;
use crate::paths::Paths;
use anyhow::{Result, bail};

fn dep_hint(cmd: &str) -> &'static str {
    match cmd {
        "pi" => "see https://github.com/earendil-works/pi  (the default runner)",
        "claude" => "see https://docs.claude.com/claude-code",
        _ => "see the tool's docs",
    }
}

/// True if `cmd` is found on $PATH (equivalent to `command -v`).
fn on_path(cmd: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(cmd);
        candidate.is_file() && is_executable(&candidate)
    })
}

#[cfg(unix)]
fn is_executable(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(_p: &std::path::Path) -> bool {
    true
}

/// Verify hard prerequisites; bail with install hints listing everything
/// missing at once (so the user fixes it in one pass).
pub fn require_deps(paths: &Paths) -> Result<()> {
    let mut missing: Vec<(String, &'static str)> = Vec::new();

    // looop runs its per-beat decide + launches workers through the configured runner's
    // `interactive` command (pi/claude), so a missing runner binary is a hard
    // prereq. Resolve from $LOOOP_CONFIG when present, else the inline default,
    // and check its first token.
    if let Ok(cfg) = Config::load(paths)
        && let Some(name) = cfg.default_runner()
        && let Some(cmd) = cfg.runner_cmd(&name, "interactive")
        && let Some(bin) = cmd.split_whitespace().next()
        && !bin.is_empty()
        && !on_path(bin)
    {
        missing.push((bin.to_string(), dep_hint(bin)));
    }

    if missing.is_empty() {
        return Ok(());
    }

    let mut msg = String::from("looop: missing required dependencies — cannot run:\n");
    for (cmd, hint) in &missing {
        msg.push_str(&format!("  {:<8} install:  {}\n", cmd, hint));
    }
    msg.push_str("\nInstall the above, then re-run looop.");
    bail!(msg);
}
