//! Worker-launch POLICY: the auto-injected contract, launch-command
//! templating, and `cmd_start_session`'s VALIDATE-THEN-COMMIT pipeline. The
//! fleet operations the gating needs (enumerate, reap, spawn) go through the
//! [`Fleet`] seam, so the policy is unit-tested with an in-memory fake fleet
//! (see the tests below) instead of real process spawns.

use crate::config::Config;
use crate::paths::Paths;
use crate::seed;
use anyhow::Result;
use std::process::ExitCode;

// Single-quoting for `bash -lc` interpolation lives in `util::shell_quote`
// (the ONE shared implementation — see its doc).
use crate::util::shell_quote;

use super::fleet::{BabysitFleet, Fleet, PULSE_SESSION};

/// The outcome of resolving the worker launch command.
#[derive(Debug)]
struct WorkerCmd {
    /// The concrete launch command (`{{prompt_file}}` substituted).
    cmd: String,
    /// True when a per-worker `--command` override replaced the config
    /// template (reported in the banner/journal so the override is auditable).
    overridden: bool,
}

/// Resolve the concrete worker launch command.
///
/// Precedence: the per-worker `override_cmd` (from `--command` /
/// `start_worker.command`) replaces the config template WHOLESALE — looop does
/// no splicing, the override IS the full command. Without an override, the
/// config `worker_command` template is used as-is.
///
/// Either way, exactly ONE placeholder exists — `{{prompt_file}}` — and it is
/// REQUIRED (the prompt file is the worker's only brief channel; a worker has
/// no stdin). looop itself has NO runner vocabulary: how to launch a worker is
/// decided at `looop init` time, per-worker variation is the override above,
/// and the policy for WHEN to override lives in the PLAYBOOK, not in looop.
///
/// REMOVED: the `{{model}}`/`{{thinking}}` placeholders and their
/// `worker_model`/`worker_thinking` config keys. A template still carrying
/// them would silently launch a broken command if expanded empty (`--model
/// --thinking …` — the next flag becomes the value), so it is REFUSED with a
/// pointer to `looop init` instead.
fn build_worker_cmd(
    tmpl: &str,
    override_cmd: Option<&str>,
    prompt_file: &str,
) -> Result<WorkerCmd, String> {
    let (raw, overridden, label) = match override_cmd.map(str::trim).filter(|s| !s.is_empty()) {
        Some(over) => (over, true, "--command override"),
        None => (tmpl, false, "worker_command"),
    };
    for gone in ["{{model}}", "{{thinking}}"] {
        if raw.contains(gone) {
            let hint = if overridden {
                "bake the value directly into the --command string instead"
            } else {
                "bake the value into the command instead: re-run `looop init` or edit the config"
            };
            return Err(format!(
                "{label} still uses the removed {gone} placeholder (and its \
                 worker_model/worker_thinking config keys are gone) — {hint}"
            ));
        }
    }
    if !raw.contains("{{prompt_file}}") {
        return Err(format!(
            "{label} must contain the {{{{prompt_file}}}} placeholder \
             (the prompt file is the worker's brief): {raw:?}"
        ));
    }
    // Shared, quote-aware substitution (same as the tick path): the path is
    // shell-quoted, and a pre-quoted `"{{prompt_file}}"` / `'{{prompt_file}}'`
    // template doesn't end up double-quoted.
    Ok(WorkerCmd {
        cmd: crate::runner::substitute_prompt_file(raw, prompt_file),
        overridden,
    })
}

const CONTRACT: &str = r#"# ⚑ WORKER CONTRACT (auto-injected — must obey)
- Never send notifications (no terminal-notifier or any OS notification). You are
  an agent; surface anything a human must see by ASKing (below) — the human sees
  it through whatever client they run.
- When you need a human decision / info / approval, do NOT guess — ASK. Two modes:
  • QUICK question (answer likely within the hour) — BLOCK on it:
      answer=$("$LOOOP_BIN" ask __ID__ --prompt "<what you need to know>")
    (optionally --ref reports/x.md and/or --options a,b). Use $answer, continue.
    You do NOT need a terminal, stdin, or attach — just call it and read its
    output. Ask once per question; it returns only when answered.
  • LONG wait (the human may take hours or days) — CHECKPOINT and DETACH:
      1) write your FULL state (done / remaining / how to continue) to
         reports/__ID__-checkpoint.md
      2) "$LOOOP_BIN" ask __ID__ --detach --prompt "…" --ref reports/__ID__-checkpoint.md
      3) end your session: "$LOOOP_BIN" kill __ID__
    Do NOT sit idle waiting: when the human answers, looop dispatches a FRESH
    worker with the answer and your checkpoint. Your exit is by design, not a
    failure. When unsure which mode, prefer detach — idling is the expensive
    mistake.
