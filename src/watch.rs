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
//! an interactive agent (pi/claude) redraws in place (cursor moves, line/screen
//! clears, carriage returns), so the raw bytes are NOT a clean line log. We
//! replay the tail through a `vt100` virtual terminal and render the resulting
//! SCREEN (with scrollback), instead of dumping every redraw frame as new
//! lines. Selecting a row in the bottom pane re-points the log pane.

use crate::paths::Paths;
use crate::session::{self, Session};
use anyhow::Result;
use std::io::{Read, Seek, SeekFrom};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use ansi_to_tui::IntoText;
use babysit::cli::ShotFormat;
use babysit::render;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::prelude::*;
use ratatui::widgets::{
    Block, Borders, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState,
};

/// How often we re-list sessions and re-read the tailed log.
const TICK: Duration = Duration::from_millis(250);
/// Tail at most this many bytes of output.log — bounds work on huge logs while
/// keeping enough scrollback to fill the pane.
const TAIL_BYTES: u64 = 256 * 1024;

/// Recorded PTY geometry of every detached worker. looop spawns with
/// `size = None` (see `session::spawn_detached`), so babysit allocates its
/// default `DEFAULT_SCREENSHOT_SIZE` PTY (80×24). The `output.log` is therefore
/// a stream meant for THIS exact grid: an interactive agent positions its
/// cursor, clears lines, and scrolls assuming these dimensions. We MUST replay
/// at the recorded size — both rows AND cols — or absolute cursor moves and the
/// scroll region drift (babysit's own screenshot path replays at the same
/// size). Sourced straight from babysit so the two can never skew. The live
/// pane height only controls how much of the (scrollback + screen) we show, not
/// the grid the stream is parsed against.
const PTY_ROWS: u16 = render::DEFAULT_SCREENSHOT_SIZE.0;
const PTY_COLS: u16 = render::DEFAULT_SCREENSHOT_SIZE.1;

/// How many rows of scrollback the virtual terminal retains. The agents don't
/// use the alternate screen (they redraw in place on the primary screen), so
/// content that scrolls off the top lands here and stays reachable via
/// PageUp/wheel/Home. Bounded so a long-running session can't grow unbounded.
const SCROLLBACK_ROWS: usize = 10_000;

/// Default recency window: hide dead sessions idle longer than this. Alive
/// sessions and the pulse are always shown regardless. Override with `--since`,
/// disable with `--all`, or toggle live in the TUI with `a`.
const DEFAULT_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

/// `looop watch [<id>] [--since <dur>] [--all]` — open the observer TUI.
///
/// An optional id preselects a session (e.g. `looop watch pulse`); otherwise the
/// most-recently-active one. `--since <dur>` sets the recency window for hiding
/// stale corpses (e.g. `1d`, `12h`, `30m`, `90s`, or bare seconds); `--all`
/// shows every session. The window defaults to [`DEFAULT_WINDOW`].
pub fn cmd_watch(paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let mut initial: Option<String> = None;
    let mut window: Option<Duration> = Some(DEFAULT_WINDOW);
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--all" | "-a" => window = None,
            "--since" | "-s" => {
                let v = iter.next().ok_or_else(|| {
                    anyhow::anyhow!("looop watch: --since needs a duration (e.g. 1d, 12h, 30m)")
                })?;
                window = Some(parse_duration(v)?);
            }
            other if other.starts_with("--since=") => {
                window = Some(parse_duration(&other["--since=".len()..])?);
            }
            other if other.starts_with('-') => {
                anyhow::bail!("looop watch: unknown flag '{other}' (--since <dur>, --all)");
            }
            id => {
                if initial.is_none() {
                    initial = Some(id.to_string());
                }
            }
        }
    }

    let mut terminal = ratatui::init();
    // Capture the mouse so wheel events reach us as `Event::Mouse` instead of
    // letting the terminal scroll its alternate screen out from under us (which
    // corrupts the rendered panes).
    let _ = execute!(std::io::stdout(), EnableMouseCapture);
    let res = App::new(paths, initial, window).run(&mut terminal, paths);
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
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
    let n: u64 = num
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("looop watch: bad duration '{s}' (try 1d, 12h, 30m, 90s)"))?;
    Ok(Duration::from_secs(n * mult))
}

