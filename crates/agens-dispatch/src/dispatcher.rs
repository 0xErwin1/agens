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
/// A raw failure carries command output, filesystem paths and other host detail. Path
/// confinement refusals are rewritten to actionable guidance that never names a host path
/// (no `/tmp`, no home directory). The rewrite matches only the closed native reason
/// phrases, not a substring in free-form MCP text. Everything else passes through
/// redacted and bounded: real bash failures never carry a `"<tool>: "` prefix and used
/// to lose all compiler and test output when they were collapsed.
pub fn sanitized_native_tool_failure(content: &str) -> String {
    if let Some((tool, reason)) = content.split_once(": ")
        && !tool.contains('\n')
        && tool
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        if reason == "outside project root" || reason == "path is outside project root" {
            return format!(
                "{tool}: path is outside the project workspace; use a relative path under \
                 the project root (not /tmp or other absolute locations outside it)"
            );
        }
        if reason == "traversal is not allowed" {
            return format!(
                "{tool}: path traversal is not allowed; use a path under the project root"
            );
        }
        if reason == "must be a non-empty relative path" || reason == "must be a non-empty path" {
            return format!("{tool}: path must be a non-empty path under the project root");
        }
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

    /// Path-confinement refusals stay free of host paths but must tell the model
    /// *why* and *what to do* — a opaque "path validation failed" left agents
    /// retrying `/tmp` forever after Allow, which never relaxes confinement.
    #[test]
    fn path_validation_is_actionable_without_host_paths() {
        let outside = sanitized_native_tool_failure("path: outside project root");
        assert!(
            outside.contains("outside the project workspace"),
            "{outside}"
        );
        assert!(outside.contains("project root"), "{outside}");
        assert!(
            !outside.contains("/home") && !outside.contains("/Users"),
            "must not leak host home paths: {outside}"
        );

        let traversal = sanitized_native_tool_failure("write: traversal is not allowed");
        assert!(traversal.starts_with("write:"), "{traversal}");
        assert!(traversal.contains("traversal"), "{traversal}");
        assert!(!traversal.contains(".."), "{traversal}");

        let empty = sanitized_native_tool_failure("edit: must be a non-empty relative path");
        assert!(empty.starts_with("edit:"), "{empty}");
        assert!(empty.contains("non-empty"), "{empty}");
    }

    /// The path-validation guard must anchor on a genuine `"<tool>: <reason>"` shape. Real bash
    /// output is multi-line and routinely contains one of the three marker phrases as ordinary
    /// grep/diff/test-runner output (all three literals live in this crate's own source), so a
    /// `tool` part spanning multiple lines can never be a real tool name and must not trigger
    /// the guard — before this fix it destroyed the whole failure and fabricated a false claim.
    #[test]
    fn multiline_content_does_not_trigger_the_path_validation_guard() {
        let content = "[stdout]\ngrep hit: traversal is not allowed\n[stderr]\nreal error here\n[exit status: 2]\n";

        let converted = sanitized_native_tool_failure(content);

        assert_eq!(converted, content);
    }

    /// A one-line MCP `isError` body can mention "outside project root" without being a native
    /// confinement refusal. The guard must not treat `Error` as a tool name and replace the
    /// server's words with our workspace guidance.
    #[test]
    fn one_line_mcp_path_message_is_not_rewritten_as_a_confinement_refusal() {
        let content = "Error: the requested resource is outside project root";

        let converted = sanitized_native_tool_failure(content);

        assert_eq!(converted, content);
        assert!(
            !converted.contains("path is outside the project workspace"),
            "{converted}"
        );
    }

    /// `render_bash_result` puts `[stderr]` and `[exit status: N]` at the end of the content, so
    /// a bound that keeps only the head of a failure larger than the cap would deliver the model
    /// stdout and neither the error nor the exit code — exactly the case this rewrite exists to
    /// serve, since a failing `cargo build` easily exceeds the 16,384-char cap.
    #[test]
    fn large_bash_failure_keeps_stderr_and_exit_status() {
        let noisy_stdout = "line of build output\n".repeat(1_000);
        let content =
            format!("[stdout]\n{noisy_stdout}[stderr]\nreal error here\n[exit status: 2]\n");
        assert!(content.chars().count() > MAX_NATIVE_TOOL_FAILURE_CHARS);

        let converted = sanitized_native_tool_failure(&content);

        assert!(converted.chars().count() < content.chars().count());
        assert!(converted.contains("[truncated:"));
        assert!(converted.contains("real error here"));
        assert!(converted.contains("[exit status: 2]"));
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
