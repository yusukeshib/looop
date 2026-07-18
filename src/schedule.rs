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

/// Record schema version stamp (absent ⇒ v1).
fn default_v() -> u32 {
    1
}

/// One parsed schedule file.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Schedule {
    /// Record schema version (v1 today; absent ⇒ v1).
    #[serde(default = "default_v")]
    pub v: u32,
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
            v: 1,
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
            v: 1,
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

/// Whether a parsed schedule is structurally INVALID: both or neither of
/// `at`/`every_s`, or a recurring period of 0 (which would bump the world hash
/// every second — a flapping signal). Invalid entries are surfaced (stable
/// "invalid" signal + a Warn event from [`list`]), never silently dropped.
fn is_invalid(s: &Schedule) -> bool {
    match (s.at, s.every_s) {
        (Some(_), None) => false,
        (None, Some(e)) => e == 0,
        _ => true,
    }
}

/// All schedules, sorted by name. An unparseable file or a structurally
/// invalid record is surfaced via a Warn event (naming the file) instead of
/// silently skipped; invalid-but-parseable records are still RETURNED so their
/// stable "invalid" signal reaches the world hash and the decider can fix them.
pub fn list(paths: &Paths) -> Vec<(String, Schedule)> {
    let store = FileStore::new(paths);
    store
        .list(&Collection::Schedules)
        .into_iter()
        .filter_map(|name| {
            let raw = store.read(&Key::Schedule(name.clone()))?;
            let s: Schedule = match serde_json::from_str(&raw) {
                Ok(s) => s,
                Err(e) => {
                    util::event(
                        util::Level::Warn,
                        "schedule.unparseable",
                        &format!("schedules/{name}.json is unparseable ({e}) — it will never fire"),
                        &[("schedule", serde_json::json!(name))],
                    );
                    return None;
                }
            };
            if is_invalid(&s) {
                util::event(
                    util::Level::Warn,
                    "schedule.invalid",
                    &format!(
                        "schedules/{name}.json is invalid (need exactly one of `at`/`every_s`, every_s > 0) — it will never fire"
                    ),
                    &[("schedule", serde_json::json!(name))],
                );
            }
            Some((name, s))
        })
        .collect()
}

/// The wake-signal value of one schedule at `now`, plus its human detail.
/// Signal stability IS the design: it changes exactly when the schedule fires.
fn reading(s: &Schedule, now: u64) -> (serde_json::Value, serde_json::Value) {
    // Structurally invalid records get a STABLE "invalid" signal (never flaps)
    // — list() has already emitted the Warn naming the file.
    if is_invalid(s) {
        return (
            serde_json::json!("invalid"),
            serde_json::json!({ "kind": "invalid", "note": s.note }),
        );
    }
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
            // A hand-written recurring schedule may omit `anchor`. Defaulting
            // it to `now` would pin the period at 0 forever (it NEVER fires);
            // anchor=0 makes it due immediately and the period then advances
            // normally — the schedule self-heals instead of silently dying.
            let anchor = s.anchor.unwrap_or(0);
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
            v: 1,
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
            v: 1,
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
    fn recurring_without_anchor_fires_immediately_and_self_heals() {
        // A hand-written schedule missing `anchor` must not be pinned at
        // period 0 forever (the old `anchor = now` default): anchor defaults
        // to 0, so it is due at once and the period then advances normally.
        let s = Schedule {
            v: 1,
            at: None,
            every_s: Some(100),
            anchor: None,
            note: String::new(),
        };
        let (sig, det) = reading(&s, 1000);
        assert_eq!(sig, serde_json::json!({"period": 10}), "fires immediately");
        assert_eq!(det["kind"], "recurring");
        // …and keeps bumping once per period afterwards.
        assert_eq!(reading(&s, 1100).0, serde_json::json!({"period": 11}));
    }

    #[test]
    fn invalid_schedules_read_as_a_stable_invalid_signal() {
        // every_s == 0 would flap the world hash every second; both-set and
        // neither-set are contradictions. All read as a STABLE "invalid".
        for s in [
            Schedule {
                v: 1,
                at: None,
                every_s: Some(0),
                anchor: Some(0),
                note: String::new(),
            },
            Schedule {
                v: 1,
                at: Some(5),
                every_s: Some(60),
                anchor: None,
                note: String::new(),
            },
            Schedule {
                v: 1,
                at: None,
                every_s: None,
                anchor: None,
                note: String::new(),
            },
        ] {
            assert_eq!(reading(&s, 1000).0, serde_json::json!("invalid"));
            assert_eq!(reading(&s, 99999).0, serde_json::json!("invalid"), "stable");
        }
    }

    #[test]
    fn hand_written_invalid_schedule_is_listed_not_dropped() {
        let p = Paths::temp();
        std::fs::create_dir_all(p.schedules_dir()).unwrap();
        std::fs::write(p.schedules_dir().join("bad.json"), br#"{"every_s": 0}"#).unwrap();
        let all = list(&p);
        assert_eq!(
            all.len(),
            1,
            "invalid entry is surfaced, not silently dropped"
        );
        let v = sys_schedules(&p);
        assert_eq!(v["signal"]["bad"], serde_json::json!("invalid"));
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
