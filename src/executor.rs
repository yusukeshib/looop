//! EXECUTE — the typed actions that mutate looop's world, exposed as `looop _ …`
//! verbs the ROOT AGENT calls. Each verb builds one [`Action`] and runs it
//! through [`run_action`], which journals the move and (for the non-idempotent
//! ones) write-ahead-logs the intent so a crash mid side-effect is surfaced, not
//! silently re-fired.
//!
//! Historically the decide phase was looop's OWN LLM call: the tick wrote one
//! JSON action to `.decision.json` and looop executed it. The judgment now lives
//! in the root agent (an external pi/claude session); looop no longer decides.
//! What survives is the execution half — these typed, gated mutations — re-homed
//! from "the thing the tick AI emitted" to "the verbs the root agent invokes".

use crate::paths::Paths;
use crate::session;
use anyhow::{Context, Result, bail};
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::process::ExitCode;

/// One typed mutation of looop's world. Built by the `_ …` verb handlers below
/// (no longer deserialized from an LLM decision) and run through [`run_action`].
#[derive(Debug, PartialEq)]
pub enum Action {
    /// The escape hatch: one ad-hoc, reversible shell command (gh query, draft,
    /// …). looop runs it (and can gate it) — arbitrary power, but ONE command,
    /// logged, not an open-ended agent session.
    RunShell { cmd: String, reason: String },
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
    /// Surface a blocker / notice to the human. Journaled; if `notification` is
    /// wired in config, looop also fires that command (best-effort).
    SendNotification { message: String, id: String },
}

/// Reject a file-name segment that could escape the data dir or hit a dotfile.
fn safe_segment(kind: &str, id: &str) -> Result<()> {
    if id.is_empty() || id.contains('/') || id.contains('\\') || id.starts_with('.') || id == ".." {
        bail!("invalid {kind} id {id:?}");
    }
    Ok(())
}

/// A short, stable word naming the action's category — for the typed stdout
/// line and the `action` field on the decided event.
pub fn kind(action: &Action) -> &'static str {
    match action {
        Action::RunShell { .. } => "shell",
        Action::WriteGoal { .. } => "goal",
        Action::ArchiveGoal { .. } => "archive",
        Action::WriteSensor { .. } => "sensor",
        Action::WritePlaybook { .. } => "playbook",
        Action::StartWorker { .. } => "worker",
        Action::SendNotification { .. } => "notify",
    }
}

/// The goal id an action targets, if any — used to stamp the per-goal activity
/// ledger that drives the `sys-goals` staleness reading (so the decider can see
/// which goals it's been neglecting and avoid starving them). Actions with no
/// goal association (noop, run_shell, write_sensor, write_playbook, notify)
/// return None.
fn goal_of(action: &Action) -> Option<String> {
    match action {
        Action::WriteGoal { id, .. } => Some(id.clone()),
        Action::ArchiveGoal { id } => Some(id.clone()),
        Action::StartWorker { id, .. } => Some(id.clone()),
        _ => None,
    }
}

/// Stamp `id` as acted-on "now" in the goal-activity ledger (goal id -> unix
/// secs). Best-effort: a write failure just means the staleness reading is a
/// beat stale.
fn record_goal_activity(paths: &Paths, id: &str) {
    let path = paths.goal_activity();
    let mut map: serde_json::Map<String, serde_json::Value> = fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    map.insert(id.to_string(), serde_json::json!(crate::util::now_unix()));
    let _ = fs::write(&path, serde_json::Value::Object(map).to_string());
}

/// Whether re-running this action a second time can cause a DUPLICATE,
/// non-reversible effect (a second PR comment, a second notification fired).
/// These are the actions the write-ahead intent log guards (H: crash between
/// the side effect and the world-hash commit must not silently double-fire).
/// Everything else is an idempotent overwrite (write_goal/sensor/playbook),
/// has its own dedup guard (start_worker's same-id alive check), or is a
/// best-effort nudge whose re-send is harmless.
fn is_non_idempotent(action: &Action) -> bool {
    matches!(
        action,
        Action::RunShell { .. } | Action::SendNotification { .. }
    )
}

