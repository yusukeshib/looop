//! The answer half of the ask/answer pair: reading an ask's answer record
//! (with corrupt-vs-missing distinguished) and durably resolving a pending
//! ask (`looop answer`).

use super::common::{warn_future_v, warn_once};
use crate::paths::Paths;
use crate::store::{FileStore, Key, StateStore};
use crate::util;
use anyhow::{Context, Result, bail};
use std::process::ExitCode;

/// The state of an ask's answer record. `Missing` and `Corrupt` are distinct
/// on purpose: collapsing a corrupt/truncated `answers/<id>.json` into "no
/// answer" made the blocking worker wait FOREVER while the ask re-listed as
/// unanswered — yet re-answering was refused without `--force` and the
/// corruption surfaced nowhere.
pub(super) enum AnswerState {
    /// No `answers/<id>.json` — still waiting on the human.
    Missing,
    /// The record exists but is unreadable (truncated / bad JSON / no `answer`
    /// field). A stderr warning naming the file has already been printed.
    Corrupt,
    /// The parsed answer text.
    Ready(String),
}

impl AnswerState {
    /// The answer text when ready (test convenience).
    #[cfg(test)]
    pub(super) fn text(&self) -> Option<&str> {
        match self {
            AnswerState::Ready(t) => Some(t),
            _ => None,
        }
    }
}

/// Read the answer record for an ask id. A parse failure warns on STDERR
/// (naming the file, mirroring the unparseable-ask warning in
/// [`pending`](super::pending)) — stdout stays machine-clean for `--json`
/// consumers.
pub(super) fn read_answer(store: &impl StateStore, ask_id: &str) -> AnswerState {
    let Some(raw) = store.read(&Key::Answer(ask_id.to_string())) else {
        return AnswerState::Missing;
    };
    // Same forward-compat signal as asks/tells — answers are records too, and
    // a v2 answer read as v1 would otherwise be misinterpreted silently.
    warn_future_v("answers", ask_id, &raw);
    // Deduplicated via warn_once: read_answer runs every beat (pending) and
    // every poll second (ask), so a durable corruption would otherwise warn
    // hundreds of times for one broken file.
    let v: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            warn_once(
                format!("answer-unparseable:{ask_id}"),
                &format!(
                    "answers/{ask_id}.json is unparseable ({e}) — re-answer with `looop answer {ask_id} --force …`"
                ),
            );
            return AnswerState::Corrupt;
        }
    };
    match v.get("answer").and_then(|x| x.as_str()) {
        Some(text) => AnswerState::Ready(text.to_owned()),
        None => {
            warn_once(
                format!("answer-no-field:{ask_id}"),
                &format!(
                    "answers/{ask_id}.json has no string `answer` field — re-answer with `looop answer {ask_id} --force …`"
                ),
            );
            AnswerState::Corrupt
        }
    }
}

/// `looop answer <ask_id> <text…>`
///
/// Root-agent callback: resolve a pending ask. Writes `answers/<ask_id>.json`,
/// which unblocks the worker's `ask`. Refuses an unknown ask id.
pub fn cmd_answer(paths: &Paths, args: &crate::cli::AnswerArgs) -> Result<ExitCode> {
    use crate::contract::Contract;
    // Body resolution mirrors `goal/sensor/playbook write`: inline words win,
    // otherwise (no body, or a lone `-`) read the whole answer from stdin so a
    // multi-line design decision can be piped or passed via heredoc without the
    // `-` (or the heredoc terminator) leaking into the saved answer. clap pulls
    // `--force` out from anywhere, so it never leaks into the body. Stdin
    // resolution is a CLI-transport concern, so it stays in the presenter.
    let rest = &args.body;
    let text = if rest.is_empty() || (rest.len() == 1 && rest[0] == "-") {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("reading answer from stdin")?;
        buf.trim_end().to_string()
    } else {
        rest.join(" ")
    };
    crate::contract::LocalContract::new(paths).answer(&args.ask_id, &text, args.force)?;
    // CLI-only feedback: the core is transport-agnostic (no stdout), so the
    // confirmation line is emitted HERE, not in `answer()`. A TUI client (which
    // calls the core directly) must not have a stray println corrupt its screen.
    util::event(
        util::Level::Ok,
        "answer",
        &format!("{}: {text}", args.ask_id),
        &[("ask_id", serde_json::json!(args.ask_id))],
    );
    Ok(ExitCode::SUCCESS)
}

