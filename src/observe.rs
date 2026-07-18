//! READ-ONLY world observation — `looop state` / `looop wait`.
//!
//! Everything here is a PURE READ of what the pulse last sensed plus the live
//! mailbox/fleet: no sensing, no mutation, no side effects. It exists so a
//! human or a helper agent (a client) can look at the world — and block until
//! it moves — without ever racing the beat in [`crate::tick`].
//!
//!   * [`state`] / [`cmd_state`] — a structured snapshot of the world (sensor
//!     readings, pending asks, workers, goals, journal tail).
//!   * [`wait_for_change`] / [`cmd_wait`] — block until a category of the world
//!     moves (or an ask is pending / the pulse is down), then report WHICH.

use crate::mailbox;
use crate::paths::Paths;
use crate::store::{Collection, FileStore, Key, StateStore};
use crate::{session, util};
use anyhow::Result;
use std::fs;
use std::process::ExitCode;

/// Read every `snapshots/*.json` into a name→value map (best-effort; unreadable
/// or non-JSON files are skipped). The pulse refreshes these each beat.
fn snapshots(paths: &Paths) -> serde_json::Map<String, serde_json::Value> {
    let mut out = serde_json::Map::new();
    // sorted_glob (not raw read_dir): serde_json::Map is currently a BTreeMap
    // (sorted on its own), but it flips to insertion-ordered the moment ANY
    // dependency enables serde_json's `preserve_order` feature — and consumers
    // ([`fingerprints`], [`state`]) iterate this map, so insertion order must
    // be deterministic regardless of which Map implementation we end up with.
    for p in util::sorted_glob(&paths.snapshots_dir(), "json") {
        if let Some(stem) = p.file_stem().map(|s| s.to_string_lossy().to_string())
            && let Ok(raw) = fs::read_to_string(&p)
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw)
        {
            out.insert(stem, v);
        }
    }
    out
}

fn goal_ids(paths: &Paths) -> Vec<String> {
    // store.list is already sorted.
    FileStore::new(paths).list(&Collection::Goals)
}

fn journal_tail(paths: &Paths, n: usize) -> Vec<String> {
    let text = FileStore::new(paths)
        .read(&Key::Journal)
        .unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..]
        .iter()
        .map(std::string::ToString::to_string)
        .collect()
}

/// The read-only world state a human (or helper agent) consumes. NO sensing, NO
/// mutation: it reads whatever the pulse last sensed plus the live mailbox/fleet.
pub fn state(paths: &Paths) -> serde_json::Value {
    let hash = crate::worldhash::world_hash(paths);

    let asks: Vec<serde_json::Value> = mailbox::pending(paths)
        .into_iter()
        .map(|a| serde_json::to_value(a).unwrap_or_default())
        .collect();

    let workers: Vec<serde_json::Value> = session::list_workers(paths)
        .into_iter()
        .map(|s| {
            serde_json::json!({
                "id": s.id,
                "state": s.state,
                "alive": s.alive,
                "exit_code": s.exit_code,
            })
        })
        .collect();

    serde_json::json!({
        "world_hash": hash,
        // Is the autonomous loop actually running? Without it the snapshots/fleet
        // below are frozen at the last beat, so a client must know the pulse is
        // down before trusting (or waiting on) this state.
        "pulse_alive": crate::run::pulse_running(paths),
        "snapshots": snapshots(paths),
        "asks": asks,
        "workers": workers,
        "goals": goal_ids(paths),
        "journal_tail": journal_tail(paths, 20),
        "data_dir": paths.data_dir.to_string_lossy(),
    })
}

/// Which kinds of change should make `wait` return. The diff is computed per
/// category (see [`fingerprints`]) so a noisy snapshot-only move can be filtered
/// out by a client that only cares about asks / journal progress.
#[derive(Clone, Copy)]
pub(crate) enum WaitFilter {
    /// Wake on ANY category change (default — the historical behavior).
    Any,
    /// Wake only when the pending-asks set changes (`--only-asks`).
    Asks,
    /// Wake only on asks or journal changes (`--actionable`).
    Actionable,
}

