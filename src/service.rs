//! Service control — `looop up` / `looop down`.
//!
//! `looop up` starts the PULSE: looop's detached, AUTONOMOUS loop — it senses,
//! decides ONE move per changed beat, and runs the worker fleet. That is looop.
//! You steer it by editing goals/PLAYBOOK and answering worker asks (optionally
//! through a client — e.g. an agent session you point at looop to watch + relay).
//! `looop down` stops the pulse and every live worker.

use crate::config;
use crate::paths::Paths;

/// The `looop down` survivor warning. A named constant (not an inline format
/// string) so the test below can verify every `looop <verb>` it mentions is a
/// REAL clap subcommand — a past version pointed users at a `looop status`
/// that never existed.
const PULSE_SURVIVED_WARNING: &str = "looop: WARNING — the pulse survived the kill (still alive after 2s); \
     not reaped — retry `looop down` or inspect it with `looop state` / `looop worker list -a`";

/// The `looop up` not-yet-alive warning — a named constant for the same drift
/// guard as above: spawning is asynchronous, so "spawned" is not "observed
/// alive", and we never report a start we didn't see.
const PULSE_NOT_OBSERVED_WARNING: &str = "looop: WARNING — the pulse was spawned but not yet observed alive (5s); \
     inspect it with `looop worker list -a` or retry `looop up`";

/// The `looop down` worker-survivor warning — the sweep's counterpart of
/// [`PULSE_SURVIVED_WARNING`], and a named constant for the same drift guard.
const WORKERS_SURVIVED_WARNING: &str = "looop: WARNING — some workers survived the sweep (still alive after 2s); \
     retry `looop down` or inspect them with `looop worker list -a`";
use crate::run;
use crate::session::{self, PULSE_SESSION};
use anyhow::Result;
use std::process::ExitCode;
use std::time::Duration;

/// `looop up [--json]` — start the autonomous pulse (idempotent). looop runs
/// itself from there; steer by editing goals/PLAYBOOK or run a client to watch
/// and relay (`an agent session pointed at `looop state`).
pub fn cmd_up(paths: &Paths, json: bool) -> Result<ExitCode> {
    // Hard gate: refuse to start the pulse until the operator has run `looop
    // init`. The runner wiring is a deliberate choice (which agent CLI drives
    // every tick + worker), so we make it explicit rather than silently booting
    // on a default the user never picked.
    if !config::is_initialized(paths) {
        eprintln!("looop: not initialized — run `looop init` first.");
        eprintln!(
            "       it picks your runner (claude/codex/opencode/pi/custom) and writes wiring."
        );
        return Ok(ExitCode::from(1));
    }
    // Initialized — now preflight the configured runner before spawning the pulse
    // (the same gate the plumbing verbs get via dispatch, run here after the init
    // check so the messages surface in the right order).
    crate::deps::require_deps(paths)?;
    // Startup lease: serialize the whole check-then-spawn below. Two
    // concurrent `looop up`s could BOTH read "no pulse" from the fleet
    // snapshot and both spawn — one pulse then loses the inner flock race and
    // lingers as a confusing half-started corpse (TOCTOU). The lease is the
    // per-directory writer lock on the pulse's lock dir: a blocking flock, so
    // the loser simply WAITS and then sees the winner's live pulse ("already
    // running"). flock (not a `StateStore::create_exclusive` lease file,
    // which has no Key for this) because it is kernel-managed — released on
    // ANY exit, `kill -9` included, so there is never a stale lease to reap.
    // Held until `_up_lease` drops at the end of this function.
    let _up_lease = match crate::store::DirLock::acquire(&paths.lock()) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("looop: cannot take the startup lease ({e}) — refusing to start the pulse");
            return Ok(ExitCode::from(1));
        }
    };
    // Enumerate the fleet ONCE, failing CLOSED: the lenient `is_alive` maps
    // an enumeration error to "no pulse", and spawning on that reading boots
    // a SECOND pulse whose inner loop then loses the flock race — a confusing
    // half-started state. Refuse instead and say why.
    let fleet = match session::try_list(paths) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("looop: cannot enumerate the fleet ({e}) — refusing to start the pulse");
            return Ok(ExitCode::from(1));
        }
    };
    if fleet.iter().any(|s| s.id == PULSE_SESSION && s.alive) {
        println!("looop: pulse already running");
    } else {
        if fleet.iter().any(|s| s.id == PULSE_SESSION) {
            session::reap(paths, PULSE_SESSION);
        }
        let bin = paths.bin.to_string_lossy().to_string();
        // The JSON flag travels to the CHILD through its own environment (an
        // argv `env`-wrapper), NEVER via `set_var` on this process: the fleet
        // probe above already built the multi-thread tokio runtime, and
        // mutating `environ` while other threads may call getenv (libstd's
        // spawn paths, babysit, any dependency) is a data race / UB on Unix —
        // the safety condition `set_var` demands cannot be upheld here.
        let cmd = if json {
            vec![
                "env".to_string(),
                "LOOOP_LOG_FORMAT=json".to_string(),
                bin,
                "pulse".to_string(),
            ]
        } else {
            vec![bin, "pulse".to_string()]
        };
        session::spawn_detached(paths, cmd, PULSE_SESSION)?;
        // Never report a start we didn't observe: the spawn is detached and
        // asynchronous, so only an is-alive sighting within the grace window
        // earns the success line — mirroring `looop down`, which refuses to
        // print "pulse stopped" while the pulse is still alive.
        if !session::await_alive(paths, PULSE_SESSION, Duration::from_secs(5)) {
            eprintln!("{PULSE_NOT_OBSERVED_WARNING}");
            return Ok(ExitCode::from(1));
        }
        println!("looop: pulse started{}", if json { " [json]" } else { "" });
    }
    Ok(ExitCode::SUCCESS)
}

