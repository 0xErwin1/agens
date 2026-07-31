//! Terminal lifecycle and input-event boundary for the interactive surface.

mod app;
mod bridge;
mod conversation;
mod render;
mod terminal;
mod widgets;

pub use agens_bus::{BridgeCancel, BridgeTx, PublishOutcome, UiEnvelope};
pub use agens_core::{
    DiffLine, DiffLineKind, ToolResultState, TuiExecution, TuiExecutionEvent, TuiExecutionState,
    TuiRuntimeEvent, TuiSubagentEvent,
};
pub use app::{AppEvent, AppState, Command, Dialog, Effect, Runtime};
pub use bridge::{TuiPermissionBridge, TuiPermissionReply, TuiPermissionRequest};
pub use conversation::{
    ActionableError, Conversation, ConversationError, ConversationEvent, SubagentCard, ToolBatch,
    ToolCall, ToolResult,
};
pub use terminal::{
    PendingPermissions, PermissionReply, TerminalControl, TerminalModeGuard, TerminalOperation,
    teardown,
};
pub use widgets::DisplayMode;

use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, VecDeque},
    io::{self, Stdout, Write},
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use agens_core::SubagentStatus;
use agens_core::SubmitOrigin;
use agens_core::{MessagePart, TurnEvent, TurnState, Usage};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use crossterm::{
    cursor::{Hide as HideCursor, Show as ShowCursor},
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
    layout::{Alignment, Constraint, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Padding, Paragraph, Wrap},
};
use unicode_width::UnicodeWidthStr;

const TRANSCRIPT_CONTENT_INDENT: u16 = 4;
/// Chrome padding left of a transcript row, once the row itself spends
/// [`widgets::ACCENT_WIDTH`] on its accent column.
///
/// The accent bar is carved out of the indent instead of added to it, so bullets
/// and content keep the screen columns they had before the bar existed.
const TRANSCRIPT_ROW_INDENT: u16 = TRANSCRIPT_CONTENT_INDENT - widgets::ACCENT_WIDTH as u16;
const TRANSCRIPT_TOP_BORDER_ROWS: u16 = 1;
const MAX_CHILD_TRANSCRIPTS: usize = 64;
const PROGRESS_CHANNEL_BUDGET: usize = 32;
const TERMINAL_WHEEL_BATCH_BUDGET: usize = 64;
const MOUSE_SCROLL_ROWS: u16 = 6;
const TERMINAL_POLL_INTERVAL: Duration = Duration::from_millis(25);
const ACTIVE_FRAME_HEARTBEAT: Duration = Duration::from_millis(80);
const EXIT_WARNING_WINDOW: Duration = Duration::from_secs(2);
const MAX_SELECTION_COPY_BYTES: usize = 64 * 1024;

const fn transcript_chrome_rows(following_bottom: bool) -> u16 {
    if following_bottom {
        TRANSCRIPT_TOP_BORDER_ROWS
    } else {
        TRANSCRIPT_TOP_BORDER_ROWS + 1
    }
}

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
    MouseWheel(MouseWheelDirection),
    MouseDown {
        column: u16,
        row: u16,
    },
    MouseDrag {
        column: u16,
        row: u16,
    },
    MouseUp {
        column: u16,
        row: u16,
    },
    Paste(String),
    /// A terminal resize in columns and rows.
    Resize {
        width: u16,
        height: u16,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MouseWheelDirection {
    Up,
    Down,
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
    /// Collapses or expands detail (thinking-first, else tool outputs).
    CtrlO,
    /// Scrolls the transcript timeline down (composer-safe).
    CtrlJ,
    /// Scrolls the transcript timeline up (composer-safe).
    CtrlK,
    /// Jumps the transcript viewport to the top.
    CtrlG,
    /// Jumps the transcript viewport to the bottom.
    CtrlShiftG,
    /// Jumps to the last user message in the transcript.
    CtrlN,
    /// Jumps to the previous user message in the transcript.
    CtrlShiftN,
    /// Opens the eligible subagent selection dialog.
    CtrlShiftA,
    /// Opens the subagent model profile editor.
    CtrlShiftM,
    /// Toggles the visible dangerous-mode session state through the composition layer.
    CtrlShiftD,
    /// Toggles the visible permission-bypass session state through the composition layer.
    CtrlShiftP,
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
    Up,
    Down,
    Tab,
}

/// The result of handling a single terminal event.
#[derive(Clone, Eq, PartialEq)]
pub enum Action {
    /// Render the current view state.
    Render,
    /// Send this prompt to the composition layer.
    Submit(String),
    /// Submit a redacted credential through the dedicated route only.
    SubmitSecret {
        action_id: String,
        secret: SecretInput,
    },
    SubmitBackground(String),
    TransitionToBackground(u64),
    CancelExecution(u64),
    SendTaskMessage {
        id: u64,
        message: String,
    },
    /// Ask the composition layer to resolve a palette dialog by stable route ID.
    OpenDialog(String),
    /// Load a bounded session-browser page through the composition layer.
    LoadSessionPage(SessionDialogRequest),
    /// Dispatch the selected dialog action through the composition layer.
    DialogAction(String),
    SafeDialogAction(String),
    /// An active engine turn was asked to cancel.
    Cancel,
    /// Copies bounded selected transcript text through an explicit terminal clipboard action.
    CopySelection(String),
    /// Opens the verification page for the active device-authentication overlay.
    OpenDeviceAuthUrl,
    /// Copies the active device-authentication verification URL through OSC-52.
    CopyDeviceAuthUrl,
    /// Copies the active device-authentication code through OSC-52.
    CopyDeviceAuthCode,
    /// A local route was cancelled before its result could be applied.
    CancelRoute,
    /// End the terminal event loop.
    Quit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TuiPresentation {
    provider: String,
    model: String,
    session: String,
    effort: Option<String>,
    context_window: Option<u64>,
    dangerous_mode: bool,
    bypass: bool,
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
            effort: None,
            context_window: None,
            dangerous_mode: false,
            bypass: false,
        }
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn with_effort(mut self, effort: impl Into<String>) -> Self {
        let effort = effort.into();
        self.effort = (!effort.is_empty()).then_some(effort);
        self
    }

    pub fn with_context_window(mut self, context_window: Option<u64>) -> Self {
        self.context_window = context_window.filter(|window| *window > 0);
        self
    }

    pub fn with_dangerous_mode(mut self, enabled: bool) -> Self {
        self.dangerous_mode = enabled;
        self
    }

    pub fn with_bypass(mut self, enabled: bool) -> Self {
        self.bypass = enabled;
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TuiSubmissionOutcome {
    ProviderTurn {
        display: String,
        prompt: String,
    },
    /// Opens an isolated credential-entry overlay.
    SecretEntry(SecretEntryView),
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
        history: Vec<Conversation>,
        draft: Option<String>,
        resume_error: Option<String>,
        /// The `@` picker candidates for the resumed session's OWN root. Without this, a
        /// post-startup resume would leave whatever candidates were set at TUI startup (or by an
        /// earlier session) in place, even though they may now name files outside the resumed
        /// session's own confined root.
        file_candidates: Vec<String>,
        /// The command/skill palette entries for the resumed session's OWN root. Without this,
        /// the composer's rendered autocomplete would keep listing whatever entries were set at
        /// TUI startup (or by an earlier session), even after those names are no longer reachable
        /// once the session is confined to a different root.
        palette_entries: Vec<PaletteEntry>,
    },
    Dialog(DialogView),
    SafeDialog(DialogView),
    TranscriptDialog,
    SelectionInfo(String),
    SelectionCancelled,
    RouteCancelled,
    SelectionError {
        message: String,
        action: String,
    },
    Quit,
}

#[derive(Clone, Eq, PartialEq)]
pub enum TuiRouteRequest {
    Input(String),
    /// Opens a device-authentication URL through the application adapter.
    DeviceAuthOpenUrl(String),
    SubmitSecret {
        action_id: String,
        secret: SecretInput,
    },
    OpenDialog(String),
    DialogAction(String),
    SessionPage(SessionDialogRequest),
}

const MAX_SECRET_INPUT_BYTES: usize = 8192;
const SECRET_REQUIRED_ERROR: &str = "API key is required.";

/// A credential buffer deliberately kept separate from all presentation input.
#[derive(Clone, Eq, PartialEq)]
pub struct SecretInput(String);

impl std::fmt::Debug for SecretInput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SecretInput(<redacted>)")
    }
}

impl SecretInput {
    /// Consumes the already-normalized secret at the persistence boundary.
    pub fn into_string(self) -> String {
        self.0
    }
}

/// Non-secret metadata for a dedicated credential-entry overlay.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SecretEntryView {
    title: String,
    help: Option<String>,
    submit_action: String,
}

impl SecretEntryView {
    pub fn new<H>(title: impl AsRef<str>, help: Option<H>, submit_action: impl AsRef<str>) -> Self
    where
        H: AsRef<str>,
    {
        Self {
            title: bounded_dialog_text(title.as_ref(), 64),
            help: help.map(|help| bounded_dialog_text(help.as_ref(), 256)),
            submit_action: bounded_dialog_text(submit_action.as_ref(), 128),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SecretEntryRender<'a> {
    title: &'a str,
    help: Option<&'a str>,
    mask: usize,
    error: Option<&'static str>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SecretEntryState {
    view: SecretEntryView,
    input: SecretInput,
    error: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DeviceAuthRender<'a> {
    verification_url: &'a str,
    user_code: &'a str,
    selected: usize,
    confirmation: Option<&'static str>,
}

#[derive(Clone)]
struct DeviceAuthState {
    verification_url: String,
    user_code: String,
    selected: usize,
    confirmation: Option<&'static str>,
}

impl std::fmt::Debug for Action {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SubmitSecret { action_id, .. } => formatter
                .debug_struct("SubmitSecret")
                .field("action_id", action_id)
                .field("secret", &"<redacted>")
                .finish(),
            Self::Render => formatter.write_str("Render"),
            Self::Submit(value) => formatter.debug_tuple("Submit").field(value).finish(),
            Self::SubmitBackground(value) => formatter
                .debug_tuple("SubmitBackground")
                .field(value)
                .finish(),
            Self::TransitionToBackground(id) => formatter
                .debug_tuple("TransitionToBackground")
                .field(id)
                .finish(),
            Self::CancelExecution(id) => {
                formatter.debug_tuple("CancelExecution").field(id).finish()
            }
            Self::SendTaskMessage { id, message } => formatter
                .debug_struct("SendTaskMessage")
                .field("id", id)
                .field("message", message)
                .finish(),
            Self::OpenDialog(value) => formatter.debug_tuple("OpenDialog").field(value).finish(),
            Self::LoadSessionPage(value) => formatter
                .debug_tuple("LoadSessionPage")
                .field(value)
                .finish(),
            Self::DialogAction(value) => {
                formatter.debug_tuple("DialogAction").field(value).finish()
            }
            Self::SafeDialogAction(value) => formatter
                .debug_tuple("SafeDialogAction")
                .field(value)
                .finish(),
            Self::Cancel => formatter.write_str("Cancel"),
            Self::CopySelection(value) => {
                formatter.debug_tuple("CopySelection").field(value).finish()
            }
            Self::OpenDeviceAuthUrl => formatter.write_str("OpenDeviceAuthUrl"),
            Self::CopyDeviceAuthUrl => formatter.write_str("CopyDeviceAuthUrl"),
            Self::CopyDeviceAuthCode => formatter.write_str("CopyDeviceAuthCode"),
            Self::CancelRoute => formatter.write_str("CancelRoute"),
            Self::Quit => formatter.write_str("Quit"),
        }
    }
}

impl std::fmt::Debug for TuiRouteRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SubmitSecret { action_id, .. } => formatter
                .debug_struct("SubmitSecret")
                .field("action_id", action_id)
                .field("secret", &"<redacted>")
                .finish(),
            Self::Input(value) => formatter.debug_tuple("Input").field(value).finish(),
            Self::DeviceAuthOpenUrl(_) => formatter.write_str("DeviceAuthOpenUrl(<redacted>)"),
            Self::OpenDialog(value) => formatter.debug_tuple("OpenDialog").field(value).finish(),
            Self::DialogAction(value) => {
                formatter.debug_tuple("DialogAction").field(value).finish()
            }
            Self::SessionPage(value) => formatter.debug_tuple("SessionPage").field(value).finish(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TuiRouteProgress {
    BrowserUrl(String),
    DeviceCode {
        verification_url: String,
        user_code: String,
    },
}

const ROUTE_ACTIVE: u8 = 0;
const ROUTE_CANCELLED: u8 = 1;
const ROUTE_COMMITTED: u8 = 2;

#[derive(Clone, Debug, Default)]
pub struct TuiRouteCancellation(Arc<AtomicU8>);

impl TuiRouteCancellation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) -> bool {
        self.0
            .compare_exchange(
                ROUTE_ACTIVE,
                ROUTE_CANCELLED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire) == ROUTE_CANCELLED
    }

    pub fn try_commit(&self) -> bool {
        self.0
            .compare_exchange(
                ROUTE_ACTIVE,
                ROUTE_COMMITTED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }
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

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct TranscriptPosition {
    pub row: usize,
    pub column: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TranscriptSelection {
    pub anchor: TranscriptPosition,
    pub head: TranscriptPosition,
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
    tool_display_modes: BTreeMap<String, widgets::DisplayMode>,
    collapse_thinking: bool,
    /// When true, auto-collapse on turn finish is skipped (user re-expanded via Ctrl+O).
    thinking_user_pinned: bool,
    focus: TranscriptFocus,
    selection: Option<TranscriptSelection>,
    selection_text: Option<String>,
    selection_too_large: bool,
    selecting: bool,
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
            tool_display_modes: BTreeMap::new(),
            collapse_thinking: false,
            thinking_user_pinned: false,
            focus: TranscriptFocus::Composer,
            selection: None,
            selection_text: None,
            selection_too_large: false,
            selecting: false,
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
    /// Whether the composer contains a recovered failed prompt that can be retried or discarded.
    pub recovered_failed_prompt: bool,
    /// Current terminal dimensions.
    pub size: (u16, u16),
    /// Whether the composed engine has an active turn.
    pub running: bool,
    /// Whether a local session restore is being prepared without starting a provider turn.
    pub session_loading: bool,
    /// Whether the current assistant item can still receive ordered text deltas.
    pub assistant_streaming: bool,
    /// Whether a second Ctrl+C inside the active warning window will quit.
    pub quit_armed: bool,
    /// Conversation entries rendered in the order they occurred.
    pub transcript: &'a [TranscriptEntry],
    /// Whether new output advances to the bottom of the transcript.
    pub following_bottom: bool,
    /// The manual transcript offset when bottom following is disabled.
    pub scroll_offset: u16,
    /// Absolute wrapped transcript anchors used to paint application-owned mouse selection.
    pub selection: Option<TranscriptSelection>,
    /// Current provider and model selected by the CLI composition root.
    pub provider_model: &'a str,
    /// Optional reasoning effort label for the footer.
    pub reasoning_effort: Option<&'a str>,
    /// Known model context window supplied by the CLI composition root.
    pub context_window: Option<u64>,
    /// Active session label supplied by the CLI composition root.
    pub session: &'a str,
    /// Project label displayed in the operational footer.
    pub project: &'a str,
    /// Current active-turn state for the dedicated status row.
    pub turn_state: Option<TurnState>,
    /// Whether the next submitted turn will carry dangerous-mode context.
    pub dangerous_mode: bool,
    /// Whether the session's permission-bypass mode is active.
    pub bypass: bool,
    /// Tool name currently being dispatched, when known.
    pub active_tool: Option<&'a str>,
    /// Current character cursor position in the editable prompt.
    pub input_cursor: usize,
    /// Typed metrics retained for rich, lossless presentation.
    pub runtime_events: &'a [TuiRuntimeEvent],
    pub turn_duration: Option<Duration>,
    pub latest_usage: Option<&'a Usage>,
    pub status: Option<&'a str>,
    /// Monotonic clock advanced by [`Tui::tick`] for active-state glyphs.
    pub now: Duration,
    /// Authoritative typed conversation projection, when a turn is active or completed.
    pub conversation: Option<&'a Conversation>,
    /// Completed typed conversations retained before the active turn.
    pub completed_conversations: &'a [Conversation],
    pub highlight_restored_syntax: bool,
    /// Per-call presentation mode; their source output remains retained regardless of mode.
    pub tool_display_modes: &'a BTreeMap<String, widgets::DisplayMode>,
    /// Whether complete reasoning is collapsed according to the UI setting.
    pub collapse_thinking: bool,
    pub focus: TranscriptFocus,
    /// A bounded informational dialog rendered above the conversation.
    pub dialog: Option<&'a DialogView>,
    /// Redacted credential-entry presentation; it carries only a mask length and fixed error.
    secret_entry: Option<SecretEntryRender<'a>>,
    /// Active device-authentication flow kept outside generic dialogs so its actions remain local.
    device_auth: Option<DeviceAuthRender<'a>>,
    /// Slash palette metadata and current filtered selection.
    pub palette: Option<PaletteView<'a>>,
    /// Open `@` file picker, its typed query, and its current selection.
    pub file_picker: Option<FilePickerView<'a>>,
    pub agent_catalog: &'a [String],
    pub selected_agent: Option<&'a str>,
    pub executions: Vec<&'a TuiExecution>,
    pub execution_selection: Option<TranscriptId>,
    /// Live tool activity of the focused subagent, for the tree's child rows.
    pub execution_activities: Vec<TuiExecutionActivity>,
    /// Tick clock reading when the active turn began, for live elapsed time.
    pub turn_started_at: Option<Duration>,
}

/// One child row of the subagent tree: a bounded, name-only activity label.
///
/// Only activities with a known native label are surfaced, so an unknown or MCP
/// tool never leaks its name or arguments into the navigation surface.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TuiExecutionActivity {
    pub parent: u64,
    pub label: String,
    pub running: bool,
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

impl<'a> PaletteView<'a> {
    pub fn entries(&self) -> &'a [PaletteEntry] {
        self.entries
    }

    pub fn selected(&self) -> usize {
        self.selected
    }
}

/// Open `@` reference: character index of the `@` and the current row selection.
#[derive(Clone, Copy, Debug)]
struct FilePicker {
    anchor: usize,
    selected: usize,
}

/// Composer-anchored `@` reference picker: the project files it can insert, the
/// token typed after the `@`, and the current row selection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FilePickerView<'a> {
    candidates: &'a [String],
    query: &'a str,
    selected: usize,
}

impl<'a> FilePickerView<'a> {
    /// The reference token typed after the `@`, without the `@` itself.
    pub fn query(&self) -> &'a str {
        self.query
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Project-relative paths matching the current query, in candidate order.
    pub fn matches(&self) -> Vec<&'a str> {
        file_picker_matches(self.candidates, self.query)
    }
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
    id: Option<String>,
}

impl DialogEntry {
    fn transcript(label: impl AsRef<str>, id: TranscriptId) -> Self {
        let mut entry = Self::action(label, "");
        entry.action = Some(DialogEntryAction::SelectTranscript(id));
        entry
    }

