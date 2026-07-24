//! EXECUTE — the typed actions that mutate looop's world. Each is built one of two
//! ways and run through the same gated path ([`run_action`], which journals the
//! move and write-ahead-logs the intent for non-idempotent ones so a crash mid
//! side-effect is surfaced, not silently re-fired):
//!
//!   * AUTONOMOUS — looop's per-beat decide: the `tick` runner writes ONE JSON
//!     action to `.decision.json`; [`consume_decision`] parses + executes it.
//!     This is the primary driver — looop is the brain.
//!   * MANUAL — the `looop …` verbs (cmd_goal/sensor/playbook/run/worker)
//!     a human or client calls to steer by hand. Same [`Action`]s, same gates.
//!
//! looop is the SOLE executor either way: judgment (free to inspect) stays
//! separate from EXECUTION (gated, logged), so risky moves can be checked.

use crate::paths::Paths;
use crate::session;
use crate::store::{FileStore, Key, StateStore};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::fs;
use std::process::ExitCode;

/// One typed mutation of looop's world. Built by the `…` verb handlers below
/// (no longer deserialized from an LLM decision) and run through [`run_action`].
#[derive(Debug, Deserialize, PartialEq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    /// A valid move when nothing needs doing (the decider's explicit "hold").
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
    /// Spawn a worker session for hands-on work. `command` is an optional
    /// per-worker launch-command override, replacing the `worker_command`
    /// template wholesale (it must carry `{{prompt_file}}`; see
    /// `session::cmd_start_session`). Policy for when to override lives in
    /// the PLAYBOOK — looop itself has no runner vocabulary.
    StartWorker {
        id: String,
        prompt: String,
        #[serde(default)]
        command: Option<String>,
        /// Optional post-condition: ONE shell command that must exit 0 once
        /// the work is truly done (compose with `&&`). Run ONCE by the pulse
        /// on the first beat after the worker dies; the verdict is surfaced in
        /// sys-sessions so "exit 0 but nothing happened" wakes the tick as a
        /// FAILED worker instead of a clean corpse. See `verify.rs`.
        #[serde(default)]
        verify: Option<String>,
    },
    /// Terminate a live worker session. The remedy for a STUCK worker (see the
    /// sys-sessions `health` reading): a worker has no input channel, so one
    /// that is alive, not waiting on an ask, and silent past the threshold can
    /// only be killed (and re-dispatched fresh if its goal still needs work).
    /// Refuses the pulse (the control loop is not a worker).
    KillWorker {
        id: String,
        #[serde(default)]
        reason: String,
    },
    /// Create/replace a durable time trigger (`schedules/<name>.json`). Exactly
    /// one of `in_s` (one-shot) / `every_s` (recurring). Unlike
    /// `next_interval_s` it survives restarts and has no 3600s cap — due-ness
    /// wakes the loop through the `sys-schedules` sensor (see `schedule.rs`).
    WriteSchedule {
        name: String,
        #[serde(default)]
        in_s: Option<u64>,
        #[serde(default)]
        every_s: Option<u64>,
        #[serde(default)]
        note: String,
    },
    /// Remove a schedule (a handled one-shot / an obsolete recurring).
    DropSchedule { name: String },
}

/// An action's CATEGORY as a closed enum — what used to be a bare `&'static
/// str` (`"noop"`, `"shell"`, …). The typed form exists so the dispatch sites
/// (`d.kind == ActionKind::Shell` in the tick, the noop-record gate in
/// tick_guards) get match/compare EXHAUSTIVENESS from the compiler instead of
/// silently never matching a typo'd string. [`ActionKind::as_str`] yields the
/// original stable word for the typed stdout line, the journal, and the
/// `action` field on the decided event — the WIRE format is unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    Noop,
    Shell,
    Goal,
    Archive,
    Sensor,
    Playbook,
    Worker,
    Kill,
    Schedule,
    DropSchedule,
}

impl ActionKind {
    /// The short, stable word naming the category (the pre-enum wire format).
    pub fn as_str(self) -> &'static str {
        match self {
            ActionKind::Noop => "noop",
            ActionKind::Shell => "shell",
            ActionKind::Goal => "goal",
            ActionKind::Archive => "archive",
            ActionKind::Sensor => "sensor",
            ActionKind::Playbook => "playbook",
            ActionKind::Worker => "worker",
            ActionKind::Kill => "kill",
            ActionKind::Schedule => "schedule",
            ActionKind::DropSchedule => "drop-schedule",
        }
    }
}

impl std::fmt::Display for ActionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The action's category — for the typed stdout line and the `action` field
/// on the decided event (via [`ActionKind::as_str`]).
pub fn kind(action: &Action) -> ActionKind {
    match action {
        Action::Noop { .. } => ActionKind::Noop,
        Action::RunShell { .. } => ActionKind::Shell,
        Action::WriteGoal { .. } => ActionKind::Goal,
        Action::ArchiveGoal { .. } => ActionKind::Archive,
        Action::WriteSensor { .. } => ActionKind::Sensor,
        Action::WritePlaybook { .. } => ActionKind::Playbook,
        Action::StartWorker { .. } => ActionKind::Worker,
        Action::KillWorker { .. } => ActionKind::Kill,
        Action::WriteSchedule { .. } => ActionKind::Schedule,
        Action::DropSchedule { .. } => ActionKind::DropSchedule,
    }
}

/// The goal id an action targets, if any — used to stamp the per-goal activity
/// ledger that drives the `sys-goals` staleness reading (so the decider can see
/// which goals it's been neglecting and avoid starving them). Actions with no
/// goal association (noop, run_shell, write_sensor, write_playbook) return None.
fn goal_of(action: &Action) -> Option<String> {
    match action {
        Action::WriteGoal { id, .. } => Some(id.clone()),
        Action::ArchiveGoal { id } => Some(id.clone()),
        Action::StartWorker { id, .. } => Some(id.clone()),
        // Worker id == goal id by convention; killing a stuck worker IS acting
        // on that goal (the next beat may re-dispatch it fresh).
        Action::KillWorker { id, .. } => Some(id.clone()),
        _ => None,
    }
}

