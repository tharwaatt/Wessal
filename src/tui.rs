// src/tui.rs — Wessal Code Review Interface
//
// Layout:
//   ┌─────────────────┬─────────────────────────────────┐
//   │ Project Overview│                                 │
//   ├─────────────────│        Diff / Output Viewer     │
//   │  Actions Log    │                                 │
//   └─────────────────┴─────────────────────────────────┘
//
// Keybindings:
//   ↑ / k          — previous action in list (left panel)
//   ↓ / j          — next action in list     (left panel)
//   g / Home       — jump to first action
//   G / End        — jump to last action
//   PageDown / s   — scroll diff DOWN        (right panel)
//   PageUp   / w   — scroll diff UP          (right panel)
//   q / Esc        — quit
//
// Scrolling notes
// ───────────────
// ratatui's Paragraph::scroll((row, col)) scrolls by SOURCE LINES when Wrap is
// disabled, which is exactly what we want for a diff viewer.  We deliberately
// do NOT enable Wrap on the diff panel so that long diff lines are truncated
// instead of reflowed — this keeps the scroll offset stable and predictable.
//
// diff_scroll_offset is reset to 0 whenever the user selects a different action.

use std::{
    collections::VecDeque,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Frame, Terminal,
};

// ══════════════════════════════════════════════════════════════════════════════
// Public event types
// ══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
pub enum TuiEvent {
    SessionId      { id: String },
    Connected      { url: String, llm: String },
    Disconnected,
    ContextPushed  { tokens: usize, files: usize },
    ActionExecuted { summary: ActionSummary },
    FileCount      { count: usize },
}

#[derive(Debug, Clone)]
pub struct ActionSummary {
    pub time:   String,
    pub kind:   ActionKind,
    pub target: String,
    pub status: ActionStatus,
    pub detail: String,
    pub output: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionKind { Read, Write, List, Context }

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionStatus { Ok, Error }

// ══════════════════════════════════════════════════════════════════════════════
// Internal TUI state
// ══════════════════════════════════════════════════════════════════════════════

struct State {
    session_id:          String,
    url:                 Option<String>,
    llm:                 String,
    file_count:          usize,
    actions:             VecDeque<ActionSummary>,
    started:             Instant,
    reconnects:          u32,
    spin:                u8,
    /// Vertical scroll offset for the right-panel diff/output viewer.
    /// Unit: source lines (Wrap is disabled on the diff panel).
    /// Reset to 0 whenever a different action is selected.
    diff_scroll_offset:  u16,
}

impl State {
    fn new() -> Self {
        Self {
            session_id:         String::new(),
            url:                None,
            llm:                String::new(),
            file_count:         0,
            actions:            VecDeque::with_capacity(200),
            started:            Instant::now(),
            reconnects:         0,
            spin:               0,
            diff_scroll_offset: 0,
        }
    }

    fn push(&mut self, a: ActionSummary) {
        if self.actions.len() >= 200 { self.actions.pop_front(); }
        self.actions.push_back(a);
    }

    fn write_count(&self) -> usize {
        self.actions.iter().filter(|a| a.kind == ActionKind::Write).count()
    }
    fn read_count(&self) -> usize {
        self.actions.iter().filter(|a| a.kind == ActionKind::Read).count()
    }

    fn apply(&mut self, ev: TuiEvent) {
        match ev {
            TuiEvent::SessionId { id } => self.session_id = id,
            TuiEvent::Connected { url, llm } => {
                if self.url.is_some() { self.reconnects += 1; }
                self.url = Some(url);
                self.llm = llm;
            }
            TuiEvent::Disconnected => self.url = None,
            TuiEvent::ContextPushed { tokens, files } => {
                self.push(ActionSummary {
                    time:   hms(),
                    kind:   ActionKind::Context,
                    target: "context".into(),
                    status: ActionStatus::Ok,
                    detail: format!("{}k tok · {} files", tokens / 1000, files),
                    output: String::new(),
                });
                // New action arrived — reset scroll.
                self.diff_scroll_offset = 0;
            }
            TuiEvent::ActionExecuted { summary } => {
                self.push(summary);
                // New action arrived — always reset scroll so the user sees the top.
                self.diff_scroll_offset = 0;
            }
            TuiEvent::FileCount { count } => self.file_count = count,
        }
    }

    fn uptime(&self) -> String {
        let s = self.started.elapsed().as_secs();
        format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    }

