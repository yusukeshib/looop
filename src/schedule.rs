//! Durable time triggers — `schedules/<name>.json`.
//!
//! The world hash deliberately excludes the clock (prompt-cache stability), so
//! "wake me at/every T" needs a first-class, LEVEL-TRIGGERED representation:
//! a schedule is a plain file, and its DUE-ness is fed to the world hash through
//! the `sys-schedules` system sensor — a schedule crossing its due time changes
//! that snapshot's `.signal`, which wakes the loop like any other world change.
//! No in-memory timer exists to lose: a pulse crash/restart re-derives due-ness
//! from the file plus the clock (RULE 2).
//!
//! Two shapes:
//!   * one-shot   `{"at": <unix>}` — signal reads "pending" then "due" (ONE wake;
//!     it stays "due" without re-waking until the decider drops it after
//!     handling — a stable signal is level-triggered, not a spammer).
//!   * recurring  `{"every_s": N, "anchor": <unix>}` — signal carries the period
//!     counter floor((now-anchor)/N), which bumps once per period (one wake per
//!     period, same stability argument).
//!
//! Written by the decider (`write_schedule` / `drop_schedule` typed actions) or
//! a human (`looop schedule …`); both funnel through the same executor path.

use crate::paths::Paths;
use crate::store::{Collection, FileStore, Key, StateStore};
use crate::util;
use anyhow::{Result, bail};

/// Minimum recurring period. A tighter loop than this belongs to the pulse
/// interval / `next_interval_s`, not a durable schedule (and would churn the
/// world hash every beat).
const MIN_EVERY_S: u64 = 60;

/// One parsed schedule file.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Schedule {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub every_s: Option<u64>,
    /// Recurring period origin (set at write time).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub anchor: Option<u64>,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub note: String,
}

/// Create/replace `schedules/<name>.json`. Exactly one of `in_s` (one-shot,
/// relative seconds from now) or `every_s` (recurring period) must be given.
pub fn write(
    paths: &Paths,
    name: &str,
    in_s: Option<u64>,
    every_s: Option<u64>,
    note: &str,
) -> Result<String> {
    util::safe_segment("schedule name", name)?;
    let now = util::now_unix();
    let sched = match (in_s, every_s) {
        (Some(_), Some(_)) | (None, None) => {
            bail!(
                "write_schedule {name:?}: give exactly one of in_s (one-shot) or every_s (recurring)"
            )
        }
        (Some(0), None) => bail!("write_schedule {name:?}: in_s must be > 0"),
        (Some(in_s), None) => Schedule {
            at: Some(now + in_s),
            every_s: None,
            anchor: None,
            note: note.to_string(),
        },
        (None, Some(e)) if e < MIN_EVERY_S => {
            bail!(
                "write_schedule {name:?}: every_s must be >= {MIN_EVERY_S} (use next_interval_s for tighter follow-ups)"
            )
        }
        (None, Some(e)) => Schedule {
            at: None,
            every_s: Some(e),
            anchor: Some(now),
            note: note.to_string(),
        },
    };
    FileStore::new(paths).write_atomic(
        &Key::Schedule(name.to_string()),
        &(serde_json::to_string_pretty(&sched)? + "\n"),
    )?;
    Ok(match (sched.at, sched.every_s) {
        (Some(at), _) => format!(
            "write-schedule {name} (one-shot, due in {}s)",
            at.saturating_sub(now)
        ),
        (_, Some(e)) => format!("write-schedule {name} (recurring, every {e}s)"),
        _ => unreachable!(),
    })
}

/// Remove `schedules/<name>.json` (idempotent).
pub fn drop(paths: &Paths, name: &str) -> Result<String> {
    util::safe_segment("schedule name", name)?;
    FileStore::new(paths).remove(&Key::Schedule(name.to_string()))?;
    Ok(format!("drop-schedule {name}"))
}

/// All schedules, sorted by name, with unparseable files skipped.
pub fn list(paths: &Paths) -> Vec<(String, Schedule)> {
    let store = FileStore::new(paths);
    store
        .list(&Collection::Schedules)
        .into_iter()
        .filter_map(|name| {
            let raw = store.read(&Key::Schedule(name.clone()))?;
            let s: Schedule = serde_json::from_str(&raw).ok()?;
            Some((name, s))
        })
        .collect()
}

/// The wake-signal value of one schedule at `now`, plus its human detail.
/// Signal stability IS the design: it changes exactly when the schedule fires.
fn reading(s: &Schedule, now: u64) -> (serde_json::Value, serde_json::Value) {
    match (s.at, s.every_s) {
        (Some(at), _) => {
            let due = now >= at;
            (
                serde_json::json!(if due { "due" } else { "pending" }),
                serde_json::json!({
                    "kind": "one-shot",
                    "due_in_s": if due { 0 } else { at - now },
                    "note": s.note,
                }),
            )
        }
        (_, Some(every)) => {
            let anchor = s.anchor.unwrap_or(now);
            let period = now.saturating_sub(anchor) / every.max(1);
            (
                serde_json::json!({ "period": period }),
                serde_json::json!({
                    "kind": "recurring",
                    "every_s": every,
                    "next_due_in_s": every - (now.saturating_sub(anchor) % every.max(1)),
                    "note": s.note,
                }),
            )
        }
        _ => (serde_json::json!("invalid"), serde_json::json!({})),
    }
}

