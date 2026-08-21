use agens_core::{
    AgentDefinition, AgentDefinitionError, AgentMode, CompletedSessionTurn,
    CompletedSessionTurnError, MAX_AGENT_DESCRIPTION_CHARS, MAX_AGENT_NAME_CHARS, MAX_AGENT_SKILLS,
    MAX_PERMISSION_GLOB_PATTERN_BYTES, MAX_PERMISSION_TARGET_BYTES, Message, MessagePart,
    PermissionDecision, PermissionPattern, PermissionRule, ReasoningEffort, Role, SessionMessage,
    SessionMessageError, SessionMetadata, SessionMetadataError,
};

fn session_message(role: Role, part: MessagePart) -> SessionMessage {
    SessionMessage::try_from(Message {
        role,
        parts: vec![part],
    })
    .unwrap()
}

fn tool_call(id: &str, name: &str, input: &str) -> MessagePart {
    MessagePart::ToolCall {
        id: id.into(),
        name: name.into(),
        input: input.into(),
    }
}

fn tool_result(tool_call_id: &str, content: &str, is_error: bool) -> MessagePart {
    MessagePart::ToolResult {
        tool_call_id: tool_call_id.into(),
        content: content.into(),
        is_error,
    }
}

fn agent_definition() -> AgentDefinition {
    AgentDefinition {
        name: "review-agent".into(),
        description: "Reviews a bounded request.".into(),
        mode: AgentMode::Primary,
        model: Some("provider/model".into()),
        model_source: None,
        reasoning_effort: Some(ReasoningEffort::Low),
        system_prompt: "Review the supplied request.".into(),
        permission_rules: Vec::new(),
        skills: Vec::new(),
    }
}

macro_rules! assert_invalid_agent {
    ($error:expr, $($field:ident: $value:expr),+) => {{
        let mut definition = agent_definition();
        $(definition.$field = $value;)+
        assert_eq!(definition.validate(), Err($error));
    }};
}

#[test]
fn session_messages_preserve_all_typed_part_payloads() {
    let messages = [
        (Role::System, MessagePart::Text("system text".into())),
        (Role::User, MessagePart::Text("user text".into())),
        (Role::Assistant, MessagePart::Text("assistant text".into())),
        (
            Role::Assistant,
            MessagePart::Reasoning("assistant reasoning".into()),
        ),
        (
            Role::Assistant,
            tool_call("call-1", "read", "{\"a\":1,\"nested\":{\"b\":2}}"),
        ),
        (Role::Tool, tool_result("call-1", "tool output", true)),
    ];

    for (role, part) in messages {
        let message = Message {
            role,
            parts: vec![part],
        };
        let session_message = SessionMessage::try_from(message.clone()).unwrap();

        assert_eq!(session_message.as_message(), &message);
        assert_eq!(session_message.into_message(), message);
    }
}

fn media_part(media_id: i64, mime: &str) -> MessagePart {
    MessagePart::Media {
        media_id,
        mime: mime.into(),
    }
}

#[test]
fn session_messages_reject_empty_and_role_incompatible_parts() {
    for (role, part) in [
        (Role::System, MessagePart::Reasoning("reasoning".into())),
        (Role::System, tool_call("id", "read", "{}")),
        (Role::System, tool_result("id", "result", false)),
        (Role::System, media_part(1, "image/png")),
        (Role::User, MessagePart::Reasoning("reasoning".into())),
        (Role::User, tool_call("id", "read", "{}")),
        (Role::User, tool_result("id", "result", false)),
        (Role::Assistant, tool_result("id", "result", false)),
        (Role::Assistant, media_part(1, "image/png")),
        (Role::Tool, MessagePart::Text("text".into())),
        (Role::Tool, MessagePart::Reasoning("reasoning".into())),
        (Role::Tool, tool_call("id", "read", "{}")),
        (Role::Tool, media_part(1, "image/png")),
    ] {
        assert_eq!(
            SessionMessage::try_from(Message {
                role,
                parts: vec![part]
            }),
            Err(SessionMessageError::PartNotAllowed { role })
        );
    }

    for (role, part) in [
        (Role::System, MessagePart::Text(String::new())),
        (Role::User, MessagePart::Text(String::new())),
        (Role::Assistant, MessagePart::Text(String::new())),
        (Role::Assistant, MessagePart::Reasoning(String::new())),
        (Role::Assistant, tool_call("", "read", "{}")),
        (Role::Assistant, tool_call("id", "", "{}")),
        (Role::Assistant, tool_call("id", "read", "")),
        (Role::Tool, tool_result("", "result", false)),
        (Role::Tool, tool_result("id", "", false)),
        (Role::User, media_part(0, "image/png")),
        (Role::User, media_part(1, "")),
    ] {
        assert_eq!(
            SessionMessage::try_from(Message {
                role,
                parts: vec![part]
            }),
            Err(SessionMessageError::EmptyPart)
        );
    }

    assert_eq!(
        SessionMessage::try_from(Message {
            role: Role::User,
            parts: Vec::new()
        }),
        Err(SessionMessageError::EmptyParts)
    );
}