    /// How many lines does the currently-selected action's output have?
    /// Used to clamp the scroll offset so we never scroll into blank space.
    fn selected_output_lines(&self, ls: &ListState) -> u16 {
        ls.selected()
            .and_then(|i| self.actions.get(i))
            .map(|a| a.output.lines().count() as u16)
            .unwrap_or(0)
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// TuiHandle — sender used from main.rs
// ══════════════════════════════════════════════════════════════════════════════

#[derive(Clone)]
pub struct TuiHandle { tx: std::sync::mpsc::Sender<TuiEvent> }

impl TuiHandle {
    pub fn send(&self, ev: TuiEvent) { let _ = self.tx.send(ev); }
}

pub fn launch() -> TuiHandle {
    let (tx, rx) = std::sync::mpsc::channel::<TuiEvent>();
    std::thread::spawn(move || {
        if let Err(e) = run(rx) { eprintln!("[Wessal TUI] {e}"); }
    });
    TuiHandle { tx }
}

// ══════════════════════════════════════════════════════════════════════════════
// Main render loop
// ══════════════════════════════════════════════════════════════════════════════

const SCROLL_STEP: u16 = 10;

fn run(rx: std::sync::mpsc::Receiver<TuiEvent>) -> std::io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;

    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let mut st       = State::new();
    let mut ls       = ListState::default();
    let tick         = Duration::from_millis(80);

    loop {
        // Drain all pending events from main thread.
        loop {
            match rx.try_recv() {
                Ok(ev) => st.apply(ev),
                Err(std::sync::mpsc::TryRecvError::Empty)        => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    return teardown(&mut terminal);
                }
            }
        }

        st.spin = (st.spin + 1) % 8;

        // Auto-follow the latest action (stay at bottom of list).
        let at_bottom = ls.selected().map(|i| i + 1 >= st.actions.len()).unwrap_or(true);
        if at_bottom && !st.actions.is_empty() {
            ls.select(Some(st.actions.len() - 1));
        }

        terminal.draw(|f| root(f, &st, &mut ls))?;

        if event::poll(tick)? {
            if let Event::Key(k) = event::read()? {
                match k.code {
                    // ── Quit ─────────────────────────────────────────────────
                    KeyCode::Char('q') | KeyCode::Esc => {
                        return teardown(&mut terminal);
                    }
                    KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        return teardown(&mut terminal);
                    }

                    // ── Navigate Action List (left panel) ────────────────────
                    KeyCode::Up | KeyCode::Char('k') => {
                        let i = ls.selected().unwrap_or(0);
                        ls.select(Some(i.saturating_sub(1)));
                        st.diff_scroll_offset = 0;
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        let max = st.actions.len().saturating_sub(1);
                        let i   = ls.selected().unwrap_or(max);
                        ls.select(Some((i + 1).min(max)));
                        st.diff_scroll_offset = 0;
                    }
                    KeyCode::Home | KeyCode::Char('g') => {
                        if !st.actions.is_empty() {
                            ls.select(Some(0));
                            st.diff_scroll_offset = 0;
                        }
                    }
                    KeyCode::End | KeyCode::Char('G') => {
                        if !st.actions.is_empty() {
                            ls.select(Some(st.actions.len() - 1));
                            st.diff_scroll_offset = 0;
                        }
                    }

                    // ── Scroll Diff Panel (right panel) ──────────────────────
                    KeyCode::PageDown | KeyCode::Char('s') => {
                        let lines = st.selected_output_lines(&ls);
                        let max   = lines.saturating_sub(5);
                        st.diff_scroll_offset =
                            st.diff_scroll_offset.saturating_add(SCROLL_STEP).min(max);
                    }
                    KeyCode::Char('d') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        let lines = st.selected_output_lines(&ls);
                        let max   = lines.saturating_sub(5);
                        st.diff_scroll_offset =
                            st.diff_scroll_offset.saturating_add(SCROLL_STEP).min(max);
                    }
                    KeyCode::PageUp | KeyCode::Char('w') => {
                        st.diff_scroll_offset =
                            st.diff_scroll_offset.saturating_sub(SCROLL_STEP);
                    }
                    KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        st.diff_scroll_offset =
                            st.diff_scroll_offset.saturating_sub(SCROLL_STEP);
                    }

                    _ => {}
                }
            }
        }
    }
}

