//! Service control — `looop up` / `looop down`.
//!
//! `looop up` starts the PULSE: looop's detached, AUTONOMOUS loop — it senses,
//! decides ONE move per changed beat, and runs the worker fleet. That is looop.
//! You steer it by editing goals/PLAYBOOK and answering worker asks (optionally
//! through a client — e.g. an agent session you point at looop to watch + relay).
//! `looop down` stops the pulse and every live worker.

use crate::config;
use crate::paths::Paths;
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
        session::await_alive(paths, PULSE_SESSION, Duration::from_secs(5));
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

    if session::is_alive(paths, PULSE_SESSION) {
        let _ = session::kill_quiet(paths, PULSE_SESSION);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while session::is_alive(paths, PULSE_SESSION) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    if session::status_exists(paths, PULSE_SESSION) {
        session::reap(paths, PULSE_SESSION);
    }
    println!("looop: pulse stopped");
    Ok(ExitCode::SUCCESS)
}

/// `looop pulse` (internal) — the headless pulse body babysit wraps. It is the
/// judgment-free sensing loop (`run::cmd_run`) running under a PTY.
/// `looop pulse` — looop's own detached spawn target: run the autonomous loop
/// in the foreground of this (detached) process. Not human-facing; `looop up`
/// spawns it.
pub fn cmd_pulse(paths: &Paths) -> Result<ExitCode> {
    run::cmd_run(paths)
}
