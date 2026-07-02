//! `looop client` — a minimal, non-agent TUI for answering worker asks.
//!
//! looop's steering surface is the `looop _ …` CONTRACT. The RECOMMENDED client
//! is an AGENT concierge — start any coding agent and tell it to "work as a
//! concierge for the `looop` command" — that watches for asks, relays them to
//! the human in plain language with a recommendation, and drives the
//! `_ answer` / `_ goal` / `_ playbook` verbs. This command is the humble,
//! hand-driven alternative: a TUI where the pending ask list is ALWAYS on
//! screen and the human answers each ask themselves.
//!
//! It is deliberately less capable than the concierge (no plain-language
//! framing, no recommendation, no steering) — that's the point. Its job is to
//! make looop's design legible: the loop decides and acts on its own; the ONE
//! thing it defers to a human is a worker's blocking ask, and this window is
//! that human ⇄ mailbox channel, laid bare.
//!
//! The ask list is a full-width TABLE (id · age · worker state · options ·
//! prompt preview). Opening an ask (ENTER/click) floats a DETAIL pane over the
//! right that shows the WORKER'S LIVE BUFFER — a scrollable `looop watch`-style
//! vt100 replay of the worker's `output.log`, with the ask itself pinned at the
//! very BOTTOM. So you read the worker's own transcript, scroll back through its
//! history, and answer the question right where it sits, at the end of the
//! buffer. ESC closes it back to the list:
//!
//! ```text
//!   ID          AGE  STATE    PROMPT        ┌──────────────────────┐
//! > triage-2    2m   running  flaky test…   │ …worker output…      ┃ │
//!   deploy-3    0s   running  dep upgrade…  │ running tests…       ┃ │
//!                                           │ ── ask ──             │ │
//!                                           │ <the question>       │ │
//!                                           │ options: ship, hold  │ │
//!                                           ┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈
//!                                           │ › ship█              │ │
//!                                           └──────────────────────┘
//!    type answer · enter send · ↑/↓ move · pgup/pgdn scroll · esc close
//! ```
//!
//! The buffer replay + scroll model + scrollbar all live in [`crate::logview`],
//! shared with `looop watch` (which shows the same buffer with no pinned ask) —
//! so the client can eventually REPLACE watch. The list is borderless (like
//! watch's log) so the bordered detail pane reads as floating on top; wheel +
//! click + scrollbar-drag all work. The input is pinned along the pane's bottom
//! and focused the moment the pane opens — no extra keystroke to "reveal" it.
//!
//! Read + one narrow write: it lists pending asks (`mailbox::pending`) and, on
//! submit, durably resolves the selected one (`mailbox::answer`). It never
//! spawns a worker or edits policy — for that, use the agent concierge or the
//! raw `_` verbs.

use crate::logview::{self, LogView};
use crate::mailbox::{self, Ask};
use crate::paths::Paths;
use crate::run;
use crate::session;
use anyhow::Result;
use std::collections::HashMap;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::prelude::*;
use ratatui::widgets::{
    Block, Borders, Cell, Clear, Paragraph, Row, Scrollbar, ScrollbarOrientation, ScrollbarState,
    Table, TableState,
};

/// Re-list asks / re-check the pulse this often, and the input-poll timeout.
const TICK: Duration = Duration::from_millis(250);

/// Rows scrolled per mouse-wheel notch (list and detail alike).
const WHEEL_STEP: usize = 3;

/// The shared dark-surface background (same as `looop watch`): the selected
/// row's highlight and the footer bar. Dark enough that per-span colors
/// (green/red state, dim gray) stay legible without overriding fg.
const SURFACE: Color = Color::Rgb(40, 40, 40);

/// The dim gray style shared by all secondary text in this TUI.
fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

