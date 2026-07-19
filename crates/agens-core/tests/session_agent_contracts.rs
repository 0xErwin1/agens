use agens_core::{
    AgentDefinition, AgentDefinitionError, AgentMode, CompletedSessionTurn, Message, MessagePart,
    Role, SessionMessage, SessionMetadata, SessionMetadataError,
};
fn text(role: Role, value: &str) -> Message {
    Message {
        role,
        parts: vec![MessagePart::Text(value.into())],
    }
}
#[test]
fn session_messages_accept_only_parts_allowed_for_their_ordered_roles() {
    let system = SessionMessage::try_from(text(Role::System, "policy")).unwrap();
    let user = SessionMessage::try_from(text(Role::User, "question")).unwrap();
    let assistant = SessionMessage::try_from(Message {
        role: Role::Assistant,
        parts: vec![
            MessagePart::Reasoning("inspect context".into()),
            MessagePart::ToolCall {
                id: "call-1".into(),
                name: "read".into(),
                input: "{\"path\":\"Cargo.toml\"}".into(),
            },
        ],
    })
    .unwrap();
    let tool = SessionMessage::try_from(Message {
        role: Role::Tool,
        parts: vec![MessagePart::ToolResult {
            tool_call_id: "call-1".into(),
            content: "manifest".into(),
            is_error: false,
        }],
    })
    .unwrap();
    let completed =
        CompletedSessionTurn::new(user, Some(system), vec![assistant], vec![tool]).unwrap();
    assert_eq!(
        completed
            .messages()
            .iter()
            .map(|message| message.role)
            .collect::<Vec<_>>(),
        vec![Role::System, Role::User, Role::Assistant, Role::Tool]
    );
    assert!(
        SessionMessage::try_from(Message {
            role: Role::User,
            parts: vec![MessagePart::Reasoning("not user content".into())],
        })
        .is_err()
    );
}

#[test]
fn session_metadata_requires_a_resumable_completed_session_and_canonical_active_agent() {
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
    let incomplete = SessionMetadata {
        completed_turn_count: 0,
        resumable: false,
        ..resumable.clone()
    };
    let invalid = SessionMetadata {
        resumable: false,
        ..resumable.clone()
    };
    let invalid_agent = SessionMetadata {
        active_agent: "Review Agent".into(),
        ..resumable
    };
    assert_eq!(incomplete.validate(), Ok(()));
    assert_eq!(
        invalid.validate(),
        Err(SessionMetadataError::InvalidResumability)
    );
    assert_eq!(
        invalid_agent.validate(),
        Err(SessionMetadataError::InvalidActiveAgent)
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
fn agent_definition_rejects_invalid_names_and_bounded_duplicate_skills() {
    let invalid_name = AgentDefinition {
        name: "Review Agent".into(),
        description: "Valid description.".into(),
        mode: AgentMode::Primary,
        model: None,
        system_prompt: "Valid prompt.".into(),
        permission_rules: Vec::new(),
        skills: Vec::new(),
    };
    let duplicate_skills = AgentDefinition {
        name: "valid-agent".into(),
        description: "Valid description.".into(),
        mode: AgentMode::Primary,
        model: None,
        system_prompt: "Valid prompt.".into(),
        permission_rules: Vec::new(),
        skills: vec!["code-review".into(), "code-review".into()],
    };
    assert_eq!(
        invalid_name.validate(),
        Err(AgentDefinitionError::InvalidName)
    );
    assert_eq!(
        duplicate_skills.validate(),
        Err(AgentDefinitionError::DuplicateSkill)
    );
}
