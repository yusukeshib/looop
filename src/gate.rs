//! Deterministic, judgment-free claim reaping (RULE 2): drop worker leases
//! whose session is no longer alive (crash-safety), so the AI never has to
//! clean up a corpse's lease.
//!
//! Claims are also the loop's mutual-exclusion primitive. `looop claim <name>`
//! is an ATOMIC, liveness-aware test-and-set: it creates `claims/<name>.json`
//! with O_EXCL and FAILS if a LIVE session already holds it, so two workers
//! racing for the same resource (e.g. a repo) can't both "win" the way the old
//! advisory `printf > file` allowed. A stale lease (holder dead) is reclaimed.

use crate::events;
use crate::paths::Paths;
use crate::session;
use crate::store::{Collection, FileStore, Key, StateStore};
use crate::util;
use anyhow::{Result, bail};
use std::process::ExitCode;

/// How long an EMPTY claim file is treated as "in flight" before it is stale
/// debris. Our own writers can never leave an empty file (create_exclusive
/// publishes complete contents via rename), so an empty claim is foreign or
/// crash debris — but give a foreign writer a short grace window (mtime-based)
/// before removing it, so a mid-write old binary isn't instantly stolen from.
const EMPTY_CLAIM_GRACE_SECS: u64 = 10;

/// The `.session` recorded in a claim body, or empty if unparseable.
fn holder_of(raw: &str) -> String {
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|v| v.get("session").and_then(|x| x.as_str()).map(str::to_owned))
        .unwrap_or_default()
}

/// The session that should own a claim: explicit `--session <id>`, else the
/// worker's exported `$LOOOP_SESSION_ID`. Empty when neither is set.
fn session_or_env(session: Option<&str>) -> String {
    match session {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => std::env::var("LOOOP_SESSION_ID").unwrap_or_default(),
    }
}

/// `looop claim <name> [--session <id>]` — atomically acquire the lease for
/// `<name>`. Exit 0 if we now hold it (or already held it), exit 1 if a LIVE
/// session holds it. The acquire is O_EXCL so two racers can't both win; a lease
/// held by a DEAD session is reclaimed. The claim body is `{session,name}`,
/// matching what `sys_claims` surfaces and `reap_stale_claims` reaps.
pub fn cmd_claim(paths: &Paths, args: &crate::cli::ClaimArgs) -> Result<ExitCode> {
    use crate::contract::{ClaimOutcome, Contract};
    let name = &args.name;
    match crate::contract::LocalContract::new(paths).claim(name, args.session.as_deref())? {
        ClaimOutcome::Won => {
            println!("claimed {name}");
            Ok(ExitCode::SUCCESS)
        }
        ClaimOutcome::AlreadyOwned => Ok(ExitCode::SUCCESS), // idempotent
        ClaimOutcome::HeldByLive(holder) => {
            eprintln!("claim {name}: held by live session '{holder}'");
            Ok(ExitCode::from(1))
        }
    }
}

