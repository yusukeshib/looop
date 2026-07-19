//! ask/answer mailbox — the worker ↔ human question channel.
//!
//! A worker that needs a decision only a HUMAN can make calls `looop ask <id>
//! --prompt "…"`, which writes a durable question file under `asks/` and then
//! BLOCKS until a matching `answers/` file appears, printing the answer to stdout.
//! The human answers with `looop answer <ask_id> "…"` — directly, or through any
//! client (an agent concierge, a notify script, …) that surfaces pending asks and
//! relays the reply. looop's own decide loop sees pending asks but does NOT answer
//! them: they
//! are the human's call.
//!
//! Why files (not stdin / a socket): durability + level-triggering (RULE 2).
//! The mailbox survives a pulse crash, needs no live process to relay, and works
//! for a head-less worker that can't sit at a tmux prompt.

mod answer;
mod ask;
mod common;
mod signal;
mod tell;

pub(crate) use answer::answer;
pub use answer::cmd_answer;
pub use ask::{
    Ask, answered_detached, archive_pair, cmd_ask, cmd_asks, pending, resume_context,
    unarchive_pair,
};
pub(crate) use ask::{ask, ask_detached};
// Shared "--session/--worker arg → $LOOOP_SESSION_ID fallback" (one rule,
// previously three copies: gate.rs and the ask/told self-callbacks).
pub(crate) use common::session_or_env;
pub use signal::sys_asks;
// `Tell` and `drain_tells` have no external consumer today, but they were part
// of the module's public surface before the split — keep the paths working.
// They sit on their OWN allowed `pub use` line so the suppression covers
// exactly the genuinely-unnamed items — a blanket allow over the whole list
// would hide a future dead-re-export warning for the used ones too.
pub(crate) use tell::discard_tells;
#[allow(unused_imports)]
pub use tell::{Tell, drain_tells};
pub use tell::{cmd_tell, cmd_told, pending_tells};

#[cfg(test)]
mod test_util {
    use crate::paths::Paths;
    use std::fs;

    /// A temp profile with the FIRST-RUN SEED suppressed: contract verbs
    /// (`cmd_answer` → `LocalContract`) run `seed::ensure_dirs`, which on a
    /// PLAYBOOK-less data dir plants the `setup-1` starter ask — noise for
    /// tests asserting exact pending/resume sets. A pre-existing PLAYBOOK
    /// marks the profile as already seeded.
    pub(crate) fn temp_seeded() -> Paths {
        let p = Paths::temp();
        fs::write(p.playbook(), "# test playbook\n").unwrap();
        p
    }

    /// Build an `AnswerArgs` the way clap would after parsing
    /// `answer <id> <text…> [--force]`.
    pub(crate) fn ans(id: &str, text: &str, force: bool) -> crate::cli::AnswerArgs {
        crate::cli::AnswerArgs {
            ask_id: id.into(),
            body: vec![text.into()],
            force,
        }
    }
}
