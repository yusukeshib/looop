//! The write-ahead INTENT log guarding non-idempotent actions (run_shell),
//! extracted from `executor.rs`: the record written just BEFORE a side effect
//! so a crash DURING it is surfaced next beat (H: a crash between the side
//! effect and the world-hash commit must not silently double-fire), plus the
//! beat-start scan that reports surviving corpses.
//!
//! The executor's [`crate::executor::run_action`] brackets every guarded move
//! with [`begin_intent`] / [`clear_intent`]; the beat's sense phase calls
//! [`warn_if_interrupted`].

use crate::executor::{Action, kind};
use crate::paths::Paths;
use crate::shell_guard::shell_timeout_secs;
use crate::store::{FileStore, Key, StateStore};

/// A stable fingerprint of a non-idempotent action's payload, so a crash report
/// names WHICH command may have half-run. Not used for dedup (the next beat's
/// AI re-decides freshly); purely diagnostic.
fn action_fingerprint(action: &Action) -> String {
    let canon = match action {
        Action::RunShell { cmd, .. } => format!("run_shell\n{cmd}"),
        _ => kind(action).as_str().to_string(),
    };
    crate::util::content_hash(canon.as_bytes())
}

/// One actor's in-flight WAL record: the per-actor key it was written under
/// and the exact serialized body, so [`clear_intent`] can compare-and-delete
/// OUR record and never a concurrent actor's.
pub(crate) struct Intent {
    actor: String,
    body: String,
}

/// One WAL record as read back by the corpse scan. Field-level defaults keep
/// the original hand-parse semantics: a missing/garbled `ts` reads as 0 (i.e.
/// ancient — consumed; safe, see [`warn_one_interrupted`]), a missing
/// `shell_timeout_secs` falls back to the current knob (old-format records),
/// and a missing kind/fingerprint renders as `?`. A record that fails to
/// parse WHOLESALE decodes as `WalRecord::default()` — the same "ancient,
/// consume it" verdict the old `unwrap_or_default()` on the raw Value gave.
#[derive(serde::Deserialize, Default)]
struct WalRecord {
    #[serde(default)]
    ts: u64,
    #[serde(default)]
    shell_timeout_secs: Option<u64>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    fingerprint: Option<String>,
}

/// Write the write-ahead intent record just BEFORE a non-idempotent side effect.
/// If the process dies during the effect, this file survives and is detected by
/// [`warn_if_interrupted`] on the next beat. PER-ACTOR (pid + process-wide
/// nonce): with a single shared key, a concurrent manual `looop run` would
/// silently OVERWRITE the pulse's record and erase its crash-detection
/// guarantee — each actor now writes its own file and [`warn_if_interrupted`]
/// scans them all.
pub(crate) fn begin_intent(paths: &Paths, action: &Action) -> Intent {
    let actor = format!("{}-{}", std::process::id(), crate::util::temp_nonce());
    let body = serde_json::json!({
        "kind": kind(action).as_str(),
        "fingerprint": action_fingerprint(action),
        "ts": crate::util::now_unix(),
        // The deadline THIS run_shell is governed by, frozen at write time:
        // warn_if_interrupted judges corpse-ness against it, and reading the
        // CURRENT knob instead would let an operator's post-crash
        // LOOOP_SHELL_TIMEOUT_SECS edit skew the judgment of a record written
        // under the old value.
        "shell_timeout_secs": shell_timeout_secs(),
    })
    .to_string();
    // Execution still proceeds on a failed WAL write — refusing the move over
    // a bookkeeping failure would be worse — but the degraded crash guard
    // (tick.interrupted detection is OFF for this move) must not be silent.
    if let Err(e) = FileStore::new(paths).write_atomic(&Key::ActionWal(actor.clone()), &body) {
        crate::util::event(
            crate::util::Level::Warn,
            "tick.guard_degraded",
            &format!(
                "failed to write the action WAL (a crash during this move would go undetected): {e}"
            ),
            &[],
        );
    }
    Intent { actor, body }
}