struct App {
    sessions: Vec<Session>,
    list_state: ListState,
    /// Lines scrolled back from the bottom (0 = follow the tail live).
    scroll_back: usize,
    /// Active recency window: dead sessions idle longer than this are hidden.
    /// `None` shows everything. Toggled live with `a`.
    window: Option<Duration>,
    /// The window to restore when toggling filtering back on (from `--since`
    /// or [`DEFAULT_WINDOW`]).
    configured: Duration,
    /// Sessions hidden by the window on the last refresh (footer hint).
    hidden: usize,
    /// Geometry of the log scrollbar from the last draw, so mouse clicks/drags
    /// on it can be mapped back to a `scroll_back` position. `None` when the
    /// pane isn't scrollable (no scrollbar rendered).
    scrollbar: Option<ScrollbarHit>,
    /// Persistent vt100 replay of the selected session's log (fed incrementally
    /// across frames). `None` until a session with a log file is selected.
    log: Option<LogReplay>,
    /// Whether we're capturing the mouse. ON (default) routes wheel/scrollbar to
    /// us; toggled OFF with `m` so the terminal's own drag-to-select works.
    mouse_capture: bool,
}

/// Persistent vt100 replay of one session's `output.log`. We keep the parser
/// across frames and feed it ONLY newly-appended bytes (tracked by `offset`),
/// instead of re-parsing a fixed tail every frame. That preserves the full
/// scrollback history, never corrupts the screen with a tail cut mid-escape,
/// and lets a paused viewport stay put as new output streams in.
struct LogReplay {
    /// Session id this replay belongs to (rebuilt when the selection changes).
    id: String,
    parser: vt100::Parser,
    /// Bytes of `output.log` already fed to the parser.
    offset: u64,
    /// Scrollback depth after the last feed, to measure how far the tail moved
    /// (so a scrolled-back viewport can be nudged to stay anchored).
    prev_scrollback: usize,
    /// Total bytes ever fed — 0 means the file exists but is empty.
    seen: u64,
}

/// The scrollbar's on-screen track and the scrollback depth it represents,
/// captured during `draw_log` for the mouse handler to consume.
#[derive(Clone, Copy)]
struct ScrollbarHit {
    /// Area the `Scrollbar` widget was rendered into (column = `right()-1`).
    area: Rect,
    /// Maximum scroll depth (`scroll_back` ranges `0..=max_scroll`).
    max_scroll: usize,
}