/// CONTRACT core for `claim`: an atomic, liveness-aware test-and-set on the
/// named lease. Transport-agnostic — returns a typed [`ClaimOutcome`] the
/// presenter maps to an exit code. `session` is the explicit owner, else the
/// worker's exported `$LOOOP_SESSION_ID`.
pub(crate) fn claim(
    paths: &Paths,
    name: &str,
    session: Option<&str>,
) -> Result<crate::contract::ClaimOutcome> {
    use crate::contract::ClaimOutcome;
    util::safe_segment("claim name", name)?;
    let session = session_or_env(session);
    // An EMPTY owner would void the mutual exclusion silently: the lease's
    // holder can never be judged alive, so anyone (including the reaper)
    // instantly reclaims it — both racers "win". Refuse instead of granting a
    // lock that locks nothing.
    if session.is_empty() {
        bail!(
            "claim {name}: no session id — pass --session <id> or run inside a \
             worker (where $LOOOP_SESSION_ID is exported)"
        );
    }
    let store = FileStore::new(paths);
    let key = Key::Claim(name.to_string());
    let body = serde_json::json!({ "session": session, "name": name }).to_string();

    // Retry a bounded number of times: each iteration is one atomic create-if-absent
    // (exclusive-create via the store); a stale lease is reclaimed via COMPARE-
    // AND-DELETE (remove only if the contents still match what we read — so a
    // lease FRESHLY re-acquired between our read and our delete is never stolen)
    // and the create retried.
    for _ in 0..8 {
        if store.create_exclusive(&key, &body)? {
            return Ok(ClaimOutcome::Won);
        }
        // Already held: inspect the holder to decide own / live / reclaim.
        let Some(raw) = store.read(&key) else {
            continue; // vanished between create and read — just retry the create
        };
        if raw.is_empty() {
            // create_exclusive guarantees "exists ⇒ contents complete", so an
            // empty file is foreign/crash debris. Treat it as in-flight only
            // while YOUNG; once past the grace window it would wedge this
            // claim name FOREVER (nobody ever fills it in) — remove it (the
            // compare-and-delete keys on the empty contents, so a real lease
            // written meanwhile survives) and retry the create.
            if store
                .age_secs(&key)
                .is_some_and(|a| a < EMPTY_CLAIM_GRACE_SECS)
            {
                std::thread::sleep(std::time::Duration::from_millis(25));
            } else {
                let _ = store.remove_if_eq(&key, "");
            }
            continue;
        }
        let holder = holder_of(&raw);
        if !holder.is_empty() && holder == session {
            return Ok(ClaimOutcome::AlreadyOwned);
        }
        if !holder.is_empty() && session::is_alive(paths, &holder) {
            return Ok(ClaimOutcome::HeldByLive(holder));
        }
        // Stale (holder unparseable or dead): compare-and-delete, then retry
        // the atomic create. A `false` (someone re-acquired) just loops — the
        // next iteration re-inspects the fresh holder.
        let _ = store.remove_if_eq(&key, &raw);
    }
    bail!("claim {name}: contention reclaiming a stale lease");
}

/// `looop unclaim <name> [--session <id>]` — release a lease we own. Removes
/// `claims/<name>.json` when it is unowned, owned by us, or held by a DEAD
/// session; refuses (exit 1) only when a DIFFERENT live session holds it.
pub fn cmd_unclaim(paths: &Paths, args: &crate::cli::ClaimArgs) -> Result<ExitCode> {
    use crate::contract::Contract;
    let name = &args.name;
    if crate::contract::LocalContract::new(paths).unclaim(name, args.session.as_deref())? {
        Ok(ExitCode::SUCCESS)
    } else {
        eprintln!("unclaim {name}: held by another live session");
        Ok(ExitCode::from(1))
    }
}

/// CONTRACT core for `unclaim`: release a lease we may own. `Ok(true)` when the
/// lease is now gone (unowned, ours, or a dead holder — all idempotent);
/// `Ok(false)` when a DIFFERENT live session holds it. Transport-agnostic.
pub(crate) fn unclaim(paths: &Paths, name: &str, session: Option<&str>) -> Result<bool> {
    util::safe_segment("claim name", name)?;
    let session = session_or_env(session);
    let store = FileStore::new(paths);
    let key = Key::Claim(name.to_string());
    let Some(raw) = store.read(&key) else {
        return Ok(true); // already released (idempotent)
    };
    let holder = holder_of(&raw);
    if holder.is_empty() || holder == session || !session::is_alive(paths, &holder) {
        // Compare-and-delete: only remove the lease we actually inspected — a
        // lease FRESHLY acquired by someone else after our read stays intact.
        return match store.remove_if_eq(&key, &raw)? {
            true => Ok(true),
            // Contents changed underneath us: someone else now holds it.
            false => Ok(false),
        };
    }
    Ok(false)
}

