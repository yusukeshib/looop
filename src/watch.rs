//! `looop watch` — a two-pane TUI for observing the running fleet.
//!
//! The control loop is invisible by design (the pulse + workers run detached),
//! so `watch` is the human window into it:
//!
//!   ┌─ log ──────────────────────────────────────────┐
//!   │ live, COLORED tail of the selected session's    │
//!   │ output.log (ANSI/SGR preserved via ansi-to-tui) │
//!   ├─ workers ──────────────────────────────────────┤
//!   │ ID       HEALTH  STATE    IDLE  UP  ASK  VERIFY │
//!   │ pulse    up      running  2s    1h  -    -      │
//!   └─────────────────────────────────────────────────┘
//!
//! Read-only: it tails files and lists sessions, never sends input. The pulse
//! and workers are PTY-backed, so their `output.log` is a RAW PTY transcript —
//! an interactive agent redraws in place (cursor moves, line/screen
//! clears, carriage returns), so the raw bytes are NOT a clean line log. We
//! replay the WHOLE log through a `vt100` virtual terminal and render the
//! resulting SCREEN plus bounded scrollback, instead of dumping every redraw
//! frame as new lines. Selecting a row in the docked worker table re-points
//! the log pane.
//!
//! Mouse capture stays on (wheel scrolls, the scrollbar scrubs); hold Shift
//! while dragging to use the terminal's own text selection / copy.

use crate::logview::LogView;
use crate::paths::Paths;
use crate::sensor::WorkerHealth;
use crate::session::{self, Session};
use anyhow::Result;
use std::collections::HashSet;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::prelude::*;
use ratatui::widgets::{
    Block, Borders, Cell, Paragraph, Row, Scrollbar, ScrollbarOrientation, ScrollbarState, Table,
    TableState,
};

/// How often we re-list sessions and re-read the tailed log.
const TICK: Duration = Duration::from_millis(250);

/// The docked fleet pane shows at most five data rows. Its border and header
/// are additional rows, so the log retains as much vertical room as possible.
const MAX_WORKER_ROWS: usize = 5;
const WORKER_PANE_CHROME_ROWS: u16 = 3; // top/bottom borders + table header

/// Which sessions the worker table shows. TAB toggles living-only (`Active`)
/// and `All`; `Recent` is retained as the initial view for `--since`.
#[derive(Clone, Copy)]
enum Filter {
    /// Only live sessions (plus the pulse). The default — dead corpses hidden.
    Active,
    /// Live + pulse + dead sessions idle less than this window.
    Recent(Duration),
    /// Every session, no matter how stale.
    All,
}

fn toggle_living_all(filter: Filter) -> Filter {
    match filter {
        Filter::All => Filter::Active,
        Filter::Active | Filter::Recent(_) => Filter::All,
    }
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
    // Resolve an explicit id before entering raw mode. Exact ids win; retain
    // the same legacy `looop-foo` → `foo` compatibility as kill/screenshot.
    let initial = if let Some(requested) = args.id.as_deref() {
        let sessions = session::try_list(paths)?;
        sessions
            .iter()
            .find(|s| s.id == requested)
            .or_else(|| {
                requested
                    .strip_prefix("looop-")
                    .and_then(|id| sessions.iter().find(|s| s.id == id))
            })
            .map(|s| s.id.clone())
            .ok_or_else(|| anyhow::anyhow!("looop watch: unknown session '{requested}'"))?
            .into()
    } else {
        None
    };
    let filter = if let Some(dur) = &args.since {
        Filter::Recent(parse_duration(dur)?)
    } else if args.all {
        Filter::All
    } else {
        Filter::Active
    };

    // Enumerate before entering raw mode so a fleet error is reported normally
    // rather than repeatedly printing through the alternate-screen UI.
    let mut app = App::new(paths, initial, filter)?;
    let mut terminal = ratatui::init();
    // Capture the mouse so wheel events reach us as `Event::Mouse`. The guard
    // also disables capture while unwinding from a panic; ratatui's own panic
    // hook handles raw mode and the alternate screen.
    let mouse = MouseCaptureGuard::enable();
    let res = app.run(&mut terminal, paths);
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
    /// The same pulse-first health projection used by `looop worker list`,
    /// filtered for this view.
    fleet: Vec<WorkerHealth>,
    table_state: TableState,
    /// Which sessions the worker table shows. TAB toggles living/all.
    filter: Filter,
    /// Sessions hidden by the current filter on the last refresh (footer hint).
    hidden: usize,
    /// An explicitly requested session stays visible even when it is finished
    /// and the current filter is Active.
    requested_id: Option<String>,
    /// The scrollable vt100 replay of the selected session's `output.log` —
    /// scroll model, background parse, render + scrollbar all live here. `watch`
    /// shows it with an empty tail (pure log).
    log: LogView,
    /// Geometry of the worker table from the last draw, so a mouse click can be
    /// mapped back to a row → worker index. `None` when the table is empty.
    selector: Option<SelectorHit>,
}

