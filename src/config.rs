//! Runner-wiring config — the only thing that needs "installing".
//!
//! Written to $LOOOP_CONFIG by `looop init`. It wires up ONE runner with two
//! commands at the top level (no profiles): `tick_command` = how to run one
//! disposable AI move; `worker_command` = how to launch a worker agent. BOTH
//! commands take the prompt the same way: the `{{prompt_file}}` placeholder is
//! substituted with the prompt file's path (read it via `$(cat {{prompt_file}})`
//! or `@{{prompt_file}}`). For the tick ONLY, omitting the placeholder is also
//! allowed — then the prompt file is piped in as stdin (the zero-config path); a
//! worker can't use stdin because that is its live attach TTY. (Re-attaching to a
//! worker is done in-process via babysit, so there is no `resume` command.)
//!
//! NOT INITIALIZED = no config file. `looop up` REFUSES to start the pulse in
//! that state and tells the operator to run `looop init`, which writes the runner
//! wiring. The inline `DEFAULT_CONFIG` (claude) is both the first-run default and
//! the safety net for `Config::load` (e.g. a plumbing verb that runs without a file).
//!
//! looop is deliberately a GLUE layer: after init, runtime config is just these
//! two command strings. Preset knowledge (codex/opencode/pi flags, model ids,
//! etc.) lives at the init UI boundary, not in the pulse/worker runtime.
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
//! MODEL ALLOCATION: the tick is one tiny decision (pick the single next move),
//! so the default claude wiring runs it on the fast model (`--model sonnet`);
//! workers do the heavy multi-step execution on the stronger model (`--model
//! opus`). Spend stays bounded because the world-hash gate skips the AI entirely
//! when nothing changed, and the tick emits only one tiny decision. Tune by
//! editing the commands (`looop init` or the file directly).
//!
//! PER-WORKER COMMAND OVERRIDE: `looop worker start --command "…"` (and the
//! contract's `start_worker.command`) replaces the `worker_command` template
//! WHOLESALE for that one worker — the override is a full launch command and
//! must carry `{{prompt_file}}` like the template. looop itself has no runner
//! vocabulary (no model/thinking knobs — the old `{{model}}`/`{{thinking}}`
//! placeholders and `worker_model`/`worker_thinking` keys are REMOVED, and a
//! template still carrying them is refused at launch with a pointer to
//! `looop init`). Policy for WHEN to override belongs in the PLAYBOOK.

use crate::paths::Paths;
use anyhow::{Context, Result};
use std::fs;

/// The inline default config. Originally a copy of the bash `default_config`,
/// it now diverges deliberately: the tick commands no longer carry the
/// `| "$LOOOP_BIN" _ fmt` seam, since output formatting runs in-process
/// (see `runner::run_streamed`).
///
/// The tick prompt is fed via STDIN (no `{{prompt_file}}` placeholder), not
/// argv: the prompt embeds the whole PLAYBOOK + goals + snapshots + journal
/// tail, and a single argv string is capped hard at 128KiB on Linux
/// (MAX_ARG_STRLEN) — a healthy data dir can exceed that, and the failure mode
/// (E2BIG on every beat) is silent backoff churn. Workers still take the
/// placeholder: their stdin is the live attach TTY, and their briefs are small.
pub static DEFAULT_CONFIG: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| wiring_json(CLAUDE_TICK_COMMAND, CLAUDE_WORKER_COMMAND));

/// The claude tick wiring — SINGLE SOURCE for both `DEFAULT_CONFIG` and the
/// `looop init` claude preset (init.rs), so the two can never drift apart.
/// Tick prompt via STDIN (no `{{prompt_file}}`): a single argv string is
/// capped at 128KiB on Linux (MAX_ARG_STRLEN) and the tick prompt can exceed it.
pub const CLAUDE_TICK_COMMAND: &str =
    "claude -p --output-format stream-json --verbose --dangerously-skip-permissions --model sonnet";

/// The claude worker wiring (see `CLAUDE_TICK_COMMAND`). Workers keep the
/// `{{prompt_file}}` placeholder — their stdin is the live attach TTY.
pub const CLAUDE_WORKER_COMMAND: &str =
    "claude --dangerously-skip-permissions --model opus \"$(cat {{prompt_file}})\"";

