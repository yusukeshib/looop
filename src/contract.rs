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
//! lease). The host-local session-I/O verbs (`kill` / `send` /
//! `screenshot`) are deliberately NOT here: they manipulate a live PTY on THIS
//! host (babysit renders a terminal grid straight to stdout), so they are a
//! host capability, not a transport-agnostic contract operation.

use crate::executor::{Action, run_action};
use crate::mailbox::Ask;
use crate::paths::Paths;
use crate::{gate, mailbox, tick};
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
    fn wait(&self, filter: tick::WaitFilter) -> Result<Value>;
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
    /// Spawn a worker session; returns the executor's summary line. `model`/
    /// `thinking` are optional per-worker overrides for the worker command
    /// template's `{{model}}`/`{{thinking}}` placeholders.
    fn worker_start(
        &self,
        id: &str,
        prompt: &str,
        model: Option<&str>,
        thinking: Option<&str>,
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
        Ok(tick::state(self.paths))
    }

    fn wait(&self, filter: tick::WaitFilter) -> Result<Value> {
        let _ = crate::seed::ensure_dirs(self.paths);
        let changed = tick::wait_for_change(self.paths, filter);
        let mut s = tick::state(self.paths);
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
        mailbox::answer(self.paths, ask_id, text, force)
    }

    fn ask(
        &self,
        worker: &str,
        prompt: &str,
        reference: &str,
        options: &[String],
    ) -> Result<String> {
        mailbox::ask(self.paths, worker, prompt, reference, options)
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
        model: Option<&str>,
        thinking: Option<&str>,
        journal: Option<&str>,
    ) -> Result<String> {
        run_action(
            self.paths,
            &Action::StartWorker {
                id: id.to_string(),
                prompt: prompt.to_string(),
                model: model.map(str::to_owned),
                thinking: thinking.map(str::to_owned),
            },
            journal,
        )
    }

    fn claim(&self, name: &str, session: Option<&str>) -> Result<ClaimOutcome> {
        gate::claim(self.paths, name, session)
    }

    fn unclaim(&self, name: &str, session: Option<&str>) -> Result<bool> {
        gate::unclaim(self.paths, name, session)
    }
}