    fn transcript_with_detail(
        label: impl AsRef<str>,
        detail: impl AsRef<str>,
        id: TranscriptId,
    ) -> Self {
        let mut entry = Self::action_with_detail(label, Some(detail), "");
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
            id: None,
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
            id: None,
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
            id: None,
        }
    }

    /// Attaches the row identity substituted into selected-key action templates.
    pub fn with_id(mut self, id: impl AsRef<str>) -> Self {
        self.id = Some(bounded_dialog_text(id.as_ref(), 128));
        self
    }

    pub fn cancel(label: impl AsRef<str>) -> Self {
        Self {
            label: bounded_dialog_text(label.as_ref(), 128),
            detail: None,
            search_text: None,
            selected_detail: None,
            action: Some(DialogEntryAction::Cancel),
            id: None,
        }
    }

    pub fn disabled(label: impl AsRef<str>, detail: impl AsRef<str>) -> Self {
        Self {
            label: bounded_dialog_text(label.as_ref(), 128),
            detail: Some(bounded_dialog_text(detail.as_ref(), 256)),
            search_text: None,
            selected_detail: None,
            action: None,
            id: None,
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
            id: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionDialogScope {
    CurrentProject,
    AllProjects,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionDialogCursor {
    updated_at: i64,
    id: i64,
}

impl SessionDialogCursor {
    pub const fn new(updated_at: i64, id: i64) -> Self {
        Self { updated_at, id }
    }

    pub const fn updated_at(self) -> i64 {
        self.updated_at
    }

    pub const fn id(self) -> i64 {
        self.id
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionDialogRequest {
    scope: SessionDialogScope,
    query: String,
    cursor: Option<SessionDialogCursor>,
    previous_cursors: Vec<Option<SessionDialogCursor>>,
    page: u64,
    generation: u64,
}

impl SessionDialogRequest {
    pub fn initial() -> Self {
        Self {
            scope: SessionDialogScope::CurrentProject,
            query: String::new(),
            cursor: None,
            previous_cursors: Vec::new(),
            page: 1,
            generation: 0,
        }
    }

    pub const fn scope(&self) -> SessionDialogScope {
        self.scope
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub const fn cursor(&self) -> Option<SessionDialogCursor> {
        self.cursor
    }

    pub const fn page(&self) -> u64 {
        self.page
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SessionDialogEntries {
    request: SessionDialogRequest,
    next_cursor: Option<SessionDialogCursor>,
    loading: bool,
    error: Option<String>,
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
    /// Whether typed characters filter instead of acting as keybindings.
    ///
    /// Armed by [`DIALOG_SEARCH_KEY`] and disarmed by Escape. Without this gate
    /// every single-key binding a dialog registers — refresh, the Confirm
    /// answers, list navigation — would be swallowed as filter text.
    searching: bool,
    selected: usize,
    offset: usize,
    interactive: bool,
    session_entries: Option<SessionDialogEntries>,
    query_action: Option<DialogQueryAction>,
    refresh_id: Option<String>,
    details_open: bool,
    empty_message: Option<String>,
    cancellation_action: Option<String>,
    shortcut_actions: Vec<(char, String)>,
    selected_key_actions: Vec<(Key, String)>,
    overlay_kind: widgets::OverlayKind,
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
            searching: false,
            selected,
            offset: 0,
            interactive: true,
            session_entries: None,
            query_action: None,
            refresh_id: None,
            details_open: false,
            empty_message: None,
            cancellation_action: None,
            shortcut_actions: Vec::new(),
            selected_key_actions: Vec::new(),
            overlay_kind: widgets::OverlayKind::Picker,
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

    pub fn help(&self) -> Option<&str> {
        self.help.as_deref()
    }

    pub fn with_empty_message(mut self, message: impl AsRef<str>) -> Self {
        self.empty_message = Some(bounded_dialog_text(message.as_ref(), 256));
        self
    }

    pub fn with_cancellation_action(mut self, action_id: impl AsRef<str>) -> Self {
        self.cancellation_action = Some(bounded_dialog_text(action_id.as_ref(), 128));
        self
    }

    pub fn with_shortcut_action(mut self, key: char, action_id: impl AsRef<str>) -> Self {
        self.shortcut_actions
            .push((key, bounded_dialog_text(action_id.as_ref(), 128)));
        self
    }

    /// Registers a template dispatched for the selected row when `key` is pressed.
    /// The literal `{selected}` is replaced with the selected entry's `with_id`
    /// identity; without an identity the key keeps its default behavior.
    pub fn with_selected_key_action(mut self, key: Key, template: impl AsRef<str>) -> Self {
        self.selected_key_actions
            .push((key, bounded_dialog_text(template.as_ref(), 128)));
        self
    }

    /// Marks this dialog as a Confirm overlay (short keys a/d/A/D before query typing).
    pub fn as_confirm(mut self) -> Self {
        self.overlay_kind = widgets::OverlayKind::Confirm;
        self
    }

    pub fn sessions_page(
        entries: Vec<DialogEntry>,
        request: SessionDialogRequest,
        next_cursor: Option<SessionDialogCursor>,
    ) -> Self {
        let entries = entries.into_iter().take(64).collect::<Vec<_>>();
        let mut dialog = Self::selection(
            session_dialog_title(request.scope),
            Some(session_dialog_help(request.scope)),
            entries,
        );
        dialog.query.clone_from(&request.query);
        dialog.session_entries = Some(SessionDialogEntries {
            request,
            next_cursor,
            loading: false,
            error: None,
        });
        dialog
    }

    pub fn sessions_loading(request: SessionDialogRequest) -> Self {
        let mut dialog = Self::sessions_page(Vec::new(), request, None);
        if let Some(session_entries) = dialog.session_entries.as_mut() {
            session_entries.loading = true;
        }
        dialog
    }

    pub fn sessions_error(request: SessionDialogRequest, message: impl AsRef<str>) -> Self {
        let mut dialog = Self::sessions_page(Vec::new(), request, None);
        if let Some(session_entries) = dialog.session_entries.as_mut() {
            session_entries.error = Some(bounded_dialog_text(message.as_ref(), 256));
        }
        dialog
    }

    pub fn is_loading(&self) -> bool {
        self.session_entries
            .as_ref()
            .is_some_and(|entries| entries.loading)
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
            searching: false,
            selected: 0,
            offset: 0,
            interactive: false,
            session_entries: None,
            query_action: None,
            refresh_id: None,
            details_open: false,
            empty_message: None,
            cancellation_action: None,
            shortcut_actions: Vec::new(),
            selected_key_actions: Vec::new(),
            overlay_kind: widgets::OverlayKind::Picker,
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

fn session_dialog_title(scope: SessionDialogScope) -> &'static str {
    match scope {
        SessionDialogScope::CurrentProject => "Resume session · Current project",
        SessionDialogScope::AllProjects => "Resume session · All projects",
    }
}

fn session_dialog_help(scope: SessionDialogScope) -> &'static str {
    match scope {
        SessionDialogScope::CurrentProject => {
            "/ search | Ctrl+A All projects | Up/Down navigate | PgUp/PgDn page | Enter resume | Esc cancel"
        }
        SessionDialogScope::AllProjects => {
            "/ search | Ctrl+A Current project | Up/Down navigate | PgUp/PgDn page | Enter resume | Esc cancel"
        }
    }
}

fn dialog_matches(dialog: &DialogView) -> Vec<(usize, &DialogEntry)> {
    if dialog.session_entries.is_some() {
        return dialog.entries.iter().enumerate().collect();
    }

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
    let notice = notice_spans(&state);
    let layout = screen_layout(area, state.input, !notice.is_empty());

    let row_width = layout
        .transcript
        .width
        .saturating_sub(TRANSCRIPT_ROW_INDENT);
    let transcript =
        SelectableTranscript::from_lines(&rendered_transcript(&state, row_width), row_width);
    let visible_rows = layout
        .transcript
        .height
        .saturating_sub(transcript_chrome_rows(state.following_bottom))
        as usize;
    let bottom_scroll = saturating_u16(transcript.rows.len().saturating_sub(visible_rows));
    let scroll = if state.following_bottom {
        bottom_scroll
    } else {
        state.scroll_offset.min(bottom_scroll)
    };
    if layout.transcript.height > 0 {
        let mut transcript_block = Block::default()
            .borders(Borders::TOP)
            .padding(Padding::left(TRANSCRIPT_ROW_INDENT))
            .border_style(Style::default().fg(Color::DarkGray));
        if !state.following_bottom {
            transcript_block = transcript_block
                .title_bottom(Span::styled(
                    format!(" SCROLL {scroll}/{bottom_scroll}"),
                    Style::default().fg(Color::DarkGray),
                ))
                .title_alignment(Alignment::Right);
        }
        frame.render_widget(
            Paragraph::new(Text::from(transcript.render_lines(state.selection)))
                .block(transcript_block)
                .scroll((scroll, 0)),
            layout.transcript,
        );
    }

    if layout.composer.height > 0 && state.active_transcript != TranscriptId::Main {
        let mut dock = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(Color::DarkGray));
        if let Some(metrics) = border_metrics(&state, layout.composer) {
            dock = dock.title_top(metrics);
        }
        frame.render_widget(
            Paragraph::new(" Subagent transcript · i to message · x to cancel")
                .style(Style::default().fg(Color::DarkGray))
                .block(dock),
            layout.composer,
        );
    }

    let composer_color = widgets::RolePalette::muted();
    if layout.composer.height > 0 && state.active_transcript == TranscriptId::Main {
        let (cursor_line, cursor_column) = cursor_position(state.input, state.input_cursor);
        let inner_width = usize::from(layout.composer.width.saturating_sub(2).max(1));
        let inner_height = usize::from(layout.composer.height.saturating_sub(2).max(1));
        let vertical_scroll = cursor_line.saturating_sub(inner_height.saturating_sub(1));
        let horizontal_scroll = cursor_column.saturating_sub(inner_width.saturating_sub(1));
        let mut composer = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(composer_color));
        if let Some(metrics) = border_metrics(&state, layout.composer) {
            composer = composer.title_bottom(metrics);
        }
        frame.render_widget(
            Paragraph::new(state.input).block(composer).scroll((
                saturating_u16(vertical_scroll),
                saturating_u16(horizontal_scroll),
            )),
            layout.composer,
        );
        if inner_width > 0
            && inner_height > 0
            && state.focus == TranscriptFocus::Composer
            && !state.running
            && !state.session_loading
            && state.dialog.is_none()
            && state.palette.is_none()
        {
            let cursor_y = layout
                .composer
                .y
                .saturating_add(1)
                .saturating_add(saturating_u16(cursor_line.saturating_sub(vertical_scroll)));
            let cursor_x = layout.composer.x.saturating_add(saturating_u16(
                cursor_column
                    .saturating_sub(horizontal_scroll)
                    .saturating_add(1),
            ));
            frame.set_cursor_position((cursor_x, cursor_y));
        }
    }

    if layout.notice.height > 0 {
        render_notice(frame, layout.notice, notice);
    }

    if layout.tree.height > 0 {
        frame.render_widget(
            Paragraph::new(Text::from(fitted_subagent_tree_lines(
                &state,
                layout.tree.height,
                layout.tree.width,
            ))),
            layout.tree,
        );
    }

    if layout.footer.height > 0 {
        frame.render_widget(
            Paragraph::new(widgets::MetricFooter::text(
                layout.footer.width,
                footer_context(&state),
            ))
            .style(Style::default().fg(widgets::RolePalette::chrome())),
            layout.footer,
        );
    }

    if let Some(dialog) = state.dialog {
        render_dialog(frame, area, dialog);
    }

    if let Some(palette) = state.palette {
        render_palette(frame, area, layout.composer, state.input, palette);
    }

    if let Some(picker) = state.file_picker {
        render_file_picker(frame, area, layout.composer, picker);
    }

    if let Some(secret_entry) = state.secret_entry {
        render_secret_entry(frame, area, secret_entry);
    }

    if let Some(device_auth) = state.device_auth {
        render_device_auth(frame, area, device_auth);
    }
}

const SECRET_ENTRY_SHORTCUTS: [widgets::OverlayShortcut<'static>; 3] = [
    widgets::OverlayShortcut {
        key: "enter",
        label: "submit",
    },
    widgets::OverlayShortcut {
        key: "esc",
        label: "cancel",
    },
    widgets::OverlayShortcut {
        key: "ctrl-c",
        label: "cancel",
    },
];

fn render_secret_entry(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    secret_entry: SecretEntryRender<'_>,
) {
    let config = widgets::OverlayConfig {
        title: secret_entry.title,
        tabs: None,
        shortcuts: &SECRET_ENTRY_SHORTCUTS,
        sizing: widgets::OverlaySizing::compact(),
        desired_content_rows: 4,
    };
    let Some(layout) = widgets::OverlayLayout::solve(area, &config) else {
        return;
    };
    widgets::OverlayFrame::render(frame, &layout, &config);
    let mut lines = Vec::new();
    if let Some(help) = secret_entry.help {
        lines.push(Line::styled(
            help.to_owned(),
            Style::default().fg(widgets::RolePalette::muted()),
        ));
    }
    if let Some(error) = secret_entry.error {
        lines.push(Line::styled(
            error,
            Style::default().fg(widgets::RolePalette::warning()),
        ));
    }
    lines.push(Line::from(format!(
        "API key: {}",
        "*".repeat(secret_entry.mask)
    )));
    frame.render_widget(Paragraph::new(Text::from(lines)), layout.content);
}

const DEVICE_AUTH_SHORTCUTS: [widgets::OverlayShortcut<'static>; 3] = [
    widgets::OverlayShortcut {
        key: "↑↓",
        label: "select",
    },
    widgets::OverlayShortcut {
        key: "enter",
        label: "run",
    },
    widgets::OverlayShortcut {
        key: "esc",
        label: "cancel",
    },
];

fn render_device_auth(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    device_auth: DeviceAuthRender<'_>,
) {
    let config = widgets::OverlayConfig {
        title: "ChatGPT device authentication",
        tabs: None,
        shortcuts: &DEVICE_AUTH_SHORTCUTS,
        sizing: widgets::OverlaySizing::dialog(),
        desired_content_rows: 9,
    };
    let Some(layout) = widgets::OverlayLayout::solve(area, &config) else {
        return;
    };
    widgets::OverlayFrame::render(frame, &layout, &config);
    let action = |index, label: &str| {
        let prefix = if device_auth.selected == index {
            "› "
        } else {
            "  "
        };
        Line::styled(
            format!("{prefix}{label}"),
            if device_auth.selected == index {
                Style::default()
                    .fg(widgets::RolePalette::selection_fg())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(widgets::RolePalette::chrome())
            },
        )
    };
    let mut lines = vec![
        Line::styled(
            "Verification URL",
            Style::default().fg(widgets::RolePalette::muted()),
        ),
        Line::from(device_auth.verification_url.to_owned()),
        Line::styled(
            "Device code",
            Style::default().fg(widgets::RolePalette::muted()),
        ),
        Line::from(device_auth.user_code.to_owned()),
        Line::styled(
            "Enter this code on the opened page.",
            Style::default().fg(widgets::RolePalette::muted()),
        ),
        action(0, "Open browser"),
        action(1, "Copy link"),
        action(2, "Copy code"),
    ];
    if let Some(confirmation) = device_auth.confirmation {
        lines.push(Line::styled(
            confirmation,
            Style::default().fg(widgets::RolePalette::muted()),
        ));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), layout.content);
}

fn footer_context<'a>(state: &ViewState<'a>) -> widgets::FooterContext<'a> {
    widgets::FooterContext {
        model: state.provider_model,
        effort: state.reasoning_effort,
        context_window: state.context_window,
        project: state.project,
        turn_label: turn_state_label(state.turn_state, state.running, state.session_loading),
        duration: state.turn_duration,
        usage: state.latest_usage,
        dangerous: state.dangerous_mode,
        bypass: state.bypass,
    }
}

/// Metadata spliced into the composer's border, right-aligned and held one column
/// off the closing corner, or `None` when the band cannot host it whole.
///
/// A single-row composer has no border row of its own to lend, so it declines
/// rather than paint the metadata over the input line.
fn border_metrics(state: &ViewState<'_>, composer: Rect) -> Option<Line<'static>> {
    if composer.height < 2 {
        return None;
    }
    let text = widgets::MetricFooter::border_text(
        border_metrics_budget(composer.width),
        footer_context(state),
    )?;
    Some(
        Line::from(vec![
            Span::styled(text, Style::default().fg(widgets::RolePalette::chrome())),
            Span::raw(" "),
        ])
        .right_aligned(),
    )
}

/// Content width below which the palette drops the description column entirely.
const PALETTE_DESCRIPTION_MIN_WIDTH: u16 = 40;

const PALETTE_SHORTCUTS: [widgets::OverlayShortcut<'static>; 3] = [
    widgets::OverlayShortcut {
        key: "↑↓",
        label: "navigate",
    },
    widgets::OverlayShortcut {
        key: "⏎",
        label: "run",
    },
    widgets::OverlayShortcut {
        key: "esc",
        label: "close",
    },
];

fn render_palette(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    composer: Rect,
    input: &str,
    palette: PaletteView<'_>,
) {
    let matches = palette_matches(palette.entries, input);
    let config = widgets::OverlayConfig {
        title: "commands",
        tabs: None,
        shortcuts: &PALETTE_SHORTCUTS,
        sizing: widgets::OverlaySizing::palette(composer),
        desired_content_rows: saturating_u16(matches.len().clamp(1, 8)),
    };
    let Some(layout) = widgets::OverlayLayout::solve(area, &config) else {
        return;
    };
    widgets::OverlayFrame::render(frame, &layout, &config);

    if matches.is_empty() {
        let empty = [widgets::OverlayRow {
            dimmed: true,
            ..widgets::OverlayRow::new("No matching commands")
        }];
        widgets::OverlayList::render(frame, layout.content, &empty, 0, empty.len());
        return;
    }

    let selected = palette.selected.min(matches.len() - 1);
    let describes = layout.content.width >= PALETTE_DESCRIPTION_MIN_WIDTH;
    let rows: Vec<widgets::OverlayRow<'_>> = matches
        .iter()
        .enumerate()
        .map(|(index, entry)| widgets::OverlayRow {
            right_label: describes
                .then_some(entry.description.as_str())
                .filter(|description| !description.is_empty())
                .map(Cow::Borrowed),
            selected: index == selected,
            ..widgets::OverlayRow::new(
                format!("/{} {}", entry.name, entry.argument_hint)
                    .trim_end()
                    .to_owned(),
            )
        })
        .collect();

    let visible = usize::from(layout.content.height);
    let offset = selected.saturating_sub(visible.saturating_sub(1));
    widgets::OverlayList::render(frame, layout.content, &rows, offset, rows.len());
}

const FILE_PICKER_SHORTCUTS: [widgets::OverlayShortcut<'static>; 3] = [
    widgets::OverlayShortcut {
        key: "↑↓",
        label: "navigate",
    },
    widgets::OverlayShortcut {
        key: "⏎",
        label: "insert",
    },
    widgets::OverlayShortcut {
        key: "esc",
        label: "close",
    },
];

fn render_file_picker(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    composer: Rect,
    picker: FilePickerView<'_>,
) {
    let matches = picker.matches();
    let config = widgets::OverlayConfig {
        title: "files",
        tabs: None,
        shortcuts: &FILE_PICKER_SHORTCUTS,
        sizing: widgets::OverlaySizing::palette(composer),
        desired_content_rows: saturating_u16(matches.len().clamp(1, 8)),
    };
    let Some(layout) = widgets::OverlayLayout::solve(area, &config) else {
        return;
    };
    widgets::OverlayFrame::render(frame, &layout, &config);

    if matches.is_empty() {
        let empty = [widgets::OverlayRow {
            dimmed: true,
            ..widgets::OverlayRow::new("No matching files")
        }];
        widgets::OverlayList::render(frame, layout.content, &empty, 0, empty.len());
        return;
    }

    let selected = picker.selected().min(matches.len() - 1);
    let describes = layout.content.width >= PALETTE_DESCRIPTION_MIN_WIDTH;
    let rows: Vec<widgets::OverlayRow<'_>> = matches
        .iter()
        .enumerate()
        .map(|(index, path)| {
            let (directory, name) = path.rsplit_once('/').unwrap_or(("", path));
            widgets::OverlayRow {
                right_label: describes
                    .then_some(directory)
                    .filter(|directory| !directory.is_empty())
                    .map(Cow::Borrowed),
                selected: index == selected,
                ..widgets::OverlayRow::new(name)
            }
        })
        .collect();

    let visible = usize::from(layout.content.height);
    let offset = selected.saturating_sub(visible.saturating_sub(1));
    widgets::OverlayList::render(frame, layout.content, &rows, offset, rows.len());
}

fn render_dialog(frame: &mut ratatui::Frame<'_>, area: Rect, dialog: &DialogView) {
    let labels = dialog_shortcut_labels(dialog);
    let shortcuts = dialog_shortcuts(&labels);
    let config = dialog_config(dialog, &shortcuts, area);
    let Some(layout) = widgets::OverlayLayout::solve(area, &config) else {
        return;
    };
    widgets::OverlayFrame::render(frame, &layout, &config);

    let sections = dialog_sections(layout.content, dialog);
    let muted = Style::default().fg(widgets::RolePalette::muted());
    if let Some(help) = sections.help {
        frame.render_widget(
            Paragraph::new(prose_text(
                dialog_help_lines(dialog, help.width),
                help.height,
                muted,
            )),
            help,
        );
    }
    if let Some(search) = sections.search {
        widgets::OverlayList::render_search(frame, search, &dialog.query);
    }
    render_dialog_rows(frame, sections.rows, dialog);
    if let Some(details) = sections.details {
        frame.render_widget(
            Paragraph::new(dialog_prose(
                dialog_selected_detail(dialog),
                details.height,
                muted,
            )),
            details,
        );
    }
}

fn dialog_prose(text: Option<&str>, rows: u16, style: Style) -> Text<'static> {
    prose_text(
        text.map(|text| text.lines().map(ToOwned::to_owned).collect())
            .unwrap_or_default(),
        rows,
        style,
    )
}

fn prose_text(lines: Vec<String>, rows: u16, style: Style) -> Text<'static> {
    Text::from(
        lines
            .into_iter()
            .take(usize::from(rows))
            .map(|line| Line::styled(line, style))
            .collect::<Vec<_>>(),
    )
}

/// Paints the entry rows, preceded by the loading, error or empty-state line.
fn render_dialog_rows(frame: &mut ratatui::Frame<'_>, area: Rect, dialog: &DialogView) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let mut area = area;
    if let Some(status) = dialog_status_line(dialog) {
        frame.render_widget(
            Paragraph::new(status),
            Rect::new(area.x, area.y, area.width, 1),
        );
        area = Rect::new(area.x, area.y + 1, area.width, area.height - 1);
        if area.height == 0 {
            return;
        }
    }

    let matches = dialog_matches(dialog);
    let rows = matches
        .iter()
        .map(|(index, entry)| dialog_row(entry, dialog.interactive && *index == dialog.selected))
        .collect::<Vec<_>>();
    if rows.is_empty() {
        return;
    }

    let offset = dialog_row_offset(dialog, &matches, usize::from(area.height));
    widgets::OverlayList::render(frame, area, &rows, offset, rows.len());
}

fn dialog_row<'a>(entry: &'a DialogEntry, selected: bool) -> widgets::OverlayRow<'a> {
    let disabled = entry.action.is_none();
    widgets::OverlayRow {
        right_label: entry.detail.as_deref().map(Cow::Borrowed),
        badge: disabled.then_some("disabled"),
        selected,
        dimmed: disabled,
        ..widgets::OverlayRow::new(entry.label.as_str())
    }
}

/// Clamps the stored scroll offset against the rows this frame can really show,
/// so a resize can never leave the selected row painted off-screen.
fn dialog_row_offset(
    dialog: &DialogView,
    matches: &[(usize, &DialogEntry)],
    visible: usize,
) -> usize {
    let offset = dialog.offset.min(matches.len().saturating_sub(visible));
    let Some(selected) = matches
        .iter()
        .position(|(index, _)| *index == dialog.selected)
    else {
        return offset;
    };
    if selected < offset {
        selected
    } else if selected >= offset.saturating_add(visible) {
        selected.saturating_add(1).saturating_sub(visible)
    } else {
        offset
    }
}

/// The single non-entry line: loading, error, or the empty-state message.
fn dialog_status_line(dialog: &DialogView) -> Option<Line<'static>> {
    let sessions = dialog.session_entries.as_ref();
    if sessions.is_some_and(|entries| entries.loading) {
        return Some(Line::styled(
            "Loading sessions…",
            Style::default().fg(widgets::RolePalette::muted()),
        ));
    }
    if let Some(error) = sessions.and_then(|entries| entries.error.as_deref()) {
        return Some(Line::styled(
            error.to_owned(),
            Style::default().fg(widgets::RolePalette::warning()),
        ));
    }

    let empty = dialog_matches(dialog).is_empty()
        && (sessions.is_some()
            || dialog.empty_message.is_some()
            || !dialog_shows_help_body(dialog));
    empty.then(|| {
        Line::styled(
            dialog_empty_message(dialog).to_owned(),
            Style::default().fg(widgets::RolePalette::muted()),
        )
    })
}

/// Detail lines the expanded selection may claim.
const MAX_DIALOG_DETAIL_ROWS: usize = 3;
/// Rows the wrapped help prose may claim before the dialog stops growing for it.
const MAX_DIALOG_HELP_ROWS: usize = 12;
/// Confirm answers offered as footer shortcuts, in prompt order.
const CONFIRM_SHORT_KEYS: [char; 4] = ['a', 'd', 'A', 'D'];
/// Arms the dialog search band; every other character is a keybinding.
const DIALOG_SEARCH_KEY: char = '/';

/// Body rows the dialog would like, before the shell subtracts its chrome.
fn dialog_desired_rows(dialog: &DialogView, width: u16) -> u16 {
    saturating_u16(
        usize::from(dialog_help_rows(dialog, width))
            .saturating_add(usize::from(dialog.searching))
            .saturating_add(dialog_matches(dialog).len().max(1))
            .saturating_add(usize::from(dialog_detail_rows(dialog))),
    )
}

/// Whether `help` still belongs in the body.
///
/// Only the session browser hands that role entirely to the derived footer: its
/// help is a keybinding list the footer reproduces. Every other caller passes
/// contextual prose (the tool being confirmed, the model source, a warning),
/// which has no other place to go, so it stays as a body row that yields to the
/// search and entry rows under pressure.
fn dialog_shows_help_body(dialog: &DialogView) -> bool {
    dialog.help.is_some() && dialog.session_entries.is_none()
}

/// The rows the help prose needs at `width`.
///
/// Help arriving through a constructor is stripped of newlines and claims one row, but
/// [`Tui::add_diagnostic`] appends further lines directly and a single diagnostic can outrun the
/// frame. Counting the lines it will actually be painted as is what keeps every diagnostic visible
/// instead of clipping the band to its first row.
fn dialog_help_rows(dialog: &DialogView, width: u16) -> u16 {
    if !dialog_shows_help_body(dialog) {
        return 0;
    }

    saturating_u16(
        dialog_help_lines(dialog, width)
            .len()
            .clamp(1, MAX_DIALOG_HELP_ROWS),
    )
}

fn dialog_help_lines(dialog: &DialogView, width: u16) -> Vec<String> {
    let Some(help) = dialog.help.as_deref() else {
        return Vec::new();
    };

    if dialog_help_is_body(dialog) {
        return wrapped_prose_lines(help, width);
    }

    help.lines().map(ToOwned::to_owned).collect()
}

/// Whether the help prose is the dialog's whole body rather than a caption above entry rows.
///
/// Only then may it claim more than one row. A selection dialog needs its rows for the entries, so
/// its caption keeps yielding to them exactly as before.
fn dialog_help_is_body(dialog: &DialogView) -> bool {
    !dialog.interactive && dialog.entries.is_empty()
}

/// Greedy word wrap that keeps the caller's own line breaks.
///
/// The row count and the painted lines both come from here, so the band can never be sized for a
/// different result than the one rendered. A word wider than the band is split rather than left to
/// overflow it.
fn wrapped_prose_lines(text: &str, width: u16) -> Vec<String> {
    let width = usize::from(width);
    if width == 0 {
        return Vec::new();
    }

    let mut lines = Vec::new();
    for paragraph in text.lines() {
        let mut current = String::new();

        for word in paragraph.split_whitespace() {
            if current.is_empty() {
                current.push_str(word);
            } else if current.chars().count() + 1 + word.chars().count() <= width {
                current.push(' ');
                current.push_str(word);
            } else {
                lines.push(std::mem::take(&mut current));
                current.push_str(word);
            }

            while current.chars().count() > width {
                let head = current.chars().take(width).collect::<String>();
                current = current.chars().skip(width).collect();
                lines.push(head);
            }
        }

        lines.push(current);
    }

    lines
}

fn dialog_selected_detail(dialog: &DialogView) -> Option<&str> {
    dialog
        .entries
        .get(dialog.selected)
        .and_then(|entry| entry.selected_detail.as_deref())
}

fn dialog_detail_rows(dialog: &DialogView) -> u16 {
    if !dialog.details_open {
        return 0;
    }
    dialog_selected_detail(dialog).map_or(0, |detail| {
        saturating_u16(detail.lines().count().min(MAX_DIALOG_DETAIL_ROWS))
    })
}

/// Vertical split of the shell content rect, in painting order.
struct DialogSections {
    help: Option<Rect>,
    search: Option<Rect>,
    rows: Rect,
    details: Option<Rect>,
}

/// Splits the content rect the shell produced, keeping at least one entry row.
fn dialog_sections(content: Rect, dialog: &DialogView) -> DialogSections {
    let mut remaining = content.height;
    let search = u16::from(dialog.searching && remaining > 1);
    remaining -= search;
    let details = dialog_detail_rows(dialog).min(remaining.saturating_sub(1));
    remaining -= details;
    let help = dialog_help_rows(dialog, content.width).min(remaining.saturating_sub(1));
    remaining -= help;

    let band = |y: u16, height: u16| Rect::new(content.x, y, content.width, height);
    let search_y = content.y.saturating_add(help);
    let rows_y = search_y.saturating_add(search);
    let details_y = rows_y.saturating_add(remaining);

    DialogSections {
        help: (help > 0).then(|| band(content.y, help)),
        search: (search > 0).then(|| band(search_y, search)),
        rows: band(rows_y, remaining),
        details: (details > 0).then(|| band(details_y, details)),
    }
}

type DialogShortcutLabels = Vec<(Cow<'static, str>, Cow<'static, str>)>;

/// Footer shortcuts derived from dialog capabilities, never parsed from `help`.
fn dialog_shortcut_labels(dialog: &DialogView) -> DialogShortcutLabels {
    if dialog.overlay_kind == widgets::OverlayKind::Confirm {
        let mut labels: DialogShortcutLabels = CONFIRM_SHORT_KEYS
            .iter()
            .filter_map(|key| {
                let answer = widgets::OverlayShell::confirm_answer(*key)?;
                Some((
                    Cow::Owned(key.to_string()),
                    Cow::Owned(answer.replace('-', " ")),
                ))
            })
            .collect();
        labels.push((Cow::Borrowed("esc"), Cow::Borrowed("cancel")));
        return labels;
    }

    let mut labels = DialogShortcutLabels::new();
    if dialog.interactive {
        labels.push((
            Cow::Borrowed(if dialog.searching {
                "↑↓"
            } else {
                "↑↓ jk"
            }),
            Cow::Borrowed("navigate"),
        ));
    }
    if let Some(sessions) = dialog.session_entries.as_ref() {
        let paging = if sessions.next_cursor.is_some() {
            "more"
        } else {
            "end"
        };
        labels.push((
            Cow::Borrowed("⇞⇟"),
            Cow::Owned(format!("page {} · {paging}", sessions.request.page)),
        ));
        labels.push((
            Cow::Borrowed("ctrl+a"),
            Cow::Borrowed(match sessions.request.scope {
                SessionDialogScope::CurrentProject => "all projects",
                SessionDialogScope::AllProjects => "current project",
            }),
        ));
        labels.push((Cow::Borrowed("⏎"), Cow::Borrowed("resume")));
    } else if dialog.interactive {
        labels.push((Cow::Borrowed("⏎"), Cow::Borrowed("select")));
    }
    // While search is armed Escape only disarms it, and every letter key is
    // filter text, so advertising the letter bindings there would be a lie.
    if dialog.searching {
        labels.push((Cow::Borrowed("esc"), Cow::Borrowed("exit search")));
        return labels;
    }

    if dialog.refresh_id.is_some() {
        labels.push((Cow::Borrowed("r"), Cow::Borrowed("refresh")));
    }
    if dialog.interactive {
        labels.push((Cow::Borrowed("/"), Cow::Borrowed("search")));
    }
    labels.push((Cow::Borrowed("esc"), Cow::Borrowed("close")));
    labels
}

fn dialog_shortcuts(labels: &DialogShortcutLabels) -> Vec<widgets::OverlayShortcut<'_>> {
    labels
        .iter()
        .map(|(key, label)| widgets::OverlayShortcut { key, label })
        .collect()
}

fn dialog_config<'a>(
    dialog: &'a DialogView,
    shortcuts: &'a [widgets::OverlayShortcut<'a>],
    area: Rect,
) -> widgets::OverlayConfig<'a> {
    let sizing = if dialog.overlay_kind == widgets::OverlayKind::Confirm {
        widgets::OverlaySizing::compact()
    } else {
        widgets::OverlaySizing::dialog()
    };
    let content_width = sizing.inner_width(area).unwrap_or_default();

    widgets::OverlayConfig {
        title: &dialog.title,
        tabs: None,
        shortcuts,
        desired_content_rows: dialog_desired_rows(dialog, content_width),
        sizing,
    }
}

/// Entry rows the dialog can show in `area`, shared by the renderer and the
/// paging keys so navigation never disagrees with what is painted.
fn dialog_visible_rows(area: Rect, dialog: &DialogView) -> usize {
    let labels = dialog_shortcut_labels(dialog);
    let shortcuts = dialog_shortcuts(&labels);
    let config = dialog_config(dialog, &shortcuts, area);
    widgets::OverlayLayout::solve(area, &config).map_or(1, |layout| {
        usize::from(dialog_sections(layout.content, dialog).rows.height).max(1)
    })
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
    } else if session_entries.request.scope == SessionDialogScope::AllProjects {
        "No resumable sessions in any project."
    } else {
        "No resumable sessions in current project."
    }
}

struct ScreenLayout {
    transcript: Rect,
    composer: Rect,
    notice: Rect,
    tree: Rect,
    footer: Rect,
}

fn conversation_surface(area: Rect) -> Rect {
    // Full terminal width — do not center a narrow column.
    area
}

/// Columns kept free on both sides of every bottom band.
///
/// The bottom chrome shares the gutter the transcript already indents its
/// content by, so the composer box, the notice, the subagent tree and the status
/// bar all start on one column instead of three different ones.
const CHROME_GUTTER: u16 = TRANSCRIPT_CONTENT_INDENT;
/// Narrowest composer box that still earns a gutter. Below it the gutter is
/// spent on input room instead of symmetry.
const MIN_GUTTERED_COMPOSER_WIDTH: u16 = 24;

/// Gutter a terminal of this width can afford, shrinking one column at a time so
/// a narrow terminal degrades to a flush composer instead of a starved one.
fn chrome_gutter(width: u16) -> u16 {
    CHROME_GUTTER.min(width.saturating_sub(MIN_GUTTERED_COMPOSER_WIDTH) / 2)
}

fn composer_width(width: u16) -> u16 {
    width.saturating_sub(2 * chrome_gutter(width))
}

/// Border columns the metadata cannot use: both corners plus the gap that keeps
/// the text from touching the closing one.
const BORDER_METRICS_CHROME: u16 = 3;

/// Columns the composer's bottom border can lend the metric footer.
fn border_metrics_budget(composer_width: u16) -> u16 {
    composer_width.saturating_sub(BORDER_METRICS_CHROME)
}

/// Whether the metadata rides the composer border instead of owning a row.
///
/// Decided from the terminal width alone: a longer model name or a growing token
/// count must never claim a row and push the composer up mid-session.
fn metrics_ride_the_border(width: u16) -> bool {
    border_metrics_budget(composer_width(width)) >= widgets::MIN_BORDER_METRICS_WIDTH
}

/// Minimum terminal height before the subagent tree may claim rows.
const TREE_MIN_SCREEN_HEIGHT: u16 = 14;
/// Executions shown as tree branches, matching the navigable transcript set.
const MAX_TREE_EXECUTIONS: usize = 3;
/// Child activity rows shown under the focused branch.
const MAX_TREE_ACTIVITIES: usize = 2;
/// Widest subagent tree the renderer can produce: the root, every branch, the
/// activities of the focused branch and the affordance row.
const MAX_TREE_ROWS: u16 = 1 + MAX_TREE_EXECUTIONS as u16 + MAX_TREE_ACTIVITIES as u16 + 1;
/// Narrowest tree worth reserving: one branch plus the affordance row that
/// absorbs everything elided.
const MIN_TREE_ROWS: u16 = 2;
/// Screen rows required per reserved tree row, so a 24-row terminal reserves
/// `MIN_TREE_ROWS` and only taller screens pay for deeper trees.
const SCREEN_ROWS_PER_TREE_ROW: u16 = 10;

/// Rows reserved below the composer for the notice, the subagent tree and the
/// metric footer fallback row.
#[derive(Clone, Copy)]
struct BottomChrome {
    notice: u16,
    tree: u16,
    footer: u16,
}

/// Where the reserved bottom rows actually land on screen.
struct ChromeBands {
    notice: Rect,
    tree: Rect,
    footer: Rect,
}

impl BottomChrome {
    fn rows(self) -> u16 {
        self.notice
            .saturating_add(self.tree)
            .saturating_add(self.footer)
    }