/// Assemble the wiring JSON from the two command strings the user supplied to
/// `looop init`. Pure serialization — NO per-runner knowledge lives here; the
/// commands are whatever the operator typed (seeded from the claude default).
pub fn wiring_json(tick: &str, worker: &str) -> String {
    let v = serde_json::json!({
        "tick_command": tick,
        "worker_command": worker,
    });
    serde_json::to_string_pretty(&v).expect("config json") + "\n"
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
            DEFAULT_CONFIG.clone()
        };
        let root: serde_json::Value =
            serde_json::from_str(&text).context("parsing looop config JSON")?;
        Ok(Config { root })
    }

    /// A short label for the configured runner — the first REAL command token
    /// of the tick command (e.g. `pi`, `claude`). Leading `KEY=VAL` env
    /// assignments and a leading `env` (plus its flags) are skipped, so
    /// `FOO=1 claude …` and `env -i claude …` both label as `claude`.
    /// For log lines only.
    pub fn runner_label(&self) -> String {
        self.runner_cmd("tick_command")
            .or_else(|| self.runner_cmd("worker_command"))
            .and_then(|c| first_command_token(&c))
            .unwrap_or_else(|| "runner".into())
    }

    /// Fetch a wiring command by key (`tick_command` / `worker_command`).
    ///
    /// BACK-COMPAT: the keys were once the bare `tick` / `interactive` (and an
    /// unused `resume`). A new key falls back to its pre-rename name, so configs
    /// written before the rename keep working without a re-init.
    ///
    /// Any trailing `| "$LOOOP_BIN" _ fmt` seam from a pre-in-process-metering
    /// config is also stripped here (`strip_fmt_seam`).
    pub fn runner_cmd(&self, key: &str) -> Option<String> {
        let legacy = match key {
            "tick_command" => Some("tick"),
            "worker_command" => Some("interactive"),
            _ => None,
        };
        self.root
            .get(key)
            .or_else(|| legacy.and_then(|l| self.root.get(l)))?
            .as_str()
            .map(strip_fmt_seam)
    }
}

/// First real command token of a shell command line: skips leading `KEY=VAL`
/// env assignments and a leading `env` (with its flags), so an env-prefixed
/// wiring labels as the actual program, not `FOO=1`/`env`. Whitespace-token
/// based (best effort): a quoted `env -S "FOO=1 claude …"` splits on spaces
/// like everything else here — acceptable for a log-only label.
fn first_command_token(cmd: &str) -> Option<String> {
    let mut after_env = false;
    for tok in cmd.split_whitespace() {
        // KEY=VAL assignment (identifier before the `=`): env prefix, skip.
        if let Some((key, _)) = tok.split_once('=')
            && !key.is_empty()
            && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            continue;
        }
        if tok == "env" {
            after_env = true;
            continue;
        }
        // `env`'s own flags (e.g. `-i`, `-S`, `--split-string`) are not the
        // command either — but only AFTER an `env`, so a real command's flags
        // (there are none before the command token) never match.
        if after_env && tok.starts_with('-') {
            continue;
        }
        return Some(tok.to_string());
    }
    None
}

/// Strip a trailing `| <bin> _ fmt` (or `_fmt`) seam from a runner command.
///
/// Tick output formatting moved in-process (`runner::run_streamed`), so
/// the old external pipe is dead. Older configs still carry it; rather
/// than force a re-seed (which would clobber user edits) we drop the seam on load.
/// Only the LAST pipe segment is inspected, and only when its FIRST token is
/// recognisably the looop binary itself (`looop`, `…/looop`, or the
/// `"$LOOOP_BIN"` the old seed used) followed by a `fmt`/`_fmt` verb — an
/// unrelated user pipeline (e.g. `… | grep looop_fmt`) is left untouched.
fn strip_fmt_seam(cmd: &str) -> String {
    // KNOWN LIMITATION: rfind('|') is quote-blind — if the seam text appears
    // QUOTED inside the command (e.g. `echo '… | looop _ fmt'`), the cut lands
    // inside the string literal. Accepted: no real wiring quotes the old seam.
    if let Some(idx) = cmd.rfind('|') {
        let toks: Vec<&str> = cmd[idx + 1..].split_whitespace().collect();
        if let Some(first) = toks.first() {
            // The invoked binary, with shell quoting peeled off.
            let bin = first.trim_matches(|c| c == '"' || c == '\'');
            let is_looop_bin = bin == "looop"
                || bin.ends_with("/looop")
                || bin == "$LOOOP_BIN"
                || bin == "${LOOOP_BIN}";
            let has_fmt_verb = toks[1..].iter().any(|t| *t == "fmt" || *t == "_fmt");
            if is_looop_bin && has_fmt_verb {
                return cmd[..idx].trim_end().to_string();
            }
        }
    }
    cmd.to_string()
}

