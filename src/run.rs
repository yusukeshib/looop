//! The pulse (`looop pulse`) — looop's AUTONOMOUS control loop.
//!
//! Each beat: sense the world, and — when it changed since last beat — hand it to
//! the configured `tick` runner for ONE move, which looop executes through the
//! typed [`crate::executor`] actions (RULE 1: one tick = one move). Judgment
//! lives HERE, in looop; the human is a peer who steers by editing goals/PLAYBOOK
//! and answers worker questions via the ask/answer mailbox (surfaced by a
//! client — the human-facing interface, not a decision-maker).
//!
//! It is a single-instance loop (flock) and the SOLE senser/decider, so two beats
//! never wipe `snapshots/` or decide under each other. An unchanged world skips
//! the AI entirely, so a quiet loop is nearly free.

use crate::config::Config;
use crate::paths::Paths;
use crate::util::Level;
use crate::{seed, tick, util};
use anyhow::Result;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

/// Retention for `sessions/<id>/` corpses (system scratch): env
/// `LOOOP_SESSION_TTL` (seconds) > config `session_ttl` > 3 days. looop owns
/// reaping its own scratch; a worker's durable output lives in reports/ + git +
/// its sandbox, never here, so this only bounds debug transcripts.
pub(crate) fn session_ttl_secs(paths: &Paths) -> u64 {
    const DEFAULT: u64 = 3 * 24 * 60 * 60; // 3 days
    if let Some(n) = util::env_knob::<u64>("LOOOP_SESSION_TTL") {
        return n;
    }
    Config::load(paths)
        .ok()
        .and_then(|c| {
            c.root
                .get("session_ttl")
                .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64)))
        })
        .unwrap_or(DEFAULT)
}

/// Resolve a cadence knob: env var > config key > fallback. Clamped to ≥ 1s:
/// a zero, negative, or fractional config value (e.g. `0`, `-5`, `0.5` — the
/// `as_u64`/`as_f64→as u64` casts collapse all of them to 0) would otherwise
/// turn the pulse into a busy spin loop of full sensing.
fn interval(env: &str, cfg: &Config, key: &str, fallback: u64) -> u64 {
    let v = util::env_knob::<u64>(env).unwrap_or_else(|| {
        cfg.root
            .get(key)
            .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64)))
            .unwrap_or(fallback)
    });
    if v == 0 {
        util::event(
            Level::Warn,
            "pulse.guard_degraded",
            &format!(
                "{key} resolved to 0s (would busy-spin) — clamped to 1s; set a positive integer"
            ),
            &[("key", serde_json::json!(key))],
        );
    }
    v.max(1)
}

/// Floor for a `next_interval_s` nudge: a re-decide sooner than this would burn
/// the AI budget on near-back-to-back beats.
const NUDGE_MIN_SECS: u64 = 5;
/// Default ceiling for a `next_interval_s` nudge (overridable via
/// `LOOOP_MAX_NEXT_INTERVAL` / config `max_next_interval`). Bounds how long the
/// pulse stays blind (not sensing) during an AI-chosen nap.
const DEFAULT_MAX_NUDGE_SECS: u64 = 300;

/// Clamp the decider's one-shot cadence nudge into `[NUDGE_MIN_SECS, max_nudge]`.
/// `max_nudge` is floored at `NUDGE_MIN_SECS` so a misconfigured tiny cap still
/// yields a valid (non-inverted) range.
fn clamp_nudge(req: u64, max_nudge: u64) -> u64 {
    req.clamp(NUDGE_MIN_SECS, max_nudge.max(NUDGE_MIN_SECS))
}

// ---- durable cadence nudge (.next-wake.json) ---------------------------------

/// The typed shape of `.next-wake.json`. Absent/unreadable/corrupt all read
/// as "no pending deadline" (same as the old hand-parse): the nudge is a
/// cadence optimization, never a correctness gate.
#[derive(serde::Deserialize)]
struct NextWake {
    due: u64,
}

/// The pending next-wake deadline plus the EXACT raw bytes it was read from.
/// The raw string is what [`consume_next_wake`] compares against — the
/// compare-and-delete token — so a deadline REWRITTEN during the tick (even
/// to a different value serialized the same length) is never mistaken for
/// the one this beat observed.
fn read_next_wake_entry(paths: &Paths) -> Option<(u64, String)> {
    let raw = fs::read_to_string(paths.next_wake()).ok()?;
    let due = serde_json::from_str::<NextWake>(&raw).ok()?.due;
    Some((due, raw))
}

/// The pending next-wake deadline (unix secs), if any.
fn read_next_wake(paths: &Paths) -> Option<u64> {
    read_next_wake_entry(paths).map(|(due, _)| due)
}

