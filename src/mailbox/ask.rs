//! The ask half of the mailbox: raising questions (blocking and detached),
//! listing pending asks, resuming answered detached asks, and the archive /
//! unarchive lifecycle of a consumed ask/answer pair.

use super::answer::{AnswerState, read_answer};
use super::common::{stamp_v1, warn_future_v, warn_once, write_new_record};
use super::tell::drain_tells;
use crate::paths::Paths;
use crate::store::{Collection, FileStore, Key, StateStore};
use crate::util;
use anyhow::{Context, Result, bail};
use std::process::ExitCode;
use std::time::Duration;

/// One pending question. Serialized to `asks/<id>.json` (with a `v: 1` schema
/// stamp injected at write time; absent `v` on read means v1).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Ask {
    /// Correlation id: `<worker>-<n>`. The answer lands at `answers/<id>.json`.
    pub id: String,
    /// The worker session that asked.
    pub worker: String,
    /// The question / what the worker is waiting on.
    pub prompt: String,
    /// Optional artifact a human/root should read before answering (e.g.
    /// `reports/triage.md`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reference: String,
    /// Optional discrete choices the answer should pick from.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    /// DETACHED ask: the worker checkpointed its state and EXITED instead of
    /// blocking on the answer. Its death is by design (not stranded); when the
    /// human answers, the decider re-dispatches a fresh worker with
    /// `start_worker.resume = <ask id>`, which injects the answer + checkpoint
    /// and archives the pair.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub detach: bool,
    /// Unix seconds the ask was raised.
    pub ts: u64,
}

/// All asks that have NO matching answer yet. Read-only; used by `state` and
/// the decide prompt (so looop sees what's blocked) and by any
/// client (so the human sees what's waiting on them).
pub fn pending(paths: &Paths) -> Vec<Ask> {
    let store = FileStore::new(paths);
    let mut out = Vec::new();
    for id in store.list(&Collection::Asks) {
        let Some(raw) = store.read(&Key::Ask(id.clone())) else {
            continue;
        };
        warn_future_v("asks", &id, &raw);
        match serde_json::from_str::<Ask>(&raw) {
            Ok(ask) => {
                // A CORRUPT answer keeps the ask listed (the human must see
                // it's unresolved) — read_answer has already warned on stderr
                // naming the broken file, so the fix (--force) is visible.
                if !matches!(read_answer(&store, &ask.id), AnswerState::Ready(_)) {
                    out.push(ask);
                }
            }
            // A record we cannot parse is a VISIBLE problem, not a silent drop
            // — a worker may be blocked on it forever. STDERR, not stdout:
            // pending() feeds machine output (`looop asks --json`,
            // `looop state --json`) and stdout must stay clean for it.
            // Once per record per process (pending runs every beat).
            Err(e) => {
                warn_once(
                    format!("ask-unparseable:{id}"),
                    &format!("asks/{id}.json is unparseable ({e}) — record ignored"),
                );
            }
        }
    }
    out.sort_by(|a, b| a.ts.cmp(&b.ts).then_with(|| a.id.cmp(&b.id)));
    out
}

/// DETACHED asks that HAVE an answer and have not been resumed yet, oldest
/// first, each paired with its answer text. These are the decider's cue to
/// re-dispatch: `start_worker` with `resume = <ask id>` injects the answer +
/// checkpoint into the fresh worker's brief and archives the pair (which
/// settles the `sys-asks` wake signal).
pub fn answered_detached(paths: &Paths) -> Vec<(Ask, String)> {
    let store = FileStore::new(paths);
    let mut out = Vec::new();
    for id in store.list(&Collection::Asks) {
        // Parse failures are surfaced by `pending()` (which scans the same
        // records every beat); here they are just skipped.
        if let Some(raw) = store.read(&Key::Ask(id.clone()))
            && let Ok(ask) = serde_json::from_str::<Ask>(&raw)
            && ask.detach
            && let AnswerState::Ready(answer) = read_answer(&store, &ask.id)
        {
            out.push((ask, answer));
        }
    }
    out.sort_by(|a, b| a.0.ts.cmp(&b.0.ts).then_with(|| a.0.id.cmp(&b.0.id)));
    out
}