#[test]
fn user_session_messages_accept_media_parts_with_text() {
    let message = Message {
        role: Role::User,
        parts: vec![
            MessagePart::Text("describe this".into()),
            media_part(42, "image/png"),
        ],
    };

    let session_message = SessionMessage::try_from(message.clone()).unwrap();

    assert_eq!(session_message.as_message(), &message);
    assert_eq!(session_message.into_message(), message);
}

#[test]
fn user_session_messages_accept_media_only_parts() {
    let message = Message {
        role: Role::User,
        parts: vec![media_part(7, "image/jpeg")],
    };

    let session_message = SessionMessage::try_from(message.clone()).unwrap();

    assert_eq!(session_message.as_message(), &message);
}

#[test]
fn completed_session_turn_preserves_chronological_tool_interleaving() {
    let messages = vec![
        session_message(Role::User, MessagePart::Text("question".into())),
        session_message(
            Role::Assistant,
            tool_call("call-1", "read", "{\"path\":\"Cargo.toml\"}"),
        ),
        session_message(Role::Tool, tool_result("call-1", "manifest", false)),
        session_message(Role::Assistant, MessagePart::Text("answer".into())),
    ];
    let completed = CompletedSessionTurn::new(messages).unwrap();

    assert_eq!(
        completed
            .messages()
            .iter()
            .map(|message| message.role)
            .collect::<Vec<_>>(),
        [Role::User, Role::Assistant, Role::Tool, Role::Assistant]
    );
}

#[test]
fn completed_session_turn_rejects_invalid_sequence_boundaries() {
    for messages in [
        vec![
            session_message(Role::Assistant, MessagePart::Text("before user".into())),
            session_message(Role::User, MessagePart::Text("question".into())),
        ],
        vec![
            session_message(Role::User, MessagePart::Text("question".into())),
            session_message(Role::System, MessagePart::Text("late reminder".into())),
        ],
        vec![
            session_message(Role::System, MessagePart::Text("one".into())),
            session_message(Role::System, MessagePart::Text("two".into())),
            session_message(Role::User, MessagePart::Text("question".into())),
        ],
    ] {
        assert_eq!(
            CompletedSessionTurn::new(messages),
            Err(CompletedSessionTurnError::InvalidMessageOrder)
        );
    }
}

/// A second user message used to be rejected here. It is legal now: input can
/// reach a running turn at a tool boundary, and it is carried as a further
/// `User` message because no provider role exists for a mid-turn speaker.
///
/// What the order still guarantees is unchanged — a turn begins with the user's
/// prompt, and nothing precedes it but a system reminder.
#[test]
fn completed_session_turn_accepts_input_that_arrived_mid_turn() {
    assert!(
        CompletedSessionTurn::new(vec![
            session_message(Role::User, MessagePart::Text("question".into())),
            session_message(Role::Assistant, MessagePart::Text("working".into())),
            session_message(Role::User, MessagePart::Text("use the other file".into())),
            session_message(Role::Assistant, MessagePart::Text("done".into())),
        ])
        .is_ok()
    );
}