/// Persist a one-shot cadence nudge as a DEADLINE, not an in-memory flag: a
/// pulse crash during the sleep no longer loses the follow-up — the next pulse
/// (or the next loop iteration) sees the file and re-decides once it's due.
///
/// Published under the data dir's per-directory writer lock (the same one
/// FileStore's writer primitives take) so [`consume_next_wake`]'s
/// compare-and-delete can never race this rename: without the lock, a fresh
/// deadline could land between the consume's compare and its delete and be
/// destroyed (exactly the hazard `StateStore::write_atomic` documents).
fn write_next_wake(paths: &Paths, due: u64) {
    let body = serde_json::json!({ "v": 1, "due": due }).to_string();
    // Lock failure degrades the CAS guarantee, not the nudge itself — still
    // publish (losing the timed follow-up outright would be worse).
    let lock = crate::store::DirLock::acquire(&paths.data_dir);
    if let Err(e) = &lock {
        util::event(
            Level::Warn,
            "pulse.guard_degraded",
            &format!("cannot take the writer lock for the next-wake deadline: {e}"),
            &[],
        );
    }
    if let Err(e) = util::write_atomic(&paths.next_wake(), body.as_bytes()) {
        util::event(
            Level::Warn,
            "pulse.guard_degraded",
            &format!("failed to persist the next-wake deadline (timed nudge may be lost): {e}"),
            &[],
        );
    }
}

/// Whether the pending nudge is DUE. Read-only — it deliberately does NOT
/// delete the file: consuming before the tick runs would lose the timed nudge
/// forever if the beat idles out (backoff / budget / config error) before
/// deciding. The pulse loop consumes it ([`consume_next_wake`]) only AFTER a
/// tick that actually attempted a decide.
#[cfg(test)]
fn next_wake_due(paths: &Paths) -> bool {
    matches!(read_next_wake(paths), Some(due) if crate::util::now_unix() >= due)
}

/// Consume the nudge — called once a due wake has been HONORED (a decide was
/// actually attempted this beat, success or failure alike). COMPARE-AND-DELETE
/// on the exact raw record the beat observed BEFORE the tick (`observed`,
/// from [`read_next_wake_entry`]): a tick can run for up to the tick timeout
/// (30 min default), and an unconditional `remove_file` here would destroy a
/// FRESH deadline written during it (a schedule/verb nudging the loop
/// mid-tick) — losing that timed follow-up entirely. Mirrors
/// `StateStore::remove_if_eq` (there is no `Key` variant addressing
/// `.next-wake.json`, so the read+compare+delete is done here under the same
/// per-directory writer lock [`write_next_wake`] publishes with).
fn consume_next_wake(paths: &Paths, observed: &str) {
    let Ok(_lock) = crate::store::DirLock::acquire(&paths.data_dir) else {
        // Cannot serialize the compare — leave the deadline in place. It
        // re-fires next beat: a duplicate forced re-decide is far cheaper
        // than deleting a deadline we can't prove is ours. Warn (like
        // `write_next_wake` does on the same failure): if the lock keeps
        // failing, every beat force-re-decides, and that must not be silent.
        util::event(
            Level::Warn,
            "pulse.guard_degraded",
            "cannot lock the data dir to consume the wake deadline — leaving it in place (forced re-decide next beat)",
            &[],
        );
        return;
    };
    match fs::read_to_string(paths.next_wake()) {
        Ok(cur) if cur == observed => {
            let _ = fs::remove_file(paths.next_wake());
        }
        // A FRESH deadline landed mid-tick — not ours to consume.
        Ok(_) => {}
        // Already gone (or unreadable — leave it; the next beat retries).
        Err(_) => {}
    }
}

/// The seconds to sleep after a beat: `want` capped to a pending next-wake
/// deadline ONLY while that deadline is in the FUTURE. A PAST-DUE deadline
/// must NOT shorten the sleep: it already forces the next beat to re-decide
/// (and only falls once a decide is attempted), so clamping to it — the old
/// `remaining.max(1)` — turned any due wake that survived an idled-out beat
/// (backoff / budget / config error) into a 1 Hz full-sense spin loop.
fn clamp_sleep_to_wake(want: u64, due: Option<u64>, now: u64) -> u64 {
    match due {
        Some(due) if due > now => want.min(due - now),
        _ => want,
    }
}

