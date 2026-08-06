//! Where a permission question reaches a person.
//!
//! Prompting is a surface: it renders for a human and waits on one. It lives
//! in a different crate from the policy so that policy never names a surface type,
//! and only reaches a person through the `PermissionPrompter` port it owns.

use std::io::{IsTerminal, Write};
use std::sync::mpsc::Receiver;

use agens_core::{HeadlessTurnCancellation, HeadlessTurnPortError};
use agens_tui::{PromptOrigin, TuiPermissionBridge, TuiPermissionReply, TuiPermissionRequest};

use agens_permissions::{
    PermissionPromptAnswer, PermissionPromptContext, PermissionPrompter, sanitize_permission_target,
};

pub struct TtyPermissionPrompter;

impl PermissionPrompter for TtyPermissionPrompter {
    fn prompt(
        &mut self,
        context: &PermissionPromptContext,
        _: &HeadlessTurnCancellation,
    ) -> Result<PermissionPromptAnswer, HeadlessTurnPortError> {
        if !std::io::stdin().is_terminal() {
            return Ok(PermissionPromptAnswer::DenyOnce);
        }

        eprint!("{}", render_permission_prompt(context));
        std::io::stderr()
            .flush()
            .map_err(|_| HeadlessTurnPortError::Permission)?;

        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .map_err(|_| HeadlessTurnPortError::Permission)?;

        Ok(parse_permission_prompt_answer(&answer).unwrap_or(PermissionPromptAnswer::DenyOnce))
    }
}

/// The terminal UI's implementation of the permission port. Each surface owns
/// its own, so the engine never chooses between them.
/// The second field names the delegated execution asking, or `None` for the
/// main thread.
pub struct TuiPermissionPrompter(pub TuiPermissionBridge, pub Option<PromptOrigin>);

pub fn production_tui_permission_bridge() -> (TuiPermissionBridge, Receiver<TuiPermissionRequest>) {
    TuiPermissionBridge::channel()
}

impl PermissionPrompter for TuiPermissionPrompter {
    fn prompt(
        &mut self,
        context: &PermissionPromptContext,
        cancellation: &HeadlessTurnCancellation,
    ) -> Result<PermissionPromptAnswer, HeadlessTurnPortError> {
        let tool = agens_core::bare_tool_name(&context.tool_identity).into_owned();
        let target =
            sanitize_permission_target(&context.tool_identity, &context.target_identifier);
        let access = format!("{:?}", context.access);
        let reason = (!context.reason.is_empty()).then(|| context.reason.clone());

        match self.0.wait_for_reply(
            tool,
            target,
            access,
            reason,
            self.1.clone(),
            cancellation,
        ) {
            TuiPermissionReply::AllowOnce => Ok(PermissionPromptAnswer::AllowOnce),
            TuiPermissionReply::AllowAlways => Ok(PermissionPromptAnswer::AllowAlways),
            TuiPermissionReply::DenyOnce => Ok(PermissionPromptAnswer::DenyOnce),
            TuiPermissionReply::DenyAlways => Ok(PermissionPromptAnswer::DenyAlways),
            TuiPermissionReply::Cancelled => Err(HeadlessTurnPortError::Cancelled),
            TuiPermissionReply::DeadlineExpired => Err(HeadlessTurnPortError::TimedOut),
        }
    }
}

pub fn parse_permission_prompt_answer(value: &str) -> Option<PermissionPromptAnswer> {
    match value.trim().to_ascii_lowercase().as_str() {
        "a" | "allow-once" | "allow once" => Some(PermissionPromptAnswer::AllowOnce),
        "always" | "allow-always" | "allow always" => Some(PermissionPromptAnswer::AllowAlways),
        "d" | "deny-once" | "deny once" => Some(PermissionPromptAnswer::DenyOnce),
        "deny-always" | "deny always" => Some(PermissionPromptAnswer::DenyAlways),
        "c" | "cancel" => Some(PermissionPromptAnswer::Cancel),
        _ => None,
    }
}

