//! `looop client` — a minimal, non-agent TUI for answering worker asks.
//!
//! looop's steering surface is the `looop _ …` CONTRACT. The RECOMMENDED client
//! is an AGENT ("concierge", see the /looop skill) that watches for asks, relays
//! them to the human in plain language with a recommendation, and drives the
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
//!   ┌─ asks ───────────┬─ detail ─────────────────────┐
//!   │ > ⚑ worker-1-1   │ worker: worker-1             │
//!   │   ⚑ triage-2     │                              │
//!   │                  │ <the worker's question>      │
//!   │                  │ ref: reports/triage.md       │
//!   │                  │ options: ship, hold          │
//!   └──────────────────┴──────────────────────────────┘
//!    enter answer · ↑/↓ move · q quit          (footer)
//!
//! Read + one narrow write: it lists pending asks (`mailbox::pending`) and, on
//! submit, durably resolves the selected one (`mailbox::answer`). It never
//! spawns a worker or edits policy — for that, use the agent concierge or the
//! raw `_` verbs.

use crate::mailbox::{self, Ask};
use crate::paths::Paths;
use crate::run;
use crate::session;
use anyhow::Result;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

/// Re-list asks / re-check the pulse this often, and the input-poll timeout.
const TICK: Duration = Duration::from_millis(250);

/// `looop client` — bring up the ask-answering TUI.
pub fn cmd_client(paths: &Paths) -> Result<ExitCode> {
    let mut terminal = ratatui::init();
    let res = App::new().run(&mut terminal, paths);
    ratatui::restore();
    res?;
    Ok(ExitCode::SUCCESS)
}

/// Browse the asks, or type an answer for the selected one.
enum Mode {
    Browse,
    Answer,
}

struct App {
    asks: Vec<Ask>,
    list_state: ListState,
    mode: Mode,
    /// The answer being typed (in `Mode::Answer`).
    input: String,
    /// Last outcome to show in the footer (an error, or an "answered X" note).
    status: Option<String>,
    pulse_alive: bool,
    /// Alive worker sessions (the pulse is excluded) — header context only.
    workers_alive: usize,
}

impl App {
    fn new() -> Self {
        Self {
            asks: Vec::new(),
            list_state: ListState::default(),
            mode: Mode::Browse,
            input: String::new(),
            status: None,
            pulse_alive: false,
            workers_alive: 0,
        }
    }

    /// Re-read the pending asks + pulse liveness, keeping the selection valid.
    fn refresh(&mut self, paths: &Paths) {
        self.asks = mailbox::pending(paths);
        self.pulse_alive = run::pulse_running(paths);
        self.workers_alive = session::list_workers(paths)
            .iter()
            .filter(|s| s.alive)
            .count();
        if self.asks.is_empty() {
            self.list_state.select(None);
        } else {
            let sel = self
                .list_state
                .selected()
                .unwrap_or(0)
                .min(self.asks.len() - 1);
            self.list_state.select(Some(sel));
        }
    }

    fn selected(&self) -> Option<&Ask> {
        self.list_state.selected().and_then(|i| self.asks.get(i))
    }

    fn move_selection(&mut self, delta: isize) {
        if self.asks.is_empty() {
            return;
        }
        let cur = self.list_state.selected().unwrap_or(0) as isize;
        let last = self.asks.len() as isize - 1;
        self.list_state
            .select(Some(cur.saturating_add(delta).clamp(0, last) as usize));
    }