/// Directives queued for the turn boundary are delivered before the prompt, so
/// the durable turn records them there. A supervisor is the only speaker that
/// can open a turn: a person's directive is a `User` message, and nothing
/// distinguishes it from the prompt it precedes.
#[test]
fn completed_session_turn_accepts_directives_delivered_before_the_prompt() {
    assert!(
        CompletedSessionTurn::new(vec![
            session_message(Role::System, MessagePart::Text("reminder".into())),
            session_message(Role::Supervisor, MessagePart::Text("replan first".into())),
            session_message(Role::User, MessagePart::Text("then continue".into())),
            session_message(Role::User, MessagePart::Text("question".into())),
            session_message(Role::Assistant, MessagePart::Text("done".into())),
        ])
        .is_ok()
    );
}

/// A turn whose only speaker is the supervisor still has no prompt, and the
/// order it claims to encode is the one it must refuse.
#[test]
fn completed_session_turn_rejects_a_turn_that_never_reaches_the_prompt() {
    assert_eq!(
        CompletedSessionTurn::new(vec![
            session_message(Role::Supervisor, MessagePart::Text("replan first".into())),
            session_message(Role::Assistant, MessagePart::Text("done".into())),
        ]),
        Err(CompletedSessionTurnError::InvalidMessageOrder)
    );
}

#[test]
fn session_metadata_enforces_identity_and_completion_boundaries() {
    let resumable = SessionMetadata {
        id: 7,
        project: "agens".into(),
        title: "Core contracts".into(),
        active_agent: "review-agent".into(),
        provider_id: Some("openai-api".into()),
        model_id: Some("gpt-5.5".into()),
        reasoning_effort: Some(ReasoningEffort::XHigh),
        created_at: 10,
        updated_at: 11,
        completed_turn_count: 1,
        resumable: true,
        parent_session_id: None,
        fork_message_count: None,
    };
    assert_eq!(resumable.validate(), Ok(()));
    assert_eq!(
        SessionMetadata {
            completed_turn_count: 0,
            resumable: true,
            ..resumable.clone()
        }
        .validate(),
        Err(SessionMetadataError::InvalidResumability)
    );
    assert_eq!(
        SessionMetadata {
            resumable: false,
            ..resumable.clone()
        }
        .validate(),
        Err(SessionMetadataError::InvalidResumability)
    );
    assert_eq!(
        SessionMetadata {
            id: 0,
            ..resumable.clone()
        }
        .validate(),
        Err(SessionMetadataError::InvalidId)
    );
    assert_eq!(
        SessionMetadata {
            project: String::new(),
            ..resumable.clone()
        }
        .validate(),
        Err(SessionMetadataError::EmptyProject)
    );
    assert_eq!(
        SessionMetadata {
            active_agent: "Review Agent".into(),
            ..resumable.clone()
        }
        .validate(),
        Err(SessionMetadataError::InvalidActiveAgent)
    );
    assert_eq!(
        SessionMetadata {
            provider_id: Some("OpenAI API".into()),
            ..resumable.clone()
        }
        .validate(),
        Err(SessionMetadataError::InvalidProviderId)
    );
    assert_eq!(
        SessionMetadata {
            model_id: Some("bad model".into()),
            ..resumable.clone()
        }
        .validate(),
        Err(SessionMetadataError::InvalidModelId)
    );
    assert_eq!(
        SessionMetadata {
            completed_turn_count: 0,
            resumable: false,
            ..resumable
        }
        .validate(),
        Ok(())
    );
}

