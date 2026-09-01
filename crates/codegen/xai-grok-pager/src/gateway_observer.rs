//! Remote-MCP observer mode hosted by the existing Grok pager binary.
//!
//! It deliberately owns no `AcpSession` or model/provider. It uses Grok's
//! real scrollback, block viewer, and read-only prompt chrome to present the
//! gateway event stream, and resolves only `AllowOnce` / `Deny` through
//! `LocalObserverBridge`.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_client_protocol as acp;
use anyhow::Result;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::Line,
    widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Widget},
};
use xai_grok_mcp_server::{
    ApprovalDecision, LocalObserverBridge, ObserverEvent, ObserverEventKind, ObserverToolDetails,
};

use crate::{
    input::key::KeyShortcut,
    scrollback::{
        BlockContent, EntryId, RenderBlock, ScrollbackPane, ScrollbackState, ToolCallBlock,
    },
    theme::Theme,
    views::{
        block_viewer::BlockViewerPane,
        permission_view::{PermissionFocus, PermissionViewState},
        prompt_widget::{PromptBg, PromptFlag, PromptInfo, PromptStyle, PromptWidget},
        shortcuts_bar::{HintItem, ShortcutsBar},
        status_bar::StatusBar,
    },
};

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
    scrollback: ScrollbackState,
    entry_by_call_id: HashMap<String, EntryId>,
    result_by_entry: HashMap<EntryId, String>,
    scrollback_area: Rect,
    mouse_pos: Option<(u16, u16)>,
    hovered_entry: Option<usize>,
    viewer: Option<BlockViewerPane>,
    approval_option: usize,
    last_tool_started: Option<Instant>,
    prompt: PromptWidget,
    focus: ObserverFocus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObserverFocus {
    Prompt,
    Scrollback,
}

impl ObserverState {
    fn new(status: GatewayObserverStatus) -> Self {
        let mut scrollback = ScrollbackState::new();
        scrollback.set_cwd(Some(status.workspace.clone().into()));
        let prompt = PromptWidget::new_with_cwd(std::path::Path::new(&status.workspace));
        Self {
            status,
            pending: VecDeque::new(),
            clients: 0,
            scrollback,
            entry_by_call_id: HashMap::new(),
            result_by_entry: HashMap::new(),
            scrollback_area: Rect::default(),
            mouse_pos: None,
            hovered_entry: None,
            viewer: None,
            approval_option: 0,
            last_tool_started: None,
            prompt,
            // Match Grok Build startup: the composer owns focus until Tab (or
            // a click) moves it into the scrollback. It is display-only here.
            focus: ObserverFocus::Prompt,
        }
    }

    fn push(&mut self, event: ObserverEvent) {
        match event.kind {
            ObserverEventKind::ClientConnected => self.clients = self.clients.saturating_add(1),
            ObserverEventKind::ClientDisconnected => self.clients = self.clients.saturating_sub(1),
            ObserverEventKind::ToolStarted => self.push_tool_started(event),
            ObserverEventKind::ToolFinished => self.finish_tool(event, None),
            ObserverEventKind::ToolFailed => self.finish_tool(event.clone(), event.summary),
            // The broker event carries the redacted operation summary. The
            // event-bus version intentionally has none, so ignore that copy.
            ObserverEventKind::ApprovalRequested if event.summary.is_some() => {
                // Keep the actual tool row in the native Grok "waiting on
                // user" state. Besides protecting it from a verb-fold while
                // the approval is visible, this is what makes ScrollbackPane
                // render the familiar static purple pending accent instead
                // of a neutral collapsed entry.
                if let Some(call_id) = event.call_id.as_deref()
                    && let Some(id) = self.entry_by_call_id.get(call_id).copied()
                {
                    self.scrollback.set_pending_user_input(id, true);
                }
                self.pending.push_back(event)
            }
            ObserverEventKind::ApprovalResolved => {
                if let Some(call_id) = event.call_id.as_deref() {
                    if let Some(id) = self.entry_by_call_id.get(call_id).copied() {
                        self.scrollback.set_pending_user_input(id, false);
                    }
                    self.pending
                        .retain(|item| item.call_id.as_deref() != Some(call_id));
                    self.approval_option = 0;
                }
            }
            _ => {}
        }
    }

