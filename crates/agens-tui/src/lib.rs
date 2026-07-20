//! Terminal lifecycle and input-event boundary for the interactive surface.

mod app;
mod bridge;
mod conversation;
mod render;
mod terminal;

pub use app::{AppEvent, AppState, Command, Dialog, Effect, Runtime};
pub use bridge::{
    BridgeCancel, BridgeTx, PublishOutcome, ToolResultState, TuiRuntimeEvent, UiEnvelope,
};
pub use conversation::{
    ActionableError, Conversation, ConversationError, ConversationEvent, DiffLine, DiffLineKind,
    ToolBatch, ToolCall, ToolResult,
};
pub use terminal::{
    PendingPermissions, PermissionReply, TerminalControl, TerminalModeGuard, TerminalOperation,
    teardown,
};

use std::{
    collections::BTreeSet,
    io::{self, Stdout, Write},
    sync::{Arc, mpsc},
    thread,
    time::Duration,
};

use agens_core::{Message, MessagePart, TurnEvent, TurnState, Usage};
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event as CrosstermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{self as crossterm_terminal, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    Terminal as RatatuiTerminal,
    backend::{Backend, CrosstermBackend},
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

/// Cancels the active engine turn. The TUI owns no provider or session logic.
pub trait Engine {
    /// Requests cooperative cancellation of the active turn.
    fn cancel(&mut self);
}

/// Input received from the terminal that affects the TUI event loop.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Event {
    /// A key that participates in normal interaction.
    Key(Key),
    Paste(String),
    /// A terminal resize in columns and rows.
    Resize {
        width: u16,
        height: u16,
    },
}

/// Keys handled by the TUI engine boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Key {
    /// An ordinary input character.
    Char(char),
    /// Deletes the preceding input character.
    Backspace,
    Delete,
    DeletePreviousWord,
    DeleteToLineStart,
    DeleteToLineEnd,
    /// Submits the current input.
    Enter,
    /// Cancels an active turn when one exists.
    Escape,
    /// Cancels an active turn, clears input, or arms quitting.
    CtrlC,
    /// Collapses or expands completed tool outputs in the visible conversation.
    CtrlO,
    ShiftEnter,
    Left,
    Right,
    PreviousWord,
    NextWord,
    Home,
    End,
    PageUp,
    PageDown,
    Up,
    Down,
    Tab,
}

/// The result of handling a single terminal event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Action {
    /// Render the current view state.
    Render,
    /// Send this prompt to the composition layer.
    Submit(String),
    /// Ask the composition layer to resolve a palette dialog by stable route ID.
    OpenDialog(String),
    /// Dispatch the selected dialog action through the composition layer.
    DialogAction(String),
    /// An active engine turn was asked to cancel.
    Cancel,
    /// End the terminal event loop.
    Quit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TuiPresentation {
    provider: String,
    model: String,
    session: String,
}

