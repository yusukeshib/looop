//! The CONTRACT — the steering surface that drives looop's world, abstracted
//! behind a trait so the transport (today: the `looop …` CLI) is decoupled
//! from the backend that fulfils it.
//!
//! For a long time looop had two abstractions at very different layers:
//!
//!   * [`crate::store::StateStore`] abstracts WHERE durable state lives (a
//!     filesystem today, a DB/remote KV tomorrow) — the STORE layer.
//!   * …nothing abstracted the contract VERBS themselves. `dispatch` matched a
//!     parsed [`crate::cli::Verb`] straight onto concrete `&Paths`-bound
//!     functions that also printed their own output, so "drive the same contract
//!     against a different backend" (e.g. talk to a remote looop over HTTP) had
//!     no seam to slot into.
//!
//! [`Contract`] is that missing seam. Each method is a contract verb expressed
//! over TYPED data (no `&Paths`, no stdout): a method either returns the data a
//! caller asked for ([`Contract::state`], [`Contract::asks`]) or the executor's
//! one-line summary of a mutation it performed. PRESENTATION (the human/JSON
//! rendering and the process exit code) lives in the CLI layer (`cmd_*`), which
//! is just one transport over a `Contract`. A future HTTP server would be a
//! second transport over the same trait; an `HttpContract` would be a second
//! impl a client drives instead of [`LocalContract`].
//!
//! Scope: this trait covers the STATE / STEERING contract — the verbs a remote
//! backend can meaningfully serve (read state, relay/answer asks, write
//! goals/sensors/PLAYBOOK, run a reversible command, spawn a worker, take a
//! lease). The host-local session-I/O verbs (`kill` / `screenshot`) are
//! deliberately NOT here: they manipulate a live PTY on THIS
//! host (babysit renders a terminal grid straight to stdout), so they are a
//! host capability, not a transport-agnostic contract operation.

use crate::executor::{Action, run_action};
use crate::mailbox::Ask;
use crate::paths::Paths;
use crate::{gate, mailbox, observe};
use anyhow::Result;
use serde_json::Value;

/// The outcome of an atomic [`Contract::claim`] — transport-agnostic, so a
/// presenter (CLI / HTTP) maps it to its own exit code / status.
#[derive(Debug, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// We created the lease this call.
    Won,
    /// We already held it (idempotent acquire).
    AlreadyOwned,
    /// A DIFFERENT live session holds it — caller should signal failure.
    HeldByLive(String),
}

/// The steering contract, abstracted over its backend. Methods return typed data
/// or an executor summary; they never print and never expose a path, so any
/// transport (CLI today, HTTP tomorrow) can drive any impl.
pub trait Contract {
    /// Full world snapshot (goals, sensors, fleet, asks) as a JSON value.
    fn state(&self) -> Result<Value>;
    /// Block until the world changes (per `filter`), then return the fresh state
    /// with a `"changed"` array describing what moved.
    fn wait(&self, filter: observe::WaitFilter) -> Result<Value>;
    /// Just the pending (unanswered) asks.
    fn asks(&self) -> Result<Vec<Ask>>;
    /// Resolve a pending ask durably. `force` overwrites an existing answer.
    fn answer(&self, ask_id: &str, text: &str, force: bool) -> Result<()>;
    /// Worker self-callback: raise a blocking ask and return the human's answer.
    fn ask(
        &self,
        worker: &str,
        prompt: &str,
        reference: &str,
        options: &[String],
    ) -> Result<String>;
    /// Worker self-callback: raise a DETACHED ask (no blocking) and return the
    /// ask id. The worker checkpoints and exits; the answer reaches a fresh
    /// worker via `worker_start(…, resume: Some(ask_id))`.
    fn ask_detached(
        &self,
        worker: &str,
        prompt: &str,
        reference: &str,
        options: &[String],
    ) -> Result<String>;
    /// Create or replace a goal; returns the executor's summary line.
    fn goal_write(&self, id: &str, body: &str, journal: Option<&str>) -> Result<String>;
    /// Archive a goal; returns the executor's summary line.
    fn goal_archive(&self, id: &str, journal: Option<&str>) -> Result<String>;
    /// Create or replace a sensor script; returns the executor's summary line.
    fn sensor_write(&self, name: &str, script: &str, journal: Option<&str>) -> Result<String>;
    /// Replace the PLAYBOOK; returns the executor's summary line.
    fn playbook_write(&self, body: &str, journal: Option<&str>) -> Result<String>;
    /// Run one ad-hoc, REVERSIBLE shell command; returns the executor's summary.
    fn run(&self, cmd: &str, reason: &str, journal: Option<&str>) -> Result<String>;
    /// Spawn a worker session; returns the executor's summary line. `command`
    /// is an optional per-worker launch-command override, replacing the
    /// `worker_command` template wholesale (must carry `{{prompt_file}}`).
    /// `verify` is an optional post-condition shell command the pulse runs
    /// once after the worker dies (exit 0 = work verified done — see
    /// `verify.rs`). `resume` names an answered DETACHED ask whose question,
    /// answer, and checkpoint reference are injected into the brief (the pair
    /// is archived once the worker launches).
    fn worker_start(
        &self,
        id: &str,
        prompt: &str,
        command: Option<&str>,
        verify: Option<&str>,
        resume: Option<&str>,
        journal: Option<&str>,
    ) -> Result<String>;
    /// Atomically acquire the named lease.
    fn claim(&self, name: &str, session: Option<&str>) -> Result<ClaimOutcome>;
    /// Release the named lease. `Ok(false)` ⇒ a different live session holds it.
    fn unclaim(&self, name: &str, session: Option<&str>) -> Result<bool>;
}