    fn push_tool_started(&mut self, event: ObserverEvent) {
        let (Some(call_id), Some(tool_name)) = (event.call_id, event.tool_name) else {
            return;
        };
        let summary = event.summary.unwrap_or_else(|| tool_name.clone());
        // Group a burst of remote activity exactly as one Grok scrollback
        // bracket. A longer idle interval begins a fresh visual activity
        // group, while preserving chronological order and every native tool
        // row/interaction.
        let joins_previous = self
            .last_tool_started
            .is_some_and(|last| last.elapsed() < Duration::from_secs(120));
        let block = observer_tool_block(&tool_name, summary, event.details.as_ref());
        let id = self.scrollback.push_block(block);
        self.scrollback
            .set_dense_group_with_previous(id, joins_previous);
        // Keep file mutations as their own native diff rows. They need to be
        // immediately legible and selectable instead of being summarized into
        // a mixed read/run activity header.
        self.scrollback.set_force_activity_group(
            id,
            !matches!(
                tool_name.as_str(),
                "search_replace" | "edit" | "apply_patch" | "strreplace" | "write"
            ),
        );
        self.scrollback.set_entry_running(id, true);
        self.entry_by_call_id.insert(call_id, id);
        self.last_tool_started = Some(Instant::now());
    }

    fn finish_tool(&mut self, event: ObserverEvent, failure: Option<String>) {
        let Some(call_id) = event.call_id.as_deref() else {
            return;
        };
        let Some(id) = self.entry_by_call_id.get(call_id).copied() else {
            return;
        };
        if let Some(entry) = self.scrollback.get_by_id_mut(id) {
            if let RenderBlock::ToolCall(tool) = &mut entry.block {
                if let Some(error) = failure {
                    set_tool_error(tool, error);
                }
                if let Some(output) = event.output {
                    self.result_by_entry.insert(id, output.clone());
                    match tool {
                        ToolCallBlock::Execute(execute) => execute.output = Some(output),
                        ToolCallBlock::ListDir(list) => list.set_output(output),
                        ToolCallBlock::Read(read) => {
                            let content = strip_read_line_anchors(&output);
                            read.total_lines = Some(content.lines().count());
                            read.content = Some(content);
                        }
                        _ => {}
                    }
                }
            }
            entry.invalidate_cache();
        }
        self.scrollback.finish_running(id);
    }

    fn resolve_current_with(&mut self, bridge: &LocalObserverBridge, decision: ApprovalDecision) {
        let Some(call_id) = self
            .pending
            .front()
            .and_then(|request| request.call_id.as_deref())
        else {
            return;
        };
        if bridge.resolve(call_id, decision) {
            if let Some(id) = self.entry_by_call_id.get(call_id).copied() {
                self.scrollback.set_pending_user_input(id, false);
            }
            self.pending.pop_front();
            self.approval_option = 0;
        }
    }

    fn move_approval_option(&mut self, delta: isize) {
        self.approval_option = (self.approval_option as isize + delta).rem_euclid(2) as usize;
    }

    fn select_at_mouse(&mut self, mouse: MouseEvent) {
        if !self
            .scrollback_area
            .contains((mouse.column, mouse.row).into())
        {
            return;
        }
        if let Some(index) = self
            .scrollback
            .entry_index_at_screen_row(mouse.row, self.scrollback_area)
        {
            self.scrollback.set_selected(Some(index));
            self.focus = ObserverFocus::Scrollback;
        }
    }

