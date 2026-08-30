//! An ordinary chat, run by the daemon, through the composed facade.
//!
//! What this proves is the composition rather than any one piece: `agens serve`
//! as the operator starts it, the chat factory the CLI actually installs, a real
//! session row, and the turn machinery a run's worker drives — reached over the
//! socket by a client that never runs a turn itself.
//!
//! The second prompt is the point. A hosted chat that answered each prompt from
//! an empty history would be a sequence of strangers, not a conversation, and
//! the only place that shows is in what the model is sent the second time.

use std::path::PathBuf;

use agens_fixtures::{Script, ScriptedTurn};
use agens_server::grpc::proto::{self, chat_client::ChatClient};
use tonic::transport::Channel;

use crate::daemon_fixture::{DaemonFixture, PATIENCE, connect, daemon_settings};

/// The model's side of the conversation: two turns, one per prompt.
fn script(marker: &std::path::Path) -> Script {
    Script::new([
        ScriptedTurn::tool_call(
            "agent-denied",
            "bash",
            serde_json::json!({"command": format!("touch {}", marker.display())}).to_string(),
        ),
        ScriptedTurn::text("it is a Rust workspace"),
        ScriptedTurn::text("agens-server holds the daemon"),
    ])
}

/// Reads the stream until the turn ends, collecting the text the model streamed
/// on the way.
async fn turn_on(
    events: &mut tonic::Streaming<proto::SessionEvent>,
) -> (Vec<String>, Option<String>) {
    let mut streamed = Vec::new();

    loop {
        let event = tokio::time::timeout(PATIENCE, events.message())
            .await
            .expect("the chat published within the wait")
            .expect("the stream is healthy")
            .and_then(|event| event.event);

        match event {
            Some(proto::session_event::Event::Progress(progress)) => {
                if let Some(proto::turn_progress::Event::ProviderPart(part)) = progress.event
                    && let Some(proto::message_part::Part::Text(text)) = part.part
                {
                    streamed.push(text);
                }
            }
            Some(proto::session_event::Event::TurnCompleted(completed)) => {
                return (streamed, Some(completed.text));
            }
            Some(proto::session_event::Event::TurnFailed(failed)) => {
                panic!("the turn failed: {}", failed.detail);
            }
            // No prompt reaches this journey: the scripted model calls no
            // tool, so nothing asks for a decision.
            Some(proto::session_event::Event::PermissionAsked(asked)) => {
                panic!(
                    "the turn asked for a decision nobody scripted: {}",
                    asked.tool
                );
            }
            Some(proto::session_event::Event::AskUserAsked(_)) => {
                panic!("the turn asked the user something nobody scripted");
            }
            Some(proto::session_event::Event::Closed(_)) | None => return (streamed, None),
        }
    }
}

async fn ask(
    client: &mut ChatClient<Channel>,
    session_id: i64,
    prompt: &str,
    events: &mut tonic::Streaming<proto::SessionEvent>,
) -> (Vec<String>, Option<String>) {
    client
        .prompt(proto::PromptRequest {
            session_id,
            prompt: prompt.to_owned(),
            parts: Vec::new(),
        })
        .await
        .expect("the prompt is accepted");

    turn_on(events).await
}