impl App {
    fn new(paths: &Paths, initial: Option<String>, window: Option<Duration>) -> Self {
        let configured = window.unwrap_or(DEFAULT_WINDOW);
        let (sessions, hidden) = list_filtered(paths, window);
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
            scroll_back: 0,
            window,
            configured,
            hidden,
            scrollbar: None,
            log: None,
            mouse_capture: true,
        }
    }

    /// Map a mouse position on the log scrollbar to a `scroll_back` value.
    /// Returns `false` if the click/drag wasn't on the scrollbar (so the
    /// caller can ignore it). The track is mapped linearly top→bottom:
    /// top (↑) = oldest scrollback, bottom (↓) = live tail.
    fn scrollbar_drag(&mut self, col: u16, row: u16) -> bool {
        let Some(hit) = self.scrollbar else {
            return false;
        };
        let a = hit.area;
        // The vertical scrollbar lives in the rightmost column of its area.
        // Accept clicks landing on that column (and the border just right of
        // it, for a forgiving hit target) within the track's row range.
        if col + 1 < a.right() || col > a.right() || row < a.top() || row >= a.bottom() {
            return false;
        }
        let span = a.height.saturating_sub(1);
        let pos = if span == 0 {
            0
        } else {
            let frac = (row - a.top()) as f64 / span as f64;
            (frac * hit.max_scroll as f64).round() as usize
        };
        // pos counts from the top (oldest); scroll_back counts from the tail.
        self.scroll_back = hit.max_scroll.saturating_sub(pos);
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
        let (sessions, hidden) = list_filtered(paths, self.window);
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
            self.scroll_back = 0; // switching sessions re-follows the tail
        }
    }

    /// Bring the persistent log replay in sync with the selected session's
    /// `output.log`: (re)build the parser on a selection change or a truncated
    /// file, then feed any newly-appended bytes. Keeps a paused viewport
    /// anchored by nudging `scroll_back` when the tail grows.
    fn sync_log(&mut self, paths: &Paths) {
        let Some(id) = self.selected_id().map(str::to_string) else {
            self.log = None;
            return;
        };
        let path = paths.sessions().output_log_path(&id);
        let Ok(meta) = std::fs::metadata(&path) else {
            self.log = None; // no log file yet
            return;
        };
        let len = meta.len();

        let reset = match &self.log {
            // New session, or the file was truncated/rotated under us.
            Some(l) => l.id != id || len < l.offset,
            None => true,
        };

        if reset {
            // Initial load is bounded to the last TAIL_BYTES so opening a
            // session with a huge log stays cheap; from here we only feed the
            // freshly-appended tail.
            let mut parser = vt100::Parser::new(PTY_ROWS, PTY_COLS, SCROLLBACK_ROWS);
            if len > 0
                && let Ok(b) = read_tail_bytes(&path, TAIL_BYTES)
            {
                parser.process(&b);
            }
            let prev_scrollback = scrollback_len(&mut parser);
            let start = len.saturating_sub(TAIL_BYTES);
            self.log = Some(LogReplay {
                id,
                parser,
                offset: len,
                prev_scrollback,
                seen: len - start,
            });
            return;
        }

        // Feed only what was appended since the last frame.
        let Some(l) = self.log.as_mut() else { return };
        if len <= l.offset {
            return;
        }
        let delta = match read_from(&path, l.offset) {
            Ok(b) => {
                l.seen += b.len() as u64;
                l.offset = len;
                l.parser.process(&b);
                let sb = scrollback_len(&mut l.parser);
                let d = sb.saturating_sub(l.prev_scrollback);
                l.prev_scrollback = sb;
                d
            }
            Err(_) => 0,
        };
        // Anchor a paused viewport: as rows scroll off the top, scroll back by
        // the same amount so the lines under the reader's eyes stay put.
        if self.scroll_back > 0 && delta > 0 {
            self.scroll_back = self.scroll_back.saturating_add(delta);
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

            // Feed any newly-appended log bytes into the persistent parser
            // (cheap: metadata + an incremental read). The vt100 replay screen
            // is then drawn in `draw_log`.
            self.sync_log(paths);

            terminal.draw(|f| self.draw(f))?;

            if event::poll(TICK)? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                        match key.code {
                            KeyCode::Char('q') | KeyCode::Esc => break,
                            KeyCode::Char('c') if ctrl => break,
                            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
                            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
                            KeyCode::Char('a') => {
                                // Toggle the recency filter: show all ↔ apply window.
                                self.window = match self.window {
                                    Some(_) => None,
                                    None => Some(self.configured),
                                };
                                self.refresh(paths);
                            }
                            KeyCode::Char('m') => {
                                // Toggle mouse capture: OFF hands drags back to
                                // the terminal so text selection works again.
                                self.mouse_capture = !self.mouse_capture;
                                let _ = if self.mouse_capture {
                                    execute!(std::io::stdout(), EnableMouseCapture)
                                } else {
                                    execute!(std::io::stdout(), DisableMouseCapture)
                                };
                            }
                            KeyCode::PageUp => {
                                self.scroll_back = self.scroll_back.saturating_add(10)
                            }
                            KeyCode::PageDown => {
                                self.scroll_back = self.scroll_back.saturating_sub(10)
                            }
                            KeyCode::Home => self.scroll_back = usize::MAX, // jump to oldest
                            KeyCode::End => self.scroll_back = 0,           // back to live tail
                            _ => {}
                        }
                    }
                    // Mouse wheel scrolls the log pane (3 lines per notch, the
                    // usual terminal step). Capturing it ourselves is what keeps
                    // the alternate screen from being scrolled and corrupted.
                    Event::Mouse(m) => match m.kind {
                        MouseEventKind::ScrollUp => {
                            self.scroll_back = self.scroll_back.saturating_add(3)
                        }
                        MouseEventKind::ScrollDown => {
                            self.scroll_back = self.scroll_back.saturating_sub(3)
                        }
                        // Click or drag on the scrollbar jumps/scrubs the
                        // viewport to that position in the scrollback.
                        MouseEventKind::Down(MouseButton::Left)
                        | MouseEventKind::Drag(MouseButton::Left) => {
                            self.scrollbar_drag(m.column, m.row);
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
        // Selector grows with the fleet but is capped so the log keeps the room.
        let rows = self.sessions.len().clamp(1, 8) as u16;
        let chunks = Layout::vertical([Constraint::Min(3), Constraint::Length(rows + 2)])
            .split(frame.area());

        self.draw_log(frame, chunks[0]);
        self.draw_selector(frame, chunks[1]);
    }

    fn draw_log(&mut self, frame: &mut Frame, area: Rect) {
        // Cleared each frame; set below only when a scrollbar is actually drawn.
        self.scrollbar = None;
        let follow = self.scroll_back == 0;
        let id = self.selected_id().unwrap_or("—").to_string();

        // Borderless: a single header line, then the log fills the rest at full
        // width. No box — side borders would eat into the 80-column stream and
        // get swept up by terminal text selection; the id + follow state live in
        // this one line instead.
        let status = if follow {
            "live"
        } else {
            "scrolled · End=live"
        };
        let header = Line::from(vec![
            Span::styled(
                format!(" {id} "),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("— {status}"), Style::default().fg(Color::DarkGray)),
        ]);
        let parts = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(area);
        frame.render_widget(Paragraph::new(header), parts[0]);
        let body = parts[1];

        // No log file, or a file that exists but is empty: a dim hint; the view
        // can't be scrolled.
        let hint = match &self.log {
            None => Some(format!("(no log for '{id}')")),
            Some(l) if l.seen == 0 => Some("(no output yet)".to_string()),
            Some(_) => None,
        };
        if let Some(hint) = hint {
            self.scroll_back = 0;
            let para = Paragraph::new(Span::styled(hint, Style::default().fg(Color::DarkGray)));
            frame.render_widget(para, body);
            return;
        }

        let pane_h = body.height.max(1) as usize;
        let log = self.log.as_mut().expect("log present: None handled above");
        let rows = log.parser.screen().size().0 as usize; // recorded grid height

        // Probe scrollback depth and clamp the viewport. The pane is filled from
        // the combined (scrollback + live screen) history, bottom-anchored
        // `back` rows from the tail (0 = follow). `max_scroll` is chosen so Home
        // lands the OLDEST line at the top of the pane, not just the oldest
        // screenful at the bottom.
        log.parser.screen_mut().set_scrollback(usize::MAX);
        let max_back = log.parser.screen().scrollback();
        let total = max_back + rows;
        let max_scroll = total.saturating_sub(pane_h);
        let back = self.scroll_back.min(max_scroll);
        self.scroll_back = back;

        // Tile the pane in chunks of one screen (`rows`): each vt100 render is a
        // screenful at a scrollback offset; we stitch enough of them to fill
        // `pane_h`, placing each line by its distance from the live tail.
        let mut window: Vec<Option<Line>> = vec![None; pane_h];
        let mut t = 0usize;
        loop {
            let off = (back + t * rows).min(max_back);
            log.parser.screen_mut().set_scrollback(off);
            // babysit's renderer emits per-row ANSI (SGR only, no cursor motion)
            // which ansi-to-tui parses into styled lines cleanly; trim=false
            // keeps the full-height screenful so row indexing is stable.
            let shot = render::render_screen(log.parser.screen(), ShotFormat::Ansi, false);
            let screen = shot
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let text = screen
                .into_text()
                .unwrap_or_else(|_| Text::from(screen.to_string()));
            for (r, line) in text.lines.iter().enumerate().take(rows) {
                if let Some(pos) = place_row(off, r, rows, back, pane_h)
                    && window[pos].is_none()
                {
                    window[pos] = Some(line.clone());
                }
            }
            if off == max_back {
                break; // can't scroll any further into history
            }
            t += 1;
            if t > pane_h / rows.max(1) + 2 {
                break; // safety: bounded by the pane height, never unbounded
            }
        }

        // window[0] is the bottom row; render top→bottom, blanks where history
        // runs out (the oldest screenful sitting below the top of the pane).
        let lines: Vec<Line> = (0..pane_h)
            .rev()
            .map(|k| window[k].take().unwrap_or_else(|| Line::from("")))
            .collect();
        frame.render_widget(Paragraph::new(Text::from(lines)), body);

        // Scrollbar at the body's right edge, reflecting position in the range.
        if max_scroll > 0 {
            let mut state = ScrollbarState::new(max_scroll).position(max_scroll - back);
            let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("↑"))
                .end_symbol(Some("↓"));
            frame.render_stateful_widget(bar, body, &mut state);
            // Remember where it landed so mouse clicks/drags can target it.
            self.scrollbar = Some(ScrollbarHit {
                area: body,
                max_scroll,
            });
        }
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

        let scope = match self.window {
            Some(_) if self.hidden > 0 => format!(" ({} hidden)", self.hidden),
            Some(_) => String::new(),
            None => String::from(" (all)"),
        };
        let recency = if self.window.is_some() {
            "a all"
        } else {
            "a recent"
        };
        let mouse = if self.mouse_capture {
            "m mouse:on"
        } else {
            "m mouse:off(select)"
        };
        let title = format!(" sessions{scope}  ↑/↓ select · {recency} · {mouse} · q quit ");
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .title_style(Style::default().add_modifier(Modifier::BOLD)),
            )
            // Uniform white background on the selected row. REVERSED would
            // flip each span's fg into its bg (green dot → green block, gray
            // detail → gray block), which reads as a messy multicolored bar;
            // forcing bg=white + fg=black patches over the per-span colors for
            // one clean highlight.
            .highlight_style(Style::default().bg(Color::White).fg(Color::Black))
            .highlight_symbol("> ");
        frame.render_stateful_widget(list, area, &mut self.list_state);
    }
}