/// A stable fingerprint of a non-idempotent action's payload, so a crash report
/// names WHICH command may have half-run. Not used for dedup (the next beat's
/// AI re-decides freshly); purely diagnostic.
fn action_fingerprint(action: &Action) -> String {
    let canon = match action {
        Action::RunShell { cmd, .. } => format!("run_shell\n{cmd}"),
        Action::SendNotification { message, id } => {
            format!("send_notification\n{message}\n{id}")
        }
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
    let _ = fs::write(paths.action_wal(), body);
}

/// Clear the intent record once execute() has returned (Ok OR Err): reaching
/// this line proves the process did not die DURING the side effect, so there is
/// nothing to recover. Only an actual crash between begin/clear leaves it.
fn clear_intent(paths: &Paths) {
    let _ = fs::remove_file(paths.action_wal());
}

/// At beat start: if a write-ahead intent record survived, the previous beat
/// died mid non-idempotent side effect (run_shell / send_notification) before
/// it could commit the world hash. We do NOT auto-retry (a duplicate command is
/// worse than a missed one); we surface it durably so a human can check whether
/// the command actually ran. Idempotent. Returns true when an interrupted
/// action was found and reported.
pub fn warn_if_interrupted(paths: &Paths) -> bool {
    let wal = paths.action_wal();
    let Ok(raw) = fs::read_to_string(&wal) else {
        return false;
    };
    let _ = fs::remove_file(&wal); // one-shot report
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
        Action::RunShell { cmd, reason } => {
            // `bash -c` (NOT `-lc`): a non-interactive, non-login shell sources no
            // rc files, so the command runs against looop's inherited environment
            // rather than re-running the operator's login profile every beat
            // (hermetic + cheaper).
            let out = std::process::Command::new("bash")
                .arg("-c")
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

        Action::SendNotification { message, id } => {
            let msg = message.trim();
            if msg.is_empty() {
                bail!("send_notification: empty message");
            }
            // The journal line IS the notice. If the operator wired a
            // `notification` command in config, also fire it (best-effort,
            // detached — a failed hook never fails the tick) so a flag can pop
            // a tmux window onto the waiting worker.
            fire_notification_hook(paths, msg, id.trim());
            Ok(format!("notify · {msg}"))
        }
    }
}

/// Fire the operator's optional `notification` command (config `.notification`)
/// when a `send_notification` action runs. Substitutes `{{message}}` / `{{id}}`
/// and exports `$LOOOP_MESSAGE` / `$LOOOP_ID`, then spawns it DETACHED via bash
/// in the data dir. Best-effort: a missing hook, a config error, or a spawn
/// failure is silently ignored — the notice is already journaled, so the hook is
/// pure surfacing and must never fail (or block) the tick.
fn fire_notification_hook(paths: &Paths, message: &str, id: &str) {
    let Ok(cfg) = crate::config::Config::load(paths) else {
        return;
    };
    let Some(cmd) = cfg.notification() else {
        return;
    };
    let rendered = cmd.replace("{{message}}", message).replace("{{id}}", id);
    let _ = std::process::Command::new("bash")
        .arg("-c") // non-login: inherit looop's env, don't re-source the login profile
        .arg(&rendered)
        .current_dir(&paths.data_dir)
        .env("LOOOP_MESSAGE", message)
        .env("LOOOP_ID", id)
        .spawn(); // detached — do NOT wait; the tick moves on immediately
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
        clear_intent(paths);
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

/// Resolve an action body from positional args, falling back to stdin when none
/// are given (so the root agent can heredoc a multi-line goal/PLAYBOOK body).
fn body_or_stdin(rest: &[String]) -> Result<String> {
    if !rest.is_empty() {
        return Ok(rest.join(" "));
    }
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .context("reading body from stdin")?;
    Ok(buf)
}

/// Strip `--journal <text>` from args, returning (journal, remaining args).
fn take_journal(args: &[String]) -> (Option<String>, Vec<String>) {
    let mut journal = None;
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--journal" {
            journal = it.next().cloned();
        } else {
            rest.push(a.clone());
        }
    }
    (journal, rest)
}

fn ok(summary: String) -> Result<ExitCode> {
    println!("{summary}");
    Ok(ExitCode::SUCCESS)
}

/// `looop _ goal write <id> [body…|stdin]` | `looop _ goal archive <id>`
pub fn cmd_goal(paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let (journal, rest) = take_journal(args);
    match rest.first().map(String::as_str) {
        Some("write") => {
            let Some(id) = rest.get(1).cloned() else {
                eprintln!("usage: looop _ goal write <id> [body…|stdin]");
                return Ok(ExitCode::from(1));
            };
            let body = body_or_stdin(&rest[2.min(rest.len())..])?;
            ok(run_action(
                paths,
                &Action::WriteGoal { id, body },
                journal.as_deref(),
            )?)
        }
        Some("archive") => {
            let Some(id) = rest.get(1).cloned() else {
                eprintln!("usage: looop _ goal archive <id>");
                return Ok(ExitCode::from(1));
            };
            ok(run_action(
                paths,
                &Action::ArchiveGoal { id },
                journal.as_deref(),
            )?)
        }
        _ => {
            eprintln!("usage: looop _ goal write <id> [body…] | looop _ goal archive <id>");
            Ok(ExitCode::from(1))
        }
    }
}

