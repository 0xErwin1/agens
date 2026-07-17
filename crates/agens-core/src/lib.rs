use std::{fmt, future::Future};

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
}

impl PermissionPattern {
    fn matches(&self, value: &str) -> bool {
        match self {
            Self::Any => true,
            Self::Exact(expected) => expected == value,
        }
    }
}

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
    pub decision: PermissionDecision,
    pub tool: PermissionPattern,
    pub target: PermissionPattern,
}

impl PermissionRule {
    pub const fn new(
        scope: PermissionScope,
        decision: PermissionDecision,
        tool: PermissionPattern,
        target: PermissionPattern,
    ) -> Self {
        Self {
            scope,
            decision,
            tool,
            target,
        }
    }

    fn matches(&self, request: &PermissionRequest) -> bool {
        self.tool.matches(&request.tool) && self.target.matches(&request.target)
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
            return decision;
        }

        if let Some(decision) = Self::grant_decision(project_grants, request) {
            return decision;
        }

        if session.temporary_bypass {
            PermissionDecision::Allow
        } else {
            PermissionDecision::Ask
        }
    }

    fn static_decision(&self, request: &PermissionRequest) -> Option<PermissionDecision> {
        if self.matches_static(PermissionScope::Global, PermissionDecision::Deny, request)
            || self.matches_static(PermissionScope::Project, PermissionDecision::Deny, request)
        {
            return Some(PermissionDecision::Deny);
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
