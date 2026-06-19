//! EXECUTE — looop deterministically performs the ONE typed action the decider
//! emitted, then journals it. This is what makes RULE 1 real: the decide phase
//! is symmetric with the sense phase (sensors emit JSON describing the world;
//! the decider emits JSON describing its single move), and looop — the
//! unbreakable shell — is the SOLE executor. A tick can therefore do at most one
//! move no matter how the model misbehaves, and irreversible action types can be
//! gated in code rather than by prompt discipline.
//!
//! The decider's contract: emit exactly one JSON object describing the move,
//! e.g. `{"action":"start_worker","id":"triage","prompt":"…","journal":"why"}`.
//! `journal` (the one-line log entry) and `next_interval_s` (an optional cadence
//! nudge, NOT a move) ride alongside the action tag and are stripped before the
//! action itself is decoded.

// Foundation slice: the schema + parser + executor land first and are exercised
// by the unit tests below. The tick wiring (prompt cutover + output capture)
// arrives in the next commit; drop this allow when `execute` gains a caller.
#![allow(dead_code)]

use crate::paths::Paths;
use crate::session;
use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// The single move the decider chose, tagged by `action`. Unknown sibling keys
/// (journal, next_interval_s, reason, …) are ignored here — `Decision` lifts the
/// metadata out before this is decoded.
#[derive(Debug, Deserialize, PartialEq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    /// A valid move when nothing needs doing.
    Noop {
        #[serde(default)]
        reason: String,
    },
    /// The escape hatch: one ad-hoc, reversible shell command (gh query, draft,
    /// …). looop runs it (and can gate it) — arbitrary power, but ONE command,
    /// logged, not an open-ended agent session.
    RunShell {
        cmd: String,
        #[serde(default)]
        reason: String,
    },
    /// Create or update goals/<id>.md.
    WriteGoal { id: String, body: String },
    /// Move goals/<id>.md -> goals/archive/<id>.md.
    ArchiveGoal { id: String },
    /// Create or update sensors/<name>.sh.
    WriteSensor { name: String, script: String },
    /// Replace PLAYBOOK.md.
    WritePlaybook { body: String },
    /// Spawn a worker session for hands-on work.
    StartWorker { id: String, prompt: String },
    /// Type text into a live worker's stdin.
    SteerSession { id: String, input: String },
    /// Send named keys (Enter, C-c, …) to a live worker.
    SendKey { id: String, keys: Vec<String> },
    /// Restart a wedged worker's wrapped command.
    RestartSession { id: String },
}

/// One tick's decision: the action plus the metadata that rides alongside it.
#[derive(Debug, PartialEq)]
pub struct Decision {
    pub action: Action,
    /// The one journal line looop appends after executing (may be empty; the
    /// executor falls back to a generated summary).
    pub journal: String,
    /// Optional one-shot cadence nudge (seconds); NOT a move. Clamped by the
    /// pulse the same way the legacy .next-interval file was.
    pub next_interval_s: Option<u64>,
}

impl Decision {
    /// Parse one decision object. `journal` / `next_interval_s` are lifted out;
    /// the remainder is decoded into the tagged `Action`.
    pub fn parse(json: &str) -> Result<Decision> {
        let v: serde_json::Value =
            serde_json::from_str(json.trim()).context("decision is not valid JSON")?;
        let journal = v
            .get("journal")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string();
        let next_interval_s = v.get("next_interval_s").and_then(|x| x.as_u64());
        let action: Action =
            serde_json::from_value(v).context("decision has no/unknown \"action\"")?;
        Ok(Decision {
            action,
            journal,
            next_interval_s,
        })
    }
}

/// Execute the decided action deterministically. Returns a short human summary
/// of what was done (used for the journal fallback + stdout rendering). The
/// caller owns appending the journal line and applying `next_interval_s`.
pub fn execute(paths: &Paths, action: &Action) -> Result<String> {
    match action {
        Action::Noop { reason } => Ok(if reason.is_empty() {
            "noop".into()
        } else {
            format!("noop · {reason}")
        }),

        Action::StartWorker { id, prompt } => {
            // Reuse the worker-launch path (contract injection, reserved-id
            // guard, corpse reuse, detached spawn).
            let code = session::cmd_start_session(paths, &[id.clone(), prompt.clone()])?;
            if code != std::process::ExitCode::SUCCESS {
                bail!("start_worker '{id}' failed");
            }
            Ok(format!("start-worker {id}"))
        }

        // Wired in subsequent slices (schema + parsing land first).
        other => bail!("action not yet wired into the executor: {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_noop_with_journal() {
        let d = Decision::parse(r#"{"action":"noop","reason":"quiet","journal":"nothing to do"}"#)
            .unwrap();
        assert_eq!(
            d.action,
            Action::Noop {
                reason: "quiet".into()
            }
        );
        assert_eq!(d.journal, "nothing to do");
        assert_eq!(d.next_interval_s, None);
    }

    #[test]
    fn parses_start_worker_and_lifts_metadata() {
        let d = Decision::parse(
            r#"{"action":"start_worker","id":"triage","prompt":"do it","journal":"started triage","next_interval_s":15}"#,
        )
        .unwrap();
        assert_eq!(
            d.action,
            Action::StartWorker {
                id: "triage".into(),
                prompt: "do it".into()
            }
        );
        assert_eq!(d.journal, "started triage");
        assert_eq!(d.next_interval_s, Some(15));
    }

    #[test]
    fn parses_run_shell_escape_hatch() {
        let d = Decision::parse(r#"{"action":"run_shell","cmd":"gh pr list","reason":"check"}"#)
            .unwrap();
        assert_eq!(
            d.action,
            Action::RunShell {
                cmd: "gh pr list".into(),
                reason: "check".into()
            }
        );
    }

    #[test]
    fn parses_all_remaining_variants() {
        for (json, want) in [
            (
                r#"{"action":"write_goal","id":"g","body":"b"}"#,
                Action::WriteGoal {
                    id: "g".into(),
                    body: "b".into(),
                },
            ),
            (
                r#"{"action":"archive_goal","id":"g"}"#,
                Action::ArchiveGoal { id: "g".into() },
            ),
            (
                r#"{"action":"write_sensor","name":"n","script":"s"}"#,
                Action::WriteSensor {
                    name: "n".into(),
                    script: "s".into(),
                },
            ),
            (
                r#"{"action":"write_playbook","body":"pb"}"#,
                Action::WritePlaybook { body: "pb".into() },
            ),
            (
                r#"{"action":"steer_session","id":"w","input":"y"}"#,
                Action::SteerSession {
                    id: "w".into(),
                    input: "y".into(),
                },
            ),
            (
                r#"{"action":"send_key","id":"w","keys":["Enter"]}"#,
                Action::SendKey {
                    id: "w".into(),
                    keys: vec!["Enter".into()],
                },
            ),
            (
                r#"{"action":"restart_session","id":"w"}"#,
                Action::RestartSession { id: "w".into() },
            ),
        ] {
            assert_eq!(Decision::parse(json).unwrap().action, want, "json: {json}");
        }
    }

    #[test]
    fn rejects_garbage_and_unknown_actions() {
        assert!(Decision::parse("not json").is_err());
        assert!(Decision::parse(r#"{"action":"frobnicate"}"#).is_err());
        assert!(Decision::parse(r#"{"reason":"no action tag"}"#).is_err());
    }
}
