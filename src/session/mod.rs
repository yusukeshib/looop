//! Start a worker session — the hands. `looop start-session <id> "<prompt>"`.
//! The pulse only LAUNCHES the agent (in the data dir) under babysit, detached;
//! it does NOT provision a workspace. Every worker gets the same contract
//! prepended so the pulse can't forget it (workers never notify — they flag and
//! wait; they sandbox their own code; the data dir's policy files are read-only).
//!
//! The module is split along its four concerns, behind this facade (the
//! `session::` paths consumers import are unchanged):
//!
//! * [`fleet`] — the in-process adapter over the `babysit` library
//!   (list/kill/spawn/reap/prune + the `Fleet` seam the launch gating is
//!   tested through).
//! * [`launch`] — worker-launch POLICY: the injected contract, command
//!   templating, and `cmd_start_session`'s validate-then-commit pipeline.
//! * [`present`] — CLI presentation: the fleet table, `--watch` repaint
//!   loop, and the kill/screenshot verbs.
//! * [`plumbing`] — low-level fd/FFI plumbing (`suppress_stdout`,
//!   `dup_cloexec`) that the fleet adapter leans on.

mod fleet;
mod launch;
mod plumbing;
mod present;

// `Session`, `kill` and `StartOutcome` are re-exported even though no
// consumer currently NAMES them (they flow through inferred types): they were
// part of the flat module's public surface, and dropping them from the facade
// would silently narrow the `session::` API this split promises to preserve.
// They sit on their OWN allowed `pub use` lines so the suppression covers
// exactly the genuinely-unnamed items — a blanket allow over the whole list
// would hide a future dead-re-export warning for the used ones too.
pub use fleet::{
    PULSE_SESSION, await_alive, is_alive, kill_quiet, list_workers, output_idle_secs, prune_aged,
    reap, run_detached_worker, spawn_detached, status_exists, try_is_alive, try_list,
};
// `list` (the lenient enumerate): sensor.rs's pulse_health uses it (the only
// enumerate that INCLUDES the pulse — needed to render the pulse row in
// `looop worker list`); the launch tests also name it to pin the
// lenient-vs-strict split against try_list.
pub use fleet::list;
#[allow(unused_imports)]
pub use fleet::{Session, kill};
#[allow(unused_imports)]
pub use launch::StartOutcome;
pub use launch::cmd_start_session;
pub(crate) use plumbing::suppress_stdout;
pub(crate) use present::{WORKER_TABLE_HEADERS, worker_table_fleet, worker_table_row};
pub use present::{cmd_kill, cmd_screenshot, cmd_worker_list};

/// GENERATION-BOUNDARY hygiene, shared by every path that retires a worker
/// id's per-generation state: worker ids ARE goal ids and get reused, so
/// state addressed to a dead generation must never leak into its successor —
/// a stale verify record would judge the new worker against the old
/// post-condition (or fail its start for the wrong reason), and an
/// undelivered tell would steer a worker with a different brief.
///
/// Callers: [`fleet::reap`] and [`fleet::prune_aged`] (the corpse's session
/// record is removed, so a verify verdict would be unattributable anyway) and
/// the pre-spawn point of `cmd_start_session` (the id is about to be reused).
/// `cmd_kill` is deliberately NOT one of them: a killed worker's session
/// record survives, so its verify obligation stays attributable and must
/// still be judged by the next reconcile — kill discards only the tells.
pub(crate) fn on_generation_end(paths: &crate::paths::Paths, id: &str) {
    crate::verify::clear(paths, id);
    crate::mailbox::discard_tells(paths, id);
}
