//! Runner-wiring config — the only thing that needs "installing".
//!
//! Written to $LOOOP_CONFIG on first run if absent. `tick` = how to run one
//! disposable AI move (stdin = the tick prompt). `interactive` = how to launch
//! a worker agent; {{prompt_file}} is substituted with the worker's prompt file.
//!
//! TICK COST ACCOUNTING (H3): both runners pipe their tick through `_fmt`, which
//! meters spend into the cost ledger. pi emits per-message usage (`_fmt` sums it);
//! claude emits a single cumulative `total_cost_usd` in its stream-json `result`
//! event (`_fmt` takes it as-is). Previously the claude tick ran as plain
//! `claude -p` with no `_fmt` seam, so `looop cost` always showed $0 for claude
//! ticks — accounting was asymmetric between runners. The default now routes
//! claude through `--output-format stream-json | _fmt` so both runners meter
//! symmetrically. (Worker sessions still self-report via `looop _cost`,
//! independent of the tick seam.)
//!
//! MODEL ALLOCATION (M4): the tick is the highest-leverage call — it picks the
//! single move that steers everything — so it must NOT run on a weaker model than
//! the workers it directs. The default tick therefore uses the same strong model
//! as the worker (claude-opus-4-8). Cost stays bounded because the world-hash gate
//! skips the AI entirely when nothing changed, and the tick emits only one tiny
//! decision; the heavier `medium` thinking budget is reserved for the worker,
//! which does the actual multi-step execution. Operators who want to trade
//! decision quality for cost can drop the tick model back to sonnet in this file.

use crate::paths::Paths;
use anyhow::{Context, Result};
use std::fs;

/// The inline default config, byte-identical to the bash `default_config`.
pub const DEFAULT_CONFIG: &str = r#"{
  "default": "pi",
  "runners": {
    "claude": {
      "tick": "claude -p --output-format stream-json --verbose --dangerously-skip-permissions | \"$LOOOP_BIN\" _fmt",
      "interactive": "claude \"$(cat {{prompt_file}})\"",
      "resume": "claude --resume"
    },
    "pi": {
      "tick": "pi -p --mode json -ne --model claude-opus-4-8 --thinking low 'Execute the looop tick instructions provided on stdin.' | \"$LOOOP_BIN\" _fmt",
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
