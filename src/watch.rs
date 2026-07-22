//! `looop watch` — a two-pane TUI for observing the running fleet.
//!
//! The control loop is invisible by design (the pulse + workers run detached),
//! so `watch` is the human window into it:
//!
//!   ┌─ log ──────────────────────────────────────────┐
//!   │ live, COLORED tail of the selected session's    │
//!   │ output.log (ANSI/SGR preserved via ansi-to-tui) │
//!   ├─ sessions ─────────────────────────────────────┤
//!   │ > ● pulse     running                           │
//!   │   ● worker-1  running                           │
//!   └─────────────────────────────────────────────────┘
//!
//! Read-only: it tails files and lists sessions, never sends input. The pulse
//! and workers are PTY-backed, so their `output.log` is a RAW PTY transcript —
//! an interactive agent redraws in place (cursor moves, line/screen
//! clears, carriage returns), so the raw bytes are NOT a clean line log. We
//! replay the WHOLE log through a `vt100` virtual terminal and render the
//! resulting SCREEN plus its scrollback, instead of dumping every redraw frame
//! as new lines — so scrolling up reaches the session's first line, not just a
//! recent tail. Selecting a row in the bottom pane re-points the log pane.
//!
//! Mouse capture stays on (wheel scrolls, the scrollbar scrubs); hold Shift
//! while dragging to use the terminal's own text selection / copy.

use crate::logview::LogView;
use crate::paths::Paths;
use crate::session::{self, Session};
use anyhow::Result;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};

/// How often we re-list sessions and re-read the tailed log.
const TICK: Duration = Duration::from_millis(250);

/// Recency window used by [`Filter::Recent`] when `--since` isn't given. Alive
/// sessions and the pulse are always shown regardless of filter.
const DEFAULT_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

/// Which sessions the selector shows. Cycled live in the TUI with `a`
/// (Active → Recent → All → Active).
#[derive(Clone, Copy)]
enum Filter {
    /// Only live sessions (plus the pulse). The default — dead corpses hidden.
    Active,
    /// Live + pulse + dead sessions idle less than this window.
    Recent(Duration),
    /// Every session, no matter how stale.
    All,
}

/// `looop watch [<id>] [--since <dur>] [--all]` — open the observer TUI.
///
/// An optional id preselects a session (e.g. `looop watch pulse`); otherwise the
/// most-recently-active one. By default only live sessions (plus the pulse) are
/// shown. `--since <dur>` widens to also include dead sessions idle less than
/// the window (e.g. `1d`, `12h`, `30m`, `90s`, or bare seconds); `--all` shows
/// every session.
struct MouseCaptureGuard;

impl MouseCaptureGuard {
    fn enable() -> Self {
        let _ = execute!(std::io::stdout(), EnableMouseCapture);
        Self
    }
}

impl Drop for MouseCaptureGuard {
    fn drop(&mut self) {
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
    }
}

pub fn cmd_watch(paths: &Paths, args: &crate::cli::WatchArgs) -> Result<ExitCode> {
    let initial: Option<String> = args.id.clone();
    if let Some(id) = initial.as_deref()
        && !session::list(paths).iter().any(|s| s.id == id)
    {
        anyhow::bail!("looop watch: unknown session '{id}'");
    }
    let filter = if let Some(dur) = &args.since {
        Filter::Recent(parse_duration(dur)?)
    } else if args.all {
        Filter::All
    } else {
        Filter::Active
    };

    let mut terminal = ratatui::init();
    // Capture the mouse so wheel events reach us as `Event::Mouse`. The guard
    // also disables capture while unwinding from a panic; ratatui's own panic
    // hook handles raw mode and the alternate screen.
    let mouse = MouseCaptureGuard::enable();
    let res = App::new(paths, initial, filter).run(&mut terminal, paths);
    drop(mouse);
    ratatui::restore();
    res?;
    Ok(ExitCode::SUCCESS)
}