/// Compact relative age of a unix timestamp: `5s` / `3m` / `2h` / `4d`. Shown
/// dim next to each ask so the list conveys how long it has been waiting.
fn fmt_age(ts: u64) -> String {
    let secs = crate::util::now_unix().saturating_sub(ts);
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

/// Width of the left ask-list column while the detail pane is open. The detail
/// pane floats starting just past it, and the list's scrollbar sits in its
/// rightmost column. Wide enough for the fixed columns below.
const LIST_W: u16 = 40;

/// Fixed column widths (cells) for the ask table: id, age, state, options.
/// PROMPT takes whatever is left. `Table` clips each cell to its width by
/// DISPLAY width, so wide (CJK) prompt text never bleeds into other columns.
const C_ID: u16 = 16;
const C_AGE: u16 = 5;
const C_STATE: u16 = 8;
const C_OPTS: u16 = 12;

/// Render the shared `looop watch`-style vertical scrollbar into `area`'s right
/// column: a `┃` thumb over a `│` track, no end caps. `pos` is the top-anchored
/// offset in `0..=max_scroll`. A no-op when nothing overflows (`max_scroll==0`).
fn render_vscrollbar(frame: &mut Frame, area: Rect, max_scroll: usize, pos: usize) {
    if max_scroll == 0 {
        return;
    }
    // ratatui sizes the thumb as `viewport * track / (content + viewport)`;
    // inflate the viewport until the thumb is at least MIN_THUMB rows so it
    // stays grabbable on a long list (affects only the thumb SIZE, not the
    // position mapping).
    const MIN_THUMB: usize = 4;
    let track = area.height as usize;
    let viewport = if track > MIN_THUMB {
        (MIN_THUMB * max_scroll.saturating_sub(1))
            .div_ceil(track - MIN_THUMB)
            .max(track)
    } else {
        track
    };
    let mut state = ScrollbarState::new(max_scroll)
        .position(pos)
        .viewport_content_length(viewport);
    let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(None)
        .end_symbol(None)
        .thumb_symbol("┃")
        .thumb_style(Style::default().fg(Color::Gray))
        .track_symbol(Some("│"))
        .track_style(Style::default().fg(Color::DarkGray));
    frame.render_stateful_widget(bar, area, &mut state);
}

/// Group a run of styled chars into a `Line`, coalescing adjacent chars that
/// share a style into one `Span` (keeps the widget's span list compact).
fn chars_to_line(chars: &[(char, Style)], base: Style) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut cur_style: Option<Style> = None;
    for &(c, st) in chars {
        if cur_style != Some(st) {
            if let Some(prev) = cur_style.take() {
                spans.push(Span::styled(std::mem::take(&mut buf), prev));
            }
            cur_style = Some(st);
        }
        buf.push(c);
    }
    if let Some(st) = cur_style {
        spans.push(Span::styled(buf, st));
    }
    Line::from(spans).style(base)
}

/// Word-wrap styled `lines` to `width` columns, preserving each span's style.
/// Breaks at the last space that fits; a word longer than the whole width is
/// hard-split. Width is counted in `char`s (good enough for the ask block; CJK
/// double-width isn't special-cased). The `LogView` paints its `tail` verbatim
/// against a fixed-width log grid, so the ask block must be pre-wrapped here to
/// stay readable in a narrow detail pane.
fn wrap_lines(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return lines;
    }
    let mut out: Vec<Line<'static>> = Vec::new();
    for line in lines {
        let base = line.style;
        // Flatten the line to a styled-char sequence, then greedily re-emit.
        let chars: Vec<(char, Style)> = line
            .spans
            .iter()
            .flat_map(|s| s.content.chars().map(move |c| (c, s.style)))
            .collect();
        if chars.is_empty() {
            out.push(Line::from("").style(base));
            continue;
        }
        let mut start = 0usize;
        while start < chars.len() {
            let mut end = (start + width).min(chars.len());
            if end < chars.len() {
                // Prefer breaking at the last space in the window (dropped).
                if let Some(sp) = (start..end).rev().find(|&i| chars[i].0 == ' ')
                    && sp > start
                {
                    end = sp;
                }
            }
            out.push(chars_to_line(&chars[start..end], base));
            start = end;
            // Swallow a single break space so it doesn't lead the next row.
            if start < chars.len() && chars[start].0 == ' ' {
                start += 1;
            }
        }
    }
    out
}

/// `looop client` — bring up the ask-answering TUI.
pub fn cmd_client(paths: &Paths) -> Result<ExitCode> {
    let mut terminal = ratatui::init();
    // Capture the mouse so wheel/click/drag reach us as `Event::Mouse` instead
    // of letting the terminal scroll its alternate screen (mirrors `watch`).
    let _ = execute!(std::io::stdout(), EnableMouseCapture);
    let res = App::new().run(&mut terminal, paths);
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    res?;
    Ok(ExitCode::SUCCESS)
}

