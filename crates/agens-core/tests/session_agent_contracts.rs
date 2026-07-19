use agens_core::{
    AgentDefinition, AgentDefinitionError, AgentMode, CompletedSessionTurn,
    CompletedSessionTurnError, MAX_AGENT_DESCRIPTION_CHARS, MAX_AGENT_NAME_CHARS, MAX_AGENT_SKILLS,
    MAX_PERMISSION_GLOB_PATTERN_BYTES, MAX_PERMISSION_TARGET_BYTES, Message, MessagePart,
    PermissionDecision, PermissionPattern, PermissionRule, Role, SessionMessage,
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

#[test]
fn session_messages_reject_empty_and_role_incompatible_parts() {
    for (role, part) in [
        (Role::System, MessagePart::Reasoning("reasoning".into())),
        (Role::System, tool_call("id", "read", "{}")),
        (Role::System, tool_result("id", "result", false)),
        (Role::User, MessagePart::Reasoning("reasoning".into())),
        (Role::User, tool_call("id", "read", "{}")),
        (Role::User, tool_result("id", "result", false)),
        (Role::Assistant, tool_result("id", "result", false)),
        (Role::Tool, MessagePart::Text("text".into())),
        (Role::Tool, MessagePart::Reasoning("reasoning".into())),
        (Role::Tool, tool_call("id", "read", "{}")),
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
        vec![
            session_message(Role::User, MessagePart::Text("one".into())),
            session_message(Role::User, MessagePart::Text("two".into())),
        ],
    ] {
        assert_eq!(
            CompletedSessionTurn::new(messages),
            Err(CompletedSessionTurnError::InvalidMessageOrder)
        );
    }
}

#[test]
fn session_metadata_enforces_identity_and_completion_boundaries() {
    let resumable = SessionMetadata {
        id: 7,
        project: "agens".into(),
        title: "Core contracts".into(),
        active_agent: "review-agent".into(),
        created_at: 10,
        updated_at: 11,
        completed_turn_count: 1,
        resumable: true,
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
            completed_turn_count: 0,
            resumable: false,
            ..resumable
        }
        .validate(),
        Ok(())
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