/// `looop down` — stop the pulse, then every live worker, then reap the pulse
/// corpse so a re-`looop up` starts clean.
///
/// KILL ORDER: pulse FIRST, workers second. The pulse is the sole autonomous
/// spawner — sweeping workers while it is still alive (possibly mid-beat)
/// races its current beat: a `StartWorker` executed after the sweep's
/// snapshot but before the pulse died would silently survive `looop down`.
/// With the pulse confirmed dead first, the worker list can no longer grow
/// under the sweep.
pub fn cmd_down(paths: &Paths) -> Result<ExitCode> {
    // Fail CLOSED on enumeration: the lenient `is_alive` reads an I/O error
    // as "not running", and `down` would then exit 0 claiming a clean stop
    // while the whole fleet is still up.
    let was_alive = match session::try_is_alive(paths, PULSE_SESSION) {
        Ok(alive) => alive,
        Err(e) => {
            eprintln!(
                "looop: cannot enumerate the fleet ({e}) — cannot tell what is running; \
                 retry `looop down`"
            );
            return Ok(ExitCode::from(1));
        }
    };
    if was_alive {
        let _ = session::kill_quiet(paths, PULSE_SESSION);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while session::is_alive(paths, PULSE_SESSION) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        // Never report a stop that didn't happen: if the pulse outlived the
        // kill past the deadline, do NOT reap its status (that would erase the
        // evidence a live pulse still exists) and do NOT print "pulse stopped".
        if session::is_alive(paths, PULSE_SESSION) {
            eprintln!("{PULSE_SURVIVED_WARNING}");
            return Ok(ExitCode::from(1));
        }
    }

    // The sole spawner is dead — NOW sweep the workers. Same fail-closed
    // stance: an error-to-empty read here would kill nothing and print a
    // clean summary over a live fleet.
    let live: Vec<String> = match session::try_list(paths) {
        Ok(fleet) => fleet
            .into_iter()
            .filter(|s| !s.is_pulse() && s.alive)
            .map(|s| s.id)
            .collect(),
        Err(e) => {
            eprintln!(
                "looop: cannot enumerate the fleet ({e}) — workers were NOT swept; \
                 retry `looop down`"
            );
            return Ok(ExitCode::from(1));
        }
    };
    for id in &live {
        let _ = session::kill_quiet(paths, id);
    }
    if !live.is_empty() {
        println!(
            "looop: stopped {} worker{} ({})",
            live.len(),
            if live.len() == 1 { "" } else { "s" },
            live.join(", ")
        );
        // Final audit: give the kills the same grace the pulse gets, then
        // warn (and fail) if anything outlived the sweep — mirroring the
        // never-report-a-stop-that-didn't-happen rule above. Same fail-closed
        // stance as the enumerations above: the lenient `list_workers` reads
        // an enumeration error as "no survivors", and the audit would then
        // exit 0 claiming a clean stop over a possibly-live fleet.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let survivors = match session::try_list(paths) {
                Ok(fleet) => fleet.iter().any(|s| !s.is_pulse() && s.alive),
                Err(e) => {
                    eprintln!(
                        "looop: cannot enumerate the fleet ({e}) — cannot confirm the \
                         worker sweep; retry `looop down`"
                    );
                    return Ok(ExitCode::from(1));
                }
            };
            if !survivors {
                break;
            }
            if std::time::Instant::now() >= deadline {
                eprintln!("{WORKERS_SURVIVED_WARNING}");
                return Ok(ExitCode::from(1));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    if session::status_exists(paths, PULSE_SESSION) {
        session::reap(paths, PULSE_SESSION);
    }
    // Report what was OBSERVED, not assumed: "pulse stopped" only when there
    // was a live pulse to stop — a `looop down` with nothing running used to
    // claim a stop that never happened.
    println!("{}", down_summary(was_alive));
    Ok(ExitCode::SUCCESS)
}

/// The `looop down` closing line, keyed on whether a live pulse was actually
/// there to stop. A named fn (not an inline branch on a format string) so the
/// drift-guard test below can pin BOTH messages against the clap tree.
fn down_summary(was_alive: bool) -> &'static str {
    if was_alive {
        "looop: pulse stopped"
    } else {
        "looop: pulse not running"
    }
}

