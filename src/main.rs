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
//! the pulse (cmd is `looop pulse`, the reconcile-loop body).

mod cli;
mod config;
mod contract;
mod deps;
mod events;
mod executor;
mod fmt;
mod gate;
mod help;
mod init;
mod logview;
mod mailbox;
mod observe;
mod paths;
mod prompt;
mod run;
mod runner;
mod schedule;
mod seed;
mod sensor;
mod service;
mod session;
mod shell_guard;
mod shellinit;
mod statefile;
mod store;
mod tick;
mod tick_guards;
mod util;
mod verify;
mod wal;
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

    let mut raw: Vec<String> = std::env::args().skip(1).collect();

    // BACK-COMPAT shim: the plumbing verbs used to live under a `_` namespace
    // (`looop _ state`, `looop _ ask`, …). They are top-level now, but prompts,
    // playbooks and running workers seeded by older versions still say `_` —
    // silently strip a leading `_` so both spellings work.
    if raw.first().map(String::as_str) == Some("_") {
        raw.remove(0);
    }

    // PRE-CLAP shortcut (the ONE path that bypasses clap): babysit's detacher
    // re-execs us as the headless session supervisor (`looop run --detached-id
    // <id> … -- <cmd>`), for BOTH workers and the pulse. babysit hard-codes the
    // `run` verb and may pass flags THIS version doesn't know; that argv must
    // tolerate unknown flags (forward-compat), the opposite of clap's strict
    // rejection — so it never reaches clap. No deps check, no pulse.
    if is_detached_run(&raw) {
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
    let cli = match cli::Cli::try_parse_from(
        std::iter::once("looop".to_string()).chain(raw.iter().cloned()),
    ) {
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
            // `{e:#}` renders the WHOLE anyhow cause chain ("open failed:
            // permission denied"), not just the top message — the chain is
            // where the actionable detail usually lives.
            eprintln!("{}", diagnostic(&format!("{e:#}")));
            ExitCode::from(1)
        }
    }
}

/// Prefix a dispatch error with `looop: ` like every other diagnostic —
/// without double-prefixing errors that already carry it (e.g. the deps
/// preflight message starts with `looop:` itself).
fn diagnostic(msg: &str) -> String {
    if msg.starts_with("looop:") {
        msg.to_string()
    } else {
        format!("looop: {msg}")
    }
}

/// Which verbs actually LAUNCH the configured runner, and therefore must
/// pass the deps preflight ([`deps::require_deps`]) before running:
/// `pulse` (the per-beat tick command) and `worker start` (the worker
/// command). Everything else — teardown (`down`/`kill`), reads
/// (`state`/`wait`/`asks`), the mailbox/lease/schedule contract, and pure
/// filesystem writes (goal/sensor/playbook) — touches only the data dir and
/// babysit sessions, so it runs UNGATED: `looop down` must work exactly when
/// the operator removed the runner, and machine consumers (`state --json`,
/// `answer`) must not break because the runner is momentarily off PATH.
/// (`up` is deliberately absent: cmd_up checks `init` FIRST, then runs the
/// preflight itself — see the dispatch arm's comment.)
///
/// Exhaustive match, NO wildcard arm: adding a verb forces an explicit
/// gated-or-not decision here (the compile error is the drift guard).
fn needs_runner(cmd: &cli::Cmd) -> bool {
    use cli::{Cmd, WorkerOp};
    match cmd {
        Cmd::Pulse => true,
        Cmd::Worker(a) => matches!(a.op, WorkerOp::Start { .. }),
        Cmd::Help { .. }
        | Cmd::Version
        | Cmd::Init
        | Cmd::Up(_)
        | Cmd::Down
        | Cmd::Watch(_)
        | Cmd::State(_)
        | Cmd::Wait(_)
        | Cmd::Asks(_)
        | Cmd::Answer(_)
        | Cmd::Goal(_)
        | Cmd::Sensor(_)
        | Cmd::Playbook(_)
        | Cmd::Run(_)
        | Cmd::Ask(_)
        | Cmd::Tell(_)
        | Cmd::Told(_)
        | Cmd::Schedule(_)
        | Cmd::Kill(_)
        | Cmd::Screenshot(_)
        | Cmd::Claim(_)
        | Cmd::Unclaim(_)
        | Cmd::Config(_) => false,
    }
}

