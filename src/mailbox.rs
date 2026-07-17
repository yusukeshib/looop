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

use crate::paths::Paths;
use crate::store::{Collection, FileStore, Key, StateStore};
use crate::util;
use anyhow::{Context, Result, bail};
use std::process::ExitCode;
use std::time::Duration;

/// One pending question. Serialized to `asks/<id>.json`.
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

/// Allocate the next ask id for a worker: `<worker>-<n>` where `n` is one past
/// the highest existing index across BOTH asks/ and answers/ (so an answered
/// ask's id is never reused while its record lingers).
fn next_ask_id(store: &impl StateStore, worker: &str) -> String {
    let mut max = 0u64;
    for coll in [Collection::Asks, Collection::Answers] {
        for stem in store.list(&coll) {
            if let Some(idx) = stem.strip_prefix(&format!("{worker}-"))
                && let Ok(n) = idx.parse::<u64>()
            {
                max = max.max(n);
            }
        }
    }
    format!("{worker}-{}", max + 1)
}

/// Read the answer text for an ask id, if it has been answered.
fn read_answer(store: &impl StateStore, ask_id: &str) -> Option<String> {
    let raw = store.read(&Key::Answer(ask_id.to_string()))?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    v.get("answer").and_then(|x| x.as_str()).map(str::to_owned)
}