/// One 1-second-slice re-check of the sleep deadline: `until` (absolute unix
/// seconds) shrinks when a FRESH `.next-wake.json` deadline — one that differs
/// from what the sleep STARTED with — lands mid-sleep. Without this, a
/// schedule/verb written during the sleep waited out the full interval (worse
/// with AI cadence nudges) because the deadline was only read at sleep start.
///
/// Only a CHANGED deadline shortens the sleep: the initial deadline was
/// already folded in by [`clamp_sleep_to_wake`], and an unchanged PAST-DUE
/// leftover must keep being ignored (see clamp_sleep_to_wake — honoring it
/// would recreate the 1 Hz spin loop it exists to prevent). A fresh deadline
/// that is ALREADY due clamps to `now` — i.e. wake immediately — which is
/// exactly what a "wake the loop now" verb wants. `until` only ever shrinks.
///
/// KNOWN LIMIT (accepted): "changed" is judged by the deadline VALUE, so a
/// mid-sleep rewrite carrying the SAME due second as the initial read is
/// indistinguishable from the leftover and does not re-shorten the sleep.
/// Reaching that state needs a writer to re-issue an identical deadline
/// within the same second the sleep started with — and even then the
/// deadline itself is still honored by [`clamp_sleep_to_wake`] on the next
/// iteration, so the cost is at most one interval of latency, not a lost
/// nudge. (The consume side is safe regardless: [`consume_next_wake`]
/// compares raw bytes, not values.)
fn recheck_wake_deadline(until: u64, initial_due: Option<u64>, due: Option<u64>, now: u64) -> u64 {
    match due {
        Some(d) if due != initial_due => until.min(d.max(now)),
        _ => until,
    }
}

/// Sleep up to `want` seconds after a beat, waking EARLY when (a) a graceful
/// shutdown signal arrives or (b) a fresh wake deadline lands mid-sleep (see
/// [`recheck_wake_deadline`]). The sleep is a chain of 1-second slices, each
/// re-reading `.next-wake.json` and the shutdown flag — so both are honored
/// within about a second instead of a full interval.
///
/// Output behavior matches [`util::sleep_countdown`], which this replaces on
/// the pulse path: a live one-line countdown repainted each second when ANSI
/// is on, and a plain silent sleep otherwise (JSON / NO_COLOR / non-PTY —
/// detected via the shared color codes being empty), so logs see no repaint
/// spam and no extra lines.
fn sleep_wake_aware(paths: &Paths, want: u64, suffix: &str) {
    let initial_due = read_next_wake(paths);
    let mut until = util::now_unix() + want;
    // ANSI proxy: the shared color codes are empty exactly when color is off.
    let ansi = !util::rst().is_empty();
    // Freeze the timestamp at the start (like sleep_countdown) so the line
    // reads as "the beat logged at [ts], next one in Ns".
    let ts = util::hms();
    loop {
        let now = util::now_unix();
        if now >= until || shutdown_requested() {
            break;
        }
        if ansi {
            // CR + clear-to-EOL so a shrinking count leaves no stale digit.
            print!(
                "\r\x1b[2K{}[{ts}] next beat in {}s ({suffix}){}",
                util::dim(),
                until - now,
                util::rst()
            );
            let _ = std::io::Write::flush(&mut std::io::stdout());
        }
        std::thread::sleep(Duration::from_secs(1));
        until = recheck_wake_deadline(until, initial_due, read_next_wake(paths), util::now_unix());
    }
    if ansi {
        // Erase the countdown line so the next beat prints clean.
        print!("\r\x1b[2K");
        let _ = std::io::Write::flush(&mut std::io::stdout());
    }
}

/// Set by the SIGTERM/SIGINT handler; polled by the beat loop and the sleep
/// loop. A relaxed/SeqCst atomic store is one of the few operations that is
/// async-signal-safe, which is exactly why the handler does NOTHING else.
static SHUTDOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether a graceful-shutdown signal has been received.
fn shutdown_requested() -> bool {
    SHUTDOWN.load(std::sync::atomic::Ordering::SeqCst)
}

