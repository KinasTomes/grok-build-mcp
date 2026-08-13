//! Remote-MCP observer mode hosted by the existing Grok pager binary.
//!
//! This is intentionally a small pager surface, not an agent session: it owns
//! no prompt widget, no `AcpSession`, and no model/provider. It subscribes to
//! the gateway's bounded event bus through `LocalObserverBridge` and resolves
//! only that bridge's pending `AllowOnce`/`Deny` requests.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol as acp;
use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};
use xai_grok_mcp_server::{
    ApprovalDecision, LocalObserverBridge, ObserverEvent, ObserverEventKind,
};
use xai_grok_pager_render::theme::Theme;

#[derive(Debug, Clone)]
pub struct GatewayObserverStatus {
    pub workspace: String,
    pub endpoint: String,
    pub downstream_servers: usize,
    pub exposed_tools: usize,
}

struct ObserverState {
    status: GatewayObserverStatus,
    pending: VecDeque<ObserverEvent>,
    clients: usize,
    tool_calls: VecDeque<ToolCall>,
    by_call_id: HashMap<String, usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolStatus {
    Running,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone)]
struct ToolCall {
    call_id: String,
    tool_name: String,
    status: ToolStatus,
    duration_ms: Option<u128>,
}

impl ObserverState {
    fn new(status: GatewayObserverStatus) -> Self {
        Self {
            status,
            pending: VecDeque::new(),
            clients: 0,
            tool_calls: VecDeque::with_capacity(128),
            by_call_id: HashMap::new(),
        }
    }

    fn push(&mut self, event: ObserverEvent) {
        match event.kind {
            ObserverEventKind::ClientConnected => self.clients = self.clients.saturating_add(1),
            ObserverEventKind::ClientDisconnected => self.clients = self.clients.saturating_sub(1),
            ObserverEventKind::ToolStarted => {
                let Some(call_id) = event.call_id.as_deref() else {
                    return;
                };
                if self.tool_calls.len() == 128 {
                    self.tool_calls.pop_front();
                    self.reindex_calls();
                }
                self.by_call_id
                    .insert(call_id.to_string(), self.tool_calls.len());
                self.tool_calls.push_back(ToolCall {
                    call_id: call_id.to_string(),
                    tool_name: event.tool_name.unwrap_or_else(|| "tool".to_string()),
                    status: ToolStatus::Running,
                    duration_ms: None,
                });
            }
            ObserverEventKind::ToolFinished | ObserverEventKind::ToolFailed => {
                let Some(call_id) = event.call_id.as_deref() else {
                    return;
                };
                if let Some(&index) = self.by_call_id.get(call_id)
                    && let Some(call) = self.tool_calls.get_mut(index)
                {
                    call.status = if event.kind == ObserverEventKind::ToolFinished {
                        ToolStatus::Succeeded
                    } else {
                        ToolStatus::Failed
                    };
                    call.duration_ms = event.duration_ms;
                }
            }
            ObserverEventKind::ApprovalRequested if event.summary.is_some() => {
                self.pending.push_back(event.clone())
            }
            ObserverEventKind::ApprovalResolved => {
                if let Some(call_id) = event.call_id.as_deref() {
                    self.pending
                        .retain(|item| item.call_id.as_deref() != Some(call_id));
                }
            }
            _ => {}
        }
    }

    fn reindex_calls(&mut self) {
        self.by_call_id = self
            .tool_calls
            .iter()
            .enumerate()
            .map(|(index, call)| (call.call_id.clone(), index))
            .collect();
    }

    fn resolve_front(&mut self, bridge: &LocalObserverBridge, decision: ApprovalDecision) {
        let Some(call_id) = self
            .pending
            .front()
            .and_then(|item| item.call_id.as_deref())
        else {
            return;
        };
        if bridge.resolve(call_id, decision) {
            self.pending.pop_front();
        }
    }
}

/// Run the existing Grok terminal renderer in remote observer mode.
///
/// This deliberately follows the normal Grok UI's visual language: tool-call
/// history occupies the scrollback surface, and an Ask overlays the existing
/// permission-view renderer. Raw input has only approval and exit bindings;
/// there is no prompt field or any route capable of submitting chat text.
pub async fn run(bridge: Arc<LocalObserverBridge>, status: GatewayObserverStatus) -> Result<()> {
    let mut updates = bridge.subscribe();
    enable_raw_mode()?;
    let mut stderr = io::stderr();
    execute!(stderr, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stderr);
    let mut terminal = Terminal::new(backend)?;
    let mut state = ObserverState::new(status);
    let result = run_loop(&mut terminal, &mut updates, &bridge, &mut state);
    bridge.disconnect();
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();
    result
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stderr>>,
    updates: &mut tokio::sync::broadcast::Receiver<ObserverEvent>,
    bridge: &LocalObserverBridge,
    state: &mut ObserverState,
) -> Result<()> {
    loop {
        while let Ok(event) = updates.try_recv() {
            state.push(event);
        }
        terminal.draw(|frame| draw(frame, state))?;
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Char('y') | KeyCode::Char('1') => {
                state.resolve_front(bridge, ApprovalDecision::AllowOnce)
            }
            KeyCode::Char('n') | KeyCode::Char('2') => {
                state.resolve_front(bridge, ApprovalDecision::Deny)
            }
            _ => {}
        }
    }
}

