//! The pulse-as-a-service layer. Historically `looop` (no args) ran the pulse in
//! the foreground forever; you watched it scroll. The service model treats the
//! pulse as just one more babysit-supervised session — uniform with
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
        println!("looop: pulse already running — see it: looop ls");
        if watch {
            println!("looop: watching {PULSE_SESSION} (Ctrl-C to stop watching)");
            session::watch(paths, PULSE_SESSION)?;
        }
        return Ok(ExitCode::SUCCESS);
    }
    if session::status_exists(paths, PULSE_SESSION) {
        session::reap(paths, PULSE_SESSION); // reuse the pulse id (targeted)
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

    println!("looop: pulse started{}", if json { " [json]" } else { "" });
    if watch {
        // The detached supervisor needs a beat to register the session; wait so
        // we don't race it and get `no session matching pulse`.
        session::await_alive(paths, PULSE_SESSION, std::time::Duration::from_secs(5));
        println!("looop: watching {PULSE_SESSION} (Ctrl-C to stop watching)");
        session::watch(paths, PULSE_SESSION)?;
    }
    Ok(ExitCode::SUCCESS)
}

/// `looop down` — stop the pulse service. The single-instance lock left behind
/// is stale-reclaimed on the next `looop up` (cmd_run checks pid liveness), so
/// no extra cleanup is needed here.
pub fn cmd_down(paths: &Paths, args: &[String]) -> Result<ExitCode> {
    // "down" = stop looop AND everything it is running. A worker is a detached
    // child of the loop; with the pulse gone, nothing would supervise, surface,
    // or reap it, so leaving live workers behind is a surprising orphan. Kill
    // them first. --keep-workers opts out (e.g. to bounce just the pulse).
    let keep_workers = args.iter().any(|a| a == "--keep-workers");
    if !keep_workers {
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
    }

    // Only a LIVE pulse is something to stop. A leftover corpse (killed/exited)
    // isn't: prune it and report nothing-to-do, so a second `down` doesn't try
    // to re-kill a finished session and error.
    if !session::is_alive(paths, PULSE_SESSION) {
        session::prune(paths);
        println!("looop: no pulse session to stop");
        return Ok(ExitCode::SUCCESS);
    }
    match session::kill_quiet(paths, PULSE_SESSION) {
        Ok(()) => {
            // Wait for the supervisor to record the exit, then reap JUST the
            // pulse corpse so `ls` is clean and a re-`up` starts fresh — worker
            // transcripts are left for the retention window, not nuked here.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while session::is_alive(paths, PULSE_SESSION) && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(50));
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

/// `looop _pulse` (internal) — the headless pulse body babysit wraps. It is just
/// the reconcile loop (`run::cmd_run`) running under a PTY; `looop up` is how a
/// user starts it.
pub fn cmd_pulse(paths: &Paths) -> Result<ExitCode> {
    run::cmd_run(paths)
}
