use std::{
    fmt,
    future::Future,
    path::{Component, Path},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use globset::{GlobBuilder, GlobMatcher};

mod permission_precedence;
mod permission_target;
pub mod redaction;
mod request_config;

pub use permission_precedence::{
    declarations_deny_every_target, prevailing_decision, prevailing_rule_decision,
};
pub use request_config::{ReasoningEffort, RequestConfig, RequestConfigError};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub role: Role,
    pub parts: Vec<MessagePart>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MessagePart {
    Text(String),
    Reasoning(String),
    ToolCall {
        id: String,
        name: String,
        input: String,
    },
    ToolResult {
        tool_call_id: String,
        content: String,
        is_error: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionMessage(Message);
impl SessionMessage {
    pub fn as_message(&self) -> &Message {
        &self.0
    }

    pub fn into_message(self) -> Message {
        self.0
    }
}

impl TryFrom<Message> for SessionMessage {
    type Error = SessionMessageError;

    fn try_from(message: Message) -> Result<Self, Self::Error> {
        if message.parts.is_empty() {
            return Err(SessionMessageError::EmptyParts);
        }

        for part in &message.parts {
            validate_session_message_part(message.role, part)?;
        }

        Ok(Self(message))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionMessageError {
    EmptyParts,
    EmptyPart,
    PartNotAllowed { role: Role },
}

fn validate_session_message_part(
    role: Role,
    part: &MessagePart,
) -> Result<(), SessionMessageError> {
    let allowed = match role {
        Role::System | Role::User => matches!(part, MessagePart::Text(_)),
        Role::Assistant => matches!(
            part,
            MessagePart::Text(_) | MessagePart::Reasoning(_) | MessagePart::ToolCall { .. }
        ),
        Role::Tool => matches!(part, MessagePart::ToolResult { .. }),
    };

    if !allowed {
        return Err(SessionMessageError::PartNotAllowed { role });
    }

    let nonempty = match part {
        MessagePart::Text(text) | MessagePart::Reasoning(text) => !text.is_empty(),
        MessagePart::ToolCall { id, name, input } => {
            !id.is_empty() && !name.is_empty() && !input.is_empty()
        }
        MessagePart::ToolResult {
            tool_call_id,
            content,
            ..
        } => !tool_call_id.is_empty() && !content.is_empty(),
    };

    nonempty.then_some(()).ok_or(SessionMessageError::EmptyPart)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletedSessionTurn {
    messages: Vec<Message>,
}

impl CompletedSessionTurn {
    pub fn new(messages: Vec<SessionMessage>) -> Result<Self, CompletedSessionTurnError> {
        let messages = messages
            .into_iter()
            .map(SessionMessage::into_message)
            .collect::<Vec<_>>();

        let user_index = usize::from(matches!(
            messages.first(),
            Some(Message {
                role: Role::System,
                ..
            })
        ));
        if messages
            .get(user_index)
            .is_none_or(|message| message.role != Role::User)
            || messages[user_index + 1..]
                .iter()
                .any(|message| !matches!(message.role, Role::Assistant | Role::Tool))
        {
            return Err(CompletedSessionTurnError::InvalidMessageOrder);
        }

        Ok(Self { messages })
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletedSessionTurnError {
    InvalidMessageOrder,
}

pub const MAX_RETRY_PROMPT_BYTES: usize = 65_536;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionAttemptStatus {
    Running,
    Completed,
    Cancelled,
    Failed,
    ProviderError,
    Interrupted,
}

impl SessionAttemptStatus {
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }

    pub const fn expected_failure_kind(self) -> Option<SessionAttemptFailureKind> {
        match self {
            Self::Running | Self::Completed => None,
            Self::Cancelled => Some(SessionAttemptFailureKind::Cancelled),
            Self::Failed => Some(SessionAttemptFailureKind::Failed),
            Self::ProviderError => Some(SessionAttemptFailureKind::ProviderError),
            Self::Interrupted => Some(SessionAttemptFailureKind::Interrupted),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionAttemptFailureKind {
    Cancelled,
    Failed,
    ProviderError,
    Interrupted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttemptKey {
    session_id: i64,
    attempt_id: i64,
}

impl AttemptKey {
    pub fn new(session_id: i64, attempt_id: i64) -> Result<Self, AttemptKeyError> {
        if session_id <= 0 {
            return Err(AttemptKeyError::InvalidSessionId);
        }

        if attempt_id <= 0 {
            return Err(AttemptKeyError::InvalidAttemptId);
        }

        Ok(Self {
            session_id,
            attempt_id,
        })
    }

    pub const fn session_id(self) -> i64 {
        self.session_id
    }

    pub const fn attempt_id(self) -> i64 {
        self.attempt_id
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttemptKeyError {
    InvalidSessionId,
    InvalidAttemptId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionAttemptSummary {
    key: AttemptKey,
    sequence: u64,
    status: SessionAttemptStatus,
    failure_kind: Option<SessionAttemptFailureKind>,
    started_at: i64,
    finished_at: Option<i64>,
}

impl SessionAttemptSummary {
    pub fn new(
        key: AttemptKey,
        sequence: u64,
        status: SessionAttemptStatus,
        failure_kind: Option<SessionAttemptFailureKind>,
        started_at: i64,
        finished_at: Option<i64>,
    ) -> Result<Self, SessionAttemptSummaryError> {
        if sequence == 0 {
            return Err(SessionAttemptSummaryError::InvalidSequence);
        }

        if status.is_terminal() != finished_at.is_some() {
            return Err(SessionAttemptSummaryError::InvalidTerminalTimestamp);
        }

        if status.expected_failure_kind() != failure_kind {
            return Err(SessionAttemptSummaryError::InvalidStatusCategory);
        }

        Ok(Self {
            key,
            sequence,
            status,
            failure_kind,
            started_at,
            finished_at,
        })
    }

    pub const fn key(&self) -> AttemptKey {
        self.key
    }

    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    pub const fn status(&self) -> SessionAttemptStatus {
        self.status
    }

    pub const fn failure_kind(&self) -> Option<SessionAttemptFailureKind> {
        self.failure_kind
    }

    pub const fn started_at(&self) -> i64 {
        self.started_at
    }

    pub const fn finished_at(&self) -> Option<i64> {
        self.finished_at
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionAttemptSummaryError {
    InvalidSequence,
    InvalidTerminalTimestamp,
    InvalidStatusCategory,
}

#[derive(Clone, PartialEq, Eq)]
pub struct RetryBoundary {
    key: AttemptKey,
    prompt: String,
}

impl RetryBoundary {
    pub fn new(key: AttemptKey, prompt: String) -> Result<Self, RetryBoundaryError> {
        if prompt.is_empty() || prompt.len() > MAX_RETRY_PROMPT_BYTES {
            return Err(RetryBoundaryError::InvalidPrompt);
        }

        Ok(Self { key, prompt })
    }

    pub const fn key(&self) -> AttemptKey {
        self.key
    }

    pub fn prompt(&self) -> &str {
        &self.prompt
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetryBoundaryError {
    InvalidPrompt,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BeginSessionAttemptError {
    AlreadyRunning(SessionAttemptSummary),
    Store,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttemptFinishOutcome {
    Finished,
    Stale,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoveryOutcome {
    Recovered(SessionAttemptSummary),
    Stale,
}

impl RecoveryOutcome {
    pub fn summary(&self) -> Option<&SessionAttemptSummary> {
        match self {
            Self::Recovered(summary) => Some(summary),
            Self::Stale => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionMetadata {
    pub id: i64,
    pub project: String,
    pub title: String,
    pub active_agent: String,
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub created_at: i64,
    pub updated_at: i64,
    pub completed_turn_count: u64,
    pub resumable: bool,
}

impl SessionMetadata {
    pub fn validate(&self) -> Result<(), SessionMetadataError> {
        if self.id <= 0 {
            return Err(SessionMetadataError::InvalidId);
        }

        if self.project.is_empty() {
            return Err(SessionMetadataError::EmptyProject);
        }

        if !is_catalog_name(&self.active_agent) {
            return Err(SessionMetadataError::InvalidActiveAgent);
        }

        if self
            .provider_id
            .as_deref()
            .is_some_and(|value| !is_catalog_name(value))
        {
            return Err(SessionMetadataError::InvalidProviderId);
        }

        if self
            .model_id
            .as_deref()
            .is_some_and(|value| !is_model_identifier(value))
        {
            return Err(SessionMetadataError::InvalidModelId);
        }

        (self.resumable == (self.completed_turn_count > 0))
            .then_some(())
            .ok_or(SessionMetadataError::InvalidResumability)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionMetadataError {
    InvalidId,
    EmptyProject,
    InvalidActiveAgent,
    InvalidProviderId,
    InvalidModelId,
    InvalidResumability,
}

/// Why a subagent turn failed. A classification the runtime makes and any
/// surface merely renders, so it lives here rather than in a terminal crate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubagentErrorKind {
    Authentication,
    Context,
    Network,
    Provider,
    Protocol,
    RateLimited,
    Rejected,
    Server,
    Tool,
    IterationLimit,
    Runtime,
}

/// How a subagent turn ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubagentStatus {
    Success,
    Failure,
    Cancelled,
}

/// Who asked for a turn. A domain fact, not a surface one: a background
/// subagent turn and a user prompt differ in what the runtime may do with them,
/// whether or not a terminal is attached.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmitOrigin {
    /// A prompt the user submitted for the main agent.
    User,
    /// A prompt the user submitted for the armed subagent to run in the background.
    Background,
    /// A turn the runtime scheduled after a background subagent finished.
    SubagentCompletion,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnState {
    Idle,
    Requesting,
    Streaming,
    Dispatching,
    Completed,
    Cancelled,
    Failed,
}

impl TurnState {
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled | Self::Failed)
    }

    pub const fn transition_to(self, target: Self) -> Result<Self, TurnTransitionError> {
        match (self, target) {
            (Self::Idle, Self::Requesting)
            | (Self::Requesting, Self::Streaming)
            | (Self::Requesting, Self::Completed)
            | (Self::Streaming, Self::Dispatching)
            | (Self::Streaming, Self::Completed)
            | (Self::Dispatching, Self::Requesting)
            | (
                Self::Requesting | Self::Streaming | Self::Dispatching,
                Self::Cancelled | Self::Failed,
            ) => Ok(target),
            _ => Err(TurnTransitionError {
                source: self,
                target,
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TurnTransitionError {
    pub source: TurnState,
    pub target: TurnState,
}

impl fmt::Display for TurnTransitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "invalid turn state transition: {:?} -> {:?}",
            self.source, self.target
        )
    }
}

impl std::error::Error for TurnTransitionError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnEvent {
    StateChanged(TurnState),
    ProviderPart(MessagePart),
    Usage(Usage),
    ToolCallRequested {
        id: String,
        name: String,
        input: String,
    },
    ToolResult(MessagePart),
    ToolResultFacts {
        identity: FactIdentity,
        facts: ToolResultFacts,
    },
    /// A transient provider failure the runtime is about to retry.
    ///
    /// It carries no failure text: the surfaces that render it are describing
    /// what the turn is doing right now, not reporting an error the user has
    /// to act on. A turn that exhausts its retries reports the failure through
    /// the ordinary terminal path.
    ProviderRetry {
        attempt: u8,
        max_attempts: Option<u8>,
        delay: Option<Duration>,
        reason: TurnRetryReason,
    },
}

/// Why a turn is waiting before it tries the provider again.
///
/// Deliberately coarser than the provider's own diagnostic classes: these are
/// the distinctions a reader of the status line can act on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnRetryReason {
    RateLimited,
    ServerError,
    Network,
    Timeout,
    Transient,
}

/// Typed decomposition of a tool call's raw argument payload.
///
/// Native tool kinds carry their authoritative field (path, pattern, command,
/// url, or skill name) so adapters can render or reason about a call without
/// parsing JSON. Unknown and MCP tools degrade to `Other`, preserving the raw
/// payload for audit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolInput {
    Read {
        path: String,
    },
    Write {
        path: String,
    },
    Edit {
        path: String,
    },
    List {
        path: String,
    },
    Search {
        path: String,
    },
    Glob {
        pattern: String,
        path: Option<String>,
    },
    Grep {
        pattern: String,
        path: Option<String>,
    },
    Bash {
        command: String,
    },
    WebFetch {
        url: String,
    },
    Skill {
        skill: String,
    },
    Other {
        name: String,
        raw: String,
    },
}

/// Identity of a single reported tool-result facts event, sufficient to order
/// facts within a session and attribute them to the attempt that produced them.
///
/// `tool_call_id` alone is unique only within one provider call: it identifies
/// neither the run nor the attempt, so a late fact from an abandoned attempt is
/// indistinguishable from one belonging to the live attempt without the rest of
/// this key. The total order within a session is `(session_id, attempt_id,
/// sequence)`. `session_id` and `attempt_id` are `None` for turns that run
/// outside a session attempt, such as subagent child turns; `dispatch_id` is
/// reserved for the gRPC facade and is currently always `None`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct FactIdentity {
    pub tool_call_id: String,
    pub session_id: Option<i64>,
    pub attempt_id: Option<i64>,
    pub sequence: u64,
    pub dispatch_id: Option<u64>,
}

/// A path a tool reported, under the contract that every consumer comparing
/// touched paths against a declared scope relies on: relative to the session
/// root, normalized, and never absolute. An absolute path would carry no
/// information about where the session root itself lives, silently breaking
/// any comparison made against a scope declared in session-relative terms.
///
/// Construction is the only door, and it is total: a value that violates the
/// contract (absolute, traversing outside the root, empty, containing a
/// control character, or longer than [`FactPath::MAX_BYTES`]) is retained as
/// unrepresentable rather than dropped or silently corrected, so the call
/// that produced it stays visible while the pathological string itself never
/// becomes readable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FactPath {
    value: Option<String>,
}

impl FactPath {
    /// Reported paths longer than this are retained as unrepresentable rather
    /// than truncated, since a truncated path could resolve to a different
    /// location than the one actually touched.
    pub const MAX_BYTES: usize = 1024;

    pub fn new(path: &str) -> Self {
        let is_well_formed = !path.is_empty()
            && path.len() <= Self::MAX_BYTES
            && !path.chars().any(|character| character.is_control())
            && !Path::new(path).is_absolute()
            && !Path::new(path)
                .components()
                .any(|component| matches!(component, Component::ParentDir));

        Self {
            value: is_well_formed.then(|| Self::normalize(path)),
        }
    }

    /// Collapses every spelling `Component`s treats as equivalent to a single
    /// canonical form, so two reports of the same file never compare unequal.
    /// `Component::CurDir` (a leading, embedded, or trailing `.`) is dropped
    /// entirely, and repeated or trailing separators are collapsed by
    /// rejoining the remaining `Normal` components with `/`. A path made up
    /// entirely of `Component::CurDir` (`.`, `./`, `././`, ...) names the
    /// session root itself rather than any file; it is kept verbatim rather
    /// than collapsed to an empty string, since an empty path is a distinct,
    /// already-rejected case.
    fn normalize(path: &str) -> String {
        let joined = Path::new(path)
            .components()
            .filter_map(|component| match component {
                Component::Normal(part) => Some(part.to_string_lossy()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("/");

        if joined.is_empty() {
            path.to_owned()
        } else {
            joined
        }
    }

    pub fn relative(&self) -> Option<&str> {
        self.value.as_deref()
    }

    pub const fn is_representable(&self) -> bool {
        self.value.is_some()
    }
}

/// The outcome the harness itself recorded for a call: the same distinction
/// it already places in `ToolOutput::is_error`, plus the pre-execution
/// denial case in which no tool ran and there is nothing to measure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ToolOutcome {
    Succeeded,
    Failed,
    Denied,
}

/// The size of a completed `write`, reported so it is comparable to an
/// `edit`'s line counts. Without `is_new_file` and `lines_written`, a
/// full-file replacement would read only as "N bytes written" beside a
/// three-line edit, with no way to tell the two magnitudes apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WriteMagnitude {
    pub is_new_file: bool,
    pub bytes_written: usize,
    pub lines_written: usize,
}

/// The size of a completed `edit`, taken from the same diff computation the
/// tool already rendered as its result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EditMagnitude {
    pub lines_added: usize,
    pub lines_removed: usize,
}

/// Typed values a native tool reports about a call it has just completed.
///
/// Each variant carries only data the tool already produced while executing;
/// nothing here is derived, scored, or interpreted. Absence of facts means the
/// harness reported none for that call, not that the call was uneventful. A
/// magnitude is `None` exactly when the tool did not run to completion, since
/// a failed or denied call has no content to measure.
///
/// Marked `#[non_exhaustive]` because further variants are expected as more
/// tools report facts, and downstream crates already match on this enum with
/// a wildcard arm.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ToolResultFacts {
    Write {
        path: FactPath,
        outcome: ToolOutcome,
        written: Option<WriteMagnitude>,
    },
    Edit {
        path: FactPath,
        outcome: ToolOutcome,
        changed: Option<EditMagnitude>,
    },
    /// Paths touched by a command are out of scope for this passive layer:
    /// the harness observes only the process's own exit status, not the
    /// filesystem effects of what it ran, so a `bash` call that mutates
    /// files is invisible here. Those paths are re-derived from git at the
    /// delivery gate instead of being reported by this variant.
    Bash {
        outcome: ToolOutcome,
        exit_code: Option<i32>,
    },
    /// A read carries no size or content hash: the harness only needs to
    /// establish that a path was read, not how much of it was returned.
    /// Only the success path is reported; a failed or denied read is
    /// invisible via `ToolResult` and never reaches this variant.
    Read {
        path: FactPath,
        outcome: ToolOutcome,
    },
    /// Neither `search` nor `grep` reports which paths matched, only how
    /// many results were found. `truncated` is `true` exactly when the
    /// result limit cut the output short; `match_count` is always the
    /// count before any truncation marker was appended to the rendered
    /// output, so the two never drift against each other.
    Search {
        outcome: ToolOutcome,
        match_count: usize,
        truncated: bool,
    },
}

const MAX_RETAINED_TOOL_RESULT_BYTES: usize = 64 * 1024;

fn bound_retained_tool_result(content: String) -> String {
    if content.len() <= MAX_RETAINED_TOOL_RESULT_BYTES {
        return content;
    }

    let mut retained_end = MAX_RETAINED_TOOL_RESULT_BYTES;
    loop {
        while !content.is_char_boundary(retained_end) {
            retained_end -= 1;
        }

        let omitted_bytes = content.len() - retained_end;
        let marker = format!("\n… [runtime truncated; {omitted_bytes} bytes omitted]");
        if retained_end + marker.len() <= MAX_RETAINED_TOOL_RESULT_BYTES {
            return format!("{}{marker}", &content[..retained_end]);
        }

        retained_end -= 1;
    }
}

/// Observed provider usage values. Unavailable upstream or model metadata remains absent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub context_window: Option<u64>,
}

/// Optional observational output for interactive surfaces. It never affects turn results.
pub type TurnProgressSink = Arc<dyn Fn(TurnEvent) + Send + Sync>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnEventError {
    Transition(TurnTransitionError),
    InvalidProviderPart,
    DuplicateToolCallId { id: String },
    UnexpectedToolResult { tool_call_id: String },
}

impl fmt::Display for TurnEventError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transition(error) => error.fmt(formatter),
            Self::InvalidProviderPart => formatter.write_str("provider cannot emit a tool result"),
            Self::DuplicateToolCallId { id } => write!(formatter, "duplicate tool call id: {id}"),
            Self::UnexpectedToolResult { tool_call_id } => {
                write!(formatter, "unexpected tool result: {tool_call_id}")
            }
        }
    }
}

impl std::error::Error for TurnEventError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletedTurnSnapshot {
    events: Vec<TurnEvent>,
}

impl CompletedTurnSnapshot {
    pub fn events(&self) -> &[TurnEvent] {
        &self.events
    }

    pub fn from_persisted_events(
        events: Vec<TurnEvent>,
    ) -> Result<Self, CompletedTurnSnapshotError> {
        validate_completed_turn_events(&events)?;

        Ok(Self { events })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletedTurnSnapshotError {
    message: String,
}

impl CompletedTurnSnapshotError {
    fn invalid() -> Self {
        Self {
            message: "invalid persisted completed turn events".into(),
        }
    }
}

impl fmt::Display for CompletedTurnSnapshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CompletedTurnSnapshotError {}

fn validate_completed_turn_events(events: &[TurnEvent]) -> Result<(), CompletedTurnSnapshotError> {
    let mut coordinator = TurnCoordinator::new();
    let mut event_index = 0;

    consume_generated_events(&mut coordinator, events, &mut event_index, |coordinator| {
        coordinator.begin()
    })?;

    while event_index < events.len() {
        match coordinator.state() {
            TurnState::Requesting => {
                let Some(TurnEvent::StateChanged(TurnState::Streaming)) = events.get(event_index)
                else {
                    return Err(CompletedTurnSnapshotError::invalid());
                };
                let Some(TurnEvent::ProviderPart(part)) = events.get(event_index + 1) else {
                    return Err(CompletedTurnSnapshotError::invalid());
                };
                let part = part.clone();

                consume_generated_events(
                    &mut coordinator,
                    events,
                    &mut event_index,
                    move |coordinator| coordinator.accept_provider_part(part),
                )?;
            }
            TurnState::Streaming => match events.get(event_index) {
                Some(TurnEvent::ProviderPart(part)) => {
                    let part = part.clone();

                    consume_generated_events(
                        &mut coordinator,
                        events,
                        &mut event_index,
                        move |coordinator| coordinator.accept_provider_part(part),
                    )?;
                }
                Some(TurnEvent::StateChanged(TurnState::Dispatching | TurnState::Completed)) => {
                    consume_generated_events(
                        &mut coordinator,
                        events,
                        &mut event_index,
                        TurnCoordinator::finish_provider_iteration,
                    )?;
                }
                _ => return Err(CompletedTurnSnapshotError::invalid()),
            },
            TurnState::Dispatching => {
                let Some(TurnEvent::ToolResult(MessagePart::ToolResult {
                    tool_call_id,
                    content,
                    is_error,
                })) = events.get(event_index)
                else {
                    return Err(CompletedTurnSnapshotError::invalid());
                };
                let tool_call_id = tool_call_id.clone();
                let content = content.clone();
                let is_error = *is_error;

                consume_generated_events(
                    &mut coordinator,
                    events,
                    &mut event_index,
                    move |coordinator| {
                        coordinator.accept_tool_result(&tool_call_id, content, is_error, None)
                    },
                )?;
            }
            TurnState::Completed => break,
            TurnState::Idle | TurnState::Cancelled | TurnState::Failed => {
                return Err(CompletedTurnSnapshotError::invalid());
            }
        }
    }

    (coordinator.state() == TurnState::Completed && event_index == events.len())
        .then_some(())
        .ok_or_else(CompletedTurnSnapshotError::invalid)
}

/// Events the coordinator emits for live observation only. They never enter
/// persisted history and are never replayed, so the completed-turn validator
/// must not see them.
const fn is_live_only_event(event: &TurnEvent) -> bool {
    matches!(event, TurnEvent::ToolResultFacts { .. })
}

fn persisted_history(events: &[TurnEvent]) -> Vec<TurnEvent> {
    events
        .iter()
        .filter(|event| !is_live_only_event(event))
        .cloned()
        .collect()
}

fn consume_generated_events(
    coordinator: &mut TurnCoordinator,
    persisted_events: &[TurnEvent],
    event_index: &mut usize,
    operation: impl FnOnce(&mut TurnCoordinator) -> Result<(), TurnEventError>,
) -> Result<(), CompletedTurnSnapshotError> {
    let generated_start = coordinator.events.len();
    operation(coordinator).map_err(|_| CompletedTurnSnapshotError::invalid())?;
    let generated_events = &coordinator.events[generated_start..];
    let persisted_end = event_index.saturating_add(generated_events.len());

    if persisted_events.get(*event_index..persisted_end) != Some(generated_events) {
        return Err(CompletedTurnSnapshotError::invalid());
    }

    *event_index = persisted_end;
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletedTurnStoreError {
    message: String,
}

impl CompletedTurnStoreError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for CompletedTurnStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CompletedTurnStoreError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompletedTurnPersistenceError {
    NotCompleted { state: TurnState },
    AlreadyPersisted,
    AlreadyAttempted,
    Store(CompletedTurnStoreError),
}

impl fmt::Display for CompletedTurnPersistenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotCompleted { state } => {
                write!(
                    formatter,
                    "cannot persist incomplete turn in state: {state:?}"
                )
            }
            Self::AlreadyPersisted => formatter.write_str("completed turn already persisted"),
            Self::AlreadyAttempted => {
                formatter.write_str("completed turn persistence already attempted")
            }
            Self::Store(error) => write!(formatter, "store: {error}"),
        }
    }
}

impl std::error::Error for CompletedTurnPersistenceError {}

pub trait CompletedTurnRepository {
    fn persist_completed_turn(
        &mut self,
        snapshot: CompletedTurnSnapshot,
    ) -> impl Future<Output = Result<(), CompletedTurnStoreError>> + Send;
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingToolCall {
    id: String,
    name: String,
    input: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnCoordinator {
    state: TurnState,
    events: Vec<TurnEvent>,
    pending_tool_calls: Vec<PendingToolCall>,
    completed_turn_persisted: bool,
    completed_turn_persistence_attempted: bool,
    attempt_key: Option<AttemptKey>,
    fact_sequence: u64,
}

impl Default for TurnCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

impl TurnCoordinator {
    pub const fn new() -> Self {
        Self {
            state: TurnState::Idle,
            events: Vec::new(),
            pending_tool_calls: Vec::new(),
            completed_turn_persisted: false,
            completed_turn_persistence_attempted: false,
            attempt_key: None,
            fact_sequence: 0,
        }
    }

    /// Builds a coordinator that knows the session attempt it belongs to, so
    /// every fact it emits carries that attempt's `session_id`/`attempt_id`.
    /// Callers with no attempt of their own (replay, subagent child turns)
    /// use `new` instead, which leaves those ids `None`.
    pub const fn for_attempt(key: AttemptKey) -> Self {
        Self {
            state: TurnState::Idle,
            events: Vec::new(),
            pending_tool_calls: Vec::new(),
            completed_turn_persisted: false,
            completed_turn_persistence_attempted: false,
            attempt_key: Some(key),
            fact_sequence: 0,
        }
    }

    pub const fn state(&self) -> TurnState {
        self.state
    }

    pub fn events(&self) -> &[TurnEvent] {
        &self.events
    }

    pub const fn has_persisted_completed_turn(&self) -> bool {
        self.completed_turn_persisted
    }

    pub async fn persist_completed_turn(
        &mut self,
        repository: &mut impl CompletedTurnRepository,
    ) -> Result<(), CompletedTurnPersistenceError> {
        if self.state != TurnState::Completed {
            return Err(CompletedTurnPersistenceError::NotCompleted { state: self.state });
        }

        if self.completed_turn_persisted {
            return Err(CompletedTurnPersistenceError::AlreadyPersisted);
        }

        if self.completed_turn_persistence_attempted {
            return Err(CompletedTurnPersistenceError::AlreadyAttempted);
        }

        let snapshot = CompletedTurnSnapshot {
            events: persisted_history(&self.events),
        };

        self.completed_turn_persistence_attempted = true;

        repository
            .persist_completed_turn(snapshot)
            .await
            .map_err(CompletedTurnPersistenceError::Store)?;

        // Mark success only after the repository has durably accepted the snapshot.
        self.completed_turn_persisted = true;
        Ok(())
    }

    pub fn begin(&mut self) -> Result<(), TurnEventError> {
        self.transition_to(TurnState::Requesting)
    }

    pub fn accept_provider_part(&mut self, part: MessagePart) -> Result<(), TurnEventError> {
        self.validate_provider_part(&part)?;

        if self.state == TurnState::Requesting {
            self.transition_to(TurnState::Streaming)?;
        }

        if let MessagePart::ToolCall { id, name, input } = &part {
            self.pending_tool_calls.push(PendingToolCall {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            });
        }

        self.events.push(TurnEvent::ProviderPart(part));
        Ok(())
    }

    pub fn finish_provider_iteration(&mut self) -> Result<(), TurnEventError> {
        self.require_state(TurnState::Streaming)?;

        if self.pending_tool_calls.is_empty() {
            return self.transition_to(TurnState::Completed);
        }

        self.transition_to(TurnState::Dispatching)?;

        for call in &self.pending_tool_calls {
            self.events.push(TurnEvent::ToolCallRequested {
                id: call.id.clone(),
                name: call.name.clone(),
                input: call.input.clone(),
            });
        }

        Ok(())
    }

    /// Records a tool result and, when the harness reported them, the typed
    /// facts for that call. `facts` is `Option` rather than defaulted so every
    /// caller states its choice explicitly, including replay, which always
    /// passes `None`. The facts event is live-only: it is excluded from
    /// persisted history and must never be regenerated during replay.
    pub fn accept_tool_result(
        &mut self,
        tool_call_id: &str,
        content: String,
        is_error: bool,
        facts: Option<ToolResultFacts>,
    ) -> Result<(), TurnEventError> {
        if self.state != TurnState::Dispatching {
            return Err(TurnEventError::UnexpectedToolResult {
                tool_call_id: tool_call_id.into(),
            });
        }

        let Some(index) = self
            .pending_tool_calls
            .iter()
            .position(|call| call.id == tool_call_id)
        else {
            return Err(TurnEventError::UnexpectedToolResult {
                tool_call_id: tool_call_id.into(),
            });
        };

        self.pending_tool_calls.remove(index);
        self.events
            .push(TurnEvent::ToolResult(MessagePart::ToolResult {
                tool_call_id: tool_call_id.into(),
                content: bound_retained_tool_result(content),
                is_error,
            }));

        if let Some(facts) = facts {
            self.fact_sequence = self.fact_sequence.saturating_add(1);
            self.events.push(TurnEvent::ToolResultFacts {
                identity: FactIdentity {
                    tool_call_id: tool_call_id.into(),
                    session_id: self.attempt_key.map(AttemptKey::session_id),
                    attempt_id: self.attempt_key.map(AttemptKey::attempt_id),
                    sequence: self.fact_sequence,
                    dispatch_id: None,
                },
                facts,
            });
        }

        if self.pending_tool_calls.is_empty() {
            self.transition_to(TurnState::Requesting)?;
        }

        Ok(())
    }

    pub fn cancel(&mut self) -> Result<(), TurnEventError> {
        self.transition_to(TurnState::Cancelled)
    }

    pub fn fail(&mut self) -> Result<(), TurnEventError> {
        self.transition_to(TurnState::Failed)
    }

    fn require_state(&self, target: TurnState) -> Result<(), TurnEventError> {
        if self.state == target {
            return Ok(());
        }

        Err(TurnEventError::Transition(TurnTransitionError {
            source: self.state,
            target,
        }))
    }

    fn validate_provider_part(&self, part: &MessagePart) -> Result<(), TurnEventError> {
        if !matches!(self.state, TurnState::Requesting | TurnState::Streaming) {
            return self.require_state(TurnState::Streaming);
        }

        if matches!(part, MessagePart::ToolResult { .. }) {
            return Err(TurnEventError::InvalidProviderPart);
        }

        if let MessagePart::ToolCall { id, .. } = part
            && self.pending_tool_calls.iter().any(|call| call.id == *id)
        {
            return Err(TurnEventError::DuplicateToolCallId { id: id.clone() });
        }

        Ok(())
    }

    fn transition_to(&mut self, target: TurnState) -> Result<(), TurnEventError> {
        self.state = self
            .state
            .transition_to(target)
            .map_err(TurnEventError::Transition)?;
        self.events.push(TurnEvent::StateChanged(self.state));
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeadlessToolCall {
    pub id: String,
    pub name: String,
    pub input: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeadlessToolOutput {
    pub content: String,
    pub is_error: bool,
    pub facts: Option<ToolResultFacts>,
}

/// Sanitized terminal outcome emitted by the built-in synchronous task tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeadlessTaskTerminal {
    Cancelled,
    TimedOut,
    AgentUnavailable,
    ModelUnavailable,
    SkillUnavailable,
    IterationLimit,
    InputLimit,
    OutputLimit,
    ConcurrencyLimit,
    ProviderFailure,
    ChildFailure,
    /// The delegated agent's own `permissions:` declarations could not be
    /// resolved into a tool surface, so no child turn started. Distinct from
    /// [`Self::ChildFailure`] because the operator can fix it by editing the
    /// agent definition, which an opaque runtime failure never tells them.
    DeclarationRejected,
}

impl HeadlessTaskTerminal {
    pub const fn message(self) -> &'static str {
        match self {
            Self::Cancelled => "task: cancelled",
            Self::TimedOut => "task: timed out",
            Self::AgentUnavailable => "task: requested agent is unavailable",
            Self::ModelUnavailable => "task: requested model is unavailable",
            Self::SkillUnavailable => "task: requested skill is unavailable",
            Self::IterationLimit => "task: iteration limit reached",
            Self::InputLimit => "task: input exceeds configured bounds",
            Self::OutputLimit => "task: output exceeds configured bounds",
            Self::ConcurrencyLimit => "task: concurrent child limit reached",
            Self::ProviderFailure => "task: provider failure",
            Self::ChildFailure => "task: child execution failed",
            Self::DeclarationRejected => "task: agent permission declaration rejected",
        }
    }
}

/// Why a subagent's provider call ended, as the parent is allowed to read it.
///
/// [`HeadlessTaskTerminal::ProviderFailure`] is one closed message covering
/// eight distinct causes -- an expired token, an oversized request, a stalled
/// connection -- and a parent that cannot tell them apart cannot choose between
/// retrying, shrinking the request, and giving up. The labels are fixed strings
/// so the sanitizer that guards model-visible output can keep verifying the
/// whole message against a closed set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskProviderFailure {
    Authentication,
    Context,
    Network,
    Protocol,
    RateLimited,
    Rejected,
    Server,
}

impl TaskProviderFailure {
    pub const ALL: [Self; 7] = [
        Self::Authentication,
        Self::Context,
        Self::Network,
        Self::Protocol,
        Self::RateLimited,
        Self::Rejected,
        Self::Server,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Authentication => "authentication",
            Self::Context => "context length",
            Self::Network => "network",
            Self::Protocol => "response protocol",
            Self::RateLimited => "rate limited",
            Self::Rejected => "request rejected",
            Self::Server => "provider server",
        }
    }
}

/// Why a preloaded skill could not reach a subagent, as a fixed token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskSkillRejection {
    Undeclared,
    Unknown,
    Unreadable,
}

impl TaskSkillRejection {
    pub const ALL: [Self; 3] = [Self::Undeclared, Self::Unknown, Self::Unreadable];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Undeclared => "not declared by the agent",
            Self::Unknown => "not in the skill catalog",
            Self::Unreadable => "instructions could not be read",
        }
    }
}

impl HeadlessToolOutput {
    pub fn success(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            facts: None,
        }
    }

    pub fn failure(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            facts: None,
        }
    }

    #[must_use]
    pub fn with_facts(mut self, facts: ToolResultFacts) -> Self {
        self.facts = Some(facts);
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeadlessTurnPortError {
    Cancelled,
    TimedOut,
    Authentication,
    Provider,
    ProviderRejected,
    ProviderContext,
    ProviderRateLimited,
    ProviderServer,
    ProviderNetwork,
    ProviderProtocol,
    Permission,
    Tool,
    TaskTerminal(HeadlessTaskTerminal),
}

pub trait TurnProvider {
    fn queue_user_messages(&mut self, messages: Vec<Message>) -> Result<(), HeadlessTurnPortError> {
        if messages.is_empty() {
            Ok(())
        } else {
            Err(HeadlessTurnPortError::Provider)
        }
    }

    fn next_parts(
        &mut self,
        events: &[TurnEvent],
        cancellation: &HeadlessTurnCancellation,
    ) -> impl Future<Output = Result<Vec<MessagePart>, HeadlessTurnPortError>> + Send;
}

pub trait HeadlessPermissionGate {
    fn evaluate(
        &mut self,
        call: &HeadlessToolCall,
        cancellation: &HeadlessTurnCancellation,
    ) -> impl Future<Output = Result<PermissionDecision, HeadlessTurnPortError>> + Send;

    /// Facts for a call this gate denied before it ran.
    ///
    /// A denial short-circuits before any tool executes, so this is the only
    /// place the harness can still report the path a denied write or edit
    /// targeted. The default reports none: most gates in this codebase are
    /// test doubles with no route from a raw call to a typed input, and a
    /// gate that cannot parse `call.input` has nothing honest to report.
    fn denial_facts(&self, _call: &HeadlessToolCall) -> Option<ToolResultFacts> {
        None
    }
}

pub trait HeadlessPermissionResolver {
    fn resolve(
        &mut self,
        call: &HeadlessToolCall,
        cancellation: &HeadlessTurnCancellation,
    ) -> impl Future<Output = Result<PermissionDecision, HeadlessTurnPortError>> + Send;
}

pub trait HeadlessToolDispatcher {
    fn dispatch(
        &mut self,
        call: HeadlessToolCall,
        cancellation: &HeadlessTurnCancellation,
    ) -> impl Future<Output = Result<HeadlessToolOutput, HeadlessTurnPortError>> + Send;
}

#[derive(Clone, Debug, Default)]
pub struct HeadlessTurnCancellation {
    cancelled: Arc<AtomicBool>,
    deadline: Option<Instant>,
}

#[derive(Clone, Debug)]
pub struct HeadlessTurnCancellationAdapter {
    cancelled: Arc<AtomicBool>,
    deadline: Option<Instant>,
}

impl HeadlessTurnCancellationAdapter {
    pub const fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub fn remaining_duration(&self) -> Option<Duration> {
        self.deadline.map(|deadline| {
            deadline
                .checked_duration_since(Instant::now())
                .unwrap_or(Duration::ZERO)
        })
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub fn cancellation_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancelled)
    }
}

impl HeadlessTurnCancellation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_deadline(timeout: Duration) -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            deadline: Some(Instant::now() + timeout),
        }
    }

    pub fn with_cancellation_and_deadline(
        cancelled: Arc<AtomicBool>,
        deadline: Option<Instant>,
    ) -> Self {
        Self {
            cancelled,
            deadline,
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub fn is_expired(&self) -> bool {
        self.deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
    }

    pub fn adapter_view(&self) -> HeadlessTurnCancellationAdapter {
        HeadlessTurnCancellationAdapter {
            cancelled: Arc::clone(&self.cancelled),
            deadline: self.deadline,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeadlessTurnError {
    Cancelled,
    TimedOut,
    Authentication,
    Provider,
    ProviderRejected,
    ProviderContext,
    ProviderRateLimited,
    ProviderServer,
    ProviderNetwork,
    ProviderProtocol,
    Permission,
    PermissionEvaluation,
    PermissionRequired,
    Tool,
    Store,
    MaxIterations,
    State,
    TaskTerminal(HeadlessTaskTerminal),
}

impl fmt::Display for HeadlessTurnError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Cancelled => "turn cancelled",
            Self::TimedOut => "turn timed out",
            Self::Authentication => "authentication required",
            Self::Provider => "provider operation failed",
            Self::ProviderRejected => "provider rejected the request",
            Self::ProviderContext => "provider rejected the request because it exceeds context",
            Self::ProviderRateLimited => "provider rate limited the request",
            Self::ProviderServer => "provider service failed",
            Self::ProviderNetwork => "provider network request failed",
            Self::ProviderProtocol => "provider response protocol failed",
            Self::Permission => "permission operation failed",
            Self::PermissionEvaluation => "permission evaluation failed",
            Self::PermissionRequired => "permission required",
            Self::Tool => "tool operation failed",
            Self::Store => "completed turn could not be saved",
            Self::MaxIterations => "turn reached the maximum iterations",
            Self::State => "invalid headless turn state",
            Self::TaskTerminal(terminal) => terminal.message(),
        };

        formatter.write_str(message)
    }
}

impl std::error::Error for HeadlessTurnError {}

pub async fn run_headless_turn(
    provider: &mut impl TurnProvider,
    permission_gate: &mut impl HeadlessPermissionGate,
    permission_resolver: &mut impl HeadlessPermissionResolver,
    dispatcher: &mut impl HeadlessToolDispatcher,
    repository: &mut impl CompletedTurnRepository,
    cancellation: &HeadlessTurnCancellation,
) -> Result<CompletedTurnSnapshot, HeadlessTurnError> {
    run_headless_turn_with_progress(
        provider,
        permission_gate,
        permission_resolver,
        dispatcher,
        repository,
        cancellation,
        None,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn run_headless_turn_with_progress(
    provider: &mut impl TurnProvider,
    permission_gate: &mut impl HeadlessPermissionGate,
    permission_resolver: &mut impl HeadlessPermissionResolver,
    dispatcher: &mut impl HeadlessToolDispatcher,
    repository: &mut impl CompletedTurnRepository,
    cancellation: &HeadlessTurnCancellation,
    progress: Option<&TurnProgressSink>,
    attempt: Option<AttemptKey>,
) -> Result<CompletedTurnSnapshot, HeadlessTurnError> {
    run_headless_turn_with_iteration_limit(
        provider,
        permission_gate,
        permission_resolver,
        dispatcher,
        repository,
        cancellation,
        None,
        progress,
        attempt,
        AskUnreachable::PromptIsReachable,
    )
    .await
}

pub async fn run_headless_turn_with_max_iterations(
    provider: &mut impl TurnProvider,
    permission_gate: &mut impl HeadlessPermissionGate,
    permission_resolver: &mut impl HeadlessPermissionResolver,
    dispatcher: &mut impl HeadlessToolDispatcher,
    repository: &mut impl CompletedTurnRepository,
    cancellation: &HeadlessTurnCancellation,
    max_iterations: usize,
) -> Result<CompletedTurnSnapshot, HeadlessTurnError> {
    run_headless_turn_with_max_iterations_and_progress(
        provider,
        permission_gate,
        permission_resolver,
        dispatcher,
        repository,
        cancellation,
        max_iterations,
        None,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn run_headless_turn_with_max_iterations_and_progress(
    provider: &mut impl TurnProvider,
    permission_gate: &mut impl HeadlessPermissionGate,
    permission_resolver: &mut impl HeadlessPermissionResolver,
    dispatcher: &mut impl HeadlessToolDispatcher,
    repository: &mut impl CompletedTurnRepository,
    cancellation: &HeadlessTurnCancellation,
    max_iterations: usize,
    progress: Option<&TurnProgressSink>,
    attempt: Option<AttemptKey>,
) -> Result<CompletedTurnSnapshot, HeadlessTurnError> {
    run_headless_turn_with_iteration_limit(
        provider,
        permission_gate,
        permission_resolver,
        dispatcher,
        repository,
        cancellation,
        Some(max_iterations),
        progress,
        attempt,
        AskUnreachable::PromptIsReachable,
    )
    .await
}

/// Runs an isolated child turn, where no human can be reached to answer a
/// permission prompt: any tool call that resolves to `Ask` is answered by
/// the child's own [`HeadlessPermissionResolver`], which denies it without
/// ever surfacing a prompt. The rendered denial states plainly that the
/// prompt could not be reached, so it is never mistaken for a policy
/// `deny`.
#[allow(clippy::too_many_arguments)]
pub async fn run_isolated_headless_turn_with_max_iterations_and_progress(
    provider: &mut impl TurnProvider,
    permission_gate: &mut impl HeadlessPermissionGate,
    permission_resolver: &mut impl HeadlessPermissionResolver,
    dispatcher: &mut impl HeadlessToolDispatcher,
    repository: &mut impl CompletedTurnRepository,
    cancellation: &HeadlessTurnCancellation,
    max_iterations: usize,
    progress: Option<&TurnProgressSink>,
    attempt: Option<AttemptKey>,
) -> Result<CompletedTurnSnapshot, HeadlessTurnError> {
    run_headless_turn_with_iteration_limit(
        provider,
        permission_gate,
        permission_resolver,
        dispatcher,
        repository,
        cancellation,
        Some(max_iterations),
        progress,
        attempt,
        AskUnreachable::PromptIsUnreachable,
    )
    .await
}

/// Whether a call resolving to [`PermissionDecision::Ask`] can actually
/// reach a human. An isolated child turn has no such channel: its
/// [`HeadlessPermissionResolver`] denies every `Ask` unconditionally, so a
/// resulting `Deny` needs its own wording rather than the generic policy
/// denial message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AskUnreachable {
    PromptIsReachable,
    PromptIsUnreachable,
}

/// What a tool call's preflight permission step decided, ahead of the
/// second pass that actually dispatches or fails it. A call that resolved
/// `Ask` inside an isolated child turn is tracked separately from a plain
/// `Deny`, so the two can be rendered with different wording.
enum PreflightAuthorization {
    InvalidArguments,
    Decided(PermissionDecision),
    UnreachablePromptDenial,
}

#[allow(clippy::too_many_arguments)]
async fn run_headless_turn_with_iteration_limit(
    provider: &mut impl TurnProvider,
    permission_gate: &mut impl HeadlessPermissionGate,
    permission_resolver: &mut impl HeadlessPermissionResolver,
    dispatcher: &mut impl HeadlessToolDispatcher,
    repository: &mut impl CompletedTurnRepository,
    cancellation: &HeadlessTurnCancellation,
    max_iterations: Option<usize>,
    progress: Option<&TurnProgressSink>,
    attempt: Option<AttemptKey>,
    ask_unreachable: AskUnreachable,
) -> Result<CompletedTurnSnapshot, HeadlessTurnError> {
    let mut coordinator = match attempt {
        Some(key) => TurnCoordinator::for_attempt(key),
        None => TurnCoordinator::new(),
    };
    coordinator.begin().map_err(|_| HeadlessTurnError::State)?;
    let mut progress_cursor = 0;
    flush_progress(&coordinator, progress, &mut progress_cursor);
    let mut iterations = 0;

    loop {
        check_cancelled(&mut coordinator, cancellation)?;
        flush_progress(&coordinator, progress, &mut progress_cursor);
        if max_iterations.is_some_and(|limit| iterations >= limit) {
            coordinator.fail().map_err(|_| HeadlessTurnError::State)?;
            flush_progress(&coordinator, progress, &mut progress_cursor);
            return Err(HeadlessTurnError::MaxIterations);
        }
        iterations += 1;

        let parts = provider
            .next_parts(coordinator.events(), cancellation)
            .await
            .map_err(|error| {
                finish_port_error(&mut coordinator, error, HeadlessTurnError::Provider)
            })?;
        check_cancelled(&mut coordinator, cancellation)?;
        let tool_calls = parts
            .iter()
            .filter_map(headless_tool_call)
            .collect::<Vec<_>>();

        for part in parts {
            coordinator
                .accept_provider_part(part)
                .map_err(|_| fail_state(&mut coordinator))?;
        }
        flush_progress(&coordinator, progress, &mut progress_cursor);

        coordinator
            .finish_provider_iteration()
            .map_err(|_| fail_state(&mut coordinator))?;
        flush_progress(&coordinator, progress, &mut progress_cursor);

        if coordinator.state() == TurnState::Completed {
            coordinator
                .persist_completed_turn(repository)
                .await
                .map_err(|_| HeadlessTurnError::Store)?;

            return CompletedTurnSnapshot::from_persisted_events(persisted_history(
                coordinator.events(),
            ))
            .map_err(|_| HeadlessTurnError::State);
        }

        let mut preflight = Vec::with_capacity(tool_calls.len());

        for call in tool_calls {
            check_cancelled(&mut coordinator, cancellation)?;
            flush_progress(&coordinator, progress, &mut progress_cursor);

            let decision = match permission_gate.evaluate(&call, cancellation).await {
                Ok(decision) => decision,
                Err(HeadlessTurnPortError::Tool) => {
                    preflight.push((call, PreflightAuthorization::InvalidArguments));
                    continue;
                }
                Err(error) => {
                    return Err(finish_port_error(
                        &mut coordinator,
                        error,
                        HeadlessTurnError::PermissionEvaluation,
                    ));
                }
            };
            let asked = decision == PermissionDecision::Ask;
            check_cancelled(&mut coordinator, cancellation)?;
            let decision = resolve_permission_decision(
                decision,
                &call,
                permission_resolver,
                &mut coordinator,
                cancellation,
            )
            .await?;
            check_cancelled(&mut coordinator, cancellation)?;

            let authorization = if asked
                && decision == PermissionDecision::Deny
                && ask_unreachable == AskUnreachable::PromptIsUnreachable
            {
                PreflightAuthorization::UnreachablePromptDenial
            } else {
                PreflightAuthorization::Decided(decision)
            };
            preflight.push((call, authorization));
        }

        for (call, authorization) in preflight {
            check_cancelled(&mut coordinator, cancellation)?;
            flush_progress(&coordinator, progress, &mut progress_cursor);

            let output = match authorization {
                PreflightAuthorization::InvalidArguments => {
                    HeadlessToolOutput::failure("invalid tool arguments")
                }
                PreflightAuthorization::Decided(PermissionDecision::Allow) => dispatcher
                    .dispatch(call.clone(), cancellation)
                    .await
                    .map_err(|error| {
                        finish_port_error(&mut coordinator, error, HeadlessTurnError::Tool)
                    })?,
                PreflightAuthorization::Decided(PermissionDecision::Deny) => {
                    let output = HeadlessToolOutput::failure("permission denied");
                    match permission_gate.denial_facts(&call) {
                        Some(facts) => output.with_facts(facts),
                        None => output,
                    }
                }
                PreflightAuthorization::UnreachablePromptDenial => {
                    let output = HeadlessToolOutput::failure(
                        "permission denied: the approval prompt could not be reached in a subagent",
                    );
                    match permission_gate.denial_facts(&call) {
                        Some(facts) => output.with_facts(facts),
                        None => output,
                    }
                }
                PreflightAuthorization::Decided(PermissionDecision::Ask) => {
                    return Err(permission_required(&mut coordinator));
                }
            };

            coordinator
                .accept_tool_result(&call.id, output.content, output.is_error, output.facts)
                .map_err(|_| fail_state(&mut coordinator))?;
            flush_progress(&coordinator, progress, &mut progress_cursor);
            if let Err(error) = check_cancelled(&mut coordinator, cancellation) {
                flush_progress(&coordinator, progress, &mut progress_cursor);
                return Err(error);
            }
        }
    }
}

fn flush_progress(
    coordinator: &TurnCoordinator,
    progress: Option<&TurnProgressSink>,
    cursor: &mut usize,
) {
    let Some(progress) = progress else {
        return;
    };

    for event in &coordinator.events()[*cursor..] {
        progress(event.clone());
    }
    *cursor = coordinator.events().len();
}

fn headless_tool_call(part: &MessagePart) -> Option<HeadlessToolCall> {
    let MessagePart::ToolCall { id, name, input } = part else {
        return None;
    };

    Some(HeadlessToolCall {
        id: id.clone(),
        name: name.clone(),
        input: input.clone(),
    })
}

async fn resolve_permission_decision(
    decision: PermissionDecision,
    call: &HeadlessToolCall,
    permission_resolver: &mut impl HeadlessPermissionResolver,
    coordinator: &mut TurnCoordinator,
    cancellation: &HeadlessTurnCancellation,
) -> Result<PermissionDecision, HeadlessTurnError> {
    if decision != PermissionDecision::Ask {
        return Ok(decision);
    }

    permission_resolver
        .resolve(call, cancellation)
        .await
        .map_err(|error| {
            finish_port_error(coordinator, error, HeadlessTurnError::PermissionEvaluation)
        })
}

fn check_cancelled(
    coordinator: &mut TurnCoordinator,
    cancellation: &HeadlessTurnCancellation,
) -> Result<(), HeadlessTurnError> {
    if !cancellation.is_cancelled() && !cancellation.is_expired() {
        return Ok(());
    }

    if cancellation.is_cancelled() {
        coordinator.cancel().map_err(|_| HeadlessTurnError::State)?;
        return Err(HeadlessTurnError::Cancelled);
    }

    coordinator.fail().map_err(|_| HeadlessTurnError::State)?;
    Err(HeadlessTurnError::TimedOut)
}

fn finish_port_error(
    coordinator: &mut TurnCoordinator,
    error: HeadlessTurnPortError,
    failure: HeadlessTurnError,
) -> HeadlessTurnError {
    if error == HeadlessTurnPortError::Cancelled {
        return coordinator
            .cancel()
            .map(|()| HeadlessTurnError::Cancelled)
            .unwrap_or(HeadlessTurnError::State);
    }

    if error == HeadlessTurnPortError::TimedOut {
        return coordinator
            .fail()
            .map(|()| HeadlessTurnError::TimedOut)
            .unwrap_or(HeadlessTurnError::State);
    }

    if error == HeadlessTurnPortError::Authentication {
        return coordinator
            .fail()
            .map(|()| HeadlessTurnError::Authentication)
            .unwrap_or(HeadlessTurnError::State);
    }

    let provider_failure = map_port_error(error);
    if let Some(provider_failure) = provider_failure {
        return coordinator
            .fail()
            .map(|()| provider_failure)
            .unwrap_or(HeadlessTurnError::State);
    }

    if let HeadlessTurnPortError::TaskTerminal(terminal) = error {
        return coordinator
            .fail()
            .map(|()| HeadlessTurnError::TaskTerminal(terminal))
            .unwrap_or(HeadlessTurnError::State);
    }

    if coordinator.fail().is_err() {
        HeadlessTurnError::State
    } else {
        failure
    }
}

fn map_port_error(error: HeadlessTurnPortError) -> Option<HeadlessTurnError> {
    match error {
        HeadlessTurnPortError::ProviderRejected => Some(HeadlessTurnError::ProviderRejected),
        HeadlessTurnPortError::ProviderContext => Some(HeadlessTurnError::ProviderContext),
        HeadlessTurnPortError::ProviderRateLimited => Some(HeadlessTurnError::ProviderRateLimited),
        HeadlessTurnPortError::ProviderServer => Some(HeadlessTurnError::ProviderServer),
        HeadlessTurnPortError::ProviderNetwork => Some(HeadlessTurnError::ProviderNetwork),
        HeadlessTurnPortError::ProviderProtocol => Some(HeadlessTurnError::ProviderProtocol),
        _ => None,
    }
}

fn fail_state(coordinator: &mut TurnCoordinator) -> HeadlessTurnError {
    let _ = coordinator.fail();
    HeadlessTurnError::State
}

fn permission_required(coordinator: &mut TurnCoordinator) -> HeadlessTurnError {
    coordinator
        .fail()
        .map(|()| HeadlessTurnError::PermissionRequired)
        .unwrap_or(HeadlessTurnError::State)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionDecision {
    Allow,
    Ask,
    Deny,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionScope {
    Global,
    Project,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionMode {
    Edit,
    Chat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolAccess {
    ReadOnly,
    Write,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PermissionPattern {
    Any,
    Exact(String),
    Glob(ValidatedPermissionGlob),
}

pub const MAX_PERMISSION_GLOB_PATTERN_BYTES: usize = 16 * 1024;
pub const MAX_PERMISSION_GLOB_SEGMENTS: usize = 256;
pub const MAX_PERMISSION_TARGET_BYTES: usize = 16 * 1024;

impl PermissionPattern {
    /// Builds a glob pattern for a path-shaped target, where `/` is a
    /// literal path-segment boundary that a bare `*` never crosses; only
    /// `**` occupying a whole segment (`prefix/**`, `**/suffix`,
    /// `dir/**/secret`) matches across it. This is the right matcher for
    /// filesystem paths and for tool-name patterns, so it is also the
    /// default used everywhere a target's kind is not otherwise known.
    ///
    /// A target that is free-form text rather than a path — most notably
    /// `bash`'s command line, which routinely contains `/` as an ordinary
    /// character rather than a hierarchy boundary — needs
    /// [`Self::glob_for_target_kind`] with [`PermissionTargetKind::FreeFormText`]
    /// instead; this constructor would otherwise make a pattern like `rm*`
    /// silently fail to match `rm -rf /tmp/x`.
    pub fn glob(pattern: impl Into<String>) -> Result<Self, PermissionPatternError> {
        Self::glob_for_target_kind(pattern, PermissionTargetKind::Path)
    }

    /// Builds a glob pattern whose `/`-crossing behavior is chosen by the
    /// target's kind. See [`PermissionTargetKind`] for the semantics of each
    /// kind and [`permission_target_kind_for_tool`] for classifying a tool by
    /// name.
    pub fn glob_for_target_kind(
        pattern: impl Into<String>,
        kind: PermissionTargetKind,
    ) -> Result<Self, PermissionPatternError> {
        let literal_separator = matches!(kind, PermissionTargetKind::Path);
        ValidatedPermissionGlob::new(pattern.into(), literal_separator).map(Self::Glob)
    }

    pub fn glob_source(&self) -> Option<&str> {
        match self {
            Self::Glob(pattern) => Some(pattern.pattern.as_str()),
            Self::Any | Self::Exact(_) => None,
        }
    }

    pub fn matches(&self, value: &str) -> bool {
        match self {
            Self::Any => true,
            Self::Exact(expected) => expected == value,
            Self::Glob(pattern) => pattern.matches(value),
        }
    }

    /// Reports whether this pattern selects every value it could be compared
    /// against, which is what makes an absent target, `*` on a free-form
    /// target, and `**` three spellings of one thing rather than three
    /// different breadths.
    ///
    /// `*` on a path-shaped target is deliberately NOT one of them: it stops
    /// at a `/`, so `src/main.rs` escapes it while `**` covers it.
    pub fn denotes_everything(&self) -> bool {
        match self {
            Self::Any => true,
            Self::Exact(_) => false,
            Self::Glob(pattern) => pattern.denotes_everything(),
        }
    }

    /// Reports whether every value `other` selects is also selected by `self`.
    ///
    /// This is a sound under-approximation: it answers `true` only where the
    /// containment can be established structurally, and `false` — meaning
    /// "not known to be broader" — for every pattern shape it cannot decide.
    /// Callers therefore have to treat `false` in both directions as
    /// "incomparable" rather than as "disjoint". [`Self::Any`] is decided by
    /// the two breadth checks above and never reaches the shape comparison.
    pub fn covers(&self, other: &Self) -> bool {
        if self.denotes_everything() {
            return true;
        }
        if other.denotes_everything() {
            return false;
        }

        match (self, other) {
            (Self::Exact(broader), Self::Exact(narrower)) => broader == narrower,
            (Self::Exact(_), _) => false,
            (_, Self::Exact(value)) => self.matches(value),
            (Self::Glob(broader), Self::Glob(narrower)) => broader.covers(narrower),
            (Self::Any, _) | (_, Self::Any) => false,
        }
    }
}

/// Classifies what kind of value a permission target holds, which decides
/// whether a bare `*` in a glob pattern may cross a `/`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionTargetKind {
    /// A filesystem-shaped target: `read`/`write`/`edit`/`list`/`search`
    /// paths, `glob`'s own file-glob argument, `grep`'s search pattern, and
    /// `webfetch` URLs (a URL's scheme/host/path components are themselves
    /// hierarchical, so treating `/` as a path-segment boundary there gives
    /// the same predictable, non-surprising behavior as an actual path).
    /// `/` is a meaningful segment boundary that a bare `*` never crosses;
    /// only an explicit `**` segment matches across it.
    Path,
    /// A free-form target that is not shaped like a path even though it may
    /// contain `/` incidentally: `bash`'s shell command line and
    /// `git_read`'s operation keyword. `/` is an ordinary character here, so
    /// a bare `*` crosses it, matching a user's plain-English expectation
    /// that `rm*` denies `rm -rf /tmp/x`.
    FreeFormText,
}

/// Classifies a native tool's permission target by name, matching the bare
/// form (`bash`), the fully-qualified native identity (`native::bash`) and the
/// dispatcher's own encoding of it. A tool this function does not recognize as
/// free-form text defaults to [`PermissionTargetKind::Path`], keeping
/// segment-discipline matching for every target whose shape is not known to be
/// free-form.
pub fn permission_target_kind_for_tool(tool: &str) -> PermissionTargetKind {
    match bare_tool_name(tool) {
        "bash" | "git_read" => PermissionTargetKind::FreeFormText,
        _ => PermissionTargetKind::Path,
    }
}

#[derive(Clone, Debug)]
pub struct ValidatedPermissionGlob {
    pattern: String,
    literal_separator: bool,
    matcher: GlobMatcher,
}

impl ValidatedPermissionGlob {
    fn new(pattern: String, literal_separator: bool) -> Result<Self, PermissionPatternError> {
        if pattern.trim().is_empty() {
            return Err(PermissionPatternError::InvalidGlob { pattern });
        }

        if pattern.len() > MAX_PERMISSION_GLOB_PATTERN_BYTES {
            return Err(PermissionPatternError::GlobTooLarge {
                actual: pattern.len(),
                limit: MAX_PERMISSION_GLOB_PATTERN_BYTES,
            });
        }

        let segments = pattern.split('/').count();
        if segments > MAX_PERMISSION_GLOB_SEGMENTS {
            return Err(PermissionPatternError::GlobTooLarge {
                actual: segments,
                limit: MAX_PERMISSION_GLOB_SEGMENTS,
            });
        }

        let matcher = GlobBuilder::new(&pattern)
            .literal_separator(literal_separator)
            .build()
            .map_err(|_| PermissionPatternError::InvalidGlob {
                pattern: pattern.clone(),
            })?
            .compile_matcher();

        Ok(Self {
            pattern,
            literal_separator,
            matcher,
        })
    }

    fn matches(&self, value: &str) -> bool {
        value.len() <= MAX_PERMISSION_TARGET_BYTES && self.matcher.is_match(value)
    }

    fn denotes_everything(&self) -> bool {
        self.pattern == "**" || self.pattern == "*" && !self.literal_separator
    }

    /// Establishes containment between two globs from the only shape it can be
    /// read off structurally: a wildcard-free prefix followed by a trailing
    /// wildcard that runs to the end of the pattern. Every other shape is left
    /// undecided, because `a*` and `*b` overlap without either containing the
    /// other and no ordering of them would be defensible.
    fn covers(&self, other: &Self) -> bool {
        if self.pattern == other.pattern && self.literal_separator == other.literal_separator {
            return true;
        }

        if !has_glob_wildcard(&other.pattern) {
            return self.matches(&other.pattern);
        }

        let (Some((prefix, crosses_separator)), Some((other_prefix, other_crosses_separator))) = (
            self.trailing_wildcard_prefix(),
            other.trailing_wildcard_prefix(),
        ) else {
            return false;
        };

        let Some(remainder) = other_prefix.strip_prefix(prefix) else {
            return false;
        };

        crosses_separator || !other_crosses_separator && !remainder.contains('/')
    }

    /// Splits `<literal><trailing wildcard>` into the literal and whether that
    /// wildcard reaches across `/`, or reports `None` for any other shape.
    fn trailing_wildcard_prefix(&self) -> Option<(&str, bool)> {
        let (prefix, crosses_separator) = match self.pattern.strip_suffix("**") {
            Some(prefix) => (prefix, true),
            None => (self.pattern.strip_suffix('*')?, !self.literal_separator),
        };

        (!has_glob_wildcard(prefix)).then_some((prefix, crosses_separator))
    }
}

fn has_glob_wildcard(pattern: &str) -> bool {
    pattern.contains(['*', '?', '[', ']', '{', '}', '\\'])
}

impl PartialEq for ValidatedPermissionGlob {
    fn eq(&self, other: &Self) -> bool {
        self.pattern == other.pattern
    }
}

impl Eq for ValidatedPermissionGlob {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PermissionPatternError {
    InvalidGlob { pattern: String },
    GlobTooLarge { actual: usize, limit: usize },
}

impl fmt::Display for PermissionPatternError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidGlob { .. } => formatter.write_str("invalid permission glob"),
            Self::GlobTooLarge { .. } => formatter.write_str("permission glob exceeds size limit"),
        }
    }
}

impl std::error::Error for PermissionPatternError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionRequest {
    pub project: String,
    pub tool: String,
    pub target: String,
    pub access: ToolAccess,
    outside_worktree: bool,
    /// The invocations a shell target would run, each given as the equivalent
    /// spellings a rule could be written against. Empty for every tool whose
    /// target is not a command line.
    invocations: Vec<Vec<String>>,
}

impl PermissionRequest {
    pub fn new(
        project: impl Into<String>,
        tool: impl Into<String>,
        target: impl Into<String>,
        access: ToolAccess,
    ) -> Self {
        Self::build(
            project.into(),
            tool.into(),
            PermissionTarget::native(target).project(),
            access,
        )
    }

    pub fn with_target(
        project: impl Into<String>,
        tool: impl Into<String>,
        target: PermissionTarget,
        access: ToolAccess,
    ) -> Self {
        Self::build(project.into(), tool.into(), target.project(), access)
    }

    /// Reads the raw target as what it names before any rule sees it: a path
    /// loses the components that select nothing, and a command line is
    /// decomposed into the invocations it would run. See
    /// [`crate::permission_target`] for what that does and does not cover.
    fn build(project: String, tool: String, target: String, access: ToolAccess) -> Self {
        let target = match permission_target_kind_for_tool(&tool) {
            PermissionTargetKind::Path => permission_target::normalized_path_target(&target),
            PermissionTargetKind::FreeFormText => target,
        };
        let invocations = if bare_tool_name(&tool) == "bash" {
            permission_target::command_invocations(&target)
        } else {
            Vec::new()
        };

        Self {
            project,
            tool,
            target,
            access,
            outside_worktree: false,
            invocations,
        }
    }

    pub fn outside_worktree(mut self) -> Self {
        self.outside_worktree = true;
        self
    }

    /// Reports whether `pattern` selects this request's target.
    ///
    /// A shell target is several calls at once, so the two directions of that
    /// question are not the same. A restrictive rule selects the command when
    /// ANY of its invocations is selected — a deny on `rm` has to hold in
    /// `cd /tmp && rm -rf x`. A permissive rule selects it only when EVERY
    /// invocation is selected, because authorizing `git*` is not authorization
    /// for whatever was chained onto it.
    fn target_selected_by(
        &self,
        pattern: &PermissionPattern,
        decision: PermissionDecision,
    ) -> bool {
        let selects = |forms: &Vec<String>| forms.iter().any(|form| pattern.matches(form));

        match decision {
            _ if self.invocations.is_empty() => pattern.matches(&self.target),
            PermissionDecision::Allow => self.invocations.iter().all(selects),
            PermissionDecision::Ask | PermissionDecision::Deny => {
                pattern.matches(&self.target) || self.invocations.iter().any(selects)
            }
        }
    }
}

/// Reduces any spelling of a tool — bare, qualified, or a dispatcher identity
/// (`native:13:native::bash`) — to the name a rule is written against.
fn bare_tool_name(tool: &str) -> &str {
    let qualified = tool
        .strip_prefix("native:")
        .and_then(|rest| rest.split_once(':'))
        .filter(|(length, _)| {
            !length.is_empty() && length.bytes().all(|byte| byte.is_ascii_digit())
        })
        .map_or(tool, |(_, name)| name);

    qualified.strip_prefix("native::").unwrap_or(qualified)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PermissionTarget {
    Path(String),
    Command(String),
    Url(String),
    Native(String),
    Mcp(String),
}

impl PermissionTarget {
    pub fn path(value: impl Into<String>) -> Self {
        Self::Path(value.into())
    }

    pub fn command(value: impl Into<String>) -> Self {
        Self::Command(value.into())
    }

    pub fn url(value: impl Into<String>) -> Self {
        Self::Url(value.into())
    }

    pub fn native(value: impl Into<String>) -> Self {
        Self::Native(value.into())
    }

    pub fn mcp(value: impl Into<String>) -> Self {
        Self::Mcp(value.into())
    }

    pub fn project(self) -> String {
        let value = match self {
            Self::Path(value)
            | Self::Command(value)
            | Self::Url(value)
            | Self::Native(value)
            | Self::Mcp(value) => value,
        };

        let mut end = value.len().min(MAX_PERMISSION_TARGET_BYTES);

        while !value.is_char_boundary(end) {
            end -= 1;
        }

        value[..end].to_owned()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionRule {
    pub scope: PermissionScope,
    pub project: Option<String>,
    pub decision: PermissionDecision,
    pub tool: PermissionPattern,
    pub target: PermissionPattern,
}

impl PermissionRule {
    pub fn global(
        decision: PermissionDecision,
        tool: PermissionPattern,
        target: PermissionPattern,
    ) -> Self {
        Self {
            scope: PermissionScope::Global,
            project: None,
            decision,
            tool,
            target,
        }
    }

    pub fn project(
        project: impl Into<String>,
        decision: PermissionDecision,
        tool: PermissionPattern,
        target: PermissionPattern,
    ) -> Self {
        Self {
            scope: PermissionScope::Project,
            project: Some(project.into()),
            decision,
            tool,
            target,
        }
    }

    fn matches(&self, request: &PermissionRequest) -> bool {
        let project_matches = match self.scope {
            PermissionScope::Global => true,
            PermissionScope::Project => self.project.as_deref() == Some(request.project.as_str()),
        };

        project_matches
            && self.tool.matches(&request.tool)
            && request.target_selected_by(&self.target, self.decision)
    }
}

pub const MAX_AGENT_NAME_CHARS: usize = 64;
pub const MAX_AGENT_DESCRIPTION_CHARS: usize = 1024;
pub const MAX_AGENT_SKILLS: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentMode {
    Primary,
    Subagent,
    All,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentDefinition {
    pub name: String,
    pub description: String,
    pub mode: AgentMode,
    pub model: Option<String>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub system_prompt: String,
    pub permission_rules: Vec<PermissionRule>,
    pub skills: Vec<String>,
}

impl AgentDefinition {
    pub fn validate(&self) -> Result<(), AgentDefinitionError> {
        if !is_catalog_name(&self.name) {
            return Err(AgentDefinitionError::InvalidName);
        }

        if !is_bounded_description(&self.description) {
            return Err(AgentDefinitionError::InvalidDescription);
        }

        if self.system_prompt.is_empty() {
            return Err(AgentDefinitionError::EmptySystemPrompt);
        }

        if self.skills.len() > MAX_AGENT_SKILLS {
            return Err(AgentDefinitionError::TooManySkills);
        }

        let mut seen_skills = std::collections::BTreeSet::new();
        if self
            .skills
            .iter()
            .any(|skill| !is_catalog_name(skill) || !seen_skills.insert(skill))
        {
            return Err(AgentDefinitionError::DuplicateSkill);
        }

        self.permission_rules
            .iter()
            .all(has_bounded_permission_patterns)
            .then_some(())
            .ok_or(AgentDefinitionError::InvalidPermissionRule)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentDefinitionError {
    InvalidName,
    InvalidDescription,
    EmptySystemPrompt,
    TooManySkills,
    DuplicateSkill,
    InvalidPermissionRule,
}

pub fn is_catalog_name(value: &str) -> bool {
    let length = value.chars().count();
    (1..=MAX_AGENT_NAME_CHARS).contains(&length)
        && !value.starts_with('-')
        && !value.ends_with('-')
        && !value.contains("--")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

pub fn is_model_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_AGENT_NAME_CHARS
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        })
}

fn is_bounded_description(value: &str) -> bool {
    let length = value.chars().count();
    (1..=MAX_AGENT_DESCRIPTION_CHARS).contains(&length) && !value.chars().any(char::is_control)
}

fn has_bounded_permission_patterns(rule: &PermissionRule) -> bool {
    fn is_valid_exact(pattern: &PermissionPattern, limit: usize) -> bool {
        !matches!(pattern, PermissionPattern::Exact(value) if value.is_empty() || value.len() > limit)
    }

    is_valid_exact(&rule.tool, MAX_PERMISSION_GLOB_PATTERN_BYTES)
        && is_valid_exact(&rule.target, MAX_PERMISSION_TARGET_BYTES)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectPermissionGrant {
    pub project: String,
    pub decision: PermissionDecision,
    pub tool: PermissionPattern,
    pub target: PermissionPattern,
}

impl ProjectPermissionGrant {
    pub fn new(
        project: impl Into<String>,
        decision: PermissionDecision,
        tool: PermissionPattern,
        target: PermissionPattern,
    ) -> Self {
        Self {
            project: project.into(),
            decision,
            tool,
            target,
        }
    }

    pub fn allow(
        project: impl Into<String>,
        tool: PermissionPattern,
        target: PermissionPattern,
    ) -> Self {
        Self::new(project, PermissionDecision::Allow, tool, target)
    }

    fn matches(&self, request: &PermissionRequest) -> bool {
        self.project == request.project
            && self.tool.matches(&request.tool)
            && request.target_selected_by(&self.target, self.decision)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PermissionSession {
    temporary_bypass: bool,
}

impl PermissionSession {
    pub const fn new() -> Self {
        Self {
            temporary_bypass: false,
        }
    }

    pub const fn with_temporary_bypass() -> Self {
        Self {
            temporary_bypass: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SafetyPredicate {
    WorktreeEscape,
    ChatWrite,
    GlobalDeny(Box<GlobalDenyPredicate>),
}

/// The operator's configured `[permissions]` rules, resolved among themselves
/// and then combined with whatever the agent declared.
///
/// Configuration is a ceiling on authority rather than one voice among many: a
/// declaration can narrow it further, but can never reopen a call the
/// configuration nets to `Deny` nor skip an approval it nets to `Ask`. The two
/// sets are resolved separately and combined by taking the more restrictive
/// answer, which is what keeps both halves of that sentence true — merging them
/// into one set would let whichever rule happened to be narrower decide alone.
///
/// Resolving the configured rules against each other first is also what keeps a
/// configured `allow` able to carve an exception out of a configured `deny`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfiguredFloor {
    rules: Vec<PermissionRule>,
    role: ConfiguredRole,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConfiguredRole {
    Governing,
    Restricting,
}

impl ConfiguredFloor {
    /// The primary path, where the operator's configuration is the authority a
    /// call is measured against: a configured `allow` authorizes on its own.
    pub fn governing(rules: Vec<PermissionRule>) -> Self {
        Self {
            rules,
            role: ConfiguredRole::Governing,
        }
    }

    /// A delegated child, where only the agent definition can authorize a tool.
    /// The same rules still restrict, so a configured `deny` or `ask` reaches
    /// the child exactly as it reaches the parent, but a configured `allow`
    /// grants a child nothing it did not declare.
    pub fn restricting(rules: Vec<PermissionRule>) -> Self {
        Self {
            rules,
            role: ConfiguredRole::Restricting,
        }
    }

    pub fn rules(&self) -> &[PermissionRule] {
        &self.rules
    }

    fn denies(&self, request: &PermissionRequest) -> bool {
        self.decision(request) == Some(PermissionDecision::Deny)
    }

    fn decision(&self, request: &PermissionRequest) -> Option<PermissionDecision> {
        prevailing_rule_decision(&self.rules, request)
    }

    /// The configured answer as far as it constrains this path: everything on
    /// the primary path, and only the non-authorizing decisions in a child.
    fn constraint(&self, request: &PermissionRequest) -> Option<PermissionDecision> {
        self.decision(request).filter(|decision| {
            self.role == ConfiguredRole::Governing || *decision != PermissionDecision::Allow
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GlobalDenyPredicate {
    pub tool: PermissionPattern,
    pub target: PermissionPattern,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionPolicy {
    mode: PermissionMode,
    static_rules: Vec<PermissionRule>,
    safety_predicates: Vec<SafetyPredicate>,
    configured_floor: Option<ConfiguredFloor>,
}

impl PermissionPolicy {
    pub fn new(mode: PermissionMode, static_rules: Vec<PermissionRule>) -> Self {
        Self::with_safety_predicates(
            mode,
            static_rules,
            vec![SafetyPredicate::WorktreeEscape, SafetyPredicate::ChatWrite],
        )
    }

    pub fn with_safety_predicates(
        mode: PermissionMode,
        static_rules: Vec<PermissionRule>,
        safety_predicates: Vec<SafetyPredicate>,
    ) -> Self {
        Self {
            mode,
            static_rules,
            safety_predicates,
            configured_floor: None,
        }
    }

    /// Holds the operator's configured rules above the declared ones. See
    /// [`ConfiguredFloor`] for why they are kept as a separate set rather than
    /// concatenated onto `static_rules`.
    #[must_use]
    pub fn with_configured_floor(mut self, floor: ConfiguredFloor) -> Self {
        self.configured_floor = Some(floor);
        self
    }

    pub fn evaluate(
        &self,
        request: &PermissionRequest,
        project_grants: &[ProjectPermissionGrant],
        session: &PermissionSession,
    ) -> PermissionDecision {
        self.evaluate_with_session_grants(request, project_grants, &[], session)
    }

    pub fn normalized_tool_aliases(&self, aliases: impl Fn(&str) -> Option<String>) -> Self {
        let mut policy = self.clone();
        for rule in &mut policy.static_rules {
            normalize_tool_pattern(&mut rule.tool, &aliases);
        }
        for predicate in &mut policy.safety_predicates {
            match predicate {
                SafetyPredicate::GlobalDeny(deny) => {
                    normalize_tool_pattern(&mut deny.tool, &aliases);
                }
                SafetyPredicate::WorktreeEscape | SafetyPredicate::ChatWrite => {}
            }
        }
        if let Some(floor) = policy.configured_floor.as_mut() {
            for rule in &mut floor.rules {
                normalize_tool_pattern(&mut rule.tool, &aliases);
            }
        }
        policy
    }

    pub fn evaluate_with_session_grants(
        &self,
        request: &PermissionRequest,
        project_grants: &[ProjectPermissionGrant],
        session_grants: &[ProjectPermissionGrant],
        session: &PermissionSession,
    ) -> PermissionDecision {
        self.evaluate_with_unmatched_override(
            request,
            project_grants,
            session_grants,
            session,
            false,
        )
    }

    /// Resolves a permission request against static rules, then persisted
    /// grants, then an unmatched-call fallback — in that order of authority —
    /// and never above what the operator's configured rules leave open.
    ///
    /// A static rule's `Deny` is sticky: once a declaration (agent-defined or
    /// configured) denies a request, no later project or session grant can
    /// reopen it. Any other matched decision keeps the existing
    /// grant-outranks-rule behavior. A matched decision is never touched by
    /// `unmatched_allow` or by `session.temporary_bypass` — those two only
    /// decide what happens when nothing matched at all, which is the sole
    /// remaining role of `resolve_ask`.
    ///
    /// Which static rule decides is [`prevailing_rule_decision`]'s answer, not
    /// the last one written: authoring order never decides safety.
    pub fn evaluate_with_unmatched_override(
        &self,
        request: &PermissionRequest,
        project_grants: &[ProjectPermissionGrant],
        session_grants: &[ProjectPermissionGrant],
        session: &PermissionSession,
        unmatched_allow: bool,
    ) -> PermissionDecision {
        if !self.hard_safety_allows(request) {
            return PermissionDecision::Deny;
        }

        let static_decision = prevailing_rule_decision(&self.static_rules, request);

        let grant_decision = project_grants
            .iter()
            .filter(|grant| grant.matches(request))
            .map(|grant| grant.decision)
            .chain(
                session_grants
                    .iter()
                    .filter(|grant| grant.matches(request))
                    .map(|grant| grant.decision),
            )
            .last();

        let matched = if static_decision == Some(PermissionDecision::Deny) {
            static_decision
        } else {
            grant_decision.or(static_decision)
        };

        let configured = self
            .configured_floor
            .as_ref()
            .and_then(|floor| floor.constraint(request));

        match (matched, configured) {
            (Some(decision), Some(floor)) => prevailing_decision(decision, floor),
            (Some(decision), None) => decision,
            (None, Some(floor)) => floor,
            (None, None) => {
                let fallback = if unmatched_allow {
                    PermissionDecision::Allow
                } else {
                    PermissionDecision::Ask
                };
                Self::resolve_ask(fallback, session)
            }
        }
    }

    pub fn hard_safety_allows(&self, request: &PermissionRequest) -> bool {
        if self
            .configured_floor
            .as_ref()
            .is_some_and(|floor| floor.denies(request))
        {
            return false;
        }

        !self.safety_predicates.iter().any(|predicate| {
            matches!(predicate, SafetyPredicate::WorktreeEscape) && request.outside_worktree
                || matches!(predicate, SafetyPredicate::ChatWrite)
                    && self.mode == PermissionMode::Chat
                    && request.access == ToolAccess::Write
                || matches!(predicate, SafetyPredicate::GlobalDeny(global_deny)
                    if global_deny.tool.matches(&request.tool)
                        && global_deny.target.matches(&request.target))
        })
    }

    fn resolve_ask(
        decision: PermissionDecision,
        session: &PermissionSession,
    ) -> PermissionDecision {
        if decision == PermissionDecision::Ask && session.temporary_bypass {
            PermissionDecision::Allow
        } else {
            decision
        }
    }
}

pub fn normalize_project_permission_grants(
    grants: &[ProjectPermissionGrant],
    aliases: impl Fn(&str) -> Option<String>,
) -> Vec<ProjectPermissionGrant> {
    grants
        .iter()
        .cloned()
        .map(|mut grant| {
            normalize_tool_pattern(&mut grant.tool, &aliases);
            grant
        })
        .collect()
}

fn normalize_tool_pattern(
    pattern: &mut PermissionPattern,
    aliases: &impl Fn(&str) -> Option<String>,
) {
    if let PermissionPattern::Exact(value) = pattern
        && let Some(canonical) = aliases(value)
    {
        *value = canonical;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorCategory {
    Config,
    Auth,
    Provider,
    Permission,
    Tool,
    Store,
    Extension,
    Ui,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    Config(String),
    Auth(String),
    Provider(String),
    Permission(String),
    Tool(String),
    Store(String),
    Extension(String),
    Ui(String),
    Cancelled,
}

impl Error {
    pub const fn category(&self) -> ErrorCategory {
        match self {
            Self::Config(_) => ErrorCategory::Config,
            Self::Auth(_) => ErrorCategory::Auth,
            Self::Provider(_) => ErrorCategory::Provider,
            Self::Permission(_) => ErrorCategory::Permission,
            Self::Tool(_) => ErrorCategory::Tool,
            Self::Store(_) => ErrorCategory::Store,
            Self::Extension(_) => ErrorCategory::Extension,
            Self::Ui(_) => ErrorCategory::Ui,
            Self::Cancelled => ErrorCategory::Cancelled,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(message) => write!(formatter, "config: {message}"),
            Self::Auth(message) => write!(formatter, "auth: {message}"),
            Self::Provider(message) => write!(formatter, "provider: {message}"),
            Self::Permission(message) => write!(formatter, "permission: {message}"),
            Self::Tool(message) => write!(formatter, "tool: {message}"),
            Self::Store(message) => write!(formatter, "store: {message}"),
            Self::Extension(message) => write!(formatter, "extension: {message}"),
            Self::Ui(message) => write!(formatter, "ui: {message}"),
            Self::Cancelled => formatter.write_str("cancelled"),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::{
        AttemptKey, CompletedTurnSnapshot, FactPath, MessagePart, PermissionPattern,
        RecoveryOutcome, RetryBoundary, SessionAttemptFailureKind, SessionAttemptStatus,
        SessionAttemptSummary, ToolOutcome, ToolResultFacts, TurnCoordinator, TurnEvent,
    };

    #[test]
    fn session_attempt_domain_rejects_invalid_status_category_and_recovery_shapes() {
        assert!(
            SessionAttemptSummary::new(
                AttemptKey::new(1, 1).unwrap(),
                1,
                SessionAttemptStatus::Running,
                None,
                10,
                None,
            )
            .is_ok()
        );
        assert!(
            SessionAttemptSummary::new(
                AttemptKey::new(1, 1).unwrap(),
                1,
                SessionAttemptStatus::Running,
                Some(SessionAttemptFailureKind::Failed),
                10,
                None,
            )
            .is_err()
        );
        assert!(
            SessionAttemptSummary::new(
                AttemptKey::new(1, 1).unwrap(),
                1,
                SessionAttemptStatus::ProviderError,
                Some(SessionAttemptFailureKind::Failed),
                10,
                Some(11),
            )
            .is_err()
        );

        let interrupted = SessionAttemptSummary::new(
            AttemptKey::new(1, 2).unwrap(),
            2,
            SessionAttemptStatus::Interrupted,
            Some(SessionAttemptFailureKind::Interrupted),
            10,
            Some(11),
        )
        .unwrap();
        assert_eq!(
            RecoveryOutcome::Recovered(interrupted.clone()).summary(),
            Some(&interrupted)
        );
        assert_eq!(RecoveryOutcome::Stale.summary(), None);

        assert!(RetryBoundary::new(AttemptKey::new(1, 2).unwrap(), "retry".into()).is_ok());
        assert!(RetryBoundary::new(AttemptKey::new(1, 2).unwrap(), String::new()).is_err());
    }

    #[test]
    fn validated_glob_source_is_available_read_only_for_persistence() {
        let pattern = PermissionPattern::glob("src/**/*.rs").unwrap();

        assert_eq!(pattern.glob_source(), Some("src/**/*.rs"));
        assert_eq!(PermissionPattern::Any.glob_source(), None);
        assert_eq!(
            PermissionPattern::Exact("native::edit".into()).glob_source(),
            None
        );
    }

    #[test]
    fn retained_tool_results_are_utf8_byte_bounded() {
        const RETAINED_TOOL_RESULT_CAP_BYTES: usize = 64 * 1024;

        let exact = "a".repeat(RETAINED_TOOL_RESULT_CAP_BYTES);
        let one_byte_over = "b".repeat(RETAINED_TOOL_RESULT_CAP_BYTES + 1);
        let multibyte = "😀".repeat((RETAINED_TOOL_RESULT_CAP_BYTES / 4) + 1);
        let repeated = "c".repeat(RETAINED_TOOL_RESULT_CAP_BYTES + 1);
        let one_byte_over_marker = "\n… [runtime truncated; 43 bytes omitted]";
        let one_byte_over_expected = format!(
            "{}{}",
            "b".repeat(RETAINED_TOOL_RESULT_CAP_BYTES - one_byte_over_marker.len()),
            one_byte_over_marker
        );
        let multibyte_marker = "\n… [runtime truncated; 48 bytes omitted]";
        let multibyte_expected = format!(
            "{}{}",
            "😀".repeat((RETAINED_TOOL_RESULT_CAP_BYTES - multibyte_marker.len()) / 4),
            multibyte_marker
        );
        let mut coordinator = TurnCoordinator::new();

        coordinator.begin().unwrap();
        for id in ["exact", "one-byte-over", "multibyte", "repeated-error"] {
            coordinator
                .accept_provider_part(MessagePart::ToolCall {
                    id: id.into(),
                    name: "read".into(),
                    input: "{}".into(),
                })
                .unwrap();
        }
        coordinator.finish_provider_iteration().unwrap();

        for (id, content, is_error) in [
            ("exact", exact.clone(), false),
            ("one-byte-over", one_byte_over, false),
            ("multibyte", multibyte, false),
            ("repeated-error", repeated, true),
        ] {
            coordinator
                .accept_tool_result(id, content, is_error, None)
                .unwrap();
        }

        let retained = coordinator
            .events()
            .iter()
            .filter_map(|event| match event {
                TurnEvent::ToolResult(MessagePart::ToolResult {
                    tool_call_id,
                    content,
                    is_error,
                }) => Some((tool_call_id.as_str(), content.as_str(), *is_error)),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(retained.len(), 4);
        assert_eq!(retained[0], ("exact", exact.as_str(), false));
        assert_eq!(retained[0].1.len(), RETAINED_TOOL_RESULT_CAP_BYTES);
        assert_eq!(
            retained[1],
            ("one-byte-over", one_byte_over_expected.as_str(), false)
        );
        assert_eq!(
            retained[2],
            ("multibyte", multibyte_expected.as_str(), false)
        );
        assert!(retained[3].1.ends_with(one_byte_over_marker));
        assert!(retained[3].2);
        assert!(
            retained
                .iter()
                .all(|(_, content, _)| content.len() <= RETAINED_TOOL_RESULT_CAP_BYTES)
        );

        coordinator
            .accept_provider_part(MessagePart::Text("complete".into()))
            .unwrap();
        coordinator.finish_provider_iteration().unwrap();

        let snapshot =
            CompletedTurnSnapshot::from_persisted_events(coordinator.events().to_vec()).unwrap();
        assert_eq!(snapshot.events(), coordinator.events());
    }

    #[test]
    fn fact_path_rejects_an_absolute_path() {
        let path = FactPath::new("/etc/passwd");

        assert!(!path.is_representable());
        assert_eq!(path.relative(), None);
    }

    #[test]
    fn fact_path_rejects_a_traversing_path() {
        let path = FactPath::new("../secret.txt");

        assert!(!path.is_representable());
        assert_eq!(path.relative(), None);
    }

    #[test]
    fn fact_path_rejects_an_over_length_path() {
        let over_length = "a".repeat(FactPath::MAX_BYTES + 1);

        let path = FactPath::new(&over_length);

        assert!(!path.is_representable());
        assert_eq!(path.relative(), None);
    }

    #[test]
    fn fact_path_rejects_an_embedded_newline() {
        let path = FactPath::new("notes\n.txt");

        assert!(!path.is_representable());
        assert_eq!(path.relative(), None);
    }

    #[test]
    fn fact_path_rejects_an_empty_path() {
        let path = FactPath::new("");

        assert!(!path.is_representable());
        assert_eq!(path.relative(), None);
    }

    #[test]
    fn fact_path_normalizes_a_leading_current_dir_component() {
        assert_eq!(FactPath::new("./notes.txt"), FactPath::new("notes.txt"));
    }

    #[test]
    fn fact_path_normalizes_repeated_leading_current_dir_components() {
        assert_eq!(FactPath::new("././notes.txt"), FactPath::new("notes.txt"));
    }

    #[test]
    fn fact_path_normalizes_an_embedded_current_dir_component() {
        assert_eq!(FactPath::new("src/./lib.rs"), FactPath::new("src/lib.rs"));
    }

    #[test]
    fn fact_path_normalizes_repeated_separators() {
        assert_eq!(FactPath::new("src//lib.rs"), FactPath::new("src/lib.rs"));
    }

    #[test]
    fn fact_path_normalizes_a_trailing_separator() {
        assert_eq!(FactPath::new("src/lib.rs/"), FactPath::new("src/lib.rs"));
    }

    #[test]
    fn fact_path_normalizes_a_trailing_current_dir_component() {
        assert_eq!(FactPath::new("src/lib.rs/."), FactPath::new("src/lib.rs"));
    }

    #[test]
    fn fact_path_keeps_a_bare_current_dir_verbatim() {
        let path = FactPath::new(".");

        assert!(path.is_representable());
        assert_eq!(path.relative(), Some("."));
    }

    #[test]
    fn fact_path_accepts_a_representable_relative_path() {
        let path = FactPath::new("src/lib.rs");

        assert!(path.is_representable());
        assert_eq!(path.relative(), Some("src/lib.rs"));
    }

    #[test]
    fn a_writes_reported_path_enforces_the_fact_path_contract() {
        let facts = ToolResultFacts::Write {
            path: FactPath::new("/etc/passwd"),
            outcome: ToolOutcome::Failed,
            written: None,
        };

        match facts {
            ToolResultFacts::Write { path, .. } => assert!(!path.is_representable()),
            other => panic!("expected write facts, got {other:?}"),
        }
    }

    #[test]
    fn an_edits_reported_path_enforces_the_fact_path_contract() {
        let facts = ToolResultFacts::Edit {
            path: FactPath::new("notes\n.txt"),
            outcome: ToolOutcome::Failed,
            changed: None,
        };

        match facts {
            ToolResultFacts::Edit { path, .. } => assert!(!path.is_representable()),
            other => panic!("expected edit facts, got {other:?}"),
        }
    }

    #[test]
    fn fact_path_status_distinguishes_representable_from_unrepresentable() {
        let representable = FactPath::new("notes.txt");
        let unrepresentable = FactPath::new("/notes.txt");

        assert_ne!(
            representable.is_representable(),
            unrepresentable.is_representable()
        );
    }
}

/// A [`CompletedTurnRepository`] that keeps nothing. For callers that run a turn
/// without owning its history — a subagent inside a parent turn, a probe — where
/// the alternative is threading an `Option` through every layer.
pub struct DiscardCompletedTurnRepository;

impl CompletedTurnRepository for DiscardCompletedTurnRepository {
    fn persist_completed_turn(
        &mut self,
        _: CompletedTurnSnapshot,
    ) -> impl std::future::Future<Output = Result<(), CompletedTurnStoreError>> + Send {
        std::future::ready(Ok(()))
    }
}

// ---------------------------------------------------------------------------
// Runtime observation
//
// What the runtime reports about its own execution: turns, tool calls, diffs,
// task and subagent lifecycle. Facts a surface renders and a daemon records, so
// they are owned by neither. The `Tui` prefix on some names is historical and is
// renamed separately.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffLine {
    pub number: u32,
    pub kind: DiffLineKind,
    pub text: String,
}

impl DiffLine {
    pub fn new(number: u32, kind: DiffLineKind, text: impl Into<String>) -> Self {
        Self {
            number,
            kind,
            text: text.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffLineKind {
    Added,
    Removed,
    Context,
}

/// Typed observational data emitted by the CLI runtime for later TUI rendering.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TuiRuntimeEvent {
    TurnStarted,
    TurnEnded {
        status: TurnState,
        duration: Option<Duration>,
    },
    Usage(Usage),
    ToolStarted {
        call_id: String,
        name: String,
        input: String,
        parsed: ToolInput,
    },
    ToolEnded {
        call_id: String,
        duration: Option<Duration>,
        result: ToolResultState,
    },
    Diff {
        call_id: String,
        lines: Vec<DiffLine>,
    },
    TaskExecution {
        agent: String,
        event: TuiExecutionEvent,
    },
    SubagentExecution(TuiSubagentEvent),
    RestoredCompletedSubagent {
        id: u64,
        agent: String,
        task_summary: String,
        final_result: String,
        tool_uses: usize,
    },
    /// A one-line, already-sanitized message to surface outside the transcript
    /// — for example an MCP server failure discovered while building this
    /// turn's tools. `severity` decides how loudly a surface may render it.
    Notice {
        text: String,
        severity: NoticeSeverity,
    },
}

/// How much salience a [`TuiRuntimeEvent::Notice`] is owed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NoticeSeverity {
    /// Context the reader may ignore without losing anything.
    Info,
    /// Something the runtime could not do. It must never be rendered in a
    /// surface's lowest-salience style.
    Failure,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TuiSubagentEvent {
    pub id: u64,
    pub update: TuiSubagentUpdate,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TuiSubagentUpdate {
    Started {
        agent: String,
        task_summary: String,
        presentation: TuiExecutionState,
    },
    Reasoning(String),
    Text(String),
    ToolCall {
        call_id: String,
        name: String,
        input: String,
        parsed: ToolInput,
    },
    ToolResult {
        call_id: String,
        output: String,
        is_error: bool,
    },
    Error {
        kind: SubagentErrorKind,
        reference: Option<String>,
    },
    Terminal {
        status: SubagentStatus,
        final_result: String,
    },
}

impl TuiSubagentEvent {
    pub fn started(
        id: u64,
        agent: impl AsRef<str>,
        task_summary: impl AsRef<str>,
        presentation: TuiExecutionState,
    ) -> Self {
        Self {
            id,
            update: TuiSubagentUpdate::Started {
                agent: sanitize_projection(agent.as_ref()),
                task_summary: sanitize_projection(task_summary.as_ref()),
                presentation,
            },
        }
    }
    /// Records a child tool call with an unknown/default `parsed` payload.
    ///
    /// Prefer [`Self::tool_call_with_parsed`] when an accurate typed input is
    /// available at the call site; this constructor exists for callers that
    /// only need the raw name/input (e.g. most tests).
    pub fn tool_call(
        id: u64,
        call_id: impl AsRef<str>,
        name: impl AsRef<str>,
        input: impl AsRef<str>,
    ) -> Self {
        let name = name.as_ref();
        let input = input.as_ref();
        Self::tool_call_with_parsed(
            id,
            call_id,
            name,
            input,
            ToolInput::Other {
                name: name.to_owned(),
                raw: input.to_owned(),
            },
        )
    }

    /// Records a child tool call with a caller-supplied typed `parsed` input.
    pub fn tool_call_with_parsed(
        id: u64,
        call_id: impl AsRef<str>,
        name: impl AsRef<str>,
        input: impl AsRef<str>,
        parsed: ToolInput,
    ) -> Self {
        Self {
            id,
            update: TuiSubagentUpdate::ToolCall {
                call_id: sanitize_projection(call_id.as_ref()),
                name: sanitize_projection(name.as_ref()),
                input: sanitize_projection(input.as_ref()),
                parsed,
            },
        }
    }

    pub fn reasoning(id: u64, delta: impl AsRef<str>) -> Self {
        Self {
            id,
            update: TuiSubagentUpdate::Reasoning(sanitize_projection(delta.as_ref())),
        }
    }

    pub fn text(id: u64, delta: impl AsRef<str>) -> Self {
        Self {
            id,
            update: TuiSubagentUpdate::Text(sanitize_projection(delta.as_ref())),
        }
    }
    pub fn tool_result(
        id: u64,
        call_id: impl AsRef<str>,
        output: impl AsRef<str>,
        is_error: bool,
    ) -> Self {
        Self {
            id,
            update: TuiSubagentUpdate::ToolResult {
                call_id: sanitize_projection(call_id.as_ref()),
                output: sanitize_projection(output.as_ref()),
                is_error,
            },
        }
    }

    pub fn error(id: u64, kind: SubagentErrorKind) -> Self {
        Self {
            id,
            update: TuiSubagentUpdate::Error {
                kind,
                reference: None,
            },
        }
    }

    pub fn error_with_reference(
        id: u64,
        kind: SubagentErrorKind,
        reference: impl AsRef<str>,
    ) -> Self {
        let reference = reference.as_ref();
        let reference = (reference.len() == 8
            && reference
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
        .then(|| reference.to_owned());
        Self {
            id,
            update: TuiSubagentUpdate::Error { kind, reference },
        }
    }

    pub fn terminal(id: u64, status: SubagentStatus, final_result: impl AsRef<str>) -> Self {
        Self {
            id,
            update: TuiSubagentUpdate::Terminal {
                status,
                final_result: sanitize_projection(final_result.as_ref()),
            },
        }
    }
}

/// Withholds only the lines that match a credential marker. A document that merely documents a
/// credential name matches the marker set as readily as a leaked value does, so replacing the whole
/// projection would blank every unrelated line of it.
fn sanitize_projection(value: &str) -> String {
    let bounded = |value: &str| value.chars().take(256).collect::<String>();

    if !contains_credential_marker(value) {
        return bounded(value);
    }

    let withheld = value
        .lines()
        .map(|line| {
            if contains_credential_marker(line) {
                "[redacted]"
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    bounded(&withheld)
}

fn contains_credential_marker(value: &str) -> bool {
    const MARKERS: [&str; 6] = [
        "api_key",
        "authorization",
        "password",
        "secret",
        "token",
        "prompt:",
    ];

    let lower = value.to_ascii_lowercase();
    MARKERS.iter().any(|marker| lower.contains(marker))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TuiExecutionEvent {
    ForegroundStarted { id: u64 },
    BackgroundStarted { id: u64 },
    Backgrounded { id: u64 },
    Completed { id: u64 },
    Failed { id: u64 },
    Cancelled { id: u64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TuiExecutionState {
    ForegroundRunning,
    BackgroundRunning,
    CompletedRecent,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TuiExecution {
    pub id: u64,
    pub agent: String,
    pub state: TuiExecutionState,
    pub started_at: Duration,
    pub last_activity: Duration,
    pub terminal_at: Option<Duration>,
}

impl TuiExecution {
    pub const fn id(&self) -> u64 {
        self.id
    }

    pub const fn state(&self) -> TuiExecutionState {
        self.state
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolResultState {
    Success,
    Failure,
}

#[cfg(test)]
mod projection_tests {
    use super::sanitize_projection;

    /// A document that merely names a credential must stay readable in the subagent panel: only the
    /// matching line is withheld, because blanking the whole projection costs every unrelated line.
    #[test]
    fn projection_withholds_only_the_lines_matching_a_credential_marker() {
        let document = concat!(
            "# Agens\n",
            "\n",
            "Configure the provider before the first run.\n",
            "export OPENAI_API_KEY=\"sk-live-abcdef\"\n",
            "Then start the CLI.\n",
            "Never commit secret values.\n",
            "Finally, verify the install."
        );

        let sanitized = sanitize_projection(document);

        assert!(sanitized.contains("# Agens"), "{sanitized:?}");
        assert!(
            sanitized.contains("Configure the provider before the first run."),
            "{sanitized:?}"
        );
        assert!(sanitized.contains("Then start the CLI."), "{sanitized:?}");
        assert!(
            sanitized.contains("Finally, verify the install."),
            "{sanitized:?}"
        );

        assert!(!sanitized.contains("sk-live-abcdef"), "{sanitized:?}");
        assert!(!sanitized.contains("OPENAI_API_KEY"), "{sanitized:?}");
        assert!(!sanitized.contains("secret values"), "{sanitized:?}");
    }

    /// A projection that is nothing but a credential has no unrelated content to keep, so the whole
    /// single line is still withheld.
    #[test]
    fn single_line_credential_projection_is_withheld_entirely() {
        assert_eq!(sanitize_projection("token=result-secret"), "[redacted]");
    }

    /// Content with no marker keeps its exact bytes up to the projection budget.
    #[test]
    fn clean_projection_is_only_bounded() {
        assert_eq!(sanitize_projection("plain body\n"), "plain body\n");
        assert_eq!(sanitize_projection(&"x".repeat(300)).chars().count(), 256);
    }
}