/// The RESUME preamble for a fresh worker taking over an answered detached
/// ask: the original question, the human's answer, and the checkpoint
/// reference. Errors when the ask is unknown or not answered yet — a resume
/// against a pending ask is a decider mistake and must fail loudly.
pub fn resume_context(paths: &Paths, ask_id: &str) -> Result<String> {
    // The id becomes a path segment (asks/<id>.json) — same guard as answer().
    util::safe_segment("ask id", ask_id)?;
    let store = FileStore::new(paths);
    let raw = store
        .read(&Key::Ask(ask_id.to_string()))
        .with_context(|| format!("resume: no ask {ask_id:?}"))?;
    let ask: Ask = serde_json::from_str(&raw).with_context(|| format!("resume: ask {ask_id:?}"))?;
    // A BLOCKING (non-detached) ask still has its original worker polling for
    // the answer: resuming it would archive the pair out from under that
    // worker (which then bails "vanished") while a SECOND worker consumes the
    // answer. Only detached asks are resumable by design.
    if !ask.detach {
        bail!(
            "ask {ask_id} is a blocking ask — its worker is already waiting; \
             resume only detached asks"
        );
    }
    let answer = match read_answer(&store, ask_id) {
        AnswerState::Ready(a) => a,
        AnswerState::Missing => bail!("resume: ask {ask_id:?} has no answer yet"),
        // Resuming against a corrupt answer would inject garbage into a fresh
        // worker's brief — fail loudly (the file was already named on stderr).
        AnswerState::Corrupt => {
            bail!("resume: answers/{ask_id}.json is unreadable — re-answer with --force first")
        }
    };
    let reference = if ask.reference.is_empty() {
        "(none — look for reports/ left by the previous worker)".to_string()
    } else {
        ask.reference.clone()
    };
    Ok(format!(
        "# ⚡ RESUME (auto-injected)\n\
         A previous worker checkpointed its state and asked the human, then exited.\n\
         You are the fresh worker carrying that work forward.\n\
         - Question asked: {}\n\
         - Human's answer: {}\n\
         - Checkpoint / reference: {}\n\
         Read the checkpoint FIRST, obey the answer, and continue from where the\n\
         previous worker left off — do not redo completed steps.\n\n---\n\n",
        ask.prompt, answer, reference
    ))
}

/// Archive a consumed ask/answer pair (asks/archive/, answers/archive/) so the
/// `sys-asks` resume signal settles. Best-effort on the answer half (an ask
/// without an answer file archives just the ask). An id that is not a safe
/// path segment archives nothing — the id becomes a filename, so it gets the
/// same guard as every other mailbox verb.
///
/// Returns whether THIS caller CONSUMED the pair. The ask half's rename into
/// the archive is atomic, so it doubles as a CLAIM (the same claim-by-rename
/// idiom as [`drain_tells`](super::drain_tells)): when two starts race to
/// resume the same answered ask, exactly one rename succeeds — the loser sees
/// NotFound and must NOT dispatch, or the same resume would be delivered to
/// two workers. `false` also covers an unarchivable pair (unsafe id, I/O
/// error): fail closed — an extra refusal is retryable, a double dispatch is
/// not.
#[must_use]
pub fn archive_pair(paths: &Paths, ask_id: &str) -> bool {
    if util::safe_segment("ask id", ask_id).is_err() {
        return false;
    }
    let store = FileStore::new(paths);
    match store.archive(&Key::Ask(ask_id.to_string())) {
        Ok(()) => {}
        // NotFound = a concurrent consumer archived the ask first (or it never
        // existed): the claim is LOST. Quiet on purpose — losing a race is a
        // normal outcome, not a fault worth a warn event.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return false,
        // Any other failure is non-fatal but must be VISIBLE — a pair that
        // fails to archive keeps the sys-asks resume signal hot (possible
        // re-dispatch) — and the caller must not dispatch on an unclaimed pair.
        Err(e) => {
            util::event(
                util::Level::Warn,
                "ask.archive_failed",
                &format!("could not archive asks/{ask_id}.json: {e}"),
                &[("ask_id", serde_json::json!(ask_id))],
            );
            return false;
        }
    }
    match store.archive(&Key::Answer(ask_id.to_string())) {
        Ok(()) => {}
        // Best-effort on the answer half: an ask without an answer file
        // archives just the ask (a detached ask killed before answering).
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => util::event(
            util::Level::Warn,
            "ask.archive_failed",
            &format!("could not archive answers/{ask_id}.json: {e}"),
            &[("ask_id", serde_json::json!(ask_id))],
        ),
    }
    // The ask half — the resume signal itself — is claimed and archived; a
    // straggling answer file is inert (pending() keys on asks/).
    true
}

