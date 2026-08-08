use agens_core::{AttemptKey, SessionAttemptStatus, SessionMetadata};
use agens_store::StoredSession;
use agens_tui::{DialogEntry, DialogView};

#[cfg(test)]
use agens_session::context::ActiveAgentRuntime;

pub fn parse_recovery_action(action_id: &str) -> Option<AttemptKey> {
    let mut parts = action_id.split(':');
    let (Some("session"), Some("recover"), Some(session_id), Some(attempt_id), None) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return None;
    };

    AttemptKey::new(session_id.parse().ok()?, attempt_id.parse().ok()?).ok()
}

pub fn session_dialog_entry(
    session: &StoredSession,
    current_session: Option<i64>,
    all_projects: bool,
    now: i64,
) -> DialogEntry {
    let metadata = &session.metadata;
    let age = session_relative_age(metadata.updated_at, now);
    let turns = if metadata.completed_turn_count == 1 {
        "1 turn".to_owned()
    } else {
        format!("{} turns", metadata.completed_turn_count)
    };
    let current = (current_session == Some(metadata.id)).then_some(" · current");
    let root = all_projects.then(|| format!(" · root={}", compact_session_root(&metadata.project)));
    let attempt_status = session
        .latest_attempt
        .as_ref()
        .map(|attempt| {
            format!(
                " · Attempt: {}",
                session_attempt_status_label(attempt.status())
            )
        })
        .unwrap_or_default();
    let row_detail = format!("{turns} · {age}");
    let selected_detail = format!(
        "Turns: {} · Agent: {}{}\nProvider: {} · Model: {}\nEffort: {} · Updated: {} ({}){} · ID: {} · {}{}",
        metadata.completed_turn_count,
        metadata.active_agent,
        current.unwrap_or_default(),
        metadata.provider_id.as_deref().unwrap_or("current runtime"),
        metadata.model_id.as_deref().unwrap_or("current runtime"),
        metadata
            .reasoning_effort
            .map(agens_core::ReasoningEffort::as_str)
            .unwrap_or_else(|| {
                if metadata.provider_id.is_some() || metadata.model_id.is_some() {
                    "Default"
                } else {
                    "current runtime"
                }
            }),
        metadata.updated_at,
        age,
        root.as_deref().unwrap_or_default(),
        metadata.id,
        metadata.title,
        attempt_status,
    );

    DialogEntry::action_with_metadata(
        format!("#{} {}", metadata.id, metadata.title),
        row_detail,
        format!(
            "{} {} {} {}",
            metadata.id, metadata.title, metadata.project, metadata.active_agent
        ),
        selected_detail,
        format!("session:{}", metadata.id),
    )
}

fn session_attempt_status_label(status: agens_core::SessionAttemptStatus) -> &'static str {
    match status {
        agens_core::SessionAttemptStatus::Running => "running",
        agens_core::SessionAttemptStatus::Completed => "completed",
        agens_core::SessionAttemptStatus::Cancelled => "cancelled",
        agens_core::SessionAttemptStatus::Failed => "failed",
        agens_core::SessionAttemptStatus::ProviderError => "provider error",
        agens_core::SessionAttemptStatus::Interrupted => "interrupted",
    }
}

pub fn resume_retry_notice(status: SessionAttemptStatus) -> Option<&'static str> {
    match status {
        SessionAttemptStatus::Cancelled
        | SessionAttemptStatus::Interrupted
        | SessionAttemptStatus::Failed
        | SessionAttemptStatus::ProviderError => {
            Some("Recovered failed prompt · Enter retry · Esc discard")
        }
        SessionAttemptStatus::Running | SessionAttemptStatus::Completed => None,
    }
}

