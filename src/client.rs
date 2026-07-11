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
//! The fleet list is a full-width TABLE (id · age · state). Every agent is a
//! row: the pulse first, then the workers, with any worker blocked on a
//! pending ask sorted to the top and marked `pending` so it's what you see. A
//! row with no ask (the pulse, an idle worker) reads dim. Opening a row
//! (ENTER/click) floats a DETAIL pane over the right that shows THAT AGENT'S
//! LIVE BUFFER — a scrollable `looop watch`-style vt100 replay of its
//! `output.log`. The pending ask (if any) sits in its OWN bordered box between
//! the buffer and the input — prompt only, no metadata — so the transcript
//! stays a pure transcript and the question never scrolls away with it. ESC
//! closes it back to the list:
//!
//! ```text
//!    active · type answer · enter send · ↑/↓ switch · pgup/pgdn scroll
//!   ID          AGE  STATE     ┌──────────────────────┐
//! > triage-2    2m   pending   │ …worker output…      ┃ │
//!   deploy-3    0s   running   │ running tests…       ┃ │
//!   pulse       —    live      │ ┌─ ask ───────────┐ │ │
//!   builder-1   5m   running   │ │ <the question>   │ │ │
//!                              │ └─────────────────┘ │ │
//!                              │ ┌─────────────────┐ │ │
//!                              │ │ ship█            │ │ │
//!                              │ └─────────────────┘ │ │
//!                              └──────────────────────┘
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
use unicode_width::UnicodeWidthChar;

/// Re-list asks / re-check the pulse this often, and the input-poll timeout.
const TICK: Duration = Duration::from_millis(250);

/// Rows scrolled per mouse-wheel notch (list and detail alike).
const WHEEL_STEP: usize = 3;
/// Body width at/above which the list+buffer sit SIDE BY SIDE (list left, buffer
/// right) instead of stacked (list top, buffer bottom). Below it the terminal is
/// too narrow to give both panes a usable column count, so we stack.
const WIDE_MIN: u16 = 90;
/// Floor for the draggable left-pane width (cols).
const LIST_MIN_W: u16 = 12;

/// The shared dark-surface background (same as `looop watch`): the selected
/// row's highlight and the status bar. Dark enough that per-span colors
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

/// Fixed column widths (cells) for the ask table: age and state. The ID
/// column sizes itself to the widest id present (`C_ID` is its floor).
const C_ID: u16 = 16;
const C_AGE: u16 = 5;
const C_STATE: u16 = 8;

