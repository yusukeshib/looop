//! `looop init` — the interactive first-run wizard.
//!
//! Asks a short series of questions (runner → tick model → worker model), each
//! prefilled with a sensible default (claude/sonnet/opus), and writes the chosen
//! runner wiring to $LOOOP_CONFIG. It is the ONLY thing that needs "installing".
//!
//! Each prompt is a small readline-style editor (`editable`): the default value
//! is placed IN the editable buffer so you can press Enter to accept it or edit
//! it in place (←/→, Home/End, Backspace/Del, Ctrl-A/E/U). Esc / Ctrl-C aborts.
//! It uses crossterm (already pulled in via ratatui) — no extra dependency.
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
use crate::util::{b, dim, rst};
use anyhow::Result;
use ratatui::crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode},
};
use std::io::{self, BufRead, IsTerminal, Write};
use std::process::ExitCode;

/// `looop init` — choose the agent runner and write its wiring.
pub fn cmd_init(paths: &Paths) -> Result<ExitCode> {
    // Lay down the data dir + starter PLAYBOOK/goals (config is written below).
    seed::ensure_dirs(paths)?;

    let tty = io::stdin().is_terminal() && io::stdout().is_terminal();

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
    if tty {
        println!(
            "{}(edit the prefilled value, or press Enter to accept; Esc to abort){}",
            dim(),
            rst()
        );
    } else {
        println!("(non-interactive stdin: taking defaults)");
    }
    println!();

    let choices: Vec<&str> = config::RUNNERS.iter().map(|r| r.name).collect();
    let Some(runner) = prompt_choice(&choices, "claude", tty) else {
        return aborted();
    };
    // Always resolvable: prompt_choice only returns a listed name or the default.
    let spec = config::runner_spec(&runner)
        .or_else(|| config::runner_spec("claude"))
        .expect("claude spec exists");

    let Some(tick) = prompt_value("Tick model (cheap per-beat decision)", spec.tick_model, tty)
    else {
        return aborted();
    };
    let Some(worker) = prompt_value("Worker model (heavy execution)", spec.worker_model, tty) else {
        return aborted();
    };

    let Some(cfg) = config::render_config(&runner, &tick, &worker) else {
        eprintln!("looop init: unknown runner '{runner}'");
        return Ok(ExitCode::from(1));
    };
    config::write(paths, &cfg)?;

    println!("\nWrote {} (runner: {runner}).", paths.config.display());
    println!("Edit that file any time to tweak the tick / interactive / resume commands.");
    print_next_steps(&runner);
    Ok(ExitCode::SUCCESS)
}

/// The highlighted "what now" block. Gray for context, bold/white for the moves
/// the human should actually make, so the next step is unmissable.
fn print_next_steps(runner: &str) {
    let (b, d, r) = (b(), dim(), rst());
    println!();
    println!("{b}Next — start your concierge to drive the first-run setup:{r}");
    println!("  {d}launch an agent (e.g.{r} {b}{runner}{r}{d}) and tell it:{r}");
    println!("    {b}\"be my looop concierge: run `looop up`, then relay the setup{r}");
    println!("    {b} goal and interview me to write my goals + sensors + PLAYBOOK\".{r}");
    println!("  {d}The first tick opens the `setup` goal, which invites that interview.{r}");
    println!("{d}(Or just `looop up` and steer by hand: edit goals/ + PLAYBOOK.md.){r}");
}

/// Common abort exit (Esc / Ctrl-C in a prompt): write nothing, exit 130.
fn aborted() -> Result<ExitCode> {
    println!("looop init: aborted (no config written).");
    Ok(ExitCode::from(130))
}

/// Read one trimmed line from stdin (line-buffered, NOT raw); None on EOF/error.
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

/// Ask for a free-form value, prefilling `default` into the editable buffer.
/// `None` = the user aborted. Empty submission (or non-TTY) takes `default`.
fn prompt_value(label: &str, default: &str, tty: bool) -> Option<String> {
    if !tty {
        return Some(default.to_string());
    }
    match editable(&format!("{label}: "), default) {
        Edit::Line(s) => Some(if s.is_empty() { default.to_string() } else { s }),
        Edit::Abort => None,
        Edit::Unsupported => Some(fallback_line(label, default)),
    }
}