/// Clear the intent record once execute() has returned (Ok OR Err): reaching
/// this line proves the process did not die DURING the side effect, so there is
/// nothing to recover. Only an actual crash between begin/clear leaves it.
///
/// Compare-and-delete on the EXACT body this actor wrote ([`begin_intent`]'s
/// return value): the WAL is a single global key, and a concurrent pulse beat +
/// manual `looop run` would otherwise clear EACH OTHER's intent — the old
/// fingerprint-read-then-remove was check-then-act and could still remove a
/// record that changed between the compare and the remove.
pub(crate) fn clear_intent(paths: &Paths, intent: &Intent) {
    let _ = FileStore::new(paths).remove_if_eq(&Key::ActionWal(intent.actor.clone()), &intent.body);
}

/// At beat start: if any write-ahead intent record survived, its actor died
/// mid non-idempotent side effect (run_shell) before it could commit the world
/// hash. We do NOT auto-retry (a duplicate command is worse than a missed
/// one); we surface it durably so a human can check whether the command
/// actually ran. Scans EVERY per-actor record (plus the legacy pre-per-actor
/// single file), so a crashed manual `looop run` and a crashed pulse are each
/// reported independently. Idempotent. Returns true when at least one
/// interrupted action was found and reported.
pub(crate) fn warn_if_interrupted(paths: &Paths) -> bool {
    let mut any = false;
    for wal in paths.action_wals() {
        if warn_one_interrupted(paths, &wal) {
            any = true;
        }
    }
    any
}