/// Parse a human duration: bare seconds (`90`) or a single unit suffix
/// `s`/`m`/`h`/`d` (`30m`, `12h`, `1d`).
fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('s') => (&s[..s.len() - 1], 1),
        Some('m') => (&s[..s.len() - 1], 60),
        Some('h') => (&s[..s.len() - 1], 60 * 60),
        Some('d') => (&s[..s.len() - 1], 24 * 60 * 60),
        _ => (s, 1),
    };
    let bad = || anyhow::anyhow!("looop watch: bad duration '{s}' (try 1d, 12h, 30m, 90s)");
    let n: u64 = num.trim().parse().map_err(|_| bad())?;
    let seconds = n.checked_mul(mult).ok_or_else(bad)?;
    Ok(Duration::from_secs(seconds))
}

struct App {
    sessions: Vec<Session>,
    list_state: ListState,
    /// Which sessions the selector shows. Cycled live with `a`.
    filter: Filter,
    /// The window used by [`Filter::Recent`] (from `--since` or
    /// [`DEFAULT_WINDOW`]). Preserved across filter cycling.
    recent_window: Duration,
    /// Sessions hidden by the current filter on the last refresh (footer hint).
    hidden: usize,
    /// An explicitly requested session stays visible even when it is finished
    /// and the current filter is Active.
    requested_id: Option<String>,
    /// `true` while the floating session picker is open (ENTER). The log is the
    /// main buffer; the list is hidden until summoned, and ENTER/ESC closes it.
    picking: bool,
    /// The scrollable vt100 replay of the selected session's `output.log` —
    /// scroll model, background parse, render + scrollbar all live here. `watch`
    /// shows it with an empty tail (pure log).
    log: LogView,
    /// Geometry of the session list from the last draw, so a mouse click can be
    /// mapped back to a row → session index. `None` when the list is empty.
    selector: Option<SelectorHit>,
}

/// The session list's on-screen geometry, captured during `draw_selector` so a
/// mouse click can be mapped back to the session under the cursor.
#[derive(Clone, Copy)]
struct SelectorHit {
    /// Inner area the session rows are drawn into (inside the border).
    area: Rect,
    /// First visible session index (the list's scroll offset), so a click on
    /// row `r` selects session `offset + (r - area.top())`.
    offset: usize,
}

impl App {
    fn new(paths: &Paths, initial: Option<String>, filter: Filter) -> Self {
        let recent_window = match filter {
            Filter::Recent(w) => w,
            _ => DEFAULT_WINDOW,
        };
        let (sessions, hidden) = list_filtered(paths, filter, initial.as_deref());
        let mut list_state = ListState::default();
        let idx = initial
            .as_deref()
            .and_then(|id| sessions.iter().position(|s| s.id == id))
            .unwrap_or(0);
        if !sessions.is_empty() {
            list_state.select(Some(idx));
        }
        App {
            sessions,
            list_state,
            filter,
            recent_window,
            hidden,
            requested_id: initial,
            picking: false,
            log: LogView::new(),
            selector: None,
        }
    }

    /// Select the session under a mouse click on the bottom list. Returns
    /// `false` if the click wasn't inside the list (so the caller can ignore
    /// it). Switching session re-follows the tail, mirroring `move_selection`.
    fn select_at(&mut self, col: u16, row: u16) -> bool {
        let Some(hit) = self.selector else {
            return false;
        };
        let a = hit.area;
        if col < a.left() || col >= a.right() || row < a.top() || row >= a.bottom() {
            return false;
        }
        let idx = hit.offset + (row - a.top()) as usize;
        if idx >= self.sessions.len() {
            return false; // click landed on a blank row below the last session
        }
        if Some(idx) != self.list_state.selected() {
            self.list_state.select(Some(idx));
            self.log.follow_tail();
        }
        true
    }

