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

mod cli;
mod client;
mod config;
mod contract;
mod deps;
mod events;
mod executor;
mod fmt;
mod gate;
mod help;
mod init;
mod mailbox;
mod paths;
mod prompt;
mod run;
mod runner;
mod seed;
mod sensor;
mod service;
mod session;
mod store;
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

    let raw: Vec<String> = std::env::args().skip(1).collect();

    // PRE-CLAP shortcut (the ONE path that bypasses clap): babysit's detacher
    // re-execs us as the headless session supervisor (`looop run --detached-id
    // <id> … -- <cmd>`), for BOTH workers and the pulse. babysit hard-codes the
    // `run` verb and may pass flags THIS version doesn't know; that argv must
    // tolerate unknown flags (forward-compat), the opposite of clap's strict
    // rejection — so it never reaches clap. No deps check, no pulse.
    if raw.first().map(String::as_str) == Some("run")
        && raw.get(1).map(String::as_str) == Some("--detached-id")
    {
        return match session::run_detached_worker(&raw[1..]) {
            Ok(c) => ExitCode::from(c.clamp(0, 255) as u8),
            Err(e) => {
                eprintln!("{e}");
                ExitCode::from(1)
            }
        };
    }

    // PRE-CLAP shortcut: a TOP-LEVEL `-h`/`--help` (like a bare `looop` or the
    // `help` verb) shows our hand-written manual, NOT clap's terse auto-help —
    // `looop --help` is the front door and must stay the full manual. Only the
    // top-level flag is intercepted: `looop <verb> --help` still falls through
    // to clap so every subcommand keeps its own (non-destructive) help.
    if matches!(raw.first().map(String::as_str), Some("-h") | Some("--help")) {
        help::print(&paths);
        return ExitCode::SUCCESS;
    }

    use clap::Parser;
    let cli = match cli::Cli::try_parse() {
        Ok(c) => c,
        Err(e) => {
            // Remap clap's exit codes to looop's convention: usage/parse errors
            // exit 1 (not clap's default 2); `--help`/`--version` still exit 0.
            let _ = e.print();
            return ExitCode::from(if e.use_stderr() { 1 } else { 0 });
        }
    };

    let result: Result<ExitCode> = dispatch(&paths, cli.cmd);

    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(1)
        }
    }
}

/// Route a parsed command to its handler. The deps gate wraps every verb that
/// actually touches the loop's tools; read-only/meta verbs (watch, help,
/// version) skip it, matching the pre-clap wiring.
fn dispatch(paths: &Paths, cmd: Option<cli::Cmd>) -> Result<ExitCode> {
    use cli::{Cmd, GoalOp, PlaybookOp, SensorOp, Verb, WorkerOp};

    // A bare `looop` is not a command (the loop runs as the `looop up` service).
    // With no verb, show clap's auto-generated SHORT command summary — the long
    // hand-written manual is reserved for the explicit `looop help` / `--help`
    // front door. (clap derives this from cli.rs, so it never drifts.)
    let Some(cmd) = cmd else {
        use clap::CommandFactory;
        let _ = cli::Cli::command().print_help();
        println!();
        return Ok(ExitCode::SUCCESS);
    };

    let gated = |f: &dyn Fn() -> Result<ExitCode>| deps::require_deps(paths).and_then(|_| f());

    match cmd {
        Cmd::Help => {
            help::print(paths);
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Version => {
            println!("looop {}", env!("CARGO_PKG_VERSION"));
            Ok(ExitCode::SUCCESS)
        }
        // Not gated: `looop init` configures the runner BEFORE its CLI need be
        // installed, so we must not preflight the runner binary here.
        Cmd::Init => init::cmd_init(paths),
        // Not gated here: cmd_up checks `init` FIRST (so an uninitialized user is
        // told to run `looop init`, not nagged about a missing runner they may
        // not even want), then runs the deps preflight itself.
        Cmd::Up(a) => service::cmd_up(paths, a.json),
        Cmd::Down => gated(&|| service::cmd_down(paths)),
        // Read-only observer TUI — no deps gate (only reads logs + lists
        // sessions, never launches an agent).
        Cmd::Watch(a) => watch::cmd_watch(paths, &a),
        // Non-agent client TUI — gated (it resolves asks, a contract write).
        Cmd::Client => gated(&|| client::cmd_client(paths)),
        Cmd::Underscore { verb } => match verb {
            Verb::Pulse => gated(&|| service::cmd_pulse(paths)),
            Verb::State(a) => gated(&|| tick::cmd_state(paths, a.json)),
            Verb::Wait(a) => gated(&|| tick::cmd_wait(paths, &a)),
            Verb::Asks(a) => gated(&|| mailbox::cmd_asks(paths, a.json)),
            Verb::Answer(a) => gated(&|| mailbox::cmd_answer(paths, &a)),
            Verb::Goal(a) => gated(&|| match &a.op {
                GoalOp::Write { id, body, journal } => {
                    executor::write_goal(paths, id, body, journal.journal.as_deref())
                }
                GoalOp::Archive { id, journal } => {
                    executor::archive_goal(paths, id, journal.journal.as_deref())
                }
            }),
            Verb::Sensor(a) => gated(&|| {
                let SensorOp::Write {
                    name,
                    script,
                    journal,
                } = &a.op;
                executor::write_sensor(paths, name, script, journal.journal.as_deref())
            }),
            Verb::Playbook(a) => gated(&|| {
                let PlaybookOp::Write { body, journal } = &a.op;
                executor::write_playbook(paths, body, journal.journal.as_deref())
            }),
            Verb::Run(a) => gated(&|| executor::cmd_run(paths, &a)),
            Verb::Worker(a) => gated(&|| match &a.op {
                WorkerOp::Start {
                    id,
                    prompt,
                    model,
                    thinking,
                    journal,
                } => executor::start_worker(
                    paths,
                    id,
                    prompt,
                    model.as_deref(),
                    thinking.as_deref(),
                    journal.journal.as_deref(),
                ),
                WorkerOp::Kill { id } => session::cmd_kill(paths, id),
            }),
            Verb::Ask(a) => gated(&|| mailbox::cmd_ask(paths, &a)),
            Verb::Kill(a) => gated(&|| session::cmd_kill(paths, &a.id)),
            Verb::Send(a) => gated(&|| session::cmd_send(paths, &a)),
            Verb::Screenshot(a) => gated(&|| session::cmd_screenshot(paths, &a)),
            Verb::Claim(a) => gated(&|| gate::cmd_claim(paths, &a)),
            Verb::Unclaim(a) => gated(&|| gate::cmd_unclaim(paths, &a)),
        },
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
    // CLAIMS_DIR / REPORTS_DIR collided with whatever the child
    // (sensors, workers, the runner pipeline) already had in scope. Exporting
    // LOOOP_CONFIG also keeps children pinned to the same resolved wiring as the
    // parent (Paths::resolve reads it as the override), so a worker that re-invokes
    // looop stays on this profile's config.
    set("LOOOP_CONFIG", paths.config.as_os_str());
    set("LOOOP_CLAIMS_DIR", paths.claims_dir().as_os_str());
    set("LOOOP_REPORTS_DIR", paths.reports_dir().as_os_str());
    // NB: no $BABYSIT_DIR. looop never configures the babysit library through the
    // environment — it passes an explicit context (`paths.sessions()`) to every
    // call, and the detached worker receives its root via `--root`.
}