/// The list is the main view (always full-width). Opening an ask (ENTER/click)
/// floats the DETAIL pane over the right — a scrollable read area on top with
/// the answer input pinned along its BOTTOM, always visible, so there is no
/// second "reveal the input" step. ESC closes it back to the list. Mirrors
/// `looop watch`, where the log owns the screen and the picker floats on top.
enum Mode {
    /// Focus on the ask list.
    List,
    /// The detail pane is open: arrows/wheel/scrollbar scroll the read area,
    /// and typing goes straight into the pinned answer input.
    Detail,
}

/// The ask list's on-screen geometry, captured during `draw_asks` so a mouse
/// click can be mapped back to the ask under the cursor (mirrors watch's
/// `SelectorHit`).
#[derive(Clone, Copy)]
struct AsksHit {
    /// Inner area the rows render into (inside the border).
    area: Rect,
    /// First visible row index (the list's scroll offset), so a click on row
    /// `r` selects ask `offset + (r - area.top())`.
    offset: usize,
}

struct App {
    asks: Vec<Ask>,
    /// Top visible row (viewport scroll). The WHEEL scrolls this directly and
    /// leaves the selection put; arrows/click move the selection and only nudge
    /// this enough to keep the selected row visible. Decoupling the two is why
    /// the list widget is driven with `selected = None` + a manual offset, and
    /// the highlight is painted onto the selected item's own style instead.
    list_offset: usize,
    /// Visible list rows from the last draw — selection-follow + wheel clamp.
    list_rows: usize,
    /// The selected ask, tracked by its STABLE id — not by list index.
    /// `mailbox::pending` re-sorts every tick and asks come and go, so an
    /// index would silently point at a different ask (and, mid-answer, drift
    /// the answer onto the wrong worker). The id is the source of truth; the
    /// list index is derived from it each refresh.
    selected_id: Option<String>,
    mode: Mode,
    /// The answer being typed in the detail pane's pinned input.
    input: String,
    /// Last outcome to show in the footer (an error, or an "answered X" note).
    status: Option<String>,
    /// The worker's live buffer shown in the detail pane: a scrollable vt100
    /// replay of the selected ask's worker `output.log`, with the ask itself
    /// pinned at the very bottom as the LogView's `tail`. Answering thus happens
    /// "at the end of the buffer" — mirroring (and, eventually, replacing)
    /// `looop watch`. Owns its own scroll model, background parse, and
    /// scrollbar.
    log: LogView,
    pulse_alive: bool,
    /// Count of alive worker sessions (pulse excluded) — header context.
    worker_count: usize,
    /// Worker session id → (alive, state string), for the ask table's STATE
    /// column (`running` / `exited` / `killed`, or `gone` when not found).
    worker_state: HashMap<String, (bool, String)>,
    /// Geometry of the ask list from the last draw, for click→row hit-testing.
    asks_hit: Option<AsksHit>,
}

impl App {
    fn new() -> Self {
        Self {
            asks: Vec::new(),
            list_offset: 0,
            list_rows: 0,
            selected_id: None,
            mode: Mode::List,
            input: String::new(),
            status: None,
            log: LogView::new(),
            pulse_alive: false,
            worker_count: 0,
            worker_state: HashMap::new(),
            asks_hit: None,
        }
    }

    /// Re-read the pending asks + pulse liveness, reconciling the selection by
    /// id. If the selected id is still present its row index is refreshed; if
    /// it vanished we fall back to the first ask WHILE BROWSING, but while
    /// answering we keep the (now-missing) id pinned so `submit` can report it
    /// instead of silently retargeting a different ask.
    fn refresh(&mut self, paths: &Paths) {
        self.asks = mailbox::pending(paths);
        self.pulse_alive = run::pulse_running(paths);
        let workers = session::list_workers(paths);
        self.worker_count = workers.iter().filter(|s| s.alive).count();
        self.worker_state = workers
            .into_iter()
            .map(|s| (s.id, (s.alive, s.state)))
            .collect();
        if self.asks.is_empty() {
            self.selected_id = None;
            self.list_offset = 0;
            return;
        }
        match self.selected_index() {
            Some(pos) => self.ensure_visible(pos),
            // Selected id gone (or none yet).
            None if matches!(self.mode, Mode::Detail) => {
                // Keep the pinned id so the open pane/`submit` can surface "it
                // vanished"; the highlight just won't show (it left the list).
            }
            None => {
                self.selected_id = Some(self.asks[0].id.clone());
                self.list_offset = 0;
            }
        }
    }