/// Render the shared `looop watch`-style vertical scrollbar into `area`'s right
/// column: a `┃` thumb over a `│` track, no end caps. `pos` is the top-anchored
/// offset in `0..=max_scroll`. A no-op when nothing overflows (`max_scroll==0`).
fn render_vscrollbar(frame: &mut Frame, area: Rect, max_scroll: usize, pos: usize) {
    if max_scroll == 0 {
        return;
    }
    // ratatui sizes the thumb as `viewport * track / (content + viewport)`;
    // inflate the viewport until the thumb is at least `min_thumb` rows so it
    // stays grabbable on a long list (affects only the thumb SIZE, not the
    // position mapping). The floor ADAPTS to the track: 4 rows on a tall pane
    // (the buffer), but never more than half a short track (the 5-row agent
    // list) — otherwise the floor dominates and the thumb reads near-full
    // regardless of how much content is hidden.
    const MIN_THUMB: usize = 4;
    let track = area.height as usize;
    let min_thumb = MIN_THUMB.min(track / 2).max(1);
    let viewport = if track > min_thumb {
        (min_thumb * max_scroll.saturating_sub(1))
            .div_ceil(track - min_thumb)
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
/// hard-split. Width is counted in display columns via `unicode-width`, so CJK
/// double-width glyphs (and other wide characters) consume two columns and wrap
/// before they run off the right edge. The `LogView` paints its `tail` verbatim
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
            // Walk forward accumulating DISPLAY columns (CJK glyphs = 2), so the
            // window ends at the last char that still fits within `width` cols.
            let mut end = start;
            let mut cols = 0usize;
            while end < chars.len() {
                // `width()` returns None for control chars (e.g. `\t`); treat
                // those as at least 1 column so they still advance the window
                // and can't silently disable wrapping.
                let w = chars[end].0.width().unwrap_or(1);
                if end > start && cols + w > width {
                    break;
                }
                cols += w;
                end += 1;
            }
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

/// What the always-on bottom input does for the SELECTED agent. The field is
/// always shown for an agent the human can act on; the verb (and thus where
/// Enter routes the text) depends on the agent's state.
#[derive(Clone, Copy, PartialEq)]
enum InputMode {
    /// The agent has a pending ask — Enter durably resolves it (`mailbox::answer`).
    Answer,
    /// A live worker with no ask — Enter types the text into its terminal as a
    /// STEER (`session::send_to`). Not offered for the pulse (raw keystrokes
    /// are refused) or a dead worker (nothing to type into).
    Send,
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
    /// Rows each agent occupies: 1 in the stacked table layout (one row per
    /// agent), or the card height in the side-by-side layout (2: an id line +
    /// a `state age` line). A click at `row` maps to agent
    /// `offset + (row - top) / stride`.
    stride: usize,
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
    /// Last outcome to show in the status bar (an error, or an "answered X" note).
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
    /// Width (cols) of the left list pane in the side-by-side layout, dragged
    /// live via the vertical divider. Clamped to a sane range each draw.
    list_w: u16,
    /// The divider's screen column from the last WIDE draw (for a drag grab);
    /// `None` when the side-by-side layout isn't active.
    divider_col: Option<u16>,
    /// The left pane's origin column from the last draw — the drag maps a
    /// pointer column back to a pane width via `col - origin`.
    list_origin_x: u16,
    /// Whether a drag on the left/right divider is in progress.
    dragging_divider: bool,
    /// Which dead workers the list includes. Toggled live with `Tab`.
    filter: Filter,
    /// Count of workers hidden by the current filter — shown in the status bar.
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
            list_w: 20,
            divider_col: None,
            list_origin_x: 0,
            dragging_divider: false,
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

        // Pulse uptime: age of the `pid` file, which the pulse REWRITES each
        // time it acquires the lock (the lock file itself is never truncated,
        // so its mtime is stale). Empty when the pulse is down or the file is
        // gone.
        let pulse_age = if self.pulse_alive {
            std::fs::metadata(paths.lock().join("pid"))
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .map(|d| fmt_secs(d.as_secs()))
                .unwrap_or_default()
        } else {
            String::new()
        };

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
            age: pulse_age,
            idle: None,
            ask: None,
        }];

        // Worker agents. A live worker is ALWAYS shown. A pending ask is only
        // *answerable* while its worker is alive to consume the answer, so a
        // dead worker's lingering ask does NOT force the row on — it's a
        // stranded ask that needs a re-spawn, not an answer, and is subject to
        // the filter like any other dead worker (Active hides it, All keeps it).
        let mut wrows: Vec<AgentRow> = Vec::new();
        let mut hidden = 0usize;
        for s in workers {
            let ask = ask_by_worker.remove(&s.id);
            let keep = s.alive
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

    /// The bottom input's mode for the current selection, or `None` when the
    /// field is hidden (no agent, a dead worker, or the pulse with no ask —
    /// none of which can receive typed text).
    fn input_mode(&self) -> Option<InputMode> {
        let r = self.selected()?;
        if !r.alive {
            return None;
        }
        if r.ask.is_some() {
            Some(InputMode::Answer)
        } else if !r.is_pulse {
            Some(InputMode::Send)
        } else {
            None
        }
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
        // "no pending ask to answer") so the status bar doesn't sit stuck on it.
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
        let idx = hit.offset + (row - a.top()) as usize / hit.stride.max(1);
        (idx < self.rows.len()).then_some(idx)
    }

    /// Whether `(col, row)` falls in the list pane (its column-header row plus
    /// the data rows, across the pane's columns) — used to route the wheel to
    /// the list vs. the buffer. The column check matters in the side-by-side
    /// layout, where the list sits to the LEFT of the buffer; when stacked the
    /// list spans the full width so it's a no-op.
    fn in_list_region(&self, col: u16, row: u16) -> bool {
        self.asks_hit.is_some_and(|hit| {
            // The header sits one row above the data area.
            let top = hit.area.top().saturating_sub(1);
            let in_rows = row >= top && row < hit.area.bottom();
            // Include one extra column past the table for the scrollbar cell.
            let in_cols = col >= hit.area.left() && col <= hit.area.right();
            in_rows && in_cols
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

    /// Route a mouse event. The top list strip and the buffer (below) scroll
    /// independently — the wheel targets whichever the cursor is over; the
    /// buffer and list each have a draggable scrollbar; a click on a list row
    /// switches the selected agent (and thus the buffer).
    fn on_mouse(&mut self, m: MouseEvent) {
        match m.kind {
            // The wheel scrolls whichever pane the cursor is over: the
            // bottom list strip scrolls its view, the buffer above scrolls
            // into history (up) / toward the tail (down).
            MouseEventKind::ScrollUp => {
                if self.in_list_region(m.column, m.row) {
                    self.list_offset = self.list_offset.saturating_sub(WHEEL_STEP);
                } else {
                    self.log.scroll(WHEEL_STEP as isize);
                }
            }
            MouseEventKind::ScrollDown => {
                if self.in_list_region(m.column, m.row) {
                    self.list_offset = self.list_offset.saturating_add(WHEEL_STEP);
                } else {
                    self.log.scroll(-(WHEEL_STEP as isize));
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                // The divider grab wins first (it sits at the pane seam);
                // otherwise a scrollbar grab (list strip or buffer), else a
                // click on a still-visible list row switches the buffer to
                // that ask. Always (re)assign grab results so a plain click
                // clears any stuck drag state (e.g. a missed mouse-up).
                self.dragging_divider = self.divider_col == Some(m.column);
                self.dragging_list_sb =
                    !self.dragging_divider && self.list_scrollbar_grab(m.column, m.row);
                self.log.dragging_scrollbar = !self.dragging_divider
                    && !self.dragging_list_sb
                    && self.log.scrollbar_grab(m.column, m.row);
                if !self.dragging_divider
                    && !self.dragging_list_sb
                    && !self.log.dragging_scrollbar
                    && let Some(idx) = self.ask_at(m.column, m.row)
                {
                    self.select_index(idx);
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.dragging_divider => {
                // Map the pointer column back to a pane width; clamp lands in
                // `draw`, so just record the raw intent here.
                self.list_w = m.column.saturating_sub(self.list_origin_x).max(LIST_MIN_W);
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
                self.dragging_divider = false;
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
        let Some(mode) = self.input_mode() else {
            self.status = Some(match self.selected() {
                Some(r) => format!("{}: nothing to send", r.id),
                None => "no agent selected".into(),
            });
            return;
        };
        if self.input.trim().is_empty() {
            self.status = Some("type some text first".into());
            return;
        }
        match mode {
            // A pending ask: durably resolve it. On success the ask leaves the
            // pending list; the buffer stays on the same agent.
            InputMode::Answer => {
                let id = self
                    .selected()
                    .and_then(|r| r.ask.as_ref())
                    .map(|a| a.id.clone())
                    .expect("answer mode implies a pending ask");
                match mailbox::answer(paths, &id, &self.input, false) {
                    Ok(()) => {
                        self.status = Some(format!("answered {id}"));
                        self.input.clear();
                        self.refresh(paths);
                    }
                    Err(e) => self.status = Some(format!("{id}: {e}")),
                }
            }
            // A live worker with no ask: type the text into its terminal.
            InputMode::Send => {
                let worker = self
                    .selected()
                    .map(|r| r.id.clone())
                    .expect("send mode implies a selected agent");
                match session::send_to(paths, &worker, &self.input, true) {
                    Ok(()) => {
                        self.status = Some(format!("sent to {worker}"));
                        self.input.clear();
                    }
                    Err(e) => self.status = Some(format!("{worker}: {e}")),
                }
            }
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
            Constraint::Length(1), // status bar (bottom)
        ])
        .split(frame.area());

        // Flex layout, driven by the SELECTION (not a separate mode): with a
        // row selected — which is always the case — the agent list is a
        // scrollable strip of at most 5 rows pinned at the TOP and the agent's
        // buffer takes the rest (flex) BELOW it, separated by a gray rule. With
        // nothing selected (only before the first refresh) the list owns the
        // whole body.
        let body = chunks[0];
        if self.selected().is_some() {
            if body.width > WIDE_MIN {
                // WIDE: list on the LEFT, buffer on the RIGHT, seamed by a
                // vertical gray rule the human can DRAG to resize. The width is
                // clamped so neither pane starves, then persisted.
                let hi = body.width.saturating_sub(20).max(LIST_MIN_W);
                let list_w = self.list_w.clamp(LIST_MIN_W, hi);
                self.list_w = list_w;
                self.list_origin_x = body.x;
                let parts = Layout::horizontal([
                    Constraint::Length(list_w), // agent list (left, borderless)
                    Constraint::Length(1),      // gray separator rule (draggable)
                    Constraint::Min(20),        // worker buffer (right, flex)
                ])
                .split(body);
                self.divider_col = Some(parts[1].x);
                self.draw_list_wide(frame, parts[0]);
                frame.render_widget(
                    Block::default().borders(Borders::LEFT).border_style(dim()),
                    parts[1],
                );
                self.draw_detail(frame, parts[2]);
            } else {
                self.divider_col = None;
                // NARROW: the agent list is a borderless strip pinned at the TOP
                // (up to 5 data rows + 1 column-header row); a gray rule seams it
                // off and the worker buffer takes the rest below.
                let data_rows = self.rows.len().clamp(1, 5) as u16;
                let want = data_rows + 1;
                let list_h = want.min(body.height.saturating_sub(4)).max(2);
                let parts = Layout::vertical([
                    Constraint::Length(list_h), // agent list (top, borderless)
                    Constraint::Length(1),      // gray separator rule
                    Constraint::Min(3),         // worker buffer (below, flex)
                ])
                .split(body);
                self.draw_asks(frame, parts[0]);
                frame.render_widget(
                    Block::default().borders(Borders::TOP).border_style(dim()),
                    parts[1],
                );
                self.draw_detail(frame, parts[2]);
            }
        } else {
            self.divider_col = None;
            self.draw_asks(frame, body);
        }
        self.draw_status(frame, chunks[1]);
    }

    fn draw_asks(&mut self, frame: &mut Frame, area: Rect) {
        // The list pane is BORDERLESS and pinned at the TOP; a gray rule (drawn
        // by `draw`) seams it off from the buffer below. All geometry — table,
        // scrollbar, hit-testing — is relative to `area` directly.
        let dim = dim();
        frame.render_widget(Clear, area);
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
                let (state, state_style) = Self::state_cell(r);
                let row = Row::new(vec![
                    Cell::from(r.id.clone()),
                    Cell::from(r.age.clone()).style(dim),
                    Cell::from(state).style(state_style),
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
        // fixed 16 that clips longer ids. Clamp to `C_ID` as a floor and to
        // whatever width the fixed columns leave as a ceiling.
        let fixed = C_AGE + C_STATE; // non-id columns
        let spacing = 2; // 2 gaps between 3 columns at column_spacing(1)
        let id_ceiling = table_w.saturating_sub(fixed + spacing).max(C_ID);
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
        ];
        let table = Table::new(rows, widths)
            .header(Row::new(["ID", "AGE", "STATE"]).style(dim.add_modifier(Modifier::BOLD)))
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
            stride: 1,
        });
    }

    /// The side-by-side (WIDE) list: one CARD per agent stacked vertically —
    /// two lines (the id, then `state` + dim age) — with a green vertical
    /// accent bar down the left edge of each line of the SELECTED card. All
    /// geometry (scroll, hit-test, scrollbar) is in agent units via the
    /// `AsksHit` stride, so the shared mouse handlers work unchanged.
    fn draw_list_wide(&mut self, frame: &mut Frame, area: Rect) {
        const STRIDE: usize = 2; // 2 content lines, no spacer
        let dim = dim();
        frame.render_widget(Clear, area);
        let col_w = area.width;

        if self.rows.is_empty() {
            self.asks_hit = None;
            frame.render_widget(Paragraph::new(Span::styled(" no agents", dim)), area);
            return;
        }

        let visible = ((area.height as usize) / STRIDE).max(1);
        self.list_rows = visible;
        let len = self.rows.len();
        let max_off = len.saturating_sub(visible);
        self.list_offset = self.list_offset.min(max_off);
        let off = self.list_offset;
        let overflow = len > visible;
        let table_w = if overflow {
            col_w.saturating_sub(1)
        } else {
            col_w
        };

        let mut lines: Vec<Line<'static>> = Vec::new();
        for r in self.rows.iter().skip(off).take(visible) {
            let selected = Some(r.id.as_str()) == self.selected_id.as_deref();
            // Green accent bar down the card's left edge when selected — no
            // background fill; the bar alone marks the selection.
            let mark = || {
                if selected {
                    Span::styled("▎", Style::default().fg(Color::Green))
                } else {
                    Span::raw(" ")
                }
            };
            let id_style = if selected {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            let (state, state_style) = Self::state_cell(r);
            // Line 1: id. Line 2: `<age> <state>` (age dim, state coloured).
            lines.push(Line::from(vec![
                mark(),
                Span::raw(" "),
                Span::styled(r.id.clone(), id_style),
            ]));
            let mut l2 = vec![mark(), Span::raw(" "), Span::styled(state, state_style)];
            if !r.age.is_empty() {
                l2.push(Span::styled(format!(" {}", r.age), dim));
            }
            lines.push(Line::from(l2));
        }
        frame.render_widget(
            Paragraph::new(lines),
            Rect {
                width: table_w,
                ..area
            },
        );

        if overflow {
            render_vscrollbar(frame, area, max_off, off);
        }
        self.asks_hit = Some(AsksHit {
            area: Rect {
                width: table_w,
                ..area
            },
            offset: off,
            max_off: if overflow { max_off } else { 0 },
            stride: STRIDE,
        });
    }

    /// The STATE cell for an agent row: a LIVE agent with a pending ask reads
    /// `pending` (bold yellow) so the row awaiting a human answer stands out.
    /// A DEAD worker still holding an (un-answerable) ask reads `gone` (dim).
    /// Otherwise the pulse reads `live` (green) / `down` (red); a worker reads
    /// `running` (green) or its recorded exit state (`killed` red, others dim).
    fn state_cell(row: &AgentRow) -> (String, Style) {
        // A pending ask is what a human must act on — surface it as its own
        // attention state (bold yellow), overriding the underlying liveness.
        // But only while the agent is ALIVE to consume the answer: a live
        // worker (or the always-live pulse) with an ask reads `pending`.
        if row.ask.is_some() && (row.alive || row.is_pulse) {
            return (
                "pending".to_string(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            );
        }
        // A DEAD worker still holding an ask: the answer can never be
        // delivered (no live process reads it), so it is NOT `pending`. Read
        // it as `gone` (dim) — a stranded ask that needs a re-spawn.
        if row.ask.is_some() {
            return ("gone".to_string(), Style::default().fg(Color::DarkGray));
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

    /// The DETAIL pane — the BOTTOM flex region (the list sits above it) shown
    /// whenever a row is selected. It shows the selected agent's WORKER BUFFER:
    /// a scrollable `looop watch`-style vt100 replay of the worker's
    /// `output.log`. A pending ask renders in its OWN bordered box between the
    /// buffer and the input — the PROMPT only (worker/ref/options are noise
    /// here: the row already names the agent, and options belong in the prompt)
    /// — fixed in place so it neither scrolls with the transcript nor crowds it
    /// beyond half the pane. The input is pinned below and focused whenever the
    /// selected agent can receive typed text (a pending ask → `answer`, a live
    /// worker with no ask → `send`). For a read-only agent (the pulse with no
    /// ask, a dead worker) the buffer fills the whole pane with no input row.
    fn draw_detail(&mut self, frame: &mut Frame, area: Rect) {
        // Borderless — like `looop watch`'s log, the buffer owns its area edge
        // to edge; the gray rule above (between list and buffer) is the seam.
        let inner = area;
        frame.render_widget(Clear, area);

        // The input exists whenever the selected agent can receive typed text
        // (see `input_mode`) — an answerable ask or a steerable live worker. A
        // read-only agent (pulse w/o ask, dead worker) has nothing to type
        // into, so the buffer takes the whole pane. Split the inner area:
        // buffer on top; when typeable, the input takes a bordered box (one
        // text row + borders) pinned along the bottom.
        let mode = self.input_mode();
        let input_h = if mode.is_some() {
            3u16.min(inner.height)
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

        // The pending ask renders in its OWN bordered box above the input —
        // prompt only. Wrap it to the box's inner width up front so its height
        // is known; cap the box at half the pane so a long ask can't evict the
        // transcript. When the prompt overflows the cap, show its TAIL (the
        // question conventionally comes last) with a dim `…` marker on top.
        // Borrow the pending ask rather than cloning it — we only need its
        // prompt to lay out owned lines, so the borrow ends with this match.
        let ask_lines: Vec<Line<'static>> = match self.selected().and_then(|r| r.ask.as_ref()) {
            Some(a) => {
                // Render the prompt as Markdown, lifting the borrowed lines to
                // owned; wrap to the box interior (borders take 2 columns).
                let mut v: Vec<Line<'static>> = Vec::new();
                for line in tui_markdown::from_str(&a.prompt).lines {
                    v.push(logview::static_line(&line));
                }
                wrap_lines(v, inner.width.saturating_sub(2) as usize)
            }
            None => vec![],
        };
        // A bordered box needs at least 3 rows (2 borders + 1 content line).
        // Cap it at half the pane so a long ask can't evict the transcript,
        // but never let it exceed the space actually left below the input —
        // and skip it entirely when there's no room for a box at all, else
        // the ask_area would extend past `inner` and clip/overlap the input.
        let ask_h: u16 = if ask_lines.is_empty() {
            0
        } else {
            let avail = inner.height.saturating_sub(input_h);
            if avail < 3 {
                0
            } else {
                let cap = (avail / 2).max(3).min(avail);
                (ask_lines.len() as u16 + 2).min(cap)
            }
        };

        let content = Rect {
            height: content.height.saturating_sub(ask_h),
            ..content
        };
        let ask_area = Rect {
            y: inner.y + content.height,
            height: ask_h,
            ..inner
        };

        // The agent vanished from the fleet while its pane was open — say so
        // at the buffer's tail (the one message that still belongs there).
        let tail: Vec<Line<'static>> = match self.selected() {
            None => vec![Line::from(Span::styled(
                "this agent is no longer listed — esc to close.",
                dim(),
            ))],
            Some(_) => vec![],
        };

        self.log.render(frame, content, &tail, true);
        if ask_h > 0 {
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(dim())
                .title(Span::styled(" ask ", dim()));
            let field = block.inner(ask_area);
            frame.render_widget(block, ask_area);
            let visible = field.height as usize;
            let shown: Vec<Line<'static>> = if ask_lines.len() > visible {
                let mut v = ask_lines[ask_lines.len() - visible..].to_vec();
                if let Some(first) = v.first_mut() {
                    *first = Line::from(Span::styled("…", dim()));
                }
                v
            } else {
                ask_lines
            };
            frame.render_widget(Paragraph::new(shown), field);
        }
        if mode.is_some() {
            self.draw_input(frame, input_area);
        }
    }

    /// The always-focused editor pinned along the bottom of the detail pane: a
    /// single input line in a bordered box. Its WHITE border (vs. the dim
    /// ask box above) marks it as the focused element — where typing lands.
    fn draw_input(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::White));
        let field = block.inner(area);
        frame.render_widget(block, area);
        if field.height == 0 {
            return;
        }
        // Single non-wrapping line: text + block cursor (1 col). If the answer
        // overflows, show its TAIL (chars, not bytes) so the caret stays
        // visible — horizontal scroll rather than run-off.
        let avail = (field.width as usize).saturating_sub(1);
        let chars: Vec<char> = self.input.chars().collect();
        let shown: String = if chars.len() > avail {
            chars[chars.len() - avail..].iter().collect()
        } else {
            self.input.clone()
        };
        // text + block cursor (1 col), then pad the rest of the field with
        // spaces so shrinking input (backspace) can't leave stale glyphs behind
        // — Paragraph doesn't clear cells it doesn't write.
        let mut spans = Vec::new();
        let shown_w = shown.chars().count();
        spans.push(Span::raw(shown));
        spans.push(Span::styled(" ", Style::default().bg(Color::White)));
        let pad = (field.width as usize).saturating_sub(shown_w + 1);
        if pad > 0 {
            spans.push(Span::raw(" ".repeat(pad)));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), field);
    }

    /// The one-line status bar pinned along the BOTTOM of the screen: the
    /// filter badge, the last outcome (an error or an "answered X" note), and
    /// the key hints.
    fn draw_status(&self, frame: &mut Frame, area: Rect) {
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
            Some(msg) => format!("{msg} "),
            // The type/enter keys only apply when the selected agent can
            // receive text (answer an ask, or steer a live worker); a
            // read-only agent (pulse w/o ask, dead worker) just scrolls.
            None => match self.input_mode() {
                Some(InputMode::Answer) => format!(
                    "{fname}{hidden}  type answer · enter send · ↑/↓ switch · tab filter · pgup/pgdn scroll · ^c quit "
                ),
                Some(InputMode::Send) => format!(
                    "{fname}{hidden}  type message · enter send · ↑/↓ switch · tab filter · pgup/pgdn scroll · ^c quit "
                ),
                None => format!(
                    "{fname}{hidden}  ↑/↓ switch · tab filter · pgup/pgdn scroll · ^c quit "
                ),
            },
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(help, style)])).style(style),
            area,
        );
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
    fn wrap_counts_cjk_as_double_width() {
        // 6 double-width glyphs = 12 display columns; at width 6 that must wrap
        // into rows of at most 3 glyphs (6 columns), not 6 glyphs (12 columns).
        let lines = vec![Line::from("あいうえおか")];
        let out = wrap_lines(lines, 6);
        assert!(out.iter().all(|l| {
            plain(l)
                .chars()
                .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
                .sum::<usize>()
                <= 6
        }));
        let joined: String = out.iter().map(plain).collect();
        assert_eq!(joined, "あいうえおか");
    }

    #[test]
    fn wrap_zero_width_is_identity() {
        let lines = vec![Line::from("anything")];
        let out = wrap_lines(lines.clone(), 0);
        assert_eq!(out.len(), 1);
        assert_eq!(plain(&out[0]), "anything");
    }

    fn row(is_pulse: bool, alive: bool, state: &str, ask: bool) -> AgentRow {
        AgentRow {
            id: "w".to_string(),
            is_pulse,
            alive,
            state: state.to_string(),
            age: String::new(),
            idle: None,
            ask: ask.then(|| Ask {
                id: "w-1".to_string(),
                worker: "w".to_string(),
                prompt: "q".to_string(),
                reference: String::new(),
                options: vec![],
                ts: 0,
            }),
        }
    }

    #[test]
    fn state_cell_live_worker_with_ask_is_pending() {
        let (label, style) = App::state_cell(&row(false, true, "running", true));
        assert_eq!(label, "pending");
        assert_eq!(style.fg, Some(Color::Yellow));
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn state_cell_pulse_with_ask_is_pending() {
        // The pulse is always live, so its ask is answerable → pending.
        let (label, _) = App::state_cell(&row(true, true, "live", true));
        assert_eq!(label, "pending");
    }

    #[test]
    fn state_cell_dead_worker_with_ask_is_gone_not_pending() {
        // The reboot bug: a dead worker still holding an ask must NOT read
        // `pending` (its answer can never be delivered) — it reads `gone`.
        let (label, style) = App::state_cell(&row(false, false, "exited", true));
        assert_eq!(label, "gone");
        assert_eq!(style.fg, Some(Color::DarkGray));
        assert!(!style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn state_cell_dead_worker_no_ask_shows_recorded_state() {
        let (label, style) = App::state_cell(&row(false, false, "killed", false));
        assert_eq!(label, "killed");
        assert_eq!(style.fg, Some(Color::Red));
    }

    #[test]
    fn state_cell_live_worker_no_ask_is_running() {
        let (label, style) = App::state_cell(&row(false, true, "running", false));
        assert_eq!(label, "running");
        assert_eq!(style.fg, Some(Color::Green));
    }
}
