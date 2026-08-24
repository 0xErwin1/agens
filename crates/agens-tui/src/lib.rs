//! Terminal lifecycle and input-event boundary for the interactive surface.

mod activity;
mod app;
mod ask_user;
mod bridge;
mod conversation;
#[cfg(feature = "perf-audit")]
pub mod perf;
mod render;
pub mod shortcuts;
mod terminal;
mod widgets;

pub use activity::{RetryActivity, TurnActivity};
pub use agens_bus::{BridgeCancel, BridgeTx, PublishOutcome, UiEnvelope};
pub use agens_core::{
    DiffLine, DiffLineKind, NoticeSeverity, PromptAttachment, ToolResultState, TuiExecution,
    TuiExecutionEvent, TuiExecutionState, TuiRuntimeEvent, TuiSubagentEvent,
};
pub use app::{
    ActiveRoute, AppEvent, AppState, Command, Dialog, Effect, PromptObservability,
    PromptTransition, QueueEntry, Runtime, TurnLifecycle,
};
pub use ask_user::{AskUserEditing, AskUserRowSnapshot, AskUserSnapshot};
pub use bridge::{
    ExternalAskUserAnswer, PromptOrigin, TuiAskUserBridge, TuiAskUserObserver, TuiAskUserRequest,
    TuiPermissionBridge, TuiPermissionReply, TuiPermissionRequest,
};
pub use conversation::{
    ActionableError, Conversation, ConversationError, ConversationEvent, SubagentCard, ToolBatch,
    ToolCall, ToolResult, TurnCost,
};
pub use terminal::{
    PendingPermissions, PermissionReply, TerminalControl, TerminalModeGuard, TerminalOperation,
    teardown,
};
pub use widgets::{ColorLevel, DisplayMode, UnicodeLevel, abbreviate_path};

use std::{
    borrow::Cow,
    cell::RefCell,
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
use agens_core::ask_user::{AskUserMode, AskUserQuestion, AskUserReply, AskUserRequest};
use agens_core::{HistoryBrowseResult, PromptMemory, PromptRecall, media_chip_label};
use agens_core::{MessagePart, TurnEvent, TurnState, Usage};
use ask_user::{AskUserEntry, AskUserOutcome, AskUserRow, AskUserState};
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
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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

/// Stands in for a failure whose cause has not arrived (and may never arrive).
/// Matched verbatim when the real cause replaces it, so it must stay a literal.
const UNEXPLAINED_FAILURE_MESSAGE: &str = "The turn failed without reporting a cause.";

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
    /// The pointer moved with no button held.
    MouseMove {
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

#[cfg(feature = "perf-audit")]
impl Event {
    /// Stable trace-field label for the event's discriminant.
    const fn trace_kind(&self) -> &'static str {
        match self {
            Self::Key(_) => "key",
            Self::MouseWheel(_) => "mouse_wheel",
            Self::MouseDown { .. } => "mouse_down",
            Self::MouseDrag { .. } => "mouse_drag",
            Self::MouseUp { .. } => "mouse_up",
            Self::MouseMove { .. } => "mouse_move",
            Self::Paste(_) => "paste",
            Self::Resize { .. } => "resize",
        }
    }
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
    DeleteNextWord,
    DeleteToLineStart,
    DeleteToLineEnd,
    /// Submits the current input.
    Enter,
    /// Dismisses overlays or returns from nested views; inert on the main running surface.
    Escape,
    /// Requests foreground cancellation, or retains the existing quit behavior when idle.
    CtrlC,
    /// Copies the active transcript selection through the terminal clipboard.
    CtrlShiftC,
    /// Advances the transcript's tool output detail level.
    CtrlO,
    /// Walks the tool output detail level back one step.
    CtrlShiftO,
    /// Shows or hides the reasoning bodies of the active transcript.
    CtrlT,
    /// Unfolds the settled turns the transcript elided, or folds them back.
    CtrlY,
    /// Scrolls the transcript timeline down (composer-safe).
    CtrlJ,
    /// Scrolls the transcript timeline up (composer-safe).
    CtrlK,
    /// Half-page down in the focused transcript; forward delete in the composer.
    CtrlD,
    /// Half-page up in the focused transcript; delete-to-line-start in the composer.
    CtrlU,
    /// Opens the keyboard shortcut overlay from any mode.
    CtrlQuestion,
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
    /// Requests an OS clipboard image attach (Ctrl+V when an image is available).
    CtrlV,
    /// Opens the subagent model profile editor.
    CtrlShiftM,
    /// Toggles the visible dangerous-mode session state through the composition layer.
    CtrlShiftD,
    /// Toggles the visible permission-bypass session state through the composition layer.
    CtrlShiftP,
    /// Starts or moves the selected subagent into background execution.
    CtrlB,
    /// Pushes non-empty composer text onto the LIFO stash, or pops when empty.
    CtrlS,
    /// Opens the session lineage browser the reader can fork from.
    CtrlR,
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
    AltUp,
    AltDown,
    ScrollUp,
    ScrollDown,
    Up,
    Down,
    Tab,
}

impl Key {
    /// Whether holding this key down should keep applying it.
    ///
    /// Typing, deleting and moving are the keys a reader holds on purpose:
    /// dropping their auto-repeat means a held backspace deletes one character
    /// and a held `Ctrl+W` one word, which is not what holding a key means
    /// anywhere else.
    ///
    /// Everything else is a command or a mode, and a command must fire once
    /// per press. A held `Ctrl+Shift+P` that toggled permission bypass forty
    /// times would land on whichever state the key release happened to leave —
    /// that is why auto-repeat was dropped wholesale, and why it stays dropped
    /// here.
    pub const fn repeats_while_held(self) -> bool {
        matches!(
            self,
            Self::Char(_)
                | Self::Backspace
                | Self::Delete
                | Self::DeletePreviousWord
                | Self::DeleteNextWord
                | Self::DeleteToLineStart
                | Self::DeleteToLineEnd
                | Self::Left
                | Self::Right
                | Self::Up
                | Self::Down
                | Self::PreviousWord
                | Self::NextWord
                | Self::PageUp
                | Self::PageDown
                | Self::ScrollUp
                | Self::ScrollDown
                | Self::CtrlJ
                | Self::CtrlK
                | Self::CtrlD
                | Self::CtrlU
        )
    }

    /// Whether this key moves the transcript viewport under the vim keymap.
    ///
    /// Reading the transcript stays available while a session is being
    /// restored, so these keys survive the load-time key filter.
    pub const fn moves_viewport(self) -> bool {
        matches!(
            self,
            Self::Char('j' | 'k' | 'g' | 'G' | '{' | '}') | Self::CtrlD | Self::CtrlU
        )
    }

    /// Whether this key's only meaning is to edit or submit the prompt.
    pub const fn edits_composer(self) -> bool {
        matches!(
            self,
            Self::Char(_)
                | Self::ShiftEnter
                | Self::Enter
                | Self::Backspace
                | Self::Delete
                | Self::DeletePreviousWord
                | Self::DeleteNextWord
                | Self::DeleteToLineStart
                | Self::DeleteToLineEnd
                | Self::Left
                | Self::Right
                | Self::PreviousWord
                | Self::NextWord
                | Self::LineStart
                | Self::LineEnd
        )
    }

    /// The composer meaning of a key whose transcript meaning differs.
    ///
    /// `Ctrl+D` and `Ctrl+U` are readline edits inside the composer and vim
    /// half-page motions over the transcript. The terminal cannot tell those
    /// apart — the mode can — so the raw key is carried to the handler and
    /// resolved here rather than being decided while mapping the event.
    const fn composer_equivalent(self) -> Self {
        match self {
            Self::CtrlD => Self::Delete,
            Self::CtrlU => Self::DeleteToLineStart,
            other => other,
        }
    }
}

/// The result of handling a single terminal event.
#[derive(Clone, Eq, PartialEq)]
pub enum Action {
    /// Render the current view state.
    Render,
    /// The event changed nothing a reader could see.
    ///
    /// Pointer movement is the reason this exists: it arrives dozens of times
    /// per second and almost all of it lands inside the block already under
    /// the cursor, where repainting shows exactly what was already on screen.
    Unchanged,
    /// Send this prompt to the composition layer.
    Submit(String),
    /// Request OS clipboard image ingest (Ctrl+V when image data is available).
    AttachClipboardImage,
    /// Replace the app-side staged media with a restored attachment set.
    ///
    /// Emitted when a stash pop, overlay paste, or history browse changes the
    /// composer's chips: the session context owns what a submit actually
    /// sends, so it must follow the restored set (possibly empty).
    SyncStagedMedia(Vec<PromptAttachment>),
    /// Classify this busy composer draft before mutating the prompt queue.
    SubmitBusy(String),
    /// Submit a redacted credential through the dedicated route only.
    SubmitSecret {
        action_id: String,
        secret: SecretInput,
    },
    SubmitBackground(String),
    TransitionToBackground(u64),
    CancelExecution(u64),
    CancelAllExecutions,
    SendTaskMessage {
        id: u64,
        message: String,
    },
    /// Ask the composition layer to resolve a palette dialog by stable route ID.
    OpenDialog(String),
    /// Load a bounded session-browser page through the composition layer.
    LoadSessionPage(SessionDialogRequest),
    /// Load the lineage a session belongs to, for the session-tree overlay.
    LoadSessionTree(SessionTreeRequest),
    /// Fork a session at the point the reader pointed at.
    ForkSession(SessionForkRequest),
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
    /// Resolves the open ask-user interaction with a validated terminal reply.
    AskUserReply {
        id: u64,
        reply: AskUserReply,
    },
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
    /// A catalog-resolved provider turn accepted into the busy prompt queue.
    BusyProviderTurn {
        display: String,
        prompt: String,
    },
    /// A busy-session refusal that preserves the composer draft for editing.
    BusyRefusal(String),
    /// Opens an isolated credential-entry overlay.
    SecretEntry(SecretEntryView),
    LocalInfo(String),
    /// Local attach succeeded: update staged attachments (chips derive from them) and status.
    MediaAttached {
        message: String,
        staged_media: Vec<PromptAttachment>,
    },
    /// App-side staged media now matches a restored attachment set.
    ///
    /// Applied silently unless the restore had to drop attachments whose blob is gone,
    /// which `notice` then reports.
    StagedMediaReplaced {
        staged_media: Vec<PromptAttachment>,
        notice: Option<String>,
    },
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
    /// A turn was taken back or put back: the transcript is replaced with the
    /// history that is live again and the prompt that started the turn goes
    /// back to the composer, so undoing costs no retyping.
    HistoryRewritten {
        message: String,
        /// What the one-line message left out — the files a rewind deliberately
        /// did not touch. Shown where the reader can read all of it, rather
        /// than truncated into the status line.
        detail: Option<String>,
        presentation: TuiPresentation,
        history: Vec<Conversation>,
        draft: Option<String>,
    },
    SessionResumed {
        message: String,
        presentation: TuiPresentation,
        history: Vec<Conversation>,
        draft: Option<String>,
        /// Staged attachments restored from the retry boundary (durable ids only; no paths).
        staged_media: Vec<PromptAttachment>,
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
    /// Open the Tui-owned searchable prompt-history overlay (stores stay on Tui).
    PromptHistoryOverlay,
    /// Open the Tui-owned prompt-stash pick/remove overlay (stores stay on Tui).
    PromptStashOverlay,
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
    /// Resolve a busy input through the router before scheduler mutation.
    BusyInput(String),
    /// Opens a device-authentication URL through the application adapter.
    DeviceAuthOpenUrl(String),
    /// Ingest OS clipboard image bytes into the durable media store.
    AttachClipboardImage {
        bytes: Vec<u8>,
        mime: Option<String>,
    },
    /// Replace the session's staged media with a restored attachment set.
    ReplaceStagedMedia {
        attachments: Vec<PromptAttachment>,
    },
    SubmitSecret {
        action_id: String,
        secret: SecretInput,
    },
    OpenDialog(String),
    DialogAction(String),
    SessionPage(SessionDialogRequest),
    SessionTree(SessionTreeRequest),
    ForkSession(SessionForkRequest),
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

/// Borrowed projection of an open ask-user interaction, for one frame.
///
/// Every field either points into the state the interaction already owns or is
/// a scalar. A renderer that copied the request would pay for the whole
/// question set — labels, explanations and context — on every frame, including
/// the frames where nothing about it changed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AskUserRender<'a> {
    request: &'a AskUserRequest,
    question: usize,
    row: AskUserRow,
    selected: &'a BTreeSet<usize>,
    other: &'a str,
    note: &'a str,
    entry: AskUserEntry,
    entry_cursor: usize,
    context_option: usize,
    context_scroll: u16,
    answered: usize,
    origin: Option<&'a crate::PromptOrigin>,
    reviewing: bool,
    /// All questions' selections, for the review list.
    selections: &'a [BTreeSet<usize>],
    others: &'a [String],
    notes: &'a [String],
}

impl<'a> AskUserRender<'a> {
    fn of(state: &'a AskUserState) -> Self {
        Self {
            request: state.request(),
            question: state.question_index(),
            row: state.row(),
            selected: state.current_selections(),
            other: state.current_other(),
            note: state.current_note(),
            entry: state.entry(),
            entry_cursor: state.entry_cursor(),
            context_option: state.context_option(),
            context_scroll: state.context_scroll(),
            answered: state.answered_count(),
            origin: state.origin(),
            reviewing: state.reviewing(),
            selections: state.selections(),
            others: state.others(),
            notes: state.notes(),
        }
    }

    fn current(&self) -> &'a AskUserQuestion {
        &self.request.questions()[self.question]
    }
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
            Self::Unchanged => formatter.write_str("Unchanged"),
            Self::Submit(value) => formatter.debug_tuple("Submit").field(value).finish(),
            Self::AttachClipboardImage => formatter.write_str("AttachClipboardImage"),
            Self::SyncStagedMedia(value) => formatter
                .debug_tuple("SyncStagedMedia")
                .field(value)
                .finish(),
            Self::SubmitBusy(value) => formatter.debug_tuple("SubmitBusy").field(value).finish(),
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
            Self::CancelAllExecutions => formatter.write_str("CancelAllExecutions"),
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
            Self::LoadSessionTree(value) => formatter
                .debug_tuple("LoadSessionTree")
                .field(value)
                .finish(),
            Self::ForkSession(value) => formatter.debug_tuple("ForkSession").field(value).finish(),
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
            Self::AskUserReply { id, reply } => formatter
                .debug_struct("AskUserReply")
                .field("id", id)
                .field("status", &ask_user_reply_status(reply))
                .finish(),
            Self::Quit => formatter.write_str("Quit"),
        }
    }
}

/// The reply's terminal status only, for `Action`'s debug rendering.
///
/// Answers, notes and free text never reach a debug rendering: only the
/// closed set of statuses the tool layer itself encodes does.
const fn ask_user_reply_status(reply: &AskUserReply) -> &'static str {
    match reply {
        AskUserReply::Answered(_) => "answered",
        AskUserReply::Discuss { .. } => "discuss",
        AskUserReply::Cancelled => "cancelled",
        AskUserReply::Unavailable(_) => "unavailable",
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
            Self::BusyInput(value) => formatter.debug_tuple("BusyInput").field(value).finish(),
            Self::DeviceAuthOpenUrl(_) => formatter.write_str("DeviceAuthOpenUrl(<redacted>)"),
            Self::AttachClipboardImage { bytes, mime } => formatter
                .debug_struct("AttachClipboardImage")
                .field("bytes", &bytes.len())
                .field("mime", mime)
                .finish(),
            Self::ReplaceStagedMedia { attachments } => formatter
                .debug_struct("ReplaceStagedMedia")
                .field("attachments", attachments)
                .finish(),
            Self::OpenDialog(value) => formatter.debug_tuple("OpenDialog").field(value).finish(),
            Self::DialogAction(value) => {
                formatter.debug_tuple("DialogAction").field(value).finish()
            }
            Self::SessionPage(value) => formatter.debug_tuple("SessionPage").field(value).finish(),
            Self::SessionTree(value) => formatter.debug_tuple("SessionTree").field(value).finish(),
            Self::ForkSession(value) => formatter.debug_tuple("ForkSession").field(value).finish(),
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

/// Which surface above the prompt holds the keyboard.
///
/// The subagent tree is deliberately absent: it hangs below the composer and is
/// walked into with the down arrow, so Tab has exactly one destination.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceFocus {
    Composer,
    Queue,
}

const DEFAULT_PROMPT_QUEUE_CAPACITY: usize = 8;

/// Which user message a jump lands on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UserMessageTarget {
    /// The nearest one above the viewport.
    Previous,
    /// The nearest one below the viewport.
    Next,
    /// The most recent one in the transcript.
    Last,
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
    /// Detail level every settled tool call of this transcript is shown at.
    ///
    /// The per-call map is what the renderer reads, but the level is what the
    /// reader actually moves, so it is held on its own: a call that settles
    /// after Ctrl+O moved the level joins its neighbours instead of appearing
    /// hidden among expanded ones.
    tool_detail: widgets::DisplayMode,
    collapse_thinking: bool,
    /// When true, the settled turns the transcript would elide stay in view.
    history_expanded: bool,
    /// The block keyboard navigation is standing on, by call id.
    focused_call: Option<String>,
    /// When true, auto-collapse on turn finish is skipped (user re-expanded via Ctrl+T).
    thinking_user_pinned: bool,
    /// Full tool args + output opened as a scrollable overlay (Grok-style).
    ///
    /// The transcript keeps a short preview; long detail lives here so it does
    /// not blow up the chat scroll.
    tool_overlay: Option<ToolDetailOverlay>,
    focus: TranscriptFocus,
    selection: Option<TranscriptSelection>,
    selection_text: Option<String>,
    selection_too_large: bool,
    selecting: bool,
    last_admitted_ordinal: Option<u64>,
    terminal: bool,
}

/// Full detail for one tool call, shown in a modal overlay.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolDetailOverlay {
    pub call_id: String,
    pub title: String,
    pub status: String,
    pub args: String,
    pub output: String,
    pub scroll: u16,
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
            tool_detail: widgets::DisplayMode::Collapsed,
            collapse_thinking: false,
            history_expanded: false,
            focused_call: None,
            thinking_user_pinned: false,
            tool_overlay: None,
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

/// Hands out a transcript generation no other transcript can be addressed by.
///
/// Cached descriptions of settled turns are addressed by position, and the
/// cache outlives any one [`Tui`]: a counter starting from zero per instance
/// would let a second transcript read the first one's rows at the same index.
fn next_transcript_generation() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// State passed to renderers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ViewState<'a> {
    pub active_transcript: TranscriptId,
    /// Bumped whenever retained conversations are dropped or replaced wholesale,
    /// so cached descriptions of settled turns cannot outlive the history they
    /// belong to.
    pub transcript_generation: u64,
    pub transcript_ids: Vec<TranscriptId>,
    /// Owner label for the active primary viewport.
    pub owner_label: &'a str,
    /// The editable prompt text.
    pub input: &'a str,
    /// Path-free media chips staged for the next turn (`[Image #N]`, …).
    pub media_chips: &'a [String],
    /// Whether the composer contains a recovered failed prompt that can be retried or discarded.
    pub recovered_failed_prompt: bool,
    /// Current terminal dimensions.
    pub size: (u16, u16),
    /// Whether the composed engine has an active turn.
    pub running: bool,
    /// Main-surface destination selected with Tab.
    pub surface_focus: SurfaceFocus,
    /// Whether the composer owns the visible terminal cursor.
    pub composer_cursor_visible: bool,
    /// Undispatched prompts shown in FIFO order.
    pub queue: Vec<&'a QueueEntry>,
    /// Selected queue index while [`SurfaceFocus::Queue`] is active.
    pub queue_selected: Option<usize>,
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
    /// Cached selectable index for the active transcript at the current row width.
    ///
    /// Shared across paint and mouse hit-testing so a drag does not rebuild the
    /// full grapheme index on every pointer event.
    pub(crate) selectable: SharedSelectable,
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
    /// Home directory, so the footer can collapse it to `~`.
    pub home: Option<&'a str>,
    /// Branch and working-tree size, refreshed outside the frame path.
    pub repository: Option<&'a RepositoryStatus>,
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
    /// Detail level the tool output cycle currently rests on.
    pub tool_detail: widgets::DisplayMode,
    /// Whether complete reasoning is collapsed according to the UI setting.
    pub collapse_thinking: bool,
    /// Whether the reader asked to see the settled turns the transcript elides.
    pub history_expanded: bool,
    /// The block keyboard navigation is standing on, when there is one.
    pub focused_call: Option<&'a str>,
    /// Open tool detail overlay, when full args/output are shown modally.
    pub tool_overlay: Option<&'a ToolDetailOverlay>,
    /// Whether this terminal renders OSC 8 hyperlinks.
    pub hyperlinks: bool,
    /// How much colour this terminal can be sent.
    pub color_level: widgets::ColorLevel,
    /// Which glyph set this terminal can show.
    pub unicode_level: widgets::UnicodeLevel,
    pub focus: TranscriptFocus,
    /// A bounded informational dialog rendered above the conversation.
    pub dialog: Option<&'a DialogView>,
    /// Redacted credential-entry presentation; it carries only a mask length and fixed error.
    secret_entry: Option<SecretEntryRender<'a>>,
    /// Active device-authentication flow kept outside generic dialogs so its actions remain local.
    device_auth: Option<DeviceAuthRender<'a>>,
    /// Open structured question set, borrowed rather than copied for the frame.
    ask_user: Option<AskUserRender<'a>>,
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
    /// What the turn is doing, as the single source of every activity label.
    pub turn_activity: TurnActivity<'a>,
}

/// Reads the already-collected repository state. Never blocks.
pub type RepositoryProbe = Arc<dyn Fn() -> Option<RepositoryStatus> + Send + Sync>;

/// Reads the directory the session's tools are working in. Never blocks.
///
/// The footer names a location, and a tool call can move the session out of
/// the root it started in, so the location is polled rather than set once.
pub type WorkingDirectoryProbe = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// How often the footer picks up a new repository reading.
const REPOSITORY_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Working-tree state of the session's repository.
///
/// Collected outside the frame path — a footer that shells out to git while
/// painting would make the whole surface as slow as the slowest `git status`.
/// An absent value therefore means "not known yet", never "clean".
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RepositoryStatus {
    pub branch: Option<String>,
    pub changed_files: u64,
    pub insertions: u64,
    pub deletions: u64,
}

impl RepositoryStatus {
    pub const fn is_dirty(&self) -> bool {
        self.changed_files > 0
    }
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
    /// Paste text and attachments into the composer and dismiss; never submits.
    FillComposer {
        text: String,
        attachments: Vec<PromptAttachment>,
    },
    Cancel,
    ToggleDetails,
}

/// Which prompt-memory store backs a composer-anchored overlay.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PromptOverlayKind {
    History,
    Stash,
}

/// Deepest lineage row that still moves right; deeper rows share this indent.
///
/// A lineage can be arbitrarily deep, but the dialog is not arbitrarily wide:
/// past this point another level of indent would cost more label than it buys
/// in structure.
const MAX_DIALOG_ENTRY_DEPTH: usize = 8;
/// Columns [`lineage_indent`] can claim at [`MAX_DIALOG_ENTRY_DEPTH`].
const MAX_LINEAGE_INDENT_CHARS: usize = 2 * MAX_DIALOG_ENTRY_DEPTH;
/// Label bound for an indented row, so the indent cannot eat the label's tail.
const LINEAGE_LABEL_LIMIT: usize = 128 + MAX_LINEAGE_INDENT_CHARS;

/// The lineage marker a row at `depth` is prefixed with.
fn lineage_indent(depth: usize) -> String {
    format!("{}└ ", "  ".repeat(depth.saturating_sub(1)))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DialogEntry {
    label: String,
    detail: Option<String>,
    search_text: Option<String>,
    selected_detail: Option<String>,
    action: Option<DialogEntryAction>,
    id: Option<String>,
    /// How deep this row sits in a lineage, for depth-prefixed flat lists.
    depth: usize,
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
            depth: 0,
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
            depth: 0,
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
            depth: 0,
        }
    }

    /// Attaches the row identity substituted into selected-key action templates.
    pub fn with_id(mut self, id: impl AsRef<str>) -> Self {
        self.id = Some(bounded_dialog_text(id.as_ref(), 128));
        self
    }

    /// Places the row at `depth` in a lineage and indents its label to match.
    ///
    /// A tree is drawn as a flat list of indented rows rather than a nested
    /// widget, for the reason the shortcut catalogue states: the dialog already
    /// filters rows, and a filtered tree would answer a search with rows whose
    /// parents are no longer on screen. The indent is baked into the label so
    /// filtering, measuring, and painting all keep seeing one row of text.
    pub fn with_depth(mut self, depth: usize) -> Self {
        let depth = depth.min(MAX_DIALOG_ENTRY_DEPTH);
        if depth == 0 {
            self.depth = 0;
            return self;
        }

        self.label = bounded_dialog_text(
            &format!("{}{}", lineage_indent(depth), self.label),
            LINEAGE_LABEL_LIMIT,
        );
        self.depth = depth;
        self
    }

    /// How deep this row sits in the lineage it was built for.
    pub const fn depth(&self) -> usize {
        self.depth
    }

    /// A row that states a fact and dispatches nothing.
    ///
    /// It still carries an action so the list will stop on it: dialog
    /// navigation walks actionable rows only, and a reference row the cursor
    /// skipped would be a row the reader cannot reach with the keyboard.
    pub fn reference(label: impl AsRef<str>, detail: impl AsRef<str>) -> Self {
        Self {
            label: bounded_dialog_text(label.as_ref(), 128),
            detail: Some(bounded_dialog_text(detail.as_ref(), 256)),
            search_text: Some(bounded_dialog_text(
                &format!("{} {}", label.as_ref(), detail.as_ref()),
                512,
            )),
            selected_detail: None,
            action: Some(DialogEntryAction::ToggleDetails),
            id: None,
            depth: 0,
        }
    }

    pub fn cancel(label: impl AsRef<str>) -> Self {
        Self {
            label: bounded_dialog_text(label.as_ref(), 128),
            detail: None,
            search_text: None,
            selected_detail: None,
            action: Some(DialogEntryAction::Cancel),
            id: None,
            depth: 0,
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
            depth: 0,
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
            depth: 0,
        }
    }

    /// Overlay row that pastes `text` and `attachments` into the composer without submitting.
    fn fill_composer(
        label: impl AsRef<str>,
        detail: Option<&str>,
        text: impl Into<String>,
        attachments: Vec<PromptAttachment>,
        id: Option<String>,
    ) -> Self {
        let text = text.into();
        Self {
            label: bounded_dialog_text(label.as_ref(), 128),
            detail: detail.map(|detail| bounded_dialog_text(detail, 256)),
            search_text: Some(bounded_dialog_text(&text, 512)),
            selected_detail: None,
            action: Some(DialogEntryAction::FillComposer { text, attachments }),
            id: id.map(|id| bounded_dialog_text(&id, 128)),
            depth: 0,
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

/// A request for the lineage a session belongs to, as a forest of sessions.
///
/// The terminal cannot name a session: it is shown a label, not an identity.
/// `root` is therefore `None` when the request is about the session the reader
/// is in, and the composition layer resolves which one that is.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionTreeRequest {
    root: Option<String>,
    generation: u64,
}

impl SessionTreeRequest {
    /// The lineage of the session the terminal is currently attached to.
    pub const fn active() -> Self {
        Self {
            root: None,
            generation: 0,
        }
    }

    /// The lineage rooted at a session the caller can already name.
    pub fn for_root(root: impl AsRef<str>) -> Self {
        Self {
            root: Some(bounded_dialog_text(root.as_ref(), 128)),
            generation: 0,
        }
    }

    pub fn root(&self) -> Option<&str> {
        self.root.as_deref()
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }
}

/// A request to fork a session at a point the reader pointed at.
///
/// `turn_prefix` is a hint, not an authority: it counts the transcript turns
/// the terminal has drawn up to and including the fork point, which is the only
/// measure this crate can take. It is not a message count — the terminal never
/// sees the persisted messages a turn expands into — so the composition layer
/// re-derives and validates the real cut and may land on a different one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionForkRequest {
    session: Option<String>,
    turn_prefix: u64,
    generation: u64,
}

impl SessionForkRequest {
    /// Forks the session the terminal is currently attached to.
    pub const fn from_active_transcript(turn_prefix: u64) -> Self {
        Self {
            session: None,
            turn_prefix,
            generation: 0,
        }
    }

    /// Forks a session the caller can already name, such as a browsed row.
    pub fn for_session(session: impl AsRef<str>, turn_prefix: u64) -> Self {
        Self {
            session: Some(bounded_dialog_text(session.as_ref(), 128)),
            turn_prefix,
            generation: 0,
        }
    }

    pub fn session(&self) -> Option<&str> {
        self.session.as_deref()
    }

    pub const fn turn_prefix(&self) -> u64 {
        self.turn_prefix
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SessionTreeEntries {
    request: SessionTreeRequest,
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
    /// Lineage-browser state, when this dialog is a session tree page.
    tree_entries: Option<SessionTreeEntries>,
    query_action: Option<DialogQueryAction>,
    refresh_id: Option<String>,
    details_open: bool,
    empty_message: Option<String>,
    cancellation_action: Option<String>,
    shortcut_actions: Vec<(char, String)>,
    selected_key_actions: Vec<(Key, String)>,
    overlay_kind: widgets::OverlayKind,
    /// When set, size/anchor like the slash palette above the composer.
    composer_anchored: bool,
    /// Prompt history/stash overlay source for filter rebuild and local keys.
    prompt_overlay: Option<PromptOverlayKind>,
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
            // Preserve newlines so confirm bodies (full bash commands, paths)
            // can wrap across rows instead of collapsing into one caption.
            help: help.map(|help| bounded_dialog_multiline(help.as_ref(), 4_096)),
            entries,
            query: String::new(),
            searching: false,
            selected,
            offset: 0,
            interactive: true,
            session_entries: None,
            tree_entries: None,
            query_action: None,
            refresh_id: None,
            details_open: false,
            empty_message: None,
            cancellation_action: None,
            shortcut_actions: Vec::new(),
            selected_key_actions: Vec::new(),
            overlay_kind: widgets::OverlayKind::Picker,
            composer_anchored: false,
            prompt_overlay: None,
        }
    }