/// Per-category content fingerprints, so `wait` can report WHAT changed, not
/// just that the world hash moved. Categories: asks (the pending mailbox),
/// journal, playbook, goals, snapshots (sensors + the live worker fleet).
fn fingerprints(paths: &Paths) -> std::collections::BTreeMap<&'static str, String> {
    let mut m = std::collections::BTreeMap::new();

    let asks: Vec<serde_json::Value> = mailbox::pending(paths)
        .into_iter()
        .map(|a| serde_json::to_value(a).unwrap_or_default())
        .collect();
    m.insert(
        "asks",
        util::content_hash(serde_json::Value::Array(asks).to_string().as_bytes()),
    );
    m.insert(
        "journal",
        util::content_hash(&fs::read(paths.journal()).unwrap_or_default()),
    );
    m.insert(
        "playbook",
        util::content_hash(&fs::read(paths.playbook()).unwrap_or_default()),
    );

    let mut goals = Vec::new();
    for id in goal_ids(paths) {
        goals.extend_from_slice(id.as_bytes());
        goals.push(b'\n');
        goals.extend_from_slice(
            &fs::read(paths.goals_dir().join(format!("{id}.md"))).unwrap_or_default(),
        );
    }
    m.insert("goals", util::content_hash(&goals));

    // Snapshots: only the wake SIGNAL (matching world_hash) so volatile `.detail`
    // never registers as a change. `snapshots()` returns sorted keys.
    let mut snaps = Vec::new();
    for (k, v) in snapshots(paths) {
        snaps.extend_from_slice(k.as_bytes());
        snaps.push(b'\n');
        snaps.extend_from_slice(crate::worldhash::wake_signal(v).to_string().as_bytes());
        snaps.push(b'\n');
    }
    m.insert("snapshots", util::content_hash(&snaps));

    m
}

/// Categories whose fingerprint differs between two snapshots, sorted (BTreeMap).
fn changed_categories(
    base: &std::collections::BTreeMap<&'static str, String>,
    cur: &std::collections::BTreeMap<&'static str, String>,
) -> Vec<String> {
    base.iter()
        .filter(|(k, v)| cur.get(*k) != Some(*v))
        .map(|(k, _)| k.to_string())
        .collect()
}

/// Block until there is something to look at, then return the list of categories
/// that changed. "Something" = a pending ask (return immediately) OR a category
/// move that passes `filter`. Pure read — never senses, so it can't race the pulse.
pub(crate) fn wait_for_change(paths: &Paths, filter: WaitFilter) -> Vec<String> {
    let poll =
        std::time::Duration::from_millis(util::env_knob("LOOOP_WAIT_POLL_MS").unwrap_or(1000));
    // An ask already waiting is actionable for every filter: don't block.
    if !mailbox::pending(paths).is_empty() {
        return vec!["asks".to_string()];
    }
    let baseline = fingerprints(paths);
    let mut last_stamp = probe_stamp(paths);
    // Close the pre-baseline race ONCE, not per poll: an ask that landed
    // between the pending check above and the stamp just taken is baked into
    // BOTH (it would never register as a diff), so re-check the mailbox now.
    // Anything landing AFTER the stamp moves the stamp and is caught below.
    if !mailbox::pending(paths).is_empty() {
        return vec!["asks".to_string()];
    }
    loop {
        // Cheap metadata precheck first: fingerprints() reads + hashes the
        // whole journal/playbook/goals/snapshots and scans the mailbox on
        // every poll, which grows linearly with the data dir. A metadata-only
        // stamp (names + mtimes + sizes) is a handful of stats; only when it
        // moves do we pay for the real content diff.
        let stamp = probe_stamp(paths);
        if stamp != last_stamp {
            last_stamp = stamp;
            // A pending ask is actionable for every filter — an absolute wake
            // condition, not a diff.
            if !mailbox::pending(paths).is_empty() {
                return vec!["asks".to_string()];
            }
            let changed = changed_categories(&baseline, &fingerprints(paths));
            let hit = match filter {
                WaitFilter::Any => !changed.is_empty(),
                WaitFilter::Asks => changed.iter().any(|c| c == "asks"),
                WaitFilter::Actionable => changed.iter().any(|c| c == "asks" || c == "journal"),
            };
            if hit {
                return changed;
            }
        }
        // The pulse is the only thing that drives autonomous change; if it isn't
        // running, these files will never move, so don't block forever — wake the
        // caller with a distinct `pulse-down` signal (filter-independent: a dead
        // loop is critical no matter what a client narrowed its wait to).
        if !crate::run::pulse_running(paths) {
            return vec!["pulse-down".to_string()];
        }
        std::thread::sleep(poll);
    }
}