/// Fork lineage is both-or-neither: a fork carries the session it came from and the cut point it
/// copied up to, and a session that was started rather than forked carries neither.
#[test]
fn session_metadata_enforces_fork_lineage_coherence() {
    let fork = SessionMetadata {
        id: 7,
        project: "agens".into(),
        title: "Core contracts".into(),
        active_agent: "review-agent".into(),
        provider_id: None,
        model_id: None,
        reasoning_effort: None,
        created_at: 10,
        updated_at: 11,
        completed_turn_count: 1,
        resumable: true,
        parent_session_id: Some(3),
        fork_message_count: Some(4),
    };
    assert_eq!(fork.validate(), Ok(()));

    for half_lineage in [
        SessionMetadata {
            fork_message_count: None,
            ..fork.clone()
        },
        SessionMetadata {
            parent_session_id: None,
            ..fork.clone()
        },
        SessionMetadata {
            parent_session_id: Some(0),
            ..fork.clone()
        },
        SessionMetadata {
            parent_session_id: Some(fork.id),
            ..fork.clone()
        },
    ] {
        assert_eq!(
            half_lineage.validate(),
            Err(SessionMetadataError::IncoherentFork)
        );
    }

    assert_eq!(
        SessionMetadata {
            fork_message_count: Some(0),
            ..fork
        }
        .validate(),
        Err(SessionMetadataError::InvalidForkMessageCount)
    );
}

#[test]
fn agent_definition_accepts_canonical_names_and_all_supported_modes() {
    for mode in [AgentMode::Primary, AgentMode::Subagent, AgentMode::All] {
        let definition = AgentDefinition {
            name: "review-agent-2".into(),
            description: "Reviews a bounded request.".into(),
            mode,
            model: Some("provider/model".into()),
            model_source: None,
            reasoning_effort: Some(ReasoningEffort::Low),
            system_prompt: "Review the supplied request.".into(),
            permission_rules: Vec::new(),
            skills: vec!["code-review".into()],
        };
        assert_eq!(definition.validate(), Ok(()));
    }
}

#[test]
fn agent_definition_accepts_exact_validation_limits() {
    let definition = AgentDefinition {
        name: "a".repeat(MAX_AGENT_NAME_CHARS),
        description: "a".repeat(MAX_AGENT_DESCRIPTION_CHARS),
        skills: (0..MAX_AGENT_SKILLS)
            .map(|index| format!("skill-{index}"))
            .collect(),
        permission_rules: vec![PermissionRule::global(
            PermissionDecision::Allow,
            PermissionPattern::Exact("t".repeat(MAX_PERMISSION_GLOB_PATTERN_BYTES)),
            PermissionPattern::Exact("p".repeat(MAX_PERMISSION_TARGET_BYTES)),
        )],
        ..agent_definition()
    };

    assert_eq!(definition.validate(), Ok(()));
}

#[test]
fn agent_definition_rejects_every_bounded_field_outside_its_limits() {
    assert_invalid_agent!(AgentDefinitionError::InvalidName, name: String::new());
    assert_invalid_agent!(AgentDefinitionError::InvalidName, name: "a".repeat(MAX_AGENT_NAME_CHARS + 1));
    assert_invalid_agent!(AgentDefinitionError::InvalidDescription, description: String::new());
    assert_invalid_agent!(AgentDefinitionError::InvalidDescription, description: "a".repeat(MAX_AGENT_DESCRIPTION_CHARS + 1));
    assert_invalid_agent!(AgentDefinitionError::InvalidDescription, description: "line\nbreak".into());
    assert_invalid_agent!(AgentDefinitionError::EmptySystemPrompt, system_prompt: String::new());
    assert_invalid_agent!(AgentDefinitionError::TooManySkills, skills: (0..=MAX_AGENT_SKILLS).map(|index| format!("skill-{index}")).collect());
    assert_invalid_agent!(AgentDefinitionError::DuplicateSkill, skills: vec!["invalid skill".into()]);
    assert_invalid_agent!(AgentDefinitionError::DuplicateSkill, skills: vec!["skill".into(), "skill".into()]);
    assert_invalid_agent!(AgentDefinitionError::InvalidPermissionRule, permission_rules: vec![PermissionRule::global(PermissionDecision::Allow, PermissionPattern::Exact("t".repeat(MAX_PERMISSION_GLOB_PATTERN_BYTES + 1)), PermissionPattern::Any)]);
    assert_invalid_agent!(AgentDefinitionError::InvalidPermissionRule, permission_rules: vec![PermissionRule::global(PermissionDecision::Allow, PermissionPattern::Any, PermissionPattern::Exact("p".repeat(MAX_PERMISSION_TARGET_BYTES + 1)))]);
}