    /// Rows the dialog actually kept, after its own bound was applied.
    pub fn entry_count(&self) -> usize {
        self.entries.len()
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

    /// One page of a session lineage, as a depth-prefixed flat list.
    ///
    /// The caller indents each row with [`DialogEntry::with_depth`]; this keeps
    /// the same bound every other dialog page keeps, so a forest wider than the
    /// cap loses its tail here rather than growing an unbounded overlay. Paging
    /// past the cap is the composition layer's job.
    pub fn session_tree_page(entries: Vec<DialogEntry>, request: SessionTreeRequest) -> Self {
        let entries = entries.into_iter().take(64).collect::<Vec<_>>();
        let mut dialog = Self::selection(
            SESSION_TREE_TITLE,
            Some("Enter resume · / search · Esc close"),
            entries,
        );
        dialog.tree_entries = Some(SessionTreeEntries {
            request,
            loading: false,
            error: None,
        });
        dialog
    }

    pub fn session_tree_loading(request: SessionTreeRequest) -> Self {
        let mut dialog = Self::session_tree_page(Vec::new(), request);
        if let Some(tree_entries) = dialog.tree_entries.as_mut() {
            tree_entries.loading = true;
        }
        dialog
    }

    pub fn session_tree_error(request: SessionTreeRequest, message: impl AsRef<str>) -> Self {
        let mut dialog = Self::session_tree_page(Vec::new(), request);
        if let Some(tree_entries) = dialog.tree_entries.as_mut() {
            tree_entries.error = Some(bounded_dialog_text(message.as_ref(), 256));
        }
        dialog
    }

    pub fn is_loading(&self) -> bool {
        self.session_entries
            .as_ref()
            .is_some_and(|entries| entries.loading)
            || self
                .tree_entries
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
            // Bodies carry "message\nAction: …"; collapsing newlines made Action
            // run into the message (unavailableAction:). Multiline keeps layout.
            help: Some(bounded_dialog_multiline(body.as_ref(), 2_048)),
            entries: Vec::new(),
            query: String::new(),
            searching: false,
            selected: 0,
            offset: 0,
            interactive: false,
            session_entries: None,
            tree_entries: None,
            query_action: None,
            refresh_id: None,
            details_open: false,
            empty_message: None,
            cancellation_action: None,
            shortcut_actions: Vec::new(),
            selected_key_actions: Vec::new(),
            overlay_kind: widgets::OverlayKind::Picker,
            composer_anchored: false,
            prompt_overlay: None,
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

const SESSION_TREE_TITLE: &str = "Session tree";

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
    // Session pages and prompt-memory overlays already filter server/store-side.
    if dialog.session_entries.is_some() || dialog.prompt_overlay.is_some() {
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

const PROMPT_OVERLAY_LIMIT: usize = 64;

fn history_overlay_entries(memory: &dyn PromptMemory, query: &str) -> Vec<DialogEntry> {
    memory
        .history_overlay(query, PROMPT_OVERLAY_LIMIT)
        .into_iter()
        .map(|entry| {
            let detail = prompt_entry_detail_label(entry.created_at, entry.attachments.len());
            DialogEntry::fill_composer(
                prompt_overlay_row_label(&entry.text, entry.attachments.len()),
                Some(&detail),
                entry.text.clone(),
                entry.attachments,
                None,
            )
        })
        .collect()
}

fn stash_overlay_entries(memory: &dyn PromptMemory, query: &str) -> Vec<DialogEntry> {
    memory
        .stash_overlay(query, PROMPT_OVERLAY_LIMIT)
        .into_iter()
        .map(|entry| {
            let detail = prompt_entry_detail_label(entry.created_at, entry.attachments.len());
            DialogEntry::fill_composer(
                prompt_overlay_row_label(&entry.text, entry.attachments.len()),
                Some(&detail),
                entry.text.clone(),
                entry.attachments,
                Some(entry.store_index.to_string()),
            )
        })
        .collect()
}

/// Overlay row label; attachment-only entries would otherwise render blank.
fn prompt_overlay_row_label(text: &str, attachment_count: usize) -> String {
    if text.is_empty() && attachment_count > 0 {
        "(attachments only)".to_owned()
    } else {
        text.to_owned()
    }
}

/// Path-free chip labels (`[Image #N]` / `[File #N]`) for a staged attachment set.
fn attachment_chip_labels(attachments: &[PromptAttachment]) -> Vec<String> {
    attachments
        .iter()
        .enumerate()
        .map(|(index, attachment)| media_chip_label(index + 1, &attachment.mime))
        .collect()
}

/// Right label for an overlay row: attachment count marker plus the date.
fn prompt_entry_detail_label(created_at: i64, attachment_count: usize) -> String {
    let date = prompt_entry_date_label(created_at);
    if attachment_count > 0 {
        format!("{attachment_count} att · {date}")
    } else {
        date
    }
}

/// Date portion (`YYYY-MM-DD`) of a unix-seconds timestamp for overlay right labels.
fn prompt_entry_date_label(created_at: i64) -> String {
    format_unix_secs_rfc3339(created_at)
        .get(..10)
        .unwrap_or("")
        .to_owned()
}

/// Format unix seconds as `YYYY-MM-DDTHH:MM:SSZ` for overlay labels.
fn format_unix_secs_rfc3339(secs: i64) -> String {
    let secs = u64::try_from(secs.max(0)).unwrap_or(0);

    const SECS_PER_DAY: u64 = 86_400;
    const DAYS_PER_CYCLE: u64 = 146_097;
    const SECS_PER_HOUR: u64 = 3_600;
    const SECS_PER_MIN: u64 = 60;

    let days = secs / SECS_PER_DAY;
    let day_secs = secs % SECS_PER_DAY;
    let hour = day_secs / SECS_PER_HOUR;
    let minute = (day_secs % SECS_PER_HOUR) / SECS_PER_MIN;
    let second = day_secs % SECS_PER_MIN;

    // Civil date from days since Unix epoch (1970-01-01), Howard Hinnant algorithm.
    let z = days + 719_468;
    let era = z / DAYS_PER_CYCLE;
    let doe = z - era * DAYS_PER_CYCLE;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };

    format!("{year:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}Z")
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
    render_frame_content(frame, &state);
    // Last, over the finished frame: the buffer is the one place every colour
    // has arrived, including the ones this crate never chose.
    let _perf_quantize = agens_perf::span!(
        "tui.frame.quantize",
        color_level = state.color_level.trace_label(),
    );
    widgets::quantize_buffer(frame.buffer_mut(), state.color_level);
}

fn render_frame_content(frame: &mut ratatui::Frame<'_>, state: &ViewState<'_>) {
    let _perf_content = agens_perf::span!("tui.frame.content");
    let area = frame.area();
    let notice = notice_spans(state);
    let layout = {
        let _perf_layout = agens_perf::span!("tui.frame.layout");
        screen_layout(
            area,
            state.input,
            state.media_chips.len(),
            state.queue.len(),
        )
    };

    let row_width = layout
        .transcript
        .width
        .saturating_sub(TRANSCRIPT_ROW_INDENT)
        .max(1);
    // The frame this renderer was handed is the only authority on width. The
    // cached index is laid out from the width the state last knew about, and
    // painting rows wrapped for a wider frame would let the buffer clip prose
    // away with no ellipsis to say anything was lost.
    let cached = state.selectable.arc();
    let transcript = if cached.row_width == row_width {
        cached
    } else {
        Arc::new(SelectableTranscript::from_lines(
            &rendered_transcript_content(state, row_width),
            row_width,
        ))
    };
    // Live status is not in the selectable cache: rebuild only the few status
    // rows from the current clock so spinner and elapsed keep moving.
    let live_status = {
        let separate = transcript
            .rows
            .last()
            .is_some_and(|row| row.cells.iter().any(|cell| !cell.text.trim().is_empty()));
        let lines = live_turn_status_lines(state, row_width, separate);
        if lines.is_empty() {
            None
        } else {
            Some(SelectableTranscript::from_lines(&lines, row_width))
        }
    };
    let live_status_rows = live_status
        .as_ref()
        .map(|status| status.rows.len())
        .unwrap_or(0);
    let total_rows = transcript.total_rows().saturating_add(live_status_rows);
    let _perf_select = agens_perf::span!("tui.transcript.select", rows = total_rows as u64);
    let visible_rows = layout
        .transcript
        .height
        .saturating_sub(transcript_chrome_rows(state.following_bottom))
        as usize;
    let bottom_scroll = saturating_u16(total_rows.saturating_sub(visible_rows));
    let scroll = if state.following_bottom {
        bottom_scroll
    } else {
        state.scroll_offset.min(bottom_scroll)
    };
    if layout.transcript.height > 0 {
        let mut transcript_block = Block::default()
            .borders(Borders::TOP)
            .padding(Padding::left(TRANSCRIPT_ROW_INDENT))
            .border_style(Style::default().fg(widgets::RolePalette::chrome()));
        if !state.following_bottom {
            transcript_block = transcript_block
                .title_bottom(Span::styled(
                    format!(" SCROLL {scroll}/{bottom_scroll}"),
                    Style::default().fg(widgets::RolePalette::chrome()),
                ))
                .title_alignment(Alignment::Right);
        }
        {
            let _perf_paint = agens_perf::span!("tui.transcript.paint", rows = total_rows as u64,);
            let mut lines = transcript.render_lines(state.selection);
            if let Some(status) = live_status.as_ref() {
                lines.extend(status.render_lines(None));
            }
            let window_scroll = scroll.saturating_sub(saturating_u16(transcript.first_row));
            frame.render_widget(
                Paragraph::new(Text::from(lines))
                    .block(transcript_block)
                    .scroll((window_scroll, 0)),
                layout.transcript,
            );
        }
        // After painting, not during: the pass reads back the laid-out rows, so
        // it is the one place every widget's output has already become columns.
        {
            let _perf_hyperlinks = agens_perf::span!("tui.transcript.hyperlinks");
            widgets::apply_hyperlinks(
                frame.buffer_mut(),
                layout.transcript,
                state.project,
                state.hyperlinks,
            );
        }
    }

    if layout.composer.height > 0 && state.active_transcript != TranscriptId::Main {
        let mut dock = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(widgets::RolePalette::chrome()));
        if let Some(metrics) = border_metrics(state, layout.composer) {
            dock = dock.title_top(metrics);
        }
        frame.render_widget(
            Paragraph::new(" Subagent transcript · i to message · x to cancel")
                .style(Style::default().fg(widgets::RolePalette::chrome()))
                .block(dock),
            layout.composer,
        );
    }

    let composer_color = widgets::RolePalette::muted();
    if layout.composer.height > 0 && state.active_transcript == TranscriptId::Main {
        // Two borders plus one blank column inside each, so the text neither
        // touches the frame nor loses the column the prose above it starts on.
        let padding = composer_padding(layout.composer.width);
        let visible_inner_width = layout
            .composer
            .width
            .saturating_sub(composer_frame_columns(layout.composer.width));
        let visible_inner_height = layout.composer.height.saturating_sub(2);
        let inner_width = usize::from(visible_inner_width.max(1));
        let inner_height = usize::from(visible_inner_height.max(1));
        let attachment_rows = attachment_preview_lines(state.media_chips, inner_width);
        let composer_layout = composer_layout(state.input, state.input_cursor, inner_width);
        let cursor_line = composer_layout.cursor_line;
        let cursor_column = composer_layout.cursor_column;
        let attachment_row_count = attachment_rows.len();
        let cursor_content_line = attachment_row_count.saturating_add(cursor_line);
        let vertical_scroll = cursor_content_line.saturating_sub(inner_height.saturating_sub(1));
        let content_rows = attachment_row_count.saturating_add(composer_layout.rows);
        let mut composer = Block::default()
            .borders(Borders::ALL)
            .padding(padding)
            .border_style(Style::default().fg(composer_color));
        if let Some(metrics) = border_metrics(state, layout.composer) {
            composer = composer.title_bottom(metrics);
        }
        if let Some(hidden) = hidden_rows_marker(
            content_rows,
            vertical_scroll,
            inner_height,
            layout.composer.width,
        ) {
            composer = composer.title_top(hidden);
        }
        let mut composer_text = attachment_rows
            .into_iter()
            .map(|line| Line::styled(line, Style::default().fg(widgets::RolePalette::muted())))
            .collect::<Vec<_>>();
        composer_text.extend(
            composer_layout
                .text
                .lines()
                .map(|line| Line::from(line.to_owned())),
        );
        frame.render_widget(
            Paragraph::new(Text::from(composer_text))
                .block(composer)
                .scroll((saturating_u16(vertical_scroll), 0)),
            layout.composer,
        );
        if visible_inner_width > 0
            && visible_inner_height > 0
            && state.focus == TranscriptFocus::Composer
            && state.surface_focus == SurfaceFocus::Composer
            && !state.session_loading
            && state.dialog.is_none()
            && state.palette.is_none()
        {
            let cursor_y = layout
                .composer
                .y
                .saturating_add(1)
                .saturating_add(saturating_u16(
                    cursor_content_line.saturating_sub(vertical_scroll),
                ));
            let cursor_x = layout
                .composer
                .x
                .saturating_add(saturating_u16(cursor_column.saturating_add(2)));
            frame.set_cursor_position((
                cursor_x.min(area.width.saturating_sub(1)),
                cursor_y.min(area.height.saturating_sub(1)),
            ));
        }
    }

    if layout.queue.height > 0 {
        render_queue(frame, layout.queue, state);
    }

    if layout.notice.height > 0 {
        // The band belongs to whatever is most urgent. A warning outranks a
        // legend, so the hints yield the row rather than share it.
        let band = if notice.is_empty() {
            hint_spans(state)
        } else {
            notice
        };
        render_notice(frame, layout.notice, band);
    }

    if layout.tree.height > 0 {
        frame.render_widget(
            Paragraph::new(Text::from(fitted_subagent_tree_lines(
                state,
                layout.tree.height,
                layout.tree.width,
            ))),
            layout.tree,
        );
    }

    if layout.footer.height > 0 {
        let _perf_footer = agens_perf::span!("tui.footer");
        frame.render_widget(
            Paragraph::new(Line::from(widgets::MetricFooter::spans(
                layout.footer.width,
                footer_context(state),
            ))),
            layout.footer,
        );
    }

    if let Some(dialog) = state.dialog {
        render_dialog(frame, area, layout.composer, dialog);
    }

    if let Some(palette) = state.palette {
        render_palette(frame, area, layout.composer, state.input, palette);
    }

    if let Some(picker) = state.file_picker {
        render_file_picker(frame, area, layout.composer, picker);
    }

    if let Some(ask_user) = state.ask_user {
        render_ask_user(frame, area, layout.composer, ask_user);
    }

    if let Some(secret_entry) = state.secret_entry {
        render_secret_entry(frame, area, secret_entry);
    }

    if let Some(device_auth) = state.device_auth {
        render_device_auth(frame, area, device_auth);
    }

    if let Some(overlay) = state.tool_overlay {
        let arguments = state
            .tool_display_modes
            .get(&overlay.call_id)
            .copied()
            .unwrap_or(state.tool_detail);
        render_tool_detail_overlay(frame, area, overlay, arguments);
    }
}

/// Draws the tool detail modal.
///
/// `arguments` is the detail level the reader last asked for on this call. Only
/// [`widgets::DisplayMode::Expanded`] draws every argument row: the other levels hide
/// a body in the transcript, which the overlay must never do — it exists to show
/// what the call carried — so they draw the bounded, marked preview instead.
fn render_tool_detail_overlay(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    overlay: &ToolDetailOverlay,
    arguments: widgets::DisplayMode,
) {
    let shortcuts = [
        widgets::OverlayShortcut {
            key: "Esc",
            label: "close",
        },
        widgets::OverlayShortcut {
            key: "↑↓",
            label: "scroll",
        },
    ];
    let title = if overlay.status.is_empty() {
        overlay.title.clone()
    } else {
        format!("{} · {}", overlay.title, overlay.status)
    };
    let config = widgets::OverlayConfig {
        title: &title,
        tabs: None,
        shortcuts: &shortcuts,
        sizing: widgets::OverlaySizing::tool_detail(),
        desired_content_rows: widgets::TOOL_DETAIL_CONTENT_ROWS,
    };
    let Some(layout) = widgets::OverlayLayout::solve(area, &config) else {
        return;
    };
    widgets::OverlayFrame::render(frame, &layout, &config);

    let mut lines = Vec::new();
    if !overlay.args.is_empty() {
        lines.push(Line::styled(
            "Arguments",
            Style::default()
                .fg(widgets::RolePalette::muted())
                .add_modifier(Modifier::BOLD),
        ));
        if arguments == widgets::DisplayMode::Expanded {
            lines.extend(overlay.args.lines().map(widgets::argument_line));
        } else {
            lines.extend(widgets::bounded_argument_preview(&overlay.args));
        }
        lines.push(Line::default());
    }
    lines.push(Line::styled(
        "Output",
        Style::default()
            .fg(widgets::RolePalette::muted())
            .add_modifier(Modifier::BOLD),
    ));
    if overlay.output.is_empty() {
        lines.push(Line::styled(
            "(no output)",
            Style::default().fg(widgets::RolePalette::muted()),
        ));
    } else {
        for line in overlay.output.lines() {
            lines.push(Line::styled(
                line.to_owned(),
                Style::default().fg(widgets::RolePalette::assistant()),
            ));
        }
    }

    let visible = usize::from(layout.content.height);
    let max_scroll = lines.len().saturating_sub(visible.max(1));
    let scroll = usize::from(overlay.scroll).min(max_scroll);
    frame.render_widget(
        Paragraph::new(Text::from(lines)).scroll((saturating_u16(scroll), 0)),
        layout.content,
    );
}

/// Inner overlay width at which the context earns a column of its own.
///
/// Composed rather than guessed: the narrowest option column that still shows a
/// label with its selection marker is 33 columns, the narrowest column that
/// wraps prose without turning it into a ladder is 32, and the divider gutter
/// between them costs [`ASK_USER_COLUMN_GAP`]. Full-width above-composer sizing
/// reaches this inner width on a classic 80-column terminal (minus gutters).
const ASK_USER_TWO_COLUMN_MIN_WIDTH: u16 = 68;
/// Space, divider rule, space between the two columns.
const ASK_USER_COLUMN_GAP: u16 = 3;
/// Wrapped context rows kept for scrolling, on top of the domain's own cap on
/// the source text.
const MAX_ASK_USER_CONTEXT_ROWS: usize = 200;
const MAX_ASK_USER_PROMPT_ROWS: usize = 3;
const MAX_ASK_USER_EXPLANATION_ROWS: usize = 2;
/// Shortest stacked context section worth carving out of the option list.
const MIN_ASK_USER_STACKED_ROWS: u16 = 3;
const ASK_USER_DEFAULT_TITLE: &str = "the agent needs an answer";
const ASK_USER_EMPTY_CONTEXT: &str = "no extra context for this option";
const ASK_USER_CONTEXT_KEYS: &str = "pgup/pgdn";

/// One frame's worth of resolved ask-user geometry and content.
///
/// Layout and paint are separated the same way [`widgets::OverlayLayout`]
/// separates them: everything here is decided before a single cell is written,
/// so the scroll ceiling the key handler enforces and the rows the renderer
/// paints are read off the same measurement.
struct AskUserFrame<'a> {
    title: String,
    shortcuts: Vec<widgets::OverlayShortcut<'static>>,
    layout: widgets::OverlayLayout,
    header: Rect,
    rule: Option<Rect>,
    list: Rect,
    divider: Option<Rect>,
    context_label: Option<Rect>,
    context: Option<Rect>,
    /// Reserved out of `context`, so it is `Some` exactly when the pane gave up
    /// a row for it.
    context_position: Option<Rect>,
    header_lines: Vec<Line<'static>>,
    rows: Vec<widgets::OverlayRow<'a>>,
    selected_row: usize,
    context_lines: Vec<String>,
    max_context_scroll: u16,
}

/// Whether the current question offers any per-option context to show.
///
/// When it does not, the pane is not merely empty — the two-column layout would
/// spend half the overlay on nothing, so the list keeps the whole width.
/// Review has no option rows, so the context column is never useful there.
fn ask_user_has_context(render: &AskUserRender<'_>) -> bool {
    if render.reviewing {
        return false;
    }
    render
        .current()
        .options()
        .iter()
        .any(|option| option.context().is_some())
}

fn ask_user_context_text<'a>(render: &AskUserRender<'a>) -> &'a str {
    render
        .current()
        .options()
        .get(render.context_option)
        .and_then(agens_core::ask_user::AskUserOption::context)
        .unwrap_or(ASK_USER_EMPTY_CONTEXT)
}

fn ask_user_shortcuts(render: &AskUserRender<'_>) -> Vec<widgets::OverlayShortcut<'static>> {
    let question = render.current();
    if render.entry != AskUserEntry::Browsing {
        return ask_user_entry_shortcuts(ask_user_has_context(render));
    }

    if render.reviewing {
        return vec![
            widgets::OverlayShortcut {
                key: "↑↓",
                label: "move",
            },
            widgets::OverlayShortcut {
                key: "Enter",
                label: "submit",
            },
            widgets::OverlayShortcut {
                key: "esc",
                label: "cancel",
            },
        ];
    }

    let mut shortcuts = vec![
        widgets::OverlayShortcut {
            key: "↑↓",
            label: "move",
        },
        widgets::OverlayShortcut {
            key: "Enter",
            label: "choose",
        },
    ];
    if render.request.questions().len() > 1 {
        shortcuts.push(widgets::OverlayShortcut {
            key: "tab",
            label: "question",
        });
    }
    shortcuts.push(widgets::OverlayShortcut {
        key: "o",
        label: "other",
    });
    if question.allow_note() {
        shortcuts.push(widgets::OverlayShortcut {
            key: "n",
            label: "note",
        });
    }
    if ask_user_has_context(render) {
        shortcuts.push(widgets::OverlayShortcut {
            key: ASK_USER_CONTEXT_KEYS,
            label: "context",
        });
    }
    shortcuts.push(widgets::OverlayShortcut {
        key: "esc",
        label: "cancel",
    });
    shortcuts
}

/// What the footer says while a free-form buffer is open.
///
/// The browsing shortcuts are actively wrong here — `⏎` does not choose an
/// option, `esc` does not cancel the question, and the arrows do not move
/// between rows — so the row is replaced rather than appended to.
fn ask_user_entry_shortcuts(has_context: bool) -> Vec<widgets::OverlayShortcut<'static>> {
    let mut shortcuts = vec![
        widgets::OverlayShortcut {
            key: "←→",
            label: "move",
        },
        widgets::OverlayShortcut {
            key: "ctrl-w",
            label: "word",
        },
        widgets::OverlayShortcut {
            key: "Enter",
            label: "done",
        },
        widgets::OverlayShortcut {
            key: "esc",
            label: "back",
        },
    ];
    if has_context {
        shortcuts.push(widgets::OverlayShortcut {
            key: ASK_USER_CONTEXT_KEYS,
            label: "context",
        });
    }
    shortcuts
}

/// ASCII characters a drawing is built out of.
///
/// Each one also occurs in ordinary prose, which is why no single occurrence
/// decides anything — only their density does.
const ASK_USER_ASCII_ART: [char; 8] = ['|', '+', '-', '/', '\\', '>', '<', '='];
/// Share of a line's visible characters that must be drawing characters before
/// the line is read as art. One in three is far above what prose reaches with
/// a hyphen or two and far below what any box or arrow row falls to.
const ASK_USER_ART_DENSITY: usize = 3;
/// Consecutive interior spaces that mark alignment rather than sentence
/// spacing. Two is what a typist leaves after a full stop; three is a column.
const ASK_USER_ALIGNMENT_RUN: &str = "   ";

/// Whether a line's own spacing carries meaning, so it must never be re-flowed.
///
/// Every row of a diagram is positioned relative to the rows above it, so
/// wrapping one row silently misaligns all of them — and the wrap also collapses
/// the interior space runs the drawing is made of. The test is structural rather
/// than a character range, because the diagram a model is most likely to draw is
/// `+---+` and `-->`, not `┌───┐`:
///
/// 1. a box-drawing, block, geometric or arrow glyph, which prose never carries;
/// 2. a run of three or more interior spaces, which is alignment, not prose;
/// 3. ASCII drawing characters at [`ASK_USER_ART_DENSITY`] of the visible
///    characters, which catches `+-----+-----+` and leaves a sentence with a
///    hyphen and a pipe in it alone.
fn ask_user_line_is_preformatted(line: &str) -> bool {
    if line
        .chars()
        .any(|character| matches!(character, '\u{2190}'..='\u{21ff}' | '\u{2500}'..='\u{25ff}'))
    {
        return true;
    }
    if line.trim().contains(ASK_USER_ALIGNMENT_RUN) {
        return true;
    }

    let visible = line.chars().filter(|character| !character.is_whitespace());
    let (art, total) = visible.fold((0usize, 0usize), |(art, total), character| {
        (
            art + usize::from(ASK_USER_ASCII_ART.contains(&character)),
            total + 1,
        )
    });
    total > 0 && art * ASK_USER_ART_DENSITY >= total
}

/// Greedy wrap measured in display columns.
///
/// Distinct from [`wrapped_prose_lines`], which counts `char`s: a pane is
/// measured in columns, and a paragraph of double-width glyphs wrapped by
/// character count produces rows twice as wide as the pane, whose overflow the
/// terminal silently clips — losing text no keypress can scroll to. Text with no
/// ASCII space to break at is broken between characters for the same reason: an
/// unwrappable paragraph must still be readable, not truncated.
fn ask_user_wrapped_lines(source: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut used = 0usize;

    for word in source.split_whitespace() {
        let word_width = word.width();
        if used > 0 && used + 1 + word_width <= width {
            current.push(' ');
            current.push_str(word);
            used += 1 + word_width;
            continue;
        }

        if used > 0 {
            lines.push(std::mem::take(&mut current));
            used = 0;
        }
        if word_width <= width {
            current.push_str(word);
            used = word_width;
            continue;
        }

        for character in word.chars() {
            let columns = UnicodeWidthChar::width(character).unwrap_or(0);
            if used > 0 && used + columns > width {
                lines.push(std::mem::take(&mut current));
                used = 0;
            }
            current.push(character);
            used += columns;
        }
    }

    lines.push(current);
    lines
}

/// Wraps context for a pane of `width` columns.
///
/// A source line that already fits is painted verbatim, indentation included,
/// because that is the only way a drawing survives at all. A line that does not
/// fit is cut with `…` when its spacing is load-bearing and wrapped otherwise: a
/// clipped diagram is still readable, a re-flowed one is noise, and a clipped
/// paragraph is text the reader can never get back.
fn ask_user_context_lines(text: &str, width: u16) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }

    let budget = usize::from(width);
    let mut lines = Vec::new();
    for source in text.lines() {
        if source.width() <= budget {
            lines.push(source.to_owned());
        } else if ask_user_line_is_preformatted(source) {
            lines.push(widgets::truncate_columns(source, budget));
        } else {
            lines.extend(ask_user_wrapped_lines(source, budget));
        }

        if lines.len() >= MAX_ASK_USER_CONTEXT_ROWS {
            lines.truncate(MAX_ASK_USER_CONTEXT_ROWS);
            break;
        }
    }
    lines
}

fn ask_user_header_lines(render: &AskUserRender<'_>, width: u16) -> Vec<Line<'static>> {
    let muted = Style::default().fg(widgets::RolePalette::muted());
    let total = render.request.questions().len();
    let status = vec![
        Span::styled(
            if render.reviewing {
                format!("Review · {total} questions")
            } else {
                format!("Question {}/{total}", render.question + 1)
            },
            Style::default().fg(widgets::RolePalette::navigation()),
        ),
        Span::styled("  ·  ", muted),
        Span::styled(
            format!("{} of {total} answered", render.answered),
            if render.answered == total {
                Style::default().fg(widgets::RolePalette::success())
            } else {
                muted
            },
        ),
    ];

    let mut lines = vec![Line::from(status)];
    if render.reviewing {
        lines.push(Line::styled(
            "Review your answers".to_owned(),
            Style::default()
                .fg(widgets::RolePalette::assistant())
                .add_modifier(Modifier::BOLD),
        ));
        return lines;
    }

    let question = render.current();
    lines.extend(
        wrapped_prose_lines(question.prompt(), width)
            .into_iter()
            .take(MAX_ASK_USER_PROMPT_ROWS)
            .map(|line| {
                Line::styled(
                    line,
                    Style::default()
                        .fg(widgets::RolePalette::assistant())
                        .add_modifier(Modifier::BOLD),
                )
            }),
    );
    if let Some(explanation) = question.explanation() {
        lines.extend(
            wrapped_prose_lines(explanation, width)
                .into_iter()
                .take(MAX_ASK_USER_EXPLANATION_ROWS)
                .map(|line| Line::styled(line, muted)),
        );
    }
    lines
}

/// The option list, its per-option sub-lines, the free-form buffers and the
/// action rows, plus the index of the row the cursor stands on.
///
/// `width` is the list column's own width, which only the buffer rows consult:
/// they are the only rows whose content depends on how much room there is to
/// show it.
fn ask_user_rows<'a>(
    render: &AskUserRender<'a>,
    width: u16,
) -> (Vec<widgets::OverlayRow<'a>>, usize) {
    if render.reviewing {
        return ask_user_review_rows(render);
    }

    let question = render.current();
    let multiple = question.mode() == AskUserMode::Multiple;
    let mut rows: Vec<widgets::OverlayRow<'a>> = Vec::new();
    let mut selected_row = 0;

    for (index, option) in question.options().iter().enumerate() {
        let chosen = render.selected.contains(&index);
        let marker = match (multiple, chosen) {
            (true, true) => "[x]",
            (true, false) => "[ ]",
            (false, true) => "(•)",
            (false, false) => "( )",
        };
        let highlighted = render.row == AskUserRow::Option(index);
        if highlighted {
            selected_row = rows.len();
        }
        rows.push(widgets::OverlayRow {
            label: Cow::Borrowed(option.label()),
            badge: Some(marker),
            selected: highlighted,
            ..widgets::OverlayRow::default()
        });
        if let Some(explanation) = option.explanation() {
            rows.push(widgets::OverlayRow {
                label: Cow::Borrowed(explanation),
                indent: 1,
                dimmed: true,
                ..widgets::OverlayRow::default()
            });
        }
    }

    rows.push(ask_user_buffer_row(
        "other",
        render.other,
        (render.entry == AskUserEntry::Other).then_some(render.entry_cursor),
        "press o to type your own answer",
        width,
    ));
    if question.allow_note() {
        rows.push(ask_user_buffer_row(
            "note",
            render.note,
            (render.entry == AskUserEntry::Note).then_some(render.entry_cursor),
            "press n to add a note",
            width,
        ));
    }

    rows.push(widgets::OverlayRow::new(""));
    let last = render.question + 1 == render.request.questions().len();
    let mut action = |row: AskUserRow, label: &'static str| {
        let highlighted = render.row == row;
        if highlighted {
            selected_row = rows.len();
        }
        rows.push(widgets::OverlayRow {
            label: Cow::Borrowed(label),
            selected: highlighted,
            ..widgets::OverlayRow::default()
        });
    };
    // Last question opens review rather than submitting immediately, so the
    // reader can still check skipped items before the final commit.
    if last {
        action(AskUserRow::Proceed, "Review answers");
    } else {
        action(AskUserRow::Proceed, "Next question");
    }
    if question.allow_discuss() {
        action(AskUserRow::Discuss, "Discuss this in chat instead");
    }
    action(AskUserRow::Cancel, "Cancel");

    (rows, selected_row)
}

/// Review-mode list: every question with its chosen answer, then Submit/Cancel.
fn ask_user_review_rows<'a>(render: &AskUserRender<'a>) -> (Vec<widgets::OverlayRow<'a>>, usize) {
    let mut rows: Vec<widgets::OverlayRow<'a>> = Vec::new();
    let mut selected_row = 0;

    for (index, question) in render.request.questions().iter().enumerate() {
        let selected = render.selections.get(index);
        let other = render.others.get(index).map(String::as_str).unwrap_or("");
        let note = render.notes.get(index).map(String::as_str).unwrap_or("");

        rows.push(widgets::OverlayRow {
            label: Cow::Owned(question.prompt().to_owned()),
            dimmed: false,
            ..widgets::OverlayRow::default()
        });
        rows.push(widgets::OverlayRow {
            label: Cow::Owned(format!(
                "→ {}",
                ask_user_answer_summary(question, selected, other)
            )),
            indent: 1,
            dimmed: true,
            ..widgets::OverlayRow::default()
        });
        if !note.trim().is_empty() {
            rows.push(widgets::OverlayRow {
                label: Cow::Owned(format!("note: {note}")),
                indent: 1,
                dimmed: true,
                ..widgets::OverlayRow::default()
            });
        }
    }

    rows.push(widgets::OverlayRow::new(""));

    let mut action = |row: AskUserRow, label: &'static str| {
        let highlighted = render.row == row;
        if highlighted {
            selected_row = rows.len();
        }
        rows.push(widgets::OverlayRow {
            label: Cow::Borrowed(label),
            selected: highlighted,
            ..widgets::OverlayRow::default()
        });
    };
    action(AskUserRow::Proceed, "Submit answers");
    action(AskUserRow::Cancel, "Cancel");

    (rows, selected_row)
}

/// Human label for one question's stored answer (options, free-text, or skip).
fn ask_user_answer_summary(
    question: &AskUserQuestion,
    selected: Option<&BTreeSet<usize>>,
    other: &str,
) -> String {
    let mut parts = Vec::new();
    if let Some(selected) = selected {
        for index in selected {
            if let Some(option) = question.options().get(*index) {
                parts.push(option.label().to_owned());
            }
        }
    }
    let other = other.trim();
    if !other.is_empty() {
        parts.push(other.to_owned());
    }
    if parts.is_empty() {
        "(skipped)".to_owned()
    } else {
        parts.join(", ")
    }
}

/// The caret drawn at the insertion point of an open buffer.
const ASK_USER_CARET: &str = "▏";

/// One free-form buffer row — `other` or `note` — for a list `width` columns
/// wide.
///
/// `cursor` is `Some` exactly while this is the buffer being typed into, and
/// carries the caret's `char` index. That is what makes the row width-aware:
/// an unedited buffer may simply run past the edge and be truncated like any
/// other label, but the row someone is typing in has to keep showing the part
/// they are typing, which is only decidable against the columns available.
fn ask_user_buffer_row<'a>(
    field: &str,
    buffer: &str,
    cursor: Option<usize>,
    hint: &str,
    width: u16,
) -> widgets::OverlayRow<'a> {
    let Some(cursor) = cursor else {
        let label = if buffer.is_empty() {
            format!("{field}: {hint}")
        } else {
            format!("{field}: {buffer}")
        };
        return widgets::OverlayRow {
            label: Cow::Owned(label),
            dimmed: buffer.is_empty(),
            ..widgets::OverlayRow::default()
        };
    };

    let prefix = format!("{field}: ");
    let available = usize::from(width.saturating_sub(widgets::ROW_LABEL_RESERVE))
        .saturating_sub(prefix.width() + ASK_USER_CARET.width());
    let (visible, caret_offset) = ask_user_entry_window(buffer, cursor, available);

    let mut label = prefix;
    label.extend(visible.chars().take(caret_offset));
    label.push_str(ASK_USER_CARET);
    label.extend(visible.chars().skip(caret_offset));

    widgets::OverlayRow {
        label: Cow::Owned(label),
        ..widgets::OverlayRow::default()
    }
}

/// Chooses the slice of `buffer` to show in `available` columns, returning it
/// with the caret's `char` offset inside that slice.
///
/// The caret is always inside the returned slice: that is the whole point of
/// the window. Trailing text is served first, up to half the room, so moving
/// the caret back into a long answer shows what follows it instead of pinning
/// it to the right edge; the rest of the room goes to the text before the
/// caret, and anything left over back to the text after it.
fn ask_user_entry_window(buffer: &str, cursor: usize, available: usize) -> (String, usize) {
    let characters: Vec<char> = buffer.chars().collect();
    let cursor = cursor.min(characters.len());
    if available == 0 {
        return (String::new(), 0);
    }

    let mut used = 0usize;
    let mut end = cursor;
    while end < characters.len() {
        let columns = UnicodeWidthChar::width(characters[end]).unwrap_or(0);
        if used + columns > available / 2 {
            break;
        }
        used += columns;
        end += 1;
    }

    let mut start = cursor;
    while start > 0 {
        let columns = UnicodeWidthChar::width(characters[start - 1]).unwrap_or(0);
        if used + columns > available {
            break;
        }
        used += columns;
        start -= 1;
    }

    while end < characters.len() {
        let columns = UnicodeWidthChar::width(characters[end]).unwrap_or(0);
        if used + columns > available {
            break;
        }
        used += columns;
        end += 1;
    }

    (characters[start..end].iter().collect(), cursor - start)
}

/// Keeps the cursor's row on screen with the least scrolling that achieves it.
const fn ask_user_list_offset(selected: usize, height: usize, total: usize) -> usize {
    if height == 0 || total <= height {
        return 0;
    }
    if selected < height {
        0
    } else {
        let last = total - height;
        let wanted = selected + 1 - height;
        if wanted < last { wanted } else { last }
    }
}

/// Resolves the whole overlay before anything is painted.
#[allow(clippy::too_many_lines)]
fn ask_user_frame<'a>(
    area: Rect,
    composer: Rect,
    render: &AskUserRender<'a>,
) -> Option<AskUserFrame<'a>> {
    let sizing = widgets::OverlaySizing::ask_user(composer);
    let inner = sizing.inner_width(area)?;
    let has_context = ask_user_has_context(render);
    let two_column = has_context && inner >= ASK_USER_TWO_COLUMN_MIN_WIDTH;
    let (list_width, context_width) = if two_column {
        let right = (inner - ASK_USER_COLUMN_GAP) / 2;
        (inner - ASK_USER_COLUMN_GAP - right, right)
    } else {
        (inner, inner)
    };

    let header_lines = ask_user_header_lines(render, inner);
    let (rows, selected_row) = ask_user_rows(render, list_width);
    let context_lines = if has_context {
        ask_user_context_lines(ask_user_context_text(render), context_width)
    } else {
        Vec::new()
    };

    let body_rows = if two_column {
        rows.len().max(context_lines.len())
    } else if has_context {
        rows.len() + context_lines.len() + 1
    } else {
        rows.len()
    };
    let shortcuts = ask_user_shortcuts(render);
    let title = prompt_title(
        render.request.title().unwrap_or(ASK_USER_DEFAULT_TITLE),
        render.origin,
    );
    let layout = widgets::OverlayLayout::solve(
        area,
        &widgets::OverlayConfig {
            title: &title,
            tabs: None,
            shortcuts: &shortcuts,
            sizing,
            desired_content_rows: saturating_u16(header_lines.len() + 1 + body_rows),
        },
    )?;

    let content = layout.content;
    let header_rows = saturating_u16(header_lines.len()).min(content.height.saturating_sub(1));
    let header = Rect::new(content.x, content.y, content.width, header_rows);
    let mut cursor = content.y + header_rows;
    let mut remaining = content.height - header_rows;
    let rule = (remaining >= 3).then(|| {
        cursor += 1;
        remaining -= 1;
        Rect::new(content.x, cursor - 1, content.width, 1)
    });
    let body = Rect::new(content.x, cursor, content.width, remaining);

    let (list, divider, context_label, mut context) = if two_column {
        (
            Rect::new(body.x, body.y, list_width, body.height),
            Some(Rect::new(body.x + list_width + 1, body.y, 1, body.height)),
            None,
            Some(Rect::new(
                body.x + list_width + ASK_USER_COLUMN_GAP,
                body.y,
                context_width,
                body.height,
            )),
        )
    } else if has_context && body.height >= 2 * MIN_ASK_USER_STACKED_ROWS {
        let (list, label, pane) = ask_user_stacked_split(body, rows.len(), context_lines.len());
        (list, None, Some(label), Some(pane))
    } else {
        (body, None, None, None)
    };

    // The scroll affordance costs a row, so it is taken out of the pane before
    // the ceiling is measured: the reader must never be told there is more
    // below on a row that is itself the last row of content. The reserved rect
    // is carried rather than recomputed at paint time, so the pane the renderer
    // fills and the row it writes the position on cannot disagree.
    let mut context_position = None;
    if let Some(pane) = context.as_mut()
        && context_lines.len() > usize::from(pane.height)
        && pane.height >= MIN_ASK_USER_STACKED_ROWS
    {
        pane.height -= 1;
        context_position = Some(Rect::new(pane.x, pane.y + pane.height, pane.width, 1));
    }
    let max_context_scroll = context.map_or(0, |pane| {
        saturating_u16(context_lines.len().saturating_sub(usize::from(pane.height)))
    });

    Some(AskUserFrame {
        title,
        shortcuts,
        layout,
        header,
        rule,
        list,
        divider,
        context_label,
        context,
        context_position,
        header_lines,
        rows,
        selected_row,
        context_lines,
        max_context_scroll,
    })
}

/// Divides a single-column body into the option list, the named divider and the
/// context section, as `(list, label, context)`.
///
/// The list is served first: an option set the reader has to scroll past before
/// discovering that Submit exists is a worse trade than a context section they
/// have already been told how to page through.
fn ask_user_stacked_split(body: Rect, rows: usize, context_lines: usize) -> (Rect, Rect, Rect) {
    let wanted = saturating_u16(context_lines + 1);
    let floor = MIN_ASK_USER_STACKED_ROWS + 1;
    let ceiling = (body.height / 2).max(floor);
    let block = body
        .height
        .saturating_sub(saturating_u16(rows))
        .clamp(floor, ceiling)
        .min(wanted.max(floor));
    let list_rows = body.height - block;

    (
        Rect::new(body.x, body.y, body.width, list_rows),
        Rect::new(body.x, body.y + list_rows, body.width, 1),
        Rect::new(body.x, body.y + list_rows + 1, body.width, block - 1),
    )
}

fn render_ask_user(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    composer: Rect,
    render: AskUserRender<'_>,
) {
    let Some(laid) = ask_user_frame(area, composer, &render) else {
        return;
    };
    let AskUserFrame {
        title,
        shortcuts,
        layout,
        header,
        rule,
        list,
        divider,
        context_label,
        context,
        context_position,
        header_lines,
        rows,
        selected_row,
        context_lines,
        max_context_scroll,
    } = laid;

    let config = widgets::OverlayConfig {
        title: &title,
        tabs: None,
        shortcuts: &shortcuts,
        sizing: widgets::OverlaySizing::ask_user(composer),
        desired_content_rows: layout.content.height,
    };
    widgets::OverlayFrame::render(frame, &layout, &config);

    let chrome = Style::default().fg(widgets::RolePalette::chrome());
    if header.height > 0 {
        frame.render_widget(Paragraph::new(Text::from(header_lines)), header);
    }
    if let Some(rule) = rule {
        frame.render_widget(
            Paragraph::new(Line::styled("─".repeat(usize::from(rule.width)), chrome)),
            rule,
        );
    }

    let offset = ask_user_list_offset(selected_row, usize::from(list.height), rows.len());
    widgets::OverlayList::render(frame, list, &rows, offset, rows.len());

    if let Some(divider) = divider {
        frame.render_widget(
            Paragraph::new(Text::from(vec![
                Line::styled("│", chrome);
                usize::from(divider.height)
            ])),
            divider,
        );
    }
    if let Some(label) = context_label {
        frame.render_widget(
            Paragraph::new(Line::styled(ask_user_context_label(label.width), chrome)),
            label,
        );
    }
    if let Some(pane) = context {
        let scroll = render.context_scroll.min(max_context_scroll);
        render_ask_user_context(frame, pane, &context_lines, scroll);
        if let Some(position) = context_position {
            render_ask_user_context_position(
                frame,
                position,
                scroll,
                pane.height,
                context_lines.len(),
            );
        }
    }
}

/// The stacked-mode divider: it names the section and the keys that reach it,
/// because a pane below the fold nobody knows how to scroll is not reachable.
fn ask_user_context_label(width: u16) -> String {
    let named = format!("── context ── {ASK_USER_CONTEXT_KEYS} ");
    if named.width() >= usize::from(width) {
        return widgets::truncate_columns(&named, usize::from(width));
    }
    format!("{named}{}", "─".repeat(usize::from(width) - named.width()))
}

fn render_ask_user_context(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    lines: &[String],
    scroll: u16,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let style = Style::default().fg(widgets::RolePalette::muted());
    let visible: Vec<Line<'static>> = lines
        .iter()
        .skip(usize::from(scroll))
        .take(usize::from(area.height))
        .map(|line| Line::styled(line.clone(), style))
        .collect();
    frame.render_widget(Paragraph::new(Text::from(visible)), area);
}

fn render_ask_user_context_position(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    scroll: u16,
    height: u16,
    total: usize,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let first = usize::from(scroll) + 1;
    let last = (usize::from(scroll) + usize::from(height)).min(total);
    frame.render_widget(
        Paragraph::new(Line::styled(
            widgets::truncate_columns(
                &format!("{first}–{last} of {total}  ·  {ASK_USER_CONTEXT_KEYS}"),
                usize::from(area.width),
            ),
            Style::default().fg(widgets::RolePalette::chrome()),
        )),
        area,
    );
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
        home: state.home,
        repository: state.repository,
        idle: state.turn_activity == crate::activity::TurnActivity::Ready,
        unicode: state.unicode_level,
        turn_label: state.turn_activity.footer_label(),
        duration: state.turn_duration,
        usage: state.latest_usage,
        dangerous: state.dangerous_mode,
        bypass: state.bypass,
        failed: state.turn_state == Some(TurnState::Failed),
    }
}

/// Caps visible queue message rows so a full queue cannot starve the transcript.
const MAX_VISIBLE_QUEUE_ROWS: usize = 6;

/// Rows reserved for the muted queue chrome line under the message stack.
const QUEUE_STATUS_ROWS: usize = 1;

/// Height budget for a non-empty queue: message rows plus the status line.
fn queue_layout_rows(queue_len: usize) -> usize {
    if queue_len == 0 {
        return 0;
    }
    queue_len
        .min(MAX_VISIBLE_QUEUE_ROWS)
        .saturating_add(QUEUE_STATUS_ROWS)
}