    /// Sheds the tree before the notice and the status bar so the composer
    /// keeps priority when it leaves fewer rows than the height budget assumes.
    fn fitted(self, rows: u16) -> Self {
        let footer = self.footer.min(rows);
        let notice = self.notice.min(rows.saturating_sub(footer));
        let tree = self
            .tree
            .min(rows.saturating_sub(footer).saturating_sub(notice));
        Self {
            notice,
            tree,
            footer,
        }
    }

    /// Places the reserved rows inside their region: the notice and the tree hug
    /// the composer and the status bar keeps the last row, so the rows a missing
    /// notice does not claim become a gap above the status bar instead of dead
    /// air under the composer. The region keeps its full reserved height either
    /// way, which is what keeps the composer a function of terminal height only.
    fn placed(self, region: Rect, notice_shown: bool) -> ChromeBands {
        let notice = if notice_shown { self.notice } else { 0 };
        let band = |y: u16, height: u16| Rect {
            x: region.x,
            y,
            width: region.width,
            height,
        };
        ChromeBands {
            notice: band(region.y, notice),
            tree: band(region.y.saturating_add(notice), self.tree),
            footer: band(region.bottom().saturating_sub(self.footer), self.footer),
        }
    }
}

/// Sizes the bottom chrome region from the terminal size alone.
///
/// The budget deliberately ignores what the notice and the tree currently hold
/// so the composer never moves while chrome content appears and disappears;
/// content taller than its budget is elided instead. The tree reserves only a
/// couple of rows on a common terminal so an idle screen shows no wide gap, and
/// the composer keeps priority below the heights where each band becomes
/// affordable. The footer row exists only for terminals too narrow to splice the
/// metadata into the composer's bottom border.
fn bottom_chrome(width: u16, height: u16) -> BottomChrome {
    let notice = u16::from(height >= 7);
    let footer = u16::from(height >= 12 && !metrics_ride_the_border(width));
    let affordable = (height / 3).saturating_sub(notice).saturating_sub(footer);
    let tree = if height >= TREE_MIN_SCREEN_HEIGHT {
        affordable.min((height / SCREEN_ROWS_PER_TREE_ROW).clamp(MIN_TREE_ROWS, MAX_TREE_ROWS))
    } else {
        0
    };
    BottomChrome {
        notice,
        tree,
        footer,
    }
}

fn composer_rows(height: u16, input: &str) -> u16 {
    match height {
        0 => 0,
        1 => 1,
        2..=6 => 2,
        7..=11 => 3,
        _ => {
            let input_lines = input.chars().filter(|character| *character == '\n').count() + 1;
            saturating_u16(input_lines.saturating_add(2)).clamp(3, 8)
        }
    }
}

fn screen_layout(area: Rect, input: &str, notice_shown: bool) -> ScreenLayout {
    let area = conversation_surface(area);
    let composer_rows = composer_rows(area.height, input).min(area.height);
    let chrome =
        bottom_chrome(area.width, area.height).fitted(area.height.saturating_sub(composer_rows));
    let transcript_rows = area
        .height
        .saturating_sub(composer_rows)
        .saturating_sub(chrome.rows());
    let chunks = Layout::vertical([
        Constraint::Length(transcript_rows),
        Constraint::Length(composer_rows),
        Constraint::Length(chrome.rows()),
    ])
    .split(area);

    let gutter = Margin::new(chrome_gutter(area.width), 0);
    let bands = chrome.placed(chunks[2].inner(gutter), notice_shown);

    ScreenLayout {
        transcript: chunks[0],
        composer: chunks[1].inner(gutter),
        notice: bands.notice,
        tree: bands.tree,
        footer: bands.footer,
    }
}

/// Builds the notice row content, empty when nothing needs announcing.
fn notice_spans(state: &ViewState<'_>) -> Vec<Span<'static>> {
    let mut left = Vec::new();
    if state.quit_armed {
        left.push(Span::styled(
            " Press Ctrl+C again to exit ",
            Style::default()
                .fg(widgets::RolePalette::warning())
                .add_modifier(Modifier::BOLD),
        ));
    } else if state.recovered_failed_prompt {
        left.push(Span::styled(
            " Recovered failed prompt · Enter retry · Esc discard ",
            Style::default()
                .fg(widgets::RolePalette::warning())
                .add_modifier(Modifier::BOLD),
        ));
    } else if state.dangerous_mode {
        left.push(Span::styled(
            " danger ",
            Style::default()
                .fg(widgets::RolePalette::warning())
                .add_modifier(Modifier::BOLD),
        ));
    }
    if !state.recovered_failed_prompt
        && let Some(status) = state.status.filter(|value| !value.is_empty())
    {
        // Transient context messages (e.g. resume notices) live here — not model/project.
        left.push(Span::styled(
            format!(" {status} "),
            Style::default().fg(widgets::RolePalette::muted()),
        ));
    }
    left
}

fn render_notice(frame: &mut ratatui::Frame<'_>, area: Rect, spans: Vec<Span<'static>>) {
    frame.render_widget(
        Paragraph::new(Line::from(spans)).wrap(Wrap { trim: false }),
        area,
    );
}

/// Navigable subagent tree rendered between the composer and the status bar.
///
/// The tree is the single navigation surface for delegated work: `Main` is the
/// root, each running or recently finished execution is a branch, and the
/// focused branch expands into its live tool activity. Its keybinding hints
/// live here rather than on the in-transcript card.
///
/// The reserved region is sized from the terminal height alone, so a taller
/// tree elides from its least informative rows: child activities first, then
/// the root, then the branches the caller's ordering ranks last. `ViewState`
/// already ranks running executions before finished ones, so elision keeps live
/// work visible. The affordance row always closes the tree and reports how many
/// branches it hides.
///
/// The tree is painted without wrapping, so every label is bounded to the
/// columns left of it by its own rail and glyph.
fn fitted_subagent_tree_lines(state: &ViewState<'_>, rows: u16, width: u16) -> Vec<Line<'static>> {
    if state.executions.is_empty() || rows == 0 {
        return Vec::new();
    }
    let width = usize::from(width);

    let body = usize::from(rows).saturating_sub(1);
    let branches = state
        .executions
        .iter()
        .copied()
        .take(body.min(MAX_TREE_EXECUTIONS))
        .collect::<Vec<_>>();
    let spare = body.saturating_sub(branches.len());

    let mut lines = Vec::new();
    if spare > 0 {
        lines.push(Line::from(Span::styled(
            render::bounded_single_line("Main", width),
            tree_row_style(state, TranscriptId::Main),
        )));
    }
    let backgroundable = branches
        .iter()
        .any(|execution| execution.state == TuiExecutionState::ForegroundRunning);
    lines.extend(tree_branch_lines(
        state,
        &branches,
        spare.saturating_sub(1),
        width,
    ));
    lines.push(tree_affordance_line(
        state.executions.len().saturating_sub(branches.len()),
        backgroundable,
        width,
    ));
    lines
}

fn tree_branch_lines(
    state: &ViewState<'_>,
    branches: &[&TuiExecution],
    activity_rows: usize,
    width: usize,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut activity_budget = activity_rows.min(MAX_TREE_ACTIVITIES);

    for (index, execution) in branches.iter().enumerate() {
        let last = index + 1 == branches.len();
        let rail = if last { "└─ " } else { "├─ " };
        let glyph = format!("{} ", execution_state_glyph(execution.state));
        lines.push(Line::from(vec![
            Span::styled(
                rail.to_owned(),
                Style::default().fg(widgets::RolePalette::chrome()),
            ),
            Span::styled(
                glyph.clone(),
                Style::default().fg(execution_state_color(execution.state)),
            ),
            Span::styled(
                render::bounded_single_line(
                    &tree_execution_label(execution, state.now),
                    width
                        .saturating_sub(rail.width())
                        .saturating_sub(glyph.width()),
                ),
                tree_row_style(state, TranscriptId::Subagent(execution.id)),
            ),
        ]));

        let children = state
            .execution_activities
            .iter()
            .filter(|activity| activity.parent == execution.id)
            .take(activity_budget)
            .collect::<Vec<_>>();
        activity_budget = activity_budget.saturating_sub(children.len());

        for (child_index, activity) in children.iter().enumerate() {
            let rail = format!(
                "{}{}",
                if last { "   " } else { "│  " },
                if child_index + 1 == children.len() {
                    "└─ "
                } else {
                    "├─ "
                }
            );
            let glyph = format!("{} ", if activity.running { "●" } else { "✓" });
            let label_width = width
                .saturating_sub(rail.width())
                .saturating_sub(glyph.width());
            lines.push(Line::from(vec![
                Span::styled(rail, Style::default().fg(widgets::RolePalette::chrome())),
                Span::styled(
                    glyph,
                    Style::default().fg(if activity.running {
                        widgets::RolePalette::accent_active()
                    } else {
                        widgets::RolePalette::success()
                    }),
                ),
                Span::styled(
                    render::bounded_single_line(&activity.label, label_width),
                    Style::default().fg(widgets::RolePalette::muted()),
                ),
            ]));
        }
    }
    lines
}

/// Closes the tree with its navigation affordance, folding the branches that
/// did not fit into the same row so the hidden count stays discoverable.
///
/// Backgrounding only applies to a branch still running in the foreground, so
/// the hint is dropped when no shown branch can accept it.
fn tree_affordance_line(
    hidden_branches: usize,
    backgroundable: bool,
    width: usize,
) -> Line<'static> {
    let text = if hidden_branches > 0 {
        format!("+{hidden_branches} more · Tab to focus")
    } else if backgroundable {
        "Tab focus · Enter inspect · Ctrl+B background".to_owned()
    } else {
        "Tab focus · Enter inspect".to_owned()
    };
    Line::from(Span::styled(
        render::bounded_single_line(&text, width),
        Style::default().fg(widgets::RolePalette::chrome()),
    ))
}

fn tree_row_style(state: &ViewState<'_>, id: TranscriptId) -> Style {
    let selected = state.execution_selection == Some(id);
    let active = state.active_transcript == id;
    let color = if selected {
        widgets::RolePalette::accent_active()
    } else if active {
        widgets::RolePalette::assistant()
    } else {
        widgets::RolePalette::muted()
    };
    let style = Style::default().fg(color);
    if selected || active {
        style.add_modifier(Modifier::BOLD)
    } else {
        style
    }
}

fn tree_execution_label(execution: &TuiExecution, now: Duration) -> String {
    let elapsed = execution
        .terminal_at
        .unwrap_or(now)
        .saturating_sub(execution.started_at);
    format!(
        "{} #{} · {} · {}s",
        display_agent_name(&execution.agent),
        execution.id,
        execution_state_label(execution.state),
        elapsed.as_secs()
    )
}

const fn execution_state_label(state: TuiExecutionState) -> &'static str {
    match state {
        TuiExecutionState::ForegroundRunning => "running",
        TuiExecutionState::BackgroundRunning => "background",
        TuiExecutionState::CompletedRecent => "done",
        TuiExecutionState::Failed => "failed",
        TuiExecutionState::Cancelled => "cancelled",
    }
}

fn execution_state_color(state: TuiExecutionState) -> Color {
    match state {
        TuiExecutionState::ForegroundRunning | TuiExecutionState::BackgroundRunning => {
            widgets::RolePalette::accent_active()
        }
        TuiExecutionState::CompletedRecent => widgets::RolePalette::success(),
        TuiExecutionState::Failed => widgets::RolePalette::error(),
        TuiExecutionState::Cancelled => widgets::RolePalette::warning(),
    }
}

fn turn_state_label(
    state: Option<TurnState>,
    running: bool,
    session_loading: bool,
) -> &'static str {
    if session_loading {
        return "Loading session…";
    }

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