/// Best-effort inverse of [`archive_pair`]: move the MOST RECENTLY archived
/// `<id>` records back into the live dirs. Used by the resume path when the
/// worker spawn fails AFTER the pair was archived — restoring the pair restores
/// the `sys-asks` resume signal, so the answer is not lost. The guard is
/// PAIRWISE: if a NEW live ask reuses the id, NEITHER half is restored —
/// restoring just the answer would attach a stale answer to a different
/// question. The ANSWER half is restored FIRST; if that restore fails, the ask
/// restore is skipped too (an answer without an ask is inert; an ask without
/// its answer re-relays as pending — worse).
pub fn unarchive_pair(paths: &Paths, ask_id: &str) {
    if util::safe_segment("ask id", ask_id).is_err() {
        return;
    }
    // Hold BOTH per-directory writer locks (fixed order: asks then answers —
    // no other path takes both, so no deadlock) for the whole check+restore:
    // exists()-then-rename without the lock was a TOCTOU — a concurrent
    // create_exclusive of a REUSED id between the check and the rename would
    // attach the old answer to a brand-new ask. create_exclusive takes the
    // same locks, so under them the guard below is race-free. Best-effort
    // (like the rest of this path): if a lock cannot be taken, restore nothing.
    let Ok(_asks_lock) = crate::store::DirLock::acquire(&paths.asks_dir()) else {
        return;
    };
    let Ok(_answers_lock) = crate::store::DirLock::acquire(&paths.answers_dir()) else {
        return;
    };
    // Pairwise guard: a live ask with this id means the id was REUSED by a new
    // ask — restore nothing (never attach the old answer to the new question).
    if paths.asks_dir().join(format!("{ask_id}.json")).exists() {
        return;
    }
    // Answer first: if it cannot be restored, skip the ask half as well.
    if !restore_newest_archived(&paths.answers_dir(), ask_id) {
        return;
    }
    restore_newest_archived(&paths.asks_dir(), ask_id);
}

/// Move the MOST RECENTLY archived `<ask_id>` record in `dir/archive/` back to
/// `dir/<ask_id>.json`. Returns true when the live record exists afterwards
/// (already present, or restored); false when there was nothing to restore or
/// the rename failed. CALLER HOLDS `dir`'s writer lock ([`unarchive_pair`]):
/// the exists-check doubles as the no-clobber guard — under the lock no
/// create_exclusive/write_atomic can land a destination between the check and
/// the rename, so an existing live record is never silently overwritten.
fn restore_newest_archived(dir: &std::path::Path, ask_id: &str) -> bool {
    let live = dir.join(format!("{ask_id}.json"));
    if live.exists() {
        return true;
    }
    let archive = dir.join("archive");
    // The archive suffixes on collision (`<id>.json`, then `<id>-1.json`, …),
    // so the HIGHEST suffix is the newest record. Scan the whole directory and
    // take the max instead of probing suffixes upward from the bare name: a
    // GAP in the sequence (a manually pruned generation) would stop an upward
    // probe early and restore a STALE record — or nothing at all when the bare
    // file itself is the gap.
    let suffix_of = |name: &str| -> Option<u64> {
        let stem = name.strip_suffix(".json")?;
        if stem == ask_id {
            return Some(0); // the bare, first-archived (oldest) record
        }
        stem.strip_prefix(&format!("{ask_id}-"))?.parse().ok()
    };
    let newest = std::fs::read_dir(&archive)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            Some((suffix_of(&name)?, e.path()))
        })
        .max_by_key(|(n, _)| *n)
        .map(|(_, p)| p);
    let Some(from) = newest else {
        return false;
    };
    match std::fs::rename(&from, &live) {
        Ok(()) => true,
        Err(e) => {
            util::event(
                util::Level::Warn,
                "ask.unarchive_failed",
                &format!(
                    "could not restore {} → {}: {e}",
                    from.display(),
                    live.display()
                ),
                &[("ask_id", serde_json::json!(ask_id))],
            );
            false
        }
    }
}

