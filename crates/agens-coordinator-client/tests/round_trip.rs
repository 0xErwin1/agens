//! What the daemon sends and what this client reads, against each other.
//!
//! The two projections are written in different crates and can drift apart
//! silently: a field renamed on one side and not the other still compiles, and
//! the first thing anybody notices is a turn rendering as nothing. So this
//! serves the real facade over a real socket and asserts that a `TurnEvent`
//! handed to a hosted chat comes back out of the client as the same value.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agens_coordinator_client::{ClientError, Coordinator, HostedChatEvent};
use agens_core::{
    HeadlessTurnCancellation, IntraTurnInputSource, MessagePart, TurnEvent, TurnProgressSink,
    TurnRetryReason, TurnState, Usage,
};
use agens_server::{
    BlockingBoundary, ChatFacade, ChatSession, ChatSessionRequest, ChatSessions, ChatTurnOutcome,
    ChatTurns, SessionAdmission, SessionBudget, SessionId, SessionProvider, SessionRuntime,
    SessionSupervisor, grpc::proto,
};
use tokio_stream::StreamExt;
use tonic::transport::Server;

const PATIENCE: Duration = Duration::from_secs(5);

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

/// Every shape the chat plane carries, so a field dropped on either side of the
/// projection fails here rather than in a surface that renders nothing.
fn every_event() -> Vec<TurnEvent> {
    vec![
        TurnEvent::StateChanged(TurnState::Streaming),
        TurnEvent::ProviderPart(MessagePart::Text("an answer".to_owned())),
        TurnEvent::ProviderPart(MessagePart::Reasoning("a thought".to_owned())),
        TurnEvent::ProviderPart(MessagePart::Media {
            media_id: 7,
            mime: "image/png".to_owned(),
        }),
        TurnEvent::Usage(Usage {
            input_tokens: Some(11),
            output_tokens: Some(22),
            total_tokens: Some(33),
            context_window: Some(200_000),
        }),
        TurnEvent::ToolCallRequested {
            id: "call-1".to_owned(),
            name: "read".to_owned(),
            input: r#"{"path":"README.md"}"#.to_owned(),
        },
        TurnEvent::ToolResult(MessagePart::ToolResult {
            tool_call_id: "call-1".to_owned(),
            content: "a readme".to_owned(),
            is_error: false,
        }),
        TurnEvent::ProviderRetry {
            attempt: 2,
            max_attempts: Some(5),
            delay: Some(Duration::from_millis(750)),
            reason: TurnRetryReason::RateLimited,
        },
        TurnEvent::IntraTurnInput {
            source: IntraTurnInputSource::Human,
            text: "also check the tests".to_owned(),
        },
    ]
}

struct StubProvider;

impl SessionProvider for StubProvider {
    fn model(&self) -> &str {
        "stub/model"
    }
}

/// A turn that reports the events the test wrote and then ends.
struct ScriptedTurns {
    started: Sender<String>,
    events: Vec<TurnEvent>,
}

impl ChatTurns for ScriptedTurns {
    fn run(
        &mut self,
        message: &agens_core::SessionMessage,
        _runtime: &SessionRuntime,
        _cancellation: &HeadlessTurnCancellation,
        _asks: &Arc<dyn agens_server::ChatAsks>,
        progress: &TurnProgressSink,
    ) -> ChatTurnOutcome {
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

        for event in self.events.clone() {
            progress(event);
        }

        ChatTurnOutcome::Completed("done".to_owned())
    }
}

struct Served {
    coordinator: Coordinator,
    supervisor: SessionSupervisor,
    started: Receiver<String>,
    shutdown: HeadlessTurnCancellation,
}

impl Drop for Served {
    fn drop(&mut self) {
        self.shutdown.cancel();
        self.supervisor.registry().cancel_all();
    }
}

async fn served(events: Vec<TurnEvent>) -> Served {
    let supervisor = SessionSupervisor::new(tokio::runtime::Handle::current());
    let (started, started_rx) = channel();
    let events = Arc::new(Mutex::new(events));

    let chats = Arc::new(ChatSessions::new(
        supervisor.clone(),
        Arc::new(move |_: &ChatSessionRequest| {
            Ok(ChatSession {
                admission: SessionAdmission::new(
                    SessionId::new(1),
                    Box::new(StubProvider),
                    SessionBudget::unlimited(),
                ),
                turns: Box::new(ScriptedTurns {
                    started: started.clone(),
                    events: events.lock().expect("the script is readable").clone(),
                }),
            })
        }),
        // No test here reads a chat back, so reaching this is a test asking for
        // something it did not set up.
        Arc::new(|_| Err(agens_server::ChatError::Unknown)),
    ));

    let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    let directory = std::env::temp_dir().join(format!(
        "agens-client-round-trip-{}-{suffix}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).expect("the scratch directory is writable");

    let socket = directory.join("facade.sock");
    let listener = tokio::net::UnixListener::bind(&socket).expect("the socket binds");
    let shutdown = HeadlessTurnCancellation::new();
    let parked = shutdown.clone();
    let blocking = BlockingBoundary::new(tokio::runtime::Handle::current());

    tokio::spawn(async move {
        Server::builder()
            .add_service(proto::chat_server::ChatServer::new(ChatFacade::new(
                chats, blocking,
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

    let coordinator = attach(&socket).await;

    Served {
        coordinator,
        supervisor,
        started: started_rx,
        shutdown,
    }
}

/// The facade binds after this task starts asking, so attaching retries rather
/// than betting on a sleep.
async fn attach(socket: &std::path::Path) -> Coordinator {
    for _ in 0..200 {
        if let Ok(coordinator) = Coordinator::attach(socket).await {
            return coordinator;
        }

        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    panic!("the facade never accepted on its socket");
}

#[tokio::test(flavor = "multi_thread")]
async fn every_event_a_turn_produces_survives_the_round_trip_unchanged() {
    let served = served(every_event()).await;
    let mut chat = served.coordinator.chat();

    let session = chat
        .open(&PathBuf::from("/projects/agens"), None)
        .await
        .expect("the chat opens");
    let mut events = chat.subscribe(session).await.expect("the chat is open");

    chat.prompt(session, "tell me everything")
        .await
        .expect("the prompt is accepted");

    assert_eq!(
        served.started.recv_timeout(PATIENCE),
        Ok("tell me everything".to_owned()),
    );

    let mut seen = Vec::new();
    loop {
        let event = tokio::time::timeout(PATIENCE, events.next())
            .await
            .expect("the chat published within the wait")
            .expect("the stream is healthy")
            .expect("the event is readable");

        match event {
            HostedChatEvent::Progress(progress) => seen.push(progress),
            HostedChatEvent::TurnCompleted { text } => {
                assert_eq!(text, "done");
                break;
            }
            other => panic!("the turn ended unexpectedly: {other:?}"),
        }
    }

    assert_eq!(
        seen,
        every_event(),
        "what the daemon sent and what the client read are the same values"
    );
}

/// The one failure a surface acts on without reading a message: there is
/// nothing to talk to, so the answer is to start one.
#[tokio::test(flavor = "multi_thread")]
async fn attaching_where_no_daemon_listens_says_so_rather_than_hanging() {
    let nowhere = std::env::temp_dir().join(format!("agens-absent-{}.sock", std::process::id()));
    let _ = fs::remove_file(&nowhere);

    assert!(matches!(
        Coordinator::attach(&nowhere).await,
        Err(ClientError::NotRunning(_))
    ));
}