    fn open_selected(&mut self) {
        // A collapsed group header is an interaction target in its own right:
        // Enter expands the native group first. The expanded verb header then
        // acts as its first member, so the next Enter opens that member's
        // detail viewer exactly like Grok's normal scrollback.
        if self.scrollback.is_selected_group_header() {
            let _ = self.scrollback.toggle_group_expansion();
            return;
        }
        let Some(index) = self.scrollback.selected() else {
            return;
        };
        let Some(entry) = self.scrollback.entry(index) else {
            return;
        };
        if let Some(viewer) = BlockViewerPane::for_execute(entry.id, entry)
            .or_else(|| BlockViewerPane::for_edit(entry.id, entry))
            .or_else(|| BlockViewerPane::for_read(entry.id, entry))
            .or_else(|| BlockViewerPane::for_grep(entry.id, entry))
            .or_else(|| BlockViewerPane::for_list_dir(entry.id, entry))
            .or_else(|| {
                self.result_by_entry
                    .get(&entry.id)
                    .map(|output| BlockViewerPane::for_plain_text("Tool output", output))
            })
        {
            self.viewer = Some(viewer);
        } else {
            self.scrollback.toggle_fold_selected();
        }
    }
}

/// Grok's `read_file` wire result places an `N→` anchor on the first line and
/// periodically thereafter so an agent can cite a line number. `ReadToolCallBlock`
/// stores actual file content, however, and the native UI renders its own line
/// gutter; keeping the wire anchors would incorrectly make them look like file
/// bytes in the observer's viewer.
fn strip_read_line_anchors(output: &str) -> String {
    let mut normalized = output
        .lines()
        .map(|line| match line.split_once('→') {
            Some((prefix, content))
                if !prefix.is_empty() && prefix.bytes().all(|byte| byte.is_ascii_digit()) =>
            {
                content
            }
            _ => line,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if output.ends_with('\n') {
        normalized.push('\n');
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::strip_read_line_anchors;

    #[test]
    fn read_wire_anchors_are_not_rendered_as_file_content() {
        assert_eq!(
            strip_read_line_anchors("1→first line\nsecond line\n10→tenth line\n"),
            "first line\nsecond line\ntenth line\n"
        );
    }

    #[test]
    fn ordinary_arrow_text_is_preserved() {
        assert_eq!(
            strip_read_line_anchors("section→keep this\n123 text\n"),
            "section→keep this\n123 text\n"
        );
    }
}

fn observer_tool_block(
    tool_name: &str,
    summary: String,
    details: Option<&ObserverToolDetails>,
) -> RenderBlock {
    let mut tool = ToolCallBlock::from_name(tool_name, summary);
    match (details, &mut tool) {
        (
            Some(ObserverToolDetails::Edit {
                old_text, new_text, ..
            }),
            ToolCallBlock::Edit(edit),
        ) => edit.set_hunks(xai_grok_pager_diff::diff_hunks_from_strings(old_text, new_text, 1)),
        (Some(ObserverToolDetails::Write { content, .. }), ToolCallBlock::Edit(edit)) => {
            edit.set_hunks(xai_grok_pager_diff::diff_hunks_from_strings("", content, 1));
            edit.prefix = "Creating ";
        }
        _ => {}
    }
    RenderBlock::ToolCall(tool)
}

fn set_tool_error(tool: &mut ToolCallBlock, error: String) {
    match tool {
        ToolCallBlock::Execute(block) => block.set_error(Some(error)),
        ToolCallBlock::Read(block) => block.set_error(Some(error)),
        ToolCallBlock::Edit(block) => block.set_error(Some(error)),
        ToolCallBlock::ListDir(block) => block.set_error(Some(error)),
        ToolCallBlock::Search(block) => block.set_error(Some(error)),
        ToolCallBlock::WebFetch(block) => block.set_error(Some(error)),
        ToolCallBlock::WebSearch(block) => block.set_error(Some(error)),
        ToolCallBlock::IntegrationSearch(block) => block.set_error(Some(error)),
        ToolCallBlock::UseTool(block) => block.set_error(Some(error)),
        ToolCallBlock::MemorySearch(block) => block.set_error(Some(error)),
        ToolCallBlock::Skill(block) | ToolCallBlock::Other(block) => block.set_error(Some(error)),
        ToolCallBlock::Lifecycle(_) => {}
    }
}

/// Run Grok's normal terminal rendering stack in remote observer mode.
pub async fn run(bridge: Arc<LocalObserverBridge>, status: GatewayObserverStatus) -> Result<()> {
    let mut updates = bridge.subscribe();
    enable_raw_mode()?;
    let mut stderr = io::stderr();
    execute!(stderr, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stderr);
    let mut terminal = Terminal::new(backend)?;
    terminal.hide_cursor()?;
    let mut state = ObserverState::new(status);
    let result = run_loop(&mut terminal, &mut updates, &bridge, &mut state);
    bridge.disconnect();
    let _ = disable_raw_mode();
    let _ = execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    );
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
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if handle_key(key, bridge, state) {
                    return Ok(());
                }
            }
            Event::Mouse(mouse) => handle_mouse(mouse, state),
            _ => {}
        }
    }
}