fn transcript_lines(entries: &[TranscriptEntry]) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for entry in entries {
        let (label, color, text, card) = match entry {
            TranscriptEntry::User(text) => ("USER", widgets::RolePalette::muted(), text, false),
            TranscriptEntry::Assistant(text) => {
                ("ASSISTANT", widgets::RolePalette::muted(), text, false)
            }
            TranscriptEntry::Reasoning(text) => {
                ("THINKING", widgets::RolePalette::muted(), text, false)
            }
            TranscriptEntry::Error(text) => ("ERROR", widgets::RolePalette::error(), text, true),
            TranscriptEntry::Info(text) => ("INFO", widgets::RolePalette::muted(), text, false),
            TranscriptEntry::Tool(text) => ("TOOL", widgets::RolePalette::muted(), text, true),
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
                    Style::default().fg(widgets::RolePalette::muted()),
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

/// Every transcript row, each already carrying the accent column it owns.
///
/// `row_width` counts that column: chrome rows that no conversation block
/// describes are padded through [`render::unaccented_row`] so their content
/// keeps the same screen column as block rows.
fn rendered_transcript(state: &ViewState<'_>, row_width: u16) -> Vec<Line<'static>> {
    let mut transcript = chrome_rows(transcript_provenance(state));
    let thinking_streaming = state.running;
    transcript.extend(
        state
            .completed_conversations
            .iter()
            .flat_map(|conversation| {
                render::conversation_lines(
                    conversation,
                    &[],
                    state.tool_display_modes,
                    row_width,
                    render::ConversationRenderState {
                        collapse_thinking: state.collapse_thinking,
                        thinking_streaming: false,
                        assistant_streaming: !state.highlight_restored_syntax,
                        now: state.now,
                    },
                )
            })
            .collect::<Vec<_>>(),
    );
    if let Some(conversation) = state.conversation {
        transcript.extend(render::conversation_lines(
            conversation,
            state.runtime_events,
            state.tool_display_modes,
            row_width,
            render::ConversationRenderState {
                collapse_thinking: state.collapse_thinking,
                thinking_streaming,
                assistant_streaming: state.assistant_streaming,
                now: state.now,
            },
        ));
    }
    let conversation_is_authoritative =
        !state.completed_conversations.is_empty() || state.conversation.is_some();
    if !conversation_is_authoritative {
        transcript = chrome_rows(transcript_lines(state.transcript));
    }
    transcript.extend(chrome_rows(render::detail_lines(
        state.runtime_events,
        conversation_is_authoritative,
    )));
    if state.running {
        transcript.push(render::unaccented_row(render::turn_status_line(
            render::TurnStatus {
                label: "Working…",
                now: state.now,
                elapsed: state
                    .turn_started_at
                    .map(|started| state.now.saturating_sub(started)),
                tokens: state
                    .latest_usage
                    .and_then(|usage| usage.total_tokens.or(usage.output_tokens)),
            },
            usize::from(row_width.max(1)).saturating_sub(widgets::ACCENT_WIDTH),
        )));
    }
    transcript
}

/// Reserves the accent column on transcript rows that no conversation block owns.
fn chrome_rows(lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    lines.into_iter().map(render::unaccented_row).collect()
}

#[derive(Clone, Debug)]
struct SelectableCell {
    text: String,
    column: u16,
    width: u16,
    style: Style,
}

#[derive(Clone, Debug, Default)]
struct SelectableRow {
    cells: Vec<SelectableCell>,
    hard_break_after: bool,
}

#[derive(Clone, Debug, Default)]
struct SelectableTranscript {
    rows: Vec<SelectableRow>,
}

impl SelectableTranscript {
    fn from_lines(lines: &[Line<'_>], width: u16) -> Self {
        let width = width.max(1);
        let mut rows = Vec::new();

        for line in lines {
            let cells = line
                .styled_graphemes(Style::default())
                .map(|grapheme| SelectableCell {
                    text: grapheme.symbol.to_owned(),
                    column: 0,
                    width: saturating_u16(grapheme.symbol.width()),
                    style: grapheme.style,
                })
                .collect();
            let wrapped = wrap_selectable_line(cells, width);
            let last = wrapped.len().saturating_sub(1);
            rows.extend(
                wrapped
                    .into_iter()
                    .enumerate()
                    .map(|(index, cells)| selectable_row(cells, index == last)),
            );
        }

        Self { rows }
    }

    fn position_at(&self, row: usize, column: u16) -> Option<TranscriptPosition> {
        let cells = &self.rows.get(row)?.cells;
        let cell = cells
            .iter()
            .find(|cell| column < cell.column.saturating_add(cell.width))
            .or_else(|| cells.last())?;
        Some(TranscriptPosition {
            row,
            column: cell.column,
        })
    }

    fn selected_text(&self, selection: TranscriptSelection) -> Result<String, ()> {
        let (start, end) = ordered_selection(selection);
        if start == end {
            return Ok(String::new());
        }

        let mut text = String::new();
        for row_index in start.row..=end.row {
            let Some(row) = self.rows.get(row_index) else {
                break;
            };
            for cell in &row.cells {
                let position = TranscriptPosition {
                    row: row_index,
                    column: cell.column,
                };
                if position < start || position > end {
                    continue;
                }
                append_bounded_selection(&mut text, &cell.text)?;
            }
            if row_index < end.row && row.hard_break_after {
                append_bounded_selection(&mut text, "\n")?;
            }
        }
        Ok(text)
    }

    fn render_lines(&self, selection: Option<TranscriptSelection>) -> Vec<Line<'static>> {
        let selection = selection.map(ordered_selection);
        self.rows
            .iter()
            .enumerate()
            .map(|(row, line)| {
                let mut spans = Vec::new();
                let mut current_text = String::new();
                let mut current_style = None;
                for cell in &line.cells {
                    let position = TranscriptPosition {
                        row,
                        column: cell.column,
                    };
                    let selected =
                        selection.is_some_and(|(start, end)| position >= start && position <= end);
                    let style = if selected {
                        cell.style.patch(
                            Style::default()
                                .fg(Color::Black)
                                .bg(widgets::RolePalette::brand()),
                        )
                    } else {
                        cell.style
                    };
                    if let Some(current) = current_style
                        && current != style
                    {
                        spans.push(Span::styled(std::mem::take(&mut current_text), current));
                    }
                    current_style = Some(style);
                    current_text.push_str(&cell.text);
                }
                if let Some(style) = current_style {
                    spans.push(Span::styled(current_text, style));
                }
                Line::from(spans)
            })
            .collect()
    }
}

fn wrap_selectable_line(cells: Vec<SelectableCell>, width: u16) -> Vec<Vec<SelectableCell>> {
    let mut rows = Vec::new();
    let mut line = Vec::new();
    let mut line_width = 0_u16;
    let mut word = Vec::new();
    let mut word_width = 0_u16;
    let mut whitespace: VecDeque<SelectableCell> = VecDeque::new();
    let mut whitespace_width = 0_u16;
    let mut previous_was_text = false;

    for cell in cells {
        if cell.width > width {
            continue;
        }
        let is_whitespace = selectable_cell_is_whitespace(&cell);
        let word_finished = previous_was_text && is_whitespace;
        let segment_overflow = line.is_empty()
            && word_width
                .saturating_add(whitespace_width)
                .saturating_add(cell.width)
                > width;
        if word_finished || segment_overflow {
            line.extend(whitespace.drain(..));
            line_width = line_width.saturating_add(whitespace_width);
            line.append(&mut word);
            line_width = line_width.saturating_add(word_width);
            whitespace_width = 0;
            word_width = 0;
        }

        let line_full = line_width >= width;
        let word_overflow = cell.width > 0
            && line_width
                .saturating_add(whitespace_width)
                .saturating_add(word_width)
                >= width;
        if line_full || word_overflow {
            let mut remaining = width.saturating_sub(line_width);
            rows.push(std::mem::take(&mut line));
            line_width = 0;
            while let Some(pending) = whitespace.front() {
                if pending.width > remaining {
                    break;
                }
                whitespace_width = whitespace_width.saturating_sub(pending.width);
                remaining = remaining.saturating_sub(pending.width);
                whitespace.pop_front();
            }
            if is_whitespace && whitespace.is_empty() {
                previous_was_text = false;
                continue;
            }
        }

        if is_whitespace {
            whitespace_width = whitespace_width.saturating_add(cell.width);
            whitespace.push_back(cell);
        } else {
            word_width = word_width.saturating_add(cell.width);
            word.push(cell);
        }
        previous_was_text = !is_whitespace;
    }

    line.extend(whitespace);
    line.append(&mut word);
    if !line.is_empty() {
        rows.push(line);
    }
    if rows.is_empty() {
        rows.push(Vec::new());
    }
    rows
}

fn selectable_cell_is_whitespace(cell: &SelectableCell) -> bool {
    cell.text == "\u{200b}" || cell.text != "\u{00a0}" && cell.text.chars().all(char::is_whitespace)
}

fn selectable_row(mut cells: Vec<SelectableCell>, hard_break_after: bool) -> SelectableRow {
    let mut column = 0;
    for cell in &mut cells {
        cell.column = column;
        column = column.saturating_add(cell.width);
    }
    SelectableRow {
        cells,
        hard_break_after,
    }
}

fn ordered_selection(selection: TranscriptSelection) -> (TranscriptPosition, TranscriptPosition) {
    if selection.anchor <= selection.head {
        (selection.anchor, selection.head)
    } else {
        (selection.head, selection.anchor)
    }
}

struct MouseSelectionSnapshot {
    transcript: SelectableTranscript,
    content_x: u16,
    content_y: u16,
    content_right: u16,
    content_bottom: u16,
    scroll: u16,
}

impl MouseSelectionSnapshot {
    fn position(&self, column: u16, row: u16) -> Option<TranscriptPosition> {
        if row < self.content_y
            || row >= self.content_bottom
            || column < self.content_x
            || column >= self.content_right
        {
            return None;
        }
        let absolute_row = usize::from(
            self.scroll
                .saturating_add(row.saturating_sub(self.content_y)),
        );
        self.transcript
            .position_at(absolute_row, column.saturating_sub(self.content_x))
    }
}

fn append_bounded_selection(output: &mut String, value: &str) -> Result<(), ()> {
    if output.len().saturating_add(value.len()) > MAX_SELECTION_COPY_BYTES {
        return Err(());
    }
    output.push_str(value);
    Ok(())
}

fn transcript_provenance(state: &ViewState<'_>) -> Vec<Line<'static>> {
    match state.active_transcript {
        TranscriptId::Main => {
            // Quiet main provenance: no debug "primary conversation" chrome.
            Vec::new()
        }
        TranscriptId::Subagent(id) => vec![
            Line::styled(
                format!("Subagent {id} · {}", state.owner_label),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Line::styled(
                "g select · m Main · h/l sibling",
                Style::default().fg(Color::DarkGray),
            ),
            Line::default(),
        ],
    }
}

fn display_agent_name(value: &str) -> String {
    let mut characters = value.chars();
    characters.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(characters).collect()
    })
}

fn is_task_tool_name(name: &str) -> bool {
    matches!(
        name,
        "task"
            | "native::task"
            | "task_control"
            | "native::task_control"
            | "task_message"
            | "native::task_message"
    )
}

fn subagent_picker_detail(card: &SubagentCard) -> String {
    let status = match card.status {
        Some(SubagentStatus::Success) => "Success",
        Some(SubagentStatus::Failure) => "Failure",
        Some(SubagentStatus::Cancelled) => "Cancelled",
        None if card.has_activity => "Running",
        None => "Initializing",
    };
    format!(
        "{status} · {}",
        card.task_summary.replace(['\n', '\r'], " ")
    )
}

fn execution_priority(state: TuiExecutionState) -> u8 {
    match state {
        TuiExecutionState::ForegroundRunning => 3,
        TuiExecutionState::BackgroundRunning => 2,
        TuiExecutionState::CompletedRecent
        | TuiExecutionState::Failed
        | TuiExecutionState::Cancelled => 1,
    }
}

const fn execution_state_glyph(state: TuiExecutionState) -> &'static str {
    match state {
        TuiExecutionState::ForegroundRunning | TuiExecutionState::BackgroundRunning => "●",
        TuiExecutionState::CompletedRecent => "✓",
        TuiExecutionState::Failed => "✗",
        TuiExecutionState::Cancelled => "○",
    }
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
    recovered_failed_prompt: bool,
    size: (u16, u16),
    running: bool,
    session_loading: bool,
    assistant_streaming: bool,
    quit_armed_until: Option<Duration>,
    transcripts: BTreeMap<TranscriptId, TranscriptRecord>,
    active_transcript: TranscriptId,
    child_transcript_order: Vec<TranscriptId>,
    transcript: Vec<TranscriptEntry>,
    provider_model: String,
    reasoning_effort: Option<String>,
    context_window: Option<u64>,
    session: String,
    project: String,
    turn_state: Option<TurnState>,
    active_tool: Option<String>,
    runtime_events: Vec<TuiRuntimeEvent>,
    turn_duration: Option<Duration>,
    turn_started_at: Option<Duration>,
    latest_usage: Option<Usage>,
    status: Option<String>,
    restored_syntax_ready_at: Option<Duration>,
    highlight_restored_syntax: bool,
    completed_conversations: Vec<Conversation>,
    conversation: Option<Conversation>,
    dialog: Option<DialogView>,
    secret_entry: Option<SecretEntryState>,
    device_auth: Option<DeviceAuthState>,
    palette_entries: Vec<PaletteEntry>,
    palette_open: bool,
    palette_selected: usize,
    file_candidates: Vec<String>,
    file_picker: Option<FilePicker>,
    agent_catalog: Vec<String>,
    selected_agent: Option<String>,
    dangerous_mode: bool,
    bypass: bool,
    executions: Vec<TuiExecution>,
    execution_selection: Option<TranscriptId>,
    pending_auto_turns: usize,
    now: Duration,
    next_runtime_ordinal: u64,
    mouse_selection_snapshot: Option<MouseSelectionSnapshot>,
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
            recovered_failed_prompt: false,
            size: (80, 24),
            running: false,
            session_loading: false,
            assistant_streaming: false,
            quit_armed_until: None,
            transcripts: BTreeMap::from([(TranscriptId::Main, TranscriptRecord::main())]),
            active_transcript: TranscriptId::Main,
            child_transcript_order: Vec::new(),
            transcript: Vec::new(),
            provider_model: String::new(),
            reasoning_effort: None,
            context_window: None,
            session: "new session".to_owned(),
            project: "agens".to_owned(),
            turn_state: None,
            active_tool: None,
            runtime_events: Vec::new(),
            turn_duration: None,
            turn_started_at: None,
            latest_usage: None,
            status: None,
            restored_syntax_ready_at: None,
            highlight_restored_syntax: true,
            completed_conversations: Vec::new(),
            conversation: None,
            dialog: None,
            secret_entry: None,
            device_auth: None,
            palette_entries: Vec::new(),
            palette_open: false,
            palette_selected: 0,
            file_candidates: Vec::new(),
            file_picker: None,
            agent_catalog: vec!["main".into()],
            selected_agent: None,
            dangerous_mode: false,
            bypass: false,
            executions: Vec::new(),
            execution_selection: None,
            pending_auto_turns: 0,
            now: Duration::ZERO,
            next_runtime_ordinal: 0,
            mouse_selection_snapshot: None,
        }
    }

    /// Handles one input or resize event without performing rendering or engine work.
    pub fn handle(&mut self, event: Event) -> Action {
        match event {
            Event::Resize { width, height } => {
                self.size = (width, height);
                self.mouse_selection_snapshot = None;
                self.clamp_palette_selection();
                self.clamp_scroll_offset();
                self.ensure_dialog_selection_visible();
                Action::Render
            }
            Event::Key(key) => self.handle_key(key),
            Event::MouseWheel(direction) => self.handle_mouse_wheel_batch(&[direction]),
            Event::MouseDown { column, row } => self
                .handle_subagent_tree_click(column, row)
                .unwrap_or_else(|| self.begin_mouse_selection(column, row)),
            Event::MouseDrag { column, row } => self.update_mouse_selection(column, row, true),
            Event::MouseUp { column, row } => self.update_mouse_selection(column, row, false),
            Event::Paste(text) if self.secret_entry.is_some() => {
                self.append_secret_text(&text);
                Action::Render
            }
            Event::Paste(text) => {
                let child_read_only = self.active_transcript != TranscriptId::Main
                    && (self.active_record_mut().terminal
                        || self.active_record_mut().focus != TranscriptFocus::Composer);
                if child_read_only
                    || self
                        .dialog
                        .as_ref()
                        .is_some_and(|dialog| dialog.interactive)
                {
                    Action::Render
                } else {
                    self.quit_armed_until = None;
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

    /// Installs the project files the `@` picker can insert.
    ///
    /// The composition root enumerates them once, confined to the project root,
    /// so the picker never touches the filesystem from the render loop.
    pub fn set_file_candidates(&mut self, candidates: Vec<String>) {
        self.file_candidates = candidates;
        self.refresh_file_picker();
    }

    pub fn set_collapse_thinking(&mut self, collapse: bool) {
        let record = self.active_record_mut();
        record.collapse_thinking = collapse;
        record.thinking_user_pinned = !collapse;
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
            execution_priority(right.state)
                .cmp(&execution_priority(left.state))
                .then_with(|| right.last_activity.cmp(&left.last_activity))
                .then_with(|| right.id.cmp(&left.id))
        });
        executions
    }

    pub fn tick(&mut self, now: Duration) {
        self.now = now;
        if self.quit_armed_until.is_some_and(|until| now >= until) {
            self.quit_armed_until = None;
        }
        if self
            .restored_syntax_ready_at
            .is_some_and(|ready_at| now >= ready_at)
        {
            self.restored_syntax_ready_at = None;
            self.highlight_restored_syntax = true;
        }
        self.executions.retain(|execution| {
            execution
                .terminal_at
                .is_none_or(|terminal_at| now < terminal_at + Duration::from_secs(60))
        });
        if self.execution_selection.is_some_and(|selection| {
            selection != TranscriptId::Main
                && !self
                    .executions
                    .iter()
                    .any(|execution| TranscriptId::Subagent(execution.id) == selection)
        }) {
            self.execution_selection = None;
        }
    }

    /// Updates active-turn state after the composition layer starts or finishes a turn.
    pub fn set_running(&mut self, running: bool) {
        let finishing = self.running && !running;
        self.running = running;
        if running {
            self.turn_started_at = Some(self.now);
            self.palette_open = false;
            self.turn_state = Some(TurnState::Requesting);
        } else if !matches!(
            self.turn_state,
            Some(TurnState::Failed | TurnState::Cancelled)
        ) {
            self.turn_state = None;
            self.active_tool = None;
        }
        if !running {
            self.assistant_streaming = false;
        }
        if finishing {
            self.settle_active_conversation();
            self.auto_collapse_thinking_on_finish();
        }
    }

    fn settle_active_conversation(&mut self) {
        if let Some(conversation) = self.conversation.as_mut() {
            conversation.mark_settled();
        }
    }

    fn auto_collapse_thinking_on_finish(&mut self) {
        let record = self.active_record_mut();
        let mode = if record.thinking_user_pinned {
            widgets::ExpandMode::Expanded
        } else if record.collapse_thinking {
            widgets::ExpandMode::Collapsed
        } else {
            widgets::ExpandMode::Streaming
        };
        record.collapse_thinking = matches!(mode.finish_stream(), widgets::ExpandMode::Collapsed);
    }

    /// Supplies concise provider, model, and active-session context for the terminal surface.
    pub fn set_presentation(
        &mut self,
        provider: impl AsRef<str>,
        model: impl AsRef<str>,
        session: impl Into<String>,
    ) {
        let provider_model = format!("{} / {}", provider.as_ref(), model.as_ref());
        if self.provider_model != provider_model {
            self.latest_usage = None;
        }
        self.provider_model = provider_model;
        self.session = session.into();
    }

    /// Sets the reasoning-effort label shown in the operational footer.
    pub fn set_reasoning_effort(&mut self, effort: Option<impl Into<String>>) {
        self.reasoning_effort = effort.map(Into::into);
    }

    /// Sets the project identity displayed in the semantic terminal header.
    pub fn set_project(&mut self, project: impl Into<String>) {
        self.project = project.into();
    }

    /// Sets the dangerous-mode state displayed for the next submitted turn.
    pub fn set_dangerous_mode(&mut self, enabled: bool) {
        self.dangerous_mode = enabled;
    }

    /// Sets the permission-bypass state displayed for the session.
    pub fn set_bypass(&mut self, enabled: bool) {
        self.bypass = enabled;
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
        self.transcript.push(TranscriptEntry::User(prompt.clone()));
        self.conversation = Some(Conversation::new(prompt));
        {
            let record = self.active_record_mut();
            record.collapse_thinking = false;
            record.thinking_user_pinned = false;
        }
        self.set_running(true);
        self.assistant_streaming = true;
    }

    /// Starts the turn a finished background subagent scheduled, but only at a safe point.
    ///
    /// The shared runtime rejects a concurrent turn, and firing over a composer the user is
    /// typing into would submit their unfinished prompt, so a scheduled turn waits for the next
    /// idle moment instead of being dropped. Every completion queued while waiting is carried by
    /// the single turn this returns.
    pub fn take_ready_auto_turn(&mut self) -> Option<String> {
        if self.pending_auto_turns == 0 || !self.auto_turn_is_safe() {
            return None;
        }

        let finished = std::mem::take(&mut self.pending_auto_turns);
        self.begin_auto_turn(finished);
        Some(auto_turn_prompt(finished))
    }

    fn auto_turn_is_safe(&self) -> bool {
        !self.running
            && !self.session_loading
            && self.input.is_empty()
            && self.dialog.is_none()
            && !self.palette_open
    }

    /// Opens the runtime-scheduled turn without a user prompt: the transcript records why the
    /// turn is running as a notice, because the user did not ask for it.
    fn begin_auto_turn(&mut self, finished: usize) {
        let notice = auto_turn_notice(finished);
        self.status = None;
        if let Some(conversation) = self.conversation.take() {
            self.completed_conversations.push(conversation);
        }
        self.runtime_events.clear();
        self.turn_duration = None;
        self.latest_usage = None;
        self.transcript.push(TranscriptEntry::Info(notice.clone()));
        self.conversation = Some(Conversation::new(String::new()));
        self.project_conversation(ConversationEvent::Info(notice));
        {
            let record = self
                .transcripts
                .get_mut(&TranscriptId::Main)
                .expect("main transcript always exists");
            record.collapse_thinking = false;
            record.thinking_user_pinned = false;
        }
        self.set_running(true);
        self.assistant_streaming = true;
    }

    pub fn begin_route(&mut self) {
        self.status = None;
        self.palette_open = false;
        self.runtime_events.clear();
        self.turn_duration = None;
        self.latest_usage = None;
        self.dialog = None;
        self.device_auth = None;
        self.running = true;
        self.turn_started_at = Some(self.now);
        self.assistant_streaming = false;
        self.turn_state = None;
        self.quit_armed_until = None;
    }

    pub fn begin_session_load(&mut self) -> bool {
        if self.running || self.session_loading {
            return false;
        }

        self.session_loading = true;
        true
    }

    pub fn finish_session_load(&mut self) {
        self.session_loading = false;
    }

    pub fn cancel_session_load(&mut self) {
        self.session_loading = false;
        self.dialog = None;
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
            } => {
                self.dialog = None;
                self.device_auth = Some(DeviceAuthState {
                    verification_url: bounded_auth_text(&verification_url, 512),
                    user_code: bounded_auth_text(&user_code, 64),
                    selected: 0,
                    confirmation: None,
                });
                return;
            }
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
        const MAX_DIAGNOSTIC_CHARS: usize = 240;

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
        self.device_auth = None;
        if !matches!(&outcome, TuiSubmissionOutcome::SecretEntry(_)) {
            self.secret_entry = None;
        }
        if !matches!(
            &outcome,
            TuiSubmissionOutcome::Dialog(_) | TuiSubmissionOutcome::SafeDialog(_)
        ) {
            self.dialog = None;
        }
        match outcome {
            TuiSubmissionOutcome::SecretEntry(view) => {
                self.dialog = None;
                self.file_picker = None;
                self.secret_entry = Some(SecretEntryState {
                    view,
                    input: SecretInput(String::new()),
                    error: false,
                });
                None
            }
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
                history,
                draft,
                resume_error,
                file_candidates,
                palette_entries,
            } => {
                self.finish_session_load();
                self.replace_projected_history(history);
                self.apply_presentation(presentation);
                self.set_file_candidates(file_candidates);
                self.set_palette_entries(palette_entries);
                self.input.clear();
                self.input_cursor = 0;
                self.recovered_failed_prompt = false;
                if let Some(draft) = draft {
                    self.restore_resume_draft(draft);
                }
                self.highlight_restored_syntax = false;
                self.restored_syntax_ready_at =
                    Some(self.now.saturating_add(ACTIVE_FRAME_HEARTBEAT));
                self.status = Some(message);
                if let Some(error) = resume_error {
                    self.show_dialog(
                        "Action required",
                        format!("Saved provider is unavailable.\nAction: {error}."),
                    );
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
            TuiSubmissionOutcome::TranscriptDialog => {
                self.set_running(false);
                self.show_transcript_dialog();
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
            TuiSubmissionOutcome::RouteCancelled => {
                self.finish_session_load();
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

    fn restore_resume_draft(&mut self, draft: String) {
        self.input = draft;
        self.input_cursor = self.input.chars().count();
        self.recovered_failed_prompt = true;
        let scroll_offset = self.following_scroll_bottom();
        let record = self.active_record_mut();
        record.focus = TranscriptFocus::Composer;
        record.following_bottom = true;
        record.scroll_offset = scroll_offset;
    }

    pub fn finish_provider_turn(&mut self, outcome: TuiProviderOutcome) {
        match outcome {
            TuiProviderOutcome::Completed(output) => {
                // Prefer the live stream body when present; heal exact dual-progress
                // duplication (live == output+output) using the completed string once.
                let body = match self.conversation.as_ref() {
                    Some(conversation) if !conversation.live_markdown.is_empty() => {
                        let live = &conversation.live_markdown;
                        if !output.is_empty()
                            && (live.as_str() == format!("{output}{output}")
                                || (live.len() == output.len().saturating_mul(2)
                                    && live.starts_with(&output)
                                    && live.ends_with(&output)))
                        {
                            output.clone()
                        } else {
                            live.clone()
                        }
                    }
                    _ => output.clone(),
                };
                self.project_conversation(ConversationEvent::MarkdownFinal(body.clone()));
                if let Some(TranscriptEntry::Assistant(text)) = self.transcript.last_mut() {
                    *text = body;
                } else {
                    self.transcript.push(TranscriptEntry::Assistant(body));
                }
                self.set_running(false);
            }
            TuiProviderOutcome::Failed { message, action } => {
                let finishing = self.running;
                self.running = false;
                self.assistant_streaming = false;
                self.turn_state = Some(TurnState::Failed);
                self.active_tool = None;
                if finishing {
                    self.settle_active_conversation();
                    self.auto_collapse_thinking_on_finish();
                }
                self.add_error(message, action);
            }
            TuiProviderOutcome::Cancelled { message, action } => {
                let finishing = self.running;
                self.running = false;
                self.assistant_streaming = false;
                self.turn_state = Some(TurnState::Cancelled);
                self.active_tool = None;
                if finishing {
                    self.settle_active_conversation();
                    self.auto_collapse_thinking_on_finish();
                }
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
        self.active_record_mut().tool_display_modes.clear();
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
        self.replace_projected_history(conversations);
        Ok(())
    }

    fn replace_projected_history(&mut self, conversations: Vec<Conversation>) {
        let completed_tool_call_ids = conversations
            .iter()
            .flat_map(|conversation| &conversation.tool_batches)
            .flat_map(|batch| &batch.calls)
            .filter(|call| call.result.is_some())
            .map(|call| call.call_id.clone())
            .collect::<Vec<_>>();
        self.transcript.clear();
        self.completed_conversations = conversations;
        self.conversation = None;
        self.runtime_events.clear();
        self.turn_duration = None;
        self.latest_usage = None;
        self.set_running(false);
        self.turn_state = None;
        self.active_tool = None;
        self.clear_current_session_transcripts();
        {
            let record = self.active_record_mut();
            record.tool_display_modes.clear();
            record.tool_display_modes.extend(
                completed_tool_call_ids
                    .into_iter()
                    .map(|call_id| (call_id, widgets::DisplayMode::Collapsed)),
            );
            // Restored history is finished: collapse thinking unless the user re-expands.
            record.collapse_thinking = true;
            record.thinking_user_pinned = false;
        }
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
        self.execution_selection = Some(self.active_transcript);
    }

    fn active_record_mut(&mut self) -> &mut TranscriptRecord {
        self.transcripts
            .get_mut(&self.active_transcript)
            .expect("active transcript always exists")
    }

    fn show_transcript_dialog(&mut self) {
        if self.child_transcript_order.is_empty() {
            return;
        }
        let mut entries = vec![DialogEntry::transcript("Main", TranscriptId::Main)];
        entries.extend(
            self.child_transcript_order
                .iter()
                .copied()
                .filter(|id| self.transcripts.contains_key(id))
                .filter_map(|id| {
                    let TranscriptId::Subagent(id_value) = id else {
                        return None;
                    };
                    let record = self.transcripts.get(&id)?;
                    let agent = display_agent_name(&record.owner_label);
                    let detail = self
                        .subagent_card(id_value)
                        .map(subagent_picker_detail)
                        .unwrap_or_else(|| "Transcript available".into());
                    Some(DialogEntry::transcript_with_detail(
                        format!("{agent} #{id_value}"),
                        detail,
                        id,
                    ))
                }),
        );
        let selected = if self.active_transcript == TranscriptId::Main && entries.len() > 1 {
            1
        } else {
            std::iter::once(TranscriptId::Main)
                .chain(self.child_transcript_order.iter().copied())
                .position(|id| id == self.active_transcript)
                .unwrap_or_default()
        };
        self.show_selection_dialog(
            DialogView::selection("Subagents", None::<&str>, entries).with_selected(selected),
        );
    }

    fn subagent_card(&self, id: u64) -> Option<&SubagentCard> {
        self.completed_conversations
            .iter()
            .chain(self.conversation.as_ref())
            .flat_map(|conversation| &conversation.subagent_cards)
            .find(|card| card.id == id)
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

    fn execution_strip_ids(&self) -> Vec<TranscriptId> {
        std::iter::once(TranscriptId::Main)
            .chain(
                self.executions()
                    .into_iter()
                    .take(3)
                    .map(|execution| TranscriptId::Subagent(execution.id)),
            )
            .collect()
    }

    fn focus_execution_strip(&mut self) {
        let ids = self.execution_strip_ids();
        self.execution_selection = match self.execution_selection {
            None => Some(TranscriptId::Main),
            Some(current) => ids
                .iter()
                .position(|id| *id == current)
                .map(|index| ids[(index + 1) % ids.len()])
                .or(Some(TranscriptId::Main)),
        };
    }

    fn move_execution_selection(&mut self, direction: isize) {
        let ids = self.execution_strip_ids();
        let current = self.execution_selection.unwrap_or(TranscriptId::Main);
        let Some(index) = ids.iter().position(|id| *id == current) else {
            self.execution_selection = Some(TranscriptId::Main);
            return;
        };
        let next = index
            .checked_add_signed(direction)
            .map(|index| index % ids.len())
            .unwrap_or(ids.len().saturating_sub(1));
        self.execution_selection = ids.get(next).copied();
    }

    fn inspect_execution_selection(&mut self) {
        if let Some(id) = self.execution_selection {
            self.select_transcript(id);
        }
    }

    /// Geometry of the current screen, so hit tests and scroll bounds address the
    /// same rows the renderer paints.
    fn screen_layout(&self) -> ScreenLayout {
        let area = Rect::new(0, 0, self.size.0.max(1), self.size.1.max(1));
        screen_layout(area, &self.input, !notice_spans(&self.view()).is_empty())
    }

    /// Selects a transcript from a click on the subagent tree.
    ///
    /// Tree rows are addressed by row, not column: the root is `Main` and each
    /// following row is one navigable execution. Child activity and hint rows
    /// carry no transcript, so a click there is ignored.
    fn handle_subagent_tree_click(&mut self, _column: u16, row: u16) -> Option<Action> {
        if self.executions.is_empty() || self.dialog.is_some() || self.palette_open {
            return None;
        }
        let layout = self.screen_layout();
        if layout.tree.height == 0 || row < layout.tree.y || row >= layout.tree.bottom() {
            return None;
        }

        let index = usize::from(row.saturating_sub(layout.tree.y));
        let id = *self.execution_strip_ids().get(index)?;
        self.execution_selection = Some(id);
        self.select_transcript(id);
        Some(Action::Render)
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
                let finishing = self.running;
                self.running = false;
                self.turn_state = Some(*status);
                self.turn_duration = *duration;
                self.active_tool = None;
                if finishing {
                    self.auto_collapse_thinking_on_finish();
                }
            }
            TuiRuntimeEvent::Usage(usage) => self.latest_usage = Some(usage.clone()),
            TuiRuntimeEvent::Diff { lines, .. } => {
                self.project_conversation(ConversationEvent::Diff(lines.clone()));
            }
            TuiRuntimeEvent::TaskExecution { agent, event } => {
                self.apply_task_execution_event(agent, *event);
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
            TuiRuntimeEvent::ToolStarted {
                call_id, parsed, ..
            } => {
                if let Some(conversation) = self.conversation.as_mut() {
                    conversation.enrich_parsed_tool_input(call_id, parsed.clone());
                }
            }
            TuiRuntimeEvent::ToolEnded { .. } => {}
            TuiRuntimeEvent::Notice(text) => {
                self.transcript.push(TranscriptEntry::Info(text.clone()));
                self.project_conversation(ConversationEvent::Info(text.clone()));
            }
        }
        self.runtime_events.push(event);
    }

    fn apply_subagent_event(&mut self, event: &TuiSubagentEvent) {
        if let Some(execution) = self
            .executions
            .iter_mut()
            .find(|execution| execution.id == event.id)
        {
            execution.last_activity = self.now;
            if execution.terminal_at.is_some()
                && matches!(
                    &event.update,
                    agens_core::TuiSubagentUpdate::Terminal { .. }
                )
            {
                execution.terminal_at = Some(self.now);
            }
        }
        if let agens_core::TuiSubagentUpdate::Started { agent, .. } = &event.update {
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
        if let agens_core::TuiSubagentUpdate::ToolResult { call_id, .. } = &event.update {
            self.transcripts
                .get_mut(&TranscriptId::Subagent(event.id))
                .expect("admitted child event has a transcript")
                .tool_display_modes
                .insert(call_id.clone(), widgets::DisplayMode::Collapsed);
        }
        if matches!(
            &event.update,
            agens_core::TuiSubagentUpdate::Terminal { .. }
        ) {
            let record = self
                .transcripts
                .get_mut(&TranscriptId::Subagent(event.id))
                .expect("admitted child event has a transcript");
            if let Some(conversation) = record.conversation.as_mut() {
                conversation.mark_settled();
            }
            if !record.thinking_user_pinned {
                record.collapse_thinking = true;
            }
        }

        self.conversation
            .get_or_insert_with(|| Conversation::new(String::new()))
            .apply_subagent_summary(event.clone(), self.now);
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
                update: agens_core::TuiSubagentUpdate::Terminal { .. },
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
                tool_display_modes: BTreeMap::new(),
                collapse_thinking: false,
                thinking_user_pinned: false,
                focus: TranscriptFocus::Viewport,
                selection: None,
                selection_text: None,
                selection_too_large: false,
                selecting: false,
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
        self.execution_selection = None;
        self.child_transcript_order.clear();
        self.next_runtime_ordinal = 0;
    }

    /// Adds a typed event to the authoritative, lossless conversation projection.
    pub fn apply_conversation_event(
        &mut self,
        event: ConversationEvent,
    ) -> Result<(), ConversationError> {
        let completed_tool_call = match &event {
            ConversationEvent::ToolResult { call_id, .. } => Some(call_id.clone()),
            _ => None,
        };
        self.conversation
            .get_or_insert_with(|| Conversation::new(String::new()))
            .apply(event)?;
        if let Some(call_id) = completed_tool_call {
            self.active_record_mut()
                .tool_display_modes
                .insert(call_id, widgets::DisplayMode::Collapsed);
        }
        Ok(())
    }

    /// Opens a generic bounded dialog without changing the underlying conversation.
    pub fn show_dialog(&mut self, title: impl Into<String>, body: impl Into<String>) {
        self.dialog = Some(DialogView::informational(title.into(), body.into()));
    }

    pub fn show_selection_dialog(&mut self, mut dialog: DialogView) {
        if let (Some(current), Some(incoming)) = (
            self.dialog
                .as_ref()
                .and_then(|dialog| dialog.session_entries.as_ref()),
            dialog.session_entries.as_ref(),
        ) && current.loading
            && current.request.generation != incoming.request.generation
        {
            return;
        }
        if dialog.refresh_id.is_some()
            && dialog.refresh_id
                == self
                    .dialog
                    .as_ref()
                    .and_then(|current| current.refresh_id.clone())
            && let Some(current) = self.dialog.as_ref()
        {
            dialog.query.clone_from(&current.query);
            dialog.searching = current.searching;
            dialog.selected = current.selected.min(dialog.entries.len().saturating_sub(1));
            dialog.details_open = current.details_open;
        }
        // A session page replaces the loading placeholder the keystroke created,
        // so search mode has to survive the swap or the band would vanish under
        // the cursor on every typed character.
        if dialog.session_entries.is_some()
            && let Some(current) = self.dialog.as_ref()
            && current.session_entries.is_some()
        {
            dialog.searching = current.searching;
        }
        self.palette_open = false;
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
            recovered_failed_prompt: self.recovered_failed_prompt,
            size: self.size,
            running: self.running,
            session_loading: self.session_loading,
            assistant_streaming: self.assistant_streaming,
            quit_armed: self.quit_is_armed(),
            transcript: &active.transcript,
            following_bottom: active.following_bottom,
            scroll_offset: active.scroll_offset,
            selection: active.selection,
            provider_model: &self.provider_model,
            reasoning_effort: self.reasoning_effort.as_deref(),
            context_window: self.context_window,
            session: &self.session,
            project: &self.project,
            turn_state: self.turn_state,
            dangerous_mode: self.dangerous_mode,
            bypass: self.bypass,
            active_tool: self.active_tool.as_deref(),
            input_cursor: self.input_cursor,
            runtime_events: &self.runtime_events,
            turn_duration: self.turn_duration,
            latest_usage: self.latest_usage.as_ref(),
            status: self.status.as_deref(),
            now: self.now,
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
            highlight_restored_syntax: self.highlight_restored_syntax,
            tool_display_modes: &active.tool_display_modes,
            collapse_thinking: active.collapse_thinking,
            focus: active.focus,
            dialog: self.dialog.as_ref(),
            secret_entry: self.secret_entry.as_ref().map(|entry| SecretEntryRender {
                title: &entry.view.title,
                help: entry.view.help.as_deref(),
                mask: entry.input.0.len(),
                error: entry.error.then_some(SECRET_REQUIRED_ERROR),
            }),
            device_auth: self.device_auth.as_ref().map(|entry| DeviceAuthRender {
                verification_url: &entry.verification_url,
                user_code: &entry.user_code,
                selected: entry.selected,
                confirmation: entry.confirmation,
            }),
            palette: self.palette_open.then_some(PaletteView {
                entries: &self.palette_entries,
                selected: self.palette_selected,
            }),
            file_picker: self.file_picker_query().map(|query| FilePickerView {
                candidates: &self.file_candidates,
                query,
                selected: self.file_picker.map_or(0, |picker| picker.selected),
            }),
            agent_catalog: &self.agent_catalog,
            selected_agent: self.selected_agent.as_deref(),
            executions: self.executions(),
            execution_selection: self.execution_selection,
            execution_activities: self.focused_execution_activities(),
            turn_started_at: self.turn_started_at,
        }
    }

    /// Live tool activity of the subagent the tree currently focuses.
    ///
    /// Focus follows an explicit tree selection, then the active transcript,
    /// then the first running execution, so the tree always expands the branch
    /// the user is most likely acting on.
    fn focused_execution_activities(&self) -> Vec<TuiExecutionActivity> {
        let visible = self
            .executions()
            .into_iter()
            .take(MAX_TREE_EXECUTIONS)
            .map(|execution| execution.id)
            .collect::<Vec<_>>();
        let preferred = match (self.execution_selection, self.active_transcript) {
            (Some(TranscriptId::Subagent(id)), _) | (_, TranscriptId::Subagent(id)) => Some(id),
            _ => None,
        }
        .filter(|id| visible.contains(id));

        if let Some(id) = preferred {
            return self.execution_activities(id);
        }
        visible
            .into_iter()
            .map(|id| self.execution_activities(id))
            .find(|activities| !activities.is_empty())
            .unwrap_or_default()
    }

    fn execution_activities(&self, parent: u64) -> Vec<TuiExecutionActivity> {
        let Some(conversation) = self
            .transcripts
            .get(&TranscriptId::Subagent(parent))
            .and_then(|record| record.conversation.as_ref())
        else {
            return Vec::new();
        };

        let mut activities = conversation
            .tool_batches
            .iter()
            .flat_map(|batch| &batch.calls)
            .filter_map(|call| {
                conversation::subagent_activity(&call.name).map(|label| TuiExecutionActivity {
                    parent,
                    label: label.to_owned(),
                    running: call.result.is_none(),
                })
            })
            .collect::<Vec<_>>();
        let start = activities.len().saturating_sub(MAX_TREE_ACTIVITIES);
        activities.drain(..start);
        activities
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
        let finished_in_background = execution.state == TuiExecutionState::BackgroundRunning
            && !matches!(state, TuiExecutionState::BackgroundRunning);
        execution.state = state;
        execution.last_activity = self.now;
        if !matches!(state, TuiExecutionState::BackgroundRunning) {
            execution.terminal_at = Some(self.now);
        }
        if finished_in_background {
            self.pending_auto_turns = self.pending_auto_turns.saturating_add(1);
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
            started_at: self.now,
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
            agens_core::TuiSubagentUpdate::Started {
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
            agens_core::TuiSubagentUpdate::Reasoning(_)
            | agens_core::TuiSubagentUpdate::Text(_)
            | agens_core::TuiSubagentUpdate::ToolCall { .. }
            | agens_core::TuiSubagentUpdate::ToolResult { .. }
            | agens_core::TuiSubagentUpdate::Error { .. } => matches!(
                execution.state,
                TuiExecutionState::ForegroundRunning | TuiExecutionState::BackgroundRunning
            ),
            agens_core::TuiSubagentUpdate::Terminal { status, .. } => {
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

    pub fn selected_text(&self) -> Option<&str> {
        self.transcripts
            .get(&self.active_transcript)
            .and_then(|record| record.selection_text.as_deref())
    }

    fn begin_mouse_selection(&mut self, column: u16, row: u16) -> Action {
        let Some(snapshot) = self.capture_mouse_selection_snapshot() else {
            return Action::Render;
        };
        let Some(position) = snapshot.position(column, row) else {
            return Action::Render;
        };
        self.mouse_selection_snapshot = Some(snapshot);
        let record = self.active_record_mut();
        record.focus = TranscriptFocus::Viewport;
        record.selection = Some(TranscriptSelection {
            anchor: position,
            head: position,
        });
        record.selection_text = None;
        record.selection_too_large = false;
        record.selecting = true;
        Action::Render
    }

    fn update_mouse_selection(&mut self, column: u16, row: u16, dragging: bool) -> Action {
        if !self
            .transcripts
            .get(&self.active_transcript)
            .expect("active transcript always exists")
            .selecting
        {
            return Action::Render;
        }
        if let Some(position) = self
            .mouse_selection_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.position(column, row))
            && let Some(selection) = self.active_record_mut().selection.as_mut()
        {
            selection.head = position;
        }
        if dragging {
            return Action::Render;
        }

        let selection = {
            let record = self.active_record_mut();
            record.selecting = false;
            record.selection
        };
        let selected_text = selection.and_then(|selection| {
            self.mouse_selection_snapshot
                .as_ref()
                .map(|snapshot| snapshot.transcript.selected_text(selection))
        });
        self.mouse_selection_snapshot = None;
        let record = self.active_record_mut();
        match selected_text {
            Some(Ok(text)) if !text.is_empty() => {
                record.selection_text = Some(text);
                record.selection_too_large = false;
            }
            Some(Ok(_)) | None => {
                record.selection = None;
                record.selection_text = None;
                record.selection_too_large = false;
            }
            Some(Err(())) => {
                record.selection_text = None;
                record.selection_too_large = true;
            }
        }
        Action::Render
    }

    fn capture_mouse_selection_snapshot(&self) -> Option<MouseSelectionSnapshot> {
        if self.dialog.is_some() || self.palette_open {
            return None;
        }
        let view = self.view();
        let layout = self.screen_layout();
        let row_width = layout
            .transcript
            .width
            .saturating_sub(TRANSCRIPT_ROW_INDENT)
            .max(1);
        let transcript =
            SelectableTranscript::from_lines(&rendered_transcript(&view, row_width), row_width);
        let chrome_rows = transcript_chrome_rows(view.following_bottom);
        let bottom = saturating_u16(transcript.rows.len().saturating_sub(usize::from(
            layout.transcript.height.saturating_sub(chrome_rows),
        )));
        let scroll = if view.following_bottom {
            bottom
        } else {
            view.scroll_offset.min(bottom)
        };

        Some(MouseSelectionSnapshot {
            transcript,
            content_x: layout.transcript.x.saturating_add(TRANSCRIPT_ROW_INDENT),
            content_y: layout.transcript.y.saturating_add(1),
            content_right: layout.transcript.right(),
            content_bottom: layout
                .transcript
                .bottom()
                .saturating_sub(chrome_rows.saturating_sub(1)),
            scroll,
        })
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
                let hidden_task = is_task_tool_name(&name);
                self.project_conversation(ConversationEvent::ToolCall {
                    call_id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                    // No parser is available on the raw live-event path; the
                    // typed `TuiRuntimeEvent::ToolStarted` carrier corrects
                    // this in `apply_runtime_event_with_ordinal`.
                    parsed: agens_core::ToolInput::Other {
                        name: name.clone(),
                        raw: input.clone(),
                    },
                });
                self.turn_state = Some(TurnState::Dispatching);
                self.active_tool = (!hidden_task).then_some(name.clone());
                if !hidden_task {
                    self.transcript
                        .push(TranscriptEntry::Tool(format!("{name} started")));
                }
            }
            TurnEvent::ToolResult(MessagePart::ToolResult {
                tool_call_id,
                content,
                is_error,
            }) => {
                let hidden_task = self.main_task_call_ids().any(|id| id == tool_call_id);
                self.project_conversation(ConversationEvent::ToolResult {
                    call_id: tool_call_id.clone(),
                    output: content.clone(),
                    is_error,
                });
                if hidden_task {
                    return;
                }
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
                let finishing = self.running;
                self.running = false;
                self.assistant_streaming = false;
                self.turn_state = Some(state);
                self.active_tool = None;
                if finishing {
                    self.settle_active_conversation();
                    self.auto_collapse_thinking_on_finish();
                }
            }
            TurnEvent::StateChanged(state) => self.turn_state = Some(state),
            _ => {}
        }
    }

    fn main_task_call_ids(&self) -> impl Iterator<Item = &str> {
        self.completed_conversations
            .iter()
            .chain(self.conversation.as_ref())
            .flat_map(|conversation| &conversation.tool_batches)
            .flat_map(|batch| &batch.calls)
            .filter(|call| is_task_tool_name(&call.name))
            .map(|call| call.call_id.as_str())
    }

    fn append_secret_text(&mut self, text: &str) {
        let Some(entry) = self.secret_entry.as_mut() else {
            return;
        };
        let remaining = MAX_SECRET_INPUT_BYTES.saturating_sub(entry.input.0.len());
        let accepted = text
            .bytes()
            .filter(|byte| byte.is_ascii_graphic() || *byte == b' ')
            .take(remaining)
            .collect::<Vec<_>>();
        if !accepted.is_empty() {
            entry.input.0.extend(accepted.into_iter().map(char::from));
            entry.error = false;
        }
    }

    fn handle_secret_key(&mut self, key: Key) -> Action {
        match key {
            Key::Char(character) if character.is_ascii() => {
                self.append_secret_text(&character.to_string());
            }
            Key::Backspace | Key::Delete => {
                if let Some(entry) = self.secret_entry.as_mut() {
                    entry.input.0.pop();
                }
            }
            Key::DeleteToLineStart => {
                if let Some(entry) = self.secret_entry.as_mut() {
                    entry.input.0.clear();
                }
            }
            Key::Escape | Key::CtrlC => {
                self.secret_entry = None;
            }
            Key::Enter => {
                let Some(mut entry) = self.secret_entry.take() else {
                    return Action::Render;
                };
                let trimmed = entry.input.0.trim();
                if trimmed.is_empty() {
                    entry.input.0.clear();
                    entry.error = true;
                    self.secret_entry = Some(entry);
                    return Action::Render;
                }
                let secret = SecretInput(trimmed.to_owned());
                return Action::SubmitSecret {
                    action_id: entry.view.submit_action,
                    secret,
                };
            }
            _ => {}
        }
        Action::Render
    }

    /// Returns the selected device-authentication value without exposing it through actions.
    pub fn device_auth_clipboard_text(&self) -> Option<&str> {
        let entry = self.device_auth.as_ref()?;
        match entry.selected {
            0 | 1 => Some(&entry.verification_url),
            2 => Some(&entry.user_code),
            _ => None,
        }
    }

    /// Returns the verification URL for the runtime's injected browser opener.
    pub fn device_auth_verification_url(&self) -> Option<&str> {
        self.device_auth
            .as_ref()
            .map(|entry| entry.verification_url.as_str())
    }

    pub fn apply_device_auth_open_result(&mut self, succeeded: bool) {
        if let Some(entry) = self.device_auth.as_mut() {
            entry.confirmation = Some(if succeeded {
                "Browser opened."
            } else {
                "Could not open browser. Copy the link instead."
            });
        }
    }

    fn handle_device_auth_key(&mut self, key: Key) -> Action {
        let Some(entry) = self.device_auth.as_mut() else {
            return Action::Render;
        };
        match key {
            Key::Up => entry.selected = entry.selected.saturating_sub(1),
            Key::Down => entry.selected = (entry.selected + 1).min(2),
            Key::Enter => match entry.selected {
                0 => return Action::OpenDeviceAuthUrl,
                1 => {
                    entry.confirmation = Some("Link copied.");
                    return Action::CopyDeviceAuthUrl;
                }
                2 => {
                    entry.confirmation = Some("Code copied.");
                    return Action::CopyDeviceAuthCode;
                }
                _ => unreachable!(),
            },
            Key::Escape => {
                self.device_auth = None;
                return self.cancel_running();
            }
            _ => {}
        }
        Action::Render
    }

    fn handle_key(&mut self, key: Key) -> Action {
        if self.device_auth.is_some() {
            return self.handle_device_auth_key(key);
        }
        if self.secret_entry.is_some() {
            return self.handle_secret_key(key);
        }
        if key != Key::CtrlC {
            self.quit_armed_until = None;
        }
        if key == Key::CtrlC {
            return self.handle_control_c();
        }
        if key == Key::Escape && self.session_loading {
            return Action::CancelRoute;
        }
        if self.session_loading
            && !matches!(
                key,
                Key::PageUp
                    | Key::PageDown
                    | Key::ScrollUp
                    | Key::ScrollDown
                    | Key::Home
                    | Key::End
                    | Key::CtrlJ
                    | Key::CtrlK
                    | Key::CtrlG
                    | Key::CtrlShiftG
            )
        {
            return Action::Render;
        }
        if !matches!(
            key,
            Key::PageUp | Key::PageDown | Key::ScrollUp | Key::ScrollDown | Key::Home | Key::End
        ) {
            self.status = None;
        }

        // Esc closes the topmost overlay first (palette, then dialog, then picker).
        if key == Key::Escape {
            match widgets::OverlayShell::topmost(
                self.palette_open,
                self.dialog.as_ref().map(|dialog| dialog.overlay_kind),
                self.file_picker_open(),
            ) {
                Some(widgets::OverlayKind::Palette) => {
                    self.palette_open = false;
                    return Action::Render;
                }
                Some(widgets::OverlayKind::FilePicker) => {
                    self.file_picker = None;
                    return Action::Render;
                }
                _ => {}
            }
        }

        if self
            .dialog
            .as_ref()
            .is_some_and(|dialog| dialog.interactive)
        {
            return self.handle_selection_dialog_key(key);
        }

        if !self.palette_open && !self.file_picker_open() && !self.executions.is_empty() {
            match key {
                Key::Tab => {
                    self.focus_execution_strip();
                    return Action::Render;
                }
                Key::Up if self.execution_selection.is_some() => {
                    self.move_execution_selection(-1);
                    return Action::Render;
                }
                Key::Down if self.execution_selection.is_some() => {
                    self.move_execution_selection(1);
                    return Action::Render;
                }
                Key::Enter if self.execution_selection.is_some() => {
                    self.inspect_execution_selection();
                    return Action::Render;
                }
                Key::CtrlB if self.execution_selection.is_some() => {
                    return self.handle_background_key();
                }
                _ => {}
            }
        }

        if key == Key::Escape && self.active_transcript != TranscriptId::Main {
            self.select_transcript(TranscriptId::Main);
            return Action::Render;
        }

        match key {
            Key::Char('g') if self.active_record_mut().focus == TranscriptFocus::Viewport => {
                self.show_transcript_dialog();
                return Action::Render;
            }
            Key::Char('m') if self.active_record_mut().focus == TranscriptFocus::Viewport => {
                self.select_transcript(TranscriptId::Main);
                return Action::Render;
            }
            Key::Char('h') if self.active_record_mut().focus == TranscriptFocus::Viewport => {
                self.select_sibling(-1);
                return Action::Render;
            }
            Key::Char('l') if self.active_record_mut().focus == TranscriptFocus::Viewport => {
                self.select_sibling(1);
                return Action::Render;
            }
            Key::Char('i')
                if self.active_record_mut().focus == TranscriptFocus::Viewport
                    && !self.active_record_mut().terminal =>
            {
                self.active_record_mut().focus = TranscriptFocus::Composer;
                self.execution_selection = None;
                return Action::Render;
            }
            Key::Char('x')
                if self.active_record_mut().focus == TranscriptFocus::Viewport
                    && !self.active_record_mut().terminal =>
            {
                if let TranscriptId::Subagent(id) = self.active_transcript {
                    return Action::CancelExecution(id);
                }
            }
            Key::Home
                if self.active_transcript != TranscriptId::Main
                    || self.active_record_mut().focus == TranscriptFocus::Viewport =>
            {
                self.scroll_to_start();
                return Action::Render;
            }
            Key::End
                if self.active_transcript != TranscriptId::Main
                    || self.active_record_mut().focus == TranscriptFocus::Viewport =>
            {
                self.scroll_to_end();
                return Action::Render;
            }
            _ => {}
        }

        if self.active_transcript != TranscriptId::Main
            && self.active_record_mut().terminal
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
            ) || (key == Key::Tab && self.palette_open))
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

        if key == Key::CtrlShiftM {
            self.palette_open = false;
            return Action::OpenDialog("subagent-profiles".into());
        }

        if key == Key::CtrlShiftD {
            self.palette_open = false;
            return Action::OpenDialog("dangerous".into());
        }

        if key == Key::CtrlShiftP {
            self.palette_open = false;
            return Action::OpenDialog("bypass".into());
        }

        if matches!(
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
        ) {
            self.execution_selection = None;
        }

        if let Some(action) = self.handle_composer_key(key) {
            return action;
        }

        match key {
            Key::CtrlO => {
                self.toggle_detail_expansion();
                Action::Render
            }
            Key::CtrlJ => {
                self.scroll_down(3);
                Action::Render
            }
            Key::CtrlK => {
                self.scroll_up(3);
                Action::Render
            }
            Key::CtrlG => {
                self.scroll_to_start();
                Action::Render
            }
            Key::CtrlShiftG => {
                self.scroll_to_end();
                Action::Render
            }
            Key::CtrlN => {
                self.jump_to_user_message(false);
                Action::Render
            }
            Key::CtrlShiftN => {
                self.jump_to_user_message(true);
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
                self.scroll_up(MOUSE_SCROLL_ROWS);
                Action::Render
            }
            Key::ScrollDown => {
                self.scroll_down(MOUSE_SCROLL_ROWS);
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
            Key::Up | Key::Down if self.file_picker_open() => {
                self.move_file_picker_selection(key == Key::Down);
                Action::Render
            }
            Key::Tab | Key::Enter if self.file_picker_open() => {
                self.complete_file_picker_selection();
                Action::Render
            }
            Key::Up | Key::Down | Key::Tab => Action::Render,
            Key::Enter if self.input.is_empty() || self.session_loading => Action::Render,
            Key::Enter if self.active_transcript != TranscriptId::Main => {
                let TranscriptId::Subagent(id) = self.active_transcript else {
                    unreachable!("non-main transcript is a subagent");
                };
                self.input_cursor = 0;
                self.active_record_mut().focus = TranscriptFocus::Viewport;
                Action::SendTaskMessage {
                    id,
                    message: std::mem::take(&mut self.input),
                }
            }
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
                self.recovered_failed_prompt = false;
                Action::Submit(std::mem::take(&mut self.input))
            }
            Key::Escape if self.recovered_failed_prompt => {
                self.input.clear();
                self.input_cursor = 0;
                self.recovered_failed_prompt = false;
                self.active_record_mut().focus = TranscriptFocus::Composer;
                self.status = Some("Recovered prompt discarded.".into());
                Action::Render
            }
            Key::Escape if self.running => self.cancel_running(),
            Key::Escape if self.dialog.is_some() => {
                self.dialog = None;
                Action::Render
            }
            Key::Escape => {
                self.active_record_mut().focus = TranscriptFocus::Viewport;
                Action::Render
            }
            Key::CtrlC => unreachable!("Ctrl+C is handled before focused input"),
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
            Key::Home => self.input_cursor = line_start(&self.input, cursor),
            Key::End => self.input_cursor = line_end(&self.input, cursor),
            Key::Backspace => {}
            _ => return None,
        }

        self.clamp_palette_selection();
        self.refresh_file_picker();
        self.active_record_mut().focus = TranscriptFocus::Composer;
        Some(Action::Render)
    }

    fn handle_background_key(&mut self) -> Action {
        if let Some(selection) = self.execution_selection {
            return match selection {
                TranscriptId::Subagent(id)
                    if self.executions.iter().any(|execution| {
                        execution.id == id
                            && execution.state == TuiExecutionState::ForegroundRunning
                    }) =>
                {
                    Action::TransitionToBackground(id)
                }
                TranscriptId::Main | TranscriptId::Subagent(_) => Action::Render,
            };
        }

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

    fn handle_mouse_wheel_batch(&mut self, directions: &[MouseWheelDirection]) -> Action {
        if self
            .dialog
            .as_ref()
            .is_some_and(|dialog| dialog.interactive)
            || self.secret_entry.is_some()
            || self.device_auth.is_some()
        {
            for direction in directions {
                let key = match direction {
                    MouseWheelDirection::Up => Key::ScrollUp,
                    MouseWheelDirection::Down => Key::ScrollDown,
                };
                let _ = self.handle_key(key);
            }
            return Action::Render;
        }

        let bottom = self.detached_scroll_bottom();
        let record = self.active_record_mut();
        for direction in directions {
            match direction {
                MouseWheelDirection::Up => {
                    let current = if record.following_bottom {
                        bottom
                    } else {
                        record.scroll_offset.min(bottom)
                    };
                    record.following_bottom = false;
                    record.scroll_offset = current.saturating_sub(MOUSE_SCROLL_ROWS);
                }
                MouseWheelDirection::Down => {
                    if record.following_bottom {
                        continue;
                    }
                    record.scroll_offset = record
                        .scroll_offset
                        .saturating_add(MOUSE_SCROLL_ROWS)
                        .min(bottom);
                    record.following_bottom = record.scroll_offset == bottom;
                }
            }
        }
        record.focus = TranscriptFocus::Viewport;
        Action::Render
    }

    fn scroll_up(&mut self, rows: u16) {
        let bottom = self.detached_scroll_bottom();
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
        let bottom = self.detached_scroll_bottom();
        let record = self.active_record_mut();
        if record.following_bottom {
            record.focus = TranscriptFocus::Viewport;
            return;
        }
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
        let scroll_offset = self.following_scroll_bottom();
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
        if self.following_bottom() {
            self.following_scroll_bottom()
        } else {
            self.detached_scroll_bottom()
        }
    }

    fn following_scroll_bottom(&self) -> u16 {
        self.max_scroll_offset_with_chrome(TRANSCRIPT_TOP_BORDER_ROWS)
    }

    fn detached_scroll_bottom(&self) -> u16 {
        self.max_scroll_offset_with_chrome(TRANSCRIPT_TOP_BORDER_ROWS + 1)
    }

    fn max_scroll_offset_with_chrome(&self, chrome_rows: u16) -> u16 {
        let layout = self.screen_layout();
        let view = self.view();
        let visible_rows = usize::from(layout.transcript.height.saturating_sub(chrome_rows));
        let row_width = layout
            .transcript
            .width
            .saturating_sub(TRANSCRIPT_ROW_INDENT);
        let transcript =
            SelectableTranscript::from_lines(&rendered_transcript(&view, row_width), row_width);
        saturating_u16(transcript.rows.len().saturating_sub(visible_rows))
    }

    /// Rows a page advances, bounded by the rows the transcript actually shows:
    /// its area spends one row on the top rule and one on the scroll indicator,
    /// so a larger step would skip content between consecutive pages.
    fn transcript_page_rows(&self) -> u16 {
        self.screen_layout()
            .transcript
            .height
            .saturating_sub(2)
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
        if !self.running && text == "@" {
            self.open_file_picker();
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
        self.quit_armed_until = None;
        self.turn_state = Some(TurnState::Cancelled);
        Action::Cancel
    }

    fn handle_control_c(&mut self) -> Action {
        let record = self
            .transcripts
            .get(&self.active_transcript)
            .expect("active transcript always exists");
        if let Some(text) = record.selection_text.clone() {
            self.quit_armed_until = None;
            return Action::CopySelection(text);
        }
        if record.selection_too_large {
            self.quit_armed_until = None;
            self.status = Some("Selection exceeds the 64 KiB copy limit.".into());
            return Action::Render;
        }

        if self.quit_is_armed() {
            self.quit_armed_until = None;
            if self.running || self.has_active_execution() {
                self.engine.cancel();
                self.turn_state = Some(TurnState::Cancelled);
            }
            return Action::Quit;
        }

        self.quit_armed_until = Some(self.now.saturating_add(EXIT_WARNING_WINDOW));
        Action::Render
    }

    fn quit_is_armed(&self) -> bool {
        self.quit_armed_until.is_some_and(|until| self.now < until)
    }

    fn has_active_execution(&self) -> bool {
        self.executions.iter().any(|execution| {
            matches!(
                execution.state,
                TuiExecutionState::ForegroundRunning | TuiExecutionState::BackgroundRunning
            )
        })
    }

    /// Confirm short keys a/d/A/D map to permission answers before query append.
    fn try_confirm_shortcut(&mut self, character: char) -> Option<Action> {
        let dialog = self.dialog.as_ref()?;
        if dialog.overlay_kind != widgets::OverlayKind::Confirm {
            return None;
        }
        let answer = widgets::OverlayShell::confirm_answer(character)?;
        let matched = dialog
            .entries
            .iter()
            .find_map(|entry| match &entry.action {
                Some(DialogEntryAction::Dispatch(action_id))
                    if widgets::OverlayShell::action_matches_answer(action_id, answer) =>
                {
                    Some(Action::DialogAction(action_id.clone()))
                }
                Some(DialogEntryAction::SafeDispatch(action_id))
                    if widgets::OverlayShell::action_matches_answer(action_id, answer) =>
                {
                    Some(Action::SafeDialogAction(action_id.clone()))
                }
                _ => None,
            })?;
        if let Action::DialogAction(action_id) = &matched {
            self.dismiss_permission_dialog(action_id);
        }
        Some(matched)
    }

    fn dismiss_permission_dialog(&mut self, action_id: &str) {
        if parse_permission_reply(action_id).is_some() {
            self.dialog = None;
        }
    }

    fn selected_key_dialog_action(&mut self, key: Key) -> Option<Action> {
        let template = self
            .dialog
            .as_ref()?
            .selected_key_actions
            .iter()
            .find(|(candidate, _)| *candidate == key)
            .map(|(_, template)| template.clone())?;
        let id = self.dialog.as_ref().and_then(|dialog| {
            dialog_matches(dialog)
                .into_iter()
                .find(|(index, _)| *index == dialog.selected)
                .and_then(|(_, entry)| entry.id.clone())
        })?;
        Some(Action::SafeDialogAction(
            template.replace("{selected}", id.as_str()),
        ))
    }

    fn handle_selection_dialog_key(&mut self, key: Key) -> Action {
        match key {
            Key::CtrlO => {
                if let Some(dialog) = self.dialog.as_mut()
                    && dialog
                        .entries
                        .get(dialog.selected)
                        .is_some_and(|entry| entry.selected_detail.is_some())
                {
                    dialog.details_open = !dialog.details_open;
                }
                Action::Render
            }
            Key::LineStart
                if self
                    .dialog
                    .as_ref()
                    .is_some_and(|dialog| dialog.session_entries.is_some()) =>
            {
                self.toggle_session_dialog_scope()
            }
            Key::Char(character) => {
                if self.dialog_is_searching() {
                    return self.edit_dialog_query(|query| {
                        if query.chars().count() < 128 {
                            query.push(character);
                        }
                    });
                }
                self.dialog_character_binding(character)
            }
            Key::Backspace => {
                if !self.dialog_is_searching() {
                    return self
                        .selected_key_dialog_action(Key::Backspace)
                        .unwrap_or(Action::Render);
                }
                self.edit_dialog_query(|query| {
                    query.pop();
                })
            }
            Key::DeletePreviousWord if self.dialog_is_searching() => {
                self.edit_dialog_query(|query| {
                    let boundary = previous_word_boundary(query, query.chars().count());
                    query.truncate(byte_index(query, boundary));
                })
            }
            key @ (Key::Left | Key::Right | Key::Tab) => self
                .selected_key_dialog_action(key)
                .unwrap_or(Action::Render),
            Key::Up | Key::Down | Key::ScrollUp | Key::ScrollDown => {
                self.move_dialog_selection(key, 1, true);
                Action::Render
            }
            Key::PageUp | Key::PageDown => {
                if self
                    .dialog
                    .as_ref()
                    .is_some_and(|dialog| dialog.session_entries.is_some())
                {
                    return self.change_session_dialog_page(key);
                }
                self.move_dialog_selection(key, self.dialog_page_rows(), false);
                Action::Render
            }
            Key::Enter => {
                if self.session_loading {
                    return Action::Render;
                }
                let action = self.dialog.as_ref().and_then(|dialog| {
                    dialog_matches(dialog)
                        .into_iter()
                        .find(|(index, _)| *index == dialog.selected)
                        .and_then(|(_, entry)| entry.action.clone())
                });
                match action {
                    Some(DialogEntryAction::Dispatch(action_id)) => {
                        self.dismiss_permission_dialog(&action_id);
                        Action::DialogAction(action_id)
                    }
                    Some(DialogEntryAction::SafeDispatch(action_id)) => {
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
            // Escape leaves search first, except while a session page is in
            // flight: there it keeps its older job of cancelling that route.
            Key::Escape if self.dialog_is_searching() && !self.session_request_loading() => {
                self.disarm_dialog_search()
            }
            Key::Escape | Key::CtrlC => {
                let action_id = self
                    .dialog
                    .as_ref()
                    .and_then(|dialog| dialog.cancellation_action.clone());
                let session_request_loading = self.session_request_loading();
                if action_id.is_none() || session_request_loading {
                    self.dialog = None;
                }
                if session_request_loading {
                    Action::CancelRoute
                } else {
                    action_id.map_or(Action::Render, Action::SafeDialogAction)
                }
            }
            _ => Action::Render,
        }
    }

    fn dialog_is_searching(&self) -> bool {
        self.dialog.as_ref().is_some_and(|dialog| dialog.searching)
    }

    fn session_request_loading(&self) -> bool {
        self.dialog.as_ref().is_some_and(|dialog| {
            dialog
                .session_entries
                .as_ref()
                .is_some_and(|entries| entries.loading)
        })
    }

    /// Resolves a character typed outside search mode against the dialog's
    /// bindings, in precedence order: caller-registered shortcuts, Confirm
    /// answers, then the built-in navigation and search keys. An unbound
    /// character is dropped rather than filtering, which is the whole point of
    /// gating search behind [`DIALOG_SEARCH_KEY`].
    fn dialog_character_binding(&mut self, character: char) -> Action {
        if let Some(action_id) = self.dialog.as_ref().and_then(|dialog| {
            dialog
                .shortcut_actions
                .iter()
                .find(|(key, _)| *key == character)
                .map(|(_, action)| action.clone())
        }) {
            return Action::DialogAction(action_id);
        }
        if let Some(action) = self.try_confirm_shortcut(character) {
            return action;
        }

        match character {
            // A Confirm has answers, not a list, so it never offers search.
            DIALOG_SEARCH_KEY => {
                if let Some(dialog) = self.dialog.as_mut().filter(|dialog| {
                    dialog.interactive && dialog.overlay_kind != widgets::OverlayKind::Confirm
                }) {
                    dialog.searching = true;
                }
                Action::Render
            }
            'r' => self
                .dialog
                .as_ref()
                .and_then(|dialog| dialog.refresh_id.clone())
                .map_or(Action::Render, Action::OpenDialog),
            'j' => {
                self.move_dialog_selection(Key::Down, 1, true);
                Action::Render
            }
            'k' => {
                self.move_dialog_selection(Key::Up, 1, true);
                Action::Render
            }
            'g' => self.select_dialog_edge(false),
            'G' => self.select_dialog_edge(true),
            _ => Action::Render,
        }
    }

    /// Moves the selection to the first or last selectable match.
    fn select_dialog_edge(&mut self, last: bool) -> Action {
        let Some(dialog) = self.dialog.as_mut() else {
            return Action::Render;
        };
        let enabled = dialog_matches(dialog)
            .into_iter()
            .filter_map(|(index, entry)| entry.action.as_ref().map(|_| index))
            .collect::<Vec<_>>();
        let target = if last {
            enabled.last().copied()
        } else {
            enabled.first().copied()
        };
        let Some(target) = target else {
            return Action::Render;
        };

        dialog.selected = target;
        dialog.details_open = false;
        self.ensure_dialog_selection_visible();
        Action::Render
    }

    /// Applies `edit` to whichever query backs the open dialog.
    ///
    /// The session browser filters server-side, so its query lives in the
    /// request and every edit costs a round trip; every other dialog filters its
    /// own entries in place.
    fn edit_dialog_query(&mut self, edit: impl FnOnce(&mut String)) -> Action {
        if let Some(request) = self.session_dialog_request() {
            let mut request = request.clone();
            edit(&mut request.query);
            request.cursor = None;
            request.previous_cursors.clear();
            request.page = 1;
            return self.start_session_dialog_request(request);
        }

        if let Some(dialog) = self.dialog.as_mut() {
            edit(&mut dialog.query);
            refresh_dialog_query_action(dialog);
        }
        self.reset_dialog_selection();
        Action::Render
    }

    /// Leaves search mode and drops the query, so the list returns to the state
    /// it had before the search band was armed.
    ///
    /// An already-empty query is left untouched: on the session browser
    /// clearing it would cost a round trip for no change.
    fn disarm_dialog_search(&mut self) -> Action {
        let empty = self.dialog.as_ref().is_some_and(|dialog| {
            dialog
                .session_entries
                .as_ref()
                .map_or(dialog.query.is_empty(), |entries| {
                    entries.request.query.is_empty()
                })
        });
        let action = if empty {
            Action::Render
        } else {
            self.edit_dialog_query(String::clear)
        };

        if let Some(dialog) = self.dialog.as_mut() {
            dialog.searching = false;
        }
        action
    }

    fn session_dialog_request(&self) -> Option<&SessionDialogRequest> {
        self.dialog
            .as_ref()?
            .session_entries
            .as_ref()
            .map(|entries| &entries.request)
    }

    fn start_session_dialog_request(&mut self, mut request: SessionDialogRequest) -> Action {
        let generation = self
            .session_dialog_request()
            .map_or(1, |current| current.generation.wrapping_add(1).max(1));
        request.generation = generation;

        let mut dialog = DialogView::sessions_loading(request.clone());
        dialog.searching = self.dialog_is_searching();
        self.dialog = Some(dialog);
        Action::LoadSessionPage(request)
    }

    fn toggle_session_dialog_scope(&mut self) -> Action {
        let Some(request) = self.session_dialog_request() else {
            return Action::Render;
        };
        let mut request = request.clone();
        request.scope = match request.scope {
            SessionDialogScope::CurrentProject => SessionDialogScope::AllProjects,
            SessionDialogScope::AllProjects => SessionDialogScope::CurrentProject,
        };
        request.cursor = None;
        request.previous_cursors.clear();
        request.page = 1;
        self.start_session_dialog_request(request)
    }

    fn change_session_dialog_page(&mut self, key: Key) -> Action {
        let Some(entries) = self
            .dialog
            .as_ref()
            .and_then(|dialog| dialog.session_entries.as_ref())
        else {
            return Action::Render;
        };
        let mut request = entries.request.clone();
        match key {
            Key::PageDown => {
                let Some(next_cursor) = entries.next_cursor else {
                    return Action::Render;
                };
                request.previous_cursors.push(request.cursor);
                request.cursor = Some(next_cursor);
                request.page = request.page.saturating_add(1);
            }
            Key::PageUp => {
                let Some(previous_cursor) = request.previous_cursors.pop() else {
                    return Action::Render;
                };
                request.cursor = previous_cursor;
                request.page = request.page.saturating_sub(1).max(1);
            }
            _ => return Action::Render,
        }
        self.start_session_dialog_request(request)
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
        dialog_visible_rows(area, dialog)
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

    /// Opens the picker when `@` starts a fresh reference token in the composer.
    fn open_file_picker(&mut self) {
        if self.palette_open || self.file_candidates.is_empty() || self.input_cursor == 0 {
            return;
        }
        let anchor = self.input_cursor - 1;
        let before = byte_index(&self.input, anchor);
        if anchor > 0
            && !self.input[..before]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace)
        {
            return;
        }
        self.file_picker = Some(FilePicker {
            anchor,
            selected: 0,
        });
    }

    /// The token typed after the `@`, or `None` once the reference no longer holds.
    fn file_picker_query(&self) -> Option<&str> {
        let picker = self.file_picker.as_ref()?;
        if self.input_cursor <= picker.anchor {
            return None;
        }
        let at = byte_index(&self.input, picker.anchor);
        if !self.input[at..].starts_with('@') {
            return None;
        }
        let query = self.input.get(
            byte_index(&self.input, picker.anchor + 1)..byte_index(&self.input, self.input_cursor),
        )?;
        if query.chars().any(char::is_whitespace) {
            return None;
        }
        Some(query)
    }

    /// Whether the picker holds the overlay layer, judged by the live token so a
    /// stale anchor left behind by a submission never answers a key.
    fn file_picker_open(&self) -> bool {
        self.file_picker_query().is_some()
    }

    fn file_picker_match_count(&self) -> usize {
        self.file_picker_query().map_or(0, |query| {
            file_picker_matches(&self.file_candidates, query).len()
        })
    }

    /// Closes the picker once its token dissolves and keeps the selection in range.
    fn refresh_file_picker(&mut self) {
        if self.file_picker.is_none() {
            return;
        }
        if self.file_picker_query().is_none() {
            self.file_picker = None;
            return;
        }
        let count = self.file_picker_match_count();
        if let Some(picker) = self.file_picker.as_mut() {
            picker.selected = picker.selected.min(count.saturating_sub(1));
        }
    }

    fn move_file_picker_selection(&mut self, forward: bool) {
        let count = self.file_picker_match_count();
        if count == 0 {
            return;
        }
        if let Some(picker) = self.file_picker.as_mut() {
            let step = if forward { 1 } else { count - 1 };
            picker.selected = (picker.selected + step) % count;
        }
    }

    /// Replaces the typed token with the selected project-relative path.
    fn complete_file_picker_selection(&mut self) {
        let Some(query) = self.file_picker_query().map(str::to_owned) else {
            self.file_picker = None;
            return;
        };
        let Some(picker) = self.file_picker else {
            return;
        };
        let selected = file_picker_matches(&self.file_candidates, &query)
            .get(picker.selected)
            .map(|path| (*path).to_owned());
        self.file_picker = None;
        if let Some(path) = selected {
            self.replace_chars(picker.anchor + 1, self.input_cursor, &path);
        }
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

    /// Shared Ctrl+O detail path: expand/collapse finished thinking first, else tool outputs.
    fn toggle_detail_expansion(&mut self) {
        if self.has_finished_thinking() {
            let thinking_collapsed = self
                .transcripts
                .get(&self.active_transcript)
                .expect("active transcript always exists")
                .collapse_thinking;
            if thinking_collapsed {
                self.toggle_thinking_expansion();
                return;
            }

            let has_completed_tools = self.completed_tool_call_ids().next().is_some();
            if !has_completed_tools {
                self.toggle_thinking_expansion();
                return;
            }
        }
        self.toggle_tool_output_expansion();
    }

    fn completed_tool_call_ids(&self) -> impl Iterator<Item = String> + '_ {
        let active = self
            .transcripts
            .get(&self.active_transcript)
            .expect("active transcript always exists");
        let (completed, conversation): (&[Conversation], Option<&Conversation>) =
            if self.active_transcript == TranscriptId::Main {
                (
                    self.completed_conversations.as_slice(),
                    self.conversation.as_ref(),
                )
            } else {
                (
                    active.completed_conversations.as_slice(),
                    active.conversation.as_ref(),
                )
            };
        completed
            .iter()
            .chain(conversation)
            .flat_map(|conversation| &conversation.tool_batches)
            .flat_map(|batch| &batch.calls)
            .filter(|call| call.result.is_some())
            .map(|call| call.call_id.clone())
    }

    fn has_finished_thinking(&self) -> bool {
        if self.running {
            return false;
        }
        let active = self
            .transcripts
            .get(&self.active_transcript)
            .expect("active transcript always exists");
        let (completed, conversation) = if self.active_transcript == TranscriptId::Main {
            (
                self.completed_conversations.as_slice(),
                self.conversation.as_ref(),
            )
        } else {
            (
                active.completed_conversations.as_slice(),
                active.conversation.as_ref(),
            )
        };
        completed
            .iter()
            .chain(conversation)
            .any(|conversation| !conversation.reasoning.is_empty())
    }

    fn toggle_thinking_expansion(&mut self) {
        let record = self.active_record_mut();
        let current = widgets::ExpandableBody::new(if record.collapse_thinking {
            widgets::ExpandMode::Collapsed
        } else {
            widgets::ExpandMode::Expanded
        });
        let next = current.toggle_detail();
        record.collapse_thinking = !next.is_visible();
        record.thinking_user_pinned = next.is_visible();
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

        // All completed calls always advance together, so they share one
        // current mode; sampling the first call's mode (or the shared
        // fallback for a call cleared by a new submission) keeps the whole
        // group synchronized on every press. The fallback matches
        // a finished `ToolCallBlock::default_mode()` so the sampled state
        // agrees with what is actually on screen.
        let modes = &mut self.active_record_mut().tool_display_modes;
        let current = completed_call_ids
            .first()
            .and_then(|call_id| modes.get(call_id).copied())
            .unwrap_or(widgets::DisplayMode::Expanded);
        let next = current.next();
        for call_id in completed_call_ids {
            modes.insert(call_id, next);
        }
    }

    fn jump_to_user_message(&mut self, previous: bool) {
        let layout = self.screen_layout();
        let row_width = layout
            .transcript
            .width
            .saturating_sub(TRANSCRIPT_ROW_INDENT)
            .max(1);
        let lines = rendered_transcript(&self.view(), row_width);
        let mut user_offsets = Vec::new();
        let mut row = 0usize;
        for line in &lines {
            let text: String = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            if text.trim_start().starts_with('❯') {
                user_offsets.push(saturating_u16(row));
            }
            row += line.width().div_ceil(usize::from(row_width)).max(1);
        }
        if user_offsets.is_empty() {
            return;
        }

        let current = {
            let record = self
                .transcripts
                .get(&self.active_transcript)
                .expect("active transcript always exists");
            if record.following_bottom {
                self.detached_scroll_bottom()
            } else {
                record.scroll_offset
            }
        };

        let target = if previous {
            user_offsets
                .iter()
                .rev()
                .find(|offset| **offset < current)
                .copied()
                .or_else(|| user_offsets.first().copied())
        } else {
            user_offsets.last().copied()
        };

        if let Some(offset) = target {
            let bottom = self.detached_scroll_bottom();
            let record = self.active_record_mut();
            record.following_bottom = false;
            record.scroll_offset = offset.min(bottom);
            record.focus = TranscriptFocus::Viewport;
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

    /// Applies composition-owned model, effort, context, session, and safety presentation state.
    pub fn apply_presentation(&mut self, presentation: TuiPresentation) {
        self.set_presentation(
            presentation.provider,
            presentation.model,
            presentation.session,
        );
        self.set_reasoning_effort(presentation.effort);
        self.context_window = presentation.context_window;
        self.set_dangerous_mode(presentation.dangerous_mode);
        self.set_bypass(presentation.bypass);
    }
}

fn auto_turn_subject(finished: usize) -> String {
    if finished == 1 {
        "1 background subagent".to_owned()
    } else {
        format!("{finished} background subagents")
    }
}

/// Opening text for a runtime-scheduled turn.
///
/// It is not a user message and must never read like one: the completion notices themselves
/// arrive as bounded coordination messages, so this only states who scheduled the turn and where
/// the recorded outcome lives.
fn auto_turn_prompt(finished: usize) -> String {
    format!(
        "[coordination source=runtime untrusted=false]\n{} finished. The completion notices \
         accompany this turn unless an earlier turn already delivered them, and \
         `task_control action=status` returns a recorded outcome. The runtime scheduled this \
         turn; the user did not send it.",
        auto_turn_subject(finished)
    )
}

fn auto_turn_notice(finished: usize) -> String {
    format!(
        "Continuing automatically: {} finished.",
        auto_turn_subject(finished)
    )
}

fn status_matches_execution(status: SubagentStatus, state: TuiExecutionState) -> bool {
    matches!(
        (status, state),
        (SubagentStatus::Success, TuiExecutionState::CompletedRecent)
            | (SubagentStatus::Failure, TuiExecutionState::Failed)
            | (SubagentStatus::Cancelled, TuiExecutionState::Cancelled)
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

/// Case-insensitive substring match over the project-relative path.
fn file_picker_matches<'a>(candidates: &'a [String], query: &str) -> Vec<&'a str> {
    if query.is_empty() {
        return candidates.iter().map(String::as_str).collect();
    }
    let query = query.to_lowercase();
    candidates
        .iter()
        .filter(|candidate| candidate.to_lowercase().contains(&query))
        .map(String::as_str)
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

    fn copy_selection(&mut self, text: &str) -> io::Result<()> {
        self.control
            .stdout
            .write_all(osc52_copy_sequence(text).as_bytes())?;
        self.control.stdout.flush()
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
            TerminalOperation::HideCursor => execute!(self.stdout, HideCursor).map(|_| ()),
            TerminalOperation::ShowCursor => execute!(self.stdout, ShowCursor).map(|_| ()),
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
    fn copy_selection(&mut self, _text: &str) -> io::Result<()> {
        Ok(())
    }
}

impl RuntimeTerminal for Terminal {
    fn poll(&mut self, timeout: Duration) -> io::Result<Option<Event>> {
        Self::poll(self, timeout)
    }

    fn copy_selection(&mut self, text: &str) -> io::Result<()> {
        Self::copy_selection(self, text)
    }
}

fn osc52_copy_sequence(text: &str) -> String {
    format!("\u{1b}]52;c;{}\u{7}", BASE64_STANDARD.encode(text))
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

fn wheel_burst<T: RuntimeTerminal>(
    terminal: &mut T,
    first: MouseWheelDirection,
) -> io::Result<(Vec<MouseWheelDirection>, Option<Event>)> {
    let mut directions = vec![first];
    while directions.len() < TERMINAL_WHEEL_BATCH_BUDGET {
        match terminal.poll(Duration::ZERO)? {
            Some(Event::MouseWheel(direction)) => directions.push(direction),
            Some(event) => return Ok((directions, Some(event))),
            None => break,
        }
    }
    Ok((directions, None))
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
    let started = Instant::now();
    let mut pending_event = None;
    renderer.render(tui.view())?;

    loop {
        let quit_armed = tui.quit_is_armed();
        let status = tui.status.clone();
        tui.tick(started.elapsed());
        if quit_armed && !tui.quit_is_armed() || status != tui.status {
            renderer.render(tui.view())?;
        }
        let event = if let Some(event) = pending_event.take() {
            event
        } else {
            let Some(event) = terminal.poll(Duration::from_millis(100))? else {
                continue;
            };
            event
        };

        let action = if let Event::MouseWheel(direction) = event {
            let (directions, deferred) = wheel_burst(&mut terminal, direction)?;
            pending_event = deferred;
            tui.handle_mouse_wheel_batch(&directions)
        } else {
            tui.handle(event)
        };
        match action {
            Action::Quit => return Ok(()),
            Action::CopySelection(text) => {
                terminal.copy_selection(&text)?;
                renderer.render(tui.view())?;
            }
            Action::Render
            | Action::Submit(_)
            | Action::SubmitSecret { .. }
            | Action::SubmitBackground(_)
            | Action::TransitionToBackground(_)
            | Action::CancelExecution(_)
            | Action::SendTaskMessage { .. }
            | Action::OpenDialog(_)
            | Action::LoadSessionPage(_)
            | Action::DialogAction(_)
            | Action::SafeDialogAction(_)
            | Action::OpenDeviceAuthUrl
            | Action::CopyDeviceAuthUrl
            | Action::CopyDeviceAuthCode
            | Action::Cancel
            | Action::CancelRoute => renderer.render(tui.view())?,
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
    let started = Instant::now();
    let mut pending_event = None;

    loop {
        let quit_armed = tui.quit_is_armed();
        let status = tui.status.clone();
        tui.tick(started.elapsed());
        if quit_armed && !tui.quit_is_armed() || status != tui.status {
            renderer.render(tui.view())?;
        }
        while let Ok(result) = receiver.try_recv() {
            tui.finish_submission(result);
            renderer.render(tui.view())?;
        }

        let event = if let Some(event) = pending_event.take() {
            event
        } else {
            let Some(event) = terminal.poll(Duration::from_millis(100))? else {
                continue;
            };
            event
        };

        let action = if let Event::MouseWheel(direction) = event {
            let (directions, deferred) = wheel_burst(&mut terminal, direction)?;
            pending_event = deferred;
            tui.handle_mouse_wheel_batch(&directions)
        } else {
            tui.handle(event)
        };
        match action {
            Action::Quit => return Ok(()),
            Action::CopySelection(text) => {
                terminal.copy_selection(&text)?;
                renderer.render(tui.view())?;
            }
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
            | Action::SubmitSecret { .. }
            | Action::SubmitBackground(_)
            | Action::TransitionToBackground(_)
            | Action::CancelExecution(_)
            | Action::SendTaskMessage { .. }
            | Action::OpenDialog(_)
            | Action::LoadSessionPage(_)
            | Action::DialogAction(_)
            | Action::SafeDialogAction(_)
            | Action::OpenDeviceAuthUrl
            | Action::CopyDeviceAuthUrl
            | Action::CopyDeviceAuthCode
            | Action::Cancel
            | Action::CancelRoute => renderer.render(tui.view())?,
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct FrameSchedule {
    last_render: Option<Duration>,
}

impl FrameSchedule {
    fn heartbeat_due(self, now: Duration, running: bool) -> bool {
        running
            && self
                .last_render
                .is_none_or(|last| now.saturating_sub(last) >= ACTIVE_FRAME_HEARTBEAT)
    }

    fn mark_rendered(&mut self, now: Duration) {
        self.last_render = Some(now);
    }

    fn poll_timeout(self, now: Duration, running: bool, backlog: bool) -> Duration {
        if backlog {
            return Duration::ZERO;
        }
        if !running {
            return TERMINAL_POLL_INTERVAL;
        }

        let heartbeat_wait = self.last_render.map_or(Duration::ZERO, |last| {
            ACTIVE_FRAME_HEARTBEAT.saturating_sub(now.saturating_sub(last))
        });
        TERMINAL_POLL_INTERVAL.min(heartbeat_wait)
    }
}

fn render_progress_frame<E, R>(
    tui: &mut Tui<E>,
    renderer: &mut R,
    schedule: &mut FrameSchedule,
    now: Duration,
    dirty: bool,
) -> io::Result<bool>
where
    E: Engine,
    R: Renderer,
{
    let execution_count = tui.executions.len();
    let quit_armed = tui.quit_is_armed();
    let restored_syntax_ready = tui.highlight_restored_syntax;
    let status = tui.status.clone();
    tui.tick(now);
    let expired_execution = tui.executions.len() != execution_count;
    let expired_quit_warning = quit_armed && !tui.quit_is_armed();
    let restored_syntax_became_ready = !restored_syntax_ready && tui.highlight_restored_syntax;
    let status_changed = status != tui.status;
    if !dirty
        && !expired_execution
        && !expired_quit_warning
        && !restored_syntax_became_ready
        && !status_changed
        && !schedule.heartbeat_due(now, tui.running)
    {
        return Ok(false);
    }

    renderer.render(tui.view())?;
    schedule.mark_rendered(now);
    Ok(true)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ChannelDrain {
    processed: usize,
    caught_up: bool,
}

impl ChannelDrain {
    fn dirty(self) -> bool {
        self.processed > 0
    }

    fn backlog(self) -> bool {
        !self.caught_up
    }
}

fn drain_channel<T>(receiver: &mpsc::Receiver<T>, mut apply: impl FnMut(T)) -> ChannelDrain {
    let mut processed = 0;
    while processed < PROGRESS_CHANNEL_BUDGET {
        match receiver.try_recv() {
            Ok(value) => {
                apply(value);
                processed += 1;
            }
            Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => {
                return ChannelDrain {
                    processed,
                    caught_up: true,
                };
            }
        }
    }

    ChannelDrain {
        processed,
        caught_up: false,
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ProviderDrain {
    dirty: bool,
    backlog: bool,
}

fn drain_provider_channels<E: Engine>(
    tui: &mut Tui<E>,
    metrics_receiver: &mpsc::Receiver<UiEnvelope<TuiRuntimeEvent>>,
    progress_receiver: &mpsc::Receiver<TurnEvent>,
    completion_receiver: &mpsc::Receiver<TuiProviderOutcome>,
) -> ProviderDrain {
    let progress = drain_channel(progress_receiver, |event| tui.apply_progress(event));
    let metrics = if progress.caught_up {
        drain_channel(metrics_receiver, |envelope| {
            let (ordinal, event) = envelope.into_parts();
            tui.apply_runtime_event_with_ordinal(ordinal, event);
        })
    } else {
        ChannelDrain::default()
    };
    let completion = if metrics.caught_up && progress.caught_up {
        drain_channel(completion_receiver, |outcome| {
            tui.finish_provider_turn(outcome)
        })
    } else {
        ChannelDrain::default()
    };

    ProviderDrain {
        dirty: progress.dirty() || metrics.dirty() || completion.dirty(),
        backlog: progress.backlog()
            || metrics.backlog()
            || completion.backlog()
            || !(metrics.caught_up && progress.caught_up),
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
        SubmitOrigin,
        mpsc::Sender<TurnEvent>,
        BridgeTx<TuiRuntimeEvent>,
    ) -> TuiProviderOutcome
    + Send
    + Sync
    + 'static,
) -> io::Result<()> {
    run_with_default_progress_submit_with_permissions(
        tui,
        move |request, progress, _| route(request, progress),
        submit,
        |_| false,
        None,
    )
}

pub fn run_with_default_progress_submit_with_permissions<E, R, F, B>(
    tui: &mut Tui<E>,
    route: R,
    submit: F,
    transition: B,
    permissions: Option<(TuiPermissionBridge, mpsc::Receiver<TuiPermissionRequest>)>,
) -> io::Result<()>
where
    E: Engine + Send,
    R: Fn(
            TuiRouteRequest,
            mpsc::Sender<TuiRouteProgress>,
            TuiRouteCancellation,
        ) -> TuiSubmissionOutcome
        + Send
        + Sync
        + 'static,
    F: Fn(
            String,
            SubmitOrigin,
            mpsc::Sender<TurnEvent>,
            BridgeTx<TuiRuntimeEvent>,
        ) -> TuiProviderOutcome
        + Send
        + Sync
        + 'static,
    B: Fn(u64) -> bool + Send + Sync + 'static,
{
    run_with_default_progress_submit_with_permissions_and_task_controls(
        tui,
        route,
        submit,
        transition,
        |_| false,
        |_, _| false,
        permissions,
    )
}

pub fn run_with_default_progress_submit_with_permissions_and_task_controls<E, R, F, B, C, M>(
    tui: &mut Tui<E>,
    route: R,
    submit: F,
    transition: B,
    cancel_execution: C,
    send_task_message: M,
    permissions: Option<(TuiPermissionBridge, mpsc::Receiver<TuiPermissionRequest>)>,
) -> io::Result<()>
where
    E: Engine + Send,
    R: Fn(
            TuiRouteRequest,
            mpsc::Sender<TuiRouteProgress>,
            TuiRouteCancellation,
        ) -> TuiSubmissionOutcome
        + Send
        + Sync
        + 'static,
    F: Fn(
            String,
            SubmitOrigin,
            mpsc::Sender<TurnEvent>,
            BridgeTx<TuiRuntimeEvent>,
        ) -> TuiProviderOutcome
        + Send
        + Sync
        + 'static,
    B: Fn(u64) -> bool + Send + Sync + 'static,
    C: Fn(u64) -> bool + Send + Sync + 'static,
    M: Fn(u64, String) -> bool + Send + Sync + 'static,
{
    let route = Arc::new(route);
    let submit = Arc::new(submit);
    let transition = Arc::new(transition);
    let cancel_execution = Arc::new(cancel_execution);
    let send_task_message = Arc::new(send_task_message);
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
    let started = Instant::now();
    let mut frame_schedule = FrameSchedule::default();
    let mut render_requested = true;
    let mut pending_event = None;
    let mut next_route_id = 0_u64;
    let mut active_route: Option<(u64, TuiRouteCancellation, bool)> = None;

    loop {
        let now = started.elapsed();
        let provider =
            drain_provider_channels(tui, &metrics_receiver, &receiver, &completion_receiver);
        let mut dirty = std::mem::take(&mut render_requested) || provider.dirty;
        let mut backlog = provider.backlog;
        let route_progress = drain_channel(&route_progress_receiver, |progress| {
            tui.apply_route_progress(progress)
        });
        dirty |= route_progress.dirty();
        backlog |= route_progress.backlog();
        let mut should_quit = false;
        let routes = if route_progress.caught_up {
            drain_channel(&route_receiver, |(route_id, outcome)| {
                if should_quit {
                    return;
                }
                let Some((active_id, cancellation, session_load)) = active_route.as_ref() else {
                    return;
                };
                if *active_id != route_id || cancellation.is_cancelled() {
                    return;
                }
                let session_load = *session_load;
                active_route = None;
                if session_load {
                    tui.finish_session_load();
                }
                let quit = matches!(outcome, TuiSubmissionOutcome::Quit);
                let Some(prompt) = tui.apply_submission_outcome(outcome) else {
                    if quit {
                        should_quit = true;
                    }
                    return;
                };
                let submit = Arc::clone(&submit);
                let sender = sender.clone();
                let metrics = metrics_sender.clone();
                let completion_sender = completion_sender.clone();
                thread::spawn(move || {
                    let outcome = submit(prompt, SubmitOrigin::User, sender, metrics);
                    let _ = completion_sender.send(outcome);
                });
            })
        } else {
            ChannelDrain::default()
        };
        if should_quit {
            return Ok(());
        }
        dirty |= routes.dirty();
        backlog |= routes.backlog();
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
            tui.show_selection_dialog(
                DialogView::selection(
                    "Permission required",
                    Some(format!("{tool}\n{target}")),
                    entries,
                )
                .as_confirm(),
            );
            dirty = true;
        }
        if active_route.is_none()
            && let Some(prompt) = tui.take_ready_auto_turn()
        {
            let submit = Arc::clone(&submit);
            let sender = sender.clone();
            let metrics = metrics_sender.clone();
            let completion_sender = completion_sender.clone();
            thread::spawn(move || {
                let outcome = submit(prompt, SubmitOrigin::SubagentCompletion, sender, metrics);
                let _ = completion_sender.send(outcome);
            });
            dirty = true;
        }
        render_progress_frame(tui, &mut renderer, &mut frame_schedule, now, dirty)?;
        let timeout = frame_schedule.poll_timeout(now, tui.running, backlog);
        let event = if let Some(event) = pending_event.take() {
            event
        } else {
            let Some(event) = runtime_terminal.poll(timeout)? else {
                continue;
            };
            event
        };
        let cancel_permission = matches!(event, Event::Key(Key::Escape));
        let action = if let Event::MouseWheel(direction) = event {
            let (directions, deferred) = wheel_burst(&mut runtime_terminal, direction)?;
            pending_event = deferred;
            tui.handle_mouse_wheel_batch(&directions)
        } else {
            tui.handle(event)
        };
        match action {
            Action::Quit => {
                if let Some((_, cancellation, session_load)) = active_route.take()
                    && cancellation.cancel()
                    && session_load
                {
                    tui.cancel_session_load();
                }
                return Ok(());
            }
            Action::Submit(prompt) => {
                let request = TuiRouteRequest::Input(prompt);
                let session_load = is_session_resume_request(&request);
                if session_load {
                    if !tui.begin_session_load() {
                        continue;
                    }
                } else if is_session_browser_request(&request) {
                    tui.show_selection_dialog(DialogView::sessions_loading(
                        SessionDialogRequest::initial(),
                    ));
                } else {
                    tui.begin_route();
                }
                next_route_id = next_route_id.wrapping_add(1).max(1);
                let route_id = next_route_id;
                let cancellation = TuiRouteCancellation::new();
                active_route = Some((route_id, cancellation.clone(), session_load));
                let route = Arc::clone(&route);
                let route_sender = route_sender.clone();
                let progress = route_progress_sender.clone();
                thread::spawn(move || {
                    let outcome = route(request, progress, cancellation);
                    let _ = route_sender.send((route_id, outcome));
                });
            }
            Action::SubmitSecret { action_id, secret } => {
                let request = TuiRouteRequest::SubmitSecret { action_id, secret };
                tui.begin_route();
                next_route_id = next_route_id.wrapping_add(1).max(1);
                let route_id = next_route_id;
                let cancellation = TuiRouteCancellation::new();
                active_route = Some((route_id, cancellation.clone(), false));
                let route = Arc::clone(&route);
                let route_sender = route_sender.clone();
                let progress = route_progress_sender.clone();
                thread::spawn(move || {
                    let outcome = route(request, progress, cancellation);
                    let _ = route_sender.send((route_id, outcome));
                });
            }
            Action::SubmitBackground(prompt) => {
                let submit = Arc::clone(&submit);
                let sender = sender.clone();
                let metrics = metrics_sender.clone();
                let completion_sender = completion_sender.clone();
                thread::spawn(move || {
                    let outcome = submit(prompt, SubmitOrigin::Background, sender, metrics);
                    let _ = completion_sender.send(outcome);
                });
            }
            Action::TransitionToBackground(id) => {
                let _ = transition(id);
            }
            Action::CancelExecution(id) => {
                let _ = cancel_execution(id);
            }
            Action::SendTaskMessage { id, message } => {
                let _ = send_task_message(id, message);
            }
            Action::OpenDialog(route_id) => {
                if route_id == "sessions"
                    && let Some((_, cancellation, _)) = active_route.take()
                {
                    cancellation.cancel();
                }
                next_route_id = next_route_id.wrapping_add(1).max(1);
                let active_id = next_route_id;
                let cancellation = TuiRouteCancellation::new();
                active_route = Some((active_id, cancellation.clone(), false));
                if route_id == "sessions" {
                    tui.show_selection_dialog(DialogView::sessions_loading(
                        SessionDialogRequest::initial(),
                    ));
                    let route = Arc::clone(&route);
                    let route_sender = route_sender.clone();
                    let progress = route_progress_sender.clone();
                    thread::spawn(move || {
                        let outcome = route(
                            TuiRouteRequest::OpenDialog(route_id),
                            progress,
                            cancellation,
                        );
                        let _ = route_sender.send((active_id, outcome));
                    });
                } else {
                    let outcome = route(
                        TuiRouteRequest::OpenDialog(route_id),
                        route_progress_sender.clone(),
                        cancellation,
                    );
                    let _ = route_sender.send((active_id, outcome));
                }
            }
            Action::LoadSessionPage(request) => {
                if let Some((_, cancellation, _)) = active_route.take() {
                    cancellation.cancel();
                }
                next_route_id = next_route_id.wrapping_add(1).max(1);
                let active_id = next_route_id;
                let cancellation = TuiRouteCancellation::new();
                active_route = Some((active_id, cancellation.clone(), false));
                let route = Arc::clone(&route);
                let route_sender = route_sender.clone();
                let progress = route_progress_sender.clone();
                thread::spawn(move || {
                    thread::sleep(Duration::from_millis(120));
                    let outcome = if cancellation.is_cancelled() {
                        TuiSubmissionOutcome::RouteCancelled
                    } else {
                        route(
                            TuiRouteRequest::SessionPage(request),
                            progress,
                            cancellation,
                        )
                    };
                    let _ = route_sender.send((active_id, outcome));
                });
            }
            Action::DialogAction(action_id) => {
                if let Some((id, reply)) = parse_permission_reply(&action_id) {
                    if let Some(permission_bridge) = permission_bridge.as_ref() {
                        let _ = permission_bridge.reply(id, reply);
                    }
                    active_permission = None;
                    render_requested = true;
                    continue;
                }
                let request = TuiRouteRequest::DialogAction(action_id);
                let session_load = is_session_resume_request(&request);
                if session_load {
                    if !tui.begin_session_load() {
                        continue;
                    }
                } else {
                    tui.begin_route();
                }
                next_route_id = next_route_id.wrapping_add(1).max(1);
                let route_id = next_route_id;
                let cancellation = TuiRouteCancellation::new();
                active_route = Some((route_id, cancellation.clone(), session_load));
                let route = Arc::clone(&route);
                let route_sender = route_sender.clone();
                let progress = route_progress_sender.clone();
                thread::spawn(move || {
                    let outcome = route(request, progress, cancellation);
                    let _ = route_sender.send((route_id, outcome));
                });
            }
            Action::SafeDialogAction(action_id) => {
                next_route_id = next_route_id.wrapping_add(1).max(1);
                let active_id = next_route_id;
                let cancellation = TuiRouteCancellation::new();
                active_route = Some((active_id, cancellation.clone(), false));
                let outcome = route(
                    TuiRouteRequest::DialogAction(action_id),
                    route_progress_sender.clone(),
                    cancellation,
                );
                let _ = route_sender.send((active_id, outcome));
            }
            Action::CancelRoute => {
                if active_route
                    .as_ref()
                    .is_some_and(|(_, cancellation, _)| cancellation.cancel())
                {
                    active_route = None;
                    tui.cancel_session_load();
                }
            }
            Action::OpenDeviceAuthUrl => {
                let Some(url) = tui.device_auth_verification_url().map(str::to_owned) else {
                    continue;
                };
                let outcome = route(
                    TuiRouteRequest::DeviceAuthOpenUrl(url),
                    route_progress_sender.clone(),
                    TuiRouteCancellation::new(),
                );
                tui.apply_device_auth_open_result(matches!(
                    outcome,
                    TuiSubmissionOutcome::LocalInfo(_)
                ));
            }
            Action::CopyDeviceAuthUrl | Action::CopyDeviceAuthCode => {
                if let Some(text) = tui.device_auth_clipboard_text() {
                    runtime_terminal.copy_selection(text)?;
                }
            }
            Action::CopySelection(text) => {
                runtime_terminal.copy_selection(&text)?;
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
        render_requested = true;
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

fn is_session_resume_action(action_id: &str) -> bool {
    action_id
        .strip_prefix("session:")
        .is_some_and(|identifier| identifier.parse::<i64>().is_ok())
}

fn is_session_resume_request(request: &TuiRouteRequest) -> bool {
    match request {
        TuiRouteRequest::Input(input) => input
            .trim()
            .strip_prefix("/resume ")
            .is_some_and(|identifier| identifier.trim().parse::<i64>().is_ok()),
        TuiRouteRequest::DialogAction(action_id) => is_session_resume_action(action_id),
        TuiRouteRequest::DeviceAuthOpenUrl(_)
        | TuiRouteRequest::SubmitSecret { .. }
        | TuiRouteRequest::OpenDialog(_)
        | TuiRouteRequest::SessionPage(_) => false,
    }
}

fn is_session_browser_request(request: &TuiRouteRequest) -> bool {
    match request {
        TuiRouteRequest::Input(input) => matches!(input.trim(), "/resume" | "/sessions"),
        TuiRouteRequest::DeviceAuthOpenUrl(_) | TuiRouteRequest::SubmitSecret { .. } => false,
        TuiRouteRequest::OpenDialog(route_id) => route_id == "sessions",
        TuiRouteRequest::SessionPage(_) => true,
        TuiRouteRequest::DialogAction(_) => false,
    }
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
            Some(Event::MouseWheel(MouseWheelDirection::Up))
        }
        CrosstermEvent::Mouse(mouse) if mouse.kind == MouseEventKind::ScrollDown => {
            Some(Event::MouseWheel(MouseWheelDirection::Down))
        }
        CrosstermEvent::Mouse(mouse)
            if mouse.kind == MouseEventKind::Down(crossterm::event::MouseButton::Left) =>
        {
            Some(Event::MouseDown {
                column: mouse.column,
                row: mouse.row,
            })
        }
        CrosstermEvent::Mouse(mouse)
            if mouse.kind == MouseEventKind::Drag(crossterm::event::MouseButton::Left) =>
        {
            Some(Event::MouseDrag {
                column: mouse.column,
                row: mouse.row,
            })
        }
        CrosstermEvent::Mouse(mouse)
            if mouse.kind == MouseEventKind::Up(crossterm::event::MouseButton::Left) =>
        {
            Some(Event::MouseUp {
                column: mouse.column,
                row: mouse.row,
            })
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
        (KeyCode::Char('j' | 'J'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            Key::CtrlJ
        }
        (KeyCode::Char('k' | 'K'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            Key::CtrlK
        }
        (KeyCode::Char('g'), modifiers)
            if modifiers == KeyModifiers::CONTROL | KeyModifiers::SHIFT =>
        {
            Key::CtrlShiftG
        }
        (KeyCode::Char('G'), modifiers) if modifiers == KeyModifiers::CONTROL => Key::CtrlShiftG,
        (KeyCode::Char('g' | 'G'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            Key::CtrlG
        }
        (KeyCode::Char('n'), modifiers)
            if modifiers == KeyModifiers::CONTROL | KeyModifiers::SHIFT =>
        {
            Key::CtrlShiftN
        }
        (KeyCode::Char('N'), modifiers) if modifiers == KeyModifiers::CONTROL => Key::CtrlShiftN,
        (KeyCode::Char('n' | 'N'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            Key::CtrlN
        }
        (KeyCode::Char('a'), modifiers)
            if modifiers == KeyModifiers::CONTROL | KeyModifiers::SHIFT =>
        {
            Key::CtrlShiftA
        }
        (KeyCode::Char('A'), modifiers) if modifiers == KeyModifiers::CONTROL => Key::CtrlShiftA,
        (KeyCode::Char('p'), modifiers)
            if modifiers == KeyModifiers::CONTROL | KeyModifiers::SHIFT =>
        {
            Key::CtrlShiftP
        }
        (KeyCode::Char('P'), modifiers) if modifiers == KeyModifiers::CONTROL => Key::CtrlShiftP,
        (KeyCode::Char('d'), modifiers)
            if modifiers == KeyModifiers::CONTROL | KeyModifiers::SHIFT =>
        {
            Key::CtrlShiftD
        }
        (KeyCode::Char('D'), modifiers) if modifiers == KeyModifiers::CONTROL => Key::CtrlShiftD,
        (KeyCode::Char('m'), modifiers)
            if modifiers == KeyModifiers::CONTROL | KeyModifiers::SHIFT =>
        {
            Key::CtrlShiftM
        }
        (KeyCode::Char('M'), modifiers) if modifiers == KeyModifiers::CONTROL => Key::CtrlShiftM,
        (KeyCode::Char('b' | 'B'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            Key::CtrlB
        }
        (KeyCode::Char('w' | 'W'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            Key::DeletePreviousWord
        }
        (KeyCode::Char('u' | 'U'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            Key::DeleteToLineStart
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
    use std::{cell::RefCell, collections::VecDeque, rc::Rc};

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

    struct EventRuntime {
        guard: TerminalModeGuard,
        control: RecordingControl,
        events: VecDeque<Event>,
        terminal_error: Option<io::ErrorKind>,
    }

    impl EventRuntime {
        fn new(
            events: impl IntoIterator<Item = Event>,
        ) -> (Self, Rc<RefCell<Vec<TerminalOperation>>>) {
            let mut control = RecordingControl::default();
            let calls = Rc::clone(&control.calls);
            let guard = TerminalModeGuard::enter(&mut control).unwrap();
            (
                Self {
                    guard,
                    control,
                    events: events.into_iter().collect(),
                    terminal_error: None,
                },
                calls,
            )
        }

        fn failing_after(
            events: impl IntoIterator<Item = Event>,
            terminal_error: io::ErrorKind,
        ) -> (Self, Rc<RefCell<Vec<TerminalOperation>>>) {
            let (mut runtime, calls) = Self::new(events);
            runtime.terminal_error = Some(terminal_error);
            (runtime, calls)
        }
    }

    impl RuntimeTerminal for EventRuntime {
        fn poll(&mut self, _: Duration) -> io::Result<Option<Event>> {
            if let Some(event) = self.events.pop_front() {
                return Ok(Some(event));
            }
            self.terminal_error
                .map_or(Ok(None), |kind| Err(io::Error::from(kind)))
        }
    }

    impl Drop for EventRuntime {
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

    #[derive(Debug, PartialEq, Eq)]
    struct RecordedFrame {
        now: Duration,
        running: bool,
        quit_armed: bool,
        turn_state: Option<TurnState>,
        markdown: String,
        spinner: String,
    }

    #[derive(Default)]
    struct RecordingRenderer {
        frames: Vec<RecordedFrame>,
    }

    impl Renderer for RecordingRenderer {
        fn render(&mut self, state: ViewState<'_>) -> io::Result<()> {
            self.frames.push(RecordedFrame {
                now: state.now,
                running: state.running,
                quit_armed: state.quit_armed,
                turn_state: state.turn_state,
                markdown: state
                    .conversation
                    .map(|conversation| conversation.live_markdown.clone())
                    .unwrap_or_default(),
                spinner: widgets::StatusGlyph::char(state.running, state.now).to_owned(),
            });
            Ok(())
        }
    }

    fn expected_terminal_calls() -> Vec<TerminalOperation> {
        vec![
            TerminalOperation::EnableRaw,
            TerminalOperation::EnterAlternate,
            TerminalOperation::HideCursor,
            TerminalOperation::EnableMouse,
            TerminalOperation::EnableKeyboardEnhancement,
            TerminalOperation::EnablePaste,
            TerminalOperation::DisablePaste,
            TerminalOperation::DisableKeyboardEnhancement,
            TerminalOperation::DisableMouse,
            TerminalOperation::ShowCursor,
            TerminalOperation::LeaveAlternate,
            TerminalOperation::DisableRaw,
        ]
    }

    #[test]
    fn runtime_keeps_mouse_capture_enabled_until_exactly_once_cleanup() {
        let (terminal, calls) = EventRuntime::new([Event::Key(Key::CtrlC), Event::Key(Key::CtrlC)]);
        let mut tui = Tui::new(NoopEngine);
        let mut renderer = RecordingRenderer::default();

        run_with_runtime_terminal(&mut tui, &mut renderer, terminal).unwrap();

        let mouse_operations = calls
            .borrow()
            .iter()
            .copied()
            .filter(|operation| {
                matches!(
                    operation,
                    TerminalOperation::EnableMouse | TerminalOperation::DisableMouse
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            mouse_operations,
            [
                TerminalOperation::EnableMouse,
                TerminalOperation::DisableMouse,
            ]
        );
    }

    #[test]
    fn wheel_burst_preserves_the_first_non_wheel_event() {
        let resize = Event::Resize {
            width: 80,
            height: 24,
        };
        let (mut terminal, _) = EventRuntime::new([
            Event::MouseWheel(MouseWheelDirection::Down),
            resize.clone(),
            Event::MouseWheel(MouseWheelDirection::Up),
        ]);

        let (directions, deferred) = wheel_burst(&mut terminal, MouseWheelDirection::Up).unwrap();

        assert_eq!(
            directions,
            [MouseWheelDirection::Up, MouseWheelDirection::Down]
        );
        assert_eq!(deferred, Some(resize));
        assert_eq!(
            terminal.poll(Duration::ZERO).unwrap(),
            Some(Event::MouseWheel(MouseWheelDirection::Up))
        );
    }

    #[test]
    fn wheel_burst_stops_at_its_fairness_budget() {
        let (mut terminal, _) = EventRuntime::new(std::iter::repeat_n(
            Event::MouseWheel(MouseWheelDirection::Down),
            TERMINAL_WHEEL_BATCH_BUDGET,
        ));

        let (directions, deferred) = wheel_burst(&mut terminal, MouseWheelDirection::Up).unwrap();

        assert_eq!(directions.len(), TERMINAL_WHEEL_BATCH_BUDGET);
        assert_eq!(directions[0], MouseWheelDirection::Up);
        assert_eq!(deferred, None);
        assert_eq!(
            terminal.poll(Duration::ZERO).unwrap(),
            Some(Event::MouseWheel(MouseWheelDirection::Down))
        );
    }

    #[test]
    fn runtime_coalesces_a_wheel_burst_into_one_scroll_frame() {
        let events = std::iter::repeat_n(Event::MouseWheel(MouseWheelDirection::Up), 20)
            .chain([Event::Key(Key::CtrlC), Event::Key(Key::CtrlC)]);
        let (terminal, _) = EventRuntime::new(events);
        let mut tui = Tui::new(NoopEngine);
        tui.handle(Event::Resize {
            width: 48,
            height: 12,
        });
        tui.begin_submission("request");
        tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
            (0..200).map(|line| format!("line {line}\n")).collect(),
        )));
        tui.apply_progress(TurnEvent::StateChanged(TurnState::Completed));
        let bottom = tui.detached_scroll_bottom();
        let mut renderer = RecordingRenderer::default();

        run_with_runtime_terminal(&mut tui, &mut renderer, terminal).unwrap();

        assert_eq!(
            tui.view().scroll_offset,
            bottom.saturating_sub(20 * MOUSE_SCROLL_ROWS)
        );
        assert_eq!(
            renderer.frames.len(),
            3,
            "initial, one coalesced wheel frame, and the armed-quit frame"
        );
    }

    #[test]
    fn osc52_copy_is_explicit_bounded_and_contains_only_base64_selection_text() {
        assert_eq!(
            osc52_copy_sequence("café 🙂"),
            "\u{1b}]52;c;Y2Fmw6kg8J+Zgg==\u{7}"
        );
    }

    #[test]
    fn captured_mouse_cleanup_restores_mouse_cursor_and_terminal_after_render_or_poll_error() {
        let expected = vec![
            TerminalOperation::EnableRaw,
            TerminalOperation::EnterAlternate,
            TerminalOperation::HideCursor,
            TerminalOperation::EnableMouse,
            TerminalOperation::EnableKeyboardEnhancement,
            TerminalOperation::EnablePaste,
            TerminalOperation::DisablePaste,
            TerminalOperation::DisableKeyboardEnhancement,
            TerminalOperation::DisableMouse,
            TerminalOperation::ShowCursor,
            TerminalOperation::LeaveAlternate,
            TerminalOperation::DisableRaw,
        ];

        let (terminal, calls) = EventRuntime::new([Event::Resize {
            width: 100,
            height: 30,
        }]);
        let mut tui = Tui::new(NoopEngine);
        let mut renderer = FailingRenderer {
            fail_on_render: 2,
            renders: 0,
        };
        assert!(run_with_runtime_terminal(&mut tui, &mut renderer, terminal).is_err());
        assert_eq!(*calls.borrow(), expected);

        let (terminal, calls) = EventRuntime::failing_after([], io::ErrorKind::UnexpectedEof);
        let mut tui = Tui::new(NoopEngine);
        let mut renderer = RecordingRenderer::default();
        assert_eq!(
            run_with_runtime_terminal(&mut tui, &mut renderer, terminal)
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert_eq!(*calls.borrow(), expected);
    }

    #[test]
    fn crossterm_mouse_commands_emit_standard_enable_and_disable_sequences() {
        let mut enabled = Vec::new();
        execute!(enabled, EnableMouseCapture).unwrap();
        let mut disabled = Vec::new();
        execute!(disabled, DisableMouseCapture).unwrap();

        assert!(enabled.starts_with(b"\x1b[?1000h"));
        assert!(enabled.windows(8).any(|window| window == b"\x1b[?1006h"));
        assert!(disabled.starts_with(b"\x1b[?1006l"));
        assert!(disabled.ends_with(b"\x1b[?1000l"));
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
    fn production_frame_clock_ticks_working_spinner_without_terminal_input() {
        let mut tui = Tui::new(NoopEngine);
        let mut renderer = RecordingRenderer::default();
        let mut schedule = FrameSchedule::default();
        tui.begin_submission("request");

        assert!(
            render_progress_frame(&mut tui, &mut renderer, &mut schedule, Duration::ZERO, true,)
                .unwrap()
        );
        assert!(
            !render_progress_frame(
                &mut tui,
                &mut renderer,
                &mut schedule,
                Duration::from_millis(79),
                false,
            )
            .unwrap()
        );
        assert!(
            render_progress_frame(
                &mut tui,
                &mut renderer,
                &mut schedule,
                Duration::from_millis(80),
                false,
            )
            .unwrap()
        );

        assert_eq!(renderer.frames.len(), 2);
        assert_ne!(renderer.frames[0].spinner, renderer.frames[1].spinner);
        assert_eq!(renderer.frames[0].now, Duration::ZERO);
        assert_eq!(renderer.frames[1].now, ACTIVE_FRAME_HEARTBEAT);
    }

    #[test]
    fn production_frame_clock_removes_expired_quit_warning_without_input() {
        let mut tui = Tui::new(NoopEngine);
        let mut renderer = RecordingRenderer::default();
        let mut schedule = FrameSchedule::default();
        assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Render);

        assert!(
            render_progress_frame(&mut tui, &mut renderer, &mut schedule, Duration::ZERO, true)
                .unwrap()
        );
        assert!(
            !render_progress_frame(
                &mut tui,
                &mut renderer,
                &mut schedule,
                EXIT_WARNING_WINDOW - Duration::from_millis(1),
                false,
            )
            .unwrap()
        );
        assert!(
            render_progress_frame(
                &mut tui,
                &mut renderer,
                &mut schedule,
                EXIT_WARNING_WINDOW,
                false,
            )
            .unwrap()
        );

        assert_eq!(renderer.frames.len(), 2);
        assert!(renderer.frames[0].quit_armed);
        assert!(!renderer.frames[1].quit_armed);
    }

    #[test]
    fn begin_submission_renders_working_frame_before_first_provider_delta() {
        let mut tui = Tui::new(NoopEngine);
        let mut renderer = RecordingRenderer::default();
        let mut schedule = FrameSchedule::default();
        tui.begin_submission("request");

        render_progress_frame(&mut tui, &mut renderer, &mut schedule, Duration::ZERO, true)
            .unwrap();
        tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text("delta".into())));
        render_progress_frame(
            &mut tui,
            &mut renderer,
            &mut schedule,
            Duration::from_millis(1),
            true,
        )
        .unwrap();

        assert_eq!(renderer.frames[0].turn_state, Some(TurnState::Requesting));
        assert!(renderer.frames[0].running);
        assert_eq!(renderer.frames[0].markdown, "");
        assert_eq!(renderer.frames[1].markdown, "delta");
    }

    #[test]
    fn restored_fence_highlights_on_the_next_idle_heartbeat_only() {
        let _guard = render::SYNTAX_CACHE_TESTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        render::reset_syntax_highlight_test_state();
        let mut tui = Tui::new(NoopEngine);
        let history = Conversation::from_messages(&[
            agens_core::Message {
                role: agens_core::Role::User,
                parts: vec![MessagePart::Text("restored prompt".into())],
            },
            agens_core::Message {
                role: agens_core::Role::Assistant,
                parts: vec![MessagePart::Text(
                    "```js\nconst restored = true;\n```\n".into(),
                )],
            },
        ])
        .unwrap();
        tui.apply_submission_outcome(TuiSubmissionOutcome::SessionResumed {
            message: "Resumed session 2.".into(),
            presentation: TuiPresentation::new("provider", "model", "session #2"),
            history,
            draft: None,
            resume_error: None,
            file_candidates: Vec::new(),
            palette_entries: Vec::new(),
        });
        let terminal = RatatuiTerminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        let mut renderer = RatatuiRenderer::new(terminal);
        let mut schedule = FrameSchedule::default();

        assert!(
            render_progress_frame(&mut tui, &mut renderer, &mut schedule, Duration::ZERO, true,)
                .unwrap()
        );
        assert_eq!(render::syntax_highlight_test_calls(), 0);
        assert!(
            !render_progress_frame(
                &mut tui,
                &mut renderer,
                &mut schedule,
                ACTIVE_FRAME_HEARTBEAT - Duration::from_millis(1),
                false,
            )
            .unwrap()
        );
        assert_eq!(render::syntax_highlight_test_calls(), 0);
        assert!(
            render_progress_frame(
                &mut tui,
                &mut renderer,
                &mut schedule,
                ACTIVE_FRAME_HEARTBEAT,
                false,
            )
            .unwrap()
        );
        assert_eq!(render::syntax_highlight_test_calls(), 1);
        assert!(
            render_progress_frame(
                &mut tui,
                &mut renderer,
                &mut schedule,
                ACTIVE_FRAME_HEARTBEAT * 2,
                true,
            )
            .unwrap()
        );
        assert_eq!(render::syntax_highlight_test_calls(), 1);
    }

    #[test]
    fn blocked_producer_exposes_first_delta_before_later_progress_and_completion() {
        let mut tui = Tui::new(NoopEngine);
        tui.begin_submission("request");
        let mut renderer = RecordingRenderer::default();
        let mut schedule = FrameSchedule::default();
        render_progress_frame(&mut tui, &mut renderer, &mut schedule, Duration::ZERO, true)
            .unwrap();
        let (_metrics_sender, metrics_receiver) = BridgeTx::bounded(4);
        let (progress_sender, progress_receiver) = mpsc::channel();
        let (completion_sender, completion_receiver) = mpsc::channel();
        let (first_sent, first_received) = mpsc::channel();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let producer_barrier = Arc::clone(&barrier);
        let producer = thread::spawn(move || {
            progress_sender
                .send(TurnEvent::ProviderPart(MessagePart::Text("delta1".into())))
                .unwrap();
            first_sent.send(()).unwrap();
            producer_barrier.wait();
            progress_sender
                .send(TurnEvent::ProviderPart(MessagePart::Text("delta2".into())))
                .unwrap();
            progress_sender
                .send(TurnEvent::StateChanged(TurnState::Completed))
                .unwrap();
            completion_sender
                .send(TuiProviderOutcome::Completed("delta1delta2".into()))
                .unwrap();
        });
        first_received.recv().unwrap();

        let first = drain_provider_channels(
            &mut tui,
            &metrics_receiver,
            &progress_receiver,
            &completion_receiver,
        );
        assert!(first.dirty);
        render_progress_frame(
            &mut tui,
            &mut renderer,
            &mut schedule,
            Duration::from_millis(1),
            first.dirty,
        )
        .unwrap();
        assert_eq!(renderer.frames[1].markdown, "delta1");
        assert!(renderer.frames[1].running);

        barrier.wait();
        producer.join().unwrap();
        let second = drain_provider_channels(
            &mut tui,
            &metrics_receiver,
            &progress_receiver,
            &completion_receiver,
        );
        render_progress_frame(
            &mut tui,
            &mut renderer,
            &mut schedule,
            Duration::from_millis(2),
            second.dirty,
        )
        .unwrap();

        assert_eq!(renderer.frames[2].markdown, "delta1delta2");
        assert!(!renderer.frames[2].running);
    }

    #[test]
    fn provider_backlog_is_bounded_per_frame_and_preserves_fifo_before_completion() {
        let mut tui = Tui::new(NoopEngine);
        tui.begin_submission("request");
        let (metrics_sender, metrics_receiver) = BridgeTx::bounded(4);
        let (progress_sender, progress_receiver) = mpsc::channel();
        let (completion_sender, completion_receiver) = mpsc::channel();
        assert!(matches!(
            metrics_sender.publish(
                TuiRuntimeEvent::Usage(Usage {
                    input_tokens: Some(3),
                    output_tokens: Some(5),
                    total_tokens: Some(8),
                    context_window: Some(128),
                }),
                &BridgeCancel::new(),
                None,
            ),
            PublishOutcome::Published { .. }
        ));
        assert!(matches!(
            metrics_sender.publish(
                TuiRuntimeEvent::TurnEnded {
                    status: TurnState::Completed,
                    duration: Some(Duration::from_millis(12)),
                },
                &BridgeCancel::new(),
                None,
            ),
            PublishOutcome::Published { .. }
        ));
        let deltas = (0..PROGRESS_CHANNEL_BUDGET + 2)
            .map(|index| format!("{index},"))
            .collect::<Vec<_>>();
        for delta in &deltas {
            progress_sender
                .send(TurnEvent::ProviderPart(MessagePart::Text(delta.clone())))
                .unwrap();
        }
        progress_sender
            .send(TurnEvent::StateChanged(TurnState::Completed))
            .unwrap();
        completion_sender
            .send(TuiProviderOutcome::Completed(deltas.concat()))
            .unwrap();

        let first = drain_provider_channels(
            &mut tui,
            &metrics_receiver,
            &progress_receiver,
            &completion_receiver,
        );
        let mut renderer = RecordingRenderer::default();
        let mut schedule = FrameSchedule::default();
        render_progress_frame(
            &mut tui,
            &mut renderer,
            &mut schedule,
            Duration::from_millis(1),
            first.dirty,
        )
        .unwrap();

        assert!(first.dirty);
        assert!(first.backlog);
        assert_eq!(
            tui.view().conversation.unwrap().live_markdown,
            deltas[..PROGRESS_CHANNEL_BUDGET].concat()
        );
        assert!(tui.view().running);
        assert!(tui.view().latest_usage.is_none());
        assert_eq!(
            renderer.frames[0].markdown,
            deltas[..PROGRESS_CHANNEL_BUDGET].concat()
        );
        assert_eq!(
            FrameSchedule::default().poll_timeout(Duration::ZERO, true, first.backlog),
            Duration::ZERO
        );

        let second = drain_provider_channels(
            &mut tui,
            &metrics_receiver,
            &progress_receiver,
            &completion_receiver,
        );
        render_progress_frame(
            &mut tui,
            &mut renderer,
            &mut schedule,
            Duration::from_millis(2),
            second.dirty,
        )
        .unwrap();

        assert!(second.dirty);
        assert!(!second.backlog);
        assert_eq!(
            tui.view().conversation.unwrap().final_markdown.as_deref(),
            Some(deltas.concat().as_str())
        );
        assert!(!tui.view().running);
        assert_eq!(
            tui.view().latest_usage.and_then(|usage| usage.total_tokens),
            Some(8)
        );
        assert_eq!(renderer.frames[1].markdown, deltas.concat());
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
    fn ctrl_shift_d_routes_only_press_events_to_dangerous_mode() {
        for (code, modifiers) in [
            (
                KeyCode::Char('d'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            (KeyCode::Char('D'), KeyModifiers::CONTROL),
        ] {
            let mut tui = Tui::new(NoopEngine);
            let event = KeyEvent::new_with_kind(code, modifiers, KeyEventKind::Press);

            assert_eq!(
                map_event(CrosstermEvent::Key(event)).map(|event| tui.handle(event)),
                Some(Action::OpenDialog("dangerous".into()))
            );
        }

        for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
            let event = KeyEvent::new_with_kind(
                KeyCode::Char('d'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                kind,
            );

            assert_eq!(map_event(CrosstermEvent::Key(event)), None);
            assert_eq!(map_key(event), None);
        }

        assert_eq!(
            map_key(KeyEvent::new_with_kind(
                KeyCode::Char('d'),
                KeyModifiers::CONTROL,
                KeyEventKind::Press,
            )),
            Some(Event::Key(Key::Delete))
        );
    }

    #[test]
    fn ctrl_shift_p_routes_only_press_events_to_bypass_mode() {
        for (code, modifiers) in [
            (
                KeyCode::Char('p'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            (KeyCode::Char('P'), KeyModifiers::CONTROL),
        ] {
            let mut tui = Tui::new(NoopEngine);
            let event = KeyEvent::new_with_kind(code, modifiers, KeyEventKind::Press);

            assert_eq!(
                map_event(CrosstermEvent::Key(event)).map(|event| tui.handle(event)),
                Some(Action::OpenDialog("bypass".into()))
            );
        }

        for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
            let event = KeyEvent::new_with_kind(
                KeyCode::Char('p'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                kind,
            );

            assert_eq!(map_event(CrosstermEvent::Key(event)), None);
            assert_eq!(map_key(event), None);
        }
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
        for (kind, direction) in [
            (
                crossterm::event::MouseEventKind::ScrollUp,
                MouseWheelDirection::Up,
            ),
            (
                crossterm::event::MouseEventKind::ScrollDown,
                MouseWheelDirection::Down,
            ),
        ] {
            assert_eq!(
                map_event(CrosstermEvent::Mouse(crossterm::event::MouseEvent {
                    kind,
                    column: 4,
                    row: 2,
                    modifiers: KeyModifiers::NONE,
                })),
                Some(Event::MouseWheel(direction))
            );
        }
    }

    #[test]
    fn maps_left_mouse_drag_lifecycle_with_screen_coordinates() {
        for (kind, expected) in [
            (
                MouseEventKind::Down(crossterm::event::MouseButton::Left),
                Event::MouseDown { column: 7, row: 3 },
            ),
            (
                MouseEventKind::Drag(crossterm::event::MouseButton::Left),
                Event::MouseDrag { column: 7, row: 3 },
            ),
            (
                MouseEventKind::Up(crossterm::event::MouseButton::Left),
                Event::MouseUp { column: 7, row: 3 },
            ),
        ] {
            assert_eq!(
                map_event(CrosstermEvent::Mouse(crossterm::event::MouseEvent {
                    kind,
                    column: 7,
                    row: 3,
                    modifiers: KeyModifiers::NONE,
                })),
                Some(expected)
            );
        }
    }

    #[test]
    fn left_drag_selects_exact_unicode_text_and_control_c_copies_without_arming_quit() {
        let mut tui = Tui::new(NoopEngine);
        tui.handle(Event::Resize {
            width: 80,
            height: 12,
        });
        tui.active_record_mut()
            .transcript
            .push(TranscriptEntry::Info("alpha café 🙂 omega".into()));

        assert_eq!(
            tui.handle(Event::MouseDown { column: 24, row: 1 }),
            Action::Render
        );
        assert_eq!(
            tui.handle(Event::MouseDrag { column: 30, row: 1 }),
            Action::Render
        );
        assert_eq!(tui.selected_text(), None);
        assert!(tui.mouse_selection_snapshot.is_some());
        assert_eq!(
            tui.handle(Event::MouseUp { column: 30, row: 1 }),
            Action::Render
        );
        assert_eq!(tui.selected_text(), Some("café 🙂"));
        assert!(tui.mouse_selection_snapshot.is_none());
        assert_eq!(
            tui.handle(Event::Key(Key::CtrlC)),
            Action::CopySelection("café 🙂".into())
        );
        assert!(!tui.view().quit_armed);
    }

    #[test]
    fn real_mouse_wheel_scrolls_immediately_while_selection_anchors_remain_absolute() {
        let mut tui = Tui::new(NoopEngine);
        tui.handle(Event::Resize {
            width: 40,
            height: 8,
        });
        tui.begin_submission("prompt");
        tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
            (0..80).map(|index| format!("line {index}\n")).collect(),
        )));
        tui.apply_progress(TurnEvent::StateChanged(TurnState::Completed));
        let wheel = map_event(CrosstermEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 4,
            row: 2,
            modifiers: KeyModifiers::NONE,
        }))
        .expect("mouse wheel should map");

        assert!(tui.following_bottom());
        let following_bottom = tui.max_scroll_offset();
        let detached_bottom = tui.detached_scroll_bottom();
        assert_eq!(detached_bottom, following_bottom.saturating_add(1));
        assert_eq!(tui.handle(wheel.clone()), Action::Render);
        assert!(!tui.following_bottom());

        let scroll_offset = tui
            .transcripts
            .get(&TranscriptId::Main)
            .unwrap()
            .scroll_offset;
        assert_eq!(
            detached_bottom.saturating_sub(scroll_offset),
            MOUSE_SCROLL_ROWS
        );
        let transcript_row = tui.screen_layout().transcript.y.saturating_add(1);
        tui.handle(Event::MouseDown {
            column: TRANSCRIPT_CONTENT_INDENT,
            row: transcript_row,
        });
        tui.handle(Event::MouseDrag {
            column: TRANSCRIPT_CONTENT_INDENT + 5,
            row: transcript_row,
        });
        tui.handle(Event::MouseUp {
            column: TRANSCRIPT_CONTENT_INDENT + 5,
            row: transcript_row,
        });
        let selection = tui.selected_text().map(str::to_owned);
        assert!(selection.is_some());

        assert_eq!(tui.handle(wheel), Action::Render);
        assert!(
            tui.transcripts
                .get(&TranscriptId::Main)
                .unwrap()
                .scroll_offset
                < scroll_offset
        );
        assert_eq!(tui.selected_text(), selection.as_deref());
    }

    #[test]
    fn user_message_jumps_still_find_the_prompt_row_behind_the_accent_column() {
        let mut tui = Tui::new(NoopEngine);
        tui.handle(Event::Resize {
            width: 48,
            height: 12,
        });
        for turn in ["first-anchor", "second-anchor"] {
            tui.begin_submission(turn);
            tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
                "body\n".repeat(40),
            )));
            tui.apply_progress(TurnEvent::StateChanged(TurnState::Completed));
        }

        tui.handle(Event::Key(Key::CtrlN));
        assert!(
            tui.view().scroll_offset > 0,
            "a prompt row is reachable even though the accent column precedes its glyph"
        );
    }

    #[test]
    fn selected_text_never_exceeds_the_osc52_copy_limit() {
        let line = Line::raw("x".repeat(MAX_SELECTION_COPY_BYTES + 1));
        let transcript = SelectableTranscript::from_lines(&[line], 80);
        let last_row = transcript.rows.len() - 1;
        let last_column = transcript.rows[last_row].cells.last().unwrap().column;

        assert_eq!(
            transcript.selected_text(TranscriptSelection {
                anchor: TranscriptPosition { row: 0, column: 0 },
                head: TranscriptPosition {
                    row: last_row,
                    column: last_column,
                },
            }),
            Err(())
        );
    }

    #[test]
    fn selectable_transcript_preserves_word_wrapping_and_indentation() {
        let transcript =
            SelectableTranscript::from_lines(&[Line::raw("AAA AAA AAAAA AA AAAAAA")], 10);
        let rows = transcript
            .render_lines(None)
            .into_iter()
            .map(|line| {
                line.spans
                    .into_iter()
                    .map(|span| span.content)
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        assert_eq!(rows, ["AAA AAA", "AAAAA AA", "AAAAAA"]);
    }

    #[test]
    fn characters_stay_keybindings_until_the_search_key_arms_the_query() {
        let mut tui = Tui::new(NoopEngine);
        let dialog = || {
            DialogView::read_only(
                "Servers",
                None::<&str>,
                (0..4)
                    .map(|index| {
                        DialogEntry::action(format!("row {index}"), format!("open:{index}"))
                    })
                    .collect(),
                "servers",
            )
        };

        // `r` is the refresh binding, not the first letter of a query.
        tui.show_selection_dialog(dialog());
        assert_eq!(
            tui.handle(Event::Key(Key::Char('r'))),
            Action::OpenDialog("servers".into())
        );

        tui.show_selection_dialog(dialog());
        tui.handle(Event::Key(Key::Char('j')));
        tui.handle(Event::Key(Key::Char('j')));
        assert_eq!(
            tui.handle(Event::Key(Key::Enter)),
            Action::DialogAction("open:2".into())
        );
        tui.handle(Event::Key(Key::Char('k')));
        assert_eq!(
            tui.handle(Event::Key(Key::Enter)),
            Action::DialogAction("open:1".into())
        );
        tui.handle(Event::Key(Key::Char('G')));
        assert_eq!(
            tui.handle(Event::Key(Key::Enter)),
            Action::DialogAction("open:3".into())
        );
        tui.handle(Event::Key(Key::Char('g')));
        assert_eq!(
            tui.handle(Event::Key(Key::Enter)),
            Action::DialogAction("open:0".into())
        );
        assert!(
            tui.view()
                .dialog
                .is_some_and(|dialog| dialog.query.is_empty()),
            "navigation keys must never reach the query"
        );

        // Armed, the very same letters become filter text.
        tui.handle(Event::Key(Key::Char(DIALOG_SEARCH_KEY)));
        for character in "row 3".chars() {
            tui.handle(Event::Key(Key::Char(character)));
        }
        assert_eq!(
            tui.handle(Event::Key(Key::Enter)),
            Action::DialogAction("open:3".into())
        );

        // Escape gives the letters back before it closes the overlay.
        assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::Render);
        assert!(
            tui.view()
                .dialog
                .is_some_and(|dialog| dialog.query.is_empty())
        );
        assert_eq!(
            tui.handle(Event::Key(Key::Char('r'))),
            Action::OpenDialog("servers".into())
        );
        assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::Render);
        assert!(tui.view().dialog.is_none());
    }

    #[test]
    fn selected_key_actions_dispatch_the_selected_entry_identity() {
        let mut tui = Tui::new(NoopEngine);
        let dialog = || {
            DialogView::selection(
                "Profiles",
                Some("help"),
                vec![
                    DialogEntry::action("worker", "profiles:edit:worker").with_id("worker"),
                    DialogEntry::action("scout", "profiles:edit:scout").with_id("scout"),
                ],
            )
            .with_selected_key_action(Key::Right, "profiles:cycle:{selected}:next")
            .with_selected_key_action(Key::Backspace, "profiles:reset:{selected}")
        };

        tui.show_selection_dialog(dialog());
        assert_eq!(
            tui.handle(Event::Key(Key::Right)),
            Action::SafeDialogAction("profiles:cycle:worker:next".into())
        );
        assert!(
            tui.view().dialog.is_some(),
            "the dialog must stay visible until the outcome replaces it"
        );

        tui.show_selection_dialog(dialog());
        tui.handle(Event::Key(Key::Down));
        assert_eq!(
            tui.handle(Event::Key(Key::Backspace)),
            Action::SafeDialogAction("profiles:reset:scout".into())
        );
        assert!(
            tui.view().dialog.is_some(),
            "the dialog must stay visible until the outcome replaces it"
        );

        // Armed search keeps Backspace as query editing.
        tui.show_selection_dialog(dialog());
        tui.handle(Event::Key(Key::Char(DIALOG_SEARCH_KEY)));
        tui.handle(Event::Key(Key::Char('x')));
        assert_eq!(tui.handle(Event::Key(Key::Backspace)), Action::Render);
        assert!(tui.view().dialog.is_some());

        // Rows without an identity keep the key's default behavior.
        tui.show_selection_dialog(
            DialogView::selection(
                "Profiles",
                Some("help"),
                vec![DialogEntry::action("worker", "profiles:edit:worker")],
            )
            .with_selected_key_action(Key::Right, "profiles:cycle:{selected}:next"),
        );
        assert_eq!(tui.handle(Event::Key(Key::Right)), Action::Render);
        assert!(tui.view().dialog.is_some());
    }

    #[test]
    fn dispatched_actions_keep_the_dialog_until_the_outcome_replaces_it() {
        let mut tui = Tui::new(NoopEngine);
        tui.show_selection_dialog(DialogView::selection(
            "Profiles",
            Some("help"),
            vec![DialogEntry::action("worker", "profiles:edit:worker")],
        ));

        assert_eq!(
            tui.handle(Event::Key(Key::Enter)),
            Action::DialogAction("profiles:edit:worker".into())
        );
        assert!(
            tui.view().dialog.is_some(),
            "dispatch must not blank the current dialog"
        );

        tui.apply_submission_outcome(TuiSubmissionOutcome::Dialog(DialogView::selection(
            "Choose profile model",
            Some("help"),
            vec![DialogEntry::action(
                "gpt-4.1",
                "profiles:set-model:worker:gpt-4.1",
            )],
        )));
        assert!(
            tui.view().dialog.is_some(),
            "a dialog outcome replaces the previous one atomically"
        );

        tui.apply_submission_outcome(TuiSubmissionOutcome::LocalInfo("done".into()));
        assert!(
            tui.view().dialog.is_none(),
            "a non-dialog outcome closes the dialog"
        );
    }

    #[test]
    fn overlays_reject_drag_selection_and_printable_input_can_return_to_the_composer() {
        let mut tui = Tui::new(NoopEngine);
        tui.active_record_mut()
            .transcript
            .push(TranscriptEntry::Info("selectable transcript".into()));
        tui.show_selection_dialog(
            DialogView::selection(
                "Permission required",
                Some("native::read"),
                vec![DialogEntry::action("Allow once", "permission:1:allow-once")],
            )
            .as_confirm(),
        );

        tui.handle(Event::MouseDown { column: 4, row: 1 });
        tui.handle(Event::MouseDrag { column: 12, row: 1 });
        tui.handle(Event::MouseUp { column: 12, row: 1 });
        assert_eq!(tui.selected_text(), None);

        tui.handle(Event::Key(Key::Escape));
        tui.handle(Event::MouseDown { column: 4, row: 1 });
        assert_eq!(tui.view().focus, TranscriptFocus::Viewport);
        assert_eq!(tui.handle(Event::Key(Key::Char('i'))), Action::Render);
        assert_eq!(tui.view().focus, TranscriptFocus::Composer);
        assert_eq!(tui.handle(Event::Key(Key::Char('x'))), Action::Render);
        assert_eq!(tui.input(), "x");
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
            (KeyCode::Char('j'), ctrl, Key::CtrlJ),
            (KeyCode::Char('k'), ctrl, Key::CtrlK),
            (KeyCode::Char('g'), ctrl, Key::CtrlG),
            (KeyCode::Char('G'), ctrl, Key::CtrlShiftG),
            (KeyCode::Char('n'), ctrl, Key::CtrlN),
            (KeyCode::Char('N'), ctrl, Key::CtrlShiftN),
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
        // Ctrl+k is timeline scroll; kill-to-end remains available as DeleteToLineEnd.
        tui.handle(Event::Key(Key::DeleteToLineEnd));
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

    fn detailed_entry(detail_lines: usize) -> DialogEntry {
        DialogEntry::action_with_metadata(
            "#7 Alpha",
            "2 turns",
            "7 alpha",
            (0..detail_lines)
                .map(|line| format!("detail {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
            "session:7",
        )
    }

    fn open_details(mut dialog: DialogView) -> DialogView {
        dialog.details_open = true;
        dialog
    }

    fn arm_search(mut dialog: DialogView) -> DialogView {
        dialog.searching = true;
        dialog
    }

    #[test]
    fn dialog_desired_rows_counts_body_help_search_matches_and_capped_details() {
        let selection = DialogView::selection(
            "Choose",
            Some("Up/Down navigate"),
            (0..3)
                .map(|index| {
                    DialogEntry::action(format!("Option {index}"), format!("pick:{index}"))
                })
                .collect(),
        );
        assert_eq!(dialog_desired_rows(&selection, 30), 4);
        assert_eq!(
            dialog_desired_rows(&arm_search(selection), 30),
            5,
            "the search band only claims a row once it is armed"
        );

        let sessions = DialogView::sessions_page(
            vec![DialogEntry::action("#7 Alpha", "session:7")],
            SessionDialogRequest::initial(),
            None,
        );
        assert_eq!(
            dialog_desired_rows(&sessions, 30),
            1,
            "session help is fully covered by the derived footer"
        );

        // A constructor strips the newline, and the result still fits one wrapped row.
        let informational = DialogView::informational("Details", "first line\nsecond line");
        assert_eq!(dialog_desired_rows(&informational, 30), 2);

        let empty = DialogView::selection("Choose", None::<String>, Vec::new());
        assert_eq!(dialog_desired_rows(&empty, 30), 1);

        let detailed = open_details(DialogView::selection(
            "Choose",
            None::<String>,
            vec![detailed_entry(5)],
        ));
        assert_eq!(dialog_desired_rows(&detailed, 30), 4);

        let confirm = DialogView::selection(
            "Permission required",
            Some("native::read\n/work/alpha"),
            vec![DialogEntry::action("Allow once", "permission:1:allow-once")],
        )
        .as_confirm();
        assert_eq!(dialog_desired_rows(&confirm, 30), 2);
    }

    #[test]
    fn help_prose_wraps_to_the_band_width_and_every_diagnostic_keeps_a_row() {
        assert_eq!(
            wrapped_prose_lines("alpha beta gamma", 11),
            vec!["alpha beta", "gamma"]
        );
        assert_eq!(
            wrapped_prose_lines("short\nalpha beta gamma", 11),
            vec!["short", "alpha beta", "gamma"]
        );
        assert_eq!(
            wrapped_prose_lines("supercalifragilistic", 8),
            vec!["supercal", "ifragili", "stic"],
            "a word wider than the band is split instead of overflowing it"
        );
        assert!(wrapped_prose_lines("alpha", 0).is_empty());

        let mut tui = Tui::new(NoopEngine);
        tui.add_diagnostic(
            "first diagnostic that is long enough to need a second row at this width",
        );
        tui.add_diagnostic("second diagnostic");
        let view = tui.view();
        let dialog = view.dialog.as_ref().unwrap();

        assert_eq!(
            dialog_help_lines(dialog, 40),
            vec![
                "first diagnostic that is long enough to",
                "need a second row at this width",
                "second diagnostic",
            ]
        );
        assert_eq!(dialog_help_rows(dialog, 40), 3);

        let selection = DialogView::selection(
            "Choose",
            Some("a caption long enough to wrap were it allowed to"),
            vec![DialogEntry::action("Option", "pick")],
        );
        assert_eq!(
            dialog_help_rows(&selection, 20),
            1,
            "a caption above entry rows must not take rows from them"
        );
    }

    #[test]
    fn dialog_sections_sacrifice_help_then_details_before_the_entry_rows() {
        let content = Rect::new(4, 2, 30, 10);
        let selection = DialogView::selection(
            "Choose",
            Some("Up/Down navigate"),
            vec![DialogEntry::action("Option", "pick")],
        );

        let idle = dialog_sections(content, &selection);
        assert!(
            idle.search.is_none(),
            "an unarmed dialog spends every row on its entries"
        );
        assert_eq!(idle.help, Some(Rect::new(4, 2, 30, 1)));
        assert_eq!(idle.rows, Rect::new(4, 3, 30, 9));

        let selection = arm_search(selection);
        let roomy = dialog_sections(content, &selection);
        assert_eq!(roomy.help, Some(Rect::new(4, 2, 30, 1)));
        assert_eq!(roomy.search, Some(Rect::new(4, 3, 30, 1)));
        assert_eq!(roomy.rows, Rect::new(4, 4, 30, 8));
        assert!(roomy.details.is_none());

        let single = dialog_sections(Rect::new(4, 2, 30, 1), &selection);
        assert!(single.search.is_none(), "one row belongs to the entries");
        assert!(single.help.is_none());
        assert_eq!(single.rows.height, 1);

        let pair = dialog_sections(Rect::new(4, 2, 30, 2), &selection);
        assert_eq!(pair.search.map(|rect| rect.y), Some(2));
        assert!(pair.help.is_none(), "help yields to search and entries");
        assert_eq!(pair.rows, Rect::new(4, 3, 30, 1));

        let informational = DialogView::informational("Details", "body prose");
        let help = dialog_sections(Rect::new(0, 0, 30, 6), &informational);
        assert_eq!(help.help, Some(Rect::new(0, 0, 30, 1)));
        assert!(help.search.is_none());
        assert_eq!(help.rows, Rect::new(0, 1, 30, 5));

        let squeezed = dialog_sections(Rect::new(0, 0, 30, 1), &informational);
        assert!(squeezed.help.is_none(), "the last row belongs to the body");
        assert_eq!(squeezed.rows.height, 1);

        let detailed = arm_search(open_details(DialogView::selection(
            "Choose",
            None::<String>,
            vec![detailed_entry(3)],
        )));
        let detail = dialog_sections(Rect::new(0, 0, 30, 4), &detailed);
        assert_eq!(detail.search.map(|rect| rect.y), Some(0));
        assert_eq!(detail.rows, Rect::new(0, 1, 30, 1));
        assert_eq!(detail.details, Some(Rect::new(0, 2, 30, 2)));
    }

    #[test]
    fn dialog_footer_shortcuts_are_derived_from_capabilities_not_from_help_prose() {
        let confirm = |help: &str| {
            DialogView::selection(
                "Permission required",
                Some(help),
                vec![DialogEntry::action("Allow once", "permission:1:allow-once")],
            )
            .as_confirm()
        };
        let labels = dialog_shortcut_labels(&confirm("native::read"));
        assert_eq!(
            labels,
            dialog_shortcut_labels(&confirm("totally different prose")),
            "footer must not depend on the help string"
        );
        assert_eq!(
            labels
                .iter()
                .map(|(key, _)| key.as_ref())
                .collect::<Vec<_>>(),
            ["a", "d", "A", "D", "esc"]
        );
        assert_eq!(
            labels
                .iter()
                .map(|(_, label)| label.as_ref())
                .collect::<Vec<_>>(),
            [
                "allow once",
                "deny once",
                "allow always",
                "deny always",
                "cancel"
            ]
        );

        let picker = DialogView::selection(
            "Choose",
            Some("Up/Down navigate, Enter selects, Esc cancels"),
            vec![DialogEntry::action("Option", "pick")],
        );
        assert_eq!(
            dialog_shortcut_labels(&picker),
            vec![
                (Cow::Borrowed("↑↓ jk"), Cow::Borrowed("navigate")),
                (Cow::Borrowed("⏎"), Cow::Borrowed("select")),
                (Cow::Borrowed("/"), Cow::Borrowed("search")),
                (Cow::Borrowed("esc"), Cow::Borrowed("close")),
            ]
        );
        assert_eq!(
            dialog_shortcut_labels(&arm_search(picker)),
            vec![
                (Cow::Borrowed("↑↓"), Cow::Borrowed("navigate")),
                (Cow::Borrowed("⏎"), Cow::Borrowed("select")),
                (Cow::Borrowed("esc"), Cow::Borrowed("exit search")),
            ],
            "armed search must not advertise the letter bindings it swallows"
        );

        let informational = dialog_shortcut_labels(&DialogView::informational("Details", "body"));
        assert_eq!(
            informational,
            vec![(Cow::Borrowed("esc"), Cow::Borrowed("close"))]
        );

        let first_page = dialog_shortcut_labels(&DialogView::sessions_page(
            vec![DialogEntry::action("#7 Alpha", "session:7")],
            SessionDialogRequest::initial(),
            Some(SessionDialogCursor::new(100, 7)),
        ));
        assert!(
            first_page.contains(&(Cow::Borrowed("⇞⇟"), Cow::Owned("page 1 · more".to_owned()))),
            "{first_page:?}"
        );
        assert!(
            first_page.contains(&(Cow::Borrowed("ctrl+a"), Cow::Borrowed("all projects"))),
            "{first_page:?}"
        );
        assert!(
            first_page.contains(&(Cow::Borrowed("⏎"), Cow::Borrowed("resume"))),
            "{first_page:?}"
        );

        let last_page = dialog_shortcut_labels(&DialogView::sessions_page(
            Vec::new(),
            SessionDialogRequest {
                scope: SessionDialogScope::AllProjects,
                ..SessionDialogRequest::initial()
            },
            None,
        ));
        assert!(
            last_page.contains(&(Cow::Borrowed("⇞⇟"), Cow::Owned("page 1 · end".to_owned()))),
            "{last_page:?}"
        );
        assert!(
            last_page.contains(&(Cow::Borrowed("ctrl+a"), Cow::Borrowed("current project"))),
            "{last_page:?}"
        );
    }

    #[test]
    fn bottom_chrome_bands_share_one_gutter_that_collapses_on_narrow_terminals() {
        let layout = screen_layout(Rect::new(0, 0, 120, 24), "", true);
        for band in [layout.composer, layout.notice, layout.tree, layout.footer] {
            assert_eq!(band.x, CHROME_GUTTER, "{band:?}");
            assert_eq!(band.width, 120 - 2 * CHROME_GUTTER, "{band:?}");
        }
        assert_eq!(layout.transcript.x, 0);
        assert_eq!(layout.transcript.width, 120);

        assert_eq!(
            [0_u16, 1, 24, 26, 28, 30, 32, 120].map(chrome_gutter),
            [0, 0, 0, 1, 2, 3, 4, 4]
        );

        for width in 0..=64_u16 {
            let layout = screen_layout(Rect::new(0, 0, width, 24), "", false);
            assert!(layout.composer.right() <= width, "width {width}");
            assert!(
                layout.composer.width >= width.min(MIN_GUTTERED_COMPOSER_WIDTH),
                "width {width} starves the composer: {:?}",
                layout.composer
            );
        }
    }

    fn secret_entry_view() -> SecretEntryView {
        SecretEntryView::new("API key", Some("Paste your API key."), "openai-api")
    }

    #[test]
    fn secret_entry_masks_typed_and_pasted_sentinel_in_test_backend() {
        let sentinel = "SECRET_TYPED_AND_PASTED_SENTINEL";
        let mut tui = Tui::new(NoopEngine);
        tui.apply_submission_outcome(TuiSubmissionOutcome::SecretEntry(secret_entry_view()));
        assert_eq!(tui.handle(Event::Key(Key::Char('A'))), Action::Render);
        assert_eq!(tui.handle(Event::Paste(sentinel.into())), Action::Render);

        let terminal = RatatuiTerminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        let mut renderer = RatatuiRenderer::new(terminal);
        renderer.render(tui.view()).unwrap();
        let rendered = renderer
            .terminal()
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!rendered.contains(sentinel));
        assert!(rendered.contains("***"));
    }

    #[test]
    fn secret_entry_isolated_from_composer_dialog_and_transcript() {
        let mut tui = Tui::new(NoopEngine);
        tui.handle(Event::Paste("composer".into()));
        tui.show_selection_dialog(DialogView::selection("ordinary", None::<&str>, Vec::new()));
        tui.apply_submission_outcome(TuiSubmissionOutcome::SecretEntry(secret_entry_view()));
        tui.handle(Event::Paste("SECRET_ISOLATION_SENTINEL".into()));
        assert_eq!(tui.input(), "composer");
        assert!(tui.view().dialog.is_none());
        assert!(tui.transcript().is_empty());
    }

    #[test]
    fn secret_entry_escape_and_ctrl_c_close_before_global_quit_copy_or_cancel() {
        for key in [Key::Escape, Key::CtrlC] {
            let mut tui = Tui::new(NoopEngine);
            tui.apply_submission_outcome(TuiSubmissionOutcome::SecretEntry(secret_entry_view()));
            tui.handle(Event::Paste("SECRET_CANCEL_SENTINEL".into()));
            assert_eq!(tui.handle(Event::Key(key)), Action::Render);
            assert!(tui.view().secret_entry.is_none());
            assert!(!tui.quit_is_armed());
        }
    }

    #[test]
    fn secret_entry_filters_edits_and_caps_at_8192_bytes() {
        let mut tui = Tui::new(NoopEngine);
        tui.apply_submission_outcome(TuiSubmissionOutcome::SecretEntry(secret_entry_view()));
        tui.handle(Event::Paste(format!(
            "{}\n\u{00e9}\t{}",
            "a".repeat(8192),
            "b"
        )));
        tui.handle(Event::Key(Key::Backspace));
        tui.handle(Event::Key(Key::Delete));
        tui.handle(Event::Key(Key::DeleteToLineStart));
        tui.handle(Event::Paste(" x".into()));
        let action = tui.handle(Event::Key(Key::Enter));
        assert!(format!("{action:?}").contains("SubmitSecret"));
        assert!(!format!("{action:?}").contains(" x"));
    }

    #[test]
    fn secret_entry_empty_submit_has_fixed_error_and_next_edit_clears_it() {
        let mut tui = Tui::new(NoopEngine);
        tui.apply_submission_outcome(TuiSubmissionOutcome::SecretEntry(secret_entry_view()));
        tui.handle(Event::Paste("   ".into()));
        assert_eq!(tui.handle(Event::Key(Key::Enter)), Action::Render);
        assert_eq!(
            tui.view().secret_entry.unwrap().error,
            Some("API key is required.")
        );
        tui.handle(Event::Key(Key::Char('x')));
        assert!(tui.view().secret_entry.unwrap().error.is_none());
    }

    #[test]
    fn secret_entry_submits_trimmed_dedicated_redacted_action_and_request() {
        let mut tui = Tui::new(NoopEngine);
        tui.apply_submission_outcome(TuiSubmissionOutcome::SecretEntry(secret_entry_view()));
        tui.handle(Event::Paste("  SECRET_SUBMISSION_SENTINEL  ".into()));
        let action = tui.handle(Event::Key(Key::Enter));
        let Action::SubmitSecret { action_id, secret } = action else {
            panic!("secret action")
        };
        assert_eq!(action_id, "openai-api");
        assert!(!format!("{secret:?}").contains("SECRET_SUBMISSION_SENTINEL"));
        let request = TuiRouteRequest::SubmitSecret { action_id, secret };
        assert!(!format!("{request:?}").contains("SECRET_SUBMISSION_SENTINEL"));
        assert!(!is_session_resume_request(&request));
        assert!(!is_session_browser_request(&request));
    }
}