fn teardown(t: &mut Terminal<CrosstermBackend<std::io::Stdout>>) -> std::io::Result<()> {
    disable_raw_mode()?;
    execute!(t.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    t.show_cursor()
}

// ══════════════════════════════════════════════════════════════════════════════
// Root layout
// ══════════════════════════════════════════════════════════════════════════════

fn root(f: &mut Frame, st: &State, ls: &mut ListState) {
    let area = f.area();
    let v = Layout::vertical([
        Constraint::Length(1),  // title bar
        Constraint::Min(0),     // body
        Constraint::Length(1),  // status bar
    ]).split(area);

    draw_titlebar(f, v[0], st);
    draw_body(f, v[1], st, ls);
    draw_statusbar(f, v[2], st);
}

// ══════════════════════════════════════════════════════════════════════════════
// Title bar (1 line)
// ══════════════════════════════════════════════════════════════════════════════

const SPIN: [char; 8] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧'];

fn draw_titlebar(f: &mut Frame, area: Rect, st: &State) {
    // Explicit type annotation to fix inference error
    let sid: String = if st.session_id.is_empty() {
        "—".to_string()
    } else {
        st.session_id.chars().take(8).collect()
    };

    let spn = Span::styled(
        format!("  {} ", SPIN[st.spin as usize]),
        Style::default().fg(Color::Rgb(50, 80, 120)),
    );

    let spans = vec![
        Span::styled("  ⚡ WESSAL", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(format!("  s:{sid}"), Style::default().fg(Color::Rgb(55, 55, 85))),
        spn,
    ];

    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::Rgb(8, 8, 18))),
        area,
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Body — Left Column (Overview + Actions) / Right Column (Diff Viewer)
// ══════════════════════════════════════════════════════════════════════════════

fn draw_body(f: &mut Frame, area: Rect, st: &State, ls: &mut ListState) {
    let cols = Layout::horizontal([
        Constraint::Percentage(36),
        Constraint::Percentage(64),
    ]).split(area);

    let left_v = Layout::vertical([
        Constraint::Length(6), // Project Overview
        Constraint::Min(0),    // Actions Log
    ]).split(cols[0]);

    draw_overview(f, left_v[0], st);
    draw_action_log(f, left_v[1], st, ls);
    draw_diff_panel(f, cols[1], st, ls);
}

// ── Project Overview ──────────────────────────────────────────────────────────

fn draw_overview(f: &mut Frame, area: Rect, st: &State) {
    let conn = if let Some(url) = &st.url {
        Span::styled(
            format!("● Connected ({})", url_host(url)),
            Style::default().fg(Color::Green),
        )
    } else {
        Span::styled("○ Disconnected", Style::default().fg(Color::Red))
    };

    let model_display = if st.llm.is_empty() { "—" } else { &st.llm };

    let lines = vec![
        Line::from(vec![
            Span::styled(" Status:   ", Style::default().fg(Color::Rgb(100, 100, 120))),
            conn,
        ]),
        Line::from(vec![
            Span::styled(" Files:    ", Style::default().fg(Color::Rgb(100, 100, 120))),
            Span::styled(st.file_count.to_string(), Style::default().fg(Color::Cyan)),
        ]),
        Line::from(vec![
            Span::styled(" Model:    ", Style::default().fg(Color::Rgb(100, 100, 120))),
            Span::styled(model_display, Style::default().fg(Color::Yellow)),
        ]),
        Line::from(vec![
            Span::styled(" Uptime:   ", Style::default().fg(Color::Rgb(100, 100, 120))),
            Span::styled(st.uptime(), Style::default().fg(Color::Rgb(180, 180, 200))),
        ]),
    ];

    let block = Block::default()
        .title(Span::styled(
            " Project Overview ",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BORDER));

    f.render_widget(Paragraph::new(lines).block(block), area);
}

// ── Action log ────────────────────────────────────────────────────────────────

fn draw_action_log(f: &mut Frame, area: Rect, st: &State, ls: &mut ListState) {
    let n = st.actions.len();
    let w = area.width.saturating_sub(4) as usize;

    let items: Vec<ListItem> = st.actions.iter().map(|a| {
        let (icon, ic) = icon_color(a);
        let time: String = a.time.chars().take(8).collect();
        let kind       = pad(kind_label(&a.kind).to_string(), 5);
        let target_w   = w.saturating_sub(8 + 2 + 6 + 2);
        let target     = clip(&a.target, target_w);

        ListItem::new(Line::from(vec![
            Span::styled(format!("{time} "), Style::default().fg(Color::Rgb(60, 60, 80))),
            Span::styled(format!("{icon} "), Style::default().fg(ic).add_modifier(Modifier::BOLD)),
            Span::styled(format!("{kind} "), Style::default().fg(Color::Rgb(80, 90, 115))),
            Span::styled(target, Style::default().fg(Color::Rgb(195, 200, 220))),
        ]))
    }).collect();

    let block = Block::default()
        .title(Line::from(vec![
            Span::styled(" Actions ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::styled(format!("({n}) "), Style::default().fg(Color::Rgb(45, 50, 72))),
        ]))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BORDER));

    let list = List::new(items)
        .block(block)
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(18, 30, 52))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    f.render_stateful_widget(list, area, ls);
}

