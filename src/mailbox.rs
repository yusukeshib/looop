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

/// Schema version stamped into serialized mailbox records. Records written
/// before versioning carry no `v` and deserialize as v1 (serde ignores unknown
/// fields on read, so `v` is also transparently ACCEPTED on Ask, whose struct
/// deliberately carries no `v` field — other modules construct Ask literals).
fn default_v() -> u32 {
    1
}

/// Stamp `"v": 1` into a serialized record body (see [`default_v`]).
fn stamp_v1(body: &str) -> Result<String> {
    let mut val: serde_json::Value = serde_json::from_str(body)?;
    if let Some(obj) = val.as_object_mut() {
        obj.insert("v".into(), serde_json::json!(1));
    }
    Ok(serde_json::to_string_pretty(&val)?)
}

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

/// Allocate the next sequential id for a worker: `<worker>-<n>` where `n` is
/// one past the highest existing index across `collections`. Shared by asks
/// (scans asks/ AND answers/, so an answered ask's id is never reused while its
/// record lingers) and tells (scans tells/ only). The scan-max+1 is inherently
/// racy across processes — callers must WRITE the record via `create_exclusive`
/// and re-scan on collision (see [`write_new_record`]).
fn next_seq_id(store: &impl StateStore, collections: &[Collection], worker: &str) -> String {
    let mut max = 0u64;
    for coll in collections {
        for stem in store.list(coll) {
            if let Some(idx) = stem.strip_prefix(&format!("{worker}-"))
                && let Ok(n) = idx.parse::<u64>()
            {
                max = max.max(n);
            }
        }
    }
    format!("{worker}-{}", max + 1)
}

