//! looop — a tiny, portable, Kubernetes-shaped control loop for your work.
//!
//! Rust port. The pulse is unbreakable code, judgment is the AI, memory is git
//! (RULE 2). babysit is still shelled out (step 2); a future step links it as a
//! lib for a single-binary build + `looop attach`-style verbs.

mod babysit;
mod config;
mod cost;
mod deps;
mod events;
mod gate;
mod help;
mod paths;
mod playbook;
mod prompt;
mod run;
mod runner;
mod seed;
mod sensor;
mod session;
mod status;
mod surface;
mod tick;
mod util;
mod worldhash;

use anyhow::Result;
use paths::Paths;
use std::process::{Command, ExitCode, Stdio};

fn main() -> ExitCode {
    restore_sigpipe();
    let paths = Paths::resolve();
    export_env(&paths);
    util::init_color();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("run");
    let rest = if args.is_empty() { &[][..] } else { &args[1..] };

    let result: Result<ExitCode> = match cmd {
        "help" | "-h" | "--help" => {
            help::print(&paths);
            Ok(ExitCode::SUCCESS)
        }
        "version" | "--version" | "-V" => {
            println!("looop {}", env!("CARGO_PKG_VERSION"));
            Ok(ExitCode::SUCCESS)
        }
        "run" | "loop" => deps::require_deps(&paths).and_then(|_| match rest.first() {
            Some(goal) => run::cmd_run_goal(&paths, goal),
            None => run::cmd_run(&paths),
        }),
        "tick" => deps::require_deps(&paths).and_then(|_| tick::cmd_tick(&paths)),
        "ls" => deps::require_deps(&paths).map(|_| ls_passthrough(rest)),
        "status" => status::cmd_status(&paths, rest),
        "start-session" => {
            deps::require_deps(&paths).and_then(|_| session::cmd_start_session(&paths, rest))
        }
        "cost" => cost::cmd_cost(&paths, rest),
        "playbook" => playbook::cmd_playbook(&paths, rest),
        "_fmt" => cost::cmd_fmt(&paths),
        "_cost" => cost::cmd_cost_record(&paths, rest),
        other => {
            eprintln!(
                "looop: unknown command '{other}' (try: run, run <goal>, tick, ls, status, playbook, start-session, help)"
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

/// `looop ls [opts]` => `babysit ls [opts]` (BABYSIT_DIR already scoped to this
/// profile via export_env). Passes through --watch/--interval/etc.
fn ls_passthrough(rest: &[String]) -> ExitCode {
    let status = Command::new("babysit")
        .arg("ls")
        .args(rest)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status();
    match status {
        Ok(s) => ExitCode::from(s.code().unwrap_or(1) as u8),
        Err(_) => ExitCode::from(1),
    }
}