/// `looop pulse` (internal) — the headless pulse body babysit wraps. It is the
/// judgment-free sensing loop (`run::cmd_run`) running under a PTY.
/// `looop pulse` — looop's own detached spawn target: run the autonomous loop
/// in the foreground of this (detached) process. Not human-facing; `looop up`
/// spawns it.
pub fn cmd_pulse(paths: &Paths) -> Result<ExitCode> {
    run::cmd_run(paths)
}

#[cfg(test)]
mod tests {
    use super::{
        PULSE_NOT_OBSERVED_WARNING, PULSE_SURVIVED_WARNING, WORKERS_SURVIVED_WARNING, down_summary,
    };

    /// The `looop up` startup lease serializes concurrent starters: while one
    /// holds it, a second acquire BLOCKS (flock LOCK_EX) until the first
    /// releases — so the check-then-spawn can never interleave and double-
    /// spawn the pulse. Timing-based, with a generous margin: the second
    /// acquire must observably wait for the drop of the first.
    #[test]
    fn up_lease_blocks_a_concurrent_starter_until_released() {
        let p = crate::paths::Paths::temp();
        let lease = crate::store::DirLock::acquire(&p.lock()).expect("first lease");
        let dir = p.lock();
        let (tx, rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            // flock conflicts across separate open fds even within one
            // process, so this genuinely queues behind the holder.
            let _second = crate::store::DirLock::acquire(&dir).expect("second lease");
            tx.send(std::time::Instant::now()).unwrap();
        });
        let held_until = std::time::Instant::now() + std::time::Duration::from_millis(300);
        std::thread::sleep(std::time::Duration::from_millis(300));
        drop(lease);
        let acquired_at = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the second starter must eventually acquire the lease");
        waiter.join().unwrap();
        assert!(
            acquired_at >= held_until,
            "the second acquire must block until the first lease is released"
        );
    }

    /// `looop down` never claims a stop it didn't perform: with no live pulse
    /// the summary is the observed "not running", not a fabricated "stopped".
    #[test]
    fn down_reports_not_running_when_nothing_was_stopped() {
        assert_eq!(down_summary(true), "looop: pulse stopped");
        assert_eq!(down_summary(false), "looop: pulse not running");
    }

    /// `looop down` fails CLOSED when the fleet can't be enumerated: the old
    /// error-to-empty read killed nothing, saw no pulse, printed a clean
    /// "pulse not running", and exited 0 — while the whole fleet could still
    /// be up. An unreadable fleet must be exit 1, not a fabricated stop.
    #[test]
    fn down_fails_closed_when_the_fleet_cannot_be_enumerated() {
        let p = crate::paths::Paths::temp();
        // A regular FILE where babysit's sessions dir belongs makes its
        // read_dir fail — the shape of any transient enumeration error.
        std::fs::write(p.data_dir.join("sessions"), "not a dir").unwrap();
        let code = super::cmd_down(&p).unwrap();
        assert_eq!(code, std::process::ExitCode::from(1));
    }

    /// Drift guard: every `looop <verb>` a user-facing message constant tells
    /// the user to run must be a REAL clap subcommand — a stale message once
    /// pointed at a nonexistent `looop status`.
    #[test]
    fn messages_reference_only_real_subcommands() {
        use clap::CommandFactory;
        let root = crate::cli::Cli::command();
        for msg in [
            PULSE_SURVIVED_WARNING,
            PULSE_NOT_OBSERVED_WARNING,
            WORKERS_SURVIVED_WARNING,
            down_summary(true),
            down_summary(false),
        ] {
            let mut rest = msg;
            while let Some(i) = rest.find("looop ") {
                let tail = &rest[i + "looop ".len()..];
                let verb: String = tail
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                    .collect();
                // Skip non-verb prose (`looop: …`, `looop — …`) — only check
                // tokens that look like a subcommand word.
                if !verb.is_empty() {
                    assert!(
                        root.find_subcommand(&verb).is_some(),
                        "message references nonexistent subcommand `looop {verb}` in: {msg}"
                    );
                }
                rest = tail;
            }
        }
    }
}
