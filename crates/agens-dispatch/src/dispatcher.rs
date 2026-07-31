//! The production `HeadlessToolDispatcher`: it executes only calls the
//! permission layer already authorized, and it rewrites a failure before the
//! model sees it.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use agens_core::{
    HeadlessToolCall, HeadlessToolDispatcher, HeadlessToolOutput, HeadlessTurnCancellation,
    HeadlessTurnPortError,
};
use agens_permissions::{AllowedNativeCall, SharedToolDispatcher};
use agens_tools::{NATIVE_FILESYSTEM_FAILURE_REASONS, ToolExecutionContext, ToolOutput};

pub struct ProductionToolDispatcher {
    dispatcher: SharedToolDispatcher,
    allowed: Arc<Mutex<BTreeMap<String, AllowedNativeCall>>>,
}

impl ProductionToolDispatcher {
    pub fn new(
        dispatcher: SharedToolDispatcher,
        allowed: Arc<Mutex<BTreeMap<String, AllowedNativeCall>>>,
    ) -> Self {
        Self {
            dispatcher,
            allowed,
        }
    }
}

impl HeadlessToolDispatcher for ProductionToolDispatcher {
    fn dispatch(
        &mut self,
        call: HeadlessToolCall,
        cancellation: &HeadlessTurnCancellation,
    ) -> impl std::future::Future<Output = Result<HeadlessToolOutput, HeadlessTurnPortError>> + Send
    {
        let allowed = self
            .allowed
            .lock()
            .map_err(|_| HeadlessTurnPortError::Tool)
            .and_then(|mut allowed| {
                let allowed_call = allowed.get(&call.id).ok_or(HeadlessTurnPortError::Tool)?;

                if allowed_call.name != call.name || allowed_call.input != call.input {
                    return Err(HeadlessTurnPortError::Tool);
                }

                allowed.remove(&call.id).ok_or(HeadlessTurnPortError::Tool)
            });
        let output = allowed.and_then(|allowed| {
            let result = self
                .dispatcher
                .lock()
                .map_err(|_| HeadlessTurnPortError::Tool)?
                .execute(
                    allowed.handle,
                    &ToolExecutionContext::from_headless_adapter(cancellation.adapter_view()),
                );
            headless_execution_result(result)
        });
        std::future::ready(output)
    }
}

fn headless_output(output: ToolOutput) -> Result<HeadlessToolOutput, HeadlessTurnPortError> {
    let facts = output.facts().cloned();
    let content = if output.terminal().is_some() {
        output.content
    } else if output.is_error {
        sanitized_native_tool_failure(&output.content)
    } else {
        output.content
    };

    Ok(HeadlessToolOutput {
        content,
        is_error: output.is_error,
        facts,
    })
}

/// Rewrites a native-tool failure into something the model may see.
///
/// A raw failure carries filesystem paths, command output and other host
/// detail. Both the tool name and the reason must come from a closed set, so a
/// new tool or a new error string degrades to the generic message rather than
/// leaking by default.
pub fn sanitized_native_tool_failure(content: &str) -> String {
    let Some((tool, reason)) = content.split_once(": ") else {
        return "tool execution failed".to_owned();
    };
    if !matches!(
        tool,
        "read"
            | "list"
            | "search"
            | "glob"
            | "grep"
            | "write"
            | "edit"
            | "bash"
            | "webfetch"
            | "file picker"
    ) {
        return "tool execution failed".to_owned();
    }

    let safe_reason = matches!(
        reason,
        "operation timed out" | "cancelled" | "invalid regex" | "invalid glob pattern"
    ) || NATIVE_FILESYSTEM_FAILURE_REASONS.contains(&reason)
        || [
            ("entry limit of ", " exceeded"),
            ("result limit of ", " exceeded"),
            ("traversal depth limit of ", " exceeded"),
        ]
        .into_iter()
        .any(|(prefix, suffix)| {
            reason
                .strip_prefix(prefix)
                .and_then(|value| value.strip_suffix(suffix))
                .is_some_and(|value| {
                    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
                })
        });
    if safe_reason {
        format!("{tool}: {reason}")
    } else if reason.contains("outside project root")
        || reason.contains("traversal is not allowed")
        || reason.contains("must be a non-empty relative path")
    {
        format!("{tool}: path validation failed")
    } else {
        "tool execution failed".to_owned()
    }
}