impl TuiPresentation {
    pub fn new(
        provider: impl Into<String>,
        model: impl Into<String>,
        session: impl Into<String>,
    ) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            session: session.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TuiSubmissionOutcome {
    ProviderTurn {
        display: String,
        prompt: String,
    },
    LocalInfo(String),
    LocalActionableError {
        message: String,
        action: String,
    },
    ResetSucceeded {
        message: String,
        presentation: TuiPresentation,
    },
    ContextChanged {
        message: String,
        presentation: TuiPresentation,
    },
    SessionResumed {
        message: String,
        presentation: TuiPresentation,
        messages: Vec<Message>,
    },
    Dialog(DialogView),
    Quit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TuiRouteRequest {
    Input(String),
    OpenDialog(String),
    DialogAction(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TuiRouteProgress {
    BrowserUrl(String),
    DeviceCode {
        verification_url: String,
        user_code: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TuiProviderOutcome {
    Completed(String),
    Failed { message: String, action: String },
    Cancelled { message: String, action: String },
}

/// A visible conversation entry in chronological order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TranscriptEntry {
    /// A prompt submitted by the user.
    User(String),
    /// Text returned by the shared runtime.
    Assistant(String),
    /// Provider reasoning returned by the shared runtime.
    Reasoning(String),
    /// A sanitized runtime failure.
    Error(String),
    /// A local session or lifecycle note.
    Info(String),
    /// A tool lifecycle result with no tool input exposure.
    Tool(String),
}

/// State passed to renderers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ViewState<'a> {
    /// The editable prompt text.
    pub input: &'a str,
    /// Current terminal dimensions.
    pub size: (u16, u16),
    /// Whether the composed engine has an active turn.
    pub running: bool,
    /// Whether an idle second Ctrl+C will quit.
    pub quit_armed: bool,
    /// Conversation entries rendered in the order they occurred.
    pub transcript: &'a [TranscriptEntry],
    /// Whether new output advances to the bottom of the transcript.
    pub following_bottom: bool,
    /// The manual transcript offset when bottom following is disabled.
    pub scroll_offset: u16,
    /// Current provider and model selected by the CLI composition root.
    pub provider_model: &'a str,
    /// Active session label supplied by the CLI composition root.
    pub session: &'a str,
    /// Current active-turn state for the dedicated status row.
    pub turn_state: Option<TurnState>,
    /// Tool name currently being dispatched, when known.
    pub active_tool: Option<&'a str>,
    /// Current character cursor position in the editable prompt.
    pub input_cursor: usize,
    /// Typed metrics retained for rich, lossless presentation.
    pub runtime_events: &'a [TuiRuntimeEvent],
    pub turn_duration: Option<Duration>,
    pub latest_usage: Option<&'a Usage>,
    pub status: Option<&'a str>,
    /// Authoritative typed conversation projection, when a turn is active or completed.
    pub conversation: Option<&'a Conversation>,
    /// Completed typed conversations retained before the active turn.
    pub completed_conversations: &'a [Conversation],
    /// Tool outputs collapsed only for presentation; their source output remains retained.
    pub collapsed_tool_outputs: &'a BTreeSet<String>,
    /// A bounded informational dialog rendered above the conversation.
    pub dialog: Option<&'a DialogView>,
    /// Slash palette metadata and current filtered selection.
    pub palette: Option<PaletteView<'a>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PaletteEntryKind {
    BuiltIn,
    Command,
    Skill,
}

impl PaletteEntryKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::BuiltIn => "built-in",
            Self::Command => "command",
            Self::Skill => "skill",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaletteEntry {
    name: String,
    description: String,
    argument_hint: String,
    kind: PaletteEntryKind,
    dialog_id: Option<String>,
}

impl PaletteEntry {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        argument_hint: impl Into<String>,
        kind: PaletteEntryKind,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            argument_hint: argument_hint.into(),
            kind,
            dialog_id: None,
        }
    }

    pub fn with_dialog(mut self, route_id: impl Into<String>) -> Self {
        self.dialog_id = Some(route_id.into());
        self
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn argument_hint(&self) -> &str {
        &self.argument_hint
    }

    pub const fn kind(&self) -> PaletteEntryKind {
        self.kind
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PaletteView<'a> {
    entries: &'a [PaletteEntry],
    selected: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum DialogEntryAction {
    Dispatch(String),
    Cancel,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DialogEntry {
    label: String,
    detail: Option<String>,
    action: Option<DialogEntryAction>,
}

impl DialogEntry {
    pub fn action(label: impl AsRef<str>, action_id: impl AsRef<str>) -> Self {
        Self {
            label: bounded_dialog_text(label.as_ref(), 128),
            detail: None,
            action: Some(DialogEntryAction::Dispatch(bounded_dialog_text(
                action_id.as_ref(),
                128,
            ))),
        }
    }

    pub fn cancel(label: impl AsRef<str>) -> Self {
        Self {
            label: bounded_dialog_text(label.as_ref(), 128),
            detail: None,
            action: Some(DialogEntryAction::Cancel),
        }
    }

    pub fn disabled(label: impl AsRef<str>, detail: impl AsRef<str>) -> Self {
        Self {
            label: bounded_dialog_text(label.as_ref(), 128),
            detail: Some(bounded_dialog_text(detail.as_ref(), 256)),
            action: None,
        }
    }
}

/// Generic bounded dialog state for informational, selection, and confirmation overlays.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DialogView {
    title: String,
    help: Option<String>,
    entries: Vec<DialogEntry>,
    selected: usize,
    interactive: bool,
}

impl DialogView {
    pub fn selection<H>(title: impl AsRef<str>, help: Option<H>, entries: Vec<DialogEntry>) -> Self
    where
        H: AsRef<str>,
    {
        let entries = entries.into_iter().take(64).collect::<Vec<_>>();
        let selected = entries
            .iter()
            .position(|entry| entry.action.is_some())
            .unwrap_or_default();
        Self {
            title: bounded_dialog_text(title.as_ref(), 64),
            help: help.map(|help| bounded_dialog_text(help.as_ref(), 2_048)),
            entries,
            selected,
            interactive: true,
        }
    }

    fn informational(title: impl AsRef<str>, body: impl AsRef<str>) -> Self {
        Self {
            title: bounded_dialog_text(title.as_ref(), 64),
            help: Some(bounded_dialog_text(body.as_ref(), 2_048)),
            entries: Vec::new(),
            selected: 0,
            interactive: false,
        }
    }
}

/// Ratatui renderer usable with both real terminals and `TestBackend`.
pub struct RatatuiRenderer<B: Backend> {
    terminal: RatatuiTerminal<B>,
}

impl<B: Backend> RatatuiRenderer<B> {
    pub fn new(terminal: RatatuiTerminal<B>) -> Self {
        Self { terminal }
    }

    pub fn terminal(&self) -> &RatatuiTerminal<B> {
        &self.terminal
    }
}

impl<B: Backend> Renderer for RatatuiRenderer<B> {
    fn render(&mut self, state: ViewState<'_>) -> io::Result<()> {
        self.terminal
            .draw(|frame| render_frame(frame, state))
            .map(|_| ())
            .map_err(|_| io::Error::other("Ratatui draw failed"))
    }
}

fn render_frame(frame: &mut ratatui::Frame<'_>, state: ViewState<'_>) {
    let area = frame.area();
    let layout = screen_layout(area, state.running);

    if layout.header.height > 0 {
        render_header(frame, layout.header, &state, layout.show_context);
    }

    let mut transcript = state
        .completed_conversations
        .iter()
        .flat_map(|conversation| {
            render::conversation_lines(conversation, &[], state.collapsed_tool_outputs)
        })
        .collect::<Vec<_>>();
    if let Some(conversation) = state.conversation {
        transcript.extend(render::conversation_lines(
            conversation,
            state.runtime_events,
            state.collapsed_tool_outputs,
        ));
    }
    let conversation_is_authoritative =
        !state.completed_conversations.is_empty() || state.conversation.is_some();
    if !conversation_is_authoritative {
        transcript = transcript_lines(state.transcript);
    }
    transcript.extend(render::detail_lines(
        state.runtime_events,
        conversation_is_authoritative,
    ));
    let visible_rows = layout.transcript.height.saturating_sub(1) as usize;
    let bottom_scroll =
        transcript_rows(&transcript, layout.transcript.width).saturating_sub(visible_rows) as u16;
    let scroll = if state.following_bottom {
        bottom_scroll
    } else {
        bottom_scroll.saturating_sub(state.scroll_offset)
    };
    let scroll_label = if state.following_bottom {
        " LIVE".to_owned()
    } else {
        format!(" SCROLL +{}", state.scroll_offset)
    };
    if layout.transcript.height > 0 {
        frame.render_widget(
            Paragraph::new(Text::from(transcript))
                .block(
                    Block::default()
                        .borders(Borders::TOP)
                        .border_style(Style::default().fg(Color::DarkGray))
                        .title(Span::styled(
                            " transcript ",
                            Style::default().fg(Color::DarkGray),
                        ))
                        .title_bottom(Span::styled(
                            scroll_label,
                            Style::default().fg(Color::DarkGray),
                        ))
                        .title_alignment(Alignment::Right),
                )
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0)),
            layout.transcript,
        );
    }

    if layout.status.height > 0 {
        render_turn_status(frame, layout.status, &state);
    }

    let composer_title = if state.running {
        " Compose · running "
    } else {
        " Compose "
    };
    let composer_color = if state.running {
        Color::Yellow
    } else {
        Color::Cyan
    };
    if layout.composer.height > 0 {
        let (cursor_line, cursor_column) = cursor_position(state.input, state.input_cursor);
        let inner_width = usize::from(layout.composer.width.saturating_sub(2));
        let inner_height = usize::from(layout.composer.height.saturating_sub(2));
        let vertical_scroll = cursor_line.saturating_sub(inner_height.saturating_sub(1));
        let horizontal_scroll = cursor_column.saturating_sub(inner_width.saturating_sub(1));
        frame.render_widget(
            Paragraph::new(state.input)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(composer_color))
                        .title(Span::styled(
                            composer_title,
                            Style::default()
                                .fg(composer_color)
                                .add_modifier(Modifier::BOLD),
                        ))
                        .title_bottom(Span::styled(
                            composer_metadata(state.input),
                            Style::default().fg(Color::DarkGray),
                        ))
                        .title_alignment(Alignment::Right),
                )
                .scroll((
                    saturating_u16(vertical_scroll),
                    saturating_u16(horizontal_scroll),
                )),
            layout.composer,
        );
        if inner_width > 0 && inner_height > 0 {
            let cursor_y = layout
                .composer
                .y
                .saturating_add(1)
                .saturating_add(saturating_u16(cursor_line.saturating_sub(vertical_scroll)));
            let cursor_x = layout
                .composer
                .x
                .saturating_add(1)
                .saturating_add(saturating_u16(
                    cursor_column.saturating_sub(horizontal_scroll),
                ));
            frame.set_cursor_position((cursor_x, cursor_y));
        }
    }

    if layout.footer.height > 0 {
        frame.render_widget(
            Paragraph::new(footer_text(area.width, &state))
                .style(Style::default().fg(Color::DarkGray)),
            layout.footer,
        );
    }

    if let Some(dialog) = state.dialog {
        render_dialog(frame, area, dialog);
    }

    if let Some(palette) = state.palette {
        render_palette(frame, area, layout.composer, state.input, palette);
    }
}

fn render_palette(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    composer: Rect,
    input: &str,
    palette: PaletteView<'_>,
) {
    let matches = palette_matches(palette.entries, input);
    let content_rows = matches.len().clamp(1, 6) as u16;
    let height = content_rows.saturating_add(2).min(composer.y);
    if height < 3 || area.width == 0 {
        return;
    }
    let palette_area = Rect::new(area.x, composer.y - height, area.width, height);
    let items = if matches.is_empty() {
        vec![ListItem::new(" No matching commands")]
    } else {
        matches
            .iter()
            .map(|entry| {
                ListItem::new(format!(
                    " /{} {}  {}  [{}]",
                    entry.name,
                    entry.argument_hint,
                    entry.description,
                    entry.kind.label()
                ))
            })
            .collect()
    };
    let mut state = ListState::default().with_selected(
        (!matches.is_empty()).then_some(palette.selected.min(matches.len().saturating_sub(1))),
    );

    frame.render_widget(Clear, palette_area);
    frame.render_stateful_widget(
        List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(Color::Cyan))
                    .title(Span::styled(
                        " commands ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    )),
            )
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        palette_area,
        &mut state,
    );
}

fn render_dialog(frame: &mut ratatui::Frame<'_>, area: Rect, dialog: &DialogView) {
    let width = area.width.saturating_sub(4).clamp(1, 64);
    let content_rows = usize::from(dialog.help.is_some())
        .saturating_add(dialog.entries.len().max(1))
        .saturating_add(2) as u16;
    let height = content_rows
        .min(12)
        .min(area.height.saturating_sub(2))
        .max(1);
    let dialog_area = Rect::new(
        area.x.saturating_add(area.width.saturating_sub(width) / 2),
        area.y
            .saturating_add(area.height.saturating_sub(height) / 2),
        width,
        height,
    );

    frame.render_widget(Clear, dialog_area);
    let mut lines: Vec<Line<'_>> = dialog
        .help
        .as_deref()
        .map(|help| {
            if dialog.entries.is_empty() {
                help.lines().map(Line::from).collect()
            } else {
                help.lines().next().map(Line::from).into_iter().collect()
            }
        })
        .unwrap_or_default();
    if dialog.entries.is_empty() && dialog.help.is_none() {
        lines.push(Line::from("No options available."));
    }
    lines.extend(dialog.entries.iter().enumerate().map(|(index, entry)| {
        let selected = dialog.interactive && index == dialog.selected;
        let text = match (&entry.action, &entry.detail) {
            (None, Some(detail)) => format!("disabled {}: {detail}", entry.label),
            (None, None) => format!("disabled {}", entry.label),
            _ if selected => format!("> {}", entry.label),
            _ => format!("  {}", entry.label),
        };
        let style = if selected {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        } else if entry.action.is_none() {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default()
        };
        Line::styled(text, style)
    }));

    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::Cyan))
                .title(Span::styled(
                    format!(" {} ", dialog.title),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )),
        ),
        dialog_area,
    );
}

struct ScreenLayout {
    header: Rect,
    transcript: Rect,
    status: Rect,
    composer: Rect,
    footer: Rect,
    show_context: bool,
}

fn screen_layout(area: Rect, running: bool) -> ScreenLayout {
    let show_header = area.height >= 8;
    let show_context = area.width >= 80 && area.height > 16;
    let show_footer = area.height >= 10;
    let show_status = running && area.height > 16;
    let composer_rows = if area.height < 8 { 2 } else { 3 };
    let chunks = Layout::vertical([
        Constraint::Length(u16::from(show_header)),
        Constraint::Min(1),
        Constraint::Length(u16::from(show_status)),
        Constraint::Length(composer_rows),
        Constraint::Length(u16::from(show_footer)),
    ])
    .split(area);

    ScreenLayout {
        header: chunks[0],
        transcript: chunks[1],
        status: chunks[2],
        composer: chunks[3],
        footer: chunks[4],
        show_context,
    }
}