- STEERING: the human can queue mid-task course corrections for you. Between
  major steps (and BEFORE any big/irreversible-adjacent step), run:
    "$LOOOP_BIN" told
  It prints and consumes any pending steering messages (nothing = no output);
  obey them immediately. Steering also rides along automatically on every ask
  answer, prefixed "[steering from the human …]".
- When the task is 100% complete and nothing is waiting, end your own session:
    "$LOOOP_BIN" kill __ID__
  (this lets the pulse prune the corpse). NEVER do this mid-task or while waiting
  on a human.
- LEASE (ONLY if the PLAYBOOK/goal tells you to claim this task) — announce
  ownership BEFORE any work so a tick or sibling can't duplicate/race you:
    "$LOOOP_BIN" claim <name>   # atomic test-and-set; <name> defined by the goal (e.g. one per repo)
  This EXITS NON-ZERO if a live session already holds <name> — if so, do NOT
  proceed: flag the human or pick other work, never race the holder. Release it
  the instant the task is fully done, right before the kill above:
    "$LOOOP_BIN" unclaim <name>
  If you crash the pulse auto-reaps your claim; on a clean finish YOU release it.
  NEVER sit/sleep/poll while holding a claim — act and move on.
- SINGLE-WRITER DATA DIR: the pulse (the tick AI) is the SOLE writer of the
  policy files — PLAYBOOK.md, goals/ and sensors/. By default you write ONLY to
  claims/ (your lease), reports/ (deliverables) and your own code sandbox. Do
  NOT edit PLAYBOOK/goals/sensors: a concurrent tick reads them every beat, so a
  racing writer tears the loop's state. If your task implies a policy change,
  write the proposal to reports/<id>.md and raise an ask — the human (or the
  next tick) applies it. EXCEPTION: if your task is explicitly a meta task (e.g.
  setup or playbook grooming), you MAY edit those files, but you MUST show the
  diff and ASK (above) for human approval BEFORE writing. When unsure whether
  your task is meta, treat the data dir as read-only and propose via reports/.
- WORKSPACE: you start in the loop data dir (read-only context for you, save the
  meta exception above). If your task touches a code repo, provision your OWN
  sandbox FIRST and cd into it — never edit code in the data dir:
    git -C <local-clone> worktree add /tmp/__ID__ -b looop/__ID__ && cd /tmp/__ID__
  (the PLAYBOOK names the repos and which to prefer.)
- DELIVERABLES: write any report / artifact a human will read into the data dir's
  reports/ folder (e.g. reports/<id>.md). That dir PERSISTS across ticks. NEVER
  write deliverables to snapshots/ — the pulse OWNS that dir and prunes/rewrites
  its files on every beat, so anything you leave there is destroyed. Reference
  the reports/ path in your flag note so I know where to look.

---

"#;

/// The result of a worker launch: the process exit code plus whether a
/// per-worker `--command` override replaced the config template (reported in
/// the journal so overrides stay auditable).
pub struct StartOutcome {
    pub code: ExitCode,
    pub overridden: bool,
    /// WHY the start was refused, when it was (`code != SUCCESS`). Carried
    /// back to the executor so the failed move's LAST FAILURE feedback names
    /// the cause (fleet cap, duplicate id, bad template, …) instead of a
    /// generic "failed" — without it the decider repeats the same refused
    /// move blind. The CLI stderr line stays alongside for the human path.
    pub reason: Option<String>,
}

impl StartOutcome {
    fn refused(reason: String) -> Self {
        StartOutcome {
            code: ExitCode::from(1),
            overridden: false,
            reason: Some(reason),
        }
    }
}

/// Commit-phase rollback guard for [`cmd_start_session`]: undoes the side
/// effects taken so far when a LATER commit step fails. The rollbacks used to
/// be hand-scattered across every error return (verify clear ×2 +
/// unarchive_pair), so adding a commit step meant remembering to extend each
/// of them; as an RAII guard, ANY early return (or panic) rolls back
/// automatically and only the one success path calls [`StartRollback::disarm`].
struct StartRollback<'a> {
    paths: &'a Paths,
    session: &'a str,
    /// Set once the resume pair has been archived — the step that then needs
    /// undoing (un-archiving) if the spawn fails, so the resume signal
    /// returns instead of being silently consumed by a worker that never ran.
    archived_resume: Option<&'a str>,
    armed: bool,
}
impl StartRollback<'_> {
    /// The spawn succeeded — the side effects are now legitimate state.
    fn disarm(&mut self) {
        self.armed = false;
    }
}
impl Drop for StartRollback<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(ask_id) = self.archived_resume {
            crate::mailbox::unarchive_pair(self.paths, ask_id);
        }
        // No stale verify for a no-show worker — it would fail the NEXT start
        // of this id for the wrong reason. Idempotent (clear of nothing is a
        // no-op), matching the old unconditional hand-rolled clears.
        // The PROMPT FILE is deliberately NOT rolled back: prompts/<id>.md is
        // rename-published and wholly overwritten by the next start of this
        // id, so the residue is inert — and after a spawn failure it is the
        // only record of the brief that never launched (post-mortem value).
        crate::verify::clear(self.paths, self.session);
    }
}

