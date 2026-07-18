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
//!
//! The wiring is VALIDATED before it is written: the worker command must carry
//! the `{{prompt_file}}` placeholder (the worker's brief — see config.rs). The
//! interactive picker warns and re-prompts; the non-interactive path errors,
//! so a broken wiring is caught here instead of at the first worker start.

use crate::config;
use crate::paths::Paths;
use crate::seed;
use crate::util::{b, char_cols, dim, display_width, rst};
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
    let tty = io::stdin().is_terminal() && io::stdout().is_terminal();
    cmd_init_with_tty(paths, tty)
}

/// The `looop init` body with the TTY decision INJECTED: tests drive the
/// non-interactive path deterministically through it (a `cargo test` run on a
/// real terminal would otherwise wander into the interactive picker and hang).
fn cmd_init_with_tty(paths: &Paths, tty: bool) -> Result<ExitCode> {
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

    // Validate the wiring at INIT time, not first worker start: session.rs
    // rejects a worker_command without `{{prompt_file}}` when a worker
    // launches, but that is hours or days after the operator could have fixed
    // it. The interactive picker re-prompts above; this belt-and-braces gate
    // catches the NON-interactive path (a script re-initing on top of a
    // hand-broken config must fail loudly, not persist a wiring every worker
    // start will refuse).
    if !worker_has_prompt_placeholder(&worker) {
        anyhow::bail!(
            "worker command must contain the {{{{prompt_file}}}} placeholder \
             (the prompt file is the worker's brief): {worker:?}"
        );
    }

    // Lay down the data dir + starter PLAYBOOK/goals only AFTER the picker:
    // aborting init (Esc / Ctrl-C above) must leave no side effects. Nothing
    // before this point reads the data dir (Config::load only touches the
    // config file / inline default).
    seed::ensure_dirs(paths)?;

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
        // Single-sourced from config.rs (also builds DEFAULT_CONFIG), so the
        // preset and the inline default can never drift apart.
        tick: config::CLAUDE_TICK_COMMAND,
        worker: config::CLAUDE_WORKER_COMMAND,
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

    let tick = prompt_value("tick(one disposable decision)", base_tick)?;
    // Re-prompt until the worker command carries `{{prompt_file}}`: writing a
    // wiring that every worker start will refuse (session.rs's launch check)
    // helps nobody — warn NOW, while the operator is right here to fix it.
    let worker = loop {
        let worker = prompt_value("worker(interactive agent)", base_worker)?;
        if worker_has_prompt_placeholder(&worker) {
            break worker;
        }
        println!(
            "looop: the worker command must contain {{{{prompt_file}}}} \
             (the worker's brief) — please add it."
        );
    };
    Some((runner, tick, worker))
}

fn current_preset_index(tick: &str, worker: &str) -> Option<usize> {
    PRESETS
        .iter()
        .position(|p| p.tick == tick && p.worker == worker)
}

/// Name the runner for the non-interactive summary line: a preset match wins;
/// otherwise the tick command's first REAL token (skipping leading `VAR=value`
/// env assignments via the shared shell rule in config.rs, so `FOO=1 mytick`
/// labels as `mytick`, not `FOO=1`).
fn infer_runner(tick: &str, worker: &str) -> Option<String> {
    PRESETS
        .iter()
        .find(|p| p.tick == tick && p.worker == worker)
        .map(|p| p.name.to_string())
        .or_else(|| {
            tick.split_whitespace()
                .find(|t| !config::is_env_assign(t))
                .map(str::to_string)
        })
}