/// CONTRACT core for `answer`: durably resolve a pending ask. Refuses an
/// unknown ask id, and (without `force`) an already-answered one. Transport-
/// agnostic: no stdin, no stdout.
pub(crate) fn answer(paths: &Paths, ask_id: &str, text: &str, force: bool) -> Result<()> {
    util::safe_segment("ask id", ask_id)?;
    if text.trim().is_empty() {
        bail!("answer: empty text");
    }
    let store = FileStore::new(paths);
    // exists_checked, NOT exists(): a transient stat error (EACCES, EIO, …)
    // must surface as an ERROR, not masquerade as "no pending ask" — the
    // human would conclude the ask was already resolved and walk away while
    // the worker keeps waiting on an answer that never comes.
    if !store
        .exists_checked(&Key::Ask(ask_id.to_string()))
        .with_context(|| format!("answer: checking asks/{ask_id}.json"))?
    {
        bail!("answer: no pending ask {ask_id:?}");
    }
    // Answers are durable: refuse to clobber one already given unless `--force`.
    // A worker that has already read its answer has moved on, so a stray re-answer
    // is almost always a misfire — fail loudly instead of silently overwriting.
    // The first answer goes through create_exclusive (an atomic test-and-set),
    // NOT exists()-then-write: two humans answering simultaneously must not
    // BOTH succeed with one answer silently lost — exactly one create wins and
    // the loser gets the already-answered error. `--force` keeps the atomic
    // overwrite path (deliberate replacement, e.g. of a corrupt record).
    let body = serde_json::to_string_pretty(&serde_json::json!({
        "answer": text,
        "ts": util::now_unix(),
    }))?;
    if force {
        store.write_atomic(&Key::Answer(ask_id.to_string()), &body)?;
    } else if !store.create_exclusive(&Key::Answer(ask_id.to_string()), &body)? {
        bail!("answer: {ask_id:?} is already answered (pass --force to overwrite)");
    }
    // The exists check above and the create are NOT one atomic step: the ask
    // can be removed in between (for example by a manual prune). The answer
    // written for a vanished
    // ask would be an ORPHAN — never read, never archived, and permanently
    // reserving its id in next_seq_id's scan. Re-check and undo OUR OWN write:
    // remove_if_eq keys on the exact body we wrote, so a concurrent --force
    // re-answer landed meanwhile is never deleted.
    match store.exists_checked(&Key::Ask(ask_id.to_string())) {
        Ok(true) => {}
        Ok(false) => {
            let _ = store.remove_if_eq(&Key::Answer(ask_id.to_string()), &body);
            bail!(
                "answer: ask {ask_id:?} vanished while answering (consumed or removed) — \
                 the answer was not recorded"
            );
        }
        // A transient stat error is NOT evidence the ask vanished: the answer
        // was durably written and the ask almost certainly still exists —
        // deleting the answer (or reporting failure for a write that
        // succeeded) would be the wrong side to fail on. Keep it.
        Err(_) => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::pending;
    use super::super::test_util::ans;
    use super::*;
    use std::fs;

    #[test]
    fn answer_refuses_unknown_ask() {
        let p = Paths::temp();
        fs::create_dir_all(p.asks_dir()).unwrap();
        assert!(cmd_answer(&p, &ans("nope-9", "x", false)).is_err());
    }

    #[test]
    fn answer_refuses_to_overwrite_without_force_but_allows_with_force() {
        let p = Paths::temp();
        fs::create_dir_all(p.asks_dir()).unwrap();
        fs::create_dir_all(p.answers_dir()).unwrap();
        fs::write(
            p.asks_dir().join("w-1.json"),
            serde_json::json!({"id":"w-1","worker":"w","prompt":"ok?","ts":1}).to_string(),
        )
        .unwrap();
        cmd_answer(&p, &ans("w-1", "first", false)).unwrap();
        // A bare re-answer is refused (a stray re-answer is almost always a misfire).
        assert!(cmd_answer(&p, &ans("w-1", "second", false)).is_err());
        assert_eq!(
            read_answer(&FileStore::new(&p), "w-1").text(),
            Some("first")
        );
        // `--force` lets the human deliberately recover from a bad answer.
        cmd_answer(&p, &ans("w-1", "second", true)).unwrap();
        assert_eq!(
            read_answer(&FileStore::new(&p), "w-1").text(),
            Some("second")
        );
    }

    #[test]
    fn concurrent_first_answers_let_exactly_one_win() {
        // Two humans answering the same ask simultaneously: the first answer
        // is an atomic test-and-set (create_exclusive), so exactly one wins
        // and the loser gets the already-answered error — no answer is ever
        // silently lost to an exists()-then-write race.
        let p = Paths::temp();
        fs::create_dir_all(p.asks_dir()).unwrap();
        fs::write(
            p.asks_dir().join("w-1.json"),
            serde_json::json!({"id":"w-1","worker":"w","prompt":"ok?","ts":1}).to_string(),
        )
        .unwrap();
        let wins: usize = std::thread::scope(|s| {
            let handles: Vec<_> = (0..8)
                .map(|i| {
                    let p = &p;
                    s.spawn(move || answer(p, "w-1", &format!("answer-{i}"), false).is_ok())
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap())
                .filter(|&won| won)
                .count()
        });
        assert_eq!(wins, 1, "exactly one first answer must win");
        // The surviving record is the winner's, intact.
        assert!(
            read_answer(&FileStore::new(&p), "w-1")
                .text()
                .is_some_and(|t| t.starts_with("answer-")),
            "the winning answer is readable and complete"
        );
    }

    #[test]
    fn corrupt_answer_keeps_ask_pending_until_force_repaired() {
        // A truncated/corrupt answers/<id>.json is NOT "no answer": the ask
        // stays visible until the human repairs it with --force.
        let p = Paths::temp();
        fs::create_dir_all(p.asks_dir()).unwrap();
        fs::create_dir_all(p.answers_dir()).unwrap();
        fs::write(
            p.asks_dir().join("w-1.json"),
            serde_json::json!({"id":"w-1","worker":"w","prompt":"ok?","ts":1}).to_string(),
        )
        .unwrap();
        fs::write(p.answers_dir().join("w-1.json"), b"{truncat").unwrap();
        assert!(matches!(
            read_answer(&FileStore::new(&p), "w-1"),
            AnswerState::Corrupt
        ));
        assert_eq!(pending(&p).len(), 1, "corrupt answer keeps the ask listed");
        // --force re-answer repairs the record and resolves the ask.
        answer(&p, "w-1", "repaired", true).unwrap();
        assert!(pending(&p).is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn answer_surfaces_a_stat_failure_instead_of_no_pending_ask() {
        // Regression: answer() used exists(), which squashed a transient stat
        // error (EACCES, EIO, …) to "absent" — the human was told "no pending
        // ask" and walked away while the worker kept waiting. The failure
        // must surface as an error naming the real cause.
        let p = Paths::temp();
        fs::create_dir_all(p.asks_dir()).unwrap();
        fs::write(
            p.asks_dir().join("w-1.json"),
            serde_json::json!({"id":"w-1","worker":"w","prompt":"ok?","ts":1}).to_string(),
        )
        .unwrap();
        let (enforced, _restore) = crate::store::deny_access(p.asks_dir());
        if !enforced {
            return; // running as root — permissions can't simulate EACCES
        }
        let err = answer(&p, "w-1", "x", false).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            !msg.contains("no pending ask"),
            "a stat failure must not be reported as a missing ask: {msg}"
        );
        assert!(
            msg.contains("checking asks/w-1.json"),
            "the error names the real failure: {msg}"
        );
    }
}