/// Stamp `id` as acted-on "now" in the goal-activity ledger (goal id -> unix
/// secs). Best-effort: a write failure just means the staleness reading is a
/// beat stale.
///
/// NOTE: this is a lossy read-modify-write — a concurrent pulse beat and a
/// manual `looop` verb can race, and the last writer wins (one stamp may be
/// dropped). Acceptable: the ledger only feeds an advisory staleness reading.
fn record_goal_activity(paths: &Paths, id: &str) {
    let store = FileStore::new(paths);
    let mut map: serde_json::Map<String, serde_json::Value> = store
        .read(&Key::GoalActivity)
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    map.insert(id.to_string(), serde_json::json!(crate::util::now_unix()));
    let _ = store.write_atomic(
        &Key::GoalActivity,
        &serde_json::Value::Object(map).to_string(),
    );
}

/// Whether re-running this action a second time can cause a DUPLICATE,
/// non-reversible effect (a second PR comment). These are the actions the
/// write-ahead intent log guards (H: crash between the side effect and the
/// world-hash commit must not silently double-fire). Everything else is an
/// idempotent overwrite (write_goal/sensor/playbook) or has its own dedup guard
/// (start_worker's same-id alive check).
fn is_non_idempotent(action: &Action) -> bool {
    matches!(action, Action::RunShell { .. })
}

// The write-ahead intent log (begin/clear/scan) lives in `crate::wal`; the
// run_shell deny-list tripwire and the shell knobs live in
// `crate::shell_guard` — both extracted from this file so EXECUTION dispatch
// stays readable on its own.

/// The last `max` chars of `s` (UTF-8 safe).
fn tail_chars(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    s.chars().skip(n - max).collect()
}

/// How many PLAYBOOK generations `playbook.d/` retains (`LOOOP_PLAYBOOK_KEEP`,
/// default 20; 0 = keep all).
fn playbook_keep() -> usize {
    crate::util::env_knob("LOOOP_PLAYBOOK_KEEP").unwrap_or(20)
}

/// Snapshot the CURRENT PLAYBOOK.md into `playbook.d/<ts>.md` before an
/// overwrite, then prune the history to [`playbook_keep`] generations. A
/// missing playbook snapshots nothing; failures are best-effort (the history is
/// a safety net, never a reason to block the write itself).
fn next_playbook_snapshot_path(dir: &std::path::Path, stamp: &str) -> std::path::PathBuf {
    let mut path = dir.join(format!("{stamp}.md"));
    let mut n = 1;
    while path.exists() {
        // Keep collision names lexicographically chronological: pruning sorts
        // paths, so an unpadded `-10` must not sort before `-2`.
        path = dir.join(format!("{stamp}-{n:04}.md"));
        n += 1;
    }
    path
}