/// The wiring rule `looop init` shares with the worker launch path: the
/// worker command must carry the `{{prompt_file}}` placeholder (the worker's
/// brief — its stdin is the live attach TTY, so unlike the tick there is no
/// stdin fallback; see the config.rs module comment).
fn worker_has_prompt_placeholder(worker: &str) -> bool {
    worker.contains("{{prompt_file}}")
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

/// RAII terminal guard: raw mode (and optionally the hidden cursor) is
/// restored on EVERY exit path — early return, `?`, panic — replacing the
/// scattered `disable_raw_mode()` / `cursor::Show` cleanup calls.
struct RawModeGuard {
    hid_cursor: bool,
}

impl RawModeGuard {
    /// Enter raw mode (optionally hiding the cursor). `None` when the terminal
    /// doesn't support raw mode — callers fall back to a plain line prompt.
    fn new(hide_cursor: bool) -> Option<Self> {
        enable_raw_mode().ok()?;
        if hide_cursor {
            let _ = execute!(io::stdout(), cursor::Hide);
        }
        Some(RawModeGuard {
            hid_cursor: hide_cursor,
        })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        if self.hid_cursor {
            let _ = execute!(io::stdout(), cursor::Show);
        }
    }
}

fn prompt_runner_tui(default: usize) -> Menu {
    let mut out = io::stdout();
    let Some(_raw) = RawModeGuard::new(true) else {
        return Menu::Unsupported;
    };

    let mut selected = default;
    let rows = PRESETS.len() + 1; // choices
    let mut drawn = false;

    // SCROLL-RESERVE: emit the menu's newlines FIRST, then move back up. Any
    // scrolling the menu needs thus happens ONCE, before drawing. Without the
    // reservation, a cursor starting near the bottom of the screen would make
    // the last row's newline scroll the screen on EVERY repaint, and the
    // relative MoveUp would land one row higher each time — smearing menu
    // copies into scrollback. (`rows >= 2` always: PRESETS is non-empty.)
    for _ in 1..rows {
        let _ = write!(out, "\r\n");
    }
    if execute!(
        out,
        cursor::MoveUp((rows - 1) as u16),
        cursor::MoveToColumn(0)
    )
    .is_err()
    {
        return Menu::Unsupported;
    }

    let result = loop {
        // Repaint from the menu's TOP row. The draw below never emits a
        // newline (rows are separated by MoveDown within the reserved block),
        // so the region cannot scroll and this relative move stays exact.
        if drawn && execute!(out, cursor::MoveUp((rows - 1) as u16)).is_err() {
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
                // Erase the reserved block (top to bottom, no newlines — same
                // no-scroll discipline as the draw) and park the cursor on its
                // now-blank top row so the next output overwrites the menu.
                let _ = execute!(out, cursor::MoveUp((rows - 1) as u16));
                for i in 0..rows {
                    let _ = execute!(out, cursor::MoveToColumn(0), Clear(ClearType::CurrentLine));
                    if i + 1 < rows {
                        let _ = execute!(out, cursor::MoveDown(1));
                    }
                }
                let _ = execute!(
                    out,
                    cursor::MoveUp((rows - 1) as u16),
                    cursor::MoveToColumn(0)
                );
            }
            Menu::Abort => {
                let _ = write!(out, "\r\n");
            }
        }
    }
    let _ = out.flush();
    result
    // _raw drops here: raw mode off, cursor shown — on every path.
}

/// Paint the menu into its pre-reserved rows, starting from the CURRENT line.
/// Rows are separated by MoveDown — never a newline — so painting the bottom
/// row can't scroll the screen and break the caller's relative-MoveUp repaint
/// (see the scroll-reserve note in [`prompt_runner_tui`]). Ends on the LAST row.
fn draw_runner_menu(out: &mut io::Stdout, selected: usize) -> io::Result<()> {
    let total = PRESETS.len() + 1;
    for i in 0..total {
        execute!(out, cursor::MoveToColumn(0), Clear(ClearType::CurrentLine))?;
        let cursor = if i == selected { "> " } else { "  " };
        match PRESETS.get(i) {
            Some(p) => write!(out, "{cursor}{}", p.name)?,
            None => write!(out, "{cursor}custom")?,
        }
        if i + 1 < total {
            execute!(out, cursor::MoveDown(1))?;
        }
    }
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
    // Only a known preset is safely called "an agent": a custom wiring's label
    // is just the tick command's first token and may be any program.
    let is_agent = PRESETS.iter().any(|p| p.name == runner);
    println!();
    println!("{b}Next — start your concierge to drive the first-run setup:{r}");
    if is_agent {
        println!("  {d}launch an agent (e.g.{r} {b}{runner}{r}{d}) and tell it:{r}");
    } else {
        println!("  {d}launch your runner ({r}{b}{runner}{r}{d}) and tell it:{r}");
    }
    println!("    {b}\"be my looop concierge: run `looop up`, then relay the setup{r}");
    println!("    {b} goal and interview me to write my goals + sensors + PLAYBOOK\".{r}");
    println!("  {d}A fresh data dir already has a pending `setup` ask for the concierge.{r}");
    println!("{d}(Or just `looop up` and steer by hand: edit goals/ + PLAYBOOK.md.){r}");
}

