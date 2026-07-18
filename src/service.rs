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
    if session::is_alive(paths, PULSE_SESSION) {
        println!("looop: pulse already running");
    } else {
        if session::status_exists(paths, PULSE_SESSION) {
            session::reap(paths, PULSE_SESSION);
        }
        if json {
            unsafe { std::env::set_var("LOOOP_LOG_FORMAT", "json") };
        }
        let bin = paths.bin.to_string_lossy().to_string();
        session::spawn_detached(paths, vec![bin, "pulse".to_string()], PULSE_SESSION)?;
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

/// `looop down` — stop every live worker and the pulse, then reap the pulse
/// corpse so a re-`looop up` starts clean.
pub fn cmd_down(paths: &Paths) -> Result<ExitCode> {
    let live: Vec<String> = session::list_workers(paths)
        .into_iter()
        .filter(|s| s.alive)
        .map(|s| s.id)
        .collect();
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
    }

    let was_alive = session::is_alive(paths, PULSE_SESSION);
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
    use super::{PULSE_NOT_OBSERVED_WARNING, PULSE_SURVIVED_WARNING, down_summary};

    /// `looop down` never claims a stop it didn't perform: with no live pulse
    /// the summary is the observed "not running", not a fabricated "stopped".
    #[test]
    fn down_reports_not_running_when_nothing_was_stopped() {
        assert_eq!(down_summary(true), "looop: pulse stopped");
        assert_eq!(down_summary(false), "looop: pulse not running");
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
