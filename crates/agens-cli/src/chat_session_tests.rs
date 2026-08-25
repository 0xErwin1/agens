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
fn script() -> Script {
    Script::new([
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
    let daemon = DaemonFixture::start(script(), daemon_settings());
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
                    checkout,
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

            let second = ask(
                &mut chat,
                opened.session_id,
                "which crate is the daemon",
                &mut events,
            )
            .await;

            chat.close(proto::ChatRef {
                session_id: opened.session_id,
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

    let requests = daemon.provider.wait_for_requests(2);
    let second_request = requests[1].body();

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