    /// Row index of the currently-selected ask id, if it is still pending.
    fn selected_index(&self) -> Option<usize> {
        let id = self.selected_id.as_deref()?;
        self.asks.iter().position(|a| a.id == id)
    }

    fn selected(&self) -> Option<&Ask> {
        self.selected_index().map(|i| &self.asks[i])
    }

    fn move_selection(&mut self, delta: isize) {
        if self.asks.is_empty() {
            return;
        }
        let cur = self.selected_index().unwrap_or(0) as isize;
        let last = self.asks.len() as isize - 1;
        let idx = cur.saturating_add(delta).clamp(0, last) as usize;
        self.select_index(idx);
    }

    /// Point the selection at row `idx`. When it actually CHANGES the target
    /// ask, reset the detail scroll and clear any half-typed answer — so a
    /// pending answer can never be submitted against a different ask than the
    /// one it was typed for. Shared by keyboard + mouse selection.
    fn select_index(&mut self, idx: usize) {
        let id = self.asks[idx].id.clone();
        if self.selected_id.as_deref() != Some(id.as_str()) {
            // New target ask: follow its worker buffer's tail and drop any
            // half-typed answer so it can't land on the wrong worker. The
            // LogView re-points to the new worker's log on the next `sync`.
            self.log.follow_tail();
            self.input.clear();
        }
        self.selected_id = Some(id);
        self.ensure_visible(idx);
    }

    /// Nudge the viewport offset just enough to keep row `idx` visible (used by
    /// selection moves, NOT by the wheel — the wheel scrolls freely).
    fn ensure_visible(&mut self, idx: usize) {
        let rows = self.list_rows.max(1);
        if idx < self.list_offset {
            self.list_offset = idx;
        } else if idx >= self.list_offset + rows {
            self.list_offset = idx + 1 - rows;
        }
    }

    /// Open the detail pane on the current selection: the worker's live buffer
    /// fills the read area (following the tail, so the newest output and the ask
    /// at the bottom are in view), with the answer input pinned below (focused).
    fn open_detail(&mut self) {
        self.input.clear();
        self.status = None;
        self.log.follow_tail();
        self.mode = Mode::Detail;
    }

    /// Row index of the ask under a click at `(col, row)`, if it landed on a
    /// real row of the list (mirrors watch's `select_at` hit-test).
    fn ask_at(&self, col: u16, row: u16) -> Option<usize> {
        let hit = self.asks_hit?;
        let a = hit.area;
        if col < a.left() || col >= a.right() || row < a.top() || row >= a.bottom() {
            return None;
        }
        let idx = hit.offset + (row - a.top()) as usize;
        (idx < self.asks.len()).then_some(idx)
    }

