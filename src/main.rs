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
//! the pulse (cmd is `looop _ pulse`, the reconcile-loop body).

mod config;
mod cost;
mod deps;
mod events;
mod executor;
mod gate;
mod help;
mod mailbox;
mod paths;
mod run;
mod seed;
mod sensor;
mod service;
mod session;
mod tick;
mod util;
mod watch;
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
    // A bare `looop` is no longer a command: the loop runs as the `looop up`
    // service (pulse + root agent). With no verb, show the manual.
    let Some(cmd) = args.first().map(String::as_str) else {
        help::print(&paths);
        return ExitCode::SUCCESS;
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
        // Service control: bring the pulse up / tear it (and workers) down. The
        // root agent is a pi/claude session YOU run separately, not looop-managed.
        "up" => deps::require_deps(&paths).and_then(|_| service::cmd_up(&paths, rest)),
        "down" => deps::require_deps(&paths).and_then(|_| service::cmd_down(&paths)),
        // Read-only observer TUI: tail the colored log of any running session
        // (pulse or worker) with a live selector. No deps gate — it only reads
        // logs + lists sessions, never launches an agent.
        "watch" => watch::cmd_watch(&paths, rest),
        // Machine-facing verbs, grouped under `_`. Two audiences: the ROOT AGENT
        // (tick/answer/goal/sensor/playbook/run/notify/worker) and the WORKER
        // self-callbacks (ask/kill/claim/unclaim/cost). `_ pulse` is looop's own
        // detached spawn. None are human-facing — humans use `up`/`down`.
        "_" => {
            match rest.first().map(String::as_str) {
                Some("pulse") => {
                    deps::require_deps(&paths).and_then(|_| service::cmd_pulse(&paths))
                }
                // Root agent: read state (now / blocking) + drive the world.
                Some("state") => {
                    deps::require_deps(&paths).and_then(|_| tick::cmd_state(&paths, &rest[1..]))
                }
                Some("wait") => {
                    deps::require_deps(&paths).and_then(|_| tick::cmd_wait(&paths, &rest[1..]))
                }
                Some("answer") => {
                    deps::require_deps(&paths).and_then(|_| mailbox::cmd_answer(&paths, &rest[1..]))
                }
                Some("goal") => {
                    deps::require_deps(&paths).and_then(|_| executor::cmd_goal(&paths, &rest[1..]))
                }
                Some("sensor") => deps::require_deps(&paths)
                    .and_then(|_| executor::cmd_sensor(&paths, &rest[1..])),
                Some("playbook") => deps::require_deps(&paths)
                    .and_then(|_| executor::cmd_playbook(&paths, &rest[1..])),
                Some("run") => {
                    deps::require_deps(&paths).and_then(|_| executor::cmd_run(&paths, &rest[1..]))
                }
                Some("notify") => deps::require_deps(&paths)
                    .and_then(|_| executor::cmd_notify(&paths, &rest[1..])),
                Some("worker") => match rest.get(1).map(String::as_str) {
                    Some("start") => deps::require_deps(&paths)
                        .and_then(|_| executor::cmd_worker_start(&paths, &rest[2..])),
                    Some("kill") => deps::require_deps(&paths)
                        .and_then(|_| session::cmd_kill(&paths, &rest[2..])),
                    other => {
                        eprintln!("looop _ worker: unknown subverb {other:?} (start, kill)");
                        Ok(ExitCode::from(1))
                    }
                },
                // Worker self-callbacks (auto-injected CONTRACT).
                Some("ask") => {
                    deps::require_deps(&paths).and_then(|_| mailbox::cmd_ask(&paths, &rest[1..]))
                }
                Some("kill") => {
                    deps::require_deps(&paths).and_then(|_| session::cmd_kill(&paths, &rest[1..]))
                }
                Some("claim") => {
                    deps::require_deps(&paths).and_then(|_| gate::cmd_claim(&paths, &rest[1..]))
                }
                Some("unclaim") => {
                    deps::require_deps(&paths).and_then(|_| gate::cmd_unclaim(&paths, &rest[1..]))
                }
                Some("cost") => cost::cmd_cost_record(&paths, &rest[1..]),
                other => {
                    eprintln!(
                        "looop _: unknown internal verb {other:?} (root: state, wait, answer, goal, sensor, playbook, run, notify, worker; worker: ask, kill, claim, unclaim, cost; pulse)"
                    );
                    Ok(ExitCode::from(1))
                }
            }
        }
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
        "cost" => cost::cmd_cost(&paths, rest),
        other => {
            eprintln!(
                "looop: unknown command '{other}' (up, down, watch, cost, config, version, help)"
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