/// The worker table's on-screen geometry, captured during `draw_selector` so a
/// mouse click can be mapped back to the worker under the cursor.
#[derive(Clone, Copy)]
struct SelectorHit {
    /// Inner area the session rows are drawn into (inside the border).
    area: Rect,
    /// First visible session index (the list's scroll offset), so a click on
    /// row `r` selects session `offset + (r - area.top())`.
    offset: usize,
}

impl App {
    fn new(paths: &Paths, initial: Option<String>, filter: Filter) -> Result<Self> {
        let (fleet, hidden) = fleet_filtered(paths, filter, initial.as_deref())?;
        let mut table_state = TableState::default();
        let idx = initial
            .as_deref()
            .and_then(|id| fleet.iter().position(|w| w.id == id))
            .unwrap_or(0);
        if !fleet.is_empty() {
            table_state.select(Some(idx));
        }
        Ok(App {
            fleet,
            table_state,
            filter,
            hidden,
            requested_id: initial,
            log: LogView::new(),
            selector: None,
        })
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
        if idx >= self.fleet.len() {
            return false; // click landed on a blank row below the last worker
        }
        if Some(idx) != self.table_state.selected() {
            self.table_state.select(Some(idx));
            self.log.follow_tail();
        }
        true
    }

    fn selector_contains(&self, col: u16, row: u16) -> bool {
        self.selector.is_some_and(|hit| {
            let a = hit.area;
            col >= a.left() && col < a.right() && row >= a.top() && row < a.bottom()
        })
    }

    fn selected_id(&self) -> Option<&str> {
        self.table_state
            .selected()
            .and_then(|i| self.fleet.get(i))
            .map(|w| w.id.as_str())
    }

    /// Re-read the shared fleet projection, preserving selection by id.
    fn refresh(&mut self, paths: &Paths) -> Result<()> {
        let keep = self.selected_id().map(str::to_string);
        let (fleet, hidden) = fleet_filtered(paths, self.filter, self.requested_id.as_deref())?;
        self.fleet = fleet;
        self.hidden = hidden;
        if self.fleet.is_empty() {
            self.table_state.select(None);
            return Ok(());
        }
        let idx = keep
            .and_then(|id| self.fleet.iter().position(|w| w.id == id))
            .unwrap_or(0);
        self.table_state.select(Some(idx));
        Ok(())
    }

