//! Deterministic, judgment-free claim reaping (RULE 2): drop worker leases
//! whose session is no longer alive (crash-safety), so the AI never has to
//! clean up a corpse's lease.

use crate::babysit;
use crate::events;
use crate::paths::Paths;
use crate::util;
use std::fs;

/// Reap claims/<name>.json whose `.session` is no longer alive. Never interprets
/// the claim body — ownership semantics live in the PLAYBOOK.
pub fn reap_stale_claims(paths: &Paths) {
    let dir = paths.claims_dir();
    if !dir.is_dir() {
        return;
    }
    let alive: Vec<String> = babysit::list()
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
            util::log(&format!(
                "  {}reaped stale claim {} (session '{}' not alive){}",
                util::dim(),
                name,
                if sess.is_empty() { "?" } else { &sess },
                util::rst()
            ));
            events::emit(
                paths,
                "claim_reaped",
                serde_json::json!({ "claim": name, "session": sess }),
            );
        }
    }
}