#[test]
fn a_prompt_a_client_sent_is_answered_by_a_turn_the_daemon_ran() {
    let marker = std::env::temp_dir().join("agens-hosted-agent-command-ran");
    let daemon = DaemonFixture::start_with_model(script(&marker), daemon_settings(), "gpt-5.5");
    let agent_root = daemon.checkout.join(".agens/agents");
    std::fs::create_dir_all(&agent_root).expect("create hosted agent directory");
    std::fs::write(
        agent_root.join("all.md"),
        "---\nname: all\ndescription: hosted all\nmode: all\npermissions: [\"deny bash\"]\n---\nHosted all agent prompt.\n",
    )
    .expect("write hosted agent");
    let project_instructions = "Hosted project instructions marker.";
    std::fs::write(daemon.checkout.join("AGENTS.md"), project_instructions)
        .expect("write hosted project instructions");
    let first_media =
        agens_store::ingest_media_bytes(&daemon.data_directory, b"first-image", "image/png")
            .expect("first hosted media is stored");
    let second_media =
        agens_store::ingest_media_bytes(&daemon.data_directory, b"second-image", "image/jpeg")
            .expect("second hosted media is stored");

    let socket = daemon.socket.clone();
    let stopper = daemon.stopper();
    let checkout = daemon.checkout.display().to_string();

    let client = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        let stopper = stopper;

        runtime.block_on(async move {
            let mut chat = ChatClient::new(connect(socket).await);

            let opened = chat
                .open(proto::OpenChatRequest {
                    checkout: checkout.clone(),
                    resume: None,
                })
                .await
                .expect("the chat opens")
                .into_inner();

            let mut events = chat
                .subscribe(proto::ChatRef {
                    session_id: opened.session_id,
                })
                .await
                .expect("the chat is open")
                .into_inner();

            let agents = chat
                .command(proto::ChatCommandRequest {
                    session_id: opened.session_id,
                    command: "/agents".to_owned(),
                })
                .await
                .expect("the hosted agent catalog crosses the wire")
                .into_inner();
            assert_eq!(
                agents.message,
                "Eligible primary agents:\nprimary (current)\nall"
            );

            let agent = chat
                .command(proto::ChatCommandRequest {
                    session_id: opened.session_id,
                    command: "/agent all".to_owned(),
                })
                .await
                .expect("the hosted agent selection crosses the wire")
                .into_inner();
            assert_eq!(agent.message, "Active agent: all.");

            let command = chat
                .command(proto::ChatCommandRequest {
                    session_id: opened.session_id,
                    command: "/effort high".to_owned(),
                })
                .await
                .expect("the hosted command executes")
                .into_inner();
            assert_eq!(command.message, "Reasoning effort: high.");

            let rejected = chat
                .command(proto::ChatCommandRequest {
                    session_id: opened.session_id,
                    command: "/model moonshotai/kimi-k3".to_owned(),
                })
                .await
                .expect_err("an unauthenticated provider is refused");
            assert!(
                rejected.message().contains("unavailable"),
                "{}",
                rejected.message()
            );

            let model = chat
                .command(proto::ChatCommandRequest {
                    session_id: opened.session_id,
                    command: "/model openai-api/gpt-4.1".to_owned(),
                })
                .await
                .expect("the hosted model command executes")
                .into_inner();
            assert_eq!(
                model.message,
                "Model: gpt-4.1. Reasoning effort reset to Default because high is unsupported."
            );

            chat.prompt(proto::PromptRequest {
                session_id: opened.session_id,
                prompt: "compatibility projection".into(),
                parts: vec![
                    proto::MessagePart {
                        part: Some(proto::message_part::Part::Media(proto::Media {
                            media_id: first_media.id,
                            mime: first_media.mime.clone(),
                        })),
                    },
                    proto::MessagePart {
                        part: Some(proto::message_part::Part::Text(
                            "what is this repository".into(),
                        )),
                    },
                    proto::MessagePart {
                        part: Some(proto::message_part::Part::Media(proto::Media {
                            media_id: second_media.id,
                            mime: second_media.mime.clone(),
                        })),
                    },
                ],
            })
            .await
            .expect("the ordered media prompt is accepted");
            let first = turn_on(&mut events).await;

            let completed_history = chat
                .history(proto::ChatRef {
                    session_id: opened.session_id,
                })
                .await
                .expect("the completed hosted turn is persisted")
                .into_inner();
            assert_eq!(completed_history.messages[0].role, "user");
            assert!(matches!(
                completed_history.messages[0].parts.as_slice(),
                [
                    proto::MessagePart {
                        part: Some(proto::message_part::Part::Media(first)),
                    },
                    proto::MessagePart {
                        part: Some(proto::message_part::Part::Text(text)),
                    },
                    proto::MessagePart {
                        part: Some(proto::message_part::Part::Media(second)),
                    },
                ] if first.media_id == first_media.id
                    && text == "what is this repository"
                    && second.media_id == second_media.id
            ));

            chat.close(proto::ChatRef {
                session_id: opened.session_id,
            })
            .await
            .expect("the chat closes before resume");

            let reopened = chat
                .open(proto::OpenChatRequest {
                    checkout,
                    resume: Some(opened.session_id),
                })
                .await
                .expect("the hosted chat resumes")
                .into_inner();
            assert_eq!(reopened.session_id, opened.session_id);

            let mut events = chat
                .subscribe(proto::ChatRef {
                    session_id: reopened.session_id,
                })
                .await
                .expect("the resumed chat is open")
                .into_inner();
            let second = ask(
                &mut chat,
                reopened.session_id,
                "which crate is the daemon",
                &mut events,
            )
            .await;

            chat.close(proto::ChatRef {
                session_id: reopened.session_id,
            })
            .await
            .expect("the chat is open");

            drop(stopper);

            (opened.session_id, first, second)
        })
    });

    daemon.serve();

    let (session_id, first, second) = client.join().expect("the client thread finished");

    assert!(session_id > 0, "the chat persists against a real session");
    assert_eq!(
        first.1.as_deref(),
        Some("it is a Rust workspace"),
        "the first turn's answer reaches the client"
    );
    assert_eq!(
        second.1.as_deref(),
        Some("agens-server holds the daemon"),
        "the second turn's answer reaches the client"
    );
    assert!(
        first.0.iter().any(|text| text.contains("Rust workspace")),
        "the answer is streamed as it is produced, not only at the end: {:?}",
        first.0
    );

    let requests = daemon.provider.wait_for_requests(3);
    let second_request = requests[2].body();

    assert!(
        [&requests[0], &requests[2]].into_iter().all(|request| {
            request.body().matches("Hosted all agent prompt.").count() == 1
                && request.body().matches(project_instructions).count() == 1
        }),
        "the selected agent prompt and project instructions reach each turn exactly once: {:?}",
        requests
            .iter()
            .map(|request| request.body())
            .collect::<Vec<_>>()
    );
    assert!(
        !marker.exists(),
        "the selected agent's denied tool capability governs the hosted runtime"
    );
    assert!(
        requests
            .iter()
            .all(|request| request.body().contains(r#""model":"gpt-4.1""#)),
        "the persisted hosted model survives resume: {:?}",
        requests
            .iter()
            .map(|request| request.body())
            .collect::<Vec<_>>()
    );
    assert!(
        !requests[0]
            .body()
            .contains(r#""reasoning":{"effort":"high"}"#),
        "changing to an incompatible model resets daemon-owned effort: {}",
        requests[0].body()
    );

    let stored = agens_store::SessionStore::open(&daemon.data_directory)
        .and_then(|store| store.load_session_for_resume(session_id))
        .expect("the hosted model selection is persisted");
    assert_eq!(stored.metadata.active_agent, "all");
    assert_eq!(stored.metadata.provider_id.as_deref(), Some("openai-api"));
    assert_eq!(stored.metadata.model_id.as_deref(), Some("gpt-4.1"));
    assert_eq!(stored.metadata.reasoning_effort, None);
    assert!(
        second_request.contains("what is this repository"),
        "the second turn carries the first one's history, so the chat is one \
         conversation rather than two strangers"
    );
    assert!(
        second_request.contains("Zmlyc3QtaW1hZ2U=") && second_request.contains("c2Vjb25kLWltYWdl"),
        "completed Media→Text→Media history is replayed with both blobs: {second_request}"
    );

    daemon.provider.assert_script_consumed();
    let _ = std::fs::remove_dir_all(PathBuf::from(&daemon.root));
}

/// A session row opened by a chat that exited before completing a turn is
/// `resumable = 0` with nothing said. The daemon that finds one — after a
/// restart, from a `--resume`, or from a stale preference — is being handed a
/// conversation that never started, and what the person is owed is that empty
/// conversation, not an error that says the session does not exist.
#[test]
fn a_chat_that_never_completed_a_turn_reopens_as_a_fresh_conversation() {
    let daemon = DaemonFixture::start(
        Script::new([ScriptedTurn::text("it is a Rust workspace")]),
        daemon_settings(),
    );

    // The orphan row, exactly as `open_session` leaves it for a chat that is
    // opened and abandoned: no completed turns, not resumable, no messages.
    // Written before the daemon serves, which is the restart shape — the
    // daemon holds no live record of this session.
    let orphan = agens_store::SessionStore::open(&daemon.data_directory)
        .and_then(|mut store| {
            store.open_session(&agens_core::SessionMetadata {
                id: 0,
                project: daemon.checkout.display().to_string(),
                title: String::new(),
                active_agent: "primary".to_owned(),
                provider_id: None,
                model_id: None,
                reasoning_effort: None,
                created_at: 1,
                updated_at: 1,
                completed_turn_count: 0,
                resumable: false,
                parent_session_id: None,
                fork_message_count: None,
            })
        })
        .expect("the orphan session row is stored");

    let socket = daemon.socket.clone();
    let stopper = daemon.stopper();
    let checkout = daemon.checkout.display().to_string();

    let client = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        let stopper = stopper;

        runtime.block_on(async move {
            let mut chat = ChatClient::new(connect(socket).await);

            let opened = chat
                .open(proto::OpenChatRequest {
                    checkout,
                    resume: Some(orphan),
                })
                .await
                .expect("the orphan session reopens")
                .into_inner();
            assert_eq!(opened.session_id, orphan);

            let history = chat
                .history(proto::ChatRef { session_id: orphan })
                .await
                .expect("an orphan session's history is an empty thread, not an error")
                .into_inner();
            assert_eq!(history.messages, Vec::new());

            let mut events = chat
                .subscribe(proto::ChatRef { session_id: orphan })
                .await
                .expect("the reopened chat is open")
                .into_inner();
            let answer = ask(&mut chat, orphan, "what is this repository", &mut events).await;

            chat.close(proto::ChatRef { session_id: orphan })
                .await
                .expect("the chat closes");

            drop(stopper);

            answer
        })
    });

    daemon.serve();

    let answer = client.join().expect("the client thread finished");
    assert_eq!(answer.1.as_deref(), Some("it is a Rust workspace"));

    let stored = agens_store::SessionStore::open(&daemon.data_directory)
        .and_then(|store| store.load_session_for_resume(orphan))
        .expect("the first completed turn makes the session resumable");
    assert!(stored.metadata.resumable);
    assert_eq!(stored.metadata.completed_turn_count, 1);

    daemon.provider.assert_script_consumed();
    let _ = std::fs::remove_dir_all(PathBuf::from(&daemon.root));
}