/// A cheap metadata-only stamp over everything [`fingerprints`] observes:
/// journal + PLAYBOOK (file mtime+len) and every entry of the goals /
/// snapshots / asks / answers dirs (name+mtime+len — creations and deletions
/// move it too). All observable writes go through rename-publish or append, so
/// any content change moves an mtime or a length. (Theoretical gap: a same-
/// length in-place rewrite within one mtime tick on a coarse-granularity
/// filesystem — APFS/ext4 report nanoseconds, and looop's own writers always
/// rename, so this is accepted.)
fn probe_stamp(paths: &Paths) -> String {
    use std::fmt::Write as _;
    let mut buf = String::new();
    let mut stamp_file = |p: &std::path::Path| {
        if let Ok(m) = fs::metadata(p) {
            let mt = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos());
            let _ = writeln!(buf, "{} {mt} {}", p.display(), m.len());
        }
    };
    stamp_file(&paths.journal());
    stamp_file(&paths.playbook());
    for dir in [
        paths.goals_dir(),
        paths.snapshots_dir(),
        paths.asks_dir(),
        paths.answers_dir(),
    ] {
        // read_dir order is filesystem-dependent and NOT guaranteed stable
        // between polls; an order shuffle over unchanged files would move the
        // stamp and waste a full fingerprint recompute. Sort before stamping.
        let mut entries: Vec<_> = fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .collect();
        entries.sort();
        for p in entries {
            stamp_file(&p);
        }
    }
    util::content_hash(buf.as_bytes())
}