/// Ask for one of `choices`, prefilling the editable buffer and re-asking on an
/// unrecognized answer (keeping the typed attempt so it can be fixed). `None` =
/// aborted; empty submission takes `default`.
fn prompt_choice(choices: &[&str], default: &str, tty: bool) -> Option<String> {
    if !tty {
        return Some(default.to_string());
    }
    let mut seed = default.to_string();
    loop {
        match editable("Runner: ", &seed) {
            Edit::Line(s) => {
                let s = if s.is_empty() {
                    default.to_string()
                } else {
                    s
                };
                if choices.contains(&s.as_str()) {
                    return Some(s);
                }
                println!(
                    "  {}'{s}' is not one of: {}{}",
                    dim(),
                    choices.join(", "),
                    rst()
                );
                seed = s;
            }
            Edit::Abort => return None,
            Edit::Unsupported => return Some(fallback_choice(choices, default)),
        }
    }
}

/// Outcome of one `editable` prompt.
enum Edit {
    /// Submitted (Enter). Trimmed; may be empty (caller maps that to the default).
    Line(String),
    /// Esc / Ctrl-C / Ctrl-D-on-empty.
    Abort,
    /// Raw mode unavailable (e.g. an odd terminal) — caller falls back to a plain
    /// line read so init never wedges.
    Unsupported,
}

/// A minimal readline-style editor: prints `prompt`, prefills `initial` into the
/// editable buffer (cursor at end), and lets the user edit in place. Returns when
/// the user submits or aborts. Restores cooked mode before returning.
fn editable(prompt: &str, initial: &str) -> Edit {
    let mut out = io::stdout();
    if enable_raw_mode().is_err() {
        return Edit::Unsupported;
    }
    let prompt_cols = prompt.chars().count() as u16;
    let mut buf: Vec<char> = initial.chars().collect();
    let mut pos = buf.len();

    let result = loop {
        // Redraw the whole line: home, clear, prompt + buffer, then park cursor.
        let line: String = buf.iter().collect();
        if execute!(out, cursor::MoveToColumn(0), Clear(ClearType::CurrentLine)).is_err() {
            break Edit::Unsupported;
        }
        let _ = write!(out, "{prompt}{line}");
        let _ = execute!(out, cursor::MoveToColumn(prompt_cols + pos as u16));
        let _ = out.flush();

        match event::read() {
            // Ignore key-release/repeat duplicates some terminals send.
            Ok(Event::Key(KeyEvent {
                kind: KeyEventKind::Release,
                ..
            })) => continue,
            Ok(Event::Key(k)) => match (k.code, k.modifiers) {
                (KeyCode::Enter, _) => break Edit::Line(buf.iter().collect()),
                (KeyCode::Esc, _) => break Edit::Abort,
                (KeyCode::Char('c'), KeyModifiers::CONTROL) => break Edit::Abort,
                (KeyCode::Char('d'), KeyModifiers::CONTROL) if buf.is_empty() => break Edit::Abort,
                (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                    buf.drain(..pos);
                    pos = 0;
                }
                (KeyCode::Char('a'), KeyModifiers::CONTROL) | (KeyCode::Home, _) => pos = 0,
                (KeyCode::Char('e'), KeyModifiers::CONTROL) | (KeyCode::End, _) => pos = buf.len(),
                (KeyCode::Left, _) => pos = pos.saturating_sub(1),
                (KeyCode::Right, _) => {
                    if pos < buf.len() {
                        pos += 1;
                    }
                }
                (KeyCode::Backspace, _) => {
                    if pos > 0 {
                        pos -= 1;
                        buf.remove(pos);
                    }
                }
                (KeyCode::Delete, _) => {
                    if pos < buf.len() {
                        buf.remove(pos);
                    }
                }
                // Printable input only (skip Ctrl-/Alt-chorded chars).
                (KeyCode::Char(c), m)
                    if !m.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    buf.insert(pos, c);
                    pos += 1;
                }
                _ => {}
            },
            Ok(_) => {}
            Err(_) => break Edit::Unsupported,
        }
    };

    let _ = disable_raw_mode();
    let _ = write!(out, "\r\n");
    let _ = out.flush();
    match result {
        Edit::Line(s) => Edit::Line(s.trim().to_string()),
        other => other,
    }
}

/// Plain-prompt fallback when raw mode is unavailable: bracketed default, Enter
/// accepts. Mirrors the pre-readline behavior so init still works anywhere.
fn fallback_line(label: &str, default: &str) -> String {
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

/// Plain-prompt fallback for the runner choice (raw mode unavailable).
fn fallback_choice(choices: &[&str], default: &str) -> String {
    loop {
        print!("Runner [{}] ({default}): ", choices.join("/"));
        let _ = io::stdout().flush();
        match read_line() {
            None => return default.to_string(),
            Some(s) if s.is_empty() => return default.to_string(),
            Some(s) if choices.contains(&s.as_str()) => return s,
            Some(s) => println!("  '{s}' is not one of: {}", choices.join(", ")),
        }
    }
}