fn headless_execution_result(
    result: Result<ToolOutput, agens_core::Error>,
) -> Result<HeadlessToolOutput, HeadlessTurnPortError> {
    match result {
        Ok(output) => headless_output(output),
        Err(agens_core::Error::Tool(message)) if message == "mcp operation timed out" => {
            Ok(HeadlessToolOutput::failure("tool operation timed out"))
        }
        Err(agens_core::Error::Extension(message))
            if message == "mcp tool infrastructure failure" =>
        {
            Ok(HeadlessToolOutput::failure("tool infrastructure failure"))
        }
        Err(error) => Err(headless_tool_error(error)),
    }
}

fn headless_tool_error(error: agens_core::Error) -> HeadlessTurnPortError {
    match error {
        agens_core::Error::Cancelled => HeadlessTurnPortError::Cancelled,
        agens_core::Error::Tool(_) | agens_core::Error::Extension(_) => HeadlessTurnPortError::Tool,
        _ => HeadlessTurnPortError::Tool,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agens_core::{HeadlessTaskTerminal, ToolOutcome, ToolResultFacts};

    #[test]
    fn mcp_failures_become_recoverable_tool_results() {
        for (error, expected) in [
            (
                agens_core::Error::Tool("mcp operation timed out".into()),
                "tool operation timed out",
            ),
            (
                agens_core::Error::Extension("mcp tool infrastructure failure".into()),
                "tool infrastructure failure",
            ),
        ] {
            let converted = headless_execution_result(Err(error))
                .expect("an MCP failure should not terminate the parent turn");

            assert!(converted.is_error);
            assert_eq!(converted.content, expected);
        }

        assert_eq!(
            headless_execution_result(Err(agens_core::Error::Cancelled)),
            Err(HeadlessTurnPortError::Cancelled)
        );
    }

    #[test]
    fn unavailable_task_inputs_return_a_tool_error_the_parent_can_recover_from() {
        for terminal in [
            HeadlessTaskTerminal::Cancelled,
            HeadlessTaskTerminal::TimedOut,
            HeadlessTaskTerminal::AgentUnavailable,
            HeadlessTaskTerminal::ModelUnavailable,
            HeadlessTaskTerminal::SkillUnavailable,
            HeadlessTaskTerminal::IterationLimit,
            HeadlessTaskTerminal::InputLimit,
            HeadlessTaskTerminal::OutputLimit,
            HeadlessTaskTerminal::ConcurrencyLimit,
            HeadlessTaskTerminal::ProviderFailure,
            HeadlessTaskTerminal::ChildFailure,
        ] {
            let converted = headless_output(ToolOutput::task_terminal(terminal))
                .expect("recoverable task preflight failures must reach the parent model");

            assert!(converted.is_error);
            assert_eq!(converted.content, terminal.message());
        }
    }

    /// A sanitized failure still has to carry its facts: the surface reports the
    /// exit code from `facts`, not from the message, so redacting the message
    /// must not cost the caller the structured result.
    #[test]
    fn sanitized_tool_failure_keeps_its_facts() {
        let output = ToolOutput::failure("bash: exit 127").with_facts(ToolResultFacts::Bash {
            outcome: ToolOutcome::Failed,
            exit_code: Some(127),
        });

        let converted = headless_output(output).expect("failing tool output is not terminal");

        assert_eq!(
            converted.content,
            sanitized_native_tool_failure("bash: exit 127")
        );
        assert_eq!(
            converted.facts,
            Some(ToolResultFacts::Bash {
                outcome: ToolOutcome::Failed,
                exit_code: Some(127)
            })
        );
    }
}
