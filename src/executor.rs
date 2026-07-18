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
        /// Resume an ANSWERED, DETACHED ask: the ask's question, the human's
        /// answer, and the checkpoint reference are injected into this
        /// worker's brief, and the ask/answer pair is archived once the
        /// worker launches (settling the sys-asks resume signal).
        #[serde(default)]
        resume: Option<String>,
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

/// A short, stable word naming the action's category — for the typed stdout
/// line and the `action` field on the decided event.
pub fn kind(action: &Action) -> &'static str {
    match action {
        Action::Noop { .. } => "noop",
        Action::RunShell { .. } => "shell",
        Action::WriteGoal { .. } => "goal",
        Action::ArchiveGoal { .. } => "archive",
        Action::WriteSensor { .. } => "sensor",
        Action::WritePlaybook { .. } => "playbook",
        Action::StartWorker { .. } => "worker",
        Action::KillWorker { .. } => "kill",
        Action::WriteSchedule { .. } => "schedule",
        Action::DropSchedule { .. } => "drop-schedule",
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

/// A stable fingerprint of a non-idempotent action's payload, so a crash report
/// names WHICH command may have half-run. Not used for dedup (the next beat's
/// AI re-decides freshly); purely diagnostic.
fn action_fingerprint(action: &Action) -> String {
    let canon = match action {
        Action::RunShell { cmd, .. } => format!("run_shell\n{cmd}"),
        _ => kind(action).to_string(),
    };
    crate::util::content_hash(canon.as_bytes())
}

/// Write the write-ahead intent record just BEFORE a non-idempotent side effect.
/// If the process dies during the effect, this file survives and is detected by
/// [`warn_if_interrupted`] on the next beat.
fn begin_intent(paths: &Paths, action: &Action) {
    let body = serde_json::json!({
        "kind": kind(action),
        "fingerprint": action_fingerprint(action),
        "ts": crate::util::now_unix(),
    })
    .to_string();
    let _ = FileStore::new(paths).write_atomic(&Key::ActionWal, &body);
}

/// Clear the intent record once execute() has returned (Ok OR Err): reaching
/// this line proves the process did not die DURING the side effect, so there is
/// nothing to recover. Only an actual crash between begin/clear leaves it.
///
/// Fingerprint-guarded: the WAL is a single global key, and a concurrent pulse
/// beat + manual `looop run` would otherwise clear EACH OTHER's intent. Only
/// remove the record when the stored fingerprint is OUR action's.
fn clear_intent(paths: &Paths, action: &Action) {
    let store = FileStore::new(paths);
    let ours = action_fingerprint(action);
    let matches = store
        .read(&Key::ActionWal)
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|v| {
            v.get("fingerprint")
                .and_then(|f| f.as_str())
                .map(|f| f == ours)
        })
        .unwrap_or(false);
    if matches {
        let _ = store.remove(&Key::ActionWal);
    }
}

/// At beat start: if a write-ahead intent record survived, the previous beat
/// died mid non-idempotent side effect (run_shell) before
/// it could commit the world hash. We do NOT auto-retry (a duplicate command is
/// worse than a missed one); we surface it durably so a human can check whether
/// the command actually ran. Idempotent. Returns true when an interrupted
/// action was found and reported.
pub fn warn_if_interrupted(paths: &Paths) -> bool {
    let store = FileStore::new(paths);
    let Some(raw) = store.read(&Key::ActionWal) else {
        return false;
    };
    let _ = store.remove(&Key::ActionWal); // one-shot report
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
    let akind = v.get("kind").and_then(|x| x.as_str()).unwrap_or("?");
    let fp = v.get("fingerprint").and_then(|x| x.as_str()).unwrap_or("?");
    crate::util::event(
        crate::util::Level::Warn,
        "tick.interrupted",
        &format!(
            "previous beat died mid '{akind}' (a non-idempotent action) before committing \
             — NOT retried automatically; verify it didn't half-run (fp {fp})"
        ),
        &[
            ("action", serde_json::json!(akind)),
            ("fingerprint", serde_json::json!(fp)),
        ],
    );
    crate::events::emit(
        paths,
        "tick_interrupted",
        serde_json::json!({ "action": akind, "fingerprint": fp }),
    );
    true
}

