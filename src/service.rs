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

use crate::paths::Paths;
use crate::session::{self, PULSE_SESSION};
use crate::{run, util};
use anyhow::Result;
use std::process::ExitCode;

/// Parsed `looop up` flags. A tiny typed surface so the command rejects
/// unknown arguments instead of silently ignoring them.
struct UpOpts {
    watch: bool,
    json: bool,
}

impl UpOpts {
    /// Parse `up`'s argv, returning `Err(bad_arg)` on the first unknown token.
    fn parse(args: &[String]) -> std::result::Result<Self, String> {
        let mut o = UpOpts {
            watch: false,
            json: false,
        };
        for a in args {
            match a.as_str() {
                "--watch" | "-w" => o.watch = true,
                "--json" => o.json = true,
                other => return Err(other.to_string()),
            }
        }
        Ok(o)
    }
}

/// `looop up [--watch] [--json]` — ensure the pulse is running as a detached
/// service. Idempotent: a live pulse is left alone; a dead corpse is pruned so
/// its id can be reused. `--json` makes the detached pulse emit NDJSON to its
/// output.log (machine-readable for an agent). `--watch` follows that output
/// after starting (Ctrl-C to stop the window; the pulse keeps running).
pub fn cmd_up(paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let opts = match UpOpts::parse(args) {
        Ok(o) => o,
        Err(bad) => {
            eprintln!("looop up: unknown option '{bad}' (expected --watch/-w and/or --json)");
            return Ok(ExitCode::from(1));
        }
    };
    let UpOpts { watch, json } = opts;

    if session::is_alive(paths, PULSE_SESSION) {
        println!("looop: pulse already running ({PULSE_SESSION}) — see it: looop ls");
        if watch {
            println!("looop: watching {PULSE_SESSION} (Ctrl-C to stop watching)");
            session::watch(paths, PULSE_SESSION)?;
        }
        return Ok(ExitCode::SUCCESS);
    }
    if session::status_exists(paths, PULSE_SESSION) {
        session::prune(paths); // reuse the id held by a dead corpse
    }

    // Propagate the output format to the detached pulse: spawn_detached re-execs
    // this binary as `_pulse`, which inherits our env, and `util::init_format`
    // there reads LOOOP_LOG_FORMAT to pick NDJSON vs human.
    if json {
        unsafe { std::env::set_var("LOOOP_LOG_FORMAT", "json") };
    }

    // babysit wraps `<looop-bin> _pulse`; its detacher re-execs looop as the
    // supervisor, which spawns this command under a PTY. `_pulse` then runs the
    // real loop (and takes the single-instance lock inside cmd_run).
    let bin = paths.bin.to_string_lossy().to_string();
    session::spawn_detached(paths, vec![bin, "_pulse".to_string()], PULSE_SESSION)?;

    println!(
        "looop: pulse started ({PULSE_SESSION}){}",
        if json { " [json]" } else { "" }
    );
    println!("  see:   {}looop ls", paths.looop_hint_env());
    println!("  watch: {}looop watch pulse", paths.looop_hint_env());
    println!("  stop:  {}looop down", paths.looop_hint_env());
    if watch {
        println!("looop: watching {PULSE_SESSION} (Ctrl-C to stop watching)");
        session::watch(paths, PULSE_SESSION)?;
    }
    Ok(ExitCode::SUCCESS)
}

/// `looop down` — stop the pulse service. The single-instance lock left behind
/// is stale-reclaimed on the next `looop up` (cmd_run checks pid liveness), so
/// no extra cleanup is needed here.
pub fn cmd_down(paths: &Paths) -> Result<ExitCode> {
    if !session::status_exists(paths, PULSE_SESSION) {
        println!("looop: no pulse session to stop");
        return Ok(ExitCode::SUCCESS);
    }
    match session::kill(paths, PULSE_SESSION) {
        Ok(()) => {
            session::prune(paths);
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