/// `looop ask <worker> --prompt "…" [--ref PATH] [--options a,b,c]`
///
/// Worker self-callback (CONTRACT). Writes the ask, then BLOCKS polling answers/
/// until the human replies (`looop answer`), printing the answer to stdout and
/// exiting 0.
/// `<worker>` defaults to `$LOOOP_SESSION_ID` when omitted.
pub fn cmd_ask(paths: &Paths, args: &crate::cli::AskArgs) -> Result<ExitCode> {
    use crate::contract::Contract;
    // worker defaults to $LOOOP_SESSION_ID (a worker self-callback omits it).
    let worker = super::common::session_or_env(args.worker.as_deref());
    if worker.is_empty() {
        eprintln!("usage: looop ask <worker> --prompt \"…\" [--ref PATH] [--options a,b]");
        return Ok(ExitCode::from(1));
    }
    let reference = args.reference.clone().unwrap_or_default();
    // clap already split `--options a,b` on commas; trim each entry.
    let options: Vec<String> = args.options.iter().map(|s| s.trim().to_string()).collect();
    if args.detach {
        // Non-blocking: write the ask and hand back its id. The worker is
        // expected to have checkpointed (--ref) and to END ITS SESSION now —
        // the answer is delivered to a FRESH worker via `--resume <id>`.
        let id = crate::contract::LocalContract::new(paths).ask_detached(
            &worker,
            &args.prompt,
            &reference,
            &options,
        )?;
        println!("{id}");
        return Ok(ExitCode::SUCCESS);
    }
    let answer = crate::contract::LocalContract::new(paths).ask(
        &worker,
        &args.prompt,
        &reference,
        &options,
    )?;
    println!("{answer}");
    Ok(ExitCode::SUCCESS)
}

/// Durably write one ask record and return its id (shared by the blocking and
/// detached paths).
fn write_ask(
    paths: &Paths,
    worker: &str,
    prompt: &str,
    reference: &str,
    options: &[String],
    detach: bool,
) -> Result<String> {
    util::safe_segment("worker id", worker)?;
    if prompt.trim().is_empty() {
        bail!("ask: empty --prompt");
    }
    let store = FileStore::new(paths);
    // Exclusive-create with re-scan on collision: two workers (or two asks
    // from one worker) racing to the same id can never silently overwrite
    // each other — the loser re-scans and takes the next id.
    let id = write_new_record(
        &store,
        &[Collection::Asks, Collection::Answers],
        worker,
        Key::Ask,
        |id| {
            let ask = Ask {
                id: id.to_string(),
                worker: worker.to_string(),
                prompt: prompt.to_string(),
                reference: reference.to_string(),
                options: options.to_vec(),
                detach,
                ts: util::now_unix(),
            };
            stamp_v1(&serde_json::to_string(&ask)?)
        },
    )?;
    util::event(
        util::Level::Step,
        "ask",
        &format!(
            "{worker} {}: {prompt}",
            if detach {
                "asked (detached — will resume on answer)"
            } else {
                "is waiting"
            }
        ),
        &[
            ("ask_id", serde_json::json!(id)),
            ("worker", serde_json::json!(worker)),
            ("detach", serde_json::json!(detach)),
        ],
    );
    Ok(id)
}

/// CONTRACT core for `ask --detach`: write the durable ask and return its ID
/// immediately — no blocking. The asking worker checkpoints and exits; the
/// answer is delivered to a FRESH worker via `worker_start(…, resume)`.
pub(crate) fn ask_detached(
    paths: &Paths,
    worker: &str,
    prompt: &str,
    reference: &str,
    options: &[String],
) -> Result<String> {
    write_ask(paths, worker, prompt, reference, options, true)
}