fn snapshot_playbook(paths: &Paths) {
    let Ok(current) = fs::read_to_string(paths.playbook()) else {
        return; // no playbook yet — nothing to preserve
    };
    let dir = paths.playbook_history_dir();
    let _ = fs::create_dir_all(&dir);
    // One move per beat makes same-second collisions unlikely; disambiguate
    // with a numeric suffix anyway so a snapshot is never silently clobbered.
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let path = next_playbook_snapshot_path(&dir, &stamp);
    let _ = fs::write(&path, &current);

    let keep = playbook_keep();
    if keep == 0 {
        return;
    }
    let mut snaps = crate::util::sorted_glob(&dir, "md");
    if snaps.len() > keep {
        let excess = snaps.len() - keep;
        for old in snaps.drain(..excess) {
            let _ = fs::remove_file(old);
        }
    }
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
///
/// The executor is SILENT on stdout — looop renders the returned summary. Some
/// underlying calls (the worker spawn's `started …` banner, babysit's
/// send/key/restart chatter) print CLI-friendly lines; we suppress fd 1 around
/// them so raw text never leaks into the pulse's structured — and under
/// `--json`, NDJSON — stream.
pub fn execute(paths: &Paths, action: &Action) -> Result<String> {
    session::suppress_stdout(|| execute_inner(paths, action))
}

fn execute_inner(paths: &Paths, action: &Action) -> Result<String> {
    match action {
        Action::Noop { reason } => Ok(if reason.trim().is_empty() {
            "noop".to_string()
        } else {
            format!("noop · {}", reason.trim())
        }),
        Action::RunShell { cmd, reason } => {
            // Tripwire (see denied_shell_pattern): fail the beat BEFORE the
            // command runs. The bail lands in LAST FAILURE via the tick's
            // failure record, so the next decide prompt names the refusal and
            // the decider can rethink instead of retrying blind.
            if !crate::shell_guard::shell_allow_dangerous()
                && let Some(what) = crate::shell_guard::denied_shell_pattern(cmd)
            {
                bail!(
                    "run_shell refused by the safety deny-list — {what}. The command was \
                     NOT executed. If this is a false positive, rephrase it (or the operator \
                     can set LOOOP_SHELL_ALLOW_DANGEROUS=1); otherwise pick a safer move."
                );
            }
            // `bash -c` (NOT `-lc`): a non-interactive, non-login shell sources no
            // rc files, so the command runs against looop's inherited environment
            // rather than re-running the operator's login profile every beat
            // (hermetic + cheaper).
            //
            // Bounded: the command runs through the shared native-timeout path
            // (`verify::run_cmd`) — spawned in its own process group and the
            // WHOLE group killed on deadline — so one hung command can never
            // wedge the pulse forever. `LOOOP_SHELL_TIMEOUT_SECS`, default 300.
            let res = crate::verify::run_cmd(
                &paths.data_dir,
                cmd,
                crate::shell_guard::shell_timeout_secs(),
                "LOOOP_SHELL_TIMEOUT_SECS",
            );
            let code = res.exit_code.unwrap_or(-1);
            // Capture the output TAIL and persist it for the NEXT decide prompt
            // (`RUN_SHELL OUTPUT`). Without this the stdout of a "query" move
            // went nowhere — the decider could ask the world a question but
            // never hear the answer. The tick arms a short cadence nudge after
            // a run_shell so the follow-up beat actually happens (the command's
            // output alone does not move the world hash).
            let tail = tail_chars(&res.output, 2048);
            let body = serde_json::json!({
                "v": 1,
                "ts": crate::util::now_unix(),
                "cmd": cmd,
                "exit_code": code,
                "output": tail,
            });
            if let Err(e) =
                crate::util::write_atomic(&paths.last_shell(), body.to_string().as_bytes())
            {
                crate::util::event(
                    crate::util::Level::Warn,
                    "tick.guard_degraded",
                    &format!(
                        "failed to persist the run_shell output (next prompt won't see it): {e}"
                    ),
                    &[],
                );
            }
            let why = if reason.is_empty() { cmd } else { reason };
            if res.ok {
                Ok(format!("run-shell · {why}"))
            } else {
                // A deadline kill leaves no exit code and stamps the timeout
                // note into the output (verify::run_cmd). Name the
                // PARTIAL-EXECUTION hazard explicitly: the command was killed
                // MID-FLIGHT, so unlike a clean nonzero exit its side effects
                // may have half-landed — the next prompt's LAST FAILURE must
                // tell the decider to verify before re-issuing.
                let partial = if res.exit_code.is_none() && res.output.contains("timed out after") {
                    " — the command may have partially executed; verify its side effects before retrying"
                } else {
                    ""
                };
                // Surface the tail in the error too, so LAST FAILURE names the
                // actual cause instead of just the exit code.
                bail!(
                    "run_shell exited {code}: {why}{partial}\n{}",
                    tail_chars(&res.output, 512)
                );
            }
        }

        Action::WriteGoal { id, body } => {
            crate::util::safe_segment("goal id", id)?;
            FileStore::new(paths)
                .write_atomic(&Key::Goal(id.clone()), &with_trailing_newline(body))?;
            Ok(format!("write-goal {id}"))
        }

        Action::ArchiveGoal { id } => {
            crate::util::safe_segment("goal id", id)?;
            FileStore::new(paths)
                .archive(&Key::Goal(id.clone()))
                .with_context(|| format!("archive_goal {id:?}"))?;
            Ok(format!("archive-goal {id}"))
        }

        Action::WriteSensor { name, script } => {
            crate::util::safe_segment("sensor id", name)?;
            FileStore::new(paths)
                .write_atomic(&Key::Sensor(name.clone()), &with_trailing_newline(script))?;
            Ok(format!("write-sensor {name}"))
        }

        Action::WritePlaybook { body } => {
            // The write API is whole-file replacement and the PLAYBOOK is the
            // most valuable human-authored artifact in the loop, so snapshot
            // the previous body FIRST — one bad rewrite (the decider's or a
            // fat-fingered human's) must never be an unrecoverable loss.
            snapshot_playbook(paths);
            FileStore::new(paths).write_atomic(&Key::Playbook, &with_trailing_newline(body))?;
            Ok("write-playbook".into())
        }

        Action::StartWorker {
            id,
            prompt,
            command,
            verify,
        } => {
            // Reuse the worker-launch path (contract injection, reserved-id
            // guard, corpse reuse, detached spawn).
            let outcome = session::cmd_start_session(
                paths,
                id,
                prompt,
                command.as_deref(),
                verify.as_deref(),
            )?;
            if outcome.code != std::process::ExitCode::SUCCESS {
                // Carry the refusal REASON (fleet cap, duplicate id, bad
                // template, …) into the error: record_failure persists this
                // message, so the next decide prompt's LAST FAILURE section
                // names the cause and the decider can change course instead of
                // repeating the same refused move blind.
                let why = outcome
                    .reason
                    .as_deref()
                    .unwrap_or("refused for an unspecified reason");
                bail!("start_worker {id:?} failed: {why}");
            }
            // Flag a command override in the journal (auditable).
            let mut note = format!("start-worker {id}");
            if outcome.overridden {
                note.push_str(" (command override)");
            }
            Ok(note)
        }

        Action::KillWorker { id, reason } => {
            // Reuses the CLI kill path: pulse guard + in-process babysit kill.
            let code = session::cmd_kill(paths, id)?;
            if code != std::process::ExitCode::SUCCESS {
                bail!("kill_worker {id:?} refused (the pulse is not a worker)");
            }
            Ok(if reason.trim().is_empty() {
                format!("kill-worker {id}")
            } else {
                format!("kill-worker {id} · {}", reason.trim())
            })
        }

        Action::WriteSchedule {
            name,
            in_s,
            every_s,
            note,
        } => crate::schedule::write(paths, name, *in_s, *every_s, note),

        Action::DropSchedule { name } => crate::schedule::remove(paths, name),
    }
}

/// Append one journal line in the canonical `- YYYY-MM-DD HH:MM <text>` format
/// (matching the timestamp the prompt hands the decider).
fn append_journal(paths: &Paths, line: &str) -> Result<()> {
    // The line may be LLM-provided (decision.journal): collapse newlines to
    // spaces so a multi-line value cannot forge EXTRA journal entries — each
    // `- YYYY-MM-DD HH:MM` line must correspond to exactly one audited move.
    let line = line.replace(['\r', '\n'], " ");
    let stamp = crate::util::date_fmt("%Y-%m-%d %H:%M");
    FileStore::new(paths).append_line(&Key::Journal, &format!("- {stamp} {line}"))?;
    Ok(())
}

/// The decider drops its single move here (one JSON object) in the data dir;
/// looop reads it, executes it, and removes it. This is what keeps judgment
/// (the LLM, free to inspect) separate from EXECUTION (looop, the sole gated
/// actor): the move is data looop runs, not a command the model runs itself.
pub const DECISION_FILE: &str = ".decision.json";

/// One tick's decision: the action plus the metadata that rides alongside it.
#[derive(Debug, PartialEq)]
pub struct Decision {
    pub action: Action,
    /// The one journal line looop appends after executing (may be empty; the
    /// executor falls back to a generated summary).
    pub journal: String,
    /// Optional one-shot cadence nudge (seconds); NOT a move. Handed to the pulse
    /// loop in-process via `Decided.next_interval_s` (the loop clamps it).
    pub next_interval_s: Option<u64>,
}

impl Decision {
    /// Parse one decision object. `journal` / `next_interval_s` are lifted out;
    /// the remainder is decoded into the tagged `Action`.
    pub fn parse(json: &str) -> Result<Decision> {
        let v: serde_json::Value =
            serde_json::from_str(strip_code_fence(json)).context("decision is not valid JSON")?;
        let journal = v
            .get("journal")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string();
        let next_interval_s = v.get("next_interval_s").and_then(serde_json::Value::as_u64);
        let action: Action =
            serde_json::from_value(v).context("decision has no/unknown \"action\"")?;
        Ok(Decision {
            action,
            journal,
            next_interval_s,
        })
    }
}

/// Strip a SINGLE leading/trailing markdown code fence (```json … ``` or
/// ``` … ```) around the decision body. LLMs habitually fence their JSON even
/// when told not to; charging a whole failed beat (backoff + LAST FAILURE) for
/// cosmetic wrapping is pure waste. ONE layer only — doubly-fenced or otherwise
/// mangled output is genuinely malformed and still errors through serde.
fn strip_code_fence(s: &str) -> &str {
    let t = s.trim();
    let Some(rest) = t.strip_prefix("```") else {
        return t;
    };
    // Drop the info string (`json`, …) — everything up to the fence line's end.
    let rest = rest.split_once('\n').map_or("", |(_, r)| r);
    match rest.trim_end().strip_suffix("```") {
        Some(inner) => inner.trim(),
        // Unterminated fence: hand the original to serde so the error names
        // the real problem instead of a mangled fragment.
        None => t,
    }
}

/// What looop executed this beat: the action category, the executor's concise
/// summary, the journal line appended, and the decider's one-shot cadence nudge.
#[derive(Debug, PartialEq)]
pub struct Decided {
    pub kind: ActionKind,
    pub summary: String,
    pub journal: String,
    pub next_interval_s: Option<u64>,
}

/// Read + execute the decider's `.decision.json` (one-shot: removed win or lose).
/// `None` ⇒ the decider wrote nothing (no move this beat). `Some(Err)` ⇒ a
/// malformed decision or a failed execute. Reuses [`run_action`] so the move is
/// WAL-guarded, goal-activity-stamped, and journaled exactly like a manual verb.
pub fn consume_decision(paths: &Paths) -> Option<Result<Decided>> {
    let path = paths.data_dir.join(DECISION_FILE);
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        // None ⇒ the decider wrote nothing — PROVEN absence only.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        // Present but unreadable (EACCES/EIO/…): a decision may EXIST that we
        // failed to consume. The old `.ok()?` squashed this into "no move this
        // beat" — fail-open: the beat would commit the world hash over a move
        // that was never executed. Carry the error so the beat FAILS (backoff
        // arms, LAST FAILURE names the cause) and the file is retried next
        // beat (deliberately NOT removed — we could not read what we'd lose).
        Err(e) => {
            return Some(Err(anyhow::anyhow!(
                "cannot read {DECISION_FILE} ({e}) — a decision may exist but could not be consumed"
            )));
        }
    };
    let _ = fs::remove_file(&path); // one-shot, win or lose
    Some((|| {
        let decision = Decision::parse(&raw)?;
        let journal = if decision.journal.trim().is_empty() {
            None
        } else {
            Some(decision.journal.as_str())
        };
        // The stored run_shell output describes the PREVIOUS move, and the
        // prompt behind THIS decision already carried it — consume it now so
        // it is shown exactly once (a run_shell below re-creates it fresh).
        // Deliberately HERE and not in run_action: a manual CLI verb executed
        // between two beats must not eat the output before the decider sees it.
        // Snapshot the record first: if the move below FAILS, the retry prompt
        // must still carry the RUN_SHELL OUTPUT that informed it — restore it,
        // UNLESS the failing move was itself a run_shell that already wrote a
        // fresh record (the fresh failure output is the more useful one).
        let prev_shell = fs::read_to_string(paths.last_shell()).ok();
        let _ = fs::remove_file(paths.last_shell());
        let summary = match run_action(paths, &decision.action, journal) {
            Ok(s) => s,
            Err(e) => {
                if !paths.last_shell().exists()
                    && let Some(prev) = prev_shell
                {
                    let _ = crate::util::write_atomic(&paths.last_shell(), prev.as_bytes());
                }
                return Err(e);
            }
        };
        let journal_line = if decision.journal.trim().is_empty() {
            summary.clone()
        } else {
            decision.journal.clone()
        };
        Ok(Decided {
            kind: kind(&decision.action),
            summary,
            journal: journal_line,
            next_interval_s: decision.next_interval_s,
        })
    })())
}

