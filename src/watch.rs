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
//! and workers are PTY-backed, so their `output.log` carries ANSI color we
//! render as-is. Selecting a row in the bottom pane re-points the log pane.

use crate::paths::Paths;
use crate::session::{self, Session};
use anyhow::Result;
use std::io::{Read, Seek, SeekFrom};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use ansi_to_tui::IntoText;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

/// How often we re-list sessions and re-read the tailed log.
const TICK: Duration = Duration::from_millis(250);
/// Tail at most this many bytes of output.log — bounds work on huge logs while
/// keeping enough scrollback to fill the pane.
const TAIL_BYTES: u64 = 256 * 1024;

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
    let res = App::new(paths, initial, window).run(&mut terminal, paths);
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
        }
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

    fn run(&mut self, terminal: &mut ratatui::DefaultTerminal, paths: &Paths) -> Result<()> {
        let mut last_refresh = Instant::now()
            .checked_sub(TICK)
            .unwrap_or_else(Instant::now);
        loop {
            if last_refresh.elapsed() >= TICK {
                self.refresh(paths);
                last_refresh = Instant::now();
            }

            // Read the tailed log fresh each frame: cheap (bounded bytes) and
            // always current.
            let text = self
                .selected_id()
                .map(|id| read_log_tail(paths, id))
                .unwrap_or_default();

            terminal.draw(|f| self.draw(f, &text))?;

            if event::poll(TICK)?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
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
                    KeyCode::PageUp => self.scroll_back = self.scroll_back.saturating_add(10),
                    KeyCode::PageDown => self.scroll_back = self.scroll_back.saturating_sub(10),
                    KeyCode::Home => self.scroll_back = usize::MAX, // jump to oldest
                    KeyCode::End => self.scroll_back = 0,           // back to live tail
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn draw(&mut self, frame: &mut Frame, log: &Text<'_>) {
        // Selector grows with the fleet but is capped so the log keeps the room.
        let rows = self.sessions.len().clamp(1, 8) as u16;
        let chunks = Layout::vertical([Constraint::Min(3), Constraint::Length(rows + 2)])
            .split(frame.area());

        self.draw_log(frame, chunks[0], log);
        self.draw_selector(frame, chunks[1]);
    }

    fn draw_log(&mut self, frame: &mut Frame, area: Rect, log: &Text<'_>) {
        let follow = self.scroll_back == 0;
        let id = self.selected_id().unwrap_or("—");
        let title = if follow {
            format!(" {id} — live ")
        } else {
            format!(" {id} — scrolled (End=live) ")
        };

        // Show the slice of lines that fits, anchored to the bottom (the tail)
        // unless the user has scrolled back. Slicing ourselves keeps the math
        // simple and avoids wrap/scroll interactions.
        let inner_h = area.height.saturating_sub(2) as usize; // minus borders
        let total = log.lines.len();
        let max_back = total.saturating_sub(inner_h);
        let back = self.scroll_back.min(max_back);
        self.scroll_back = back; // clamp Home's usize::MAX to the real maximum
        let end = total - back;
        let start = end.saturating_sub(inner_h);
        let visible: Vec<Line> = log.lines[start..end].to_vec();

        let para = Paragraph::new(visible).block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .title_style(Style::default().add_modifier(Modifier::BOLD)),
        );
        frame.render_widget(para, area);
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

        let title = match self.window {
            Some(_) if self.hidden > 0 => format!(
                " sessions ({} hidden)  ↑/↓ select · a all · q quit ",
                self.hidden
            ),
            Some(_) => String::from(" sessions  ↑/↓ select · a all · q quit "),
            None => String::from(" sessions (all)  ↑/↓ select · a recent · q quit "),
        };
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .title_style(Style::default().add_modifier(Modifier::BOLD)),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
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

/// Read the tail of a session's `output.log` and parse its ANSI into styled
/// ratatui text. Bounded to [`TAIL_BYTES`]; a missing/empty log yields a hint.
fn read_log_tail(paths: &Paths, id: &str) -> Text<'static> {
    let path = paths.sessions().output_log_path(id);
    let bytes = match read_tail_bytes(&path, TAIL_BYTES) {
        Ok(b) if !b.is_empty() => b,
        Ok(_) => {
            return Text::from(Span::styled(
                "(no output yet)",
                Style::default().fg(Color::DarkGray),
            ));
        }
        Err(_) => {
            return Text::from(Span::styled(
                format!("(no log for '{id}')"),
                Style::default().fg(Color::DarkGray),
            ));
        }
    };
    // PTY logs use CRLF; drop the CR so lines don't render with stray carriage
    // returns. ansi-to-tui turns SGR escapes into styled spans (and skips the
    // cursor-movement escapes an interactive agent emits).
    let cleaned = String::from_utf8_lossy(&bytes).replace('\r', "");
    cleaned
        .into_text()
        .unwrap_or_else(|_| Text::from(cleaned.clone()))
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
}
