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
    hosted::{
        CatalogEntry, CatalogKind, CatalogResult, CatalogSnapshot, FileError, HostedChildTurn,
        HostedControlCommand, HostedControlKind, HostedControlResult, HostedMcpAction,
        HostedMcpControl, HostedMcpResult, HostedMcpServer, HostedMcpState, HostedTaskEvent,
        HostedTaskJournal, HostedTaskRecord, HostedTaskReplay, HostedTaskSnapshot, HostedTaskState,
        TaskControlError, WorkspaceFileContent,
    },
};
use agens_server::{
    BlockingBoundary, ChatFacade, ChatSession, ChatSessionRequest, ChatSessions, ChatTurnOutcome,
    ChatTurns, ConfinedWorkspaceFiles, HostedCatalogSet, SessionAdmission, SessionBudget,
    SessionId, SessionProvider, SessionRuntime, SessionSupervisor, grpc::proto,
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
    fn command(&mut self, command: &str) -> Result<String, agens_server::ChatError> {
        Ok(format!("executed:{command}"))
    }

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

struct StubMcp;
impl HostedMcpControl for StubMcp {
    fn status(&self) -> Vec<HostedMcpServer> {
        vec![HostedMcpServer::new("files", HostedMcpState::Idle, 0, None)]
    }
    fn control(&mut self, server: &str, action: HostedMcpAction) -> HostedMcpResult {
        let state = match action {
            HostedMcpAction::Connect | HostedMcpAction::Reconnect => HostedMcpState::Ready,
            HostedMcpAction::Disconnect => HostedMcpState::Closed,
        };
        HostedMcpResult::new(vec![HostedMcpServer::new(server, state, 1, None)], None)
    }
}

struct StubTasks;
impl HostedTaskJournal for StubTasks {
    fn append_event(
        &mut self,
        _: i64,
        _: &str,
        _: HostedTaskState,
        _: &str,
    ) -> Result<HostedTaskEvent, TaskControlError> {
        Err(TaskControlError::Storage)
    }
    fn persist_completed_child_turn(
        &mut self,
        _: i64,
        _: &str,
        _: u64,
        _: &str,
    ) -> Result<(), TaskControlError> {
        Ok(())
    }
    fn completed_child_turns(&self, _: i64) -> Result<Vec<HostedChildTurn>, TaskControlError> {
        Ok(vec![])
    }
    fn snapshot_tail(&self, session: i64) -> Result<HostedTaskReplay, TaskControlError> {
        if session != 41 {
            return Err(TaskControlError::WrongSession);
        }
        Ok(HostedTaskReplay::SnapshotTail {
            snapshot: HostedTaskSnapshot::new(
                7,
                vec![HostedTaskRecord::new("1", HostedTaskState::Running)],
            )
            .with_child_turns(vec![HostedChildTurn::new("1", 1, "completed child")]),
            events: vec![HostedTaskEvent::new(
                8,
                "1",
                HostedTaskState::Background,
                "detached",
            )],
        })
    }
    fn replay_after(&self, session: i64, _: u64) -> Result<HostedTaskReplay, TaskControlError> {
        self.snapshot_tail(session)
    }
    fn apply_control(
        &mut self,
        command: &HostedControlCommand,
    ) -> Result<HostedControlResult, TaskControlError> {
        if command.session_id() != 41 {
            return Err(TaskControlError::WrongSession);
        }
        let state = match command.kind() {
            HostedControlKind::Background | HostedControlKind::Message(_) => {
                HostedTaskState::Background
            }
            HostedControlKind::Cancel | HostedControlKind::CancelAll => HostedTaskState::Cancelled,
        };
        Ok(HostedControlResult::new(state, false))
    }
}

struct Served {
    coordinator: Coordinator,
    supervisor: SessionSupervisor,
    started: Receiver<String>,
    shutdown: HeadlessTurnCancellation,
    directory: PathBuf,
}

impl Drop for Served {
    fn drop(&mut self) {
        self.shutdown.cancel();
        self.supervisor.registry().cancel_all();
        let _ = fs::remove_dir_all(&self.directory);
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
            .add_service(proto::chat_server::ChatServer::new(
                ChatFacade::new(chats, blocking)
                    .with_hosted(
                        Arc::new(HostedCatalogSet::new(
                            Some(CatalogSnapshot::new(
                                "commands-v2",
                                vec![CatalogEntry::new("/help", "Show help", true)],
                            )),
                            None,
                        )),
                        Arc::new(ConfinedWorkspaceFiles::default()),
                    )
                    .with_hosted_mcp(Box::new(StubMcp))
                    .with_hosted_tasks(Box::new(StubTasks)),
            ))
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
        directory,
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

#[tokio::test(flavor = "multi_thread")]
async fn qualified_model_command_survives_the_client_round_trip() {
    let served = served(Vec::new()).await;
    let mut chat = served.coordinator.chat();
    let session = chat
        .open(&PathBuf::from("/projects/agens"), None)
        .await
        .expect("the chat opens");

    assert_eq!(
        chat.command(session, "/model openai-api/gpt-4.1")
            .await
            .expect("the hosted command executes"),
        "executed:/model openai-api/gpt-4.1"
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

#[tokio::test(flavor = "multi_thread")]
async fn catalogs_files_round_trip_preserves_typed_outcomes() {
    let served = served(Vec::new()).await;
    fs::write(served.directory.join("README.sh"), "#!/bin/sh\nfalse\n").unwrap();
    fs::write(served.directory.join(".gitignore"), "ignored.txt\n").unwrap();
    fs::write(served.directory.join("ignored.txt"), "secret").unwrap();
    let mut chat = served.coordinator.chat();

    assert!(matches!(
        chat.catalog(CatalogKind::Command, None).await.unwrap(),
        CatalogResult::Current(snapshot) if snapshot.revision() == "commands-v2"
    ));
    assert_eq!(
        chat.catalog(CatalogKind::Command, Some("commands-v1"))
            .await
            .unwrap(),
        CatalogResult::Stale {
            current_revision: "commands-v2".into()
        }
    );
    assert_eq!(
        chat.catalog(CatalogKind::Skill, None).await.unwrap(),
        CatalogResult::Unsupported
    );

    let listed = chat
        .list_workspace_files(&served.directory, PathBuf::from(".").as_path())
        .await
        .unwrap()
        .unwrap();
    assert!(
        listed
            .iter()
            .any(|file| file.path() == std::path::Path::new("README.sh"))
    );
    assert!(
        !listed
            .iter()
            .any(|file| file.path() == std::path::Path::new("ignored.txt"))
    );
    assert!(matches!(
        chat.read_workspace_file(&served.directory, std::path::Path::new("README.sh")).await.unwrap(),
        Ok(WorkspaceFileContent::Text { text, .. }) if text.contains("false")
    ));
    assert_eq!(
        chat.read_workspace_file(&served.directory, std::path::Path::new("../outside"))
            .await
            .unwrap(),
        Err(FileError::OutsideRoot)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn hosted_task_and_mcp_round_trip_preserves_resulting_state_and_replay() {
    let served = served(Vec::new()).await;
    let mut chat = served.coordinator.chat();

    let status = chat.mcp_status().await.unwrap();
    assert_eq!(status.servers()[0].state(), HostedMcpState::Idle);
    let connected = chat
        .mcp_control("files", HostedMcpAction::Connect)
        .await
        .unwrap();
    assert_eq!(connected.servers()[0].state(), HostedMcpState::Ready);
    let disconnected = chat
        .mcp_control("files", HostedMcpAction::Disconnect)
        .await
        .unwrap();
    assert_eq!(disconnected.servers()[0].state(), HostedMcpState::Closed);
    let reconnected = chat
        .mcp_control("files", HostedMcpAction::Reconnect)
        .await
        .unwrap();
    assert_eq!(reconnected.servers()[0].state(), HostedMcpState::Ready);

    let replay = chat.task_snapshot(41).await.unwrap().unwrap();
    let HostedTaskReplay::SnapshotTail { snapshot, events } = replay else {
        panic!("snapshot tail")
    };
    assert_eq!(snapshot.tasks()[0].state(), HostedTaskState::Running);
    assert_eq!(snapshot.child_turns()[0].payload(), "completed child");
    assert_eq!(events[0].state(), HostedTaskState::Background);

    for (id, kind, expected) in [
        (
            "background",
            HostedControlKind::Background,
            HostedTaskState::Background,
        ),
        (
            "message",
            HostedControlKind::Message("continue".into()),
            HostedTaskState::Background,
        ),
        (
            "cancel",
            HostedControlKind::Cancel,
            HostedTaskState::Cancelled,
        ),
        (
            "cancel-all",
            HostedControlKind::CancelAll,
            HostedTaskState::Cancelled,
        ),
    ] {
        let command = HostedControlCommand::new(41, Some("1".into()), id, kind);
        assert_eq!(
            chat.task_control(&command).await.unwrap().unwrap().state(),
            expected
        );
    }
    assert_eq!(
        chat.task_snapshot(99).await.unwrap(),
        Err(TaskControlError::WrongSession)
    );
}
