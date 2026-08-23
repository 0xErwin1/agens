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
    release: Arc<Mutex<Receiver<Script>>>,
}

impl ChatTurns for ScriptedTurns {
    fn run(
        &mut self,
        prompt: &str,
        _runtime: &SessionRuntime,
        _cancellation: &HeadlessTurnCancellation,
        progress: &agens_core::TurnProgressSink,
    ) -> ChatTurnOutcome {
        let _ = self.started.send(prompt.to_owned());

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
    release: Sender<Script>,
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

    let (started, started_rx) = channel();
    let (release, release_rx) = channel();
    let release_rx = Arc::new(Mutex::new(release_rx));

    let chats = Arc::new(ChatSessions::new(
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
                    release: Arc::clone(&release_rx),
                }),
            })
        }),
    ));

    let directory = scratch_directory(name);
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
        release,
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
