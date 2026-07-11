//! ask/answer mailbox — the worker ↔ human question channel.
//!
//! A worker that needs a decision only a HUMAN can make calls `looop _ ask <id>
//! --prompt "…"`, which writes a durable question file under `asks/` and then
//! BLOCKS until a matching `answers/` file appears, printing the answer to stdout.
//! The human answers with `looop _ answer <ask_id> "…"` — directly, or through any
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

/// Every ask raised by `worker` (answered or not), oldest first. Used by the
/// rpc-bridge to weave a worker's ask/answer turns into its own transcript.
pub(crate) fn asks_of(paths: &Paths, worker: &str) -> Vec<Ask> {
    let store = FileStore::new(paths);
    let mut out: Vec<Ask> = store
        .list(&Collection::Asks)
        .into_iter()
        .filter_map(|id| store.read(&Key::Ask(id)))
        .filter_map(|raw| serde_json::from_str::<Ask>(&raw).ok())
        .filter(|a| a.worker == worker)
        .collect();
    out.sort_by_key(|a| a.ts);
    out
}

/// The answer text for `ask_id`, if it has been answered (public sibling of
/// [`read_answer`] for callers outside the mailbox).
pub(crate) fn answer_text(paths: &Paths, ask_id: &str) -> Option<String> {
    read_answer(&FileStore::new(paths), ask_id)
}

/// All asks that have NO matching answer yet. Read-only; used by `_ state` and
/// the decide prompt (so looop sees what's blocked) and by `looop watch` / any
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

/// `looop _ ask <worker> --prompt "…" [--ref PATH] [--options a,b,c]`
///
/// Worker self-callback (CONTRACT). Writes the ask, then BLOCKS polling answers/
/// until the human replies (`looop _ answer`), printing the answer to stdout and
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
        eprintln!("usage: looop _ ask <worker> --prompt \"…\" [--ref PATH] [--options a,b]");
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

/// CONTRACT core for `_ ask`: write the durable ask, then BLOCK polling answers/
/// until the human replies, returning the answer text. Transport-agnostic (no
/// stdout): the CLI presenter prints it; a remote backend would long-poll.
pub(crate) fn ask(
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
    let id = next_ask_id(&store, worker);
    let ask = Ask {
        id: id.clone(),
        worker: worker.to_string(),
        prompt: prompt.to_string(),
        reference: reference.to_string(),
        options: options.to_vec(),
        ts: util::now_unix(),
    };
    store.write_atomic(&Key::Ask(id.clone()), &serde_json::to_string_pretty(&ask)?)?;
    util::event(
        util::Level::Step,
        "ask",
        &format!("{worker} is waiting: {prompt}"),
        &[
            ("ask_id", serde_json::json!(id)),
            ("worker", serde_json::json!(worker)),
        ],
    );

    // Block until answered. The human sees this ask (via `looop watch` / a
    // client / `looop _ state`) and replies with `looop _ answer <id>`.
    let poll = Duration::from_millis(
        std::env::var("LOOOP_ASK_POLL_MS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(1000),
    );
    loop {
        if let Some(answer) = read_answer(&store, &id) {
            return Ok(answer);
        }
        std::thread::sleep(poll);
    }
}

/// `looop _ answer <ask_id> <text…>`
///
/// Root-agent callback: resolve a pending ask. Writes `answers/<ask_id>.json`,
/// which unblocks the worker's `_ ask`. Refuses an unknown ask id.
pub fn cmd_answer(paths: &Paths, args: &crate::cli::AnswerArgs) -> Result<ExitCode> {
    use crate::contract::Contract;
    // Body resolution mirrors `_ goal/sensor/playbook write`: inline words win,
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
    Ok(ExitCode::SUCCESS)
}

/// CONTRACT core for `_ answer`: durably resolve a pending ask. Refuses an
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
    util::event(
        util::Level::Ok,
        "answer",
        &format!("{ask_id}: {text}"),
        &[("ask_id", serde_json::json!(ask_id))],
    );
    Ok(())
}

/// `looop _ asks [--json]` — a client's narrow view: ONLY the pending asks,
/// not the full `_ state` dump (snapshots / journal / fleet). Plain output is a
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
    /// `_ answer <id> <text…> [--force]`.
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
