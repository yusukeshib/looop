//! looop — a tiny, portable, Kubernetes-shaped control loop for your work.
//!
//! Rust port (in progress). The pulse is unbreakable code, judgment is the AI,
//! memory is git (RULE 2). This file wires the CLI; the tick engine lives in
//! `tick` (ported incrementally — see the bash `looop` at the repo root, which
//! remains the reference during the port).

mod config;
mod deps;
mod help;
mod paths;

use anyhow::Result;
use paths::Paths;
use std::process::ExitCode;

/// Subcommands, mirroring the bash dispatch `case`. Parsed by hand rather than
/// via clap derive so `looop` (no args) and `looop run <goal>` keep their exact
/// shorthand semantics, and `--help` can emit the full design manual.
fn main() -> ExitCode {
    let paths = Paths::resolve();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("run");

    let result: Result<ExitCode> = match cmd {
        "help" | "-h" | "--help" => {
            help::print(&paths);
            Ok(ExitCode::SUCCESS)
        }
        "version" | "--version" | "-V" => {
            println!("looop {}", env!("CARGO_PKG_VERSION"));
            Ok(ExitCode::SUCCESS)
        }
        // ---- ported foundation; engine commands are stubs for now ----------
        "run" | "loop" => not_yet("run / run <goal>", &paths),
        "tick" => not_yet("tick", &paths),
        "ls" => not_yet("ls", &paths),
        "playbook" => not_yet("playbook", &paths),
        "start-session" => not_yet("start-session", &paths),
        "cost" => not_yet("cost", &paths),
        "_fmt" => not_yet("_fmt", &paths),
        "_cost" => not_yet("_cost", &paths),
        other => {
            eprintln!(
                "looop: unknown command '{other}' (try: run, run <goal>, tick, ls, playbook, start-session, help)"
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

/// Temporary landing for engine commands not yet ported. Runs the real dep
/// preflight so the wiring is exercised, then reports the porting status.
fn not_yet(name: &str, paths: &Paths) -> Result<ExitCode> {
    deps::require_deps(paths)?;
    eprintln!("looop: '{name}' is not ported to Rust yet (use the bash `looop` for now)");
    Ok(ExitCode::from(2))
}
