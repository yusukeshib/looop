//! `looop init` — the interactive setup.
//!
//! Lets you choose a common runner preset (`claude`, `codex`, `opencode`, `pi`) or
//! `custom`, then opens the TWO command strings (`tick_command`, `worker_command`)
//! in the inline editor before writing them to $LOOOP_CONFIG.
//!
//! The presets are the same ready-to-paste wirings documented in the README. looop
//! still treats them as plain command strings after init — the runtime stays glue.
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
    if !tty {
        println!("(non-interactive stdin: keeping current/default values)");
        println!();
    }

    // Seed from the EXISTING config when re-running, else the inline claude
    // default (Config::load falls back to it when no file exists).
    let cfg = config::Config::load(paths)?;
    let current_tick = cfg.runner_cmd("tick_command").unwrap_or_default();
    let current_worker = cfg.runner_cmd("worker_command").unwrap_or_default();
    let Some((runner, tick, worker)) = choose_wiring(&current_tick, &current_worker, tty) else {
        return aborted();
    };

    let json = config::wiring_json(&tick, &worker);
    config::write(paths, &json)?;

    println!("\nWrote {} (runner: {runner}).", paths.config.display());
    print_next_steps(&runner);
    Ok(ExitCode::SUCCESS)
}

#[derive(Clone, Copy)]
struct RunnerPreset {
    name: &'static str,
    tick: &'static str,
    worker: &'static str,
}

const PRESETS: &[RunnerPreset] = &[
    RunnerPreset {
        name: "claude",
        tick: "claude -p --output-format stream-json --verbose --dangerously-skip-permissions --model sonnet \"$(cat {{prompt_file}})\"",
        worker: "claude --dangerously-skip-permissions --model opus \"$(cat {{prompt_file}})\"",
    },
    RunnerPreset {
        name: "codex",
        tick: "codex exec --json --dangerously-bypass-approvals-and-sandbox \"$(cat {{prompt_file}})\"",
        worker: "codex --dangerously-bypass-approvals-and-sandbox \"$(cat {{prompt_file}})\"",
    },
    RunnerPreset {
        name: "opencode",
        tick: "opencode run \"$(cat {{prompt_file}})\"",
        worker: "opencode \"$(cat {{prompt_file}})\"",
    },
    RunnerPreset {
        name: "pi",
        tick: "pi -p --mode json -ne --model claude-sonnet-4-5 --thinking low @{{prompt_file}}",
        worker: "pi --model claude-opus-4-8 --thinking medium @{{prompt_file}}",
    },
];

fn choose_wiring(
    current_tick: &str,
    current_worker: &str,
    tty: bool,
) -> Option<(String, String, String)> {
    if !tty {
        let runner = infer_runner(current_tick, current_worker).unwrap_or_else(|| "custom".into());
        return Some((runner, current_tick.to_string(), current_worker.to_string()));
    }

    let default = current_preset_index(current_tick, current_worker).unwrap_or(PRESETS.len());
    let choice = prompt_runner(default)?;
    let (runner, base_tick, base_worker) = if choice < PRESETS.len() {
        let p = PRESETS[choice];
        (p.name.to_string(), p.tick, p.worker)
    } else {
        ("custom".to_string(), current_tick, current_worker)
    };

    let tick = prompt_value("tick(one disposable decision)", base_tick, tty)?;
    let worker = prompt_value("worker(interactive agent)", base_worker, tty)?;
    Some((runner, tick, worker))
}

fn current_preset_index(tick: &str, worker: &str) -> Option<usize> {
    PRESETS
        .iter()
        .position(|p| p.tick == tick && p.worker == worker)
}

fn infer_runner(tick: &str, worker: &str) -> Option<String> {
    PRESETS
        .iter()
        .find(|p| p.tick == tick && p.worker == worker)
        .map(|p| p.name.to_string())
        .or_else(|| tick.split_whitespace().next().map(str::to_string))
}

fn prompt_runner(default: usize) -> Option<usize> {
    match prompt_runner_tui(default) {
        Menu::Selected(i) => Some(i),
        Menu::Abort => None,
        Menu::Unsupported => prompt_runner_line(default),
    }
}

enum Menu {
    Selected(usize),
    Abort,
    Unsupported,
}

