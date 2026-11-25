//! Minimal ratatui observer UI for the standalone `xai-grok-mcp` binary.

use std::collections::VecDeque;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
};
use tokio::sync::broadcast::error::TryRecvError;
use xai_grok_mcp_server::{
    ApprovalDecision, LocalObserverBridge, ObserverEvent, ObserverEventKind,
};

const MAX_LOG_ENTRIES: usize = 500;

pub struct ObserverStatus {
    pub workspace: String,
    pub endpoint: String,
    pub downstream_servers: usize,
    pub exposed_tools: usize,
}

struct PendingPrompt {
    call_id: String,
    tool_name: String,
    summary: String,
}

struct UiState {
    status: ObserverStatus,
    logs: VecDeque<String>,
    pending: VecDeque<PendingPrompt>,
    scroll_offset: usize,
}

impl UiState {
    fn new(status: ObserverStatus) -> Self {
        Self {
            status,
            logs: VecDeque::with_capacity(MAX_LOG_ENTRIES),
            pending: VecDeque::new(),
            scroll_offset: 0,
        }
    }

    fn push_log(&mut self, line: String) {
        if self.logs.len() >= MAX_LOG_ENTRIES {
            self.logs.pop_front();
        }
        self.logs.push_back(line);
        self.scroll_offset = 0;
    }

    fn handle_event(&mut self, ev: ObserverEvent) {
        let tool = ev.tool_name.as_deref().unwrap_or("-");
        let summary = ev.summary.as_deref().unwrap_or("");
        match ev.kind {
            ObserverEventKind::ClientConnected => self.push_log("[connect] MCP client connected".into()),
            ObserverEventKind::ClientDisconnected => {
                self.push_log("[disconnect] MCP client disconnected".into())
            }
            ObserverEventKind::ToolStarted => {
                self.push_log(format!("[start] {tool} {summary}"))
            }
            ObserverEventKind::ToolFinished => {
                let ms = ev.duration_ms.unwrap_or(0);
                self.push_log(format!("[done] {tool} ({ms}ms)"))
            }
            ObserverEventKind::ToolFailed => {
                self.push_log(format!("[fail] {tool} {summary}"))
            }
            ObserverEventKind::ApprovalRequested => {
                if let Some(call_id) = ev.call_id {
                    let line = format!("[ask] {tool}: {summary}");
                    self.push_log(line);
                    self.pending.push_back(PendingPrompt {
                        call_id,
                        tool_name: tool.to_string(),
                        summary: summary.to_string(),
                    });
                }
            }
            ObserverEventKind::ApprovalResolved => {
                let decision = if ev.allowed.unwrap_or(false) {
                    "allowed"
                } else {
                    "denied"
                };
                self.push_log(format!("[resolved] {tool}: {decision}"));
                if let Some(id) = ev.call_id {
                    self.pending.retain(|p| p.call_id != id);
                }
            }
        }
    }
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

pub async fn run(bridge: Arc<LocalObserverBridge>, status: ObserverStatus) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let _guard = TerminalGuard;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut events_rx = bridge.subscribe();
    let mut state = UiState::new(status);
    let mut should_quit = false;

    while !should_quit {
        loop {
            match events_rx.try_recv() {
                Ok(ev) => state.handle_event(ev),
                Err(TryRecvError::Lagged(n)) => {
                    state.push_log(format!("[warn] skipped {n} observer events"));
                }
                Err(TryRecvError::Empty | TryRecvError::Closed) => break,
            }
        }

        terminal.draw(|frame| render(frame, &state))?;

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c')
                {
                    should_quit = true;
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') => should_quit = true,
                    KeyCode::Char('y') | KeyCode::Enter => {
                        if let Some(prompt) = state.pending.pop_front() {
                            bridge.resolve(&prompt.call_id, ApprovalDecision::AllowOnce);
                        }
                    }
                    KeyCode::Char('n') | KeyCode::Esc => {
                        if let Some(prompt) = state.pending.pop_front() {
                            bridge.resolve(&prompt.call_id, ApprovalDecision::Deny);
                        }
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        state.scroll_offset = state.scroll_offset.saturating_add(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        state.scroll_offset = state.scroll_offset.saturating_sub(1);
                    }
                    _ => {}
                }
            }
        }
    }

    bridge.disconnect();
    Ok(())
}

fn render(frame: &mut ratatui::Frame<'_>, state: &UiState) {
    let approval_height = if state.pending.is_empty() { 0 } else { 5 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Min(5),
            Constraint::Length(approval_height),
            Constraint::Length(1),
        ])
        .split(frame.area());

    let header = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("Endpoint: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(&state.status.endpoint),
        ]),
        Line::from(vec![
            Span::styled("Workspace: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(&state.status.workspace),
            Span::raw(format!(
                "  |  downstream: {}  |  tools: {}",
                state.status.downstream_servers, state.status.exposed_tools
            )),
        ]),
    ])
    .block(Block::default().borders(Borders::ALL).title("xai-grok-mcp"));
    frame.render_widget(header, chunks[0]);

    let visible_rows = chunks[1].height.saturating_sub(2) as usize;
    let total = state.logs.len();
    let max_offset = total.saturating_sub(visible_rows);
    let offset = state.scroll_offset.min(max_offset);
    let end = total.saturating_sub(offset);
    let start = end.saturating_sub(visible_rows);
    let items: Vec<ListItem<'_>> = state
        .logs
        .iter()
        .skip(start)
        .take(end.saturating_sub(start))
        .map(|line| ListItem::new(line.as_str()))
        .collect();
    let list = List::new(items).block(Block::default().borders(Borders::ALL).title("Activity"));
    frame.render_widget(list, chunks[1]);

    if let Some(prompt) = state.pending.front() {
        let box_widget = Paragraph::new(format!(
            "Tool: {}\n{}\n[y/Enter] Allow once   [n/Esc] Deny   ({} pending)",
            prompt.tool_name,
            prompt.summary,
            state.pending.len()
        ))
        .style(Style::default().fg(Color::Yellow))
        .wrap(Wrap { trim: true })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Approval Required"),
        );
        frame.render_widget(box_widget, chunks[2]);
    }

    let footer = Paragraph::new(" q/Ctrl+C: quit  |  Up/Down (j/k): scroll  |  y/n: approve/deny")
        .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(footer, chunks[3]);
}
