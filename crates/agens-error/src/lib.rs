//! The runtime's error and exit-status contract.
//!
//! Shared by every layer that can fail, which is why it is not owned by any one
//! of them. `CliError` keeps its name for now; the rename to something that does
//! not claim a surface is a separate, mechanical change.

//! Exit-status and error types shared by every CLI command body, plus the
//! `CliError` constructors that translate domain failures into them.

use std::fmt;

use agens_core::redaction::{bounded_detail, redact_credential_values};
use agens_core::{HeadlessTurnCancellation, HeadlessTurnError};

/// Caps rendered failure detail at 2,048 characters, mirroring
/// `SUBAGENT_RESULT_TRUNCATION_MARKER`'s visible-truncation contract
/// (`agens-session/src/turns.rs`).
const FAILURE_DETAIL_MAX_CHARS: usize = 2_048;

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
    detail: Option<String>,
    preformatted: bool,
    /// The runtime variant this error was built from, when it was built from
    /// one.
    ///
    /// Kept because the envelope erases it: `category` and `message` are what a
    /// person reads, and two failures a caller must treat differently — a
    /// rejected request and an exhausted context window — share a category and
    /// differ only in prose. A supervisor deciding whether a compaction would
    /// unblock the turn cannot make that decision on prose.
    runtime: Option<HeadlessTurnError>,
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
            detail: None,
            preformatted: true,
            runtime: None,
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
                "provider credentials are unavailable or invalid",
            ),
            HeadlessTurnError::Provider => {
                (ExitStatus::Failure, "provider", "provider request failed")
            }
            HeadlessTurnError::ProviderRejected => (
                ExitStatus::Failure,
                "provider",
                "provider request was rejected",
            ),
            HeadlessTurnError::ProviderContext => (
                ExitStatus::Failure,
                "provider",
                "request exceeds the model context window",
            ),
            HeadlessTurnError::ProviderHistoryBudget => (
                ExitStatus::Failure,
                "provider",
                "this session's history outgrew what one request can replay",
            ),
            HeadlessTurnError::ProviderRateLimited { .. } => (
                ExitStatus::Failure,
                "provider",
                "provider request was rate limited",
            ),
            HeadlessTurnError::ProviderServer => {
                (ExitStatus::Failure, "provider", "provider service failed")
            }
            HeadlessTurnError::ProviderNetwork => {
                (ExitStatus::Failure, "provider", "network request failed")
            }
            HeadlessTurnError::ProviderProtocol => (
                ExitStatus::Failure,
                "provider",
                "provider response protocol failed",
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
        Self {
            runtime: Some(error),
            ..Self::new(status, category, message)
        }
    }

    pub fn new(status: ExitStatus, category: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            category,
            message: message.into(),
            detail: None,
            preformatted: false,
            runtime: None,
        }
    }

    pub fn with_diagnostic_reference(mut self, reference: &str) -> Self {
        self.message.push_str(" [ref: ");
        self.message.push_str(reference);
        self.message.push(']');
        self
    }

    /// Attaches failure detail rendered by `Display` after the existing envelope, never into
    /// `message` itself — `has_error_message` (`agens-tui-app/src/router/mod.rs`) selects a
    /// TUI action by exact match on `message`, so appending here would silently break action
    /// selection. The detail is redacted for credential-shaped values and bounded before
    /// storage; `None` leaves `Display` unchanged.
    pub fn with_failure_detail(mut self, detail: Option<String>) -> Self {
        self.detail = detail.map(|detail| {
            bounded_detail(&redact_credential_values(&detail), FAILURE_DETAIL_MAX_CHARS)
        });
        self
    }

    pub const fn status(&self) -> ExitStatus {
        self.status
    }

    pub const fn is_preformatted(&self) -> bool {
        self.preformatted
    }

    /// The runtime variant behind this error, for a caller that has to act on
    /// which failure it is rather than describe it.
    pub const fn runtime_error(&self) -> Option<HeadlessTurnError> {
        self.runtime
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.category.is_empty() {
            formatter.write_str(&self.message)?;
        } else {
            write!(formatter, "{}: {}", self.category, self.message)?;
        }

        if let Some(detail) = &self.detail {
            write!(formatter, "\n{detail}")?;
        }

        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A runtime budget is not the model's context window. Saying it is sends
    /// the reader to shorten a prompt that was never too long — the session in
    /// the report was using a tenth of its window.
    #[test]
    fn a_runtime_budget_never_reports_itself_as_the_model_context_window() {
        let history = CliError::runtime(HeadlessTurnError::ProviderHistoryBudget).to_string();
        assert!(history.contains("history"), "{history:?}");
        assert!(!history.contains("context window"), "{history:?}");

        let context = CliError::runtime(HeadlessTurnError::ProviderContext).to_string();
        assert!(
            context.contains("exceeds the model context window"),
            "the remote signal keeps its own words: {context:?}"
        );
    }

    #[test]
    fn with_failure_detail_none_leaves_display_unchanged() {
        let error = CliError::runtime(HeadlessTurnError::Provider).with_failure_detail(None);

        assert_eq!(error.to_string(), "provider: provider request failed");
    }

    #[test]
    fn with_failure_detail_some_appends_after_the_envelope() {
        let error = CliError::runtime(HeadlessTurnError::ProviderRejected)
            .with_failure_detail(Some("HTTP 400 rejected model \"gpt-9-missing\"".to_owned()));

        let rendered = error.to_string();
        assert!(rendered.starts_with("provider: provider request was rejected"));
        assert!(rendered.contains("HTTP 400 rejected model \"gpt-9-missing\""));
    }

    #[test]
    fn with_failure_detail_redacts_a_credential_value_but_keeps_surrounding_text() {
        let error = CliError::runtime(HeadlessTurnError::Provider).with_failure_detail(Some(
            "Authorization: Bearer abcdefghijklmnopqrstuvwx failed".to_owned(),
        ));

        let rendered = error.to_string();
        assert!(!rendered.contains("abcdefghijklmnopqrstuvwx"));
        assert!(rendered.contains("Authorization: Bearer [redacted:"));
        assert!(rendered.contains("failed"));
    }

    #[test]
    fn with_failure_detail_keeps_absolute_paths_and_benign_detail_verbatim() {
        let error = CliError::runtime(HeadlessTurnError::Provider).with_failure_detail(Some(
            "reading /home/user/project/config.toml: request exceeds 128000 tokens".to_owned(),
        ));

        let rendered = error.to_string();
        assert!(rendered.contains("/home/user/project/config.toml"));
        assert!(rendered.contains("request exceeds 128000 tokens"));
    }

    #[test]
    fn with_failure_detail_bounds_over_cap_detail_with_a_visible_marker() {
        let long_detail = "d".repeat(4_096);
        let error =
            CliError::runtime(HeadlessTurnError::Provider).with_failure_detail(Some(long_detail));

        let rendered = error.to_string();
        assert!(rendered.contains("[truncated:"));
        assert!(
            rendered
                .chars()
                .filter(|&character| character == 'd')
                .count()
                < 4_096,
            "expected the detail to be bounded below its original length"
        );
    }

    #[test]
    fn preformatted_usage_carries_no_detail_by_default() {
        let error = CliError::preformatted_usage("clap already rendered this");

        assert_eq!(error.to_string(), "usage: clap already rendered this");
        assert!(error.is_preformatted());
    }
}
