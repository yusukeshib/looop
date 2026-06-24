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
//! replay the WHOLE log through a `vt100` virtual terminal and render the
//! resulting SCREEN plus its scrollback, instead of dumping every redraw frame
//! as new lines — so scrolling up reaches the session's first line, not just a
//! recent tail. Selecting a row in the bottom pane re-points the log pane.
//!
//! Mouse capture stays on (wheel scrolls, the scrollbar scrubs); hold Shift
//! while dragging to use the terminal's own text selection / copy.

use crate::paths::Paths;
use crate::session::{self, Session};
use anyhow::Result;
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
    ScrollbarState,
};

/// How often we re-list sessions and re-read the tailed log.
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
pub fn cmd_watch(paths: &Paths, args: &crate::cli::WatchArgs) -> Result<ExitCode> {
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
        .map_err(|_| anyhow::anyhow!("looop watch: bad duration '{s}' (try 1d, 12h, 30m, 90s)"))?;
    Ok(Duration::from_secs(n * mult))
}

struct App {
    sessions: Vec<Session>,
    list_state: ListState,
    /// Lines scrolled back from the bottom (0 = follow the tail live).
    scroll_back: usize,
    /// Which sessions the selector shows. Cycled live with `a`.
    filter: Filter,
    /// The window used by [`Filter::Recent`] (from `--since` or
    /// [`DEFAULT_WINDOW`]). Preserved across filter cycling.
    recent_window: Duration,
    /// Sessions hidden by the current filter on the last refresh (footer hint).
    hidden: usize,
    /// `true` while the floating session picker is open (ESC). The log is the
    /// main buffer; the list is hidden until summoned, and ENTER/ESC closes it.
    picking: bool,
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
/// (e.g. a filter toggle that doesn't touch the log) reuse the cached lines.
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
        let (sessions, hidden) = list_filtered(paths, filter);
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
            scroll_back: 0,
            filter,
            recent_window,
            hidden,
            scrollbar: None,
            log: None,
            log_cache: None,
            selector: None,
            picking: false,
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
            self.scroll_back = 0;
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
        let (sessions, hidden) = list_filtered(paths, self.filter);
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
                            // Main buffer (log): scroll, or ESC to open the picker.
                            // `scroll_back` counts rows from the live tail, so
                            // scrolling UP (into history) ADDS and DOWN subtracts.
                            let half = (self.log_rows / 2).max(1);
                            let page = self.log_rows.max(1);
                            match key.code {
                                KeyCode::Char('q') => break,
                                KeyCode::Esc => self.picking = true,
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
                        MouseEventKind::ScrollUp => {
                            self.scroll_back = self.scroll_back.saturating_add(3)
                        }
                        MouseEventKind::ScrollDown => {
                            self.scroll_back = self.scroll_back.saturating_sub(3)
                        }
                        // Grab the scrollbar on press; once grabbed, keep
                        // scrubbing on every drag (row only) until release, so
                        // the cursor can leave the column without dropping it.
                        MouseEventKind::Down(MouseButton::Left) => {
                            self.dragging_scrollbar = self.scrollbar_grab(m.column, m.row);
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
        // The log is the main buffer and owns the whole screen, save a one-row
        // footer for the dim help/legend line.
        let chunks =
            Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).split(frame.area());
        let log_area = chunks[0];
        self.draw_log(frame, log_area);
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
            format!(" {name}{hidden}  ↑/↓ move · a filter · enter select · esc cancel ")
        } else {
            let id = self.selected_id().unwrap_or("—").to_string();
            format!(" {id}  ↑/↓ scroll · esc sessions · q quit ")
        };
        let style = Style::default().bg(Color::White).fg(Color::Black);
        frame.render_widget(Paragraph::new(Span::styled(help, style)).style(style), area);
    }

    fn draw_log(&mut self, frame: &mut Frame, area: Rect) {
        // Cleared each frame; set below only when a scrollbar is actually drawn.
        self.scrollbar = None;
        let id = self.selected_id().unwrap_or("—").to_string();

        // Borderless and headerless: the log fills the whole pane at full width.
        // No box — side borders would eat into the 80-column stream and get swept
        // up by terminal text selection. The selected id is already shown in the
        // bottom session pane, so no title line is needed here.
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
        // Hidden while the picker is open — it owns the input then, and a stray
        // bar behind the floating box is just noise.
        if max_scroll > 0 && !self.picking {
            // ratatui sizes the thumb as `viewport * track / (content + viewport)`,
            // so a deep scrollback collapses it to a single hard-to-grab row.
            // Inflate the viewport length until the thumb is at least
            // `MIN_THUMB` rows (it only affects the thumb's *size*, not the
            // position mapping, which our `scrollbar_drag` computes itself).
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
        let items: Vec<ListItem> = if self.sessions.is_empty() {
            vec![ListItem::new(Line::from(Span::styled(
                "  no sessions — run `looop up` to start the pulse",
                Style::default().fg(Color::DarkGray),
            )))]
        } else {
            self.sessions.iter().map(session_row).collect()
        };

        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL))
            // Uniform white background on the selected row. REVERSED would
            // flip each span's fg into its bg (green dot → green block, gray
            // detail → gray block), which reads as a messy multicolored bar;
            // forcing bg=white + fg=black patches over the per-span colors for
            // one clean highlight.
            .highlight_style(Style::default().bg(Color::White).fg(Color::Black));
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
fn list_filtered(paths: &Paths, filter: Filter) -> (Vec<Session>, usize) {
    let all = session::list(paths);
    let total = all.len();
    let kept: Vec<Session> = match filter {
        Filter::All => return (all, 0),
        Filter::Active => all
            .into_iter()
            .filter(|s| s.alive || s.is_pulse())
            .collect(),
        Filter::Recent(window) => all
            .into_iter()
            .filter(|s| s.alive || s.is_pulse() || s.idle_for().map(|d| d < window).unwrap_or(true))
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
            std::env::temp_dir().join(format!("looop-watch-test-{}-{name}", std::process::id()));
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
        let p = std::env::temp_dir().join("looop-watch-test-does-not-exist");
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