/// Common abort exit (Esc / Ctrl-C in a prompt): write nothing, exit 130.
fn aborted() -> Result<ExitCode> {
    // Diagnostic, not output: stderr, so `looop init | tee`-style captures
    // stay clean and scripts branching on stdout see nothing.
    eprintln!("aborted (no config written).");
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
/// user aborted. Empty submission keeps `current`. Only reached on a TTY:
/// `choose_wiring` returns early for non-interactive stdin.
fn prompt_value(label: &str, current: &str) -> Option<String> {
    match editable(label, current) {
        Edit::Line(s) => Some(if s.is_empty() { current.to_string() } else { s }),
        Edit::Abort => None,
        Edit::Unsupported => fallback_line(label, current),
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

// The inline editor's cursor / horizontal-scroll math runs in DISPLAY COLUMNS
// via the SHARED width table `util::char_cols` / `util::display_width` — this
// module used to carry its own private copy, which drifted from util's on the
// emoji ranges (a 1-vs-2 miss here misplaces the editor cursor).

/// A readline-style editor. Prints `label` on its own dim line, then edits the
/// command on the line below, prefilled with `initial` (cursor at end). Long
/// commands SCROLL HORIZONTALLY within one physical line (window = term width-1),
/// so wrapping never confuses the cursor math. All window/cursor math is in
/// DISPLAY COLUMNS (via [`char_cols`]), not chars, so CJK/emoji in the command
/// keep the cursor aligned. Restores cooked mode before returning.
fn editable(label: &str, initial: &str) -> Edit {
    let mut out = io::stdout();
    let Some(_raw) = RawModeGuard::new(false) else {
        return Edit::Unsupported;
    };
    // Optional "label: " (gray) prefix; the command (normal) is edited after it,
    // scrolling horizontally so it never wraps. `+2` = the ": " suffix.
    let label_cols = if label.is_empty() {
        0
    } else {
        display_width(label.chars()) as u16 + 2
    };
    let mut buf: Vec<char> = initial.chars().collect();
    let mut pos = buf.len();

    let result = loop {
        // Single-line horizontal-scroll window so long commands never wrap (which
        // would break absolute-column cursor math). Keep the cursor visible.
        let cols = size().map_or(80, |(w, _)| w as usize).max(1);
        // Window budget in DISPLAY COLUMNS (not chars): a wide glyph consumes
        // two of them, so `scroll_window` walks char-by-char summing widths.
        let win = cols.saturating_sub(label_cols as usize + 1).max(8);
        let (start, end, cursor_cols) = scroll_window(&buf, pos, win);
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
        let _ = execute!(out, cursor::MoveToColumn(label_cols + cursor_cols as u16));
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
                (KeyCode::Right, _) if pos < buf.len() => {
                    pos += 1;
                }
                (KeyCode::Backspace, _) if pos > 0 => {
                    pos -= 1;
                    buf.remove(pos);
                }
                (KeyCode::Delete, _) if pos < buf.len() => {
                    buf.remove(pos);
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

    let _ = write!(out, "\r\n");
    let _ = out.flush();
    match result {
        Edit::Line(s) => Edit::Line(s.trim().to_string()),
        other => other,
    }
    // _raw drops here: raw mode restored on every path (incl. panics).
}

/// The editor's horizontal-scroll window over `buf`, in DISPLAY COLUMNS:
/// given the cursor at char index `pos` and a window `win` columns wide,
/// returns `(start, end, cursor_cols)` — the visible char range
/// `buf[start..end]` and the display width of `buf[start..pos]` (the column
/// the terminal cursor is drawn at, relative to the window's left edge).
/// Pure (no terminal I/O) so the wide-char (CJK/emoji) math is unit-testable:
///   • walk BACK from the cursor first, reserving one column so a cursor
///     sitting past the last visible char still fits the window;
///   • then extend the slice FORWARD while it fits the column budget.
fn scroll_window(buf: &[char], pos: usize, win: usize) -> (usize, usize, usize) {
    let mut start = pos;
    let mut cursor_cols = 0usize; // display width of buf[start..pos]
    while start > 0 {
        let w = char_cols(buf[start - 1]);
        if cursor_cols + w > win.saturating_sub(1) {
            break;
        }
        cursor_cols += w;
        start -= 1;
    }
    let mut end = start;
    let mut used = 0usize;
    while end < buf.len() {
        let w = char_cols(buf[end]);
        if used + w > win {
            break;
        }
        used += w;
        end += 1;
    }
    (start, end, cursor_cols)
}

/// Plain-prompt fallback when raw mode is unavailable: shows the current value in
/// brackets, Enter keeps it, EOF (Ctrl-D / closed stdin) ABORTS — aligned with
/// the raw-mode editor's Esc/Ctrl-C/Ctrl-D semantics, so a closed stdin never
/// silently "accepts" a wiring the user didn't confirm.
fn fallback_line(label: &str, current: &str) -> Option<String> {
    print!("{label} [{current}]: ");
    let _ = io::stdout().flush();
    match read_line() {
        Some(s) if !s.is_empty() => Some(s),
        Some(_) => Some(current.to_string()),
        None => None,
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
    fn editor_width_math_uses_the_shared_table() {
        // The editor's window/cursor arithmetic runs on the SHARED util width
        // table — assert the glyphs a command string realistically carries
        // (paths, prompts, model names) still measure as the editor expects.
        assert_eq!(char_cols('a'), 1);
        assert_eq!(char_cols('-'), 1);
        assert_eq!(char_cols('あ'), 2); // Hiragana
        assert_eq!(char_cols('🎉'), 2); // emoji
        assert_eq!(display_width("pi -p あ🎉".chars()), 6 + 4);
    }

    #[test]
    fn non_interactive_custom_keeps_existing_commands() {
        let (runner, tick, worker) = choose_wiring("mytick --x", "myworker --y", false).unwrap();
        assert_eq!(runner, "mytick");
        assert_eq!(tick, "mytick --x");
        assert_eq!(worker, "myworker --y");
    }

    #[test]
    fn infer_runner_skips_env_assignments_with_the_shared_shell_rule() {
        // Leading `VAR=value` prefixes are not the runner…
        assert_eq!(
            infer_runner("FOO=1 mytick --x", "w").as_deref(),
            Some("mytick")
        );
        // …but a digit-leading `9X=1` is the COMMAND to the shell, so it is
        // the label (the pin the old private copies disagreed on).
        assert_eq!(infer_runner("9X=1 --x", "w").as_deref(), Some("9X=1"));
    }

    #[test]
    fn init_rejects_a_worker_command_without_the_prompt_placeholder() {
        // The launch-time rule, checked at init: no `{{prompt_file}}`, no brief.
        assert!(worker_has_prompt_placeholder(
            "claude \"$(cat {{prompt_file}})\""
        ));
        assert!(!worker_has_prompt_placeholder("claude -p"));

        // End-to-end on the NON-interactive path (cargo test runs without a
        // TTY, so cmd_init takes it): a config whose worker command lacks the
        // placeholder must fail the re-init loudly instead of persisting a
        // wiring every worker start would refuse…
        let p = Paths::temp();
        config::write(&p, &config::wiring_json("mytick -p", "myworker -p")).unwrap();
        let err = cmd_init_with_tty(&p, false).unwrap_err().to_string();
        assert!(
            err.contains("{{prompt_file}}"),
            "init must name the missing placeholder: {err}"
        );

        // …while a valid wiring re-inits cleanly.
        config::write(
            &p,
            &config::wiring_json("mytick -p", "myworker {{prompt_file}}"),
        )
        .unwrap();
        assert!(cmd_init_with_tty(&p, false).is_ok());
    }

    #[test]
    fn scroll_window_positions_the_cursor_for_wide_chars() {
        // ASCII fits whole: the window is the full buffer, cursor at its col.
        let buf: Vec<char> = "abcdef".chars().collect();
        assert_eq!(scroll_window(&buf, 3, 20), (0, 6, 3));
        assert_eq!(scroll_window(&buf, 0, 20), (0, 6, 0));
        assert_eq!(scroll_window(&buf, 6, 20), (0, 6, 6));

        // CJK: every glyph is TWO display columns — the cursor column is the
        // WIDTH of the chars before it, not their count.
        let cjk: Vec<char> = "あいうえお".chars().collect();
        assert_eq!(scroll_window(&cjk, 3, 20), (0, 5, 6));

        // Scrolling: a narrow window keeps the cursor visible by walking BACK
        // from it — with win=8 and the cursor at the end of 5 wide glyphs,
        // only the last 3 fit ((8-1)/2 = 3 whole glyphs of the reserve-adjusted
        // budget), so the window starts at char 2.
        assert_eq!(scroll_window(&cjk, 5, 8), (2, 5, 6));

        // A wide glyph NEVER splits: with an ODD reserve-adjusted budget the
        // walk stops before half a glyph would be needed.
        let (start, _, cursor_cols) = scroll_window(&cjk, 5, 9);
        assert_eq!((start, cursor_cols), (1, 8));

        // Mixed ASCII + CJK: cursor after `abあ` sits at column 4 (1+1+2).
        let mix: Vec<char> = "abあcd".chars().collect();
        assert_eq!(scroll_window(&mix, 3, 20), (0, 5, 4));

        // Empty buffer: degenerate but well-defined.
        assert_eq!(scroll_window(&[], 0, 8), (0, 0, 0));
    }
}