/// Route a parsed command to its handler. The deps gate wraps ONLY the verbs
/// that actually launch the configured runner (see [`needs_runner`]);
/// everything filesystem/session-only runs ungated so teardown, reads, and
/// the machine contract keep working when the runner binary is missing.
fn dispatch(paths: &Paths, cmd: Option<cli::Cmd>) -> Result<ExitCode> {
    use cli::{Cmd, GoalOp, PlaybookOp, SensorOp, WorkerOp};

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

    // The runner preflight, applied ONCE up front to the runner-launching
    // verbs (needs_runner) instead of per-arm — so the gated set lives in one
    // testable predicate rather than scattered wrappers.
    if needs_runner(&cmd) {
        deps::require_deps(paths)?;
    }

    match cmd {
        Cmd::Help { topic } => {
            if topic.is_empty() {
                help::print(paths);
                return Ok(ExitCode::SUCCESS);
            }
            // `looop help worker` used to be a clap error; be a front door
            // instead: a KNOWN topic renders that subcommand's own help right
            // here (not just a pointer the user must re-type); an unknown one
            // lists the real topics instead of suggesting a command that would
            // itself error.
            use clap::CommandFactory;
            // Walk the full topic chain in ONE pass: `looop help worker start`
            // descends into the `worker` subcommand and then its `start`
            // subcommand, matching the `trailing_var_arg` the CLI accepts (the
            // old code only looked at topic[0], dropping nested topics). The
            // helper fn does the whole traversal mutably — returning `None` on
            // an unknown step — so there is no validate-then-expect double
            // walk. (A fn also sidesteps the borrow-checker's overlapping-&mut
            // rejection on inline loops, E0499.)
            let mut root = cli::Cli::command();
            match descend_help(&mut root, &topic) {
                Some(cmd) => {
                    let _ = cmd.print_help();
                    Ok(ExitCode::SUCCESS)
                }
                None => {
                    let topics: Vec<String> = cli::Cli::command()
                        .get_subcommands()
                        .map(|s| s.get_name().to_string())
                        .collect();
                    eprintln!("looop help: unknown topic `{}`", topic.join(" "));
                    eprintln!("topics: {}", topics.join(", "));
                    Ok(ExitCode::from(1))
                }
            }
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
        Cmd::Down => service::cmd_down(paths),
        // Read-only observer: no runner preflight; it only lists sessions and
        // replays their existing PTY logs.
        Cmd::Watch(a) => watch::cmd_watch(paths, &a),
        Cmd::Pulse => service::cmd_pulse(paths),
        Cmd::State(a) => observe::cmd_state(paths, a.json),
        Cmd::Wait(a) => observe::cmd_wait(paths, &a),
        Cmd::Asks(a) => mailbox::cmd_asks(paths, a.json),
        Cmd::Answer(a) => mailbox::cmd_answer(paths, &a),
        Cmd::Goal(a) => match &a.op {
            GoalOp::Write { id, body, journal } => {
                executor::write_goal(paths, id, body, journal.journal.as_deref())
            }
            GoalOp::Archive { id, journal } => {
                executor::archive_goal(paths, id, journal.journal.as_deref())
            }
        },
        Cmd::Sensor(a) => {
            let SensorOp::Write {
                name,
                script,
                journal,
            } = &a.op;
            executor::write_sensor(paths, name, script, journal.journal.as_deref())
        }
        Cmd::Playbook(a) => {
            let PlaybookOp::Write { body, journal } = &a.op;
            executor::write_playbook(paths, body, journal.journal.as_deref())
        }
        Cmd::Run(a) => executor::cmd_run(paths, &a),
        Cmd::Worker(a) => match &a.op {
            WorkerOp::Start {
                id,
                prompt,
                command,
                verify,
                journal,
            } => executor::start_worker(
                paths,
                id,
                prompt,
                command.as_deref(),
                verify.as_deref(),
                journal.journal.as_deref(),
            ),
            WorkerOp::Kill { id } => session::cmd_kill(paths, id),
            WorkerOp::List {
                json,
                all,
                watch,
                interval,
            } => session::cmd_worker_list(paths, *json, *all, *watch, *interval),
        },
        Cmd::Ask(a) => mailbox::cmd_ask(paths, &a),
        Cmd::Tell(a) => mailbox::cmd_tell(paths, &a),
        Cmd::Told(a) => mailbox::cmd_told(paths, &a),
        Cmd::Schedule(a) => schedule::cmd_schedule(paths, &a),
        Cmd::Kill(a) => session::cmd_kill(paths, &a.id),
        Cmd::Screenshot(a) => session::cmd_screenshot(paths, &a),
        Cmd::Claim(a) => gate::cmd_claim(paths, &a),
        Cmd::Unclaim(a) => gate::cmd_unclaim(paths, &a),
        // Not gated: shell integration is meta (like help/version) and must work
        // before the runner is installed so a user can wire completions early.
        Cmd::Config(a) => shellinit::cmd_config(&a.shell),
    }
}

/// Rust sets SIGPIPE to SIG_IGN at startup, which turns a closed pipe (e.g.
/// `looop state | head`) into a panic on the next write. Restore the default
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

/// Descend a subcommand path (e.g. `["worker", "start"]`) returning the
/// deepest subcommand, or `None` when any step doesn't resolve — the single
/// traversal that both VALIDATES the topic chain and yields the `&mut` that
/// `print_help` needs (no separate immutable pre-pass, no `expect`). Wrapped
/// in a fn so the borrow checker accepts the chained `&mut` traversal (inline
/// loops that reuse the `&mut` result trip E0499).
fn descend_help<'a>(cmd: &'a mut clap::Command, path: &[String]) -> Option<&'a mut clap::Command> {
    let mut current = cmd;
    for name in path {
        current = current.find_subcommand_mut(name)?;
    }
    Some(current)
}

