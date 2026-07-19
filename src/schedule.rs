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
    // The summary is built alongside the shape it describes, in the SAME
    // match arm — re-deriving it from the finished Schedule afterwards needed
    // a second match with an `unreachable!()` fourth arm.
    let (sched, summary) = match (in_s, every_s) {
        (Some(_), Some(_)) | (None, None) => {
            bail!(
                "write_schedule {name:?}: give exactly one of in_s (one-shot) or every_s (recurring)"
            )
        }
        (Some(0), None) => bail!("write_schedule {name:?}: in_s must be > 0"),
        // saturating_add: a huge --in value must not overflow (panic in debug,
        // wrap→immediately-due in release) — clamp to "the end of time".
        (Some(in_s), None) => {
            let at = now.saturating_add(in_s);
            (
                Schedule {
                    v: 1,
                    at: Some(at),
                    every_s: None,
                    anchor: None,
                    note: note.to_string(),
                },
                format!(
                    "write-schedule {name} (one-shot, due in {}s)",
                    at.saturating_sub(now)
                ),
            )
        }
        (None, Some(e)) if e < MIN_EVERY_S => {
            bail!(
                "write_schedule {name:?}: every_s must be >= {MIN_EVERY_S} (use next_interval_s for tighter follow-ups)"
            )
        }
        (None, Some(e)) => (
            Schedule {
                v: 1,
                at: None,
                every_s: Some(e),
                anchor: Some(now),
                note: note.to_string(),
            },
            format!("write-schedule {name} (recurring, every {e}s)"),
        ),
    };
    FileStore::new(paths).write_atomic(
        &Key::Schedule(name.to_string()),
        &(serde_json::to_string_pretty(&sched)? + "\n"),
    )?;
    Ok(summary)
}

/// Remove `schedules/<name>.json` (idempotent). Named `remove`, not `drop`
/// (which would shadow `std::mem::drop` for readers of this module).
pub fn remove(paths: &Paths, name: &str) -> Result<String> {
    util::safe_segment("schedule name", name)?;
    FileStore::new(paths).remove(&Key::Schedule(name.to_string()))?;
    Ok(format!("drop-schedule {name}"))
}

/// Whether a parsed schedule is structurally INVALID: both or neither of
/// `at`/`every_s`, or a recurring period below [`MIN_EVERY_S`]. The floor is
/// the SAME one `write()` enforces — a hand-written `every_s` of 1–59 used to
/// pass this read-side check and churn the world hash up to every beat, the
/// exact flapping the write-side floor exists to prevent. Invalid entries are
/// surfaced (stable "invalid" signal, plus a warning from the human-facing
/// `schedule list`), never silently dropped.
fn is_invalid(s: &Schedule) -> bool {
    match (s.at, s.every_s) {
        (Some(_), None) => false,
        (None, Some(e)) => e < MIN_EVERY_S,
        _ => true,
    }
}

/// All schedules, sorted by name. This runs on EVERY beat (via the
/// `sys-schedules` sensor), so it never warns — a broken file would otherwise
/// spam stderr forever. Instead the breakage is carried in-band:
/// invalid-but-parseable records are still RETURNED so their stable "invalid"
/// signal reaches the world hash, and a JSON-BROKEN file is returned as `None`
/// so it too contributes a stable "unparseable" signal — dropping it entirely
/// would remove it from the world hash (one wake, then invisible forever),
/// leaving the decider no level-triggered cue to repair it. The one-time
/// human-facing warning lives in `looop schedule list` ([`warn_broken`]).
pub fn list(paths: &Paths) -> Vec<(String, Option<Schedule>)> {
    let store = FileStore::new(paths);
    store
        .list(&Collection::Schedules)
        .into_iter()
        .map(|name| {
            // A READ failure (permissions, I/O) folds into the same `None` as
            // JSON-broken content: the entry must still be RETURNED so its
            // stable "unparseable" signal reaches the world hash. Dropping it
            // (as an early-return `?` in a filter_map once did) removed the
            // file from the world hash entirely — contradicting the
            // never-silently-dropped contract above.
            let s: Option<Schedule> = store
                .read(&Key::Schedule(name.clone()))
                .and_then(|raw| serde_json::from_str(&raw).ok());
            (name, s)
        })
        .collect()
}