    fn move_selection(&mut self, delta: isize) {
        if self.fleet.is_empty() {
            return;
        }
        let cur = self.table_state.selected().unwrap_or(0) as isize;
        let next = (cur + delta).clamp(0, self.fleet.len() as isize - 1) as usize;
        if Some(next) != self.table_state.selected() {
            self.table_state.select(Some(next));
            self.log.follow_tail(); // switching sessions re-follows the tail
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        // Release and an in-progress log-scrollbar drag are global: the pointer
        // may cross into the docked worker pane before the button comes up.
        // Routing by pane first would swallow those events and leave the log
        // scrollbar stuck in dragging mode.
        if mouse.kind == MouseEventKind::Up(MouseButton::Left) {
            self.log.dragging_scrollbar = false;
            return;
        }
        if self.log.dragging_scrollbar && mouse.kind == MouseEventKind::Drag(MouseButton::Left) {
            self.log.scrollbar_scrub(mouse.row);
            return;
        }

        if self.selector_contains(mouse.column, mouse.row) {
            match mouse.kind {
                MouseEventKind::ScrollUp => self.move_selection(-1),
                MouseEventKind::ScrollDown => self.move_selection(1),
                MouseEventKind::Down(MouseButton::Left) => {
                    self.select_at(mouse.column, mouse.row);
                }
                _ => {}
            }
            return;
        }

        match mouse.kind {
            MouseEventKind::ScrollUp => self.log.scroll(3),
            MouseEventKind::ScrollDown => self.log.scroll(-3),
            MouseEventKind::Down(MouseButton::Left) => {
                self.log.dragging_scrollbar = self.log.scrollbar_grab(mouse.column, mouse.row);
            }
            _ => {}
        }
    }

    fn run(&mut self, terminal: &mut ratatui::DefaultTerminal, paths: &Paths) -> Result<()> {
        let mut last_refresh = Instant::now()
            .checked_sub(TICK)
            .unwrap_or_else(Instant::now);
        loop {
            if last_refresh.elapsed() >= TICK {
                self.refresh(paths)?;
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
                        let ctrl = key.modifiers == KeyModifiers::CONTROL;
                        let plain = key.modifiers.is_empty();
                        let shifted = key.modifiers == KeyModifiers::SHIFT;
                        if ctrl && matches!(key.code, KeyCode::Char('c')) {
                            break;
                        }
                        if plain && key.code == KeyCode::Tab {
                            // TAB is deliberately binary even when `--since`
                            // supplied the initial Recent view: first show all,
                            // then toggle living-only ↔ all.
                            self.filter = toggle_living_all(self.filter);
                            self.refresh(paths)?;
                            continue;
                        }

                        // Keyboard shortcuts are global: arrows always navigate
                        // workers, while Ctrl-P/N and the paging keys scroll the log.
                        let half = (self.log.rows() / 2).max(1) as isize;
                        let page = self.log.rows().max(1) as isize;
                        match key.code {
                            KeyCode::Char('q') if plain => break,
                            KeyCode::Down if plain => self.move_selection(1),
                            KeyCode::Up if plain => self.move_selection(-1),
                            KeyCode::Char('n') if ctrl => self.log.scroll(-1),
                            KeyCode::Char('p') if ctrl => self.log.scroll(1),
                            // Half page: Ctrl-D down, Ctrl-U up (vim/less).
                            KeyCode::Char('d') if ctrl => self.log.scroll(-half),
                            KeyCode::Char('u') if ctrl => self.log.scroll(half),
                            // Full page: Ctrl-F / PageDown down, Ctrl-B / PageUp up.
                            KeyCode::Char('f') if ctrl => self.log.scroll(-page),
                            KeyCode::Char('b') if ctrl => self.log.scroll(page),
                            KeyCode::PageDown if plain => self.log.scroll(-page),
                            KeyCode::PageUp if plain => self.log.scroll(page),
                            // Jump to ends: g/Home oldest, G/End live tail.
                            KeyCode::Char('g') | KeyCode::Home if plain => self.log.jump_oldest(),
                            KeyCode::Char('G') if plain || shifted => self.log.follow_tail(),
                            KeyCode::End if plain => self.log.follow_tail(),
                            _ => {}
                        }
                    }
                    Event::Mouse(mouse) => self.handle_mouse(mouse),
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn draw(&mut self, frame: &mut Frame) {
        // The fleet is a real docked pane, never an overlay. Five DATA rows fit
        // at most; the pane's border and header are accounted for separately.
        let worker_height = worker_pane_height(self.fleet.len());
        let chunks = Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(worker_height),
            Constraint::Length(1),
        ])
        .split(frame.area());

        self.log.render(frame, chunks[0], &[], true);
        self.draw_selector(frame, chunks[1]);
        self.draw_footer(frame, chunks[2]);
    }

    /// The dim help/legend line along the very bottom of the screen.
    fn draw_footer(&mut self, frame: &mut Frame, area: Rect) {
        let name = match self.filter {
            Filter::Active => "living",
            Filter::Recent(_) => "recent",
            Filter::All => "all",
        };
        let hidden = if self.hidden > 0 {
            format!(" ({} hidden)", self.hidden)
        } else {
            String::new()
        };
        let id = self.selected_id().unwrap_or("—");
        let help = format!(
            " {id} · {name}{hidden}  ↑/↓ worker · ^P/^N scroll · ^U/^D half-page · tab living/all · q quit "
        );
        let style = Style::default().bg(Color::Rgb(40, 40, 40)).fg(Color::White);
        frame.render_widget(Paragraph::new(Span::styled(help, style)).style(style), area);
    }

    fn draw_selector(&mut self, frame: &mut Frame, area: Rect) {
        let id_width = self
            .fleet
            .iter()
            .map(|w| w.id.len())
            .max()
            .unwrap_or("no workers".len())
            .max(session::WORKER_TABLE_HEADERS[0].len()) as u16;
        let rows: Vec<Row> = if self.fleet.is_empty() {
            vec![Row::new([Cell::from("no workers")])]
        } else {
            self.fleet.iter().map(worker_table_row).collect()
        };
        let header =
            Row::new(session::WORKER_TABLE_HEADERS).style(Style::default().fg(Color::DarkGray));
        let widths = [
            Constraint::Length(id_width),
            Constraint::Length(11),
            Constraint::Length(8),
            Constraint::Length(6),
            Constraint::Length(6),
            Constraint::Length(7),
            Constraint::Length(6),
        ];
        let table = Table::new(rows, widths)
            .header(header)
            .column_spacing(2)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray)),
            )
            // Preserve per-cell health colors under the selection highlight.
            .row_highlight_style(Style::default().bg(Color::Rgb(40, 40, 40)));
        frame.render_stateful_widget(table, area, &mut self.table_state);

        let data_area = Rect {
            x: area.x.saturating_add(1),
            y: area.y.saturating_add(2), // border + header
            width: area.width.saturating_sub(2),
            height: area.height.saturating_sub(WORKER_PANE_CHROME_ROWS),
        };
        self.selector = if self.fleet.is_empty() {
            None
        } else {
            Some(SelectorHit {
                area: data_area,
                // Read offset AFTER render so Table has applied selection scroll.
                offset: self.table_state.offset(),
            })
        };

        if self.fleet.len() > MAX_WORKER_ROWS && data_area.height > 0 {
            let mut state = ScrollbarState::new(self.fleet.len())
                .position(self.table_state.selected().unwrap_or(0))
                .viewport_content_length(data_area.height as usize);
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .thumb_symbol("┃")
                .track_symbol(Some("│"))
                .track_style(Style::default().fg(Color::DarkGray));
            // Render on the pane's right border, alongside data rows only.
            let scroll_area = Rect {
                x: area.x,
                y: data_area.y,
                width: area.width,
                height: data_area.height,
            };
            frame.render_stateful_widget(scrollbar, scroll_area, &mut state);
        }
    }
}

