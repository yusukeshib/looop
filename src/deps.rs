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

/// The binary a command line actually invokes: the first token that is not a
/// leading `VAR=value` environment assignment (`FOO=1 claude -p` → `claude`).
/// Quote-aware: a token opening with `'` or `"` yields the QUOTED SPAN
/// (`'/path/with spaces/claude' -p` → `/path/with spaces/claude`), so quoted
/// binaries resolve on PATH instead of failing the preflight.
///
/// This is deliberately NOT a shell parser: tokens that need real shell
/// quoting rules — backslash escapes (`path\ with\ spaces`, `"a\"b"`),
/// embedded/unterminated quotes — return `None`, which SKIPS the preflight
/// check for that command (the same treatment `require_deps` gives
/// `$VAR` binaries). A naive parse of those tokens produced FALSE "missing
/// dependency" reports, which hard-gate every verb; skipping is the safe
/// failure mode for a preflight.
fn command_bin(cmd: &str) -> Option<&str> {
    let mut rest = cmd.trim_start();
    while !rest.is_empty() {
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let tok = &rest[..end];
        if is_env_assign(tok) {
            rest = rest[end..].trim_start();
            continue;
        }
        // Backslashes mean shell escaping we do not model — unparseable.
        if tok.contains('\\') {
            return None;
        }
        // Peel surrounding quotes: the binary is the quoted span (which may
        // contain whitespace). An unterminated quote or an escaped span is
        // unparseable.
        let first = tok.as_bytes()[0];
        if first == b'\'' || first == b'"' {
            let close = rest[1..].find(first as char)?;
            let span = &rest[1..1 + close];
            if span.contains('\\') {
                return None;
            }
            return Some(span);
        }
        // A quote EMBEDDED mid-token (`foo"bar"`) needs concatenation rules
        // we do not model — unparseable.
        if tok.contains('\'') || tok.contains('"') {
            return None;
        }
        return Some(tok);
    }
    None
}