/// Pending prompts as numbered message lines above the composer (Grok/OpenCode style).
fn render_queue(frame: &mut ratatui::Frame<'_>, area: Rect, state: &ViewState<'_>) {
    if area.height == 0 || state.queue.is_empty() {
        return;
    }

    let status_rows = usize::from(area.height > 1).min(QUEUE_STATUS_ROWS);
    let message_budget = usize::from(area.height).saturating_sub(status_rows);
    let visible = state
        .queue
        .len()
        .min(message_budget)
        .min(MAX_VISIBLE_QUEUE_ROWS);
    let focused = state.surface_focus == SurfaceFocus::Queue;
    let width = usize::from(area.width);

    let mut lines = state
        .queue
        .iter()
        .take(visible)
        .enumerate()
        .map(|(index, entry)| {
            let selected = focused && state.queue_selected == Some(index);
            queue_row_line(index + 1, entry.prompt(), width, selected)
        })
        .collect::<Vec<_>>();

    if status_rows > 0 {
        lines.push(queue_status_line(state.queue.len(), focused, width));
    }

    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

fn queue_row_line(index: usize, prompt: &str, width: usize, selected: bool) -> Line<'static> {
    let text_fg = if selected {
        widgets::RolePalette::selection_fg()
    } else {
        widgets::RolePalette::assistant()
    };
    let index_fg = if selected {
        widgets::RolePalette::selection_fg()
    } else {
        widgets::RolePalette::muted()
    };
    let background = selected.then_some(widgets::RolePalette::selection_bg());
    let base = match background {
        Some(bg) => Style::default().fg(text_fg).bg(bg),
        None => Style::default().fg(text_fg),
    };
    let index_style = match background {
        Some(bg) => Style::default().fg(index_fg).bg(bg),
        None => Style::default().fg(index_fg),
    };

    let label = format!("#{index} ");
    let label_width = UnicodeWidthStr::width(label.as_str());
    let text = render::bounded_single_line(prompt, width.saturating_sub(label_width));
    let used = label_width.saturating_add(UnicodeWidthStr::width(text.as_str()));
    let padding = width.saturating_sub(used);

    let mut spans = vec![Span::styled(label, index_style), Span::styled(text, base)];
    if padding > 0 {
        spans.push(Span::styled(
            " ".repeat(padding),
            match background {
                Some(bg) => Style::default().bg(bg),
                None => Style::default(),
            },
        ));
    }
    Line::from(spans)
}

fn queue_status_line(count: usize, focused: bool, width: usize) -> Line<'static> {
    let muted = Style::default().fg(widgets::RolePalette::muted());
    let accent = Style::default().fg(widgets::RolePalette::chrome());
    let label = if focused {
        if count == 1 {
            "Queued · Enter edit · Del remove".to_owned()
        } else {
            format!("Queued ({count}) · Enter edit · Del remove")
        }
    } else if count == 1 {
        "Queued · Tab manage".to_owned()
    } else {
        format!("Queued ({count}) · Tab manage")
    };
    let text = render::bounded_single_line(&label, width);
    Line::from(vec![Span::styled(
        text,
        if focused { accent } else { muted },
    )])
}

/// Marker spliced into the composer's top border counting the rows scrolled out
/// of view, or `None` when everything typed is on screen or the border is too
/// narrow to say so whole.
///
/// The composer stops growing at its ceiling, so past that point the only thing
/// that tells a reader their text continues above or below the box is this
/// count.
fn hidden_rows_marker(
    content_rows: usize,
    scroll: usize,
    visible_rows: usize,
    composer_width: u16,
) -> Option<Line<'static>> {
    let above = scroll;
    let below = content_rows.saturating_sub(scroll.saturating_add(visible_rows));
    if above == 0 && below == 0 {
        return None;
    }

    let mut label = String::new();
    if above > 0 {
        label.push_str(&format!("↑{above}"));
    }
    if below > 0 {
        if !label.is_empty() {
            label.push(' ');
        }
        label.push_str(&format!("↓{below}"));
    }
    let label = format!(" {label} ");

    if saturating_u16(label.width()) > border_metrics_budget(composer_width) {
        return None;
    }

    Some(
        Line::from(Span::styled(
            label,
            Style::default().fg(widgets::RolePalette::chrome()),
        ))
        .right_aligned(),
    )
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
    let mut spans = widgets::MetricFooter::border_spans(
        border_metrics_budget(composer.width),
        footer_context(state),
    )?;
    spans.push(Span::raw(" "));
    Some(Line::from(spans).right_aligned())
}

/// Content width below which the palette drops the description column entirely.
const PALETTE_DESCRIPTION_MIN_WIDTH: u16 = 40;