/// True once the operator has run `looop init` (the config file exists). `looop
/// up` gates on this and refuses to start the pulse when false, directing the
/// user to `looop init`.
pub fn is_initialized(paths: &Paths) -> bool {
    paths.config.is_file()
}

/// Write the runner wiring to $LOOOP_CONFIG (creating its parent dir). Used by
/// `looop init`; always overwrites any existing file.
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
    use super::{first_command_token, strip_fmt_seam};

    #[test]
    fn runner_label_skips_env_prefixes() {
        // Plain command: first token wins (unchanged behavior).
        assert_eq!(
            first_command_token("claude -p --model sonnet").unwrap(),
            "claude"
        );
        // KEY=VAL prefixes are skipped…
        assert_eq!(
            first_command_token("FOO=1 BAR=2 claude -p").unwrap(),
            "claude"
        );
        // …as is a leading `env` and its flags.
        assert_eq!(
            first_command_token("env FOO=1 claude -p").unwrap(),
            "claude"
        );
        assert_eq!(first_command_token("env -i claude -p").unwrap(), "claude");
        // A $VAR-invoked binary still labels as the token (log-only).
        assert_eq!(
            first_command_token("\"$LOOOP_TICK_BIN\" -p").unwrap(),
            "\"$LOOOP_TICK_BIN\""
        );
        // Nothing but assignments → no label (caller falls back to "runner").
        assert_eq!(first_command_token("FOO=1"), None);

        let cfg = super::Config {
            root: serde_json::json!({ "tick_command": "RUST_LOG=debug env -i pi -p" }),
        };
        assert_eq!(cfg.runner_label(), "pi");
    }

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
        // A user pipeline that merely MENTIONS looop + fmt is NOT the seam:
        // the first tail token must be the looop binary itself.
        let grep = "claude -p | grep looop_fmt";
        assert_eq!(strip_fmt_seam(grep), grep);
        let awk = "claude -p | awk '/looop/ {print}' # _fmt";
        assert_eq!(strip_fmt_seam(awk), awk);
        // Path-qualified looop binary still counts as the seam.
        assert_eq!(
            strip_fmt_seam("claude -p | /usr/local/bin/looop _ fmt"),
            "claude -p"
        );
    }

    #[test]
    fn default_config_has_no_fmt_seam() {
        assert!(!super::DEFAULT_CONFIG.contains("_ fmt"));
        assert!(!super::DEFAULT_CONFIG.contains("_fmt"));
    }

    #[test]
    fn default_config_is_valid_claude_wiring() {
        let cfg = super::Config {
            root: serde_json::from_str(&super::DEFAULT_CONFIG).expect("default config parses"),
        };
        assert_eq!(cfg.runner_label(), "claude");
        let worker = cfg.runner_cmd("worker_command").unwrap();
        // The worker prompt placeholder survives JSON round-trip un-escaped.
        assert!(worker.contains("{{prompt_file}}"));
        assert!(worker.contains("$(cat"));
        // The tick prompt travels via STDIN (no placeholder): a single argv
        // string is capped at 128KiB on Linux (MAX_ARG_STRLEN), and the tick
        // prompt — PLAYBOOK + goals + snapshots + journal — can exceed it.
        let tick = cfg.runner_cmd("tick_command").unwrap();
        assert!(!tick.contains("{{prompt_file}}"));
    }

    #[test]
    fn wiring_json_round_trips_the_two_commands() {
        let json = super::wiring_json("T cmd", "W {{prompt_file}}");
        let cfg = super::Config {
            root: serde_json::from_str(&json).expect("wiring json parses"),
        };
        assert_eq!(cfg.runner_cmd("tick_command").unwrap(), "T cmd");
        assert_eq!(
            cfg.runner_cmd("worker_command").unwrap(),
            "W {{prompt_file}}"
        );
    }

    #[test]
    fn runner_cmd_falls_back_to_legacy_keys() {
        // A pre-rename config (bare keys + the now-unused `resume`) still reads.
        let cfg = super::Config {
            root: serde_json::json!({
                "tick": "old tick",
                "interactive": "old worker",
                "resume": "old resume"
            }),
        };
        assert_eq!(cfg.runner_cmd("tick_command").unwrap(), "old tick");
        assert_eq!(cfg.runner_cmd("worker_command").unwrap(), "old worker");
    }
}
