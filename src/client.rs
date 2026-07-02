//! `looop client` — THE human TUI over a running loop: live session logs,
//! pending asks, and an always-available input line, in one window.
//!
//! The control loop is invisible by design (the pulse + workers run detached),
//! so `client` is the human window into it:
//!
//!   ┌─ log ──────────────────────────────┬ ⚑ 1 ask — a ┐
//!   │ live, COLORED tail of the selected session's      │
//!   │ output.log (ANSI/SGR preserved via ansi-to-tui)   │
//!   ├┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┤
//!   │ answer triage-2 › ship it█                        │
//!   └─ footer ──────────────────────────────────────────┘
//!
//! Three surfaces:
//!
//!   * The LOG fills the screen (the vt100 replay inherited from the old
//!     `looop watch`): scroll reaches the session's first line, ENTER floats
//!     the session picker, `a` cycles its filter.
//!   * A floating ⚑ BADGE (top-right) appears the moment any ask is pending,
//!     so a blocked worker is impossible to miss; `a` jumps to it.
//!   * ONE input line is pinned above the footer and can ALWAYS send
//!     something to the selected session: when that session has a pending ask
//!     the line is an ANSWER (submits `mailbox::answer`, durable); otherwise
//!     it TYPES into the worker's PTY (`session::send_text`, plus Enter) —
//!     the same two channels a human drives via `_ answer` / `_ send`.
//!
//! Focus is modal but shallow: the log owns the keys by default (vim-ish
//! scrolling); `i`/Tab (or a click on the input) focuses the input, ESC/Tab
//! returns. While the input is focused and an ask is pending, the ask's
//! prompt/ref/options float above the input so you answer with the question —
//! and the worker's log — in view.
//!
//! The PTY-backed `output.log` is a RAW transcript — an interactive agent
//! redraws in place (cursor moves, line/screen clears, carriage returns) — so
//! we replay the WHOLE log through a `vt100` virtual terminal and render the
//! resulting SCREEN plus its scrollback, instead of dumping every redraw frame
//! as new lines.
//!
//! Mouse capture stays on (wheel scrolls, the scrollbar scrubs); hold Shift
//! while dragging to use the terminal's own text selection / copy.

use crate::mailbox::{self, Ask};
use crate::paths::Paths;
use crate::session::{self, Session};
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc::{Receiver, Sender};
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
    Block, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Wrap,
};

/// How often we re-list sessions/asks and re-read the tailed log.
const TICK: Duration = Duration::from_millis(250);
/// Cap on the INITIAL replay read. We feed the WHOLE `output.log` so scrollback
/// reaches the session's first line (the worker streams its transcript with
/// newlines — only the status block repaints in place — so the full history is
/// recoverable). This cap only bounds latency on pathological logs: at/below it
/// we read from byte 0 (first line reachable); above it we fall back to the last
/// `MAX_REPLAY_BYTES` (live tail preserved, oldest lines dropped). 16 MiB covers
/// every observed session and parses in well under ~1.5s; from there we only
/// ever feed the freshly-appended tail.
const MAX_REPLAY_BYTES: u64 = 16 * 1024 * 1024;

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
/// PageUp/wheel/Home. vt100 grows scrollback lazily (only rows that actually
/// scrolled off cost memory), so this is just an upper bound; a long worker
/// session can scroll well past 10k rows (a ~10 MiB log produced ~14k), so keep
/// generous headroom to avoid silently dropping the oldest lines.
const SCROLLBACK_ROWS: usize = 100_000;

/// Recency window used by [`Filter::Recent`] when `--since` isn't given. Alive
/// sessions and the pulse are always shown regardless of filter.
const DEFAULT_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

/// The shared dark-surface background: the footer bar and the picker's
/// selected-row highlight. Dark enough that per-span colors stay legible.
const SURFACE: Color = Color::Rgb(40, 40, 40);

/// The dim gray style shared by all secondary text in this TUI.
fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

/// Which sessions the picker shows. Cycled live in the TUI with `a`
/// (Active → Recent → All → Active). Sessions with a pending ask are ALWAYS
/// shown regardless of filter — an unanswered question must not be hideable.
#[derive(Clone, Copy)]
enum Filter {
    /// Only live sessions (plus the pulse). The default — dead corpses hidden.
    Active,
    /// Live + pulse + dead sessions idle less than this window.
    Recent(Duration),
    /// Every session, no matter how stale.
    All,
}

/// Where key presses land. Shallow and explicit: the log owns the keys by
/// default; `i`/Tab (or a click) moves them to the input line, ESC/Tab back.
/// (The floating session picker is tracked separately as `picking` — it is a
/// transient overlay on top of whichever focus is current.)
#[derive(Clone, Copy, PartialEq)]
enum Focus {
    /// Scroll keys drive the log.
    Log,
    /// Printable keys type into the answer/send input.
    Input,
}

/// `looop client [<id>] [--since <dur>] [--all]` — open the TUI.
///
/// An optional id preselects a session (e.g. `looop client pulse`); otherwise
/// the most-recently-active one. By default only live sessions (plus the pulse
/// and any session with a pending ask) are shown. `--since <dur>` widens to
/// also include dead sessions idle less than the window (e.g. `1d`, `12h`,
/// `30m`, `90s`, or bare seconds); `--all` shows every session.
pub fn cmd_client(paths: &Paths, args: &crate::cli::ClientArgs) -> Result<ExitCode> {
    let initial: Option<String> = args.id.clone();
    let filter = if let Some(dur) = &args.since {
        Filter::Recent(parse_duration(dur)?)
    } else if args.all {
        Filter::All
    } else {
        Filter::Active
    };

    let mut terminal = ratatui::init();
    // Capture the mouse so wheel events reach us as `Event::Mouse` instead of
    // letting the terminal scroll its alternate screen out from under us (which
    // corrupts the rendered panes).
    let _ = execute!(std::io::stdout(), EnableMouseCapture);
    let res = App::new(paths, initial, filter).run(&mut terminal, paths);
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
        .map_err(|_| anyhow::anyhow!("looop client: bad duration '{s}' (try 1d, 12h, 30m, 90s)"))?;
    Ok(Duration::from_secs(n * mult))
}