/// Allocate an id and durably create the record for it, retrying on collision:
/// scan-max+1 then EXCLUSIVE-create — when two issuers race to the same id,
/// exactly one create wins and the loser re-scans. `make` builds the record
/// body for a candidate id; `key` maps the id to its store key. Bounded (~20
/// attempts) so pathological contention errors out instead of spinning.
fn write_new_record(
    store: &impl StateStore,
    collections: &[Collection],
    worker: &str,
    key: impl Fn(String) -> Key,
    make: impl Fn(&str) -> Result<String>,
) -> Result<String> {
    for _ in 0..20 {
        let id = next_seq_id(store, collections, worker);
        let body = make(&id)?;
        if store.create_exclusive(&key(id.clone()), &body)? {
            return Ok(id);
        }
        // Collision: another issuer took this id first — re-scan and retry.
    }
    bail!("mailbox: could not allocate an id for {worker:?} after 20 attempts (contention)")
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
        let Some(raw) = store.read(&Key::Ask(id.clone())) else {
            continue;
        };
        match serde_json::from_str::<Ask>(&raw) {
            Ok(ask) => {
                if read_answer(&store, &ask.id).is_none() {
                    out.push(ask);
                }
            }
            // A record we cannot parse is a VISIBLE problem, not a silent drop
            // — a worker may be blocked on it forever. STDERR, not stdout:
            // pending() feeds machine output (`looop asks --json`,
            // `looop state --json`) and stdout must stay clean for it.
            Err(e) => {
                eprintln!("asks/{id}.json is unparseable ({e}) — record ignored");
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
/// without an answer file archives just the ask). An id that is not a safe
/// path segment archives nothing — the id becomes a filename, so it gets the
/// same guard as every other mailbox verb.
pub fn archive_pair(paths: &Paths, ask_id: &str) {
    if util::safe_segment("ask id", ask_id).is_err() {
        return;
    }
    let store = FileStore::new(paths);
    // Failures are non-fatal but must be VISIBLE — a pair that fails to
    // archive keeps the sys-asks resume signal hot (possible re-dispatch).
    if let Err(e) = store.archive(&Key::Ask(ask_id.to_string())) {
        util::event(
            util::Level::Warn,
            "ask.archive_failed",
            &format!("could not archive asks/{ask_id}.json: {e}"),
            &[("ask_id", serde_json::json!(ask_id))],
        );
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
/// the rename failed.
fn restore_newest_archived(dir: &std::path::Path, ask_id: &str) -> bool {
    let live = dir.join(format!("{ask_id}.json"));
    if live.exists() {
        return true;
    }
    let archive = dir.join("archive");
    // The archive suffixes on collision (`<id>.json`, `<id>-1.json`, …);
    // the highest suffix is the record we just archived.
    let mut newest: Option<std::path::PathBuf> = None;
    let mut n = 0u64;
    let mut candidate = archive.join(format!("{ask_id}.json"));
    while candidate.is_file() {
        newest = Some(candidate);
        n += 1;
        candidate = archive.join(format!("{ask_id}-{n}.json"));
    }
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
    let poll = Duration::from_millis(crate::util::env_knob("LOOOP_ASK_POLL_MS").unwrap_or(1000));
    // Optional wall-clock bound: `LOOOP_ASK_TIMEOUT_S` (default: none — an ask
    // legitimately waits on a human indefinitely).
    // checked_add: an absurd value (u64::MAX) would overflow Instant + Duration
    // and panic — treat overflow as "no deadline".
    let deadline = crate::util::env_knob::<u64>("LOOOP_ASK_TIMEOUT_S")
        .and_then(|s| std::time::Instant::now().checked_add(Duration::from_secs(s)));
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
        // Escape hatches so a worker can never block FOREVER on a dead ask:
        // (a) the ask record itself vanished (deleted / archived out from
        //     under us) — nothing can ever answer it now;
        if !store.exists(&Key::Ask(id.clone())) {
            bail!("ask {id}: the ask record vanished (deleted or archived) — no answer can arrive");
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

// ---- tells — the human → worker steering channel --------------------------------

/// One steering message for a running worker. Serialized to `tells/<id>.json`;
/// consumed (deleted) when the worker drains it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Tell {
    /// Record schema version (v1 today; absent ⇒ v1).
    #[serde(default = "default_v")]
    pub v: u32,
    pub id: String,
    pub worker: String,
    pub msg: String,
    pub ts: u64,
}

/// Undelivered tells for `worker`, oldest first.
pub fn pending_tells(paths: &Paths, worker: &str) -> Vec<Tell> {
    let store = FileStore::new(paths);
    let mut out: Vec<Tell> = store
        .list(&Collection::Tells)
        .into_iter()
        .filter_map(|id| {
            let raw = store.read(&Key::Tell(id.clone()))?;
            match serde_json::from_str::<Tell>(&raw) {
                Ok(t) => Some(t),
                Err(e) => {
                    // Visible, not silent: a dropped tell is lost steering.
                    // STDERR, not stdout: `looop told` prints the drained
                    // tells on stdout and that stream IS the text a worker
                    // consumes — a warning there would be read as steering.
                    eprintln!("tells/{id}.json is unparseable ({e}) — record ignored");
                    None
                }
            }
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
    // Same collision-safe allocation as asks: exclusive-create, re-scan on loss.
    let id = write_new_record(
        &store,
        &[Collection::Tells],
        &args.worker,
        Key::Tell,
        |id| {
            let tell = Tell {
                v: 1,
                id: id.to_string(),
                worker: args.worker.clone(),
                msg: msg.clone(),
                ts: util::now_unix(),
            };
            Ok(serde_json::to_string_pretty(&tell)?)
        },
    )?;
    util::event(
        util::Level::Ok,
        "tell",
        &format!("queued for {}: {msg}", args.worker),
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
        assert_eq!(read_answer(&store, "triage-1").as_deref(), Some("yes"));
        assert_eq!(
            next_seq_id(&store, &[Collection::Asks, Collection::Answers], "triage"),
            "triage-2"
        );
    }

    #[test]
    fn write_new_record_rescans_on_collision() {
        let p = Paths::temp();
        let store = FileStore::new(&p);
        fs::create_dir_all(p.asks_dir()).unwrap();
        // Simulate a racer: the FIRST time the body is built (i.e. after the
        // scan chose an id, before our exclusive create), another issuer lands
        // the same id. Our create must lose, re-scan, and take the next id.
        let raced = std::cell::Cell::new(false);
        let id = write_new_record(
            &store,
            &[Collection::Asks, Collection::Answers],
            "w",
            Key::Ask,
            |id| {
                if !raced.replace(true) {
                    fs::write(p.asks_dir().join(format!("{id}.json")), "{\"racer\":1}").unwrap();
                }
                Ok(format!("{{\"mine\":\"{id}\"}}"))
            },
        )
        .unwrap();
        assert_eq!(id, "w-2", "loser re-scans past the racer's id");
        assert_eq!(
            fs::read_to_string(p.asks_dir().join("w-1.json")).unwrap(),
            "{\"racer\":1}",
            "the racer's record is never overwritten"
        );
        assert!(p.asks_dir().join("w-2.json").is_file());
    }

    /// Restores an env var to its pre-test value on drop (panic-safe).
    struct EnvRestore(&'static str, Option<std::ffi::OsString>);
    impl EnvRestore {
        fn set(key: &'static str, value: &str) -> Self {
            let prev = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            EnvRestore(key, prev)
        }
    }
    impl Drop for EnvRestore {
        fn drop(&mut self) {
            match &self.1 {
                Some(v) => unsafe { std::env::set_var(self.0, v) },
                None => unsafe { std::env::remove_var(self.0) },
            }
        }
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
            let id = next_seq_id(&store, &[Collection::Tells], "triage");
            assert_eq!(id, format!("triage-{}", i + 1));
            let t = Tell {
                v: 1,
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
    fn unarchive_restores_the_pair_and_never_pairs_a_stale_answer_with_a_new_ask() {
        let p = Paths::temp();
        let id = ask_detached(&p, "w", "first?", "", &[]).unwrap();
        cmd_answer(&p, &ans(&id, "one", false)).unwrap();
        archive_pair(&p, &id);

        // Plain restore: both halves come back, the pair is resumable again.
        unarchive_pair(&p, &id);
        assert!(p.asks_dir().join(format!("{id}.json")).is_file());
        assert!(p.answers_dir().join(format!("{id}.json")).is_file());
        assert_eq!(answered_detached(&p).len(), 1);

        // Archive again, then let a NEW live ask REUSE the id: unarchive must
        // restore NEITHER half — restoring only the answer would attach the
        // stale "one" to the brand-new question.
        archive_pair(&p, &id);
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