/// Render a unix-seconds age as a compact human delta ("just now", "4m", "2h",
/// "3d") so the plain `state` / `wait` output can show how long an ask has
/// been waiting without the caller doing clock math.
fn fmt_ago(ts: u64) -> String {
    let now = util::now_unix();
    let secs = now.saturating_sub(ts);
    if secs < 45 {
        "just now".to_string()
    } else if secs < 90 {
        "1m ago".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

/// First line of `s`, trimmed and clipped to `max` chars (… suffix when cut), so
/// a multi-line ask prompt collapses to a single readable summary line.
fn one_line(s: &str, max: usize) -> String {
    let first = s.lines().next().unwrap_or("").trim();
    if first.chars().count() > max {
        let head: String = first.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    } else {
        first.to_string()
    }
}

/// Print the current state. `--json` = full structured object; else a summary.
/// `changed` (set by `wait`) is surfaced as a `changed: […]` diff summary so a
/// caller knows WHICH categories moved without re-diffing the whole state.
///
/// The plain summary is intentionally rich enough to STAND ALONE: pending asks
/// (with age), the live worker fleet, each sensor's wake signal, and the last
/// few journal lines — so a client woken by `wait` never has to follow up
/// with `tail journal.md` / `state --json | jq` to see what actually moved.
/// Render a state value (from [`crate::contract::Contract::state`] / `wait`) to
/// stdout. A `"changed"` array on the value (present only for `wait`) prints the
/// `changed:` diff line; absent (plain `state`) skips it. PRESENTATION ONLY — the
/// data assembly lives behind the contract, so this is the CLI transport's job.
pub(crate) fn render_state(s: &serde_json::Value, json: bool) -> Result<ExitCode> {
    if json {
        println!("{}", serde_json::to_string_pretty(s)?);
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(ch) = s.get("changed").and_then(|c| c.as_array()) {
        println!(
            "changed: {}",
            if ch.is_empty() {
                "(none)".to_string()
            } else {
                ch.iter()
                    .filter_map(|c| c.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        );
    }
    let asks = s["asks"].as_array().cloned().unwrap_or_default();
    let workers = s["workers"].as_array().cloned().unwrap_or_default();
    let goals = s["goals"].as_array().map_or(0, std::vec::Vec::len);
    let live = workers
        .iter()
        .filter(|w| w["alive"].as_bool().unwrap_or(false))
        .count();
    let pulse_alive = s["pulse_alive"].as_bool().unwrap_or(false);
    println!(
        "pulse: {}  ·  asks: {}  ·  workers: {live} live / {}  ·  goals: {goals}",
        if pulse_alive { "live" } else { "DOWN" },
        asks.len(),
        workers.len()
    );
    if !pulse_alive {
        println!(
            "  ⚠ the autonomous loop is not running — run `looop up` (no beats, snapshots are stale)"
        );
    }

    // Pending asks, each with WHICH worker + HOW LONG it has been waiting, so the
    // freshness of a blocked decision is obvious at a glance.
    for a in &asks {
        let mut head = format!(
            "  ⚑ {} ({} · {}): {}",
            a["id"].as_str().unwrap_or("?"),
            a["worker"].as_str().unwrap_or("?"),
            fmt_ago(a["ts"].as_u64().unwrap_or(0)),
            one_line(a["prompt"].as_str().unwrap_or(""), 100),
        );
        if let Some(r) = a["reference"].as_str().filter(|r| !r.is_empty()) {
            head.push_str(&format!("\n      ref: {r}"));
        }
        if let Some(opts) = a["options"].as_array().filter(|o| !o.is_empty()) {
            let opts: Vec<&str> = opts.iter().filter_map(|o| o.as_str()).collect();
            head.push_str(&format!("\n      options: {}", opts.join(", ")));
        }
        println!("{head}");
    }

    // Sensor readings — one line per snapshot's wake SIGNAL. This is where a
    // user `gh`/PR-review sensor surfaces (e.g. a stale CHANGES_REQUESTED), so
    // a client sees PR state in `state` instead of shelling out to `gh`.
    let snaps = s["snapshots"].as_object().cloned().unwrap_or_default();
    if !snaps.is_empty() {
        println!("sensors:");
        for (k, v) in &snaps {
            let signal = crate::worldhash::wake_signal(v.clone());
            println!("  {k}: {}", one_line(&signal.to_string(), 100));
        }
    }

    // Live workers — id + state, so "who is running" needs no `--json | jq`.
    let alive: Vec<&serde_json::Value> = workers
        .iter()
        .filter(|w| w["alive"].as_bool().unwrap_or(false))
        .collect();
    if !alive.is_empty() {
        println!("workers (live):");
        for w in alive {
            println!(
                "  ● {}  {}",
                w["id"].as_str().unwrap_or("?"),
                w["state"].as_str().unwrap_or("?")
            );
        }
    }

    // Last few journal lines — so a `changed: journal` wake is self-explanatory
    // and the caller never has to `tail journal.md` to learn what looop just did.
    let jtail = s["journal_tail"].as_array().cloned().unwrap_or_default();
    let recent: Vec<&str> = jtail
        .iter()
        .rev()
        .take(3)
        .filter_map(|l| l.as_str())
        .collect();
    if !recent.is_empty() {
        println!("journal (latest):");
        for l in recent.into_iter().rev() {
            println!("  {l}");
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `looop state [--json]` — read the current world state. Pure read: no
/// sensing, no side effects (the autonomous pulse keeps snapshots fresh).
pub fn cmd_state(paths: &Paths, json: bool) -> Result<ExitCode> {
    use crate::contract::Contract;
    let s = crate::contract::LocalContract::new(paths).state()?;
    render_state(&s, json)
}

/// `looop wait [--json] [--only-asks|--actionable]` — BLOCK until there is
/// something to look at, then print the fresh state plus a `changed: […]` diff
/// summary. By default any category move (asks / journal / playbook / goals /
/// snapshots) wakes it; `--actionable` narrows to asks+journal and `--only-asks`
/// to asks alone, so a watching client can ignore noisy snapshot-only moves.
///
/// It also wakes — regardless of filter — with `changed: [pulse-down]` if the
/// autonomous loop isn't running, so a blocked client is never left hanging on a
/// dead pulse (nothing would ever change the files to wake it otherwise).
pub fn cmd_wait(paths: &Paths, args: &crate::cli::WaitArgs) -> Result<ExitCode> {
    let _ = crate::seed::ensure_dirs(paths);
    use crate::contract::Contract;
    let filter = if args.only_asks {
        WaitFilter::Asks
    } else if args.actionable {
        WaitFilter::Actionable
    } else {
        WaitFilter::Any
    };
    let s = crate::contract::LocalContract::new(paths).wait(filter)?;
    render_state(&s, args.json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_reports_goals_and_pending_asks() {
        let p = Paths::temp();
        let _ = crate::seed::ensure_dirs(&p);
        fs::write(p.goals_dir().join("triage.md"), b"triage\n").unwrap();
        fs::write(
            p.asks_dir().join("triage-1.json"),
            serde_json::json!({"id":"triage-1","worker":"triage","prompt":"merge?","ts":1})
                .to_string(),
        )
        .unwrap();

        let s = state(&p);
        let goals: Vec<String> = s["goals"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert!(goals.contains(&"triage".to_string()));
        let asks = s["asks"].as_array().unwrap();
        assert_eq!(asks.len(), 2);
        assert!(asks.iter().any(|a| a["id"] == "setup-1"));
        assert!(asks.iter().any(|a| a["id"] == "triage-1"));
    }

    #[test]
    fn fingerprints_pin_each_category_independently() {
        let p = Paths::temp();
        let _ = crate::seed::ensure_dirs(&p);
        let base = fingerprints(&p);

        // A goal edit moves only the goals category.
        fs::write(p.goals_dir().join("g.md"), b"do the thing\n").unwrap();
        assert_eq!(changed_categories(&base, &fingerprints(&p)), vec!["goals"]);

        // A new pending ask moves only the asks category.
        let after_goal = fingerprints(&p);
        fs::write(
            p.asks_dir().join("w-1.json"),
            serde_json::json!({"id":"w-1","worker":"w","prompt":"ok?","ts":1}).to_string(),
        )
        .unwrap();
        assert_eq!(
            changed_categories(&after_goal, &fingerprints(&p)),
            vec!["asks"]
        );

        // A journal append moves only the journal category.
        let after_ask = fingerprints(&p);
        fs::write(p.journal(), b"progress\n").unwrap();
        assert_eq!(
            changed_categories(&after_ask, &fingerprints(&p)),
            vec!["journal"]
        );
    }

    #[test]
    fn probe_stamp_moves_on_every_observable_write_kind() {
        let p = Paths::temp();
        let _ = crate::seed::ensure_dirs(&p);
        let s0 = probe_stamp(&p);

        // Journal append (length moves even if mtime granularity is coarse).
        fs::write(p.journal(), b"progress\n").unwrap();
        let s1 = probe_stamp(&p);
        assert_ne!(s0, s1, "journal append moves the stamp");

        // A new goal file (dir entry creation).
        fs::write(p.goals_dir().join("g.md"), b"do\n").unwrap();
        let s2 = probe_stamp(&p);
        assert_ne!(s1, s2, "goal creation moves the stamp");

        // A new ask (mailbox dir entry).
        fs::write(
            p.asks_dir().join("w-9.json"),
            serde_json::json!({"id":"w-9","worker":"w","prompt":"ok?","ts":1}).to_string(),
        )
        .unwrap();
        let s3 = probe_stamp(&p);
        assert_ne!(s2, s3, "ask creation moves the stamp");

        // Deletion moves it too.
        fs::remove_file(p.goals_dir().join("g.md")).unwrap();
        assert_ne!(s3, probe_stamp(&p), "goal deletion moves the stamp");

        // No change — stable stamp.
        assert_eq!(
            probe_stamp(&p),
            probe_stamp(&p),
            "unchanged world is stable"
        );
    }

    #[test]
    fn wait_returns_immediately_when_an_ask_is_already_pending() {
        let p = Paths::temp();
        let _ = crate::seed::ensure_dirs(&p);
        fs::write(
            p.asks_dir().join("w-1.json"),
            serde_json::json!({"id":"w-1","worker":"w","prompt":"ok?","ts":1}).to_string(),
        )
        .unwrap();
        // No blocking: a waiting ask is actionable for every filter.
        assert_eq!(wait_for_change(&p, WaitFilter::Asks), vec!["asks"]);
        assert_eq!(wait_for_change(&p, WaitFilter::Actionable), vec!["asks"]);
        assert_eq!(wait_for_change(&p, WaitFilter::Any), vec!["asks"]);
    }

    #[test]
    fn state_reports_pulse_down_when_nothing_holds_the_lock() {
        let p = Paths::temp();
        let _ = crate::seed::ensure_dirs(&p);
        // No pulse has ever acquired the flock in a fresh temp dir.
        assert_eq!(state(&p)["pulse_alive"], serde_json::json!(false));
    }

    #[test]
    fn wait_wakes_with_pulse_down_instead_of_blocking_when_the_loop_is_dead() {
        let p = Paths::temp();
        let _ = crate::seed::ensure_dirs(&p);
        crate::mailbox::answer(&p, "setup-1", "handled", false).unwrap();
        // No ask pending and no pulse running: every filter must wake (not hang)
        // with the distinct pulse-down signal.
        assert_eq!(wait_for_change(&p, WaitFilter::Any), vec!["pulse-down"]);
        assert_eq!(wait_for_change(&p, WaitFilter::Asks), vec!["pulse-down"]);
        assert_eq!(
            wait_for_change(&p, WaitFilter::Actionable),
            vec!["pulse-down"]
        );
    }
}