/// Returns true when observer mode should exit.
fn handle_key(key: KeyEvent, bridge: &LocalObserverBridge, state: &mut ObserverState) -> bool {
    if let Some(viewer) = state.viewer.as_mut() {
        if viewer.is_close_key(&key) {
            state.viewer = None;
            return false;
        }
        let _ = viewer.handle_key(&key);
        return false;
    }
    if !state.pending.is_empty() {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => state.move_approval_option(-1),
            KeyCode::Down | KeyCode::Char('j') => state.move_approval_option(1),
            KeyCode::Enter => {
                let decision = if state.approval_option == 0 {
                    ApprovalDecision::AllowOnce
                } else {
                    ApprovalDecision::Deny
                };
                state.resolve_current_with(bridge, decision);
            }
            KeyCode::Esc | KeyCode::Char('q') => return true,
            _ => {}
        }
        return false;
    }
    if key.code == KeyCode::Tab {
        state.focus = match state.focus {
            ObserverFocus::Prompt => {
                state.scrollback.on_activate();
                ObserverFocus::Scrollback
            }
            ObserverFocus::Scrollback => ObserverFocus::Prompt,
        };
        return false;
    }
    if state.focus == ObserverFocus::Prompt {
        return matches!(key.code, KeyCode::Char('q') | KeyCode::Esc);
    }
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return true,
        KeyCode::Up | KeyCode::Char('k') => state.scrollback.select_prev(),
        KeyCode::Down | KeyCode::Char('j') => state.scrollback.select_next(),
        KeyCode::Enter | KeyCode::Char(' ') => state.open_selected(),
        KeyCode::Left | KeyCode::Char('h') => state.scrollback.collapse_selected(),
        KeyCode::Right | KeyCode::Char('l') => state.scrollback.expand_selected(),
        _ => {}
    }
    false
}

fn handle_mouse(mouse: MouseEvent, state: &mut ObserverState) {
    if state.viewer.is_some() {
        let close = state
            .viewer
            .as_ref()
            .and_then(|viewer| viewer.modal.close_button_rect);
        let popup = state
            .viewer
            .as_ref()
            .and_then(|viewer| viewer.modal.popup_area);
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && (close.is_some_and(|area| area.contains((mouse.column, mouse.row).into()))
                || popup.is_some_and(|area| !area.contains((mouse.column, mouse.row).into())))
        {
            state.viewer = None;
            return;
        }
        let Some(viewer) = state.viewer.as_mut() else {
            return;
        };
        if matches!(mouse.kind, MouseEventKind::Moved) {
            viewer.modal.close_hovered =
                close.is_some_and(|area| area.contains((mouse.column, mouse.row).into()));
        }
        if matches!(mouse.kind, MouseEventKind::ScrollUp) {
            viewer.handle_scroll(-3);
        } else if matches!(mouse.kind, MouseEventKind::ScrollDown) {
            viewer.handle_scroll(3);
        } else {
            let _ = viewer.handle_mouse(mouse.kind, mouse.column, mouse.row);
        }
        return;
    }
    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
        && !state
            .scrollback_area
            .contains((mouse.column, mouse.row).into())
    {
        state.focus = ObserverFocus::Prompt;
        return;
    }
    match mouse.kind {
        MouseEventKind::Moved => {
            state.mouse_pos = Some((mouse.column, mouse.row));
            state.hovered_entry = state
                .scrollback
                .entry_index_at_screen_row(mouse.row, state.scrollback_area);
        }
        MouseEventKind::Down(MouseButton::Left) => state.select_at_mouse(mouse),
        MouseEventKind::ScrollUp => state.scrollback.scroll_up(3),
        MouseEventKind::ScrollDown => state.scrollback.scroll_down(3),
        _ => {}
    }
}

