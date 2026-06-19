//! looop — a tiny, portable, Kubernetes-shaped control loop for your work.
//!
//! Rust port. The pulse is unbreakable code, judgment is the AI, memory is the
//! files in the data dir (RULE 2). babysit is linked as a LIBRARY and driven
//! entirely in-process —
//! list/prune/status/kill/flag/unflag/attach AND detached spawn all run through
//! the library, no `babysit` binary. The one process re-exec is babysit's
//! detacher re-execing looop itself (current_exe) as the headless session
//! supervisor (`looop run --detached-id <id> -- <cmd>`). That ONE path
//! supervises both kinds of detached session: a worker (cmd is the agent) and
//! the pulse (cmd is `looop _pulse`, the reconcile-loop body).

mod config;
mod cost;
mod deps;
mod events;
mod executor;
mod gate;
mod help;
mod journal;
mod paths;
mod prompt;
mod run;
mod runner;
mod seed;
mod sensor;
mod service;
mod session;
mod status;
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
    util::init_format();
    util::init_color();

    let args: Vec<String> = std::env::args().skip(1).collect();
    // A bare `looop` (or `looop --json`) IS the command: it brings the pulse up
    // as a session, streams it, and tears it down on exit. There is no detached
    // up/down pair anymore — the foreground invocation owns the loop's lifecycle.
    let Some(cmd) = args.first().map(String::as_str) else {
        return match deps::require_deps(&paths).and_then(|_| service::cmd_serve(&paths, &[])) {
            Ok(code) => code,
            Err(e) => {
                eprintln!("{e}");
                ExitCode::from(1)
            }
        };
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
        // Hidden: babysit's detacher re-execs us as the headless session
        // supervisor (`looop run --detached-id <id> -- <cmd>`), for BOTH workers
        // and the pulse. babysit hard-codes the `run` verb. Route straight to
        // the supervisor; no deps check, no pulse.
        "run" if rest.first().map(String::as_str) == Some("--detached-id") => {
            session::run_detached_worker(rest).map(|c| ExitCode::from(c.clamp(0, 255) as u8))
        }
        // A leading flag with no verb is still the foreground serve (`looop --json`).
        "--json" => deps::require_deps(&paths).and_then(|_| service::cmd_serve(&paths, &args)),
        // The pulse foreground + its read-only window.
        "watch" => deps::require_deps(&paths).and_then(|_| session::cmd_watch(&paths, rest)),
        "log" | "logs" => deps::require_deps(&paths).and_then(|_| session::cmd_log(&paths, rest)),
        "shot" | "screenshot" => {
            deps::require_deps(&paths).and_then(|_| session::cmd_screenshot(&paths, rest))
        }
        "send" => deps::require_deps(&paths).and_then(|_| session::cmd_send(&paths, rest)),
        "key" => deps::require_deps(&paths).and_then(|_| session::cmd_key(&paths, rest)),
        "expect" => deps::require_deps(&paths).and_then(|_| session::cmd_expect(&paths, rest)),
        "wait" => deps::require_deps(&paths).and_then(|_| session::cmd_wait(&paths, rest)),
        "wait-idle" => {
            deps::require_deps(&paths).and_then(|_| session::cmd_wait_idle(&paths, rest))
        }
        "resize" => deps::require_deps(&paths).and_then(|_| session::cmd_resize(&paths, rest)),
        "restart" => deps::require_deps(&paths).and_then(|_| session::cmd_restart(&paths, rest)),
        "detach" => deps::require_deps(&paths).and_then(|_| session::cmd_detach(&paths, rest)),
        // Hidden: the headless pulse body babysit wraps under a PTY (a bare `looop`).
        "_pulse" => deps::require_deps(&paths).and_then(|_| service::cmd_pulse(&paths)),
        "ls" => deps::require_deps(&paths).and_then(|_| ls_inproc(&paths, rest)),
        "status" => status::cmd_status(&paths, rest),
        "start-session" => {
            deps::require_deps(&paths).and_then(|_| session::cmd_start_session(&paths, rest))
        }
        "attach" => deps::require_deps(&paths).and_then(|_| session::cmd_attach(&paths, rest)),
        "kill" => deps::require_deps(&paths).and_then(|_| session::cmd_kill(&paths, rest)),
        "flag" => deps::require_deps(&paths).and_then(|_| session::cmd_flag(&paths, rest)),
        "unflag" => deps::require_deps(&paths).and_then(|_| session::cmd_unflag(&paths, rest)),
        "prune" => deps::require_deps(&paths).and_then(|_| session::cmd_prune(&paths, rest)),
        "config" => match rest.first().map(String::as_str) {
            Some("zsh") => {
                print!("{}", include_str!("completions/looop.zsh"));
                Ok(ExitCode::SUCCESS)
            }
            Some("bash") => {
                print!("{}", include_str!("completions/looop.bash"));
                Ok(ExitCode::SUCCESS)
            }
            _ => {
                eprintln!("looop config: specify a shell — zsh or bash");
                eprintln!("  zsh:  eval \"$(looop config zsh)\"");
                eprintln!("  bash: eval \"$(looop config bash)\"");
                Ok(ExitCode::from(1))
            }
        },
        "journal" => journal::cmd_journal(&paths, rest),
        "cost" => cost::cmd_cost(&paths, rest),
        "_fmt" => cost::cmd_fmt(&paths),
        "_cost" => cost::cmd_cost_record(&paths, rest),
        other => {
            eprintln!(
                "looop: unknown command '{other}' (run `looop` to start the loop; or: watch <id>, ls, status, journal, log, shot, send, key, expect, wait, wait-idle, resize, restart, attach, detach, kill, flag, unflag, prune, help)"
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
    // All looop-owned env lives under the LOOOP_ namespace (M1): bare CONFIG /
    // CLAIMS_DIR / REPORTS_DIR / COST_LEDGER collided with whatever the child
    // (sensors, workers, the runner pipeline) already had in scope. Exporting
    // LOOOP_CONFIG also keeps children pinned to the same resolved wiring as the
    // parent (Paths::resolve reads it as the override), so a worker that re-invokes
    // looop stays on this profile's config.
    set("LOOOP_CONFIG", paths.config.as_os_str());
    set("LOOOP_CLAIMS_DIR", paths.claims_dir().as_os_str());
    set("LOOOP_REPORTS_DIR", paths.reports_dir().as_os_str());
    set("LOOOP_COST_LEDGER", paths.cost_ledger().as_os_str());
    // NB: no $BABYSIT_DIR. looop never configures the babysit library through the
    // environment — it passes an explicit context (`paths.sessions()`) to every
    // call, and the detached worker receives its root via `--root`.
}

/// `looop ls [--json] [--watch] [--interval <dur>]` — render the fleet table
/// IN-PROCESS via the babysit library (no `babysit` binary). Parses the same
/// flags babysit ls accepts.
fn ls_inproc(paths: &Paths, rest: &[String]) -> Result<ExitCode> {
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
    match session::ls(paths, json, watch, interval) {
        Ok(()) => Ok(ExitCode::SUCCESS),
        Err(_) => Ok(ExitCode::from(1)),
    }
}

#[cfg(test)]
mod tests {
    /// `looop config zsh` must emit a script that registers the completion
    /// (the `#compdef` autoload tag plus the live `compdef` call), so a bare
    /// `eval "$(looop config zsh)"` actually wires up completion.
    #[test]
    fn zsh_completion_registers_itself() {
        let s = include_str!("completions/looop.zsh");
        assert!(s.contains("#compdef looop"), "missing #compdef tag");
        assert!(s.contains("compdef _looop looop"), "missing compdef call");
    }

    /// `looop config bash` must emit a script that registers the completion via
    /// `complete -F`, so `eval "$(looop config bash)"` wires it up.
    #[test]
    fn bash_completion_registers_itself() {
        let s = include_str!("completions/looop.bash");
        assert!(
            s.contains("complete -F _looop looop"),
            "missing complete -F registration"
        );
    }
}