/// The signal handler proper: flag-and-return, nothing more (only
/// async-signal-safe operations are legal here — no allocation, no locks, no
/// I/O). The beat loop notices the flag at its next check and exits cleanly.
#[cfg_attr(not(unix), allow(dead_code))] // only the Unix installer + tests reference it
extern "C" fn on_shutdown_signal(_sig: i32) {
    SHUTDOWN.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Register the graceful-shutdown handler for SIGTERM and SIGINT. Without it
/// a signal kills the pulse mid-beat: [`LockGuard`] never drops, so the pid
/// file lingers, and an in-flight beat is torn wherever it happens to be.
/// With it the current beat FINISHES, the sleep loop exits within a second,
/// and `cmd_run` returns normally — the guard drops and the pid file is
/// removed.
///
/// SCOPE: direct SIGTERM/Ctrl-C only — DELIBERATELY not `looop down`, whose
/// babysit kill path delivers SIGHUP (and expects the pulse gone within its
/// 2s deadline; a graceful beat-completion can take up to the tick timeout,
/// minutes). `down` stays an immediate stop: hard death is safe BY DESIGN
/// here — state is level-triggered plain files, the flock dies with the
/// process, and the WAL guard reports any torn non-idempotent action — so
/// graceful exit is a nicety for operator signals, not a correctness
/// requirement.
///
/// libc-free via the same extern-"C" technique as [`util::kill_process_group`]
/// / `flock_file`. `signal(2)` rather than `sigaction(2)` deliberately: the
/// sigaction struct's layout is platform-specific (padding, field order differ
/// across macOS/Linux) and getting it wrong is silent UB, while `signal`'s ABI
/// is a stable two-argument call on every Unix this project targets — and the
/// one semantic difference that matters (one-shot SysV reset on some
/// platforms) is harmless here because the first signal already initiates
/// shutdown.
#[cfg(unix)]
fn install_shutdown_handler() {
    // NB: the handler is passed as `usize` (not a typed fn pointer) so this
    // declaration matches the ONE other extern signal declaration in the
    // crate (`main::restore_sigpipe`, which needs SIG_DFL = 0) — two extern
    // "C" fns with the same name but different signatures trip
    // `clashing_extern_declarations`.
    unsafe extern "C" {
        fn signal(signum: i32, handler: usize) -> usize;
    }
    const SIGINT: i32 = 2;
    const SIGTERM: i32 = 15;
    let handler = on_shutdown_signal as extern "C" fn(i32) as usize;
    unsafe {
        let _ = signal(SIGINT, handler);
        let _ = signal(SIGTERM, handler);
    }
}
#[cfg(not(unix))]
fn install_shutdown_handler() {}

/// A non-blocking exclusive `flock(2)` on an open fd. `true` = we hold it now.
/// flock is the right primitive for single-instance: the kernel releases it when
/// the holding process dies for ANY reason (normal exit, panic, `kill -9`, crash),
/// so there is no stale lock to reclaim and no PID-liveness guessing that a reused
/// PID can fool. Routes through the shared [`util::flock_file`] declaration.
fn try_flock(f: &std::fs::File) -> bool {
    util::flock_file(f, false)
}

/// Whether a live pulse currently holds the single-instance flock. The
/// authoritative "is the loop actually running" probe (a babysit session can be
/// alive while its inner loop has crashed): open the lock file read-only and try
/// to take the flock; if we CAN, nobody holds it. Exercised by the lock tests.
///
/// KNOWN WINDOW: the probe actually TAKES the flock for the instant between
/// the successful try and the fd drop at the end of this function. A `looop
/// up` racing exactly into that window would see the lock held and report
/// "already running" — a false negative for the starter: `up` exits 1 with
/// the "already running" notice (there is NO automatic retry), and the
/// operator simply re-runs it. The window is a few microseconds and the
/// failure mode is benign, so a probe-without-acquire (which flock(2) simply
/// doesn't offer) isn't worth the complexity.
pub(crate) fn pulse_running(paths: &Paths) -> bool {
    let Ok(f) = std::fs::File::open(paths.lock().join("lock")) else {
        return false;
    };
    !try_flock(&f)
}

/// Holds the lock file open for the pulse's lifetime; the flock is released by the
/// kernel when `_file` is dropped (or the process dies). Only the PID file is
/// removed on a clean exit — the lock FILE itself is deliberately NEVER deleted:
/// flock(2) is per-inode, so unlinking it would let the next pulse open a FRESH
/// inode and "acquire" a lock a still-live pulse holds on the old one (two
/// pulses both believing they own the singleton).
struct LockGuard {
    path: PathBuf,
    _file: std::fs::File,
}
impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.path.join("pid"));
    }
}

/// Why [`acquire_lock`] did not hand back the lock. The two cases are
/// OPPOSITE diagnoses — "another pulse is live" vs "this pulse can't even
/// open the lock file (permissions, disk full)" — so collapsing them into one
/// `None` (the old shape) made an I/O failure masquerade as "already running".
#[derive(Debug)]
enum LockDenied {
    /// A live pulse holds the flock (kernel-managed, no PID guess).
    Held,
    /// The lock file itself could not be created/opened — report the real
    /// error, do NOT claim another pulse is running.
    Io(std::io::Error),
}

/// Acquire the single-instance lock via `flock(2)` on `<data>/.lock/lock`.
/// Returns the guard (lock held for its lifetime) on success, or the reason
/// it was denied (see [`LockDenied`]). The pulse is the sole beat runner, so
/// holding this for its lifetime guarantees no two beats ever wipe/regenerate
/// the shared snapshots/ dir under each other (H4). A pid file is written
/// alongside purely for human-facing messages (`looop state`, the "already
/// running" notice).
fn acquire_lock(paths: &Paths) -> Result<LockGuard, LockDenied> {
    let dir = paths.lock();
    let _ = fs::create_dir_all(&dir);
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("lock"))
        .map_err(LockDenied::Io)?;
    if !try_flock(&file) {
        return Err(LockDenied::Held); // a live pulse holds the flock
    }
    // Rename-published like every other state file: a reader (`looop state`,
    // the "already running" notice) must never see a torn pid.
    let _ = crate::util::write_atomic(
        &dir.join("pid"),
        format!("{}\n", std::process::id()).as_bytes(),
    );
    Ok(LockGuard {
        path: dir,
        _file: file,
    })
}