/// Hard cap on a run_shell command's runtime (seconds): the escape hatch runs
/// inside the pulse beat, so it must be bounded like a verify command.
/// `LOOOP_SHELL_TIMEOUT_SECS`, default 300.
fn shell_timeout_secs() -> u64 {
    std::env::var("LOOOP_SHELL_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(300)
}

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
    std::env::var("LOOOP_PLAYBOOK_KEEP")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(20)
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
                shell_timeout_secs(),
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
            let _ = fs::write(paths.last_shell(), body.to_string());
            let why = if reason.is_empty() { cmd } else { reason };
            if res.ok {
                Ok(format!("run-shell · {why}"))
            } else {
                // Surface the tail in the error too, so LAST FAILURE names the
                // actual cause instead of just the exit code.
                bail!(
                    "run_shell exited {code}: {why}\n{}",
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
            resume,
        } => {
            // Reuse the worker-launch path (contract injection, reserved-id
            // guard, corpse reuse, detached spawn).
            let outcome = session::cmd_start_session(
                paths,
                id,
                prompt,
                command.as_deref(),
                verify.as_deref(),
                resume.as_deref(),
            )?;
            if outcome.code != std::process::ExitCode::SUCCESS {
                bail!("start_worker {id:?} failed");
            }
            // Flag a command override / resume in the journal (auditable).
            let mut note = format!("start-worker {id}");
            if outcome.overridden {
                note.push_str(" (command override)");
            }
            if let Some(r) = resume {
                note.push_str(&format!(" (resume {r})"));
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

        Action::DropSchedule { name } => crate::schedule::drop(paths, name),
    }
}

/// Append one journal line in the canonical `- YYYY-MM-DD HH:MM <text>` format
/// (matching the timestamp the prompt hands the decider).
fn append_journal(paths: &Paths, line: &str) -> Result<()> {
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

/// What looop executed this beat: the action category, the executor's concise
/// summary, the journal line appended, and the decider's one-shot cadence nudge.
#[derive(Debug, PartialEq)]
pub struct Decided {
    pub kind: &'static str,
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
    let raw = fs::read_to_string(&path).ok()?; // None ⇒ decider wrote nothing
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
        let _ = fs::remove_file(paths.last_shell());
        let summary = run_action(paths, &decision.action, journal)?;
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
    let guarded = is_non_idempotent(action);
    if guarded {
        begin_intent(paths, action);
    }
    let exec_result = execute(paths, action);
    if guarded {
        clear_intent(paths, action);
    }
    let summary = exec_result?;
    if let Some(id) = goal_of(action) {
        record_goal_activity(paths, &id);
    }
    let line = match journal {
        Some(j) if !j.trim().is_empty() => j.trim().to_string(),
        _ => summary.clone(),
    };
    append_journal(paths, &line)?;
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

/// `looop run [--reason TEXT] <cmd…>` — one ad-hoc, REVERSIBLE shell command.
/// The command is captured verbatim (its own `--flags` pass through), so
/// `--reason`/`--journal` must precede it.
pub fn cmd_run(paths: &Paths, args: &crate::cli::RunArgs) -> Result<ExitCode> {
    use crate::contract::Contract;
    let cmd = args.cmd.join(" ");
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
    resume: Option<&str>,
    journal: Option<&str>,
) -> Result<ExitCode> {
    use crate::contract::Contract;
    let prompt = resolve_body(prompt)?;
    if prompt.trim().is_empty() {
        eprintln!("usage: looop worker start <id> <prompt…|->");
        return Ok(ExitCode::from(1));
    }
    ok(crate::contract::LocalContract::new(paths)
        .worker_start(id, &prompt, command, verify, resume, journal)?)
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
            act.get("triage").and_then(|v| v.as_u64()).is_some(),
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
        assert_eq!(kind(&a), "kill");
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
            verify: None,
            resume: None
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
            !p.action_wal().exists(),
            "the write-ahead intent is cleared once execute returns"
        );
        assert!(!warn_if_interrupted(&p), "no interrupted action to report");
    }

    #[test]
    fn warn_if_interrupted_detects_and_clears_a_stale_intent() {
        let p = Paths::temp();
        begin_intent(
            &p,
            &Action::RunShell {
                cmd: "gh pr comment 1 -b hi".into(),
                reason: String::new(),
            },
        );
        assert!(p.action_wal().exists(), "intent written before the effect");
        assert!(
            warn_if_interrupted(&p),
            "a leftover intent is reported as an interrupted beat"
        );
        assert!(!p.action_wal().exists(), "the report is one-shot");
        assert!(!warn_if_interrupted(&p));
    }

    #[test]
    fn clear_intent_only_removes_a_matching_fingerprint() {
        let p = Paths::temp();
        let ours = Action::RunShell {
            cmd: "echo ours".into(),
            reason: String::new(),
        };
        let theirs = Action::RunShell {
            cmd: "echo theirs".into(),
            reason: String::new(),
        };
        // A concurrent actor's WAL must survive OUR clear…
        begin_intent(&p, &theirs);
        clear_intent(&p, &ours);
        assert!(
            p.action_wal().exists(),
            "another actor's intent must not be cleared"
        );
        // …while our own is cleared normally.
        begin_intent(&p, &ours);
        clear_intent(&p, &ours);
        assert!(!p.action_wal().exists(), "our own intent is cleared");
    }

    #[test]
    fn run_shell_times_out_and_kills_the_command() {
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
        unsafe { std::env::remove_var("LOOOP_SHELL_TIMEOUT_SECS") };
        assert!(
            t0.elapsed().as_secs() < 10,
            "must not wait out the 30s sleep"
        );
        assert!(
            err.to_string().contains("timed out after 1s"),
            "error names the timeout: {err}"
        );
        // The captured record carries the timeout diagnosis for the next prompt.
        let raw = fs::read_to_string(p.last_shell()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["exit_code"], -1, "a killed command has no exit code");
        assert!(v["output"].as_str().unwrap().contains("timed out after 1s"));
    }

    #[test]
    fn fingerprint_is_stable_and_payload_sensitive() {
        let a = Action::RunShell {
            cmd: "echo a".into(),
            reason: "r1".into(),
        };
        let a2 = Action::RunShell {
            cmd: "echo a".into(),
            reason: "r2-ignored".into(),
        };
        let b = Action::RunShell {
            cmd: "echo b".into(),
            reason: "r1".into(),
        };
        assert_eq!(action_fingerprint(&a), action_fingerprint(&a2));
        assert_ne!(action_fingerprint(&a), action_fingerprint(&b));
    }

    #[test]
    fn goal_of_maps_only_goal_targeting_actions() {
        assert_eq!(
            goal_of(&Action::StartWorker {
                id: "triage".into(),
                prompt: "p".into(),
                command: None,
                verify: None,
                resume: None
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