    /// Durably resolve the selected ask with the typed text. On success drop
    /// back to Browse and refresh (the answered ask leaves the pending list);
    /// on failure stay in Answer so the human can fix + retry.
    fn submit(&mut self, paths: &Paths) {
        let Some(ask) = self.selected() else {
            self.mode = Mode::Browse;
            return;
        };
        if self.input.trim().is_empty() {
            self.status = Some("answer: empty text (esc to cancel)".into());
            return;
        }
        let id = ask.id.clone();
        match mailbox::answer(paths, &id, &self.input, false) {
            Ok(()) => {
                self.status = Some(format!("answered {id}"));
                self.input.clear();
                self.mode = Mode::Browse;
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
            terminal.draw(|f| self.draw(f))?;

            if !event::poll(TICK)? {
                continue;
            }
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            if ctrl && matches!(key.code, KeyCode::Char('c')) {
                break;
            }
            match self.mode {
                Mode::Browse => match key.code {
                    KeyCode::Char('q') => break,
                    KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
                    KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
                    KeyCode::Enter | KeyCode::Char('a') if self.selected().is_some() => {
                        self.input.clear();
                        self.status = None;
                        self.mode = Mode::Answer;
                    }
                    _ => {}
                },
                Mode::Answer => match key.code {
                    KeyCode::Esc => {
                        self.input.clear();
                        self.mode = Mode::Browse;
                    }
                    KeyCode::Enter => self.submit(paths),
                    KeyCode::Backspace => {
                        self.input.pop();
                    }
                    KeyCode::Char(c) if !ctrl => self.input.push(c),
                    _ => {}
                },
            }
        }
        Ok(())
    }

    fn draw(&mut self, frame: &mut Frame) {
        let chunks = Layout::vertical([
            Constraint::Length(1), // header
            Constraint::Min(3),    // asks | detail
            Constraint::Length(1), // footer
        ])
        .split(frame.area());
        self.draw_header(frame, chunks[0]);

        let body =
            Layout::horizontal([Constraint::Length(26), Constraint::Min(20)]).split(chunks[1]);
        self.draw_asks(frame, body[0]);
        self.draw_detail(frame, body[1]);
        self.draw_footer(frame, chunks[2]);

        if matches!(self.mode, Mode::Answer) {
            self.draw_input(frame, chunks[1]);
        }
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
                self.workers_alive,
                self.asks.len()
            )),
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }

    fn draw_asks(&mut self, frame: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = if self.asks.is_empty() {
            vec![ListItem::new(Line::from(Span::styled(
                " none",
                Style::default().fg(Color::DarkGray),
            )))]
        } else {
            self.asks
                .iter()
                .map(|a| {
                    ListItem::new(Line::from(vec![
                        Span::styled("⚑ ", Style::default().fg(Color::Yellow)),
                        Span::raw(a.id.clone()),
                    ]))
                })
                .collect()
        };
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" asks ")
                    .border_style(Style::default().fg(Color::DarkGray)),
            )
            .highlight_style(Style::default().bg(Color::Rgb(40, 40, 40)));
        frame.render_stateful_widget(list, area, &mut self.list_state);
    }

    fn draw_detail(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" detail ")
            .border_style(Style::default().fg(Color::DarkGray));
        let lines: Vec<Line> = match self.selected() {
            None => vec![
                Line::from(Span::styled(
                    "no pending asks.",
                    Style::default().fg(Color::DarkGray),
                )),
                Line::raw(""),
                Line::from(Span::styled(
                    "looop decides + acts on its own each beat. The one thing it",
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(Span::styled(
                    "defers to you is a worker's blocking question — those appear",
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(Span::styled(
                    "here for you to answer. (An agent concierge does this better;",
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(Span::styled(
                    "see the /looop skill.)",
                    Style::default().fg(Color::DarkGray),
                )),
            ],
            Some(a) => {
                let mut v = vec![
                    Line::from(vec![
                        Span::styled("worker: ", Style::default().fg(Color::DarkGray)),
                        Span::raw(a.worker.clone()),
                    ]),
                    Line::raw(""),
                    Line::raw(a.prompt.clone()),
                ];
                if !a.reference.is_empty() {
                    v.push(Line::raw(""));
                    v.push(Line::from(vec![
                        Span::styled("ref: ", Style::default().fg(Color::DarkGray)),
                        Span::raw(a.reference.clone()),
                    ]));
                }
                if !a.options.is_empty() {
                    v.push(Line::from(vec![
                        Span::styled("options: ", Style::default().fg(Color::DarkGray)),
                        Span::raw(a.options.join(", ")),
                    ]));
                }
                v
            }
        };
        frame.render_widget(
            Paragraph::new(lines)
                .block(block)
                .wrap(Wrap { trim: false }),
            area,
        );
    }

    fn draw_footer(&self, frame: &mut Frame, area: Rect) {
        let style = Style::default().bg(Color::Rgb(40, 40, 40)).fg(Color::White);
        let help = match &self.status {
            Some(msg) => format!(" {msg} "),
            None => match self.mode {
                Mode::Browse => " enter/a answer · ↑/↓ move · q quit ".to_string(),
                Mode::Answer => " type answer · enter submit · esc cancel ".to_string(),
            },
        };
        frame.render_widget(Paragraph::new(Span::styled(help, style)).style(style), area);
    }

    /// Floating single-line answer editor, overlaid at the bottom of the body.
    fn draw_input(&self, frame: &mut Frame, body: Rect) {
        let h = 3.min(body.height);
        let float = Rect {
            x: body.x,
            y: body.bottom().saturating_sub(h),
            width: body.width,
            height: h,
        };
        frame.render_widget(Clear, float);
        let id = self.selected().map(|a| a.id.as_str()).unwrap_or("—");
        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" answer {id} "))
            .border_style(Style::default().fg(Color::Yellow));
        // The field is a single non-wrapping line. Borders eat 2 cols; keep 1
        // for the cursor block. If the answer is longer than the field, show
        // its TAIL (chars, not bytes) so the caret stays visible while typing —
        // horizontal scroll rather than letting text run off the edge.
        let avail = (float.width as usize).saturating_sub(3);
        let chars: Vec<char> = self.input.chars().collect();
        let shown: String = if chars.len() > avail {
            chars[chars.len() - avail..].iter().collect()
        } else {
            self.input.clone()
        };
        // Trailing block cursor so the human sees where they're typing.
        let text = Line::from(vec![
            Span::raw(shown),
            Span::styled(" ", Style::default().bg(Color::White)),
        ]);
        frame.render_widget(Paragraph::new(text).block(block), float);
    }
}