/// List sessions according to `filter`. Alive sessions and the pulse are always
/// kept. Returns the visible list plus the count of hidden sessions.
fn list_filtered(
    paths: &Paths,
    filter: Filter,
    requested_id: Option<&str>,
) -> Result<(Vec<Session>, usize)> {
    let all = session::try_list(paths)?;
    let total = all.len();
    let explicitly_requested = |s: &Session| requested_id == Some(s.id.as_str());
    let kept: Vec<Session> = match filter {
        Filter::All => return Ok((all, 0)),
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
    Ok((kept, hidden))
}

/// Apply watch's session visibility policy to the same pulse-first health
/// projection used by `looop worker list`.
fn fleet_filtered(
    paths: &Paths,
    filter: Filter,
    requested_id: Option<&str>,
) -> Result<(Vec<WorkerHealth>, usize)> {
    let (sessions, hidden) = list_filtered(paths, filter, requested_id)?;
    let visible: HashSet<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
    let mut fleet = session::worker_table_fleet(paths, true);
    // The pulse row is always present, even when down, exactly like worker list.
    fleet.retain(|w| w.id == session::PULSE_SESSION || visible.contains(w.id.as_str()));
    Ok((fleet, hidden))
}

fn worker_pane_height(workers: usize) -> u16 {
    workers.clamp(1, MAX_WORKER_ROWS) as u16 + WORKER_PANE_CHROME_ROWS
}

fn right_cell(value: String) -> Cell<'static> {
    Cell::from(Text::from(value).alignment(Alignment::Right))
}