fn prompt_runner_tui(default: usize) -> Menu {
    let mut out = io::stdout();
    if enable_raw_mode().is_err() {
        return Menu::Unsupported;
    }
    let _ = execute!(out, cursor::Hide);

    let mut selected = default;
    let rows = PRESETS.len() + 1; // choices
    let mut drawn = false;

    let result = loop {
        if drawn && execute!(out, cursor::MoveUp(rows as u16)).is_err() {
            break Menu::Unsupported;
        }
        drawn = true;

        if draw_runner_menu(&mut out, selected).is_err() {
            break Menu::Unsupported;
        }

        match event::read() {
            Ok(Event::Key(KeyEvent {
                kind: KeyEventKind::Release,
                ..
            })) => continue,
            Ok(Event::Key(k)) => match (k.code, k.modifiers) {
                (KeyCode::Enter, _) => break Menu::Selected(selected),
                (KeyCode::Esc, _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                    break Menu::Abort;
                }
                (KeyCode::Up, _) | (KeyCode::Char('k'), _) => {
                    selected = selected.saturating_sub(1);
                }
                (KeyCode::Down, _) | (KeyCode::Char('j'), _) => {
                    selected = (selected + 1).min(PRESETS.len());
                }
                (KeyCode::Home, _) => selected = 0,
                (KeyCode::End, _) => selected = PRESETS.len(),
                _ => {}
            },
            Ok(_) => {}
            Err(_) => break Menu::Unsupported,
        }
    };

    if drawn {
        match result {
            Menu::Selected(_) | Menu::Unsupported => {
                let _ = execute!(out, cursor::MoveUp(rows as u16));
                for _ in 0..rows {
                    let _ = execute!(out, cursor::MoveToColumn(0), Clear(ClearType::CurrentLine));
                    let _ = writeln!(out);
                }
                let _ = execute!(out, cursor::MoveUp(rows as u16));
            }
            Menu::Abort => {
                let _ = writeln!(out);
            }
        }
    }
    let _ = disable_raw_mode();
    let _ = execute!(out, cursor::Show);
    let _ = out.flush();
    result
}

fn draw_runner_menu(out: &mut io::Stdout, selected: usize) -> io::Result<()> {
    for (i, p) in PRESETS.iter().enumerate() {
        execute!(out, cursor::MoveToColumn(0), Clear(ClearType::CurrentLine))?;
        let cursor = if i == selected { "> " } else { "  " };
        writeln!(out, "{cursor}{}", p.name)?;
    }
    execute!(out, cursor::MoveToColumn(0), Clear(ClearType::CurrentLine))?;
    let custom = PRESETS.len();
    let cursor = if custom == selected { "> " } else { "  " };
    writeln!(out, "{cursor}custom")?;
    out.flush()
}

fn prompt_runner_line(default: usize) -> Option<usize> {
    loop {
        println!("Select runner:");
        for (i, p) in PRESETS.iter().enumerate() {
            let mark = if i == default { "*" } else { " " };
            println!("  {mark} {}. {}", i + 1, p.name);
        }
        let custom = PRESETS.len();
        let mark = if custom == default { "*" } else { " " };
        println!("  {mark} {}. custom", custom + 1);
        print!("runner [{}]: ", default + 1);
        let _ = io::stdout().flush();

        let line = read_line()?;
        if line.is_empty() {
            return Some(default);
        }
        if let Ok(n) = line.parse::<usize>()
            && (1..=PRESETS.len() + 1).contains(&n)
        {
            return Some(n - 1);
        }
        let lowered = line.to_ascii_lowercase();
        if lowered == "custom" {
            return Some(PRESETS.len());
        }
        if let Some(i) = PRESETS.iter().position(|p| p.name == lowered) {
            return Some(i);
        }
        println!("Please enter 1-{} or a runner name.\n", PRESETS.len() + 1);
    }
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
    println!("  {d}A fresh data dir already has a pending `setup` ask for the concierge.{r}");
    println!("{d}(Or just `looop up` and steer by hand: edit goals/ + PLAYBOOK.md.){r}");
}

/// Common abort exit (Esc / Ctrl-C in a prompt): write nothing, exit 130.
fn aborted() -> Result<ExitCode> {
    println!("aborted (no config written).");
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
    // Optional "label: " (gray) prefix; the command (normal) is edited after it,
    // scrolling horizontally so it never wraps. `+2` = the ": " suffix.
    let label_cols = if label.is_empty() {
        0
    } else {
        label.chars().count() as u16 + 2
    };
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
        // Redraw optional "label: " (gray) + the visible window of the command.
        if label.is_empty() {
            let _ = write!(out, "{visible}");
        } else {
            let _ = write!(out, "{}{label}:{} {visible}", dim(), rst());
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_detection_recognizes_builtin_wiring() {
        for (i, p) in PRESETS.iter().enumerate() {
            assert_eq!(current_preset_index(p.tick, p.worker), Some(i));
            assert_eq!(infer_runner(p.tick, p.worker).as_deref(), Some(p.name));
        }
    }

    #[test]
    fn non_interactive_custom_keeps_existing_commands() {
        let (runner, tick, worker) = choose_wiring("mytick --x", "myworker --y", false).unwrap();
        assert_eq!(runner, "mytick");
        assert_eq!(tick, "mytick --x");
        assert_eq!(worker, "myworker --y");
    }
}