/// Start one worker session against the real (babysit-backed) fleet. Thin
/// binding over [`start_session`] — the policy itself is fleet-agnostic so the
/// gating is testable with a fake.
pub fn cmd_start_session(
    paths: &Paths,
    id: &str,
    prompt: &str,
    command: Option<&str>,
    verify: Option<&str>,
    resume: Option<&str>,
) -> Result<StartOutcome> {
    let out = start_session(
        paths,
        &BabysitFleet::new(paths),
        id,
        prompt,
        command,
        verify,
        resume,
    )?;
    // The stderr line is a CLI-TRANSPORT concern, emitted HERE and not inside
    // the core: start_session is the transport-agnostic policy, and a
    // contract/TUI caller consumes StartOutcome.reason instead of scraping
    // stderr — the same presenter/core split as cmd_answer / answer().
    if let Some(reason) = &out.reason {
        eprintln!("start-session: {reason}");
    }
    Ok(out)
}

/// Start one worker session. Structured as VALIDATE-THEN-COMMIT: every check
/// (resume context, id, prompt, config, fleet cap, duplicate id, launch
/// command) runs BEFORE the first side effect, so a refusal never leaves a
/// half-started worker behind (in particular: no stale verify record from a
/// start that failed a later step — the old shape stored the verify before
/// resolving the launch command). All fleet access (enumerate / reap / spawn)
/// goes through the [`Fleet`] seam.
#[allow(clippy::too_many_arguments)]
fn start_session(
    paths: &Paths,
    fleet: &dyn Fleet,
    id: &str,
    prompt: &str,
    command: Option<&str>,
    verify: Option<&str>,
    resume: Option<&str>,
) -> Result<StartOutcome> {
    seed::ensure_dirs(paths)?;

    // One shape for every refusal: the reason travels in the outcome, for the
    // executor (whose bail! feeds record_failure) AND for the CLI binding
    // (cmd_start_session prints it to stderr for the human). The core itself
    // never prints — contract-layer policy stays transport-agnostic.
    let refuse = |msg: String| Ok(StartOutcome::refused(msg));

    // ---- VALIDATION PHASE (no side effects) --------------------------------

    // Resolve the RESUME context FIRST: an unknown / not-yet-answered ask id
    // is a decider mistake and must fail the move loudly (LAST FAILURE names
    // it) before anything is spawned. The pair is ARCHIVED just before the
    // spawn (archive-then-spawn, so a crash between the two can't re-dispatch
    // the same resume) and UN-ARCHIVED if the spawn fails, below.
    let resume_block = match resume {
        Some(ask_id) => match crate::mailbox::resume_context(paths, ask_id) {
            Ok(block) => Some(block),
            Err(e) => return refuse(e.to_string()),
        },
        None => None,
    };

    // The id becomes both a path segment (the prompt file) and the session id,
    // so reject traversal/dotfile/separator ids up front — the same guard the
    // executor applies to goal/sensor ids.
    if let Err(e) = crate::util::safe_segment("worker id", id) {
        return refuse(e.to_string());
    }
    if prompt.is_empty() {
        return refuse("missing prompt".to_string());
    }

    // Routed through refuse(), not `?`: a broken config is a validation
    // failure exactly like every other check here, and only the unified
    // refusal shape reaches the decider's LAST FAILURE (a bare Err bypassed
    // it, leaving the failed move without its cause).
    let cfg = match Config::load(paths) {
        Ok(c) => c,
        Err(e) => return refuse(format!("cannot load config: {e}")),
    };
    let runner = cfg.runner_label();
    let Some(tmpl) = cfg.runner_cmd("worker_command") else {
        return refuse("no `worker_command` configured".to_string());
    };

    // The worker's session id IS the goal id (no prefix — the fleet root is
    // looop-exclusive). `pulse` is reserved for the control loop, so a worker
    // can never collide with the pulse.
    if id == PULSE_SESSION {
        return refuse(format!("'{id}' is reserved for the pulse; pick another id"));
    }
    let session = id.to_string();

    // Fleet-size ceiling (`LOOOP_MAX_WORKERS`, default 8; 0 disables): one move
    // per beat bounds the spawn RATE but not the standing fleet — without this,
    // a pathological goal/playbook can accumulate a heavy agent per beat
    // indefinitely. The refusal reaches the decider as a failed move (LAST
    // FAILURE names the cap via `StartOutcome.reason`), so it can kill or wait
    // instead of piling on.
    // Enumerate the fleet ONCE for the two gates below, and fail CLOSED when
    // the enumeration itself fails: the lenient `list()` collapses an I/O
    // error to an empty fleet, which would silently bypass BOTH the cap and
    // the duplicate-id check and admit a worker over N already running. The
    // refusal reaches the decider's LAST FAILURE the same way the cap does,
    // so it can retry or wait instead of piling on.
    let fleet_now = match fleet.try_list() {
        Ok(f) => f,
        Err(e) => {
            return refuse(format!(
                "cannot enumerate the fleet ({e}); refusing to start until the fleet is readable"
            ));
        }
    };

    let cap: usize = crate::util::env_knob("LOOOP_MAX_WORKERS").unwrap_or(8);
    if cap != 0 {
        // Check-then-spawn is a TOCTOU race in principle (two concurrent
        // starts could both pass the count — the pulse's decider races a
        // manual `looop worker start` in another process). Over-admitting by
        // one under that rare interleaving is benign — not worth a lock.
        let live = fleet_now
            .iter()
            .filter(|w| !w.is_pulse() && w.alive)
            .count();
        if live >= cap {
            return refuse(format!(
                "{live} live workers — at the fleet cap (LOOOP_MAX_WORKERS={cap}); \
                 kill or wait out an existing worker first"
            ));
        }
    }

    // Check-then-act (exists → alive → reap-in-commit-phase) races a
    // concurrent start of the same id in principle (same manual-verb
    // concurrency as the cap above); the spawn below fails loudly on a true
    // collision, so the window is accepted.
    let id_taken = fleet_now.iter().any(|s| s.id == session);
    if id_taken && fleet_now.iter().any(|s| s.id == session && s.alive) {
        return refuse(format!("session {session} is already running"));
    }

    // Resolve the launch command LAST among the checks (it needs only the
    // prompt file's PATH, not its contents): a per-worker `--command` override
    // replaces the template wholesale; otherwise the template. Only
    // `{{prompt_file}}` is substituted (looop has no runner vocabulary).
    let prompt_file = paths.prompts_dir().join(format!("{session}.md"));
    let expanded = match build_worker_cmd(&tmpl, command, &prompt_file.to_string_lossy()) {
        Ok(e) => e,
        Err(msg) => return refuse(msg),
    };
    let cmd = expanded.cmd;

    // ---- COMMIT PHASE (side effects; all checks passed) ---------------------

    if id_taken {
        fleet.reap(&session); // reuse the id held by a dead corpse (targeted)
    }

    // GENERATION BOUNDARY: this id is about to be reused, so its previous
    // generation's per-id state — undelivered tells AND any stale verify — is
    // retired via the shared hygiene helper. Tells: one can only be queued for
    // a LIVE worker (cmd_tell refuses corpses), so anything pending here was
    // addressed to the PREVIOUS, now-dead worker; delivering it to the new
    // generation (via `told` or an ask-answer piggyback) would apply stale
    // steering to a worker with a different brief. reap/prune also run this
    // hygiene, but this pre-spawn point holds even when the corpse was removed
    // by some other path — and it is race-safe: cmd_tell can't queue for this
    // id until the worker is alive, i.e. after the spawn below.
    super::on_generation_end(paths, &session);

    // Persist the post-condition BEFORE the spawn: a verify declared for a
    // worker that dies instantly must still be checked on the next beat. (No
    // verify ⇒ nothing to store; the stale one is already cleared above.)
    // Rolled back (cleared) below if a later commit step fails — a stale
    // verify record for a worker that never launched would fail the NEXT
    // start of this id for the wrong reason.
    if let Some(v) = verify.filter(|v| !v.trim().is_empty()) {
        // Routed through refuse(), not a bare `?`: a store failure here must
        // reach the decider's LAST FAILURE through the same unified refusal
        // shape as every other check (a bare Err bypassed it). The hygiene
        // above is idempotent, so refusing after it leaves no half-state.
        if let Err(e) = crate::verify::store(paths, &session, v) {
            return refuse(format!("cannot store the verify command: {e}"));
        }
    }

    // From here on, every failure return rolls back the commit-phase side
    // effects via this guard (see [`StartRollback`]); the success path
    // disarms it after the spawn.
    let mut rollback = StartRollback {
        paths,
        session: &session,
        archived_resume: None,
        armed: true,
    };

    // Prompt via file (avoids quoting hell; also a record of the ask), with the
    // contract prepended.
    // ONE placeholder: a worker's session id IS its goal id (see the `session`
    // binding above), so the old `__SESSION__`/`__ID__` pair always expanded
    // to the same value — two names for one thing invited them to drift apart.
    let contract = CONTRACT.replace("__ID__", id);
    let resume_part = resume_block.as_deref().unwrap_or("");
    // Atomic write (temp + fsync + rename), not fs::write: the worker command
    // reads this file via `$(cat {{prompt_file}})`, and on an id REUSE a
    // truncate-then-write could expose a torn brief to a concurrent reader —
    // rename-publish means the path only ever holds a complete prompt.
    if let Err(e) = crate::util::write_atomic(
        &prompt_file,
        format!("{contract}{resume_part}{prompt}\n").as_bytes(),
    ) {
        return Err(e.into()); // the guard clears the stored verify
    }

    // The worker runs in the DATA dir. The in-process spawner inherits the
    // current process cwd (babysit's Pane uses `std::env::current_dir`), so we
    // `cd` there inside the shell command instead of mutating looop's own cwd.
    // Export LOOOP_SESSION_ID so the worker knows its OWN session id (for its
    // lease claim, etc.) through a looop-branded var.
    // `worker_command` always carries `{{prompt_file}}` (build_worker_cmd
    // enforces it), so the launch reads the brief by PATH and is safe to wrap
    // in the env-gated retry loop (no-op by default). Group with `{ …; }` so a
    // multi-statement wrapper stays gated on the `cd` succeeding.
    let launch = format!(
        "export LOOOP_SESSION_ID={}; cd {} && {{ {rcmd} ; }}",
        shell_quote(&session),
        shell_quote(&paths.data_dir.to_string_lossy()),
        rcmd = crate::runner::wrap_with_retry(&cmd),
    );

    // ARCHIVE-THEN-SPAWN: consume the answered resume pair BEFORE launching.
    // The old order (spawn, then archive) re-dispatched the same resume when a
    // crash landed between the two — the sys-asks resume signal stayed hot with
    // a worker already running. Archiving first makes a crash lose at most one
    // dispatch (recoverable: the record stays under asks/archive/); if the
    // spawn FAILS we un-archive so the resume signal returns.
    //
    // The archive rename IS the resume's CLAIM (exactly one concurrent start
    // wins — see archive_pair): resume_context above only VALIDATED the pair,
    // and two starts racing past that validation would otherwise both spawn a
    // worker carrying the same answer. The loser refuses instead — the guard
    // rolls back the stored verify, and nothing was spawned.
    if let Some(ask_id) = resume {
        if !crate::mailbox::archive_pair(paths, ask_id) {
            return refuse(format!(
                "resume {ask_id}: already consumed by a concurrent start (or the pair could \
                 not be archived)"
            ));
        }
        rollback.archived_resume = Some(ask_id); // now armed to un-archive too
    }

    // Launch the worker detached, IN-PROCESS via the babysit library (no
    // `babysit` binary). babysit re-execs looop as the headless supervisor.
    // `-c`, not `-lc`: a non-login shell sources no rc files, so the worker
    // launches against looop's inherited environment instead of re-running the
    // operator's login profile (hermetic + cheaper). The runner template itself
    // is still a shell string ($(cat ...), &&), so the shell stays.
    // On Err the rollback guard un-archives the resume + clears the verify.
    fleet.spawn(vec!["bash".to_string(), "-c".to_string(), launch], &session)?;
    rollback.disarm(); // launched — the side effects are legitimate state now

    // Label the banner with what actually launched: the override's first
    // token when a per-worker `--command` replaced the template, else the
    // configured runner. Flag the override so it is visible at a glance.
    let (runner, override_note) = if expanded.overridden {
        let tok = cmd
            .split_whitespace()
            .next()
            .unwrap_or("runner")
            .to_string();
        (tok, ", command override")
    } else {
        (runner, "")
    };
    println!(
        "started {session} (runner: {runner}{override_note}, cwd: {})",
        paths.data_dir.display()
    );
    println!("  peek: looop screenshot {id}");
    Ok(StartOutcome {
        code: ExitCode::SUCCESS,
        overridden: expanded.overridden,
        reason: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Session, list, try_is_alive, try_list};
    use std::cell::RefCell;

    #[test]
    fn start_session_refuses_a_resume_without_an_answer() {
        // The resume check runs BEFORE any spawn: an unknown ask id — or one
        // the human hasn't answered yet — must fail the move loudly.
        let p = crate::paths::Paths::temp();
        let out = cmd_start_session(&p, "w", "brief", None, None, Some("ghost-1")).unwrap();
        assert_eq!(out.code, ExitCode::from(1));

        // A pending (unanswered) detached ask is refused too.
        let id = crate::mailbox::ask_detached(&p, "w", "q?", "", &[]).unwrap();
        let out = cmd_start_session(&p, "w", "brief", None, None, Some(&id)).unwrap();
        assert_eq!(out.code, ExitCode::from(1));
    }

    #[test]
    fn concurrent_starts_never_double_dispatch_one_resume() {
        // Regression: resume consumption was validate (resume_context) … then
        // archive — non-atomic, so two concurrent starts could BOTH pass the
        // validation and dispatch workers carrying the SAME answered ask. The
        // archive rename is now the claim gating the spawn (see archive_pair):
        // exactly one start wins, the loser refuses before spawning.
        let p = crate::paths::Paths::temp();
        let _ = crate::seed::ensure_dirs(&p);
        let id = crate::mailbox::ask_detached(&p, "w", "go?", "", &[]).unwrap();
        crate::mailbox::answer(&p, &id, "go", false).unwrap();
        let run = |worker: &str| {
            let fleet = FakeFleet::with(Vec::new());
            let out = start_session(&p, &fleet, worker, "brief", None, None, Some(&id)).unwrap();
            (out, fleet.spawned.borrow().len())
        };
        // Distinct worker ids so neither start trips the duplicate-id gate —
        // the ONLY thing that may serialize them is the resume claim.
        let ((out_a, spawned_a), (out_b, spawned_b)) = std::thread::scope(|s| {
            let a = s.spawn(|| run("a"));
            let b = s.spawn(|| run("b"));
            (a.join().unwrap(), b.join().unwrap())
        });
        let successes = [&out_a, &out_b]
            .iter()
            .filter(|o| o.code == ExitCode::SUCCESS)
            .count();
        assert_eq!(
            successes, 1,
            "exactly one start may consume an answered resume"
        );
        assert_eq!(
            spawned_a + spawned_b,
            1,
            "the losing start must never reach the spawn"
        );
        let reason = [&out_a, &out_b]
            .into_iter()
            .find_map(|o| o.reason.clone())
            .expect("the loser's refusal names its cause");
        assert!(
            reason.contains(&id),
            "the refusal names the contested ask: {reason}"
        );
    }

    #[test]
    fn start_session_fails_closed_when_the_fleet_cannot_be_enumerated() {
        // The lenient list() used to collapse EVERY babysit error to an empty
        // fleet, so a transient I/O failure sailed past the cap AND the
        // duplicate-id gate. The gating path must refuse instead — and the
        // refusal must carry its cause to the decider's LAST FAILURE.
        let p = crate::paths::Paths::temp();
        let _ = crate::seed::ensure_dirs(&p);
        // Sabotage: a regular FILE where babysit's sessions dir belongs makes
        // its read_dir fail — the shape of any transient enumeration error.
        std::fs::write(p.data_dir.join("sessions"), "not a dir").unwrap();
        let out = cmd_start_session(&p, "w", "brief", None, None, None)
            .expect("an enumeration failure is a refusal, not an Err");
        assert_eq!(out.code, ExitCode::from(1));
        let reason = out.reason.expect("the refusal names its cause");
        assert!(
            reason.contains("cannot enumerate the fleet"),
            "the reason names the enumeration failure: {reason}"
        );
        // The fail-closed probes surface the error …
        assert!(try_list(&p).is_err(), "try_list must surface the error");
        assert!(
            try_is_alive(&p, "w").is_err(),
            "try_is_alive must surface the error"
        );
        // … while the lenient sensor path still degrades to empty (warned,
        // not wedged).
        assert!(list(&p).is_empty());
    }

    #[test]
    fn start_session_routes_a_broken_config_through_the_refusal_shape() {
        // A broken config used to escape as a bare Err (`Config::load(paths)?`),
        // bypassing the unified refuse() shape every other validation uses —
        // so the decider's LAST FAILURE never learned the cause. It must now
        // come back as a refused StartOutcome carrying the reason.
        let p = crate::paths::Paths::temp();
        let _ = crate::seed::ensure_dirs(&p);
        std::fs::write(&p.config, "{ not json").unwrap();
        let out = cmd_start_session(&p, "w", "brief", None, None, None)
            .expect("a config error is a refusal, not an Err");
        assert_eq!(out.code, ExitCode::from(1));
        let reason = out.reason.expect("the refusal names its cause");
        assert!(
            reason.contains("cannot load config"),
            "the reason names the config as the culprit: {reason}"
        );
    }

    // ---- Gating policy against an in-memory fake fleet ---------------------
    // The Fleet seam exists exactly for these: cap / duplicate-id / fail-closed
    // are pure POLICY, so they are asserted here without a single real spawn.

    /// In-memory [`Fleet`]: a canned session list (or a forced enumeration
    /// error) plus a record of what the policy asked it to reap/spawn.
    struct FakeFleet {
        sessions: Vec<Session>,
        /// Simulate the enumeration itself failing (transient I/O error).
        fail_list: bool,
        reaped: RefCell<Vec<String>>,
        spawned: RefCell<Vec<String>>,
    }

    impl FakeFleet {
        fn with(sessions: Vec<Session>) -> Self {
            FakeFleet {
                sessions,
                fail_list: false,
                reaped: RefCell::new(Vec::new()),
                spawned: RefCell::new(Vec::new()),
            }
        }
    }

    impl Fleet for FakeFleet {
        fn try_list(&self) -> anyhow::Result<Vec<Session>> {
            if self.fail_list {
                anyhow::bail!("simulated enumeration failure");
            }
            Ok(self.sessions.clone())
        }
        fn reap(&self, session: &str) {
            self.reaped.borrow_mut().push(session.to_string());
        }
        fn spawn(&self, _cmd: Vec<String>, session: &str) -> anyhow::Result<()> {
            self.spawned.borrow_mut().push(session.to_string());
            Ok(())
        }
    }

    fn live(id: &str) -> Session {
        Session {
            id: id.to_string(),
            alive: true,
            ..Default::default()
        }
    }

    fn corpse(id: &str) -> Session {
        Session {
            id: id.to_string(),
            alive: false,
            ..Default::default()
        }
    }

    /// The default cap unless the ambient environment overrides the knob (the
    /// policy reads `LOOOP_MAX_WORKERS` — tests must not mutate process env,
    /// so they adapt to it instead).
    fn cap() -> usize {
        crate::util::env_knob("LOOOP_MAX_WORKERS").unwrap_or(8)
    }

    #[test]
    fn start_session_refuses_at_the_fleet_cap() {
        let cap = cap();
        if cap == 0 {
            return; // the ambient env disabled the cap — nothing to enforce
        }
        let p = crate::paths::Paths::temp();
        let fleet = FakeFleet::with((0..cap).map(|i| live(&format!("w{i}"))).collect());
        let out = start_session(&p, &fleet, "new", "brief", None, None, None).unwrap();
        assert_eq!(out.code, ExitCode::from(1));
        let reason = out.reason.expect("the refusal names its cause");
        assert!(
            reason.contains("fleet cap"),
            "the reason names the cap: {reason}"
        );
        assert!(
            fleet.spawned.borrow().is_empty(),
            "a cap refusal must not spawn"
        );
    }

    #[test]
    fn start_session_cap_ignores_the_pulse_and_corpses() {
        // The cap bounds WORKERS: the pulse (control loop) and dead corpses
        // must not consume slots — cap-1 live workers + a pulse + a corpse is
        // still under the ceiling.
        let cap = cap();
        if cap == 0 {
            return;
        }
        let p = crate::paths::Paths::temp();
        let mut sessions: Vec<Session> = (0..cap - 1).map(|i| live(&format!("w{i}"))).collect();
        sessions.push(live(PULSE_SESSION));
        sessions.push(corpse("old"));
        let fleet = FakeFleet::with(sessions);
        let out = start_session(&p, &fleet, "new", "brief", None, None, None).unwrap();
        assert_eq!(out.code, ExitCode::SUCCESS, "under the cap — admitted");
        assert_eq!(fleet.spawned.borrow().as_slice(), ["new".to_string()]);
    }

    #[test]
    fn start_session_fail_closed_gating_never_reaches_the_spawn() {
        // Fail CLOSED: when the fleet cannot be enumerated, the cap and the
        // duplicate-id gate CANNOT be checked — the start must refuse, not
        // sail through against an assumed-empty fleet.
        let p = crate::paths::Paths::temp();
        let mut fleet = FakeFleet::with(Vec::new());
        fleet.fail_list = true;
        let out = start_session(&p, &fleet, "w", "brief", None, None, None).unwrap();
        assert_eq!(out.code, ExitCode::from(1));
        let reason = out.reason.expect("the refusal names its cause");
        assert!(
            reason.contains("cannot enumerate the fleet"),
            "the reason names the enumeration failure: {reason}"
        );
        assert!(
            fleet.spawned.borrow().is_empty() && fleet.reaped.borrow().is_empty(),
            "an unreadable fleet must block every commit-phase side effect"
        );
    }

    #[test]
    fn start_session_refuses_a_duplicate_live_id() {
        let p = crate::paths::Paths::temp();
        let fleet = FakeFleet::with(vec![live("w")]);
        let out = start_session(&p, &fleet, "w", "brief", None, None, None).unwrap();
        assert_eq!(out.code, ExitCode::from(1));
        let reason = out.reason.expect("the refusal names its cause");
        assert!(
            reason.contains("already running"),
            "the reason names the collision: {reason}"
        );
        assert!(
            fleet.spawned.borrow().is_empty() && fleet.reaped.borrow().is_empty(),
            "a live duplicate must be refused untouched (never reaped)"
        );
    }

    #[test]
    fn start_session_reaps_a_dead_corpse_before_id_reuse() {
        // An id held only by a corpse is REUSABLE: the policy reaps the corpse
        // (targeted) in the commit phase, then spawns under the freed id.
        let p = crate::paths::Paths::temp();
        let fleet = FakeFleet::with(vec![corpse("w")]);
        let out = start_session(&p, &fleet, "w", "brief", None, None, None).unwrap();
        assert_eq!(out.code, ExitCode::SUCCESS);
        assert_eq!(fleet.reaped.borrow().as_slice(), ["w".to_string()]);
        assert_eq!(fleet.spawned.borrow().as_slice(), ["w".to_string()]);
    }

    #[test]
    fn contract_uses_the_single_id_placeholder() {
        // Finding: `__SESSION__` and `__ID__` always expanded to the same
        // value (a worker's session id IS its goal id) — the contract now
        // carries exactly ONE placeholder, fully substituted.
        assert!(
            !CONTRACT.contains("__SESSION__"),
            "the legacy __SESSION__ placeholder is gone"
        );
        assert!(CONTRACT.contains("__ID__"));
        let rendered = CONTRACT.replace("__ID__", "triage");
        assert!(
            !rendered.contains("__"),
            "substituting __ID__ leaves no unexpanded placeholder behind"
        );
    }

    // No override: the template renders with only {{prompt_file}} substituted.
    #[test]
    fn build_worker_cmd_template_default() {
        let tmpl = "pi --model opus @{{prompt_file}}";
        let out = build_worker_cmd(tmpl, None, "/p/x.md").unwrap();
        assert_eq!(out.cmd, "pi --model opus @'/p/x.md'");
        assert!(!out.overridden);
    }

    // The worker path uses the SAME quote-aware substitution as the tick path:
    // a pre-quoted `"{{prompt_file}}"` template is not double-quoted, and a
    // path with shell metacharacters stays a single argument.
    #[test]
    fn build_worker_cmd_quotes_like_the_tick_path() {
        let out = build_worker_cmd("claude @\"{{prompt_file}}\"", None, "/p/x.md").unwrap();
        assert_eq!(out.cmd, "claude @'/p/x.md'");
        let out = build_worker_cmd("claude {{prompt_file}}", None, "/p/a b.md").unwrap();
        assert_eq!(out.cmd, "claude '/p/a b.md'");
    }

    // A --command override replaces the template WHOLESALE.
    #[test]
    fn build_worker_cmd_override_replaces_template() {
        let out = build_worker_cmd(
            "claude @{{prompt_file}}",
            Some("pi --model gpt-6 --no-tools @{{prompt_file}}"),
            "/p/x.md",
        )
        .unwrap();
        assert_eq!(out.cmd, "pi --model gpt-6 --no-tools @'/p/x.md'");
        assert!(out.overridden);
    }

    // A command WITHOUT {{prompt_file}} is refused — override or template:
    // the prompt file is the worker's only brief channel, so such a worker
    // would launch blind.
    #[test]
    fn build_worker_cmd_requires_prompt_placeholder() {
        let err =
            build_worker_cmd("claude @{{prompt_file}}", Some("pi -p"), "/p/x.md").unwrap_err();
        assert!(err.contains("{{prompt_file}}"));
        assert!(err.contains("--command"));

        let err = build_worker_cmd("claude -p", None, "/p/x.md").unwrap_err();
        assert!(err.contains("worker_command"));
    }

    // A blank override falls back to the template (treated as absent).
    #[test]
    fn build_worker_cmd_blank_override_is_ignored() {
        let out = build_worker_cmd("claude @{{prompt_file}}", Some("  "), "/p/x.md").unwrap();
        assert_eq!(out.cmd, "claude @'/p/x.md'");
        assert!(!out.overridden);
    }

    // REMOVED: a command still carrying {{model}}/{{thinking}} is
    // refused outright — expanding them empty would silently launch a broken
    // command (`--model --thinking …` parses the next flag as the value).
    #[test]
    fn build_worker_cmd_refuses_removed_placeholders() {
        let tmpl = "pi --model {{model}} --thinking {{thinking}} @{{prompt_file}}";
        let err = build_worker_cmd(tmpl, None, "/p/x.md").unwrap_err();
        assert!(err.contains("{{model}}"));
        assert!(err.contains("looop init"));

        let err = build_worker_cmd(
            "claude @{{prompt_file}}",
            Some("pi --thinking {{thinking}} @{{prompt_file}}"),
            "/p/x.md",
        )
        .unwrap_err();
        assert!(err.contains("{{thinking}}"));
        // Override case: hint points at the --command string, not `looop init`
        // (there's no config template to re-run init for).
        assert!(!err.contains("looop init"));
        assert!(err.contains("--command"));
    }

    // REGRESSION (#1): placeholder checks run against the ORIGINAL command,
    // BEFORE {{prompt_file}} substitution — a prompt path containing the
    // literal `{{model}}` (e.g. via a crafted session id) is substituted
    // verbatim and can never trip the removed-placeholder refusal.
    #[test]
    fn build_worker_cmd_prompt_path_with_literal_placeholder() {
        let out = build_worker_cmd("claude @{{prompt_file}}", None, "/p/{{model}}.md").unwrap();
        assert_eq!(out.cmd, "claude @'/p/{{model}}.md'");
    }
}