/// CONTRACT core for `ask`: write the durable ask, then BLOCK polling answers/
/// until the human replies, returning the answer text. Transport-agnostic (no
/// stdout): the CLI presenter prints it; a remote backend would long-poll.
pub(crate) fn ask(
    paths: &Paths,
    worker: &str,
    prompt: &str,
    reference: &str,
    options: &[String],
) -> Result<String> {
    let id = write_ask(paths, worker, prompt, reference, options, false)?;
    let store = FileStore::new(paths);

    // Block until answered. The human sees this ask (via a
    // client / `looop state`) and replies with `looop answer <id>`.
    // Clamped to ≥10ms: `LOOOP_ASK_POLL_MS=0` would busy-spin a full core for
    // the whole (potentially hours-long) human wait — like every other knob,
    // an absurd value gets a safe meaning instead of a pathological one.
    let poll = Duration::from_millis(
        crate::util::env_knob("LOOOP_ASK_POLL_MS")
            .unwrap_or(1000)
            .max(10),
    );
    // Optional wall-clock bound: `LOOOP_ASK_TIMEOUT_S` (default: none — an ask
    // legitimately waits on a human indefinitely).
    // checked_add: an absurd value (u64::MAX) would overflow Instant + Duration
    // and panic — treat overflow as "no deadline".
    let deadline = crate::util::env_knob::<u64>("LOOOP_ASK_TIMEOUT_S")
        .and_then(|s| std::time::Instant::now().checked_add(Duration::from_secs(s)));
    loop {
        match read_answer(&store, &id) {
            AnswerState::Ready(answer) => {
                // Piggyback any steering the human sent while this worker was
                // blocked (`looop tell`): the ask's return is the one moment we KNOW
                // the worker is reading, so undelivered tells ride along with the
                // answer instead of waiting for a `told` poll that may never come.
                let tells = drain_tells(paths, worker);
                if tells.is_empty() {
                    return Ok(answer);
                }
                return Ok(format!(
                    "[steering from the human while you waited — obey alongside the answer]\n{}\n---\n{answer}",
                    tells.join("\n")
                ));
            }
            // A CORRUPT answer record can never resolve by waiting — spinning
            // silently would block this worker forever while the ask re-lists
            // as unanswered (and a re-answer is refused without --force).
            // Error out loudly so the failure is actionable, not invisible.
            AnswerState::Corrupt => bail!(
                "ask {id}: answers/{id}.json exists but is unreadable — \
                 have the human re-answer with `looop answer {id} --force …`"
            ),
            AnswerState::Missing => {}
        }
        // Escape hatches so a worker can never block FOREVER on a dead ask:
        // (a) the ask record itself vanished (deleted / archived out from
        //     under us) — nothing can ever answer it now. exists_checked, NOT
        //     exists(): the plain form squashes a transient stat error
        //     (EACCES, EIO, …) to "absent", which would KILL a legitimately
        //     waiting worker over a hiccup. Only a definitive NotFound proves
        //     no answer can arrive; on an error, warn (once) and keep polling.
        match store.exists_checked(&Key::Ask(id.clone())) {
            Ok(true) => {}
            Ok(false) => bail!(
                "ask {id}: the ask record vanished (deleted or archived) — no answer can arrive"
            ),
            Err(e) => {
                warn_once(
                    format!("ask-stat:{id}"),
                    &format!("ask {id}: cannot stat asks/{id}.json ({e}) — still waiting"),
                );
            }
        }
        // (b) the optional timeout expired.
        if let Some(d) = deadline
            && std::time::Instant::now() >= d
        {
            bail!("ask {id}: timed out waiting for an answer (LOOOP_ASK_TIMEOUT_S)");
        }
        std::thread::sleep(poll);
    }
}

