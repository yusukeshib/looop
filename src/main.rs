//! looop — a tiny, portable, Kubernetes-shaped control loop for your work.
//!
//! Rust port. The pulse is unbreakable code, judgment is the AI, memory is git
//! (RULE 2). babysit is linked as a LIBRARY: list/prune/status/kill/flag/unflag/
//! attach all run in-process. The one verb that still shells out is detached
//! worker spawn (`start-session` -> `babysit run -d`), because babysit's detacher
//! re-execs its own binary as the supervisor.

mod babysit;
mod config;
mod cost;
mod deps;
mod events;
mod gate;
mod help;
mod paths;
mod prompt;
mod run;
mod runner;
mod seed;
mod sensor;
mod service;
mod session;
mod status;
mod surface;
mod tick;
mod util;
mod worldhash;

use anyhow::Result;
use paths::Paths;
use std::process::ExitCode;

fn main() -> ExitCode {
    restore_sigpipe();
    let paths = Paths::resolve();
    export_env(&paths);
    util::init_color();

    let args: Vec<String> = std::env::args().skip(1).collect();
    // A bare `looop` is invalid: with `up` / `watch` / `run` all explicit, a
    // no-arg invocation silently launching a foreground infinite loop is more
    // surprising than helpful. Require a verb (foreground pulse = `looop run`).
    let Some(cmd) = args.first().map(String::as_str) else {
        eprintln!(
            "looop: no command — try: up, down, run [<goal>], tick, ls, status, start-session, attach, kill, flag, unflag, prune, help"
        );
        return ExitCode::from(1);
    };
    let rest = &args[1..];

    let result: Result<ExitCode> = match cmd {
        "help" | "-h" | "--help" => {
            help::print(&paths);
            Ok(ExitCode::SUCCESS)
        }
        "version" | "--version" | "-V" => {
            println!("looop {}", env!("CARGO_PKG_VERSION"));
            Ok(ExitCode::SUCCESS)
        }
        // Hidden: babysit's detacher re-execs us as the headless worker
        // supervisor (`looop run --detached-id <id> -- <cmd>`). Route straight
        // to serve_worker; no deps check, no pulse.
        "run" | "loop" if rest.first().map(String::as_str) == Some("--detached-id") => {
            babysit::run_detached_worker(rest).map(|c| ExitCode::from(c.clamp(0, 255) as u8))
        }
        "run" | "loop" => deps::require_deps(&paths).and_then(|_| match rest.first() {
            Some(goal) => run::cmd_run_goal(&paths, goal),
            None => run::cmd_run(&paths),
        }),
        "tick" => deps::require_deps(&paths).and_then(|_| tick::cmd_tick(&paths)),
        // The pulse-as-a-service trio + its read-only window.
        "up" => deps::require_deps(&paths).and_then(|_| service::cmd_up(&paths)),
        "down" => service::cmd_down(&paths),
        // Hidden: the headless pulse body babysit wraps under a PTY (`looop up`).
        "_pulse" => deps::require_deps(&paths).and_then(|_| service::cmd_pulse(&paths)),
        "ls" => deps::require_deps(&paths).and_then(|_| ls_inproc(rest)),
        "status" => status::cmd_status(&paths, rest),
        "start-session" => {
            deps::require_deps(&paths).and_then(|_| session::cmd_start_session(&paths, rest))
        }
        "attach" => deps::require_deps(&paths).and_then(|_| session::cmd_attach(&paths, rest)),
        "kill" => deps::require_deps(&paths).and_then(|_| session::cmd_kill(&paths, rest)),
        "flag" => deps::require_deps(&paths).and_then(|_| session::cmd_flag(&paths, rest)),
        "unflag" => deps::require_deps(&paths).and_then(|_| session::cmd_unflag(&paths, rest)),
        "prune" => deps::require_deps(&paths).and_then(|_| session::cmd_prune(&paths, rest)),
        "cost" => cost::cmd_cost(&paths, rest),
        "_fmt" => cost::cmd_fmt(&paths),
        "_cost" => cost::cmd_cost_record(&paths, rest),
        other => {
            eprintln!(
                "looop: unknown command '{other}' (try: run, run <goal>, up, down, tick, ls, status, start-session, attach, kill, flag, unflag, prune, help)"
            );
            Ok(ExitCode::from(1))
        }
    };

    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(1)
        }
    }
}

/// Rust sets SIGPIPE to SIG_IGN at startup, which turns a closed pipe (e.g.
/// `looop status | head`) into a panic on the next write. Restore the default
/// so we exit quietly on a broken pipe (same fix babysit makes).
#[cfg(unix)]
fn restore_sigpipe() {
    const SIGPIPE: i32 = 13;
    const SIG_DFL: usize = 0;
    unsafe extern "C" {
        fn signal(signum: i32, handler: usize) -> usize;
    }
    unsafe {
        signal(SIGPIPE, SIG_DFL);
    }
}
#[cfg(not(unix))]
fn restore_sigpipe() {}

/// Export the env children rely on (sensors, workers, the runner pipeline that
/// references "$LOOOP_BIN"). Mirrors the bash `export` list.
fn export_env(paths: &Paths) {
    let set = |k: &str, v: &std::ffi::OsStr| unsafe { std::env::set_var(k, v) };
    set("LOOOP_BIN", paths.bin.as_os_str());
    set("LOOOP_DATA_DIR", paths.data_dir.as_os_str());
    set("CONFIG", paths.config.as_os_str());
    set("CLAIMS_DIR", paths.claims_dir().as_os_str());
    set("REPORTS_DIR", paths.reports_dir().as_os_str());
    set("COST_LEDGER", paths.cost_ledger().as_os_str());
    if let Some(bd) = &paths.babysit_dir {
        set("BABYSIT_DIR", bd.as_os_str());
    }
}

/// `looop ls [--json] [--watch] [--interval <dur>]` — render the fleet table
/// IN-PROCESS via the babysit library (no `babysit` binary). Parses the same
/// flags babysit ls accepts.
fn ls_inproc(rest: &[String]) -> Result<ExitCode> {
    let mut json = false;
    let mut watch = false;
    let mut interval = "2s".to_string();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--json" => json = true,
            "--watch" | "-w" => watch = true,
            "--interval" | "-n" => {
                if let Some(v) = it.next() {
                    interval = v.clone();
                }
            }
            _ => {} // ignore unknown flags
        }
    }
    match babysit::ls(json, watch, interval) {
        Ok(()) => Ok(ExitCode::SUCCESS),
        Err(_) => Ok(ExitCode::from(1)),
    }
}
