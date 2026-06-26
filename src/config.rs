//! Runner-wiring config — the only thing that needs "installing".
//!
//! Written to $LOOOP_CONFIG by `looop init` (the interactive setup wizard). It
//! wires up ONE runner with three commands at the top level (no profiles): `tick`
//! = how to run one disposable AI move (stdin = the tick prompt). `interactive` =
//! how to launch a worker agent; {{prompt_file}} is substituted with the worker's
//! prompt file. `resume` = how to re-attach a worker session.
//!
//! NOT INITIALIZED = no config file. The loop still runs: `Config::load` falls
//! back to the inline `DEFAULT_CONFIG` (claude) so a fresh checkout works out of
//! the box. `looop init` lets the operator pick another runner
//! (claude/codex/opencode/pi) and prefilled models, then writes the wiring.
//! Switch later by re-running `looop init` or editing the three commands by hand.
//!
//! TICK OUTPUT (H3): `runner::run_streamed` renders every tick IN-PROCESS off the
//! runner's NDJSON stdout. Both runners therefore need their structured stream
//! enabled in the tick command (pi: `--mode json`, claude: `--output-format
//! stream-json --verbose`), but NEITHER pipes through an external formatter —
//! there is no `| _ fmt` seam anymore.
//!
//! BACK-COMPAT: a stored config written before this change may still end
//! its tick command with `| "$LOOOP_BIN" _ fmt`. `runner_cmd` strips that trailing
//! seam on load (see `strip_fmt_seam`), so old configs keep working unchanged.
//!
//! MODEL ALLOCATION (M4): the tick is one tiny decision (pick the single next
//! move), so the default `pi` wiring runs it on a fast model at low thinking
//! (claude-sonnet-4-5, `--thinking low`); workers do the heavy multi-step
//! execution on the stronger model (claude-opus-4-8, `--thinking medium`).
//!
//! Spend stays bounded because the world-hash gate skips the AI entirely when
//! nothing changed, and the tick emits only one tiny decision. Operators who want
//! to trade decision quality for cost can drop the tick model in this file.

use crate::paths::Paths;
use anyhow::{Context, Result};
use std::fs;

/// The inline default config. Originally a copy of the bash `default_config`,
/// it now diverges deliberately: the tick commands no longer carry the
/// `| "$LOOOP_BIN" _ fmt` seam, since output formatting runs in-process
/// (see `runner::run_streamed`).
pub const DEFAULT_CONFIG: &str = r#"{
  "tick": "claude -p --output-format stream-json --verbose --dangerously-skip-permissions --model sonnet",
  "interactive": "claude --dangerously-skip-permissions --model opus \"$(cat {{prompt_file}})\"",
  "resume": "claude --resume"
}
"#;

/// A runner the `looop init` wizard can wire up. `tick_model` / `worker_model`
/// are the PREFILLED defaults shown in the wizard; an empty string means "let the
/// runner choose its own default" — the `--model` flag is then omitted from the
/// rendered command. Order here is the order the wizard offers them.
pub struct RunnerSpec {
    pub name: &'static str,
    pub tick_model: &'static str,
    pub worker_model: &'static str,
}

/// The runners `looop init` offers. claude is the default (first). The tick is one
/// tiny decision, so it gets the cheap/fast model; workers do the heavy
/// multi-step execution on the stronger model.
pub const RUNNERS: &[RunnerSpec] = &[
    RunnerSpec {
        name: "claude",
        tick_model: "sonnet",
        worker_model: "opus",
    },
    RunnerSpec {
        name: "codex",
        tick_model: "",
        worker_model: "",
    },
    RunnerSpec {
        name: "opencode",
        tick_model: "",
        worker_model: "",
    },
    RunnerSpec {
        name: "pi",
        tick_model: "claude-sonnet-4-5",
        worker_model: "claude-opus-4-8",
    },
];

/// Look up a runner spec by name (e.g. the wizard's chosen runner).
pub fn runner_spec(name: &str) -> Option<&'static RunnerSpec> {
    RUNNERS.iter().find(|r| r.name == name)
}

/// `" <prefix> <model>"` when `model` is non-empty, else `""` — so an empty model
/// drops the flag entirely and the runner uses its own configured default.
fn model_flag(prefix: &str, model: &str) -> String {
    let m = model.trim();
    if m.is_empty() {
        String::new()
    } else {
        format!(" {prefix} {m}")
    }
}