struct App {
    sessions: Vec<Session>,
    list_state: ListState,
    /// All pending asks, oldest first (`mailbox::pending`), refreshed per tick.
    asks: Vec<Ask>,
    /// Lines scrolled back from the bottom (0 = follow the tail live).
    scroll_back: usize,
    /// Which sessions the picker shows. Cycled live with `a` (in the picker).
    filter: Filter,
    /// The window used by [`Filter::Recent`] (from `--since` or
    /// [`DEFAULT_WINDOW`]). Preserved across filter cycling.
    recent_window: Duration,
    /// Sessions hidden by the current filter on the last refresh (footer hint).
    hidden: usize,
    /// `true` while the floating session picker is open (ENTER). The log is the
    /// main buffer; the list is hidden until summoned, and ENTER/ESC closes it.
    picking: bool,
    /// Where key presses land: the log (scrolling) or the input line.
    focus: Focus,
    /// The answer/send text being typed in the pinned input line.
    input: String,
    /// Last outcome to show in the footer (an error, or an "answered X" note).
    status: Option<String>,
    /// Top-anchored scroll offset (wrapped lines) of the floating ask pane.
    ask_scroll: usize,
    /// Inner height of the ask pane from the last draw (page-scroll size).
    ask_rows: usize,
    /// Geometry of the input line from the last draw, for click-to-focus.
    input_hit: Option<Rect>,
    /// Height (rows) of the log pane on the last draw, so `Ctrl-D`/`Ctrl-U`
    /// (half page) and `Ctrl-F`/`Ctrl-B` (full page) scroll by the viewport.
    log_rows: usize,
    /// Geometry of the log scrollbar from the last draw, so mouse clicks/drags
    /// on it can be mapped back to a `scroll_back` position. `None` when the
    /// pane isn't scrollable (no scrollbar rendered).
    scrollbar: Option<ScrollbarHit>,
    /// Persistent vt100 replay of the selected session's log (fed incrementally
    /// across frames). `None` until a session with a log file is selected.
    log: Option<LogReplay>,
    /// Cached output of the (expensive) vt100→ANSI→ratatui render in `draw_log`,
    /// reused on frames where none of its inputs changed.
    log_cache: Option<LogCache>,
    /// Geometry of the session list from the last draw, so a mouse click can be
    /// mapped back to a row → session index. `None` when the list is empty.
    selector: Option<SelectorHit>,
    /// Background vt100-replay worker. Replaying a multi-MB log can take ~1s in
    /// debug builds, so the initial (re)parse on a session switch runs off the
    /// UI thread; the live tail is then fed incrementally on the UI thread
    /// (cheap). `loading` names the session currently being parsed (drawn as a
    /// hint), and `parse_gen` discards results from superseded requests.
    parse_tx: Sender<ParseRequest>,
    parse_rx: Receiver<ParseResult>,
    parse_gen: u64,
    loading: Option<String>,
    /// `true` while the left mouse button is held after grabbing the scrollbar,
    /// so drags keep scrubbing even when the cursor drifts off the thin column.
    dragging_scrollbar: bool,
}

/// A request to the replay worker: parse `path` for session `id`. `gen` lets
/// the UI ignore a result whose session is no longer selected.
struct ParseRequest {
    generation: u64,
    id: String,
    path: PathBuf,
}

/// A completed replay handed back from the worker, ready to install as `log`.
struct ParseResult {
    generation: u64,
    replay: LogReplay,
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

/// Cached result of the expensive vt100→ANSI→ratatui render done by `draw_log`.
/// That path renders the whole screen to ANSI and re-parses it with
/// `ansi-to-tui` for each tile — far too costly to repeat on every frame. We
/// keep the last result and only rebuild it when an input that affects the
/// rendered lines changes: the selected session, the log content (`seen`), the
/// scroll position, or the pane size. Idle frames and unrelated input events
/// (e.g. typing into the answer input) reuse the cached lines.
struct LogCache {
    id: String,
    /// Log content version — total bytes fed to the parser.
    seen: u64,
    /// Clamped scroll position the cached frame was rendered at.
    scroll_back: usize,
    pane_w: u16,
    pane_h: u16,
    lines: Vec<Line<'static>>,
    max_scroll: usize,
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
    fn new(paths: &Paths, initial: Option<String>, filter: Filter) -> Self {
        let recent_window = match filter {
            Filter::Recent(w) => w,
            _ => DEFAULT_WINDOW,
        };
        let asks = mailbox::pending(paths);
        let ask_workers: HashSet<String> = asks.iter().map(|a| a.worker.clone()).collect();
        let (sessions, hidden) = list_filtered(paths, filter, &ask_workers);
        let mut list_state = ListState::default();
        let idx = initial
            .as_deref()
            .and_then(|id| sessions.iter().position(|s| s.id == id))
            .unwrap_or(0);
        if !sessions.is_empty() {
            list_state.select(Some(idx));
        }
        let (parse_tx, parse_rx) = spawn_replay_worker();
        App {
            sessions,
            list_state,
            asks,
            scroll_back: 0,
            filter,
            recent_window,
            hidden,
            scrollbar: None,
            log: None,
            log_cache: None,
            selector: None,
            picking: false,
            focus: Focus::Log,
            input: String::new(),
            status: None,
            ask_scroll: 0,
            ask_rows: 0,
            input_hit: None,
            log_rows: 0,
            parse_tx,
            parse_rx,
            parse_gen: 0,
            loading: None,
            dragging_scrollbar: false,
        }
    }

