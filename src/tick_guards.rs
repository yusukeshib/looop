//! The beat's GUARD mechanisms — the pure bookkeeping that bounds a loop's
//! spend and surfaces pathological states, extracted from `tick.rs` (which
//! orchestrates ONE beat) so each guard is readable and testable on its own:
//!
//!   * failure BACKOFF (H1) — exponential wait after consecutive failed beats.
//!     Only a SUCCESSFUL beat resets the counter; a moving world hash does not
//!     (a failing action can move the world every beat). A human steering edit
//!     (PLAYBOOK/goals — the policy hash) cuts the WAIT short without touching
//!     the counter.
//!   * noop TTL — a wrong noop must not park a world state forever; the skip
//!     gate is bypassed once the committed noop ages past the TTL.
//!   * FLAPPING-sensor detection — a volatile `.signal` silently defeats both
//!     the skip gate and the backoff; track per-snapshot change streaks and
//!     flag offenders for the prompt.
//!   * decide-rate CAP — the hard hourly ceiling on decide ATTEMPTS underneath
//!     everything else.
//!
//! All state is small JSON files in the data dir (stateless-process
//! discipline, same as the rest of the beat).

use crate::paths::Paths;
use crate::util::{self, Level};
use crate::{events, worldhash};
use std::fs;
use std::path::PathBuf;

// ---- backoff (H1) -------------------------------------------------------------

/// Exponential-backoff bounds for a repeatedly-failing world state (H1).
const BACKOFF_BASE_SECS: u64 = 60;
const BACKOFF_CAP_SECS: u64 = 3600;

/// Backoff window after `fails` consecutive failed beats:
/// base·2^(fails-1), capped. `fails == 0` => no wait.
pub(crate) fn backoff_delay(fails: u32) -> u64 {
    if fails == 0 {
        return 0;
    }
    let shift = (fails - 1).min(20);
    BACKOFF_BASE_SECS
        .saturating_mul(1u64 << shift)
        .min(BACKOFF_CAP_SECS)
}

fn backoff_path(paths: &Paths) -> PathBuf {
    paths.data_dir.join(".tick-backoff")
}

/// Read backoff state as `(policy_hash, consecutive_fails, last_fail_unix)`.
/// The stored hash is the POLICY hash (PLAYBOOK + goals) as of the last failed
/// beat — a change there means the human steered and the wait may be cut short.
/// `None` when absent/unparseable (no backoff in effect).
pub(crate) fn read_backoff(paths: &Paths) -> Option<(String, u32, u64)> {
    let v: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(backoff_path(paths)).ok()?).ok()?;
    let hash = v.get("hash")?.as_str()?.to_string();
    let fails = v
        .get("fails")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0) as u32;
    let ts = v.get("ts").and_then(serde_json::Value::as_u64).unwrap_or(0);
    Some((hash, fails, ts))
}

pub(crate) fn clear_backoff(paths: &Paths) {
    let _ = fs::remove_file(backoff_path(paths));
}

/// Record a failed attempt; returns the new CONSECUTIVE-fail count. The counter
/// increments on EVERY failure regardless of how the world hash moved — a failing
/// action that mutates the world each beat would otherwise look "new" forever and
/// reset the count, defeating the backoff. Only a SUCCESS ([`clear_backoff`])
/// resets it. `policy` is the current POLICY hash ([`worldhash::policy_hash`]):
/// the wait gate in [`crate::tick`] compares it to the live one so a steering
/// edit (PLAYBOOK/goals) retries promptly, without resetting the counter.
pub(crate) fn record_backoff(paths: &Paths, policy: &str) -> u32 {
    let fails = read_backoff(paths).map_or(1, |(_, n, _)| n.saturating_add(1));
    let body =
        serde_json::json!({ "v": 1, "hash": policy, "fails": fails, "ts": util::now_unix() })
            .to_string();
    if let Err(e) = util::write_atomic(&backoff_path(paths), body.as_bytes()) {
        util::event(
            Level::Warn,
            "tick.guard_degraded",
            &format!("failed to persist backoff state (retry guard degraded): {e}"),
            &[],
        );
    }
    fails
}