/// Run one typed action: write-ahead-log the intent for non-idempotent moves,
/// execute, stamp per-goal activity, and append the journal line. `journal`
/// overrides the auto-generated summary as the logged "why" when non-empty.
/// Returns the executor's concise summary.
pub fn run_action(paths: &Paths, action: &Action, journal: Option<&str>) -> Result<String> {
    // Write-ahead the intent for non-idempotent actions so a crash DURING the
    // side effect is detectable next beat instead of silently re-firing.
    // clear_intent runs whether execute returns Ok or Err.
    let wal_body = if is_non_idempotent(action) {
        Some(crate::wal::begin_intent(paths, action))
    } else {
        None
    };
    let exec_result = execute(paths, action);
    if let Some(body) = &wal_body {
        crate::wal::clear_intent(paths, body);
    }
    let summary = exec_result?;
    if let Some(id) = goal_of(action) {
        record_goal_activity(paths, &id);
    }
    let line = match journal {
        Some(j) if !j.trim().is_empty() => j.trim().to_string(),
        _ => summary.clone(),
    };
    // The journal is an AUDIT log, not a commit precondition: the side effect
    // above already happened, so failing the beat here would arm backoff and
    // leave the world hash uncommitted — the same (possibly non-idempotent)
    // move could be re-issued just because the audit line didn't land.
    // Degrade to a warning and report success.
    if let Err(e) = append_journal(paths, &line) {
        crate::util::event(
            crate::util::Level::Warn,
            "tick.guard_degraded",
            &format!("executed ok but failed to append the journal line (audit-trail gap): {e}"),
            &[],
        );
        eprintln!("looop: failed to append the journal line (audit-trail gap): {e}");
    }
    Ok(summary)
}