/// All asks that have NO matching answer yet. Read-only; used by `state` and
/// the decide prompt (so looop sees what's blocked) and by any
/// client (so the human sees what's waiting on them).
pub fn pending(paths: &Paths) -> Vec<Ask> {
    let store = FileStore::new(paths);
    let mut out = Vec::new();
    for id in store.list(&Collection::Asks) {
        if let Some(raw) = store.read(&Key::Ask(id.clone()))
            && let Ok(ask) = serde_json::from_str::<Ask>(&raw)
            && read_answer(&store, &ask.id).is_none()
        {
            out.push(ask);
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
        if let Some(raw) = store.read(&Key::Ask(id.clone()))
            && let Ok(ask) = serde_json::from_str::<Ask>(&raw)
            && ask.detach
            && let Some(answer) = read_answer(&store, &ask.id)
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
    let store = FileStore::new(paths);
    let raw = store
        .read(&Key::Ask(ask_id.to_string()))
        .with_context(|| format!("resume: no ask {ask_id:?}"))?;
    let ask: Ask = serde_json::from_str(&raw).with_context(|| format!("resume: ask {ask_id:?}"))?;
    let answer = read_answer(&store, ask_id)
        .with_context(|| format!("resume: ask {ask_id:?} has no answer yet"))?;
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
/// without an answer file archives just the ask).
pub fn archive_pair(paths: &Paths, ask_id: &str) {
    let store = FileStore::new(paths);
    let _ = store.archive(&Key::Ask(ask_id.to_string()));
    let _ = store.archive(&Key::Answer(ask_id.to_string()));
}

/// The `sys-asks` system-sensor probe: makes the mailbox a FIRST-CLASS part of
/// the world hash. Signal: the pending ask ids plus the answered-detached ids
/// awaiting a resume — an ask being raised, answered, or resumed each changes
/// the signal exactly once (level-triggered, no clock in the signal). Volatile
/// context (ages, prompts) rides in detail.
pub fn sys_asks(paths: &Paths) -> serde_json::Value {
    let now = util::now_unix();
    let pending = pending(paths);
    let resume: Vec<(Ask, String)> = answered_detached(paths);
    let mut detail = serde_json::Map::new();
    for a in &pending {
        detail.insert(
            a.id.clone(),
            serde_json::json!({
                "worker": a.worker,
                "detach": a.detach,
                "age_s": now.saturating_sub(a.ts),
            }),
        );
    }
    for (a, _) in &resume {
        detail.insert(
            a.id.clone(),
            serde_json::json!({
                "worker": a.worker,
                "answered": true,
                "age_s": now.saturating_sub(a.ts),
            }),
        );
    }
    serde_json::json!({
        "signal": {
            "pending": pending.iter().map(|a| a.id.clone()).collect::<Vec<_>>(),
            "resume": resume.iter().map(|(a, _)| a.id.clone()).collect::<Vec<_>>(),
        },
        "detail": detail,
    })
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
    let worker = match &args.worker {
        Some(w) if !w.is_empty() => w.clone(),
        _ => std::env::var("LOOOP_SESSION_ID").unwrap_or_default(),
    };
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
    let id = next_ask_id(&store, worker);
    let ask = Ask {
        id: id.clone(),
        worker: worker.to_string(),
        prompt: prompt.to_string(),
        reference: reference.to_string(),
        options: options.to_vec(),
        detach,
        ts: util::now_unix(),
    };
    store.write_atomic(&Key::Ask(id.clone()), &serde_json::to_string_pretty(&ask)?)?;
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
    let poll = Duration::from_millis(
        std::env::var("LOOOP_ASK_POLL_MS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(1000),
    );
    loop {
        if let Some(answer) = read_answer(&store, &id) {
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
        std::thread::sleep(poll);
    }
}

// ---- tells — the human → worker steering channel --------------------------------

/// One steering message for a running worker. Serialized to `tells/<id>.json`;
/// consumed (deleted) when the worker drains it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Tell {
    pub id: String,
    pub worker: String,
    pub msg: String,
    pub ts: u64,
}

/// Allocate `<worker>-<n>` one past the highest pending tell for the worker.
fn next_tell_id(store: &impl StateStore, worker: &str) -> String {
    let mut max = 0u64;
    for stem in store.list(&Collection::Tells) {
        if let Some(idx) = stem.strip_prefix(&format!("{worker}-"))
            && let Ok(n) = idx.parse::<u64>()
        {
            max = max.max(n);
        }
    }
    format!("{worker}-{}", max + 1)
}

/// Undelivered tells for `worker`, oldest first.
pub fn pending_tells(paths: &Paths, worker: &str) -> Vec<Tell> {
    let store = FileStore::new(paths);
    let mut out: Vec<Tell> = store
        .list(&Collection::Tells)
        .into_iter()
        .filter_map(|id| {
            let raw = store.read(&Key::Tell(id))?;
            serde_json::from_str::<Tell>(&raw).ok()
        })
        .filter(|t| t.worker == worker)
        .collect();
    out.sort_by(|a, b| a.ts.cmp(&b.ts).then_with(|| a.id.cmp(&b.id)));
    out
}

/// Consume (return + delete) every pending tell for `worker`, oldest first.
pub fn drain_tells(paths: &Paths, worker: &str) -> Vec<String> {
    let store = FileStore::new(paths);
    pending_tells(paths, worker)
        .into_iter()
        .map(|t| {
            let _ = store.remove(&Key::Tell(t.id.clone()));
            format!("• {}", t.msg)
        })
        .collect()
}

/// `looop tell <worker> <message…|->` — human/concierge verb: queue a steering
/// message INTO a live worker. Delivery is pull-based (the worker's only I/O is
/// the mailbox): the worker picks it up at its next `looop told` check, or
/// piggybacked on its next `looop ask` answer. Refuses a dead worker — a corpse
/// will never read it; steer via goals / a fresh worker instead.
pub fn cmd_tell(paths: &Paths, args: &crate::cli::TellArgs) -> Result<ExitCode> {
    util::safe_segment("worker id", &args.worker)?;
    let msg = args.body.join(" ").trim().to_string();
    if msg.is_empty() {
        bail!("tell: empty message");
    }
    if !crate::session::is_alive(paths, &args.worker) {
        bail!(
            "tell {}: not a live worker (a dead worker can never read it — steer via goals or a fresh worker)",
            args.worker
        );
    }
    let store = FileStore::new(paths);
    let id = next_tell_id(&store, &args.worker);
    let tell = Tell {
        id: id.clone(),
        worker: args.worker.clone(),
        msg,
        ts: util::now_unix(),
    };
    store.write_atomic(
        &Key::Tell(id.clone()),
        &serde_json::to_string_pretty(&tell)?,
    )?;
    util::event(
        util::Level::Ok,
        "tell",
        &format!("queued for {}: {}", args.worker, tell.msg),
        &[
            ("tell_id", serde_json::json!(id)),
            ("worker", serde_json::json!(args.worker)),
        ],
    );
    Ok(ExitCode::SUCCESS)
}

/// `looop told [worker]` — worker self-callback: print + consume any steering
/// messages queued for it (one per line). Prints nothing when there are none.
/// `<worker>` defaults to `$LOOOP_SESSION_ID`.
pub fn cmd_told(paths: &Paths, args: &crate::cli::ToldArgs) -> Result<ExitCode> {
    let worker = match &args.worker {
        Some(w) if !w.is_empty() => w.clone(),
        _ => std::env::var("LOOOP_SESSION_ID").unwrap_or_default(),
    };
    if worker.is_empty() {
        eprintln!("usage: looop told [worker]  (or run inside a worker with $LOOOP_SESSION_ID)");
        return Ok(ExitCode::from(1));
    }
    for line in drain_tells(paths, &worker) {
        println!("{line}");
    }
    Ok(ExitCode::SUCCESS)
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
    if !store.exists(&Key::Ask(ask_id.to_string())) {
        bail!("answer: no pending ask {ask_id:?}");
    }
    // Answers are durable: refuse to clobber one already given unless `--force`.
    // A worker that has already read its answer has moved on, so a stray re-answer
    // is almost always a misfire — fail loudly instead of silently overwriting.
    if store.exists(&Key::Answer(ask_id.to_string())) && !force {
        bail!("answer: {ask_id:?} is already answered (pass --force to overwrite)");
    }
    let body = serde_json::json!({ "answer": text, "ts": util::now_unix() });
    store.write_atomic(
        &Key::Answer(ask_id.to_string()),
        &serde_json::to_string_pretty(&body)?,
    )?;
    Ok(())
}

/// `looop asks [--json]` — a client's narrow view: ONLY the pending asks,
/// not the full `state` dump (snapshots / journal / fleet). Plain output is a
/// compact list; `--json` emits the array of ask objects. A client's main job is
/// relaying asks, so this makes that a single cheap call.
pub fn cmd_asks(paths: &Paths, json: bool) -> Result<ExitCode> {
    use crate::contract::Contract;
    let asks = crate::contract::LocalContract::new(paths).asks()?;
    if json {
        let arr: Vec<serde_json::Value> = asks
            .iter()
            .map(|a| serde_json::to_value(a).unwrap_or_default())
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
    use super::*;
    use std::fs;

    /// Build an `AnswerArgs` the way clap would after parsing
    /// `answer <id> <text…> [--force]`.
    fn ans(id: &str, text: &str, force: bool) -> crate::cli::AnswerArgs {
        crate::cli::AnswerArgs {
            ask_id: id.into(),
            body: vec![text.into()],
            force,
        }
    }

    #[test]
    fn ask_ids_increment_and_pending_excludes_answered() {
        let p = Paths::temp();
        let store = FileStore::new(&p);
        fs::create_dir_all(p.asks_dir()).unwrap();
        fs::create_dir_all(p.answers_dir()).unwrap();

        assert_eq!(next_ask_id(&store, "triage"), "triage-1");
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

        assert_eq!(next_ask_id(&store, "triage"), "triage-2");
        assert_eq!(pending(&p).len(), 1, "unanswered ask is pending");

        // Answering it removes it from pending but keeps the id reserved.
        cmd_answer(&p, &ans("triage-1", "yes", false)).unwrap();
        assert!(pending(&p).is_empty(), "answered ask is not pending");
        assert_eq!(read_answer(&store, "triage-1").as_deref(), Some("yes"));
        assert_eq!(next_ask_id(&store, "triage"), "triage-2");
    }

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
            read_answer(&FileStore::new(&p), "w-1").as_deref(),
            Some("first")
        );
        // `--force` lets the human deliberately recover from a bad answer.
        cmd_answer(&p, &ans("w-1", "second", true)).unwrap();
        assert_eq!(
            read_answer(&FileStore::new(&p), "w-1").as_deref(),
            Some("second")
        );
    }

    #[test]
    fn tells_queue_in_order_and_drain_consumes() {
        let p = Paths::temp();
        let store = FileStore::new(&p);
        for (i, msg) in ["focus the PR", "skip the docs"].iter().enumerate() {
            let id = next_tell_id(&store, "triage");
            assert_eq!(id, format!("triage-{}", i + 1));
            let t = Tell {
                id: id.clone(),
                worker: "triage".into(),
                msg: msg.to_string(),
                ts: i as u64,
            };
            store
                .write_atomic(&Key::Tell(id), &serde_json::to_string(&t).unwrap())
                .unwrap();
        }
        assert_eq!(pending_tells(&p, "triage").len(), 2);
        assert_eq!(pending_tells(&p, "other").len(), 0, "scoped per worker");

        let drained = drain_tells(&p, "triage");
        assert_eq!(drained, vec!["• focus the PR", "• skip the docs"]);
        assert!(pending_tells(&p, "triage").is_empty(), "drain consumes");
    }

    #[test]
    fn detached_ask_returns_id_and_resumes_through_the_pair_lifecycle() {
        let p = Paths::temp();
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
        archive_pair(&p, &id);
        assert!(answered_detached(&p).is_empty());
        assert!(p.asks_dir().join("archive/triage-1.json").is_file());
        assert!(p.answers_dir().join("archive/triage-1.json").is_file());
    }

    #[test]
    fn sys_asks_signal_tracks_the_ask_lifecycle() {
        let p = Paths::temp();
        let v = sys_asks(&p);
        assert_eq!(v["signal"]["pending"], serde_json::json!([]));
        assert_eq!(v["signal"]["resume"], serde_json::json!([]));

        let id = ask_detached(&p, "w", "q?", "", &[]).unwrap();
        let v = sys_asks(&p);
        assert_eq!(v["signal"]["pending"], serde_json::json!([id.clone()]));

        cmd_answer(&p, &ans(&id, "a", false)).unwrap();
        let v = sys_asks(&p);
        assert_eq!(v["signal"]["pending"], serde_json::json!([]));
        assert_eq!(v["signal"]["resume"], serde_json::json!([id.clone()]));
        assert_eq!(v["detail"][&id]["answered"], serde_json::json!(true));

        archive_pair(&p, &id);
        let v = sys_asks(&p);
        assert_eq!(
            v["signal"]["resume"],
            serde_json::json!([]),
            "archiving settles the wake signal"
        );
    }

    #[test]
    fn archived_ask_ids_are_reusable_without_clobbering_the_archive() {
        let p = Paths::temp();
        let id = ask_detached(&p, "w", "first?", "", &[]).unwrap();
        cmd_answer(&p, &ans(&id, "one", false)).unwrap();
        archive_pair(&p, &id);
        // The id is free again (next_ask_id scans only live dirs)…
        let id2 = ask_detached(&p, "w", "second?", "", &[]).unwrap();
        assert_eq!(id2, id, "live id space is reusable after archive");
        cmd_answer(&p, &ans(&id2, "two", false)).unwrap();
        archive_pair(&p, &id2);
        // …and the second archive did not clobber the first record.
        assert!(p.asks_dir().join("archive/w-1.json").is_file());
        assert!(p.asks_dir().join("archive/w-1-1.json").is_file());
    }

    #[test]
    fn tell_refuses_a_dead_worker() {
        let p = Paths::temp();
        let args = crate::cli::TellArgs {
            worker: "ghost".into(),
            body: vec!["hello".into()],
        };
        assert!(
            cmd_tell(&p, &args).is_err(),
            "a corpse can never read a tell — refuse it"
        );
    }

    #[test]
    fn asks_lists_only_pending() {
        let p = Paths::temp();
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
