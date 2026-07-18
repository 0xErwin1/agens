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
    Provider,
    Permission,
    Tool,
    Store,
    State,
}

impl fmt::Display for HeadlessTurnError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Cancelled => "turn cancelled",
            Self::TimedOut => "turn timed out",
            Self::Provider => "provider operation failed",
            Self::Permission => "permission operation failed",
            Self::Tool => "tool operation failed",
            Self::Store => "completed turn could not be saved",
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
    let mut coordinator = TurnCoordinator::new();
    coordinator.begin().map_err(|_| HeadlessTurnError::State)?;

    loop {
        check_cancelled(&mut coordinator, cancellation)?;

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

        coordinator
            .finish_provider_iteration()
            .map_err(|_| fail_state(&mut coordinator))?;

        if coordinator.state() == TurnState::Completed {
            coordinator
                .persist_completed_turn(repository)
                .await
                .map_err(|_| HeadlessTurnError::Store)?;

            return CompletedTurnSnapshot::from_persisted_events(coordinator.events().to_vec())
                .map_err(|_| HeadlessTurnError::State);
        }

        for call in tool_calls {
            check_cancelled(&mut coordinator, cancellation)?;

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

            let output = match decision {
                PermissionDecision::Allow => dispatcher
                    .dispatch(call.clone(), cancellation)
                    .await
                    .map_err(|error| {
                        finish_port_error(&mut coordinator, error, HeadlessTurnError::Tool)
                    })?,
                PermissionDecision::Deny => HeadlessToolOutput::failure("permission denied"),
                PermissionDecision::Ask => return Err(fail_state(&mut coordinator)),
            };
            check_cancelled(&mut coordinator, cancellation)?;

            coordinator
                .accept_tool_result(&call.id, output.content, output.is_error)
                .map_err(|_| fail_state(&mut coordinator))?;
        }
    }
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
            target: target.into(),
            access,
        }
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
pub struct PermissionPolicy {
    mode: PermissionMode,
    static_rules: Vec<PermissionRule>,
}

impl PermissionPolicy {
    pub fn new(mode: PermissionMode, static_rules: Vec<PermissionRule>) -> Self {
        Self { mode, static_rules }
    }

    pub fn evaluate(
        &self,
        request: &PermissionRequest,
        project_grants: &[ProjectPermissionGrant],
        session: &PermissionSession,
    ) -> PermissionDecision {
        // Global denials are evaluated outside ordinary conflicts so no later input can weaken them.
        if self.matches_static(PermissionScope::Global, PermissionDecision::Deny, request) {
            return PermissionDecision::Deny;
        }

        if self.mode == PermissionMode::Chat && request.access == ToolAccess::Write {
            return PermissionDecision::Deny;
        }

        if let Some(decision) = self.static_decision(request) {
            return Self::resolve_ask(decision, session);
        }

        if let Some(decision) = Self::grant_decision(project_grants, request) {
            return Self::resolve_ask(decision, session);
        }

        Self::resolve_ask(PermissionDecision::Ask, session)
    }

    fn static_decision(&self, request: &PermissionRequest) -> Option<PermissionDecision> {
        if self.matches_static(PermissionScope::Global, PermissionDecision::Deny, request)
            || self.matches_static(PermissionScope::Project, PermissionDecision::Deny, request)
        {
            return Some(PermissionDecision::Deny);
        }

        if self.matches_static(PermissionScope::Global, PermissionDecision::Ask, request)
            || self.matches_static(PermissionScope::Project, PermissionDecision::Ask, request)
        {
            return Some(PermissionDecision::Ask);
        }

        if self.matches_static(PermissionScope::Global, PermissionDecision::Allow, request)
            || self.matches_static(PermissionScope::Project, PermissionDecision::Allow, request)
        {
            return Some(PermissionDecision::Allow);
        }

        None
    }

    fn matches_static(
        &self,
        scope: PermissionScope,
        decision: PermissionDecision,
        request: &PermissionRequest,
    ) -> bool {
        self.static_rules
            .iter()
            .any(|rule| rule.scope == scope && rule.decision == decision && rule.matches(request))
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

    fn grant_decision(
        project_grants: &[ProjectPermissionGrant],
        request: &PermissionRequest,
    ) -> Option<PermissionDecision> {
        if project_grants
            .iter()
            .any(|grant| grant.decision == PermissionDecision::Deny && grant.matches(request))
        {
            return Some(PermissionDecision::Deny);
        }

        project_grants
            .iter()
            .any(|grant| grant.decision == PermissionDecision::Allow && grant.matches(request))
            .then_some(PermissionDecision::Allow)
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