/// Warn (stderr — stdout stays machine-clean for `--json` consumers) about
/// broken schedule entries. Only the human-facing `schedule list` calls this:
/// the per-beat sensor path stays silent and signals the breakage through the
/// stable "invalid"/"unparseable" wake signals instead (see [`list`]).
fn warn_broken(all: &[(String, Option<Schedule>)]) {
    for (name, s) in all {
        match s {
            None => eprintln!("schedules/{name}.json is unparseable — it will never fire"),
            Some(s) if is_invalid(s) => eprintln!(
                "schedules/{name}.json is invalid (need exactly one of `at`/`every_s`, every_s >= {MIN_EVERY_S}) — it will never fire"
            ),
            Some(_) => {}
        }
    }
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
            // `every >= MIN_EVERY_S > 0` is guaranteed here: anything below
            // the floor is structurally invalid and returned above
            // ([`is_invalid`]) — no `.max(1)` divide-by-zero defense needed.
            let elapsed = now.saturating_sub(anchor);
            let period = elapsed / every;
            let rem = elapsed % every;
            (
                serde_json::json!({ "period": period }),
                serde_json::json!({
                    "kind": "recurring",
                    "every_s": every,
                    // rem == 0 is the exact due instant (the period just
                    // bumped): report 0 ("due now"), not a full period — the
                    // next fire is NOT `every` seconds away at the very moment
                    // this one fires.
                    "next_due_in_s": if rem == 0 { 0 } else { every - rem },
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
        let (sig, det) = match &s {
            Some(s) => reading(s, now),
            // A JSON-broken file still contributes a STABLE "unparseable"
            // signal (never flaps, like "invalid") so the decider gets a
            // level-triggered cue to repair or drop it — the file must never
            // just vanish from the world hash.
            None => (
                serde_json::json!("unparseable"),
                serde_json::json!({ "kind": "unparseable" }),
            ),
        };
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
            // The human-facing list is where broken files get their warning —
            // the per-beat sensor path would repeat it forever (see list()).
            warn_broken(&all);
            if *json {
                // An unparseable record serializes as null — present (the human
                // sees the name), visibly broken, machine-distinguishable.
                let v: serde_json::Map<String, serde_json::Value> = all
                    .iter()
                    .map(|(n, s)| (n.clone(), serde_json::to_value(s).unwrap_or_default()))
                    .collect();
                println!("{}", serde_json::Value::Object(v));
            } else if all.is_empty() {
                println!("(no schedules)");
            } else {
                for (name, s) in &all {
                    let (sig, det) = match s {
                        Some(s) => reading(s, now),
                        None => (
                            serde_json::json!("unparseable"),
                            serde_json::json!({ "kind": "unparseable" }),
                        ),
                    };
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
        // every_s below MIN_EVERY_S (0 flaps every second; 1–59 churn up to
        // every beat — the read side must enforce the same floor write()
        // does, or a hand-written file bypasses it); both-set and neither-set
        // are contradictions. All read as a STABLE "invalid".
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
                at: None,
                every_s: Some(MIN_EVERY_S - 1),
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
    fn recurring_next_due_reports_zero_at_the_exact_due_instant() {
        // Regression: at rem == 0 the old `every - rem` reported a FULL
        // period ("due in 100s") at the very moment the schedule fired.
        let s = Schedule {
            v: 1,
            at: None,
            every_s: Some(100),
            anchor: Some(1000),
            note: String::new(),
        };
        assert_eq!(
            reading(&s, 1100).1["next_due_in_s"],
            serde_json::json!(0),
            "due NOW at the period boundary"
        );
        assert_eq!(reading(&s, 1101).1["next_due_in_s"], serde_json::json!(99));
        assert_eq!(reading(&s, 1199).1["next_due_in_s"], serde_json::json!(1));
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_schedule_file_is_surfaced_not_dropped() {
        // Regression: a read FAILURE (not just unparseable JSON) used to be
        // dropped from list() by a `?` inside filter_map — silently removing
        // the file from the world hash, contradicting list()'s own contract.
        use std::os::unix::fs::PermissionsExt;
        let p = Paths::temp();
        std::fs::create_dir_all(p.schedules_dir()).unwrap();
        let f = p.schedules_dir().join("locked.json");
        std::fs::write(&f, br#"{"at": 123}"#).unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Running as root (some CI containers), chmod 000 does not make the
        // read fail — there is nothing to assert then.
        if std::fs::read_to_string(&f).is_ok() {
            return;
        }
        let all = list(&p);
        assert_eq!(all.len(), 1, "read failure is surfaced, not dropped");
        assert!(
            all[0].1.is_none(),
            "an unreadable entry carries no Schedule"
        );
        let v = sys_schedules(&p);
        assert_eq!(v["signal"]["locked"], serde_json::json!("unparseable"));
        // Restore permissions so the temp dir can be cleaned up.
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
    }

    #[test]
    fn hand_written_sub_minimum_every_s_reads_invalid_not_churning() {
        // Regression: a hand-written every_s of 1–59 was accepted on read
        // (only write() enforced the MIN_EVERY_S floor), so its period
        // counter bumped the world hash up to every beat — the exact churn
        // the floor exists to prevent. It must read as a STABLE "invalid".
        let p = Paths::temp();
        std::fs::create_dir_all(p.schedules_dir()).unwrap();
        std::fs::write(
            p.schedules_dir().join("tight.json"),
            br#"{"every_s": 30, "anchor": 0}"#,
        )
        .unwrap();
        let v = sys_schedules(&p);
        assert_eq!(v["signal"]["tight"], serde_json::json!("invalid"));
        let v2 = sys_schedules(&p);
        assert_eq!(v["signal"]["tight"], v2["signal"]["tight"], "stable");
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
    fn unparseable_schedule_contributes_a_stable_unparseable_signal() {
        // A JSON-broken file must not vanish from the world hash (one wake,
        // then invisible forever): it reads as a STABLE "unparseable" signal
        // so the decider gets a level-triggered cue to repair or drop it.
        let p = Paths::temp();
        std::fs::create_dir_all(p.schedules_dir()).unwrap();
        std::fs::write(p.schedules_dir().join("broken.json"), b"{not json").unwrap();
        let all = list(&p);
        assert_eq!(all.len(), 1, "unparseable entry is surfaced, not dropped");
        assert!(all[0].1.is_none(), "unparseable entry carries no Schedule");
        let v = sys_schedules(&p);
        assert_eq!(v["signal"]["broken"], serde_json::json!("unparseable"));
        assert_eq!(v["detail"]["broken"]["kind"], "unparseable");
        // Stability: the signal is identical on the next probe (no flapping).
        let v2 = sys_schedules(&p);
        assert_eq!(v["signal"]["broken"], v2["signal"]["broken"]);
    }

    #[test]
    fn write_with_a_huge_in_s_saturates_instead_of_overflowing() {
        // `now + u64::MAX` would panic in debug / wrap to immediately-due in
        // release; saturating_add clamps to "the end of time" (never due).
        let p = Paths::temp();
        write(&p, "far", Some(u64::MAX), None, "").unwrap();
        let all = list(&p);
        assert_eq!(all[0].1.as_ref().unwrap().at, Some(u64::MAX));
        let v = sys_schedules(&p);
        assert_eq!(v["signal"]["far"], serde_json::json!("pending"));
    }

    #[test]
    fn write_round_trips_and_validates() {
        let p = Paths::temp();
        // one-shot
        write(&p, "digest", Some(3600), None, "daily digest").unwrap();
        let all = list(&p);
        assert_eq!(all.len(), 1);
        assert!(all[0].1.as_ref().unwrap().at.is_some());
        // recurring
        write(&p, "poll", None, Some(600), "").unwrap();
        assert_eq!(list(&p).len(), 2);
        // both / neither / too-tight rejected
        assert!(write(&p, "bad", Some(1), Some(60), "").is_err());
        assert!(write(&p, "bad", None, None, "").is_err());
        assert!(write(&p, "bad", None, Some(5), "").is_err());
        assert!(write(&p, "../evil", Some(1), None, "").is_err());
        // remove is idempotent
        remove(&p, "digest").unwrap();
        remove(&p, "digest").unwrap();
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