/// Spelling the Enter key out costs four columns over the glyph, and the
/// footer drops whole shortcuts rather than wrapping past two rows — in the
/// narrowest palette that was enough to push `esc close` off the overlay
/// entirely. `move` buys those columns back, and matches what every other
/// overlay in this file already calls the same motion.
const PALETTE_SHORTCUTS: [widgets::OverlayShortcut<'static>; 3] = [
    widgets::OverlayShortcut {
        key: "↑↓",
        label: "move",
    },
    widgets::OverlayShortcut {
        key: "Enter",
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
        key: "Enter",
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

fn render_dialog(frame: &mut ratatui::Frame<'_>, area: Rect, composer: Rect, dialog: &DialogView) {
    let labels = dialog_shortcut_labels(dialog);
    let shortcuts = dialog_shortcuts(&labels);
    let config = dialog_config(dialog, &shortcuts, area, composer);
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
    let tree = dialog.tree_entries.as_ref();
    if sessions.is_some_and(|entries| entries.loading) {
        return Some(Line::styled(
            "Loading sessions…",
            Style::default().fg(widgets::RolePalette::muted()),
        ));
    }
    if tree.is_some_and(|entries| entries.loading) {
        return Some(Line::styled(
            "Loading session tree…",
            Style::default().fg(widgets::RolePalette::muted()),
        ));
    }
    if let Some(error) = sessions
        .and_then(|entries| entries.error.as_deref())
        .or_else(|| tree.and_then(|entries| entries.error.as_deref()))
    {
        return Some(Line::styled(
            error.to_owned(),
            Style::default().fg(widgets::RolePalette::warning()),
        ));
    }

    let empty = dialog_matches(dialog).is_empty()
        && (sessions.is_some()
            || tree.is_some()
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

    // Confirm overlays (permission) and informational bodies wrap so long
    // commands/paths stay readable instead of being mid-line ellipsized.
    if dialog_help_is_body(dialog) || dialog.overlay_kind == widgets::OverlayKind::Confirm {
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
        labels.push((Cow::Borrowed("Enter"), Cow::Borrowed("resume")));
    } else if dialog.interactive {
        let enter = match dialog.prompt_overlay {
            Some(_) => "paste",
            None => "select",
        };
        labels.push((Cow::Borrowed("Enter"), Cow::Borrowed(enter)));
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
    if dialog.prompt_overlay == Some(PromptOverlayKind::Stash) {
        labels.push((Cow::Borrowed("x/del"), Cow::Borrowed("remove")));
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
    composer: Rect,
) -> widgets::OverlayConfig<'a> {
    // Confirm uses the full dialog sizing so a long bash command can wrap in
    // the help band without the compact max-height clipping it to one line.
    let sizing = if dialog.composer_anchored {
        widgets::OverlaySizing::palette(composer)
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

/// Fallback composer band when only the terminal area is known (paging keys).
fn approximate_composer_rect(area: Rect) -> Rect {
    let height = 4u16.min(area.height);
    Rect::new(
        area.x,
        area.y.saturating_add(area.height.saturating_sub(height)),
        area.width,
        height,
    )
}

/// Entry rows the dialog can show in `area`, shared by the renderer and the
/// paging keys so navigation never disagrees with what is painted.
fn dialog_visible_rows(area: Rect, dialog: &DialogView) -> usize {
    let labels = dialog_shortcut_labels(dialog);
    let shortcuts = dialog_shortcuts(&labels);
    let config = dialog_config(dialog, &shortcuts, area, approximate_composer_rect(area));
    widgets::OverlayLayout::solve(area, &config).map_or(1, |layout| {
        usize::from(dialog_sections(layout.content, dialog).rows.height).max(1)
    })
}

fn dialog_empty_message(dialog: &DialogView) -> &str {
    if let Some(message) = dialog.empty_message.as_deref() {
        return message;
    }
    let Some(session_entries) = dialog.session_entries.as_ref() else {
        if dialog.tree_entries.is_some() {
            return if dialog.query.is_empty() {
                "No forks of this session."
            } else {
                "No sessions match search."
            };
        }
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
    queue: Rect,
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

    /// Places the reserved rows inside their region: the tree hugs the composer,
    /// the notice and hint band sits under it, and the status bar keeps the last
    /// row. The region keeps its full reserved height, which is what keeps the
    /// composer a function of terminal height only.
    ///
    /// The tree goes first because a reader walks into it with Down out of the
    /// prompt, and a legend wedged between the two would break the only reason
    /// that gesture reads as movement.
    ///
    /// The notice band always claims its row, because it always has something
    /// to say: a warning when there is one, and the contextual key hints
    /// otherwise. It used to be conditional, and the condition was evaluated
    /// once by the renderer and once by hit-testing — which is exactly how a
    /// click came to land on a different tree row than the one on screen.
    fn placed(self, region: Rect) -> ChromeBands {
        let notice = self.notice;
        let band = |y: u16, height: u16| Rect {
            x: region.x,
            y,
            width: region.width,
            height,
        };
        ChromeBands {
            tree: band(region.y, self.tree),
            notice: band(region.y.saturating_add(self.tree), notice),
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

fn attachment_preview_lines(media_chips: &[String], width: usize) -> Vec<String> {
    media_chips
        .iter()
        .map(|label| widgets::truncate_columns(label.trim_matches(['[', ']']), width))
        .collect()
}

/// Columns the composer frame takes from the text when it can afford them:
/// both borders and the blank column inside each of them.
const COMPOSER_FRAME_COLUMNS: u16 = 4;

/// Blank columns between the composer's borders and its text.
///
/// The left column keeps typed text on the column the prose above it starts
/// on, so it survives any width. The right column is breathing room, and a box
/// too narrow to hold a glyph beside it spends it on the glyph instead.
fn composer_padding(composer_width: u16) -> Padding {
    if composer_width > COMPOSER_FRAME_COLUMNS {
        Padding::horizontal(1)
    } else {
        Padding::left(1)
    }
}

/// Columns the frame leaves unavailable to the text at this composer width.
fn composer_frame_columns(composer_width: u16) -> u16 {
    let padding = composer_padding(composer_width);
    padding.left.saturating_add(padding.right).saturating_add(2)
}

/// Tallest the composer may ever grow, however tall the terminal is.
const MAX_COMPOSER_ROWS: u16 = 8;
/// Shortest composer the layout still calls scrollable: both borders plus one
/// row of text.
const MIN_COMPOSER_ROWS: u16 = 3;
/// Share of the screen the composer may claim before it scrolls internally: a
/// third, so the transcript always keeps the majority of the rows.
const COMPOSER_VIEWPORT_SHARE: u16 = 3;

/// Rows the composer may grow to on a screen of this height.
///
/// The ceiling is a function of the terminal alone, never of what was typed or
/// staged, so a long prompt or a stack of attachments scrolls inside the box
/// instead of pushing the transcript off screen.
fn composer_ceiling(height: u16) -> u16 {
    (height / COMPOSER_VIEWPORT_SHARE).clamp(MIN_COMPOSER_ROWS, MAX_COMPOSER_ROWS)
}

fn composer_rows(height: u16, input: &str, media_count: usize, width: usize) -> u16 {
    match height {
        0 => 0,
        1 => 1,
        2..=6 => 2,
        7..=11 => 3,
        _ => saturating_u16(
            composer_layout(input, input.chars().count(), width)
                .rows
                .saturating_add(media_count)
                .saturating_add(2),
        )
        .clamp(MIN_COMPOSER_ROWS, composer_ceiling(height)),
    }
}

fn screen_layout(area: Rect, input: &str, media_count: usize, queue_len: usize) -> ScreenLayout {
    let area = conversation_surface(area);
    let gutter = chrome_gutter(area.width);
    let composer_width = area.width.saturating_sub(gutter.saturating_mul(2));
    let inner_width = usize::from(
        composer_width
            .saturating_sub(composer_frame_columns(composer_width))
            .max(1),
    );
    let composer_rows =
        composer_rows(area.height, input, media_count, inner_width).min(area.height);
    let after_composer = area.height.saturating_sub(composer_rows);
    let chrome = bottom_chrome(area.width, area.height).fitted(after_composer);
    let remaining = after_composer.saturating_sub(chrome.rows());
    let wanted_queue = saturating_u16(queue_layout_rows(queue_len));
    let queue_rows = wanted_queue.min(remaining);
    let transcript_rows = remaining.saturating_sub(queue_rows);
    let chunks = Layout::vertical([
        Constraint::Length(transcript_rows),
        Constraint::Length(queue_rows),
        Constraint::Length(composer_rows),
        Constraint::Length(chrome.rows()),
    ])
    .split(area);

    let gutter = Margin::new(chrome_gutter(area.width), 0);
    let bands = chrome.placed(chunks[3].inner(gutter));
    // The transcript keeps its own left indent, so only the right edge is owed
    // the gutter. Without it the prose ran past the composer it belongs to, and
    // a line that outruns the box you typed it into reads as a different column.
    let transcript = Rect {
        width: chunks[0].width.saturating_sub(chrome_gutter(area.width)),
        ..chunks[0]
    };

    ScreenLayout {
        transcript,
        queue: chunks[1].inner(gutter),
        composer: chunks[2].inner(gutter),
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
    } else if let Some(failure) = turn_failure_banner(state) {
        left.push(failure);
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

/// Restates the failure that ended the current turn in the reserved band above
/// the composer.
///
/// The error card can sit anywhere in the transcript, including far outside the
/// viewport, so it cannot be the only place the cause is readable. The band is
/// already reserved, always on screen and independent of scroll, which is what
/// a transient overlay would have had to reinvent with a timer of its own. It
/// clears by itself when the next turn starts.
fn turn_failure_banner(state: &ViewState<'_>) -> Option<Span<'static>> {
    if state.turn_state != Some(TurnState::Failed) {
        return None;
    }

    let cause = state
        .conversation?
        .errors
        .last()?
        .message
        .lines()
        .next()?
        .trim();

    (!cause.is_empty()).then(|| {
        Span::styled(
            format!(" {cause} "),
            Style::default()
                .fg(widgets::RolePalette::error())
                .add_modifier(Modifier::BOLD),
        )
    })
}

/// Keys worth naming for what the reader is doing at this exact moment.
///
/// A fixed legend is either too long to read or too short to help, and it goes
/// stale the moment the keymap moves. This is short because it is contextual:
/// `Enter` is only worth a slot once there is something to send, and the mode
/// badge only exists while a mode other than typing is on. Everything not named
/// here lives in the shortcuts overlay, which is the one list that cannot drift
/// out of date.
fn hint_spans(state: &ViewState<'_>) -> Vec<Span<'static>> {
    let mut hints: Vec<(&str, &str)> = Vec::new();

    if state.surface_focus == SurfaceFocus::Queue {
        if state.size.0 < 52 {
            hints.push(("QUEUE", "Enter edit · Del remove"));
        } else {
            hints.extend([
                ("QUEUE", "selected"),
                ("↑↓", "select"),
                ("Enter", "edit"),
                ("Del", "remove"),
                ("Alt↑↓", "reorder"),
                ("Tab", "composer"),
            ]);
        }
    } else if state.execution_selection.is_some() {
        if state.size.0 < 52 {
            hints.push(("ACTIVITY", "x cancel X all"));
        } else {
            hints.extend([
                ("ACTIVITY", "selected"),
                ("↑↓", "select"),
                ("x", "cancel selected"),
                ("X", "cancel all"),
                ("Enter", "inspect"),
            ]);
        }
    } else if state.focus == TranscriptFocus::Viewport {
        hints.push(("j/k", "scroll"));
        if state.selection.is_some() {
            hints.push(("^⇧C", "copy"));
        }
        if state.running {
            hints.push(("Ctrl+C", "cancel"));
        }
        hints.push(("i", "insert"));
    } else {
        if !state.input.is_empty() || !state.media_chips.is_empty() {
            hints.push(("Enter", if state.running { "queue" } else { "send" }));
        }
        if state.running {
            hints.push(("Ctrl+C", "cancel"));
            hints.push(("Tab", "queue"));
        } else {
            hints.push(("Esc", "normal"));
        }
    }
    hints.push(("^?", "shortcuts"));

    let mut spans = Vec::new();
    if state.focus == TranscriptFocus::Viewport {
        spans.push(Span::styled(
            " NORMAL ",
            Style::default()
                .fg(widgets::RolePalette::navigation())
                .add_modifier(Modifier::BOLD),
        ));
    } else {
        spans.push(Span::raw(" "));
    }

    for (index, (key, label)) in hints.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(
                "  ",
                Style::default().fg(widgets::RolePalette::chrome()),
            ));
        }
        spans.push(Span::styled(
            (*key).to_owned(),
            Style::default().fg(widgets::RolePalette::muted()),
        ));
        spans.push(Span::styled(
            format!(":{label}"),
            Style::default().fg(widgets::RolePalette::chrome()),
        ));
    }

    spans
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
    fitted_subagent_tree(state, rows, width).0
}

/// The tree's rows, paired with the transcript each one is a way into.
///
/// Both come from the same pass on purpose. The rows are not one per branch —
/// the root is dropped when the tree is tight, activity rows sit under their
/// branch, and the affordance closes the list — so anything deriving an index
/// from a row on its own was reading a different tree than the one on screen.
fn fitted_subagent_tree(
    state: &ViewState<'_>,
    rows: u16,
    width: u16,
) -> (Vec<Line<'static>>, Vec<Option<TranscriptId>>) {
    let _perf_tree = agens_perf::span!("tui.tree", rows = rows, width = width);
    if state.executions.is_empty() || rows == 0 {
        return (Vec::new(), Vec::new());
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
    let mut ids: Vec<Option<TranscriptId>> = Vec::new();
    if spare > 0 {
        lines.push(tree_root_line(state, width));
        ids.push(Some(TranscriptId::Main));
    }
    let backgroundable = branches
        .iter()
        .any(|execution| execution.state == TuiExecutionState::ForegroundRunning);
    let (branch_lines, branch_ids) =
        tree_branch_lines(state, &branches, spare.saturating_sub(1), width);
    lines.extend(branch_lines);
    ids.extend(branch_ids);
    let cancellable = branches.iter().any(|execution| {
        state.active_transcript == TranscriptId::Subagent(execution.id)
            && matches!(
                execution.state,
                TuiExecutionState::ForegroundRunning
                    | TuiExecutionState::BackgroundRunning
                    | TuiExecutionState::CancellationRequested
            )
    });
    lines.push(tree_affordance_line(
        state.executions.len().saturating_sub(branches.len()),
        backgroundable,
        cancellable,
        width,
    ));
    ids.push(None);
    (lines, ids)
}

/// The tree's root: the parent transcript, and how much delegated work hangs
/// off it.
///
/// The count is what makes the row worth its line. "Main" alone says nothing a
/// reader cannot see; "Main · 3 running" answers how much is in flight before
/// any branch is read, and keeps answering it when the branches themselves have
/// been elided for height. Finished branches are counted separately so a row of
/// leftovers never reads as live work.
fn tree_root_line(state: &ViewState<'_>, width: usize) -> Line<'static> {
    let running = state
        .executions
        .iter()
        .filter(|execution| {
            matches!(
                execution.state,
                TuiExecutionState::ForegroundRunning | TuiExecutionState::BackgroundRunning
            )
        })
        .count();
    let finished = state.executions.len().saturating_sub(running);

    let mut label = "Main".to_owned();
    if running > 0 {
        label.push_str(&format!(" · {running} running"));
    }
    if finished > 0 {
        label.push_str(&format!(" · {finished} done"));
    }

    Line::from(Span::styled(
        render::bounded_single_line(&label, width),
        tree_row_style(state, TranscriptId::Main),
    ))
}

fn tree_branch_lines(
    state: &ViewState<'_>,
    branches: &[&TuiExecution],
    activity_rows: usize,
    width: usize,
) -> (Vec<Line<'static>>, Vec<Option<TranscriptId>>) {
    let mut lines = Vec::new();
    let mut ids: Vec<Option<TranscriptId>> = Vec::new();
    let mut activity_budget = activity_rows.min(MAX_TREE_ACTIVITIES);

    for (index, execution) in branches.iter().enumerate() {
        let last = index + 1 == branches.len();
        let rail = if last { "└─ " } else { "├─ " };
        let glyph = format!(
            "{} ",
            execution_state_glyph(execution.state).text(state.unicode_level)
        );
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
        ids.push(Some(TranscriptId::Subagent(execution.id)));

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
            let glyph = format!(
                "{} ",
                if activity.running {
                    widgets::Glyph::Running.text(state.unicode_level)
                } else {
                    widgets::Glyph::Succeeded.text(state.unicode_level)
                }
            );
            let label_width = width
                .saturating_sub(rail.width())
                .saturating_sub(glyph.width());
            lines.push(Line::from(vec![
                Span::styled(rail, Style::default().fg(widgets::RolePalette::chrome())),
                Span::styled(
                    glyph,
                    Style::default().fg(if activity.running {
                        widgets::RolePalette::running()
                    } else {
                        widgets::RolePalette::success()
                    }),
                ),
                Span::styled(
                    render::bounded_single_line(&activity.label, label_width),
                    Style::default().fg(widgets::RolePalette::muted()),
                ),
            ]));
            // An activity row belongs to its branch but is not a way into it:
            // it names a tool call, and there is no transcript to open for one.
            ids.push(None);
        }
    }
    (lines, ids)
}

/// Closes the tree with its navigation affordance, folding the branches that
/// did not fit into the same row so the hidden count stays discoverable.
///
/// Backgrounding only applies to a branch still running in the foreground, and
/// cancelling only to a branch still running at all, so each hint is dropped
/// when nothing on screen can accept it. A key advertised over work it cannot
/// act on is worse than no hint: it invites a press that does nothing.
fn tree_affordance_line(
    hidden_branches: usize,
    backgroundable: bool,
    cancellable: bool,
    width: usize,
) -> Line<'static> {
    let text = if hidden_branches > 0 {
        format!("+{hidden_branches} more · ↓ to focus")
    } else {
        let mut hints = vec!["↑↓ walk", "Enter inspect"];
        if backgroundable {
            hints.push("Ctrl+B background");
        }
        if cancellable {
            hints.push("x cancel");
        }
        hints.join(" · ")
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
        widgets::RolePalette::navigation()
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

/// One supervisable row: who is running, in what state, for how long, on what.
///
/// The model closes the row because it is the datum that makes the elapsed time
/// mean something — a slow branch on a large model and a slow branch on a small
/// one are different problems. It is omitted rather than filled in when the
/// runtime has not reported it, so the row never guesses.
fn tree_execution_label(execution: &TuiExecution, now: Duration) -> String {
    let elapsed = execution
        .terminal_at
        .unwrap_or(now)
        .saturating_sub(execution.started_at);
    let mut label = format!(
        "{} #{} · {} · {}",
        display_agent_name(&execution.agent),
        execution.id,
        execution_state_label(execution.state),
        render::elapsed_label(elapsed)
    );
    if let Some(model) = execution.model.as_deref().filter(|model| !model.is_empty()) {
        label.push_str(" · ");
        label.push_str(model);
    }
    label
}

const fn execution_state_label(state: TuiExecutionState) -> &'static str {
    match state {
        TuiExecutionState::ForegroundRunning => "running",
        TuiExecutionState::BackgroundRunning => "background",
        TuiExecutionState::CancellationRequested => "cancellation requested",
        TuiExecutionState::CompletedRecent => "done",
        TuiExecutionState::Failed => "failed",
        TuiExecutionState::Cancelled => "cancelled",
    }
}

fn execution_state_color(state: TuiExecutionState) -> Color {
    match state {
        TuiExecutionState::ForegroundRunning
        | TuiExecutionState::BackgroundRunning
        | TuiExecutionState::CancellationRequested => widgets::RolePalette::running(),
        TuiExecutionState::CompletedRecent => widgets::RolePalette::success(),
        TuiExecutionState::Failed => widgets::RolePalette::error(),
        TuiExecutionState::Cancelled => widgets::RolePalette::warning(),
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
/// Settled turns kept in view before the transcript starts eliding.
///
/// Enough that the reader can see what led here, few enough that a long session
/// does not make every scroll a walk through work that is already finished.
const VISIBLE_SETTLED_TURNS: usize = 6;

/// How many settled turns the transcript folds behind its count row.
///
/// Nothing is elided while the reader has asked to see everything, and nothing
/// is elided when folding would hide fewer turns than the row costs to say so.
fn elided_turn_count(state: &ViewState<'_>) -> usize {
    if state.history_expanded {
        return 0;
    }
    let settled = state.completed_conversations.len();
    if settled <= VISIBLE_SETTLED_TURNS + 1 {
        return 0;
    }
    settled - VISIBLE_SETTLED_TURNS
}

fn rendered_transcript(state: &ViewState<'_>, row_width: u16) -> Vec<Line<'static>> {
    let mut lines = rendered_transcript_content(state, row_width);
    append_live_turn_status(&mut lines, state, row_width);
    lines
}

/// Conversation content without the live turn-status chrome.
///
/// The selectable cache is keyed by content epoch only. Spinner and elapsed
/// time must not be baked into that cache — otherwise a waiting turn freezes
/// the counter until the next content invalidation (often tens of seconds).
fn rendered_transcript_content(state: &ViewState<'_>, row_width: u16) -> Vec<Line<'static>> {
    assemble_transcript(state, row_width, false).0
}

/// Blank separator + live status row for an in-flight turn, or nothing when idle.
fn live_turn_status_lines(
    state: &ViewState<'_>,
    row_width: u16,
    separate_from_content: bool,
) -> Vec<Line<'static>> {
    if !state.running {
        return Vec::new();
    }

    let mut lines = Vec::new();
    // The status row reports the turn, it is not part of it: without a blank
    // row it reads as another line of whatever the agent just said.
    if separate_from_content {
        lines.push(render::unaccented_row(Line::default()));
    }
    let label = state.turn_activity.status_label();
    lines.push(render::unaccented_row(render::turn_status_line(
        render::TurnStatus {
            label: &label,
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
    lines
}

fn append_live_turn_status(lines: &mut Vec<Line<'static>>, state: &ViewState<'_>, row_width: u16) {
    let separate = lines.last().is_some_and(|line| line.width() > 0);
    lines.extend(live_turn_status_lines(state, row_width, separate));
}

/// Which tool call owns each transcript row, for the rows any call owns.
///
/// Computed only when something asks — hit-testing a pointer, and nothing else —
/// because the owners bypass the settled-conversation cache that the lines
/// themselves enjoy.
fn transcript_call_owners(state: &ViewState<'_>, row_width: u16) -> Vec<Option<String>> {
    assemble_transcript(state, row_width, true).1
}

/// The rows a turn contributes, described only when the caller wants them.
///
/// A turn's owners can be shorter than its rows — trailing rows belong to no
/// call — so the row count always comes from the painted lines. Copying those
/// lines is what a pointer movement cannot afford, and hit-testing never reads
/// them, so it gets rows of the right shape and no content.
fn turn_placeholder_rows(lines: &[Line<'static>], want_owners: bool) -> Vec<Line<'static>> {
    if want_owners {
        vec![Line::default(); lines.len()]
    } else {
        lines.to_vec()
    }
}

fn assemble_transcript(
    state: &ViewState<'_>,
    row_width: u16,
    want_owners: bool,
) -> (Vec<Line<'static>>, Vec<Option<String>>) {
    let _perf_assemble = agens_perf::span!(
        "tui.transcript.assemble",
        row_width = row_width,
        settled_turns = state.completed_conversations.len() as u64,
        want_owners = want_owners,
    );
    let mut transcript = chrome_rows(transcript_provenance(state));
    let mut owners = vec![None; transcript.len()];
    let thinking_streaming = state.running;
    let mut turn_lines = Vec::new();
    let mut turn_owners: Vec<Option<String>> = Vec::new();
    let mut turn_rows = 0usize;
    let mut append_turn = |lines: Vec<Line<'static>>, mut lines_owners: Vec<Option<String>>| {
        if lines.is_empty() {
            return;
        }
        if turn_rows > 0 {
            turn_lines.push(Line::default());
            turn_owners.push(None);
            turn_rows += 1;
        }
        lines_owners.resize(lines.len(), None);
        turn_rows += lines.len();
        turn_lines.extend(lines);
        turn_owners.extend(lines_owners);
    };
    let elided = elided_turn_count(state);
    if elided > 0 {
        append_turn(
            vec![render::history_elision_row(
                elided,
                row_width,
                state.unicode_level,
            )],
            Vec::new(),
        );
    }
    for (index, conversation) in state
        .completed_conversations
        .iter()
        .enumerate()
        .skip(elided)
    {
        let settled_state = render::ConversationRenderState {
            collapse_thinking: state.collapse_thinking,
            thinking_streaming: false,
            assistant_streaming: !state.highlight_restored_syntax,
            now: state.now,
            focused_call: state.focused_call,
            unicode: state.unicode_level,
        };
        let identity = render::SettledConversation {
            generation: state.transcript_generation,
            transcript: state.active_transcript,
            index,
        };
        let blocks = render::settled_conversation_blocks(
            identity,
            conversation,
            state.tool_display_modes,
            row_width,
            settled_state,
        );
        append_turn(
            turn_placeholder_rows(&blocks.lines, want_owners),
            if want_owners {
                blocks.owners.to_vec()
            } else {
                Vec::new()
            },
        );
    }
    if let Some(conversation) = state.conversation {
        let live_state = render::ConversationRenderState {
            collapse_thinking: state.collapse_thinking,
            thinking_streaming,
            assistant_streaming: state.assistant_streaming,
            now: state.now,
            focused_call: state.focused_call,
            unicode: state.unicode_level,
        };
        let painted = render::painted_conversation(
            conversation,
            state.runtime_events,
            state.tool_display_modes,
            row_width,
            live_state,
        );
        append_turn(
            turn_placeholder_rows(&painted.lines, want_owners),
            if want_owners {
                painted.owners
            } else {
                Vec::new()
            },
        );
    }
    let turn_start = transcript.len();
    transcript.extend(turn_lines);
    owners.resize(turn_start, None);
    owners.extend(turn_owners);
    let conversation_is_authoritative =
        !state.completed_conversations.is_empty() || state.conversation.is_some();
    if !conversation_is_authoritative {
        transcript = chrome_rows(transcript_lines(state.transcript));
    }
    transcript.extend(chrome_rows(render::detail_lines(
        state.runtime_events,
        conversation_is_authoritative,
    )));
    // Live spinner / elapsed chrome is appended outside the selectable cache
    // (see `append_live_turn_status`) so heartbeats do not freeze the clock.
    owners.resize(transcript.len(), None);
    (transcript, owners)
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
    /// Whether this cell is part of the text or part of the layout.
    ///
    /// A continuation row opens with the indent that keeps it under its
    /// paragraph. That indent exists because the terminal is narrow, so it is
    /// painted and highlighted like everything else but never copied.
    ///
    /// Leading accent and bullet gutter columns are also non-copyable so a
    /// drag that starts at column zero does not pull chrome spaces into the
    /// clipboard.
    copyable: bool,
}

/// Cached [`SelectableTranscript`] keyed by content epoch, transcript, and width.
struct SelectableCache {
    epoch: u64,
    transcript_id: TranscriptId,
    row_width: u16,
    first_row: usize,
    visible_rows: usize,
    /// Wave frame the cached rows were painted in, when any of them animates.
    ///
    /// Only rows carrying a running accent bar change with the clock, so an
    /// otherwise settled transcript stays cached across every tick.
    animated_at: Option<u128>,
    transcript: Arc<SelectableTranscript>,
}

/// View-facing handle to a shared selectable index.
///
/// Equality is pointer identity only: render snapshots compare presentation
/// state, not the full grapheme table that paint reuses.
#[derive(Clone, Debug)]
struct SharedSelectable(Arc<SelectableTranscript>);

impl SharedSelectable {
    fn empty() -> Self {
        Self(Arc::new(SelectableTranscript::default()))
    }

    fn from_arc(transcript: Arc<SelectableTranscript>) -> Self {
        Self(transcript)
    }

    fn arc(&self) -> Arc<SelectableTranscript> {
        Arc::clone(&self.0)
    }
}

impl PartialEq for SharedSelectable {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for SharedSelectable {}

/// What separates a row from the one below it in the copied text.
///
/// Only `Hard` is a line the author wrote. The soft variants are wrap seams:
/// rejoining puts back the space the wrap consumed, or nothing at all when the
/// wrap cut through the middle of a word.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum RowBreak {
    #[default]
    Hard,
    SoftSpace,
    SoftTight,
}

#[derive(Clone, Debug, Default)]
struct SelectableRow {
    cells: Vec<SelectableCell>,
    break_after: RowBreak,
}

#[derive(Clone, Debug, Default)]
struct SelectableTranscript {
    rows: Vec<SelectableRow>,
    first_row: usize,
    total_rows: usize,
    /// Content width these rows were wrapped at, so paint can detect a frame
    /// whose width the index does not describe.
    row_width: u16,
}

struct PlannedLine<'a> {
    line: &'a Line<'a>,
    first_row: usize,
    row_count: usize,
    continues_into_line: bool,
}

struct WrapPlan<'a> {
    lines: Vec<PlannedLine<'a>>,
    width: u16,
    total_rows: usize,
}

impl<'a> WrapPlan<'a> {
    fn from_lines(lines: &'a [Line<'a>], width: u16) -> Self {
        let width = width.max(1);
        let mut planned = Vec::with_capacity(lines.len());
        let mut total_rows = 0;
        let mut continues_into_line = false;

        for line in lines {
            let row_count = SelectableTranscript::row_count(std::slice::from_ref(line), width);
            planned.push(PlannedLine {
                line,
                first_row: total_rows,
                row_count,
                continues_into_line,
            });
            total_rows = total_rows.saturating_add(row_count);
            continues_into_line = line.spans.last().is_some_and(|span| {
                matches!(
                    span.content.as_ref(),
                    render::WRAP_JOINER_SPACE | render::WRAP_JOINER_TIGHT
                )
            });
        }

        Self {
            lines: planned,
            width,
            total_rows,
        }
    }
}

impl SelectableTranscript {
    fn from_lines(lines: &[Line<'_>], width: u16) -> Self {
        let width = width.max(1);
        let mut rows = Vec::new();
        let mut continues_into_line = false;

        for line in lines {
            let (line_rows, joined) = selectable_rows_for_line(line, width, continues_into_line);
            rows.extend(line_rows);
            continues_into_line = joined;
        }

        let total_rows = rows.len();
        Self {
            rows,
            first_row: 0,
            total_rows,
            row_width: width,
        }
    }

    fn window(plan: &WrapPlan<'_>, first_row: usize, row_count: usize) -> Self {
        let first_row = first_row.min(plan.total_rows);
        let end_row = first_row.saturating_add(row_count).min(plan.total_rows);
        let mut rows = Vec::with_capacity(end_row.saturating_sub(first_row));

        for planned in &plan.lines {
            let line_end = planned.first_row.saturating_add(planned.row_count);
            if line_end <= first_row || planned.first_row >= end_row {
                continue;
            }

            let (line_rows, _) =
                selectable_rows_for_line(planned.line, plan.width, planned.continues_into_line);
            let start = first_row.saturating_sub(planned.first_row);
            let end = end_row
                .saturating_sub(planned.first_row)
                .min(line_rows.len());
            rows.extend(
                line_rows
                    .into_iter()
                    .skip(start)
                    .take(end.saturating_sub(start)),
            );
        }

        Self {
            rows,
            first_row,
            total_rows: plan.total_rows,
            row_width: plan.width,
        }
    }

    const fn total_rows(&self) -> usize {
        self.total_rows
    }

    /// How many rows these lines wrap into, without materializing any of them.
    ///
    /// Scroll bounds need the row count and nothing else. Building the full
    /// index for it means allocating a styled, copyable, text-carrying cell
    /// for every character in the transcript — tens of thousands of them, on
    /// a path that runs for every scroll tick and every mouse press.
    fn row_count(lines: &[Line<'_>], width: u16) -> usize {
        let width = width.max(1);
        let mut rows = 0;

        for line in lines {
            let mut shapes = line
                .styled_graphemes(Style::default())
                .map(CellShape::from_grapheme)
                .collect::<Vec<_>>();

            if shapes.last().is_some_and(CellShape::is_wrap_joiner) {
                shapes.pop();
            }

            rows += wrap_cells(shapes, width, CellShape::width, CellShape::is_whitespace).len();
        }

        rows
    }

    fn position_at(&self, row: usize, column: u16) -> Option<TranscriptPosition> {
        let cells = &self.rows.get(row.checked_sub(self.first_row)?)?.cells;
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
        let selection_start = start.row.max(self.first_row);
        let selection_end = end.row.min(
            self.first_row
                .saturating_add(self.rows.len())
                .saturating_sub(1),
        );
        for row_index in selection_start..=selection_end {
            let Some(row) = self.rows.get(row_index.saturating_sub(self.first_row)) else {
                break;
            };
            for cell in &row.cells {
                let position = TranscriptPosition {
                    row: row_index,
                    column: cell.column,
                };
                if position < start || position > end || !cell.copyable {
                    continue;
                }
                append_bounded_selection(&mut text, &cell.text)?;
            }
            if row_index < end.row {
                match row.break_after {
                    RowBreak::Hard => append_bounded_selection(&mut text, "\n")?,
                    RowBreak::SoftSpace
                        if !text.ends_with(char::is_whitespace) && !text.is_empty() =>
                    {
                        append_bounded_selection(&mut text, " ")?;
                    }
                    RowBreak::SoftSpace | RowBreak::SoftTight => {}
                }
            }
        }
        Ok(text)
    }

    fn render_lines(&self, selection: Option<TranscriptSelection>) -> Vec<Line<'static>> {
        let selection = selection.map(ordered_selection);
        self.rows
            .iter()
            .enumerate()
            .map(|(relative_row, line)| {
                let row = self.first_row.saturating_add(relative_row);
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
                                .fg(widgets::RolePalette::selection_fg())
                                .bg(widgets::RolePalette::selection_bg()),
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

fn selectable_rows_for_line(
    line: &Line<'_>,
    width: u16,
    continues_into_line: bool,
) -> (Vec<SelectableRow>, bool) {
    let chrome_width = saturating_u16(widgets::ACCENT_WIDTH.saturating_add(widgets::GUTTER_WIDTH));
    let mut cells = line
        .styled_graphemes(Style::default())
        .map(|grapheme| SelectableCell {
            text: grapheme.symbol.to_owned(),
            column: 0,
            width: saturating_u16(grapheme.symbol.width()),
            style: grapheme.style,
            copyable: true,
        })
        .collect::<Vec<_>>();

    let joined = match cells.last().map(|cell| cell.text.as_str()) {
        Some(render::WRAP_JOINER_SPACE) => Some(RowBreak::SoftSpace),
        Some(render::WRAP_JOINER_TIGHT) => Some(RowBreak::SoftTight),
        _ => None,
    };
    if joined.is_some() {
        cells.pop();
    }

    let mut chrome_column = 0_u16;
    for cell in &mut cells {
        if chrome_column >= chrome_width {
            break;
        }
        cell.copyable = false;
        chrome_column = chrome_column.saturating_add(cell.width);
    }

    if continues_into_line {
        for cell in cells
            .iter_mut()
            .take_while(|cell| selectable_cell_is_whitespace(cell))
        {
            cell.copyable = false;
        }
    }

    let wrapped = wrap_selectable_line(cells, width);
    let last = wrapped.len().saturating_sub(1);
    let rows = wrapped
        .into_iter()
        .enumerate()
        .map(|(index, cells)| {
            let break_after = if index == last {
                joined.unwrap_or(RowBreak::Hard)
            } else {
                RowBreak::SoftSpace
            };
            selectable_row(cells, break_after)
        })
        .collect();

    (rows, joined.is_some())
}

fn wrap_selectable_line(cells: Vec<SelectableCell>, width: u16) -> Vec<Vec<SelectableCell>> {
    wrap_cells(
        cells,
        width,
        |cell| cell.width,
        selectable_cell_is_whitespace,
    )
}

/// Everything the wrap algorithm needs to know about a cell, and nothing else.
///
/// Deliberately `Copy` and three bytes wide: this is what a transcript costs
/// when it only has to be counted.
#[derive(Clone, Copy)]
struct CellShape {
    width: u16,
    whitespace: bool,
    wrap_joiner: bool,
}

impl CellShape {
    fn from_grapheme(grapheme: ratatui::text::StyledGrapheme<'_>) -> Self {
        let symbol = grapheme.symbol;
        Self {
            width: saturating_u16(symbol.width()),
            whitespace: symbol == "\u{200b}"
                || symbol != "\u{00a0}" && symbol.chars().all(char::is_whitespace),
            wrap_joiner: symbol == render::WRAP_JOINER_SPACE || symbol == render::WRAP_JOINER_TIGHT,
        }
    }

    const fn width(&self) -> u16 {
        self.width
    }

    const fn is_whitespace(&self) -> bool {
        self.whitespace
    }

    const fn is_wrap_joiner(&self) -> bool {
        self.wrap_joiner
    }
}

/// Wraps a line's cells into rows.
///
/// Generic over the cell so that counting rows and materializing them run the
/// same algorithm: only the advance width and whether a cell is whitespace
/// affect where a row breaks, and a second implementation for counting would
/// be free to drift away from this one without any test noticing.
fn wrap_cells<T>(
    cells: Vec<T>,
    width: u16,
    cell_width: impl Fn(&T) -> u16,
    is_whitespace_cell: impl Fn(&T) -> bool,
) -> Vec<Vec<T>> {
    let mut rows = Vec::new();
    let mut line = Vec::new();
    let mut line_width = 0_u16;
    let mut word = Vec::new();
    let mut word_width = 0_u16;
    let mut whitespace: VecDeque<T> = VecDeque::new();
    let mut whitespace_width = 0_u16;
    let mut previous_was_text = false;

    for cell in cells {
        if cell_width(&cell) > width {
            continue;
        }
        let is_whitespace = is_whitespace_cell(&cell);
        let word_finished = previous_was_text && is_whitespace;
        let segment_overflow = line.is_empty()
            && word_width
                .saturating_add(whitespace_width)
                .saturating_add(cell_width(&cell))
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
        let word_overflow = cell_width(&cell) > 0
            && line_width
                .saturating_add(whitespace_width)
                .saturating_add(word_width)
                >= width;
        if line_full || word_overflow {
            let mut remaining = width.saturating_sub(line_width);
            rows.push(std::mem::take(&mut line));
            line_width = 0;
            while let Some(pending) = whitespace.front() {
                if cell_width(pending) > remaining {
                    break;
                }
                whitespace_width = whitespace_width.saturating_sub(cell_width(pending));
                remaining = remaining.saturating_sub(cell_width(pending));
                whitespace.pop_front();
            }
            if is_whitespace && whitespace.is_empty() {
                previous_was_text = false;
                continue;
            }
        }

        if is_whitespace {
            whitespace_width = whitespace_width.saturating_add(cell_width(&cell));
            whitespace.push_back(cell);
        } else {
            word_width = word_width.saturating_add(cell_width(&cell));
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

fn selectable_row(mut cells: Vec<SelectableCell>, break_after: RowBreak) -> SelectableRow {
    let mut column = 0;
    for cell in &mut cells {
        cell.column = column;
        column = column.saturating_add(cell.width);
    }
    SelectableRow { cells, break_after }
}

fn ordered_selection(selection: TranscriptSelection) -> (TranscriptPosition, TranscriptPosition) {
    if selection.anchor <= selection.head {
        (selection.anchor, selection.head)
    } else {
        (selection.head, selection.anchor)
    }
}

struct MouseSelectionSnapshot {
    transcript: Arc<SelectableTranscript>,
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
                    .fg(widgets::RolePalette::warning())
                    .add_modifier(Modifier::BOLD),
            ),
            Line::styled(
                "gt select · m Main · [/] sibling",
                Style::default().fg(widgets::RolePalette::chrome()),
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
        TuiExecutionState::CancellationRequested => 3,
    }
}

const fn execution_state_glyph(state: TuiExecutionState) -> widgets::Glyph {
    match state {
        TuiExecutionState::ForegroundRunning | TuiExecutionState::BackgroundRunning => {
            widgets::Glyph::Running
        }
        TuiExecutionState::CompletedRecent => widgets::Glyph::Succeeded,
        TuiExecutionState::Failed => widgets::Glyph::Failed,
        TuiExecutionState::CancellationRequested => widgets::Glyph::Running,
        TuiExecutionState::Cancelled => widgets::Glyph::Cancelled,
    }
}

struct ComposerLayout {
    text: String,
    cursor_line: usize,
    cursor_column: usize,
    rows: usize,
}

/// Lays the composer text out at `width` columns.
///
/// Words wrap whole onto the next row; only a word wider than a row breaks at
/// the column edge. A space that lands past the edge stays off screen, so the
/// cursor after it starts the next row instead of hanging past the border.
fn composer_layout(input: &str, cursor: usize, width: usize) -> ComposerLayout {
    let width = width.max(1);
    let cursor = cursor.min(input.chars().count());
    let mut text = String::with_capacity(input.len());
    let mut character_index = 0_usize;
    let mut line = 0;
    let mut column = 0;
    let mut cursor_position = None;
    let mut at_word_start = true;

    for (offset, grapheme) in input.grapheme_indices(true) {
        let grapheme_end = character_index.saturating_add(grapheme.chars().count());
        let holds_cursor = cursor >= character_index && cursor < grapheme_end;
        if grapheme.ends_with('\n') {
            if holds_cursor {
                cursor_position = Some(wrapped_cursor_position(line, column, width));
            }

            text.push_str(grapheme);
            character_index = grapheme_end;
            line += 1;
            column = 0;
            at_word_start = true;
            continue;
        }

        let grapheme_width = grapheme.width();
        let is_space = grapheme.chars().all(char::is_whitespace);
        if is_space {
            at_word_start = true;

            if column.saturating_add(grapheme_width) > width {
                if holds_cursor {
                    cursor_position = Some(wrapped_cursor_position(line, width, width));
                }

                character_index = grapheme_end;
                column = width;
                continue;
            }
        } else if at_word_start {
            at_word_start = false;

            let word_width = word_width_at(input, offset);
            if column > 0 && column.saturating_add(word_width) > width && word_width <= width {
                text.push('\n');
                line += 1;
                column = 0;
            }
        }

        if column > 0 && column.saturating_add(grapheme_width) > width {
            text.push('\n');
            line += 1;
            column = 0;
        }
        if holds_cursor {
            let prefix_characters = cursor.saturating_sub(character_index);
            let prefix_end = grapheme
                .char_indices()
                .nth(prefix_characters)
                .map_or(grapheme.len(), |(index, _)| index);
            let prefix_width = grapheme[..prefix_end].width().min(width);
            cursor_position = Some(wrapped_cursor_position(
                line,
                column.saturating_add(prefix_width),
                width,
            ));
        }

        let rendered_grapheme = if grapheme_width > width {
            "\u{fffd}"
        } else {
            grapheme
        };
        text.push_str(rendered_grapheme);
        character_index = grapheme_end;
        column = column.saturating_add(grapheme_width.min(width));
    }

    let (cursor_line, cursor_column) =
        cursor_position.unwrap_or_else(|| wrapped_cursor_position(line, column, width));
    let rows = line.saturating_add(1).max(cursor_line.saturating_add(1));

    ComposerLayout {
        text,
        cursor_line,
        cursor_column,
        rows,
    }
}

/// Display width of the word starting at byte `offset`: every grapheme up to
/// the next whitespace or line break.
fn word_width_at(input: &str, offset: usize) -> usize {
    input[offset..]
        .graphemes(true)
        .take_while(|grapheme| !grapheme.chars().all(char::is_whitespace))
        .map(UnicodeWidthStr::width)
        .sum()
}

fn wrapped_cursor_position(line: usize, column: usize, width: usize) -> (usize, usize) {
    (line.saturating_add(column / width), column % width)
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
            writeln!(stdout, "{}", state.turn_activity.status_label())?;
        }
        write!(stdout, "> {}", state.input)?;
        stdout.flush()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AttachmentToken {
    attachment: PromptAttachment,
    start: usize,
    length: usize,
}

impl AttachmentToken {
    fn end(&self) -> usize {
        self.start + self.length
    }
}

/// Small event engine shared by the terminal lifecycle and future TUI components.
pub struct Tui<E> {
    engine: E,
    scheduler: AppState,
    busy_policy_routing: bool,
    surface_focus: SurfaceFocus,
    queue_selected: Option<usize>,
    input: String,
    input_cursor: usize,
    attachment_tokens: Vec<AttachmentToken>,
    /// Path-free media chips staged for the next turn (`[Image #N]`, …).
    media_chips: Vec<String>,
    /// The staged attachments behind `media_chips` (durable ids + mimes, no paths).
    ///
    /// Mirrors the app-side session staging so prompt history/stash can record
    /// what the chips stand for and restores can hand the exact set back.
    staged_media: Vec<PromptAttachment>,
    /// The attachments the running turn carried, snapshotted when it started.
    ///
    /// Completion clears only this set, never whatever is staged at that moment:
    /// media attached mid-turn belongs to the next prompt.
    submitted_media: Vec<PromptAttachment>,
    /// Whether a durable prompt-history write failure was already reported.
    prompt_history_write_reported: bool,
    recovered_failed_prompt: bool,
    size: (u16, u16),
    local_route_active: bool,
    session_loading: bool,
    assistant_streaming: bool,
    quit_armed_until: Option<Duration>,
    transcripts: BTreeMap<TranscriptId, TranscriptRecord>,
    active_transcript: TranscriptId,
    transcript_generation: u64,
    child_transcript_order: Vec<TranscriptId>,
    transcript: Vec<TranscriptEntry>,
    provider_model: String,
    reasoning_effort: Option<String>,
    context_window: Option<u64>,
    session: String,
    project: String,
    home: Option<String>,
    repository: Option<RepositoryStatus>,
    repository_probe: Option<RepositoryProbe>,
    repository_polled_at: Option<Duration>,
    working_directory_probe: Option<WorkingDirectoryProbe>,
    turn_state: Option<TurnState>,
    /// Whether the visible failure of the current turn is the projection's own
    /// placeholder, still waiting to be replaced by the real cause.
    placeholder_failure: bool,
    active_tool: Option<String>,
    /// The transient provider failure currently being waited out, if any.
    pending_retry: Option<RetryActivity>,
    /// Tick clock reading when the current reasoning stretch began.
    reasoning_started_at: Option<Duration>,
    /// Tick clock reading when the request went out with nothing back yet.
    waiting_started_at: Option<Duration>,
    runtime_events: Vec<TuiRuntimeEvent>,
    turn_duration: Option<Duration>,
    turn_started_at: Option<Duration>,
    /// Tokens billed by the rounds of the active turn, summed as they report.
    turn_context_tokens: Option<u64>,
    turn_output_tokens: Option<u64>,
    latest_usage: Option<Usage>,
    status: Option<String>,
    restored_syntax_ready_at: Option<Duration>,
    highlight_restored_syntax: bool,
    /// Whether the terminal this session is attached to renders OSC 8 links.
    ///
    /// Decided once from the environment rather than per frame, so the render
    /// path stays a pure function of state and a test can state the answer.
    hyperlinks: bool,
    color_level: widgets::ColorLevel,
    unicode_level: widgets::UnicodeLevel,
    completed_conversations: Vec<Conversation>,
    conversation: Option<Conversation>,
    dialog: Option<DialogView>,
    secret_entry: Option<SecretEntryState>,
    device_auth: Option<DeviceAuthState>,
    ask_user: Option<AskUserState>,
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
    now: Duration,
    next_runtime_ordinal: u64,
    mouse_selection_snapshot: Option<MouseSelectionSnapshot>,
    /// Content generation for the selectable transcript cache.
    selectable_epoch: u64,
    /// Last built selectable index; reused across paint and mouse hit-tests.
    selectable_cache: RefCell<Option<SelectableCache>>,
    /// First key of an unfinished viewport chord, such as the `g` of `gg`.
    pending_viewport_key: Option<char>,
    /// Optional prompt history/stash port installed by the composition root.
    prompt_memory: Option<Box<dyn PromptMemory>>,
    /// Generation stamped on the last fork request this terminal emitted.
    fork_generation: u64,
}

impl<E> Tui<E>
where
    E: Engine,
{
    /// Creates a TUI event engine around an injected application engine handle.
    pub fn new(engine: E) -> Self {
        Self::with_queue_capacity(engine, DEFAULT_PROMPT_QUEUE_CAPACITY)
    }

    /// Creates a TUI with the scheduler capacity used for explicit prompts.
    pub fn with_queue_capacity(engine: E, queue_capacity: usize) -> Self {
        Self {
            engine,
            scheduler: AppState::new(queue_capacity),
            busy_policy_routing: false,
            surface_focus: SurfaceFocus::Composer,
            queue_selected: None,
            hyperlinks: true,
            color_level: widgets::ColorLevel::default(),
            unicode_level: widgets::UnicodeLevel::default(),
            input: String::new(),
            input_cursor: 0,
            attachment_tokens: Vec::new(),
            media_chips: Vec::new(),
            staged_media: Vec::new(),
            submitted_media: Vec::new(),
            prompt_history_write_reported: false,
            recovered_failed_prompt: false,
            size: (80, 24),
            local_route_active: false,
            session_loading: false,
            assistant_streaming: false,
            quit_armed_until: None,
            transcripts: BTreeMap::from([(TranscriptId::Main, TranscriptRecord::main())]),
            active_transcript: TranscriptId::Main,
            transcript_generation: next_transcript_generation(),
            child_transcript_order: Vec::new(),
            transcript: Vec::new(),
            provider_model: String::new(),
            reasoning_effort: None,
            context_window: None,
            session: "new session".to_owned(),
            project: "agens".to_owned(),
            home: None,
            repository: None,
            repository_probe: None,
            repository_polled_at: None,
            working_directory_probe: None,
            turn_state: None,
            placeholder_failure: false,
            active_tool: None,
            pending_retry: None,
            reasoning_started_at: None,
            waiting_started_at: None,
            runtime_events: Vec::new(),
            turn_duration: None,
            turn_started_at: None,
            turn_context_tokens: None,
            turn_output_tokens: None,
            latest_usage: None,
            status: None,
            restored_syntax_ready_at: None,
            highlight_restored_syntax: true,
            completed_conversations: Vec::new(),
            conversation: None,
            dialog: None,
            secret_entry: None,
            device_auth: None,
            ask_user: None,
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
            now: Duration::ZERO,
            next_runtime_ordinal: 0,
            mouse_selection_snapshot: None,
            selectable_epoch: 0,
            selectable_cache: RefCell::new(None),
            pending_viewport_key: None,
            prompt_memory: None,
            fork_generation: 0,
        }
    }

    /// Handles one input or resize event without performing rendering or engine work.
    pub fn handle(&mut self, event: Event) -> Action {
        let _perf_event = agens_perf::span!("tui.event", kind = event.trace_kind(), batch = 1u64,);
        // A chord is only ever completed by the key that follows it. Anything
        // else the reader does abandons it, so `gt` cannot be completed by a
        // `t` typed a mouse click and a resize later.
        if !matches!(event, Event::Key(_)) {
            self.pending_viewport_key = None;
        }
        match event {
            Event::Resize { width, height } => {
                self.size = (width, height);
                self.mouse_selection_snapshot = None;
                self.clamp_palette_selection();
                self.clamp_scroll_offset();
                self.ensure_dialog_selection_visible();
                self.clamp_ask_user_context_scroll();
                Action::Render
            }
            Event::Key(key) => self.handle_key(key),
            Event::MouseWheel(direction) => self.handle_mouse_wheel_batch(&[direction]),
            Event::MouseDown { column, row } => self
                .handle_subagent_tree_click(column, row)
                .unwrap_or_else(|| self.begin_mouse_selection(column, row)),
            Event::MouseDrag { column, row } => self.update_mouse_selection(column, row, true),
            Event::MouseUp { column, row } => {
                // A plain click (no selection text) on a tool row opens the
                // detail modal. Drag-select still only selects text.
                let had_text = self
                    .transcripts
                    .get(&self.active_transcript)
                    .expect("active transcript always exists")
                    .selection_text
                    .as_ref()
                    .is_some_and(|text| !text.is_empty());
                let call_under_pointer = self.tool_call_id_at(column, row);
                let action = self.update_mouse_selection(column, row, false);
                let still_no_text = self
                    .transcripts
                    .get(&self.active_transcript)
                    .expect("active transcript always exists")
                    .selection_text
                    .as_ref()
                    .is_none_or(|text| text.is_empty());
                if !had_text
                    && still_no_text
                    && let Some(call_id) = call_under_pointer
                    && self.open_tool_detail_overlay(Some(&call_id))
                {
                    Action::Render
                } else {
                    action
                }
            }
            Event::MouseMove { column, row } => {
                if self
                    .transcripts
                    .get(&self.active_transcript)
                    .expect("active transcript always exists")
                    .selecting
                {
                    self.update_mouse_selection(column, row, true)
                } else {
                    self.hover_block(column, row)
                }
            }
            Event::Paste(text) if self.secret_entry.is_some() => {
                self.quit_armed_until = None;
                self.append_secret_text(&text);
                Action::Render
            }
            Event::Paste(text) => {
                self.quit_armed_until = None;
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

    /// Returns undispatched queue entries in their current FIFO order.
    pub fn queue_entries(&self) -> Vec<&QueueEntry> {
        self.scheduler.queued_entries()
    }

    /// Returns the current non-secret status message.
    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
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

    /// States the terminal hyperlink capability.
    ///
    /// A constructed TUI assumes a capable terminal rather than reading one out
    /// of the environment, so a frame a test renders is the same frame on every
    /// machine. [`Tui::adopt_environment`] is what a real session calls to
    /// replace those assumptions with what the attached terminal actually says.
    pub fn set_hyperlinks(&mut self, enabled: bool) {
        self.hyperlinks = enabled;
    }

    /// States the terminal colour and glyph capabilities, for the same reason
    /// [`Tui::set_hyperlinks`] exists.
    pub fn set_capabilities(&mut self, color: widgets::ColorLevel, unicode: widgets::UnicodeLevel) {
        self.color_level = color;
        self.unicode_level = unicode;
    }

    /// States the home directory the footer abbreviates project paths against.
    ///
    /// `None` leaves every path spelled out in full.
    pub fn set_home(&mut self, home: Option<String>) {
        self.home = home.filter(|home| !home.is_empty());
    }

    /// Replaces the constructed assumptions about the terminal with what the
    /// process environment claims: colour depth, glyph repertoire, hyperlink
    /// support and the home directory paths are abbreviated against.
    ///
    /// The composition root of a real session calls this once. Nothing else
    /// should: a surface that read the environment on its own would render a
    /// different frame for every `TERM`, `COLORTERM` and `HOME` a runner
    /// happens to export, which is exactly what a rendering test cannot have.
    pub fn adopt_environment(&mut self) {
        self.set_hyperlinks(widgets::hyperlinks_enabled());
        self.set_capabilities(
            widgets::detect_color_level(
                std::env::var("NO_COLOR").ok().as_deref(),
                std::env::var("AGENS_COLOR").ok().as_deref(),
                std::env::var("COLORTERM").ok().as_deref(),
                std::env::var("TERM").ok().as_deref(),
            ),
            widgets::detect_unicode_level(
                std::env::var("AGENS_GLYPHS").ok().as_deref(),
                std::env::var("LC_ALL")
                    .or_else(|_| std::env::var("LC_CTYPE"))
                    .or_else(|_| std::env::var("LANG"))
                    .ok()
                    .as_deref(),
            ),
        );
        self.set_home(std::env::var("HOME").ok());
    }

    pub fn set_collapse_thinking(&mut self, collapse: bool) {
        let record = self.active_record_mut();
        record.collapse_thinking = collapse;
        record.thinking_user_pinned = !collapse;
        self.bump_selectable_epoch();
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

    /// Whether anything is still moving and therefore owed a repaint.
    ///
    /// The parent turn is not the only live thing on screen: a background
    /// subagent keeps running after its turn ends, and its card reports an
    /// elapsed time. Tying the frame heartbeat to the parent alone froze that
    /// clock until the next keystroke, so the surface looked hung and then
    /// jumped when the runtime continued on its own.
    pub fn has_live_work(&self) -> bool {
        self.foreground_running()
            || self.executions.iter().any(|execution| {
                matches!(
                    execution.state,
                    TuiExecutionState::ForegroundRunning
                        | TuiExecutionState::BackgroundRunning
                        | TuiExecutionState::CancellationRequested
                )
            })
    }

    pub fn tick(&mut self, now: Duration) {
        self.now = now;
        self.poll_repository(now);
        self.poll_working_directory();
        if self.quit_armed_until.is_some_and(|until| now >= until) {
            self.quit_armed_until = None;
        }
        if self
            .restored_syntax_ready_at
            .is_some_and(|ready_at| now >= ready_at)
        {
            self.restored_syntax_ready_at = None;
            self.highlight_restored_syntax = true;
            self.bump_selectable_epoch();
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

    fn set_foreground_presentation(&mut self, running: bool) {
        let finishing = self.foreground_running() && !running;
        self.pending_retry = None;
        self.reasoning_started_at = None;
        if running {
            self.turn_started_at = Some(self.now);
            self.turn_context_tokens = None;
            self.turn_output_tokens = None;
            self.palette_open = false;
            self.turn_state = Some(TurnState::Requesting);
            self.placeholder_failure = false;
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

    /// Compatibility helper for callers that need to seed a foreground turn in tests.
    pub fn set_running(&mut self, running: bool) {
        if running && !self.foreground_running() {
            let _ = self.scheduler.reduce(AppEvent::SubmitPrompt(String::new()));
        }
        if !running && let Some(generation) = self.active_generation() {
            let _ = self
                .scheduler
                .reduce(AppEvent::TurnReleasedFor { generation });
        }
        self.set_foreground_presentation(running);
    }

    fn foreground_running(&self) -> bool {
        !matches!(self.scheduler.lifecycle(), TurnLifecycle::Idle)
    }

    fn settle_active_conversation(&mut self) {
        // Elapsed comes from the tick clock rather than the runtime's own
        // `TurnEnded`, which may arrive after the turn has already settled.
        let cost = TurnCost {
            duration: self
                .turn_started_at
                .map(|started| self.now.saturating_sub(started)),
            context_tokens: self.turn_context_tokens,
            output_tokens: self.turn_output_tokens,
        };
        if let Some(conversation) = self.conversation.as_mut() {
            conversation.cost = cost;
            conversation.mark_settled();
        }
    }

    /// Folds one round's report into the active turn, per figure.
    ///
    /// See [`TurnCost`]: output accumulates because each round generates its
    /// own, and the prompt takes the maximum because each round resends the
    /// last one's.
    fn accumulate_turn_usage(&mut self, usage: &Usage) {
        if let Some(input) = usage.input_tokens {
            self.turn_context_tokens = Some(self.turn_context_tokens.unwrap_or(0).max(input));
        }
        if let Some(output) = usage.output_tokens {
            self.turn_output_tokens = Some(self.turn_output_tokens.unwrap_or(0) + output);
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
        self.bump_selectable_epoch();
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

    /// Installs the source the footer reads branch and working-tree size from.
    ///
    /// The probe is called on the tick, so it must already have the answer:
    /// collecting it is the caller's job, on the caller's own clock. A probe
    /// that shells out to git here would put every frame behind it.
    pub fn set_repository_probe(&mut self, probe: RepositoryProbe) {
        self.repository = probe();
        self.repository_probe = Some(probe);
    }

    /// Installs the source the footer reads the session's location from, so a
    /// tool call that moved the session moves the footer with it.
    pub fn set_working_directory_probe(&mut self, probe: WorkingDirectoryProbe) {
        if let Some(directory) = probe() {
            self.project = directory;
        }
        self.working_directory_probe = Some(probe);
    }

    fn poll_repository(&mut self, now: Duration) {
        let Some(probe) = self.repository_probe.as_ref() else {
            return;
        };
        if self
            .repository_polled_at
            .is_some_and(|polled| now.saturating_sub(polled) < REPOSITORY_POLL_INTERVAL)
        {
            return;
        }
        self.repository_polled_at = Some(now);
        self.repository = probe();
    }

    /// Reads the session's location on every tick. Unlike the repository
    /// reading, which is collected by shelling out to git, this one is a read
    /// of state the tools already wrote, so it costs nothing to keep current.
    fn poll_working_directory(&mut self) {
        let Some(probe) = self.working_directory_probe.as_ref() else {
            return;
        };
        if let Some(directory) = probe() {
            self.project = directory;
        }
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
        if self.scheduler.lifecycle() == &TurnLifecycle::Idle {
            let _ = self
                .scheduler
                .reduce(AppEvent::SubmitPrompt(prompt.clone()));
        }
        if let Some(conversation) = self.conversation.take() {
            self.completed_conversations.push(conversation);
        }
        self.runtime_events.clear();
        self.turn_duration = None;
        self.submitted_media = self.staged_media.clone();
        self.transcript.push(TranscriptEntry::User(prompt.clone()));
        self.conversation = Some(Conversation::new_with_media(
            prompt,
            self.submitted_media
                .iter()
                .map(|attachment| attachment.mime.as_str()),
        ));
        {
            let record = self.active_record_mut();
            record.collapse_thinking = false;
            record.thinking_user_pinned = false;
        }
        self.set_foreground_presentation(true);
        self.assistant_streaming = true;
        self.bump_selectable_epoch();
    }

    fn active_generation(&self) -> Option<u64> {
        self.scheduler
            .lifecycle()
            .active()
            .map(ActiveRoute::generation)
    }

    /// Enables router-owned classification for busy composer submissions.
    pub fn enable_busy_policy_routing(&mut self) {
        self.busy_policy_routing = true;
    }

    /// Starts the turn a finished background subagent scheduled, but only at a safe point.
    ///
    /// The shared runtime rejects a concurrent turn, and firing over a composer the user is
    /// typing into would submit their unfinished prompt, so a scheduled turn waits for the next
    /// idle moment instead of being dropped. Every completion queued while waiting is carried by
    /// the single turn this returns.
    pub fn take_ready_auto_turn(&mut self) -> Option<String> {
        if !self.auto_turn_is_safe() {
            return None;
        }

        self.scheduler.set_composer(self.input.clone());
        let finished = self.scheduler.take_ready_auto_turn()?;
        self.begin_auto_turn(finished);
        Some(auto_turn_prompt(finished))
    }

    fn auto_turn_is_safe(&self) -> bool {
        !self.foreground_running()
            && !self.session_loading
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
        self.submitted_media = self.staged_media.clone();
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
        self.set_foreground_presentation(true);
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
        self.local_route_active = true;
        self.turn_started_at = Some(self.now);
        self.assistant_streaming = false;
        self.turn_state = None;
        self.quit_armed_until = None;
    }

    pub fn begin_session_load(&mut self) -> bool {
        if self.foreground_running() || self.session_loading || self.local_route_active {
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
            TuiSubmissionOutcome::BusyProviderTurn { display, prompt } => {
                self.enqueue_resolved_composer(display, prompt);
                None
            }
            TuiSubmissionOutcome::BusyRefusal(message) => {
                self.status = Some(message);
                None
            }
            TuiSubmissionOutcome::LocalInfo(message) => {
                self.add_info(message);
                None
            }
            TuiSubmissionOutcome::MediaAttached {
                message,
                staged_media,
            } => {
                self.insert_new_attachment_tokens(&staged_media);
                self.set_staged_media(staged_media);
                self.add_info(message);
                None
            }
            TuiSubmissionOutcome::StagedMediaReplaced {
                staged_media,
                notice,
            } => {
                self.set_staged_media(staged_media);
                if let Some(notice) = notice {
                    self.add_info(notice);
                }
                None
            }
            TuiSubmissionOutcome::LocalActionableError { message, action } => {
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
                self.apply_presentation(presentation);
                self.add_info(message);
                None
            }
            TuiSubmissionOutcome::SessionResumed {
                message,
                presentation,
                history,
                draft,
                staged_media,
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
                self.set_staged_media(staged_media);
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
            TuiSubmissionOutcome::HistoryRewritten {
                message,
                detail,
                presentation,
                history,
                draft,
            } => {
                self.replace_projected_history(history);
                self.apply_presentation(presentation);
                self.input.clear();
                self.input_cursor = 0;
                if let Some(draft) = draft {
                    self.restore_resume_draft(draft);
                }
                self.status = Some(message);
                if let Some(detail) = detail {
                    self.show_dialog("Files left alone", detail);
                }
                None
            }
            TuiSubmissionOutcome::Dialog(dialog) => {
                self.show_selection_dialog(dialog);
                None
            }
            TuiSubmissionOutcome::SafeDialog(dialog) => {
                self.show_selection_dialog(dialog);
                None
            }
            TuiSubmissionOutcome::TranscriptDialog => {
                self.show_transcript_dialog();
                None
            }
            TuiSubmissionOutcome::PromptHistoryOverlay => {
                self.show_history_overlay();
                None
            }
            TuiSubmissionOutcome::PromptStashOverlay => {
                self.show_stash_overlay();
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
                self.local_route_active = false;
                None
            }
        }
    }

    /// Applies a route result that was classified while the foreground lifecycle is active.
    pub fn apply_busy_submission_outcome(
        &mut self,
        outcome: TuiSubmissionOutcome,
    ) -> Option<String> {
        match outcome {
            TuiSubmissionOutcome::BusyProviderTurn { display, prompt } => {
                self.enqueue_resolved_composer(display, prompt);
                None
            }
            TuiSubmissionOutcome::BusyRefusal(message) => {
                self.status = Some(message);
                None
            }
            TuiSubmissionOutcome::LocalInfo(message) => {
                self.clear_composer();
                self.add_info(message);
                None
            }
            TuiSubmissionOutcome::LocalActionableError { message, action }
            | TuiSubmissionOutcome::SelectionError { message, action } => {
                self.show_dialog("Action required", format!("{message}\nAction: {action}"));
                None
            }
            TuiSubmissionOutcome::Dialog(dialog) | TuiSubmissionOutcome::SafeDialog(dialog) => {
                self.clear_composer();
                self.show_selection_dialog(dialog);
                None
            }
            TuiSubmissionOutcome::TranscriptDialog => {
                self.clear_composer();
                self.show_transcript_dialog();
                None
            }
            TuiSubmissionOutcome::PromptHistoryOverlay => {
                self.clear_composer();
                self.show_history_overlay();
                None
            }
            TuiSubmissionOutcome::PromptStashOverlay => {
                self.clear_composer();
                self.show_stash_overlay();
                None
            }
            TuiSubmissionOutcome::Quit => self.apply_submission_outcome(TuiSubmissionOutcome::Quit),
            outcome => self.apply_submission_outcome(outcome),
        }
    }

    pub fn set_composer_draft(&mut self, draft: impl Into<String>) {
        self.input = draft.into();
        self.input_cursor = self.input.chars().count();
        self.recovered_failed_prompt = false;
    }

    fn restore_resume_draft(&mut self, draft: String) {
        self.set_composer_draft(draft);
        self.recovered_failed_prompt = true;
        let scroll_offset = self.following_scroll_bottom();
        let record = self.active_record_mut();
        record.focus = TranscriptFocus::Composer;
        record.following_bottom = true;
        record.scroll_offset = scroll_offset;
    }

    fn insert_new_attachment_tokens(&mut self, attachments: &[PromptAttachment]) {
        let mut existing = self.staged_media.clone();
        for (index, attachment) in attachments.iter().enumerate() {
            if let Some(position) = existing
                .iter()
                .position(|candidate| candidate == attachment)
            {
                existing.remove(position);
                continue;
            }

            let label = media_chip_label(index + 1, &attachment.mime);
            let start = self.input_cursor;
            let length = label.chars().count();
            for token in &mut self.attachment_tokens {
                if token.start >= start {
                    token.start += length;
                }
            }
            let byte = byte_index(&self.input, start);
            self.input.insert_str(byte, &label);
            self.input_cursor += length;
            self.attachment_tokens.push(AttachmentToken {
                attachment: attachment.clone(),
                start,
                length,
            });
        }
        self.attachment_tokens.sort_by_key(|token| token.start);
    }

    fn delete_adjacent_attachment(&mut self, key: Key) -> Option<Action> {
        let position = self.attachment_tokens.iter().position(|token| match key {
            Key::Backspace => token.end() == self.input_cursor,
            Key::Delete => token.start == self.input_cursor,
            _ => false,
        })?;
        let token = self.attachment_tokens.remove(position);
        let start_byte = byte_index(&self.input, token.start);
        let end_byte = byte_index(&self.input, token.end());
        self.input.replace_range(start_byte..end_byte, "");
        self.input_cursor = token.start;
        for remaining in &mut self.attachment_tokens {
            if remaining.start >= token.end() {
                remaining.start -= token.length;
            }
        }
        if let Some(position) = self
            .staged_media
            .iter()
            .position(|attachment| attachment == &token.attachment)
        {
            self.staged_media.remove(position);
        }
        self.media_chips = attachment_chip_labels(&self.staged_media);
        Some(Action::SyncStagedMedia(self.staged_media.clone()))
    }

    fn provider_prompt(&self) -> String {
        let mut prompt = self.input.clone();
        for token in self.attachment_tokens.iter().rev() {
            let start = byte_index(&prompt, token.start);
            let end = byte_index(&prompt, token.end());
            prompt.replace_range(start..end, "");
        }
        prompt
    }

    /// Replaces the staged attachments; chip labels derive from the mimes.
    pub fn set_staged_media(&mut self, attachments: Vec<PromptAttachment>) {
        self.media_chips = attachment_chip_labels(&attachments);
        self.staged_media = attachments;
    }

    /// Current path-free media chips shown above the composer.
    pub fn media_chips(&self) -> &[String] {
        &self.media_chips
    }

    /// The staged attachments behind the chips (durable ids + mimes).
    pub fn staged_media(&self) -> &[PromptAttachment] {
        &self.staged_media
    }

    /// Clears staged media chips (after a discard).
    pub fn clear_media_chips(&mut self) {
        self.media_chips.clear();
        self.staged_media.clear();
        self.submitted_media.clear();
    }

    /// Drops only the chips the finished turn carried, keeping anything staged since.
    ///
    /// A stash pop or clipboard attach mid-turn stages media for the NEXT prompt and,
    /// in the stash case, has already deleted the durable row it came from; clearing
    /// every chip on completion would destroy it with nothing left to restore from.
    /// The app side removes the same consumed set from its session staging, so the two
    /// views stay in step without a further sync round-trip.
    fn clear_submitted_media(&mut self) {
        let consumed = std::mem::take(&mut self.submitted_media);
        if consumed.is_empty() {
            return;
        }

        let mut remaining = std::mem::take(&mut self.staged_media);
        for attachment in &consumed {
            if let Some(index) = remaining.iter().position(|staged| staged == attachment) {
                remaining.remove(index);
            }
        }

        self.set_staged_media(remaining);
    }

    pub fn finish_provider_turn(&mut self, outcome: TuiProviderOutcome) -> Option<String> {
        self.finish_provider_turn_scheduled(outcome)
            .map(|next| next.prompt)
    }

    fn finish_provider_turn_scheduled(
        &mut self,
        outcome: TuiProviderOutcome,
    ) -> Option<ScheduledPrompt> {
        let generation = self.active_generation()?;

        self.finish_provider_turn_scheduled_for_generation(generation, outcome)
    }

    fn finish_detached_provider_turn(&mut self, outcome: TuiProviderOutcome) {
        if let TuiProviderOutcome::Failed { message, .. } = outcome {
            self.status = Some(message);
        }
    }

    fn finish_provider_turn_scheduled_for_generation(
        &mut self,
        generation: u64,
        outcome: TuiProviderOutcome,
    ) -> Option<ScheduledPrompt> {
        if self.active_generation() != Some(generation) {
            let _ = self
                .scheduler
                .reduce(AppEvent::TurnFailedFor { generation });
            return None;
        }

        let terminal_event = match &outcome {
            TuiProviderOutcome::Completed(output) => Some(AppEvent::TurnCompletedFor {
                generation,
                output: output.clone(),
            }),
            TuiProviderOutcome::Failed { .. } => Some(AppEvent::TurnFailedFor { generation }),
            TuiProviderOutcome::Cancelled { .. } => Some(AppEvent::TurnCancelledFor { generation }),
            TuiProviderOutcome::Backgrounded => Some(AppEvent::TurnReleasedFor { generation }),
        };

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
                self.clear_submitted_media();
                self.set_foreground_presentation(false);
            }
            TuiProviderOutcome::Failed { message, action } => {
                let finishing = self.foreground_running();
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
                let finishing = self.foreground_running();
                self.assistant_streaming = false;
                self.turn_state = Some(TurnState::Cancelled);
                self.active_tool = None;
                if finishing {
                    self.settle_active_conversation();
                    self.auto_collapse_thinking_on_finish();
                }
                self.add_error(message, action);
            }
            TuiProviderOutcome::Backgrounded => self.set_foreground_presentation(false),
        }

        terminal_event.and_then(|event| next_scheduled_prompt(self.scheduler.reduce(event)))
    }

    /// Clears the current visible conversation for a new session.
    pub fn clear_transcript(&mut self) {
        self.invalidate_settled_conversations();
        self.transcript.clear();
        self.completed_conversations.clear();
        self.conversation = None;
        let record = self.active_record_mut();
        record.tool_display_modes.clear();
        record.tool_detail = widgets::DisplayMode::Collapsed;
        self.set_foreground_presentation(false);
        self.turn_state = None;
        self.placeholder_failure = false;
        self.active_tool = None;
        self.clear_current_session_transcripts();
        self.bump_selectable_epoch();
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
        self.invalidate_settled_conversations();
        self.transcript.clear();
        self.completed_conversations = conversations;
        self.conversation = None;
        self.runtime_events.clear();
        self.turn_duration = None;
        self.latest_usage = None;
        self.set_foreground_presentation(false);
        self.turn_state = None;
        self.active_tool = None;
        self.clear_current_session_transcripts();
        self.bump_selectable_epoch();
        {
            let record = self.active_record_mut();
            record.tool_display_modes.clear();
            record.tool_detail = widgets::DisplayMode::Collapsed;
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
        self.bump_selectable_epoch();
    }

    fn active_record(&self) -> &TranscriptRecord {
        self.transcripts
            .get(&self.active_transcript)
            .expect("active transcript always exists")
    }

    fn active_record_mut(&mut self) -> &mut TranscriptRecord {
        self.transcripts
            .get_mut(&self.active_transcript)
            .expect("active transcript always exists")
    }

    /// The record owning `self.conversation`, whichever transcript is on screen.
    ///
    /// Parent turn events arrive while the reader may be watching a subagent, so
    /// the transcript they belong to and the transcript being looked at are
    /// different questions. Presentation state for a parent call belongs to the
    /// former.
    fn main_record_mut(&mut self) -> &mut TranscriptRecord {
        self.transcripts
            .get_mut(&TranscriptId::Main)
            .expect("the main transcript always exists")
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
                    .map(|execution| TranscriptId::Subagent(execution.id)),
            )
            .collect()
    }

    /// Enters the tree at its first row, which is the one nearest the composer.
    fn focus_execution_strip(&mut self) {
        self.execution_selection = self.execution_strip_ids().first().copied();
    }

    /// Walks the tree, treating its ends as the edges of a list rather than a
    /// ring.
    ///
    /// Wrapping would make the tree a loop with no way out, and the way out is
    /// the point: the composer sits directly above the first row, so walking up
    /// off that row is how a reader gets back to typing.
    fn move_execution_selection(&mut self, direction: isize) {
        let ids = self.execution_strip_ids();
        let current = self.execution_selection.unwrap_or(TranscriptId::Main);
        let Some(index) = ids.iter().position(|id| *id == current) else {
            self.execution_selection = Some(TranscriptId::Main);
            return;
        };

        match index.checked_add_signed(direction) {
            None => self.execution_selection = None,
            Some(next) if next < ids.len() => self.execution_selection = ids.get(next).copied(),
            Some(_) => {}
        }
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
        screen_layout(
            area,
            &self.input,
            self.media_chips.len(),
            self.scheduler.queued_entries().len(),
        )
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
        let id = (*fitted_subagent_tree(
            &self.view_without_selectable(),
            layout.tree.height,
            layout.tree.width,
        )
        .1
        .get(index)?)?;
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

    /// Projects only cancellation IDs the execution authority confirmed active.
    pub fn apply_confirmed_cancellations(&mut self, ids: impl IntoIterator<Item = u64>) {
        for id in ids {
            self.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
                agent: String::new(),
                event: TuiExecutionEvent::CancellationRequested { id },
            });
        }
    }

    /// Retains typed runtime metrics in source order without altering turn persistence.
    pub fn apply_runtime_event_with_ordinal(&mut self, ordinal: u64, event: TuiRuntimeEvent) {
        self.next_runtime_ordinal = self.next_runtime_ordinal.max(ordinal.saturating_add(1));
        if !self.admit_runtime_event(ordinal, &event) {
            return;
        }
        self.bump_selectable_epoch();

        match &event {
            TuiRuntimeEvent::TurnStarted => {
                self.turn_state = Some(TurnState::Requesting);
                self.waiting_started_at = Some(self.now);
            }
            TuiRuntimeEvent::TurnEnded { status, duration } => {
                let finishing = self.foreground_running();
                self.turn_state = Some(*status);
                self.turn_duration = *duration;
                self.active_tool = None;
                self.waiting_started_at = None;
                if finishing {
                    self.auto_collapse_thinking_on_finish();
                }
                if *status == TurnState::Failed {
                    self.note_turn_failure();
                }
            }
            TuiRuntimeEvent::Usage(usage) => {
                self.accumulate_turn_usage(usage);
                self.latest_usage = Some(usage.clone());
            }
            TuiRuntimeEvent::Diff { call_id, lines } => {
                self.project_conversation(ConversationEvent::Diff {
                    call_id: call_id.clone(),
                    lines: lines.clone(),
                });
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
            TuiRuntimeEvent::Notice { text, severity } => match severity {
                NoticeSeverity::Info => {
                    self.transcript.push(TranscriptEntry::Info(text.clone()));
                    self.project_conversation(ConversationEvent::Info(text.clone()));
                }
                NoticeSeverity::Failure => {
                    self.transcript.push(TranscriptEntry::Error(text.clone()));
                    self.project_conversation(ConversationEvent::FailureNotice(text.clone()));
                }
            },
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
        if let agens_core::TuiSubagentUpdate::Started { agent, model, .. } = &event.update {
            self.transcripts
                .get_mut(&TranscriptId::Subagent(event.id))
                .expect("admitted child event has a transcript")
                .owner_label
                .clone_from(agent);
            if let Some(execution) = self
                .executions
                .iter_mut()
                .find(|execution| execution.id == event.id)
            {
                execution.model.clone_from(model);
            }
        }
        self.transcripts
            .get_mut(&TranscriptId::Subagent(event.id))
            .expect("admitted child event has a transcript")
            .conversation
            .get_or_insert_with(|| Conversation::new(String::new()))
            .apply_child_event(event.clone());
        if let agens_core::TuiSubagentUpdate::ToolResult { call_id, .. } = &event.update {
            let record = self
                .transcripts
                .get_mut(&TranscriptId::Subagent(event.id))
                .expect("admitted child event has a transcript");
            let detail = record.tool_detail;
            record.tool_display_modes.insert(call_id.clone(), detail);
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
                | TuiExecutionEvent::CancellationRequested { id }
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
                tool_detail: widgets::DisplayMode::Collapsed,
                collapse_thinking: false,
                history_expanded: false,
                focused_call: None,
                thinking_user_pinned: false,
                tool_overlay: None,
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

    /// Retires cached descriptions of settled turns before their history changes.
    ///
    /// Cached rows are addressed by position, so dropping or replacing retained
    /// conversations has to move the whole transcript to a new generation rather
    /// than leave a stale entry addressable at the same index.
    fn invalidate_settled_conversations(&mut self) {
        self.transcript_generation = next_transcript_generation();
    }

    fn clear_current_session_transcripts(&mut self) {
        self.invalidate_settled_conversations();
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
            let record = self.main_record_mut();
            let detail = record.tool_detail;
            record.tool_display_modes.insert(call_id, detail);
        }
        self.bump_selectable_epoch();
        Ok(())
    }

    /// Opens a generic bounded dialog without changing the underlying conversation.
    pub fn show_dialog(&mut self, title: impl Into<String>, body: impl Into<String>) {
        self.dialog = Some(DialogView::informational(title.into(), body.into()));
    }

    /// Opens the searchable prompt-history overlay (composer-anchored, newest-first, window 64).
    pub fn show_history_overlay(&mut self) {
        let entries = self
            .prompt_memory
            .as_ref()
            .map(|memory| history_overlay_entries(memory.as_ref(), ""))
            .unwrap_or_default();
        let mut dialog = DialogView::selection(
            "Prompt history",
            Some("/ search · Enter paste · Esc close"),
            entries,
        )
        .with_empty_message("No matching history.");
        dialog.composer_anchored = true;
        dialog.prompt_overlay = Some(PromptOverlayKind::History);
        self.show_selection_dialog(dialog);
    }

    /// Opens the stash pick/remove overlay (composer-anchored, newest-first, window 64).
    pub fn show_stash_overlay(&mut self) {
        let entries = self
            .prompt_memory
            .as_ref()
            .map(|memory| stash_overlay_entries(memory.as_ref(), ""))
            .unwrap_or_default();
        let mut dialog = DialogView::selection(
            "Prompt stash",
            Some("/ search · Enter paste · x/Del remove · Esc close"),
            entries,
        )
        .with_empty_message("No stashed prompts.");
        dialog.composer_anchored = true;
        dialog.prompt_overlay = Some(PromptOverlayKind::Stash);
        self.show_selection_dialog(dialog);
    }

    /// Installs the prompt history/stash port. Surfaces only route keys through this trait.
    pub fn set_prompt_memory(&mut self, memory: Box<dyn PromptMemory>) {
        self.prompt_memory = Some(memory);
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
        // Same guard for the lineage browser: a tree page that answers a request
        // the reader has already replaced must not paint over the newer one.
        if let (Some(current), Some(incoming)) = (
            self.dialog
                .as_ref()
                .and_then(|dialog| dialog.tree_entries.as_ref()),
            dialog.tree_entries.as_ref(),
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
        let mut state = self.view_without_selectable();
        let layout = self.screen_layout();
        let row_width = self.transcript_row_width();
        let visible_rows = usize::from(
            layout
                .transcript
                .height
                .saturating_sub(transcript_chrome_rows(state.following_bottom)),
        );
        let first_row = usize::from(if state.following_bottom {
            self.following_scroll_bottom()
        } else {
            state.scroll_offset
        });
        state.selectable = SharedSelectable::from_arc(self.selectable_transcript_for(
            &state,
            row_width,
            first_row,
            visible_rows,
        ));
        state
    }

    /// View fields without building the selectable index.
    ///
    /// Scroll bounds only need row counts, and rebuilding the full grapheme
    /// index for every wheel tick is what the light path exists to avoid.
    fn view_without_selectable(&self) -> ViewState<'_> {
        let active = self
            .transcripts
            .get(&self.active_transcript)
            .expect("active transcript always exists");
        ViewState {
            active_transcript: self.active_transcript,
            transcript_generation: self.transcript_generation,
            transcript_ids: std::iter::once(TranscriptId::Main)
                .chain(self.child_transcript_order.iter().copied())
                .collect(),
            owner_label: &active.owner_label,
            input: &self.input,
            media_chips: &self.media_chips,
            recovered_failed_prompt: self.recovered_failed_prompt,
            size: self.size,
            running: self.foreground_running(),
            surface_focus: self.surface_focus,
            composer_cursor_visible: self.surface_focus == SurfaceFocus::Composer,
            queue: self.scheduler.queued_entries(),
            queue_selected: self.queue_selected,
            session_loading: self.session_loading,
            assistant_streaming: self.assistant_streaming,
            quit_armed: self.quit_is_armed(),
            transcript: &active.transcript,
            following_bottom: active.following_bottom,
            scroll_offset: active.scroll_offset,
            selection: active.selection,
            selectable: SharedSelectable::empty(),
            provider_model: &self.provider_model,
            reasoning_effort: self.reasoning_effort.as_deref(),
            context_window: self.context_window,
            session: &self.session,
            project: &self.project,
            home: self.home.as_deref(),
            repository: self.repository.as_ref(),
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
            tool_detail: active.tool_detail,
            collapse_thinking: active.collapse_thinking,
            history_expanded: active.history_expanded,
            focused_call: active.focused_call.as_deref(),
            tool_overlay: active.tool_overlay.as_ref(),
            hyperlinks: self.hyperlinks,
            color_level: self.color_level,
            unicode_level: self.unicode_level,
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
            ask_user: self.ask_user.as_ref().map(AskUserRender::of),
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
            turn_activity: self.current_activity(),
        }
    }

    /// Content width available to selectable transcript rows.
    fn transcript_row_width(&self) -> u16 {
        self.screen_layout()
            .transcript
            .width
            .saturating_sub(TRANSCRIPT_ROW_INDENT)
            .max(1)
    }

    /// Invalidates the cached selectable index after transcript content or modes change.
    fn bump_selectable_epoch(&mut self) {
        self.selectable_epoch = self.selectable_epoch.saturating_add(1);
        self.selectable_cache.borrow_mut().take();
    }

    /// Returns the selectable transcript for the active view, rebuilding only on key mismatch.
    fn selectable_transcript_for(
        &self,
        state: &ViewState<'_>,
        row_width: u16,
        first_row: usize,
        visible_rows: usize,
    ) -> Arc<SelectableTranscript> {
        let row_width = row_width.max(1);
        let epoch = self.selectable_epoch;
        let transcript_id = state.active_transcript;
        let wave_frame = widgets::RowAccent::wave_frame(state.now);

        if let Some(cache) = self.selectable_cache.borrow().as_ref()
            && cache.epoch == epoch
            && cache.transcript_id == transcript_id
            && cache.row_width == row_width
            && cache.first_row == first_row
            && cache.visible_rows == visible_rows
            && cache.animated_at.is_none_or(|frame| frame == wave_frame)
        {
            return Arc::clone(&cache.transcript);
        }

        let _perf_select = agens_perf::span!("tui.transcript.select", row_width = row_width);
        // Content only: live turn status is time-varying and painted separately.
        let lines = rendered_transcript_content(state, row_width);
        let animated_at = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .any(|span| widgets::RowAccent::is_wave_span(span, state.unicode_level))
            .then_some(wave_frame);
        let transcript = if lines.len() <= visible_rows {
            SelectableTranscript::from_lines(&lines, row_width)
        } else {
            let plan = WrapPlan::from_lines(&lines, row_width);
            SelectableTranscript::window(&plan, first_row, visible_rows)
        };
        let transcript = Arc::new(transcript);
        *self.selectable_cache.borrow_mut() = Some(SelectableCache {
            epoch,
            transcript_id,
            row_width,
            first_row,
            visible_rows,
            animated_at,
            transcript: Arc::clone(&transcript),
        });
        transcript
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
            TuiExecutionEvent::CancellationRequested { id } => {
                (id, TuiExecutionState::CancellationRequested)
            }
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
        // A cancellation is a decision someone already took, not news: waking
        // the model for it made a cancelled duplicate answer anyway, which is
        // exactly what cancelling it was meant to prevent.
        let finished_in_background = execution.state == TuiExecutionState::BackgroundRunning
            && !matches!(
                state,
                TuiExecutionState::BackgroundRunning | TuiExecutionState::Cancelled
            );
        execution.state = state;
        execution.last_activity = self.now;
        if !matches!(
            state,
            TuiExecutionState::ForegroundRunning
                | TuiExecutionState::BackgroundRunning
                | TuiExecutionState::CancellationRequested
        ) {
            execution.terminal_at = Some(self.now);
        }
        if finished_in_background {
            let _ = self.scheduler.reduce(AppEvent::DeferAutoTurn);
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
            model: None,
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
                TuiExecutionState::ForegroundRunning
                    | TuiExecutionState::BackgroundRunning
                    | TuiExecutionState::CancellationRequested
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
        // An overlay owns the whole screen while it is up, so a press under it
        // belongs to the overlay and must not disturb what is behind it.
        if self.dialog.is_some() || self.palette_open {
            return Action::Render;
        }

        let snapshot = self.capture_mouse_selection_snapshot();
        let position = snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.position(column, row));

        let Some(position) = position else {
            // A press that lands on no transcript row is the reader putting the
            // selection down. Leaving it painted, and the transcript focused,
            // is what used to make dismissing it cost a keypress of its own.
            self.clear_selection();
            self.active_record_mut().focus = TranscriptFocus::Composer;
            return Action::Render;
        };

        self.mouse_selection_snapshot = snapshot;
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

        let mut scrolled = false;
        if dragging {
            scrolled = self.auto_scroll_selection_edge(row);
        }

        let previous_head = self
            .transcripts
            .get(&self.active_transcript)
            .expect("active transcript always exists")
            .selection
            .map(|selection| selection.head);

        if let Some(position) = self
            .mouse_selection_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.position(column, row))
            && let Some(selection) = self.active_record_mut().selection.as_mut()
        {
            selection.head = position;
        }

        if dragging {
            let head = self
                .transcripts
                .get(&self.active_transcript)
                .expect("active transcript always exists")
                .selection
                .map(|selection| selection.head);
            if !scrolled && head == previous_head {
                return Action::Unchanged;
            }
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
                // A press that selected nothing is a plain click, and a plain
                // click is how the reader dismisses what was selected before.
                record.selection = None;
                record.selection_text = None;
                record.selection_too_large = false;
                record.focus = TranscriptFocus::Composer;
            }
            Some(Err(())) => {
                record.selection_text = None;
                record.selection_too_large = true;
            }
        }
        Action::Render
    }

    /// Scrolls the transcript one row when a drag rests on a content edge.
    ///
    /// Returns whether the scroll offset moved, so the caller can repaint even
    /// when the pointer column did not change.
    fn auto_scroll_selection_edge(&mut self, row: u16) -> bool {
        let Some(snapshot) = self.mouse_selection_snapshot.as_ref() else {
            return false;
        };
        let content_y = snapshot.content_y;
        let content_bottom = snapshot.content_bottom;
        if content_bottom <= content_y {
            return false;
        }

        let direction = if row <= content_y.saturating_add(1) {
            -1_i16
        } else if row.saturating_add(1) >= content_bottom {
            1_i16
        } else {
            return false;
        };

        let previous = {
            let record = self
                .transcripts
                .get(&self.active_transcript)
                .expect("active transcript always exists");
            if record.following_bottom {
                self.following_scroll_bottom()
            } else {
                record.scroll_offset
            }
        };

        if direction < 0 {
            if previous == 0 {
                return false;
            }
            let record = self.active_record_mut();
            record.following_bottom = false;
            record.scroll_offset = previous.saturating_sub(1);
        } else {
            let bottom = self.detached_scroll_bottom();
            if previous >= bottom {
                return false;
            }
            let record = self.active_record_mut();
            record.following_bottom = false;
            record.scroll_offset = previous.saturating_add(1).min(bottom);
        }

        let scroll = self
            .transcripts
            .get(&self.active_transcript)
            .expect("active transcript always exists")
            .scroll_offset;
        self.rewindow_mouse_selection_snapshot(scroll);
        true
    }

    /// Keeps the drag snapshot's window on the rows `scroll` currently shows.
    ///
    /// The snapshot stores a bounded selectable window, not the whole
    /// transcript. Updating only `scroll` would map the pointer onto absolute
    /// rows the window no longer holds, so a drag that auto-scrolls at the
    /// edge would lose the head and dismiss the selection.
    fn rewindow_mouse_selection_snapshot(&mut self, scroll: u16) {
        let Some(snapshot) = self.mouse_selection_snapshot.as_ref() else {
            return;
        };
        if snapshot.scroll == scroll && snapshot.transcript.first_row == usize::from(scroll) {
            return;
        }

        let view = self.view_without_selectable();
        let layout = self.screen_layout();
        let row_width = layout
            .transcript
            .width
            .saturating_sub(TRANSCRIPT_ROW_INDENT)
            .max(1);
        let visible_rows = usize::from(
            layout
                .transcript
                .height
                .saturating_sub(transcript_chrome_rows(view.following_bottom)),
        );
        let transcript =
            self.selectable_transcript_for(&view, row_width, usize::from(scroll), visible_rows);

        if let Some(snapshot) = self.mouse_selection_snapshot.as_mut() {
            snapshot.scroll = scroll;
            snapshot.transcript = transcript;
        }
    }

    fn capture_mouse_selection_snapshot(&self) -> Option<MouseSelectionSnapshot> {
        if self.dialog.is_some() || self.palette_open {
            return None;
        }
        let view = self.view_without_selectable();
        let layout = self.screen_layout();
        let row_width = layout
            .transcript
            .width
            .saturating_sub(TRANSCRIPT_ROW_INDENT)
            .max(1);
        let chrome_rows = transcript_chrome_rows(view.following_bottom);
        let visible_rows = usize::from(layout.transcript.height.saturating_sub(chrome_rows));
        let first_row = if view.following_bottom {
            self.following_scroll_bottom()
        } else {
            view.scroll_offset
        };
        let transcript =
            self.selectable_transcript_for(&view, row_width, usize::from(first_row), visible_rows);
        let bottom = saturating_u16(transcript.total_rows().saturating_sub(visible_rows));
        let scroll = if view.following_bottom {
            bottom
        } else {
            view.scroll_offset.min(bottom)
        };

        Some(MouseSelectionSnapshot {
            transcript,
            // Cells start at column 0 of the paragraph content, which already
            // includes the accent bar. The block's left padding is the only
            // chrome outside that coordinate space.
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
        self.bump_selectable_epoch();
        match event {
            TurnEvent::ProviderPart(MessagePart::Text(delta)) => {
                self.project_conversation(ConversationEvent::MarkdownDelta(delta.clone()));
                self.turn_state = Some(TurnState::Streaming);
                self.clear_provider_retry();
                self.reasoning_started_at = None;
                self.waiting_started_at = None;
                match self.transcript.last_mut() {
                    Some(TranscriptEntry::Assistant(text)) => text.push_str(&delta),
                    _ => self.transcript.push(TranscriptEntry::Assistant(delta)),
                }
            }
            TurnEvent::ProviderPart(MessagePart::Reasoning(delta)) => {
                self.project_conversation(ConversationEvent::ReasoningDelta(delta.clone()));
                self.clear_provider_retry();
                self.waiting_started_at = None;
                if self.reasoning_started_at.is_none() {
                    self.reasoning_started_at = Some(self.now);
                }
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
                self.clear_provider_retry();
                self.reasoning_started_at = None;
                self.waiting_started_at = None;
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
            TurnEvent::StateChanged(TurnState::Completed) => {
                self.set_foreground_presentation(false);
                self.turn_state = Some(TurnState::Completed);
            }
            TurnEvent::StateChanged(state @ (TurnState::Cancelled | TurnState::Failed)) => {
                self.set_foreground_presentation(false);
                self.turn_state = Some(state);
                if state == TurnState::Failed {
                    self.note_turn_failure();
                }
            }
            TurnEvent::StateChanged(state) => self.turn_state = Some(state),
            TurnEvent::ProviderRetry {
                attempt,
                max_attempts,
                delay,
                reason,
            } => {
                self.pending_retry = Some(RetryActivity {
                    attempt,
                    max_attempts,
                    delay,
                    reason,
                });
            }
            _ => {}
        }
    }

    /// Drops the pending retry once the attempt it was waiting for produced
    /// something. A retry is only ever cleared by evidence of progress or by
    /// the turn ending — never by a timer, which would leave a long backoff
    /// silent again.
    fn clear_provider_retry(&mut self) {
        self.pending_retry = None;
    }

    fn current_activity(&self) -> TurnActivity<'_> {
        activity::TurnActivity::derive(activity::ActivityInputs {
            turn_state: self.turn_state,
            running: self.foreground_running(),
            session_loading: self.session_loading,
            active_tool: self.active_tool.as_deref(),
            retry: self.pending_retry,
            reasoning_elapsed: self
                .reasoning_started_at
                .map(|started| self.now.saturating_sub(started)),
            waiting_elapsed: self
                .waiting_started_at
                .map(|started| self.now.saturating_sub(started)),
        })
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
                return Action::CancelRoute;
            }
            _ => {}
        }
        Action::Render
    }

    /// Opens a bounded structured question set as a dedicated modal
    /// interaction, sibling to secret entry and device authentication.
    pub fn open_ask_user(&mut self, id: u64, request: AskUserRequest) {
        self.open_ask_user_from(id, request, None);
    }

    /// Opens an interaction that names the delegated execution it came from,
    /// so the overlay can say who is asking.
    pub fn open_ask_user_from(
        &mut self,
        id: u64,
        request: AskUserRequest,
        origin: Option<PromptOrigin>,
    ) {
        self.ask_user = Some(AskUserState::new(id, request, origin));
    }

    /// A read-only snapshot of the open ask-user interaction, if one exists.
    pub fn ask_user_snapshot(&self) -> Option<AskUserSnapshot> {
        self.ask_user.as_ref().map(AskUserState::snapshot)
    }

    /// Closes the open ask-user overlay if it is the one identified by
    /// `id`, without resolving anything itself. Returns whether it closed
    /// one.
    ///
    /// This is for a request the BRIDGE resolved on its own — a deadline
    /// expiring, an external cancellation, or the surface closing — none of
    /// which travel through `Action::AskUserReply`. Without this, the
    /// overlay would keep holding the keyboard for a turn that has already
    /// ended, and whatever the person then typed or submitted into it would
    /// be silently dropped rather than answering anyone.
    pub fn dismiss_ask_user(&mut self, id: u64) -> bool {
        if self.ask_user.as_ref().is_some_and(|state| state.id() == id) {
            self.ask_user = None;
            true
        } else {
            false
        }
    }

    /// The context pane's scroll ceiling in the frame the reader is looking at.
    ///
    /// Measured from the same geometry the renderer solves, so `End` stores a
    /// real offset instead of a sentinel the next `PageUp` would have to walk
    /// back from one step at a time.
    fn ask_user_max_context_scroll(&self) -> u16 {
        let Some(state) = self.ask_user.as_ref() else {
            return 0;
        };
        let render = AskUserRender::of(state);
        let area = Rect::new(0, 0, self.size.0, self.size.1);
        let composer = self.screen_layout().composer;
        ask_user_frame(area, composer, &render).map_or(0, |laid| laid.max_context_scroll)
    }

    fn clamp_ask_user_context_scroll(&mut self) {
        let max = self.ask_user_max_context_scroll();
        if let Some(state) = self.ask_user.as_mut() {
            state.clamp_context_scroll(max);
        }
    }

    fn handle_ask_user_key(&mut self, key: Key) -> Action {
        let max_context_scroll = self.ask_user_max_context_scroll();
        let Some(state) = self.ask_user.as_mut() else {
            return Action::Render;
        };
        let outcome = state.reduce(key, max_context_scroll);
        let id = state.id();
        match outcome {
            AskUserOutcome::Unchanged => Action::Unchanged,
            AskUserOutcome::Changed => {
                self.clamp_ask_user_context_scroll();
                Action::Render
            }
            AskUserOutcome::Resolved(reply) => {
                self.ask_user = None;
                Action::AskUserReply { id, reply }
            }
        }
    }

    fn handle_key(&mut self, key: Key) -> Action {
        if key != Key::CtrlC {
            self.quit_armed_until = None;
        }
        if key == Key::CtrlShiftC {
            return self.handle_copy_selection();
        }
        if key == Key::CtrlC {
            return self.handle_control_c();
        }
        if self.device_auth.is_some() {
            return self.handle_device_auth_key(key);
        }
        if self.secret_entry.is_some() {
            return self.handle_secret_key(key);
        }
        if self.ask_user.is_some() {
            return self.handle_ask_user_key(key);
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
            && !(self.viewport_focused() && key.moves_viewport())
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

        if key == Key::Escape && self.tool_overlay_is_open() {
            self.close_tool_detail_overlay();
            return Action::Render;
        }

        if self.tool_overlay_is_open() {
            match key {
                Key::Up | Key::CtrlK => {
                    self.scroll_tool_overlay(-1);
                    return Action::Render;
                }
                Key::Down | Key::CtrlJ => {
                    self.scroll_tool_overlay(1);
                    return Action::Render;
                }
                Key::PageUp => {
                    self.scroll_tool_overlay(-10);
                    return Action::Render;
                }
                Key::PageDown => {
                    self.scroll_tool_overlay(10);
                    return Action::Render;
                }
                Key::CtrlO => {
                    self.cycle_tool_detail(true);
                    return Action::Render;
                }
                Key::CtrlShiftO => {
                    self.cycle_tool_detail(false);
                    return Action::Render;
                }
                _ => {}
            }
        }

        if key == Key::Escape && self.dialog.is_some() {
            self.dialog = None;
            return Action::Render;
        }

        if !self.palette_open && !self.file_picker_open() && key == Key::Tab {
            self.toggle_queue_focus();
            return Action::Render;
        }

        if let Some(action) = self.handle_surface_focus_key(key) {
            return action;
        }

        // Prompt history browse owns empty Up and in-browse Up/Down before the
        // execution strip claims empty Down.
        if !self.palette_open
            && !self.file_picker_open()
            && !self.viewport_focused()
            && let Some(action) = self.handle_prompt_history_key(key)
        {
            return action;
        }

        if !self.palette_open && !self.file_picker_open() && !self.executions.is_empty() {
            match key {
                // The tree hangs below the composer, so walking down out of the
                // prompt walks into it. Only from an empty prompt: with text in
                // it, Down belongs to the text.
                Key::Down
                    if self.execution_selection.is_none()
                        && self.input.is_empty()
                        && !self.viewport_focused()
                        && !self
                            .prompt_memory
                            .as_ref()
                            .is_some_and(|memory| memory.is_browsing()) =>
                {
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
                // Main is where the reader already is, so accepting it is a way
                // back to the prompt rather than a transcript switch.
                Key::Enter if self.execution_selection == Some(TranscriptId::Main) => {
                    self.select_transcript(TranscriptId::Main);
                    self.execution_selection = None;
                    return Action::Render;
                }
                Key::Enter if self.execution_selection.is_some() => {
                    self.inspect_execution_selection();
                    return Action::Render;
                }
                Key::CtrlB if self.execution_selection.is_some() => {
                    return self.handle_background_key();
                }
                // Cancellation belongs to the tree only while the reader is
                // standing in it; with no row selected these are just letters.
                Key::Char('x') if self.execution_selection.is_some() => {
                    if let Some(action) = self.selected_execution_cancellation() {
                        return action;
                    }
                }
                Key::Char('X') if self.execution_selection.is_some() => {
                    return Action::CancelAllExecutions;
                }
                _ => {}
            }
        }

        if key == Key::Escape && self.active_transcript != TranscriptId::Main {
            self.select_transcript(TranscriptId::Main);
            return Action::Render;
        }

        // The shortcut list answers "what can I press", so it answers from
        // wherever the question was asked — including on top of an open dialog.
        if key == Key::CtrlQuestion {
            self.palette_open = false;
            self.show_selection_dialog(shortcuts::shortcuts_dialog());
            return Action::Render;
        }

        // With nothing left to dismiss, Esc is how the reader leaves the prompt
        // for Normal mode. It stays inert on the turn itself: focusing the
        // transcript mid-stream is a way to read, never a way to cancel.
        if key == Key::Escape {
            let record = self.active_record_mut();
            if record.focus == TranscriptFocus::Composer {
                record.focus = TranscriptFocus::Viewport;
            }
            return Action::Render;
        }

        // An open palette or picker is typed into, so it owns the alphabet even
        // when the transcript behind it still holds focus.
        if self.viewport_focused()
            && !self.palette_open
            && !self.file_picker_open()
            && let Some(action) = self.handle_viewport_key(key)
        {
            return action;
        }

        match key {
            Key::Home if self.active_transcript != TranscriptId::Main => {
                self.scroll_to_start();
                return Action::Render;
            }
            Key::End if self.active_transcript != TranscriptId::Main => {
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
                    | Key::DeleteNextWord
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

        if key == Key::CtrlS {
            return self.handle_prompt_stash_key();
        }

        if key == Key::CtrlV {
            return Action::AttachClipboardImage;
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

        // The lineage browser opens over the transcript, never over a palette or
        // picker: those are being typed into, and an overlay that stole the next
        // keystroke would abandon a half-written command with no way back to it.
        if key == Key::CtrlR {
            if self.palette_open || self.file_picker_open() {
                return Action::Render;
            }
            return self.start_session_tree_request(SessionTreeRequest::active());
        }

        if matches!(
            key,
            Key::Char(_)
                | Key::ShiftEnter
                | Key::Backspace
                | Key::Delete
                | Key::DeletePreviousWord
                | Key::DeleteNextWord
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
                self.cycle_tool_detail(true);
                Action::Render
            }
            Key::CtrlShiftO => {
                self.cycle_tool_detail(false);
                Action::Render
            }
            Key::CtrlT => {
                self.toggle_thinking_expansion();
                Action::Render
            }
            Key::CtrlY => {
                let record = self.active_record_mut();
                record.history_expanded = !record.history_expanded;
                self.bump_selectable_epoch();
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
                self.jump_to_user_message(UserMessageTarget::Last);
                Action::Render
            }
            Key::CtrlShiftN => {
                self.jump_to_user_message(UserMessageTarget::Previous);
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
            Key::Enter
                if (self.input.is_empty() && self.media_chips.is_empty())
                    || self.session_loading =>
            {
                Action::Render
            }
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
            Key::Enter => self.submit_composer_or_palette(),
            Key::Escape => unreachable!("Escape is handled before focused input"),
            Key::CtrlC => unreachable!("Ctrl+C is handled before focused input"),
            _ => unreachable!("composer keys are handled before global keys"),
        }
    }

    /// Tab toggles between the prompt and the queue sitting above it.
    ///
    /// The subagent tree used to be the third stop on this ring; it is reached
    /// with the down arrow instead, which is the direction it actually lies in.
    fn toggle_queue_focus(&mut self) {
        self.surface_focus = match self.surface_focus {
            SurfaceFocus::Composer => {
                self.queue_selected = (!self.scheduler.queued_entries().is_empty()).then_some(0);
                SurfaceFocus::Queue
            }
            SurfaceFocus::Queue => SurfaceFocus::Composer,
        };
    }

    fn handle_surface_focus_key(&mut self, key: Key) -> Option<Action> {
        if self.surface_focus != SurfaceFocus::Queue {
            return None;
        }

        let entries = self.scheduler.queued_entries();
        if entries.is_empty() {
            self.queue_selected = None;
            self.surface_focus = SurfaceFocus::Composer;
            return None;
        }
        let selected = self.queue_selected.unwrap_or(0).min(entries.len() - 1);
        match key {
            Key::Up => self.queue_selected = Some(selected.saturating_sub(1)),
            Key::Down => self.queue_selected = Some((selected + 1).min(entries.len() - 1)),
            Key::AltUp => self.move_selected_queue_entry(-1),
            Key::AltDown => self.move_selected_queue_entry(1),
            Key::Delete => self.remove_selected_queue_entry(),
            Key::Enter => self.edit_selected_queue_entry(),
            key if key.edits_composer() => {
                self.surface_focus = SurfaceFocus::Composer;
                self.queue_selected = None;
                return None;
            }
            _ => return Some(Action::Render),
        }
        Some(Action::Render)
    }

    /// Resolves Enter from the composer, including an open slash palette.
    ///
    /// Busy turns must use the same palette selection path as idle ones: otherwise
    /// Enter on `/` with a highlighted row submits the bare slash instead of the
    /// selected command or dialog.
    fn submit_composer_or_palette(&mut self) -> Action {
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

        if self.foreground_running() && self.busy_policy_routing {
            let prompt = self.provider_prompt();
            self.record_prompt_history(&prompt);
            return Action::SubmitBusy(prompt);
        }
        if self.foreground_running() {
            return self.enqueue_composer();
        }

        self.input_cursor = 0;
        self.recovered_failed_prompt = false;
        // Keep media_chips until the provider turn completes successfully so a
        // preflight / early failure can leave staged chips visible for retry.
        let prompt = self.provider_prompt();
        self.input.clear();
        self.attachment_tokens.clear();
        self.record_prompt_history(&prompt);
        Action::Submit(prompt)
    }

    fn move_selected_queue_entry(&mut self, offset: isize) {
        let Some(selected) = self.queue_selected else {
            return;
        };
        let id = {
            let entries = self.scheduler.queued_entries();
            let Some(entry) = entries.get(selected) else {
                return;
            };
            entry.id()
        };
        if self.scheduler.move_queue_entry(id, offset) {
            self.queue_selected = Some(
                selected
                    .saturating_add_signed(offset)
                    .min(self.scheduler.queued_entries().len().saturating_sub(1)),
            );
        }
    }

    fn remove_selected_queue_entry(&mut self) {
        let Some(selected) = self.queue_selected else {
            return;
        };
        let id = {
            let entries = self.scheduler.queued_entries();
            let Some(entry) = entries.get(selected) else {
                return;
            };
            entry.id()
        };
        let _ = self.scheduler.remove_queue_entry(id);
        let remaining = self.scheduler.queued_entries().len();
        self.queue_selected = (remaining > 0).then_some(selected.min(remaining - 1));
    }

    fn edit_selected_queue_entry(&mut self) {
        let Some(selected) = self.queue_selected else {
            return;
        };
        let id = {
            let entries = self.scheduler.queued_entries();
            let Some(entry) = entries.get(selected) else {
                return;
            };
            entry.id()
        };
        let Some(entry) = self.scheduler.remove_queue_entry(id) else {
            return;
        };
        self.input = entry.prompt().to_owned();
        self.input_cursor = self.input.chars().count();
        self.surface_focus = SurfaceFocus::Composer;
        self.queue_selected = None;
    }

    fn enqueue_composer(&mut self) -> Action {
        let draft = self.input.clone();
        let effects = self.scheduler.reduce(AppEvent::SubmitPrompt(draft.clone()));
        if let Some(Effect::RefusePrompt(message)) = effects.first() {
            self.status = Some(message.clone());
            return Action::Render;
        }
        self.record_prompt_history(&draft);
        self.input.clear();
        self.input_cursor = 0;
        self.recovered_failed_prompt = false;
        self.surface_focus = SurfaceFocus::Composer;
        Action::Render
    }

    /// Clears the composer TEXT only.
    ///
    /// Staged chips survive: this runs from paths that emit no [`Action`], so dropping
    /// them here would take the chips off the surface while the session still holds the
    /// same media, and the next submit would ship attachments the user can no longer see.
    /// Chips are released when the turn that carried them completes.
    fn clear_composer(&mut self) {
        self.input.clear();
        self.input_cursor = 0;
        self.attachment_tokens.clear();
        self.recovered_failed_prompt = false;
    }

    fn enqueue_resolved_composer(&mut self, display: String, prompt: String) {
        let effects = self.scheduler.reduce(AppEvent::QueuePrompt {
            display,
            prompt: prompt.clone(),
        });
        if let Some(Effect::RefusePrompt(message)) = effects.first() {
            self.status = Some(message.clone());
            return;
        }
        self.record_prompt_history(&prompt);
        self.clear_composer();
        self.surface_focus = SurfaceFocus::Composer;
    }

    /// Records a submitted prompt with the attachments staged when it was sent.
    ///
    /// A failed durable write is reported once per session: the prompt itself still went
    /// through, so repeating the notice on every submit would bury the turn's own output,
    /// but staying silent would let history quietly stop recording after a loud open.
    fn record_prompt_history(&mut self, text: &str) {
        let Some(memory) = self.prompt_memory.as_mut() else {
            return;
        };

        if memory.record_submission(text, &self.staged_media).is_err()
            && !self.prompt_history_write_reported
        {
            self.prompt_history_write_reported = true;
            self.add_info("Could not save this prompt to history.");
        }
    }

    /// Applies restored composer content (text plus attachments) after a stash
    /// pop, overlay paste, or history browse step.
    fn apply_prompt_recall(&mut self, recall: PromptRecall) -> Action {
        self.apply_composer_text(recall.text);
        self.apply_restored_attachments(recall.attachments)
    }

    /// Replaces the staged chips with a restored attachment set.
    ///
    /// Returns [`Action::SyncStagedMedia`] when the set changed, so the app
    /// side replaces its session staging to match; the composer alone deciding
    /// what a submit sends would leave the two views divergent.
    fn apply_restored_attachments(&mut self, attachments: Vec<PromptAttachment>) -> Action {
        if self.staged_media == attachments {
            return Action::Render;
        }

        self.set_staged_media(attachments.clone());
        Action::SyncStagedMedia(attachments)
    }

    fn apply_composer_text(&mut self, text: String) {
        self.input = text;
        self.input_cursor = self.input.chars().count();
        self.recovered_failed_prompt = false;
        self.clamp_palette_selection();
        self.refresh_file_picker();
        self.active_record_mut().focus = TranscriptFocus::Composer;
    }

    /// Up/Down linear history browse. Returns `None` when the key is not history-owned.
    fn handle_prompt_history_key(&mut self, key: Key) -> Option<Action> {
        match key {
            Key::Up => {
                let recall = self
                    .prompt_memory
                    .as_mut()?
                    .browse_up(&self.input, &self.staged_media)?;
                self.execution_selection = None;
                Some(self.apply_prompt_recall(recall))
            }
            Key::Down
                if self
                    .prompt_memory
                    .as_ref()
                    .is_some_and(|memory| memory.is_browsing()) =>
            {
                let result = self.prompt_memory.as_mut()?.browse_down();
                match result {
                    HistoryBrowseResult::Entry(recall)
                    | HistoryBrowseResult::RestoreDraft(recall) => {
                        Some(self.apply_prompt_recall(recall))
                    }
                    HistoryBrowseResult::Idle => None,
                }
            }
            _ => None,
        }
    }

    /// Ctrl+S: push a non-empty draft (text and/or staged attachments), or pop
    /// the LIFO top when the composer is fully empty. Never submits.
    ///
    /// Staged attachments with empty text still push — popping a stashed
    /// prompt on top of visible chips would silently merge two drafts.
    fn handle_prompt_stash_key(&mut self) -> Action {
        if self.prompt_memory.is_none() {
            return Action::Render;
        }

        if !self.input.is_empty() || !self.staged_media.is_empty() {
            let text = std::mem::take(&mut self.input);
            let chips = std::mem::take(&mut self.media_chips);
            let attachments = std::mem::take(&mut self.staged_media);
            self.input_cursor = 0;
            self.recovered_failed_prompt = false;

            let push_failed = {
                let memory = self.prompt_memory.as_mut().expect("checked above");
                memory.clear_browse();
                memory.stash_push(&text, &attachments).is_err()
            };

            if push_failed {
                // Restore draft text and chips when durable push fails so the
                // user does not lose input.
                self.input = text;
                self.input_cursor = self.input.chars().count();
                self.media_chips = chips;
                self.staged_media = attachments;
                self.add_info("Could not save to stash.");
                self.clamp_palette_selection();
                self.refresh_file_picker();
                return Action::Render;
            }

            self.add_info("Saved to stash.");
            self.clamp_palette_selection();
            self.refresh_file_picker();
            if attachments.is_empty() {
                return Action::Render;
            }
            // The chips moved into the stash; the app-side staging must follow.
            return Action::SyncStagedMedia(Vec::new());
        }

        let popped = {
            let memory = self.prompt_memory.as_mut().expect("checked above");
            match memory.stash_pop() {
                Ok(Some(recall)) => {
                    memory.clear_browse();
                    Ok(Some(recall))
                }
                Ok(None) => Ok(None),
                Err(_) => Err(()),
            }
        };

        match popped {
            Ok(Some(recall)) => self.apply_prompt_recall(recall),
            Ok(None) => Action::Render,
            // A read that failed is not an empty stash, and the push path says so out loud.
            Err(()) => {
                self.add_info("Could not read the stash.");
                Action::Render
            }
        }
    }

    fn handle_composer_key(&mut self, key: Key) -> Option<Action> {
        let key = key.composer_equivalent();
        if let Some(action) = self.delete_adjacent_attachment(key) {
            self.clamp_palette_selection();
            self.refresh_file_picker();
            self.active_record_mut().focus = TranscriptFocus::Composer;
            return Some(action);
        }
        let cursor = self.input_cursor;
        match key {
            Key::Char(character) => self.insert_text(&character.to_string()),
            Key::ShiftEnter => self.insert_text("\n"),
            Key::Backspace if cursor > 0 => {
                self.replace_chars(previous_grapheme_boundary(&self.input, cursor), cursor, "");
            }
            Key::Delete => {
                self.replace_chars(cursor, next_grapheme_boundary(&self.input, cursor), "");
            }
            Key::DeletePreviousWord => {
                self.replace_chars(previous_word_boundary(&self.input, cursor), cursor, "");
            }
            Key::DeleteNextWord => {
                self.replace_chars(cursor, next_word_boundary(&self.input, cursor), "");
            }
            Key::DeleteToLineStart => {
                self.replace_chars(line_start(&self.input, cursor), cursor, "");
            }
            Key::DeleteToLineEnd => {
                self.replace_chars(cursor, line_end(&self.input, cursor), "");
            }
            Key::Left => {
                self.input_cursor = self
                    .attachment_tokens
                    .iter()
                    .find(|token| token.end() == cursor)
                    .map_or_else(
                        || previous_grapheme_boundary(&self.input, cursor),
                        |token| token.start,
                    );
            }
            Key::Right => {
                self.input_cursor = self
                    .attachment_tokens
                    .iter()
                    .find(|token| token.start == cursor)
                    .map_or_else(
                        || next_grapheme_boundary(&self.input, cursor),
                        AttachmentToken::end,
                    );
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

    fn selected_execution_cancellation(&self) -> Option<Action> {
        let TranscriptId::Subagent(id) = self.execution_selection? else {
            return Some(Action::Render);
        };
        self.executions
            .iter()
            .any(|execution| {
                execution.id == id
                    && matches!(
                        execution.state,
                        TuiExecutionState::ForegroundRunning | TuiExecutionState::BackgroundRunning
                    )
            })
            .then_some(Action::CancelExecution(id))
            .or(Some(Action::Render))
    }

    fn handle_mouse_wheel_batch(&mut self, directions: &[MouseWheelDirection]) -> Action {
        let _perf_event = agens_perf::span!(
            "tui.event.wheel_batch",
            kind = "mouse_wheel",
            batch = directions.len() as u64,
        );
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

        let following_bottom = self.following_scroll_bottom();
        let detached_bottom = self.detached_scroll_bottom();
        let record = self.active_record_mut();
        for direction in directions {
            match direction {
                MouseWheelDirection::Up => {
                    let current = if record.following_bottom {
                        following_bottom
                    } else {
                        record.scroll_offset.min(detached_bottom)
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
                        .min(detached_bottom);
                    record.following_bottom = record.scroll_offset == detached_bottom;
                }
            }
        }
        Action::Render
    }

    // Scrolling moves the viewport and nothing else. Focus is a mode now, and a
    // mode that Ctrl+K or the wheel could switch by accident would leave the
    // reader typing into a prompt that had quietly stopped accepting text.
    fn scroll_up(&mut self, rows: u16) {
        let following_bottom = self.following_bottom();
        let bottom = if following_bottom {
            self.following_scroll_bottom()
        } else {
            self.detached_scroll_bottom()
        };
        let record = self.active_record_mut();
        let current = if following_bottom {
            bottom
        } else {
            record.scroll_offset.min(bottom)
        };
        record.following_bottom = false;
        record.scroll_offset = current.saturating_sub(rows);
    }

    fn scroll_down(&mut self, rows: u16) {
        let bottom = self.detached_scroll_bottom();
        let record = self.active_record_mut();
        if record.following_bottom {
            return;
        }
        record.scroll_offset = record.scroll_offset.saturating_add(rows).min(bottom);
        record.following_bottom = record.scroll_offset == bottom;
    }

    fn scroll_to_start(&mut self) {
        let record = self.active_record_mut();
        record.following_bottom = false;
        record.scroll_offset = 0;
    }

    fn scroll_to_end(&mut self) {
        let scroll_offset = self.following_scroll_bottom();
        let record = self.active_record_mut();
        record.following_bottom = true;
        record.scroll_offset = scroll_offset;
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
        let view = self.view_without_selectable();
        let visible_rows = usize::from(layout.transcript.height.saturating_sub(chrome_rows));
        let row_width = layout
            .transcript
            .width
            .saturating_sub(TRANSCRIPT_ROW_INDENT);
        let rows =
            SelectableTranscript::row_count(&rendered_transcript(&view, row_width), row_width);
        saturating_u16(rows.saturating_sub(visible_rows))
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
        // Slash commands remain available while a turn is running; busy policy
        // decides which ones execute immediately, queue as provider turns, or refuse.
        if self.input == "/" {
            self.palette_open = true;
            self.palette_selected = 0;
        }
        if !self.foreground_running() && text == "@" {
            self.open_file_picker();
        }
        self.clamp_palette_selection();
    }

    fn replace_chars(&mut self, start: usize, end: usize, replacement: &str) {
        let character_count = self.input.chars().count();
        let start = grapheme_boundary_at_or_before(&self.input, start.min(character_count));
        let end = grapheme_boundary_at_or_after(&self.input, end.min(character_count)).max(start);
        let start_byte = byte_index(&self.input, start);
        let end_byte = byte_index(&self.input, end);
        let replacement_length = replacement.chars().count();
        let removed_length = end - start;
        self.input.replace_range(start_byte..end_byte, replacement);
        if replacement_length >= removed_length {
            let shift = replacement_length - removed_length;
            for token in &mut self.attachment_tokens {
                if token.start >= end {
                    token.start += shift;
                }
            }
        } else {
            let shift = removed_length - replacement_length;
            for token in &mut self.attachment_tokens {
                if token.start >= end {
                    token.start = token.start.saturating_sub(shift);
                }
            }
        }
        self.input_cursor = start + replacement_length;
        // Composer edits abandon history browse while keeping the current input.
        if let Some(memory) = self.prompt_memory.as_mut() {
            memory.clear_browse();
        }
    }

    fn viewport_focused(&self) -> bool {
        self.transcripts
            .get(&self.active_transcript)
            .expect("active transcript always exists")
            .focus
            == TranscriptFocus::Viewport
    }

    /// Drops the current transcript selection without changing the active turn.
    fn clear_selection(&mut self) -> bool {
        let record = self.active_record_mut();
        let painted = record.selection.is_some()
            || record.selection_text.is_some()
            || record.selection_too_large;

        record.selection = None;
        record.selection_text = None;
        record.selection_too_large = false;
        record.selecting = false;

        painted
    }

    /// The transcript keymap, applied only while the viewport holds focus.
    ///
    /// Returning `None` lets the key continue to the global and composer
    /// handlers; every arm that returns swallows it, which is what stops a
    /// motion from also landing as text in the prompt underneath.
    fn handle_viewport_key(&mut self, key: Key) -> Option<Action> {
        if self.pending_viewport_key.take() == Some('g') {
            return Some(self.handle_g_chord(key));
        }

        match key {
            Key::Char('g') => {
                self.pending_viewport_key = Some('g');
                Some(Action::Render)
            }
            Key::Char('G') => {
                self.scroll_to_end();
                Some(Action::Render)
            }
            Key::Char('j') | Key::Down => {
                self.scroll_down(1);
                Some(Action::Render)
            }
            Key::Char('k') | Key::Up => {
                self.scroll_up(1);
                Some(Action::Render)
            }
            Key::CtrlD => {
                self.scroll_down(self.half_page_rows());
                Some(Action::Render)
            }
            Key::CtrlU => {
                self.scroll_up(self.half_page_rows());
                Some(Action::Render)
            }
            Key::Home => {
                self.scroll_to_start();
                Some(Action::Render)
            }
            Key::End => {
                self.scroll_to_end();
                Some(Action::Render)
            }
            Key::Char('{') => {
                self.jump_to_user_message(UserMessageTarget::Previous);
                Some(Action::Render)
            }
            Key::Char('}') => {
                self.jump_to_user_message(UserMessageTarget::Next);
                Some(Action::Render)
            }
            Key::Char('J') => {
                self.move_block_focus(true);
                self.scroll_to_focused_block();
                Some(Action::Render)
            }
            Key::Char('K') => {
                self.move_block_focus(false);
                self.scroll_to_focused_block();
                Some(Action::Render)
            }
            Key::Char('o') => {
                // Inline expand of the focused tool only — modal is Enter/click.
                self.cycle_focused_block_detail();
                self.scroll_to_focused_block();
                Some(Action::Render)
            }
            // Forking cuts at the block the reader is standing on. With no block
            // focused there is no such point, and forking at a guessed one would
            // cut a turn nobody chose, so the key does nothing instead.
            Key::Char('f') => {
                match self.focused_block_turn_prefix() {
                    Some(turn_prefix) => Some(self.start_fork_request(
                        SessionForkRequest::from_active_transcript(turn_prefix),
                    )),
                    None => Some(Action::Unchanged),
                }
            }
            Key::Enter => {
                let call_id = self.active_record().focused_call.clone();
                if self.open_tool_detail_overlay(call_id.as_deref()) {
                    Some(Action::Render)
                } else {
                    Some(Action::Unchanged)
                }
            }
            Key::Char('[') => {
                self.select_sibling(-1);
                Some(Action::Render)
            }
            Key::Char(']') => {
                self.select_sibling(1);
                Some(Action::Render)
            }
            Key::Char('m') => {
                self.select_transcript(TranscriptId::Main);
                Some(Action::Render)
            }
            Key::Char('?') => {
                self.show_selection_dialog(shortcuts::shortcuts_dialog());
                Some(Action::Render)
            }
            Key::Char('i' | 'a') if !self.active_record_mut().terminal => {
                self.clear_selection();
                self.active_record_mut().focus = TranscriptFocus::Composer;
                self.execution_selection = None;
                Some(Action::Render)
            }
            Key::Char('x') if !self.active_record_mut().terminal => match self.active_transcript {
                TranscriptId::Subagent(id) => Some(Action::CancelExecution(id)),
                TranscriptId::Main => Some(Action::Render),
            },
            // Nothing types while the transcript holds focus. Letting an
            // unclaimed editing key through would put text into a prompt whose
            // cursor is not even being drawn.
            key if key.edits_composer() => Some(Action::Render),
            _ => None,
        }
    }

    fn handle_g_chord(&mut self, key: Key) -> Action {
        match key {
            Key::Char('g') => self.scroll_to_start(),
            Key::Char('t') => self.show_transcript_dialog(),
            _ => {}
        }
        Action::Render
    }

    /// Rows a `Ctrl+D`/`Ctrl+U` step advances.
    fn half_page_rows(&self) -> u16 {
        (self.transcript_page_rows() / 2).max(1)
    }

    fn handle_copy_selection(&mut self) -> Action {
        let record = self
            .transcripts
            .get(&self.active_transcript)
            .expect("active transcript always exists");
        if let Some(text) = record.selection_text.clone() {
            // Copying ends the selection. It is also what keeps the exit
            // reachable: with the selection gone, the next Ctrl+C arms the
            // quit it always did.
            let record = self.active_record_mut();
            record.selection = None;
            record.selection_text = None;
            record.selection_too_large = false;
            return Action::CopySelection(text);
        }
        if record.selection_too_large {
            self.status = Some("Selection exceeds the 64 KiB copy limit.".into());
        }
        Action::Render
    }

    fn handle_control_c(&mut self) -> Action {
        // An open ask-user interaction resolves here, ahead of the copy and
        // quit-arming logic below and ahead of the bridge's own cancellation
        // poll, so Ctrl+C cancels it deterministically on a single press.
        if let Some(state) = self.ask_user.take() {
            return Action::AskUserReply {
                id: state.id(),
                reply: AskUserReply::Cancelled,
            };
        }

        // Mouse capture takes the terminal's own selection away, so the only
        // copy the user has is ours. Many terminals also swallow Ctrl+Shift+C
        // for themselves, which left a made selection with no way to reach the
        // clipboard: with something selected, Ctrl+C copies rather than arms
        // the exit it would otherwise arm.
        if self
            .transcripts
            .get(&self.active_transcript)
            .and_then(|record| record.selection_text.as_ref())
            .is_some()
        {
            return self.handle_copy_selection();
        }

        if matches!(self.scheduler.lifecycle(), TurnLifecycle::Running(_)) {
            if self
                .scheduler
                .reduce(AppEvent::Key(Key::CtrlC, Instant::now()))
                .contains(&Effect::CancelTurn)
            {
                self.engine.cancel();
                self.status = Some("Cancellation requested; waiting for confirmation.".into());
            }
            return Action::Render;
        }
        if matches!(self.scheduler.lifecycle(), TurnLifecycle::Cancelling(_)) {
            return Action::Render;
        }
        if self.quit_is_armed() {
            self.quit_armed_until = None;
            if self.foreground_running() || self.has_active_execution() {
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
                TuiExecutionState::ForegroundRunning
                    | TuiExecutionState::BackgroundRunning
                    | TuiExecutionState::CancellationRequested
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
                    Some(DialogEntryAction::FillComposer { text, attachments }) => {
                        self.dialog = None;
                        self.apply_composer_text(text);
                        self.apply_restored_attachments(attachments)
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
            Key::Delete if !self.dialog_is_searching() && self.is_prompt_stash_overlay() => {
                self.remove_selected_stash_overlay_entry()
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
        if character == 'x' && self.is_prompt_stash_overlay() {
            return self.remove_selected_stash_overlay_entry();
        }
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
        self.rebuild_prompt_overlay_entries();
        self.reset_dialog_selection();
        Action::Render
    }

    fn is_prompt_stash_overlay(&self) -> bool {
        self.dialog
            .as_ref()
            .is_some_and(|dialog| dialog.prompt_overlay == Some(PromptOverlayKind::Stash))
    }

    /// Rebuild history/stash rows from the full store after a filter edit.
    fn rebuild_prompt_overlay_entries(&mut self) {
        let Some(kind) = self
            .dialog
            .as_ref()
            .and_then(|dialog| dialog.prompt_overlay)
        else {
            return;
        };
        let query = self
            .dialog
            .as_ref()
            .map(|dialog| dialog.query.clone())
            .unwrap_or_default();
        let entries = match (kind, self.prompt_memory.as_ref()) {
            (PromptOverlayKind::History, Some(memory)) => {
                history_overlay_entries(memory.as_ref(), &query)
            }
            (PromptOverlayKind::Stash, Some(memory)) => {
                stash_overlay_entries(memory.as_ref(), &query)
            }
            _ => Vec::new(),
        };
        if let Some(dialog) = self.dialog.as_mut() {
            dialog.entries = entries;
            dialog.selected = dialog
                .entries
                .iter()
                .position(|entry| entry.action.is_some())
                .unwrap_or_default();
            dialog.offset = 0;
            dialog.details_open = false;
        }
    }

    /// Remove the selected stash row through the prompt-memory port; keep overlay open.
    fn remove_selected_stash_overlay_entry(&mut self) -> Action {
        let index = self.dialog.as_ref().and_then(|dialog| {
            dialog_matches(dialog)
                .into_iter()
                .find(|(row, _)| *row == dialog.selected)
                .and_then(|(_, entry)| entry.id.as_ref()?.parse::<usize>().ok())
        });
        let Some(index) = index else {
            return Action::Render;
        };

        if let Some(memory) = self.prompt_memory.as_mut() {
            let _ = memory.stash_remove_at(index);
        }
        self.rebuild_prompt_overlay_entries();
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

    fn session_tree_request(&self) -> Option<&SessionTreeRequest> {
        self.dialog
            .as_ref()?
            .tree_entries
            .as_ref()
            .map(|entries| &entries.request)
    }

    /// Opens the lineage browser on a loading page and asks for its content.
    ///
    /// The generation is what lets the answer be matched to the question: a page
    /// that arrives for a request the reader has already replaced is dropped by
    /// [`Self::show_selection_dialog`] rather than painted.
    fn start_session_tree_request(&mut self, mut request: SessionTreeRequest) -> Action {
        let generation = self
            .session_tree_request()
            .map_or(1, |current| current.generation.wrapping_add(1).max(1));
        request.generation = generation;

        self.dialog = Some(DialogView::session_tree_loading(request.clone()));
        Action::LoadSessionTree(request)
    }

    /// Stamps a fork request with the generation that makes it the current one.
    fn start_fork_request(&mut self, mut request: SessionForkRequest) -> Action {
        self.fork_generation = self.fork_generation.wrapping_add(1).max(1);
        request.generation = self.fork_generation;
        Action::ForkSession(request)
    }

    /// Whether `request` is still the fork this terminal is waiting on.
    ///
    /// A fork answered after the reader asked for another one must not be
    /// applied: the composition layer asks this before acting on a result.
    pub const fn is_current_fork(&self, request: &SessionForkRequest) -> bool {
        request.generation == self.fork_generation && self.fork_generation != 0
    }

    /// The fork point the focused transcript block stands on, as a turn count.
    ///
    /// Only the main transcript can be forked: a subagent transcript is not a
    /// session. Returns `None` when no block is focused or the focused block
    /// belongs to no turn this terminal can place, so the key does nothing
    /// rather than forking at a point the reader did not choose.
    fn focused_block_turn_prefix(&self) -> Option<u64> {
        if self.active_transcript != TranscriptId::Main {
            return None;
        }
        let focused = self
            .transcripts
            .get(&TranscriptId::Main)?
            .focused_call
            .as_deref()?;

        self.completed_conversations
            .iter()
            .chain(self.conversation.as_ref())
            .position(|conversation| {
                conversation
                    .tool_batches
                    .iter()
                    .flat_map(|batch| &batch.calls)
                    .any(|call| call.call_id == focused)
            })
            .map(|index| u64::try_from(index.saturating_add(1)).unwrap_or(u64::MAX))
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

    /// Shows or hides the reasoning bodies of the active transcript.
    ///
    /// Pinning survives the auto-collapse a finishing turn performs, so a reader
    /// who asked to see the thought keeps seeing it.
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
        self.bump_selectable_epoch();
    }

    /// Moves the transcript's tool output detail one step along its cycle.
    ///
    /// The level is the whole state this key owns: it advances even when no call
    /// has settled yet, so what the footer names is always what the next result
    /// will be shown at. Every settled call is rewritten to the new level, which
    /// is what makes the cycle legible — three levels, one meaning each, rather
    /// than a per-call state the reader would have to track block by block.
    /// Every settled tool call of the active transcript, in transcript order.
    ///
    /// This is the set AGN-109 collapses, which makes it the set block
    /// navigation has to be able to reach.
    fn settled_call_ids(&self) -> Vec<String> {
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
            .collect()
    }

    /// Moves the focused block, or starts focus at the newest block.
    ///
    /// Focus enters at the end rather than the start because the block a reader
    /// wants is almost always the one that just happened.
    fn move_block_focus(&mut self, forward: bool) {
        let calls = self.settled_call_ids();
        if calls.is_empty() {
            return;
        }

        let record = self.active_record_mut();
        let next = match record
            .focused_call
            .as_ref()
            .and_then(|focused| calls.iter().position(|call| call == focused))
        {
            None => calls.len() - 1,
            Some(index) if forward => (index + 1).min(calls.len() - 1),
            Some(index) => index.saturating_sub(1),
        };
        record.focused_call = Some(calls[next].clone());
        self.bump_selectable_epoch();
    }

    /// Cycles the detail of the focused block alone (inline expand, not modal).
    ///
    /// The transcript-wide cycle answers "how much of everything"; this answers
    /// "what is in this one". The modal is opened by Enter or a click, not by
    /// this path.
    fn cycle_focused_block_detail(&mut self) {
        let record = self.active_record_mut();
        let Some(call_id) = record.focused_call.clone() else {
            return;
        };
        let current = record
            .tool_display_modes
            .get(&call_id)
            .copied()
            .unwrap_or(record.tool_detail);
        let next = current.next();
        record.tool_display_modes.insert(call_id, next);
        self.bump_selectable_epoch();
        self.report_detail_level("block", next);
    }

    /// Advances or walks back the transcript-wide tool output cycle (Ctrl+O).
    ///
    /// Inline only: Collapsed → Truncated → Expanded in the transcript. The
    /// detail modal is not on this key — open it with Enter on a focused tool
    /// or by clicking a tool row.
    fn cycle_tool_detail(&mut self, forward: bool) {
        let completed_call_ids = self.settled_call_ids();
        let record = self.active_record_mut();
        let next = if forward {
            record.tool_detail.next()
        } else {
            record.tool_detail.previous()
        };
        record.tool_detail = next;
        for call_id in completed_call_ids {
            record.tool_display_modes.insert(call_id, next);
        }
        self.bump_selectable_epoch();
        self.report_detail_level("tools", next);
    }

    fn close_tool_detail_overlay(&mut self) {
        let record = self.active_record_mut();
        if record.tool_overlay.take().is_some() {
            self.bump_selectable_epoch();
        }
    }

    /// Opens the scrollable overlay for one tool call (click / Enter, not Ctrl+O).
    ///
    /// `preferred` is the focused or clicked call; falls back to the newest
    /// settled call only when callers pass `None` intentionally.
    fn open_tool_detail_overlay(&mut self, preferred: Option<&str>) -> bool {
        let call_id = match preferred {
            Some(id) => id.to_owned(),
            None => return false,
        };
        let Some(call) = self.find_tool_call(&call_id) else {
            return false;
        };

        let title = {
            let header = widgets::tool_header(&call.parsed, 80);
            header
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };
        let args = widgets::tool_argument_detail_text(&call.parsed, &call.input);
        let (status, output) = match &call.result {
            Some(result) => {
                let status = if result.is_error {
                    "Failure".to_owned()
                } else {
                    "Success".to_owned()
                };
                (status, result.output.clone())
            }
            None => ("Running…".to_owned(), String::new()),
        };

        let record = self.active_record_mut();
        record.focused_call = Some(call_id.clone());
        record.tool_overlay = Some(ToolDetailOverlay {
            call_id,
            title,
            status,
            args,
            output,
            scroll: 0,
        });
        true
    }

    fn find_tool_call(&self, call_id: &str) -> Option<conversation::ToolCall> {
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
            .find(|call| call.call_id == call_id)
            .cloned()
    }

    /// Call id owning the transcript row under the pointer, if any.
    fn tool_call_id_at(&self, column: u16, row: u16) -> Option<String> {
        let layout = self.screen_layout();
        if layout.transcript.height == 0
            || row < layout.transcript.y
            || row >= layout.transcript.bottom()
            || column < layout.transcript.x
            || column >= layout.transcript.right()
        {
            return None;
        }

        let row_width = layout
            .transcript
            .width
            .saturating_sub(TRANSCRIPT_ROW_INDENT)
            .max(1);
        let view = self.view_without_selectable();
        let scroll = usize::from(if view.following_bottom {
            self.detached_scroll_bottom()
        } else {
            view.scroll_offset
        });
        let inside = row
            .checked_sub(layout.transcript.y)
            .and_then(|offset| offset.checked_sub(1))?;
        let target = scroll.saturating_add(usize::from(inside));
        transcript_call_owners(&view, row_width)
            .into_iter()
            .nth(target)
            .flatten()
    }

    fn scroll_tool_overlay(&mut self, delta: i32) -> bool {
        let record = self.active_record_mut();
        let Some(overlay) = record.tool_overlay.as_mut() else {
            return false;
        };
        if delta < 0 {
            overlay.scroll = overlay.scroll.saturating_sub((-delta) as u16);
        } else {
            overlay.scroll = overlay.scroll.saturating_add(delta as u16);
        }
        true
    }

    fn tool_overlay_is_open(&self) -> bool {
        self.active_record().tool_overlay.is_some()
    }

    /// Says where the detail cycle now rests, for as long as that is news.
    ///
    /// The cycle has three positions and no key that names them, so pressing it
    /// blind is guesswork. The footer used to carry the answer permanently,
    /// which spent a slot on an instruction that was only ever interesting in
    /// the instant after the press — so it is announced instead, and the next
    /// key clears it.
    fn report_detail_level(&mut self, subject: &str, mode: widgets::DisplayMode) {
        self.status = Some(format!("{subject} {}", mode.label()));
    }

    /// Absolute wrapped rows every user message of the active transcript starts on.
    fn user_message_offsets(&self) -> Vec<u16> {
        let layout = self.screen_layout();
        let row_width = layout
            .transcript
            .width
            .saturating_sub(TRANSCRIPT_ROW_INDENT)
            .max(1);
        let lines = rendered_transcript(&self.view(), row_width);

        let mut offsets = Vec::new();
        let mut row = 0usize;
        for line in &lines {
            if line
                .spans
                .get(1)
                .is_some_and(|span| span.content.starts_with('❯'))
            {
                offsets.push(saturating_u16(row));
            }
            row += line.width().div_ceil(usize::from(row_width)).max(1);
        }

        offsets
    }

    fn jump_to_user_message(&mut self, target: UserMessageTarget) {
        let user_offsets = self.user_message_offsets();
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

        let target = match target {
            UserMessageTarget::Previous => user_offsets
                .iter()
                .rev()
                .find(|offset| **offset < current)
                .copied()
                .or_else(|| user_offsets.first().copied()),
            UserMessageTarget::Next => user_offsets
                .iter()
                .find(|offset| **offset > current)
                .copied()
                .or_else(|| user_offsets.last().copied()),
            UserMessageTarget::Last => user_offsets.last().copied(),
        };

        if let Some(offset) = target {
            let bottom = self.detached_scroll_bottom();
            let record = self.active_record_mut();
            record.following_bottom = false;
            record.scroll_offset = offset.min(bottom);
        }
    }

    /// Moves block focus to whatever the pointer is resting on.
    ///
    /// Hover is an accelerator over the keyboard path, never a second one: it
    /// sets the same focus `J`/`K` set and opens nothing by itself. Opening the
    /// modal still costs a deliberate click or Enter.
    fn hover_block(&mut self, column: u16, row: u16) -> Action {
        let layout = self.screen_layout();
        if layout.transcript.height == 0
            || row < layout.transcript.y
            || row >= layout.transcript.bottom()
            || column < layout.transcript.x
            || column >= layout.transcript.right()
        {
            return Action::Unchanged;
        }
        // The transcript block draws a top border, so its first content row sits
        // one below the rect. A pointer on the border itself owns nothing.
        if row == layout.transcript.y {
            return Action::Unchanged;
        }

        let hovered = self.tool_call_id_at(column, row);
        let record = self.active_record_mut();
        if record.focused_call == hovered {
            return Action::Unchanged;
        }

        record.focused_call = hovered;
        self.bump_selectable_epoch();
        Action::Render
    }

    /// Brings the focused block into view and stops the viewport chasing the
    /// bottom.
    ///
    /// Detaching is what keeps the detail from displacing the transcript: rows
    /// opening below a header never move the header, but a viewport still stuck
    /// to the bottom would slide everything up under the reader instead.
    fn scroll_to_focused_block(&mut self) {
        let layout = self.screen_layout();
        let row_width = layout
            .transcript
            .width
            .saturating_sub(TRANSCRIPT_ROW_INDENT)
            .max(1);
        let lines = rendered_transcript(&self.view(), row_width);

        let mut row = 0usize;
        let mut target = None;
        for line in &lines {
            if target.is_none()
                && line
                    .spans
                    .iter()
                    .any(|span| span.style.fg == Some(widgets::RolePalette::navigation()))
            {
                target = Some(saturating_u16(row));
            }
            row += line.width().div_ceil(usize::from(row_width)).max(1);
        }

        let Some(target) = target else {
            return;
        };
        let bottom = self.detached_scroll_bottom();
        let visible = usize::from(
            layout
                .transcript
                .height
                .saturating_sub(transcript_chrome_rows(false)),
        );
        let record = self.active_record_mut();
        record.following_bottom = false;
        if target < record.scroll_offset {
            record.scroll_offset = target.min(bottom);
        } else if usize::from(target) >= usize::from(record.scroll_offset) + visible {
            record.scroll_offset = saturating_u16(
                usize::from(target)
                    .saturating_sub(visible.saturating_sub(1))
                    .min(usize::from(bottom)),
            );
        }
    }

    fn project_conversation(&mut self, event: ConversationEvent) {
        self.bump_selectable_epoch();
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

    /// Guarantees a failed turn leaves a transcript entry even when nothing
    /// explains it.
    ///
    /// The terminal state signal and the outcome carrying the cause travel on
    /// separate channels, so either can arrive first and the outcome can be
    /// lost entirely. This records a placeholder when the turn has explained
    /// nothing yet; [`Self::add_error`] later replaces it with the real cause
    /// rather than appending a second entry for the same failure.
    fn note_turn_failure(&mut self) {
        let explained = self
            .conversation
            .as_ref()
            .is_some_and(|conversation| !conversation.errors.is_empty());
        if explained || self.placeholder_failure {
            return;
        }

        self.add_error(
            UNEXPLAINED_FAILURE_MESSAGE.to_owned(),
            "Open /diagnostics for the runtime event, then retry.".to_owned(),
        );
        self.placeholder_failure = true;
    }

    fn drop_placeholder_failure(&mut self) {
        if !std::mem::take(&mut self.placeholder_failure) {
            return;
        }

        if let Some(index) = self.transcript.iter().rposition(|entry| {
            matches!(entry, TranscriptEntry::Error(text) if text == UNEXPLAINED_FAILURE_MESSAGE)
        }) {
            self.transcript.remove(index);
        }
        if let Some(conversation) = self.conversation.as_mut() {
            conversation.remove_error(UNEXPLAINED_FAILURE_MESSAGE);
        }
    }

    fn add_error(&mut self, message: String, action: String) {
        self.drop_placeholder_failure();
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
/// Opening marker of a prompt the runtime wrote for itself.
pub const RUNTIME_SCHEDULED_PROMPT_MARKER: &str = "[coordination source=runtime";

/// Whether `prompt` was scheduled by the runtime rather than typed by the user.
///
/// A failed turn's prompt is offered back for retry, which is only ever right
/// for something the user wrote. Handing back a coordination prompt puts text
/// nobody typed in the composer and invites retrying a turn the runtime owns.
pub fn is_runtime_scheduled_prompt(prompt: &str) -> bool {
    prompt
        .trim_start()
        .starts_with(RUNTIME_SCHEDULED_PROMPT_MARKER)
}

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
    auto_turn_notice_for(&auto_turn_subject(finished))
}

fn auto_turn_notice_for(subject: &str) -> String {
    format!("Continuing automatically: {subject} finished.")
}

/// The notice a runtime-scheduled prompt should be shown as, if it is one.
///
/// A scheduled turn opens with no user prompt at all: the live path records a
/// notice and leaves the prompt empty. The coordination text exists only
/// because the provider has to be told, and the provider is told in a user-role
/// message — which is what the session store keeps. Replaying that verbatim on
/// resume attributed to the reader a prompt whose own words say the user did
/// not send it.
///
/// The subject is read back out of the prompt so the restored notice says the
/// same thing the live one did. It sits next to the generator on purpose: the
/// two formats have to move together.
pub fn runtime_scheduled_notice(prompt: &str) -> Option<String> {
    if !is_runtime_scheduled_prompt(prompt) {
        return None;
    }

    let subject = prompt
        .split_once('\n')
        .and_then(|(_, body)| body.split_once(" finished."))
        .map(|(subject, _)| subject.trim())
        .filter(|subject| !subject.is_empty());

    Some(subject.map_or_else(
        || "Continued automatically after background work finished.".to_owned(),
        auto_turn_notice_for,
    ))
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

fn previous_grapheme_boundary(input: &str, cursor: usize) -> usize {
    let mut boundary = 0;

    for grapheme in input.graphemes(true) {
        let end = boundary + grapheme.chars().count();
        if end >= cursor {
            return boundary;
        }
        boundary = end;
    }

    boundary
}

fn next_grapheme_boundary(input: &str, cursor: usize) -> usize {
    let mut boundary = 0;

    for grapheme in input.graphemes(true) {
        boundary += grapheme.chars().count();
        if cursor < boundary {
            return boundary;
        }
    }

    boundary
}

fn grapheme_boundary_at_or_before(input: &str, cursor: usize) -> usize {
    let mut boundary = 0;

    for grapheme in input.graphemes(true) {
        let end = boundary + grapheme.chars().count();
        if end > cursor {
            return boundary;
        }
        boundary = end;
    }

    boundary
}

fn grapheme_boundary_at_or_after(input: &str, cursor: usize) -> usize {
    let mut boundary = 0;

    for grapheme in input.graphemes(true) {
        boundary += grapheme.chars().count();
        if cursor <= boundary {
            return boundary;
        }
    }

    boundary
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
            Action::AttachClipboardImage
            | Action::SyncStagedMedia(_)
            | Action::Unchanged
            | Action::Render
            | Action::Submit(_)
            | Action::SubmitBusy(_)
            | Action::SubmitSecret { .. }
            | Action::SubmitBackground(_)
            | Action::TransitionToBackground(_)
            | Action::CancelExecution(_)
            | Action::CancelAllExecutions
            | Action::SendTaskMessage { .. }
            | Action::OpenDialog(_)
            | Action::LoadSessionPage(_)
            | Action::LoadSessionTree(_)
            | Action::ForkSession(_)
            | Action::DialogAction(_)
            | Action::SafeDialogAction(_)
            | Action::OpenDeviceAuthUrl
            | Action::CopyDeviceAuthUrl
            | Action::CopyDeviceAuthCode
            | Action::Cancel
            | Action::CancelRoute
            | Action::AskUserReply { .. } => renderer.render(tui.view())?,
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
            Action::SubmitBusy(_) => {
                tui.enqueue_composer();
                renderer.render(tui.view())?;
            }
            Action::AttachClipboardImage
            | Action::SyncStagedMedia(_)
            | Action::Unchanged
            | Action::Render
            | Action::SubmitSecret { .. }
            | Action::SubmitBackground(_)
            | Action::TransitionToBackground(_)
            | Action::CancelExecution(_)
            | Action::CancelAllExecutions
            | Action::SendTaskMessage { .. }
            | Action::OpenDialog(_)
            | Action::LoadSessionPage(_)
            | Action::LoadSessionTree(_)
            | Action::ForkSession(_)
            | Action::DialogAction(_)
            | Action::SafeDialogAction(_)
            | Action::OpenDeviceAuthUrl
            | Action::CopyDeviceAuthUrl
            | Action::CopyDeviceAuthCode
            | Action::Cancel
            | Action::CancelRoute
            | Action::AskUserReply { .. } => renderer.render(tui.view())?,
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

struct AskUserBridgeTeardown(Option<TuiAskUserBridge>);
impl Drop for AskUserBridgeTeardown {
    fn drop(&mut self) {
        let _ = self.0.as_ref().is_some_and(TuiAskUserBridge::close);
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct FrameSchedule {
    last_render: Option<Duration>,
}

impl FrameSchedule {
    fn heartbeat_due(self, now: Duration, live: bool) -> bool {
        live && self
            .last_render
            .is_none_or(|last| now.saturating_sub(last) >= ACTIVE_FRAME_HEARTBEAT)
    }

    fn mark_rendered(&mut self, now: Duration) {
        self.last_render = Some(now);
    }

    fn poll_timeout(self, now: Duration, live: bool, backlog: bool) -> Duration {
        if backlog {
            return Duration::ZERO;
        }
        if !live {
            return TERMINAL_POLL_INTERVAL;
        }

        let heartbeat_wait = self.last_render.map_or(Duration::ZERO, |last| {
            ACTIVE_FRAME_HEARTBEAT.saturating_sub(now.saturating_sub(last))
        });
        TERMINAL_POLL_INTERVAL.min(heartbeat_wait)
    }
}

/// Drives one pass of the render-skip gate.
///
/// A live turn or subagent forces a full frame: spinner and elapsed counters
/// have to move. The settled-turn cache already avoids rebuilding frozen
/// conversations, but the frame still walks the window and runs the buffer
/// post-passes (selection paint, OSC 8, colour quantize). Those passes have
/// no region they can legally skip, so a spinner tick cannot be a one-glyph
/// update without a second renderer. That cost is structural.
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
    let _perf_gate = agens_perf::span!(
        "tui.frame.gate",
        rendered = agens_perf::Pending,
        live_work = agens_perf::Pending,
    );

    let execution_count = tui.executions.len();
    let quit_armed = tui.quit_is_armed();
    let restored_syntax_ready = tui.highlight_restored_syntax;
    let status = tui.status.clone();
    tui.tick(now);
    let expired_execution = tui.executions.len() != execution_count;
    let expired_quit_warning = quit_armed && !tui.quit_is_armed();
    let restored_syntax_became_ready = !restored_syntax_ready && tui.highlight_restored_syntax;
    let status_changed = status != tui.status;
    let live_work = tui.has_live_work();
    agens_perf::field!(live_work = live_work);
    if !dirty
        && !expired_execution
        && !expired_quit_warning
        && !restored_syntax_became_ready
        && !status_changed
        && !schedule.heartbeat_due(now, live_work)
    {
        agens_perf::field!(rendered = false);
        return Ok(false);
    }

    agens_perf::field!(rendered = true);
    let view = tui.view();
    let _perf_frame = agens_perf::span!(
        "tui.frame",
        width = view.size.0,
        height = view.size.1,
        scroll_offset = view.scroll_offset,
    );
    renderer.render(view)?;
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

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ProviderDrain {
    dirty: bool,
    backlog: bool,
    next_prompt: Option<ScheduledPrompt>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ScheduledPrompt {
    display: String,
    prompt: String,
}

fn next_scheduled_prompt(effects: Vec<Effect>) -> Option<ScheduledPrompt> {
    effects.into_iter().find_map(|effect| match effect {
        Effect::StartPrompt(prompt) => Some(ScheduledPrompt {
            display: prompt.clone(),
            prompt,
        }),
        Effect::StartQueuedPrompt { display, prompt } => Some(ScheduledPrompt { display, prompt }),
        _ => None,
    })
}

fn drain_provider_channels<E: Engine>(
    tui: &mut Tui<E>,
    metrics_receiver: &mpsc::Receiver<UiEnvelope<TuiRuntimeEvent>>,
    progress_receiver: &mpsc::Receiver<TurnEvent>,
    completion_receiver: &mpsc::Receiver<(Option<u64>, TuiProviderOutcome)>,
) -> ProviderDrain {
    let _perf_drain = agens_perf::span!(
        "tui.drain",
        progress = agens_perf::Pending,
        metrics = agens_perf::Pending,
        backlog = agens_perf::Pending,
    );

    let progress = drain_channel(progress_receiver, |event| tui.apply_progress(event));
    let metrics = if progress.caught_up {
        drain_channel(metrics_receiver, |envelope| {
            let (ordinal, event) = envelope.into_parts();
            tui.apply_runtime_event_with_ordinal(ordinal, event);
        })
    } else {
        ChannelDrain::default()
    };
    let mut next_prompt = None;
    let completion = if metrics.caught_up && progress.caught_up {
        drain_channel(completion_receiver, |(generation, outcome)| {
            // Only adopt a newly scheduled prompt. Later terminals in the same
            // drain (stale foreground generations or detached background outcomes)
            // must not clear a queue handoff already taken from an earlier terminal.
            match generation {
                Some(generation) => {
                    if let Some(next) =
                        tui.finish_provider_turn_scheduled_for_generation(generation, outcome)
                        && next_prompt.is_none()
                    {
                        next_prompt = Some(next);
                    }
                }
                None => tui.finish_detached_provider_turn(outcome),
            }
        })
    } else {
        ChannelDrain::default()
    };

    let result = ProviderDrain {
        dirty: progress.dirty() || metrics.dirty() || completion.dirty(),
        backlog: progress.backlog()
            || metrics.backlog()
            || completion.backlog()
            || !(metrics.caught_up && progress.caught_up),
        next_prompt,
    };
    agens_perf::field!(progress = progress.processed as u64);
    agens_perf::field!(metrics = metrics.processed as u64);
    agens_perf::field!(backlog = result.backlog);
    result
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
    run_with_default_progress_submit_with_permissions_task_controls_and_ask_user(
        tui,
        route,
        submit,
        transition,
        cancel_execution,
        Vec::new,
        send_task_message,
        permissions,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn run_with_default_progress_submit_with_permissions_task_controls_and_ask_user<
    E,
    R,
    F,
    B,
    C,
    A,
    M,
>(
    tui: &mut Tui<E>,
    route: R,
    submit: F,
    transition: B,
    cancel_execution: C,
    cancel_all_executions: A,
    send_task_message: M,
    permissions: Option<(TuiPermissionBridge, mpsc::Receiver<TuiPermissionRequest>)>,
    ask_user: Option<(TuiAskUserBridge, mpsc::Receiver<TuiAskUserRequest>)>,
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
    A: Fn() -> Vec<u64> + Send + Sync + 'static,
    M: Fn(u64, String) -> bool + Send + Sync + 'static,
{
    tui.enable_busy_policy_routing();
    let route = Arc::new(route);
    let submit = Arc::new(submit);
    let transition = Arc::new(transition);
    let cancel_execution = Arc::new(cancel_execution);
    let cancel_all_executions = Arc::new(cancel_all_executions);
    let send_task_message = Arc::new(send_task_message);
    let (sender, receiver) = mpsc::channel();
    let (completion_sender, completion_receiver) = mpsc::channel();
    let (route_sender, route_receiver) = mpsc::channel();
    let (route_progress_sender, route_progress_receiver) = mpsc::channel();
    let (metrics_sender, metrics_receiver) = BridgeTx::bounded(128);
    let (permission_bridge, permission_requests) = permissions.unzip();
    let _permission_teardown = PermissionBridgeTeardown(permission_bridge.clone());
    let mut active_permission = None;
    let (ask_user_bridge, ask_user_requests) = ask_user.unzip();
    let _ask_user_teardown = AskUserBridgeTeardown(ask_user_bridge.clone());
    let mut active_ask_user: Option<u64> = None;
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
        if let Some(next) = provider.next_prompt {
            tui.begin_submission(next.display);
            let generation = tui.active_generation();
            let submit = Arc::clone(&submit);
            let sender = sender.clone();
            let metrics = metrics_sender.clone();
            let completion_sender = completion_sender.clone();
            thread::spawn(move || {
                let outcome = submit(next.prompt, SubmitOrigin::User, sender, metrics);
                let _ = completion_sender.send((generation, outcome));
            });
            dirty = true;
        }
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
                let generation = tui.active_generation();
                let submit = Arc::clone(&submit);
                let sender = sender.clone();
                let metrics = metrics_sender.clone();
                let completion_sender = completion_sender.clone();
                thread::spawn(move || {
                    let outcome = submit(prompt, SubmitOrigin::User, sender, metrics);
                    let _ = completion_sender.send((generation, outcome));
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
                    prompt_title(permission_dialog_title(tool), request.origin()),
                    Some(permission_dialog_body(
                        target,
                        request.access(),
                        request.reason(),
                    )),
                    entries,
                )
                .as_confirm(),
            );
            dirty = true;
        }
        if dismiss_resolved_ask_user(tui, active_ask_user, ask_user_bridge.as_ref()) {
            active_ask_user = None;
            dirty = true;
        }
        if let Some(id) = drain_ask_user_request(
            tui,
            active_ask_user,
            ask_user_bridge.as_ref(),
            ask_user_requests.as_ref(),
        ) {
            active_ask_user = Some(id);
            dirty = true;
        }
        if active_route.is_none()
            && let Some(prompt) = tui.take_ready_auto_turn()
        {
            let generation = tui.active_generation();
            let submit = Arc::clone(&submit);
            let sender = sender.clone();
            let metrics = metrics_sender.clone();
            let completion_sender = completion_sender.clone();
            thread::spawn(move || {
                let outcome = submit(prompt, SubmitOrigin::SubagentCompletion, sender, metrics);
                let _ = completion_sender.send((generation, outcome));
            });
            dirty = true;
        }
        render_progress_frame(tui, &mut renderer, &mut frame_schedule, now, dirty)?;
        let timeout = frame_schedule.poll_timeout(now, tui.has_live_work(), backlog);
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
        let changed_something = !matches!(action, Action::Unchanged);
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
            Action::AttachClipboardImage => {
                if let Some((bytes, mime)) = read_os_clipboard_image() {
                    let outcome = route(
                        TuiRouteRequest::AttachClipboardImage { bytes, mime },
                        route_progress_sender.clone(),
                        TuiRouteCancellation::new(),
                    );
                    let _ = tui.apply_submission_outcome(outcome);
                }
            }
            Action::SyncStagedMedia(attachments) => {
                let outcome = route(
                    TuiRouteRequest::ReplaceStagedMedia { attachments },
                    route_progress_sender.clone(),
                    TuiRouteCancellation::new(),
                );
                let _ = tui.apply_submission_outcome(outcome);
            }
            Action::SubmitBusy(input) => {
                let outcome = route(
                    TuiRouteRequest::BusyInput(input),
                    route_progress_sender.clone(),
                    TuiRouteCancellation::new(),
                );
                let quit = matches!(outcome, TuiSubmissionOutcome::Quit);
                let _ = tui.apply_busy_submission_outcome(outcome);
                if quit {
                    return Ok(());
                }
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
                    let _ = completion_sender.send((None, outcome));
                });
            }
            Action::TransitionToBackground(id) => {
                let _ = transition(id);
            }
            Action::CancelExecution(id) => {
                if cancel_execution(id) {
                    tui.apply_confirmed_cancellations([id]);
                }
            }
            Action::CancelAllExecutions => {
                tui.apply_confirmed_cancellations(cancel_all_executions());
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
            // The lineage load is one keystroke, not a typed query, so it goes
            // out without the search debounce the session pages need.
            Action::LoadSessionTree(request) => {
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
                    let outcome = if cancellation.is_cancelled() {
                        TuiSubmissionOutcome::RouteCancelled
                    } else {
                        route(
                            TuiRouteRequest::SessionTree(request),
                            progress,
                            cancellation,
                        )
                    };
                    let _ = route_sender.send((active_id, outcome));
                });
            }
            // A fork ends attached to the fork, so it is a session load: the
            // transcript is about to be replaced and the keymap has to say so.
            Action::ForkSession(request) => {
                if !tui.begin_session_load() {
                    continue;
                }
                if let Some((_, cancellation, _)) = active_route.take() {
                    cancellation.cancel();
                }
                next_route_id = next_route_id.wrapping_add(1).max(1);
                let active_id = next_route_id;
                let cancellation = TuiRouteCancellation::new();
                active_route = Some((active_id, cancellation.clone(), true));
                let route = Arc::clone(&route);
                let route_sender = route_sender.clone();
                let progress = route_progress_sender.clone();
                thread::spawn(move || {
                    let outcome = route(
                        TuiRouteRequest::ForkSession(request),
                        progress,
                        cancellation,
                    );
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
            Action::Unchanged => {}
            Action::AskUserReply { id, reply } => {
                resolve_ask_user_reply(ask_user_bridge.as_ref(), id, reply);
                active_ask_user = None;
                render_requested = true;
                continue;
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
        if changed_something {
            render_requested = true;
        }
    }
}

/// The event loop's ask-user drain arm: opens a newly parked request as the
/// active overlay, if none is already open and the request the drain
/// received is still genuinely pending. Factored out of the loop so it can
/// be exercised directly by a test without a live terminal.
/// Puts the asking subagent in the prompt's own title.
///
/// The title is where it has to go rather than the body: with several
/// subagents running, "allow bash?" is a question you cannot answer
/// responsibly until you know whose it is, so the answer has to be visible
/// before the reader starts reading the target.
fn prompt_title(base: &str, origin: Option<&PromptOrigin>) -> String {
    match origin {
        Some(origin) => format!("{base} · {} #{}", origin.agent, origin.execution),
        None => base.to_owned(),
    }
}

/// Title for a permission confirm, keyed off the bare tool name.
fn permission_dialog_title(tool: &str) -> &'static str {
    match tool {
        "bash" => "Bash command",
        "write" | "edit" => "Write file",
        "read" | "list" | "search" | "glob" | "grep" | "git_read" => "Read access",
        "webfetch" => "Network access",
        _ => "Permission required",
    }
}

/// Multi-line body for a permission confirm: full target, access, optional reason.
///
/// Newlines are preserved so the target can wrap without being collapsed into
/// a single ellipsized caption.
fn permission_dialog_body(target: &str, access: &str, reason: Option<&str>) -> String {
    let mut body = target.to_owned();
    if !access.is_empty() {
        body.push('\n');
        body.push_str("Access: ");
        body.push_str(access);
    }
    if let Some(reason) = reason.filter(|reason| !reason.is_empty()) {
        body.push('\n');
        body.push_str(reason);
    }
    body
}

fn drain_ask_user_request<E: Engine>(
    tui: &mut Tui<E>,
    active_ask_user: Option<u64>,
    ask_user_bridge: Option<&TuiAskUserBridge>,
    ask_user_requests: Option<&mpsc::Receiver<TuiAskUserRequest>>,
) -> Option<u64> {
    if active_ask_user.is_some() {
        return None;
    }
    let request = ask_user_requests?.try_recv().ok()?;
    if !ask_user_bridge?.is_pending(request.id()) {
        return None;
    }
    let id = request.id();
    tui.open_ask_user_from(id, request.request().clone(), request.origin().cloned());
    Some(id)
}

/// The event loop's ask-user teardown arm: dismisses the open overlay when
/// the bridge resolved the active request from its own side — deadline
/// expiry, external cancellation, or a closed surface — none of which
/// travel through `Action::AskUserReply`. Factored out for the same reason
/// as the drain arm above.
fn dismiss_resolved_ask_user<E: Engine>(
    tui: &mut Tui<E>,
    active_ask_user: Option<u64>,
    ask_user_bridge: Option<&TuiAskUserBridge>,
) -> bool {
    let Some(id) = active_ask_user else {
        return false;
    };
    if ask_user_bridge.is_none_or(|bridge| bridge.is_pending(id)) {
        return false;
    }
    tui.dismiss_ask_user(id)
}

/// The event loop's ask-user reply arm: forwards a UI-driven resolution to
/// the bridge. Factored out for the same reason as the drain and teardown
/// arms above.
fn resolve_ask_user_reply(
    ask_user_bridge: Option<&TuiAskUserBridge>,
    id: u64,
    reply: AskUserReply,
) {
    if let Some(bridge) = ask_user_bridge {
        let _ = bridge.reply(id, reply);
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
        TuiRouteRequest::BusyInput(_) => false,
        TuiRouteRequest::DialogAction(action_id) => is_session_resume_action(action_id),
        TuiRouteRequest::DeviceAuthOpenUrl(_)
        | TuiRouteRequest::AttachClipboardImage { .. }
        | TuiRouteRequest::ReplaceStagedMedia { .. }
        | TuiRouteRequest::SubmitSecret { .. }
        | TuiRouteRequest::OpenDialog(_)
        | TuiRouteRequest::SessionPage(_)
        // A fork already begins its own session load where it is dispatched, so
        // it must not be classified into a second one here.
        | TuiRouteRequest::ForkSession(_)
        | TuiRouteRequest::SessionTree(_) => false,
    }
}

fn is_session_browser_request(request: &TuiRouteRequest) -> bool {
    match request {
        TuiRouteRequest::Input(input) => matches!(input.trim(), "/resume" | "/sessions"),
        TuiRouteRequest::BusyInput(_) => false,
        TuiRouteRequest::DeviceAuthOpenUrl(_)
        | TuiRouteRequest::AttachClipboardImage { .. }
        | TuiRouteRequest::ReplaceStagedMedia { .. }
        | TuiRouteRequest::SubmitSecret { .. } => false,
        TuiRouteRequest::OpenDialog(route_id) => route_id == "sessions",
        TuiRouteRequest::SessionPage(_) => true,
        // The lineage browser installs its own loading page, not this one.
        TuiRouteRequest::SessionTree(_)
        | TuiRouteRequest::ForkSession(_)
        | TuiRouteRequest::DialogAction(_) => false,
    }
}

/// Reads an image from the OS clipboard when available (arboard → PNG).
///
/// Text clipboard pastes continue to use bracketed paste (`Event::Paste`).
fn read_os_clipboard_image() -> Option<(Vec<u8>, Option<String>)> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    let image = clipboard.get_image().ok()?;
    let width = u32::try_from(image.width).ok()?;
    let height = u32::try_from(image.height).ok()?;
    if width == 0 || height == 0 {
        return None;
    }
    let buffer = image::RgbaImage::from_raw(width, height, image.bytes.into_owned())?;
    let mut png = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut png);
    use image::ImageEncoder;
    encoder
        .write_image(
            buffer.as_raw(),
            width,
            height,
            image::ExtendedColorType::Rgba8,
        )
        .ok()?;
    Some((png, Some("image/png".into())))
}

fn sync_terminal_size<E: Engine>(tui: &mut Tui<E>) -> io::Result<()> {
    let (width, height) = crossterm_terminal::size()?;
    tui.handle(Event::Resize { width, height });
    Ok(())
}

fn map_event(event: CrosstermEvent) -> Option<Event> {
    match event {
        CrosstermEvent::Resize(width, height) => Some(Event::Resize { width, height }),
        CrosstermEvent::Key(key)
            if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
        {
            map_key(key)
        }
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
        CrosstermEvent::Mouse(mouse) if mouse.kind == MouseEventKind::Moved => {
            Some(Event::MouseMove {
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
    // A release never acts. A repeat acts only for the keys holding down is
    // meant to repeat; see [`Key::repeats_while_held`].
    if !matches!(event.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }

    let key = match (event.code, event.modifiers) {
        (KeyCode::Char('c'), modifiers)
            if modifiers == KeyModifiers::CONTROL | KeyModifiers::SHIFT =>
        {
            Key::CtrlShiftC
        }
        (KeyCode::Char('C'), modifiers) if modifiers == KeyModifiers::CONTROL => Key::CtrlShiftC,
        (KeyCode::Char('c' | 'C'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            Key::CtrlC
        }
        (KeyCode::Char('o'), modifiers)
            if modifiers == KeyModifiers::CONTROL | KeyModifiers::SHIFT =>
        {
            Key::CtrlShiftO
        }
        (KeyCode::Char('O'), modifiers) if modifiers == KeyModifiers::CONTROL => Key::CtrlShiftO,
        (KeyCode::Char('o' | 'O'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            Key::CtrlO
        }
        (KeyCode::Char('t' | 'T'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            Key::CtrlT
        }
        (KeyCode::Char('y' | 'Y'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            Key::CtrlY
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
        (KeyCode::Char('s' | 'S'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            Key::CtrlS
        }
        (KeyCode::Char('r' | 'R'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            Key::CtrlR
        }
        (KeyCode::Char('v' | 'V'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            Key::CtrlV
        }
        (KeyCode::Char('w' | 'W'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            Key::DeletePreviousWord
        }
        // `?` is already Shift+`/`, so a terminal reports Ctrl+? with or without
        // the shift bit depending on how it resolves the layout. Both are the
        // same press to the reader.
        (KeyCode::Char('?'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            Key::CtrlQuestion
        }
        (KeyCode::Char('u' | 'U'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            Key::CtrlU
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
            Key::CtrlD
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
        (KeyCode::Backspace, modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            Key::DeletePreviousWord
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
        (KeyCode::Delete, modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            Key::DeleteNextWord
        }
        (KeyCode::Delete, _) => Key::Delete,
        (KeyCode::Home, _) => Key::Home,
        (KeyCode::End, _) => Key::End,
        (KeyCode::PageUp, _) => Key::PageUp,
        (KeyCode::PageDown, _) => Key::PageDown,
        (KeyCode::Up, KeyModifiers::ALT) => Key::AltUp,
        (KeyCode::Down, KeyModifiers::ALT) => Key::AltDown,
        (KeyCode::Up, _) => Key::Up,
        (KeyCode::Down, _) => Key::Down,
        (KeyCode::Tab, _) => Key::Tab,
        (KeyCode::Esc, _) => Key::Escape,
        _ => return None,
    };

    if event.kind == KeyEventKind::Repeat && !key.repeats_while_held() {
        return None;
    }

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

    #[test]
    fn a_supplied_composer_draft_is_ready_for_an_ordinary_submit() {
        let mut tui = Tui::new(NoopEngine);

        tui.set_composer_draft("coordinate this");

        assert_eq!(tui.input(), "coordinate this");
        assert!(!tui.view().recovered_failed_prompt);
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
        assert_eq!(
            tui.finish_provider_turn(TuiProviderOutcome::Completed("done".into())),
            None
        );
        let bottom = tui.following_scroll_bottom();
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
    fn control_c_requires_a_second_press_and_the_warning_expires() {
        let mut tui = Tui::new(NoopEngine);
        let mut renderer = RecordingRenderer::default();
        let mut schedule = FrameSchedule::default();

        assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Render);
        assert!(tui.view().quit_armed);
        assert!(
            render_progress_frame(&mut tui, &mut renderer, &mut schedule, Duration::ZERO, true)
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
        assert!(!tui.view().quit_armed);

        assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Render);
        assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Quit);
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
            staged_media: Vec::new(),
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
                .send((
                    Some(1),
                    TuiProviderOutcome::Completed("delta1delta2".into()),
                ))
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
            .send((Some(1), TuiProviderOutcome::Completed(deltas.concat())))
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
    fn maps_control_shift_c_to_copy_and_plain_control_c_to_exit() {
        for (code, modifiers) in [
            (
                KeyCode::Char('c'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            (KeyCode::Char('C'), KeyModifiers::CONTROL),
        ] {
            assert_eq!(
                map_key(KeyEvent::new(code, modifiers)),
                Some(Event::Key(Key::CtrlShiftC))
            );
        }
        assert_eq!(
            map_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(Event::Key(Key::CtrlC))
        );
    }

    #[test]
    fn maps_control_r_to_the_session_tree_in_both_reported_cases() {
        for code in [KeyCode::Char('r'), KeyCode::Char('R')] {
            assert_eq!(
                map_key(KeyEvent::new(code, KeyModifiers::CONTROL)),
                Some(Event::Key(Key::CtrlR))
            );
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
            Some(Event::Key(Key::CtrlD))
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
    fn maps_ctrl_s_to_prompt_stash_key() {
        for code in [KeyCode::Char('s'), KeyCode::Char('S')] {
            assert_eq!(
                map_key(KeyEvent::new(code, KeyModifiers::CONTROL)),
                Some(Event::Key(Key::CtrlS))
            );
        }

        assert_eq!(
            map_key(KeyEvent::new_with_kind(
                KeyCode::Char('s'),
                KeyModifiers::CONTROL,
                KeyEventKind::Repeat,
            )),
            None,
            "Ctrl+S is a command and must not auto-repeat"
        );
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
    fn left_drag_selects_exact_unicode_text_and_control_shift_c_copies() {
        let mut tui = Tui::new(NoopEngine);
        tui.handle(Event::Resize {
            width: 80,
            height: 12,
        });
        tui.active_record_mut()
            .transcript
            .push(TranscriptEntry::Info("alpha café 🙂 omega".into()));
        tui.bump_selectable_epoch();

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
            tui.handle(Event::Key(Key::CtrlShiftC)),
            Action::CopySelection("café 🙂".into())
        );
        assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Render);
        assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Quit);
    }

    #[test]
    fn mouse_move_extends_selection_while_selecting() {
        let mut tui = Tui::new(NoopEngine);
        tui.handle(Event::Resize {
            width: 80,
            height: 12,
        });
        tui.active_record_mut()
            .transcript
            .push(TranscriptEntry::Info("alpha café 🙂 omega".into()));
        tui.bump_selectable_epoch();

        assert_eq!(
            tui.handle(Event::MouseDown { column: 24, row: 1 }),
            Action::Render
        );
        // Many terminals emit Move (not Drag) while the button is held.
        assert_eq!(
            tui.handle(Event::MouseMove { column: 30, row: 1 }),
            Action::Render
        );
        assert_eq!(tui.selected_text(), None);
        assert_eq!(
            tui.handle(Event::MouseUp { column: 30, row: 1 }),
            Action::Render
        );
        assert_eq!(tui.selected_text(), Some("café 🙂"));
    }

    #[test]
    fn mouse_drag_to_the_same_cell_is_a_no_op_render() {
        let mut tui = Tui::new(NoopEngine);
        tui.handle(Event::Resize {
            width: 80,
            height: 12,
        });
        tui.active_record_mut()
            .transcript
            .push(TranscriptEntry::Info("alpha café 🙂 omega".into()));
        tui.bump_selectable_epoch();

        assert_eq!(
            tui.handle(Event::MouseDown { column: 24, row: 1 }),
            Action::Render
        );
        assert_eq!(
            tui.handle(Event::MouseDrag { column: 30, row: 1 }),
            Action::Render
        );
        assert_eq!(
            tui.handle(Event::MouseDrag { column: 30, row: 1 }),
            Action::Unchanged
        );
        assert_eq!(
            tui.handle(Event::MouseMove { column: 30, row: 1 }),
            Action::Unchanged
        );
    }

    #[test]
    fn selectable_transcript_marks_leading_chrome_non_copyable() {
        let accent = " ".repeat(widgets::ACCENT_WIDTH);
        let gutter = " ".repeat(widgets::GUTTER_WIDTH);
        let line = Line::raw(format!("{accent}{gutter}hello world"));
        let transcript = SelectableTranscript::from_lines(&[line], 40);
        let chrome_end = saturating_u16(widgets::ACCENT_WIDTH + widgets::GUTTER_WIDTH);

        assert_eq!(
            transcript.selected_text(TranscriptSelection {
                anchor: TranscriptPosition { row: 0, column: 0 },
                head: TranscriptPosition {
                    row: 0,
                    column: chrome_end.saturating_sub(1),
                },
            }),
            Ok(String::new()),
            "accent + gutter alone must not copy"
        );
        assert_eq!(
            transcript.selected_text(TranscriptSelection {
                anchor: TranscriptPosition { row: 0, column: 0 },
                head: TranscriptPosition {
                    row: 0,
                    column: chrome_end.saturating_add(4),
                },
            }),
            Ok("hello".into()),
            "selection that starts on chrome still copies content cells"
        );
    }

    #[test]
    fn selectable_transcript_windows_match_full_rendering_at_all_offsets() {
        let lines = vec![
            Line::raw("alpha beta gamma delta"),
            Line::styled("café 🙂 omega", Style::default().fg(Color::Cyan)),
            Line::raw("short"),
            Line::raw("one two three four five six"),
        ];

        for width in [5, 9, 16] {
            let full = SelectableTranscript::from_lines(&lines, width);
            let plan = WrapPlan::from_lines(&lines, width);

            for first_row in 0..=full.rows.len() {
                for row_count in 0..=full.rows.len().saturating_add(1) {
                    let window = SelectableTranscript::window(&plan, first_row, row_count);
                    let expected = full
                        .render_lines(None)
                        .into_iter()
                        .skip(first_row)
                        .take(row_count)
                        .collect::<Vec<_>>();

                    assert_eq!(window.render_lines(None), expected);
                    assert_eq!(window.first_row, first_row.min(full.rows.len()));
                    assert_eq!(window.total_rows(), full.rows.len());
                }
            }
        }
    }

    #[test]
    fn selectable_transcript_window_preserves_absolute_selection_positions() {
        let lines = vec![
            Line::raw("alpha beta gamma delta"),
            Line::raw("café 🙂 omega"),
            Line::raw("tail"),
        ];
        let width = 8;
        let full = SelectableTranscript::from_lines(&lines, width);
        let plan = WrapPlan::from_lines(&lines, width);
        let window = SelectableTranscript::window(&plan, 2, 3);
        let selection = TranscriptSelection {
            anchor: TranscriptPosition { row: 2, column: 0 },
            head: TranscriptPosition { row: 4, column: 3 },
        };

        assert_eq!(
            window.position_at(2, 0).map(|position| position.row),
            Some(2)
        );
        assert_eq!(
            window.selected_text(selection),
            full.selected_text(selection)
        );
        assert_eq!(
            window.render_lines(Some(selection)),
            full.render_lines(Some(selection))[2..5].to_vec()
        );
    }

    #[test]
    fn selectable_cache_reuses_transcript_across_view_calls() {
        let mut tui = Tui::new(NoopEngine);
        tui.handle(Event::Resize {
            width: 80,
            height: 12,
        });
        tui.active_record_mut()
            .transcript
            .push(TranscriptEntry::Info("cache me".into()));
        tui.bump_selectable_epoch();

        let first = tui.view().selectable.arc();
        let second = tui.view().selectable.arc();
        assert!(
            Arc::ptr_eq(&first, &second),
            "unchanged content must reuse the cached selectable transcript"
        );
        assert!(
            !first.rows.is_empty(),
            "cached selectable transcript must be non-empty after content exists"
        );
    }

    #[test]
    fn selectable_cache_survives_live_clock_ticks_while_waiting() {
        // Content stays cached; paint rebuilds only the status chrome from `now`.
        let mut tui = Tui::new(NoopEngine);
        tui.handle(Event::Resize {
            width: 80,
            height: 12,
        });
        tui.begin_submission("wait");
        tui.tick(Duration::from_secs(46));
        let first = tui.view().selectable.arc();
        tui.tick(Duration::from_secs(119));
        let second = tui.view().selectable.arc();
        assert!(
            Arc::ptr_eq(&first, &second),
            "clock ticks must not rebuild the selectable content index"
        );
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
            following_bottom.saturating_sub(scroll_offset),
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
    fn counting_rows_agrees_with_building_them() {
        let lines = vec![
            Line::raw("short"),
            Line::raw(""),
            Line::raw("a much longer paragraph that has to wrap several times at this width"),
            Line::raw("AAA AAA AAAAA AA AAAAAA"),
            Line::raw("supercalifragilisticexpialidocious antidisestablishmentarianism"),
            Line::raw("  indented continuation with trailing space   "),
            Line::raw("café ñandú 日本語 mixed widths"),
            Line::raw(format!("joined{}", render::WRAP_JOINER_SPACE)),
            Line::raw(format!("tight{}", render::WRAP_JOINER_TIGHT)),
        ];

        for width in [1, 2, 3, 7, 10, 23, 80] {
            assert_eq!(
                SelectableTranscript::row_count(&lines, width),
                SelectableTranscript::from_lines(&lines, width).rows.len(),
                "row count and row construction disagreed at width {width}"
            );
        }
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

    /// Three settled turns, long enough that scrolling has somewhere to go and
    /// that every prompt row a `{`/`}` jump looks for is really in the render.
    fn scrollable_tui() -> Tui<NoopEngine> {
        let mut tui = Tui::new(NoopEngine);
        tui.handle(Event::Resize {
            width: 80,
            height: 24,
        });
        for turn in 0..12 {
            let body = "body\n".repeat(40);
            tui.begin_submission(format!("prompt-{turn}"));
            tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(body.clone())));
            tui.apply_progress(TurnEvent::StateChanged(TurnState::Completed));
            assert_eq!(
                tui.finish_provider_turn(TuiProviderOutcome::Completed(body)),
                None
            );
        }
        // Settled turns are elided by default, which would leave the prompt rows
        // a jump navigates by collapsed out of the render.
        tui.active_record_mut().history_expanded = true;
        tui.scroll_to_end();
        tui.active_record_mut().focus = TranscriptFocus::Composer;
        tui
    }

    /// Esc moves the reader, not the turn: it hands focus to the transcript so
    /// a running answer can be read from the top, and leaves the turn running.
    #[test]
    fn escape_focuses_the_transcript_without_cancelling_a_running_turn() {
        let mut tui = scrollable_tui();
        tui.set_running(true);

        assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::Render);
        assert_eq!(tui.view().focus, TranscriptFocus::Viewport);
        assert!(tui.view().running);
    }

    /// The prompt is one keystroke away again, and the transcript keymap that
    /// Esc turned on is what makes `i` mean "back to typing" instead of text.
    #[test]
    fn escape_then_i_returns_to_the_composer() {
        let mut tui = scrollable_tui();

        tui.handle(Event::Key(Key::Escape));
        assert_eq!(tui.view().focus, TranscriptFocus::Viewport);

        tui.handle(Event::Key(Key::Char('i')));
        assert_eq!(tui.view().focus, TranscriptFocus::Composer);

        tui.handle(Event::Key(Key::Char('x')));
        assert_eq!(tui.input(), "x");
    }

    #[test]
    fn escape_preserves_main_surface_selection_while_a_turn_is_running() {
        let mut tui = selected_tui();
        tui.set_running(true);
        let selected = tui.selected_text().map(str::to_owned);

        assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::Render);
        assert_eq!(tui.selected_text(), selected.as_deref());
        assert!(tui.view().running);
    }

    #[test]
    fn viewport_navigation_detaches_from_the_bottom() {
        let mut tui = scrollable_tui();
        assert!(tui.following_bottom());

        let bottom = tui.detached_scroll_bottom();
        let record = tui.active_record_mut();
        record.scroll_offset = bottom;
        record.following_bottom = false;
        record.focus = TranscriptFocus::Viewport;
        assert!(!tui.following_bottom());

        tui.handle(Event::Key(Key::Char('G')));
        assert!(tui.following_bottom(), "G re-attaches to the bottom");
    }

    #[test]
    fn viewport_vim_motions_scroll_rows_rather_than_typing_into_the_composer() {
        let mut tui = scrollable_tui();
        tui.active_record_mut().focus = TranscriptFocus::Viewport;

        let bottom = tui.view().scroll_offset;
        tui.handle(Event::Key(Key::Char('k')));
        assert_eq!(tui.view().scroll_offset, bottom - 1, "k scrolls one row up");

        tui.handle(Event::Key(Key::Char('j')));
        assert_eq!(tui.view().scroll_offset, bottom, "j scrolls one row down");

        tui.handle(Event::Key(Key::CtrlU));
        assert!(tui.view().scroll_offset < bottom, "Ctrl+U is a half page");

        tui.handle(Event::Key(Key::Char('g')));
        tui.handle(Event::Key(Key::Char('g')));
        assert_eq!(tui.view().scroll_offset, 0, "gg reaches the top");

        assert_eq!(tui.input(), "", "no motion ever reached the prompt");
    }

    #[test]
    fn braces_walk_the_transcript_prompt_by_prompt_in_both_directions() {
        let mut tui = scrollable_tui();
        tui.active_record_mut().focus = TranscriptFocus::Viewport;
        tui.handle(Event::Key(Key::Char('g')));
        tui.handle(Event::Key(Key::Char('g')));
        assert_eq!(tui.view().scroll_offset, 0);

        tui.handle(Event::Key(Key::Char('}')));
        let first = tui.view().scroll_offset;
        assert!(first > 0, "}} advances to the next prompt");

        tui.handle(Event::Key(Key::Char('}')));
        let second = tui.view().scroll_offset;
        assert!(second > first, "}} keeps advancing rather than resting");

        tui.handle(Event::Key(Key::Char('{')));
        assert_eq!(tui.view().scroll_offset, first, "{{ walks back one prompt");
    }

    /// `g` alone is a prefix now, so it must not act until its second key.
    #[test]
    fn an_abandoned_g_chord_neither_acts_nor_survives_the_next_event() {
        let mut tui = scrollable_tui();
        tui.handle(Event::Key(Key::Escape));
        let resting = tui.view().scroll_offset;

        tui.handle(Event::Key(Key::Char('g')));
        assert_eq!(tui.view().scroll_offset, resting, "g alone moves nothing");

        tui.handle(Event::Key(Key::Char('z')));
        assert_eq!(tui.view().scroll_offset, resting);

        tui.handle(Event::Key(Key::Char('g')));
        tui.handle(Event::Resize {
            width: 80,
            height: 24,
        });
        tui.handle(Event::Key(Key::Char('g')));
        assert_eq!(
            tui.view().scroll_offset,
            resting,
            "a resize between the two g's abandons the chord"
        );
    }

    #[test]
    fn i_returns_to_the_composer_and_typing_resumes() {
        let mut tui = scrollable_tui();
        tui.active_record_mut().focus = TranscriptFocus::Viewport;

        assert_eq!(tui.handle(Event::Key(Key::Char('i'))), Action::Render);
        assert_eq!(tui.view().focus, TranscriptFocus::Composer);

        tui.handle(Event::Key(Key::Char('j')));
        assert_eq!(tui.input(), "j", "j is a character again in the composer");
    }

    /// The legend is contextual, so it can stay short enough to actually read.
    #[test]
    fn the_hint_row_names_only_the_keys_the_current_state_can_use() {
        let text = |tui: &Tui<NoopEngine>| {
            hint_spans(&tui.view())
                .iter()
                .map(|span| span.content.as_ref().to_owned())
                .collect::<String>()
        };

        let mut tui = scrollable_tui();
        let empty = text(&tui);
        assert!(!empty.contains("Enter"), "nothing to send yet: {empty:?}");
        assert!(empty.contains("Esc:normal"), "{empty:?}");
        assert!(empty.contains("^?:shortcuts"), "{empty:?}");

        tui.handle(Event::Key(Key::Char('h')));
        let typing = text(&tui);
        assert!(typing.contains("Enter:send"), "{typing:?}");

        tui.set_running(true);
        let running = text(&tui);
        assert!(running.contains("Enter:queue"), "{running:?}");

        tui.set_running(false);
        tui.handle(Event::Key(Key::Escape));
        let normal = text(&tui);
        assert_eq!(tui.view().focus, TranscriptFocus::Viewport);
        assert!(normal.contains("j/k:scroll"), "{normal:?}");
        assert!(normal.contains("i:insert"), "{normal:?}");
        assert!(
            !normal.contains("Esc:normal"),
            "the door it already went through is not a hint: {normal:?}"
        );
    }

    /// The tree hangs below the composer, so the arrows walk between them as
    /// one vertical surface. Tab used to be the only door in, and it cycled.
    #[test]
    fn the_arrows_walk_between_the_prompt_and_the_subagent_tree() {
        let mut tui = scrollable_tui();
        tui.apply_task_execution_event(
            "bug-hunter",
            TuiExecutionEvent::ForegroundStarted { id: 1 },
        );
        assert!(!tui.executions.is_empty(), "a subagent is running");

        tui.handle(Event::Key(Key::Down));
        assert_eq!(
            tui.view().execution_selection,
            Some(TranscriptId::Main),
            "an empty prompt walks down into the tree"
        );

        tui.handle(Event::Key(Key::Down));
        assert_eq!(
            tui.view().execution_selection,
            Some(TranscriptId::Subagent(1))
        );

        tui.handle(Event::Key(Key::Up));
        assert_eq!(tui.view().execution_selection, Some(TranscriptId::Main));

        tui.handle(Event::Key(Key::Up));
        assert_eq!(
            tui.view().execution_selection,
            None,
            "walking up off the first row returns to the prompt"
        );
    }

    /// Down belongs to the text as soon as there is any.
    #[test]
    fn a_prompt_with_text_keeps_the_down_arrow() {
        let mut tui = scrollable_tui();
        tui.apply_task_execution_event(
            "bug-hunter",
            TuiExecutionEvent::ForegroundStarted { id: 1 },
        );
        tui.handle(Event::Key(Key::Char('x')));

        tui.handle(Event::Key(Key::Down));
        assert_eq!(tui.view().execution_selection, None);
    }

    /// Main is where the reader already is, so accepting it is a way back.
    #[test]
    fn enter_on_main_returns_to_the_prompt() {
        let mut tui = scrollable_tui();
        tui.apply_task_execution_event(
            "bug-hunter",
            TuiExecutionEvent::ForegroundStarted { id: 1 },
        );
        tui.handle(Event::Key(Key::Down));

        tui.handle(Event::Key(Key::Enter));
        assert_eq!(tui.view().execution_selection, None);
        assert_eq!(tui.active_transcript, TranscriptId::Main);
    }

    /// The list has to be reachable from wherever the question is asked.
    #[test]
    fn ctrl_question_opens_the_shortcut_list_from_either_mode() {
        let mut tui = scrollable_tui();
        tui.handle(Event::Key(Key::CtrlQuestion));
        assert_eq!(
            tui.view().dialog.map(|dialog| dialog.title.clone()),
            Some("Keyboard shortcuts".to_owned())
        );

        tui.handle(Event::Key(Key::Escape));
        assert_eq!(tui.view().focus, TranscriptFocus::Composer);
        tui.active_record_mut().focus = TranscriptFocus::Viewport;
        tui.handle(Event::Key(Key::Char('?')));
        assert!(tui.view().dialog.is_some(), "? opens it in Normal mode");
    }

    /// A warning outranks a legend: the band is one row and cannot show both.
    #[test]
    fn a_notice_takes_the_band_from_the_hints() {
        let mut tui = scrollable_tui();
        tui.begin_submission("active");
        assert!(notice_spans(&tui.view()).is_empty());

        tui.handle(Event::Key(Key::CtrlC));
        let notice = notice_spans(&tui.view())
            .iter()
            .map(|span| span.content.as_ref().to_owned())
            .collect::<String>();
        assert!(
            notice.contains("Cancellation requested; waiting for confirmation."),
            "{notice:?}"
        );
    }

    /// Scrolling is not a mode switch. Ctrl+J/Ctrl+K and the wheel are reachable
    /// from the prompt, so letting them focus the transcript would leave the
    /// reader typing into a composer that had silently stopped accepting text.
    #[test]
    fn scrolling_detaches_the_viewport_without_taking_focus_from_the_composer() {
        for scroll in [
            Event::Key(Key::CtrlK),
            Event::MouseWheel(MouseWheelDirection::Up),
            Event::Key(Key::PageUp),
            Event::Key(Key::CtrlN),
        ] {
            let mut tui = scrollable_tui();

            tui.handle(scroll.clone());
            assert!(!tui.following_bottom(), "the viewport detached");
            assert_eq!(
                tui.view().focus,
                TranscriptFocus::Composer,
                "scrolling never enters the transcript keymap"
            );

            tui.handle(Event::Key(Key::Char('j')));
            assert_eq!(tui.input(), "j", "the composer still takes text");
        }
    }

    /// The palette is typed into, so it keeps the alphabet even when a mouse
    /// press left the transcript focused behind it.
    #[test]
    fn an_open_palette_keeps_the_alphabet_from_the_transcript_keymap() {
        let mut tui = scrollable_tui();
        tui.handle(Event::Key(Key::Char('/')));
        assert!(tui.palette_open);

        tui.active_record_mut().focus = TranscriptFocus::Viewport;
        tui.handle(Event::Key(Key::Char('g')));
        assert_eq!(tui.input(), "/g", "the palette query kept the key");
    }

    #[test]
    fn slash_opens_the_command_palette_while_a_turn_is_running() {
        let mut tui = Tui::new(NoopEngine);
        tui.begin_submission("active");
        assert!(tui.view().running);

        tui.handle(Event::Key(Key::Char('/')));
        assert!(tui.palette_open, "busy turns must still open / commands");
        assert_eq!(tui.input(), "/");
    }

    #[test]
    fn busy_palette_enter_opens_the_selected_dialog_instead_of_submitting_slash() {
        let mut tui = Tui::new(NoopEngine);
        tui.enable_busy_policy_routing();
        tui.set_palette_entries(vec![
            PaletteEntry::new("help", "Show commands", "", PaletteEntryKind::BuiltIn)
                .with_dialog("help"),
            PaletteEntry::new(
                "subagent",
                "Choose subagent",
                "[name]",
                PaletteEntryKind::BuiltIn,
            )
            .with_dialog("subagent"),
        ]);
        tui.begin_submission("active");
        tui.handle(Event::Key(Key::Char('/')));
        tui.handle(Event::Key(Key::Down));
        assert_eq!(
            tui.handle(Event::Key(Key::Enter)),
            Action::OpenDialog("subagent".into()),
            "busy Enter must resolve the highlighted palette row"
        );
        assert!(!tui.palette_open);
        assert!(tui.input().is_empty());
    }

    #[test]
    fn busy_palette_renders_registered_commands() {
        let mut tui = Tui::new(NoopEngine);
        tui.set_palette_entries(vec![
            PaletteEntry::new("help", "Show commands", "", PaletteEntryKind::BuiltIn),
            PaletteEntry::new(
                "subagent",
                "Choose subagent",
                "[name]",
                PaletteEntryKind::BuiltIn,
            ),
        ]);
        tui.handle(Event::Resize {
            width: 80,
            height: 24,
        });
        tui.begin_submission("active");
        tui.handle(Event::Key(Key::Char('/')));

        let mut renderer = RatatuiRenderer::new(
            RatatuiTerminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap(),
        );
        renderer.render(tui.view()).unwrap();
        let rendered = renderer
            .terminal()
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(
            rendered.contains("help") && rendered.contains("subagent"),
            "busy palette must paint catalog entries: {rendered:?}"
        );
    }

    #[test]
    fn typing_from_queue_focus_returns_to_composer_and_opens_the_palette() {
        let mut tui = Tui::with_queue_capacity(NoopEngine, 2);
        tui.begin_submission("active");
        tui.input = "queued".into();
        tui.enqueue_composer();
        tui.handle(Event::Key(Key::Tab));
        assert_eq!(tui.view().surface_focus, SurfaceFocus::Queue);

        tui.handle(Event::Key(Key::Char('/')));
        assert_eq!(tui.view().surface_focus, SurfaceFocus::Composer);
        assert!(tui.palette_open);
        assert_eq!(tui.input(), "/");
    }

    /// Ctrl+D and Ctrl+U carry two meanings; the composer must keep its own.
    #[test]
    fn ctrl_d_and_ctrl_u_still_edit_the_composer_while_it_holds_focus() {
        let mut tui = Tui::new(NoopEngine);
        for character in "hello world".chars() {
            tui.handle(Event::Key(Key::Char(character)));
        }

        tui.input_cursor = 5;
        tui.handle(Event::Key(Key::CtrlD));
        assert_eq!(tui.input(), "helloworld", "Ctrl+D deletes forward");

        tui.handle(Event::Key(Key::CtrlU));
        assert_eq!(tui.input(), "world", "Ctrl+U deletes to the line start");
    }

    /// Drags across one known transcript row, leaving a painted selection.
    fn selected_tui() -> Tui<NoopEngine> {
        let mut tui = Tui::new(NoopEngine);
        tui.handle(Event::Resize {
            width: 80,
            height: 12,
        });
        tui.active_record_mut()
            .transcript
            .push(TranscriptEntry::Info("alpha café 🙂 omega".into()));
        tui.bump_selectable_epoch();

        tui.handle(Event::MouseDown { column: 24, row: 1 });
        tui.handle(Event::MouseDrag { column: 30, row: 1 });
        tui.handle(Event::MouseUp { column: 30, row: 1 });
        assert_eq!(tui.selected_text(), Some("café 🙂"));

        tui
    }

    /// The selection used to outlive every click that missed the transcript.
    #[test]
    fn a_click_outside_the_transcript_drops_the_selection_and_restores_the_composer() {
        let mut tui = selected_tui();
        assert_eq!(tui.view().focus, TranscriptFocus::Viewport);

        let composer_row = tui.size().1 - 2;
        tui.handle(Event::MouseDown {
            column: 4,
            row: composer_row,
        });
        assert_eq!(tui.selected_text(), None);
        assert_eq!(tui.view().focus, TranscriptFocus::Composer);
    }

    /// A press and release on the same cell selects nothing, which is exactly
    /// how a reader dismisses what was selected before.
    #[test]
    fn a_bare_click_inside_the_transcript_drops_the_selection_and_restores_the_composer() {
        let mut tui = selected_tui();

        tui.handle(Event::MouseDown { column: 24, row: 1 });
        tui.handle(Event::MouseUp { column: 24, row: 1 });
        assert_eq!(tui.selected_text(), None);
        assert_eq!(tui.view().focus, TranscriptFocus::Composer);
    }

    fn press<E: Engine>(tui: &mut Tui<E>, code: KeyCode, modifiers: KeyModifiers) -> Action {
        let event = map_key(crossterm::event::KeyEvent::new(code, modifiers)).unwrap();
        tui.handle(event)
    }

    /// Holding a key down is how a reader deletes a line or walks a word at a
    /// time. Dropping auto-repeat wholesale made every one of those a
    /// one-shot, which is why a held Ctrl+W removed a single word.
    #[test]
    fn held_editing_keys_repeat_while_commands_and_modes_fire_once() {
        let ctrl = KeyModifiers::CONTROL;
        let repeated = |code, modifiers| {
            map_key(crossterm::event::KeyEvent::new_with_kind(
                code,
                modifiers,
                KeyEventKind::Repeat,
            ))
        };

        for (code, modifiers) in [
            (KeyCode::Backspace, KeyModifiers::NONE),
            (KeyCode::Backspace, ctrl),
            (KeyCode::Delete, ctrl),
            (KeyCode::Char('w'), ctrl),
            (KeyCode::Char('a'), KeyModifiers::NONE),
            (KeyCode::Left, KeyModifiers::NONE),
            (KeyCode::Up, KeyModifiers::NONE),
            (KeyCode::PageDown, KeyModifiers::NONE),
        ] {
            assert!(
                repeated(code, modifiers).is_some(),
                "{code:?} with {modifiers:?} should repeat while held"
            );
        }

        for (code, modifiers) in [
            (KeyCode::Enter, KeyModifiers::NONE),
            (KeyCode::Esc, KeyModifiers::NONE),
            (KeyCode::Tab, KeyModifiers::NONE),
            (KeyCode::Char('c'), ctrl),
            (KeyCode::Char('o'), ctrl),
            (KeyCode::Char('D'), ctrl),
            (KeyCode::Char('P'), ctrl),
        ] {
            assert!(
                repeated(code, modifiers).is_none(),
                "{code:?} with {modifiers:?} is a command and must fire once per press"
            );
        }
    }

    #[test]
    fn control_delete_removes_the_word_ahead_of_the_cursor() {
        let mut tui = Tui::new(NoopEngine);
        for character in "borra esta palabra".chars() {
            press(&mut tui, KeyCode::Char(character), KeyModifiers::NONE);
        }
        press(&mut tui, KeyCode::Home, KeyModifiers::NONE);
        press(&mut tui, KeyCode::Delete, KeyModifiers::CONTROL);

        assert_eq!(tui.input(), " esta palabra");
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
            (KeyCode::Backspace, ctrl, Key::DeletePreviousWord),
            (KeyCode::Delete, ctrl, Key::DeleteNextWord),
            (KeyCode::Char('u'), ctrl, Key::CtrlU),
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
            (KeyCode::Char('d'), ctrl, Key::CtrlD),
            (KeyCode::Char('o'), ctrl, Key::CtrlO),
            (KeyCode::Char('O'), ctrl, Key::CtrlShiftO),
            (KeyCode::Char('t'), ctrl, Key::CtrlT),
            (KeyCode::Char('y'), ctrl, Key::CtrlY),
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
        assert!(running.input().is_empty());
        assert_eq!(running.queue_entries()[0].prompt(), "queued 🙂");
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

        // Informational bodies keep newlines so "message\nAction: …" does not glue
        // together; two help lines + the empty-list placeholder row.
        let informational = DialogView::informational("Details", "first line\nsecond line");
        assert_eq!(dialog_desired_rows(&informational, 30), 3);

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
        // Help keeps its newlines and wraps for Confirm; two help lines + one entry.
        assert_eq!(dialog_desired_rows(&confirm, 30), 3);
        assert_eq!(
            dialog_help_lines(&confirm, 40),
            vec!["native::read", "/work/alpha"]
        );
    }

    #[test]
    fn permission_dialog_body_keeps_the_full_target_and_access() {
        assert_eq!(permission_dialog_title("bash"), "Bash command");
        assert_eq!(permission_dialog_title("write"), "Write file");
        assert_eq!(
            permission_dialog_title("probe::tool"),
            "Permission required"
        );

        let body = permission_dialog_body(
            "cd /tmp && git commit -m long-message",
            "Write",
            Some("permission policy requires confirmation"),
        );
        assert!(body.contains("cd /tmp && git commit -m long-message"));
        assert!(body.contains("Access: Write"));
        assert!(body.contains("permission policy requires confirmation"));
        assert!(!body.contains('…'));
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
                (Cow::Borrowed("Enter"), Cow::Borrowed("select")),
                (Cow::Borrowed("/"), Cow::Borrowed("search")),
                (Cow::Borrowed("esc"), Cow::Borrowed("close")),
            ]
        );
        assert_eq!(
            dialog_shortcut_labels(&arm_search(picker)),
            vec![
                (Cow::Borrowed("↑↓"), Cow::Borrowed("navigate")),
                (Cow::Borrowed("Enter"), Cow::Borrowed("select")),
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
            first_page.contains(&(Cow::Borrowed("Enter"), Cow::Borrowed("resume"))),
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
        let layout = screen_layout(Rect::new(0, 0, 120, 24), "", 0, 0);
        for band in [
            layout.composer,
            layout.notice,
            layout.tree,
            layout.footer,
            layout.queue,
        ] {
            if band.height == 0 {
                continue;
            }
            assert_eq!(band.x, CHROME_GUTTER, "{band:?}");
            assert_eq!(band.width, 120 - 2 * CHROME_GUTTER, "{band:?}");
        }
        // The transcript indents its own content on the left, so it owes the
        // gutter only on the right — where prose would otherwise outrun the
        // composer it belongs to.
        assert_eq!(layout.transcript.x, 0);
        assert_eq!(layout.transcript.right(), 120 - CHROME_GUTTER);
        assert_eq!(layout.composer.right(), layout.transcript.right());
        assert_eq!(layout.queue.height, 0);

        assert_eq!(
            [0_u16, 1, 24, 26, 28, 30, 32, 120].map(chrome_gutter),
            [0, 0, 0, 1, 2, 3, 4, 4]
        );

        for width in 0..=64_u16 {
            let layout = screen_layout(Rect::new(0, 0, width, 24), "", 0, 0);
            assert!(layout.composer.right() <= width, "width {width}");
            assert!(
                layout.composer.width >= width.min(MIN_GUTTERED_COMPOSER_WIDTH),
                "width {width} starves the composer: {:?}",
                layout.composer
            );
        }

        let with_queue = screen_layout(Rect::new(0, 0, 120, 24), "", 0, 3);
        // Three message rows plus the muted Queued status line.
        assert_eq!(with_queue.queue.height, 4);
        assert_eq!(with_queue.queue.x, CHROME_GUTTER);
        assert_eq!(with_queue.queue.width, with_queue.composer.width);
        assert_eq!(with_queue.queue.bottom(), with_queue.composer.y);
    }

    #[test]
    fn composer_soft_wraps_long_unicode_input_without_horizontal_scroll() {
        let input = "abcdefghijklmnop🙂qrstuvwxyz";
        let wrapped = composer_layout(input, input.chars().count(), 21);
        assert_eq!(wrapped.text, "abcdefghijklmnop🙂qrs\ntuvwxyz");
        assert_eq!((wrapped.cursor_line, wrapped.cursor_column), (1, 7));

        let before_wrapped_emoji = composer_layout("abcdefghijklmnopqrst🙂", 20, 21);
        assert_eq!(
            (
                before_wrapped_emoji.cursor_line,
                before_wrapped_emoji.cursor_column,
            ),
            (1, 0)
        );

        let family = "👩‍👩‍👧‍👦";
        let family_input = format!("abcdefghijklmnopqrst{family}");
        let wrapped_family = composer_layout(&family_input, family_input.chars().count(), 21);
        assert_eq!(
            wrapped_family.text,
            format!("abcdefghijklmnopqrst\n{family}")
        );
        assert_eq!(
            (wrapped_family.cursor_line, wrapped_family.cursor_column),
            (1, 2)
        );

        let combining_mark = composer_layout("e\u{301}", 1, 10);
        assert_eq!(
            (combining_mark.cursor_line, combining_mark.cursor_column),
            (0, 1)
        );

        let windows_lines = composer_layout("a\r\nb", 4, 10);
        assert_eq!(windows_lines.text, "a\r\nb");
        assert_eq!(
            (windows_lines.cursor_line, windows_lines.cursor_column),
            (1, 1)
        );
        assert_eq!(windows_lines.rows, 2);

        let layout = screen_layout(Rect::new(0, 0, 30, 12), input, 0, 0);
        assert_eq!(layout.composer.height, 4);

        let mut tui = Tui::new(NoopEngine);
        assert_eq!(tui.handle(Event::Paste(input.into())), Action::Render);
        let terminal = RatatuiTerminal::new(ratatui::backend::TestBackend::new(30, 12)).unwrap();
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

        assert!(rendered.contains("abcdefghijklmnop"), "{rendered:?}");
        assert!(rendered.contains("tuvwxyz"), "{rendered:?}");
    }

    #[test]
    fn composer_stops_growing_at_its_ceiling() {
        let long = "x".repeat(700);

        let tall = screen_layout(Rect::new(0, 0, 80, 40), &long, 0, 0);
        assert_eq!(tall.composer.height, MAX_COMPOSER_ROWS);
        assert!(tall.transcript.height > 0);

        // A short terminal caps the composer below the absolute ceiling so the
        // transcript keeps most of the screen.
        let short = screen_layout(Rect::new(0, 0, 80, 15), &long, 0, 0);
        assert_eq!(short.composer.height, composer_ceiling(15));
        assert!(short.composer.height < MAX_COMPOSER_ROWS);
        assert!(short.transcript.height > short.composer.height);

        // Attachments scroll inside the box instead of lifting the ceiling.
        let with_attachments = screen_layout(Rect::new(0, 0, 80, 40), &long, 12, 0);
        assert_eq!(with_attachments.composer.height, MAX_COMPOSER_ROWS);
    }

    #[test]
    fn composer_scrolls_past_its_ceiling_keeping_the_cursor_and_marking_hidden_rows() {
        let input = (0..30)
            .map(|index| format!("row{index:02}"))
            .collect::<Vec<_>>()
            .join("\n");

        let mut tui = Tui::new(NoopEngine);
        assert_eq!(tui.handle(Event::Paste(input)), Action::Render);
        let terminal = RatatuiTerminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        let mut renderer = RatatuiRenderer::new(terminal);
        renderer.render(tui.view()).unwrap();
        let buffer = renderer.terminal().backend().buffer().clone();
        let rows = buffer
            .content
            .chunks(usize::from(buffer.area.width))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>();
        let rendered = rows.join("\n");

        assert!(
            rows.iter().any(|row| row.contains("row29")),
            "the cursor line must stay visible: {rendered}"
        );
        assert!(
            !rows.iter().any(|row| row.contains("row00")),
            "rows past the ceiling must scroll out of view: {rendered}"
        );
        assert!(
            rows.iter().any(|row| row.contains("↑24")),
            "the composer must say how many rows are hidden: {rendered}"
        );
    }

    #[test]
    fn composer_replaces_a_grapheme_that_cannot_fit_the_viewport() {
        let narrow_emoji = composer_layout("🙂", 1, 1);
        assert_eq!(narrow_emoji.text, "\u{fffd}");
        assert_eq!(
            (narrow_emoji.cursor_line, narrow_emoji.cursor_column),
            (1, 0)
        );
        assert_eq!(narrow_emoji.rows, 2);

        let mut tui = Tui::new(NoopEngine);
        assert_eq!(tui.handle(Event::Paste("🙂".into())), Action::Render);
        let terminal = RatatuiTerminal::new(ratatui::backend::TestBackend::new(4, 12)).unwrap();
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

        assert!(rendered.contains('\u{fffd}'), "{rendered:?}");
    }

    #[test]
    fn composer_wraps_between_words_instead_of_inside_them() {
        let input = "hello wonderful world";
        let wrapped = composer_layout(input, input.chars().count(), 12);
        assert_eq!(wrapped.text, "hello \nwonderful \nworld");
        assert_eq!((wrapped.cursor_line, wrapped.cursor_column), (2, 5));
        assert_eq!(wrapped.rows, 3);

        // A cursor inside a word that moved to the next row moves with it.
        let inside_word = composer_layout(input, 8, 12);
        assert_eq!((inside_word.cursor_line, inside_word.cursor_column), (1, 2));

        // A word that cannot fit any row still breaks at the column edge.
        let long_word = "hi abcdefghij";
        let hard_broken = composer_layout(long_word, long_word.chars().count(), 6);
        assert_eq!(hard_broken.text, "hi abc\ndefghi\nj");
        assert_eq!((hard_broken.cursor_line, hard_broken.cursor_column), (2, 1));

        // A hard newline keeps its own row even when the words around it fit.
        let explicit = composer_layout("one two\nthree", 8, 20);
        assert_eq!(explicit.text, "one two\nthree");
        assert_eq!((explicit.cursor_line, explicit.cursor_column), (1, 0));
    }

    #[test]
    fn composer_space_past_the_edge_moves_the_cursor_to_the_next_row() {
        let full_row = "hello world";
        let after_space = format!("{full_row} ");
        let layout = composer_layout(&after_space, after_space.chars().count(), 11);
        assert_eq!(layout.text, full_row);
        assert_eq!((layout.cursor_line, layout.cursor_column), (1, 0));
        assert_eq!(layout.rows, 2);

        let next_word = format!("{full_row} x");
        let continued = composer_layout(&next_word, next_word.chars().count(), 11);
        assert_eq!(continued.text, "hello world\nx");
        assert_eq!((continued.cursor_line, continued.cursor_column), (1, 1));

        let before_x = composer_layout(&next_word, 12, 11);
        assert_eq!((before_x.cursor_line, before_x.cursor_column), (1, 0));
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
    fn secret_entry_escape_closes_while_double_ctrl_c_exits_globally() {
        let mut escape = Tui::new(NoopEngine);
        escape.apply_submission_outcome(TuiSubmissionOutcome::SecretEntry(secret_entry_view()));
        escape.handle(Event::Paste("SECRET_CANCEL_SENTINEL".into()));
        assert_eq!(escape.handle(Event::Key(Key::Escape)), Action::Render);
        assert!(escape.view().secret_entry.is_none());

        let mut control_c = Tui::new(NoopEngine);
        control_c.apply_submission_outcome(TuiSubmissionOutcome::SecretEntry(secret_entry_view()));
        assert_eq!(control_c.handle(Event::Key(Key::CtrlC)), Action::Render);
        assert!(control_c.view().secret_entry.is_some());
        assert_eq!(control_c.handle(Event::Key(Key::CtrlC)), Action::Quit);
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

    fn runtime_glue_ask_user_request() -> AskUserRequest {
        use agens_core::ask_user::AskUserOption;

        let options = vec![
            AskUserOption::new("a", "Option A", None, None),
            AskUserOption::new("b", "Option B", None, None),
        ];
        let question = AskUserQuestion::new(
            "plan",
            "Which plan?",
            None,
            AskUserMode::Single,
            options,
            false,
            false,
            false,
        );
        AskUserRequest::new(None, vec![question]).expect("valid request")
    }

    /// Calls the real drain arm until it opens the request the waiter
    /// thread just parked, spinning rather than sleeping: the waiter sends
    /// on its own thread, so the exact instant its message reaches
    /// `requests` is not observable from here without polling for it.
    fn open_first_ask_user_request(
        tui: &mut Tui<NoopEngine>,
        bridge: &TuiAskUserBridge,
        requests: &mpsc::Receiver<TuiAskUserRequest>,
    ) -> u64 {
        loop {
            if let Some(id) = drain_ask_user_request(tui, None, Some(bridge), Some(requests)) {
                return id;
            }
            thread::yield_now();
        }
    }

    /// Drives a real request through the event loop's actual drain and
    /// reply arms — not a reimplementation of them, the exact functions the
    /// running loop calls — proving they open the overlay for a genuinely
    /// parked request and, once the UI resolves it, deliver that resolution
    /// back to the parked tool thread through the bridge.
    #[test]
    fn the_ask_user_drain_and_reply_arms_carry_a_real_request_through_open_and_resolve() {
        use agens_core::ask_user::AskUserAnswer;

        let (bridge, requests) = TuiAskUserBridge::channel();
        let cancellation = agens_core::HeadlessTurnCancellation::new();
        let waiting_bridge = bridge.clone();

        let waiter = thread::spawn(move || {
            waiting_bridge.wait_for_reply(runtime_glue_ask_user_request(), None, &cancellation)
        });

        let mut tui = Tui::new(NoopEngine);
        let id = open_first_ask_user_request(&mut tui, &bridge, &requests);
        assert!(tui.ask_user_snapshot().is_some());
        assert!(bridge.is_pending(id));

        let stale_drain =
            drain_ask_user_request(&mut tui, Some(id), Some(&bridge), Some(&requests));
        assert_eq!(
            stale_drain, None,
            "the drain arm must not reopen while a request is already active"
        );

        let answer = AskUserAnswer {
            question_id: "plan".into(),
            selected: vec!["a".into()],
            other: None,
            note: None,
        };
        resolve_ask_user_reply(
            Some(&bridge),
            id,
            AskUserReply::Answered(vec![answer.clone()]),
        );

        assert_eq!(
            waiter.join().expect("waiter thread should not panic"),
            AskUserReply::Answered(vec![answer])
        );
        assert!(!bridge.is_pending(id));
    }

    /// The spec's "surface closes" scenario as seen by the running loop: the
    /// bridge resolving a request on its own releases the parked tool thread
    /// as expected, but the bug this pins is that nothing DROVE the overlay
    /// closed on the UI side — before this fix, `self.ask_user` stayed
    /// populated, kept answering keys instead of routing them anywhere real,
    /// and no later UI action could ever resolve the (already-resolved)
    /// request again.
    ///
    /// Cancellation is the trigger because it is the only self-triggered
    /// resolution left: a question no longer resolves itself on a deadline.
    #[test]
    fn bridge_side_cancellation_dismisses_the_open_overlay_and_releases_the_keyboard() {
        let (bridge, requests) = TuiAskUserBridge::channel();
        let cancellation = agens_core::HeadlessTurnCancellation::new();
        let waiting_bridge = bridge.clone();
        let waiting_cancellation = cancellation.clone();

        let waiter = thread::spawn(move || {
            waiting_bridge.wait_for_reply(
                runtime_glue_ask_user_request(),
                None,
                &waiting_cancellation,
            )
        });

        let mut tui = Tui::new(NoopEngine);
        let id = open_first_ask_user_request(&mut tui, &bridge, &requests);
        assert!(tui.ask_user_snapshot().is_some());

        cancellation.cancel();
        assert_eq!(
            waiter.join().expect("waiter thread should not panic"),
            AskUserReply::Cancelled
        );

        assert!(
            !dismiss_resolved_ask_user(&mut tui, None, Some(&bridge)),
            "there must be no active overlay to dismiss before the loop notices the resolution"
        );
        let dismissed = dismiss_resolved_ask_user(&mut tui, Some(id), Some(&bridge));

        assert!(
            dismissed,
            "a cancelled request must release the overlay it opened, not leave it stuck \
             answering a turn that already ended"
        );
        assert!(
            tui.ask_user_snapshot().is_none(),
            "the overlay must be gone so `handle_key` stops routing keystrokes into a dead \
             ask-user interaction and returns to ordinary composer handling"
        );
    }

    #[test]
    fn terminal_key_mapping_preserves_queue_and_activity_control_keys() {
        let key = |code, modifiers| {
            map_key(KeyEvent::new_with_kind(
                code,
                modifiers,
                KeyEventKind::Press,
            ))
        };

        assert_eq!(
            key(KeyCode::Tab, KeyModifiers::NONE),
            Some(Event::Key(Key::Tab))
        );
        assert_eq!(
            key(KeyCode::Delete, KeyModifiers::NONE),
            Some(Event::Key(Key::Delete))
        );
        assert_eq!(
            key(KeyCode::Up, KeyModifiers::ALT),
            Some(Event::Key(Key::AltUp))
        );
        assert_eq!(
            key(KeyCode::Down, KeyModifiers::ALT),
            Some(Event::Key(Key::AltDown))
        );
        assert_eq!(
            key(KeyCode::Char('x'), KeyModifiers::NONE),
            Some(Event::Key(Key::Char('x')))
        );
        assert_eq!(
            key(KeyCode::Char('X'), KeyModifiers::SHIFT),
            Some(Event::Key(Key::Char('X')))
        );
        assert_eq!(
            key(KeyCode::Char('c'), KeyModifiers::CONTROL),
            Some(Event::Key(Key::CtrlC))
        );
    }

    fn assert_provider_channels_discard_stale_terminal(stale_outcome: TuiProviderOutcome) {
        let mut tui = Tui::with_queue_capacity(NoopEngine, 2);
        let (_metrics_sender, metrics_receiver) = BridgeTx::bounded(1);
        let (_progress_sender, progress_receiver) = mpsc::channel();
        let (completion_sender, completion_receiver) = mpsc::channel();

        tui.begin_submission("first");
        let first_generation = tui.scheduler.lifecycle().active().unwrap().generation();
        tui.input = "second".into();
        tui.enqueue_composer();

        completion_sender
            .send((
                Some(first_generation),
                TuiProviderOutcome::Cancelled {
                    message: "cancelled".into(),
                    action: "retry".into(),
                },
            ))
            .unwrap();
        let next = drain_provider_channels(
            &mut tui,
            &metrics_receiver,
            &progress_receiver,
            &completion_receiver,
        )
        .next_prompt
        .expect("matching cancellation releases exactly one queued prompt");
        tui.begin_submission(next.display);
        let second_generation = tui.scheduler.lifecycle().active().unwrap().generation();

        completion_sender
            .send((Some(first_generation), stale_outcome))
            .unwrap();
        let stale = drain_provider_channels(
            &mut tui,
            &metrics_receiver,
            &progress_receiver,
            &completion_receiver,
        );

        assert!(stale.next_prompt.is_none());
        assert_eq!(
            tui.scheduler.lifecycle().active().unwrap().generation(),
            second_generation
        );
        assert_eq!(
            tui.transcript(),
            [
                TranscriptEntry::User("first".into()),
                TranscriptEntry::Error("cancelled".into()),
                TranscriptEntry::User("second".into()),
            ]
        );
        assert_eq!(tui.scheduler.observability().stale_event_dropped(), 1);
    }

    #[test]
    fn provider_channels_discard_out_of_order_terminals_before_projection_or_dispatch() {
        for stale_outcome in [
            TuiProviderOutcome::Completed("stale output".into()),
            TuiProviderOutcome::Failed {
                message: "stale failure".into(),
                action: "retry".into(),
            },
            TuiProviderOutcome::Cancelled {
                message: "stale cancellation".into(),
                action: "retry".into(),
            },
        ] {
            assert_provider_channels_discard_stale_terminal(stale_outcome);
        }
    }

    #[test]
    fn detached_background_outcome_cannot_mutate_a_concurrent_foreground_turn() {
        let mut tui = Tui::with_queue_capacity(NoopEngine, 2);
        let (_metrics_sender, metrics_receiver) = BridgeTx::bounded(1);
        let (_progress_sender, progress_receiver) = mpsc::channel();
        let (completion_sender, completion_receiver) = mpsc::channel();

        tui.begin_submission("foreground");
        let generation = tui.scheduler.lifecycle().active().unwrap().generation();
        tui.input = "queued".into();
        tui.enqueue_composer();
        let transcript = tui.transcript().to_vec();

        completion_sender
            .send((None, TuiProviderOutcome::Backgrounded))
            .unwrap();
        let drain = drain_provider_channels(
            &mut tui,
            &metrics_receiver,
            &progress_receiver,
            &completion_receiver,
        );

        assert!(drain.next_prompt.is_none());
        assert_eq!(
            tui.scheduler.lifecycle().active().unwrap().generation(),
            generation
        );
        assert_eq!(tui.transcript(), transcript);
        assert!(tui.view().running);
        assert_eq!(tui.scheduler.queued_prompts(), vec!["queued"]);
        assert!(tui.take_ready_auto_turn().is_none());
    }

    #[test]
    fn later_terminals_in_the_same_drain_cannot_wipe_a_queued_next_prompt() {
        let mut tui = Tui::with_queue_capacity(NoopEngine, 2);
        let (_metrics_sender, metrics_receiver) = BridgeTx::bounded(1);
        let (_progress_sender, progress_receiver) = mpsc::channel();
        let (completion_sender, completion_receiver) = mpsc::channel();

        tui.begin_submission("active");
        let generation = tui.scheduler.lifecycle().active().unwrap().generation();
        tui.input = "queued next".into();
        tui.enqueue_composer();

        completion_sender
            .send((
                Some(generation),
                TuiProviderOutcome::Completed("done".into()),
            ))
            .unwrap();
        // A detached terminal often arrives in the same frame as the foreground
        // completion. Assigning `next_prompt = None` from it used to drop the FIFO
        // handoff after the queue entry was already dequeued.
        completion_sender
            .send((None, TuiProviderOutcome::Backgrounded))
            .unwrap();
        // A stale re-delivery of the same generation must not clear it either.
        completion_sender
            .send((
                Some(generation),
                TuiProviderOutcome::Completed("stale".into()),
            ))
            .unwrap();

        let drain = drain_provider_channels(
            &mut tui,
            &metrics_receiver,
            &progress_receiver,
            &completion_receiver,
        );

        assert_eq!(
            drain.next_prompt.map(|next| next.prompt),
            Some("queued next".into())
        );
        assert!(tui.scheduler.queued_prompts().is_empty());
        assert!(tui.view().running);
    }
}