/// The open answer describes the configuration the composed factory actually
/// gives a session: the daemon's configured model for a fresh chat, and the
/// persisted selection for a resumed one. This is what lets an attaching
/// terminal open on the daemon's configuration instead of placeholders.
#[test]
fn opening_a_chat_describes_the_sessions_own_configuration() {
    let daemon = DaemonFixture::start(Script::new([]), daemon_settings());

    let selected = agens_store::SessionStore::open(&daemon.data_directory)
        .and_then(|mut store| {
            store.open_session(&agens_core::SessionMetadata {
                id: 0,
                project: daemon.checkout.display().to_string(),
                title: String::new(),
                active_agent: "primary".to_owned(),
                provider_id: Some("openai-api".to_owned()),
                model_id: Some("gpt-5.5".to_owned()),
                reasoning_effort: Some(agens_core::ReasoningEffort::High),
                created_at: 1,
                updated_at: 1,
                completed_turn_count: 0,
                resumable: false,
                parent_session_id: None,
                fork_message_count: None,
            })
        })
        .expect("the selected session row is stored");

    let socket = daemon.socket.clone();
    let stopper = daemon.stopper();
    let checkout = daemon.checkout.display().to_string();

    let client = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        let stopper = stopper;

        runtime.block_on(async move {
            let mut chat = ChatClient::new(connect(socket).await);

            let fresh = chat
                .open(proto::OpenChatRequest {
                    checkout: checkout.clone(),
                    resume: None,
                })
                .await
                .expect("a fresh chat opens")
                .into_inner();
            assert_eq!(fresh.provider.as_deref(), Some("openai-api"));
            assert_eq!(fresh.model.as_deref(), Some("gpt-4.1"));
            assert_eq!(fresh.reasoning_effort, None);
            assert_eq!(
                fresh.context_window,
                agens_models::context_window_for("gpt-4.1"),
            );

            let resumed = chat
                .open(proto::OpenChatRequest {
                    checkout,
                    resume: Some(selected),
                })
                .await
                .expect("the selected session reopens")
                .into_inner();
            assert_eq!(resumed.session_id, selected);
            assert_eq!(resumed.provider.as_deref(), Some("openai-api"));
            assert_eq!(resumed.model.as_deref(), Some("gpt-5.5"));
            assert_eq!(resumed.reasoning_effort.as_deref(), Some("high"));
            assert_eq!(
                resumed.context_window,
                agens_models::context_window_for("gpt-5.5"),
            );

            drop(stopper);
        })
    });

    daemon.serve();

    client.join().expect("the client thread finished");
    let _ = std::fs::remove_dir_all(PathBuf::from(&daemon.root));
}