/// Should this argv take the PRE-CLAP detached-supervisor shortcut? The
/// internal invocation has a FIXED shape — babysit's detacher re-execs
/// `looop run --detached-id <id> [flags…] -- <cmd…>` (see
/// `session::run_detached_worker`) — so the predicate requires exactly it:
/// `--detached-id` must be the FIRST token after `run`, AND a bare `--` must
/// separate the wrapped command. A loose "appears anywhere before `--`" match
/// (the previous rule) hijacked a plain HUMAN `looop run grep --detached-id
/// log.txt` (no bare `--`) into the supervisor path; a human argv can no
/// longer stray into it. Flags babysit ADDS after `--detached-id <id>` still
/// pass (run_detached_worker ignores unknown flags for forward-compat), but a
/// reorder that moves `--detached-id` off the front is a deliberate
/// coordinated change, not something to absorb silently.
fn is_detached_run(raw: &[String]) -> bool {
    raw.first().map(String::as_str) == Some("run")
        && raw.get(1).map(String::as_str) == Some("--detached-id")
        && raw[1..].iter().any(|a| a == "--")
}

#[cfg(test)]
mod tests {
    use super::is_detached_run;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn detached_run_shortcut_requires_the_exact_supervisor_shape() {
        // babysit's argv: `--detached-id` right after the verb, then `--`.
        assert!(is_detached_run(&v(&[
            "run",
            "--detached-id",
            "w1",
            "--",
            "cmd"
        ])));
        // Flags babysit ADDS between the id and `--` still pass (the
        // supervisor parser ignores unknown flags for forward-compat).
        assert!(is_detached_run(&v(&[
            "run",
            "--detached-id",
            "w1",
            "--root",
            "/x",
            "--",
            "cmd"
        ])));
        // Not the `run` verb: never shortcut.
        assert!(!is_detached_run(&v(&["worker", "--detached-id", "w1"])));
        // `--detached-id` only AFTER a bare `--` is the wrapped command's own
        // argv, not ours — clap must see this one.
        assert!(!is_detached_run(&v(&["run", "--", "cmd", "--detached-id"])));
        // A plain human `looop run <cmd>` stays on the clap path.
        assert!(!is_detached_run(&v(&["run", "echo", "hi"])));
        assert!(!is_detached_run(&v(&[])));
    }

