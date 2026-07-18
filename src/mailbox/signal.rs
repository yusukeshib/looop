//! Signal assembly: the mailbox's view into the world hash.

use super::ask::{Ask, answered_detached, pending};
use crate::paths::Paths;
use crate::util;

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

#[cfg(test)]
mod tests {
    use super::super::test_util::{ans, temp_seeded};
    use super::super::{archive_pair, ask_detached, cmd_answer};
    use super::*;

    #[test]
    fn sys_asks_signal_tracks_the_ask_lifecycle() {
        let p = temp_seeded();
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
}