pub fn recovery_confirmation_dialog(
    metadata: &SessionMetadata,
    attempt: &agens_core::SessionAttemptSummary,
    refusal: Option<&str>,
) -> DialogView {
    let mut help = format!(
        "Session: {} · ID: {}\nStatus: running\nStarted: {}\nThis may invalidate an attempt still running in another process.",
        metadata.title,
        metadata.id,
        attempt.started_at(),
    );
    if let Some(refusal) = refusal {
        help.push('\n');
        help.push_str(refusal);
    }

    DialogView::selection(
        "Recover interrupted attempt",
        Some(help),
        vec![
            DialogEntry::action(
                "Recover interrupted attempt",
                format!(
                    "session:recover:{}:{}",
                    attempt.key().session_id(),
                    attempt.key().attempt_id()
                ),
            ),
            DialogEntry::cancel("Cancel"),
        ],
    )
}

fn compact_session_root(root: &str) -> String {
    const MAX_CHARS: usize = 30;
    let character_count = root.chars().count();
    if character_count <= MAX_CHARS {
        return root.into();
    }

    format!(
        "...{}",
        root.chars()
            .skip(character_count - MAX_CHARS)
            .collect::<String>()
    )
}

pub fn session_relative_age(updated_at: i64, now: i64) -> String {
    let age = now.saturating_sub(updated_at);
    match age {
        ..=0 => "now".into(),
        1..=59 => format!("{age}s ago"),
        60..=3_599 => format!("{}m ago", age / 60),
        3_600..=86_399 => format!("{}h ago", age / 3_600),
        _ => format!("{}d ago", age / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use agens_core::Message;
    use agens_session::context::SessionContext;

    use agens_headless::HeadlessChatRequest;
    use agens_session::context::{
        AgentRotationError, SessionMutationError, reset_session, rotate_active_agent,
    };
    use agens_store::SessionStore;
    use std::sync::{Arc, Mutex};

    use agens_core::{
        CompletedSessionTurn, CompletedTurnSnapshot, MessagePart, PermissionMode,
        PermissionSession, Role, SessionMessage, TurnEvent, TurnState,
    };
    use agens_tools::{
        TaskExecutionRegistry, TaskLaunchMode, TaskMessageSource, TaskMessageTarget,
        ToolDispatchRequest, ToolEvaluationOutcome,
    };

    use super::*;
    use crate::test_support::{rotation_agent, rotation_dispatcher};
    use agens_fixtures::BundledModelValidator;
    use agens_headless::provider_messages;
    use agens_permissions::permission_policy;
    use agens_session::turns::completed_session_turn;

    #[test]
    fn subagent_message_and_cancellation_leave_the_primary_agent_unchanged() {
        let registry = TaskExecutionRegistry::new();
        let id = registry.admit(TaskLaunchMode::Background).unwrap();
        let dispatcher = rotation_dispatcher();
        let primary = rotation_agent("primary", None, false);
        let active = ActiveAgentRuntime::build(
            &primary,
            Some("gpt-5.5"),
            "project",
            &dispatcher,
            &BundledModelValidator,
        )
        .unwrap();
        let session = SessionContext {
            active_agent: Some(active),
            ..SessionContext::fresh()
        };

        registry
            .send_message(
                TaskMessageSource::User,
                TaskMessageTarget::Execution(id),
                "continue".into(),
            )
            .unwrap();
        assert!(registry.cancel(id));

        assert_eq!(
            session
                .active_agent
                .as_ref()
                .map(|agent| agent.name.as_str()),
            Some("primary")
        );
        assert_eq!(
            session
                .active_agent
                .as_ref()
                .and_then(|agent| agent.model.as_deref()),
            Some("gpt-5.5")
        );
    }

    #[test]
    fn idle_agent_rotation_restores_runtime_and_queues_expansion_reminders_atomically() {
        let temporary = std::env::temp_dir().join(format!(
            "agens-agent-rotation-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let dispatcher = rotation_dispatcher();
        let primary = rotation_agent("primary", Some("gpt-4.1"), false);
        let reviewer = rotation_agent("reviewer", Some("gpt-4o"), true);
        let mut store = SessionStore::open(&temporary).unwrap();
        let metadata = SessionMetadata {
            id: 0,
            project: "project".into(),
            title: "title".into(),
            active_agent: "primary".into(),
            provider_id: None,
            model_id: None,
            reasoning_effort: None,
            created_at: 1,
            updated_at: 1,
            completed_turn_count: 0,
            resumable: false,
            parent_session_id: None,
            fork_message_count: None,
        };
        let turn = CompletedSessionTurn::new(vec![
            SessionMessage::try_from(Message {
                role: Role::User,
                parts: vec![MessagePart::Text("first".into())],
            })
            .unwrap(),
        ])
        .unwrap();
        let metadata = store
            .persist_completed_session_turn(&metadata, &turn)
            .unwrap();
        let primary_runtime = ActiveAgentRuntime::build(
            &primary,
            None,
            "project",
            &dispatcher,
            &BundledModelValidator,
        )
        .unwrap();
        let mut context = SessionContext::resumed(
            1,
            metadata.clone(),
            Vec::new(),
            primary_runtime,
            std::path::PathBuf::from("project"),
        );
        let original = context.clone();
        context.running = true;
        let busy_original = context.clone();

        let busy = rotate_active_agent(
            &mut context,
            &reviewer,
            Some("gpt-4.1"),
            "project",
            &dispatcher,
            &BundledModelValidator,
            Some(&mut store),
        );
        assert_eq!(busy, Err(AgentRotationError::Busy));
        assert_eq!(context, busy_original);
        context.running = false;
        assert_eq!(
            SessionStore::open(&temporary)
                .unwrap()
                .load_session_for_resume(1)
                .unwrap()
                .metadata
                .active_agent,
            "primary"
        );

        let mut conflicting = metadata.clone();
        conflicting.title = "changed elsewhere".into();
        conflicting.updated_at = 2;
        let conflicting = store
            .persist_completed_session_turn(&conflicting, &turn)
            .unwrap();
        let rollback = rotate_active_agent(
            &mut context,
            &reviewer,
            Some("gpt-4.1"),
            "project",
            &dispatcher,
            &BundledModelValidator,
            Some(&mut store),
        );
        assert_eq!(rollback, Err(AgentRotationError::Persistence));
        assert_eq!(context, original);

        context.metadata = Some(conflicting);
        rotate_active_agent(
            &mut context,
            &reviewer,
            Some("gpt-4.1"),
            "project",
            &dispatcher,
            &BundledModelValidator,
            Some(&mut store),
        )
        .unwrap();
        assert_eq!(
            context.pending_system_reminder.as_deref(),
            Some("Agent capabilities expanded: primary -> reviewer.")
        );

        let request = agens_headless::apply_session_to_request(
            &context,
            HeadlessChatRequest {
                prompt: "next".into(),
                history: Vec::new(),
                model: None,
                system_prompt: None,
                max_iterations: None,
                mode: PermissionMode::Edit,
                dangerously_allow_all: false,
                dangerous_mode: false,
                request_config: agens_core::RequestConfig::default(),
                session_reasoning_effort: None,
                session: None,
                active_agent: None,
                effective_capabilities: None,
                pending_system_reminder: None,
                skills: None,
                media_ids: Vec::new(),
                media_mimes: Vec::new(),
            },
        );
        assert_eq!(request.active_agent.as_deref(), Some("reviewer"));
        assert_eq!(request.model.as_deref(), Some("gpt-4o"));
        assert_eq!(request.system_prompt.as_deref(), Some("You are reviewer."));
        assert_eq!(
            request.effective_capabilities,
            context
                .active_agent
                .as_ref()
                .map(|agent| agent.capabilities.clone())
        );
        assert_eq!(
            provider_messages(&request, false),
            vec![
                Message {
                    role: Role::System,
                    parts: vec![MessagePart::Text(
                        "Agent capabilities expanded: primary -> reviewer.".into(),
                    )],
                },
                Message {
                    role: Role::User,
                    parts: vec![MessagePart::Text("next".into())],
                },
            ]
        );

        rotate_active_agent(
            &mut context,
            &reviewer,
            Some("gpt-4.1"),
            "project",
            &dispatcher,
            &BundledModelValidator,
            Some(&mut store),
        )
        .unwrap();
        assert_eq!(
            context.pending_system_reminder.as_deref(),
            Some("Agent capabilities expanded: primary -> reviewer.")
        );

        let policy = permission_policy(
            &[],
            "project",
            PermissionMode::Edit,
            &Arc::new(Mutex::new(rotation_dispatcher())),
            request.effective_capabilities.as_ref(),
        )
        .unwrap();
        assert!(matches!(
            rotation_dispatcher()
                .evaluate(
                    &policy,
                    &[],
                    &PermissionSession::new(),
                    ToolDispatchRequest::new(
                        "project",
                        "native::read",
                        serde_json::json!({"target":"file"})
                    ),
                )
                .unwrap(),
            ToolEvaluationOutcome::Authorized(_)
        ));

        let snapshot = CompletedTurnSnapshot::from_persisted_events(vec![
            TurnEvent::StateChanged(TurnState::Requesting),
            TurnEvent::StateChanged(TurnState::Streaming),
            TurnEvent::ProviderPart(MessagePart::Text("answer".into())),
            TurnEvent::StateChanged(TurnState::Completed),
        ])
        .unwrap();
        let turn = completed_session_turn(
            "next",
            &snapshot,
            request.pending_system_reminder.as_deref(),
        )
        .unwrap();
        let persisted = store
            .persist_completed_session_turn(context.metadata.as_ref().unwrap(), &turn)
            .unwrap();
        context.metadata = Some(persisted);
        context.pending_system_reminder = None;
        let reopened = SessionStore::open(&temporary)
            .unwrap()
            .load_session_for_resume(1)
            .unwrap();
        assert_eq!(reopened.metadata.active_agent, "reviewer");
        let reminder = reopened
            .messages
            .iter()
            .find(|message| message.role == Role::System)
            .unwrap();
        assert_eq!(
            reminder.parts,
            vec![MessagePart::Text(
                "Agent capabilities expanded: primary -> reviewer.".into()
            )]
        );
        assert!(context.pending_system_reminder.is_none());

        let mut no_expansion = SessionContext::resumed(
            1,
            reopened.metadata,
            reopened.messages,
            context.active_agent.clone().unwrap(),
            std::path::PathBuf::from("project"),
        );
        assert!(!no_expansion.bypass_permissions);
        no_expansion.metadata = None;
        rotate_active_agent(
            &mut no_expansion,
            &reviewer,
            Some("gpt-4.1"),
            "project",
            &dispatcher,
            &BundledModelValidator,
            None,
        )
        .unwrap();
        assert!(no_expansion.pending_system_reminder.is_none());

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_session_reset_refuses_running_mutation_without_state_change() {
        let mut context = SessionContext::fresh();
        context.identifier = Some(7);
        context.running = true;
        let original = context.clone();

        assert_eq!(reset_session(&mut context), Err(SessionMutationError::Busy));
        assert_eq!(context, original);
    }

    #[test]
    fn tui_session_reset_clears_resumed_state_when_idle() {
        let mut context = SessionContext::fresh();
        context.identifier = Some(7);
        context.metadata = Some(SessionMetadata {
            id: 7,
            project: "project".into(),
            title: "conversation".into(),
            active_agent: "primary".into(),
            provider_id: None,
            model_id: None,
            reasoning_effort: None,
            created_at: 1,
            updated_at: 1,
            completed_turn_count: 1,
            resumable: true,
            parent_session_id: None,
            fork_message_count: None,
        });
        context.messages = vec![Message {
            role: Role::User,
            parts: vec![MessagePart::Text("previous request".into())],
        }];
        context.selected_subagent = Some("reviewer".into());

        reset_session(&mut context).expect("idle reset should synchronize the backend state");

        assert_eq!(context, SessionContext::fresh());
    }

    #[test]
    fn session_relative_age_uses_stable_boundaries() {
        for (updated_at, expected) in [
            (100_000, "now"),
            (99_941, "59s ago"),
            (99_940, "1m ago"),
            (96_401, "59m ago"),
            (96_400, "1h ago"),
            (13_601, "23h ago"),
            (13_600, "1d ago"),
        ] {
            assert_eq!(session_relative_age(updated_at, 100_000), expected);
        }
    }

    #[test]
    fn resumed_tui_session_preserves_typed_history_for_the_next_prompt() {
        let metadata = SessionMetadata {
            id: 7,
            project: "project".into(),
            title: "conversation".into(),
            active_agent: "primary".into(),
            provider_id: None,
            model_id: None,
            reasoning_effort: None,
            created_at: 10,
            updated_at: 20,
            completed_turn_count: 1,
            resumable: true,
            parent_session_id: None,
            fork_message_count: None,
        };
        let messages = vec![
            Message {
                role: Role::Assistant,
                parts: vec![
                    MessagePart::Reasoning("previous reasoning\nprevious reasoning body".into()),
                    MessagePart::ToolCall {
                        id: "call-1".into(),
                        name: "native::read".into(),
                        input: r#"{"path":"notes.md"}"#.into(),
                    },
                ],
            },
            Message {
                role: Role::Tool,
                parts: vec![MessagePart::ToolResult {
                    tool_call_id: "call-1".into(),
                    content: "previous result".into(),
                    is_error: false,
                }],
            },
        ];

        let dispatcher = rotation_dispatcher();
        let active_agent = ActiveAgentRuntime::build(
            &rotation_agent("primary", None, false),
            None,
            "project",
            &dispatcher,
            &BundledModelValidator,
        )
        .unwrap();
        let context = SessionContext::resumed(
            7,
            metadata,
            messages.clone(),
            active_agent,
            std::path::PathBuf::from("project"),
        );
        let request = agens_headless::apply_session_to_request(
            &context,
            HeadlessChatRequest {
                prompt: "next question".into(),
                history: Vec::new(),
                model: None,
                system_prompt: None,
                max_iterations: None,
                mode: PermissionMode::Edit,
                dangerously_allow_all: false,
                dangerous_mode: false,
                request_config: agens_core::RequestConfig::default(),
                session_reasoning_effort: None,
                session: None,
                active_agent: None,
                effective_capabilities: None,
                pending_system_reminder: None,
                skills: None,
                media_ids: Vec::new(),
                media_mimes: Vec::new(),
            },
        );

        assert_eq!(request.prompt, "next question");
        assert_eq!(request.history, messages);
        assert_eq!(request.system_prompt.as_deref(), Some("You are primary."));
        assert_eq!(request.session.as_ref().map(|session| session.id), Some(7));
    }

    #[test]
    fn fresh_tui_session_does_not_reuse_prior_context() {
        let request = agens_headless::apply_session_to_request(
            &SessionContext::fresh(),
            HeadlessChatRequest {
                prompt: "new question".into(),
                history: Vec::new(),
                model: None,
                system_prompt: None,
                max_iterations: None,
                mode: PermissionMode::Edit,
                dangerously_allow_all: false,
                dangerous_mode: false,
                request_config: agens_core::RequestConfig::default(),
                session_reasoning_effort: None,
                session: None,
                active_agent: None,
                effective_capabilities: None,
                pending_system_reminder: None,
                skills: None,
                media_ids: Vec::new(),
                media_mimes: Vec::new(),
            },
        );

        assert_eq!(request.system_prompt, None);
    }
}