    #[test]
    fn human_run_with_detached_id_payload_is_not_hijacked() {
        // Regression: `looop run grep --detached-id log.txt` (a human command
        // whose PAYLOAD merely mentions the flag, no bare `--`) used to be
        // hijacked into the detached-supervisor path. It must reach clap.
        assert!(!is_detached_run(&v(&[
            "run",
            "grep",
            "--detached-id",
            "log.txt"
        ])));
        // Even in first position, a missing `--` separator is not the
        // supervisor shape (babysit ALWAYS passes one).
        assert!(!is_detached_run(&v(&["run", "--detached-id", "log.txt"])));
        // And off-front positions never match, `--` or not.
        assert!(!is_detached_run(&v(&[
            "run",
            "--root",
            "/x",
            "--detached-id",
            "w1",
            "--",
            "cmd"
        ])));
    }

    /// Drift guard for the deps gate: EXACTLY the runner-launching verbs
    /// (`pulse`, `worker start`) preflight the runner binary; every
    /// filesystem/session-only verb must stay ungated — `looop down` has to
    /// work precisely when the operator removed the runner, and the machine
    /// contract (`state --json`, `asks --json`, `answer`) must not break when
    /// the runner is momentarily off PATH (or before `looop init`). Parses
    /// real argvs through clap so the assertion covers the shapes users type,
    /// not hand-built enum values. (The exhaustive match in `needs_runner`
    /// already forces a decision for NEW verbs at compile time; this test
    /// pins the decisions for the EXISTING ones.)
    #[test]
    fn deps_gate_covers_only_runner_launching_verbs() {
        use super::needs_runner;
        use clap::Parser;
        let parse = |argv: &[&str]| {
            let full: Vec<&str> = std::iter::once("looop")
                .chain(argv.iter().copied())
                .collect();
            crate::cli::Cli::try_parse_from(&full)
                .unwrap_or_else(|e| panic!("argv {argv:?} must parse: {e}"))
                .cmd
                .expect("argv carries a verb")
        };

        // GATED: the only verbs that launch the configured runner.
        for argv in [
            vec!["pulse"],
            vec!["worker", "start", "w1", "do", "the", "thing"],
        ] {
            assert!(
                needs_runner(&parse(&argv)),
                "{argv:?} launches the runner and must be deps-gated"
            );
        }

        // UNGATED: teardown, reads, mailbox/lease/schedule contract, pure
        // filesystem writes, ad-hoc shell, and meta verbs.
        for argv in [
            vec!["help"],
            vec!["version"],
            vec!["init"],
            vec!["up"], // cmd_up runs the preflight itself, AFTER the init check
            vec!["down"],
            vec!["watch"],
            vec!["state", "--json"],
            vec!["wait"],
            vec!["asks", "--json"],
            vec!["answer", "a1", "yes"],
            vec!["goal", "write", "g1", "body"],
            vec!["goal", "archive", "g1"],
            vec!["sensor", "write", "s1", "echo ok"],
            vec!["playbook", "write", "body"],
            vec!["run", "echo", "hi"],
            vec!["worker", "kill", "w1"],
            vec!["worker", "list"],
            vec!["ask", "w1", "--prompt", "q?"],
            vec!["tell", "w1", "msg"],
            vec!["told", "w1"],
            vec!["schedule", "list"],
            vec!["kill", "w1"],
            vec!["screenshot", "w1"],
            vec!["claim", "repo"],
            vec!["unclaim", "repo"],
            vec!["config", "zsh"],
        ] {
            assert!(
                !needs_runner(&parse(&argv)),
                "{argv:?} never launches the runner and must run ungated"
            );
        }
    }

    #[test]
    fn diagnostic_prefixes_once() {
        use super::diagnostic;
        // A bare error gains the prefix…
        assert_eq!(diagnostic("boom"), "looop: boom");
        // …an already-prefixed one (e.g. the deps preflight) is untouched.
        assert_eq!(
            diagnostic("looop: missing required dependencies"),
            "looop: missing required dependencies"
        );
    }
}
