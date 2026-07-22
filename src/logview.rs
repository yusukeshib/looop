//! `LogView` — a reusable, scrollable replay of a session's `output.log`.
//!
//! `looop watch` takes a PTY-backed session's raw `output.log` (an interactive
//! agent redraws in place — cursor moves, line/screen clears, carriage returns
//! — so the bytes are NOT a clean line log), replays the WHOLE stream through a
//! `vt100` virtual terminal, and renders the resulting SCREEN plus bounded
//! scrollback into a pane.
//!
//! This module owns the replay machinery:
//!
//! - the persistent [`LogReplay`] (fed only newly-appended bytes each frame),
//! - a background replay worker (the initial multi-MB parse is off the UI
//!   thread),
//! - the expensive vt100→ANSI→ratatui render, cached across idle frames,
//! - the bottom-anchored scroll model (`scroll_back` counts rows up from the
//!   live tail; 0 = follow) with a `looop watch`-style scrollbar,
//! - and an optional block of `tail` lines appended at the bottom (watch uses
//!   an empty tail, but keeping the renderer generic avoids coupling it to the
//!   observer's current layout).

use crate::paths::Paths;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};

use ansi_to_tui::IntoText;
use babysit::cli::ShotFormat;
use babysit::render;
use ratatui::prelude::*;
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};

/// Cap on the initial replay read. Logs within the cap start at byte zero;
/// larger logs keep their live tail. The virtual terminal separately bounds
/// retained scrollback rows, so very long histories may drop their oldest rows.
/// This also caps each incremental read, so one burst cannot force
/// an unbounded allocation; remaining bytes are consumed on subsequent ticks.
/// At/below the cap we read from byte 0; above it we fall back to the last
/// `MAX_REPLAY_BYTES` (live tail preserved, oldest bytes dropped).
const MAX_REPLAY_BYTES: u64 = 16 * 1024 * 1024;

/// Recorded PTY geometry of every detached worker. looop spawns with
/// `size = None` (see `session::spawn_detached`), so babysit allocates its
/// default `DEFAULT_SCREENSHOT_SIZE` PTY (80×24). The `output.log` is therefore
/// a stream meant for THIS exact grid: an interactive agent positions its
/// cursor, clears lines, and scrolls assuming these dimensions. We MUST replay
/// at the recorded size — both rows AND cols — or absolute cursor moves and the
/// scroll region drift (babysit's own screenshot path replays at the same
/// size). Sourced straight from babysit so the two can never skew.
const PTY_ROWS: u16 = render::DEFAULT_SCREENSHOT_SIZE.0;
const PTY_COLS: u16 = render::DEFAULT_SCREENSHOT_SIZE.1;

/// How many rows of scrollback the virtual terminal retains. The agents don't
/// use the alternate screen (they redraw in place on the primary screen), so
/// content that scrolls off the top lands here and stays reachable. vt100 grows
/// scrollback lazily, so this is just an upper bound. Keep enough for long
/// sessions while bounding the observer's worst-case memory (vt100 cells are
/// substantially larger than the source bytes).
const SCROLLBACK_ROWS: usize = 25_000;

/// Bytes immediately before the consumed offset used to detect an in-place
/// truncate/rewrite that regrew past that offset between UI ticks. File
/// replacement/rotation is detected separately by identity.
const PREFIX_GUARD_BYTES: u64 = 4096;

/// The dim gray style shared by hints and secondary text.
fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

/// A request to the replay worker: parse `path` for session `id`. `generation`
/// lets the UI ignore a result whose session is no longer selected.
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
    /// Identity of the open log generation. Atomic replacement/rotation changes
    /// this even when the new file has already regrown beyond `offset`.
    identity: FileIdentity,
    /// Bytes of `output.log` already fed to the parser.
    offset: u64,
    /// Scrollback depth after the last feed, to measure how far the tail moved
    /// (so a scrolled-back viewport can be nudged to stay anchored).
    prev_scrollback: usize,
    /// Total bytes ever fed — 0 means the file exists but is empty.
    seen: u64,
    /// Hash of the bytes immediately before `offset`. An append-only file keeps
    /// this stable; a truncate/replace that regrows past `offset` does not.
    prefix_guard: u64,
}