/// `looop _ sensor write <name> [script…|stdin]`
pub fn cmd_sensor(paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let (journal, rest) = take_journal(args);
    if rest.first().map(String::as_str) != Some("write") {
        eprintln!("usage: looop _ sensor write <name> [script…|stdin]");
        return Ok(ExitCode::from(1));
    }
    let Some(name) = rest.get(1).cloned() else {
        eprintln!("usage: looop _ sensor write <name> [script…|stdin]");
        return Ok(ExitCode::from(1));
    };
    let script = body_or_stdin(&rest[2.min(rest.len())..])?;
    ok(run_action(
        paths,
        &Action::WriteSensor { name, script },
        journal.as_deref(),
    )?)
}

/// `looop _ playbook write [body…|stdin]`
pub fn cmd_playbook(paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let (journal, rest) = take_journal(args);
    if rest.first().map(String::as_str) != Some("write") {
        eprintln!("usage: looop _ playbook write [body…|stdin]");
        return Ok(ExitCode::from(1));
    }
    let body = body_or_stdin(&rest[1.min(rest.len())..])?;
    ok(run_action(
        paths,
        &Action::WritePlaybook { body },
        journal.as_deref(),
    )?)
}

/// `looop _ run <cmd…> [--reason TEXT]` — one ad-hoc, REVERSIBLE shell command.
pub fn cmd_run(paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let (journal, mut rest) = take_journal(args);
    let mut reason = String::new();
    if let Some(i) = rest.iter().position(|a| a == "--reason") {
        rest.remove(i);
        if i < rest.len() {
            reason = rest.remove(i);
        }
    }
    let cmd = rest.join(" ");
    if cmd.trim().is_empty() {
        eprintln!("usage: looop _ run <cmd…> [--reason TEXT]");
        return Ok(ExitCode::from(1));
    }
    ok(run_action(
        paths,
        &Action::RunShell { cmd, reason },
        journal.as_deref(),
    )?)
}

/// `looop _ worker start <id> <prompt…>` — spawn a worker session (journaled).
pub fn cmd_worker_start(paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let (journal, rest) = take_journal(args);
    let Some(id) = rest.first().cloned() else {
        eprintln!("usage: looop _ worker start <id> <prompt…>");
        return Ok(ExitCode::from(1));
    };
    let prompt = body_or_stdin(&rest[1.min(rest.len())..])?;
    if prompt.trim().is_empty() {
        eprintln!("usage: looop _ worker start <id> <prompt…>");
        return Ok(ExitCode::from(1));
    }
    ok(run_action(
        paths,
        &Action::StartWorker { id, prompt },
        journal.as_deref(),
    )?)
}

/// `looop _ notify <message…> [--id WORKER]` — surface a notice to the human.
pub fn cmd_notify(paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let (journal, mut rest) = take_journal(args);
    let mut id = String::new();
    if let Some(i) = rest.iter().position(|a| a == "--id") {
        rest.remove(i);
        if i < rest.len() {
            id = rest.remove(i);
        }
    }
    let message = rest.join(" ");
    if message.trim().is_empty() {
        eprintln!("usage: looop _ notify <message…> [--id WORKER]");
        return Ok(ExitCode::from(1));
    }
    ok(run_action(
        paths,
        &Action::SendNotification { message, id },
        journal.as_deref(),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_segment_blocks_traversal() {
        assert!(safe_segment("goal", "ok").is_ok());
        for bad in ["", "..", "a/b", ".hidden", "a\\b"] {
            assert!(safe_segment("goal", bad).is_err(), "should reject {bad:?}");
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
    fn notify_journals_and_rejects_empty() {
        let p = Paths::temp();
        let summary = run_action(
            &p,
            &Action::SendNotification {
                message: "goals A and B conflict".into(),
                id: String::new(),
            },
            None,
        )
        .unwrap();
        assert_eq!(summary, "notify · goals A and B conflict");
        assert!(
            execute(
                &p,
                &Action::SendNotification {
                    message: "  ".into(),
                    id: String::new(),
                }
            )
            .is_err()
        );
    }

    #[test]
    fn only_run_shell_and_notification_are_guarded() {
        assert!(is_non_idempotent(&Action::RunShell {
            cmd: "gh pr comment".into(),
            reason: String::new()
        }));
        assert!(is_non_idempotent(&Action::SendNotification {
            message: "x".into(),
            id: String::new()
        }));
        assert!(!is_non_idempotent(&Action::WriteGoal {
            id: "g".into(),
            body: "b".into()
        }));
        assert!(!is_non_idempotent(&Action::StartWorker {
            id: "w".into(),
            prompt: "p".into()
        }));
    }

    #[test]
    fn run_action_clears_wal_around_a_guarded_action() {
        let p = Paths::temp();
        run_action(
            &p,
            &Action::SendNotification {
                message: "creds expired".into(),
                id: String::new(),
            },
            Some("told human"),
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
                prompt: "p".into()
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
            goal_of(&Action::SendNotification {
                message: "x".into(),
                id: String::new(),
            }),
            None
        );
    }
}