/// List sessions, hiding dead corpses idle longer than `window` (alive sessions
/// and the pulse are always kept). Returns the visible list plus the count of
/// hidden sessions. `window == None` keeps everything.
fn list_filtered(paths: &Paths, window: Option<Duration>) -> (Vec<Session>, usize) {
    let all = session::list(paths);
    let Some(window) = window else {
        return (all, 0);
    };
    let total = all.len();
    let kept: Vec<Session> = all
        .into_iter()
        .filter(|s| s.alive || s.is_pulse() || s.idle_for().map(|d| d < window).unwrap_or(true))
        .collect();
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
    let detail = match s.exit_code {
        Some(code) if !s.alive => format!("{} (exit {code})", s.state),
        _ => s.state.clone(),
    };
    ListItem::new(Line::from(vec![
        Span::styled(format!("{dot} "), Style::default().fg(color)),
        Span::raw(format!("{label:<20} ")),
        Span::styled(detail, Style::default().fg(Color::DarkGray)),
    ]))
}

/// Map one rendered screen row to its slot in the pane, or `None` if it falls
/// outside the visible window. `off` is the row's scrollback offset, `r` its
/// index within the `rows`-tall screen (0 = top), `back` the bottom-anchored
/// scroll position, `pane_h` the visible height. Slot 0 is the bottom row.
///
/// `ft` is the row's distance from the live tail: at scrollback `off`, the
/// screen's bottom row (`r == rows-1`) sits `off` rows back, and each row up
/// adds one. A row is visible iff its `ft` lands in `[back, back + pane_h)`.
fn place_row(off: usize, r: usize, rows: usize, back: usize, pane_h: usize) -> Option<usize> {
    let ft = off + (rows - 1 - r);
    if ft >= back && ft < back + pane_h {
        Some(ft - back)
    } else {
        None
    }
}