/// Reap claims/<name>.json whose `.session` is no longer alive. The reaper
/// reads exactly ONE field of the claim body — `.session`, the liveness
/// anchor (via [`holder_of`]; an unparseable body reads as an empty holder
/// and is therefore treated as stale once past the empty-claim grace
/// handling below). Everything else in the body is opaque here — ownership
/// SEMANTICS (what a claim means, who should hold it) live in the PLAYBOOK.
pub fn reap_stale_claims(paths: &Paths) {
    let store = FileStore::new(paths);
    let alive: Vec<String> = session::list(paths)
        .into_iter()
        .filter(|s| s.alive)
        .map(|s| s.id)
        .collect();

    for name in store.list(&Collection::Claims) {
        let key = Key::Claim(name.clone());
        let Some(raw) = store.read(&key) else {
            continue;
        };
        if raw.is_empty() {
            // "exists ⇒ contents complete" holds for our own creates, so an
            // empty file is foreign/crash debris. Skip it while YOUNG (a
            // foreign writer may still be mid-write); once past the grace
            // window remove it, or the name is wedged forever.
            if store
                .age_secs(&key)
                .is_some_and(|a| a >= EMPTY_CLAIM_GRACE_SECS)
                && store.remove_if_eq(&key, &raw).unwrap_or(false)
            {
                util::event(
                    util::Level::Info,
                    "claim.reaped",
                    &format!("reaped stale empty claim {name} (crash debris)"),
                    &[("claim", serde_json::json!(name))],
                );
                events::emit(
                    paths,
                    "claim_reaped",
                    serde_json::json!({ "claim": name, "session": "" }),
                );
            }
            continue;
        }
        let sess = holder_of(&raw);
        if sess.is_empty() || !alive.iter().any(|a| a == &sess) {
            // The `alive` snapshot was taken ONCE, before this sweep: a worker
            // that started and claimed DURING the sweep would be misjudged
            // dead and its LIVE lease reaped. Re-check this holder's liveness
            // individually, immediately before removal — the same per-claim
            // check claim() itself uses — so the snapshot is only a cheap
            // first-pass filter, never the final verdict.
            if !sess.is_empty() && session::is_alive(paths, &sess) {
                continue;
            }
            // Compare-and-delete: never reap a lease that was freshly re-
            // acquired between our read and this delete.
            if !store.remove_if_eq(&key, &raw).unwrap_or(false) {
                continue;
            }
            util::event(
                util::Level::Info,
                "claim.reaped",
                &format!(
                    "reaped stale claim {name} (session '{}' not alive)",
                    if sess.is_empty() { "?" } else { &sess }
                ),
                &[
                    ("claim", serde_json::json!(name)),
                    ("session", serde_json::json!(sess)),
                ],
            );
            events::emit(
                paths,
                "claim_reaped",
                serde_json::json!({ "claim": name, "session": sess }),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn args(name: &str, sess: &str) -> crate::cli::ClaimArgs {
        crate::cli::ClaimArgs {
            name: name.into(),
            session: Some(sess.into()),
        }
    }

    #[test]
    fn claim_creates_lease_and_is_idempotent_for_owner() {
        let p = Paths::temp();
        assert_eq!(
            cmd_claim(&p, &args("repo-x", "w1")).unwrap(),
            ExitCode::SUCCESS
        );
        let path = p.claims_dir().join("repo-x.json");
        assert!(path.is_file());
        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["session"], "w1");
        assert_eq!(v["name"], "repo-x");
        // The owner re-claiming is an idempotent success, not an error.
        assert_eq!(
            cmd_claim(&p, &args("repo-x", "w1")).unwrap(),
            ExitCode::SUCCESS
        );
    }

    #[test]
    fn claim_reclaims_a_stale_lease_from_a_dead_holder() {
        let p = Paths::temp();
        fs::create_dir_all(p.claims_dir()).unwrap();
        // A lease from a session that isn't alive (no real babysit session here).
        fs::write(
            p.claims_dir().join("repo-y.json"),
            br#"{"session":"dead","name":"repo-y"}"#,
        )
        .unwrap();
        assert_eq!(
            cmd_claim(&p, &args("repo-y", "w2")).unwrap(),
            ExitCode::SUCCESS
        );
        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(p.claims_dir().join("repo-y.json")).unwrap())
                .unwrap();
        assert_eq!(v["session"], "w2", "a dead holder's lease is reclaimed");
    }

    #[test]
    fn unclaim_removes_owned_and_is_idempotent() {
        let p = Paths::temp();
        cmd_claim(&p, &args("repo-z", "w1")).unwrap();
        assert_eq!(
            cmd_unclaim(&p, &args("repo-z", "w1")).unwrap(),
            ExitCode::SUCCESS
        );
        assert!(!p.claims_dir().join("repo-z.json").exists());
        // Releasing again is a no-op success.
        assert_eq!(
            cmd_unclaim(&p, &args("repo-z", "w1")).unwrap(),
            ExitCode::SUCCESS
        );
    }

    #[test]
    fn claim_name_after_session_flag_is_not_the_flag_value() {
        let p = Paths::temp();
        // `claim --session w1 repo-q` must claim repo-q, not "w1" (clap binds the
        // positional `name` distinctly from the `--session` value).
        assert_eq!(
            cmd_claim(&p, &args("repo-q", "w1")).unwrap(),
            ExitCode::SUCCESS
        );
        assert!(p.claims_dir().join("repo-q.json").is_file());
        assert!(!p.claims_dir().join("w1.json").exists());
    }

    #[test]
    fn reclaim_is_compare_and_delete_never_stealing_a_fresh_lease() {
        // The mechanism claim()/unclaim()/reap use: remove_if_eq only deletes
        // the lease the caller actually INSPECTED — a fresh lease written
        // between the read and the delete survives.
        let p = Paths::temp();
        let store = crate::store::FileStore::new(&p);
        let key = crate::store::Key::Claim("repo-r".into());
        store
            .create_exclusive(&key, r#"{"session":"dead","name":"repo-r"}"#)
            .unwrap();
        let observed = store.read(&key).unwrap();
        // A racer reclaims first and writes its FRESH lease…
        store.remove(&key).unwrap();
        store
            .create_exclusive(&key, r#"{"session":"fresh","name":"repo-r"}"#)
            .unwrap();
        // …so our delete (keyed to the stale contents) must lose.
        assert!(!store.remove_if_eq(&key, &observed).unwrap());
        let v: serde_json::Value = serde_json::from_str(&store.read(&key).unwrap()).unwrap();
        assert_eq!(v["session"], "fresh", "the fresh lease was not stolen");
    }

    #[test]
    fn empty_claim_file_is_in_flight_only_while_young() {
        // A FRESH empty file may be a foreign writer mid-write: claim() retries
        // then reports contention instead of instantly stealing, and the
        // reaper skips it.
        let p = Paths::temp();
        fs::create_dir_all(p.claims_dir()).unwrap();
        let path = p.claims_dir().join("repo-e.json");
        fs::write(&path, b"").unwrap();
        assert!(
            claim(&p, "repo-e", Some("w9")).is_err(),
            "a young empty holder is retried then surfaced as contention, never stolen"
        );
        reap_stale_claims(&p);
        assert!(
            path.is_file(),
            "the reaper must not reap a young (possibly in-flight) empty claim"
        );
    }

    #[test]
    fn old_empty_claim_file_is_stale_debris_and_is_reclaimed() {
        // An empty file OLDER than the grace window can never be completed
        // (rename-published creates are all-or-nothing) — leaving it would
        // wedge the claim name forever. claim() removes it and wins…
        let p = Paths::temp();
        fs::create_dir_all(p.claims_dir()).unwrap();
        let path = p.claims_dir().join("repo-o.json");
        let age = |path: &std::path::Path| {
            let old = std::time::SystemTime::now()
                - std::time::Duration::from_secs(EMPTY_CLAIM_GRACE_SECS + 5);
            fs::OpenOptions::new()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(old)
                .unwrap();
        };
        fs::write(&path, b"").unwrap();
        age(&path);
        assert!(matches!(
            claim(&p, "repo-o", Some("w9")).unwrap(),
            crate::contract::ClaimOutcome::Won
        ));
        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["session"], "w9", "the stale debris was reclaimed");

        // …and the reaper removes an old empty file outright.
        let path2 = p.claims_dir().join("repo-p.json");
        fs::write(&path2, b"").unwrap();
        age(&path2);
        reap_stale_claims(&p);
        assert!(
            !path2.exists(),
            "the reaper removes empty crash debris past the grace window"
        );
    }

    #[test]
    fn claim_with_an_empty_session_is_refused() {
        // No --session and no $LOOOP_SESSION_ID: the lease would have an empty
        // holder that is never "alive", so anyone could instantly reclaim it —
        // mutual exclusion silently void. Refuse loudly instead.
        let _env = crate::util::test_env_lock();
        struct Restore(Option<std::ffi::OsString>);
        impl Drop for Restore {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(v) => unsafe { std::env::set_var("LOOOP_SESSION_ID", v) },
                    None => unsafe { std::env::remove_var("LOOOP_SESSION_ID") },
                }
            }
        }
        let _restore = Restore(std::env::var_os("LOOOP_SESSION_ID"));
        unsafe { std::env::remove_var("LOOOP_SESSION_ID") };
        let p = Paths::temp();
        for sess in [None, Some("")] {
            let err = claim(&p, "repo-anon", sess).unwrap_err();
            assert!(
                err.to_string().contains("--session"),
                "the refusal tells the user how to fix it: {err}"
            );
        }
        assert!(
            !p.claims_dir().join("repo-anon.json").exists(),
            "no empty-holder lease is ever written"
        );
    }

    #[test]
    fn claim_rejects_unsafe_names() {
        let p = Paths::temp();
        for bad in ["", "..", "a/b", ".hidden"] {
            assert!(
                cmd_claim(&p, &args(bad, "w1")).is_err(),
                "should reject {bad:?}"
            );
        }
    }
}
