//! Terminal lifecycle and input-event boundary for the interactive surface.

mod app;
mod bridge;
mod conversation;
mod render;
mod terminal;

pub use app::{AppEvent, AppState, Command, Dialog, Effect, Runtime};
pub use bridge::{
    BridgeCancel, BridgeTx, PublishOutcome, ToolResultState, TuiExecution, TuiExecutionEvent,
    TuiExecutionState, TuiPermissionBridge, TuiPermissionReply, TuiPermissionRequest,
    TuiRuntimeEvent, TuiSubagentErrorKind, TuiSubagentEvent, TuiSubagentStatus, UiEnvelope,
};
pub use conversation::{
    ActionableError, Conversation, ConversationError, ConversationEvent, DiffLine, DiffLineKind,
    SubagentCard, ToolBatch, ToolCall, ToolResult,
};
pub use terminal::{
    PendingPermissions, PermissionReply, TerminalControl, TerminalModeGuard, TerminalOperation,
    teardown,
};

use std::{
    collections::{BTreeMap, BTreeSet},
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
        KeyboardEnhancementFlags, MouseEventKind, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
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
    widgets::{
        Block, BorderType, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Wrap,
    },
};

const TRANSCRIPT_CONTENT_INDENT: u16 = 4;
const MAX_CHILD_TRANSCRIPTS: usize = 64;

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
    /// Opens the eligible subagent selection dialog.
    CtrlShiftA,
    /// Starts or moves the selected subagent into background execution.
    CtrlB,
    ShiftEnter,
    Left,
    Right,
    PreviousWord,
    NextWord,
    LineStart,
    LineEnd,
    Home,
    End,
    PageUp,
    PageDown,
    ScrollUp,
    ScrollDown,
    F5,
    F6,
    F7,
    F8,
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
    SubmitBackground(String),
    TransitionToBackground(u64),
    /// Ask the composition layer to resolve a palette dialog by stable route ID.
    OpenDialog(String),
    /// Dispatch the selected dialog action through the composition layer.
    DialogAction(String),
    SafeDialogAction(String),
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
    SafeDialog(DialogView),
    SelectionInfo(String),
    SelectionCancelled,
    SelectionError {
        message: String,
        action: String,
    },
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
    Backgrounded,
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

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum TranscriptId {
    Main,
    Subagent(u64),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TranscriptFocus {
    Composer,
    Viewport,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptRecord {
    id: TranscriptId,
    owner_label: String,
    transcript: Vec<TranscriptEntry>,
    conversation: Option<Conversation>,
    completed_conversations: Vec<Conversation>,
    following_bottom: bool,
    scroll_offset: u16,
    collapsed_tool_outputs: BTreeSet<String>,
    collapse_thinking: bool,
    focus: TranscriptFocus,
    last_admitted_ordinal: Option<u64>,
    terminal: bool,
}

impl TranscriptRecord {
    fn main() -> Self {
        Self {
            id: TranscriptId::Main,
            owner_label: "main".to_owned(),
            transcript: Vec::new(),
            conversation: None,
            completed_conversations: Vec::new(),
            following_bottom: true,
            scroll_offset: 0,
            collapsed_tool_outputs: BTreeSet::new(),
            collapse_thinking: false,
            focus: TranscriptFocus::Composer,
            last_admitted_ordinal: None,
            terminal: false,
        }
    }

    pub const fn id(&self) -> &TranscriptId {
        &self.id
    }

    pub const fn last_admitted_ordinal(&self) -> Option<u64> {
        self.last_admitted_ordinal
    }

    pub const fn is_terminal(&self) -> bool {
        self.terminal
    }
}

/// State passed to renderers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ViewState<'a> {
    pub active_transcript: TranscriptId,
    pub transcript_ids: Vec<TranscriptId>,
    /// Owner label for the active primary viewport.
    pub owner_label: &'a str,
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
    /// Whether complete reasoning is collapsed according to the UI setting.
    pub collapse_thinking: bool,
    pub focus: TranscriptFocus,
    /// A bounded informational dialog rendered above the conversation.
    pub dialog: Option<&'a DialogView>,
    /// Slash palette metadata and current filtered selection.
    pub palette: Option<PaletteView<'a>>,
    pub agent_catalog: &'a [String],
    pub selected_agent: Option<&'a str>,
    pub executions: Vec<&'a TuiExecution>,
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
    SafeDispatch(String),
    SelectTranscript(TranscriptId),
    Cancel,
    ToggleDetails,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DialogEntry {
    label: String,
    detail: Option<String>,
    search_text: Option<String>,
    selected_detail: Option<String>,
    action: Option<DialogEntryAction>,
}

impl DialogEntry {
    fn transcript(label: impl AsRef<str>, id: TranscriptId) -> Self {
        let mut entry = Self::action(label, "");
        entry.action = Some(DialogEntryAction::SelectTranscript(id));
        entry
    }

    pub fn action(label: impl AsRef<str>, action_id: impl AsRef<str>) -> Self {
        Self::action_with_detail(label, None::<String>, action_id)
    }

    pub fn action_with_detail<D>(
        label: impl AsRef<str>,
        detail: Option<D>,
        action_id: impl AsRef<str>,
    ) -> Self
    where
        D: AsRef<str>,
    {
        Self {
            label: bounded_dialog_text(label.as_ref(), 128),
            detail: detail.map(|detail| bounded_dialog_text(detail.as_ref(), 256)),
            search_text: None,
            selected_detail: None,
            action: Some(DialogEntryAction::Dispatch(bounded_dialog_text(
                action_id.as_ref(),
                128,
            ))),
        }
    }

    pub fn safe_action(label: impl AsRef<str>, action_id: impl AsRef<str>) -> Self {
        Self {
            label: bounded_dialog_text(label.as_ref(), 128),
            detail: None,
            search_text: None,
            selected_detail: None,
            action: Some(DialogEntryAction::SafeDispatch(bounded_dialog_text(
                action_id.as_ref(),
                128,
            ))),
        }
    }

    pub fn action_with_metadata(
        label: impl AsRef<str>,
        detail: impl AsRef<str>,
        search_text: impl AsRef<str>,
        selected_detail: impl AsRef<str>,
        action_id: impl AsRef<str>,
    ) -> Self {
        Self {
            label: bounded_dialog_text(label.as_ref(), 128),
            detail: Some(bounded_dialog_text(detail.as_ref(), 256)),
            search_text: Some(bounded_dialog_text(search_text.as_ref(), 512)),
            selected_detail: Some(bounded_dialog_multiline(selected_detail.as_ref(), 512)),
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
            search_text: None,
            selected_detail: None,
            action: Some(DialogEntryAction::Cancel),
        }
    }

    pub fn disabled(label: impl AsRef<str>, detail: impl AsRef<str>) -> Self {
        Self {
            label: bounded_dialog_text(label.as_ref(), 128),
            detail: Some(bounded_dialog_text(detail.as_ref(), 256)),
            search_text: None,
            selected_detail: None,
            action: None,
        }
    }

    pub fn read_only(
        label: impl AsRef<str>,
        search_text: impl AsRef<str>,
        selected_detail: impl AsRef<str>,
    ) -> Self {
        Self {
            label: bounded_dialog_text(label.as_ref(), 256),
            detail: None,
            search_text: Some(bounded_dialog_text(search_text.as_ref(), 512)),
            selected_detail: Some(bounded_dialog_multiline(selected_detail.as_ref(), 2_048)),
            action: Some(DialogEntryAction::ToggleDetails),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SessionDialogEntries {
    current_project: Vec<DialogEntry>,
    all_projects: Vec<DialogEntry>,
    showing_all_projects: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DialogQueryAction {
    label_prefix: String,
    label_suffix: String,
    action_prefix: String,
    base_entry_count: usize,
    max_query_chars: usize,
}

/// Generic bounded dialog state for informational, selection, and confirmation overlays.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DialogView {
    title: String,
    help: Option<String>,
    entries: Vec<DialogEntry>,
    query: String,
    selected: usize,
    offset: usize,
    interactive: bool,
    session_entries: Option<SessionDialogEntries>,
    query_action: Option<DialogQueryAction>,
    refresh_id: Option<String>,
    details_open: bool,
    empty_message: Option<String>,
    cancellation_action: Option<String>,
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
            query: String::new(),
            selected,
            offset: 0,
            interactive: true,
            session_entries: None,
            query_action: None,
            refresh_id: None,
            details_open: false,
            empty_message: None,
            cancellation_action: None,
        }
    }

    pub fn read_only<H>(
        title: impl AsRef<str>,
        help: Option<H>,
        entries: Vec<DialogEntry>,
        refresh_id: impl AsRef<str>,
    ) -> Self
    where
        H: AsRef<str>,
    {
        let mut dialog = Self::selection(title, help, Vec::new());
        dialog.entries = entries;
        dialog.refresh_id = Some(bounded_dialog_text(refresh_id.as_ref(), 64));
        dialog
    }

    pub fn with_empty_message(mut self, message: impl AsRef<str>) -> Self {
        self.empty_message = Some(bounded_dialog_text(message.as_ref(), 256));
        self
    }

    pub fn with_cancellation_action(mut self, action_id: impl AsRef<str>) -> Self {
        self.cancellation_action = Some(bounded_dialog_text(action_id.as_ref(), 128));
        self
    }

    pub fn sessions(current_project: Vec<DialogEntry>, all_projects: Vec<DialogEntry>) -> Self {
        let current_project = current_project.into_iter().take(64).collect::<Vec<_>>();
        let all_projects = all_projects.into_iter().take(64).collect::<Vec<_>>();
        let mut dialog = Self::selection(
            "Resume session · Current project",
            Some(
                "Type to search | Ctrl+A All projects | Up/Down navigate | Enter resume | Esc cancel",
            ),
            current_project.clone(),
        );
        dialog.session_entries = Some(SessionDialogEntries {
            current_project,
            all_projects,
            showing_all_projects: false,
        });
        dialog
    }

    pub fn with_selected(mut self, selected: usize) -> Self {
        if self
            .entries
            .get(selected)
            .is_some_and(|entry| entry.action.is_some())
        {
            self.selected = selected;
        }
        self
    }

    pub fn with_identifier_query_action(
        mut self,
        label_prefix: impl AsRef<str>,
        label_suffix: impl AsRef<str>,
        action_prefix: impl AsRef<str>,
        max_query_chars: usize,
    ) -> Self {
        self.query_action = Some(DialogQueryAction {
            label_prefix: bounded_dialog_text(label_prefix.as_ref(), 64),
            label_suffix: bounded_dialog_text(label_suffix.as_ref(), 64),
            action_prefix: bounded_dialog_text(action_prefix.as_ref(), 64),
            base_entry_count: self.entries.len(),
            max_query_chars,
        });
        refresh_dialog_query_action(&mut self);
        self
    }

    fn informational(title: impl AsRef<str>, body: impl AsRef<str>) -> Self {
        Self {
            title: bounded_dialog_text(title.as_ref(), 64),
            help: Some(bounded_dialog_text(body.as_ref(), 2_048)),
            entries: Vec::new(),
            query: String::new(),
            selected: 0,
            offset: 0,
            interactive: false,
            session_entries: None,
            query_action: None,
            refresh_id: None,
            details_open: false,
            empty_message: None,
            cancellation_action: None,
        }
    }
}

fn refresh_dialog_query_action(dialog: &mut DialogView) {
    let Some(action) = dialog.query_action.clone() else {
        return;
    };
    dialog.entries.truncate(action.base_entry_count);
    let query_chars = dialog.query.chars().count();
    if query_chars == 0
        || query_chars > action.max_query_chars
        || !dialog.query.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        })
        || !dialog_matches(dialog).is_empty()
    {
        return;
    }

    dialog.entries.push(DialogEntry::action(
        format!(
            "{}{}{}",
            action.label_prefix, dialog.query, action.label_suffix
        ),
        format!("{}{}", action.action_prefix, dialog.query),
    ));
}

fn dialog_matches(dialog: &DialogView) -> Vec<(usize, &DialogEntry)> {
    let query = dialog.query.to_lowercase();
    dialog
        .entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            query.is_empty()
                || entry.label.to_lowercase().contains(&query)
                || entry
                    .detail
                    .as_ref()
                    .is_some_and(|detail| detail.to_lowercase().contains(&query))
                || entry
                    .search_text
                    .as_ref()
                    .is_some_and(|text| text.to_lowercase().contains(&query))
        })
        .collect()
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
    let layout = screen_layout(
        area,
        state.running,
        state.selected_agent.is_some(),
        state.executions.len(),
    );

    if layout.header.height > 0 {
        render_header(frame, layout.header, &state, layout.show_context);
    }

    let transcript = rendered_transcript(&state);
    let visible_rows = layout.transcript.height.saturating_sub(1) as usize;
    let transcript_width = layout
        .transcript
        .width
        .saturating_sub(TRANSCRIPT_CONTENT_INDENT);
    let bottom_scroll =
        saturating_u16(transcript_rows(&transcript, transcript_width).saturating_sub(visible_rows));
    let scroll = if state.following_bottom {
        bottom_scroll
    } else {
        state.scroll_offset.min(bottom_scroll)
    };
    let scroll_label = if state.following_bottom {
        format!(" LIVE {bottom_scroll}/{bottom_scroll}")
    } else {
        format!(" SCROLL {scroll}/{bottom_scroll}")
    };
    if layout.transcript.height > 0 {
        frame.render_widget(
            Paragraph::new(Text::from(transcript))
                .block(
                    Block::default()
                        .borders(Borders::TOP)
                        .padding(Padding::left(TRANSCRIPT_CONTENT_INDENT))
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

    if layout.composer.height > 0 && state.active_transcript != TranscriptId::Main {
        frame.render_widget(
            Paragraph::new(" Subagent transcript · read-only")
                .style(Style::default().fg(Color::DarkGray))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(Color::DarkGray))
                        .title(Span::styled(
                            " Viewport ",
                            Style::default()
                                .fg(Color::DarkGray)
                                .add_modifier(Modifier::BOLD),
                        )),
                ),
            layout.composer,
        );
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
    if layout.composer.height > 0 && state.active_transcript == TranscriptId::Main {
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
    let dialog_area = dialog_area(area, dialog);
    let matches = dialog_matches(dialog);
    let layout = dialog_content_layout(dialog, dialog_area.height);

    frame.render_widget(Clear, dialog_area);
    let mut lines: Vec<Line<'_>> = if layout.show_help {
        dialog
            .help
            .as_deref()
            .and_then(|help| help.lines().next())
            .map(Line::from)
            .into_iter()
            .collect()
    } else if dialog.entries.is_empty() && dialog.session_entries.is_none() {
        dialog
            .help
            .as_deref()
            .map(|help| help.lines().map(Line::from).collect())
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    if dialog.interactive {
        lines.push(Line::styled(
            format!("Search: {}", dialog.query),
            Style::default().fg(Color::DarkGray),
        ));
    }
    if matches.is_empty()
        && (dialog.session_entries.is_some()
            || dialog.help.is_none()
            || dialog.empty_message.is_some())
    {
        lines.push(Line::from(dialog_empty_message(dialog)));
    }
    lines.extend(
        matches
            .iter()
            .skip(dialog.offset)
            .take(layout.entry_rows)
            .map(|(index, entry)| {
                let selected = dialog.interactive && *index == dialog.selected;
                let text = match (&entry.action, &entry.detail) {
                    (None, Some(detail)) => format!("disabled {}: {detail}", entry.label),
                    (None, None) => format!("disabled {}", entry.label),
                    (Some(_), Some(detail)) if selected => {
                        format!("> {} - {detail}", entry.label)
                    }
                    (Some(_), Some(detail)) => format!("  {} - {detail}", entry.label),
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
            }),
    );
    if let Some(detail) = dialog
        .details_open
        .then(|| {
            matches
                .iter()
                .find(|(index, _)| *index == dialog.selected)
                .and_then(|(_, entry)| entry.selected_detail.as_deref())
        })
        .flatten()
        .or_else(|| {
            matches
                .iter()
                .find(|(index, entry)| {
                    *index == dialog.selected
                        && !matches!(entry.action, Some(DialogEntryAction::ToggleDetails))
                })
                .and_then(|(_, entry)| entry.selected_detail.as_deref())
        })
    {
        lines.extend(
            detail
                .lines()
                .take(layout.detail_rows)
                .map(|line| Line::styled(line, Style::default().fg(Color::DarkGray))),
        );
    }

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

fn dialog_area(area: Rect, dialog: &DialogView) -> Rect {
    let width = area.width.saturating_sub(4).clamp(1, 64);
    let content_rows = usize::from(dialog.help.is_some())
        .saturating_add(usize::from(dialog.interactive))
        .saturating_add(dialog_matches(dialog).len().max(1))
        .saturating_add(
            dialog
                .entries
                .get(dialog.selected)
                .and_then(|entry| entry.selected_detail.as_deref())
                .map_or(0, |detail| detail.lines().count().min(3)),
        )
        .saturating_add(2) as u16;
    let height = content_rows
        .min(if dialog.session_entries.is_some() {
            16
        } else {
            12
        })
        .min(area.height.saturating_sub(2))
        .max(1);
    Rect::new(
        area.x.saturating_add(area.width.saturating_sub(width) / 2),
        area.y
            .saturating_add(area.height.saturating_sub(height) / 2),
        width,
        height,
    )
}

#[derive(Clone, Copy)]
struct DialogContentLayout {
    show_help: bool,
    entry_rows: usize,
    detail_rows: usize,
}

fn dialog_content_layout(dialog: &DialogView, height: u16) -> DialogContentLayout {
    let inner_rows = usize::from(height.saturating_sub(2));
    let search_rows = usize::from(dialog.interactive);
    let detail_rows = dialog
        .entries
        .get(dialog.selected)
        .and_then(|entry| entry.selected_detail.as_deref())
        .map_or(0, |detail| {
            detail
                .lines()
                .count()
                .min(3)
                .min(inner_rows.saturating_sub(search_rows.saturating_add(1)))
        });
    let show_help = dialog.help.is_some()
        && inner_rows > search_rows.saturating_add(detail_rows).saturating_add(1);
    let entry_rows = inner_rows
        .saturating_sub(search_rows)
        .saturating_sub(detail_rows)
        .saturating_sub(usize::from(show_help))
        .max(1);

    DialogContentLayout {
        show_help,
        entry_rows,
        detail_rows,
    }
}

fn dialog_empty_message(dialog: &DialogView) -> &str {
    if let Some(message) = dialog.empty_message.as_deref() {
        return message;
    }
    let Some(session_entries) = dialog.session_entries.as_ref() else {
        return "No options available.";
    };
    if !dialog.query.is_empty() {
        "No sessions match search."
    } else if session_entries.showing_all_projects {
        "No resumable sessions in any project."
    } else {
        "No resumable sessions in current project."
    }
}

struct ScreenLayout {
    header: Rect,
    transcript: Rect,
    status: Rect,
    composer: Rect,
    footer: Rect,
    show_context: bool,
}

fn screen_layout(
    area: Rect,
    running: bool,
    has_selected_agent: bool,
    execution_count: usize,
) -> ScreenLayout {
    let show_header = area.height >= 8;
    let header_rows = 1_u16
        .saturating_add(u16::from(has_selected_agent))
        .saturating_add(execution_count.try_into().unwrap_or(u16::MAX))
        .min(area.height.saturating_sub(6));
    let show_context = area.width >= 80 && area.height > 16;
    let show_footer = area.height >= 10;
    let show_status = running && area.height > 16;
    let composer_rows = if area.height < 8 { 2 } else { 3 };
    let chunks = Layout::vertical([
        Constraint::Length(u16::from(show_header) * header_rows),
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
        left.push(Span::styled(
            format!("  ·  agents {}", state.agent_catalog.join(", ")),
            Style::default().fg(Color::Gray),
        ));
    }
    let mut lines = vec![Line::from(left)];
    if show_context {
        if let Some(agent) = state.selected_agent {
            lines.push(Line::styled(
                format!(" selected {agent}"),
                Style::default().fg(Color::Yellow),
            ));
        }
        lines.extend(state.executions.iter().map(|execution| {
            Line::styled(
                format!(
                    " {} · {}",
                    execution.agent,
                    execution_label(execution.state())
                ),
                Style::default().fg(Color::Yellow),
            )
        }));
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
        area,
    );
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

fn execution_label(state: TuiExecutionState) -> &'static str {
    match state {
        TuiExecutionState::ForegroundRunning => "foreground running",
        TuiExecutionState::BackgroundRunning => "background running",
        TuiExecutionState::CompletedRecent => "completed recent",
        TuiExecutionState::Failed => "failed",
        TuiExecutionState::Cancelled => "cancelled",
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

fn rendered_transcript(state: &ViewState<'_>) -> Vec<Line<'static>> {
    let mut transcript = transcript_provenance(state);
    transcript.extend(
        state
            .completed_conversations
            .iter()
            .flat_map(|conversation| {
                render::conversation_lines(
                    conversation,
                    &[],
                    state.collapsed_tool_outputs,
                    state.collapse_thinking,
                )
            })
            .collect::<Vec<_>>(),
    );
    if let Some(conversation) = state.conversation {
        transcript.extend(render::conversation_lines(
            conversation,
            state.runtime_events,
            state.collapsed_tool_outputs,
            state.collapse_thinking,
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
    transcript
}

fn transcript_provenance(state: &ViewState<'_>) -> Vec<Line<'static>> {
    let owner = match state.active_transcript {
        TranscriptId::Main => "Main · primary conversation".to_owned(),
        TranscriptId::Subagent(id) => format!("Subagent {id} · {}", state.owner_label),
    };
    vec![
        Line::styled(
            owner,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Line::styled(
            "F5 select · F6 Main · F7/F8 sibling",
            Style::default().fg(Color::DarkGray),
        ),
        Line::default(),
    ]
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
    transcripts: BTreeMap<TranscriptId, TranscriptRecord>,
    active_transcript: TranscriptId,
    child_transcript_order: Vec<TranscriptId>,
    transcript: Vec<TranscriptEntry>,
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
    dialog: Option<DialogView>,
    palette_entries: Vec<PaletteEntry>,
    palette_open: bool,
    palette_selected: usize,
    agent_catalog: Vec<String>,
    selected_agent: Option<String>,
    executions: Vec<TuiExecution>,
    now: Duration,
    next_runtime_ordinal: u64,
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
            size: (80, 24),
            running: false,
            quit_armed: false,
            transcripts: BTreeMap::from([(TranscriptId::Main, TranscriptRecord::main())]),
            active_transcript: TranscriptId::Main,
            child_transcript_order: Vec::new(),
            transcript: Vec::new(),
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
            dialog: None,
            palette_entries: Vec::new(),
            palette_open: false,
            palette_selected: 0,
            agent_catalog: vec!["main".into()],
            selected_agent: None,
            executions: Vec::new(),
            now: Duration::ZERO,
            next_runtime_ordinal: 0,
        }
    }

    /// Handles one input or resize event without performing rendering or engine work.
    pub fn handle(&mut self, event: Event) -> Action {
        match event {
            Event::Resize { width, height } => {
                self.size = (width, height);
                self.quit_armed = false;
                self.clamp_palette_selection();
                self.clamp_scroll_offset();
                self.ensure_dialog_selection_visible();
                Action::Render
            }
            Event::Key(key) => self.handle_key(key),
            Event::Paste(text) => {
                if self.active_transcript != TranscriptId::Main
                    || self
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

    pub fn set_collapse_thinking(&mut self, collapse: bool) {
        self.active_record_mut().collapse_thinking = collapse;
    }

    pub fn set_agent_catalog<I, S>(&mut self, eligible: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.agent_catalog = std::iter::once("main".to_owned())
            .chain(eligible.into_iter().map(|agent| agent.as_ref().to_owned()))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if self.selected_agent.as_ref().is_some_and(|agent| {
            self.agent_catalog.binary_search(agent).is_err() || agent == "main"
        }) {
            self.selected_agent = None;
        }
    }

    pub fn select_agent(&mut self, agent: impl AsRef<str>) {
        let agent = agent.as_ref();
        self.selected_agent = self
            .agent_catalog
            .binary_search_by(|candidate| candidate.as_str().cmp(agent))
            .ok()
            .and_then(|index| (self.agent_catalog[index] != "main").then(|| agent.to_owned()));
    }

    pub fn agent_catalog(&self) -> &[String] {
        &self.agent_catalog
    }

    pub fn selected_agent(&self) -> Option<&str> {
        self.selected_agent.as_deref()
    }

    pub fn executions(&self) -> Vec<&TuiExecution> {
        let mut executions = self.executions.iter().collect::<Vec<_>>();
        executions.sort_unstable_by(|left, right| {
            right
                .last_activity
                .cmp(&left.last_activity)
                .then_with(|| right.id.cmp(&left.id))
        });
        executions
    }

    pub fn tick(&mut self, now: Duration) {
        self.now = now;
        self.executions.retain(|execution| {
            execution
                .terminal_at
                .is_none_or(|terminal_at| now < terminal_at + Duration::from_secs(60))
        });
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
        self.active_record_mut().collapsed_tool_outputs.clear();
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

    /// Shows a local session or lifecycle notice outside the conversation.
    pub fn add_info(&mut self, text: impl Into<String>) {
        self.status = Some(text.into());
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
        if !matches!(
            &outcome,
            TuiSubmissionOutcome::Dialog(_) | TuiSubmissionOutcome::SafeDialog(_)
        ) {
            self.dialog = None;
        }
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
                if message == "connect or choose provider" {
                    self.show_dialog(
                        "Action required",
                        "Saved provider is unavailable.\nAction: connect or choose provider.",
                    );
                } else {
                    self.status = Some(message);
                }
                None
            }
            TuiSubmissionOutcome::Dialog(dialog) => {
                self.set_running(false);
                self.show_selection_dialog(dialog);
                None
            }
            TuiSubmissionOutcome::SafeDialog(dialog) => {
                self.show_selection_dialog(dialog);
                None
            }
            TuiSubmissionOutcome::SelectionInfo(message) => {
                self.add_info(message);
                None
            }
            TuiSubmissionOutcome::SelectionCancelled => {
                self.add_info("File selection cancelled.");
                None
            }
            TuiSubmissionOutcome::SelectionError { message, action } => {
                self.show_dialog("Action required", format!("{message}\nAction: {action}"));
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
            TuiProviderOutcome::Backgrounded => self.set_running(false),
        }
    }

    /// Clears the current visible conversation for a new session.
    pub fn clear_transcript(&mut self) {
        self.transcript.clear();
        self.completed_conversations.clear();
        self.conversation = None;
        self.active_record_mut().collapsed_tool_outputs.clear();
        self.set_running(false);
        self.turn_state = None;
        self.active_tool = None;
        self.clear_current_session_transcripts();
    }

    pub fn replace_history(
        &mut self,
        messages: &[agens_core::Message],
    ) -> Result<(), ConversationError> {
        let conversations = Conversation::from_messages(messages)?;
        self.transcript.clear();
        self.completed_conversations = conversations;
        self.conversation = None;
        self.active_record_mut().collapsed_tool_outputs.clear();
        self.runtime_events.clear();
        self.turn_duration = None;
        self.latest_usage = None;
        self.set_running(false);
        self.turn_state = None;
        self.active_tool = None;
        self.clear_current_session_transcripts();
        Ok(())
    }

    /// Returns the visible conversation for composition and focused tests.
    pub fn transcript(&self) -> &[TranscriptEntry] {
        &self.transcript
    }

    pub fn transcript_record(&self, id: &TranscriptId) -> Option<&TranscriptRecord> {
        self.transcripts.get(id)
    }

    pub fn select_transcript(&mut self, id: TranscriptId) {
        self.active_transcript = if self.transcripts.contains_key(&id) {
            id
        } else {
            TranscriptId::Main
        };
    }

    fn active_record_mut(&mut self) -> &mut TranscriptRecord {
        self.transcripts
            .get_mut(&self.active_transcript)
            .expect("active transcript always exists")
    }

    fn show_transcript_dialog(&mut self) {
        let entries: Vec<_> = self
            .child_transcript_order
            .iter()
            .copied()
            .filter(|id| self.transcripts.contains_key(id))
            .filter_map(|id| match id {
                TranscriptId::Main => None,
                TranscriptId::Subagent(id_value) => {
                    Some(DialogEntry::transcript(format!("Subagent {id_value}"), id))
                }
            })
            .collect();
        if entries.is_empty() {
            return;
        }
        self.show_selection_dialog(DialogView::selection(
            "Select transcript",
            Some("Enter select | Esc cancel"),
            entries,
        ));
    }

    fn select_sibling(&mut self, direction: isize) {
        let sibling = if self.active_transcript == TranscriptId::Main {
            if direction.is_negative() {
                self.child_transcript_order.last().copied()
            } else {
                self.child_transcript_order.first().copied()
            }
        } else {
            self.child_transcript_order
                .iter()
                .position(|id| *id == self.active_transcript)
                .and_then(|index| index.checked_add_signed(direction))
                .and_then(|index| self.child_transcript_order.get(index).copied())
        };
        if let Some(id) = sibling.filter(|id| self.transcripts.contains_key(id)) {
            self.select_transcript(id);
        }
    }

    /// Retains typed runtime metrics for the renderer without altering turn persistence.
    pub fn apply_runtime_event(&mut self, event: TuiRuntimeEvent) {
        let ordinal = self.next_runtime_ordinal;
        self.next_runtime_ordinal = self.next_runtime_ordinal.saturating_add(1);
        self.apply_runtime_event_with_ordinal(ordinal, event);
    }

    /// Retains typed runtime metrics in source order without altering turn persistence.
    pub fn apply_runtime_event_with_ordinal(&mut self, ordinal: u64, event: TuiRuntimeEvent) {
        self.next_runtime_ordinal = self.next_runtime_ordinal.max(ordinal.saturating_add(1));
        if !self.admit_runtime_event(ordinal, &event) {
            return;
        }

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
            TuiRuntimeEvent::TaskExecution { agent, event } => {
                self.apply_task_execution_event(agent, *event);
                if matches!(event, TuiExecutionEvent::Backgrounded { .. }) {
                    self.set_running(false);
                }
            }
            TuiRuntimeEvent::SubagentExecution(event) => {
                if self.subagent_event_matches_execution(event) {
                    self.apply_subagent_event(event);
                }
            }
            TuiRuntimeEvent::RestoredCompletedSubagent {
                id,
                agent,
                task_summary,
                final_result,
                tool_uses,
            } => self
                .conversation
                .get_or_insert_with(|| Conversation::new(String::new()))
                .restore_completed_subagent(
                    *id,
                    agent.clone(),
                    task_summary.clone(),
                    final_result.clone(),
                    *tool_uses,
                ),
            TuiRuntimeEvent::ToolStarted { .. } | TuiRuntimeEvent::ToolEnded { .. } => {}
        }
        self.runtime_events.push(event);
    }

    fn apply_subagent_event(&mut self, event: &TuiSubagentEvent) {
        if let bridge::TuiSubagentUpdate::Started { agent, .. } = &event.update {
            self.transcripts
                .get_mut(&TranscriptId::Subagent(event.id))
                .expect("admitted child event has a transcript")
                .owner_label
                .clone_from(agent);
        }
        self.transcripts
            .get_mut(&TranscriptId::Subagent(event.id))
            .expect("admitted child event has a transcript")
            .conversation
            .get_or_insert_with(|| Conversation::new(String::new()))
            .apply_child_event(event.clone());

        self.conversation
            .get_or_insert_with(|| Conversation::new(String::new()))
            .apply_subagent_summary(event.clone());
    }

    fn admit_runtime_event(&mut self, ordinal: u64, event: &TuiRuntimeEvent) -> bool {
        let id = match event {
            TuiRuntimeEvent::TaskExecution { event, .. } => match event {
                TuiExecutionEvent::ForegroundStarted { id }
                | TuiExecutionEvent::BackgroundStarted { id } => {
                    let id = TranscriptId::Subagent(*id);
                    self.ensure_child_transcript(id);
                    id
                }
                TuiExecutionEvent::Backgrounded { id }
                | TuiExecutionEvent::Completed { id }
                | TuiExecutionEvent::Failed { id }
                | TuiExecutionEvent::Cancelled { id } => TranscriptId::Subagent(*id),
            },
            TuiRuntimeEvent::SubagentExecution(event) => TranscriptId::Subagent(event.id),
            _ => TranscriptId::Main,
        };

        let Some(record) = self.transcripts.get_mut(&id) else {
            return false;
        };
        if record.terminal
            || record
                .last_admitted_ordinal
                .is_some_and(|last| ordinal <= last)
        {
            return false;
        }

        record.last_admitted_ordinal = Some(ordinal);
        if matches!(
            event,
            TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent {
                update: bridge::TuiSubagentUpdate::Terminal { .. },
                ..
            })
        ) {
            record.terminal = true;
            self.evict_terminal_transcripts();
        }
        true
    }

    fn ensure_child_transcript(&mut self, id: TranscriptId) {
        if self.transcripts.contains_key(&id) {
            return;
        }

        self.transcripts.insert(
            id,
            TranscriptRecord {
                id,
                owner_label: String::new(),
                transcript: Vec::new(),
                conversation: None,
                completed_conversations: Vec::new(),
                following_bottom: true,
                scroll_offset: 0,
                collapsed_tool_outputs: BTreeSet::new(),
                collapse_thinking: false,
                focus: TranscriptFocus::Viewport,
                last_admitted_ordinal: None,
                terminal: false,
            },
        );
        self.child_transcript_order.push(id);
    }

    fn evict_terminal_transcripts(&mut self) {
        while self.child_transcript_order.len() > MAX_CHILD_TRANSCRIPTS {
            let Some(index) = self.child_transcript_order.iter().position(|id| {
                *id != self.active_transcript
                    && self
                        .transcripts
                        .get(id)
                        .is_some_and(|record| record.terminal)
            }) else {
                return;
            };
            let id = self.child_transcript_order.remove(index);
            self.transcripts.remove(&id);
        }
    }

    fn clear_current_session_transcripts(&mut self) {
        self.transcripts.clear();
        self.transcripts
            .insert(TranscriptId::Main, TranscriptRecord::main());
        self.active_transcript = TranscriptId::Main;
        self.child_transcript_order.clear();
        self.next_runtime_ordinal = 0;
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

    pub fn show_selection_dialog(&mut self, mut dialog: DialogView) {
        if dialog.refresh_id.is_some()
            && dialog.refresh_id
                == self
                    .dialog
                    .as_ref()
                    .and_then(|current| current.refresh_id.clone())
            && let Some(current) = self.dialog.as_ref()
        {
            dialog.query.clone_from(&current.query);
            dialog.selected = current.selected.min(dialog.entries.len().saturating_sub(1));
            dialog.details_open = current.details_open;
        }
        self.palette_open = false;
        self.quit_armed = false;
        self.dialog = Some(dialog);
        self.ensure_dialog_selection_visible();
    }

    pub fn runtime_events(&self) -> &[TuiRuntimeEvent] {
        &self.runtime_events
    }

    /// Returns an immutable snapshot for a renderer.
    pub fn view(&self) -> ViewState<'_> {
        let active = self
            .transcripts
            .get(&self.active_transcript)
            .expect("active transcript always exists");
        ViewState {
            active_transcript: self.active_transcript,
            transcript_ids: std::iter::once(TranscriptId::Main)
                .chain(self.child_transcript_order.iter().copied())
                .collect(),
            owner_label: &active.owner_label,
            input: &self.input,
            size: self.size,
            running: self.running,
            quit_armed: self.quit_armed,
            transcript: &active.transcript,
            following_bottom: active.following_bottom,
            scroll_offset: active.scroll_offset,
            provider_model: &self.provider_model,
            session: &self.session,
            turn_state: self.turn_state,
            active_tool: self.active_tool.as_deref(),
            input_cursor: self.input_cursor,
            runtime_events: &self.runtime_events,
            turn_duration: self.turn_duration,
            latest_usage: self.latest_usage.as_ref(),
            status: self.status.as_deref(),
            conversation: if self.active_transcript == TranscriptId::Main {
                self.conversation.as_ref()
            } else {
                active.conversation.as_ref()
            },
            completed_conversations: if self.active_transcript == TranscriptId::Main {
                &self.completed_conversations
            } else {
                &active.completed_conversations
            },
            collapsed_tool_outputs: &active.collapsed_tool_outputs,
            collapse_thinking: active.collapse_thinking,
            focus: active.focus,
            dialog: self.dialog.as_ref(),
            palette: self.palette_open.then_some(PaletteView {
                entries: &self.palette_entries,
                selected: self.palette_selected,
            }),
            agent_catalog: &self.agent_catalog,
            selected_agent: self.selected_agent.as_deref(),
            executions: self.executions(),
        }
    }

    fn apply_task_execution_event(&mut self, agent: &str, event: TuiExecutionEvent) {
        let (id, state) = match event {
            TuiExecutionEvent::ForegroundStarted { id } => {
                self.add_execution(agent, id, TuiExecutionState::ForegroundRunning);
                return;
            }
            TuiExecutionEvent::BackgroundStarted { id } => {
                self.add_execution(agent, id, TuiExecutionState::BackgroundRunning);
                return;
            }
            TuiExecutionEvent::Backgrounded { id } => (id, TuiExecutionState::BackgroundRunning),
            TuiExecutionEvent::Completed { id } => (id, TuiExecutionState::CompletedRecent),
            TuiExecutionEvent::Failed { id } => (id, TuiExecutionState::Failed),
            TuiExecutionEvent::Cancelled { id } => (id, TuiExecutionState::Cancelled),
        };
        let Some(execution) = self
            .executions
            .iter_mut()
            .find(|execution| execution.id == id)
        else {
            return;
        };
        if execution.terminal_at.is_some()
            || matches!(state, TuiExecutionState::BackgroundRunning)
                && execution.state != TuiExecutionState::ForegroundRunning
        {
            return;
        }
        execution.state = state;
        execution.last_activity = self.now;
        if !matches!(state, TuiExecutionState::BackgroundRunning) {
            execution.terminal_at = Some(self.now);
        }
        if state == TuiExecutionState::BackgroundRunning
            && let Some(card) = self.conversation.as_mut().and_then(|conversation| {
                conversation
                    .subagent_cards
                    .iter_mut()
                    .find(|card| card.id == id)
            })
        {
            card.presentation = TuiExecutionState::BackgroundRunning;
        }
    }

    fn add_execution(&mut self, agent: &str, id: u64, state: TuiExecutionState) {
        if self.executions.iter().any(|execution| execution.id == id) {
            return;
        }
        self.executions.push(TuiExecution {
            id,
            agent: agent.to_owned(),
            state,
            last_activity: self.now,
            terminal_at: None,
        });
    }

    fn subagent_event_matches_execution(&self, event: &TuiSubagentEvent) -> bool {
        let Some(execution) = self
            .executions
            .iter()
            .find(|execution| execution.id == event.id)
        else {
            return false;
        };
        match &event.update {
            bridge::TuiSubagentUpdate::Started {
                agent,
                presentation,
                ..
            } => {
                execution.agent == *agent
                    && matches!(
                        (execution.state, presentation),
                        (
                            TuiExecutionState::ForegroundRunning,
                            TuiExecutionState::ForegroundRunning
                        ) | (
                            TuiExecutionState::BackgroundRunning,
                            TuiExecutionState::BackgroundRunning
                        )
                    )
            }
            bridge::TuiSubagentUpdate::Reasoning(_)
            | bridge::TuiSubagentUpdate::Text(_)
            | bridge::TuiSubagentUpdate::ToolCall { .. }
            | bridge::TuiSubagentUpdate::ToolResult { .. }
            | bridge::TuiSubagentUpdate::Error { .. } => matches!(
                execution.state,
                TuiExecutionState::ForegroundRunning | TuiExecutionState::BackgroundRunning
            ),
            bridge::TuiSubagentUpdate::Terminal { status, .. } => {
                status_matches_execution(*status, execution.state)
                    && self.conversation.as_ref().is_some_and(|conversation| {
                        conversation
                            .subagent_cards
                            .iter()
                            .any(|card| card.id == event.id && card.status.is_none())
                    })
            }
        }
    }

    pub fn following_bottom(&self) -> bool {
        self.transcripts
            .get(&self.active_transcript)
            .expect("active transcript always exists")
            .following_bottom
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
    }

    fn handle_key(&mut self, key: Key) -> Action {
        if key != Key::CtrlC {
            self.quit_armed = false;
        }
        if !matches!(
            key,
            Key::PageUp | Key::PageDown | Key::ScrollUp | Key::ScrollDown | Key::Home | Key::End
        ) {
            self.status = None;
        }

        if self
            .dialog
            .as_ref()
            .is_some_and(|dialog| dialog.interactive)
        {
            return self.handle_selection_dialog_key(key);
        }

        match key {
            Key::F5 => {
                self.show_transcript_dialog();
                return Action::Render;
            }
            Key::F6 => {
                self.select_transcript(TranscriptId::Main);
                return Action::Render;
            }
            Key::F7 => {
                self.select_sibling(-1);
                return Action::Render;
            }
            Key::F8 => {
                self.select_sibling(1);
                return Action::Render;
            }
            Key::Home => {
                self.scroll_to_start();
                return Action::Render;
            }
            Key::End => {
                self.scroll_to_end();
                return Action::Render;
            }
            _ => {}
        }

        if self.active_transcript != TranscriptId::Main
            && (matches!(
                key,
                Key::Char(_)
                    | Key::ShiftEnter
                    | Key::Backspace
                    | Key::Delete
                    | Key::DeletePreviousWord
                    | Key::DeleteToLineStart
                    | Key::DeleteToLineEnd
                    | Key::Left
                    | Key::Right
                    | Key::PreviousWord
                    | Key::NextWord
                    | Key::LineStart
                    | Key::LineEnd
                    | Key::Enter
                    | Key::CtrlB
            ) || (key == Key::CtrlC && !self.input.is_empty())
                || (key == Key::Tab && self.palette_open))
        {
            return Action::Render;
        }

        if key == Key::CtrlB {
            if self.dialog.is_some() {
                return Action::Render;
            }
            return self.handle_background_key();
        }

        if key == Key::CtrlShiftA {
            self.palette_open = false;
            return Action::OpenDialog("subagent".into());
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
                self.scroll_up(self.transcript_page_rows());
                Action::Render
            }
            Key::PageDown => {
                self.scroll_down(self.transcript_page_rows());
                Action::Render
            }
            Key::ScrollUp => {
                self.scroll_up(3);
                Action::Render
            }
            Key::ScrollDown => {
                self.scroll_down(3);
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
            Key::Enter if self.running && self.input.trim() == "/select" => {
                self.palette_open = false;
                self.input.clear();
                self.input_cursor = 0;
                Action::OpenDialog("select".into())
            }
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
            Key::CtrlC if self.running || self.has_active_execution() => self.cancel_running(),
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
            Key::LineStart => self.input_cursor = line_start(&self.input, cursor),
            Key::LineEnd => self.input_cursor = line_end(&self.input, cursor),
            Key::Home => {
                let record = self.active_record_mut();
                record.following_bottom = false;
                record.scroll_offset = 0;
                self.input_cursor = line_start(&self.input, cursor);
            }
            Key::End => {
                let scroll_offset = self.max_scroll_offset();
                let record = self.active_record_mut();
                record.following_bottom = true;
                record.scroll_offset = scroll_offset;
                self.input_cursor = line_end(&self.input, cursor);
            }
            Key::Backspace => {}
            _ => return None,
        }

        self.clamp_palette_selection();
        self.active_record_mut().focus = TranscriptFocus::Composer;
        Some(Action::Render)
    }

    fn handle_background_key(&mut self) -> Action {
        if let Some(id) = self.selected_agent.as_deref().and_then(|agent| {
            self.executions
                .iter()
                .find(|execution| {
                    execution.agent == agent
                        && execution.state == TuiExecutionState::ForegroundRunning
                })
                .map(TuiExecution::id)
        }) {
            return Action::TransitionToBackground(id);
        }

        if self.selected_agent.is_none() || self.input.trim().is_empty() {
            return Action::Render;
        }

        self.palette_open = false;
        self.input_cursor = 0;
        Action::SubmitBackground(std::mem::take(&mut self.input))
    }

    fn scroll_up(&mut self, rows: u16) {
        let bottom = self.max_scroll_offset();
        let record = self.active_record_mut();
        let current = if record.following_bottom {
            bottom
        } else {
            record.scroll_offset.min(bottom)
        };
        record.following_bottom = false;
        record.scroll_offset = current.saturating_sub(rows);
        record.focus = TranscriptFocus::Viewport;
    }

    fn scroll_down(&mut self, rows: u16) {
        let bottom = self.max_scroll_offset();
        let record = self.active_record_mut();
        record.scroll_offset = record.scroll_offset.saturating_add(rows).min(bottom);
        record.following_bottom = record.scroll_offset == bottom;
        record.focus = TranscriptFocus::Viewport;
    }

    fn scroll_to_start(&mut self) {
        let record = self.active_record_mut();
        record.following_bottom = false;
        record.scroll_offset = 0;
        record.focus = TranscriptFocus::Viewport;
    }

    fn scroll_to_end(&mut self) {
        let scroll_offset = self.max_scroll_offset();
        let record = self.active_record_mut();
        record.following_bottom = true;
        record.scroll_offset = scroll_offset;
        record.focus = TranscriptFocus::Viewport;
    }

    fn clamp_scroll_offset(&mut self) {
        let bottom = self.max_scroll_offset();
        let record = self.active_record_mut();
        if record.following_bottom {
            record.scroll_offset = bottom;
        } else {
            record.scroll_offset = record.scroll_offset.min(bottom);
        }
    }

    fn max_scroll_offset(&self) -> u16 {
        let area = Rect::new(0, 0, self.size.0.max(1), self.size.1.max(1));
        let layout = screen_layout(
            area,
            self.running,
            self.selected_agent.is_some(),
            self.executions.len(),
        );
        let visible_rows = usize::from(layout.transcript.height.saturating_sub(1));
        saturating_u16(
            transcript_rows(
                &rendered_transcript(&self.view()),
                layout
                    .transcript
                    .width
                    .saturating_sub(TRANSCRIPT_CONTENT_INDENT),
            )
            .saturating_sub(visible_rows),
        )
    }

    fn transcript_page_rows(&self) -> u16 {
        let area = Rect::new(0, 0, self.size.0.max(1), self.size.1.max(1));
        screen_layout(
            area,
            self.running,
            self.selected_agent.is_some(),
            self.executions.len(),
        )
        .transcript
        .height
        .saturating_sub(1)
        .max(1)
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

    fn has_active_execution(&self) -> bool {
        self.executions.iter().any(|execution| {
            matches!(
                execution.state,
                TuiExecutionState::ForegroundRunning | TuiExecutionState::BackgroundRunning
            )
        })
    }

    fn handle_selection_dialog_key(&mut self, key: Key) -> Action {
        match key {
            Key::LineStart
                if self
                    .dialog
                    .as_ref()
                    .is_some_and(|dialog| dialog.session_entries.is_some()) =>
            {
                self.toggle_session_dialog_scope();
                Action::Render
            }
            Key::Char(character) => {
                if character == 'r'
                    && self
                        .dialog
                        .as_ref()
                        .is_some_and(|dialog| dialog.query.is_empty())
                    && let Some(refresh_id) = self
                        .dialog
                        .as_ref()
                        .and_then(|dialog| dialog.refresh_id.clone())
                {
                    return Action::OpenDialog(refresh_id);
                }
                if let Some(dialog) = self.dialog.as_mut() {
                    dialog.query.push(character);
                    refresh_dialog_query_action(dialog);
                }
                self.reset_dialog_selection();
                Action::Render
            }
            Key::Backspace => {
                if let Some(dialog) = self.dialog.as_mut() {
                    dialog.query.pop();
                    refresh_dialog_query_action(dialog);
                }
                self.reset_dialog_selection();
                Action::Render
            }
            Key::DeletePreviousWord => {
                if let Some(dialog) = self.dialog.as_mut() {
                    let boundary =
                        previous_word_boundary(&dialog.query, dialog.query.chars().count());
                    dialog.query.truncate(byte_index(&dialog.query, boundary));
                    refresh_dialog_query_action(dialog);
                }
                self.reset_dialog_selection();
                Action::Render
            }
            Key::Up | Key::Down | Key::ScrollUp | Key::ScrollDown => {
                self.move_dialog_selection(key, 1, true);
                Action::Render
            }
            Key::PageUp | Key::PageDown => {
                self.move_dialog_selection(key, self.dialog_page_rows(), false);
                Action::Render
            }
            Key::Enter => {
                let action = self.dialog.as_ref().and_then(|dialog| {
                    dialog_matches(dialog)
                        .into_iter()
                        .find(|(index, _)| *index == dialog.selected)
                        .and_then(|(_, entry)| entry.action.clone())
                });
                match action {
                    Some(DialogEntryAction::Dispatch(action_id)) => {
                        self.dialog = None;
                        Action::DialogAction(action_id)
                    }
                    Some(DialogEntryAction::SafeDispatch(action_id)) => {
                        self.dialog = None;
                        Action::SafeDialogAction(action_id)
                    }
                    Some(DialogEntryAction::SelectTranscript(id)) => {
                        self.dialog = None;
                        self.select_transcript(id);
                        Action::Render
                    }
                    Some(DialogEntryAction::Cancel) => {
                        self.dialog = None;
                        Action::Render
                    }
                    Some(DialogEntryAction::ToggleDetails) => {
                        if let Some(dialog) = self.dialog.as_mut() {
                            dialog.details_open = !dialog.details_open;
                        }
                        Action::Render
                    }
                    None => Action::Render,
                }
            }
            Key::Escape | Key::CtrlC => {
                let action_id = self
                    .dialog
                    .as_ref()
                    .and_then(|dialog| dialog.cancellation_action.clone());
                self.dialog = None;
                action_id.map_or(Action::Render, Action::SafeDialogAction)
            }
            _ => Action::Render,
        }
    }

    fn toggle_session_dialog_scope(&mut self) {
        let Some(dialog) = self.dialog.as_mut() else {
            return;
        };
        let Some(session_entries) = dialog.session_entries.as_mut() else {
            return;
        };

        session_entries.showing_all_projects = !session_entries.showing_all_projects;
        if session_entries.showing_all_projects {
            dialog.title = "Resume session · All projects".into();
            dialog.help = Some(
                "Type to search | Ctrl+A Current project | Up/Down navigate | Enter resume | Esc cancel"
                    .into(),
            );
            dialog.entries.clone_from(&session_entries.all_projects);
        } else {
            dialog.title = "Resume session · Current project".into();
            dialog.help = Some(
                "Type to search | Ctrl+A All projects | Up/Down navigate | Enter resume | Esc cancel"
                    .into(),
            );
            dialog.entries.clone_from(&session_entries.current_project);
        }
        self.reset_dialog_selection();
    }

    fn move_dialog_selection(&mut self, key: Key, amount: usize, wrap: bool) {
        let Some(dialog) = self.dialog.as_mut() else {
            return;
        };
        let enabled = dialog_matches(dialog)
            .into_iter()
            .filter_map(|(index, entry)| entry.action.as_ref().map(|_| index))
            .collect::<Vec<_>>();
        let Some(position) = enabled.iter().position(|index| *index == dialog.selected) else {
            return;
        };
        let backwards = matches!(key, Key::Up | Key::ScrollUp | Key::PageUp);
        let next = if backwards && wrap {
            (position + enabled.len() - 1) % enabled.len()
        } else if backwards {
            position.saturating_sub(amount)
        } else if wrap {
            (position + 1) % enabled.len()
        } else {
            position.saturating_add(amount).min(enabled.len() - 1)
        };
        dialog.selected = enabled[next];
        dialog.details_open = false;
        self.ensure_dialog_selection_visible();
    }

    fn ensure_dialog_selection_visible(&mut self) {
        let capacity = self.dialog_page_rows();
        let Some(dialog) = self.dialog.as_mut() else {
            return;
        };
        let matches = dialog_matches(dialog);
        let selected = matches
            .iter()
            .position(|(index, _)| *index == dialog.selected)
            .unwrap_or_default();
        dialog.offset = dialog.offset.min(matches.len().saturating_sub(capacity));
        if selected < dialog.offset {
            dialog.offset = selected;
        } else if selected >= dialog.offset.saturating_add(capacity) {
            dialog.offset = selected.saturating_add(1).saturating_sub(capacity);
        }
    }

    fn reset_dialog_selection(&mut self) {
        if let Some(dialog) = self.dialog.as_mut() {
            let matches = dialog_matches(dialog);
            dialog.selected = matches
                .iter()
                .find(|(_, entry)| entry.action.is_some())
                .or_else(|| matches.first())
                .map(|(index, _)| *index)
                .unwrap_or_default();
            dialog.offset = 0;
            dialog.details_open = false;
        }
        self.ensure_dialog_selection_visible();
    }

    fn dialog_page_rows(&self) -> usize {
        let Some(dialog) = self.dialog.as_ref() else {
            return 1;
        };
        let area = Rect::new(0, 0, self.size.0.max(1), self.size.1.max(1));
        let height = dialog_area(area, dialog).height;
        dialog_content_layout(dialog, height).entry_rows
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
        let active = self
            .transcripts
            .get(&self.active_transcript)
            .expect("active transcript always exists");
        let (completed, conversation) = if self.active_transcript == TranscriptId::Main {
            (&self.completed_conversations, self.conversation.as_ref())
        } else {
            (
                &active.completed_conversations,
                active.conversation.as_ref(),
            )
        };
        let completed_call_ids = completed
            .iter()
            .chain(conversation)
            .flat_map(|conversation| &conversation.tool_batches)
            .flat_map(|batch| &batch.calls)
            .filter(|call| call.result.is_some())
            .map(|call| call.call_id.clone())
            .collect::<Vec<_>>();
        if completed_call_ids.is_empty() {
            return;
        }

        let collapsed = &mut self.active_record_mut().collapsed_tool_outputs;
        if completed_call_ids
            .iter()
            .all(|call_id| !collapsed.contains(call_id))
        {
            collapsed.extend(completed_call_ids);
        } else {
            for call_id in completed_call_ids {
                collapsed.remove(&call_id);
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

fn status_matches_execution(status: TuiSubagentStatus, state: TuiExecutionState) -> bool {
    matches!(
        (status, state),
        (
            TuiSubagentStatus::Success,
            TuiExecutionState::CompletedRecent
        ) | (TuiSubagentStatus::Failure, TuiExecutionState::Failed)
            | (TuiSubagentStatus::Cancelled, TuiExecutionState::Cancelled)
    )
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

fn bounded_dialog_multiline(value: &str, limit: usize) -> String {
    value
        .chars()
        .filter(|character| *character == '\n' || !character.is_control())
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

fn keyboard_enhancement_flags() -> KeyboardEnhancementFlags {
    KeyboardEnhancementFlags::REPORT_EVENT_TYPES
        | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
        | KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
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
            TerminalOperation::EnableKeyboardEnhancement => {
                if !crossterm_terminal::supports_keyboard_enhancement()? {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "terminal does not support keyboard enhancement",
                    ));
                }
                execute!(
                    self.stdout,
                    PushKeyboardEnhancementFlags(keyboard_enhancement_flags())
                )
                .map(|_| ())
            }
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
    let terminal = Terminal::enter()?;
    sync_terminal_size(tui)?;
    run_with_runtime_terminal(tui, renderer, terminal)
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
            | Action::SubmitBackground(_)
            | Action::TransitionToBackground(_)
            | Action::OpenDialog(_)
            | Action::DialogAction(_)
            | Action::SafeDialogAction(_)
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
    sync_terminal_size(tui)?;
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
            Action::Render
            | Action::SubmitBackground(_)
            | Action::TransitionToBackground(_)
            | Action::OpenDialog(_)
            | Action::DialogAction(_)
            | Action::SafeDialogAction(_)
            | Action::Cancel => renderer.render(tui.view())?,
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

struct PermissionBridgeTeardown(Option<TuiPermissionBridge>);
impl Drop for PermissionBridgeTeardown {
    fn drop(&mut self) {
        let _ = self.0.as_ref().is_some_and(TuiPermissionBridge::close);
    }
}

pub fn run_with_default_progress_submit(
    tui: &mut Tui<impl Engine + Send>,
    route: impl Fn(TuiRouteRequest, mpsc::Sender<TuiRouteProgress>) -> TuiSubmissionOutcome
    + Send
    + Sync
    + 'static,
    submit: impl Fn(
        String,
        bool,
        mpsc::Sender<TurnEvent>,
        BridgeTx<TuiRuntimeEvent>,
    ) -> TuiProviderOutcome
    + Send
    + Sync
    + 'static,
) -> io::Result<()> {
    run_with_default_progress_submit_with_permissions(tui, route, submit, |_| false, None)
}

pub fn run_with_default_progress_submit_with_permissions<E, R, F, C>(
    tui: &mut Tui<E>,
    route: R,
    submit: F,
    transition: C,
    permissions: Option<(TuiPermissionBridge, mpsc::Receiver<TuiPermissionRequest>)>,
) -> io::Result<()>
where
    E: Engine + Send,
    R: Fn(TuiRouteRequest, mpsc::Sender<TuiRouteProgress>) -> TuiSubmissionOutcome
        + Send
        + Sync
        + 'static,
    F: Fn(String, bool, mpsc::Sender<TurnEvent>, BridgeTx<TuiRuntimeEvent>) -> TuiProviderOutcome
        + Send
        + Sync
        + 'static,
    C: Fn(u64) -> bool + Send + Sync + 'static,
{
    let route = Arc::new(route);
    let submit = Arc::new(submit);
    let transition = Arc::new(transition);
    let (sender, receiver) = mpsc::channel();
    let (completion_sender, completion_receiver) = mpsc::channel();
    let (route_sender, route_receiver) = mpsc::channel();
    let (route_progress_sender, route_progress_receiver) = mpsc::channel();
    let (metrics_sender, metrics_receiver) = BridgeTx::bounded(128);
    let (permission_bridge, permission_requests) = permissions.unzip();
    let _permission_teardown = PermissionBridgeTeardown(permission_bridge.clone());
    let mut active_permission = None;
    let mut runtime_terminal = Terminal::enter()?;
    sync_terminal_size(tui)?;
    let terminal = RatatuiTerminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut renderer = RatatuiRenderer::new(terminal);
    renderer.render(tui.view())?;

    loop {
        for _ in 0..32 {
            let Ok(envelope) = metrics_receiver.try_recv() else {
                break;
            };
            let (ordinal, event) = envelope.into_parts();
            tui.apply_runtime_event_with_ordinal(ordinal, event);
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
                let outcome = submit(prompt, false, sender, metrics);
                let _ = completion_sender.send(outcome);
            });
        }
        if active_permission.is_none()
            && let (Some(permission_bridge), Some(permission_requests)) =
                (permission_bridge.as_ref(), permission_requests.as_ref())
            && let Ok(request) = permission_requests.try_recv()
            && permission_bridge.is_pending(request.id())
        {
            active_permission = Some(request.id());
            let (tool, target) = request.details();
            let entries = [
                ("Allow once", "allow-once"),
                ("Always allow", "allow-always"),
                ("Deny once", "deny-once"),
                ("Always deny", "deny-always"),
            ]
            .into_iter()
            .map(|(label, answer)| {
                DialogEntry::action(label, format!("permission:{}:{answer}", request.id()))
            })
            .collect();
            tui.show_selection_dialog(DialogView::selection(
                "Permission required",
                Some(format!("{tool}\n{target}")),
                entries,
            ));
        }
        renderer.render(tui.view())?;
        let Some(event) = runtime_terminal.poll(Duration::from_millis(25))? else {
            continue;
        };
        let cancel_permission = matches!(event, Event::Key(Key::Escape | Key::CtrlC));
        match tui.handle(event) {
            Action::Quit => {
                return Ok(());
            }
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
            Action::SubmitBackground(prompt) => {
                let submit = Arc::clone(&submit);
                let sender = sender.clone();
                let metrics = metrics_sender.clone();
                let completion_sender = completion_sender.clone();
                thread::spawn(move || {
                    let outcome = submit(prompt, true, sender, metrics);
                    let _ = completion_sender.send(outcome);
                });
            }
            Action::TransitionToBackground(id) => {
                let _ = transition(id);
            }
            Action::OpenDialog(route_id) => {
                let outcome = route(
                    TuiRouteRequest::OpenDialog(route_id),
                    route_progress_sender.clone(),
                );
                let _ = route_sender.send(outcome);
            }
            Action::DialogAction(action_id) => {
                if let Some((id, reply)) = parse_permission_reply(&action_id) {
                    if let Some(permission_bridge) = permission_bridge.as_ref() {
                        let _ = permission_bridge.reply(id, reply);
                    }
                    active_permission = None;
                    continue;
                }
                tui.begin_route();
                let route = Arc::clone(&route);
                let route_sender = route_sender.clone();
                let progress = route_progress_sender.clone();
                thread::spawn(move || {
                    let outcome = route(TuiRouteRequest::DialogAction(action_id), progress);
                    let _ = route_sender.send(outcome);
                });
            }
            Action::SafeDialogAction(action_id) => {
                let outcome = route(
                    TuiRouteRequest::DialogAction(action_id),
                    route_progress_sender.clone(),
                );
                let _ = route_sender.send(outcome);
            }
            Action::Render | Action::Cancel => {
                if cancel_permission
                    && let (Some(id), Some(permission_bridge)) =
                        (active_permission.take(), permission_bridge.as_ref())
                {
                    let _ = permission_bridge.reply(id, TuiPermissionReply::Cancelled);
                }
            }
        }
    }
}

fn parse_permission_reply(action_id: &str) -> Option<(u64, TuiPermissionReply)> {
    let mut parts = action_id.split(':');
    let (Some("permission"), Some(id), Some(answer), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    let reply = match answer {
        "allow-once" => TuiPermissionReply::AllowOnce,
        "allow-always" => TuiPermissionReply::AllowAlways,
        "deny-once" => TuiPermissionReply::DenyOnce,
        "deny-always" => TuiPermissionReply::DenyAlways,
        _ => return None,
    };
    id.parse().ok().map(|id| (id, reply))
}

fn sync_terminal_size<E: Engine>(tui: &mut Tui<E>) -> io::Result<()> {
    let (width, height) = crossterm_terminal::size()?;
    tui.handle(Event::Resize { width, height });
    Ok(())
}

fn map_event(event: CrosstermEvent) -> Option<Event> {
    match event {
        CrosstermEvent::Resize(width, height) => Some(Event::Resize { width, height }),
        CrosstermEvent::Key(key) if key.kind == KeyEventKind::Press => map_key(key),
        CrosstermEvent::Mouse(mouse) if mouse.kind == MouseEventKind::ScrollUp => {
            Some(Event::Key(Key::ScrollUp))
        }
        CrosstermEvent::Mouse(mouse) if mouse.kind == MouseEventKind::ScrollDown => {
            Some(Event::Key(Key::ScrollDown))
        }
        CrosstermEvent::Paste(text) => Some(Event::Paste(text)),
        _ => None,
    }
}

fn map_key(event: KeyEvent) -> Option<Event> {
    if event.kind != KeyEventKind::Press {
        return None;
    }

    let key = match (event.code, event.modifiers) {
        (KeyCode::Char('c' | 'C'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            Key::CtrlC
        }
        (KeyCode::Char('o' | 'O'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            Key::CtrlO
        }
        (KeyCode::Char('a'), modifiers)
            if modifiers == KeyModifiers::CONTROL | KeyModifiers::SHIFT =>
        {
            Key::CtrlShiftA
        }
        (KeyCode::Char('A'), modifiers) if modifiers == KeyModifiers::CONTROL => Key::CtrlShiftA,
        (KeyCode::Char('b' | 'B'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            Key::CtrlB
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
            Key::LineStart
        }
        (KeyCode::Char('e' | 'E'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            Key::LineEnd
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
        (KeyCode::F(5), _) => Key::F5,
        (KeyCode::F(6), _) => Key::F6,
        (KeyCode::F(7), _) => Key::F7,
        (KeyCode::F(8), _) => Key::F8,
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

    struct QuitRuntime {
        guard: TerminalModeGuard,
        control: RecordingControl,
    }

    impl QuitRuntime {
        fn new() -> (Self, Rc<RefCell<Vec<TerminalOperation>>>) {
            let mut control = RecordingControl::default();
            let calls = Rc::clone(&control.calls);
            let guard = TerminalModeGuard::enter(&mut control).unwrap();

            (Self { guard, control }, calls)
        }
    }

    impl RuntimeTerminal for QuitRuntime {
        fn poll(&mut self, _: Duration) -> io::Result<Option<Event>> {
            Ok(Some(Event::Key(Key::CtrlC)))
        }
    }

    impl Drop for QuitRuntime {
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
    fn terminal_setup_enables_exactly_the_required_keyboard_enhancement_flags() {
        assert_eq!(
            keyboard_enhancement_flags(),
            KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                | KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        );
    }

    #[test]
    fn runtime_restores_each_mode_once_after_successful_quit() {
        let (terminal, calls) = QuitRuntime::new();
        let mut tui = Tui::new(NoopEngine);
        let mut renderer = FailingRenderer {
            fail_on_render: usize::MAX,
            renders: 0,
        };

        run_with_runtime_terminal(&mut tui, &mut renderer, terminal).unwrap();

        assert_eq!(*calls.borrow(), expected_terminal_calls());
    }

    #[test]
    fn maps_control_o_to_tool_output_toggle() {
        assert_eq!(
            map_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL)),
            Some(Event::Key(Key::CtrlO))
        );
    }

    #[test]
    fn first_press_dialog_route_opens_once_for_grounded_ctrl_shift_a_encodings() {
        let accepted = [
            (
                KeyCode::Char('a'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            (KeyCode::Char('A'), KeyModifiers::CONTROL),
        ];

        for (code, modifiers) in accepted {
            let mut tui = Tui::new(NoopEngine);
            let event = CrosstermEvent::Key(KeyEvent::new_with_kind(
                code,
                modifiers,
                KeyEventKind::Press,
            ));

            assert_eq!(
                map_event(event).map(|event| tui.handle(event)),
                Some(Action::OpenDialog("subagent".into()))
            );
        }

        for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
            let event = KeyEvent::new_with_kind(
                KeyCode::Char('a'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                kind,
            );
            assert_eq!(map_event(CrosstermEvent::Key(event)), None);
            assert_eq!(map_key(event), None);
        }

        for (code, modifiers) in [
            (KeyCode::Char('a'), KeyModifiers::CONTROL),
            (
                KeyCode::Char('a'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT | KeyModifiers::ALT,
            ),
            (
                KeyCode::Char('A'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
        ] {
            let event = KeyEvent::new_with_kind(code, modifiers, KeyEventKind::Press);
            assert_ne!(map_key(event), Some(Event::Key(Key::CtrlShiftA)));
        }

        assert_eq!(
            map_key(KeyEvent::new_with_kind(
                KeyCode::Char('a'),
                KeyModifiers::CONTROL,
                KeyEventKind::Press,
            )),
            Some(Event::Key(Key::LineStart))
        );

        let mut tui = Tui::new(NoopEngine);
        tui.show_selection_dialog(DialogView::selection(
            "Choose",
            None::<String>,
            vec![DialogEntry::action("Alpha", "alpha")],
        ));
        tui.handle(Event::Key(Key::Char('a')));

        assert_eq!(
            map_event(CrosstermEvent::Key(KeyEvent::new_with_kind(
                KeyCode::Esc,
                KeyModifiers::NONE,
                KeyEventKind::Press,
            )))
            .map(|event| tui.handle(event)),
            Some(Action::Render)
        );
        assert!(tui.view().dialog.is_none());
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

    #[test]
    fn maps_real_mouse_wheel_events_to_scroll_keys() {
        for (kind, key) in [
            (crossterm::event::MouseEventKind::ScrollUp, Key::ScrollUp),
            (
                crossterm::event::MouseEventKind::ScrollDown,
                Key::ScrollDown,
            ),
        ] {
            assert_eq!(
                map_event(CrosstermEvent::Mouse(crossterm::event::MouseEvent {
                    kind,
                    column: 4,
                    row: 2,
                    modifiers: KeyModifiers::NONE,
                })),
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
            (KeyCode::Char('a'), ctrl, Key::LineStart),
            (KeyCode::Char('e'), ctrl, Key::LineEnd),
            (KeyCode::Char('b'), ctrl, Key::CtrlB),
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
