//! The runtime's error and exit-status contract.
//!
//! Shared by every layer that can fail, which is why it is not owned by any one
//! of them. `CliError` keeps its name for now; the rename to something that does
//! not claim a surface is a separate, mechanical change.

//! Exit-status and error types shared by every CLI command body, plus the
//! `CliError` constructors that translate domain failures into them.

use std::fmt;

use agens_core::{HeadlessTurnCancellation, HeadlessTurnError};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitStatus {
    Success,
    Failure,
    Usage,
    Configuration,
    Authentication,
    Unavailable,
}

impl ExitStatus {
    pub const fn code(self) -> u8 {
        match self {
            Self::Success => 0,
            Self::Failure => 1,
            Self::Usage => 2,
            Self::Configuration => 3,
            Self::Authentication => 4,
            Self::Unavailable => 5,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct CommandResult {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CliError {
    status: ExitStatus,
    pub category: &'static str,
    pub message: String,
    preformatted: bool,
}

impl CliError {
    pub fn usage(message: impl Into<String>) -> Self {
        Self::new(ExitStatus::Usage, "usage", message)
    }

    /// Wraps text clap has already fully rendered (its own `error: ` prefix
    /// and usage block). `error_result` emits `message` verbatim instead of
    /// wrapping it in the `error: {category}: {message}` envelope, which
    /// would otherwise double the `error: ` prefix.
    pub fn preformatted_usage(message: impl Into<String>) -> Self {
        Self {
            status: ExitStatus::Usage,
            category: "usage",
            message: message.into(),
            preformatted: true,
        }
    }

    pub fn configuration(message: impl Into<String>) -> Self {
        Self::new(ExitStatus::Configuration, "config", message)
    }

    pub fn authentication(message: impl Into<String>) -> Self {
        Self::new(ExitStatus::Authentication, "auth", message)
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(ExitStatus::Unavailable, "unavailable", message)
    }

    pub fn storage(message: impl Into<String>) -> Self {
        Self::new(ExitStatus::Failure, "store", message)
    }

    pub fn runtime(error: HeadlessTurnError) -> Self {
        let (status, category, message) = match error {
            HeadlessTurnError::Cancelled => (
                ExitStatus::Failure,
                "cancelled",
                "headless turn was cancelled",
            ),
            HeadlessTurnError::TimedOut => {
                (ExitStatus::Failure, "timeout", "headless turn timed out")
            }
            HeadlessTurnError::Authentication => (
                ExitStatus::Authentication,
                "auth",
                "ChatGPT credentials are unavailable or invalid",
            ),
            HeadlessTurnError::Provider => {
                (ExitStatus::Failure, "provider", "provider request failed")
            }
            HeadlessTurnError::ProviderRejected => (
                ExitStatus::Failure,
                "provider",
                "ChatGPT request was rejected",
            ),
            HeadlessTurnError::ProviderContext => (
                ExitStatus::Failure,
                "provider",
                "request exceeds the model context window",
            ),
            HeadlessTurnError::ProviderRateLimited => (
                ExitStatus::Failure,
                "provider",
                "ChatGPT request was rate limited",
            ),
            HeadlessTurnError::ProviderServer => {
                (ExitStatus::Failure, "provider", "ChatGPT service failed")
            }
            HeadlessTurnError::ProviderNetwork => {
                (ExitStatus::Failure, "provider", "network request failed")
            }
            HeadlessTurnError::ProviderProtocol => (
                ExitStatus::Failure,
                "provider",
                "ChatGPT response protocol failed",
            ),
            HeadlessTurnError::Permission => (
                ExitStatus::Failure,
                "permission",
                "permission evaluation failed",
            ),
            HeadlessTurnError::PermissionEvaluation => (
                ExitStatus::Failure,
                "permission",
                "permission target could not be evaluated; correct the tool arguments and retry",
            ),
            HeadlessTurnError::PermissionRequired => (
                ExitStatus::Failure,
                "permission",
                "permission approval is required",
            ),
            HeadlessTurnError::Tool => (ExitStatus::Failure, "tool", "tool execution failed"),
            HeadlessTurnError::Store => (
                ExitStatus::Failure,
                "store",
                "completed turn could not be saved",
            ),
            HeadlessTurnError::MaxIterations => (
                ExitStatus::Failure,
                "runtime",
                "headless turn reached the maximum iterations",
            ),
            HeadlessTurnError::State => (
                ExitStatus::Failure,
                "runtime",
                "headless turn entered an invalid state",
            ),
            HeadlessTurnError::TaskTerminal(terminal) => {
                (ExitStatus::Failure, "", terminal.message())
            }
        };
        Self::new(status, category, message)
    }

    pub fn new(status: ExitStatus, category: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            category,
            message: message.into(),
            preformatted: false,
        }
    }

    pub fn with_diagnostic_reference(mut self, reference: &str) -> Self {
        self.message.push_str(" [ref: ");
        self.message.push_str(reference);
        self.message.push(']');
        self
    }

    pub const fn status(&self) -> ExitStatus {
        self.status
    }

    pub const fn is_preformatted(&self) -> bool {
        self.preformatted
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.category.is_empty() {
            return formatter.write_str(&self.message);
        }

        write!(formatter, "{}: {}", self.category, self.message)
    }
}

impl std::error::Error for CliError {}

pub fn cancellation_result(cancellation: &HeadlessTurnCancellation) -> Result<(), CliError> {
    if cancellation.is_cancelled() {
        return Err(CliError::runtime(HeadlessTurnError::Cancelled));
    }
    if cancellation.is_expired() {
        return Err(CliError::runtime(HeadlessTurnError::TimedOut));
    }
    Ok(())
}
