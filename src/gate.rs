//! Deterministic, judgment-free claim reaping (RULE 2): drop worker leases
//! whose session is no longer alive (crash-safety), so the AI never has to
//! clean up a corpse's lease.
//!
//! Claims are also the loop's mutual-exclusion primitive. `looop _ claim <name>`
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

/// Reject a claim name that could escape claims/ or hit a dotfile.
fn safe_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name.starts_with('.')
        || name == ".."
    {
        bail!("invalid claim name {name:?}");
    }
    Ok(())
}

/// The `.session` recorded in a claim, or empty if absent/unparseable.
fn claim_holder(store: &impl StateStore, key: &Key) -> String {
    store
        .read(key)
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("session").and_then(|x| x.as_str()).map(str::to_owned))
        .unwrap_or_default()
}

/// The session that should own a claim: explicit `--session <id>`, else the
/// worker's exported `$LOOOP_SESSION_ID`. Empty when neither is set.
fn claim_session(args: &crate::cli::ClaimArgs) -> String {
    match &args.session {
        Some(s) if !s.is_empty() => s.clone(),
        _ => std::env::var("LOOOP_SESSION_ID").unwrap_or_default(),
    }
}

/// `looop _ claim <name> [--session <id>]` — atomically acquire the lease for
/// `<name>`. Exit 0 if we now hold it (or already held it), exit 1 if a LIVE
/// session holds it. The acquire is O_EXCL so two racers can't both win; a lease
/// held by a DEAD session is reclaimed. The claim body is `{session,name}`,
/// matching what `sys_claims` surfaces and `reap_stale_claims` reaps.
pub fn cmd_claim(paths: &Paths, args: &crate::cli::ClaimArgs) -> Result<ExitCode> {
    let name = args.name.clone();
    safe_name(&name)?;
    let session = claim_session(args);
    let store = FileStore::new(paths);
    let key = Key::Claim(name.clone());
    let body = serde_json::json!({ "session": session, "name": name }).to_string();

    // Retry a bounded number of times: each iteration is one atomic create-if-absent
    // (O_EXCL via the store); a stale lease is removed and the create retried (the
    // loop only re-runs when we reclaimed a dead holder, so it terminates).
    for _ in 0..8 {
        if store.create_exclusive(&key, &body)? {
            println!("claimed {name}");
            return Ok(ExitCode::SUCCESS);
        }
        // Already held: inspect the holder to decide own / live / reclaim.
        let holder = claim_holder(&store, &key);
        if !holder.is_empty() && holder == session {
            return Ok(ExitCode::SUCCESS); // idempotent: we already own it
        }
        if !holder.is_empty() && session::is_alive(paths, &holder) {
            eprintln!("claim {name}: held by live session '{holder}'");
            return Ok(ExitCode::from(1));
        }
        // Stale (holder empty or dead): reclaim and retry the atomic create.
        let _ = store.remove(&key);
    }
    bail!("claim {name}: contention reclaiming a stale lease");
}

/// `looop _ unclaim <name> [--session <id>]` — release a lease we own. Removes
/// `claims/<name>.json` when it is unowned, owned by us, or held by a DEAD
/// session; refuses (exit 1) only when a DIFFERENT live session holds it.
pub fn cmd_unclaim(paths: &Paths, args: &crate::cli::ClaimArgs) -> Result<ExitCode> {
    let name = args.name.clone();
    safe_name(&name)?;
    let session = claim_session(args);
    let store = FileStore::new(paths);
    let key = Key::Claim(name.clone());
    if !store.exists(&key) {
        return Ok(ExitCode::SUCCESS); // already released (idempotent)
    }
    let holder = claim_holder(&store, &key);
    if holder.is_empty() || holder == session || !session::is_alive(paths, &holder) {
        store.remove(&key)?;
        return Ok(ExitCode::SUCCESS);
    }
    eprintln!("unclaim {name}: held by another live session '{holder}'");
    Ok(ExitCode::from(1))
}

/// Reap claims/<name>.json whose `.session` is no longer alive. Never interprets
/// the claim body — ownership semantics live in the PLAYBOOK.
pub fn reap_stale_claims(paths: &Paths) {
    let store = FileStore::new(paths);
    let alive: Vec<String> = session::list(paths)
        .into_iter()
        .filter(|s| s.alive)
        .map(|s| s.id)
        .collect();

    for name in store.list(&Collection::Claims) {
        let key = Key::Claim(name.clone());
        let sess = claim_holder(&store, &key);
        if sess.is_empty() || !alive.iter().any(|a| a == &sess) {
            let _ = store.remove(&key);
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
        assert_eq!(cmd_claim(&p, &args("repo-q", "w1")).unwrap(), ExitCode::SUCCESS);
        assert!(p.claims_dir().join("repo-q.json").is_file());
        assert!(!p.claims_dir().join("w1.json").exists());
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