// ══════════════════════════════════════════════════════════════════════════════
// Diff panel — the main code review surface
// ══════════════════════════════════════════════════════════════════════════════

fn draw_diff_panel(f: &mut Frame, area: Rect, st: &State, ls: &ListState) {
    let selected = ls.selected().and_then(|i| st.actions.get(i));

    match selected {
        None                                      => draw_welcome(f, area, st),
        Some(a) if a.output.is_empty()            => draw_empty_output(f, area, a),
        Some(a) if is_unified_diff(&a.output)     => draw_unified_diff(f, area, a, st),
        Some(a)                                   => draw_plain_output(f, area, a, st),
    }
}

// ── Welcome screen (no action selected) ──────────────────────────────────────

fn draw_welcome(f: &mut Frame, area: Rect, st: &State) {
    let lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "  ⚡ Wessal — Local File Bridge",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            "  How to use",
            Style::default().fg(Color::Rgb(180, 185, 210)).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "  ──────────────────────────────────────────",
            Style::default().fg(Color::Rgb(35, 38, 58)),
        )),
        Line::from(Span::styled(
            "  1. Open ChatGPT / Claude / Gemini / AI Studio / Kimi / ChatGLM",
            Style::default().fg(Color::Rgb(130, 138, 165)),
        )),
        Line::from(Span::styled(
            "  2. Ask the AI to read or write a file",
            Style::default().fg(Color::Rgb(130, 138, 165)),
        )),
        Line::from(Span::styled(
            "  3. Click ⚡ in the page corner when you see action tags",
            Style::default().fg(Color::Rgb(130, 138, 165)),
        )),
        Line::from(Span::styled(
            "  4. Review the verified diff here, then continue the conversation",
            Style::default().fg(Color::Rgb(130, 138, 165)),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            "  Keybindings",
            Style::default().fg(Color::Rgb(180, 185, 210)).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "  ──────────────────────────────────────────",
            Style::default().fg(Color::Rgb(35, 38, 58)),
        )),
        Line::from(Span::styled(
            "  ↑ / k          previous action",
            Style::default().fg(Color::Rgb(100, 108, 135)),
        )),
        Line::from(Span::styled(
            "  ↓ / j          next action",
            Style::default().fg(Color::Rgb(100, 108, 135)),
        )),
        Line::from(Span::styled(
            "  PgDown / s     scroll diff down",
            Style::default().fg(Color::Rgb(100, 108, 135)),
        )),
        Line::from(Span::styled(
            "  PgUp   / w     scroll diff up",
            Style::default().fg(Color::Rgb(100, 108, 135)),
        )),
        Line::from(Span::styled(
            "  g / G          jump to first / last",
            Style::default().fg(Color::Rgb(100, 108, 135)),
        )),
        Line::from(Span::styled(
            "  q / Esc        quit",
            Style::default().fg(Color::Rgb(100, 108, 135)),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            format!("  Uptime: {}", st.uptime()),
            Style::default().fg(Color::Rgb(55, 58, 78)),
        )),
    ];

    let block = Block::default()
        .title(Span::styled(
            " Wessal ",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BORDER));

    f.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
}

// ── Empty output (action ran but produced no diff text) ───────────────────────

