//! Runner-wiring config — the only thing that needs "installing".
//!
//! Written to $LOOOP_CONFIG on first run if absent. A runner needs ONE command:
//! `interactive` = how to launch an agent session (the root agent AND each
//! worker); `{{prompt_file}}` is substituted with that session's seed prompt.
//! looop runs no LLM of its own (the root agent decides), so there is no `tick`
//! command or in-process cost metering anymore.
//!
//! BACK-COMPAT: a stored config written before this change may still end a
//! command with `| "$LOOOP_BIN" _ fmt`. `runner_cmd` strips that trailing seam on
//! load (see `strip_fmt_seam`), so old configs keep working unchanged.

use crate::paths::Paths;
use anyhow::{Context, Result};
use std::fs;

/// The inline default config. looop no longer runs its own LLM (the root agent
/// decides), so a runner only needs an `interactive` command — how to launch an
/// agent session (the root agent AND each worker). `{{prompt_file}}` is the
/// session's seed prompt.
pub const DEFAULT_CONFIG: &str = r#"{
  "default": "pi",
  "runners": {
    "claude": {
      "interactive": "claude \"$(cat {{prompt_file}})\""
    },
    "pi": {
      "interactive": "pi --model claude-opus-4-8 --thinking medium @{{prompt_file}}"
    }
  }
}
"#;

/// The parsed config — kept as a generic JSON value so the runner table stays
/// open-ended (mirrors the bash `jq` lookups rather than a rigid schema).
pub struct Config {
    pub root: serde_json::Value,
}

impl Config {
    /// Load $LOOOP_CONFIG, falling back to the inline default when absent
    /// (matches bash: `[ -f "$CONFIG" ] && cat || default_config`).
    pub fn load(paths: &Paths) -> Result<Self> {
        let text = if paths.config.is_file() {
            fs::read_to_string(&paths.config)
                .with_context(|| format!("reading config {}", paths.config.display()))?
        } else {
            DEFAULT_CONFIG.to_string()
        };
        let root: serde_json::Value =
            serde_json::from_str(&text).context("parsing looop config JSON")?;
        Ok(Config { root })
    }

    /// `.notification` — an OPTIONAL shell command template looop runs when a
    /// `send_notification` action fires (a worker flagged the human, or the pulse
    /// is blocked on a human edit). looop substitutes `{{message}}` / `{{id}}`
    /// and also exports `$LOOOP_MESSAGE` / `$LOOOP_ID`, then spawns it detached
    /// (best-effort — a failure never fails the tick). Typical value pops a tmux
    /// window onto the flagged worker:
    ///   "notification": "tmux new-window -n looop 'looop attach {{id}}'"
    pub fn notification(&self) -> Option<String> {
        self.root
            .get("notification")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    }

    /// The active runner name (`.default`).
    pub fn default_runner(&self) -> Option<String> {
        self.root
            .get("default")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    }

    /// `.runners[<name>].<key>` — e.g. the `tick` / `interactive` command.
    ///
    /// Any trailing `| "$LOOOP_BIN" _ fmt` seam from a pre-in-process-metering
    /// config is stripped here (`strip_fmt_seam`), so stored configs written
    /// before the formatter moved in-process keep working without re-seeding.
    pub fn runner_cmd(&self, name: &str, key: &str) -> Option<String> {
        self.root
            .get("runners")?
            .get(name)?
            .get(key)?
            .as_str()
            .map(strip_fmt_seam)
    }
}

/// Strip a trailing `| <bin> _ fmt` (or `_fmt`) seam from a runner command.
///
/// Tick formatting + cost metering moved in-process (`runner::run_streamed`), so
/// the old external pipe is dead. Older configs still carry it; rather
/// than force a re-seed (which would clobber user edits) we drop the seam on load.
/// Only the LAST pipe segment is inspected, and only when it is recognisably the
/// fmt seam (mentions the looop binary and ends in `_ fmt`/`_fmt`) — any other
/// user pipeline is left untouched.
fn strip_fmt_seam(cmd: &str) -> String {
    if let Some(idx) = cmd.rfind('|') {
        let tail: String = cmd[idx + 1..]
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let is_fmt = (tail.ends_with("_ fmt") || tail.ends_with("_fmt"))
            && (tail.contains("LOOOP_BIN") || tail.contains("looop"));
        if is_fmt {
            return cmd[..idx].trim_end().to_string();
        }
    }
    cmd.to_string()
}

/// Seed $LOOOP_CONFIG with the inline default if it does not exist yet.
pub fn ensure_config(paths: &Paths) -> Result<()> {
    if !paths.config.is_file() {
        if let Some(dir) = paths.config.parent() {
            fs::create_dir_all(dir)
                .with_context(|| format!("creating config dir {}", dir.display()))?;
        }
        fs::write(&paths.config, DEFAULT_CONFIG)
            .with_context(|| format!("seeding config {}", paths.config.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::strip_fmt_seam;

    #[test]
    fn strips_trailing_fmt_seam() {
        assert_eq!(
            strip_fmt_seam("pi -p --mode json | \"$LOOOP_BIN\" _ fmt"),
            "pi -p --mode json"
        );
        // Joined verb form (`_fmt`) and bare `looop` binary token both match.
        assert_eq!(strip_fmt_seam("claude -p | looop _fmt"), "claude -p");
    }

    #[test]
    fn leaves_unrelated_pipelines_untouched() {
        let cmd = "pi -p --mode json | jq .";
        assert_eq!(strip_fmt_seam(cmd), cmd);
        let plain = "pi -p --mode json";
        assert_eq!(strip_fmt_seam(plain), plain);
        // A trailing pipe that is not the fmt seam stays put.
        let other = "claude -p | tee out.log";
        assert_eq!(strip_fmt_seam(other), other);
    }

    #[test]
    fn default_config_has_no_fmt_seam() {
        assert!(!super::DEFAULT_CONFIG.contains("_ fmt"));
        assert!(!super::DEFAULT_CONFIG.contains("_fmt"));
    }
}