/// Ratatui styling over the shared `worker list` textual row projection.
fn worker_table_row(worker: &WorkerHealth) -> Row<'static> {
    let row = session::worker_table_row(worker);
    let health_style = match row.health {
        "stuck" | "down" => Style::default().fg(Color::Red),
        "waiting-ask" => Style::default().fg(Color::Yellow),
        "dead" => Style::default().fg(Color::DarkGray),
        _ => Style::default(),
    };
    let verify_style = if row.verify_failed {
        Style::default().fg(Color::Red)
    } else {
        Style::default()
    };
    Row::new([
        Cell::from(row.id),
        Cell::from(row.health).style(health_style),
        Cell::from(row.state),
        right_cell(row.idle),
        right_cell(row.up),
        right_cell(row.ask),
        Cell::from(row.verify).style(verify_style),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn health(id: impl Into<String>) -> WorkerHealth {
        WorkerHealth {
            id: id.into(),
            state: "running".to_string(),
            alive: true,
            exit_code: None,
            health: "busy",
            idle_s: Some(65),
            uptime_s: Some(3600),
            ask_age_s: None,
            verify: None,
            verify_output: None,
        }
    }

    fn app_with_fleet(fleet: Vec<WorkerHealth>) -> App {
        let mut table_state = TableState::default();
        if !fleet.is_empty() {
            table_state.select(Some(0));
        }
        App {
            fleet,
            table_state,
            filter: Filter::Active,
            hidden: 0,
            requested_id: None,
            log: LogView::new(),
            selector: None,
        }
    }

    fn render_selector(app: &mut App) -> String {
        let width = 100;
        let height = worker_pane_height(app.fleet.len());
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| app.draw_selector(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer.cell((x, y)).unwrap().symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

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

    #[test]
    fn releasing_log_scrollbar_over_worker_pane_ends_drag() {
        let mut app = app_with_fleet(vec![health("pulse")]);
        app.selector = Some(SelectorHit {
            area: Rect::new(0, 0, 20, 5),
            offset: 0,
        });
        app.log.dragging_scrollbar = true;

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 3,
            row: 2,
            modifiers: KeyModifiers::NONE,
        });

        assert!(!app.log.dragging_scrollbar);
    }

    #[test]
    fn tab_filter_is_a_binary_living_all_toggle() {
        assert!(matches!(toggle_living_all(Filter::Active), Filter::All));
        assert!(matches!(toggle_living_all(Filter::All), Filter::Active));
        assert!(matches!(
            toggle_living_all(Filter::Recent(Duration::from_secs(60))),
            Filter::All
        ));
    }

    #[test]
    fn worker_pane_caps_five_data_rows_beyond_header_and_border() {
        assert_eq!(worker_pane_height(0), 4);
        assert_eq!(worker_pane_height(1), 4);
        assert_eq!(worker_pane_height(5), 8);
        assert_eq!(worker_pane_height(6), 8);
    }

    #[test]
    fn worker_table_uses_worker_list_columns_and_values() {
        let mut worker = health("w1");
        worker.state = "exited".to_string();
        worker.alive = false;
        worker.exit_code = Some(7);
        worker.health = "dead";
        worker.verify = Some(false);
        let mut app = app_with_fleet(vec![worker]);

        let rendered = render_selector(&mut app);
        for heading in session::WORKER_TABLE_HEADERS {
            assert!(rendered.contains(heading), "missing heading {heading}");
        }
        for value in ["w1", "dead", "exit 7", "1m", "1h", "FAIL"] {
            assert!(rendered.contains(value), "missing table value {value}");
        }
        assert_eq!(app.selector.unwrap().area.height, 1);
        assert!(!rendered.contains('┃'));
    }

    #[test]
    fn worker_table_scrolls_after_five_rows_and_shows_scrollbar() {
        let fleet = (0..6).map(|i| health(format!("w{i}"))).collect();
        let mut app = app_with_fleet(fleet);
        app.table_state.select(Some(5));

        let rendered = render_selector(&mut app);
        assert_eq!(app.selector.unwrap().area.height, 5);
        assert_eq!(app.table_state.offset(), 1);
        assert!(!rendered.contains("w0"));
        assert!(rendered.contains("w5"));
        assert!(rendered.contains('┃'));
    }

    #[test]
    fn empty_worker_table_keeps_one_row_without_a_scrollbar() {
        let mut app = app_with_fleet(Vec::new());
        let rendered = render_selector(&mut app);
        assert!(rendered.contains("no workers"));
        assert!(app.selector.is_none());
        assert!(!rendered.contains('┃'));
    }
}
