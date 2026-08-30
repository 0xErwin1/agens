//! The Chat plane as a client reaches it: a prompt over the wire, a turn in the
//! daemon, and its events coming back on a stream.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agens_core::{
    HeadlessTurnCancellation, MessagePart, ToolOutcome, ToolResultFacts, TurnCoordinator,
    TurnEvent, TurnState,
};
use agens_server::grpc::proto::{self, chat_client::ChatClient};
use agens_server::{
    BlockingBoundary, ChatFacade, ChatSession, ChatSessionRequest, ChatSessions, ChatTurnOutcome,
    ChatTurns, SessionAdmission, SessionBudget, SessionId, SessionProvider, SessionRuntime,
    SessionSupervisor,
};
use tonic::transport::{Endpoint, Server, Uri};
use tonic::{Code, Request};

/// How long a test waits for the daemon to do something on another thread.
const PATIENCE: Duration = Duration::from_secs(5);

/// How long a scripted turn waits to be released before it gives up.
///
/// Shorter than the assertion wait, and deliberately so: a test that never
/// releases its turn is holding a session the runtime has to join on the way
/// out, and the wait it holds it for is time every such test pays.
const RELEASE_PATIENCE: Duration = Duration::from_millis(500);

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

struct StubProvider;

impl SessionProvider for StubProvider {
    fn model(&self) -> &str {
        "stub/model"
    }
}

/// What one scripted turn reports before it ends.
struct Script {
    progress: Vec<TurnEvent>,
    outcome: ChatTurnOutcome,
}

/// A turn the test wrote: it announces the prompt it received, reports the
/// scripted progress, then waits to be released so a test can observe a chat
/// while a turn is still running.
struct ScriptedTurns {
    started: Sender<String>,
    structured: Sender<agens_core::SessionMessage>,
    release: Arc<Mutex<Receiver<Script>>>,
}

impl ChatTurns for ScriptedTurns {
    fn command(&mut self, command: &str) -> Result<String, agens_server::ChatError> {
        Ok(format!("executed:{command}"))
    }

