//! `tear top` — interactive curses dashboard for the running
//! tear-daemon. Shows live session inventory, per-pane
//! subscriber counts, input policy, and recording state. Polls
//! every `refresh_ms` (default 1000ms).
//!
//! Keys:
//!   q          — quit
//!   r          — refresh immediately
//!   ↑ / ↓ / j / k — move selection
//!   l          — toggle input_policy on selected pane
//!   R          — toggle recording on selected pane
//!   K          — kill selected pane's session
//!
//! Implementation notes
//! --------------------
//! Pure ratatui + crossterm. No async runtime — the polling
//! loop is sync, sleeping between refreshes. Reads from the
//! daemon via a single tear-client connection held for the
//! duration of the program. On disconnect the dashboard exits
//! gracefully.

use std::io;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};
use tear_client::Client;
use tear_types::{InputPolicy, MultiplexerControl, PaneId, TearPane, TearSession};

pub fn run(client: Client, refresh_ms: u64) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_loop(&mut terminal, &client, refresh_ms);

    // Clean up regardless of outcome.
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    result
}

// ratatui 0.30 made Backend::Error an associated type; anyhow's `?`
// needs it Send + Sync + 'static (trivially true for the crossterm
// backend's io::Error — the bound just names it).
fn run_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    client: &Client,
    refresh_ms: u64,
) -> Result<()>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let mut state = DashboardState::default();
    let mut last_refresh = Instant::now() - Duration::from_secs(60);
    let refresh = Duration::from_millis(refresh_ms);

    loop {
        if last_refresh.elapsed() >= refresh {
            state.refresh(client);
            last_refresh = Instant::now();
        }
        terminal.draw(|f| draw(f, &mut state))?;

        // Block for events up to the next refresh tick.
        let until_next = refresh.saturating_sub(last_refresh.elapsed());
        if event::poll(until_next.min(Duration::from_millis(200)))? {
            if let Event::Key(key) = event::read()? {
                if !handle_key(key, &mut state, client) {
                    return Ok(());
                }
                // Re-render on every key.
                continue;
            }
        }
    }
}

/// Returns `false` when the user wants to quit.
fn handle_key(key: KeyEvent, state: &mut DashboardState, client: &Client) -> bool {
    if key.kind != KeyEventKind::Press {
        return true;
    }
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return false,
        KeyCode::Char('r') => state.refresh(client),
        KeyCode::Down | KeyCode::Char('j') => state.move_down(),
        KeyCode::Up | KeyCode::Char('k') => state.move_up(),
        KeyCode::Char('l') => state.toggle_input_policy(client),
        KeyCode::Char('R') => state.toggle_recording(client),
        KeyCode::Char('K') => state.kill_selected_session(client),
        _ => {}
    }
    true
}

#[derive(Default)]
struct DashboardState {
    rows: Vec<Row>,
    list_state: ListState,
    status: String,
    last_error: Option<String>,
}

struct Row {
    session: TearSession,
    pane: TearPane,
    subscribers: u32,
    recording_enabled: bool,
    recording_events: u32,
}

impl DashboardState {
    fn refresh(&mut self, client: &Client) {
        let sessions = match client.list_sessions() {
            Ok(s) => s,
            Err(e) => {
                self.last_error = Some(format!("list_sessions: {e}"));
                self.rows.clear();
                return;
            }
        };
        let mut rows = Vec::new();
        for session in sessions {
            for (pane_id, pane) in &session.panes {
                let subscribers = client.pane_subscriber_count(*pane_id).unwrap_or(0);
                let (recording_enabled, recording_events) =
                    client.pane_recording_status(*pane_id).unwrap_or((false, 0));
                rows.push(Row {
                    session: session.clone(),
                    pane: pane.clone(),
                    subscribers,
                    recording_enabled,
                    recording_events,
                });
            }
        }
        self.rows = rows;
        self.last_error = None;
        if self.list_state.selected().is_none() && !self.rows.is_empty() {
            self.list_state.select(Some(0));
        } else if let Some(sel) = self.list_state.selected() {
            if sel >= self.rows.len() {
                self.list_state.select(self.rows.len().checked_sub(1));
            }
        }
        self.status = format!(
            "{} session(s)  {} pane(s)  refreshed {}",
            self.rows
                .iter()
                .map(|r| r.session.id)
                .fold(std::collections::BTreeSet::new(), |mut acc, id| {
                    acc.insert(id);
                    acc
                },)
                .len(),
            self.rows.len(),
            chrono_like_now(),
        );
    }

    fn move_down(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let next = self
            .list_state
            .selected()
            .map(|i| (i + 1) % self.rows.len())
            .unwrap_or(0);
        self.list_state.select(Some(next));
    }

    fn move_up(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let next = self
            .list_state
            .selected()
            .map(|i| if i == 0 { self.rows.len() - 1 } else { i - 1 })
            .unwrap_or(0);
        self.list_state.select(Some(next));
    }

    fn selected_pane(&self) -> Option<PaneId> {
        self.list_state
            .selected()
            .and_then(|i| self.rows.get(i))
            .map(|r| r.pane.id)
    }

