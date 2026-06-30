//! Dependency preflight — the pulse must not limp along half-wired (a RULE).
//!
//! looop is glue: it orchestrates external tools. If a required command is
//! missing, fail fast with install instructions. Unlike the bash version, the
//! Rust port needs neither `jq` (JSON is handled in-process by serde_json) nor
//! the `babysit` binary (babysit is linked as a library and the whole worker
//! fleet — spawn / list / attach / kill / prune — runs in-process). The
//! single hard prerequisite is the configured runner (claude/codex/opencode/pi/custom,
//! chosen via `looop init`) used for looop's per-beat decide (`tick`) and to
//! launch worker sessions.

use crate::config::Config;
use crate::paths::Paths;
use anyhow::{Result, bail};

fn dep_hint(cmd: &str) -> &'static str {
    match cmd {
        "claude" => "see https://docs.claude.com/claude-code  (the default runner)",
        "codex" => "see https://developers.openai.com/codex/cli",
        "opencode" => "see https://opencode.ai/docs",
        "pi" => "see https://github.com/earendil-works/pi",
        _ => "see the tool's docs",
    }
}

/// Verify hard prerequisites; bail with install hints listing everything
/// missing at once (so the user fixes it in one pass).
pub fn require_deps(paths: &Paths) -> Result<()> {
    let mut missing: Vec<(String, &'static str)> = Vec::new();

    // looop runs its per-beat decide + launches workers through the configured
    // runner's `worker_command`, so a missing runner binary is a hard prereq.
    // Resolve from $LOOOP_CONFIG when present, else the inline default, and check
    // its first token.
    if let Ok(cfg) = Config::load(paths)
        && let Some(cmd) = cfg.runner_cmd("worker_command")
        && let Some(bin) = cmd.split_whitespace().next()
        && !bin.is_empty()
        && !crate::util::on_path(bin)
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
    msg.push_str("\nInstall the above, then re-run looop.\n");
    msg.push_str("Or run `looop init` to choose a different runner.");
    bail!(msg);
}
