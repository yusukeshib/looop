//! `looop client` — a minimal, non-agent TUI for watching the fleet and
//! answering worker asks.
//!
//! looop's steering surface is the `looop _ …` CONTRACT. The RECOMMENDED client
//! is an AGENT concierge — start any coding agent and tell it to "work as a
//! concierge for the `looop` command" — that watches for asks, relays them to
//! the human in plain language with a recommendation, and drives the
//! `_ answer` / `_ goal` / `_ playbook` verbs. This command is the humble,
//! hand-driven alternative: a TUI where the WHOLE live fleet — the pulse plus
//! every running worker — is ALWAYS on screen, waiting agents float to the top,
//! and the human answers each ask themselves.
//!
//! It is deliberately less capable than the concierge (no plain-language
//! framing, no recommendation, no steering) — that's the point. Its job is to
//! make looop's design legible: the loop decides and acts on its own; the ONE
//! thing it defers to a human is a worker's blocking ask, and this window is
//! that human ⇄ mailbox channel, laid bare.
//!
//! The fleet list is a full-width TABLE (id · age · state · options · prompt
//! preview). Every agent is a row: the pulse first, then the workers, with any
//! worker blocked on a pending ask sorted to the top so it's what you see. A
//! row with no ask (the pulse, an idle worker) reads dim. Opening a row
//! (ENTER/click) floats a DETAIL pane over the right that shows THAT AGENT'S
//! LIVE BUFFER — a scrollable `looop watch`-style vt100 replay of its
//! `output.log`, with the pending ask (if any) pinned at the very BOTTOM. So
//! you read the agent's own transcript, scroll back through its history, and
//! answer the question right where it sits, at the end of the buffer. ESC
//! closes it back to the list:
//!
//! ```text
//!   ID          AGE  STATE    PROMPT        ┌──────────────────────┐
//! > triage-2    2m   running  flaky test…   │ …worker output…      ┃ │
//!   deploy-3    0s   running  dep upgrade…  │ running tests…       ┃ │
//!   pulse       —    live     control loop  │ ── ask ──             │ │
//!   builder-1   5m   running  —             │ <the question>       │ │
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
//! Read + one narrow write: it lists the fleet (`session::list_workers` +
//! `run::pulse_running`) with pending asks merged on (`mailbox::pending`) and,
//! on submit, durably resolves the selected agent's ask (`mailbox::answer`). It
//! never spawns a worker or edits policy — for that, use the agent concierge or
//! the raw `_` verbs.

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