/// Renders the question a person answers.
///
/// The tool is named the way a rule names it rather than by the dispatcher's
/// own identity for it, because the person answering has to recognize the tool
/// and may want to write a rule about it afterwards. That holds for a remote
/// tool as much as for a native one: both identities are length-headed and
/// neither is a spelling anyone writes down. The identity keeps deciding the
/// grant; it is only unfit to be read.
pub fn render_permission_prompt(context: &PermissionPromptContext) -> String {
    format!(
        "Permission required for {} ({:?})\nTarget: {}\n[a]llow once, allow [always], [d]eny once, deny [always], or [c]ancel: ",
        agens_core::bare_tool_name(&context.tool_identity),
        context.access,
        sanitize_permission_target(&context.tool_identity, &context.target_identifier),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use agens_core::{
        HeadlessPermissionGate, HeadlessPermissionResolver, HeadlessToolCall,
        HeadlessToolDispatcher, HeadlessToolOutput, HeadlessTurnPortError, PermissionDecision,
        PermissionMode, PermissionPattern, PermissionPolicy, PermissionRule, PermissionSession,
    };
    use agens_permissions::*;

    use super::*;
    use agens_dispatch::ProductionToolDispatcher;
    use agens_store::PermissionGrantStore;
    use agens_tools::{DispatchTool, ToolDispatcher, ToolExecutionContext, ToolOutput};

    #[test]
    fn production_prompt_decisions_authorize_only_allowed_calls() {
        struct RecordingTool(Arc<std::sync::atomic::AtomicUsize>);

        impl DispatchTool for RecordingTool {
            fn permission_target(
                &self,
                arguments: &serde_json::Value,
            ) -> Result<String, agens_core::Error> {
                arguments
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .map(ToOwned::to_owned)
                    .ok_or_else(|| agens_core::Error::Tool("missing path".into()))
            }

            fn execute(
                &mut self,
                _: &ToolExecutionContext,
                _: serde_json::Value,
            ) -> Result<ToolOutput, agens_core::Error> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(ToolOutput::success("executed"))
            }
        }

        fn run_ready<T>(
            future: impl std::future::Future<Output = Result<T, HeadlessTurnPortError>>,
        ) -> Result<T, HeadlessTurnPortError> {
            let mut future = std::pin::pin!(future);
            let context = &mut std::task::Context::from_waker(std::task::Waker::noop());

            match future.as_mut().poll(context) {
                std::task::Poll::Ready(result) => result,
                std::task::Poll::Pending => {
                    panic!("production permission ports must complete synchronously")
                }
            }
        }

        for (answer, expected_executions, expected_grants) in [
            (PermissionPromptAnswer::AllowOnce, 1, 0),
            (PermissionPromptAnswer::AllowAlways, 2, 1),
            (PermissionPromptAnswer::DenyOnce, 0, 0),
            (PermissionPromptAnswer::DenyAlways, 0, 1),
            (PermissionPromptAnswer::Cancel, 0, 0),
        ] {
            let directory = std::env::temp_dir().join(format!(
                "agens-production-permission-{}-{:?}",
                std::process::id(),
                answer
            ));
            let _ = std::fs::remove_dir_all(&directory);

            let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let dispatcher = Arc::new(Mutex::new(ToolDispatcher::new()));
            dispatcher
                .lock()
                .expect("dispatcher lock should be available")
                .register_native(
                    "native::read",
                    agens_core::ToolAccess::ReadOnly,
                    RecordingTool(Arc::clone(&executions)),
                )
                .expect("recording tool should register");

            let grants = Arc::new(Mutex::new(Vec::new()));
            let allowed = Arc::new(Mutex::new(BTreeMap::new()));
            let prompts = Arc::new(Mutex::new(BTreeMap::new()));
            let policy = PermissionPolicy::new(
                PermissionMode::Edit,
                vec![PermissionRule::global(
                    PermissionDecision::Ask,
                    PermissionPattern::Exact("native::read".into()),
                    PermissionPattern::Exact("notes.md".into()),
                )],
            );
            let call = HeadlessToolCall {
                id: "current".into(),
                name: "native::read".into(),
                input: r#"{"path":"notes.md"}"#.into(),
            };
            let cancellation = HeadlessTurnCancellation::new();
            let mut gate = ProductionPermissionGate::new(
                policy.clone(),
                Arc::clone(&grants),
                PermissionSession::new(),
                "project".into(),
                Arc::clone(&dispatcher),
                Arc::clone(&allowed),
                Arc::clone(&prompts),
            );
            let store = PermissionGrantStore::open(&directory).expect("grant store should open");
            let (bridge, requests) = production_tui_permission_bridge();
            let reply_bridge = bridge.clone();
            let reply = std::thread::spawn(move || {
                let request = requests
                    .recv()
                    .expect("permission request should reach the TUI");
                let reply = match answer {
                    PermissionPromptAnswer::AllowOnce => TuiPermissionReply::AllowOnce,
                    PermissionPromptAnswer::AllowAlways => TuiPermissionReply::AllowAlways,
                    PermissionPromptAnswer::DenyOnce => TuiPermissionReply::DenyOnce,
                    PermissionPromptAnswer::DenyAlways => TuiPermissionReply::DenyAlways,
                    PermissionPromptAnswer::Cancel => TuiPermissionReply::Cancelled,
                };
                let replied = reply_bridge.reply(request.id(), reply);
                (request, replied)
            });
            let mut resolver = ProductionPermissionResolver::new(
                TuiPermissionPrompter(bridge, None),
                store,
                Arc::clone(&grants),
                Arc::clone(&prompts),
                ProductionPromptAuthorization {
                    policy,
                    session: PermissionSession::new(),
                    project: "project".into(),
                    dispatcher: Arc::clone(&dispatcher),
                    allowed: Arc::clone(&allowed),
                },
            );
            let mut production_dispatcher = ProductionToolDispatcher::new(dispatcher, allowed);

            assert_eq!(
                run_ready(gate.evaluate(&call, &cancellation)),
                Ok(PermissionDecision::Ask)
            );
            let decision = run_ready(resolver.resolve(&call, &cancellation));
            let (request, replied) = reply.join().expect("TUI permission reply should finish");
            assert_eq!(request.details().0, "read");
            assert!(request.details().1.contains("notes.md"));
            assert!(replied);

            match answer {
                PermissionPromptAnswer::AllowOnce | PermissionPromptAnswer::AllowAlways => {
                    assert_eq!(decision, Ok(PermissionDecision::Allow));
                    let changed_call = HeadlessToolCall {
                        input: r#"{"path":"changed.md"}"#.into(),
                        ..call.clone()
                    };
                    assert_eq!(
                        run_ready(production_dispatcher.dispatch(changed_call, &cancellation)),
                        Err(HeadlessTurnPortError::Tool)
                    );
                    assert_eq!(
                        run_ready(production_dispatcher.dispatch(call.clone(), &cancellation)),
                        Ok(HeadlessToolOutput::success("executed"))
                    );
                    assert_eq!(
                        run_ready(production_dispatcher.dispatch(call.clone(), &cancellation)),
                        Err(HeadlessTurnPortError::Tool)
                    );
                    if answer == PermissionPromptAnswer::AllowAlways {
                        let later_call = HeadlessToolCall {
                            id: "later".into(),
                            ..call.clone()
                        };
                        assert_eq!(
                            run_ready(gate.evaluate(&later_call, &cancellation)),
                            Ok(PermissionDecision::Allow)
                        );
                        assert_eq!(
                            run_ready(production_dispatcher.dispatch(later_call, &cancellation)),
                            Ok(HeadlessToolOutput::success("executed"))
                        );
                    }
                }
                PermissionPromptAnswer::DenyOnce | PermissionPromptAnswer::DenyAlways => {
                    assert_eq!(decision, Ok(PermissionDecision::Deny));
                }
                PermissionPromptAnswer::Cancel => {
                    assert_eq!(decision, Err(HeadlessTurnPortError::Cancelled));
                }
            }

            assert_eq!(
                executions.load(std::sync::atomic::Ordering::SeqCst),
                expected_executions
            );
            assert_eq!(
                PermissionGrantStore::open(&directory)
                    .expect("grant store should reopen")
                    .grants_for_project("project")
                    .expect("project grants should load")
                    .len(),
                expected_grants
            );
            std::fs::remove_dir_all(&directory)
                .expect("temporary grant directory should be removed");
        }
    }
    #[test]
    fn permission_prompt_answers_preserve_choices_and_redact_sensitive_targets() {
        for (input, expected) in [
            ("a", PermissionPromptAnswer::AllowOnce),
            ("always", PermissionPromptAnswer::AllowAlways),
            ("d", PermissionPromptAnswer::DenyOnce),
            ("deny-always", PermissionPromptAnswer::DenyAlways),
            ("cancel", PermissionPromptAnswer::Cancel),
        ] {
            assert_eq!(parse_permission_prompt_answer(input), Some(expected));
        }
        assert_eq!(parse_permission_prompt_answer("unknown"), None);

        let prompt = render_permission_prompt(&agens_tools::PermissionPromptContext {
            project_id: "project".into(),
            tool_identity: "native::webfetch".into(),
            target_identifier:
                "https://user:SENTINEL_URL_SECRET@example.test/path?token=SENTINEL_TOKEN".into(),
            access: agens_core::ToolAccess::ReadOnly,
            reason: "permission policy requires confirmation".into(),
        });

        assert!(prompt.contains("Permission required for webfetch"));
        assert!(prompt.contains("https://example.test/path"));
        assert!(!prompt.contains("SENTINEL_URL_SECRET"));
        assert!(!prompt.contains("SENTINEL_TOKEN"));

        let prompt = render_permission_prompt(&agens_tools::PermissionPromptContext {
            project_id: "project".into(),
            tool_identity: "native::webfetch".into(),
            target_identifier:
                "https://user:SENTINEL_URL_SECRET@example.test?token=SENTINEL_TOKEN#fragment".into(),
            access: agens_core::ToolAccess::ReadOnly,
            reason: "permission policy requires confirmation".into(),
        });

        assert!(prompt.contains("https://example.test/"));
        assert!(!prompt.contains("SENTINEL_URL_SECRET"));
        assert!(!prompt.contains("SENTINEL_TOKEN"));
        assert!(!prompt.contains("fragment"));

        let prompt = render_permission_prompt(&agens_tools::PermissionPromptContext {
            project_id: "project".into(),
            tool_identity: "native::webfetch".into(),
            target_identifier: r#"{"url":"https://example.test","token":"SENTINEL_JSON"}"#.into(),
            access: agens_core::ToolAccess::ReadOnly,
            reason: "permission policy requires confirmation".into(),
        });

        assert!(prompt.contains("Target: [redacted]"));
        assert!(!prompt.contains("SENTINEL_JSON"));
    }

    /// A `bash` prompt shows the command with shaped secrets redacted, and the
    /// context it is asked about comes from the dispatcher rather than from a
    /// literal written here.
    ///
    /// A prompt context carries the dispatcher's own identity for the tool
    /// (`native:4:bash`), not the qualified name a rule is written in. A guard
    /// comparing it against `"native::bash"` reads as redacting and redacts
    /// nothing, and a test constructing the context by hand cannot see that —
    /// it writes down whichever spelling makes the guard fire. So the context
    /// here is built the way production builds it: register the tool, evaluate
    /// a call against a policy that asks, and prompt with what comes back.
    #[test]
    fn a_prompt_for_a_bash_call_shows_the_command_with_secrets_redacted() {
        struct BashLikeTool;

        impl DispatchTool for BashLikeTool {
            fn permission_target(
                &self,
                arguments: &serde_json::Value,
            ) -> Result<String, agens_core::Error> {
                Ok(arguments["command"].as_str().unwrap_or_default().to_owned())
            }

            fn execute(
                &mut self,
                _: &ToolExecutionContext,
                _: serde_json::Value,
            ) -> Result<ToolOutput, agens_core::Error> {
                Ok(ToolOutput::success("unused"))
            }
        }

        let mut dispatcher = ToolDispatcher::new();
        dispatcher
            .register_native("native::bash", agens_core::ToolAccess::Write, BashLikeTool)
            .expect("the probe dispatcher must accept the tool");

        let policy = PermissionPolicy::new(
            PermissionMode::Edit,
            vec![PermissionRule::global(
                PermissionDecision::Ask,
                PermissionPattern::Exact("native::bash".into()),
                PermissionPattern::Any,
            )],
        );
        let outcome = dispatcher
            .evaluate(
                &policy,
                &[],
                &PermissionSession::new(),
                agens_tools::ToolDispatchRequest::new(
                    "project",
                    "bash",
                    serde_json::json!({"command": "curl -H 'Bearer SENTINEL_COMMAND' https://example.test"}),
                ),
            )
            .expect("the call must be decidable");
        let agens_tools::ToolEvaluationOutcome::PromptRequired(context) = outcome else {
            panic!("a rule that asks must produce a prompt: {outcome:?}");
        };

        let prompt = render_permission_prompt(&context);

        assert!(
            prompt.contains("curl") && prompt.contains("https://example.test"),
            "a bash prompt must show the command so the person can decide: {prompt}"
        );
        assert!(
            !prompt.contains("SENTINEL_COMMAND"),
            "shaped secrets in the command must be redacted: {prompt}"
        );
        assert!(
            !prompt.contains("[command redacted]"),
            "the command must not be wiped wholesale: {prompt}"
        );
        assert!(
            prompt.contains("bash") && !prompt.contains("native:4:"),
            "the prompt has to name the tool the way a rule names it, not by the dispatcher's \
             own encoding: {prompt}"
        );
    }

    /// A remote tool is named in the prompt the way a rule names it too.
    ///
    /// The dispatcher's identity for a remote tool is length-headed
    /// (`mcp:5:probe:14:read_text_file`) and carries no spelling anyone writes
    /// down. The person answering has to recognize the tool and may want to
    /// write a rule about it afterwards, so what the prompt shows is
    /// `<server>::<tool>` — the one of the two names a remote tool answers to
    /// that says on its own that it is remote.
    #[test]
    fn a_prompt_for_a_remote_call_names_the_tool_the_way_a_rule_names_it() {
        struct RemoteLikeTool;

        impl DispatchTool for RemoteLikeTool {
            fn permission_target(
                &self,
                _: &serde_json::Value,
            ) -> Result<String, agens_core::Error> {
                Ok("probe::read_text_file".to_owned())
            }

            fn execute(
                &mut self,
                _: &ToolExecutionContext,
                _: serde_json::Value,
            ) -> Result<ToolOutput, agens_core::Error> {
                Ok(ToolOutput::success("unused"))
            }
        }

        let mut dispatcher = ToolDispatcher::new();
        dispatcher
            .register_mcp(
                &agens_tools::RemoteToolMetadata {
                    qualified_name: "probe::read_text_file".into(),
                    server_name: "probe".into(),
                    tool_name: "read_text_file".into(),
                    description: None,
                    input_schema: serde_json::json!({"type": "object"}),
                    access: agens_tools::RemoteToolAccess::ReadOnly,
                },
                RemoteLikeTool,
            )
            .expect("the probe dispatcher must accept the remote tool");

        let policy = PermissionPolicy::new(
            PermissionMode::Edit,
            vec![PermissionRule::global(
                PermissionDecision::Ask,
                PermissionPattern::Exact("probe::read_text_file".into()),
                PermissionPattern::Any,
            )],
        );
        let outcome = dispatcher
            .evaluate(
                &policy,
                &[],
                &PermissionSession::new(),
                agens_tools::ToolDispatchRequest::new(
                    "project",
                    "probe_read_text_file",
                    serde_json::json!({"path": "notes.md"}),
                ),
            )
            .expect("the call must be decidable");
        let agens_tools::ToolEvaluationOutcome::PromptRequired(context) = outcome else {
            panic!("a rule that asks must produce a prompt: {outcome:?}");
        };

        let prompt = render_permission_prompt(&context);

        assert!(
            prompt.contains("Permission required for probe::read_text_file"),
            "the prompt has to name the remote tool the way a rule names it: {prompt}"
        );
        assert!(
            !prompt.contains("mcp:5:"),
            "the dispatcher's own encoding must not reach the person answering: {prompt}"
        );
    }
}