/// Probe a parser's current scrollback depth (rows above the live screen).
/// Leaves the viewport parked at the oldest line; the next `draw_log`
/// repositions it before rendering.
fn scrollback_len(parser: &mut vt100::Parser) -> usize {
    parser.screen_mut().set_scrollback(usize::MAX);
    parser.screen().scrollback()
}

/// Read `path` from byte `start` to EOF — the bytes appended since the last
/// frame, fed incrementally into the persistent parser.
fn read_from(path: &std::path::Path, start: u64) -> std::io::Result<Vec<u8>> {
    let mut f = std::fs::File::open(path)?;
    if start > 0 {
        f.seek(SeekFrom::Start(start))?;
    }
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    Ok(buf)
}

/// Read at most `max` bytes from the end of `path`. The first line may be
/// partial (we seek mid-file); that's acceptable for a scrolling tail.
fn read_tail_bytes(path: &std::path::Path, max: u64) -> std::io::Result<Vec<u8>> {
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(max);
    if start > 0 {
        f.seek(SeekFrom::Start(start))?;
    }
    let mut buf = Vec::with_capacity((len - start) as usize);
    f.read_to_end(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp(name: &str, contents: &[u8]) -> std::path::PathBuf {
        let p =
            std::env::temp_dir().join(format!("looop-watch-test-{}-{name}", std::process::id()));
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(contents).unwrap();
        p
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
    }

    #[test]
    fn tail_returns_whole_small_file() {
        let p = tmp("small", b"hello world");
        assert_eq!(read_tail_bytes(&p, 1024).unwrap(), b"hello world");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn tail_caps_at_max_bytes_from_the_end() {
        let p = tmp("big", b"0123456789");
        // only the last 4 bytes are returned when the cap is smaller than the file
        assert_eq!(read_tail_bytes(&p, 4).unwrap(), b"6789");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn tail_missing_file_is_err() {
        let p = std::env::temp_dir().join("looop-watch-test-does-not-exist");
        assert!(read_tail_bytes(&p, 64).is_err());
    }

    #[test]
    fn place_row_follows_tail() {
        // 24-row screen, 24-row pane, following the tail (back = 0).
        // off = 0 (the live screen): bottom row (r=23) -> slot 0, top (r=0) -> 23.
        assert_eq!(place_row(0, 23, 24, 0, 24), Some(0));
        assert_eq!(place_row(0, 0, 24, 0, 24), Some(23));
        // A row from a higher scrollback offset is above the visible window.
        assert_eq!(place_row(24, 23, 24, 0, 24), None);
    }

    #[test]
    fn place_row_scrolled_back() {
        // Scrolled back 10 rows: the live screen's bottom 10 rows fall below the
        // window; rows at ft in [10, 34) are visible. off=0 bottom row ft=0 -> out.
        assert_eq!(place_row(0, 23, 24, 10, 24), None); // ft=0, below window
        assert_eq!(place_row(0, 0, 24, 10, 24), Some(13)); // ft=23 -> slot 13
        // The next screenful up (off=24) supplies the top of the pane.
        assert_eq!(place_row(24, 23, 24, 10, 24), Some(14)); // ft=24 -> slot 14
        assert_eq!(place_row(24, 14, 24, 10, 24), Some(23)); // ft=33 -> slot 23 (top)
        assert_eq!(place_row(24, 13, 24, 10, 24), None); // ft=34, above window
    }

    #[test]
    fn place_row_taller_pane_tiles() {
        // 24-row screen into a 50-row pane: a single screenful can't fill it, so
        // higher offsets contribute the upper rows. Live screen fills slots 0..24.
        assert_eq!(place_row(0, 23, 24, 0, 50), Some(0));
        assert_eq!(place_row(0, 0, 24, 0, 50), Some(23));
        // The screenful one tile up fills slots 24..48.
        assert_eq!(place_row(24, 23, 24, 0, 50), Some(24));
        assert_eq!(place_row(24, 0, 24, 0, 50), Some(47));
    }
}
