//! EXECUTE — looop deterministically performs the ONE typed action the decider
//! emitted, then journals it. This is what makes RULE 1 real: the decide phase
//! is symmetric with the sense phase (sensors emit JSON describing the world;
//! the decider emits JSON describing its single move), and looop — the
//! unbreakable shell — is the SOLE executor. A tick can therefore do at most one
//! move no matter how the model misbehaves, and irreversible action types can be
//! gated in code rather than by prompt discipline.
//!
//! The decider's contract: write exactly one JSON object describing the move to
//! `.decision.json` in the data dir, e.g.
//! `{"action":"start_worker","id":"triage","prompt":"…","journal":"why"}`.
//! `journal` (the one-line log entry looop appends) and `next_interval_s` (an
//! optional cadence nudge, NOT a move) ride alongside the action tag and are
//! lifted out before the action itself is decoded.

use crate::paths::Paths;
use crate::session;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;

/// The one-shot file the decider writes its single move to (relative to the data
/// dir). looop reads, executes, and deletes it each beat — a stale decision can
/// never re-run (level-triggered).
pub const DECISION_FILE: &str = ".decision.json";

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
    /// Create or update sensors/<name>.sh (made executable).
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
    /// Optional one-shot cadence nudge (seconds); NOT a move. Handed to the
    /// pulse via the same `.next-interval` file the loop already clamps + reads.
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

/// Reject a file-name segment that could escape the data dir or hit a dotfile.
fn safe_segment(kind: &str, id: &str) -> Result<()> {
    if id.is_empty() || id.contains('/') || id.contains('\\') || id.starts_with('.') || id == ".." {
        bail!("invalid {kind} id {id:?}");
    }
    Ok(())
}

/// Normalize + guard a worker-session target: strip a legacy `looop-` prefix and
/// refuse the reserved pulse id so a move can never hijack the control loop.
fn worker_target(id: &str) -> Result<String> {
    let s = id.strip_prefix("looop-").unwrap_or(id);
    if s.is_empty() {
        bail!("empty session id");
    }
    if s == session::PULSE_SESSION {
        bail!("'{s}' is the pulse (the control loop), not a worker");
    }
    Ok(s.to_string())
}

fn with_trailing_newline(body: &str) -> String {
    if body.ends_with('\n') {
        body.to_string()
    } else {
        format!("{body}\n")
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

        Action::RunShell { cmd, reason } => {
            let out = std::process::Command::new("bash")
                .arg("-lc")
                .arg(cmd)
                .current_dir(&paths.data_dir)
                .output()
                .with_context(|| format!("run_shell: {cmd}"))?;
            let code = out.status.code().unwrap_or(-1);
            let why = if reason.is_empty() { cmd } else { reason };
            if out.status.success() {
                Ok(format!("run-shell · {why}"))
            } else {
                bail!("run_shell exited {code}: {why}");
            }
        }

        Action::WriteGoal { id, body } => {
            safe_segment("goal", id)?;
            fs::create_dir_all(paths.goals_dir())?;
            fs::write(
                paths.goals_dir().join(format!("{id}.md")),
                with_trailing_newline(body),
            )?;
            Ok(format!("write-goal {id}"))
        }

        Action::ArchiveGoal { id } => {
            safe_segment("goal", id)?;
            let from = paths.goals_dir().join(format!("{id}.md"));
            let archive = paths.goals_dir().join("archive");
            fs::create_dir_all(&archive)?;
            fs::rename(&from, archive.join(format!("{id}.md")))
                .with_context(|| format!("archive_goal {id:?}"))?;
            Ok(format!("archive-goal {id}"))
        }

        Action::WriteSensor { name, script } => {
            safe_segment("sensor", name)?;
            fs::create_dir_all(paths.sensors_dir())?;
            let p = paths.sensors_dir().join(format!("{name}.sh"));
            fs::write(&p, with_trailing_newline(script))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perm = fs::metadata(&p)?.permissions();
                perm.set_mode(0o755);
                fs::set_permissions(&p, perm)?;
            }
            Ok(format!("write-sensor {name}"))
        }

        Action::WritePlaybook { body } => {
            fs::write(paths.playbook(), with_trailing_newline(body))?;
            Ok("write-playbook".into())
        }

        Action::StartWorker { id, prompt } => {
            // Reuse the worker-launch path (contract injection, reserved-id
            // guard, corpse reuse, detached spawn).
            let code = session::cmd_start_session(paths, &[id.clone(), prompt.clone()])?;
            if code != std::process::ExitCode::SUCCESS {
                bail!("start_worker {id:?} failed");
            }
            Ok(format!("start-worker {id}"))
        }

        Action::SteerSession { id, input } => {
            let s = worker_target(id)?;
            session::send(paths, &s, input.clone(), true, false)?;
            Ok(format!("steer {s}"))
        }

        Action::SendKey { id, keys } => {
            let s = worker_target(id)?;
            session::key(paths, &s, keys.clone(), false)?;
            Ok(format!("key {s} · {}", keys.join(" ")))
        }

        Action::RestartSession { id } => {
            let s = worker_target(id)?;
            session::restart(paths, &s, false)?;
            Ok(format!("restart {s}"))
        }
    }
}