/// The host-backed [`Contract`]: fulfils every verb against the local
/// filesystem and session fleet (via the existing module cores). Borrows the
/// resolved [`Paths`] so it stays a thin binding from logical verb to local effect.
pub struct LocalContract<'a> {
    paths: &'a Paths,
}

impl<'a> LocalContract<'a> {
    pub fn new(paths: &'a Paths) -> Self {
        LocalContract { paths }
    }
}

impl Contract for LocalContract<'_> {
    fn state(&self) -> Result<Value> {
        let _ = crate::seed::ensure_dirs(self.paths);
        Ok(observe::state(self.paths))
    }

    fn wait(&self, filter: observe::WaitFilter) -> Result<Value> {
        let _ = crate::seed::ensure_dirs(self.paths);
        let changed = observe::wait_for_change(self.paths, filter);
        let mut s = observe::state(self.paths);
        if let Some(obj) = s.as_object_mut() {
            obj.insert("changed".to_string(), serde_json::json!(changed));
        }
        Ok(s)
    }

    fn asks(&self) -> Result<Vec<Ask>> {
        let _ = crate::seed::ensure_dirs(self.paths);
        Ok(mailbox::pending(self.paths))
    }

    fn answer(&self, ask_id: &str, text: &str, force: bool) -> Result<()> {
        // Same seeding policy as EVERY verb that touches the data layout
        // directly (state/wait/asks/ask/claim/…): ensure the layout exists
        // (cheap, idempotent) so a fresh checkout can't make one contract verb
        // behave differently from its siblings. The steering writes
        // (goal_write … worker_start) get the same guarantee inside
        // `run_action`.
        let _ = crate::seed::ensure_dirs(self.paths);
        mailbox::answer(self.paths, ask_id, text, force)
    }

    fn ask(
        &self,
        worker: &str,
        prompt: &str,
        reference: &str,
        options: &[String],
    ) -> Result<String> {
        let _ = crate::seed::ensure_dirs(self.paths);
        mailbox::ask(self.paths, worker, prompt, reference, options)
    }

    fn ask_detached(
        &self,
        worker: &str,
        prompt: &str,
        reference: &str,
        options: &[String],
    ) -> Result<String> {
        let _ = crate::seed::ensure_dirs(self.paths);
        mailbox::ask_detached(self.paths, worker, prompt, reference, options)
    }

    fn goal_write(&self, id: &str, body: &str, journal: Option<&str>) -> Result<String> {
        run_action(
            self.paths,
            &Action::WriteGoal {
                id: id.to_string(),
                body: body.to_string(),
            },
            journal,
        )
    }

    fn goal_archive(&self, id: &str, journal: Option<&str>) -> Result<String> {
        run_action(
            self.paths,
            &Action::ArchiveGoal { id: id.to_string() },
            journal,
        )
    }

    fn sensor_write(&self, name: &str, script: &str, journal: Option<&str>) -> Result<String> {
        run_action(
            self.paths,
            &Action::WriteSensor {
                name: name.to_string(),
                script: script.to_string(),
            },
            journal,
        )
    }

    fn playbook_write(&self, body: &str, journal: Option<&str>) -> Result<String> {
        run_action(
            self.paths,
            &Action::WritePlaybook {
                body: body.to_string(),
            },
            journal,
        )
    }

    fn run(&self, cmd: &str, reason: &str, journal: Option<&str>) -> Result<String> {
        run_action(
            self.paths,
            &Action::RunShell {
                cmd: cmd.to_string(),
                reason: reason.to_string(),
            },
            journal,
        )
    }

    fn worker_start(
        &self,
        id: &str,
        prompt: &str,
        command: Option<&str>,
        verify: Option<&str>,
        resume: Option<&str>,
        journal: Option<&str>,
    ) -> Result<String> {
        run_action(
            self.paths,
            &Action::StartWorker {
                id: id.to_string(),
                prompt: prompt.to_string(),
                command: command.map(str::to_owned),
                verify: verify.map(str::to_owned),
                resume: resume.map(str::to_owned),
            },
            journal,
        )
    }

    fn claim(&self, name: &str, session: Option<&str>) -> Result<ClaimOutcome> {
        let _ = crate::seed::ensure_dirs(self.paths);
        gate::claim(self.paths, name, session)
    }

    fn unclaim(&self, name: &str, session: Option<&str>) -> Result<bool> {
        let _ = crate::seed::ensure_dirs(self.paths);
        gate::unclaim(self.paths, name, session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fake a LIVE babysit session by writing the meta/status pair the library
    /// reads, with OUR OWN pid as the owner — the test process is alive by
    /// definition, so `session::is_alive` sees a genuinely live holder without
    /// spawning anything.
    fn fake_live_session(paths: &Paths, id: &str) {
        let dir = paths.data_dir.join("sessions").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("meta.json"),
            format!(
                r#"{{"id":"{id}","cmd":["x"],"babysit_pid":{},"started_at":"2026-01-01T00:00:00Z"}}"#,
                std::process::id()
            ),
        )
        .unwrap();
        std::fs::write(
            dir.join("status.json"),
            r#"{"state":"running","child_pid":null,"exit_code":null,"last_change":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();
    }

    /// Drive the claim verb's THREE outcomes through the [`Contract`] trait
    /// object (not the gate module directly), so the transport-agnostic seam
    /// itself is what's under test — a future HTTP impl must reproduce
    /// exactly this mapping.
    #[test]
    fn claim_outcomes_three_way_through_the_contract_trait() {
        let p = Paths::temp();
        let local = LocalContract::new(&p);
        let c: &dyn Contract = &local;

        // 1) A fresh name is WON.
        assert_eq!(c.claim("repo-x", Some("w1")).unwrap(), ClaimOutcome::Won);
        // …and the same session re-claiming is the idempotent ALREADY-OWNED,
        // never an error (a worker may re-announce ownership mid-task).
        assert_eq!(
            c.claim("repo-x", Some("w1")).unwrap(),
            ClaimOutcome::AlreadyOwned
        );

        // 2) A DEAD holder (no live session backs "w1") is stale: a different
        // session reclaims and WINS rather than being locked out forever.
        assert_eq!(c.claim("repo-x", Some("w2")).unwrap(), ClaimOutcome::Won);

        // 3) A LIVE holder blocks everyone else: HELD-BY-LIVE names the
        // holder so the caller can report who owns the lease.
        fake_live_session(&p, "live-1");
        assert_eq!(
            c.claim("repo-y", Some("live-1")).unwrap(),
            ClaimOutcome::Won
        );
        match c.claim("repo-y", Some("w9")).unwrap() {
            ClaimOutcome::HeldByLive(holder) => assert_eq!(
                holder, "live-1",
                "the live holder is named so the caller can surface it"
            ),
            other => panic!("a live holder must block the claim, got {other:?}"),
        }

        // unclaim mirrors the same liveness rule: a non-holder is refused
        // (Ok(false)) while the live holder releases cleanly (Ok(true)).
        assert!(!c.unclaim("repo-y", Some("w9")).unwrap());
        assert!(c.unclaim("repo-y", Some("live-1")).unwrap());
    }
}
