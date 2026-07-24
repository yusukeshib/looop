//! Signal assembly: the mailbox's view into the world hash.

use super::ask::pending;
use crate::paths::Paths;
use crate::util;

/// The `sys-asks` system-sensor probe: makes the mailbox a first-class part of
/// the world hash. The stable signal is the set of pending ask ids, so raising
/// or answering an ask changes it exactly once (level-triggered, no clock in
/// the signal). Volatile context rides in detail.
pub fn sys_asks(paths: &Paths) -> serde_json::Value {
    let now = util::now_unix();
    let pending = pending(paths);
    let detail = pending
        .iter()
        .map(|a| {
            (
                a.id.clone(),
                serde_json::json!({
                    "worker": a.worker,
                    "age_s": now.saturating_sub(a.ts),
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    serde_json::json!({
        "signal": {
            "pending": pending.iter().map(|a| a.id.clone()).collect::<Vec<_>>(),
        },
        "detail": detail,
    })
}

#[cfg(test)]
mod tests {
    use super::super::cmd_answer;
    use super::super::test_util::{ans, temp_seeded};
    use super::*;

    #[test]
    fn sys_asks_signal_tracks_the_ask_lifecycle() {
        let p = temp_seeded();
        let v = sys_asks(&p);
        assert_eq!(v["signal"]["pending"], serde_json::json!([]));

        let id = "w-1";
        std::fs::create_dir_all(p.asks_dir()).unwrap();
        std::fs::write(
            p.asks_dir().join(format!("{id}.json")),
            serde_json::json!({
                "v": 1,
                "id": id,
                "worker": "w",
                "prompt": "q?",
                "ts": 1,
            })
            .to_string(),
        )
        .unwrap();
        let v = sys_asks(&p);
        assert_eq!(v["signal"]["pending"], serde_json::json!([id]));

        cmd_answer(&p, &ans(id, "a", false)).unwrap();
        let v = sys_asks(&p);
        assert_eq!(v["signal"]["pending"], serde_json::json!([]));
    }
}
