use std::{
    fmt,
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use globset::{GlobBuilder, GlobMatcher};

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionMetadata {
    pub id: i64,
    pub project: String,
    pub title: String,
    pub active_agent: String,
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
    InvalidResumability,
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
    ToolCallRequested {
        id: String,
        name: String,
        input: String,
    },
    ToolResult(MessagePart),
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
                        coordinator.accept_tool_result(&tool_call_id, content, is_error)
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
            events: self.events.clone(),
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

    pub fn accept_tool_result(
        &mut self,
        tool_call_id: &str,
        content: String,
        is_error: bool,
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
                content,
                is_error,
            }));

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
}

impl HeadlessToolOutput {
    pub fn success(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    pub fn failure(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeadlessTurnPortError {
    Cancelled,
    TimedOut,
    Authentication,
    Provider,
    Permission,
    Tool,
}

pub trait TurnProvider {
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
    Permission,
    PermissionRequired,
    Tool,
    Store,
    MaxIterations,
    State,
}

impl fmt::Display for HeadlessTurnError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Cancelled => "turn cancelled",
            Self::TimedOut => "turn timed out",
            Self::Authentication => "authentication required",
            Self::Provider => "provider operation failed",
            Self::Permission => "permission operation failed",
            Self::PermissionRequired => "permission required",
            Self::Tool => "tool operation failed",
            Self::Store => "completed turn could not be saved",
            Self::MaxIterations => "turn reached the maximum iterations",
            Self::State => "invalid headless turn state",
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
    )
    .await
}

pub async fn run_headless_turn_with_progress(
    provider: &mut impl TurnProvider,
    permission_gate: &mut impl HeadlessPermissionGate,
    permission_resolver: &mut impl HeadlessPermissionResolver,
    dispatcher: &mut impl HeadlessToolDispatcher,
    repository: &mut impl CompletedTurnRepository,
    cancellation: &HeadlessTurnCancellation,
    progress: Option<&TurnProgressSink>,
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
    )
    .await
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
) -> Result<CompletedTurnSnapshot, HeadlessTurnError> {
    let mut coordinator = TurnCoordinator::new();
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

            return CompletedTurnSnapshot::from_persisted_events(coordinator.events().to_vec())
                .map_err(|_| HeadlessTurnError::State);
        }

        let mut preflight = Vec::with_capacity(tool_calls.len());

        for call in tool_calls {
            check_cancelled(&mut coordinator, cancellation)?;
            flush_progress(&coordinator, progress, &mut progress_cursor);

            let decision = permission_gate
                .evaluate(&call, cancellation)
                .await
                .map_err(|error| {
                    finish_port_error(&mut coordinator, error, HeadlessTurnError::Permission)
                })?;
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

            preflight.push((call, decision));
        }

        for (call, decision) in preflight {
            check_cancelled(&mut coordinator, cancellation)?;
            flush_progress(&coordinator, progress, &mut progress_cursor);

            let output = match decision {
                PermissionDecision::Allow => dispatcher
                    .dispatch(call.clone(), cancellation)
                    .await
                    .map_err(|error| {
                        finish_port_error(&mut coordinator, error, HeadlessTurnError::Tool)
                    })?,
                PermissionDecision::Deny => HeadlessToolOutput::failure("permission denied"),
                PermissionDecision::Ask => return Err(permission_required(&mut coordinator)),
            };

            coordinator
                .accept_tool_result(&call.id, output.content, output.is_error)
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
        .map_err(|error| finish_port_error(coordinator, error, HeadlessTurnError::Permission))
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

    if coordinator.fail().is_err() {
        HeadlessTurnError::State
    } else {
        failure
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
    pub fn glob(pattern: impl Into<String>) -> Result<Self, PermissionPatternError> {
        ValidatedPermissionGlob::new(pattern.into()).map(Self::Glob)
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
}

#[derive(Clone, Debug)]
pub struct ValidatedPermissionGlob {
    pattern: String,
    matcher: GlobMatcher,
}

impl ValidatedPermissionGlob {
    fn new(pattern: String) -> Result<Self, PermissionPatternError> {
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
            .literal_separator(true)
            .build()
            .map_err(|_| PermissionPatternError::InvalidGlob {
                pattern: pattern.clone(),
            })?
            .compile_matcher();

        Ok(Self { pattern, matcher })
    }

    fn matches(&self, value: &str) -> bool {
        value.len() <= MAX_PERMISSION_TARGET_BYTES && self.matcher.is_match(value)
    }
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
}

impl PermissionRequest {
    pub fn new(
        project: impl Into<String>,
        tool: impl Into<String>,
        target: impl Into<String>,
        access: ToolAccess,
    ) -> Self {
        Self {
            project: project.into(),
            tool: tool.into(),
            target: PermissionTarget::native(target).project(),
            access,
            outside_worktree: false,
        }
    }

    pub fn with_target(
        project: impl Into<String>,
        tool: impl Into<String>,
        target: PermissionTarget,
        access: ToolAccess,
    ) -> Self {
        Self {
            project: project.into(),
            tool: tool.into(),
            target: target.project(),
            access,
            outside_worktree: false,
        }
    }

    pub fn outside_worktree(mut self) -> Self {
        self.outside_worktree = true;
        self
    }
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

        project_matches && self.tool.matches(&request.tool) && self.target.matches(&request.target)
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
            && self.target.matches(&request.target)
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
        }
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
            if let SafetyPredicate::GlobalDeny(deny) = predicate {
                normalize_tool_pattern(&mut deny.tool, &aliases);
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
        if self.safety_predicates.iter().any(|predicate| {
            matches!(predicate, SafetyPredicate::WorktreeEscape) && request.outside_worktree
                || matches!(predicate, SafetyPredicate::ChatWrite)
                    && self.mode == PermissionMode::Chat
                    && request.access == ToolAccess::Write
                || matches!(predicate, SafetyPredicate::GlobalDeny(global_deny)
                    if global_deny.tool.matches(&request.tool)
                        && global_deny.target.matches(&request.target))
        }) {
            return PermissionDecision::Deny;
        }

        let decision = self
            .static_rules
            .iter()
            .filter(|rule| rule.matches(request))
            .map(|rule| rule.decision)
            .chain(
                project_grants
                    .iter()
                    .filter(|grant| grant.matches(request))
                    .map(|grant| grant.decision),
            )
            .chain(
                session_grants
                    .iter()
                    .filter(|grant| grant.matches(request))
                    .map(|grant| grant.decision),
            )
            .last()
            .unwrap_or(PermissionDecision::Ask);

        Self::resolve_ask(decision, session)
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
    use super::PermissionPattern;

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
}