fn draw(frame: &mut ratatui::Frame<'_>, state: &ObserverState) {
    let theme = Theme::current();
    let frame_area = frame.area();
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(if state.pending.is_empty() { 2 } else { 12 }),
        ])
        .split(frame_area);

    frame
        .buffer_mut()
        .set_style(frame_area, Style::default().bg(theme.bg_base));
    let header = vec![
        Line::from(vec![
            Span::styled(
                "≡  Grok Build · Remote MCP",
                Style::default()
                    .fg(theme.text_primary)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "   {} client{}",
                    state.clients,
                    if state.clients == 1 { "" } else { "s" }
                ),
                Style::default().fg(theme.gray),
            ),
        ]),
        Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(
                state.status.workspace.clone(),
                Style::default().fg(theme.gray),
            ),
            Span::styled("  ·  ", Style::default().fg(theme.gray)),
            Span::styled(
                state.status.endpoint.clone(),
                Style::default().fg(theme.accent_user),
            ),
            Span::styled(
                format!(
                    "  ·  {} downstream · {} tools",
                    state.status.downstream_servers, state.status.exposed_tools
                ),
                Style::default().fg(theme.gray),
            ),
        ]),
    ];
    frame.render_widget(Paragraph::new(header), areas[0]);
    render_tool_history(frame.buffer_mut(), areas[1], state, &theme);

    if let Some(request) = state.pending.front() {
        let permission = observer_permission_view(request);
        crate::views::permission_view::render_permission_view(
            frame.buffer_mut(),
            areas[2],
            &permission,
            "",
            None,
            None,
            &theme,
            true,
        );
        frame.render_widget(
            Paragraph::new("1 / y: allow once    2 / n: deny    Esc / q: deny all and quit")
                .style(Style::default().fg(theme.gray)),
            Rect::new(
                areas[2].x + 2,
                areas[2].bottom().saturating_sub(1),
                areas[2].width.saturating_sub(4),
                1,
            ),
        );
    } else {
        frame.render_widget(
            Paragraph::new("Remote MCP observer · no local chat input · q: quit")
                .style(Style::default().fg(theme.gray))
                .wrap(Wrap { trim: true }),
            areas[2],
        );
    }
}

fn render_tool_history(buf: &mut Buffer, area: Rect, state: &ObserverState, theme: &Theme) {
    if state.tool_calls.is_empty() {
        buf.set_line(
            area.x + 2,
            area.y + 1,
            &Line::from(Span::styled(
                "Waiting for MCP tool calls…",
                Style::default().fg(theme.gray),
            )),
            area.width.saturating_sub(4),
        );
        return;
    }
    let mut y = area.y + 1;
    for call in state
        .tool_calls
        .iter()
        .rev()
        .take(area.height.saturating_sub(2) as usize)
        .rev()
    {
        let (glyph, label_style, suffix) = match call.status {
            ToolStatus::Running => (
                "◇",
                Style::default().fg(theme.accent_user),
                "running".to_string(),
            ),
            ToolStatus::Succeeded => (
                "◆",
                Style::default().fg(theme.accent_assistant),
                format_duration(call.duration_ms),
            ),
            ToolStatus::Failed => (
                "◆",
                Style::default().fg(Color::Red),
                format!("failed {}", format_duration(call.duration_ms)),
            ),
        };
        let line = Line::from(vec![
            Span::styled(format!("  {glyph} "), label_style),
            Span::styled(
                call.tool_name.clone(),
                label_style.add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  {suffix}"), Style::default().fg(theme.gray)),
        ]);
        buf.set_line(area.x, y, &line, area.width);
        y += 1;
    }
}

fn format_duration(duration_ms: Option<u128>) -> String {
    duration_ms
        .map(|value| format!("{:.1}s", value as f64 / 1000.0))
        .unwrap_or_else(|| "done".to_string())
}

/// Translate only display data into the pager's existing permission renderer.
/// The throwaway ACP sender is never used: `LocalObserverBridge` remains the
/// sole authority that resolves the real gateway approval request.
fn observer_permission_view(
    event: &ObserverEvent,
) -> crate::views::permission_view::PermissionViewState {
    let (response_tx, _response_rx) = tokio::sync::oneshot::channel();
    let call_id = event.call_id.as_deref().unwrap_or("gateway-call");
    let tool_name = event.tool_name.as_deref().unwrap_or("tool");
    let request = acp::RequestPermissionRequest::new(
        acp::SessionId::new(Arc::from("remote-mcp")),
        acp::ToolCallUpdate::new(
            acp::ToolCallId::new(Arc::from(call_id)),
            acp::ToolCallUpdateFields::default(),
        ),
        vec![],
    );
    let options = vec![
        acp::PermissionOption::new(
            acp::PermissionOptionId::new(Arc::from("allow-once")),
            "Yes",
            acp::PermissionOptionKind::AllowOnce,
        ),
        acp::PermissionOption::new(
            acp::PermissionOptionId::new(Arc::from("deny")),
            "No",
            acp::PermissionOptionKind::RejectOnce,
        ),
    ];
    crate::views::permission_view::PermissionViewState {
        request: xai_acp_lib::AcpArgs {
            request,
            response_tx,
        },
        id: 0,
        focus: crate::views::permission_view::PermissionFocus::Options,
        options,
        active_idx: 0,
        bash_highlights: None,
        bash_selection_count: 0,
        bash_command_raw: None,
        mcp_scope: None,
        title: format!("Allow {tool_name}?"),
        description: event
            .summary
            .as_deref()
            .map(str::to_owned)
            .into_iter()
            .collect(),
        args_expanded: true,
        desc_scroll: 0,
        subagent_label: Some("Remote MCP client".to_string()),
        options_area_height: 0,
        options_scroll_offset: 0,
    }
}
