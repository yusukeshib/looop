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
use crate::util;
use anyhow::{Result, bail};
use std::fs;
use std::io::Write;
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

/// The session that should own a claim: explicit `--session <id>`, else the
/// worker's exported `$LOOOP_SESSION_ID`. Empty when neither is set.
fn claim_session(args: &[String]) -> String {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--session" {
            return it.next().cloned().unwrap_or_default();
        }
    }
    std::env::var("LOOOP_SESSION_ID").unwrap_or_default()
}

/// The first positional argument (the claim name), skipping `--session <val>`
/// and any other `--flag` so a flag value is never mistaken for the name.
fn claim_positional(args: &[String]) -> String {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--session" {
            it.next(); // skip its value
            continue;
        }
        if !a.starts_with("--") {
            return a.clone();
        }
    }
    String::new()
}

/// `looop claim <name> [--session <id>]` — atomically acquire the lease for
/// `<name>`. Exit 0 if we now hold it (or already held it), exit 1 if a LIVE
/// session holds it. The acquire is O_EXCL so two racers can't both win; a lease
/// held by a DEAD session is reclaimed. The claim body is `{session,name}`,
/// matching what `sys_claims` surfaces and `reap_stale_claims` reaps.
pub fn cmd_claim(paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let name = claim_positional(args);
    safe_name(&name)?;
    let session = claim_session(args);
    let dir = paths.claims_dir();
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{name}.json"));
    let body = serde_json::json!({ "session": session, "name": name }).to_string();

    // Retry a bounded number of times: each iteration is one atomic create_new;
    // a stale lease is removed and the create retried (the loop only re-runs when
    // we reclaimed a dead holder, so it terminates).
    for _ in 0..8 {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                writeln!(f, "{body}")?;
                println!("claimed {name}");
                return Ok(ExitCode::SUCCESS);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let holder = fs::read_to_string(&path)
                    .ok()
                    .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                    .and_then(|v| v.get("session").and_then(|x| x.as_str()).map(str::to_owned))
                    .unwrap_or_default();
                if !holder.is_empty() && holder == session {
                    return Ok(ExitCode::SUCCESS); // idempotent: we already own it
                }
                if !holder.is_empty() && session::is_alive(paths, &holder) {
                    eprintln!("claim {name}: held by live session '{holder}'");
                    return Ok(ExitCode::from(1));
                }
                // Stale (holder empty or dead): reclaim and retry the atomic create.
                let _ = fs::remove_file(&path);
            }
            Err(e) => return Err(e.into()),
        }
    }
    bail!("claim {name}: contention reclaiming a stale lease");
}

/// `looop unclaim <name> [--session <id>]` — release a lease we own. Removes
/// `claims/<name>.json` when it is unowned, owned by us, or held by a DEAD
/// session; refuses (exit 1) only when a DIFFERENT live session holds it.
pub fn cmd_unclaim(paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let name = claim_positional(args);
    safe_name(&name)?;
    let session = claim_session(args);
    let path = paths.claims_dir().join(format!("{name}.json"));
    if !path.exists() {
        return Ok(ExitCode::SUCCESS); // already released (idempotent)
    }
    let holder = fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("session").and_then(|x| x.as_str()).map(str::to_owned))
        .unwrap_or_default();
    if holder.is_empty() || holder == session || !session::is_alive(paths, &holder) {
        fs::remove_file(&path)?;
        return Ok(ExitCode::SUCCESS);
    }
    eprintln!("unclaim {name}: held by another live session '{holder}'");
    Ok(ExitCode::from(1))
}

/// Reap claims/<name>.json whose `.session` is no longer alive. Never interprets
/// the claim body — ownership semantics live in the PLAYBOOK.
pub fn reap_stale_claims(paths: &Paths) {
    let dir = paths.claims_dir();
    if !dir.is_dir() {
        return;
    }
    let alive: Vec<String> = session::list(paths)
        .into_iter()
        .filter(|s| s.alive)
        .map(|s| s.id)
        .collect();

    for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
        let cf = entry.path();
        if cf.extension().map(|e| e != "json").unwrap_or(true) {
            continue;
        }
        let sess = fs::read_to_string(&cf)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| v.get("session").and_then(|x| x.as_str()).map(str::to_owned))
            .unwrap_or_default();
        if sess.is_empty() || !alive.iter().any(|a| a == &sess) {
            let _ = fs::remove_file(&cf);
            let name = cf
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
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

    fn args(name: &str, sess: &str) -> Vec<String> {
        vec![name.into(), "--session".into(), sess.into()]
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
        // `claim --session w1 repo-q` must claim repo-q, not "w1".
        let a = vec!["--session".into(), "w1".into(), "repo-q".into()];
        assert_eq!(cmd_claim(&p, &a).unwrap(), ExitCode::SUCCESS);
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