    /// Route a mouse event by focus — wheel + click on the list (List) or the
    /// detail modal + its scrollbar (Detail). Mirrors watch's mouse model.
    fn on_mouse(&mut self, m: MouseEvent) {
        match self.mode {
            Mode::List => match m.kind {
                // The wheel scrolls the VIEW, not the selection (the highlight
                // stays on its ask); clamped to range in `draw_asks`.
                MouseEventKind::ScrollUp => {
                    self.list_offset = self.list_offset.saturating_sub(WHEEL_STEP);
                }
                MouseEventKind::ScrollDown => {
                    self.list_offset = self.list_offset.saturating_add(WHEEL_STEP);
                }
                // Click a row: select it AND open its detail pane.
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(idx) = self.ask_at(m.column, m.row) {
                        self.select_index(idx);
                        self.open_detail();
                    }
                }
                _ => {}
            },
            // The detail pane shows the worker's scrollable buffer: the wheel
            // and scrollbar scroll it (up = into history, toward the tail =
            // down); a click on a still-visible list row switches the buffer to
            // that ask's worker.
            Mode::Detail => match m.kind {
                MouseEventKind::ScrollUp => self.log.scroll(WHEEL_STEP as isize),
                MouseEventKind::ScrollDown => self.log.scroll(-(WHEEL_STEP as isize)),
                MouseEventKind::Down(MouseButton::Left) => {
                    // Prefer the scrollbar; otherwise a click on a still-visible
                    // list row switches the buffer to that ask.
                    if self.log.scrollbar_grab(m.column, m.row) {
                        self.log.dragging_scrollbar = true;
                    } else if let Some(idx) = self.ask_at(m.column, m.row) {
                        self.select_index(idx);
                    }
                }
                MouseEventKind::Drag(MouseButton::Left) if self.log.dragging_scrollbar => {
                    self.log.scrollbar_scrub(m.row);
                }
                MouseEventKind::Up(MouseButton::Left) => self.log.dragging_scrollbar = false,
                _ => {}
            },
        }
    }

    /// Durably resolve the selected ask with the typed text. On success close
    /// the pane back to the list and refresh (the answered ask leaves the
    /// pending list). On failure — including the ask having vanished (another
    /// client answered it, the worker exited) — STAY in the open pane with the
    /// typed text intact and surface the reason, so the human can copy/edit.
    fn submit(&mut self, paths: &Paths) {
        let Some(id) = self.selected_id.clone() else {
            self.status = Some("no ask selected".into());
            self.mode = Mode::List;
            return;
        };
        if self.input.trim().is_empty() {
            self.status = Some("answer: empty text (esc to cancel)".into());
            return;
        }
        match mailbox::answer(paths, &id, &self.input, false) {
            Ok(()) => {
                self.status = Some(format!("answered {id}"));
                self.input.clear();
                self.mode = Mode::List;
                self.refresh(paths);
            }
            Err(e) => self.status = Some(format!("{id}: {e}")),
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

            // Keep the buffer pointed at the selected ask's worker in BOTH
            // modes so the detail pane opens instantly (no re-parse flash) and
            // feed any newly-appended bytes. The heavy initial parse runs on the
            // LogView's background worker; the incremental tail feed is cheap.
            self.log
                .set_target(self.selected().map(|a| a.worker.clone()));
            self.log.sync(paths);

            terminal.draw(|f| self.draw(f))?;

            // Block up to a tick for the first event, then DRAIN every event
            // already buffered before looping back to draw. A trackpad wheel
            // burst (or any input flood) is thus coalesced into ONE redraw per
            // frame instead of one full redraw per event — which is what made
            // it back up and lag.
            if !event::poll(TICK)? {
                continue;
            }
            loop {
                match event::read()? {
                    Event::Key(k) if k.kind == KeyEventKind::Press => {
                        if self.on_key(k, paths) {
                            return Ok(());
                        }
                    }
                    Event::Mouse(m) => self.on_mouse(m),
                    _ => {}
                }
                if !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
    }

    /// Handle one key press. Returns `true` when the app should quit.
    fn on_key(&mut self, key: KeyEvent, paths: &Paths) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('c')) {
            return true;
        }
        match self.mode {
            // Focus on the list: navigate rows, open the detail pane.
            Mode::List => match key.code {
                KeyCode::Char('q') => return true,
                KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
                KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
                KeyCode::Enter if self.selected().is_some() => self.open_detail(),
                _ => {}
            },
            // The detail pane is open. The bottom input is always focused, so
            // PRINTABLE keys type the answer and Enter submits it. Up/Down keep
            // navigating the LIST even with the pane focused (the pane follows
            // the selection); the read area scrolls via page keys, Ctrl-d/u,
            // the wheel, and the scrollbar. ESC closes back to the list.
            //
            // Note there is no `q`-to-quit here: `q` is a legal answer
            // character. Quit with ESC then `q`, or Ctrl-C anywhere.
            Mode::Detail => {
                // The buffer scrolls up into history and down toward the tail
                // (where the ask sits); `Home`/`End` jump to oldest/live.
                let page = self.log.rows().max(1) as isize;
                match key.code {
                    KeyCode::Esc => self.mode = Mode::List,
                    KeyCode::Enter => self.submit(paths),
                    KeyCode::Backspace => {
                        self.input.pop();
                    }
                    KeyCode::Down => self.move_selection(1),
                    KeyCode::Up => self.move_selection(-1),
                    KeyCode::PageDown => self.log.scroll(-page),
                    KeyCode::PageUp => self.log.scroll(page),
                    KeyCode::Char('d') if ctrl => self.log.scroll(-(page / 2)),
                    KeyCode::Char('u') if ctrl => self.log.scroll(page / 2),
                    KeyCode::End => self.log.follow_tail(),
                    KeyCode::Home => self.log.jump_oldest(),
                    // Everything else printable is answer text.
                    KeyCode::Char(c) if !ctrl => self.input.push(c),
                    _ => {}
                }
            }
        }
        false
    }

    fn draw(&mut self, frame: &mut Frame) {
        let chunks = Layout::vertical([
            Constraint::Length(1), // header
            Constraint::Min(3),    // asks | detail
            Constraint::Length(1), // footer
        ])
        .split(frame.area());
        self.draw_header(frame, chunks[0]);

        // The list is the main view and owns the whole body at full width; the
        // detail pane floats OVER its right (mirroring `looop watch`'s
        // log+picker) and carries the answer input pinned at its bottom.
        let body = chunks[1];
        self.draw_asks(frame, body);
        if matches!(self.mode, Mode::Detail) {
            self.draw_detail(frame, body);
        }
        self.draw_footer(frame, chunks[2]);
    }

    fn draw_header(&self, frame: &mut Frame, area: Rect) {
        let (pulse, pstyle) = if self.pulse_alive {
            ("live", Style::default().fg(Color::Green))
        } else {
            ("DOWN — run `looop up`", Style::default().fg(Color::Red))
        };
        let line = Line::from(vec![
            Span::styled(
                " looop client ",
                Style::default().fg(Color::Black).bg(Color::White),
            ),
            Span::raw("  pulse: "),
            Span::styled(pulse, pstyle),
            Span::raw(format!(
                "  ·  {} running  ·  {} pending",
                self.worker_count,
                self.asks.len()
            )),
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }

    fn draw_asks(&mut self, frame: &mut Frame, area: Rect) {
        // Borderless — like watch's log. With the detail pane CLOSED the list
        // owns the whole width (full screen); when it's OPEN the list shrinks to
        // the left `LIST_W` column and the bordered detail pane floats over the
        // rest, reading as ON TOP rather than as a second split pane.
        let col_w = if matches!(self.mode, Mode::Detail) {
            LIST_W.min(area.width)
        } else {
            area.width
        };
        let dim = dim();

        if self.asks.is_empty() {
            self.asks_hit = None;
            frame.render_widget(
                Paragraph::new(Span::styled(" no pending asks", dim)),
                Rect {
                    width: col_w,
                    ..area
                },
            );
            return;
        }

        // The `Table` draws its own column header on the top row; data rows
        // start one row below. Scroll/hit-test math is over the DATA rows only.
        let visible = (area.height as usize).saturating_sub(1);
        self.list_rows = visible;
        let len = self.asks.len();
        let max_off = len.saturating_sub(visible);
        self.list_offset = self.list_offset.min(max_off);
        let off = self.list_offset;
        let overflow = len > visible;

        // Reserve the rightmost cell for the scrollbar when overflowing.
        let table_w = if overflow {
            col_w.saturating_sub(1)
        } else {
            col_w
        };
        let table_area = Rect {
            width: table_w,
            ..area
        };

        let rows: Vec<Row> = self
            .asks
            .iter()
            .map(|a| {
                let (state, color) = self.state_cell(&a.worker);
                let prompt = a.prompt.split_whitespace().collect::<Vec<_>>().join(" ");
                let row = Row::new(vec![
                    Cell::from(a.id.clone()),
                    Cell::from(fmt_age(a.ts)).style(dim),
                    Cell::from(state).style(Style::default().fg(color)),
                    Cell::from(a.options.join("/")).style(dim),
                    Cell::from(prompt),
                ]);
                // Highlight the selected row via its own style (the widget runs
                // with `selected = None` + a manual offset — see below).
                if Some(a.id.as_str()) == self.selected_id.as_deref() {
                    row.style(Style::default().bg(SURFACE))
                } else {
                    row
                }
            })
            .collect();

        let widths = [
            Constraint::Length(C_ID),
            Constraint::Length(C_AGE),
            Constraint::Length(C_STATE),
            Constraint::Length(C_OPTS),
            Constraint::Min(10),
        ];
        let table = Table::new(rows, widths)
            .header(
                Row::new(["ID", "AGE", "STATE", "OPTS", "PROMPT"])
                    .style(dim.add_modifier(Modifier::BOLD)),
            )
            .column_spacing(1);

        // `selected = None` + a MANUAL offset: with no selection the widget
        // honors our offset verbatim (no snap-to-selection), so the wheel
        // scrolls the view without dragging the highlight around.
        let mut state = TableState::default();
        *state.offset_mut() = off;
        frame.render_stateful_widget(table, table_area, &mut state);

        // Data rows begin one row below the header; the scrollbar + click
        // hit-test target that region.
        let data_area = Rect {
            y: area.y + 1,
            height: area.height.saturating_sub(1),
            ..area
        };
        if overflow {
            render_vscrollbar(
                frame,
                Rect {
                    width: col_w,
                    ..data_area
                },
                max_off,
                off,
            );
        }
        self.asks_hit = Some(AsksHit {
            area: Rect {
                width: table_w,
                ..data_area
            },
            offset: off,
        });
    }

    /// The STATE cell for the worker behind an ask: `running` (green), the
    /// recorded exit state (`exited` dim / `killed` red), or `gone` (dim)
    /// when the session isn't in the session list at all.
    fn state_cell(&self, worker: &str) -> (String, Color) {
        match self.worker_state.get(worker) {
            Some((true, _)) => ("running".into(), Color::Green),
            Some((false, st)) if st == "killed" => (st.clone(), Color::Red),
            Some((false, st)) => (st.clone(), Color::DarkGray),
            None => ("gone".into(), Color::DarkGray),
        }
    }

    /// The floating DETAIL pane — overlaid on the list's right while
    /// `Mode::Detail`. It shows the selected ask's WORKER BUFFER: a scrollable
    /// `looop watch`-style vt100 replay of the worker's `output.log`, with the
    /// ask itself (worker · prompt · ref · options) pinned at the very BOTTOM
    /// as the buffer's tail. The answer input is pinned below that and always
    /// focused — so answering happens at the end of the buffer.
    fn draw_detail(&mut self, frame: &mut Frame, area: Rect) {
        // Floats where the right pane used to sit: anchored to the right, full
        // height, starting just past the ask-list column so that column stays
        // visible on the left. It overlays the list rather than splitting it.
        let list_w = LIST_W.min(area.width);
        let float = Rect {
            x: area.x + list_w,
            y: area.y,
            width: area.width.saturating_sub(list_w).max(1),
            height: area.height,
        };

        let block = Block::default().borders(Borders::ALL).border_style(dim());
        let inner = block.inner(float);
        frame.render_widget(Clear, float);
        frame.render_widget(block, float);

        // Split the inner area: the buffer fills the top; the input takes a
        // separator row + one text row pinned along the bottom.
        let input_h = 2u16.min(inner.height);
        let content = Rect {
            height: inner.height - input_h,
            ..inner
        };
        let input_area = Rect {
            y: inner.y + content.height,
            height: input_h,
            ..inner
        };

        // Build the ask block pinned at the bottom of the buffer. A dim rule
        // separates the worker's own output above from the structured ask.
        let tail: Vec<Line<'static>> = match self.selected().cloned() {
            // The ask vanished (answered elsewhere, worker exited) while open.
            None => vec![Line::from(Span::styled(
                "this ask is no longer pending — esc to close.",
                dim(),
            ))],
            Some(a) => {
                let mut v: Vec<Line<'static>> = vec![
                    Line::from(Span::styled("── ask ──", dim())),
                    Line::from(vec![
                        Span::styled("worker: ", dim()),
                        Span::raw(a.worker.clone()),
                    ]),
                    Line::from(""),
                ];
                // Render the ask prompt as Markdown, lifting the borrowed lines
                // to owned so the LogView can cache them.
                for line in tui_markdown::from_str(&a.prompt).lines {
                    v.push(logview::static_line(&line));
                }
                if !a.reference.is_empty() {
                    v.push(Line::from(""));
                    v.push(Line::from(vec![
                        Span::styled("ref: ", dim()),
                        Span::raw(a.reference.clone()),
                    ]));
                }
                if !a.options.is_empty() {
                    v.push(Line::from(vec![
                        Span::styled("options: ", dim()),
                        Span::raw(a.options.join(", ")),
                    ]));
                }
                v
            }
        };
        // The LogView paints tail lines verbatim (the log grid is fixed-width),
        // so word-wrap the ask to the pane first — long prompts stay readable.
        let tail = wrap_lines(tail, content.width as usize);

        self.log.render(frame, content, &tail, true);
        self.draw_input(frame, input_area);
    }

    /// The always-focused answer editor pinned along the bottom of the detail
    /// pane: a `┈` separator row above a single `› …` input line.
    fn draw_input(&self, frame: &mut Frame, area: Rect) {
        let sep = Block::default().borders(Borders::TOP).border_style(dim());
        let field = sep.inner(area);
        frame.render_widget(sep, area);
        if field.height == 0 {
            return;
        }
        // Single non-wrapping line: `› ` prompt (2 cols) + text + block cursor
        // (1 col). If the answer overflows, show its TAIL (chars, not bytes) so
        // the caret stays visible — horizontal scroll rather than run-off.
        let avail = (field.width as usize).saturating_sub(3);
        let chars: Vec<char> = self.input.chars().collect();
        let shown: String = if chars.len() > avail {
            chars[chars.len() - avail..].iter().collect()
        } else {
            self.input.clone()
        };
        let mut spans = vec![Span::styled("› ", dim())];
        if self.input.is_empty() {
            // Block cursor first, then a dim placeholder so the field reads as
            // focused and self-explanatory before anything is typed.
            spans.push(Span::styled(" ", Style::default().bg(Color::White)));
            spans.push(Span::styled(" type answer · enter to send", dim()));
        } else {
            spans.push(Span::raw(shown));
            spans.push(Span::styled(" ", Style::default().bg(Color::White)));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), field);
    }

    fn draw_footer(&self, frame: &mut Frame, area: Rect) {
        let style = Style::default().bg(SURFACE).fg(Color::White);
        let help = match &self.status {
            Some(msg) => format!(" {msg} "),
            None => match self.mode {
                Mode::List => " ↑/↓ move · enter open · q quit ".to_string(),
                Mode::Detail => {
                    " type answer · enter send · ↑/↓ move · pgup/pgdn scroll · esc close "
                        .to_string()
                }
            },
        };
        frame.render_widget(Paragraph::new(Span::styled(help, style)).style(style), area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn wrap_breaks_at_spaces_and_preserves_text() {
        let lines = vec![Line::from("the quick brown fox jumps")];
        let out = wrap_lines(lines, 10);
        // Each wrapped row fits the width and no word is split at a space break.
        assert!(out.iter().all(|l| plain(l).chars().count() <= 10));
        let joined: String = out.iter().map(plain).collect::<Vec<_>>().join(" ");
        assert_eq!(joined, "the quick brown fox jumps");
    }

    #[test]
    fn wrap_hard_splits_overlong_word() {
        let lines = vec![Line::from("abcdefghijklmnop")];
        let out = wrap_lines(lines, 5);
        assert_eq!(out.len(), 4); // 16 chars / 5
        assert!(out.iter().all(|l| plain(l).chars().count() <= 5));
        let joined: String = out.iter().map(|l| plain(l)).collect();
        assert_eq!(joined, "abcdefghijklmnop");
    }

    #[test]
    fn wrap_preserves_span_styles() {
        let styled = Line::from(vec![
            Span::styled("aaa", Style::default().fg(Color::Red)),
            Span::styled(" bbb", Style::default().fg(Color::Green)),
        ]);
        let out = wrap_lines(vec![styled], 3);
        // "aaa" then "bbb" on separate rows, keeping their colors.
        assert_eq!(plain(&out[0]), "aaa");
        assert_eq!(out[0].spans[0].style.fg, Some(Color::Red));
        assert_eq!(plain(&out[1]), "bbb");
        assert_eq!(out[1].spans.last().unwrap().style.fg, Some(Color::Green));
    }

    #[test]
    fn wrap_zero_width_is_identity() {
        let lines = vec![Line::from("anything")];
        let out = wrap_lines(lines.clone(), 0);
        assert_eq!(out.len(), 1);
        assert_eq!(plain(&out[0]), "anything");
    }
}
