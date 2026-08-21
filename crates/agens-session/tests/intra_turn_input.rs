use agens_core::{IntraTurnInputSource, MessagePart, Role, TurnEvent, TurnState};
use agens_session::turns::{completed_session_turn_from_events, drain_turn_directive_messages};
use agens_store::{DirectiveGrain, DirectiveStore};

fn tool_call(id: &str) -> TurnEvent {
    TurnEvent::ProviderPart(MessagePart::ToolCall {
        id: id.into(),
        name: "read".into(),
        input: "{\"path\":\"Cargo.toml\"}".into(),
    })
}

fn tool_result(id: &str) -> TurnEvent {
    TurnEvent::ToolResult(MessagePart::ToolResult {
        tool_call_id: id.into(),
        content: "contents".into(),
        is_error: false,
    })
}

fn roles(events: &[TurnEvent]) -> Vec<Role> {
    completed_session_turn_from_events("do the thing", events, None)
        .expect("the turn encodes")
        .messages()
        .iter()
        .map(|message| message.role)
        .collect()
}

struct Temporary {
    path: std::path::PathBuf,
}

impl Temporary {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "agens-session-turn-directives-{label}-{}",
            std::process::id()
        ));
        std::fs::remove_dir_all(&path).ok();
        std::fs::create_dir_all(&path).expect("test data directory");
        Self { path }
    }
}

impl Drop for Temporary {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.path).ok();
    }
}

#[test]
fn next_turn_drains_turn_directives_before_new_input_with_their_recorded_roles() {
    let temporary = Temporary::new("drain");
    let mut store = DirectiveStore::open(&temporary.path).expect("the queue opens");
    store
        .enqueue(
            42,
            IntraTurnInputSource::Supervisor,
            DirectiveGrain::Turn,
            "replan first",
        )
        .unwrap();
    store
        .enqueue(
            42,
            IntraTurnInputSource::Human,
            DirectiveGrain::Turn,
            "then continue",
        )
        .unwrap();
    store
        .enqueue(
            42,
            IntraTurnInputSource::Human,
            DirectiveGrain::ToolCall,
            "not at turn start",
        )
        .unwrap();

    let messages = drain_turn_directive_messages(&temporary.path, 42).expect("the queue drains");

    assert_eq!(
        messages
            .iter()
            .map(|message| (message.role, message.parts.clone()))
            .collect::<Vec<_>>(),
        vec![
            (
                Role::Supervisor,
                vec![MessagePart::Text("replan first".into())]
            ),
            (Role::User, vec![MessagePart::Text("then continue".into())]),
        ],
        "turn directives precede the new prompt in durable queue order"
    );

    let mut reopened = DirectiveStore::open(&temporary.path).unwrap();
    assert!(
        reopened.drain(42, DirectiveGrain::Turn).unwrap().is_empty(),
        "the next turn marks every injected directive delivered"
    );
    assert_eq!(
        reopened.drain(42, DirectiveGrain::ToolCall).unwrap().len(),
        1,
        "the turn boundary leaves tool-call directives for their own safe point"
    );
}

/// The turn's own tool results must stay adjacent to the assistant message that
/// called them. A speaker landing between them is what the provider history
/// validator refuses, so the encoded roles are the proof that the safe point is
/// placed where it belongs.
#[test]
fn input_at_a_batch_boundary_never_separates_tool_results_from_their_call() {
    let encoded = roles(&[
        TurnEvent::StateChanged(TurnState::Requesting),
        TurnEvent::StateChanged(TurnState::Streaming),
        tool_call("call-1"),
        tool_call("call-2"),
        TurnEvent::StateChanged(TurnState::Dispatching),
        tool_result("call-1"),
        tool_result("call-2"),
        TurnEvent::StateChanged(TurnState::Requesting),
        TurnEvent::IntraTurnInput {
            source: IntraTurnInputSource::Human,
            text: "use the other file".into(),
        },
        TurnEvent::StateChanged(TurnState::Streaming),
        TurnEvent::ProviderPart(MessagePart::Text("done".into())),
        TurnEvent::StateChanged(TurnState::Completed),
    ]);

    // Prompt, the assistant turn that called the tools, both results coalesced
    // into one tool message, the mid-turn input, then the assistant's reply.
    // The input lands after the batch, never inside it.
    assert_eq!(
        encoded,
        vec![
            Role::User,
            Role::Assistant,
            Role::Tool,
            Role::User,
            Role::Assistant
        ]
    );
}

/// A supervisor speaks in its own message. Merged into the user's, its narrower
/// authority would be indistinguishable from the user's own.
#[test]
fn a_supervisor_message_is_never_merged_into_the_users() {
    let encoded = roles(&[
        TurnEvent::StateChanged(TurnState::Requesting),
        TurnEvent::IntraTurnInput {
            source: IntraTurnInputSource::Supervisor,
            text: "prefer the manifest".into(),
        },
        TurnEvent::StateChanged(TurnState::Streaming),
        TurnEvent::ProviderPart(MessagePart::Text("done".into())),
        TurnEvent::StateChanged(TurnState::Completed),
    ]);

    assert_eq!(
        encoded,
        vec![Role::User, Role::Supervisor, Role::Assistant],
        "the supervisor speaks in its own role, not as the person"
    );
}