/// Resolve an action body from the parsed positional words, falling back to
/// stdin when none are given OR a lone `-` is passed (so a human/client can
/// heredoc a multi-line goal/PLAYBOOK body, matching the `answer` convention).
/// clap already rejects mistyped flags (`playbook write --help` prints help
/// instead of writing the literal text), so this no longer has to guard against
/// flag-like bodies — a body that genuinely starts with `--` arrives here only
/// via the `--` end-of-options separator or stdin.
fn resolve_body(words: &[String]) -> Result<String> {
    if words.is_empty() || (words.len() == 1 && words[0] == "-") {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("reading body from stdin")?;
        return Ok(buf);
    }
    Ok(words.join(" "))
}

fn ok(summary: String) -> Result<ExitCode> {
    println!("{summary}");
    Ok(ExitCode::SUCCESS)
}

/// `looop goal write <id> [body…|-]`
pub fn write_goal(
    paths: &Paths,
    id: &str,
    body: &[String],
    journal: Option<&str>,
) -> Result<ExitCode> {
    use crate::contract::Contract;
    let body = resolve_body(body)?;
    ok(crate::contract::LocalContract::new(paths).goal_write(id, &body, journal)?)
}

/// `looop goal archive <id>`
pub fn archive_goal(paths: &Paths, id: &str, journal: Option<&str>) -> Result<ExitCode> {
    use crate::contract::Contract;
    ok(crate::contract::LocalContract::new(paths).goal_archive(id, journal)?)
}

/// `looop sensor write <name> [script…|-]`
pub fn write_sensor(
    paths: &Paths,
    name: &str,
    script: &[String],
    journal: Option<&str>,
) -> Result<ExitCode> {
    use crate::contract::Contract;
    let script = resolve_body(script)?;
    ok(crate::contract::LocalContract::new(paths).sensor_write(name, &script, journal)?)
}

/// `looop playbook write [body…|-]`
pub fn write_playbook(paths: &Paths, body: &[String], journal: Option<&str>) -> Result<ExitCode> {
    use crate::contract::Contract;
    let body = resolve_body(body)?;
    ok(crate::contract::LocalContract::new(paths).playbook_write(&body, journal)?)
}

/// Reassemble `looop run`'s trailing argv into the ONE shell string that
/// run_shell hands to `bash -c`.
///
/// A SINGLE argument passes through VERBATIM: `looop run 'a && b'` is the
/// documented way to run intentional shell syntax, and quoting it would
/// neuter the operators. MULTIPLE arguments are an argv, so each element is
/// shell-quoted (via the shared [`crate::util::shell_quote`]) before joining
/// — the old plain `join(" ")` lost the caller's quoting and re-split
/// `looop run touch "file with spaces"` into four words instead of two.
/// (A single element round-trips identically under either rule, so the two
/// cases can never disagree on it.)
fn shell_command_from_argv(words: &[String]) -> String {
    match words {
        [one] => one.clone(),
        many => many
            .iter()
            .map(|w| crate::util::shell_quote(w))
            .collect::<Vec<_>>()
            .join(" "),
    }
}

/// `looop run [--reason TEXT] <cmd…>` — one ad-hoc, REVERSIBLE shell command.
/// The command is captured verbatim (its own `--flags` pass through), so
/// `--reason`/`--journal` must precede it.
///
/// QUOTING: see [`shell_command_from_argv`] — one argument is the literal
/// shell string (operators intact); several arguments are treated as an argv
/// whose word boundaries survive re-execution.
pub fn cmd_run(paths: &Paths, args: &crate::cli::RunArgs) -> Result<ExitCode> {
    use crate::contract::Contract;
    let cmd = shell_command_from_argv(&args.cmd);
    if cmd.trim().is_empty() {
        eprintln!("usage: looop run [--reason TEXT] <cmd…>");
        return Ok(ExitCode::from(1));
    }
    let reason = args.reason.clone().unwrap_or_default();
    ok(crate::contract::LocalContract::new(paths).run(
        &cmd,
        &reason,
        args.journal.journal.as_deref(),
    )?)
}