    /// Begin a scrollbar drag. Returns `true` if `(col, row)` landed on the
    /// bar's column (within the track), scrubbing the viewport to that row. The
    /// caller then holds the grab and feeds later moves to `scrollbar_scrub`
    /// (row only) until mouse-up, so the cursor can wander off the thin column
    /// without the grab snapping loose.
    fn scrollbar_grab(&mut self, col: u16, row: u16) -> bool {
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
        self.scrollbar_scrub(row);
        true
    }

    /// Scrub the viewport to `row` on the scrollbar track, ignoring the column
    /// (used while a grab is held). The row is clamped into the track, so
    /// dragging past the top/bottom pins to the oldest/newest line. The track
    /// maps linearly top→bottom: top (↑) = oldest scrollback, bottom (↓) = tail.
    fn scrollbar_scrub(&mut self, row: u16) {
        let Some(hit) = self.scrollbar else {
            return;
        };
        let a = hit.area;
        let span = a.height.saturating_sub(1);
        let clamped = row.clamp(a.top(), a.bottom().saturating_sub(1));
        let pos = if span == 0 {
            0
        } else {
            let frac = (clamped - a.top()) as f64 / span as f64;
            (frac * hit.max_scroll as f64).round() as usize
        };
        // pos counts from the top (oldest); scroll_back counts from the tail.
        self.scroll_back = hit.max_scroll.saturating_sub(pos);
    }

    /// Select the session under a mouse click on the picker. Returns `false`
    /// if the click wasn't inside the list (so the caller can ignore it).
    /// Switching session re-follows the tail, mirroring `move_selection`.
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
        self.select_index(idx);
        true
    }

    fn selected_id(&self) -> Option<&str> {
        self.list_state
            .selected()
            .and_then(|i| self.sessions.get(i))
            .map(|s| s.id.as_str())
    }

    /// The OLDEST pending ask raised by the selected session, if any. This is
    /// what the input line answers; the next one surfaces after it's resolved.
    fn selected_ask(&self) -> Option<&Ask> {
        let id = self.selected_id()?;
        self.asks.iter().find(|a| a.worker == id)
    }

    /// `true` while a floating overlay (picker or ask pane) covers part of the
    /// log — the log scrollbar is hidden then (it would poke through as noise).
    fn overlay_open(&self) -> bool {
        self.picking || (self.focus == Focus::Input && self.selected_ask().is_some())
    }

    /// Can the input line send anything for the current selection? A pending
    /// ask (⇒ answer) OR a live worker PTY (⇒ send) makes it usable; the pulse
    /// with no ask is READ-ONLY (nothing to send), so focusing it is pointless.
    fn input_enabled(&self) -> bool {
        match self.selected_id() {
            None => false,
            Some(id) => id != session::PULSE_SESSION || self.selected_ask().is_some(),
        }
    }

    /// `a` (log focus): jump to a pending ask. If the selected session already
    /// has one, just focus the input; otherwise select the oldest ask's worker
    /// (so the log context matches the question) and focus the input there.
    fn jump_to_ask(&mut self) {
        if self.asks.is_empty() {
            self.status = Some("no pending asks".into());
            return;
        }
        if self.selected_ask().is_none() {
            let target = self
                .asks
                .iter()
                .find_map(|a| self.sessions.iter().position(|s| s.id == a.worker));
            match target {
                Some(idx) => self.select_index(idx),
                None => {
                    // Ask(s) whose worker session no longer exists at all: not
                    // reachable through a session-anchored UI — point at the
                    // plumbing instead of silently doing nothing.
                    self.status = Some(format!(
                        "{} ask(s) from vanished sessions — `looop _ asks` / `_ answer`",
                        self.asks.len()
                    ));
                    return;
                }
            }
        }
        self.status = None;
        self.ask_scroll = 0;
        self.focus = Focus::Input;
    }

    /// Point the selection at row `idx`. When it actually CHANGES the target
    /// session, re-follow the tail, reset the ask scroll and clear any
    /// half-typed input — so a pending answer can never be submitted against a
    /// different session than the one it was typed for.
    fn select_index(&mut self, idx: usize) {
        if Some(idx) != self.list_state.selected() {
            self.list_state.select(Some(idx));
            self.scroll_back = 0;
            self.ask_scroll = 0;
            self.input.clear();
        }
    }

    /// Re-list sessions + pending asks, preserving the current selection by id
    /// (the list is re-sorted most-recently-active first, so the index drifts).
    fn refresh(&mut self, paths: &Paths) {
        self.asks = mailbox::pending(paths);
        let ask_workers: HashSet<String> = self.asks.iter().map(|a| a.worker.clone()).collect();
        let keep = self.selected_id().map(str::to_string);
        let (sessions, hidden) = list_filtered(paths, self.filter, &ask_workers);
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
        self.select_index(next);
    }