fn render_header(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    state: &ViewState<'_>,
    show_context: bool,
) {
    let state_label = turn_state_label(state.turn_state, state.running);
    let mut left = vec![Span::styled(
        " agens ",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )];
    if show_context {
        left.push(Span::styled("  ", Style::default()));
        left.push(Span::styled(
            state.provider_model,
            Style::default().fg(Color::Gray),
        ));
        left.push(Span::styled("  ·  ", Style::default().fg(Color::DarkGray)));
        left.push(Span::styled(
            state.session,
            Style::default().fg(Color::Gray),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(left)), area);
    let state_width = state_label.len() as u16 + 1;
    if area.width > state_width {
        let state_area = Rect::new(area.right() - state_width, area.y, state_width, area.height);
        frame.render_widget(
            Paragraph::new(state_label)
                .style(Style::default().fg(turn_state_color(state.turn_state, state.running)))
                .alignment(Alignment::Right),
            state_area,
        );
    }
}

fn render_turn_status(frame: &mut ratatui::Frame<'_>, area: Rect, state: &ViewState<'_>) {
    let label = match (state.turn_state, state.active_tool) {
        (Some(TurnState::Dispatching), Some(tool)) => {
            format!(" {} Tool: {tool}", activity_marker(state))
        }
        _ => format!(
            " {} {}",
            activity_marker(state),
            turn_state_label(state.turn_state, state.running)
        ),
    };
    frame.render_widget(
        Paragraph::new(label)
            .style(Style::default().fg(turn_state_color(state.turn_state, state.running))),
        area,
    );
}

fn activity_marker(state: &ViewState<'_>) -> &'static str {
    match state.turn_state {
        Some(TurnState::Requesting) => "·",
        Some(TurnState::Streaming) => "~",
        Some(TurnState::Dispatching) => "*",
        Some(TurnState::Cancelled) => "·",
        Some(TurnState::Failed) => "!",
        _ if state.running => "~",
        _ => "·",
    }
}

fn turn_state_label(state: Option<TurnState>, running: bool) -> &'static str {
    match state {
        Some(TurnState::Requesting) => "Waiting",
        Some(TurnState::Streaming) => "Responding",
        Some(TurnState::Dispatching) => "Using tool",
        Some(TurnState::Cancelled) => "Cancelling",
        Some(TurnState::Failed) => "Failed",
        Some(TurnState::Completed) => "Completed",
        _ if running => "Working",
        _ => "Ready",
    }
}

fn turn_state_color(state: Option<TurnState>, running: bool) -> Color {
    match state {
        Some(TurnState::Failed) => Color::Red,
        Some(TurnState::Cancelled) => Color::Yellow,
        Some(TurnState::Dispatching) => Color::Magenta,
        Some(TurnState::Streaming) => Color::Cyan,
        _ if running => Color::Cyan,
        _ => Color::DarkGray,
    }
}

fn composer_metadata(input: &str) -> String {
    let lines = input.chars().filter(|character| *character == '\n').count() + 1;
    format!(" {lines} lines · {} chars ", input.chars().count())
}

fn footer_text(width: u16, state: &ViewState<'_>) -> String {
    if let Some(status) = state.status {
        return format!(" {status}");
    }
    let duration = state.turn_duration.map_or_else(String::new, |value| {
        if value.as_secs() > 0 {
            format!(" · {}s", value.as_secs())
        } else {
            format!(" · {}ms", value.as_millis())
        }
    });
    let usage = state.latest_usage.map_or_else(String::new, |usage| {
        let tokens = usage
            .total_tokens
            .map_or_else(|| "unavailable".into(), |value| value.to_string());
        let context = usage
            .context_window
            .map_or_else(|| "unavailable".into(), |value| value.to_string());
        format!(" · tokens {tokens} · context {context}")
    });
    let metrics = format!(
        " {}{duration}{usage}",
        turn_state_label(state.turn_state, state.running)
    );
    if width < 60 {
        format!("{metrics}  ·  Enter send  ·  Ctrl+C cancel/quit")
    } else {
        format!(
            "{metrics}  ·  Enter send  ·  Shift+Enter newline  ·  Ctrl+O output  ·  Ctrl+C cancel/quit  ·  PgUp/PgDn scroll  ·  End follow"
        )
    }
}

fn transcript_lines(entries: &[TranscriptEntry]) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for entry in entries {
        let (label, color, text, card) = match entry {
            TranscriptEntry::User(text) => ("USER", Color::Green, text, false),
            TranscriptEntry::Assistant(text) => ("ASSISTANT", Color::Cyan, text, false),
            TranscriptEntry::Reasoning(text) => ("THINKING", Color::Blue, text, false),
            TranscriptEntry::Error(text) => ("ERROR", Color::Red, text, true),
            TranscriptEntry::Info(text) => ("INFO", Color::Yellow, text, false),
            TranscriptEntry::Tool(text) => ("TOOL", Color::Magenta, text, true),
        };
        let label_style = Style::default().fg(color).add_modifier(Modifier::BOLD);
        if card {
            lines.push(Line::from(Span::styled(
                format!("  ┌ {label} "),
                label_style,
            )));
            lines.push(Line::from(vec![
                Span::styled("  │ ", Style::default().fg(color)),
                Span::raw(text.clone()),
            ]));
            if matches!(entry, TranscriptEntry::Error(_)) {
                lines.push(Line::from(Span::styled(
                    "  │ Action: retry the request or inspect the runtime error.",
                    Style::default().fg(Color::Yellow),
                )));
            }
            lines.push(Line::from(Span::styled("  └", Style::default().fg(color))));
        } else {
            lines.push(Line::from(vec![
                Span::styled("  │ ", Style::default().fg(color)),
                Span::styled(format!("{label:<9} "), label_style),
                Span::raw(text.clone()),
            ]));
        }
        lines.push(Line::default());
    }
    lines
}