    fn selected_id(&self) -> Option<&str> {
        self.list_state
            .selected()
            .and_then(|i| self.sessions.get(i))
            .map(|s| s.id.as_str())
    }

    /// Re-list sessions, preserving the current selection by id (the list is
    /// re-sorted most-recently-active first, so the index drifts).
    fn refresh(&mut self, paths: &Paths) {
        let keep = self.selected_id().map(str::to_string);
        let (sessions, hidden) = list_filtered(paths, self.filter, self.requested_id.as_deref());
        self.sessions = sessions;
        self.hidden = hidden;
        if self.sessions.is_empty() {
            self.list_state.select(None);
            return;
        }
        let idx = keep
            .and_then(|id| self.sessions.iter().position(|s| s.id == id))
            .unwrap_or(0);
        self.list_state.select(Some(idx));
    }

    fn move_selection(&mut self, delta: isize) {
        if self.sessions.is_empty() {
            return;
        }
        let cur = self.list_state.selected().unwrap_or(0) as isize;
        let next = (cur + delta).clamp(0, self.sessions.len() as isize - 1) as usize;
        if Some(next) != self.list_state.selected() {
            self.list_state.select(Some(next));
            self.log.follow_tail(); // switching sessions re-follows the tail
        }
    }

    fn run(&mut self, terminal: &mut ratatui::DefaultTerminal, paths: &Paths) -> Result<()> {
        let mut last_refresh = Instant::now()
            .checked_sub(TICK)
            .unwrap_or_else(Instant::now);
        loop {
            if last_refresh.elapsed() >= TICK {
                self.refresh(paths);
                last_refresh = Instant::now();
            }

            // Point the view at the selected session and feed any newly-appended
            // bytes into its persistent parser (cheap on the UI thread; the
            // heavy initial parse runs on the LogView's background worker).
            self.log.set_target(self.selected_id().map(str::to_string));
            self.log.sync(paths);

            terminal.draw(|f| self.draw(f))?;

            if event::poll(TICK)? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                        if ctrl && matches!(key.code, KeyCode::Char('c')) {
                            break;
                        }
                        if self.picking {
                            // Floating picker: navigate sessions, ENTER/ESC closes
                            // it and hands focus back to the log.
                            match key.code {
                                KeyCode::Char('q') => break,
                                KeyCode::Enter | KeyCode::Esc => self.picking = false,
                                KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
                                KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
                                KeyCode::Char('a') => {
                                    // Cycle the filter: Active → Recent → All → Active.
                                    self.filter = match self.filter {
                                        Filter::Active => Filter::Recent(self.recent_window),
                                        Filter::Recent(_) => Filter::All,
                                        Filter::All => Filter::Active,
                                    };
                                    self.refresh(paths);
                                }
                                _ => {}
                            }
                        } else {
                            // Main buffer (log): scroll, or ENTER to open the picker.
                            // Scrolling UP goes into history, DOWN toward the tail.
                            let half = (self.log.rows() / 2).max(1) as isize;
                            let page = self.log.rows().max(1) as isize;
                            match key.code {
                                KeyCode::Char('q') => break,
                                KeyCode::Enter => self.picking = true,
                                KeyCode::Down | KeyCode::Char('j') => self.log.scroll(-1),
                                KeyCode::Up | KeyCode::Char('k') => self.log.scroll(1),
                                // Half page: Ctrl-D down, Ctrl-U up (vim/less).
                                KeyCode::Char('d') if ctrl => self.log.scroll(-half),
                                KeyCode::Char('u') if ctrl => self.log.scroll(half),
                                // Full page: Ctrl-F / PageDown down, Ctrl-B / PageUp up.
                                KeyCode::Char('f') if ctrl => self.log.scroll(-page),
                                KeyCode::Char('b') if ctrl => self.log.scroll(page),
                                KeyCode::PageDown => self.log.scroll(-page),
                                KeyCode::PageUp => self.log.scroll(page),
                                // Jump to ends: g/Home oldest, G/End live tail.
                                KeyCode::Char('g') | KeyCode::Home => self.log.jump_oldest(),
                                KeyCode::Char('G') | KeyCode::End => self.log.follow_tail(),
                                _ => {}
                            }
                        }
                    }
                    // The floating picker, when open, is modal: it captures the
                    // wheel (to move the selection) and clicks (to pick a row).
                    // Otherwise the mouse drives the log: wheel scrolls and the
                    // scrollbar can be clicked/dragged. Capturing the wheel
                    // ourselves keeps the alternate screen from being corrupted.
                    Event::Mouse(m) if self.picking => match m.kind {
                        MouseEventKind::ScrollUp => self.move_selection(-1),
                        MouseEventKind::ScrollDown => self.move_selection(1),
                        MouseEventKind::Down(MouseButton::Left) => {
                            let _ = self.select_at(m.column, m.row);
                        }
                        _ => {}
                    },
                    Event::Mouse(m) => match m.kind {
                        MouseEventKind::ScrollUp => self.log.scroll(3),
                        MouseEventKind::ScrollDown => self.log.scroll(-3),
                        // Grab the scrollbar on press; once grabbed, keep
                        // scrubbing on every drag (row only) until release, so
                        // the cursor can leave the column without dropping it.
                        MouseEventKind::Down(MouseButton::Left) => {
                            self.log.dragging_scrollbar = self.log.scrollbar_grab(m.column, m.row);
                        }
                        MouseEventKind::Drag(MouseButton::Left) if self.log.dragging_scrollbar => {
                            self.log.scrollbar_scrub(m.row);
                        }
                        MouseEventKind::Up(MouseButton::Left) => {
                            self.log.dragging_scrollbar = false;
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn draw(&mut self, frame: &mut Frame) {
        // The log is the main buffer and owns the whole screen, save a one-row
        // footer for the dim help/legend line.
        let chunks =
            Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).split(frame.area());
        let log_area = chunks[0];
        // Pure log (empty tail); hide the scrollbar while the picker floats on top.
        self.log.render(frame, log_area, &[], !self.picking);
        self.draw_footer(frame, chunks[1]);

        if self.picking {
            // Floating session picker, overlaid on the bottom of the log. Capped
            // so it never swallows the whole pane; `Clear` wipes the log rows
            // underneath so the list reads cleanly on top.
            let rows = self.sessions.len().clamp(1, 8) as u16;
            let h = (rows + 2).min(log_area.height);
            let float = Rect {
                x: log_area.x,
                y: log_area.bottom().saturating_sub(h),
                width: log_area.width,
                height: h,
            };
            frame.render_widget(Clear, float);
            self.draw_selector(frame, float);
        } else {
            // No list on screen → nothing for a mouse click to hit-test.
            self.selector = None;
        }
    }

    /// The dim help/legend line along the very bottom of the screen. Adapts to
    /// the focus: scroll/quit hints for the log, navigate/filter hints (with
    /// the active filter + hidden count) while the picker is open.
    fn draw_footer(&mut self, frame: &mut Frame, area: Rect) {
        let help = if self.picking {
            let name = match self.filter {
                Filter::Active => "active",
                Filter::Recent(_) => "recent",
                Filter::All => "all",
            };
            let hidden = if self.hidden > 0 {
                format!(" ({} hidden)", self.hidden)
            } else {
                String::new()
            };
            format!(" {name}{hidden}  ↑/↓ move · a filter · enter select · esc cancel · q quit ")
        } else {
            let id = self.selected_id().unwrap_or("—").to_string();
            format!(" {id}  ↑/↓ scroll · enter sessions · q quit ")
        };
        let style = Style::default().bg(Color::Rgb(40, 40, 40)).fg(Color::White);
        frame.render_widget(Paragraph::new(Span::styled(help, style)).style(style), area);
    }

    fn draw_selector(&mut self, frame: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = if self.sessions.is_empty() {
            vec![ListItem::new(Line::from(Span::styled(
                "  no sessions — run `looop up` to start the pulse",
                Style::default().fg(Color::DarkGray),
            )))]
        } else {
            self.sessions.iter().map(session_row).collect()
        };

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray)),
            )
            // Subtle highlight on the selected row: just a dim bg. The dark
            // background keeps the per-span colors (green dot, gray detail)
            // legible, so — unlike a white highlight — we don't override fg.
            .highlight_style(Style::default().bg(Color::Rgb(40, 40, 40)));
        frame.render_stateful_widget(list, area, &mut self.list_state);

