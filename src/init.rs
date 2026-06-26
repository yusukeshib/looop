//! `looop init` — the interactive first-run wizard.
//!
//! Asks a short series of questions (runner → tick model → worker model), each
//! prefilled with a sensible default (claude/sonnet/opus), and writes the chosen
//! runner wiring to $LOOOP_CONFIG. It is the ONLY thing that needs "installing".
//!
//! Not deps-gated: the whole point is to configure looop BEFORE the runner CLI is
//! necessarily on PATH, so we never preflight the runner binary here.
//!
//! Non-interactive stdin (piped / not a TTY) takes every default silently and
//! never clobbers an existing config — so `looop init </dev/null` is a safe way
//! to lay down the default wiring in scripts.

use crate::config;
use crate::paths::Paths;
use crate::seed;
use anyhow::Result;
use std::io::{self, BufRead, IsTerminal, Write};
use std::process::ExitCode;

/// `looop init` — choose the agent runner and write its wiring.
pub fn cmd_init(paths: &Paths) -> Result<ExitCode> {
    // Lay down the data dir + starter PLAYBOOK/goals (config is written below).
    seed::ensure_dirs(paths)?;

    let tty = io::stdin().is_terminal();

    if config::is_initialized(paths) {
        if !tty {
            println!(
                "looop: already initialized ({}) — nothing to do.",
                paths.config.display()
            );
            return Ok(ExitCode::SUCCESS);
        }
        print!(
            "looop: config already exists at {} — overwrite? [y/N]: ",
            paths.config.display()
        );
        let _ = io::stdout().flush();
        if !read_yes() {
            println!("looop init: keeping the existing config.");
            return Ok(ExitCode::SUCCESS);
        }
    }

    println!("looop init — configure the agent runner that drives ticks and workers.");
    if !tty {
        println!("(non-interactive stdin: taking defaults)");
    }
    println!();

    let choices: Vec<&str> = config::RUNNERS.iter().map(|r| r.name).collect();
    let runner = prompt_choice("Runner", &choices, "claude", tty);
    // Always resolvable: prompt_choice only returns a listed name or the default.
    let spec = config::runner_spec(&runner)
        .or_else(|| config::runner_spec("claude"))
        .expect("claude spec exists");

    let tick = prompt_value("Tick model (cheap per-beat decision)", spec.tick_model, tty);
    let worker = prompt_value("Worker model (heavy execution)", spec.worker_model, tty);

    let Some(cfg) = config::render_config(&runner, &tick, &worker) else {
        eprintln!("looop init: unknown runner '{runner}'");
        return Ok(ExitCode::from(1));
    };
    config::write(paths, &cfg)?;

    println!("\nWrote {} (runner: {runner}).", paths.config.display());
    println!("Edit that file any time to tweak the tick / interactive / resume commands.");
    println!();
    println!("Next — start your concierge to drive the first-run setup:");
    println!("  launch an agent (e.g. `{runner}`) and tell it:");
    println!("    \"be my looop concierge: run `looop up`, then relay the setup goal");
    println!("     and interview me to write my goals + sensors + PLAYBOOK\".");
    println!("  The first tick opens the `setup` goal, which invites that interview.");
    println!("(Or just `looop up` and steer by hand: edit goals/ + PLAYBOOK.md.)");
    Ok(ExitCode::SUCCESS)
}

/// Read one trimmed line from stdin; None on EOF or error.
fn read_line() -> Option<String> {
    let mut s = String::new();
    match io::stdin().lock().read_line(&mut s) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(s.trim().to_string()),
    }
}

/// y/Y/yes → true; anything else (incl. EOF) → false.
fn read_yes() -> bool {
    matches!(read_line().as_deref(), Some("y" | "Y" | "yes"))
}

/// Prompt for a free-form value with a prefilled default. Empty input (or
/// non-TTY) takes the default. An empty default is shown as "runner default".
fn prompt_value(label: &str, default: &str, tty: bool) -> String {
    if !tty {
        return default.to_string();
    }
    if default.is_empty() {
        print!("{label} [runner default]: ");
    } else {
        print!("{label} [{default}]: ");
    }
    let _ = io::stdout().flush();
    match read_line() {
        Some(s) if !s.is_empty() => s,
        _ => default.to_string(),
    }
}

/// Prompt for one of `choices`, re-asking on an unrecognized answer. Empty input,
/// EOF, or non-TTY take `default`.
fn prompt_choice(label: &str, choices: &[&str], default: &str, tty: bool) -> String {
    if !tty {
        return default.to_string();
    }
    loop {
        print!("{label} [{}] ({default}): ", choices.join("/"));
        let _ = io::stdout().flush();
        match read_line() {
            None => return default.to_string(),
            Some(s) if s.is_empty() => return default.to_string(),
            Some(s) if choices.contains(&s.as_str()) => return s,
            Some(s) => println!("  '{s}' is not one of: {}", choices.join(", ")),
        }
    }
}
