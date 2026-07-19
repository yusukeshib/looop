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

use crate::events;
use crate::paths::Paths;
use crate::statefile::{StateRead, read_state};
use crate::util::{self, Level};
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

/// Fail count assumed when the backoff file EXISTS but cannot be parsed (torn
/// write, disk corruption). Restarting at 1 would fail OPEN — a garbled state
/// file silently resets the exponential wait to its minimum right when the
/// loop is already failing. Resuming from a mid-range count keeps the guard
/// conservative (4 fails ⇒ a base·2³ = 480s window) without jumping straight
/// to the cap; a success still clears everything as usual.
const BACKOFF_CORRUPT_FAILS: u32 = 4;

fn backoff_path(paths: &Paths) -> PathBuf {
    paths.data_dir.join(".tick-backoff")
}

/// The three observable states of the backoff file. ABSENT and CORRUPT must
/// stay distinguishable for [`record_backoff`]: squashing corruption into
/// "absent" restarted the fail count at 1 and reset the exponential backoff.
enum BackoffRead {
    Absent,
    Corrupt,
    Parsed(String, u32, u64),
}

/// The typed shape of `.tick-backoff`. `hash` is REQUIRED (a record without
/// the policy hash carries nothing to gate a steering-edit retry on);
/// `fails`/`ts` default to 0 like the old hand-parse. A record whose fields
/// carry the WRONG TYPE now reads as Corrupt wholesale (the old per-field
/// `unwrap_or(0)` silently zeroed it) — strictly MORE conservative, since
/// Corrupt resumes from [`BACKOFF_CORRUPT_FAILS`] instead of fails=0.
#[derive(serde::Deserialize)]
struct BackoffState {
    hash: String,
    #[serde(default)]
    fails: u32,
    #[serde(default)]
    ts: u64,
}

fn read_backoff_raw(paths: &Paths) -> BackoffRead {
    match read_state::<BackoffState>(&backoff_path(paths)) {
        StateRead::Absent => BackoffRead::Absent,
        // Present but unreadable (EACCES/EIO/…): the state file EXISTS, so
        // restarting at 1 would fail OPEN just like a parse error. Treat it as
        // CORRUPT so record_backoff resumes from BACKOFF_CORRUPT_FAILS.
        StateRead::Unreadable(e) => {
            util::event(
                Level::Warn,
                "tick.guard_degraded",
                &format!(
                    "backoff state file is unreadable ({e}) — treating as corrupt so the next \
                     record_backoff resumes conservatively instead of resetting the exponential \
                     backoff"
                ),
                &[],
            );
            BackoffRead::Corrupt
        }
        StateRead::Corrupt(_) => BackoffRead::Corrupt,
        StateRead::Parsed(s) => BackoffRead::Parsed(s.hash, s.fails, s.ts),
    }
}

/// Read backoff state as `(policy_hash, consecutive_fails, last_fail_unix)`.
/// The stored hash is the POLICY hash (PLAYBOOK + goals) as of the last failed
/// beat — a change there means the human steered and the wait may be cut short.
/// `None` only when ABSENT.
///
/// A CORRUPT record fails CLOSED: the old `None` skipped the exponential WAIT
/// entirely (the counter was only repaired on the next failure, so state
/// corruption bought a free full-rate retry). The record carries no usable
/// ts/hash, so we assume `ts = now` and [`BACKOFF_CORRUPT_FAILS`] fails — and
/// REPAIR the file with that well-formed record (stamped with the CURRENT
/// policy hash, so a human steering edit still cuts the wait short). Without
/// the persisted repair, every read would re-assume `ts = now` and the wait
/// would never elapse.
pub(crate) fn read_backoff(paths: &Paths) -> Option<(String, u32, u64)> {
    match read_backoff_raw(paths) {
        BackoffRead::Parsed(hash, fails, ts) => Some((hash, fails, ts)),
        BackoffRead::Absent => None,
        BackoffRead::Corrupt => {
            let policy = crate::worldhash::policy_hash(paths);
            let ts = util::now_unix();
            util::event(
                Level::Warn,
                "tick.guard_degraded",
                &format!(
                    "backoff state file is corrupt — imposing the conservative wait \
                     ({BACKOFF_CORRUPT_FAILS} fails from now) instead of skipping the backoff"
                ),
                &[],
            );
            let body = serde_json::json!({
                "v": 1, "hash": policy, "fails": BACKOFF_CORRUPT_FAILS, "ts": ts
            })
            .to_string();
            if let Err(e) = util::write_atomic(&backoff_path(paths), body.as_bytes()) {
                util::event(
                    Level::Warn,
                    "tick.guard_degraded",
                    &format!("failed to repair the corrupt backoff record: {e}"),
                    &[],
                );
            }
            Some((policy, BACKOFF_CORRUPT_FAILS, ts))
        }
    }
}