    fn run(
        &mut self,
        message: &agens_core::SessionMessage,
        _runtime: &SessionRuntime,
        _cancellation: &HeadlessTurnCancellation,
        _asks: &Arc<dyn agens_server::ChatAsks>,
        progress: &agens_core::TurnProgressSink,
    ) -> ChatTurnOutcome {
        let _ = self.structured.send(message.clone());
        let prompt = message
            .as_message()
            .parts
            .iter()
            .filter_map(|part| match part {
                MessagePart::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        let _ = self.started.send(prompt);

        let Ok(script) = self
            .release
            .lock()
            .expect("the release channel is readable")
            .recv_timeout(RELEASE_PATIENCE)
        else {
            return ChatTurnOutcome::Failed("the turn was never released".to_owned());
        };

        for event in script.progress {
            progress(event);
        }

        script.outcome
    }
}

/// A served facade and the client that reaches it.
struct Wire {
    client: ChatClient<tonic::transport::Channel>,
    _chats: Arc<ChatSessions>,
    supervisor: SessionSupervisor,
    started: Receiver<String>,
    structured: Receiver<agens_core::SessionMessage>,
    release: Sender<Script>,
    media_directory: PathBuf,
    shutdown: HeadlessTurnCancellation,
}

impl Drop for Wire {
    /// The facade stops accepting, and every hosted session is asked to end.
    ///
    /// The sessions are cancelled explicitly rather than left to the chats
    /// being dropped: the served facade holds its own handle on them, so
    /// whether dropping this one is the last drop is a race the test would
    /// lose by hanging the runtime's shutdown on a session parked on an inbox.
    fn drop(&mut self) {
        self.shutdown.cancel();
        self.supervisor.registry().cancel_all();
    }
}

fn scratch_directory(name: &str) -> PathBuf {
    let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    let directory = std::env::temp_dir().join(format!(
        "agens-chat-wire-{}-{name}-{suffix}",
        std::process::id()
    ));

    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).expect("the scratch directory is writable");

    directory
}

async fn connect_unix(path: PathBuf) -> tonic::transport::Channel {
    // The authority is never used: the connector hands back a unix stream
    // whatever the URI says, and gRPC still wants a syntactically valid one.
    Endpoint::try_from("http://localhost")
        .unwrap()
        .connect_with_connector(tower::service_fn(move |_: Uri| {
            let path = path.clone();

            async move {
                let stream = tokio::net::UnixStream::connect(path).await?;

                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
            }
        }))
        .await
        .unwrap()
}

async fn wire(name: &str) -> Wire {
    let supervisor = SessionSupervisor::new(tokio::runtime::Handle::current());
    let directory = scratch_directory(name);

    let (started, started_rx) = channel();
    let (structured, structured_rx) = channel();
    let (release, release_rx) = channel();
    let release_rx = Arc::new(Mutex::new(release_rx));

    let chats = Arc::new(
        ChatSessions::new(
            supervisor.clone(),
            Arc::new(move |request: &ChatSessionRequest| {
                Ok(ChatSession {
                    admission: SessionAdmission::new(
                        SessionId::new(request.resume.unwrap_or(1)),
                        Box::new(StubProvider),
                        SessionBudget::unlimited(),
                    ),
                    turns: Box::new(ScriptedTurns {
                        started: started.clone(),
                        structured: structured.clone(),
                        release: Arc::clone(&release_rx),
                    }),
                })
            }),
            Arc::new(|_| {
                Ok(vec![agens_core::Message {
                    role: agens_core::Role::Assistant,
                    parts: vec![agens_core::MessagePart::Text("what we said".to_owned())],
                }])
            }),
        )
        .with_media_store(directory.clone()),
    );

    let socket = directory.join("facade.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();

    let shutdown = HeadlessTurnCancellation::new();
    let parked = shutdown.clone();
    let blocking = BlockingBoundary::new(tokio::runtime::Handle::current());
    let served = Arc::clone(&chats);

    tokio::spawn(async move {
        Server::builder()
            .add_service(proto::chat_server::ChatServer::new(ChatFacade::new(
                served, blocking,
            )))
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::UnixListenerStream::new(listener),
                async move {
                    while !parked.is_cancelled() {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                },
            )
            .await
    });

    let client = ChatClient::new(connect_unix(socket).await);

    Wire {
        client,
        _chats: chats,
        supervisor,
        started: started_rx,
        structured: structured_rx,
        release,
        media_directory: directory,
        shutdown,
    }
}

fn open_request(checkout: &str) -> proto::OpenChatRequest {
    proto::OpenChatRequest {
        checkout: checkout.to_owned(),
        resume: None,
    }
}

/// A `ToolResultFacts` event, built the only way one can be.
///
/// Its identity is `#[non_exhaustive]`, so no crate but `agens-core` can write
/// one down: the event is produced by handing a turn coordinator the facts a
/// tool reported, which is how one reaches a surface in production too.
fn tool_result_facts_event() -> TurnEvent {
    let mut turn = TurnCoordinator::new();
    turn.begin().expect("a fresh coordinator starts a turn");
    turn.accept_provider_part(MessagePart::ToolCall {
        id: "call-1".to_owned(),
        name: "bash".to_owned(),
        input: "{\"command\":\"true\"}".to_owned(),
    })
    .expect("the provider may call a tool");
    turn.finish_provider_iteration()
        .expect("the iteration ends on a tool call");
    turn.accept_tool_result(
        "call-1",
        String::new(),
        false,
        Some(ToolResultFacts::Bash {
            outcome: ToolOutcome::Succeeded,
            exit_code: Some(0),
        }),
    )
    .expect("the call is the one that was made");

    turn.events()
        .iter()
        .find(|event| matches!(event, TurnEvent::ToolResultFacts { .. }))
        .expect("the coordinator reported the facts it was given")
        .clone()
}

/// The next event the stream carries, or a panic naming what was waited for.
async fn next_event(
    stream: &mut tonic::Streaming<proto::SessionEvent>,
) -> Option<proto::session_event::Event> {
    let event = tokio::time::timeout(PATIENCE, stream.message())
        .await
        .expect("the chat published within the wait")
        .expect("the stream is healthy")?;

    event.event
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_over_the_wire_executes_on_the_hosted_chat() {
    let mut wire = wire("command").await;
    let handle = wire
        .client
        .open(Request::new(open_request("/projects/agens")))
        .await
        .expect("the chat opens")
        .into_inner();

    let result = wire
        .client
        .command(Request::new(proto::ChatCommandRequest {
            session_id: handle.session_id,
            command: "/effort high".to_owned(),
        }))
        .await
        .expect("the command executes")
        .into_inner();

    assert_eq!(result.message, "executed:/effort high");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_over_the_wire_runs_in_the_daemon_and_its_turn_comes_back_on_the_stream() {
    let mut wire = wire("prompt").await;

    let handle = wire
        .client
        .open(Request::new(open_request("/projects/agens")))
        .await
        .expect("the chat opens")
        .into_inner();

    let mut events = wire
        .client
        .subscribe(Request::new(proto::ChatRef {
            session_id: handle.session_id,
        }))
        .await
        .expect("the chat is open")
        .into_inner();

    wire.client
        .prompt(Request::new(proto::PromptRequest {
            session_id: handle.session_id,
            prompt: "what does this repository do".to_owned(),
            parts: Vec::new(),
        }))
        .await
        .expect("the prompt is accepted");

    assert_eq!(
        wire.started.recv_timeout(PATIENCE),
        Ok("what does this repository do".to_owned()),
    );

    wire.release
        .send(Script {
            progress: vec![
                TurnEvent::StateChanged(TurnState::Streaming),
                TurnEvent::ProviderPart(MessagePart::Text("it hosts chats".to_owned())),
            ],
            outcome: ChatTurnOutcome::Completed("it hosts chats".to_owned()),
        })
        .expect("the turn is waiting");

    assert_eq!(
        next_event(&mut events).await,
        Some(proto::session_event::Event::Progress(proto::TurnProgress {
            event: Some(proto::turn_progress::Event::State("streaming".to_owned())),
        })),
    );

    assert_eq!(
        next_event(&mut events).await,
        Some(proto::session_event::Event::Progress(proto::TurnProgress {
            event: Some(proto::turn_progress::Event::ProviderPart(
                proto::MessagePart {
                    part: Some(proto::message_part::Part::Text("it hosts chats".to_owned())),
                }
            )),
        })),
    );

    assert_eq!(
        next_event(&mut events).await,
        Some(proto::session_event::Event::TurnCompleted(
            proto::TurnCompleted {
                text: "it hosts chats".to_owned(),
            }
        )),
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn structured_prompt_parts_are_authoritative_and_reach_the_turn_in_order() {
    let mut wire = wire("structured-prompt").await;
    let first = agens_store::ingest_media_bytes(&wire.media_directory, b"first", "image/png")
        .expect("media is stored");
    let second =
        agens_store::ingest_media_bytes(&wire.media_directory, b"second", "application/pdf")
            .expect("media is stored");
    let handle = wire
        .client
        .open(Request::new(open_request("/projects/agens")))
        .await
        .expect("the chat opens")
        .into_inner();
    assert!(handle.supports_prompt_parts);

    wire.client
        .prompt(Request::new(proto::PromptRequest {
            session_id: handle.session_id,
            prompt: "compatibility text must not be duplicated".into(),
            parts: vec![
                proto::MessagePart {
                    part: Some(proto::message_part::Part::Media(proto::Media {
                        media_id: first.id,
                        mime: first.mime,
                    })),
                },
                proto::MessagePart {
                    part: Some(proto::message_part::Part::Text("between".into())),
                },
                proto::MessagePart {
                    part: Some(proto::message_part::Part::Media(proto::Media {
                        media_id: second.id,
                        mime: second.mime,
                    })),
                },
            ],
        }))
        .await
        .expect("the structured prompt is accepted");

    let received = wire
        .structured
        .recv_timeout(PATIENCE)
        .expect("the turn starts");
    assert_eq!(
        received.as_message().parts,
        vec![
            MessagePart::Media {
                media_id: first.id,
                mime: "image/png".into(),
            },
            MessagePart::Text("between".into()),
            MessagePart::Media {
                media_id: second.id,
                mime: "application/pdf".into(),
            },
        ]
    );
    wire.release
        .send(Script {
            progress: Vec::new(),
            outcome: ChatTurnOutcome::Completed("done".into()),
        })
        .expect("the turn is waiting");
}

#[tokio::test(flavor = "multi_thread")]
async fn unavailable_media_is_rejected_before_queue_admission_or_turn_start() {
    let mut wire = wire("missing-media").await;
    let handle = wire
        .client
        .open(Request::new(open_request("/projects/agens")))
        .await
        .expect("the chat opens")
        .into_inner();

    let refused = wire
        .client
        .prompt(Request::new(proto::PromptRequest {
            session_id: handle.session_id,
            prompt: String::new(),
            parts: vec![proto::MessagePart {
                part: Some(proto::message_part::Part::Media(proto::Media {
                    media_id: 999_999,
                    mime: "image/png".into(),
                })),
            }],
        }))
        .await
        .expect_err("missing media fails closed");
    assert_eq!(refused.code(), Code::InvalidArgument);
    assert!(wire.started.try_recv().is_err(), "no turn was started");

    let mismatched = agens_store::ingest_media_bytes(&wire.media_directory, b"image", "image/png")
        .expect("media is stored");
    let refused = wire
        .client
        .prompt(Request::new(proto::PromptRequest {
            session_id: handle.session_id,
            prompt: String::new(),
            parts: vec![proto::MessagePart {
                part: Some(proto::message_part::Part::Media(proto::Media {
                    media_id: mismatched.id,
                    mime: "application/pdf".into(),
                })),
            }],
        }))
        .await
        .expect_err("stored MIME mismatch fails closed");
    assert_eq!(refused.code(), Code::InvalidArgument);

    let missing_blob =
        agens_store::ingest_media_bytes(&wire.media_directory, b"gone", "image/jpeg")
            .expect("media is stored");
    let (_, blob) = agens_store::open_media(&wire.media_directory, missing_blob.id)
        .expect("the blob exists before deletion");
    fs::remove_file(blob).expect("the test removes only its scratch blob");
    let refused = wire
        .client
        .prompt(Request::new(proto::PromptRequest {
            session_id: handle.session_id,
            prompt: String::new(),
            parts: vec![proto::MessagePart {
                part: Some(proto::message_part::Part::Media(proto::Media {
                    media_id: missing_blob.id,
                    mime: missing_blob.mime,
                })),
            }],
        }))
        .await
        .expect_err("a media row whose blob is missing fails closed");
    assert_eq!(refused.code(), Code::InvalidArgument);
    assert!(
        wire.started.try_recv().is_err(),
        "invalid media never starts a turn"
    );

    wire.client
        .prompt(Request::new(proto::PromptRequest {
            session_id: handle.session_id,
            prompt: "legacy still works".into(),
            parts: Vec::new(),
        }))
        .await
        .expect("the rejected message did not consume the inbox");
    assert_eq!(
        wire.started.recv_timeout(PATIENCE),
        Ok("legacy still works".into())
    );
}

/// The turn's own facts about a tool result are what the daemon feeds its ingest
/// with; no surface renders them. They are skipped at the boundary rather than
/// ending the stream or arriving as a shape that does not mean them.
#[tokio::test(flavor = "multi_thread")]
async fn an_event_this_wire_does_not_carry_is_skipped_without_breaking_the_stream() {
    let mut wire = wire("skipped").await;

    let handle = wire
        .client
        .open(Request::new(open_request("/projects/agens")))
        .await
        .expect("the chat opens")
        .into_inner();

    let mut events = wire
        .client
        .subscribe(Request::new(proto::ChatRef {
            session_id: handle.session_id,
        }))
        .await
        .expect("the chat is open")
        .into_inner();

    wire.client
        .prompt(Request::new(proto::PromptRequest {
            session_id: handle.session_id,
            prompt: "run the tests".to_owned(),
            parts: Vec::new(),
        }))
        .await
        .expect("the prompt is accepted");

    assert_eq!(
        wire.started.recv_timeout(PATIENCE),
        Ok("run the tests".to_owned()),
    );

    wire.release
        .send(Script {
            progress: vec![
                tool_result_facts_event(),
                TurnEvent::StateChanged(TurnState::Completed),
            ],
            outcome: ChatTurnOutcome::Completed("green".to_owned()),
        })
        .expect("the turn is waiting");

    assert_eq!(
        next_event(&mut events).await,
        Some(proto::session_event::Event::Progress(proto::TurnProgress {
            event: Some(proto::turn_progress::Event::State("completed".to_owned())),
        })),
        "the event after the skipped one still arrives, in order"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn closing_a_chat_over_the_wire_ends_the_stream_following_it() {
    let mut wire = wire("closed").await;

    let handle = wire
        .client
        .open(Request::new(open_request("/projects/agens")))
        .await
        .expect("the chat opens")
        .into_inner();

    let mut events = wire
        .client
        .subscribe(Request::new(proto::ChatRef {
            session_id: handle.session_id,
        }))
        .await
        .expect("the chat is open")
        .into_inner();

    wire.client
        .close(Request::new(proto::ChatRef {
            session_id: handle.session_id,
        }))
        .await
        .expect("the chat is open");

    assert_eq!(
        next_event(&mut events).await,
        Some(proto::session_event::Event::Closed(proto::ChatClosed {})),
    );
    assert_eq!(next_event(&mut events).await, None, "the stream is over");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_for_a_chat_nobody_opened_is_not_found() {
    let mut wire = wire("unknown").await;

    let refused = wire
        .client
        .prompt(Request::new(proto::PromptRequest {
            session_id: 404,
            prompt: "hello".to_owned(),
            parts: Vec::new(),
        }))
        .await
        .expect_err("there is no such chat");

    assert_eq!(refused.code(), Code::NotFound);
}

/// Waiting changes nothing about a chat that is already running a turn with one
/// prompt behind it, so the refusal is a precondition rather than exhaustion.
#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_past_the_one_that_may_wait_is_a_failed_precondition() {
    let mut wire = wire("busy").await;

    let handle = wire
        .client
        .open(Request::new(open_request("/projects/agens")))
        .await
        .expect("the chat opens")
        .into_inner();

    for prompt in ["first", "second"] {
        wire.client
            .prompt(Request::new(proto::PromptRequest {
                session_id: handle.session_id,
                prompt: prompt.to_owned(),
                parts: Vec::new(),
            }))
            .await
            .expect("the running turn and the one waiting behind it are both accepted");
    }

    assert_eq!(wire.started.recv_timeout(PATIENCE), Ok("first".to_owned()));

    let refused = wire
        .client
        .prompt(Request::new(proto::PromptRequest {
            session_id: handle.session_id,
            prompt: "third".to_owned(),
            parts: Vec::new(),
        }))
        .await
        .expect_err("a third prompt has nowhere to wait");

    assert_eq!(refused.code(), Code::FailedPrecondition);
}

/// proto3 cannot tell an unset string from an empty one, so a client that
/// forgot the field is refused rather than given a chat rooted at the daemon's
/// own working directory.
#[tokio::test(flavor = "multi_thread")]
async fn a_chat_that_names_no_checkout_is_refused() {
    let mut wire = wire("unscoped").await;

    let refused = wire
        .client
        .open(Request::new(open_request("")))
        .await
        .expect_err("a chat names its checkout");

    assert_eq!(refused.code(), Code::InvalidArgument);
}

/// A terminal that detached comes back to the conversation it left. Opening a
/// second one beside it would leave the first answering into a stream nobody
/// reads.
#[tokio::test(flavor = "multi_thread")]
async fn a_client_that_comes_back_is_told_which_chat_is_already_open_here() {
    let mut wire = wire("listed").await;

    let handle = wire
        .client
        .open(Request::new(open_request("/projects/agens")))
        .await
        .expect("the chat opens")
        .into_inner();

    let open = wire
        .client
        .list(Request::new(proto::ListChatsRequest {
            checkout: "/projects/agens".to_owned(),
        }))
        .await
        .expect("the listing is served")
        .into_inner();

    assert_eq!(open.chats.len(), 1);
    assert_eq!(open.chats[0].session_id, handle.session_id);
    assert_eq!(open.chats[0].checkout, "/projects/agens");
    assert!(!open.chats[0].answering, "no turn is running yet");
}

/// A bare relaunch settles the listing by opening the id it was offered. The
/// daemon's chats outlive the client, so that id is usually still live — and
/// opening it again from the same checkout is coming back, not a conflict.
#[tokio::test(flavor = "multi_thread")]
async fn a_relaunch_that_opens_the_live_chat_it_was_offered_rejoins_it() {
    let mut wire = wire("rejoined").await;

    let handle = wire
        .client
        .open(Request::new(open_request("/projects/agens")))
        .await
        .expect("the chat opens")
        .into_inner();

    let rejoined = wire
        .client
        .open(Request::new(proto::OpenChatRequest {
            checkout: "/projects/agens".to_owned(),
            resume: Some(handle.session_id),
        }))
        .await
        .expect("the live chat is rejoined rather than refused")
        .into_inner();

    assert_eq!(rejoined.session_id, handle.session_id);

    wire.client
        .prompt(Request::new(proto::PromptRequest {
            session_id: handle.session_id,
            prompt: "still here".to_owned(),
            parts: Vec::new(),
        }))
        .await
        .expect("the live loop still serves prompts");
    assert_eq!(
        wire.started.recv_timeout(PATIENCE),
        Ok("still here".to_owned()),
    );
}

/// One daemon serves N projects, so a listing scoped to one checkout never
/// offers another project's conversation.
#[tokio::test(flavor = "multi_thread")]
async fn a_chat_open_for_another_checkout_is_not_offered_here() {
    let mut wire = wire("scoped").await;

    wire.client
        .open(Request::new(open_request("/projects/agens")))
        .await
        .expect("the chat opens");

    let elsewhere = wire
        .client
        .list(Request::new(proto::ListChatsRequest {
            checkout: "/projects/something-else".to_owned(),
        }))
        .await
        .expect("the listing is served")
        .into_inner();

    assert!(elsewhere.chats.is_empty());
}

/// A client that comes back mid-answer is told so, rather than being handed a
/// chat that looks idle while a turn is still producing.
#[tokio::test(flavor = "multi_thread")]
async fn a_chat_that_is_answering_says_so_in_the_listing() {
    let mut wire = wire("answering").await;

    let handle = wire
        .client
        .open(Request::new(open_request("/projects/agens")))
        .await
        .expect("the chat opens")
        .into_inner();

    wire.client
        .prompt(Request::new(proto::PromptRequest {
            session_id: handle.session_id,
            prompt: "take your time".to_owned(),
            parts: Vec::new(),
        }))
        .await
        .expect("the prompt is accepted");

    assert_eq!(
        wire.started.recv_timeout(PATIENCE),
        Ok("take your time".to_owned()),
    );

    let open = wire
        .client
        .list(Request::new(proto::ListChatsRequest {
            checkout: "/projects/agens".to_owned(),
        }))
        .await
        .expect("the listing is served")
        .into_inner();

    assert!(open.chats[0].answering);
}

/// A listing that names no checkout would be a listing across every project on
/// the machine.
#[tokio::test(flavor = "multi_thread")]
async fn a_listing_that_names_no_checkout_is_refused() {
    let mut wire = wire("unscoped-list").await;

    let refused = wire
        .client
        .list(Request::new(proto::ListChatsRequest {
            checkout: String::new(),
        }))
        .await
        .expect_err("a listing names its checkout");

    assert_eq!(refused.code(), Code::InvalidArgument);
}

/// A terminal that comes back draws the conversation it left rather than an
/// empty one.
#[tokio::test(flavor = "multi_thread")]
async fn the_conversation_so_far_comes_back_over_the_wire() {
    let mut wire = wire("history").await;

    let handle = wire
        .client
        .open(Request::new(open_request("/projects/agens")))
        .await
        .expect("the chat opens")
        .into_inner();

    let history = wire
        .client
        .history(Request::new(proto::ChatRef {
            session_id: handle.session_id,
        }))
        .await
        .expect("the conversation is served")
        .into_inner();

    assert_eq!(history.messages.len(), 1);
    assert_eq!(history.messages[0].role, "assistant");
    assert_eq!(
        history.messages[0].parts[0].part,
        Some(proto::message_part::Part::Text("what we said".to_owned())),
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_conversation_of_a_chat_nobody_opened_is_not_found() {
    let mut wire = wire("history-unknown").await;

    let refused = wire
        .client
        .history(Request::new(proto::ChatRef { session_id: 404 }))
        .await
        .expect_err("there is no such chat");

    assert_eq!(refused.code(), Code::NotFound);
}
