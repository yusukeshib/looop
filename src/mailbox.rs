//! ask/answer mailbox — the worker ↔ root-agent question channel.
//!
//! This replaces the old `_ flag` + attach-and-type-into-stdin path. A worker
//! that needs a human/root decision calls `looop _ ask <id> --prompt "…"`, which
//! writes a durable question file under `asks/` and then BLOCKS until a matching
//! `answers/` file appears, printing the answer to stdout. The root agent
//! discovers pending asks (via `looop _ state --json`), decides or relays to the
//! human, and replies with `looop _ answer <ask_id> "…"`.
//!
//! Why files (not stdin / a socket): durability + level-triggering (RULE 2).
//! The mailbox survives a pulse crash, needs no live process to relay, and works
//! for a head-less worker that can't sit at a tmux prompt. The pulse senses a
//! new ask as just another world change and pokes the root agent.

use crate::paths::Paths;
use crate::util;
use anyhow::{Result, bail};
use std::fs;
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

/// Reject an id segment that could escape the mailbox dirs or hit a dotfile.
fn safe(seg: &str) -> Result<()> {
    if seg.is_empty()
        || seg.contains('/')
        || seg.contains('\\')
        || seg.starts_with('.')
        || seg == ".."
    {
        bail!("invalid id {seg:?}");
    }
    Ok(())
}

/// Allocate the next ask id for a worker: `<worker>-<n>` where `n` is one past
/// the highest existing index across BOTH asks/ and answers/ (so an answered
/// ask's id is never reused while its record lingers).
fn next_ask_id(paths: &Paths, worker: &str) -> String {
    let mut max = 0u64;
    for dir in [paths.asks_dir(), paths.answers_dir()] {
        for e in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if let Some(stem) = name.strip_suffix(".json")
                && let Some(idx) = stem.strip_prefix(&format!("{worker}-"))
                && let Ok(n) = idx.parse::<u64>()
            {
                max = max.max(n);
            }
        }
    }
    format!("{worker}-{}", max + 1)
}

/// Read the answer text for an ask id, if it has been answered.
fn read_answer(paths: &Paths, ask_id: &str) -> Option<String> {
    let raw = fs::read_to_string(paths.answers_dir().join(format!("{ask_id}.json"))).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    v.get("answer").and_then(|x| x.as_str()).map(str::to_owned)
}

/// All asks that have NO matching answer yet. Read-only; used by `_ state` (so the
/// root agent sees what's waiting) and by the pulse (so it knows to poke).
pub fn pending(paths: &Paths) -> Vec<Ask> {
    let mut out = Vec::new();
    for e in fs::read_dir(paths.asks_dir())
        .into_iter()
        .flatten()
        .flatten()
    {
        let p = e.path();
        if p.extension().map(|x| x == "json").unwrap_or(false)
            && let Ok(raw) = fs::read_to_string(&p)
            && let Ok(ask) = serde_json::from_str::<Ask>(&raw)
            && read_answer(paths, &ask.id).is_none()
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
/// until the root agent replies, printing the answer to stdout and exiting 0.
/// `<worker>` defaults to `$LOOOP_SESSION_ID` when omitted.
pub fn cmd_ask(paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let mut worker = String::new();
    let mut prompt = String::new();
    let mut reference = String::new();
    let mut options: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--prompt" => prompt = it.next().cloned().unwrap_or_default(),
            "--ref" => reference = it.next().cloned().unwrap_or_default(),
            "--options" => {
                options = it
                    .next()
                    .map(|s| s.split(',').map(|x| x.trim().to_string()).collect())
                    .unwrap_or_default()
            }
            other if !other.starts_with("--") && worker.is_empty() => worker = other.to_string(),
            _ => {}
        }
    }
    if worker.is_empty() {
        worker = std::env::var("LOOOP_SESSION_ID").unwrap_or_default();
    }
    if worker.is_empty() {
        eprintln!("usage: looop _ ask <worker> --prompt \"…\" [--ref PATH] [--options a,b]");
        return Ok(ExitCode::from(1));
    }
    safe(&worker)?;
    if prompt.trim().is_empty() {
        bail!("ask: empty --prompt");
    }

    fs::create_dir_all(paths.asks_dir())?;
    fs::create_dir_all(paths.answers_dir())?;
    let id = next_ask_id(paths, &worker);
    let ask = Ask {
        id: id.clone(),
        worker: worker.clone(),
        prompt: prompt.clone(),
        reference,
        options,
        ts: util::now_unix(),
    };
    fs::write(
        paths.asks_dir().join(format!("{id}.json")),
        serde_json::to_string_pretty(&ask)?,
    )?;
    util::event(
        util::Level::Step,
        "ask",
        &format!("{worker} is waiting: {prompt}"),
        &[
            ("ask_id", serde_json::json!(id)),
            ("worker", serde_json::json!(worker)),
        ],
    );

    // Block until answered. The root agent sees this ask via `looop _ wait`
    // (the pulse keeps the world fresh) and replies via `looop _ answer <id>`.
    let poll = Duration::from_millis(
        std::env::var("LOOOP_ASK_POLL_MS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(1000),
    );
    loop {
        if let Some(answer) = read_answer(paths, &id) {
            println!("{answer}");
            return Ok(ExitCode::SUCCESS);
        }
        std::thread::sleep(poll);
    }
}

/// `looop _ answer <ask_id> <text…>`
///
/// Root-agent callback: resolve a pending ask. Writes `answers/<ask_id>.json`,
/// which unblocks the worker's `_ ask`. Refuses an unknown ask id.
pub fn cmd_answer(paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let Some((ask_id, rest)) = args.split_first() else {
        eprintln!("usage: looop _ answer <ask_id> <text…>");
        return Ok(ExitCode::from(1));
    };
    safe(ask_id)?;
    let text = rest.join(" ");
    if text.trim().is_empty() {
        bail!("answer: empty text");
    }
    if !paths.asks_dir().join(format!("{ask_id}.json")).is_file() {
        bail!("answer: no pending ask {ask_id:?}");
    }
    fs::create_dir_all(paths.answers_dir())?;
    let body = serde_json::json!({ "answer": text, "ts": util::now_unix() });
    fs::write(
        paths.answers_dir().join(format!("{ask_id}.json")),
        serde_json::to_string_pretty(&body)?,
    )?;
    util::event(
        util::Level::Ok,
        "answer",
        &format!("{ask_id}: {text}"),
        &[("ask_id", serde_json::json!(ask_id))],
    );
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ask_ids_increment_and_pending_excludes_answered() {
        let p = Paths::temp();
        fs::create_dir_all(p.asks_dir()).unwrap();
        fs::create_dir_all(p.answers_dir()).unwrap();

        assert_eq!(next_ask_id(&p, "triage"), "triage-1");
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

        assert_eq!(next_ask_id(&p, "triage"), "triage-2");
        assert_eq!(pending(&p).len(), 1, "unanswered ask is pending");

        // Answering it removes it from pending but keeps the id reserved.
        cmd_answer(&p, &["triage-1".into(), "yes".into()]).unwrap();
        assert!(pending(&p).is_empty(), "answered ask is not pending");
        assert_eq!(read_answer(&p, "triage-1").as_deref(), Some("yes"));
        assert_eq!(next_ask_id(&p, "triage"), "triage-2");
    }

    #[test]
    fn answer_refuses_unknown_ask() {
        let p = Paths::temp();
        fs::create_dir_all(p.asks_dir()).unwrap();
        assert!(cmd_answer(&p, &["nope-9".into(), "x".into()]).is_err());
    }
}