fn draw_empty_output(f: &mut Frame, area: Rect, a: &ActionSummary) {
    let title = format!(" {} · {} ", kind_label(&a.kind), clip(&a.target, 40));
    let msg   = match a.status {
        ActionStatus::Error => "✗  Error — check daemon logs for details.",
        ActionStatus::Ok    => match a.kind {
            ActionKind::Write   => "✓  New file created (no previous content to diff).",
            ActionKind::Read    => "✓  Content sent to browser input field.",
            ActionKind::List    => "✓  Directory listing sent to browser.",
            ActionKind::Context => "✓  Context pushed to browser.",
        },
    };
    let color = match a.status {
        ActionStatus::Ok    => Color::Rgb(65, 175, 80),
        ActionStatus::Error => Color::Rgb(210, 60, 60),
    };

    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("\n  {msg}"),
            Style::default().fg(color),
        )))
        .block(styled_diff_block(&title, a)),
        area,
    );
}

// ── Plain text output (search results, outlines, directory listings) ──────────

fn draw_plain_output(f: &mut Frame, area: Rect, a: &ActionSummary, st: &State) {
    let title   = format!(" {} · {} ", kind_label(&a.kind), clip(&a.target, 40));
    let inner_w = area.width.saturating_sub(4) as usize;

    let lines: Vec<Line> = a.output.lines().map(|l| {
        Line::from(Span::styled(
            clip(l, inner_w),
            Style::default().fg(Color::Rgb(180, 188, 210)),
        ))
    }).collect();

    f.render_widget(
        Paragraph::new(Text::from(lines))
            .block(styled_diff_block(&title, a))
            .wrap(Wrap { trim: false })
            .scroll((st.diff_scroll_offset, 0)),
        area,
    );
}

// ── Unified diff viewer ───────────────────────────────────────────────────────

fn draw_unified_diff(f: &mut Frame, area: Rect, a: &ActionSummary, st: &State) {
    let inner_w = area.width.saturating_sub(2) as usize;

    let mut lines: Vec<Line> = a.output
        .lines()
        .map(|l| render_diff_line(l, inner_w))
        .collect();

    lines.push(Line::from(Span::styled(
        "  [END OF DIFF]",
        Style::default()
            .fg(Color::Rgb(75, 80, 105))
            .add_modifier(Modifier::ITALIC),
    )));

    let scroll_hint = if st.diff_scroll_offset > 0 {
        format!(" ↕L{} ", st.diff_scroll_offset)
    } else {
        String::new()
    };
    let title = format!(" ✎ diff · {}{} ", clip(&a.target, 38), scroll_hint);

    let block = Block::default()
        .title(Span::styled(
            title,
            Style::default()
                .fg(Color::Rgb(65, 185, 85))
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Rgb(25, 60, 35)));

    f.render_widget(
        Paragraph::new(Text::from(lines))
            .block(block)
            .scroll((st.diff_scroll_offset, 0)),
        area,
    );
}

// ── Per-line diff styling ─────────────────────────────────────────────────────

fn render_diff_line(line: &str, max_width: usize) -> Line<'static> {
    let display = clip(line, max_width.saturating_sub(1));

    if display.starts_with("---") || display.starts_with("+++") {
        return Line::from(Span::styled(
            display,
            Style::default().fg(Color::Rgb(115, 118, 148)),
        ));
    }

    if display.starts_with("@@") {
        return Line::from(Span::styled(
            display,
            Style::default()
                .fg(Color::Rgb(100, 195, 225))
                .bg(Color::Rgb(10, 32, 60))
                .add_modifier(Modifier::BOLD),
        ));
    }

    if display.starts_with('+') {
        return Line::from(vec![
            Span::styled(
                "+".to_string(),
                Style::default()
                    .fg(Color::Rgb(85, 215, 110))
                    .bg(Color::Rgb(10, 52, 25))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                display.chars().skip(1).collect::<String>(),
                Style::default()
                    .fg(Color::Rgb(185, 242, 198))
                    .bg(Color::Rgb(10, 52, 25)),
            ),
        ]);
    }

    if display.starts_with('-') {
        return Line::from(vec![
            Span::styled(
                "-".to_string(),
                Style::default()
                    .fg(Color::Rgb(220, 65, 65))
                    .bg(Color::Rgb(62, 12, 12))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                display.chars().skip(1).collect::<String>(),
                Style::default()
                    .fg(Color::Rgb(242, 185, 185))
                    .bg(Color::Rgb(62, 12, 12)),
            ),
        ]);
    }

    Line::from(Span::styled(
        display,
        Style::default().fg(Color::Rgb(150, 155, 178)),
    ))
}

// ══════════════════════════════════════════════════════════════════════════════
// Status bar (1 line)
// ══════════════════════════════════════════════════════════════════════════════

