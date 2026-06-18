//! Runner-wiring config — the only thing that needs "installing".
//!
//! Written to $LOOOP_CONFIG on first run if absent. `tick` = how to run one
//! disposable AI move (stdin = the tick prompt). `interactive` = how to launch
//! a worker agent; {{prompt_file}} is substituted with the worker's prompt file.

use crate::paths::Paths;
use anyhow::{Context, Result};
use std::fs;

/// The inline default config, byte-identical to the bash `default_config`.
pub const DEFAULT_CONFIG: &str = r#"{
  "default": "pi",
  "runners": {
    "claude": {
      "tick": "claude -p --dangerously-skip-permissions",
      "interactive": "claude \"$(cat {{prompt_file}})\"",
      "resume": "claude --resume"
    },
    "pi": {
      "tick": "pi -p --mode json -ne --model claude-sonnet-4-5 --thinking low 'Execute the looop tick instructions provided on stdin.' | \"$LOOOP_BIN\" _fmt",
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
    pub fn runner_cmd(&self, name: &str, key: &str) -> Option<String> {
        self.root
            .get("runners")?
            .get(name)?
            .get(key)?
            .as_str()
            .map(str::to_owned)
    }

    /// The active runner's command for `key`, resolving `.default` first.
    pub fn active_runner_cmd(&self, key: &str) -> Option<String> {
        let name = self.default_runner()?;
        self.runner_cmd(&name, key)
    }
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
