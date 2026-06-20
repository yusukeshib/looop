//! The pulse-as-a-service layer. Historically `looop up` spawned the pulse as a
//! detached babysit session and `looop down` stopped it. That detached lifecycle
//! is gone: the pulse is still just one more babysit-supervised session (uniform
//! with the workers, captured to its own output.log), but it now has a single
//! foreground owner — a bare `looop` invocation.
//!
//!   looop        bring the pulse up (as a session), stream it, and on exit
//!                (Ctrl-C or the pulse dying) tear it AND its workers down.
//!   looop _ pulse the headless pulse body babysit actually wraps (internal)
//!
//! `_ pulse` is just the existing reconcile loop (`run::cmd_run`): under babysit it
//! runs in a PTY, so its colored `util::log` output is captured to the session's
//! `output.log`. Watch it live with `looop watch pulse`, or check the fleet with
//! `looop ls`. There is no detached "start and walk away" mode — closing the
//! foreground `looop` stops the loop.

use crate::paths::Paths;
use crate::session::{self, PULSE_SESSION};
use crate::{run, util};
use anyhow::Result;
use std::process::ExitCode;
use std::time::Duration;

/// `looop [--json]` — the foreground control surface. Brings the pulse up as a
/// supervised session (spawning it if needed), streams its output live, and on
/// exit (Ctrl-C, or the pulse process dying) tears the pulse AND its live
/// workers down. `--json` makes the pulse emit NDJSON to its output.log
/// (machine-readable for an agent watching the stream).
///
/// This is the ONLY way to run the loop. There is no detached mode: to run it
/// unattended, background this command (`looop &` / `nohup` / a service
/// manager) — note that a hard kill (SIGKILL) skips the teardown, leaving the
/// detached pulse session running, which the next `looop` reattaches to.
pub fn cmd_serve(paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let mut json = false;
    for a in args {
        match a.as_str() {
            "--json" => json = true,
            other => {
                eprintln!("looop: unknown option '{other}' (the only flag is --json)");
                return Ok(ExitCode::from(1));
            }
        }
    }

    // Bring the pulse up if it isn't already (idempotent): a live pulse is
    // reattached to; a leftover corpse is pruned so its id can be reused.
    if session::is_alive(paths, PULSE_SESSION) {
        println!("looop: pulse already running — attaching (Ctrl-C stops it)");
    } else {
        if session::status_exists(paths, PULSE_SESSION) {
            session::reap(paths, PULSE_SESSION); // reuse the pulse id (targeted)
        }
        // Propagate the output format to the detached pulse: spawn_detached
        // re-execs this binary as `_ pulse`, which inherits our env, and
        // `util::init_format` there reads LOOOP_LOG_FORMAT to pick NDJSON vs human.
        if json {
            unsafe { std::env::set_var("LOOOP_LOG_FORMAT", "json") };
        }
        // babysit wraps `<looop-bin> _ pulse`; its detacher re-execs looop as the
        // supervisor, which spawns this command under a PTY. `_ pulse` then runs
        // the real loop (and takes the single-instance lock inside cmd_run).
        let bin = paths.bin.to_string_lossy().to_string();
        session::spawn_detached(
            paths,
            vec![bin, "_".to_string(), "pulse".to_string()],
            PULSE_SESSION,
        )?;
        // The detached supervisor needs a beat to register the session; wait so
        // the follow below doesn't race it and get `no session matching pulse`.
        session::await_alive(paths, PULSE_SESSION, Duration::from_secs(5));
        println!(
            "looop: pulse started{} — streaming (Ctrl-C stops it)",
            if json { " [json]" } else { "" }
        );
    }

    // Foreground: stream the pulse's output until Ctrl-C OR the pulse exits.
    // Either way we fall through to teardown — closing the window stops the loop.
    session::serve_follow(paths, PULSE_SESSION)?;

    // Teardown: stop the pulse AND its live workers. A worker is a detached
    // child of the loop; with the pulse gone nothing would supervise, surface,
    // or reap it, so leaving live workers behind is a surprising orphan.
    stop_all(paths)
}

/// Stop the pulse and every live worker, then reap the pulse corpse so a
/// re-`looop` starts clean. Worker transcripts are left for the retention
/// window (not nuked here). Used as the foreground teardown path.
fn stop_all(paths: &Paths) -> Result<ExitCode> {
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

    if !session::is_alive(paths, PULSE_SESSION) {
        session::prune(paths);
        println!("looop: pulse stopped");
        return Ok(ExitCode::SUCCESS);
    }
    match session::kill_quiet(paths, PULSE_SESSION) {
        Ok(()) => {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while session::is_alive(paths, PULSE_SESSION) && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            session::reap(paths, PULSE_SESSION);
            println!("looop: pulse stopped");
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => {
            util::event(util::Level::Error, "down", &e.to_string(), &[]);
            Ok(ExitCode::from(1))
        }
    }
}

/// `looop _ pulse` (internal) — the headless pulse body babysit wraps. It is just
/// the reconcile loop (`run::cmd_run`) running under a PTY; a bare `looop` is how
/// a user starts it.
pub fn cmd_pulse(paths: &Paths) -> Result<ExitCode> {
    run::cmd_run(paths)
}