        // Record the list geometry so a mouse click can hit-test a row. The
        // rows render inside the border; read `offset()` AFTER the render so it
        // reflects any scrolling the widget just applied.
        self.selector = if self.sessions.is_empty() {
            None
        } else {
            Some(SelectorHit {
                area: Rect {
                    x: area.x.saturating_add(1),
                    y: area.y.saturating_add(1),
                    width: area.width.saturating_sub(2),
                    height: area.height.saturating_sub(2),
                },
                offset: self.list_state.offset(),
            })
        };
    }
}

/// List sessions according to `filter`. Alive sessions and the pulse are always
/// kept. Returns the visible list plus the count of hidden sessions.
fn list_filtered(
    paths: &Paths,
    filter: Filter,
    requested_id: Option<&str>,
) -> (Vec<Session>, usize) {
    let all = session::list(paths);
    let total = all.len();
    let explicitly_requested = |s: &Session| requested_id == Some(s.id.as_str());
    let kept: Vec<Session> = match filter {
        Filter::All => return (all, 0),
        Filter::Active => all
            .into_iter()
            .filter(|s| s.alive || s.is_pulse() || explicitly_requested(s))
            .collect(),
        Filter::Recent(window) => all
            .into_iter()
            .filter(|s| {
                s.alive
                    || s.is_pulse()
                    || explicitly_requested(s)
                    || s.idle_for().map(|d| d < window).unwrap_or(false)
            })
            .collect(),
    };
    let hidden = total - kept.len();
    (kept, hidden)
}