/// The `sys-schedules` system-sensor probe: one `{signal,detail}` snapshot over
/// every schedule. A due one-shot / a bumped period changes `.signal`, waking
/// the loop through the same world-hash path as every other sensor.
pub fn sys_schedules(paths: &Paths) -> serde_json::Value {
    let now = util::now_unix();
    let mut signal = serde_json::Map::new();
    let mut detail = serde_json::Map::new();
    for (name, s) in list(paths) {
        let (sig, det) = reading(&s, now);
        signal.insert(name.clone(), sig);
        detail.insert(name, det);
    }
    serde_json::json!({ "signal": signal, "detail": detail })
}

// ---- CLI presenters -------------------------------------------------------------

pub fn cmd_schedule(
    paths: &Paths,
    args: &crate::cli::ScheduleArgs,
) -> Result<std::process::ExitCode> {
    use crate::cli::ScheduleOp;
    match &args.op {
        ScheduleOp::Write {
            name,
            in_s,
            every,
            note,
            journal,
        } => {
            let action = crate::executor::Action::WriteSchedule {
                name: name.clone(),
                in_s: *in_s,
                every_s: *every,
                note: note.clone().unwrap_or_default(),
            };
            let summary = crate::executor::run_action(paths, &action, journal.journal.as_deref())?;
            println!("{summary}");
            Ok(std::process::ExitCode::SUCCESS)
        }
        ScheduleOp::Rm { name, journal } => {
            let action = crate::executor::Action::DropSchedule { name: name.clone() };
            let summary = crate::executor::run_action(paths, &action, journal.journal.as_deref())?;
            println!("{summary}");
            Ok(std::process::ExitCode::SUCCESS)
        }
        ScheduleOp::List { json } => {
            let now = util::now_unix();
            let all = list(paths);
            if *json {
                let v: serde_json::Map<String, serde_json::Value> = all
                    .iter()
                    .map(|(n, s)| (n.clone(), serde_json::to_value(s).unwrap_or_default()))
                    .collect();
                println!("{}", serde_json::Value::Object(v));
            } else if all.is_empty() {
                println!("(no schedules)");
            } else {
                for (name, s) in &all {
                    let (sig, det) = reading(s, now);
                    println!("{name}\t{sig}\t{det}");
                }
            }
            Ok(std::process::ExitCode::SUCCESS)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_shot_signal_flips_to_due_and_stays_stable() {
        let s = Schedule {
            at: Some(1000),
            every_s: None,
            anchor: None,
            note: String::new(),
        };
        assert_eq!(reading(&s, 999).0, serde_json::json!("pending"));
        assert_eq!(reading(&s, 1000).0, serde_json::json!("due"));
        // Stays "due" (one wake, no spam) until dropped.
        assert_eq!(reading(&s, 99999).0, serde_json::json!("due"));
    }

    #[test]
    fn recurring_signal_bumps_once_per_period() {
        let s = Schedule {
            at: None,
            every_s: Some(100),
            anchor: Some(1000),
            note: String::new(),
        };
        assert_eq!(reading(&s, 1000).0, serde_json::json!({"period": 0}));
        assert_eq!(reading(&s, 1099).0, serde_json::json!({"period": 0}));
        assert_eq!(reading(&s, 1100).0, serde_json::json!({"period": 1}));
        assert_eq!(reading(&s, 1350).0, serde_json::json!({"period": 3}));
    }

    #[test]
    fn write_round_trips_and_validates() {
        let p = Paths::temp();
        // one-shot
        write(&p, "digest", Some(3600), None, "daily digest").unwrap();
        let all = list(&p);
        assert_eq!(all.len(), 1);
        assert!(all[0].1.at.is_some());
        // recurring
        write(&p, "poll", None, Some(600), "").unwrap();
        assert_eq!(list(&p).len(), 2);
        // both / neither / too-tight rejected
        assert!(write(&p, "bad", Some(1), Some(60), "").is_err());
        assert!(write(&p, "bad", None, None, "").is_err());
        assert!(write(&p, "bad", None, Some(5), "").is_err());
        assert!(write(&p, "../evil", Some(1), None, "").is_err());
        // drop is idempotent
        drop(&p, "digest").unwrap();
        drop(&p, "digest").unwrap();
        assert_eq!(list(&p).len(), 1);
    }

    #[test]
    fn sys_schedules_reports_signal_and_detail() {
        let p = Paths::temp();
        write(&p, "checkin", Some(999999), None, "follow up").unwrap();
        let v = sys_schedules(&p);
        assert_eq!(v["signal"]["checkin"], serde_json::json!("pending"));
        assert_eq!(v["detail"]["checkin"]["kind"], "one-shot");
        assert_eq!(v["detail"]["checkin"]["note"], "follow up");
    }
}