/// Cached result of the expensive vt100→ANSI→ratatui render. That path renders
/// the whole screen to ANSI and re-parses it with `ansi-to-tui` for each tile —
/// far too costly to repeat every frame. We keep the last result and only
/// rebuild it when an input that affects the rendered lines changes: the
/// session, the log content (`seen`), the scroll position, the pane size, or
/// the appended `tail` (`tail_sig`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct FileIdentity {
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(not(unix))]
    created: Option<std::time::SystemTime>,
}

fn file_identity(meta: &std::fs::Metadata) -> FileIdentity {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        FileIdentity {
            dev: meta.dev(),
            ino: meta.ino(),
        }
    }
    #[cfg(not(unix))]
    {
        FileIdentity {
            created: meta.created().ok(),
        }
    }
}

struct LogCache {
    id: String,
    seen: u64,
    scroll_back: usize,
    pane_w: u16,
    pane_h: u16,
    /// Signature of the appended tail lines (so a changed ask invalidates it).
    tail_sig: u64,
    lines: Vec<Line<'static>>,
    max_scroll: usize,
}

/// The scrollbar's on-screen track and the scroll depth it represents, captured
/// during [`LogView::render`] for the mouse handler to consume.
#[derive(Clone, Copy)]
pub struct ScrollbarHit {
    /// Area the `Scrollbar` widget was rendered into (column = `right()-1`).
    area: Rect,
    /// Maximum scroll depth (`scroll_back` ranges `0..=max_scroll`).
    max_scroll: usize,
}

/// A scrollable replay of one session's `output.log`, with an optional block of
/// caller-supplied lines pinned at the very bottom of the buffer.
pub struct LogView {
    /// Rows scrolled back from the bottom (0 = follow the tail live). The tail
    /// is the LAST appended line when `tail` is non-empty, else the live screen.
    pub scroll_back: usize,
    /// `true` while the left button is held after grabbing the scrollbar, so
    /// drags keep scrubbing even when the cursor drifts off the thin column.
    pub dragging_scrollbar: bool,
    /// Session id whose log to show (`None` clears the view).
    target: Option<String>,
    /// Persistent vt100 replay of the target's log (fed incrementally).
    log: Option<LogReplay>,
    /// Cached output of the vt100→ANSI→ratatui render, reused on idle frames.
    log_cache: Option<LogCache>,
    /// Background vt100-replay worker: the heavy initial parse runs off the UI
    /// thread; the live tail is then fed incrementally on the UI thread.
    parse_tx: Sender<ParseRequest>,
    parse_rx: Receiver<ParseResult>,
    parse_gen: u64,
    loading: Option<String>,
    /// Geometry of the scrollbar from the last draw, for mouse clicks/drags.
    scrollbar: Option<ScrollbarHit>,
    /// Pane height (rows) from the last draw, for half/full-page scroll keys.
    pane_rows: usize,
}

impl Default for LogView {
    fn default() -> Self {
        Self::new()
    }
}

impl LogView {
    pub fn new() -> Self {
        let (parse_tx, parse_rx) = spawn_replay_worker();
        LogView {
            scroll_back: 0,
            dragging_scrollbar: false,
            target: None,
            log: None,
            log_cache: None,
            parse_tx,
            parse_rx,
            parse_gen: 0,
            loading: None,
            scrollbar: None,
            pane_rows: 0,
        }
    }

    /// Pane height from the last draw (min 1), for page-scroll math.
    pub fn rows(&self) -> usize {
        self.pane_rows.max(1)
    }

    /// Point the view at a session's log. Switching targets re-follows the tail
    /// (`scroll_back = 0`), mirroring `looop watch`'s selection moves; the
    /// actual (re)parse happens on the next [`sync`](Self::sync).
    pub fn set_target(&mut self, id: Option<String>) {
        if self.target.as_deref() != id.as_deref() {
            self.scroll_back = 0;
            self.target = id;
        }
    }

