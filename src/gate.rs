//! Two deterministic, judgment-free halves of the controller (RULE 2):
//!   * claim reaping — drop worker leases whose session is no longer alive
//!     (crash-safety), so the AI never has to clean up a corpse's lease;
//!   * the PLAYBOOK approval gate — a change to the guardrail must be
//!     HUMAN-approved before it can steer any tick. Pure file compare/copy.

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

fn bytes_eq(a: &std::path::Path, b: &std::path::Path) -> bool {
    match (fs::read(a), fs::read(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// Before the AI runs: ensure an approved baseline exists and fold any idle/human
/// edit into it (an edit seen while the loop is idle IS the approval).
pub fn playbook_gate_pre(paths: &Paths) {
    let pb = paths.playbook();
    let approved = paths.playbook_approved();
    if !pb.is_file() {
        return;
    }
    if !approved.is_file() {
        let _ = fs::copy(&pb, &approved);
        return;
    }
    if !bytes_eq(&pb, &approved) {
        let _ = fs::copy(&pb, &approved); // human edit between beats: adopt
    }
}

/// After the AI runs: if it touched PLAYBOOK.md, that is a PROPOSAL, not a live
/// change. Park it and restore the approved baseline.
pub fn playbook_gate_post(paths: &Paths) {
    let pb = paths.playbook();
    let approved = paths.playbook_approved();
    if !approved.is_file() || !pb.is_file() {
        return;
    }
    if !bytes_eq(&pb, &approved) {
        let _ = fs::copy(&pb, paths.playbook_proposed()); // park AI's version
        let _ = fs::copy(&approved, &pb); // roll live PLAYBOOK back to baseline
        util::log(&format!(
            "  {}📝 the tick proposed a PLAYBOOK change — parked for approval (looop playbook diff){}",
            util::yel(),
            util::rst()
        ));
        events::emit(paths, "proposal_parked", serde_json::json!({}));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Paths;

    #[test]
    fn pre_seeds_baseline_when_missing() {
        let p = Paths::temp();
        fs::write(p.playbook(), b"v1").unwrap();
        assert!(!p.playbook_approved().is_file());
        playbook_gate_pre(&p);
        assert_eq!(fs::read(p.playbook_approved()).unwrap(), b"v1");
    }

    #[test]
    fn pre_adopts_idle_human_edit() {
        let p = Paths::temp();
        fs::write(p.playbook(), b"v1").unwrap();
        fs::write(p.playbook_approved(), b"v1").unwrap();
        // Human edits the live PLAYBOOK while the loop is idle.
        fs::write(p.playbook(), b"v2-human").unwrap();
        playbook_gate_pre(&p);
        assert_eq!(fs::read(p.playbook_approved()).unwrap(), b"v2-human");
    }

    #[test]
    fn post_parks_ai_proposal_and_rolls_back() {
        let p = Paths::temp();
        fs::write(p.playbook(), b"approved").unwrap();
        fs::write(p.playbook_approved(), b"approved").unwrap();
        // AI rewrote the live PLAYBOOK during the tick.
        fs::write(p.playbook(), b"ai-edit").unwrap();
        playbook_gate_post(&p);
        // Proposal parked, live PLAYBOOK rolled back to the approved baseline.
        assert_eq!(fs::read(p.playbook_proposed()).unwrap(), b"ai-edit");
        assert_eq!(fs::read(p.playbook()).unwrap(), b"approved");
    }

    #[test]
    fn post_is_a_noop_when_unchanged() {
        let p = Paths::temp();
        fs::write(p.playbook(), b"approved").unwrap();
        fs::write(p.playbook_approved(), b"approved").unwrap();
        playbook_gate_post(&p);
        assert!(!p.playbook_proposed().is_file(), "nothing to park");
    }
}