    fn toggle_input_policy(&mut self, client: &Client) {
        let Some(pane_id) = self.selected_pane() else {
            return;
        };
        let current = self
            .rows
            .iter()
            .find(|r| r.pane.id == pane_id)
            .map(|r| r.pane.input_policy)
            .unwrap_or(InputPolicy::Free);
        let new = match current {
            InputPolicy::Free => InputPolicy::Locked,
            InputPolicy::Locked => InputPolicy::Free,
            // Leader policy is operator-set with an explicit id; the
            // dashboard's `t` toggle returns the pane to Free rather
            // than guess a leader.
            InputPolicy::Leader { .. } => InputPolicy::Free,
        };
        if let Err(e) = client.set_input_policy(pane_id, new) {
            self.last_error = Some(format!("set_input_policy: {e}"));
        }
        self.refresh(client);
    }

    fn toggle_recording(&mut self, client: &Client) {
        let Some(pane_id) = self.selected_pane() else {
            return;
        };
        let enabled = self
            .rows
            .iter()
            .find(|r| r.pane.id == pane_id)
            .map(|r| r.recording_enabled)
            .unwrap_or(false);
        let result = if enabled {
            client.stop_pane_recording(pane_id)
        } else {
            client.start_pane_recording(pane_id)
        };
        if let Err(e) = result {
            self.last_error = Some(format!("toggle recording: {e}"));
        }
        self.refresh(client);
    }

    fn kill_selected_session(&mut self, client: &Client) {
        let Some(sel) = self.list_state.selected() else {
            return;
        };
        let Some(row) = self.rows.get(sel) else {
            return;
        };
        let sid = row.session.id;
        if let Err(e) = client.kill_session(sid) {
            self.last_error = Some(format!("kill_session: {e}"));
        }
        self.refresh(client);
    }
}

fn draw(f: &mut ratatui::Frame, state: &mut DashboardState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header
            Constraint::Min(0),    // list
            Constraint::Length(3), // footer
        ])
        .split(f.area());

    // ── header ────────────────────────────────────────────
    let mut header_text = vec![Line::from(vec![
        Span::styled(
            "tear top",
            Style::default()
                .add_modifier(Modifier::BOLD)
                .fg(Color::Cyan),
        ),
        Span::raw("  "),
        Span::raw(&state.status),
    ])];
    if let Some(err) = &state.last_error {
        header_text.push(Line::from(Span::styled(
            err.clone(),
            Style::default().fg(Color::Red),
        )));
    }
    let header = Paragraph::new(header_text).block(Block::default().borders(Borders::BOTTOM));
    f.render_widget(header, chunks[0]);

    // ── list ──────────────────────────────────────────────
    let items: Vec<ListItem> = state
        .rows
        .iter()
        .map(|r| {
            let line = Line::from(vec![
                Span::styled(
                    format!("{}", r.session.id),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw("  "),
                Span::styled(
                    format!("{:<20}", truncate(&r.session.name, 20)),
                    Style::default().fg(Color::Cyan),
                ),
                Span::raw("  "),
                Span::styled(
                    format!("{}", r.pane.id),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw("  "),
                Span::raw(format!("{}x{}", r.pane.size_cells.0, r.pane.size_cells.1)),
                Span::raw("  policy="),
                policy_span(r.pane.input_policy),
                Span::raw("  subs="),
                Span::raw(format!("{}", r.subscribers)),
                Span::raw("  rec="),
                recording_span(r.recording_enabled, r.recording_events),
                Span::raw("  src="),
                source_span(&r.session.source),
            ]);
            ListItem::new(line)
        })
        .collect();
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" sessions / panes "),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, chunks[1], &mut state.list_state);

    // ── footer ────────────────────────────────────────────
    let footer = Paragraph::new(Line::from(vec![
        Span::styled("q", Style::default().fg(Color::Yellow)),
        Span::raw(" quit  "),
        Span::styled("r", Style::default().fg(Color::Yellow)),
        Span::raw(" refresh  "),
        Span::styled("↑↓/jk", Style::default().fg(Color::Yellow)),
        Span::raw(" select  "),
        Span::styled("l", Style::default().fg(Color::Yellow)),
        Span::raw(" toggle lock  "),
        Span::styled("R", Style::default().fg(Color::Yellow)),
        Span::raw(" toggle record  "),
        Span::styled("K", Style::default().fg(Color::Yellow)),
        Span::raw(" kill session"),
    ]))
    .block(Block::default().borders(Borders::TOP));
    f.render_widget(footer, chunks[2]);
}

fn policy_span(p: InputPolicy) -> Span<'static> {
    match p {
        InputPolicy::Free => Span::styled("free", Style::default().fg(Color::Green)),
        InputPolicy::Locked => Span::styled("locked", Style::default().fg(Color::Red)),
        InputPolicy::Leader { id } => {
            Span::styled(format!("leader({id})"), Style::default().fg(Color::Yellow))
        }
    }
}

fn recording_span(enabled: bool, events: u32) -> Span<'static> {
    if enabled {
        Span::styled(
            format!("on({events})"),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(
            format!("off({events})"),
            Style::default().fg(Color::DarkGray),
        )
    }
}

fn source_span(src: &tear_types::SessionSource) -> Span<'static> {
    use tear_types::SessionSource;
    match src {
        SessionSource::Human => Span::styled("human", Style::default().fg(Color::Cyan)),
        SessionSource::Agent => Span::styled("agent", Style::default().fg(Color::Magenta)),
        SessionSource::Named(s) => {
            Span::styled(format!("named:{s}"), Style::default().fg(Color::LightBlue))
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max - 1).collect();
        t.push('…');
        t
    }
}

/// Mini ISO-8601-ish timestamp without pulling in chrono.
fn chrono_like_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let secs = now % 60;
    let mins = (now / 60) % 60;
    let hours = (now / 3600) % 24;
    format!("{hours:02}:{mins:02}:{secs:02}")
}