fn transcript_rows(lines: &[Line<'_>], width: u16) -> usize {
    let width = usize::from(width.max(1));
    lines
        .iter()
        .map(|line| line.width().div_ceil(width).max(1))
        .sum()
}

fn cursor_position(input: &str, cursor: usize) -> (usize, usize) {
    let mut line = 0;
    let mut current_line = String::new();
    for character in input.chars().take(cursor) {
        if character == '\n' {
            line += 1;
            current_line.clear();
        } else {
            current_line.push(character);
        }
    }
    (line, Line::from(current_line).width())
}

fn saturating_u16(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

/// Renders the current TUI state. Rendering is deliberately independent of event handling.
pub trait Renderer {
    /// Draws one frame for the supplied TUI state.
    fn render(&mut self, state: ViewState<'_>) -> io::Result<()>;
}

/// Minimal terminal renderer for the runnable TUI command.
pub struct PlainRenderer;

impl Renderer for PlainRenderer {
    fn render(&mut self, state: ViewState<'_>) -> io::Result<()> {
        let mut stdout = io::stdout();
        write!(stdout, "\x1b[2J\x1b[HAgens\n\n")?;

        for entry in state.transcript {
            match entry {
                TranscriptEntry::User(text) => writeln!(stdout, "You: {text}\n")?,
                TranscriptEntry::Assistant(text) => writeln!(stdout, "Assistant: {text}\n")?,
                TranscriptEntry::Reasoning(text) => writeln!(stdout, "Reasoning: {text}\n")?,
                TranscriptEntry::Error(text) => writeln!(stdout, "Error: {text}\n")?,
                TranscriptEntry::Info(text) => writeln!(stdout, "{text}\n")?,
                TranscriptEntry::Tool(text) => writeln!(stdout, "Tool: {text}\n")?,
            }
        }

        if state.running {
            writeln!(stdout, "Working…")?;
        }
        write!(stdout, "> {}", state.input)?;
        stdout.flush()
    }
}

/// Small event engine shared by the terminal lifecycle and future TUI components.
pub struct Tui<E> {
    engine: E,
    input: String,
    input_cursor: usize,
    size: (u16, u16),
    running: bool,
    quit_armed: bool,
    transcript: Vec<TranscriptEntry>,
    following_bottom: bool,
    scroll_offset: u16,
    provider_model: String,
    session: String,
    turn_state: Option<TurnState>,
    active_tool: Option<String>,
    runtime_events: Vec<TuiRuntimeEvent>,
    turn_duration: Option<Duration>,
    latest_usage: Option<Usage>,
    status: Option<String>,
    completed_conversations: Vec<Conversation>,
    conversation: Option<Conversation>,
    collapsed_tool_outputs: BTreeSet<String>,
    dialog: Option<DialogView>,
    palette_entries: Vec<PaletteEntry>,
    palette_open: bool,
    palette_selected: usize,
}

impl<E> Tui<E>
where
    E: Engine,
{
    /// Creates a TUI event engine around an injected application engine handle.
    pub fn new(engine: E) -> Self {
        Self {
            engine,
            input: String::new(),
            input_cursor: 0,
            size: (0, 0),
            running: false,
            quit_armed: false,
            transcript: Vec::new(),
            following_bottom: true,
            scroll_offset: 0,
            provider_model: "provider / model".to_owned(),
            session: "new session".to_owned(),
            turn_state: None,
            active_tool: None,
            runtime_events: Vec::new(),
            turn_duration: None,
            latest_usage: None,
            status: None,
            completed_conversations: Vec::new(),
            conversation: None,
            collapsed_tool_outputs: BTreeSet::new(),
            dialog: None,
            palette_entries: Vec::new(),
            palette_open: false,
            palette_selected: 0,
        }
    }

    /// Handles one input or resize event without performing rendering or engine work.
    pub fn handle(&mut self, event: Event) -> Action {
        match event {
            Event::Resize { width, height } => {
                self.size = (width, height);
                self.quit_armed = false;
                self.clamp_palette_selection();
                Action::Render
            }
            Event::Key(key) => self.handle_key(key),
            Event::Paste(text) => {
                if self
                    .dialog
                    .as_ref()
                    .is_some_and(|dialog| dialog.interactive)
                {
                    Action::Render
                } else {
                    self.quit_armed = false;
                    self.status = None;
                    self.insert_text(&text);
                    Action::Render
                }
            }
        }
    }

    /// Returns the input buffer for composition and focused tests.
    pub fn input(&self) -> &str {
        &self.input
    }

    /// Returns the most recently received terminal size.
    pub fn size(&self) -> (u16, u16) {
        self.size
    }

    /// Gives the composition layer mutable access to the shared engine handle.
    pub fn engine(&mut self) -> &mut E {
        &mut self.engine
    }

    pub fn set_palette_entries(&mut self, entries: Vec<PaletteEntry>) {
        self.palette_entries = entries;
        self.clamp_palette_selection();
    }

    /// Updates active-turn state after the composition layer starts or finishes a turn.
    pub fn set_running(&mut self, running: bool) {
        self.running = running;
        if running {
            self.palette_open = false;
            self.quit_armed = false;
            self.turn_state = Some(TurnState::Requesting);
        } else if !matches!(
            self.turn_state,
            Some(TurnState::Failed | TurnState::Cancelled)
        ) {
            self.turn_state = None;
            self.active_tool = None;
        }
    }

    /// Supplies concise provider, model, and active-session context for the terminal surface.
    pub fn set_presentation(
        &mut self,
        provider: impl AsRef<str>,
        model: impl AsRef<str>,
        session: impl Into<String>,
    ) {
        self.provider_model = format!("{} / {}", provider.as_ref(), model.as_ref());
        self.session = session.into();
    }

    /// Adds a user prompt before the composition layer starts the shared runtime.
    pub fn begin_submission(&mut self, prompt: impl Into<String>) {
        self.palette_open = false;
        let prompt = prompt.into();
        self.status = None;
        if let Some(conversation) = self.conversation.take() {
            self.completed_conversations.push(conversation);
        }
        self.runtime_events.clear();
        self.turn_duration = None;
        self.latest_usage = None;
        self.transcript.push(TranscriptEntry::User(prompt.clone()));
        self.conversation = Some(Conversation::new(prompt));
        self.collapsed_tool_outputs.clear();
        self.set_running(true);
    }

    pub fn begin_route(&mut self) {
        self.status = None;
        self.palette_open = false;
        self.runtime_events.clear();
        self.turn_duration = None;
        self.latest_usage = None;
        self.dialog = None;
        self.running = true;
        self.turn_state = None;
        self.quit_armed = false;
    }

    pub fn apply_route_progress(&mut self, progress: TuiRouteProgress) {
        let (title, body) = match progress {
            TuiRouteProgress::BrowserUrl(url) => (
                "ChatGPT authentication",
                format!("Open {}", bounded_auth_text(&url, 512)),
            ),
            TuiRouteProgress::DeviceCode {
                verification_url,
                user_code,
            } => (
                "ChatGPT device authentication",
                format!(
                    "Open {}\nCode: {}",
                    bounded_auth_text(&verification_url, 512),
                    bounded_auth_text(&user_code, 64)
                ),
            ),
        };
        self.show_dialog(title, body);
    }

    /// Records a completed runtime result without exposing provider internals.
    pub fn finish_submission(&mut self, result: Result<String, String>) {
        let outcome = match result {
            Ok(output) => TuiProviderOutcome::Completed(output),
            Err(message) => TuiProviderOutcome::Failed {
                message,
                action: "Retry the request or inspect the runtime error.".into(),
            },
        };
        self.finish_provider_turn(outcome);
    }

    /// Adds a local session or lifecycle note to the visible conversation.
    pub fn add_info(&mut self, text: impl Into<String>) {
        let text = text.into();
        self.transcript.push(TranscriptEntry::Info(text.clone()));
        self.project_conversation(ConversationEvent::Info(text));
    }

    pub fn add_diagnostic(&mut self, text: impl AsRef<str>) {
        const MAX_DIAGNOSTICS: usize = 8;
        const MAX_DIAGNOSTIC_CHARS: usize = 160;

        let text = text
            .as_ref()
            .chars()
            .filter(|character| !character.is_control())
            .take(MAX_DIAGNOSTIC_CHARS)
            .collect::<String>();
        match self.dialog.as_mut() {
            Some(dialog)
                if dialog.title == "Extension diagnostics"
                    && dialog.help.as_deref().unwrap_or_default().lines().count()
                        < MAX_DIAGNOSTICS =>
            {
                let help = dialog.help.get_or_insert_default();
                help.push('\n');
                help.push_str(&text);
            }
            Some(dialog) if dialog.title == "Extension diagnostics" => {}
            _ => self.show_dialog("Extension diagnostics", text),
        }
    }

    pub fn apply_submission_outcome(&mut self, outcome: TuiSubmissionOutcome) -> Option<String> {
        self.palette_open = false;
        self.dialog = None;
        match outcome {
            TuiSubmissionOutcome::ProviderTurn { display, prompt } => {
                self.begin_submission(display);
                Some(prompt)
            }
            TuiSubmissionOutcome::LocalInfo(message) => {
                self.set_running(false);
                self.add_info(message);
                None
            }
            TuiSubmissionOutcome::LocalActionableError { message, action } => {
                self.set_running(false);
                self.show_dialog("Action required", format!("{message}\nAction: {action}"));
                None
            }
            TuiSubmissionOutcome::ResetSucceeded {
                message,
                presentation,
            } => {
                self.clear_transcript();
                self.apply_presentation(presentation);
                self.status = Some(message);
                None
            }
            TuiSubmissionOutcome::ContextChanged {
                message,
                presentation,
            } => {
                self.set_running(false);
                self.apply_presentation(presentation);
                self.add_info(message);
                None
            }
            TuiSubmissionOutcome::SessionResumed {
                message,
                presentation,
                messages,
            } => {
                if self.replace_history(&messages).is_err() {
                    self.show_dialog(
                        "Action required",
                        "Saved session history is invalid.\nAction: Choose another session.",
                    );
                    return None;
                }
                self.apply_presentation(presentation);
                self.status = Some(message);
                None
            }
            TuiSubmissionOutcome::Dialog(dialog) => {
                self.set_running(false);
                self.show_selection_dialog(dialog);
                None
            }
            TuiSubmissionOutcome::Quit => {
                self.set_running(false);
                None
            }
        }
    }

    pub fn finish_provider_turn(&mut self, outcome: TuiProviderOutcome) {
        match outcome {
            TuiProviderOutcome::Completed(output) => {
                if self
                    .conversation
                    .as_ref()
                    .is_some_and(|conversation| conversation.live_markdown.is_empty())
                {
                    self.project_conversation(ConversationEvent::MarkdownFinal(output.clone()));
                } else if let Some(conversation) = self.conversation.as_mut() {
                    conversation.final_markdown = Some(output.clone());
                }
                if !matches!(self.transcript.last(), Some(TranscriptEntry::Assistant(_))) {
                    self.transcript.push(TranscriptEntry::Assistant(output));
                }
                self.set_running(false);
            }
            TuiProviderOutcome::Failed { message, action } => {
                self.running = false;
                self.turn_state = Some(TurnState::Failed);
                self.active_tool = None;
                self.add_error(message, action);
            }
            TuiProviderOutcome::Cancelled { message, action } => {
                self.running = false;
                self.turn_state = Some(TurnState::Cancelled);
                self.active_tool = None;
                self.add_error(message, action);
            }
        }
    }

    /// Clears the current visible conversation for a new session.
    pub fn clear_transcript(&mut self) {
        self.transcript.clear();
        self.completed_conversations.clear();
        self.conversation = None;
        self.collapsed_tool_outputs.clear();
        self.set_running(false);
        self.turn_state = None;
        self.active_tool = None;
    }

    pub fn replace_history(
        &mut self,
        messages: &[agens_core::Message],
    ) -> Result<(), ConversationError> {
        let conversations = Conversation::from_messages(messages)?;
        self.transcript.clear();
        self.completed_conversations = conversations;
        self.conversation = None;
        self.collapsed_tool_outputs.clear();
        self.runtime_events.clear();
        self.turn_duration = None;
        self.latest_usage = None;
        self.following_bottom = true;
        self.scroll_offset = 0;
        self.set_running(false);
        self.turn_state = None;
        self.active_tool = None;
        Ok(())
    }

    /// Returns the visible conversation for composition and focused tests.
    pub fn transcript(&self) -> &[TranscriptEntry] {
        &self.transcript
    }

    /// Retains typed runtime metrics for the renderer without altering turn persistence.
    pub fn apply_runtime_event(&mut self, event: TuiRuntimeEvent) {
        match &event {
            TuiRuntimeEvent::TurnStarted => self.turn_state = Some(TurnState::Requesting),
            TuiRuntimeEvent::TurnEnded { status, duration } => {
                self.running = false;
                self.turn_state = Some(*status);
                self.turn_duration = *duration;
                self.active_tool = None;
            }
            TuiRuntimeEvent::Usage(usage) => self.latest_usage = Some(usage.clone()),
            TuiRuntimeEvent::Diff { lines, .. } => {
                self.project_conversation(ConversationEvent::Diff(lines.clone()));
            }
            TuiRuntimeEvent::ToolStarted { .. } | TuiRuntimeEvent::ToolEnded { .. } => {}
        }
        self.runtime_events.push(event);
    }

    /// Adds a typed event to the authoritative, lossless conversation projection.
    pub fn apply_conversation_event(
        &mut self,
        event: ConversationEvent,
    ) -> Result<(), ConversationError> {
        self.conversation
            .get_or_insert_with(|| Conversation::new(String::new()))
            .apply(event)
    }

    /// Opens a generic bounded dialog without changing the underlying conversation.
    pub fn show_dialog(&mut self, title: impl Into<String>, body: impl Into<String>) {
        self.dialog = Some(DialogView::informational(title.into(), body.into()));
    }

    pub fn show_selection_dialog(&mut self, dialog: DialogView) {
        self.palette_open = false;
        self.quit_armed = false;
        self.dialog = Some(dialog);
    }

    pub fn runtime_events(&self) -> &[TuiRuntimeEvent] {
        &self.runtime_events
    }

    /// Returns an immutable snapshot for a renderer.
    pub fn view(&self) -> ViewState<'_> {
        ViewState {
            input: &self.input,
            size: self.size,
            running: self.running,
            quit_armed: self.quit_armed,
            transcript: &self.transcript,
            following_bottom: self.following_bottom,
            scroll_offset: self.scroll_offset,
            provider_model: &self.provider_model,
            session: &self.session,
            turn_state: self.turn_state,
            active_tool: self.active_tool.as_deref(),
            input_cursor: self.input_cursor,
            runtime_events: &self.runtime_events,
            turn_duration: self.turn_duration,
            latest_usage: self.latest_usage.as_ref(),
            status: self.status.as_deref(),
            conversation: self.conversation.as_ref(),
            completed_conversations: &self.completed_conversations,
            collapsed_tool_outputs: &self.collapsed_tool_outputs,
            dialog: self.dialog.as_ref(),
            palette: self.palette_open.then_some(PaletteView {
                entries: &self.palette_entries,
                selected: self.palette_selected,
            }),
        }
    }

    pub const fn following_bottom(&self) -> bool {
        self.following_bottom
    }

    /// Applies ordered runtime progress without changing completed persistence semantics.
    pub fn apply_progress(&mut self, event: TurnEvent) {
        match event {
            TurnEvent::ProviderPart(MessagePart::Text(delta)) => {
                self.project_conversation(ConversationEvent::MarkdownDelta(delta.clone()));
                self.turn_state = Some(TurnState::Streaming);
                match self.transcript.last_mut() {
                    Some(TranscriptEntry::Assistant(text)) => text.push_str(&delta),
                    _ => self.transcript.push(TranscriptEntry::Assistant(delta)),
                }
            }
            TurnEvent::ProviderPart(MessagePart::Reasoning(delta)) => {
                self.project_conversation(ConversationEvent::ReasoningDelta(delta.clone()));
                match self.transcript.last_mut() {
                    Some(TranscriptEntry::Reasoning(text)) => text.push_str(&delta),
                    _ => self.transcript.push(TranscriptEntry::Reasoning(delta)),
                }
            }
            TurnEvent::ToolCallRequested { id, name, input } => {
                self.project_conversation(ConversationEvent::ToolCall {
                    call_id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                });
                self.turn_state = Some(TurnState::Dispatching);
                self.active_tool = Some(name.clone());
                self.transcript
                    .push(TranscriptEntry::Tool(format!("{name} started")));
            }
            TurnEvent::ToolResult(MessagePart::ToolResult {
                tool_call_id,
                content,
                is_error,
            }) => {
                self.project_conversation(ConversationEvent::ToolResult {
                    call_id: tool_call_id.clone(),
                    output: content.clone(),
                    is_error,
                });
                let name = self
                    .transcript
                    .iter()
                    .rev()
                    .find_map(|entry| match entry {
                        TranscriptEntry::Tool(value) => value.strip_suffix(" started"),
                        _ => None,
                    })
                    .unwrap_or("tool");
                let outcome = if is_error { "failed" } else { "completed" };
                self.transcript.push(TranscriptEntry::Tool(format!(
                    "{name} {outcome}: {content}"
                )));
            }
            TurnEvent::StateChanged(TurnState::Completed) => self.set_running(false),
            TurnEvent::StateChanged(state @ (TurnState::Cancelled | TurnState::Failed)) => {
                self.running = false;
                self.turn_state = Some(state);
                self.active_tool = None;
            }
            TurnEvent::StateChanged(state) => self.turn_state = Some(state),
            _ => {}
        }
        if self.following_bottom {
            self.scroll_offset = 0;
        }
    }

    fn handle_key(&mut self, key: Key) -> Action {
        if key != Key::CtrlC {
            self.quit_armed = false;
        }
        if !matches!(key, Key::PageUp | Key::PageDown) {
            self.status = None;
        }

        if self
            .dialog
            .as_ref()
            .is_some_and(|dialog| dialog.interactive)
        {
            return self.handle_selection_dialog_key(key);
        }

        if let Some(action) = self.handle_composer_key(key) {
            return action;
        }

        match key {
            Key::CtrlO => {
                self.toggle_tool_output_expansion();
                Action::Render
            }
            Key::PageUp => {
                self.following_bottom = false;
                self.scroll_offset = self.scroll_offset.saturating_add(5);
                Action::Render
            }
            Key::PageDown => {
                self.scroll_offset = self.scroll_offset.saturating_sub(5);
                if self.scroll_offset == 0 {
                    self.following_bottom = true;
                }
                Action::Render
            }
            Key::Up if self.palette_open => {
                let count = palette_matches(&self.palette_entries, &self.input).len();
                if count > 0 {
                    self.palette_selected = (self.palette_selected + count - 1) % count;
                }
                Action::Render
            }
            Key::Down if self.palette_open => {
                let count = palette_matches(&self.palette_entries, &self.input).len();
                if count > 0 {
                    self.palette_selected = (self.palette_selected + 1) % count;
                }
                Action::Render
            }
            Key::Tab if self.palette_open => {
                self.complete_palette_selection();
                Action::Render
            }
            Key::Up | Key::Down | Key::Tab => Action::Render,
            Key::Enter if self.input.is_empty() => Action::Render,
            Key::Enter if self.running => {
                self.transcript.push(TranscriptEntry::Info(
                    "A response is already in progress.".into(),
                ));
                Action::Render
            }
            Key::Enter => {
                if self.palette_open {
                    if let Some(route_id) = self.selected_palette_dialog() {
                        self.palette_open = false;
                        self.input.clear();
                        self.input_cursor = 0;
                        return Action::OpenDialog(route_id);
                    }
                    self.complete_palette_selection();
                }
                self.palette_open = false;
                self.input_cursor = 0;
                Action::Submit(std::mem::take(&mut self.input))
            }
            Key::Escape if self.palette_open => {
                self.palette_open = false;
                Action::Render
            }
            Key::Escape if self.running => self.cancel_running(),
            Key::Escape if self.dialog.is_some() => {
                self.dialog = None;
                Action::Render
            }
            Key::Escape => Action::Render,
            Key::CtrlC if self.running => self.cancel_running(),
            Key::CtrlC if !self.input.is_empty() => {
                self.input.clear();
                self.input_cursor = 0;
                self.palette_open = false;
                Action::Render
            }
            Key::CtrlC if self.quit_armed => Action::Quit,
            Key::CtrlC => {
                self.quit_armed = true;
                Action::Render
            }
            _ => unreachable!("composer keys are handled before global keys"),
        }
    }

    fn handle_composer_key(&mut self, key: Key) -> Option<Action> {
        let cursor = self.input_cursor;
        match key {
            Key::Char(character) => self.insert_text(&character.to_string()),
            Key::ShiftEnter => self.insert_text("\n"),
            Key::Backspace if cursor > 0 => self.replace_chars(cursor - 1, cursor, ""),
            Key::Delete => self.replace_chars(cursor, cursor.saturating_add(1), ""),
            Key::DeletePreviousWord => {
                self.replace_chars(previous_word_boundary(&self.input, cursor), cursor, "");
            }
            Key::DeleteToLineStart => {
                self.replace_chars(line_start(&self.input, cursor), cursor, "");
            }
            Key::DeleteToLineEnd => {
                self.replace_chars(cursor, line_end(&self.input, cursor), "");
            }
            Key::Left => self.input_cursor = cursor.saturating_sub(1),
            Key::Right => {
                self.input_cursor = cursor.saturating_add(1).min(self.input.chars().count());
            }
            Key::PreviousWord => {
                self.input_cursor = previous_word_boundary(&self.input, cursor);
            }
            Key::NextWord => self.input_cursor = next_word_boundary(&self.input, cursor),
            Key::Home => self.input_cursor = line_start(&self.input, cursor),
            Key::End => {
                self.following_bottom = true;
                self.scroll_offset = 0;
                self.input_cursor = line_end(&self.input, cursor);
            }
            Key::Backspace => {}
            _ => return None,
        }

        self.clamp_palette_selection();
        Some(Action::Render)
    }

    fn insert_text(&mut self, text: &str) {
        if self
            .dialog
            .as_ref()
            .is_some_and(|dialog| dialog.title == "Extension diagnostics")
        {
            self.dialog = None;
        }
        self.replace_chars(self.input_cursor, self.input_cursor, text);
        if !self.running && self.input == "/" {
            self.palette_open = true;
            self.palette_selected = 0;
        }
        self.clamp_palette_selection();
    }

    fn replace_chars(&mut self, start: usize, end: usize, replacement: &str) {
        let character_count = self.input.chars().count();
        let start = start.min(character_count);
        let end = end.min(character_count).max(start);
        let start_byte = byte_index(&self.input, start);
        let end_byte = byte_index(&self.input, end);
        self.input.replace_range(start_byte..end_byte, replacement);
        self.input_cursor = start + replacement.chars().count();
    }

    fn cancel_running(&mut self) -> Action {
        self.palette_open = false;
        self.engine.cancel();
        self.quit_armed = false;
        self.turn_state = Some(TurnState::Cancelled);
        Action::Cancel
    }

    fn handle_selection_dialog_key(&mut self, key: Key) -> Action {
        match key {
            Key::Up | Key::Down => {
                let Some(dialog) = self.dialog.as_mut() else {
                    return Action::Render;
                };
                let enabled = dialog
                    .entries
                    .iter()
                    .enumerate()
                    .filter_map(|(index, entry)| entry.action.as_ref().map(|_| index))
                    .collect::<Vec<_>>();
                if let Some(position) = enabled.iter().position(|index| *index == dialog.selected) {
                    let next = if key == Key::Up {
                        (position + enabled.len() - 1) % enabled.len()
                    } else {
                        (position + 1) % enabled.len()
                    };
                    dialog.selected = enabled[next];
                }
                Action::Render
            }
            Key::Enter => {
                let action = self.dialog.as_ref().and_then(|dialog| {
                    dialog
                        .entries
                        .get(dialog.selected)
                        .and_then(|entry| entry.action.clone())
                });
                match action {
                    Some(DialogEntryAction::Dispatch(action_id)) => {
                        self.dialog = None;
                        Action::DialogAction(action_id)
                    }
                    Some(DialogEntryAction::Cancel) => {
                        self.dialog = None;
                        Action::Render
                    }
                    None => Action::Render,
                }
            }
            Key::Escape | Key::CtrlC => {
                self.dialog = None;
                Action::Render
            }
            _ => Action::Render,
        }
    }

    fn clamp_palette_selection(&mut self) {
        if !self.palette_open {
            return;
        }
        if !self.input.starts_with('/') {
            self.palette_open = false;
            return;
        }
        let count = palette_matches(&self.palette_entries, &self.input).len();
        self.palette_selected = self.palette_selected.min(count.saturating_sub(1));
    }

    fn complete_palette_selection(&mut self) {
        let matches = palette_matches(&self.palette_entries, &self.input);
        let Some(entry) = matches.get(self.palette_selected) else {
            return;
        };
        let invocation = self.input.strip_prefix('/').unwrap_or(&self.input);
        let arguments = invocation
            .find(char::is_whitespace)
            .map_or("", |index| invocation[index..].trim());
        self.input = if arguments.is_empty() {
            format!("/{} ", entry.name)
        } else {
            format!("/{} {arguments}", entry.name)
        };
        self.input_cursor = self.input.chars().count();
        self.palette_selected = 0;
    }

    fn selected_palette_dialog(&self) -> Option<String> {
        let invocation = self.input.strip_prefix('/').unwrap_or(&self.input);
        let arguments = invocation
            .find(char::is_whitespace)
            .map_or("", |index| invocation[index..].trim());
        if !arguments.is_empty() {
            return None;
        }

        palette_matches(&self.palette_entries, &self.input)
            .get(self.palette_selected)
            .and_then(|entry| entry.dialog_id.clone())
    }

    fn toggle_tool_output_expansion(&mut self) {
        let completed_call_ids = self
            .completed_conversations
            .iter()
            .chain(self.conversation.iter())
            .flat_map(|conversation| &conversation.tool_batches)
            .flat_map(|batch| &batch.calls)
            .filter(|call| call.result.is_some())
            .map(|call| call.call_id.clone())
            .collect::<Vec<_>>();

        if completed_call_ids.is_empty() {
            return;
        }

        if completed_call_ids
            .iter()
            .all(|call_id| !self.collapsed_tool_outputs.contains(call_id))
        {
            self.collapsed_tool_outputs.extend(completed_call_ids);
        } else {
            for call_id in completed_call_ids {
                self.collapsed_tool_outputs.remove(&call_id);
            }
        }
    }

    fn project_conversation(&mut self, event: ConversationEvent) {
        if self.apply_conversation_event(event).is_err() {
            self.conversation
                .as_mut()
                .expect("conversation is initialized before projection")
                .errors
                .push(ActionableError {
                    message: "Conversation event could not be projected.".into(),
                    action: "Inspect the runtime error and retry the request.".into(),
                });
        }
    }

    fn add_error(&mut self, message: String, action: String) {
        self.project_conversation(ConversationEvent::Error { message, action });
        let message = self
            .conversation
            .as_ref()
            .and_then(|conversation| conversation.errors.last())
            .map_or_else(
                || "Runtime request failed.".into(),
                |error| error.message.clone(),
            );
        self.transcript.push(TranscriptEntry::Error(message));
    }

    fn apply_presentation(&mut self, presentation: TuiPresentation) {
        self.set_presentation(
            presentation.provider,
            presentation.model,
            presentation.session,
        );
    }
}

fn palette_matches<'a>(entries: &'a [PaletteEntry], input: &str) -> Vec<&'a PaletteEntry> {
    let prefix = input
        .strip_prefix('/')
        .unwrap_or_default()
        .split(char::is_whitespace)
        .next()
        .unwrap_or_default();
    entries
        .iter()
        .filter(|entry| entry.name.starts_with(prefix))
        .collect()
}

