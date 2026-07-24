//! The ask half of the mailbox: raising blocking questions and listing pending
//! asks.

use super::answer::{AnswerState, read_answer};
use super::common::{stamp_v1, warn_future_v, warn_once, write_new_record};
use super::tell::drain_tells;
use crate::paths::Paths;
use crate::store::{Collection, FileStore, Key, StateStore};
use crate::util;
use anyhow::{Result, bail};
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
    /// Read-only migration marker for records written by the removed
    /// `ask --detach` feature. No new record sets it, and it is never exposed
    /// again when serializing state. It only prevents a later worker reusing
    /// the same id from being mistaken for the process blocked on this ask.
    #[serde(default, rename = "detach", skip_serializing)]
    pub(crate) legacy_detached: bool,
    /// Unix seconds the ask was raised.
    pub ts: u64,
}

impl Ask {
    /// Whether a live worker with `self.worker` can actually be blocked inside
    /// this ask. Legacy detached records had no waiting process.
    pub(crate) fn blocks_worker(&self) -> bool {
        !self.legacy_detached
    }
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
                legacy_detached: false,
                ts: util::now_unix(),
            };
            stamp_v1(&serde_json::to_string(&ask)?)
        },
    )?;
    // `write_ask` MUST NOT log to stdout: the blocking callback is normally
    // invoked through shell command substitution (`answer=$(looop ask …)`),
    // where stdout is the worker's answer protocol.  Mixing the ask event into
    // that stream leaves the runner with a log line instead of a clean answer
    // and can prevent it from returning to the model after the human replies.
    // The mailbox record itself is the durable event; presenters may report it
    // on a separate channel.
    Ok(id)
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
    let id = write_ask(paths, worker, prompt, reference, options)?;
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
            legacy_detached: false,
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
    fn legacy_detach_field_is_migration_only() {
        let ask: Ask =
            serde_json::from_str(r#"{"id":"w-1","worker":"w","prompt":"q?","detach":true,"ts":1}"#)
                .unwrap();
        assert_eq!(ask.id, "w-1");
        assert!(!ask.blocks_worker());
        assert!(
            !serde_json::to_value(&ask)
                .unwrap()
                .as_object()
                .unwrap()
                .contains_key("detach"),
            "the removed field is accepted only for migration and is never re-exposed"
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
        assert_eq!(got, "yes", "the worker callback returns only the answer");
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