    /// Bring the persistent log replay in sync with the selected session's
    /// `output.log`: (re)build the parser on a selection change or a truncated
    /// file, then feed any newly-appended bytes. Keeps a paused viewport
    /// anchored by nudging `scroll_back` when the tail grows.
    fn sync_log(&mut self, paths: &Paths) {
        // Install any finished background replay that still matches the current
        // selection (stale ones — from sessions navigated past — are dropped).
        while let Ok(res) = self.parse_rx.try_recv() {
            if res.generation == self.parse_gen {
                self.log = Some(res.replay);
                self.loading = None;
                self.log_cache = None; // fresh buffer → drop the render cache
            }
        }

        let Some(id) = self.selected_id().map(str::to_string) else {
            self.log = None;
            self.loading = None;
            return;
        };
        let path = paths.sessions().output_log_path(&id);
        let Ok(meta) = std::fs::metadata(&path) else {
            self.log = None; // no log file yet
            self.loading = None;
            return;
        };
        let len = meta.len();

        let reset = match &self.log {
            // New session, or the file was truncated/rotated under us.
            Some(l) => l.id != id || len < l.offset,
            None => true,
        };

        if reset {
            // The full replay can take ~1s in debug builds, so hand it to the
            // background worker instead of freezing the UI. Only fire a fresh
            // request when we're not already parsing this exact session; until
            // the parser lands, `draw_log` shows a "loading…" hint.
            if self.loading.as_deref() != Some(id.as_str()) {
                self.parse_gen += 1;
                self.loading = Some(id.clone());
                self.log = None;
                let _ = self.parse_tx.send(ParseRequest {
                    generation: self.parse_gen,
                    id,
                    path,
                });
            }
            return;
        }

        // Feed only what was appended since the last frame (cheap, UI thread).
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

    /// Enter (input focus): submit the input line through whichever channel the
    /// selected session currently exposes — a pending ask ⇒ `mailbox::answer`
    /// (durable); otherwise ⇒ type into the worker's PTY. On failure the typed
    /// text stays intact and the reason lands in the footer.
    fn submit(&mut self, paths: &Paths) {
        let Some(sid) = self.selected_id().map(str::to_string) else {
            self.status = Some("no session selected".into());
            return;
        };
        if self.input.trim().is_empty() {
            self.status = Some("empty input (esc to leave)".into());
            return;
        }
        if let Some(ask_id) = self.selected_ask().map(|a| a.id.clone()) {
            match mailbox::answer(paths, &ask_id, &self.input, false) {
                Ok(()) => {
                    self.status = Some(format!("answered {ask_id}"));
                    self.input.clear();
                    self.ask_scroll = 0;
                    self.refresh(paths);
                }
                Err(e) => self.status = Some(format!("{ask_id}: {e}")),
            }
            return;
        }
        match session::send_text(paths, &sid, &self.input) {
            Ok(()) => {
                self.status = Some(format!("sent to {sid}"));
                self.input.clear();
            }
            Err(e) => self.status = Some(format!("send {sid}: {e}")),
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
                        } else if self.focus == Focus::Input {
                            // The input line is focused: printable keys TYPE.
                            // ↑/↓ still scroll the log (context stays readable
                            // while composing); page keys scroll the floating
                            // ask pane when it's open, else the log.
                            let ask_open = self.selected_ask().is_some();
                            let page = self.ask_rows.max(1);
                            match key.code {
                                KeyCode::Esc | KeyCode::Tab => self.focus = Focus::Log,
                                KeyCode::Enter => self.submit(paths),
                                KeyCode::Backspace => {
                                    self.input.pop();
                                }
                                KeyCode::Down => {
                                    self.scroll_back = self.scroll_back.saturating_sub(1)
                                }
                                KeyCode::Up => {
                                    self.scroll_back = self.scroll_back.saturating_add(1)
                                }
                                KeyCode::PageDown if ask_open => {
                                    self.ask_scroll = self.ask_scroll.saturating_add(page)
                                }
                                KeyCode::PageUp if ask_open => {
                                    self.ask_scroll = self.ask_scroll.saturating_sub(page)
                                }
                                KeyCode::PageDown => {
                                    self.scroll_back = self.scroll_back.saturating_sub(page)
                                }
                                KeyCode::PageUp => {
                                    self.scroll_back = self.scroll_back.saturating_add(page)
                                }
                                KeyCode::Char('d') if ctrl && ask_open => {
                                    self.ask_scroll = self.ask_scroll.saturating_add(page / 2)
                                }
                                KeyCode::Char('u') if ctrl && ask_open => {
                                    self.ask_scroll = self.ask_scroll.saturating_sub(page / 2)
                                }
                                // Everything else printable is input text.
                                KeyCode::Char(c) if !ctrl => self.input.push(c),
                                _ => {}
                            }
                        } else {
                            // Main buffer (log): scroll, ENTER opens the picker,
                            // `i`/Tab focuses the input, `a` jumps to an ask.
                            // `scroll_back` counts rows from the live tail, so
                            // scrolling UP (into history) ADDS and DOWN subtracts.
                            let half = (self.log_rows / 2).max(1);
                            let page = self.log_rows.max(1);
                            match key.code {
                                KeyCode::Char('q') => break,
                                KeyCode::Enter => self.picking = true,
                                KeyCode::Char('i') | KeyCode::Tab if self.input_enabled() => {
                                    self.status = None;
                                    self.ask_scroll = 0;
                                    self.focus = Focus::Input;
                                }
                                KeyCode::Char('i') | KeyCode::Tab => {
                                    self.status = Some("read-only — nothing to send here".into());
                                }
                                KeyCode::Char('a') => self.jump_to_ask(),
                                KeyCode::Down | KeyCode::Char('j') => {
                                    self.scroll_back = self.scroll_back.saturating_sub(1)
                                }
                                KeyCode::Up | KeyCode::Char('k') => {
                                    self.scroll_back = self.scroll_back.saturating_add(1)
                                }
                                // Half page: Ctrl-D down, Ctrl-U up (vim/less).
                                KeyCode::Char('d') if ctrl => {
                                    self.scroll_back = self.scroll_back.saturating_sub(half)
                                }
                                KeyCode::Char('u') if ctrl => {
                                    self.scroll_back = self.scroll_back.saturating_add(half)
                                }
                                // Full page: Ctrl-F / PageDown down, Ctrl-B / PageUp up.
                                KeyCode::Char('f') if ctrl => {
                                    self.scroll_back = self.scroll_back.saturating_sub(page)
                                }
                                KeyCode::Char('b') if ctrl => {
                                    self.scroll_back = self.scroll_back.saturating_add(page)
                                }
                                KeyCode::PageDown => {
                                    self.scroll_back = self.scroll_back.saturating_sub(page)
                                }
                                KeyCode::PageUp => {
                                    self.scroll_back = self.scroll_back.saturating_add(page)
                                }
                                // Jump to ends: g/Home oldest, G/End live tail.
                                KeyCode::Char('g') | KeyCode::Home => self.scroll_back = usize::MAX,
                                KeyCode::Char('G') | KeyCode::End => self.scroll_back = 0,
                                _ => {}
                            }
                        }
                    }
                    // The floating picker, when open, is modal: it captures the
                    // wheel (to move the selection) and clicks (to pick a row).
                    // Otherwise the mouse drives the log: wheel scrolls (the
                    // ask pane instead, while it's open), the scrollbar can be
                    // clicked/dragged, and a click on the input line focuses it.
                    Event::Mouse(m) if self.picking => match m.kind {
                        MouseEventKind::ScrollUp => self.move_selection(-1),
                        MouseEventKind::ScrollDown => self.move_selection(1),
                        MouseEventKind::Down(MouseButton::Left) => {
                            let _ = self.select_at(m.column, m.row);
                        }
                        _ => {}
                    },
                    Event::Mouse(m) => match m.kind {
                        MouseEventKind::ScrollUp => {
                            if self.focus == Focus::Input && self.selected_ask().is_some() {
                                self.ask_scroll = self.ask_scroll.saturating_sub(3);
                            } else {
                                self.scroll_back = self.scroll_back.saturating_add(3);
                            }
                        }
                        MouseEventKind::ScrollDown => {
                            if self.focus == Focus::Input && self.selected_ask().is_some() {
                                self.ask_scroll = self.ask_scroll.saturating_add(3);
                            } else {
                                self.scroll_back = self.scroll_back.saturating_sub(3);
                            }
                        }
                        // Click: the input line focuses the input; the scrollbar
                        // starts a grab (kept scrubbing on every drag until
                        // release); anywhere else hands focus back to the log.
                        MouseEventKind::Down(MouseButton::Left) => {
                            let on_input = self
                                .input_hit
                                .is_some_and(|a| m.row >= a.top() && m.row < a.bottom());
                            if on_input && self.input_enabled() {
                                self.status = None;
                                self.focus = Focus::Input;
                            } else {
                                self.dragging_scrollbar = self.scrollbar_grab(m.column, m.row);
                                if !self.dragging_scrollbar {
                                    self.focus = Focus::Log;
                                }
                            }
                        }
                        MouseEventKind::Drag(MouseButton::Left) if self.dragging_scrollbar => {
                            self.scrollbar_scrub(m.row);
                        }
                        MouseEventKind::Up(MouseButton::Left) => {
                            self.dragging_scrollbar = false;
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
        // The log is the main buffer and owns the whole screen, save the pinned
        // input BOX (a 3-row bordered field) and a one-row footer.
        let chunks = Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(frame.area());
        let log_area = chunks[0];
        self.draw_log(frame, log_area);
        self.draw_badge(frame, log_area);

        if self.focus == Focus::Input && !self.picking {
            self.draw_ask_float(frame, log_area);
        }
        if self.picking {
            // The picker owns the input then; leave the pinned line blank so its
            // placeholder isn't stray noise under the float.
            frame.render_widget(Clear, chunks[1]);
            self.input_hit = None;
        } else {
            self.draw_input(frame, chunks[1]);
        }
        self.draw_footer(frame, chunks[2]);

        if self.picking {
            // Floating session picker, overlaid on the TOP of the log. Capped
            // so it never swallows the whole pane; `Clear` wipes the log rows
            // underneath so the list reads cleanly on top.
            let rows = self.sessions.len().clamp(1, 8) as u16;
            let h = (rows + 2).min(log_area.height);
            let float = Rect {
                x: log_area.x,
                y: log_area.y,
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

    /// The floating ⚑ badge (top-right of the log): pending asks are impossible
    /// to miss the moment they arrive, without a permanent pane. Hidden while
    /// the picker is open (its rows carry per-session badges then).
    fn draw_badge(&mut self, frame: &mut Frame, area: Rect) {
        if self.asks.is_empty() || self.picking {
            return;
        }
        let n = self.asks.len();
        let text = if self.selected_ask().is_some() {
            " ⚑ ask pending — a to answer ".to_string()
        } else if n == 1 {
            " ⚑ 1 ask — a ".to_string()
        } else {
            format!(" ⚑ {n} asks — a ")
        };
        let w = (text.chars().count() as u16).min(area.width);
        let float = Rect {
            x: area.right().saturating_sub(w),
            y: area.y,
            width: w,
            height: 1.min(area.height),
        };
        frame.render_widget(Clear, float);
        frame.render_widget(
            Paragraph::new(Span::styled(
                text,
                Style::default().fg(Color::Yellow).bg(SURFACE),
            )),
            float,
        );
    }

    /// The floating ask pane: while the input is focused and the selected
    /// session has a pending ask, its prompt (rendered as Markdown), reference
    /// and options float along the TOP of the log — the question stays put
    /// while the worker's log and the input line below remain in view. Scrolls
    /// via PgUp/PgDn / the wheel.
    fn draw_ask_float(&mut self, frame: &mut Frame, area: Rect) {
        let Some(ask) = self.selected_ask().cloned() else {
            return;
        };
        let h = (area.height * 2 / 5).clamp(5.min(area.height), area.height);
        let float = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: h,
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow))
            .title(Span::styled(
                format!(" ⚑ {} ", ask.id),
                Style::default().fg(Color::Yellow),
            ));
        let inner = block.inner(float);
        frame.render_widget(Clear, float);
        frame.render_widget(block, float);
        if inner.height == 0 || inner.width == 0 {
            return;
        }

        // Render the ask prompt as Markdown: headings/bold/lists/code become
        // styled lines instead of a single raw block.
        let mut lines: Vec<Line> = tui_markdown::from_str(&ask.prompt).lines;
        if !ask.reference.is_empty() {
            lines.push(Line::raw(""));
            lines.push(Line::from(vec![
                Span::styled("ref: ", dim()),
                Span::raw(ask.reference.as_str()),
            ]));
        }
        if !ask.options.is_empty() {
            lines.push(Line::raw(""));
            lines.push(Line::from(vec![
                Span::styled("options: ", dim()),
                Span::raw(ask.options.join(", ")),
            ]));
        }

        // Measure the wrapped content to clamp the scroll offset and size the
        // scrollbar. `line_count` wraps exactly as the rendered `Paragraph`
        // will (same width + Wrap).
        let visible = inner.height as usize;
        let total = Paragraph::new(lines.clone())
            .wrap(Wrap { trim: false })
            .line_count(inner.width);
        let max_scroll = total.saturating_sub(visible);
        self.ask_scroll = self.ask_scroll.min(max_scroll);
        self.ask_rows = visible;

        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((self.ask_scroll.min(u16::MAX as usize) as u16, 0)),
            inner,
        );
        render_vscrollbar(frame, inner, max_scroll, self.ask_scroll);
    }

    /// The pinned input BOX above the footer. The bordered box's TITLE
    /// announces the channel — `⚑ answer <ask-id>` (yellow) when the selected
    /// session has a pending ask, else `send <id>`. The border is the focus
    /// affordance: dim while idle (placeholder: `press i to type`), WHITE+bold
    /// once activated. A read-only selection (the pulse, no ask) draws nothing.
    fn draw_input(&mut self, frame: &mut Frame, area: Rect) {
        // Read-only selection (the pulse, no ask): nothing to send, so the box
        // is pure noise — leave it blank rather than frame an inert field.
        if !self.input_enabled() {
            frame.render_widget(Clear, area);
            self.input_hit = None;
            return;
        }
        self.input_hit = Some(area);

        let focused = self.focus == Focus::Input;
        // The border is the affordance: a mid gray while idle (bright enough to
        // stand off the log), WHITE (bold) once the input is activated — so
        // "where do I type" is answered at a glance.
        let border_style = if focused {
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        let mut block = Block::default()
            .borders(Borders::ALL)
            .border_style(border_style);
        // Only the ANSWER channel needs a title (which ask is being answered);
        // for SEND the target session id is already in the footer, so a title
        // would just be redundant chrome on the border.
        if let Some(a) = self.selected_ask() {
            block = block.title(Span::styled(
                format!(" ⚑ answer {} ", a.id),
                Style::default().fg(Color::Yellow),
            ));
        }
        let field = block.inner(area);
        frame.render_widget(Clear, area);
        frame.render_widget(block, area);
        if field.height == 0 || field.width == 0 {
            return;
        }

        // Content row: the typed text (tail-scrolled so the caret stays in view)
        // plus a block cursor when focused, or a guiding placeholder when idle.
        let prompt = "› ";
        let avail = (field.width as usize)
            .saturating_sub(prompt.chars().count())
            .saturating_sub(1);
        let chars: Vec<char> = self.input.chars().collect();
        let shown: String = if chars.len() > avail {
            chars[chars.len().saturating_sub(avail)..].iter().collect()
        } else {
            self.input.clone()
        };

        let mut spans = vec![Span::styled(prompt, dim())];
        if focused {
            if self.input.is_empty() {
                spans.push(Span::styled(" ", Style::default().bg(Color::White)));
                spans.push(Span::styled(" type · enter to send · esc to leave", dim()));
            } else {
                spans.push(Span::raw(shown));
                spans.push(Span::styled(" ", Style::default().bg(Color::White)));
            }
        } else if self.input.is_empty() {
            spans.push(Span::styled("press i to type", dim()));
        } else {
            // A half-typed draft, waiting to be resumed.
            spans.push(Span::styled(shown, dim()));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), field);
    }

    /// The dim help/legend line along the very bottom of the screen. Adapts to
    /// the focus, and surfaces the last submit outcome when there is one.
    fn draw_footer(&mut self, frame: &mut Frame, area: Rect) {
        let style = Style::default().bg(SURFACE).fg(Color::White);
        let help = if let Some(msg) = &self.status {
            format!(" {msg} ")
        } else if self.picking {
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
        } else if self.focus == Focus::Input {
            " type · enter send · ↑/↓ log · pgup/pgdn ask · esc log ".to_string()
        } else {
            let id = self.selected_id().unwrap_or("—").to_string();
            let asks = if self.asks.is_empty() {
                String::new()
            } else {
                format!(" · ⚑{} a answer", self.asks.len())
            };
            let type_hint = if self.input_enabled() { " · i type" } else { "" };
            format!(" {id}  ↑/↓ scroll{type_hint}{asks} · enter sessions · q quit ")
        };
        frame.render_widget(Paragraph::new(Span::styled(help, style)).style(style), area);
    }

    fn draw_log(&mut self, frame: &mut Frame, area: Rect) {
        // Cleared each frame; set below only when a scrollbar is actually drawn.
        self.scrollbar = None;
        let id = self.selected_id().unwrap_or("—").to_string();

        // Borderless and headerless: the log fills the whole pane at full width.
        // No box — side borders would eat into the 80-column stream and get swept
        // up by terminal text selection. The selected id is already shown in the
        // footer, so no title line is needed here.
        let body = area;

        // No log file, or a file that exists but is empty: a dim hint; the view
        // can't be scrolled.
        let hint = match &self.log {
            None if self.loading.as_deref() == Some(id.as_str()) => {
                Some(format!("(loading '{id}'…)"))
            }
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

        let pane_h_u16 = body.height.max(1);
        let pane_h = pane_h_u16 as usize;
        self.log_rows = pane_h; // for half/full-page scroll keys

        // Cache fast path: if the selected session, log content, scroll position
        // and pane size all match the last render, reuse those lines instead of
        // replaying the vt100 screen + re-parsing ANSI for every tile again.
        let seen = self.log.as_ref().map(|l| l.seen).unwrap_or(0);
        if let Some(c) = &self.log_cache
            && c.id == id
            && c.seen == seen
            && c.scroll_back == self.scroll_back
            && c.pane_w == body.width
            && c.pane_h == pane_h_u16
        {
            let max_scroll = c.max_scroll;
            let lines = c.lines.clone();
            self.render_log_lines(frame, body, lines, max_scroll, self.scroll_back);
            return;
        }

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

        // window[0] is the bottom-most row (distance 0 from the tail).
        let lines: Vec<Line> = if max_scroll == 0 {
            // Everything fits: anchor to the TOP (oldest first), blanks BELOW —
            // a short/near-empty log fills from the top like a normal terminal
            // instead of clinging to the bottom with a blank void above it.
            let mut v: Vec<Line> = (0..total)
                .rev()
                .map(|k| window[k].take().unwrap_or_else(|| Line::from("")))
                .collect();
            v.resize(pane_h, Line::from(""));
            v
        } else {
            // Overflowing: follow the tail (newest at the bottom), filling the
            // pane; blanks only where scrollback history runs out at the top.
            (0..pane_h)
                .rev()
                .map(|k| window[k].take().unwrap_or_else(|| Line::from("")))
                .collect()
        };
        // Stash the computed frame so unchanged subsequent frames skip the
        // whole vt100 replay above, then draw it.
        self.log_cache = Some(LogCache {
            id,
            seen,
            scroll_back: back,
            pane_w: body.width,
            pane_h: pane_h_u16,
            lines: lines.clone(),
            max_scroll,
        });
        self.render_log_lines(frame, body, lines, max_scroll, back);
    }

    /// Paint the (possibly cached) log lines plus the scrollbar, and record the
    /// scrollbar geometry for the mouse handler. Shared by the compute path and
    /// the cache fast path in `draw_log`.
    fn render_log_lines(
        &mut self,
        frame: &mut Frame,
        body: Rect,
        lines: Vec<Line<'static>>,
        max_scroll: usize,
        back: usize,
    ) {
        frame.render_widget(Paragraph::new(Text::from(lines)), body);

        // Scrollbar at the body's right edge, reflecting position in the range.
        // Hidden while an overlay (picker / ask pane) is open — it owns the
        // input then, and a stray bar behind a floating box is just noise.
        if max_scroll > 0 && !self.overlay_open() {
            // ratatui sizes the thumb as `viewport * track / (content + viewport)`,
            // so a deep scrollback collapses it to a single hard-to-grab row.
            // Inflate the viewport length until the thumb is at least
            // `MIN_THUMB` rows (it only affects the thumb's *size*, not the
            // position mapping, which our `scrollbar_scrub` computes itself).
            const MIN_THUMB: usize = 4;
            let track = body.height as usize;
            let viewport = if track > MIN_THUMB {
                let need = (MIN_THUMB * max_scroll.saturating_sub(1)).div_ceil(track - MIN_THUMB);
                need.max(track)
            } else {
                track
            };
            let mut state = ScrollbarState::new(max_scroll)
                .position(max_scroll - back)
                .viewport_content_length(viewport);
            let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .thumb_symbol("┃")
                .thumb_style(Style::default().fg(Color::Gray))
                .track_symbol(Some("│"))
                .track_style(Style::default().fg(Color::DarkGray));
            frame.render_stateful_widget(bar, body, &mut state);
            // Remember where it landed so mouse clicks/drags can target it.
            self.scrollbar = Some(ScrollbarHit {
                area: body,
                max_scroll,
            });
        } else {
            self.scrollbar = None;
        }
    }

    fn draw_selector(&mut self, frame: &mut Frame, area: Rect) {
        // Pending-ask count per worker, so blocked sessions carry a ⚑ badge.
        let mut ask_counts: HashMap<&str, usize> = HashMap::new();
        for a in &self.asks {
            *ask_counts.entry(a.worker.as_str()).or_default() += 1;
        }
        let items: Vec<ListItem> = if self.sessions.is_empty() {
            vec![ListItem::new(Line::from(Span::styled(
                "  no sessions — run `looop up` to start the pulse",
                Style::default().fg(Color::DarkGray),
            )))]
        } else {
            self.sessions
                .iter()
                .map(|s| session_row(s, ask_counts.get(s.id.as_str()).copied().unwrap_or(0)))
                .collect()
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
            .highlight_style(Style::default().bg(SURFACE));
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

/// Render the shared vertical scrollbar into `area`'s right column: a `┃`
/// thumb over a `│` track, no end caps. `pos` is the top-anchored offset in
/// `0..=max_scroll`. A no-op when nothing overflows (`max_scroll == 0`).
fn render_vscrollbar(frame: &mut Frame, area: Rect, max_scroll: usize, pos: usize) {
    if max_scroll == 0 {
        return;
    }
    // ratatui sizes the thumb as `viewport * track / (content + viewport)`;
    // inflate the viewport until the thumb is at least MIN_THUMB rows so it
    // stays grabbable on long content (affects only the thumb SIZE, not the
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

/// List sessions according to `filter`. Alive sessions, the pulse, and any
/// session with a pending ask are always kept — an unanswered question must
/// stay reachable no matter the filter. Returns the visible list plus the
/// count of hidden sessions.
fn list_filtered(
    paths: &Paths,
    filter: Filter,
    ask_workers: &HashSet<String>,
) -> (Vec<Session>, usize) {
    let all = session::list(paths);
    let total = all.len();
    let kept: Vec<Session> = match filter {
        Filter::All => return (all, 0),
        Filter::Active => all
            .into_iter()
            .filter(|s| s.alive || s.is_pulse() || ask_workers.contains(&s.id))
            .collect(),
        Filter::Recent(window) => all
            .into_iter()
            .filter(|s| {
                s.alive
                    || s.is_pulse()
                    || ask_workers.contains(&s.id)
                    || s.idle_for().map(|d| d < window).unwrap_or(true)
            })
            .collect(),
    };
    let hidden = total - kept.len();
    (kept, hidden)
}

/// Render one session as a colored row: a state dot, the id (pulse flagged),
/// a ⚑ badge when it has pending asks, and its state/exit detail.
fn session_row(s: &Session, asks: usize) -> ListItem<'static> {
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
    let badge = if asks > 0 {
        format!("⚑{asks} ")
    } else {
        String::new()
    };
    ListItem::new(Line::from(vec![
        Span::styled(format!("{dot} "), Style::default().fg(color)),
        Span::raw(format!("{label:<20} ")),
        Span::styled(badge, Style::default().fg(Color::Yellow)),
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

/// Replay a session's `output.log` into a fresh vt100 parser. This is the
/// expensive step (a multi-MB tail can take ~1s in debug builds), so it runs
/// on the background worker rather than the UI thread.
fn build_replay(id: String, path: &Path) -> LogReplay {
    let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let mut parser = vt100::Parser::new(PTY_ROWS, PTY_COLS, SCROLLBACK_ROWS);
    // `0` for any log within the cap (first line reachable); only an over-cap
    // log starts mid-stream at the last MAX_REPLAY_BYTES.
    let start = len.saturating_sub(MAX_REPLAY_BYTES);
    if len > 0
        && let Ok(b) = read_from(path, start)
    {
        parser.process(&b);
    }
    let prev_scrollback = scrollback_len(&mut parser);
    LogReplay {
        id,
        parser,
        offset: len,
        prev_scrollback,
        seen: len.saturating_sub(start),
    }
}

/// Spawn the background replay worker. It owns the heavy `build_replay`, so a
/// session switch never blocks the UI. When several requests pile up (fast
/// picker navigation), it skips straight to the newest so it doesn't replay
/// every session passed over.
fn spawn_replay_worker() -> (Sender<ParseRequest>, Receiver<ParseResult>) {
    let (req_tx, req_rx) = std::sync::mpsc::channel::<ParseRequest>();
    let (res_tx, res_rx) = std::sync::mpsc::channel::<ParseResult>();
    std::thread::spawn(move || {
        while let Ok(mut req) = req_rx.recv() {
            while let Ok(newer) = req_rx.try_recv() {
                req = newer; // collapse a backlog to the latest request
            }
            let generation = req.generation;
            let replay = build_replay(req.id, &req.path);
            if res_tx.send(ParseResult { generation, replay }).is_err() {
                break; // UI gone
            }
        }
    });
    (req_tx, res_rx)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp(name: &str, contents: &[u8]) -> std::path::PathBuf {
        let p =
            std::env::temp_dir().join(format!("looop-client-test-{}-{name}", std::process::id()));
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(contents).unwrap();
        p
    }

    #[test]
    #[ignore] // perf probe: set LOOOP_BENCH_LOG=/path/to/output.log
    fn bench_replay() {
        let Ok(p) = std::env::var("LOOOP_BENCH_LOG") else {
            eprintln!("set LOOOP_BENCH_LOG to a real output.log to run this probe");
            return;
        };
        let path = std::path::Path::new(&p);
        let len = std::fs::metadata(path).unwrap().len();
        for cap in [1u64, 2, 4, 8, 16].map(|m| m * 1024 * 1024) {
            let start = len.saturating_sub(cap);
            let bytes = read_from(path, start).unwrap();
            let t0 = Instant::now();
            let mut parser = vt100::Parser::new(PTY_ROWS, PTY_COLS, SCROLLBACK_ROWS);
            parser.process(&bytes);
            let process = t0.elapsed();
            let t1 = Instant::now();
            parser.screen_mut().set_scrollback(usize::MAX);
            let sb = parser.screen().scrollback();
            let scrollback = t1.elapsed();
            let t2 = Instant::now();
            let _ = render::render_screen(parser.screen(), ShotFormat::Ansi, false);
            let render = t2.elapsed();
            println!(
                "cap={:>2}MiB bytes={:>9} process={:>8.1?} scrollback_len={sb} ({:?}) render_screen={:?}",
                cap / 1024 / 1024,
                bytes.len(),
                process,
                scrollback,
                render
            );
        }
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
    fn read_from_start_returns_whole_file() {
        let p = tmp("whole", b"hello world");
        assert_eq!(read_from(&p, 0).unwrap(), b"hello world");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn read_from_offset_returns_appended_tail() {
        let p = tmp("appended", b"0123456789");
        // Feeding incrementally: only the bytes after the last offset come back.
        assert_eq!(read_from(&p, 6).unwrap(), b"6789");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn read_from_missing_file_is_err() {
        let p = std::env::temp_dir().join("looop-client-test-does-not-exist");
        assert!(read_from(&p, 0).is_err());
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