/// Render one session as a colored row: a state dot, the id (pulse flagged),
/// and its state/exit detail.
fn session_row(s: &Session) -> ListItem<'static> {
    let (dot, color) = match (s.alive, s.state.as_str()) {
        (true, _) => ("●", Color::Green),
        (false, "exited") => ("✓", Color::DarkGray),
        (false, "killed") => ("✗", Color::Red),
        (false, _) => ("○", Color::DarkGray),
    };
    let label = if s.is_pulse() {
        format!("{} (pulse)", s.id)
    } else {
        s.id.clone()
    };
    // The status dot already conveys "running" for alive sessions, so only
    // show a textual detail once the session has finished.
    let detail = if s.alive {
        String::new()
    } else {
        match s.exit_code {
            Some(code) => format!("{} (exit {code})", s.state),
            None => s.state.clone(),
        }
    };
    ListItem::new(Line::from(vec![
        Span::styled(format!("{dot} "), Style::default().fg(color)),
        Span::raw(format!("{label:<20} ")),
        Span::styled(detail, Style::default().fg(Color::DarkGray)),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_duration_units() {
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("45s").unwrap(), Duration::from_secs(45));
        assert_eq!(parse_duration("30m").unwrap(), Duration::from_secs(1800));
        assert_eq!(parse_duration("12h").unwrap(), Duration::from_secs(43200));
        assert_eq!(parse_duration("1d").unwrap(), Duration::from_secs(86400));
        assert_eq!(parse_duration(" 2d ").unwrap(), Duration::from_secs(172800));
    }

    #[test]
    fn parse_duration_rejects_garbage() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("1w").is_err());
        assert!(parse_duration("d").is_err());
        assert!(parse_duration("18446744073709551615d").is_err());
    }
}