/// Compact relative age from a second count: `5s` / `3m` / `2h` / `4d`.
fn fmt_secs(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

/// Compact relative age of a unix timestamp. Shown dim next to each row so the
/// list conveys how long an ask has been waiting (or an agent has been idle).
fn fmt_age(ts: u64) -> String {
    fmt_secs(crate::util::now_unix().saturating_sub(ts))
}

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

/// Which dead workers the fleet list includes (alive workers, the pulse, and
/// any worker holding a pending ask are ALWAYS shown). Toggled live with
/// `Tab`. Finished workers older than the session TTL are reaped by the pulse,
/// so `All` is naturally bounded by that retention window.
#[derive(Clone, Copy)]
enum Filter {
    /// Only the live fleet — finished/dead workers hidden. The default.
    Active,
    /// Every worker still on disk, including finished/dead ones.
    All,
}

/// `looop client` — bring up the ask-answering TUI.
pub fn cmd_client(paths: &Paths, args: &crate::cli::ClientArgs) -> Result<ExitCode> {
    let filter = if args.all {
        Filter::All
    } else {
        Filter::Active
    };
    let mut terminal = ratatui::init();
    // Capture the mouse so wheel/click/drag reach us as `Event::Mouse` instead
    // of letting the terminal scroll its alternate screen (mirrors `watch`).
    let _ = execute!(std::io::stdout(), EnableMouseCapture);
    let res = App::new(args.id.clone(), filter).run(&mut terminal, paths);
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    res?;
    Ok(ExitCode::SUCCESS)
}

/// The list's on-screen geometry, captured during `draw_asks` so a mouse
/// click can be mapped back to the agent row under the cursor (mirrors watch's
/// `SelectorHit`).
#[derive(Clone, Copy)]
struct AsksHit {
    /// Inner area the rows render into (inside the border).
    area: Rect,
    /// First visible row index (the list's scroll offset), so a click on row
    /// `r` selects agent `offset + (r - area.top())`.
    offset: usize,
    /// Max scroll offset (`len - visible`), for scrollbar drag mapping. Zero
    /// when the list fits and no scrollbar is drawn.
    max_off: usize,
}

/// One row of the client list: an AGENT (the pulse or a worker session), with
/// its pending ask attached when it has one. The list shows the whole live
/// fleet — the pulse plus every running worker — not just the agents that are
/// currently blocked on a human, so an idle worker is still visible.
#[derive(Clone)]
struct AgentRow {
    /// Session id — a worker id, or [`session::PULSE_SESSION`] for the pulse.
    id: String,
    /// The control loop's row (rendered first, styled distinctly).
    is_pulse: bool,
    /// Whether the underlying session is currently alive.
    alive: bool,
    /// Session state string (`running` / `exited` / `killed` / `gone`), or
    /// `live` / `down` for the pulse.
    state: String,
    /// Precomputed relative age for the AGE column (ask age when waiting, else
    /// the session's idle time).
    age: String,
    /// Time since the session's last state change — the sort key that keeps
    /// the list in most-recently-active/finished order. `None` sorts last.
    idle: Option<Duration>,
    /// This agent's pending ask, if it is currently blocked on a human.
    ask: Option<Ask>,
}

struct App {
    /// The live fleet: pulse + workers, each with its pending ask (if any).
    rows: Vec<AgentRow>,
    /// Top visible row (viewport scroll). The WHEEL scrolls this directly and
    /// leaves the selection put; arrows/click move the selection and only nudge
    /// this enough to keep the selected row visible. Decoupling the two is why
    /// the list widget is driven with `selected = None` + a manual offset, and
    /// the highlight is painted onto the selected item's own style instead.
    list_offset: usize,
    /// Visible list rows from the last draw — selection-follow + wheel clamp.
    list_rows: usize,
    /// The selected AGENT, tracked by its STABLE session id — not by list
    /// index. The fleet re-sorts every tick and agents/asks come and go, so an
    /// index would silently point at a different agent (and, mid-answer, drift
    /// the answer onto the wrong worker). The id is the source of truth; the
    /// list index is derived from it each refresh.
    selected_id: Option<String>,
    /// The answer being typed in the pinned input (active whenever the selected
    /// agent has a pending ask).
    input: String,
    /// Last outcome to show in the footer (an error, or an "answered X" note).
    status: Option<String>,
    /// The selected agent's live buffer shown in the detail pane: a scrollable
    /// vt100 replay of its `output.log`, with the pending ask (if any) pinned at
    /// the very bottom as the LogView's `tail`. Answering thus happens "at the
    /// end of the buffer" — mirroring (and, eventually, replacing) `looop
    /// watch`. Owns its own scroll model, background parse, and scrollbar.
    log: LogView,
    /// Whether the pulse (control loop) currently holds its single-instance
    /// lock — the pulse row's state.
    pulse_alive: bool,
    /// Geometry of the list from the last draw, for click→row hit-testing.
    asks_hit: Option<AsksHit>,
    /// Whether a drag on the bottom list's scrollbar is in progress — mirrors
    /// `LogView::dragging_scrollbar` for the main buffer.
    dragging_list_sb: bool,
    /// Which dead workers the list includes. Toggled live with `Tab`.
    filter: Filter,
    /// Count of workers hidden by the current filter — shown in the footer.
    hidden: usize,
    /// A `--id`/`looop client <id>` preselect to honor once the row appears.
    pending_select: Option<String>,
}

impl App {
    fn new(initial: Option<String>, filter: Filter) -> Self {
        Self {
            rows: Vec::new(),
            list_offset: 0,
            list_rows: 0,
            selected_id: None,
            input: String::new(),
            status: None,
            log: LogView::new(),
            pulse_alive: false,
            asks_hit: None,
            dragging_list_sb: false,
            filter,
            hidden: 0,
            pending_select: initial,
        }
    }

    /// Rebuild the live fleet (pulse + workers, each with its pending ask) and
    /// re-check the pulse, reconciling the selection by id. There is ALWAYS one
    /// row selected: if the selected id is still present its row index is
    /// refreshed; if it vanished (or nothing was selected yet) we fall back to
    /// the top row (the pulse).
    fn refresh(&mut self, paths: &Paths) {
        self.pulse_alive = run::pulse_running(paths);

        // worker id → its earliest pending ask (a blocked worker has one).
        // `mailbox::pending` is sorted by ts asc, so `or_insert` keeps the
        // oldest ask per worker.
        let mut ask_by_worker: HashMap<String, Ask> = HashMap::new();
        for ask in mailbox::pending(paths) {
            ask_by_worker.entry(ask.worker.clone()).or_insert(ask);
        }

        let workers = session::list_workers(paths);

        // The pulse (control loop) is always the top row.
        let mut rows = vec![AgentRow {
            id: session::PULSE_SESSION.to_string(),
            is_pulse: true,
            alive: self.pulse_alive,
            state: if self.pulse_alive { "live" } else { "down" }.to_string(),
            age: String::new(),
            idle: None,
            ask: None,
        }];

        // Worker agents. A pending ask (answerable) or a live worker is ALWAYS
        // shown; a dead worker with nothing waiting is subject to the filter
        // (Active hides it, All keeps it).
        let mut wrows: Vec<AgentRow> = Vec::new();
        let mut hidden = 0usize;
        for s in workers {
            let ask = ask_by_worker.remove(&s.id);
            let keep = ask.is_some()
                || s.alive
                || match self.filter {
                    Filter::All => true,
                    Filter::Active => false,
                };
            if !keep {
                hidden += 1;
                continue;
            }
            let idle = s.idle_for();
            let age = match &ask {
                Some(a) => fmt_age(a.ts),
                None => idle.map(|d| fmt_secs(d.as_secs())).unwrap_or_default(),
            };
            wrows.push(AgentRow {
                id: s.id.clone(),
                is_pulse: false,
                alive: s.alive,
                state: s.state.clone(),
                age,
                idle,
                ask,
            });
        }
        // Any ask whose worker isn't in the session list at all (session reaped
        // but the ask lingers) — surface it so it stays answerable.
        for (worker, ask) in ask_by_worker.drain() {
            wrows.push(AgentRow {
                id: worker,
                is_pulse: false,
                alive: false,
                state: "gone".to_string(),
                age: fmt_age(ask.ts),
                idle: None,
                ask: Some(ask),
            });
        }
        // Waiting agents (with a pending ask) first — what a human acts on —
        // longest-waiting ask on top. Then the rest in most-recently-
        // active/finished order (smallest idle first; unknown idle last),
        // id as the final tiebreak for stability.
        wrows.sort_by(|a, b| {
            match (&a.ask, &b.ask) {
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                // Oldest ask (longest waiting) on top.
                (Some(x), Some(y)) => x.ts.cmp(&y.ts),
                // Smallest idle (most recently active/finished) first;
                // unknown idle sinks to the bottom.
                (None, None) => {
                    let key = |r: &AgentRow| r.idle.unwrap_or(Duration::MAX);
                    key(a).cmp(&key(b))
                }
            }
            .then_with(|| a.id.cmp(&b.id))
        });
        rows.extend(wrows);
        self.rows = rows;
        self.hidden = hidden;

        // Honor a `looop client <id>` preselect once that row shows up.
        if let Some(want) = self.pending_select.clone()
            && self.rows.iter().any(|r| r.id == want)
        {
            self.selected_id = Some(want);
            self.pending_select = None;
        }

        match self.selected_index() {
            // Still listed: leave the viewport ALONE. The wheel/scrollbar scroll
            // the list freely and the highlight may scroll out of view (like the
            // buffer); snapping it back into view every tick would fight the
            // user's scroll. `ensure_visible` runs only on explicit selection
            // moves (arrows/click), and `draw_asks` clamps the offset to range.
            Some(_) => {}
            // ALWAYS keep one row selected — default to the top row (the
            // pulse). There is no "nothing selected" state: launch, and every
            // refresh, leaves exactly one row highlighted (buffer shown).
            None => {
                self.selected_id = self.rows.first().map(|r| r.id.clone());
                self.list_offset = 0;
            }
        }
    }

    /// Row index of the currently-selected agent id, if it is still listed.
    fn selected_index(&self) -> Option<usize> {
        let id = self.selected_id.as_deref()?;
        self.rows.iter().position(|r| r.id == id)
    }

    fn selected(&self) -> Option<&AgentRow> {
        self.selected_index().map(|i| &self.rows[i])
    }

    fn move_selection(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() as isize - 1;
        let idx = match self.selected_index() {
            // Move relative to the current selection.
            Some(cur) => (cur as isize).saturating_add(delta).clamp(0, last) as usize,
            // Nothing selected yet (fresh start): the first arrow enters the
            // list at the top rather than skipping row 0.
            None => 0,
        };
        self.select_index(idx);
    }

    /// Point the selection at row `idx`. When it actually CHANGES the target
    /// agent, reset the detail scroll and clear any half-typed answer — so a
    /// pending answer can never be submitted against a different agent than the
    /// one it was typed for. Shared by keyboard + mouse selection.
    fn select_index(&mut self, idx: usize) {
        // Navigating away dismisses any lingering action status (e.g. a stale
        // "no pending ask to answer") so the footer doesn't sit stuck on it.
        self.status = None;
        let id = self.rows[idx].id.clone();
        if self.selected_id.as_deref() != Some(id.as_str()) {
            // New target agent: follow its buffer's tail and drop any half-typed
            // answer so it can't land on the wrong worker. The LogView re-points
            // to the new agent's log on the next `sync`.
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

    /// Row index of the agent under a click at `(col, row)`, if it landed on a
    /// real row of the list (mirrors watch's `select_at` hit-test).
    fn ask_at(&self, col: u16, row: u16) -> Option<usize> {
        let hit = self.asks_hit?;
        let a = hit.area;
        if col < a.left() || col >= a.right() || row < a.top() || row >= a.bottom() {
            return None;
        }
        let idx = hit.offset + (row - a.top()) as usize;
        (idx < self.rows.len()).then_some(idx)
    }

    /// Whether `row` falls in the bottom list strip (its column-header row plus
    /// the data rows) — used to route the wheel to the list vs. the buffer.
    fn in_list_region(&self, row: u16) -> bool {
        self.asks_hit.is_some_and(|hit| {
            // The header sits one row above the data area.
            let top = hit.area.top().saturating_sub(1);
            row >= top && row < hit.area.bottom()
        })
    }

    /// Begin a drag on the bottom list's scrollbar. Returns `true` (and scrubs
    /// the offset) if `(col, row)` landed on the bar's rightmost column within
    /// the track. Mirrors `LogView::scrollbar_grab` for the main buffer.
    fn list_scrollbar_grab(&mut self, col: u16, row: u16) -> bool {
        let Some(hit) = self.asks_hit else {
            return false;
        };
        let a = hit.area;
        if hit.max_off == 0
            || col + 1 < a.right()
            || col > a.right()
            || row < a.top()
            || row >= a.bottom()
        {
            return false;
        }
        self.list_scrollbar_scrub(row);
        true
    }

    /// Scrub the list offset to `row` on the scrollbar track (column ignored,
    /// used while a grab is held): top = first row, bottom = last page.
    fn list_scrollbar_scrub(&mut self, row: u16) {
        let Some(hit) = self.asks_hit else {
            return;
        };
        let a = hit.area;
        let span = a.height.saturating_sub(1);
        let clamped = row.clamp(a.top(), a.bottom().saturating_sub(1));
        self.list_offset = if span == 0 {
            0
        } else {
            let frac = (clamped - a.top()) as f64 / span as f64;
            (frac * hit.max_off as f64).round() as usize
        };
    }

    /// Route a mouse event. The buffer (top) and the bottom list strip scroll
    /// independently — the wheel targets whichever the cursor is over; the
    /// buffer and list each have a draggable scrollbar; a click on a list row
    /// switches the selected agent (and thus the buffer).
    fn on_mouse(&mut self, m: MouseEvent) {
        match m.kind {
            // The wheel scrolls whichever pane the cursor is over: the
            // bottom list strip scrolls its view, the buffer above scrolls
            // into history (up) / toward the tail (down).
            MouseEventKind::ScrollUp => {
                if self.in_list_region(m.row) {
                    self.list_offset = self.list_offset.saturating_sub(WHEEL_STEP);
                } else {
                    self.log.scroll(WHEEL_STEP as isize);
                }
            }
            MouseEventKind::ScrollDown => {
                if self.in_list_region(m.row) {
                    self.list_offset = self.list_offset.saturating_add(WHEEL_STEP);
                } else {
                    self.log.scroll(-(WHEEL_STEP as isize));
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                // Prefer a scrollbar grab (list strip or buffer); otherwise a
                // click on a still-visible list row switches the buffer to
                // that ask. Always (re)assign grab results so a plain click
                // clears any stuck drag state (e.g. a missed mouse-up).
                self.dragging_list_sb = self.list_scrollbar_grab(m.column, m.row);
                self.log.dragging_scrollbar =
                    !self.dragging_list_sb && self.log.scrollbar_grab(m.column, m.row);
                if !self.dragging_list_sb
                    && !self.log.dragging_scrollbar
                    && let Some(idx) = self.ask_at(m.column, m.row)
                {
                    self.select_index(idx);
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.dragging_list_sb => {
                self.list_scrollbar_scrub(m.row);
            }
            MouseEventKind::Drag(MouseButton::Left) if self.log.dragging_scrollbar => {
                self.log.scrollbar_scrub(m.row);
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.log.dragging_scrollbar = false;
                self.dragging_list_sb = false;
            }
            _ => {}
        }
    }

    /// Durably resolve the selected agent's pending ask with the typed text. On
    /// success clear the input and refresh (the answered ask leaves the pending
    /// list; the buffer stays on the same agent). On failure — including the ask
    /// having vanished (another client answered it, the worker exited) or the
    /// selected agent having none — keep the typed text intact and surface the
    /// reason, so the human can copy/edit.
    fn submit(&mut self, paths: &Paths) {
        let Some(id) = self
            .selected()
            .filter(|r| r.alive)
            .and_then(|r| r.ask.as_ref())
            .map(|a| a.id.clone())
        else {
            self.status = Some(match self.selected() {
                Some(r) => format!("{}: no pending ask to answer", r.id),
                None => "no agent selected".into(),
            });
            return;
        };
        if self.input.trim().is_empty() {
            self.status = Some("answer: type some text first".into());
            return;
        }
        match mailbox::answer(paths, &id, &self.input, false) {
            Ok(()) => {
                self.status = Some(format!("answered {id}"));
                self.input.clear();
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

            // Keep the buffer pointed at the selected agent's session in BOTH
            // modes so the detail pane opens instantly (no re-parse flash) and
            // feed any newly-appended bytes. The heavy initial parse runs on the
            // LogView's background worker; the incremental tail feed is cheap.
            self.log.set_target(self.selected().map(|r| r.id.clone()));
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
    ///
    /// There is a single unified view: the selected agent's buffer with the
    /// answer input always focused (when it has a pending ask). PRINTABLE keys
    /// type the answer; Enter submits. Up/Down switch the selected agent (the
    /// buffer follows); the buffer scrolls via page keys, Ctrl-d/u, the wheel,
    /// and the scrollbar. Quit is Ctrl-C ONLY — so `q`/`j`/`k`/Esc are all
    /// legal answer characters, never control keys.
    fn on_key(&mut self, key: KeyEvent, paths: &Paths) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('c')) {
            return true;
        }
        // The buffer scrolls up into history and down toward the tail (where the
        // ask sits); `Home`/`End` jump to oldest/live.
        let page = self.log.rows().max(1) as isize;
        match key.code {
            KeyCode::Enter => self.submit(paths),
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Down => self.move_selection(1),
            KeyCode::Up => self.move_selection(-1),
            // Tab toggles whether finished/dead workers are listed. It's not
            // a printable answer char, so it can't collide with typing an
            // answer.
            KeyCode::Tab => {
                self.filter = match self.filter {
                    Filter::Active => Filter::All,
                    Filter::All => Filter::Active,
                };
                self.refresh(paths);
            }
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
        false
    }

    fn draw(&mut self, frame: &mut Frame) {
        let chunks = Layout::vertical([
            Constraint::Min(3),    // asks | detail
            Constraint::Length(1), // footer
        ])
        .split(frame.area());

        // Flex layout, driven by the SELECTION (not a separate mode): with a
        // row selected — which is always the case — the agent's buffer takes
        // the TOP (flex) and the list shrinks to a scrollable strip of at most
        // 5 rows pinned at the BOTTOM. With nothing selected (only before the
        // first refresh) the list owns the whole body.
        let body = chunks[0];
        if self.selected().is_some() {
            // List height = up to 5 data rows + 1 column-header row + 2 border
            // rows; keep ≥3 rows for the (borderless) buffer above.
            let data_rows = self.rows.len().clamp(1, 5) as u16;
            let want = data_rows + 3;
            let list_h = want.min(body.height.saturating_sub(3)).max(3);
            let parts =
                Layout::vertical([Constraint::Min(3), Constraint::Length(list_h)]).split(body);
            self.draw_detail(frame, parts[0]);
            self.draw_asks(frame, parts[1]);
        } else {
            self.draw_asks(frame, body);
        }
        self.draw_footer(frame, chunks[1]);
    }

    fn draw_asks(&mut self, frame: &mut Frame, area: Rect) {
        // The list pane is BORDERED (the buffer above is borderless), so the
        // agent strip reads as a distinct panel. All geometry below — table,
        // scrollbar, hit-testing — is relative to the block's INNER area.
        let dim = dim();
        let block = Block::default().borders(Borders::ALL).border_style(dim);
        let inner = block.inner(area);
        frame.render_widget(Clear, area);
        frame.render_widget(block, area);
        let area = inner;
        let col_w = area.width;

        if self.rows.is_empty() {
            self.asks_hit = None;
            frame.render_widget(
                Paragraph::new(Span::styled(" no agents", dim)),
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
        let len = self.rows.len();
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
            .rows
            .iter()
            .map(|r| {
                let (state, state_style) = self.state_cell(r);
                // The PROMPT column shows the pending ask (what a human acts
                // on); an agent with no ask reads dim — the pulse as its role,
                // an idle worker as a `—` placeholder.
                let (opts, prompt, prompt_style) = match &r.ask {
                    Some(a) => {
                        let prompt = a.prompt.split_whitespace().collect::<Vec<_>>().join(" ");
                        (a.options.join("/"), prompt, Style::default())
                    }
                    None if r.is_pulse => (String::new(), "control loop".to_string(), dim),
                    None => (String::new(), "—".to_string(), dim),
                };
                let row = Row::new(vec![
                    Cell::from(r.id.clone()),
                    Cell::from(r.age.clone()).style(dim),
                    Cell::from(state).style(state_style),
                    Cell::from(opts).style(dim),
                    Cell::from(prompt).style(prompt_style),
                ]);
                // Highlight the selected row via its own style (the widget runs
                // with `selected = None` + a manual offset — see below).
                if Some(r.id.as_str()) == self.selected_id.as_deref() {
                    row.style(Style::default().bg(SURFACE))
                } else {
                    row
                }
            })
            .collect();

        // Size the ID column to the widest id actually present, using
        // `chars().count()` as a cheap approximation of display width
        // (ids are user-chosen path segments and in practice ASCII, so this
        // matches; it can under-count for wide/combining chars, but that
        // only risks a slightly tight column, never a panic) instead of a
        // fixed 16 that clips longer ids. Clamp
        // to `C_ID` as a floor and leave at least `MIN_PROMPT` cells for the
        // PROMPT column so a long id can't crowd out what a human acts on.
        const MIN_PROMPT: u16 = 20;
        let fixed = C_AGE + C_STATE + C_OPTS; // non-id, non-prompt columns
        let spacing = 4; // 4 gaps between 5 columns at column_spacing(1)
        let id_ceiling = table_w
            .saturating_sub(fixed + spacing + MIN_PROMPT)
            .max(C_ID);
        let id_w = self
            .rows
            .iter()
            .map(|r| r.id.chars().count() as u16)
            .chain(std::iter::once(2)) // the "ID" header
            .max()
            .unwrap_or(C_ID)
            .clamp(C_ID, id_ceiling);
        let widths = [
            Constraint::Length(id_w),
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
            max_off: if overflow { max_off } else { 0 },
        });
    }

    /// The STATE cell for an agent row: an agent with a pending ask reads
    /// `pending` (bold yellow) so the row awaiting a human answer stands out.
    /// Otherwise the pulse reads `live` (green) / `down` (red); a worker reads
    /// `running` (green), its recorded exit state (`killed` red, others dim),
    /// or `gone` (dim) when reaped out from under a lingering ask.
    fn state_cell(&self, row: &AgentRow) -> (String, Style) {
        // A pending ask is what a human must act on — surface it as its own
        // attention state (bold yellow), overriding the underlying liveness.
        if row.ask.is_some() {
            return (
                "pending".to_string(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            );
        }
        let color = if row.alive {
            Color::Green
        } else if row.state == "killed" || (row.is_pulse && row.state == "down") {
            Color::Red
        } else {
            Color::DarkGray
        };
        (row.state.clone(), Style::default().fg(color))
    }

    /// The DETAIL pane — the TOP flex region (the list sits below it) shown
    /// whenever a row is selected. It shows the selected ask's WORKER BUFFER: a scrollable
    /// `looop watch`-style vt100 replay of the worker's `output.log`, with the
    /// ask itself (worker · prompt · ref · options) pinned at the very BOTTOM
    /// as the buffer's tail. The answer input is pinned below that and focused
    /// — so answering happens at the end of the buffer — but ONLY when the
    /// selected agent has a pending ask; for a read-only agent (the pulse, an
    /// idle worker) the buffer fills the whole pane with no input row.
    fn draw_detail(&mut self, frame: &mut Frame, area: Rect) {
        // Borderless — like `looop watch`'s log, the buffer owns its area edge
        // to edge; the bordered list below provides the visual seam.
        let inner = area;
        frame.render_widget(Clear, area);

        // The answer input only exists when the selected agent has a pending
        // ask — a read-only agent (pulse / idle worker) has nothing to answer,
        // so the buffer takes the whole pane. Split the inner area: buffer on
        // top; when answerable, the input takes a separator + one text row
        // pinned along the bottom.
        let answerable = self.selected().is_some_and(|r| r.alive && r.ask.is_some());
        let input_h = if answerable {
            2u16.min(inner.height)
        } else {
            0
        };
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
            // The agent vanished from the fleet while its pane was open.
            None => vec![Line::from(Span::styled(
                "this agent is no longer listed — esc to close.",
                dim(),
            ))],
            // A live agent with nothing to answer — the pane is a read-only
            // transcript viewer (the pulse, or an idle/finished worker), so
            // there's no footer: the absent input area speaks for itself.
            Some(r) if r.ask.is_none() => vec![],
            Some(r) => {
                let a = r.ask.expect("ask present");
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
        if answerable {
            self.draw_input(frame, input_area);
        }
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
        // The filter badge (with any hidden-worker count) leads the hint line,
        // mirroring `looop watch`'s selector footer.
        let fname = match self.filter {
            Filter::Active => "active",
            Filter::All => "all",
        };
        let hidden = if self.hidden > 0 {
            format!(" ({} hidden)", self.hidden)
        } else {
            String::new()
        };
        let help = match &self.status {
            Some(msg) => format!(" {msg} "),
            // The answer keys only apply when the selected agent has a pending
            // ask; a read-only agent (pulse / idle worker) just scrolls.
            None if self.selected().is_some_and(|r| r.alive && r.ask.is_some()) => format!(
                " {fname}{hidden}  type answer · enter send · ↑/↓ switch · tab filter · pgup/pgdn scroll · ^c quit "
            ),
            None => {
                format!(" {fname}{hidden}  ↑/↓ switch · tab filter · pgup/pgdn scroll · ^c quit ")
            }
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