    /// Bring the persistent replay in sync with the target's `output.log`:
    /// (re)build the parser on a target change or a truncated file, then feed
    /// any newly-appended bytes. Keeps a paused viewport anchored by nudging
    /// `scroll_back` when the tail grows.
    pub fn sync(&mut self, paths: &Paths) {
        // Install any finished background replay that still matches the current
        // target (stale ones — from sessions navigated past — are dropped).
        while let Ok(res) = self.parse_rx.try_recv() {
            if res.generation == self.parse_gen {
                self.log = Some(res.replay);
                self.loading = None;
                self.log_cache = None; // fresh buffer → drop the render cache
            }
        }

        let Some(id) = self.target.clone() else {
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
        let identity = file_identity(&meta);

        let reset = match &self.log {
            // New session, replacement/rotation, truncation, or an in-place
            // rewrite that changed the consumed suffix before regrowing.
            Some(l) => {
                l.id != id
                    || l.identity != identity
                    || len < l.offset
                    || prefix_guard(&path, l.offset)
                        .map(|guard| guard != l.prefix_guard)
                        .unwrap_or(true)
            }
            None => true,
        };

        if reset {
            // The full replay can take ~1s in debug builds, so hand it to the
            // background worker instead of freezing the UI. Only fire a fresh
            // request when we're not already parsing this exact session.
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
        let start = l.offset;
        let end = len.min(start.saturating_add(MAX_REPLAY_BYTES));
        let delta = match read_range(&path, start, end) {
            Ok(b) => {
                let consumed = b.len() as u64;
                l.seen += consumed;
                // Advance by what this read ACTUALLY consumed. The file can be
                // appended or truncated after the metadata snapshot; using
                // `len` here could replay bytes twice or skip them.
                l.offset = start.saturating_add(consumed);
                l.parser.process(&b);
                if let Ok(guard) = prefix_guard(&path, l.offset) {
                    l.prefix_guard = guard;
                }
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

    /// Scroll by `delta` rows: positive scrolls UP into history (older),
    /// negative scrolls DOWN toward the live tail. Clamped at the tail here;
    /// the clamp to the oldest line happens in [`render`](Self::render).
    pub fn scroll(&mut self, delta: isize) {
        self.scroll_back = self.scroll_back.saturating_add_signed(delta);
    }

    /// Jump to the OLDEST line (top of history). Clamped in `render`.
    pub fn jump_oldest(&mut self) {
        self.scroll_back = usize::MAX;
    }

    /// Jump to the live TAIL (follow new output).
    pub fn follow_tail(&mut self) {
        self.scroll_back = 0;
    }

    /// Render the log (plus any `tail` lines pinned at the bottom) into `area`.
    /// `show_scrollbar` hides the bar while a floating overlay owns the view.
    pub fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        tail: &[Line<'static>],
        show_scrollbar: bool,
    ) {
        self.scrollbar = None;
        let pane_h_u16 = area.height.max(1);
        let pane_h = pane_h_u16 as usize;
        self.pane_rows = pane_h;
        let id = self.target.clone().unwrap_or_else(|| "—".to_string());

        // No usable log yet: stack a dim hint above the tail and bottom-anchor
        // it (short content, so no vt100 tiling needed).
        let hint = match &self.log {
            None if self.loading.as_deref() == Some(id.as_str()) => {
                Some(format!("(loading '{id}'…)"))
            }
            None => Some(format!("(no log for '{id}')")),
            Some(l) if l.seen == 0 => Some("(no output yet)".to_string()),
            Some(_) => None,
        };
        if let Some(hint) = hint {
            let mut content: Vec<Line<'static>> = vec![Line::from(Span::styled(hint, dim()))];
            if !tail.is_empty() {
                content.push(Line::from(""));
                content.extend(tail.iter().cloned());
            }
            let (lines, max_scroll, back) = window_of(&content, pane_h, self.scroll_back);
            self.scroll_back = back;
            self.paint(frame, area, lines, max_scroll, back, show_scrollbar);
            return;
        }

        let seen = self.log.as_ref().map(|l| l.seen).unwrap_or(0);
        let tail_sig = tail_signature(tail);

        // Cache fast path: reuse the rendered lines when nothing that affects
        // them changed (session, content, scroll, pane size, appended tail).
        if let Some(c) = &self.log_cache
            && c.id == id
            && c.seen == seen
            && c.scroll_back == self.scroll_back
            && c.pane_w == area.width
            && c.pane_h == pane_h_u16
            && c.tail_sig == tail_sig
        {
            let max_scroll = c.max_scroll;
            let lines = c.lines.clone();
            self.paint(
                frame,
                area,
                lines,
                max_scroll,
                self.scroll_back,
                show_scrollbar,
            );
            return;
        }

        let extra = tail.len();
        let log = self.log.as_mut().expect("log present: None handled above");
        let rows = log.parser.screen().size().0 as usize; // recorded grid height

        // Probe scrollback depth and clamp the viewport against the COMBINED
        // (scrollback + live screen + tail) height, bottom-anchored `back` rows
        // from the very bottom (0 = follow). Home lands the OLDEST line at the
        // top of the pane.
        log.parser.screen_mut().set_scrollback(usize::MAX);
        let max_back = log.parser.screen().scrollback();
        let log_total = max_back + rows;
        let total = log_total + extra;
        let max_scroll = total.saturating_sub(pane_h);
        let back = self.scroll_back.min(max_scroll);
        self.scroll_back = back;

        // Compose the visible pane from the bottom up. Slot 0 is the bottom row
        // (overall distance `back` from the very bottom); slot k is distance
        // `back + k`. The tail occupies overall distances `0..extra` (its LAST
        // line is distance 0); log rows sit above, a log-tail-distance `L`
        // mapping to overall distance `L + extra`.
        let mut window: Vec<Option<Line>> = vec![None; pane_h];
        for (i, line) in tail.iter().enumerate() {
            let d = extra - 1 - i; // distance of this tail line from the bottom
            if d >= back && d < back + pane_h {
                window[d - back] = Some(line.clone());
            }
        }

        // Tile the log in chunks of one screen (`rows`): each vt100 render is a
        // screenful at a scrollback offset; we stitch enough to fill the slots
        // the tail didn't. `log_back` is where the log portion starts.
        let log_back = back.saturating_sub(extra);
        let mut t = 0usize;
        loop {
            let off = (log_back + t * rows).min(max_back);
            log.parser.screen_mut().set_scrollback(off);
            // babysit's renderer emits per-row ANSI (SGR only, no cursor motion)
            // which ansi-to-tui parses into styled lines cleanly; trim=false
            // keeps the full-height screenful so row indexing is stable.
            let shot = render::render_screen(log.parser.screen(), ShotFormat::Ansi, false);
            let screen = shot
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            // babysit emits per-row SGR but trims trailing blank cells, so a row
            // that ends mid-style (e.g. a red error line whose own `\x1b[0m`
            // landed in the now-trimmed trailing blanks) never emits its reset.
            // ANSI SGR state carries across `\n`, so that color then bleeds onto
            // the following dim rows. Each vt100 row is an independent screen
            // render, so terminate every row with a reset before it is parsed.
            let screen = screen.replace('\n', "\x1b[0m\n");
            let text = screen
                .as_str()
                .into_text()
                .unwrap_or_else(|_| Text::from(screen.clone()));
            for (r, line) in text.lines.iter().enumerate().take(rows) {
                let l = off + (rows - 1 - r); // this row's log-tail distance
                let overall = l + extra;
                if overall >= back && overall < back + pane_h && window[overall - back].is_none() {
                    window[overall - back] = Some(line.clone());
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

        // window[0] is the bottom-most row (distance `back` from the bottom).
        let lines: Vec<Line> = if max_scroll == 0 {
            // Everything fits: anchor to the TOP (oldest first), blanks BELOW —
            // a short buffer fills from the top like a normal terminal.
            let mut v: Vec<Line> = (0..total)
                .rev()
                .map(|k| window[k].take().unwrap_or_else(|| Line::from("")))
                .collect();
            v.resize(pane_h, Line::from(""));
            v
        } else {
            // Overflowing: follow the tail (newest at the bottom); blanks only
            // where scrollback history runs out at the top.
            (0..pane_h)
                .rev()
                .map(|k| window[k].take().unwrap_or_else(|| Line::from("")))
                .collect()
        };

        self.log_cache = Some(LogCache {
            id,
            seen,
            scroll_back: back,
            pane_w: area.width,
            pane_h: pane_h_u16,
            tail_sig,
            lines: lines.clone(),
            max_scroll,
        });
        self.paint(frame, area, lines, max_scroll, back, show_scrollbar);
    }

    /// Paint the (possibly cached) pane lines plus the scrollbar, and record the
    /// scrollbar geometry for the mouse handler.
    fn paint(
        &mut self,
        frame: &mut Frame,
        body: Rect,
        lines: Vec<Line<'static>>,
        max_scroll: usize,
        back: usize,
        show_scrollbar: bool,
    ) {
        frame.render_widget(Paragraph::new(Text::from(lines)), body);
        if max_scroll > 0 && show_scrollbar {
            // ratatui sizes the thumb as `viewport * track / (content + viewport)`,
            // so a deep scrollback collapses it to a single hard-to-grab row.
            // Inflate the viewport length until the thumb is at least MIN_THUMB
            // rows (affects only the thumb SIZE, not the position mapping).
            const MIN_THUMB: usize = 4;
            let track = body.height as usize;
            let viewport = if track > MIN_THUMB {
                let need = (MIN_THUMB * max_scroll.saturating_sub(1)).div_ceil(track - MIN_THUMB);
                need.max(track)
            } else {
                track
            };
            let mut state = ScrollbarState::new(max_scroll.saturating_add(1))
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
            self.scrollbar = Some(ScrollbarHit {
                area: body,
                max_scroll,
            });
        } else {
            self.scrollbar = None;
        }
    }

    /// Begin a scrollbar drag. Returns `true` if `(col, row)` landed on the
    /// bar's column within the track, scrubbing the viewport to that row; the
    /// caller then holds the grab and feeds later moves to `scrollbar_scrub`.
    pub fn scrollbar_grab(&mut self, col: u16, row: u16) -> bool {
        let Some(hit) = self.scrollbar else {
            return false;
        };
        let a = hit.area;
        // The vertical scrollbar lives in the rightmost column of its area.
        // Accept clicks on that column (and the border just right of it, for a
        // forgiving target) within the track's row range.
        if col + 1 < a.right() || col > a.right() || row < a.top() || row >= a.bottom() {
            return false;
        }
        self.scrollbar_scrub(row);
        true
    }

    /// Scrub the viewport to `row` on the scrollbar track (column ignored, used
    /// while a grab is held). The track maps linearly top→bottom: top (↑) =
    /// oldest scrollback, bottom (↓) = live tail.
    pub fn scrollbar_scrub(&mut self, row: u16) {
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
}

/// Bottom-anchor a short `content` slice into a `pane_h`-tall window at
/// `scroll_back` rows up from the bottom. Returns the top-first pane lines, the
/// clamped `max_scroll`, and the clamped `back`. Used only for the non-tiled
/// (hint) path; the big-log path tiles the vt100 screen directly.
fn window_of(
    content: &[Line<'static>],
    pane_h: usize,
    scroll_back: usize,
) -> (Vec<Line<'static>>, usize, usize) {
    let total = content.len();
    let max_scroll = total.saturating_sub(pane_h);
    let back = scroll_back.min(max_scroll);
    let lines: Vec<Line> = if max_scroll == 0 {
        // Fits: top-anchored (oldest first), blanks below.
        let mut v: Vec<Line> = content.to_vec();
        v.resize(pane_h, Line::from(""));
        v
    } else {
        // Overflowing: bottom-anchored. Bottom row = content[total-1-back].
        let bottom = total - 1 - back;
        (0..pane_h)
            .map(|k| {
                let dist = pane_h - 1 - k; // distance of this row from the bottom
                match bottom.checked_sub(dist) {
                    Some(idx) => content[idx].clone(),
                    None => Line::from(""),
                }
            })
            .collect()
    };
    (lines, max_scroll, back)
}

/// A cheap signature of the appended tail lines, so a changed ask invalidates
/// the render cache without hashing the whole (expensive) log.
fn tail_signature(tail: &[Line]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    tail.len().hash(&mut h);
    for line in tail {
        for span in &line.spans {
            span.content.as_ref().hash(&mut h);
        }
    }
    h.finish()
}

/// Probe a parser's current scrollback depth (rows above the live screen).
/// Leaves the viewport parked at the oldest line; the next render repositions
/// it before drawing.
fn scrollback_len(parser: &mut vt100::Parser) -> usize {
    parser.screen_mut().set_scrollback(usize::MAX);
    parser.screen().scrollback()
}

/// Replay a session's `output.log` into a fresh vt100 parser. This is the
/// expensive step (a multi-MB tail can take ~1s in debug builds), so it runs on
/// the background worker rather than the UI thread.
fn build_replay(id: String, path: &Path) -> LogReplay {
    let mut parser = vt100::Parser::new(PTY_ROWS, PTY_COLS, SCROLLBACK_ROWS);
    let Ok(mut file) = std::fs::File::open(path) else {
        return LogReplay {
            id,
            parser,
            identity: FileIdentity::default(),
            offset: 0,
            prev_scrollback: 0,
            seen: 0,
            prefix_guard: 0,
        };
    };
    let meta = file.metadata().ok();
    let len = meta.as_ref().map_or(0, std::fs::Metadata::len);
    let identity = meta.as_ref().map(file_identity).unwrap_or_default();
    // `0` for any log within the cap (first line reachable); only an over-cap
    // log starts mid-stream at the last MAX_REPLAY_BYTES.
    let start = len.saturating_sub(MAX_REPLAY_BYTES);
    let consumed = if len > 0 {
        read_range_from(&mut file, start, len)
            .map(|b| {
                parser.process(&b);
                b.len() as u64
            })
            .unwrap_or(0)
    } else {
        0
    };
    let prev_scrollback = scrollback_len(&mut parser);
    let offset = start.saturating_add(consumed);
    LogReplay {
        id,
        parser,
        identity,
        offset,
        prev_scrollback,
        seen: consumed,
        prefix_guard: prefix_guard(path, offset).unwrap_or(0),
    }
}

/// Spawn the background replay worker. It owns the heavy `build_replay`, so a
/// session switch never blocks the UI. When several requests pile up (fast
/// navigation), it skips straight to the newest.
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

/// Hash the consumed prefix's trailing window. This catches the practical
/// truncate-then-regrow case even when the new length already exceeds the old
/// offset; replacement/rotation is handled by [`FileIdentity`]. output.log is
/// append-only in normal operation, so a stable identity + stable consumed
/// suffix is sufficient to continue incremental replay.
fn prefix_guard(path: &Path, offset: u64) -> std::io::Result<u64> {
    let start = offset.saturating_sub(PREFIX_GUARD_BYTES);
    let bytes = read_range(path, start, offset)?;
    // FNV-1a: stable, tiny, and sufficient as a corruption guard (not security).
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    Ok(hash)
}

/// Read exactly the snapshotted byte range `[start, end)`, or fewer bytes if
/// the file was truncated concurrently. Appends after `end` wait for the next
/// sync so no byte can be fed to the terminal twice.
fn read_range(path: &Path, start: u64, end: u64) -> std::io::Result<Vec<u8>> {
    let mut file = std::fs::File::open(path)?;
    read_range_from(&mut file, start, end)
}

fn read_range_from(file: &mut std::fs::File, start: u64, end: u64) -> std::io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    file.take(end.saturating_sub(start)).read_to_end(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
fn read_from(path: &Path, start: u64) -> std::io::Result<Vec<u8>> {
    let end = std::fs::metadata(path)?.len();
    read_range(path, start, end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp(name: &str, contents: &[u8]) -> std::path::PathBuf {
        let p =
            std::env::temp_dir().join(format!("looop-logview-test-{}-{name}", std::process::id()));
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(contents).unwrap();
        p
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
        assert_eq!(read_from(&p, 6).unwrap(), b"6789");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn read_range_stops_at_the_snapshotted_end() {
        let p = tmp("bounded", b"0123456789");
        assert_eq!(read_range(&p, 2, 6).unwrap(), b"2345");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    #[cfg(unix)]
    fn file_identity_detects_atomic_replacement() {
        let p = tmp("identity", b"old");
        let before = file_identity(&std::fs::metadata(&p).unwrap());
        let replacement = tmp("identity-replacement", b"new");
        std::fs::rename(&replacement, &p).unwrap();
        let after = file_identity(&std::fs::metadata(&p).unwrap());
        assert_ne!(after, before);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn prefix_guard_detects_a_rewritten_consumed_prefix() {
        let p = tmp("guard", b"0123456789");
        let before = prefix_guard(&p, 10).unwrap();
        std::fs::write(&p, b"abcdefghij-extra").unwrap();
        assert_ne!(prefix_guard(&p, 10).unwrap(), before);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn read_from_missing_file_is_err() {
        let p = std::env::temp_dir().join("looop-logview-test-does-not-exist");
        assert!(read_from(&p, 0).is_err());
    }

    #[test]
    fn window_of_fits_anchors_top() {
        let content: Vec<Line> = (0..3).map(|i| Line::from(format!("l{i}"))).collect();
        let (lines, max_scroll, back) = window_of(&content, 5, 0);
        assert_eq!(max_scroll, 0);
        assert_eq!(back, 0);
        assert_eq!(lines.len(), 5);
        // Oldest at the top, blanks below.
        assert_eq!(lines[0].spans[0].content.as_ref(), "l0");
        assert_eq!(lines[2].spans[0].content.as_ref(), "l2");
        assert!(lines[4].spans.is_empty() || lines[4].spans[0].content.as_ref().is_empty());
    }

    // A red Error line that WRAPS must not bleed its color onto the following
    // dim heartbeat lines. babysit's per-row renderer trims trailing blank cells
    // (where the line's own reset lived), so we re-terminate each row before
    // ansi_to_tui parses it. Guards against the "red leaks downward" regression.
    #[test]
    fn wrapped_color_does_not_bleed_onto_next_rows() {
        let mut parser = vt100::Parser::new(30, 40, 0); // narrow → the error line wraps
        let bytes = concat!(
            "\x1b[2m[08:05:24]\x1b[0m \x1b[1m\x1b[31m\u{2717}\x1b[0m ",
            "\x1b[31mtick failed after 300s (fail #1) \u{b7} replay: /Users/y/.local/state/looop/runs/tick-1\x1b[0m\r\n",
            "\x1b[2m[08:05:24] \u{b7} next beat in 60s (idle)\x1b[0m\r\n",
            "\x1b[2m[08:06:24] \u{b7} 20 sensors ok (39s)\x1b[0m\r\n",
        );
        parser.process(bytes.as_bytes());
        let shot = render::render_screen(parser.screen(), ShotFormat::Ansi, false);
        let screen = shot
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        // Same normalization the render path applies.
        let screen = screen.replace('\n', "\x1b[0m\n");
        let text = screen.as_str().into_text().unwrap();
        // Find the two heartbeat rows and assert none of their spans are red.
        let mut checked = 0;
        for line in &text.lines {
            let joined: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            if joined.contains("next beat") || joined.contains("sensors ok") {
                checked += 1;
                for sp in &line.spans {
                    assert_ne!(
                        sp.style.fg,
                        Some(Color::Red),
                        "red leaked onto dim row: {:?}",
                        joined
                    );
                }
            }
        }
        assert_eq!(checked, 2, "expected to find both heartbeat rows");
    }

    #[test]
    fn window_of_overflow_follows_tail() {
        let content: Vec<Line> = (0..10).map(|i| Line::from(format!("l{i}"))).collect();
        // pane 3, follow tail: bottom row is the last line.
        let (lines, max_scroll, back) = window_of(&content, 3, 0);
        assert_eq!(max_scroll, 7);
        assert_eq!(back, 0);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[2].spans[0].content.as_ref(), "l9");
        assert_eq!(lines[0].spans[0].content.as_ref(), "l7");
        // Scrolled back 2: bottom row is l7.
        let (lines, _, back) = window_of(&content, 3, 2);
        assert_eq!(back, 2);
        assert_eq!(lines[2].spans[0].content.as_ref(), "l7");
        // Scrolled past the top clamps to max_scroll (oldest at the top).
        let (lines, _, back) = window_of(&content, 3, usize::MAX);
        assert_eq!(back, 7);
        assert_eq!(lines[0].spans[0].content.as_ref(), "l0");
    }
}