fn draw_statusbar(f: &mut Frame, area: Rect, st: &State) {
    let line = Line::from(vec![
        Span::styled(
            format!(" {} acts · {}w {}r ", st.actions.len(), st.write_count(), st.read_count()),
            Style::default().fg(Color::Rgb(48, 55, 75)),
        ),
        Span::styled(
            "  ·  ↑↓/jk list  ·  PgUp/PgDn or w/s scroll diff  ·  q quit",
            Style::default().fg(Color::Rgb(32, 35, 52)),
        ),
    ]);
    f.render_widget(
        Paragraph::new(line).style(Style::default().bg(Color::Rgb(5, 5, 14))),
        area,
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Style & Text helpers
// ══════════════════════════════════════════════════════════════════════════════

const BORDER: Color = Color::Rgb(25, 38, 65);

fn styled_diff_block(title: &str, a: &ActionSummary) -> Block<'static> {
    let title_color = match a.status {
        ActionStatus::Ok    => Color::Rgb(95, 160, 215),
        ActionStatus::Error => Color::Rgb(210, 65, 65),
    };
    Block::default()
        .title(Span::styled(
            title.to_string(),
            Style::default().fg(title_color).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BORDER))
}

fn icon_color(a: &ActionSummary) -> (&'static str, Color) {
    match (&a.kind, &a.status) {
        (_, ActionStatus::Error)  => ("✗", Color::Rgb(210, 55, 55)),
        (ActionKind::Read,    _)  => ("↓", Color::Rgb(65, 125, 205)),
        (ActionKind::Write,   _)  => ("✎", Color::Rgb(65, 185, 75)),
        (ActionKind::List,    _)  => ("▤", Color::Rgb(65, 175, 175)),
        (ActionKind::Context, _)  => ("◉", Color::Rgb(95, 95, 185)),
    }
}

fn kind_label(k: &ActionKind) -> &'static str {
    match k {
        ActionKind::Read    => "read",
        ActionKind::Write   => "write",
        ActionKind::List    => "ls",
        ActionKind::Context => "ctx",
    }
}

fn is_unified_diff(text: &str) -> bool {
    text.lines().any(|l| l.starts_with("@@") || l.starts_with("+++") || l.starts_with("---"))
}

/// Safely clip a string to a maximum number of **characters** (not bytes).
pub fn clip(s: &str, max: usize) -> String {
    if max == 0 { return String::new(); }
    
    let chars: Vec<char> = s.chars().take(max).collect();
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut result: String = chars.into_iter().collect();
        if !result.is_empty() {
            result.pop();
            result.push('…');
        }
        result
    }
}

/// Pad a string to a fixed width (in characters), clipping if necessary.
pub fn pad(s: String, width: usize) -> String {
    let char_count = s.chars().count();
    if char_count >= width { 
        clip(&s, width) 
    } else { 
        // Add spaces to reach the desired width
        let spaces = width - char_count;
        format!("{}{}", s, " ".repeat(spaces))
    }
}

pub fn url_host(url: &str) -> &str {
    url.trim_start_matches("https://")
       .trim_start_matches("http://")
       .split('/')
       .next()
       .unwrap_or(url)
}

fn hms() -> String {
    let s = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{:02}:{:02}:{:02}", (s / 3600) % 24, (s / 60) % 60, s % 60)
}

// ══════════════════════════════════════════════════════════════════════════════
// Unit tests
// ══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_ascii() {
        assert_eq!(clip("hello", 10), "hello");
        assert_eq!(clip("hello world", 5), "hell…");
        assert_eq!(clip("hi", 2), "hi");
        assert_eq!(clip("", 5), "");
    }

    #[test]
    fn clip_utf8() {
        assert_eq!(clip("مرحبا", 3), "مر…");
        assert_eq!(clip("😀😁😂🤣", 2), "😀…");
        assert_eq!(clip("abcمرحبا", 5), "abcم…");
    }

    #[test]
    fn clip_edge_cases() {
        assert_eq!(clip("test", 0), "");
        assert_eq!(clip("", 0), "");
        assert_eq!(clip("x", 1), "x");
        assert_eq!(clip("xy", 1), "…");
    }

    #[test]
    fn pad_ascii() {
        assert_eq!(pad("hi".to_string(), 5), "hi   ");
        assert_eq!(pad("hello".to_string(), 3), "he…");
    }

    #[test]
    fn pad_utf8() {
        let result = pad("مرحبا".to_string(), 7);
        assert_eq!(result.chars().count(), 7);
    }
}