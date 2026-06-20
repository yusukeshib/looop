//! Runner-wiring config — the only thing that needs "installing".
//!
//! Written to $LOOOP_CONFIG on first run if absent. `tick` = how to run one
//! disposable AI move (stdin = the tick prompt). `interactive` = how to launch
//! a worker agent; {{prompt_file}} is substituted with the worker's prompt file.
//!
//! TICK COST ACCOUNTING (H3): `runner::run_streamed` meters every tick IN-PROCESS
//! off the runner's NDJSON stdout. pi emits per-message usage (the meter sums it);
//! claude emits a single cumulative `total_cost_usd` in its stream-json `result`
//! event (the meter takes it as-is). Both runners therefore need their structured
//! stream enabled in the tick command (pi: `--mode json`, claude:
//! `--output-format stream-json --verbose`), but NEITHER pipes through an external
//! formatter — there is no `| _ fmt` seam anymore. (Worker sessions self-report
//! via `looop _ cost`, independent of the tick meter.)
//!
//! BACK-COMPAT: a stored `looop.json` written before this change may still end
//! its tick command with `| "$LOOOP_BIN" _ fmt`. `runner_cmd` strips that trailing
//! seam on load (see `strip_fmt_seam`), so old configs keep working unchanged.
//!
//! MODEL ALLOCATION (M4): the tick is the highest-leverage call — it picks the
//! single move that steers everything — so it must NOT run on a weaker model than
//! the workers it directs. This is enforced per runner:
//!   * `pi` pins both tick and worker to the same strong model (claude-opus-4-8);
//!     the tick runs at `--thinking low` (one tiny decision) while the heavier
//!     `medium` budget is reserved for the worker's multi-step execution.
//!   * `claude` pins no model on either command, so both inherit the CLI default
//!     — equal, which still satisfies "tick not weaker than worker".
//! Cost stays bounded because the world-hash gate skips the AI entirely when
//! nothing changed, and the tick emits only one tiny decision. Operators who want
//! to trade decision quality for cost can drop the tick model in this file.

use crate::paths::Paths;
use anyhow::{Context, Result};
use std::fs;

/// The inline default config. Originally a copy of the bash `default_config`,
/// it now diverges deliberately: the tick commands no longer carry the
/// `| "$LOOOP_BIN" _ fmt` seam, since formatting + cost metering run in-process
/// (see `runner::run_streamed`).
pub const DEFAULT_CONFIG: &str = r#"{
  "default": "pi",
  "runners": {
    "claude": {
      "tick": "claude -p --output-format stream-json --verbose --dangerously-skip-permissions",
      "interactive": "claude \"$(cat {{prompt_file}})\"",
      "resume": "claude --resume"
    },
    "pi": {
      "tick": "pi -p --mode json -ne --model claude-opus-4-8 --thinking low 'Execute the looop tick instructions provided on stdin.'",
      "interactive": "pi --model claude-opus-4-8 --thinking medium @{{prompt_file}}",
      "resume": "pi --session"
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

    /// The active runner's command for `key`, resolving `.default` first.
    pub fn active_runner_cmd(&self, key: &str) -> Option<String> {
        let name = self.default_runner()?;
        self.runner_cmd(&name, key)
    }
}

/// Strip a trailing `| <bin> _ fmt` (or `_fmt`) seam from a runner command.
///
/// Tick formatting + cost metering moved in-process (`runner::run_streamed`), so
/// the old external pipe is dead. Older `looop.json` files still carry it; rather
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