// ---- noop TTL (revisit) -------------------------------------------------------

/// How long an unchanged world may coast on a `noop` decision before the beat
/// re-decides anyway. A single wrong noop must not park a world state forever:
/// the skip gate is bypassed once the last decision was a noop older than this.
/// `LOOOP_NOOP_TTL` seconds; 0 disables; default 6h.
pub(crate) fn noop_ttl_secs() -> u64 {
    util::env_knob("LOOOP_NOOP_TTL").unwrap_or(6 * 3600)
}

/// Record that the latest decision was a noop at `hash` (or clear it for any
/// other action — a real move resets the revisit clock).
pub(crate) fn record_noop(paths: &Paths, kind: &str, hash: &str) {
    if kind == "noop" {
        let body = serde_json::json!({ "v": 1, "ts": util::now_unix(), "hash": hash }).to_string();
        let _ = util::write_atomic(&paths.noop_at(), body.as_bytes());
    } else {
        let _ = fs::remove_file(paths.noop_at());
    }
}

/// Whether the skip gate should be BYPASSED: the last decision at this same
/// world hash was a noop, and it has aged past the TTL. Consuming the record
/// (fresh one written after the re-decision) keeps this one-shot per TTL window.
pub(crate) fn noop_revisit_due(paths: &Paths, hash: &str) -> bool {
    let ttl = noop_ttl_secs();
    if ttl == 0 {
        return false;
    }
    let Ok(raw) = fs::read_to_string(paths.noop_at()) else {
        return false;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return false;
    };
    let same = v.get("hash").and_then(|h| h.as_str()) == Some(hash);
    let ts = v.get("ts").and_then(serde_json::Value::as_u64).unwrap_or(0);
    same && util::now_unix().saturating_sub(ts) >= ttl
}

// ---- flapping-sensor detection --------------------------------------------------

/// How many CONSECUTIVE beats a snapshot's wake signal must change before it is
/// flagged as flapping (`LOOOP_FLAP_STREAK`; 0 disables; default 5).
fn flap_streak_threshold() -> u32 {
    util::env_knob("LOOOP_FLAP_STREAK").unwrap_or(5)
}

/// Update the per-snapshot signal-change streaks after a sense, and return the
/// names currently at/over the flapping threshold.
///
/// WHY THIS EXISTS: the loop's entire cost model — "an unchanged world costs no
/// AI call" — hinges on sensor authors correctly splitting volatile fields into
/// `.detail`. A sensor that leaks a timestamp/counter into `.signal` silently
/// defeats the skip gate (the world hash never settles), turning a quiet loop
/// into one decide per beat forever — only the failure backoff and the hourly
/// cap still bound the spend. Nothing else in the system detects that mistake, so
/// the beat tracks it mechanically: a signal that has changed on N consecutive
/// beats is surfaced in the prompt (`FLAPPING SENSORS`) for the decider to fix
/// (move the volatile fields to `.detail`) and warned once when crossing the
/// threshold.
pub(crate) fn update_flap(paths: &Paths) -> Vec<String> {
    let threshold = flap_streak_threshold();
    if threshold == 0 {
        return Vec::new();
    }
    let prev: serde_json::Value = fs::read_to_string(paths.flap_state())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(serde_json::Value::Null);
    let prev_snaps = prev
        .get("snaps")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();

    let mut snaps = serde_json::Map::new();
    let mut flapping = Vec::new();
    for (name, signal) in worldhash::world_items(paths) {
        let Some(name) = name.strip_prefix("snap:") else {
            continue; // policy files are the human's/decider's to edit — not flap
        };
        let streak = match prev_snaps.get(name) {
            Some(e) if e.get("last").and_then(|v| v.as_str()) == Some(signal.as_str()) => 0,
            Some(e) => {
                e.get("streak")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0) as u32
                    + 1
            }
            None => 0, // first sighting — nothing to compare against
        };
        if streak >= threshold {
            flapping.push(name.to_string());
            if streak == threshold {
                util::event(
                    Level::Warn,
                    "sense.flapping",
                    &format!(
                        "{name}: wake signal changed on {streak} consecutive beats — volatile \
                         data is likely leaking into .signal (belongs in .detail); every such \
                         beat costs a decide"
                    ),
                    &[
                        ("sensor", serde_json::json!(name)),
                        ("streak", serde_json::json!(streak)),
                    ],
                );
                events::emit(
                    paths,
                    "sensor_flapping",
                    serde_json::json!({ "sensor": name, "streak": streak }),
                );
            }
        }
        snaps.insert(
            name.to_string(),
            serde_json::json!({ "last": signal, "streak": streak }),
        );
    }
    let body = serde_json::json!({ "v": 1, "snaps": snaps }).to_string();
    if let Err(e) = util::write_atomic(&paths.flap_state(), body.as_bytes()) {
        util::event(
            Level::Warn,
            "tick.guard_degraded",
            &format!("failed to persist the flap ledger (flapping detection degraded): {e}"),
            &[],
        );
    }
    flapping
}