/// `looop worker start <id> <prompt…|-> [--command CMD] [--verify CMD]` —
/// spawn a worker session (journaled). `command` optionally replaces the
/// `worker_command` template wholesale for this one worker.
pub fn start_worker(
    paths: &Paths,
    id: &str,
    prompt: &[String],
    command: Option<&str>,
    verify: Option<&str>,
    journal: Option<&str>,
) -> Result<ExitCode> {
    use crate::contract::Contract;
    let prompt = resolve_body(prompt)?;
    if prompt.trim().is_empty() {
        eprintln!("usage: looop worker start <id> <prompt…|->");
        return Ok(ExitCode::from(1));
    }
    ok(crate::contract::LocalContract::new(paths)
        .worker_start(id, &prompt, command, verify, journal)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_body_joins_words_and_keeps_inner_dash() {
        // Inline words join with spaces.
        assert_eq!(
            resolve_body(&["hello".to_string(), "world".to_string()]).unwrap(),
            "hello world"
        );
        // A `-` alongside real words is content, not the stdin sentinel (only a
        // LONE `-`, or no words at all, falls through to stdin — not unit-tested
        // here since it blocks on a real stdin read). clap rejects mistyped flags
        // (`--help` prints help) before they ever reach here, so the old
        // flag-like-body guard is gone; a literal `--word` body arrives only via
        // the `--` separator.
        assert_eq!(
            resolve_body(&["a".to_string(), "-".to_string(), "b".to_string()]).unwrap(),
            "a - b"
        );
    }

    #[test]
    fn safe_segment_blocks_traversal() {
        use crate::util::safe_segment;
        assert!(safe_segment("goal id", "ok").is_ok());
        for bad in ["", "..", "a/b", ".hidden", "a\\b", "a b", "a\tb", "a\nb"] {
            assert!(
                safe_segment("goal id", bad).is_err(),
                "should reject {bad:?}"
            );
        }
    }

    #[test]
    fn run_action_write_and_archive_goal_round_trip() {
        let p = Paths::temp();
        let body = "goal: ship it\nnotes here";
        run_action(
            &p,
            &Action::WriteGoal {
                id: "ship".into(),
                body: body.into(),
            },
            None,
        )
        .unwrap();
        let written = fs::read_to_string(p.goals_dir().join("ship.md")).unwrap();
        assert_eq!(written, format!("{body}\n"), "trailing newline normalized");

        run_action(&p, &Action::ArchiveGoal { id: "ship".into() }, None).unwrap();
        assert!(!p.goals_dir().join("ship.md").exists());
        assert!(p.goals_dir().join("archive").join("ship.md").exists());
    }

    #[test]
    fn run_action_journals_and_stamps_goal_activity() {
        let p = Paths::temp();
        run_action(
            &p,
            &Action::WriteGoal {
                id: "triage".into(),
                body: "do it".into(),
            },
            Some("made triage"),
        )
        .unwrap();
        let journal = fs::read_to_string(p.journal()).unwrap();
        assert!(journal.contains("made triage"), "journal line appended");
        assert!(journal.starts_with("- "), "canonical journal prefix");
        let act: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(p.goal_activity()).unwrap()).unwrap();
        assert!(
            act.get("triage")
                .and_then(serde_json::Value::as_u64)
                .is_some(),
            "acting on a goal stamps its activity time"
        );
    }

    #[test]
    fn kill_worker_action_parses_and_is_goal_stamped() {
        // The tick's JSON decision shape round-trips into the typed action…
        let a: Action =
            serde_json::from_str(r#"{"action":"kill_worker","id":"triage","reason":"stuck 22m"}"#)
                .unwrap();
        assert_eq!(
            a,
            Action::KillWorker {
                id: "triage".into(),
                reason: "stuck 22m".into()
            }
        );
        // …`reason` is optional…
        assert!(serde_json::from_str::<Action>(r#"{"action":"kill_worker","id":"t"}"#).is_ok());
        // …and killing a worker counts as acting on its goal (worker id == goal
        // id), so sys-goals staleness doesn't misreport the goal as neglected.
        assert_eq!(goal_of(&a), Some("triage".to_string()));
        assert_eq!(kind(&a), ActionKind::Kill);
        assert_eq!(kind(&a).as_str(), "kill", "the wire word is unchanged");
        assert!(!is_non_idempotent(&a));
    }

    #[test]
    fn playbook_overwrite_snapshots_the_previous_body() {
        let p = Paths::temp();
        // No playbook yet: the first write has nothing to preserve.
        run_action(
            &p,
            &Action::WritePlaybook {
                body: "v1 rules".into(),
            },
            None,
        )
        .unwrap();
        assert!(
            crate::util::sorted_glob(&p.playbook_history_dir(), "md").is_empty(),
            "nothing to snapshot before the first playbook"
        );

        // Overwriting preserves the OLD body in playbook.d/.
        run_action(
            &p,
            &Action::WritePlaybook {
                body: "v2 rules".into(),
            },
            None,
        )
        .unwrap();
        let snaps = crate::util::sorted_glob(&p.playbook_history_dir(), "md");
        assert_eq!(snaps.len(), 1);
        assert_eq!(fs::read_to_string(&snaps[0]).unwrap(), "v1 rules\n");
        assert_eq!(
            fs::read_to_string(p.playbook()).unwrap(),
            "v2 rules\n",
            "the live playbook carries the new body"
        );
    }

    #[test]
    fn playbook_snapshot_collision_names_sort_chronologically() {
        let p = Paths::temp();
        fs::write(p.playbook(), "original\n").unwrap();
        let dir = p.playbook_history_dir();
        fs::create_dir_all(&dir).unwrap();
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
        fs::write(dir.join(format!("{stamp}.md")), "base").unwrap();
        for n in 1..=10 {
            fs::write(dir.join(format!("{stamp}-{n:04}.md")), "collision").unwrap();
        }

        let next = next_playbook_snapshot_path(&dir, &stamp);
        assert_eq!(
            next.file_name().unwrap().to_string_lossy(),
            format!("{stamp}-0011.md")
        );
    }

    #[test]
    fn run_shell_output_is_captured_and_consumed_once() {
        let p = Paths::temp();
        run_action(
            &p,
            &Action::RunShell {
                cmd: "echo query-result".into(),
                reason: "probe".into(),
            },
            None,
        )
        .unwrap();
        let raw = fs::read_to_string(p.last_shell()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["exit_code"], 0);
        assert!(v["output"].as_str().unwrap().contains("query-result"));

        // A MANUAL verb between beats must NOT eat it — the decider hasn't
        // seen it yet (the prompt is built at decide time).
        run_action(
            &p,
            &Action::Noop {
                reason: "manual".into(),
            },
            None,
        )
        .unwrap();
        assert!(
            p.last_shell().is_file(),
            "a manual action leaves the record for the decider"
        );

        // The next DECISION consumes it (its prompt carried the output).
        fs::write(
            p.data_dir.join(DECISION_FILE),
            r#"{"action":"noop","reason":"seen it","journal":"ok"}"#,
        )
        .unwrap();
        consume_decision(&p).unwrap().unwrap();
        assert!(!p.last_shell().is_file(), "consumed by the next decision");
    }

    #[test]
    fn failed_decision_restores_the_run_shell_output_for_the_retry_prompt() {
        let p = Paths::temp();
        // A previous beat's run_shell output is on record.
        crate::util::write_atomic(
            &p.last_shell(),
            br#"{"v":1,"cmd":"echo x","exit_code":0,"output":"query-result"}"#,
        )
        .unwrap();
        // This decision FAILS to execute (traversal id is refused): the retry
        // prompt must still carry the RUN_SHELL OUTPUT that informed it.
        fs::write(
            p.data_dir.join(DECISION_FILE),
            r#"{"action":"write_goal","id":"../evil","body":"x","journal":"bad"}"#,
        )
        .unwrap();
        consume_decision(&p).unwrap().unwrap_err();
        let raw = fs::read_to_string(p.last_shell()).expect("record restored on failure");
        assert!(raw.contains("query-result"));

        // But a failing run_shell decision writes a FRESH record — the fresh
        // failure output wins over the stale restore.
        fs::write(
            p.data_dir.join(DECISION_FILE),
            r#"{"action":"run_shell","cmd":"echo fresh; exit 7","journal":"probe"}"#,
        )
        .unwrap();
        consume_decision(&p).unwrap().unwrap_err();
        let raw = fs::read_to_string(p.last_shell()).unwrap();
        assert!(raw.contains("fresh"), "the fresh record is kept: {raw}");
    }

    #[test]
    fn failed_run_shell_records_output_and_names_it_in_the_error() {
        let p = Paths::temp();
        let err = run_action(
            &p,
            &Action::RunShell {
                cmd: "echo boom >&2; exit 7".into(),
                reason: String::new(),
            },
            None,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("boom"),
            "LAST FAILURE sees the cause"
        );
        let raw = fs::read_to_string(p.last_shell()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["exit_code"], 7);
    }

    #[test]
    fn only_run_shell_is_guarded() {
        assert!(is_non_idempotent(&Action::RunShell {
            cmd: "gh pr comment".into(),
            reason: String::new()
        }));
        assert!(!is_non_idempotent(&Action::WriteGoal {
            id: "g".into(),
            body: "b".into()
        }));
        assert!(!is_non_idempotent(&Action::StartWorker {
            id: "w".into(),
            prompt: "p".into(),
            command: None,
            verify: None
        }));
    }

    #[test]
    fn run_action_clears_wal_around_a_guarded_action() {
        let p = Paths::temp();
        run_action(
            &p,
            &Action::RunShell {
                cmd: "true".into(),
                reason: "noop check".into(),
            },
            Some("ran a guarded command"),
        )
        .unwrap();
        assert!(
            p.action_wals().is_empty(),
            "the write-ahead intent is cleared once execute returns"
        );
        assert!(
            !crate::wal::warn_if_interrupted(&p),
            "no interrupted action to report"
        );
    }

    #[test]
    fn run_shell_times_out_and_kills_the_command() {
        // Serialize with other env-mutating tests, and restore the knob even
        // if an assert below panics.
        let _env = crate::util::test_env_lock();
        struct Restore;
        impl Drop for Restore {
            fn drop(&mut self) {
                unsafe { std::env::remove_var("LOOOP_SHELL_TIMEOUT_SECS") };
            }
        }
        let _restore = Restore;
        let p = Paths::temp();
        // Short native timeout via the env override the run_shell path reads.
        unsafe { std::env::set_var("LOOOP_SHELL_TIMEOUT_SECS", "1") };
        let t0 = std::time::Instant::now();
        let err = run_action(
            &p,
            &Action::RunShell {
                cmd: "echo pre; sleep 30".into(),
                reason: String::new(),
            },
            None,
        )
        .unwrap_err();
        assert!(
            t0.elapsed().as_secs() < 10,
            "must not wait out the 30s sleep"
        );
        assert!(
            err.to_string().contains("timed out after 1s"),
            "error names the timeout: {err}"
        );
        assert!(
            err.to_string().contains("may have partially executed"),
            "a deadline kill must name the partial-execution hazard: {err}"
        );
        // The captured record carries the timeout diagnosis for the next prompt.
        let raw = fs::read_to_string(p.last_shell()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["exit_code"], -1, "a killed command has no exit code");
        assert!(v["output"].as_str().unwrap().contains("timed out after 1s"));
    }

    #[test]
    fn shell_command_from_argv_quotes_multi_arg_and_passes_single_through() {
        // MULTI-ARG: each element is quoted, so word boundaries survive — the
        // old join(" ") re-split "file with spaces" into three words.
        let multi = shell_command_from_argv(&["touch".into(), "file with spaces".into()]);
        assert_eq!(multi, "'touch' 'file with spaces'");
        // Round-trip through a real shell: exactly TWO words come out.
        let out = std::process::Command::new("bash")
            .arg("-c")
            .arg(format!("count() {{ echo $#; }}; count {multi}"))
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "2");

        // SINGLE-ARG: verbatim pass-through keeps intentional shell syntax
        // (`looop run 'a && b'`) — quoting it would neuter the operators.
        assert_eq!(
            shell_command_from_argv(&["echo a && echo b".into()]),
            "echo a && echo b"
        );
    }

    #[test]
    fn denied_run_shell_fails_the_beat_without_executing() {
        // Reads the bypass knob — serialize with the env-mutating bypass test
        // so its LOOOP_SHELL_ALLOW_DANGEROUS=1 can't leak into this run.
        let _env = crate::util::test_env_lock();
        let p = Paths::temp();
        let canary = p.data_dir.join("canary");
        let err = run_action(
            &p,
            &Action::RunShell {
                // The deny-list must fire BEFORE bash: the sudo prefix trips it
                // even though the rest of the command is harmless.
                cmd: format!("sudo touch {}", canary.display()),
                reason: String::new(),
            },
            None,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("deny-list"),
            "the refusal names the tripwire so LAST FAILURE explains it: {err}"
        );
        assert!(
            err.to_string().contains("LOOOP_SHELL_ALLOW_DANGEROUS"),
            "the refusal names the escape hatch: {err}"
        );
        assert!(!canary.exists(), "the command must not have run");
    }

    #[test]
    fn deny_list_bypass_knob_allows_denied_commands() {
        let _env = crate::util::test_env_lock();
        struct Restore;
        impl Drop for Restore {
            fn drop(&mut self) {
                unsafe { std::env::remove_var("LOOOP_SHELL_ALLOW_DANGEROUS") };
            }
        }
        let _restore = Restore;
        unsafe { std::env::set_var("LOOOP_SHELL_ALLOW_DANGEROUS", "1") };
        let p = Paths::temp();
        // A command the deny-list would refuse (sudo prefix) runs — and merely
        // fails on its own merits (exit code), never on the tripwire.
        let err = run_action(
            &p,
            &Action::RunShell {
                cmd: "sudo --bogus-flag-that-fails 2>/dev/null; exit 9".into(),
                reason: String::new(),
            },
            None,
        )
        .unwrap_err();
        assert!(
            !err.to_string().contains("deny-list"),
            "the knob bypasses the tripwire entirely: {err}"
        );
        assert!(
            err.to_string().contains("exited 9"),
            "bash actually ran: {err}"
        );
    }

    #[test]
    fn journal_newlines_are_collapsed_to_one_entry() {
        let p = Paths::temp();
        append_journal(&p, "did a thing\n- 2020-01-01 00:00 forged entry").unwrap();
        let journal = fs::read_to_string(p.journal()).unwrap();
        assert_eq!(
            journal.lines().count(),
            1,
            "an LLM journal value with newlines must not forge extra entries: {journal}"
        );
        assert!(
            journal.contains("did a thing - 2020-01-01 00:00 forged entry"),
            "the payload is preserved, just flattened: {journal}"
        );
    }

    #[test]
    fn decision_parse_strips_a_single_code_fence() {
        // A fenced but otherwise valid reply must not cost a beat.
        let fenced = "```json\n{\"action\":\"noop\",\"reason\":\"r\",\"journal\":\"j\"}\n```";
        let d = Decision::parse(fenced).unwrap();
        assert_eq!(d.journal, "j");
        assert_eq!(d.action, Action::Noop { reason: "r".into() });

        // Bare fence (no info string) too.
        let bare = "```\n{\"action\":\"noop\",\"reason\":\"\"}\n```";
        Decision::parse(bare).unwrap();

        // ONE layer only: double-fenced garbage is genuinely malformed.
        let double = "```\n```json\n{\"action\":\"noop\"}\n```\n```";
        assert!(
            Decision::parse(double).is_err(),
            "nested fences are not silently unwrapped"
        );

        // An unfenced decision still parses exactly as before.
        Decision::parse("{\"action\":\"noop\"}").unwrap();
    }

    #[test]
    fn corrupt_decision_is_consumed_one_shot() {
        let p = Paths::temp();
        let path = p.data_dir.join(DECISION_FILE);
        fs::write(&path, "this is not json {{{").unwrap();
        let res = consume_decision(&p).expect("a file was present");
        assert!(res.is_err(), "garbage is a parse error, not a silent skip");
        assert!(
            !path.exists(),
            "the corrupt decision is removed win or lose — it must not wedge every beat"
        );
        assert!(consume_decision(&p).is_none(), "one-shot: nothing left");
    }

    #[test]
    fn unreadable_decision_is_an_error_not_a_silent_skip() {
        // Regression: `.ok()?` mapped EVERY read error to "the decider wrote
        // nothing", so an EACCES/EIO on a PRESENT decision file silently
        // dropped the move while the beat committed the world hash over it.
        // A DIRECTORY at the decision path is the portable stand-in for such
        // a non-NotFound read failure (read_to_string errors with EISDIR).
        let p = Paths::temp();
        let path = p.data_dir.join(DECISION_FILE);
        fs::create_dir_all(&path).unwrap();
        let res = consume_decision(&p).expect("an unreadable decision is not 'no decision'");
        let err = res.expect_err("the read failure must surface as the beat's error");
        assert!(
            err.to_string().contains("could not be consumed"),
            "the error names the consume failure: {err}"
        );
    }

    #[test]
    fn goal_of_maps_only_goal_targeting_actions() {
        assert_eq!(
            goal_of(&Action::StartWorker {
                id: "triage".into(),
                prompt: "p".into(),
                command: None,
                verify: None
            }),
            Some("triage".into())
        );
        assert_eq!(
            goal_of(&Action::WriteGoal {
                id: "ship".into(),
                body: "b".into()
            }),
            Some("ship".into())
        );
        assert_eq!(
            goal_of(&Action::RunShell {
                cmd: "echo hi".into(),
                reason: String::new(),
            }),
            None
        );
    }
}