/// `looop asks [--json]` — a client's narrow view: ONLY the pending asks,
/// not the full `state` dump (snapshots / journal / fleet). Plain output is a
/// compact list; `--json` emits the array of ask objects. A client's main job is
/// relaying asks, so this makes that a single cheap call.
pub fn cmd_asks(paths: &Paths, json: bool) -> Result<ExitCode> {
    use crate::contract::Contract;
    let asks = crate::contract::LocalContract::new(paths).asks()?;
    if json {
        // filter_map, NOT unwrap_or_default: a serialization failure must
        // DROP the entry — defaulting would inject a `null` element into the
        // array and break every consumer that indexes into ask objects.
        let arr: Vec<serde_json::Value> = asks
            .iter()
            .filter_map(|a| serde_json::to_value(a).ok())
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::Value::Array(arr))?
        );
        return Ok(ExitCode::SUCCESS);
    }
    if asks.is_empty() {
        println!("no pending asks");
        return Ok(ExitCode::SUCCESS);
    }
    for a in &asks {
        println!("⚑ {} ({}): {}", a.id, a.worker, a.prompt);
        if !a.reference.is_empty() {
            println!("    ref: {}", a.reference);
        }
        if !a.options.is_empty() {
            println!("    options: {}", a.options.join(", "));
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::super::common::next_seq_id;
    use super::super::test_util::{ans, temp_seeded};
    use super::super::{answer, cmd_answer};
    use super::*;
    use crate::store::EnvRestore;
    use std::fs;

    #[test]
    fn ask_ids_increment_and_pending_excludes_answered() {
        let p = temp_seeded();
        let store = FileStore::new(&p);
        fs::create_dir_all(p.asks_dir()).unwrap();
        fs::create_dir_all(p.answers_dir()).unwrap();

        assert_eq!(
            next_seq_id(&store, &[Collection::Asks, Collection::Answers], "triage"),
            "triage-1"
        );
        let a = Ask {
            id: "triage-1".into(),
            worker: "triage".into(),
            prompt: "merge?".into(),
            reference: String::new(),
            options: vec![],
            detach: false,
            ts: 1,
        };
        fs::write(
            p.asks_dir().join("triage-1.json"),
            serde_json::to_string(&a).unwrap(),
        )
        .unwrap();

        assert_eq!(
            next_seq_id(&store, &[Collection::Asks, Collection::Answers], "triage"),
            "triage-2"
        );
        assert_eq!(pending(&p).len(), 1, "unanswered ask is pending");

        // Answering it removes it from pending but keeps the id reserved.
        cmd_answer(&p, &ans("triage-1", "yes", false)).unwrap();
        assert!(pending(&p).is_empty(), "answered ask is not pending");
        assert_eq!(read_answer(&store, "triage-1").text(), Some("yes"));
        assert_eq!(
            next_seq_id(&store, &[Collection::Asks, Collection::Answers], "triage"),
            "triage-2"
        );
    }

    #[test]
    fn ask_errors_when_the_ask_record_vanishes() {
        // set_var is process-global: serialize against other env-mutating
        // tests, and restore the knob even if an assert below panics.
        let _g = crate::util::test_env_lock();
        let _r = EnvRestore::set("LOOOP_ASK_POLL_MS", "10");
        let p = Paths::temp();
        let ask_file = p.asks_dir().join("w-1.json");
        let handle = std::thread::spawn(move || ask(&p, "w", "anyone there?", "", &[]));
        // Wait for the ask record to appear, then delete it out from under the
        // blocked worker — the poll loop must ERROR instead of spinning forever.
        for _ in 0..200 {
            if ask_file.is_file() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(ask_file.is_file(), "ask record never appeared");
        fs::remove_file(&ask_file).unwrap();
        let res = handle.join().unwrap();
        let err = res.expect_err("a vanished ask must error, not block forever");
        assert!(err.to_string().contains("vanished"), "got: {err}");
    }

    #[test]
    fn detached_ask_returns_id_and_resumes_through_the_pair_lifecycle() {
        let p = temp_seeded();
        // Raise: returns the id immediately (no blocking), pending shows it.
        let id = ask_detached(&p, "triage", "merge or split?", "reports/cp.md", &[]).unwrap();
        assert_eq!(id, "triage-1");
        let pend = pending(&p);
        assert_eq!(pend.len(), 1);
        assert!(pend[0].detach, "the record carries the detach flag");
        assert!(
            answered_detached(&p).is_empty(),
            "nothing to resume before the answer"
        );
        // resume_context before the answer is a loud error.
        assert!(resume_context(&p, &id).is_err());
        // …and so is resuming a BLOCKING (non-detached) ask: its worker is
        // already polling for the answer.
        let blocking = write_ask(&p, "other", "quick q?", "", &[], false).unwrap();
        answer(&p, &blocking, "quick a", false).unwrap();
        let err = resume_context(&p, &blocking).unwrap_err().to_string();
        assert!(err.contains("blocking ask"), "got: {err}");
        // …and so is an id that is not a safe path segment (traversal guard).
        assert!(resume_context(&p, "../evil").is_err());

        // Answer: the ask leaves pending and becomes resumable.
        cmd_answer(&p, &ans(&id, "split it", false)).unwrap();
        assert!(pending(&p).is_empty());
        let resumable = answered_detached(&p);
        assert_eq!(resumable.len(), 1);
        assert_eq!(resumable[0].1, "split it");

        // The resume preamble carries question, answer, and checkpoint.
        let block = resume_context(&p, &id).unwrap();
        assert!(block.contains("merge or split?"));
        assert!(block.contains("split it"));
        assert!(block.contains("reports/cp.md"));

        // Consuming archives the pair: nothing left to resume, records kept.
        assert!(archive_pair(&p, &id), "the first consumer claims the pair");
        assert!(answered_detached(&p).is_empty());
        assert!(p.asks_dir().join("archive/triage-1.json").is_file());
        assert!(p.answers_dir().join("archive/triage-1.json").is_file());
        // A SECOND consume of the same pair loses the claim — the ask half's
        // rename is the atomic test-and-set a concurrent start is gated on.
        assert!(
            !archive_pair(&p, &id),
            "an already-consumed pair must not be claimable again"
        );
    }

    #[test]
    fn racing_archive_pair_claims_let_exactly_one_win() {
        // Regression: resume consumption used to be fire-and-forget, so two
        // concurrent `worker start --resume <id>` calls could BOTH dispatch
        // the same answered-detached ask. The ask half's archive rename is
        // the claim (same idiom as drain_tells): exactly one claimant wins.
        let p = temp_seeded();
        let id = ask_detached(&p, "w", "go?", "", &[]).unwrap();
        answer(&p, &id, "go", false).unwrap();
        let wins: usize = std::thread::scope(|s| {
            let handles: Vec<_> = (0..8).map(|_| s.spawn(|| archive_pair(&p, &id))).collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap())
                .filter(|won| *won)
                .count()
        });
        assert_eq!(wins, 1, "exactly one concurrent consumer claims the pair");
        assert!(
            p.asks_dir().join(format!("archive/{id}.json")).is_file(),
            "the claimed pair is archived, not lost"
        );
    }

    #[test]
    fn archived_ask_ids_are_reusable_without_clobbering_the_archive() {
        let p = Paths::temp();
        let id = ask_detached(&p, "w", "first?", "", &[]).unwrap();
        cmd_answer(&p, &ans(&id, "one", false)).unwrap();
        assert!(archive_pair(&p, &id));
        // The id is free again (next_ask_id scans only live dirs)…
        let id2 = ask_detached(&p, "w", "second?", "", &[]).unwrap();
        assert_eq!(id2, id, "live id space is reusable after archive");
        cmd_answer(&p, &ans(&id2, "two", false)).unwrap();
        assert!(archive_pair(&p, &id2));
        // …and the second archive did not clobber the first record.
        assert!(p.asks_dir().join("archive/w-1.json").is_file());
        assert!(p.asks_dir().join("archive/w-1-1.json").is_file());
    }

    #[test]
    fn unarchive_restores_the_pair_and_never_pairs_a_stale_answer_with_a_new_ask() {
        let p = Paths::temp();
        let id = ask_detached(&p, "w", "first?", "", &[]).unwrap();
        cmd_answer(&p, &ans(&id, "one", false)).unwrap();
        assert!(archive_pair(&p, &id));

        // Plain restore: both halves come back, the pair is resumable again.
        unarchive_pair(&p, &id);
        assert!(p.asks_dir().join(format!("{id}.json")).is_file());
        assert!(p.answers_dir().join(format!("{id}.json")).is_file());
        assert_eq!(answered_detached(&p).len(), 1);

        // Archive again, then let a NEW live ask REUSE the id: unarchive must
        // restore NEITHER half — restoring only the answer would attach the
        // stale "one" to the brand-new question.
        assert!(archive_pair(&p, &id));
        let id2 = ask_detached(&p, "w", "second, unrelated?", "", &[]).unwrap();
        assert_eq!(id2, id, "the live id space reuses archived ids");
        unarchive_pair(&p, &id);
        assert!(
            !p.answers_dir().join(format!("{id}.json")).exists(),
            "a stale answer must not be attached to the new ask"
        );
        assert!(
            p.asks_dir()
                .join("archive")
                .join(format!("{id}.json"))
                .is_file(),
            "the archived ask half stays archived"
        );
        assert!(
            pending(&p).iter().any(|a| a.prompt == "second, unrelated?"),
            "the new live ask is untouched"
        );
    }

    #[test]
    fn unarchive_survives_a_gap_in_the_archive_suffix_sequence() {
        // Regression: the restore scan used to probe suffixes upward from the
        // bare name and stop at the first hole, so `<id>.json` + `<id>-2.json`
        // (a pruned `-1` generation) restored the STALE bare record. The scan
        // must pick the HIGHEST suffix present, gaps notwithstanding.
        let p = Paths::temp();
        let arch = p.asks_dir().join("archive");
        fs::create_dir_all(&arch).unwrap();
        fs::write(arch.join("w-1.json"), r#"{"gen":"oldest"}"#).unwrap();
        fs::write(arch.join("w-1-2.json"), r#"{"gen":"newest"}"#).unwrap();
        assert!(restore_newest_archived(&p.asks_dir(), "w-1"));
        assert_eq!(
            fs::read_to_string(p.asks_dir().join("w-1.json")).unwrap(),
            r#"{"gen":"newest"}"#,
            "the highest suffix (newest generation) wins, not the first probe hit"
        );
        assert!(
            arch.join("w-1.json").is_file(),
            "the older generation stays archived"
        );
    }

    #[test]
    #[cfg(unix)]
    fn ask_keeps_polling_through_a_transient_stat_failure() {
        // Regression: the poll-exit used exists(), so a transient stat error
        // read as "the ask record vanished" and KILLED a legitimately waiting
        // worker. The loop must keep polling through the error window and
        // still deliver the answer once the store recovers.
        let _g = crate::util::test_env_lock();
        let _r = EnvRestore::set("LOOOP_ASK_POLL_MS", "10");
        let p = Paths::temp();
        let ask_file = p.asks_dir().join("w-1.json");
        // A NON-cleaning copy of the profile for the worker thread: the outer
        // `p` owns the temp-dir teardown, and the test needs `p` on this side
        // too (to deny/restore access and to answer).
        let worker_paths = Paths {
            bin: p.bin.clone(),
            data_dir: p.data_dir.clone(),
            config: p.config.clone(),
            default_profile: p.default_profile,
            temp_cleanup: false,
        };
        let handle = std::thread::spawn(move || ask(&worker_paths, "w", "still there?", "", &[]));
        for _ in 0..200 {
            if ask_file.is_file() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(ask_file.is_file(), "ask record never appeared");
        {
            let (enforced, _deny) = crate::store::deny_access(p.asks_dir());
            if enforced {
                // Several poll intervals inside the error window: the worker
                // must survive it (warn-once + keep polling), not bail.
                std::thread::sleep(Duration::from_millis(100));
                assert!(
                    !handle.is_finished(),
                    "a transient stat failure must not kill a waiting worker"
                );
            }
        } // perms restored — the store has "recovered"
        answer(&p, "w-1", "yes", false).unwrap();
        let got = handle.join().unwrap().expect("the answer still arrives");
        assert!(got.contains("yes"), "got: {got}");
    }

    #[test]
    fn asks_lists_only_pending() {
        let p = temp_seeded();
        fs::create_dir_all(p.asks_dir()).unwrap();
        fs::create_dir_all(p.answers_dir()).unwrap();
        fs::write(
            p.asks_dir().join("w-1.json"),
            serde_json::json!({"id":"w-1","worker":"w","prompt":"ok?","ts":1}).to_string(),
        )
        .unwrap();
        assert_eq!(pending(&p).len(), 1);
        // cmd_asks is a thin view over pending(); answering empties it.
        cmd_answer(&p, &ans("w-1", "yes", false)).unwrap();
        assert!(pending(&p).is_empty());
    }
}