fn byte_index(input: &str, character_index: usize) -> usize {
    input
        .char_indices()
        .nth(character_index)
        .map_or(input.len(), |(index, _)| index)
}

fn line_start(input: &str, cursor: usize) -> usize {
    input
        .chars()
        .take(cursor)
        .enumerate()
        .filter_map(|(index, character)| (character == '\n').then_some(index + 1))
        .last()
        .unwrap_or_default()
}

fn line_end(input: &str, cursor: usize) -> usize {
    input
        .chars()
        .skip(cursor)
        .position(|character| character == '\n')
        .map_or_else(|| input.chars().count(), |offset| cursor + offset)
}

fn previous_word_boundary(input: &str, cursor: usize) -> usize {
    let mut last_word_start = 0;
    let mut found_word = false;
    let mut in_word = false;

    for (index, character) in input.chars().take(cursor).enumerate() {
        if character.is_whitespace() {
            in_word = false;
        } else if !in_word {
            last_word_start = index;
            found_word = true;
            in_word = true;
        }
    }

    if found_word { last_word_start } else { 0 }
}

fn next_word_boundary(input: &str, cursor: usize) -> usize {
    let mut in_word = false;

    for (offset, character) in input.chars().skip(cursor).enumerate() {
        if character.is_whitespace() {
            if in_word {
                return cursor + offset;
            }
        } else {
            in_word = true;
        }
    }

    input.chars().count()
}