fn draw(frame: &mut ratatui::Frame<'_>, state: &mut ObserverState) {
    let theme = Theme::current();
    let frame_area = frame.area();
    frame
        .buffer_mut()
        .set_style(frame_area, Style::default().bg(theme.bg_base));

    // Use the same viewport gutter as AgentView. The observer has a reduced
    // set of panes, but it must not look like an unrelated full-screen TUI:
    // Grok's status, scrollback, composer and shortcut rows all live inside
    // this padded viewport.
    let appearance = state.scrollback.appearance().clone();
    let layout_cfg = appearance.scrollback.layout;
    let compact = appearance.prompt.compact;
    let outer = Block::default()
        .padding(Padding::new(
            layout_cfg.eff_hpad_left(compact),
            layout_cfg.eff_hpad_right(compact),
            layout_cfg.eff_outer_vpad(compact),
            layout_cfg.eff_outer_vpad(compact),
        ))
        .style(Style::default().bg(theme.bg_base));
    let viewport = outer.inner(frame_area);
    outer.render(frame_area, frame.buffer_mut());
    let approval_height = if state.pending.is_empty() { 0 } else { 10 };
    let mut constraints = vec![
        Constraint::Length(1), // normal Grok status row
        Constraint::Length(1), // status-to-scrollback gap
        Constraint::Min(5),    // tool history
    ];
    if approval_height > 0 {
        constraints.push(Constraint::Length(1));
        constraints.push(Constraint::Length(approval_height));
    }
    constraints.extend([
        Constraint::Length(1), // scrollback/approval-to-composer gap
        // A one-line Grok composer is exactly top divider + text row +
        // bottom info divider. Giving it a fourth row causes `Min(1)` inside
        // PromptWidget to absorb the spare row and leaves the text visibly
        // top-aligned instead of vertically centered.
        Constraint::Length(3),
        Constraint::Length(1), // composer-to-shortcuts gap
        Constraint::Length(1),
    ]);
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(viewport);
    let scrollback_index = 2;
    let approval_index = if approval_height > 0 { Some(4) } else { None };
    let composer_index = if approval_height > 0 { 5 } else { 4 };
    let shortcuts_index = composer_index + 2;
    let left = state.status.workspace.as_str();
    let right = format!(
        "{} · {} client{} · {} downstream · {} tools",
        state.status.endpoint,
        state.clients,
        if state.clients == 1 { "" } else { "s" },
        state.status.downstream_servers,
        state.status.exposed_tools
    );
    frame.render_widget(StatusBar::new(left).right(&right), areas[0]);

    state.scrollback_area = areas[scrollback_index];
    state.scrollback.prepare_layout(
        areas[scrollback_index].width,
        areas[scrollback_index].height,
    );
    let output = ScrollbackPane::new()
        .active(state.focus == ObserverFocus::Scrollback)
        .with_mouse_pos(state.mouse_pos.unwrap_or((u16::MAX, u16::MAX)))
        .with_hovered_entry(state.hovered_entry)
        .render_with_scratch(
            areas[scrollback_index],
            frame.buffer_mut(),
            &state.scrollback,
            &mut Default::default(),
        );
    if let Some(selection) = output.selection_box {
        selection.render(frame.buffer_mut());
    }
    if state.scrollback.len() == 0 {
        frame.render_widget(
            Paragraph::new("Waiting for MCP tool calls…").style(Style::default().fg(theme.gray)),
            Rect::new(
                areas[scrollback_index].x + 2,
                areas[scrollback_index].y + 1,
                areas[scrollback_index].width.saturating_sub(4),
                1,
            ),
        );
    }

    if let (Some(request), Some(approval_index)) = (state.pending.front(), approval_index) {
        let permission = observer_permission_view(request, state.approval_option);
        crate::views::permission_view::render_permission_view(
            frame.buffer_mut(),
            areas[approval_index],
            &permission,
            "",
            None,
            None,
            &theme,
            true,
        );
    }
    let flags = [PromptFlag {
        text: "manual approval",
        color: Some(theme.accent_system),
        bold: false,
    }];
    let prompt_info = PromptInfo {
        model_name: "Remote MCP",
        flags: &flags,
        multiline: false,
        usage_warning: None,
        usage_warning_critical: false,
    };
    let prompt_style = PromptStyle {
        focused: state.focus == ObserverFocus::Prompt && state.pending.is_empty(),
        show_prefix: appearance.prompt.show_prefix,
        vpad_top: 1,
        chrome: true,
        chrome_pad_left: layout_cfg.block_pad_left,
        chrome_pad_right: layout_cfg.block_pad_right,
        bg: PromptBg::Default,
        accent_color_override: None,
        border_color_override: None,
        prefix_override: None,
        placeholder_override: Some("ChatGPT Web is the active agent"),
        placeholder_when_focused: true,
        compact,
        show_accent_line: false,
        show_borders: true,
        title: Some("Remote MCP observer".to_string()),
        image_preview: false,
    };
    let _ = state.prompt.draw(
        frame.buffer_mut(),
        areas[composer_index],
        None,
        &prompt_style,
        Some(&prompt_info),
        None,
    );

    let hints = if !state.pending.is_empty() {
        vec![
            HintItem::paired(
                KeyShortcut::key(KeyCode::Up),
                KeyShortcut::key(KeyCode::Down),
                "select",
            ),
            HintItem::new(KeyShortcut::key(KeyCode::Enter), "approve / deny"),
            HintItem::new(KeyShortcut::key(KeyCode::Esc), "deny and quit"),
        ]
    } else if state.focus == ObserverFocus::Prompt {
        vec![
            HintItem::new(KeyShortcut::key(KeyCode::Tab), "history"),
            HintItem::new(KeyShortcut::key(KeyCode::Esc), "quit"),
        ]
    } else {
        vec![
            HintItem::paired(
                KeyShortcut::key(KeyCode::Up),
                KeyShortcut::key(KeyCode::Down),
                "select",
            ),
            HintItem::new(KeyShortcut::key(KeyCode::Enter), "open"),
            HintItem::new(KeyShortcut::key(KeyCode::Tab), "prompt"),
            HintItem::new(KeyShortcut::key(KeyCode::Esc), "quit"),
        ]
    };
    frame.render_widget(ShortcutsBar::new(&hints), areas[shortcuts_index]);

    // Render the viewer *after* the ordinary observer surface so it is a
    // genuine modal over the retained transcript/composer, rather than a
    // replacement screen with an empty background.
    if let Some(viewer) = state.viewer.as_mut() {
        let overlay_area = Rect {
            x: frame_area.x,
            y: frame_area.y,
            width: frame_area.width,
            height: areas[shortcuts_index].y.saturating_sub(frame_area.y),
        };
        draw_viewer(
            frame.buffer_mut(),
            overlay_area,
            viewer,
            &state.scrollback,
            &theme,
        );
    }
}

