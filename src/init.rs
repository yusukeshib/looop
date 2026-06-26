//! `looop init` — the interactive setup.
//!
//! Lets you edit the TWO command strings of the wiring (`tick_command`,
//! `worker_command`) and writes them to $LOOOP_CONFIG. Each prompt is prefilled
//! with the CURRENT value (or the inline claude default on first run), so
//! re-running init shows what you have now and you tweak it in place.
//!
//! NO per-runner knowledge lives here — looop is glue. We seed the prompts from
//! the claude default (config.rs) and otherwise just edit whatever strings the
//! operator types. Ready-to-paste wirings for codex/opencode/pi are in the README.
//!
//! Each prompt is a small readline-style editor (`editable`): the value is in the
//! editable buffer so you can press Enter to accept or edit in place (←/→,
//! Home/End, Backspace/Del, Ctrl-A/E/U); long commands scroll horizontally within
//! one line. Esc / Ctrl-C aborts. It uses crossterm (already pulled in via
//! ratatui) — no extra dependency.
//!
//! Not deps-gated: the whole point is to configure looop BEFORE the runner CLI is
//! necessarily on PATH, so we never preflight the runner binary here.
//!
//! Non-interactive stdin (piped / not a TTY) keeps every current/default value
//! silently, so `looop init </dev/null` lays down the default wiring in scripts.
//! Re-running `looop init` always overwrites the existing config (no prompt).

use crate::config;
use crate::paths::Paths;
use crate::seed;
use crate::util::{b, dim, rst};
use anyhow::Result;
use ratatui::crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode, size},
};
use std::io::{self, BufRead, IsTerminal, Write};
use std::process::ExitCode;

/// `looop init` — choose the agent runner and write its wiring.
pub fn cmd_init(paths: &Paths) -> Result<ExitCode> {
    // Lay down the data dir + starter PLAYBOOK/goals (config is written below).
    seed::ensure_dirs(paths)?;

    let tty = io::stdin().is_terminal() && io::stdout().is_terminal();

    // Re-running init always overwrites (the config is small + easy to redo);
    // we never prompt to confirm.
    println!("looop init — edit the agent commands that drive ticks and workers.");
    if tty {
        println!(
            "{d}each line is prefilled with the current value; edit it or press Enter to keep.{r}",
            d = dim(),
            r = rst()
        );
        println!(
            "{d}see the README for ready-made claude / codex / opencode / pi wirings. Esc aborts.{r}",
            d = dim(),
            r = rst()
        );
    } else {
        println!("(non-interactive stdin: keeping current/default values)");
    }
    println!();

    // Seed each prompt from the EXISTING config when re-running, else the inline
    // claude default (Config::load falls back to it when no file exists). NO
    // per-runner knowledge here — we just edit whatever strings are there.
    let cfg = config::Config::load(paths)?;
    let mut edited: Vec<String> = Vec::with_capacity(config::KEYS.len());
    for key in config::KEYS {
        let current = cfg.runner_cmd(key).unwrap_or_default();
        let Some(val) = prompt_value(key, &current, tty) else {
            return aborted();
        };
        edited.push(val);
    }

    let json = config::wiring_json(&edited[0], &edited[1]);
    config::write(paths, &json)?;

    // The runner label is just the first token of the tick command — for display.
    let runner = edited[0]
        .split_whitespace()
        .next()
        .unwrap_or("your runner")
        .to_string();
    println!("\nWrote {} (runner: {runner}).", paths.config.display());
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

/// Ask for a value, prefilling `current` into the editable buffer. `None` = the
/// user aborted. Empty submission (or non-TTY) keeps `current`.
fn prompt_value(label: &str, current: &str, tty: bool) -> Option<String> {
    if !tty {
        return Some(current.to_string());
    }
    match editable(label, current) {
        Edit::Line(s) => Some(if s.is_empty() { current.to_string() } else { s }),
        Edit::Abort => None,
        Edit::Unsupported => Some(fallback_line(label, current)),
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

/// A readline-style editor. Prints `label` on its own dim line, then edits the
/// command on the line below, prefilled with `initial` (cursor at end). Long
/// commands SCROLL HORIZONTALLY within one physical line (window = term width-1),
/// so wrapping never confuses the cursor math. Restores cooked mode before
/// returning.
fn editable(label: &str, initial: &str) -> Edit {
    let mut out = io::stdout();
    if enable_raw_mode().is_err() {
        return Edit::Unsupported;
    }
    // "label: " (gray) is the inline prefix; the command (normal) is edited after
    // it, scrolling horizontally so it never wraps. `+2` = the ": " suffix.
    let label_cols = label.chars().count() as u16 + 2;
    let mut buf: Vec<char> = initial.chars().collect();
    let mut pos = buf.len();

    let result = loop {
        // Single-line horizontal-scroll window so long commands never wrap (which
        // would break absolute-column cursor math). Keep the cursor visible.
        let cols = size().map(|(w, _)| w as usize).unwrap_or(80).max(1);
        let win = cols.saturating_sub(label_cols as usize + 1).max(8);
        let start = if pos >= win { pos - win + 1 } else { 0 };
        let end = (start + win).min(buf.len());
        let visible: String = buf[start..end].iter().collect();
        if execute!(out, cursor::MoveToColumn(0), Clear(ClearType::CurrentLine)).is_err() {
            break Edit::Unsupported;
        }
        // Redraw "label: " (gray) + the visible window of the command (normal).
        let _ = write!(out, "{}{label}:{} {visible}", dim(), rst());
        let _ = execute!(out, cursor::MoveToColumn(label_cols + (pos - start) as u16));
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

/// Plain-prompt fallback when raw mode is unavailable: shows the current value in
/// brackets, Enter keeps it. Mirrors the editor's keep-or-replace semantics so
/// init still works on terminals without raw mode.
fn fallback_line(label: &str, current: &str) -> String {
    print!("{label} [{current}]: ");
    let _ = io::stdout().flush();
    match read_line() {
        Some(s) if !s.is_empty() => s,
        _ => current.to_string(),
    }
}
