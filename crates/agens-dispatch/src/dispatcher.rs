//! The production `HeadlessToolDispatcher`: it executes only calls the
//! permission layer already authorized, and it rewrites a failure before the
//! model sees it.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use agens_core::redaction::{bounded_detail, redact_absolute_paths, redact_credential_values};
use agens_core::{
    HeadlessToolCall, HeadlessToolDispatcher, HeadlessToolOutput, HeadlessTurnCancellation,
    HeadlessTurnPortError,
};
use agens_permissions::{AllowedNativeCall, SharedToolDispatcher};
use agens_tools::{ToolExecutionContext, ToolOutput};

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

/// This is the model-visible sink (`MessagePart::ToolResult.content`): both credential
/// values and host filesystem paths are withheld, per the two-audience rule.
const MAX_NATIVE_TOOL_FAILURE_CHARS: usize = 16_384;

/// Rewrites a native-tool failure into something the model may see.
///
/// A raw failure carries command output, filesystem paths and other host detail. Everything
/// except the path-validation reasons below passes through, redacted and bounded, instead of
/// collapsing to a generic message: real bash failures never carry a `"<tool>: "` prefix and
/// used to lose all compiler and test output as a result.
pub fn sanitized_native_tool_failure(content: &str) -> String {
    if let Some((tool, reason)) = content.split_once(": ")
        && (reason.contains("outside project root")
            || reason.contains("traversal is not allowed")
            || reason.contains("must be a non-empty relative path"))
    {
        return format!("{tool}: path validation failed");
    }

    bounded_detail(
        &redact_absolute_paths(&redact_credential_values(content)),
        MAX_NATIVE_TOOL_FAILURE_CHARS,
    )
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
        for error in [
            agens_core::Error::Tool("mcp operation timed out ".into()),
            agens_core::Error::Tool("other tool failure".into()),
            agens_core::Error::Extension("mcp tool infrastructure failure!".into()),
            agens_core::Error::Extension("other extension failure".into()),
        ] {
            assert_eq!(
                headless_execution_result(Err(error)),
                Err(HeadlessTurnPortError::Tool)
            );
        }
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
    ///
    /// The fixture is the real `render_bash_result` shape
    /// (`agens-tools/src/lib.rs:6226-6260`), not the `"bash: exit 127"` shape production
    /// never emits.
    #[test]
    fn sanitized_tool_failure_keeps_its_facts() {
        let content =
            "[stdout]\nbuilding...\n[stderr]\nerror: could not compile\n[exit status: 127]\n";
        let output = ToolOutput::failure(content).with_facts(ToolResultFacts::Bash {
            outcome: ToolOutcome::Failed,
            exit_code: Some(127),
        });

        let converted = headless_output(output).expect("failing tool output is not terminal");

        assert_eq!(converted.content, sanitized_native_tool_failure(content));
        assert_eq!(
            converted.facts,
            Some(ToolResultFacts::Bash {
                outcome: ToolOutcome::Failed,
                exit_code: Some(127)
            })
        );
    }

    /// Real bash failures never carry a `"<tool>: "` prefix; they are always
    /// `[stdout]/[stderr]/[exit status: N]`. Before this change every such failure collapsed
    /// to the generic `"tool execution failed"`, losing all compiler and test output.
    #[test]
    fn real_bash_failure_reaches_the_model() {
        let content = "[stdout]\nrunning tests...\n[stderr]\nassertion failed: left == right\n[exit status: 101]\n";
        let output = ToolOutput::failure(content);

        let converted = headless_output(output).expect("failing tool output is not terminal");

        assert!(converted.is_error);
        assert_ne!(converted.content, "tool execution failed");
        assert!(converted.content.contains("[exit status: 101]"));
        assert!(
            converted
                .content
                .contains("assertion failed: left == right")
        );
    }

    /// MCP `isError` server text (`map_call_result`, `agens-tools/src/lib.rs:3007-3022`)
    /// already carries the server's own explanation; the dispatcher must forward it, bounded
    /// and redacted, rather than discard it behind the generic message.
    #[test]
    fn mcp_server_error_text_survives() {
        let server_text = "remote tool rejected the call: quota exceeded for this workspace";
        let output = ToolOutput::failure(server_text);

        let converted = headless_output(output).expect("failing tool output is not terminal");

        assert!(converted.is_error);
        assert_ne!(converted.content, "tool execution failed");
        assert!(
            converted
                .content
                .contains("quota exceeded for this workspace")
        );
    }

    /// Path-validation reasons stay a closed, generic message with no host path — the one
    /// evidenced threat this rewrite still guards against.
    #[test]
    fn path_validation_stays_generic() {
        for content in [
            "path: outside project root",
            "read: outside project root",
            "write: traversal is not allowed",
            "edit: must be a non-empty relative path",
        ] {
            let converted = sanitized_native_tool_failure(content);

            let (tool, _) = content.split_once(": ").expect("fixture has a tool prefix");
            assert_eq!(converted, format!("{tool}: path validation failed"));
            assert!(!converted.contains('/'));
        }
    }

    /// This is the model-visible sink, so absolute host paths must be withheld even though the
    /// rest of the failure passes through.
    #[test]
    fn absolute_host_path_is_withheld() {
        let content = "read: /home/user/project/.env: permission denied";
        let output = ToolOutput::failure(content);

        let converted = headless_output(output).expect("failing tool output is not terminal");

        assert!(converted.is_error);
        assert!(!converted.content.contains("/home/user/project"));
        assert!(converted.content.contains("[path]"));
        assert!(converted.content.contains("permission denied"));
    }
}
