//! The pulse-as-a-service layer. Historically `looop` (no args) ran the pulse in
//! the foreground forever; you watched it scroll. The service model treats the
//! pulse (the 親玉) as just one more babysit-supervised session — uniform with
//! the workers — so a separate `looop watch` window can tail it the same way it
//! tails any agent, and the pulse keeps running when you close the window.
//!
//!   looop up     spawn the pulse as a detached babysit session (if not running)
//!   looop down   stop the pulse session
//!   looop _pulse the headless pulse body babysit actually wraps (internal)
//!
//! `_pulse` is just the existing foreground loop (`run::cmd_run`): under babysit
//! it runs in a PTY, so its colored `util::log` output is captured to the
//! session's `output.log`. Watch it live with `looop attach pulse`, or check
//! the fleet with `looop ls`.

use crate::babysit::{self, PULSE_SESSION};
use crate::paths::Paths;
use crate::{run, util};
use anyhow::Result;
use std::process::ExitCode;

/// `looop up` — ensure the pulse is running as a detached service. Idempotent:
/// if a live pulse session already exists we leave it alone; a dead corpse is
/// pruned so its id can be reused.
pub fn cmd_up(paths: &Paths) -> Result<ExitCode> {
    if babysit::is_alive(PULSE_SESSION) {
        println!("looop: pulse already running ({PULSE_SESSION}) — see it: looop ls");
        return Ok(ExitCode::SUCCESS);
    }
    if babysit::status_exists(PULSE_SESSION) {
        babysit::prune(); // reuse the id held by a dead corpse
    }

    // babysit wraps `<looop-bin> _pulse`; its detacher re-execs looop as the
    // supervisor, which spawns this command under a PTY. `_pulse` then runs the
    // real loop (and takes the single-instance lock inside cmd_run).
    let bin = paths.bin.to_string_lossy().to_string();
    babysit::spawn_detached(vec![bin, "_pulse".to_string()], PULSE_SESSION)?;

    println!("looop: pulse started ({PULSE_SESSION})");
    println!("  see:   {}looop ls", paths.looop_hint_env());
    println!("  watch: {}looop attach pulse", paths.looop_hint_env());
    println!("  stop:  {}looop down", paths.looop_hint_env());
    Ok(ExitCode::SUCCESS)
}

/// `looop down` — stop the pulse service. The single-instance lock left behind
/// is stale-reclaimed on the next `looop up` (cmd_run checks pid liveness), so
/// no extra cleanup is needed here.
pub fn cmd_down(_paths: &Paths) -> Result<ExitCode> {
    if !babysit::status_exists(PULSE_SESSION) {
        println!("looop: no pulse session to stop");
        return Ok(ExitCode::SUCCESS);
    }
    match babysit::kill(PULSE_SESSION) {
        Ok(()) => {
            babysit::prune();
            println!("looop: pulse stopped");
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => {
            util::log(&format!("{}down: {e}{}", util::red(), util::rst()));
            Ok(ExitCode::from(1))
        }
    }
}

/// `looop _pulse` (internal) — the headless pulse body babysit wraps. Identical
/// to the foreground `looop run`; the only difference is who owns the terminal.
pub fn cmd_pulse(paths: &Paths) -> Result<ExitCode> {
    run::cmd_run(paths)
}
