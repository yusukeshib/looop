//! tells — the human → worker steering channel.

use super::common::{default_v, warn_future_v, warn_once, write_new_record};
use crate::paths::Paths;
use crate::store::{Collection, FileStore, Key, StateStore};
use crate::util;
use anyhow::{Result, bail};
use std::process::ExitCode;

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
            warn_future_v("tells", &id, &raw);
            match serde_json::from_str::<Tell>(&raw) {
                Ok(t) => Some(t),
                Err(e) => {
                    // Visible, not silent: a dropped tell is lost steering.
                    // STDERR, not stdout: `looop told` prints the drained
                    // tells on stdout and that stream IS the text a worker
                    // consumes — a warning there would be read as steering.
                    // Once per record per process (polled via `told`).
                    warn_once(
                        format!("tell-unparseable:{id}"),
                        &format!("tells/{id}.json is unparseable ({e}) — record ignored"),
                    );
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
///
/// CLAIM-BY-RENAME, then deliver — the same atomic-claim idiom the archive
/// paths use. The old remove-then-return shape had two holes: a crash after
/// the remove silently LOST the steering, and two concurrent drains (a `told`
/// poll racing an ask-answer piggyback) could both read a tell before either
/// removed it — double-delivery. rename(2) makes exactly one claimant win each
/// tell (the loser's rename fails NotFound and it delivers nothing), and a
/// claimed tell lands under `tells/consumed/` instead of vanishing, so a crash
/// between claim and delivery leaves an auditable record rather than silence.
///
/// Semantics chosen: AT-MOST-ONCE. Steering must never be applied twice (a
/// duplicated "abort X" could abort the retry too), so the claim happens
/// BEFORE the return; the residual claimed-but-not-delivered crash window is
/// accepted and made VISIBLE under consumed/ rather than eliminated —
/// exactly-once would need an ack from the worker, a protocol the mailbox
/// deliberately doesn't have.
pub fn drain_tells(paths: &Paths, worker: &str) -> Vec<String> {
    let consumed = paths.tells_dir().join("consumed");
    prune_consumed_tells(&consumed);
    let mut out = Vec::new();
    for t in pending_tells(paths, worker) {
        if std::fs::create_dir_all(&consumed).is_err() {
            // Can't build the claim destination — leave the tell pending (it
            // stays deliverable later) rather than falling back to a lossy
            // remove.
            continue;
        }
        // Unique destination per claim: tell ids are REUSED after consumption
        // (next_seq_id scans only the live dir), so a fixed consumed/<id>.json
        // would clobber the audit record of a previous generation.
        let to = consumed.join(format!(
            "{}.{}.{}.json",
            t.id,
            std::process::id(),
            util::temp_nonce()
        ));
        // The rename IS the claim: only the winner delivers this tell. A tell
        // is written once via create_exclusive and never rewritten, so the
        // body read by pending_tells above cannot have changed under us.
        if std::fs::rename(paths.tells_dir().join(format!("{}.json", t.id)), &to).is_ok() {
            out.push(format!("• {}", t.msg));
        }
    }
    out
}

/// Best-effort retention sweep for `tells/consumed/`: every delivered tell
/// leaves an audit record there (see [`drain_tells`]) and nothing else ever
/// removes them, so without a sweep the dir grows WITHOUT BOUND — the same
/// failure class the events.jsonl / journal.md size caps guard against. Age
/// is the right axis here (the records exist for post-mortem audit, and an
/// old one has served its purpose): entries older than
/// `LOOOP_TELLS_CONSUMED_KEEP_SECS` (default 7 days; 0 = keep forever) are
/// removed by mtime. Runs at drain time — the only writer of consumed/ — so
/// the sweep piggybacks on exactly the path that causes the growth. A sweep
/// failure must never fail (or even slow) a delivery, hence all-ignore.
fn prune_consumed_tells(consumed: &std::path::Path) {
    let keep: u64 =
        crate::util::env_knob("LOOOP_TELLS_CONSUMED_KEEP_SECS").unwrap_or(7 * 24 * 3600);
    // checked_sub: an absurd knob (u64::MAX) would underflow SystemTime and
    // panic — treat it as "keep forever", same spirit as the ask deadline.
    let Some(cutoff) = (keep > 0)
        .then(|| std::time::SystemTime::now().checked_sub(std::time::Duration::from_secs(keep)))
        .flatten()
    else {
        return;
    };
    for e in std::fs::read_dir(consumed).into_iter().flatten().flatten() {
        let aged = e
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| t < cutoff);
        if aged {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Drop every pending tell for `worker` UNDELIVERED. Called when a worker
/// corpse is removed (reap / prune), after a kill, and just before a new
/// worker reuses the id: worker ids ARE goal ids and get reused across generations,
/// and a tell addressed to a dead generation must never be delivered to its
/// successor (via `told` or an ask-answer piggyback) — it steered a worker
/// with a different brief. Plain remove, not rename-claim: nothing here
/// delivers, so there is no double-delivery race; a concurrent drain that
/// claims one first was, by definition, still the live generation's drain.
pub(crate) fn discard_tells(paths: &Paths, worker: &str) {
    let store = FileStore::new(paths);
    for t in pending_tells(paths, worker) {
        let _ = store.remove(&Key::Tell(t.id));
    }
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
    // try_is_alive, not the lenient is_alive: an enumeration failure used to
    // collapse to "dead", and the refusal below would send the operator
    // chasing a worker that may be perfectly alive. Distinguish the two — an
    // unreadable fleet is a retryable condition, not evidence of death.
    match crate::session::try_is_alive(paths, &args.worker) {
        Ok(true) => {}
        Ok(false) => bail!(
            "tell {}: not a live worker (a dead worker can never read it — steer via goals or a fresh worker)",
            args.worker
        ),
        Err(e) => bail!(
            "tell {}: cannot enumerate the fleet ({e}) — cannot confirm the worker is alive; retry",
            args.worker
        ),
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
    let worker = super::common::session_or_env(args.worker.as_deref());
    if worker.is_empty() {
        eprintln!("usage: looop told [worker]  (or run inside a worker with $LOOOP_SESSION_ID)");
        return Ok(ExitCode::from(1));
    }
    for line in drain_tells(paths, &worker) {
        println!("{line}");
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::super::common::next_seq_id;
    use super::*;

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

    /// Write one tell file directly (the shape `cmd_tell` produces), as if it
    /// was queued while a worker with this id was alive.
    fn queue_tell(p: &Paths, worker: &str, n: u64, msg: &str) {
        let store = FileStore::new(p);
        let id = format!("{worker}-{n}");
        let t = Tell {
            v: 1,
            id: id.clone(),
            worker: worker.into(),
            msg: msg.into(),
            ts: n,
        };
        store
            .write_atomic(&Key::Tell(id), &serde_json::to_string(&t).unwrap())
            .unwrap();
    }

    #[test]
    fn stale_tells_are_discarded_and_never_reach_a_reincarnated_worker() {
        // Regression: worker ids are goal ids and get REUSED. A tell queued
        // for a worker that then died used to survive its corpse (reap/prune/
        // kill never touched tells/) and be delivered to the NEXT worker
        // started under the same id. The lifecycle paths (session::reap,
        // session::prune_aged, session::cmd_kill, and the pre-spawn point of
        // cmd_start_session) now call discard_tells at the generation
        // boundary; this pins the helper they share. (Exercised at this level
        // because starting a real worker in a unit test would spawn a real
        // babysit supervisor process.)
        let p = Paths::temp();
        queue_tell(&p, "triage", 1, "stale steering for the DEAD generation");
        assert_eq!(pending_tells(&p, "triage").len(), 1);
        // …worker dies, corpse pruned / id about to be reused…
        discard_tells(&p, "triage");
        // …the new generation must start with a CLEAN tell queue.
        assert!(
            drain_tells(&p, "triage").is_empty(),
            "a tell for a dead previous generation must never be delivered"
        );
        assert!(
            !p.tells_dir().join("consumed").is_dir()
                || std::fs::read_dir(p.tells_dir().join("consumed"))
                    .unwrap()
                    .next()
                    .is_none(),
            "discard drops the record outright — it was never delivered, so it must not \
             masquerade as consumed"
        );
    }

    #[test]
    fn concurrent_drains_deliver_each_tell_exactly_once() {
        // Regression for the read-then-remove drain: a `told` poll racing an
        // ask-answer piggyback could both read a tell before either removed
        // it. With claim-by-rename exactly one drain wins each tell.
        let p = Paths::temp();
        queue_tell(&p, "w", 1, "first");
        queue_tell(&p, "w", 2, "second");
        let (a, b) = std::thread::scope(|scope| {
            let a = scope.spawn(|| drain_tells(&p, "w"));
            let b = scope.spawn(|| drain_tells(&p, "w"));
            (a.join().unwrap(), b.join().unwrap())
        });
        let mut all: Vec<String> = a.into_iter().chain(b).collect();
        all.sort();
        assert_eq!(
            all,
            vec!["• first", "• second"],
            "each tell is delivered exactly once across racing drains"
        );
        assert!(pending_tells(&p, "w").is_empty(), "nothing left pending");
        assert_eq!(
            std::fs::read_dir(p.tells_dir().join("consumed"))
                .unwrap()
                .count(),
            2,
            "every delivered tell leaves an audit record under consumed/"
        );
    }

    #[test]
    fn consumed_tell_audit_records_are_swept_past_the_retention_window() {
        // Regression guard for unbounded growth: drain_tells leaves one audit
        // file under tells/consumed/ per delivery and nothing else removes
        // them — the drain-time sweep must reap records older than the
        // retention window while leaving fresh ones alone.
        let p = Paths::temp();
        queue_tell(&p, "w", 1, "deliver me");
        assert_eq!(drain_tells(&p, "w").len(), 1);
        let consumed = p.tells_dir().join("consumed");
        let entry = std::fs::read_dir(&consumed)
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        // A fresh record survives a drain (the sweep is age-based).
        assert!(drain_tells(&p, "w").is_empty());
        assert_eq!(
            std::fs::read_dir(&consumed).unwrap().count(),
            1,
            "a fresh audit record is kept"
        );
        // Age it past the default 7-day retention; the next drain sweeps it
        // (even with nothing pending — the sweep runs unconditionally).
        std::fs::File::open(entry.path())
            .unwrap()
            .set_modified(
                std::time::SystemTime::now() - std::time::Duration::from_secs(8 * 24 * 3600),
            )
            .unwrap();
        assert!(drain_tells(&p, "w").is_empty());
        assert_eq!(
            std::fs::read_dir(&consumed).unwrap().count(),
            0,
            "an aged audit record is swept"
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
}