/// Render the three-command wiring JSON for `runner` with the chosen models.
/// Returns None for an unknown runner. Each runner must run UNATTENDED (no
/// permission prompts — the detached pulse can't answer them) and the `tick`
/// command must emit a structured stream that `runner::run_streamed` can archive.
///
/// `tick` reads its prompt from STDIN (run_streamed pipes the prompt file in), so
/// no prompt argument is passed. `interactive` substitutes {{prompt_file}} with
/// the worker's prompt file path.
pub fn render_config(runner: &str, tick_model: &str, worker_model: &str) -> Option<String> {
    let t = model_flag("--model", tick_model);
    let w = model_flag("--model", worker_model);
    let tc = model_flag("-m", tick_model);
    let wc = model_flag("-m", worker_model);
    let (tick, interactive, resume) = match runner {
        "claude" => (
            format!(
                "claude -p --output-format stream-json --verbose --dangerously-skip-permissions{t}"
            ),
            format!("claude --dangerously-skip-permissions{w} \"$(cat {{{{prompt_file}}}})\""),
            "claude --resume".to_string(),
        ),
        // codex exec reads the prompt from stdin; --json emits JSONL; the bypass
        // flag is required so a detached run never stops on an approval prompt.
        "codex" => (
            format!("codex exec --json --dangerously-bypass-approvals-and-sandbox{tc}"),
            format!(
                "codex --dangerously-bypass-approvals-and-sandbox{wc} \"$(cat {{{{prompt_file}}}})\""
            ),
            "codex resume".to_string(),
        ),
        // opencode wiring is best-effort (verify against your installed version).
        "opencode" => (
            format!("opencode run{tc}"),
            format!("opencode{wc} \"$(cat {{{{prompt_file}}}})\""),
            "opencode --continue".to_string(),
        ),
        "pi" => (
            format!(
                "pi -p --mode json -ne{t} --thinking low 'Execute the looop tick instructions provided on stdin.'"
            ),
            format!("pi{w} --thinking medium @{{{{prompt_file}}}}"),
            "pi --session".to_string(),
        ),
        _ => return None,
    };
    let v = serde_json::json!({
        "tick": tick,
        "interactive": interactive,
        "resume": resume,
    });
    Some(serde_json::to_string_pretty(&v).expect("config json") + "\n")
}

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

    /// A short label for the configured runner — the first token of the `tick`
    /// command (e.g. `pi`, `claude`). For log lines only.
    pub fn runner_label(&self) -> String {
        self.runner_cmd("tick")
            .or_else(|| self.runner_cmd("interactive"))
            .and_then(|c| c.split_whitespace().next().map(str::to_owned))
            .unwrap_or_else(|| "runner".into())
    }

    /// `.<key>` — e.g. the `tick` / `interactive` / `resume` command.
    ///
    /// Any trailing `| "$LOOOP_BIN" _ fmt` seam from a pre-in-process-metering
    /// config is stripped here (`strip_fmt_seam`), so stored configs written
    /// before the formatter moved in-process keep working without re-seeding.
    pub fn runner_cmd(&self, key: &str) -> Option<String> {
        self.root.get(key)?.as_str().map(strip_fmt_seam)
    }
}

/// Strip a trailing `| <bin> _ fmt` (or `_fmt`) seam from a runner command.
///
/// Tick output formatting moved in-process (`runner::run_streamed`), so
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

/// True once the operator has run `looop init` (the config file exists). When
/// false the loop runs on the inline `DEFAULT_CONFIG`; callers can nudge the user
/// toward `looop init` without blocking.
pub fn is_initialized(paths: &Paths) -> bool {
    paths.config.is_file()
}

/// Write the runner wiring to $LOOOP_CONFIG (creating its parent dir). Used by
/// `looop init`; overwrites any existing file (the wizard confirms first).
pub fn write(paths: &Paths, contents: &str) -> Result<()> {
    if let Some(dir) = paths.config.parent() {
        fs::create_dir_all(dir)
            .with_context(|| format!("creating config dir {}", dir.display()))?;
    }
    fs::write(&paths.config, contents)
        .with_context(|| format!("writing config {}", paths.config.display()))?;
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

    #[test]
    fn default_config_is_valid_claude_wiring() {
        let cfg = super::Config {
            root: serde_json::from_str(super::DEFAULT_CONFIG).expect("default config parses"),
        };
        assert_eq!(cfg.runner_label(), "claude");
        let inter = cfg.runner_cmd("interactive").unwrap();
        // The worker prompt placeholder survives JSON round-trip un-escaped.
        assert!(inter.contains("{{prompt_file}}"));
        assert!(inter.contains("$(cat"));
    }

    #[test]
    fn render_config_covers_every_offered_runner() {
        for spec in super::RUNNERS {
            let json = super::render_config(spec.name, spec.tick_model, spec.worker_model)
                .expect("known runner renders");
            let v: serde_json::Value = serde_json::from_str(&json).expect("renders valid json");
            for key in ["tick", "interactive", "resume"] {
                assert!(
                    v.get(key).and_then(|x| x.as_str()).is_some(),
                    "{} missing {key}",
                    spec.name
                );
            }
            // The runner label is the first token of the tick command.
            assert!(
                v["tick"].as_str().unwrap().starts_with(spec.name),
                "{} tick should start with the runner name",
                spec.name
            );
            // {{prompt_file}} must survive into the interactive command.
            assert!(
                v["interactive"]
                    .as_str()
                    .unwrap()
                    .contains("{{prompt_file}}")
            );
        }
        assert!(super::render_config("nope", "", "").is_none());
    }

    #[test]
    fn empty_model_drops_the_flag() {
        let json = super::render_config("codex", "", "").unwrap();
        assert!(!json.contains("--model"));
        assert!(!json.contains(" -m "));
        let json = super::render_config("claude", "sonnet", "opus").unwrap();
        assert!(json.contains("--model sonnet"));
        assert!(json.contains("--model opus"));
    }
}