/// Append one journal line in the canonical `- YYYY-MM-DD HH:MM <text>` format
/// (matching the timestamp the prompt hands the decider).
fn append_journal(paths: &Paths, line: &str) -> Result<()> {
    let stamp = crate::util::date_fmt("%Y-%m-%d %H:%M");
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.journal())?;
    writeln!(f, "- {stamp} {line}")?;
    Ok(())
}

/// Consume the one-shot decision the decider left in `.decision.json` (if any):
/// execute it, append the journal line, apply any cadence nudge, and remove the
/// file so a stale decision can never re-run. Returns `None` when no decision
/// was written this beat, else the executed action's human summary (or the
/// parse/execution error — the file is removed regardless).
pub fn consume_decision(paths: &Paths) -> Option<Result<String>> {
    let path = paths.data_dir.join(DECISION_FILE);
    let raw = fs::read_to_string(&path).ok()?; // None ⇒ decider wrote nothing
    let _ = fs::remove_file(&path); // one-shot, win or lose

    Some((|| {
        let decision = Decision::parse(&raw)?;
        let summary = execute(paths, &decision.action)?;
        let line = if decision.journal.trim().is_empty() {
            summary.clone()
        } else {
            decision.journal.clone()
        };
        append_journal(paths, &line)?;
        if let Some(secs) = decision.next_interval_s {
            let _ = fs::write(paths.data_dir.join(".next-interval"), format!("{secs}\n"));
        }
        Ok(summary)
    })())
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

    #[test]
    fn safe_segment_blocks_traversal() {
        assert!(safe_segment("goal", "ok").is_ok());
        for bad in ["", "..", "a/b", ".hidden", "a\\b"] {
            assert!(safe_segment("goal", bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn worker_target_refuses_pulse_and_strips_prefix() {
        assert_eq!(worker_target("triage").unwrap(), "triage");
        assert_eq!(worker_target("looop-triage").unwrap(), "triage");
        assert!(worker_target("pulse").is_err());
        assert!(worker_target("").is_err());
    }

    #[test]
    fn execute_write_and_archive_goal_round_trip() {
        let p = Paths::temp();
        let body = "goal: ship it\nnotes here";
        execute(
            &p,
            &Action::WriteGoal {
                id: "ship".into(),
                body: body.into(),
            },
        )
        .unwrap();
        let written = fs::read_to_string(p.goals_dir().join("ship.md")).unwrap();
        assert_eq!(written, format!("{body}\n"), "trailing newline normalized");

        execute(&p, &Action::ArchiveGoal { id: "ship".into() }).unwrap();
        assert!(!p.goals_dir().join("ship.md").exists());
        assert!(p.goals_dir().join("archive").join("ship.md").exists());
    }

    #[test]
    fn consume_decision_executes_journals_and_clears_file() {
        let p = Paths::temp();
        let path = p.data_dir.join(DECISION_FILE);
        fs::write(
            &path,
            r#"{"action":"noop","reason":"all quiet","journal":"did nothing","next_interval_s":30}"#,
        )
        .unwrap();

        let summary = consume_decision(&p)
            .expect("a decision was present")
            .unwrap();
        assert_eq!(summary, "noop · all quiet");
        assert!(!path.exists(), "decision file is one-shot");

        let journal = fs::read_to_string(p.journal()).unwrap();
        assert!(journal.contains("did nothing"), "journal line appended");
        assert!(journal.starts_with("- "), "canonical journal prefix");

        let next = fs::read_to_string(p.data_dir.join(".next-interval")).unwrap();
        assert_eq!(next.trim(), "30");
    }

    #[test]
    fn consume_decision_absent_is_none() {
        let p = Paths::temp();
        assert!(consume_decision(&p).is_none());
    }
}