pub(crate) fn clear_backoff(paths: &Paths) {
    let _ = fs::remove_file(backoff_path(paths));
}

/// Record a failed attempt; returns the new CONSECUTIVE-fail count. The counter
/// increments on EVERY failure regardless of how the world hash moved — a failing
/// action that mutates the world each beat would otherwise look "new" forever and
/// reset the count, defeating the backoff. Only a SUCCESS ([`clear_backoff`])
/// resets it. `policy` is the current POLICY hash ([`crate::worldhash::policy_hash`]):
/// the wait gate in [`crate::tick`] compares it to the live one so a steering
/// edit (PLAYBOOK/goals) retries promptly, without resetting the counter.
pub(crate) fn record_backoff(paths: &Paths, policy: &str) -> u32 {
    let fails = match read_backoff_raw(paths) {
        BackoffRead::Absent => 1,
        BackoffRead::Parsed(_, n, _) => n.saturating_add(1),
        // Present but unparseable: the count is LOST, not zero. Resuming from
        // a conservative count (instead of 1) keeps the exponential backoff
        // failing CLOSED under state corruption — see BACKOFF_CORRUPT_FAILS.
        BackoffRead::Corrupt => {
            util::event(
                Level::Warn,
                "tick.guard_degraded",
                &format!(
                    "backoff state file is unparseable — resuming from a conservative fail \
                     count ({BACKOFF_CORRUPT_FAILS}) instead of resetting the exponential backoff"
                ),
                &[],
            );
            BACKOFF_CORRUPT_FAILS
        }
    };
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
pub(crate) fn record_noop(paths: &Paths, kind: crate::executor::ActionKind, hash: &str) {
    if kind == crate::executor::ActionKind::Noop {
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
    /// The typed shape of `.noop-at`. `ts` defaults to 0 (an aged, i.e.
    /// revisit-due, record) and `hash` to None (never matches — no bypass),
    /// matching the old hand-parse exactly.
    #[derive(serde::Deserialize)]
    struct NoopState {
        #[serde(default)]
        ts: u64,
        #[serde(default)]
        hash: Option<String>,
    }
    // Absent/unreadable/corrupt all mean "no usable noop record" here — the
    // bypass simply doesn't fire (the safe direction: normal skip rules).
    let StateRead::Parsed(v) = read_state::<NoopState>(&paths.noop_at()) else {
        return false;
    };
    let same = v.hash.as_deref() == Some(hash);
    same && util::now_unix().saturating_sub(v.ts) >= ttl
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
///
/// KNOWN DETECTION GAP (accepted): the ledger below is rebuilt from the
/// snapshots present THIS beat, so an entry whose snapshot is ABSENT for one
/// beat vanishes immediately — an appear/disappear-type flapper (a sensor
/// whose snapshot alternates between existing and not existing) re-baselines
/// at streak 0 on every reappearance and is never flagged, even though each
/// flip moves the world hash and costs a decide. Detecting it would need
/// tombstones for missing entries plus an expiry for genuinely-removed
/// sensors; the hourly decide cap still bounds the spend, so the extra
/// machinery isn't worth it. Documented so the gap is a decision, not a
/// surprise.
pub(crate) fn update_flap(
    paths: &Paths,
    items: &std::collections::BTreeMap<String, String>,
) -> Vec<String> {
    let threshold = flap_streak_threshold();
    if threshold == 0 {
        return Vec::new();
    }
    /// The typed OUTER shape of the flap ledger. The per-snapshot entries stay
    /// `serde_json::Value` deliberately: [`update_flap`] tolerates a corrupt
    /// `last`/`streak` PER ENTRY (re-baseline that one sensor), and a typed
    /// entry struct would escalate one bad entry into whole-ledger corruption.
    #[derive(serde::Deserialize, Default)]
    struct FlapLedger {
        #[serde(default)]
        snaps: serde_json::Map<String, serde_json::Value>,
    }
    // Absent/unreadable/corrupt ledger ⇒ empty baseline (silently, as before:
    // the ledger is advisory and rebuilt every beat).
    let prev_snaps = match read_state::<FlapLedger>(&paths.flap_state()) {
        StateRead::Parsed(l) => l.snaps,
        _ => Default::default(),
    };

    let mut snaps = serde_json::Map::new();
    let mut flapping = Vec::new();
    // `items` are the world items THIS beat sensed ([`crate::tick::sense`]),
    // handed in rather than re-read from disk: a second worldhash pass here
    // would duplicate the IO and could observe a DIFFERENT world than the one
    // the beat is acting on (a snapshot rewritten in between).
    for (name, signal) in items {
        let Some(name) = name.strip_prefix("snap:") else {
            continue; // policy files are the human's/decider's to edit — not flap
        };
        let prev_streak = |e: &serde_json::Value| {
            e.get("streak")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0) as u32
        };
        let streak = match prev_snaps.get(name) {
            // Unchanged beat: DECAY the streak (−1) instead of resetting it
            // to 0. A hard reset let an INTERMITTENT flapper (change, change,
            // still, repeating) hover below the threshold forever while still
            // costing a decide on most beats. Decrement keeps a net upward
            // drift for any pattern that changes more often than it settles
            // (halving would converge BELOW the default threshold of 5 for
            // that same change-change-still pattern). Tradeoff: a genuinely
            // settled sensor now takes `streak` quiet beats to be fully
            // forgiven instead of one — acceptable, since it drops below the
            // threshold (and out of the prompt) after the first quiet beat.
            Some(e) if e.get("last").and_then(|v| v.as_str()) == Some(signal.as_str()) => {
                prev_streak(e).saturating_sub(1)
            }
            // Changed signal — but only a WELL-FORMED prior entry proves a
            // change happened. See the `_` arm for the corrupt case.
            Some(e) if e.get("last").and_then(|v| v.as_str()).is_some() => prev_streak(e) + 1,
            // First sighting, or an entry whose `last` is missing/corrupt:
            // nothing trustworthy to compare against, so re-baseline at 0. A
            // torn ledger must not inflate streaks by counting as a "change".
            _ => 0,
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

/// Read the decide-attempt timestamps. Entries are validated INDIVIDUALLY so
/// one corrupt entry costs only itself: deserializing `ts` straight into
/// `Vec<u64>` blanked the WHOLE ledger on a single non-numeric value, silently
/// resetting the hourly cap (fail-open). Whole-file corruption still empties
/// the ledger (there is nothing to salvage) but WARNS instead of resetting
/// the cap silently. An absent file is a fresh ledger, not corruption.
pub(crate) fn read_decide_ledger(paths: &Paths) -> Vec<u64> {
    // Entries stay `serde_json::Value` (not `u64`) so ONE corrupt entry costs
    // only itself — see the doc above.
    #[derive(serde::Deserialize)]
    struct Ledger {
        #[serde(default)]
        ts: Vec<serde_json::Value>,
    }
    let ledger = match read_state::<Ledger>(&paths.decide_ledger()) {
        // Absent file: a fresh ledger, not corruption — stay quiet.
        StateRead::Absent => return Vec::new(),
        // Present but unreadable (EACCES/EIO/…): the ledger EXISTS, so silently
        // returning empty would fail-open. WARN and restart empty (there is
        // nothing to salvage), matching whole-file corruption discipline.
        StateRead::Unreadable(e) => {
            util::event(
                Level::Warn,
                "tick.guard_degraded",
                &format!("the decide ledger is unreadable ({e}) — the hourly cap restarts empty"),
                &[],
            );
            return Vec::new();
        }
        StateRead::Corrupt(e) => {
            util::event(
                Level::Warn,
                "tick.guard_degraded",
                &format!("the decide ledger is unparseable — the hourly cap restarts empty: {e}"),
                &[],
            );
            return Vec::new();
        }
        StateRead::Parsed(l) => l,
    };
    let ts: Vec<u64> = ledger
        .ts
        .iter()
        .filter_map(serde_json::Value::as_u64)
        .collect();
    if ts.len() != ledger.ts.len() {
        util::event(
            Level::Warn,
            "tick.guard_degraded",
            &format!(
                "the decide ledger has {} non-numeric ts entry(ies) — skipped; the {} valid \
                 attempt(s) still count against the hourly cap",
                ledger.ts.len() - ts.len(),
                ts.len()
            ),
            &[],
        );
    }
    ts
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

    /// Sense the world and update the flap ledger, the way tick() wires them.
    fn flap_beat(p: &Paths) -> Vec<String> {
        update_flap(p, &crate::worldhash::world_items(p))
    }

    #[test]
    fn flapping_is_flagged_after_consecutive_signal_changes_and_decays() {
        let p = Paths::temp();
        fs::create_dir_all(p.snapshots_dir()).unwrap();
        let snap = p.snapshots_dir().join("sensor-noisy.json");
        let write = |n: u64| fs::write(&snap, format!(r#"{{"signal":{{"n":{n}}}}}"#)).unwrap();

        // First sighting establishes a baseline; each subsequent CHANGE bumps
        // the streak. Threshold 5 ⇒ flagged on the 5th consecutive change.
        write(0);
        assert!(flap_beat(&p).is_empty());
        for i in 1..=4u64 {
            write(i);
            assert!(flap_beat(&p).is_empty(), "streak {i} is below threshold");
        }
        write(5);
        assert_eq!(flap_beat(&p), vec!["sensor-noisy".to_string()]);
        assert_eq!(
            flapping_sensors(&p),
            vec!["sensor-noisy".to_string()],
            "the prompt reads the same verdict from the ledger"
        );

        // An unchanged beat DECAYS the streak below the threshold — a settled
        // sensor drops out of the prompt after one quiet beat.
        assert!(flap_beat(&p).is_empty());
        assert!(flapping_sensors(&p).is_empty());
    }

    #[test]
    fn intermittent_flapper_still_reaches_the_threshold() {
        // Regression: reset-to-zero on any single unchanged beat let a
        // change-change-still flapper (two decides out of every three beats)
        // hover below the threshold forever. With decay (−1), the streak
        // drifts +1 net per cycle and eventually crosses it.
        let p = Paths::temp();
        fs::create_dir_all(p.snapshots_dir()).unwrap();
        let snap = p.snapshots_dir().join("sensor-burst.json");
        let write = |n: u64| fs::write(&snap, format!(r#"{{"signal":{{"n":{n}}}}}"#)).unwrap();

        write(0);
        assert!(flap_beat(&p).is_empty(), "baseline");
        let mut n = 0u64;
        let mut flagged = false;
        for _ in 0..10 {
            // Two changed beats…
            for _ in 0..2 {
                n += 1;
                write(n);
                flagged |= !flap_beat(&p).is_empty();
            }
            // …then one still beat (the old hard reset zeroed the streak here).
            flagged |= !flap_beat(&p).is_empty();
        }
        assert!(
            flagged,
            "a sustained change-change-still flapper must eventually be flagged"
        );
    }

    #[test]
    fn flap_entry_with_corrupt_last_counts_as_first_sighting() {
        // Regression: an entry whose `last` key is missing/corrupt used to
        // fall into the streak+1 arm — a torn ledger inflated streaks. It
        // must re-baseline (streak 0) like a first sighting instead.
        let p = Paths::temp();
        fs::create_dir_all(p.snapshots_dir()).unwrap();
        fs::write(
            p.snapshots_dir().join("sensor-x.json"),
            br#"{"signal":{"n":1}}"#,
        )
        .unwrap();
        // Streak one below the threshold, `last` missing entirely: the old
        // +1 arm would flag it this very beat.
        fs::write(
            p.flap_state(),
            serde_json::json!({ "v": 1, "snaps": { "sensor-x": { "streak": 4 } } }).to_string(),
        )
        .unwrap();
        assert!(
            flap_beat(&p).is_empty(),
            "corrupt entry must not be flagged"
        );
        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(p.flap_state()).unwrap()).unwrap();
        assert_eq!(
            v["snaps"]["sensor-x"]["streak"],
            serde_json::json!(0),
            "re-baselined at 0"
        );
    }

    #[test]
    fn update_flap_trusts_the_sensed_items_over_the_live_disk() {
        // Regression for the duplicate-sense TOCTOU: update_flap takes the
        // items the beat SENSED; a snapshot rewritten after the sense must
        // not leak into this beat's ledger.
        let p = Paths::temp();
        fs::create_dir_all(p.snapshots_dir()).unwrap();
        let snap = p.snapshots_dir().join("sensor-t.json");
        fs::write(&snap, br#"{"signal":{"n":1}}"#).unwrap();
        let items = crate::worldhash::world_items(&p);
        // The world moves AFTER the sense…
        fs::write(&snap, br#"{"signal":{"n":2}}"#).unwrap();
        let _ = update_flap(&p, &items);
        // …but the ledger records what was sensed, not what is on disk now.
        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(p.flap_state()).unwrap()).unwrap();
        assert_eq!(
            v["snaps"]["sensor-t"]["last"],
            serde_json::json!(r#"{"n":1}"#)
        );
    }

    #[test]
    fn corrupt_backoff_state_imposes_the_conservative_wait_and_repairs_the_record() {
        // Regression (two generations): a present-but-unparseable backoff file
        // first reset the fail count to 1 (fail-open), then — after the count
        // repair — still SKIPPED the exponential wait until the next failure
        // rewrote the record. It must now fail fully CLOSED: the read itself
        // imposes a conservative window (ts = now, BACKOFF_CORRUPT_FAILS
        // fails) and persists the repaired record so the wait actually
        // elapses instead of being re-assumed from "now" every beat.
        let p = Paths::temp();
        let _ = crate::seed::ensure_dirs(&p);
        fs::write(p.data_dir.join(".tick-backoff"), b"{not json").unwrap();
        let (hash, fails, ts) = read_backoff(&p).expect("corrupt reads as a conservative record");
        assert_eq!(
            fails, BACKOFF_CORRUPT_FAILS,
            "the wait is imposed, not skipped"
        );
        assert!(
            util::now_unix().saturating_sub(ts) <= 2,
            "ts is assumed 'now' so the window starts from the corruption sighting"
        );
        assert_eq!(
            hash,
            crate::worldhash::policy_hash(&p),
            "the repair stamps the CURRENT policy hash so a steering edit still cuts the wait"
        );
        // The repair is PERSISTED (well-formed on disk now): a next failure
        // increments normally instead of re-triggering the corrupt path.
        let (h2, f2, _) = read_backoff(&p).expect("the repaired record round-trips");
        assert_eq!((h2, f2), (hash, BACKOFF_CORRUPT_FAILS));
        assert_eq!(record_backoff(&p, "h"), BACKOFF_CORRUPT_FAILS + 1);
    }

    #[test]
    fn record_backoff_on_a_corrupt_record_resumes_conservatively() {
        // record_backoff hits a still-corrupt file only when no read_backoff
        // repaired it first (e.g. a failure recorded outside the beat's wait
        // gate). It must resume from the conservative count, not restart at 1.
        let p = Paths::temp();
        let _ = crate::seed::ensure_dirs(&p);
        fs::write(p.data_dir.join(".tick-backoff"), b"{not json").unwrap();
        assert_eq!(
            record_backoff(&p, "h"),
            BACKOFF_CORRUPT_FAILS,
            "resume from the conservative count, not 1"
        );
        // The rewritten (now well-formed) record increments normally again.
        assert_eq!(record_backoff(&p, "h"), BACKOFF_CORRUPT_FAILS + 1);
    }

    #[test]
    fn decide_ledger_skips_corrupt_entries_but_keeps_valid_ones() {
        // Regression: one non-numeric ts entry used to blank the WHOLE ledger,
        // resetting the hourly decide cap (fail-open).
        let p = Paths::temp();
        fs::write(
            p.decide_ledger(),
            serde_json::json!({ "v": 1, "ts": [100, "garbage", 200, -1] }).to_string(),
        )
        .unwrap();
        assert_eq!(
            read_decide_ledger(&p),
            vec![100, 200],
            "valid entries survive a corrupt sibling"
        );
        // Whole-file corruption: nothing to salvage — empty (but warned).
        fs::write(p.decide_ledger(), b"{not json").unwrap();
        assert!(read_decide_ledger(&p).is_empty());
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
        record_noop(&p, crate::executor::ActionKind::Noop, "h1");
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
        record_noop(&p, crate::executor::ActionKind::Goal, "h1");
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