fn draw_viewer(
    buf: &mut Buffer,
    area: Rect,
    viewer: &mut BlockViewerPane,
    scrollback: &ScrollbackState,
    theme: &Theme,
) {
    let Some(entry) = scrollback.get_by_id(viewer.entry_id) else {
        return;
    };
    // Match Grok Build's normal centered viewer geometry. Fullscreen file
    // viewers deliberately use the near-edge layout, but a selected tool
    // result is a normal modal and must retain the generous surrounding
    // scrollback context.
    let popup_w = ((area.width as f32 * 0.75) as u16).max(60).min(area.width);
    let popup_h = ((area.height as f32 * 0.75) as u16)
        .max(12)
        .min(area.height.saturating_sub(2));
    let h_pad = 2u16;
    let inner_w = popup_w.saturating_sub(2);
    let content_w = inner_w.saturating_sub(h_pad * 2);
    if popup_h.saturating_sub(2) < 3 || inner_w < 10 {
        return;
    }
    let popup_x = area.x + (area.width.saturating_sub(popup_w)) / 2;
    let popup_y = area.y + (area.height.saturating_sub(popup_h)) / 2;
    let popup_area = Rect::new(popup_x, popup_y, popup_w, popup_h);
    crate::views::file_search::line_viewer::dim_area(buf, area, theme.bg_base, 0.5);
    Clear.render(popup_area, buf);
    let base_style = Style::default().fg(theme.text_primary).bg(theme.bg_base);
    let border = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.gray_dim))
        .style(base_style);
    let inner = border.inner(popup_area);
    if inner.height < 3 || inner.width < 10 {
        return;
    }
    border.render(popup_area, buf);
    let close_x = inner.x + inner.width.saturating_sub(4);
    let close_y = inner.y;
    let close_rect = Rect::new(close_x, close_y, 3, 1);
    for (index, ch) in ['[', 'x', ']'].into_iter().enumerate() {
        if let Some(cell) = buf.cell_mut((close_x + index as u16, close_y)) {
            cell.set_char(ch);
            cell.set_style(if viewer.modal.close_hovered {
                Style::default()
                    .fg(theme.text_primary)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.gray_dim)
            });
        }
    }
    viewer.modal.close_button_rect = Some(close_rect);
    viewer.modal.popup_area = Some(popup_area);
    let block_context = crate::scrollback::BlockContext {
        mode: crate::scrollback::DisplayMode::Expanded,
        is_running: entry.is_running,
        width: content_w,
        raw: entry.raw,
        max_lines: None,
        appearance: scrollback.appearance().clone(),
        is_selected: false,
        cwd: scrollback.cwd().map(std::path::Path::to_path_buf),
    };
    let mut preamble: Vec<Line<'static>> = entry
        .block
        .preamble(&block_context)
        .map(|output| output.lines)
        .unwrap_or_default();
    if !preamble.is_empty() {
        preamble.push(Line::default());
    }
    let content_area = Rect {
        x: inner.x + h_pad,
        y: inner.y + 1,
        width: content_w,
        height: inner.height.saturating_sub(1),
    };
    viewer.render_content(content_area, buf, entry, true, &preamble);
    viewer.render_text_drag_overlay(buf);
    ShortcutsBar::new(&viewer.shortcuts_hints()).render(
        Rect::new(
            inner.x + h_pad,
            inner.bottom().saturating_sub(1),
            content_w,
            1,
        ),
        buf,
    );
}

fn observer_permission_view(event: &ObserverEvent, active_idx: usize) -> PermissionViewState {
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
            "Yes, proceed",
            acp::PermissionOptionKind::AllowOnce,
        ),
        acp::PermissionOption::new(
            acp::PermissionOptionId::new(Arc::from("deny")),
            "No, reject",
            acp::PermissionOptionKind::RejectOnce,
        ),
    ];
    PermissionViewState {
        request: xai_acp_lib::AcpArgs {
            request,
            response_tx,
        },
        id: 0,
        focus: PermissionFocus::Options,
        options,
        active_idx,
        bash_highlights: None,
        bash_selection_count: 0,
        bash_deny_selection_count: 0,
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