/// True for a shell `NAME=value` prefix token (NAME = [A-Za-z_][A-Za-z0-9_]*).
fn is_env_assign(tok: &str) -> bool {
    match tok.split_once('=') {
        Some((name, _)) => {
            !name.is_empty()
                && !name.starts_with(|c: char| c.is_ascii_digit())
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        None => false,
    }
}

/// Verify hard prerequisites; bail with install hints listing everything
/// missing at once (so the user fixes it in one pass).
pub fn require_deps(paths: &Paths) -> Result<()> {
    let mut missing: Vec<(String, &'static str)> = Vec::new();

    // looop runs its per-beat decide through `tick_command` AND launches
    // workers through `worker_command`, so a missing binary in EITHER is a
    // hard prereq. Resolve from $LOOOP_CONFIG when present, else the inline
    // default, and check each command's real binary token (skipping any
    // leading VAR=value environment assignments).
    if let Ok(cfg) = Config::load(paths) {
        for key in ["tick_command", "worker_command"] {
            if let Some(cmd) = cfg.runner_cmd(key)
                && let Some(bin) = command_bin(&cmd)
                // A `$VAR` in the binary token is shell expansion we cannot
                // resolve here (`"$LOOOP_BIN" -p`) — skip the check rather
                // than hard-gate every verb on a false negative.
                && !bin.contains('$')
                && !crate::util::on_path(bin)
                && !missing.iter().any(|(b, _)| b == bin)
            {
                missing.push((bin.to_string(), dep_hint(bin)));
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_bin_skips_leading_env_assignments() {
        assert_eq!(command_bin("claude -p"), Some("claude"));
        assert_eq!(command_bin("FOO=1 BAR=x_y claude -p"), Some("claude"));
        // Not an assignment: `9X=1` has a digit-leading name; `a b=c` is fine.
        assert_eq!(command_bin("9X=1"), Some("9X=1"));
        assert_eq!(command_bin(""), None);
        assert_eq!(command_bin("FOO=only assignments=no"), None);
    }

    #[test]
    fn command_bin_peels_quotes_and_surfaces_shell_variables() {
        // Quoted binaries resolve to the unquoted name/path.
        assert_eq!(command_bin("\"claude\" -p"), Some("claude"));
        assert_eq!(command_bin("'claude' -p"), Some("claude"));
        assert_eq!(
            command_bin("'/path/with spaces/claude' -p"),
            Some("/path/with spaces/claude")
        );
        assert_eq!(
            command_bin("FOO=1 \"/opt/my tools/pi\" --model x"),
            Some("/opt/my tools/pi")
        );
        // Shell variables come back verbatim — require_deps skips them.
        assert_eq!(command_bin("\"$LOOOP_BIN\" -p"), Some("$LOOOP_BIN"));
        assert_eq!(command_bin("$LOOOP_BIN -p"), Some("$LOOOP_BIN"));
        // Unterminated quote: unparseable — skip rather than misreport.
        assert_eq!(command_bin("'claude -p"), None);
    }

    #[test]
    fn command_bin_skips_unparseable_complex_tokens() {
        // Escaped quote inside a quoted span: a naive close-quote scan would
        // stop at the `\"` and misdetect the binary — skip instead.
        assert_eq!(command_bin("\"a\\\"b\" -p"), None);
        // Backslash-escaped spaces: the shell sees ONE path token, a
        // whitespace split sees two — skip instead of reporting `/my\` missing.
        assert_eq!(command_bin("/my\\ tools/claude -p"), None);
        assert_eq!(command_bin("FOO=1 path\\ with\\ spaces/bin -p"), None);
        // Quote embedded mid-token (shell concatenation): skip.
        assert_eq!(command_bin("foo\"bar\" -p"), None);
        // Env assignments before a complex token are still peeled first.
        assert_eq!(command_bin("FOO=1 claude -p"), Some("claude"));
    }

    #[test]
    fn preflight_skips_unparseable_complex_binaries() {
        let p = crate::paths::Paths::temp();
        // Backslash-escaped paths and escaped quotes cannot be resolved
        // statically — the preflight must NOT hard-gate on a false "missing".
        crate::config::write(
            &p,
            &crate::config::wiring_json("/no\\ such/claude -p", "\"a\\\"b\" {{prompt_file}}"),
        )
        .unwrap();
        assert!(require_deps(&p).is_ok());
    }

    #[test]
    fn preflight_skips_shell_variable_binaries() {
        let p = crate::paths::Paths::temp();
        // Both commands invoke through a $VAR we cannot resolve statically —
        // the preflight must NOT hard-gate on it.
        crate::config::write(
            &p,
            &crate::config::wiring_json(
                "\"$LOOOP_TICK_BIN\" -p",
                "$LOOOP_WORKER_BIN {{prompt_file}}",
            ),
        )
        .unwrap();
        assert!(require_deps(&p).is_ok());
    }

    #[test]
    fn preflight_checks_both_tick_and_worker_commands() {
        let p = crate::paths::Paths::temp();
        // tick binary missing (behind env assignments), worker binary present.
        crate::config::write(
            &p,
            &crate::config::wiring_json(
                "FOO=1 no-such-looop-tick-bin -p",
                "sh -c 'true {{prompt_file}}'",
            ),
        )
        .unwrap();
        let err = require_deps(&p).unwrap_err().to_string();
        assert!(
            err.contains("no-such-looop-tick-bin"),
            "tick command's binary must be preflighted: {err}"
        );

        // Worker binary missing too → listed as well (everything in one pass).
        crate::config::write(
            &p,
            &crate::config::wiring_json(
                "no-such-looop-tick-bin -p",
                "no-such-looop-worker-bin {{prompt_file}}",
            ),
        )
        .unwrap();
        let err = require_deps(&p).unwrap_err().to_string();
        assert!(err.contains("no-such-looop-tick-bin"));
        assert!(err.contains("no-such-looop-worker-bin"));

        // Both present → ok.
        crate::config::write(
            &p,
            &crate::config::wiring_json("sh -c tick", "sh {{prompt_file}}"),
        )
        .unwrap();
        assert!(require_deps(&p).is_ok());
    }
}