/// The snapshot names currently flagged as flapping (streak at/over the
/// threshold), read from the ledger [`update_flap`] maintains. Consumed by the
/// decide prompt's `FLAPPING SENSORS` section.
pub(crate) fn flapping_sensors(paths: &Paths) -> Vec<String> {
    let threshold = flap_streak_threshold();
    if threshold == 0 {
        return Vec::new();
    }
    let Ok(raw) = fs::read_to_string(paths.flap_state()) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return Vec::new();
    };
    let mut out: Vec<String> = v
        .get("snaps")
        .and_then(|s| s.as_object())
        .map(|m| {
            m.iter()
                .filter(|(_, e)| {
                    e.get("streak")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0)
                        >= threshold as u64
                })
                .map(|(k, _)| k.clone())
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

// ---- decide rate cap (global spend ceiling) --------------------------------------

/// Max decide ATTEMPTS per rolling hour (`LOOOP_MAX_DECIDES_PER_HOUR`; 0
/// disables; default 120). The skip gate and backoff bound a QUIET loop's
/// spend; nothing else bounds a noisy one — cadence nudges can legally reach
/// one decide per 5s (720/h), and a flapping sensor re-arms the beat forever.
/// This is the hard ceiling underneath both: attempts (not successes) count,
/// so failing beats spend budget too.
pub(crate) fn decide_cap_per_hour() -> u64 {
    util::env_knob("LOOOP_MAX_DECIDES_PER_HOUR").unwrap_or(120)
}

pub(crate) fn read_decide_ledger(paths: &Paths) -> Vec<u64> {
    fs::read_to_string(paths.decide_ledger())
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("ts").cloned())
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

/// Whether the hourly decide budget still has room. Returns `Err(retry_in_s)`
/// when exhausted (seconds until the oldest attempt ages out of the window).
pub(crate) fn decide_budget(now: u64, ledger: &[u64], cap: u64) -> Result<(), u64> {
    if cap == 0 {
        return Ok(());
    }
    let recent: Vec<u64> = ledger
        .iter()
        .copied()
        .filter(|t| now.saturating_sub(*t) < 3600)
        .collect();
    if (recent.len() as u64) < cap {
        return Ok(());
    }
    let oldest = recent.iter().copied().min().unwrap_or(now);
    Err((oldest + 3600).saturating_sub(now).max(1))
}

/// Record one decide attempt and prune the ledger to the rolling hour.
pub(crate) fn record_decide(paths: &Paths) {
    let now = util::now_unix();
    let mut ts = read_decide_ledger(paths);
    ts.retain(|t| now.saturating_sub(*t) < 3600);
    ts.push(now);
    let body = serde_json::json!({ "v": 1, "ts": ts }).to_string();
    if let Err(e) = util::write_atomic(&paths.decide_ledger(), body.as_bytes()) {
        util::event(
            Level::Warn,
            "tick.guard_degraded",
            &format!("failed to persist the decide ledger (spend guard degraded): {e}"),
            &[],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decide_budget_blocks_at_cap_and_names_the_retry() {
        // Under cap: fine.
        assert!(decide_budget(1000, &[], 2).is_ok());
        assert!(decide_budget(1000, &[500], 2).is_ok());
        // At cap: blocked until the oldest attempt ages out of the hour.
        let err = decide_budget(1000, &[500, 900], 2).unwrap_err();
        assert_eq!(err, 500 + 3600 - 1000);
        // Old attempts age out of the window.
        assert!(decide_budget(5000, &[500, 900], 2).is_ok());
        // 0 disables.
        assert!(decide_budget(1000, &[1, 2, 3], 0).is_ok());
    }

    #[test]
    fn record_decide_appends_and_prunes_the_rolling_hour() {
        let p = Paths::temp();
        let old = util::now_unix() - 4000;
        fs::write(
            p.decide_ledger(),
            serde_json::json!({ "v": 1, "ts": [old] }).to_string(),
        )
        .unwrap();
        record_decide(&p);
        let ts = read_decide_ledger(&p);
        assert_eq!(ts.len(), 1, "the aged-out attempt was pruned");
        assert!(util::now_unix() - ts[0] < 5);
    }

    #[test]
    fn flapping_is_flagged_after_consecutive_signal_changes_and_resets() {
        let p = Paths::temp();
        fs::create_dir_all(p.snapshots_dir()).unwrap();
        let snap = p.snapshots_dir().join("sensor-noisy.json");
        let write = |n: u64| fs::write(&snap, format!(r#"{{"signal":{{"n":{n}}}}}"#)).unwrap();

        // First sighting establishes a baseline; each subsequent CHANGE bumps
        // the streak. Threshold 5 ⇒ flagged on the 5th consecutive change.
        write(0);
        assert!(update_flap(&p).is_empty());
        for i in 1..=4u64 {
            write(i);
            assert!(update_flap(&p).is_empty(), "streak {i} is below threshold");
        }
        write(5);
        assert_eq!(update_flap(&p), vec!["sensor-noisy".to_string()]);
        assert_eq!(
            flapping_sensors(&p),
            vec!["sensor-noisy".to_string()],
            "the prompt reads the same verdict from the ledger"
        );

        // An unchanged beat resets the streak — a settled sensor is forgiven.
        assert!(update_flap(&p).is_empty());
        assert!(flapping_sensors(&p).is_empty());
    }

    #[test]
    fn backoff_delay_grows_then_caps() {
        assert_eq!(backoff_delay(0), 0);
        assert_eq!(backoff_delay(1), BACKOFF_BASE_SECS);
        assert_eq!(backoff_delay(2), BACKOFF_BASE_SECS * 2);
        assert_eq!(backoff_delay(99), BACKOFF_CAP_SECS);
    }

    #[test]
    fn noop_ttl_bypasses_skip_only_for_an_aged_noop_at_the_same_hash() {
        let p = Paths::temp();
        // No record: never revisit.
        assert!(!noop_revisit_due(&p, "h1"));

        // Fresh noop at h1: not due yet.
        record_noop(&p, "noop", "h1");
        assert!(!noop_revisit_due(&p, "h1"));

        // Age the record past the TTL: due at the SAME hash only.
        let old = util::now_unix() - noop_ttl_secs() - 1;
        fs::write(
            p.noop_at(),
            serde_json::json!({ "ts": old, "hash": "h1" }).to_string(),
        )
        .unwrap();
        assert!(
            noop_revisit_due(&p, "h1"),
            "aged noop at same hash re-decides"
        );
        assert!(
            !noop_revisit_due(&p, "h2"),
            "different world: normal skip rules"
        );

        // A real (non-noop) decision clears the record.
        record_noop(&p, "goal", "h1");
        assert!(!p.noop_at().is_file());
        assert!(!noop_revisit_due(&p, "h1"));
    }

    #[test]
    fn backoff_round_trips_and_clears() {
        let p = Paths::temp();
        let _ = crate::seed::ensure_dirs(&p);
        assert!(read_backoff(&p).is_none());
        assert_eq!(record_backoff(&p, "h"), 1);
        assert_eq!(record_backoff(&p, "h"), 2);
        let (h, n, _) = read_backoff(&p).unwrap();
        assert_eq!((h.as_str(), n), ("h", 2));
        clear_backoff(&p);
        assert!(read_backoff(&p).is_none());
    }
}