pub fn cmd_run(paths: &Paths) -> Result<ExitCode> {
    seed::ensure_dirs(paths)?;
    let cfg = Config::load(paths)?;
    let beat = interval("LOOOP_INTERVAL", &cfg, "interval", 60);
    // Upper bound on the decider's one-shot `next_interval_s` nudge. The pulse
    // does not SENSE during an AI-chosen nap, so a world change (e.g. a PR
    // going red just after a beat) stays invisible until the nap ends. Capping
    // the nudge bounds that blindness window; 3600s (the old cap) meant up to
    // an hour blind. Default 300s (5 min); env/config overridable.
    let max_nudge = interval(
        "LOOOP_MAX_NEXT_INTERVAL",
        &cfg,
        "max_next_interval",
        DEFAULT_MAX_NUDGE_SECS,
    );

    // Single-instance lock (flock-based; released by the kernel on exit/crash).
    let _guard = match acquire_lock(paths) {
        Ok(g) => g,
        Err(LockDenied::Held) => {
            let oldpid = fs::read_to_string(paths.lock().join("pid")).unwrap_or_default();
            eprintln!("looop: already running (pid {})", oldpid.trim());
            return Ok(ExitCode::from(1));
        }
        // An unopenable lock file is NOT "already running" — surface the real
        // I/O error (permissions, disk full) so the operator fixes the cause.
        Err(LockDenied::Io(e)) => {
            return Err(anyhow::anyhow!(
                "cannot open the pulse lock file {}: {e}",
                paths.lock().join("lock").display()
            ));
        }
    };

    // Graceful shutdown: from here on a SIGTERM/SIGINT finishes the current
    // beat and exits the loop cleanly instead of killing the pulse mid-beat.
    install_shutdown_handler();

    let runner_name = cfg.runner_label();
    util::event(
        Level::Ok,
        "pulse.start",
        &format!("pulse started · deciding every {beat}s · runner {runner_name}"),
        &[
            ("interval", serde_json::json!(beat)),
            ("runner", serde_json::json!(runner_name)),
        ],
    );
    if !paths.default_profile {
        util::event(
            Level::Info,
            "pulse.profile",
            &format!(
                "this profile's sessions live under {d} (LOOOP_DATA_DIR={d} looop ls)",
                d = paths.data_dir.display()
            ),
            &[(
                "data_dir",
                serde_json::json!(paths.data_dir.display().to_string()),
            )],
        );
    }

    // Decide forever. `force` makes a beat re-decide even if the world hash is
    // unchanged. It starts TRUE so the FIRST beat of every pulse process always
    // takes a move: `looop up` should act immediately, not sit idle for a full
    // interval because the world happens to match a `.last-tick-hash` left by a
    // previous run in this data dir. After that it is reset every beat and only
    // re-armed by a `next_interval_s` cadence nudge (a goal scheduling a
    // time-based follow-up). Steady-state beats stay gated by the world hash, so
    // a quiet loop is still nearly free. (Failure backoff still applies on the
    // forced beat, so a crash-restart loop can't burn unbounded AI calls.)
    let mut force = true;
    loop {
        // Re-read the cadence each beat: goals, PLAYBOOK, and sensors are all
        // re-read per beat already, so a config `interval` edit should not be
        // the one change that needs a pulse restart. A config error mid-flight
        // falls back to the startup value (the tick itself reports the broken
        // config loudly — the pulse must not die on a bad edit).
        let beat = Config::load(paths)
            .map(|c| interval("LOOOP_INTERVAL", &c, "interval", 60))
            .unwrap_or(beat);

        // A due durable nudge (written below, survives crashes) forces this
        // beat to re-decide even over an unchanged world. It is only READ here
        // — consumed after the tick, and only when a decide actually ran, so a
        // beat that idles out (backoff / budget / config error) leaves the
        // timed follow-up armed for the next beat instead of losing it. The
        // RAW record is captured too: the consume below is a compare-and-
        // delete against exactly what this beat observed, so a fresh deadline
        // written DURING the (up to 30 min) tick survives it.
        let wake = read_next_wake_entry(paths);
        let wake_due = matches!(&wake, Some((due, _)) if crate::util::now_unix() >= *due);
        if wake_due {
            force = true;
        }
        let outcome = tick::tick(paths, force);
        force = false;
        if wake_due
            && outcome.decided_or_failed
            && let Some((_, observed)) = &wake
        {
            consume_next_wake(paths, observed);
        }

        // One-shot AI cadence nudge (clamped 5..3600), persisted as a DEADLINE
        // (`.next-wake.json`) rather than carried in memory: a crash during the
        // sleep no longer loses the follow-up, and the consume above re-arms the
        // forced re-decide once it's due.
        let mut want = beat;
        if let Some(req) = outcome.next_interval_s {
            let req = clamp_nudge(req, max_nudge);
            util::event(
                Level::Info,
                "cadence",
                &format!("AI cadence override: next beat in {req}s (default {beat}s)"),
                &[
                    ("secs", serde_json::json!(req)),
                    ("default", serde_json::json!(beat)),
                ],
            );
            write_next_wake(paths, util::now_unix() + req);
            want = req;
        }
        // Never sleep PAST a pending FUTURE deadline (this beat's nudge or a
        // leftover from a previous pulse). A past-due deadline does not
        // shorten the sleep — see clamp_sleep_to_wake.
        want = clamp_sleep_to_wake(want, read_next_wake(paths), util::now_unix());
        let suffix = if outcome.acted { "acted" } else { "idle" };
        if util::is_json() {
            // JSON watchers can't see the live countdown — keep the structured
            // marker; sleep_wake_aware is silent without ANSI.
            util::event(
                Level::Info,
                "sleep",
                &format!("next beat in {want}s ({suffix})"),
                &[
                    ("secs", serde_json::json!(want)),
                    ("acted", serde_json::json!(outcome.acted)),
                ],
            );
        }
        // Wake-aware sleep: honors a mid-sleep deadline AND the shutdown flag
        // within about a second (human mode shows the live countdown).
        sleep_wake_aware(paths, want, suffix);
        // Graceful shutdown: the signal handler only sets the flag — the beat
        // that was in flight has FINISHED, so exiting here is clean. Breaking
        // (not exiting) lets `_guard` drop normally: the pid file is removed
        // and the flock released.
        if shutdown_requested() {
            break;
        }
    }
    util::event(
        Level::Ok,
        "pulse.stop",
        "pulse stopped (SIGTERM/SIGINT) — current beat finished, lock released",
        &[],
    );
    Ok(ExitCode::SUCCESS)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn next_wake_is_durable_and_due_ness_is_a_pure_read() {
        let p = Paths::temp();
        assert!(!next_wake_due(&p), "no file, nothing due");

        // A future deadline persists across reads.
        let future = crate::util::now_unix() + 3600;
        write_next_wake(&p, future);
        assert_eq!(read_next_wake(&p), Some(future));
        assert!(!next_wake_due(&p), "not due yet");
        assert!(p.next_wake().is_file(), "undue nudge survives (durable)");

        // A passed deadline is due — and READING due-ness never deletes it.
        write_next_wake(&p, crate::util::now_unix() - 1);
        assert!(next_wake_due(&p), "due nudge forces a re-decide");
        assert!(next_wake_due(&p), "due-ness is a pure read (not consumed)");
        assert!(p.next_wake().is_file());

        // Explicit consume (with the observed record) removes it exactly once.
        let (_, observed) = read_next_wake_entry(&p).unwrap();
        consume_next_wake(&p, &observed);
        assert!(!p.next_wake().is_file(), "consumed");
        assert!(!next_wake_due(&p), "one-shot");
    }

    #[test]
    fn consume_next_wake_is_a_compare_and_delete() {
        // Regression: the consume used to be an unconditional remove_file — a
        // FRESH deadline written DURING the tick (which can run up to 30 min)
        // was deleted alongside the honored one, silently losing the timed
        // follow-up.
        let p = Paths::temp();
        write_next_wake(&p, 100);
        let (_, observed) = read_next_wake_entry(&p).unwrap();
        // A fresh deadline lands "mid-tick" (after the beat's observation)…
        write_next_wake(&p, 200);
        consume_next_wake(&p, &observed);
        assert_eq!(
            read_next_wake(&p),
            Some(200),
            "a deadline written mid-tick survives the consume of the old one"
        );
        // …and only a consume carrying the CURRENT record removes it.
        let (_, current) = read_next_wake_entry(&p).unwrap();
        consume_next_wake(&p, &current);
        assert!(!p.next_wake().is_file(), "the observed record is consumed");
        // Consuming an already-gone deadline is a no-op (idempotent).
        consume_next_wake(&p, &current);
        assert!(!p.next_wake().is_file());
    }

    #[test]
    fn next_wake_survives_an_idle_beat_and_falls_after_a_decide_attempt() {
        let p = Paths::temp();
        let _ = crate::seed::ensure_dirs(&p);
        write_next_wake(&p, crate::util::now_unix() - 1);
        assert!(next_wake_due(&p));

        // An IDLE beat (unparseable config — tick bails before deciding) must
        // leave the timed nudge armed: the pulse loop consumes it only when
        // `decided_or_failed` is true.
        fs::write(&p.config, "{ not json").unwrap();
        let idle = crate::tick::tick(&p, true);
        assert!(!idle.decided_or_failed, "config error idles the beat out");
        assert!(next_wake_due(&p), "the timed nudge survives an idle beat");

        // A beat that actually LAUNCHES the runner (even a failing one) counts
        // as a decide attempt — the pulse loop then consumes the wake.
        fs::write(
            &p.config,
            serde_json::json!({
                "tick_command": "false",
                "worker_command": "true {{prompt_file}}"
            })
            .to_string(),
        )
        .unwrap();
        let attempted = crate::tick::tick(&p, true);
        assert!(attempted.decided_or_failed, "the runner was launched");
        // What the pulse loop does on decided_or_failed: compare-and-delete
        // with the record it observed before the tick.
        let (_, observed) = read_next_wake_entry(&p).unwrap();
        consume_next_wake(&p, &observed);
        assert!(!next_wake_due(&p));
        assert!(!p.next_wake().is_file());
    }

    #[test]
    fn clamp_nudge_bounds_the_ai_cadence_override() {
        // A giant nap is capped to the ceiling — this is the whole point: the
        // pulse must not go blind for an hour because the decider asked to.
        assert_eq!(
            clamp_nudge(3600, DEFAULT_MAX_NUDGE_SECS),
            DEFAULT_MAX_NUDGE_SECS
        );
        assert_eq!(clamp_nudge(100_000, 300), 300);
        // A too-eager tiny nudge is floored so back-to-back beats can't burn budget.
        assert_eq!(clamp_nudge(0, 300), NUDGE_MIN_SECS);
        assert_eq!(clamp_nudge(1, 300), NUDGE_MIN_SECS);
        // In-range values pass through untouched.
        assert_eq!(clamp_nudge(120, 300), 120);
        // A misconfigured cap below the floor still yields a valid range
        // (never an inverted clamp that would panic).
        assert_eq!(clamp_nudge(10, 2), NUDGE_MIN_SECS);
    }

    #[test]
    fn interval_clamps_zero_negative_and_fractional_to_one_second() {
        let cfg = |v: serde_json::Value| Config {
            root: serde_json::json!({ "interval": v }),
        };
        // The env knob is deliberately one no test sets, so the config path is
        // what's exercised.
        let env = "LOOOP_TEST_INTERVAL_UNSET";
        // 0, negative, and fractional all collapse to 0 via the casts — and
        // must clamp to 1s instead of busy-spinning.
        assert_eq!(interval(env, &cfg(serde_json::json!(0)), "interval", 60), 1);
        assert_eq!(
            interval(env, &cfg(serde_json::json!(-5)), "interval", 60),
            1
        );
        assert_eq!(
            interval(env, &cfg(serde_json::json!(0.5)), "interval", 60),
            1
        );
        // Sane values pass through; an absent key uses the fallback.
        assert_eq!(
            interval(env, &cfg(serde_json::json!(30)), "interval", 60),
            30
        );
        let empty = Config {
            root: serde_json::json!({}),
        };
        assert_eq!(interval(env, &empty, "interval", 60), 60);
    }

    #[test]
    fn recheck_shortens_only_for_a_fresh_deadline() {
        let now = 1_000u64;
        let until = 1_060u64;
        // No deadline at all: the sleep runs its course.
        assert_eq!(recheck_wake_deadline(until, None, None, now), until);
        // The deadline the sleep STARTED with (clamped at sleep start already,
        // and — when past-due — deliberately ignored, see clamp_sleep_to_wake):
        // unchanged, so it never re-shortens.
        assert_eq!(
            recheck_wake_deadline(until, Some(900), Some(900), now),
            until,
            "an unchanged past-due leftover must not recreate the spin loop"
        );
        // A FRESH future deadline shortens the sleep to it…
        assert_eq!(recheck_wake_deadline(until, None, Some(1_010), now), 1_010);
        assert_eq!(
            recheck_wake_deadline(until, Some(900), Some(1_010), now),
            1_010,
            "a mid-sleep rewrite of the deadline counts as fresh"
        );
        // …a fresh ALREADY-DUE deadline means "wake now"…
        assert_eq!(recheck_wake_deadline(until, None, Some(990), now), now);
        // …and a fresh deadline LATER than the sleep never extends it.
        assert_eq!(recheck_wake_deadline(until, None, Some(2_000), now), until);
        // A consumed deadline (fresh None) leaves the sleep alone.
        assert_eq!(recheck_wake_deadline(until, Some(1_010), None, now), until);
    }

    #[test]
    fn sleep_wakes_early_on_a_mid_sleep_deadline() {
        // Serialize with the shutdown test below (shared SHUTDOWN static).
        let _env = crate::util::test_env_lock();
        let p = Paths::temp();
        // A second Paths onto the SAME data dir for the writer thread
        // (Paths::temp's cleanup-on-drop stays with `p` alone).
        let p2 = Paths {
            bin: p.bin.clone(),
            data_dir: p.data_dir.clone(),
            config: p.config.clone(),
            default_profile: p.default_profile,
            temp_cleanup: false,
        };
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(1200));
            // An immediately-due wake written mid-sleep — e.g. a schedule/verb
            // nudging the loop — must cut the sleep short.
            write_next_wake(&p2, crate::util::now_unix());
        });
        let t0 = std::time::Instant::now();
        sleep_wake_aware(&p, 60, "idle");
        writer.join().unwrap();
        assert!(
            t0.elapsed().as_secs() < 10,
            "a fresh mid-sleep deadline must wake the loop within seconds, \
             not after the full 60s interval"
        );
    }

    #[test]
    fn sleep_exits_promptly_on_shutdown() {
        // Serialize with the mid-sleep test above (shared SHUTDOWN static),
        // and ALWAYS reset the flag — even on panic — so no sibling test
        // inherits a poisoned shutdown state.
        let _env = crate::util::test_env_lock();
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                SHUTDOWN.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let _reset = Reset;
        let p = Paths::temp();
        on_shutdown_signal(15); // what the real SIGTERM delivery does
        assert!(shutdown_requested());
        let t0 = std::time::Instant::now();
        sleep_wake_aware(&p, 60, "idle");
        assert!(
            t0.elapsed().as_secs() < 5,
            "a pending shutdown must end the sleep promptly, not after 60s"
        );
    }

    #[test]
    fn past_due_wake_does_not_clamp_the_sleep() {
        // No deadline: the default interval stands.
        assert_eq!(clamp_sleep_to_wake(60, None, 1_000), 60);
        // A FUTURE deadline caps the sleep to its remaining time…
        assert_eq!(clamp_sleep_to_wake(60, Some(1_030), 1_000), 30);
        // …but never extends a shorter sleep.
        assert_eq!(clamp_sleep_to_wake(10, Some(1_030), 1_000), 10);
        // A deadline due exactly NOW or PAST-DUE must not shorten the sleep:
        // it already forces the next beat, and clamping it (the old
        // `remaining.max(1)`) spun a 1s full-sense loop whenever a due wake
        // survived an idled-out beat.
        assert_eq!(clamp_sleep_to_wake(60, Some(1_000), 1_000), 60);
        assert_eq!(clamp_sleep_to_wake(60, Some(900), 1_000), 60);
    }

    #[test]
    fn lock_is_exclusive_and_self_heals_after_release() {
        let p = Paths::temp();
        // Nobody holds it yet.
        assert!(!pulse_running(&p), "no pulse before any acquire");

        let g = acquire_lock(&p).expect("first acquire succeeds");
        // A second acquire (separate fd, even same process) is denied by flock
        // — and is diagnosed as HELD, not as an I/O failure.
        assert!(
            matches!(acquire_lock(&p), Err(LockDenied::Held)),
            "second acquire blocked while held"
        );
        // An outside observer (looop state) sees it as running.
        assert!(
            pulse_running(&p),
            "pulse_running true while the lock is held"
        );

        // Releasing the guard releases the flock; the next start re-acquires with
        // no stale-lock reclaim and no PID-liveness guessing.
        //
        // Poll with a short deadline instead of asserting immediately: flock is
        // held by the OPEN FILE DESCRIPTION, and a fork duplicates every fd —
        // so a sibling test spawning a child in the fork→exec window (before
        // CLOEXEC strips inherited fds) briefly co-owns the lock. The kernel
        // releases it as soon as that transient duplicate closes; production
        // never cares about this microsecond staleness, but a parallel test
        // harness hits the window often enough to flake an instant assert.
        drop(g);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while pulse_running(&p) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!pulse_running(&p), "not running once released");
        let g2 = acquire_lock(&p).expect("re-acquire after release");
        drop(g2);
    }

    #[test]
    fn stale_lock_dir_is_not_mistaken_for_a_live_pulse() {
        let p = Paths::temp();
        // Simulate a crashed pulse: the lock dir + files exist, but no flock holder.
        let dir = p.lock();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("lock"), b"").unwrap();
        std::fs::write(dir.join("pid"), b"999999\n").unwrap();

        assert!(
            !pulse_running(&p),
            "a leftover lock dir is not a running pulse"
        );
        // And a fresh start reclaims it cleanly.
        let g = acquire_lock(&p).expect("acquire over a stale lock dir");
        drop(g);
    }
}
