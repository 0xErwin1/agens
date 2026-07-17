use std::fmt;

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