fn bounded_auth_text(value: &str, limit: usize) -> String {
    bounded_dialog_text(value, limit)
}

fn bounded_dialog_text(value: &str, limit: usize) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .take(limit)
        .collect()
}

/// Owns raw-mode and alternate-screen restoration for an interactive terminal session.
pub struct Terminal {
    control: CrosstermControl,
    guard: TerminalModeGuard,
}

impl Terminal {
    /// Enters the terminal modes required by the TUI.
    pub fn enter() -> io::Result<Self> {
        let mut control = CrosstermControl {
            stdout: io::stdout(),
        };
        let guard = TerminalModeGuard::enter(&mut control)?;
        Ok(Self { control, guard })
    }

    /// Waits up to `timeout` for a terminal event relevant to the TUI engine.
    pub fn poll(&mut self, timeout: Duration) -> io::Result<Option<Event>> {
        if !event::poll(timeout)? {
            return Ok(None);
        }

        Ok(map_event(event::read()?))
    }

    /// Restores the main screen and normal terminal mode. It is safe to call repeatedly.
    pub fn restore(&mut self) -> io::Result<()> {
        self.guard.restore(&mut self.control)
    }
}

struct CrosstermControl {
    stdout: Stdout,
}

impl TerminalControl for CrosstermControl {
    fn apply(&mut self, operation: TerminalOperation) -> io::Result<()> {
        match operation {
            TerminalOperation::EnableRaw => crossterm_terminal::enable_raw_mode(),
            TerminalOperation::DisableRaw => crossterm_terminal::disable_raw_mode(),
            TerminalOperation::EnterAlternate => {
                execute!(self.stdout, EnterAlternateScreen).map(|_| ())
            }
            TerminalOperation::LeaveAlternate => {
                execute!(self.stdout, LeaveAlternateScreen).map(|_| ())
            }
            TerminalOperation::EnableMouse => execute!(self.stdout, EnableMouseCapture).map(|_| ()),
            TerminalOperation::DisableMouse => {
                execute!(self.stdout, DisableMouseCapture).map(|_| ())
            }
            TerminalOperation::EnableKeyboardEnhancement => execute!(
                self.stdout,
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            )
            .map(|_| ()),
            TerminalOperation::DisableKeyboardEnhancement => {
                execute!(self.stdout, PopKeyboardEnhancementFlags).map(|_| ())
            }
            TerminalOperation::EnablePaste => {
                execute!(self.stdout, EnableBracketedPaste).map(|_| ())
            }
            TerminalOperation::DisablePaste => {
                execute!(self.stdout, DisableBracketedPaste).map(|_| ())
            }
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

trait RuntimeTerminal {
    fn poll(&mut self, timeout: Duration) -> io::Result<Option<Event>>;
}

impl RuntimeTerminal for Terminal {
    fn poll(&mut self, timeout: Duration) -> io::Result<Option<Event>> {
        Self::poll(self, timeout)
    }
}

/// Runs a terminal event loop and hands rendering to the caller-owned renderer.
pub fn run<E, R>(tui: &mut Tui<E>, renderer: &mut R) -> io::Result<()>
where
    E: Engine,
    R: Renderer,
{
    run_with_runtime_terminal(tui, renderer, Terminal::enter()?)
}

fn run_with_runtime_terminal<E, R, T>(
    tui: &mut Tui<E>,
    renderer: &mut R,
    mut terminal: T,
) -> io::Result<()>
where
    E: Engine,
    R: Renderer,
    T: RuntimeTerminal,
{
    renderer.render(tui.view())?;

    loop {
        let Some(event) = terminal.poll(Duration::from_millis(100))? else {
            continue;
        };

        match tui.handle(event) {
            Action::Quit => return Ok(()),
            Action::Render
            | Action::Submit(_)
            | Action::OpenDialog(_)
            | Action::DialogAction(_)
            | Action::Cancel => renderer.render(tui.view())?,
        }
    }
}

/// Runs the terminal loop while sending prompt submissions through the injected shared runtime.
pub fn run_with_submit<E, R, F>(tui: &mut Tui<E>, renderer: &mut R, submit: F) -> io::Result<()>
where
    E: Engine + Send,
    R: Renderer,
    F: Fn(String) -> Result<String, String> + Send + Sync + 'static,
{
    let submit = Arc::new(submit);
    let (sender, receiver) = mpsc::channel();
    let mut terminal = Terminal::enter()?;
    renderer.render(tui.view())?;

    loop {
        while let Ok(result) = receiver.try_recv() {
            tui.finish_submission(result);
            renderer.render(tui.view())?;
        }

        let Some(event) = terminal.poll(Duration::from_millis(100))? else {
            continue;
        };

        match tui.handle(event) {
            Action::Quit => return Ok(()),
            Action::Submit(prompt) => {
                tui.begin_submission(prompt.clone());
                let submit = Arc::clone(&submit);
                let sender = sender.clone();
                thread::spawn(move || {
                    let _ = sender.send(submit(prompt));
                });
                renderer.render(tui.view())?;
            }
            Action::Render | Action::OpenDialog(_) | Action::DialogAction(_) | Action::Cancel => {
                renderer.render(tui.view())?
            }
        }
    }
}

/// Runs the production fullscreen Ratatui surface and restores the terminal on every exit path.
pub fn run_with_default_submit<E, F>(tui: &mut Tui<E>, submit: F) -> io::Result<()>
where
    E: Engine + Send,
    F: Fn(String) -> Result<String, String> + Send + Sync + 'static,
{
    let terminal = RatatuiTerminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut renderer = RatatuiRenderer::new(terminal);
    run_with_submit(tui, &mut renderer, submit)
}

/// Runs a submit worker that can forward ordered runtime events while it is active.
pub fn run_with_default_progress_submit<E, R, F>(
    tui: &mut Tui<E>,
    route: R,
    submit: F,
) -> io::Result<()>
where
    E: Engine + Send,
    R: Fn(TuiRouteRequest, mpsc::Sender<TuiRouteProgress>) -> TuiSubmissionOutcome
        + Send
        + Sync
        + 'static,
    F: Fn(String, mpsc::Sender<TurnEvent>, BridgeTx<TuiRuntimeEvent>) -> TuiProviderOutcome
        + Send
        + Sync
        + 'static,
{
    let route = Arc::new(route);
    let submit = Arc::new(submit);
    let (sender, receiver) = mpsc::channel();
    let (completion_sender, completion_receiver) = mpsc::channel();
    let (route_sender, route_receiver) = mpsc::channel();
    let (route_progress_sender, route_progress_receiver) = mpsc::channel();
    let (metrics_sender, metrics_receiver) = BridgeTx::bounded(128);
    let mut runtime_terminal = Terminal::enter()?;
    let terminal = RatatuiTerminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut renderer = RatatuiRenderer::new(terminal);
    renderer.render(tui.view())?;

    loop {
        for _ in 0..32 {
            let Ok(envelope) = metrics_receiver.try_recv() else {
                break;
            };
            tui.apply_runtime_event(envelope.into_parts().1);
        }
        while let Ok(event) = receiver.try_recv() {
            tui.apply_progress(event);
        }
        while let Ok(progress) = route_progress_receiver.try_recv() {
            tui.apply_route_progress(progress);
        }
        while let Ok(outcome) = completion_receiver.try_recv() {
            tui.finish_provider_turn(outcome);
        }
        while let Ok(outcome) = route_receiver.try_recv() {
            let quit = matches!(outcome, TuiSubmissionOutcome::Quit);
            let Some(prompt) = tui.apply_submission_outcome(outcome) else {
                if quit {
                    return Ok(());
                }
                continue;
            };
            let submit = Arc::clone(&submit);
            let sender = sender.clone();
            let metrics = metrics_sender.clone();
            let completion_sender = completion_sender.clone();
            thread::spawn(move || {
                let outcome = submit(prompt, sender, metrics);
                let _ = completion_sender.send(outcome);
            });
        }
        renderer.render(tui.view())?;
        let Some(event) = runtime_terminal.poll(Duration::from_millis(25))? else {
            continue;
        };
        match tui.handle(event) {
            Action::Quit => return Ok(()),
            Action::Submit(prompt) => {
                tui.begin_route();
                let route = Arc::clone(&route);
                let route_sender = route_sender.clone();
                let progress = route_progress_sender.clone();
                thread::spawn(move || {
                    let outcome = route(TuiRouteRequest::Input(prompt), progress);
                    let _ = route_sender.send(outcome);
                });
            }
            Action::OpenDialog(route_id) => {
                let outcome = route(
                    TuiRouteRequest::OpenDialog(route_id),
                    route_progress_sender.clone(),
                );
                let _ = route_sender.send(outcome);
            }
            Action::DialogAction(action_id) => {
                tui.begin_route();
                let route = Arc::clone(&route);
                let route_sender = route_sender.clone();
                let progress = route_progress_sender.clone();
                thread::spawn(move || {
                    let outcome = route(TuiRouteRequest::DialogAction(action_id), progress);
                    let _ = route_sender.send(outcome);
                });
            }
            Action::Render | Action::Cancel => {}
        }
    }
}

fn map_event(event: CrosstermEvent) -> Option<Event> {
    match event {
        CrosstermEvent::Resize(width, height) => Some(Event::Resize { width, height }),
        CrosstermEvent::Key(key) if key.kind == KeyEventKind::Press => map_key(key),
        CrosstermEvent::Paste(text) => Some(Event::Paste(text)),
        _ => None,
    }
}

fn map_key(event: KeyEvent) -> Option<Event> {
    let key = match (event.code, event.modifiers) {
        (KeyCode::Char('c' | 'C'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            Key::CtrlC
        }
        (KeyCode::Char('o' | 'O'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            Key::CtrlO
        }
        (KeyCode::Char('w' | 'W'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            Key::DeletePreviousWord
        }
        (KeyCode::Char('u' | 'U'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            Key::DeleteToLineStart
        }
        (KeyCode::Char('k' | 'K'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            Key::DeleteToLineEnd
        }
        (KeyCode::Char('a' | 'A'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            Key::Home
        }
        (KeyCode::Char('e' | 'E'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            Key::End
        }
        (KeyCode::Char('b' | 'B'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            Key::Left
        }
        (KeyCode::Char('f' | 'F'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            Key::Right
        }
        (KeyCode::Char('d' | 'D'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            Key::Delete
        }
        (KeyCode::Char('b' | 'B'), modifiers) if modifiers.contains(KeyModifiers::ALT) => {
            Key::PreviousWord
        }
        (KeyCode::Char('f' | 'F'), modifiers) if modifiers.contains(KeyModifiers::ALT) => {
            Key::NextWord
        }
        (KeyCode::Char(character), modifiers)
            if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT =>
        {
            Key::Char(character)
        }
        (KeyCode::Backspace, _) => Key::Backspace,
        (KeyCode::Enter, modifiers) if modifiers.contains(KeyModifiers::SHIFT) => Key::ShiftEnter,
        (KeyCode::Enter, _) => Key::Enter,
        (KeyCode::Left, modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            Key::PreviousWord
        }
        (KeyCode::Right, modifiers) if modifiers.contains(KeyModifiers::CONTROL) => Key::NextWord,
        (KeyCode::Left, _) => Key::Left,
        (KeyCode::Right, _) => Key::Right,
        (KeyCode::Delete, _) => Key::Delete,
        (KeyCode::Home, _) => Key::Home,
        (KeyCode::End, _) => Key::End,
        (KeyCode::PageUp, _) => Key::PageUp,
        (KeyCode::PageDown, _) => Key::PageDown,
        (KeyCode::Up, _) => Key::Up,
        (KeyCode::Down, _) => Key::Down,
        (KeyCode::Tab, _) => Key::Tab,
        (KeyCode::Esc, _) => Key::Escape,
        _ => return None,
    };

    Some(Event::Key(key))
}

#[cfg(test)]
mod runtime_tests {
    use super::*;
    use crate::terminal::{TerminalControl, TerminalModeGuard, TerminalOperation};
    use std::{cell::RefCell, rc::Rc};

    #[derive(Default)]
    struct RecordingControl {
        calls: Rc<RefCell<Vec<TerminalOperation>>>,
    }

    impl TerminalControl for RecordingControl {
        fn apply(&mut self, operation: TerminalOperation) -> io::Result<()> {
            self.calls.borrow_mut().push(operation);
            Ok(())
        }
    }

    struct GuardedRuntime {
        guard: TerminalModeGuard,
        control: RecordingControl,
        input_error: io::ErrorKind,
    }

    impl GuardedRuntime {
        fn new(input_error: io::ErrorKind) -> (Self, Rc<RefCell<Vec<TerminalOperation>>>) {
            let mut control = RecordingControl::default();
            let calls = Rc::clone(&control.calls);
            let guard = TerminalModeGuard::enter(&mut control).unwrap();

            (
                Self {
                    guard,
                    control,
                    input_error,
                },
                calls,
            )
        }
    }

    impl RuntimeTerminal for GuardedRuntime {
        fn poll(&mut self, _: Duration) -> io::Result<Option<Event>> {
            Err(io::Error::from(self.input_error))
        }
    }

    impl Drop for GuardedRuntime {
        fn drop(&mut self) {
            let _ = self.guard.restore(&mut self.control);
        }
    }

    struct NoopEngine;

    impl Engine for NoopEngine {
        fn cancel(&mut self) {}
    }

    struct FailingRenderer {
        fail_on_render: usize,
        renders: usize,
    }

    impl Renderer for FailingRenderer {
        fn render(&mut self, _: ViewState<'_>) -> io::Result<()> {
            self.renders += 1;
            if self.renders == self.fail_on_render {
                return Err(io::Error::other("injected renderer failure"));
            }

            Ok(())
        }
    }

    fn expected_terminal_calls() -> Vec<TerminalOperation> {
        vec![
            TerminalOperation::EnableRaw,
            TerminalOperation::EnterAlternate,
            TerminalOperation::EnableMouse,
            TerminalOperation::EnableKeyboardEnhancement,
            TerminalOperation::EnablePaste,
            TerminalOperation::DisablePaste,
            TerminalOperation::DisableKeyboardEnhancement,
            TerminalOperation::DisableMouse,
            TerminalOperation::LeaveAlternate,
            TerminalOperation::DisableRaw,
        ]
    }

    #[test]
    fn runtime_restores_each_mode_once_after_renderer_failure() {
        let (terminal, calls) = GuardedRuntime::new(io::ErrorKind::Other);
        let mut tui = Tui::new(NoopEngine);
        let mut renderer = FailingRenderer {
            fail_on_render: 1,
            renders: 0,
        };

        let error = run_with_runtime_terminal(&mut tui, &mut renderer, terminal).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(*calls.borrow(), expected_terminal_calls());
    }

    #[test]
    fn runtime_restores_each_mode_once_after_input_poll_or_read_failure() {
        for input_error in [io::ErrorKind::TimedOut, io::ErrorKind::UnexpectedEof] {
            let (terminal, calls) = GuardedRuntime::new(input_error);
            let mut tui = Tui::new(NoopEngine);
            let mut renderer = FailingRenderer {
                fail_on_render: 2,
                renders: 0,
            };

            let error = run_with_runtime_terminal(&mut tui, &mut renderer, terminal).unwrap_err();

            assert_eq!(error.kind(), input_error);
            assert_eq!(*calls.borrow(), expected_terminal_calls());
        }
    }

    #[test]
    fn maps_control_o_to_tool_output_toggle() {
        assert_eq!(
            map_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL)),
            Some(Event::Key(Key::CtrlO))
        );
    }

    #[test]
    fn maps_palette_navigation_keys() {
        for (code, key) in [
            (KeyCode::Up, Key::Up),
            (KeyCode::Down, Key::Down),
            (KeyCode::Tab, Key::Tab),
        ] {
            assert_eq!(
                map_key(KeyEvent::new(code, KeyModifiers::NONE)),
                Some(Event::Key(key))
            );
        }
    }

    fn press<E: Engine>(tui: &mut Tui<E>, code: KeyCode, modifiers: KeyModifiers) -> Action {
        let event = map_key(crossterm::event::KeyEvent::new(code, modifiers)).unwrap();
        tui.handle(event)
    }

    #[test]
    fn maps_readline_crossterm_keys_to_composer_actions() {
        let ctrl = KeyModifiers::CONTROL;
        let alt = KeyModifiers::ALT;
        for (code, modifiers, expected) in [
            (KeyCode::Backspace, KeyModifiers::NONE, Key::Backspace),
            (KeyCode::Delete, KeyModifiers::NONE, Key::Delete),
            (KeyCode::Home, KeyModifiers::NONE, Key::Home),
            (KeyCode::End, KeyModifiers::NONE, Key::End),
            (KeyCode::Left, KeyModifiers::NONE, Key::Left),
            (KeyCode::Right, KeyModifiers::NONE, Key::Right),
            (KeyCode::Enter, KeyModifiers::NONE, Key::Enter),
            (KeyCode::Enter, KeyModifiers::SHIFT, Key::ShiftEnter),
            (KeyCode::Char('w'), ctrl, Key::DeletePreviousWord),
            (KeyCode::Char('u'), ctrl, Key::DeleteToLineStart),
            (KeyCode::Char('k'), ctrl, Key::DeleteToLineEnd),
            (KeyCode::Char('a'), ctrl, Key::Home),
            (KeyCode::Char('e'), ctrl, Key::End),
            (KeyCode::Char('b'), ctrl, Key::Left),
            (KeyCode::Char('f'), ctrl, Key::Right),
            (KeyCode::Char('b'), alt, Key::PreviousWord),
            (KeyCode::Char('f'), alt, Key::NextWord),
            (KeyCode::Left, ctrl, Key::PreviousWord),
            (KeyCode::Right, ctrl, Key::NextWord),
            (KeyCode::Char('d'), ctrl, Key::Delete),
            (KeyCode::Char('c'), ctrl, Key::CtrlC),
            (KeyCode::Char('o'), ctrl, Key::CtrlO),
        ] {
            let event = crossterm::event::KeyEvent::new(code, modifiers);
            assert_eq!(map_key(event), Some(Event::Key(expected)));
            assert_eq!(
                Tui::new(NoopEngine).handle(map_key(event).unwrap()),
                Action::Render
            );
        }
    }

    #[test]
    fn real_key_events_edit_unicode_multiline_text_without_changing_submission_semantics() {
        let mut tui = Tui::new(NoopEngine);
        tui.handle(map_event(CrosstermEvent::Paste("café 🙂\nsecond line".into())).unwrap());

        press(&mut tui, KeyCode::Home, KeyModifiers::NONE);
        press(&mut tui, KeyCode::Left, KeyModifiers::CONTROL);
        assert_eq!(tui.view().input_cursor, 5);
        press(&mut tui, KeyCode::Right, KeyModifiers::CONTROL);
        assert_eq!(tui.view().input_cursor, 6);

        let mut tui = Tui::new(NoopEngine);
        tui.handle(map_event(CrosstermEvent::Paste("café 🙂\nsecond line".into())).unwrap());
        press(&mut tui, KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert_eq!(tui.input(), "café 🙂\nsecond ");
        press(&mut tui, KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert_eq!(tui.input(), "café 🙂\n");

        press(&mut tui, KeyCode::Home, KeyModifiers::NONE);
        press(&mut tui, KeyCode::Backspace, KeyModifiers::NONE);
        press(&mut tui, KeyCode::Left, KeyModifiers::NONE);
        press(&mut tui, KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert_eq!(tui.input(), "café ");
        press(&mut tui, KeyCode::Left, KeyModifiers::NONE);
        press(&mut tui, KeyCode::Delete, KeyModifiers::NONE);
        press(&mut tui, KeyCode::Char('!'), KeyModifiers::NONE);
        assert_eq!(tui.input(), "café!");

        press(&mut tui, KeyCode::Home, KeyModifiers::NONE);
        press(&mut tui, KeyCode::Char('k'), KeyModifiers::CONTROL);
        assert_eq!(tui.input(), "");
        tui.handle(map_event(CrosstermEvent::Paste("café!".into())).unwrap());

        assert_eq!(
            press(&mut tui, KeyCode::Enter, KeyModifiers::SHIFT),
            Action::Render
        );
        press(&mut tui, KeyCode::Char('é'), KeyModifiers::NONE);
        assert_eq!(
            press(&mut tui, KeyCode::Enter, KeyModifiers::NONE),
            Action::Submit("café!\né".into())
        );

        let mut running = Tui::new(NoopEngine);
        running.begin_submission("active");
        running.handle(map_event(CrosstermEvent::Paste("queued 🙂".into())).unwrap());
        assert_eq!(
            press(&mut running, KeyCode::Enter, KeyModifiers::NONE),
            Action::Render
        );
        assert_eq!(running.input(), "queued 🙂");
    }

    #[test]
    fn selection_dialog_consumes_readline_keys_before_the_composer() {
        let mut tui = Tui::new(NoopEngine);
        tui.handle(map_event(CrosstermEvent::Paste("draft text".into())).unwrap());
        tui.show_selection_dialog(DialogView::selection(
            "Choose",
            None::<String>,
            vec![DialogEntry::action("Keep", "keep")],
        ));

        assert_eq!(
            press(&mut tui, KeyCode::Char('w'), KeyModifiers::CONTROL),
            Action::Render
        );
        assert_eq!(tui.input(), "draft text");
        assert!(tui.view().dialog.is_some());
    }
}