/// Judge ONE surviving WAL record; consume + report it when it is
/// unambiguously a corpse. Returns true when reported.
fn warn_one_interrupted(paths: &Paths, wal: &std::path::Path) -> bool {
    // Operate on the concrete path throughout (never reconstruct a Key from
    // the file name): a foreign/debris name like `.action-wal....json` would
    // decode to an actor the Key layer rejects, turning debris into a
    // panic-per-beat crash loop. The scan already holds the real path — use it.
    let Ok(raw) = std::fs::read_to_string(wal) else {
        return false; // vanished (owner cleared it) or unreadable — retry next beat
    };
    // A YOUNG record may belong to a LIVE actor: a concurrent manual
    // `looop run` can legitimately hold its WAL for up to the run_shell
    // deadline (LOOOP_SHELL_TIMEOUT_SECS). Consuming it here would eat a live
    // run's crash guard — leave it alone until it is unambiguously a corpse
    // (older than the shell deadline plus slack). An unparseable record reads
    // as [`WalRecord::default()`] — ts 0, i.e. ancient — consumed. That
    // immediate-consume path is SAFE: WALs are write_atomic-published (rename,
    // all-or-nothing), so a torn record can never exist on disk — an
    // unparseable body is corrupt/foreign debris, never a live actor's record
    // caught mid-write.
    let v: WalRecord = serde_json::from_str(&raw).unwrap_or_default();
    // Judge against the timeout the record was WRITTEN under (begin_intent
    // freezes it into the WAL): the writer's run_shell was bounded by THAT
    // value, so it — not whatever the knob says now — decides when the record
    // is unambiguously a corpse. Back-compat: an old-format WAL without the
    // field falls back to the current knob (the pre-freeze behavior).
    let timeout = v.shell_timeout_secs.unwrap_or_else(shell_timeout_secs);
    // LOOOP_SHELL_TIMEOUT_SECS=0 means "no run_shell deadline": a LIVE actor
    // can then legitimately hold its WAL for ANY length of time, so there is
    // no age at which the record is unambiguously a crash corpse. Skip the
    // age-based judgment entirely rather than misclassify a live
    // long-running run_shell (grace would collapse to 60s) as interrupted.
    if timeout == 0 {
        return false;
    }
    if crate::util::now_unix().saturating_sub(v.ts) < timeout + 60 {
        return false;
    }
    // One-shot report — plain remove, no compare-and-delete: the record's
    // writer is judged DEAD by its own recorded deadline (a live holder never
    // reaches this line), so there is no owner left to race with. The only
    // concurrent party is another reaper, and the worst outcome of that race
    // is a duplicate report — the same as the CAS path's, without needing to
    // reconstruct a per-actor Key from an (untrusted) file name.
    let _ = std::fs::remove_file(wal);
    let akind = v.kind.as_deref().unwrap_or("?");
    let fp = v.fingerprint.as_deref().unwrap_or("?");
    crate::util::event(
        crate::util::Level::Warn,
        "tick.interrupted",
        &format!(
            "previous beat died mid '{akind}' (a non-idempotent action) before committing \
             — NOT retried automatically; verify it didn't half-run (fp {fp})"
        ),
        &[
            ("action", serde_json::json!(akind)),
            ("fingerprint", serde_json::json!(fp)),
        ],
    );
    crate::events::emit(
        paths,
        "tick_interrupted",
        serde_json::json!({ "action": akind, "fingerprint": fp }),
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warn_if_interrupted_detects_and_clears_a_stale_intent() {
        // Serialize with the env-mutating tests (shell_timeout_zero_disables_*,
        // run_shell_times_out_*): shell_timeout_secs() reads
        // LOOOP_SHELL_TIMEOUT_SECS, and a sibling setting it to 0 mid-test
        // would make warn_if_interrupted take the "no deadline" early-return
        // and false-fail every assertion below. Hold the shared env lock for
        // the whole body and pin the knob to its default so no concurrent
        // test can poison the age math.
        let _env = crate::util::test_env_lock();
        struct Restore;
        impl Drop for Restore {
            fn drop(&mut self) {
                unsafe { std::env::remove_var("LOOOP_SHELL_TIMEOUT_SECS") };
            }
        }
        let _restore = Restore;
        unsafe { std::env::remove_var("LOOOP_SHELL_TIMEOUT_SECS") };
        let p = Paths::temp();
        // A YOUNG intent may belong to a LIVE actor (a manual `looop run`
        // mid-run_shell) — it must be left alone, not eaten every beat.
        let young = begin_intent(
            &p,
            &Action::RunShell {
                cmd: "gh pr comment 1 -b hi".into(),
                reason: String::new(),
            },
        );
        assert_eq!(p.action_wals().len(), 1, "intent written before the effect");
        assert!(
            !warn_if_interrupted(&p),
            "a young WAL may be a live actor's — not reported"
        );
        assert_eq!(p.action_wals().len(), 1, "a young WAL is left alone");
        clear_intent(&p, &young);
        // An OLD intent (past the shell deadline + slack) is a crash corpse:
        // reported once and consumed.
        let old = serde_json::json!({
            "kind": "run_shell",
            "fingerprint": "fp-old",
            "ts": crate::util::now_unix() - (shell_timeout_secs() + 61),
        })
        .to_string();
        FileStore::new(&p)
            .write_atomic(&Key::ActionWal("999-0".into()), &old)
            .unwrap();
        assert!(
            warn_if_interrupted(&p),
            "a leftover intent is reported as an interrupted beat"
        );
        assert!(p.action_wals().is_empty(), "the report is one-shot");
        assert!(!warn_if_interrupted(&p));
    }

    #[test]
    fn legacy_single_file_wal_is_still_reported_after_upgrade() {
        // A pre-per-actor binary that crashed mid run_shell left the old
        // single-key `.action-wal.json`. The scan must still find, report,
        // and consume it — an upgrade must not lose a pending crash report.
        let _env = crate::util::test_env_lock();
        let p = Paths::temp();
        let old = serde_json::json!({
            "kind": "run_shell",
            "fingerprint": "fp-legacy",
            "ts": crate::util::now_unix() - (shell_timeout_secs() + 61),
        })
        .to_string();
        crate::util::write_atomic(&p.data_dir.join(".action-wal.json"), old.as_bytes()).unwrap();
        assert!(warn_if_interrupted(&p), "the legacy corpse is reported");
        assert!(p.action_wals().is_empty(), "…and consumed one-shot");
    }

    #[test]
    fn clear_intent_only_removes_our_exact_record() {
        let p = Paths::temp();
        let ours = Action::RunShell {
            cmd: "echo ours".into(),
            reason: String::new(),
        };
        let theirs = Action::RunShell {
            cmd: "echo theirs".into(),
            reason: String::new(),
        };
        // Regression (single-key WAL): a second actor's begin_intent used to
        // OVERWRITE the first record, silently erasing its crash guard. With
        // per-actor keys both records coexist…
        let our_intent = begin_intent(&p, &ours);
        let their_intent = begin_intent(&p, &theirs);
        assert_eq!(
            p.action_wals().len(),
            2,
            "concurrent actors' intents coexist — no clobbering"
        );
        // …and OUR clear removes only our own record.
        clear_intent(&p, &our_intent);
        assert_eq!(
            p.action_wals().len(),
            1,
            "another actor's intent must not be cleared"
        );
        clear_intent(&p, &their_intent);
        assert!(p.action_wals().is_empty(), "our own intent is cleared");
    }

    #[test]
    fn wal_corpse_judgment_uses_the_recorded_timeout() {
        // The knob the record was WRITTEN under governs — not the current env.
        // No env mutation needed: both cases below discriminate the recorded
        // value from any plausible current knob.
        let p = Paths::temp();
        // Recorded timeout HUGE, age 400s: the current default (300s) would
        // judge this a corpse (400 > 300+60), but the writer's own deadline
        // says it may still be live — left alone.
        let young_under_recorded = serde_json::json!({
            "kind": "run_shell",
            "fingerprint": "fp-recorded",
            "ts": crate::util::now_unix() - 400,
            "shell_timeout_secs": 1_000_000u64,
        })
        .to_string();
        FileStore::new(&p)
            .write_atomic(&Key::ActionWal("111-0".into()), &young_under_recorded)
            .unwrap();
        assert!(
            !warn_if_interrupted(&p),
            "the recorded (large) timeout wins over the current knob"
        );
        assert_eq!(p.action_wals().len(), 1, "the possibly-live record is kept");

        // Recorded timeout TINY, same age: unambiguously a corpse under the
        // writer's deadline even though a big current knob would call it young.
        let corpse_under_recorded = serde_json::json!({
            "kind": "run_shell",
            "fingerprint": "fp-corpse",
            "ts": crate::util::now_unix() - 400,
            "shell_timeout_secs": 1u64,
        })
        .to_string();
        FileStore::new(&p)
            .write_atomic(&Key::ActionWal("222-0".into()), &corpse_under_recorded)
            .unwrap();
        assert!(
            warn_if_interrupted(&p),
            "the recorded (tiny) timeout judges the corpse regardless of the env"
        );
        assert_eq!(
            p.action_wals().len(),
            1,
            "only the corpse is consumed — the live record survives the scan"
        );
    }

    #[test]
    fn fingerprint_is_stable_and_payload_sensitive() {
        let a = Action::RunShell {
            cmd: "echo a".into(),
            reason: "r1".into(),
        };
        let a2 = Action::RunShell {
            cmd: "echo a".into(),
            reason: "r2-ignored".into(),
        };
        let b = Action::RunShell {
            cmd: "echo b".into(),
            reason: "r1".into(),
        };
        assert_eq!(action_fingerprint(&a), action_fingerprint(&a2));
        assert_ne!(action_fingerprint(&a), action_fingerprint(&b));
    }

    #[test]
    fn shell_timeout_zero_disables_the_wal_corpse_judgment() {
        // LOOOP_SHELL_TIMEOUT_SECS=0 = "no deadline": a live run_shell may hold
        // its WAL indefinitely, so age can never prove a crash — even an
        // ancient record must be left alone, not reported.
        let _env = crate::util::test_env_lock();
        struct Restore;
        impl Drop for Restore {
            fn drop(&mut self) {
                unsafe { std::env::remove_var("LOOOP_SHELL_TIMEOUT_SECS") };
            }
        }
        let _restore = Restore;
        unsafe { std::env::set_var("LOOOP_SHELL_TIMEOUT_SECS", "0") };
        let p = Paths::temp();
        let old = serde_json::json!({
            "kind": "run_shell",
            "fingerprint": "fp-ancient",
            "ts": 1, // effectively infinitely old
        })
        .to_string();
        FileStore::new(&p)
            .write_atomic(&Key::ActionWal("333-0".into()), &old)
            .unwrap();
        assert!(
            !warn_if_interrupted(&p),
            "with no shell deadline, no WAL age is unambiguously a corpse"
        );
        assert_eq!(
            p.action_wals().len(),
            1,
            "the record is left alone for the (possibly live) holder"
        );
    }
}
