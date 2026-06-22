//! Service control — `looop up` / `looop down`.
//!
//! `looop up` starts the PULSE: a detached, judgment-free sensing loop that keeps
//! `snapshots/` fresh. That is all looop runs. The JUDGMENT lives in a root agent
//! (a pi/claude session YOU start in another window and tell to observe looop) —
//! looop does not launch or manage it. The root agent watches the world by
//! blocking on `looop _ wait` and acts through the `looop _ …` verbs.
//! `looop down` stops the pulse and every live worker.

use crate::paths::Paths;
use crate::run;
use crate::session::{self, PULSE_SESSION};
use anyhow::Result;
use std::process::ExitCode;
use std::time::Duration;

/// `looop up [--json]` — start the pulse (idempotent). Then start your own agent
/// (pi/claude) in another window and tell it to observe looop:
///   "watch looop: loop on `looop _ wait --json` and act; read `looop --help`."
pub fn cmd_up(paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let mut json = false;
    for a in args {
        match a.as_str() {
            "--json" => json = true,
            other => {
                eprintln!("looop up: unknown option '{other}' (the only flag is --json)");
                return Ok(ExitCode::from(1));
            }
        }
    }

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
        session::spawn_detached(
            paths,
            vec![bin, "_".to_string(), "pulse".to_string()],
            PULSE_SESSION,
        )?;
        session::await_alive(paths, PULSE_SESSION, Duration::from_secs(5));
        println!("looop: pulse started{}", if json { " [json]" } else { "" });
    }
    println!(
        "looop: now start your agent and point it at looop, e.g.\n  \
         pi   # then: \"observe looop — loop on `looop _ wait --json` and act; read `looop --help`\""
    );
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

/// `looop _ pulse` (internal) — the headless pulse body babysit wraps. It is the
/// judgment-free sensing loop (`run::cmd_run`) running under a PTY.
pub fn cmd_pulse(paths: &Paths) -> Result<ExitCode> {
    run::cmd_run(paths)
}
