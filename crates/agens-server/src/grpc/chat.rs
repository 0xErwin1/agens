//! The Chat plane over the wire: the user's own conversation, hosted here.
//!
//! Every method is a transport onto [`ChatSessions`] and decides nothing. The
//! one thing it does decide is where the work runs: opening a chat builds a
//! session, and a session is built out of a store, a provider client and a
//! project on disk, so the call crosses the daemon's blocking boundary rather
//! than stalling a runtime worker with it.
//!
//! `Prompt` answers as soon as the turn has somewhere to start, never when the
//! turn ends. A turn that a client had to wait out would be a turn that ends
//! when the client goes away, which is the arrangement this plane exists to
//! replace.

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex};

use agens_core::ask_user::{AskUserAnswer, AskUserReply, AskUserUnavailable};
use agens_core::hosted::{
    CatalogKind, CatalogResult, FileError, HostedCatalogs, HostedControlCommand, HostedControlKind,
    HostedMcpAction, HostedMcpControl, HostedMcpResult, HostedMcpState, HostedTaskJournal,
    HostedTaskReplay, HostedTaskState, HostedWorkspaceFiles, TaskControlError,
    WorkspaceFileContent, WorkspaceFileKind,
};
use tokio_stream::{Stream, wrappers::ReceiverStream};
use tonic::{Request, Response, Status};

use agens_core::{Message, MessagePart, Role, SessionMessage};

use super::proto::chat_server::Chat;
use super::subscriptions::{
    FORWARD_PATIENCE, LIVE_SUBSCRIPTIONS, SUBSCRIPTION_BUFFER, SubscriptionSlots, forward,
};
use super::{proto, turn};
use crate::blocking::{BlockingBoundary, BlockingError};
use crate::chat::{ChatError, ChatSessionRequest, ChatSessions};
use crate::sessions::SessionId;
use crate::{ConfinedWorkspaceFiles, HostedCatalogSet};

pub struct ChatFacade {
    chats: Arc<ChatSessions>,
    blocking: BlockingBoundary,
    slots: Arc<SubscriptionSlots>,
    catalogs: Arc<dyn HostedCatalogs>,
    files: Arc<dyn HostedWorkspaceFiles>,
    mcp: Option<Arc<Mutex<Box<dyn HostedMcpControl>>>>,
    tasks: Option<Arc<Mutex<Box<dyn HostedTaskJournal>>>>,
}

impl ChatFacade {
    #[must_use]
    pub fn new(chats: Arc<ChatSessions>, blocking: BlockingBoundary) -> Self {
        Self::with_subscription_ceiling(chats, blocking, LIVE_SUBSCRIPTIONS)
    }

    /// A facade that forwards at most `ceiling` subscriptions at once.
    ///
    /// Exists for the tests that drive the ceiling itself: reaching the
    /// production one over the wire would mean opening sixty-four streams to
    /// assert about the sixty-fifth.
    #[must_use]
    pub fn with_subscription_ceiling(
        chats: Arc<ChatSessions>,
        blocking: BlockingBoundary,
        ceiling: usize,
    ) -> Self {
        Self {
            chats,
            blocking,
            slots: Arc::new(SubscriptionSlots::new(ceiling)),
            catalogs: Arc::new(HostedCatalogSet::default()),
            files: Arc::new(ConfinedWorkspaceFiles::default()),
            mcp: None,
            tasks: None,
        }
    }

    #[must_use]
    pub fn with_hosted(
        mut self,
        catalogs: Arc<dyn HostedCatalogs>,
        files: Arc<dyn HostedWorkspaceFiles>,
    ) -> Self {
        self.catalogs = catalogs;
        self.files = files;
        self
    }

    #[must_use]
    pub fn with_hosted_mcp(mut self, mcp: Box<dyn HostedMcpControl>) -> Self {
        self.mcp = Some(Arc::new(Mutex::new(mcp)));
        self
    }

    pub fn with_hosted_tasks(mut self, tasks: Box<dyn HostedTaskJournal>) -> Self {
        self.tasks = Some(Arc::new(Mutex::new(tasks)));
        self
    }

    /// Runs one synchronous chat operation off the runtime's workers.
    async fn off_runtime<T, F>(&self, work: F) -> Result<T, Status>
    where
        F: FnOnce(&ChatSessions) -> Result<T, ChatError> + Send + 'static,
        T: Send + 'static,
    {
        let chats = Arc::clone(&self.chats);

        self.blocking
            .run(move || work(&chats))
            .await
            .map_err(unavailable)?
            .map_err(refusal)
    }
}

type SessionEventStream = Pin<Box<dyn Stream<Item = Result<proto::SessionEvent, Status>> + Send>>;

#[tonic::async_trait]
impl Chat for ChatFacade {
    type SubscribeStream = SessionEventStream;

    /// The attach handshake. Answered from compiled-in constants and one lock,
    /// never from the store: a client asking what this process is must get an
    /// answer even when the daemon's own journal is what the skew broke.
    async fn build_info(
        &self,
        _request: Request<proto::BuildInfoRequest>,
    ) -> Result<Response<proto::DaemonBuild>, Status> {
        let answering = i64::try_from(self.chats.answering_chats()).unwrap_or(i64::MAX);

        Ok(Response::new(proto::DaemonBuild {
            wire_revision: crate::identity::WIRE_REVISION,
            build: crate::identity::BUILD_STAMP.to_owned(),
            answering_chats: answering,
        }))
    }

    async fn catalog(
        &self,
        request: Request<proto::HostedCatalogRequest>,
    ) -> Result<Response<proto::HostedCatalogResult>, Status> {
        let request = request.into_inner();
        let kind = match proto::HostedCatalogKind::try_from(request.kind) {
            Ok(proto::HostedCatalogKind::Command) => CatalogKind::Command,
            Ok(proto::HostedCatalogKind::Skill) => CatalogKind::Skill,
            _ => return Err(Status::invalid_argument("catalog kind is required")),
        };
        let catalogs = Arc::clone(&self.catalogs);
        let result = self
            .blocking
            .run(move || catalogs.catalog(kind, request.known_revision.as_deref()))
            .await
            .map_err(unavailable)?;
        Ok(Response::new(encode_catalog(result)))
    }

    async fn list_workspace_files(
        &self,
        request: Request<proto::WorkspaceFilesRequest>,
    ) -> Result<Response<proto::HostedWorkspaceFilesResult>, Status> {
        let request = request.into_inner();
        let root = checkout(request.checkout)?;
        let selector = PathBuf::from(request.selector);
        let files = Arc::clone(&self.files);
        let result = self
            .blocking
            .run(move || files.list(&root, &selector))
            .await
            .map_err(unavailable)?;
        Ok(Response::new(encode_file_list(result)))
    }

    async fn read_workspace_file(
        &self,
        request: Request<proto::WorkspaceFileRequest>,
    ) -> Result<Response<proto::HostedWorkspaceFileResult>, Status> {
        let request = request.into_inner();
        let root = checkout(request.checkout)?;
        let selector = PathBuf::from(request.selector);
        let files = Arc::clone(&self.files);
        let result = self
            .blocking
            .run(move || files.read(&root, &selector))
            .await
            .map_err(unavailable)?;
        Ok(Response::new(encode_file(result)))
    }

    async fn mcp_status(
        &self,
        _request: Request<proto::HostedMcpStatusRequest>,
    ) -> Result<Response<proto::HostedMcpResult>, Status> {
        let mcp = self
            .mcp
            .clone()
            .ok_or_else(|| Status::unimplemented("hosted MCP is unavailable"))?;
        let result = self
            .blocking
            .run(move || {
                mcp.lock()
                    .map_err(|_| ())
                    .map(|mcp| HostedMcpResult::new(mcp.status(), None))
            })
            .await
            .map_err(unavailable)?
            .map_err(|()| Status::internal("hosted MCP is unavailable"))?;
        Ok(Response::new(encode_mcp_result(result)))
    }

    async fn mcp_control(
        &self,
        request: Request<proto::HostedMcpControlRequest>,
    ) -> Result<Response<proto::HostedMcpResult>, Status> {
        let request = request.into_inner();
        if request.server.is_empty() {
            return Err(Status::invalid_argument("MCP server is required"));
        }
        let action = match proto::HostedMcpAction::try_from(request.action) {
            Ok(proto::HostedMcpAction::Connect) => HostedMcpAction::Connect,
            Ok(proto::HostedMcpAction::Disconnect) => HostedMcpAction::Disconnect,
            Ok(proto::HostedMcpAction::Reconnect) => HostedMcpAction::Reconnect,
            _ => return Err(Status::invalid_argument("MCP action is required")),
        };
        let mcp = self
            .mcp
            .clone()
            .ok_or_else(|| Status::unimplemented("hosted MCP is unavailable"))?;
        let result = self
            .blocking
            .run(move || {
                mcp.lock()
                    .map_err(|_| ())
                    .map(|mut mcp| mcp.control(&request.server, action))
            })
            .await
            .map_err(unavailable)?
            .map_err(|()| Status::internal("hosted MCP is unavailable"))?;
        Ok(Response::new(encode_mcp_result(result)))
    }

    async fn task_snapshot(
        &self,
        request: Request<proto::HostedTaskSnapshotRequest>,
    ) -> Result<Response<proto::HostedTaskReplayResult>, Status> {
        let session = request.into_inner().session_id;
        let tasks = self
            .tasks
            .clone()
            .ok_or_else(|| Status::unimplemented("hosted task journal is unavailable"))?;
        let result = self
            .blocking
            .run(move || {
                tasks
                    .lock()
                    .map_err(|_| TaskControlError::Storage)?
                    .snapshot_tail(session)
            })
            .await
            .map_err(unavailable)?;
        Ok(Response::new(encode_task_replay(result)))
    }

    async fn task_replay(
        &self,
        request: Request<proto::HostedTaskReplayRequest>,
    ) -> Result<Response<proto::HostedTaskReplayResult>, Status> {
        let request = request.into_inner();
        let tasks = self
            .tasks
            .clone()
            .ok_or_else(|| Status::unimplemented("hosted task journal is unavailable"))?;
        let result = self
            .blocking
            .run(move || {
                tasks
                    .lock()
                    .map_err(|_| TaskControlError::Storage)?
                    .replay_after(request.session_id, request.after_cursor)
            })
            .await
            .map_err(unavailable)?;
        Ok(Response::new(encode_task_replay(result)))
    }

    async fn task_control(
        &self,
        request: Request<proto::HostedTaskControlRequest>,
    ) -> Result<Response<proto::HostedTaskControlResult>, Status> {
        let request = request.into_inner();
        let kind = decode_control_kind(&request)?;
        let command = HostedControlCommand::new(
            request.session_id,
            (!request.task_id.is_empty()).then_some(request.task_id),
            request.command_id,
            kind,
        );
        let tasks = self
            .tasks
            .clone()
            .ok_or_else(|| Status::unimplemented("hosted task journal is unavailable"))?;
        let result = self
            .blocking
            .run(move || {
                tasks
                    .lock()
                    .map_err(|_| TaskControlError::Storage)?
                    .apply_control(&command)
            })
            .await
            .map_err(unavailable)?;
        Ok(Response::new(encode_control_result(result)))
    }

    async fn open(
        &self,
        request: Request<proto::OpenChatRequest>,
    ) -> Result<Response<proto::ChatHandle>, Status> {
        let request = request.into_inner();
        let checkout = checkout(request.checkout)?;

        let opened = ChatSessionRequest {
            checkout,
            resume: request.resume,
        };

        let session = self.off_runtime(move |chats| chats.open(&opened)).await?;

        Ok(Response::new(proto::ChatHandle {
            session_id: session.session.value(),
            supports_prompt_parts: true,
            provider: session.presentation.provider,
            model: session.presentation.model,
            reasoning_effort: session.presentation.reasoning_effort,
            context_window: session.presentation.context_window,
            bypass_permissions: Some(session.presentation.bypass_permissions),
            dangerous_mode: Some(session.presentation.dangerous_mode),
        }))
    }

    async fn prompt(
        &self,
        request: Request<proto::PromptRequest>,
    ) -> Result<Response<proto::ChatAck>, Status> {
        let request = request.into_inner();
        let session = SessionId::new(request.session_id);
        let message = prompt_message(request)?;

        self.off_runtime(move |chats| chats.prompt(session, message))
            .await?;

        Ok(Response::new(proto::ChatAck {}))
    }

    async fn command(
        &self,
        request: Request<proto::ChatCommandRequest>,
    ) -> Result<Response<proto::ChatCommandResult>, Status> {
        let request = request.into_inner();
        let session = SessionId::new(request.session_id);
        let command = request.command;
        let message = self
            .off_runtime(move |chats| chats.command(session, command))
            .await?;

        Ok(Response::new(proto::ChatCommandResult { message }))
    }

    async fn cancel(
        &self,
        request: Request<proto::ChatRef>,
    ) -> Result<Response<proto::ChatAck>, Status> {
        let session = SessionId::new(request.into_inner().session_id);

        self.off_runtime(move |chats| chats.cancel(session)).await?;

        Ok(Response::new(proto::ChatAck {}))
    }

    async fn close(
        &self,
        request: Request<proto::ChatRef>,
    ) -> Result<Response<proto::ChatAck>, Status> {
        let session = SessionId::new(request.into_inner().session_id);

        self.off_runtime(move |chats| chats.close(session)).await?;

        Ok(Response::new(proto::ChatAck {}))
    }

    /// What is already open for a checkout, newest first — or every open chat
    /// on the machine when no checkout is named.
    ///
    /// The scoped form exists for attach safety: a daemon serves N projects,
    /// and a terminal offered another project's conversation is a terminal
    /// that can attach to the wrong one. The unscoped form is the operator's
    /// read-only board, and nothing attaches through it.
    async fn list(
        &self,
        request: Request<proto::ListChatsRequest>,
    ) -> Result<Response<proto::OpenChats>, Status> {
        let checkout = request.into_inner().checkout;

        let chats = if checkout.is_empty() {
            self.off_runtime(|chats| chats.open_chats_snapshot())
                .await?
        } else {
            let checkout = PathBuf::from(checkout);
            self.off_runtime(move |chats| chats.open_against(&checkout))
                .await?
        };

        Ok(Response::new(proto::OpenChats {
            chats: chats
                .into_iter()
                .map(|chat| proto::OpenChat {
                    session_id: chat.session_id.value(),
                    checkout: chat.checkout.display().to_string(),
                    answering: chat.answering,
                })
                .collect(),
        }))
    }

    /// Answers a question the chat's turn is stopped on.
    async fn answer_permission(
        &self,
        request: Request<proto::AnswerPermissionRequest>,
    ) -> Result<Response<proto::ChatAck>, Status> {
        let request = request.into_inner();
        let session = SessionId::new(request.session_id);
        let prompt_id = request.prompt_id;

        let answer = request.answer;
        self.off_runtime(move |chats| chats.answer_value(session, prompt_id, &answer))
            .await?;

        Ok(Response::new(proto::ChatAck {}))
    }

    async fn answer_ask_user(
        &self,
        request: Request<proto::AnswerAskUserRequest>,
    ) -> Result<Response<proto::ChatAck>, Status> {
        let request = request.into_inner();
        let session = SessionId::new(request.session_id);
        let prompt_id = request.prompt_id;
        let answer = ask_user_reply(
            request
                .reply
                .ok_or_else(|| Status::invalid_argument("an ask-user answer carried nothing"))?,
        )?;

        self.off_runtime(move |chats| chats.answer_ask_user(session, prompt_id, answer))
            .await?;

        Ok(Response::new(proto::ChatAck {}))
    }

    /// What the chat has said so far.
    async fn history(
        &self,
        request: Request<proto::ChatRef>,
    ) -> Result<Response<proto::ChatHistory>, Status> {
        let session = SessionId::new(request.into_inner().session_id);

        let messages = self
            .off_runtime(move |chats| chats.history(session))
            .await?;

        Ok(Response::new(proto::ChatHistory {
            messages: messages.iter().map(turn::message).collect(),
        }))
    }

    /// Opens a subscription to one chat and forwards it to the client.
    ///
    /// One thread per subscriber, for the reason the journal's `Subscribe` has
    /// one: the chat's end of the fan-out is a synchronous channel, and a
    /// forwarder that spends its whole life parked on one has no business
    /// holding a slot on the pool every other operation crosses into.
    ///
    /// It ends when the chat stops publishing, when the client hangs up, or
    /// when the client stops reading for longer than the forwarder waits.
    async fn subscribe(
        &self,
        request: Request<proto::ChatRef>,
    ) -> Result<Response<Self::SubscribeStream>, Status> {
        let session = SessionId::new(request.into_inner().session_id);

        // Before the subscription is opened rather than after: a subscription
        // the chat registered and nothing forwards is an event queued for a
        // reader that will never come, held against the fan-out's backlog.
        let Some(slot) = self.slots.take() else {
            return Err(Status::resource_exhausted(
                "this daemon is forwarding as many subscriptions as it can; \
                 close one before opening another",
            ));
        };

        let events = self
            .off_runtime(move |chats| chats.subscribe(session))
            .await?;

        let (sender, receiver) = tokio::sync::mpsc::channel(SUBSCRIPTION_BUFFER);
        let session_id = session.value();

        std::thread::spawn(move || {
            // Moved into the forwarder so the slot is released by the thread
            // ending, whichever of the three ways it ends.
            let _slot = slot;

            loop {
                match events.recv_timeout(FORWARD_PATIENCE) {
                    Ok(event) => {
                        // An event this wire has no projection for is skipped
                        // rather than ending the stream: it carries no state a
                        // later event depends on, and nothing downstream counts
                        // events.
                        let Some(event) = turn::session_event(session_id, &event) else {
                            continue;
                        };

                        if !forward(&sender, Ok(event), FORWARD_PATIENCE) {
                            return;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) if sender.is_closed() => return,
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }
}

fn ask_user_reply(reply: proto::AskUserReply) -> Result<AskUserReply, Status> {
    use proto::ask_user_reply::Reply;

    match reply
        .reply
        .ok_or_else(|| Status::invalid_argument("an ask-user reply carried nothing"))?
    {
        Reply::Answered(answered) => Ok(AskUserReply::Answered(
            answered
                .answers
                .into_iter()
                .map(|answer| AskUserAnswer {
                    question_id: answer.question_id,
                    selected: answer.selected,
                    other: answer.other,
                    note: answer.note,
                })
                .collect(),
        )),
        Reply::Discuss(discuss) => Ok(AskUserReply::Discuss {
            question_id: discuss.question_id,
            note: discuss.note,
        }),
        Reply::Cancelled(_) => Ok(AskUserReply::Cancelled),
        Reply::Unavailable(unavailable) => Ok(AskUserReply::Unavailable(
            match unavailable.reason.as_str() {
                "no_interactive_surface" => AskUserUnavailable::NoInteractiveSurface,
                "surface_closed" => AskUserUnavailable::SurfaceClosed,
                _ => {
                    return Err(Status::invalid_argument(
                        "unknown ask-user unavailable reason",
                    ));
                }
            },
        )),
    }
}

fn encode_catalog(result: CatalogResult) -> proto::HostedCatalogResult {
    use proto::hosted_catalog_result::Result;
    let result = match result {
        CatalogResult::Current(snapshot) => Result::Current(proto::HostedCatalogSnapshot {
            revision: snapshot.revision().to_owned(),
            entries: snapshot
                .entries()
                .iter()
                .map(|entry| proto::HostedCatalogEntry {
                    name: entry.name().to_owned(),
                    description: entry.description().to_owned(),
                    built_in: entry.built_in(),
                })
                .collect(),
        }),
        CatalogResult::Stale { current_revision } => {
            Result::Stale(proto::HostedCatalogStale { current_revision })
        }
        CatalogResult::Unsupported => Result::Unsupported(proto::HostedUnsupported {}),
    };
    proto::HostedCatalogResult {
        result: Some(result),
    }
}

fn encode_file_list(
    result: Result<Vec<agens_core::hosted::WorkspaceFile>, FileError>,
) -> proto::HostedWorkspaceFilesResult {
    use proto::hosted_workspace_files_result::Result;
    let result = match result {
        Ok(files) => Result::Files(proto::HostedWorkspaceFileList {
            files: files
                .into_iter()
                .map(|file| proto::HostedWorkspaceFile {
                    path: file.path().display().to_string(),
                    byte_len: file.byte_len(),
                    kind: encode_file_kind(file.kind()) as i32,
                })
                .collect(),
        }),
        Err(error) => Result::Error(encode_file_error(error) as i32),
    };
    proto::HostedWorkspaceFilesResult {
        result: Some(result),
    }
}

fn encode_file(
    result: Result<WorkspaceFileContent, FileError>,
) -> proto::HostedWorkspaceFileResult {
    use proto::hosted_workspace_file_result::Result;
    let result = match result {
        Ok(WorkspaceFileContent::Text { path, text }) => {
            Result::Text(proto::HostedWorkspaceFileText {
                path: path.display().to_string(),
                text,
            })
        }
        Ok(WorkspaceFileContent::Media {
            path,
            mime,
            bytes,
            media_id,
            ..
        }) => Result::Media(proto::HostedWorkspaceFileMedia {
            path: path.display().to_string(),
            mime,
            bytes,
            media_id,
        }),
        Err(error) => Result::Error(encode_file_error(error) as i32),
    };
    proto::HostedWorkspaceFileResult {
        result: Some(result),
    }
}

const fn encode_file_kind(kind: WorkspaceFileKind) -> proto::HostedFileKind {
    match kind {
        WorkspaceFileKind::Text => proto::HostedFileKind::Text,
        WorkspaceFileKind::Media => proto::HostedFileKind::Media,
    }
}

const fn encode_file_error(error: FileError) -> proto::HostedFileError {
    match error {
        FileError::InvalidSelector => proto::HostedFileError::InvalidSelector,
        FileError::OutsideRoot => proto::HostedFileError::OutsideRoot,
        FileError::Ignored => proto::HostedFileError::Ignored,
        FileError::Missing => proto::HostedFileError::Missing,
        FileError::Unsupported => proto::HostedFileError::Unsupported,
        FileError::Oversized => proto::HostedFileError::Oversized,
        FileError::EntryLimit => proto::HostedFileError::EntryLimit,
        FileError::Unreadable => proto::HostedFileError::Unreadable,
    }
}

/// The checkout a chat's tools run in.
///
/// Empty is refused rather than read as "wherever the daemon happens to be":
/// proto3 cannot tell an unset string from an empty one, so a client that
/// forgot the field would otherwise open a chat rooted at the daemon's own
/// working directory, which is nobody's project.
fn checkout(checkout: String) -> Result<PathBuf, Status> {
    if checkout.is_empty() {
        return Err(Status::invalid_argument(
            "a chat names the checkout its tools run in",
        ));
    }

    Ok(PathBuf::from(checkout))
}

/// A refusal the chat plane made, as the client's own status.
///
/// `Busy` is `failed_precondition` rather than `resource_exhausted`: nothing is
/// exhausted and waiting changes nothing, because what has to happen first is
/// the turn already running finishing.
fn refusal(error: ChatError) -> Status {
    match error {
        ChatError::Unknown => Status::not_found(error.to_string()),
        ChatError::Busy => Status::failed_precondition(error.to_string()),
        // Not `not_found`: the chat is there, and what is gone is the question.
        // A client that answered something already resolved has to know the
        // difference in order to stop showing it.
        ChatError::NotAsked => Status::failed_precondition(error.to_string()),
        ChatError::InvalidMessage => Status::invalid_argument(error.to_string()),
        ChatError::Unavailable(detail) => Status::unavailable(detail),
    }
}

fn prompt_message(request: proto::PromptRequest) -> Result<SessionMessage, Status> {
    let parts = if request.parts.is_empty() {
        vec![MessagePart::Text(request.prompt)]
    } else {
        request
            .parts
            .into_iter()
            .map(|part| match part.part {
                Some(proto::message_part::Part::Text(text)) => Ok(MessagePart::Text(text)),
                Some(proto::message_part::Part::Media(media))
                    if agens_store::is_media_mime(&media.mime) =>
                {
                    Ok(MessagePart::Media {
                        media_id: media.media_id,
                        mime: media.mime,
                    })
                }
                Some(proto::message_part::Part::Media(_)) => {
                    Err(Status::invalid_argument("the attached message is invalid"))
                }
                _ => Err(Status::invalid_argument(
                    "attached chat accepts only text and media parts",
                )),
            })
            .collect::<Result<Vec<_>, _>>()?
    };

    SessionMessage::try_from(Message {
        role: Role::User,
        parts,
    })
    .map_err(|_| Status::invalid_argument("the attached message is invalid"))
}

fn unavailable(error: BlockingError) -> Status {
    match error {
        BlockingError::Panicked => Status::internal("the chat plane failed to answer"),
        BlockingError::ShuttingDown => Status::unavailable("the daemon is shutting down"),
    }
}

fn decode_control_kind(
    request: &proto::HostedTaskControlRequest,
) -> Result<HostedControlKind, Status> {
    match proto::HostedTaskControlKind::try_from(request.kind) {
        Ok(proto::HostedTaskControlKind::Background) => Ok(HostedControlKind::Background),
        Ok(proto::HostedTaskControlKind::Cancel) => Ok(HostedControlKind::Cancel),
        Ok(proto::HostedTaskControlKind::CancelAll) => Ok(HostedControlKind::CancelAll),
        Ok(proto::HostedTaskControlKind::Message) if !request.message.is_empty() => {
            Ok(HostedControlKind::Message(request.message.clone()))
        }
        _ => Err(Status::invalid_argument(
            "hosted task control kind is required",
        )),
    }
}

fn encode_mcp_result(result: HostedMcpResult) -> proto::HostedMcpResult {
    proto::HostedMcpResult {
        servers: result
            .servers()
            .iter()
            .map(|server| proto::HostedMcpServer {
                name: server.name().to_owned(),
                state: encode_mcp_state(server.state()) as i32,
                generation: server.generation(),
                error: server.error().map(str::to_owned),
            })
            .collect(),
        error: result.error().map(str::to_owned),
    }
}

const fn encode_mcp_state(state: HostedMcpState) -> proto::HostedMcpState {
    match state {
        HostedMcpState::Disabled => proto::HostedMcpState::Disabled,
        HostedMcpState::Idle => proto::HostedMcpState::Idle,
        HostedMcpState::Connecting => proto::HostedMcpState::Connecting,
        HostedMcpState::Ready => proto::HostedMcpState::Ready,
        HostedMcpState::Degraded => proto::HostedMcpState::Degraded,
        HostedMcpState::Failed => proto::HostedMcpState::Failed,
        HostedMcpState::Closed => proto::HostedMcpState::Closed,
    }
}

fn encode_task_replay(
    result: Result<HostedTaskReplay, TaskControlError>,
) -> proto::HostedTaskReplayResult {
    use proto::hosted_task_replay_result::Result;
    let result = match result {
        Ok(HostedTaskReplay::Events(events)) => Result::Events(proto::HostedTaskEvents {
            events: events.into_iter().map(encode_task_event).collect(),
        }),
        Ok(HostedTaskReplay::SnapshotTail { snapshot, events }) => {
            Result::SnapshotTail(proto::HostedTaskSnapshotTail {
                snapshot: Some(proto::HostedTaskSnapshot {
                    cursor: snapshot.cursor(),
                    tasks: snapshot
                        .tasks()
                        .iter()
                        .map(|task| proto::HostedTaskRecord {
                            task_id: task.task_id().to_owned(),
                            state: encode_task_state(task.state()) as i32,
                        })
                        .collect(),
                    child_turns: snapshot
                        .child_turns()
                        .iter()
                        .map(|turn| proto::HostedChildTurn {
                            task_id: turn.task_id().to_owned(),
                            sequence: turn.sequence(),
                            payload: turn.payload().to_owned(),
                        })
                        .collect(),
                }),
                events: events.into_iter().map(encode_task_event).collect(),
            })
        }
        Ok(HostedTaskReplay::Gap { oldest_cursor }) => {
            Result::Gap(proto::HostedTaskGap { oldest_cursor })
        }
        Err(error) => Result::Error(encode_task_error(error) as i32),
    };
    proto::HostedTaskReplayResult {
        result: Some(result),
    }
}

fn encode_control_result(
    result: Result<agens_core::hosted::HostedControlResult, TaskControlError>,
) -> proto::HostedTaskControlResult {
    use proto::hosted_task_control_result::Result;
    let result = match result {
        Ok(applied) => Result::Applied(proto::HostedTaskControlApplied {
            state: encode_task_state(applied.state()) as i32,
            replayed: applied.replayed(),
        }),
        Err(error) => Result::Error(encode_task_error(error) as i32),
    };
    proto::HostedTaskControlResult {
        result: Some(result),
    }
}

fn encode_task_event(event: agens_core::hosted::HostedTaskEvent) -> proto::HostedTaskEvent {
    proto::HostedTaskEvent {
        cursor: event.cursor(),
        task_id: event.task_id().to_owned(),
        state: encode_task_state(event.state()) as i32,
        payload: event.payload().to_owned(),
    }
}
const fn encode_task_state(state: HostedTaskState) -> proto::HostedTaskState {
    match state {
        HostedTaskState::Running => proto::HostedTaskState::Running,
        HostedTaskState::Background => proto::HostedTaskState::Background,
        HostedTaskState::Completed => proto::HostedTaskState::Completed,
        HostedTaskState::Cancelled => proto::HostedTaskState::Cancelled,
        HostedTaskState::Failed => proto::HostedTaskState::Failed,
    }
}
const fn encode_task_error(error: TaskControlError) -> proto::HostedTaskError {
    match error {
        TaskControlError::WrongSession => proto::HostedTaskError::WrongSession,
        TaskControlError::UnknownTask => proto::HostedTaskError::UnknownTask,
        TaskControlError::InvalidTransition => proto::HostedTaskError::InvalidTransition,
        TaskControlError::CommandConflict => proto::HostedTaskError::CommandConflict,
        TaskControlError::ControlCapacity => proto::HostedTaskError::ControlCapacity,
        TaskControlError::InvalidRequest => proto::HostedTaskError::InvalidRequest,
        TaskControlError::Storage => proto::HostedTaskError::Storage,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_parts_reject_unset_provider_and_tool_shapes() {
        let invalid = [
            proto::MessagePart { part: None },
            proto::MessagePart {
                part: Some(proto::message_part::Part::Reasoning("no".into())),
            },
            proto::MessagePart {
                part: Some(proto::message_part::Part::ToolCall(proto::ToolCall {
                    id: "call".into(),
                    name: "bash".into(),
                    input: "{}".into(),
                })),
            },
            proto::MessagePart {
                part: Some(proto::message_part::Part::ToolResult(proto::ToolResult {
                    tool_call_id: "call".into(),
                    content: "no".into(),
                    is_error: false,
                })),
            },
        ];

        for part in invalid {
            assert!(
                prompt_message(proto::PromptRequest {
                    session_id: 1,
                    prompt: "compatibility text".into(),
                    parts: vec![part],
                })
                .is_err()
            );
        }
    }

    #[test]
    fn legacy_prompt_and_media_only_parts_are_distinct_valid_messages() {
        let legacy = prompt_message(proto::PromptRequest {
            session_id: 1,
            prompt: "legacy".into(),
            parts: Vec::new(),
        })
        .unwrap();
        assert_eq!(
            legacy.as_message().parts,
            vec![MessagePart::Text("legacy".into())]
        );

        let media = prompt_message(proto::PromptRequest {
            session_id: 1,
            prompt: "must not be copied".into(),
            parts: vec![proto::MessagePart {
                part: Some(proto::message_part::Part::Media(proto::Media {
                    media_id: 7,
                    mime: "image/png".into(),
                })),
            }],
        })
        .unwrap();
        assert_eq!(
            media.as_message().parts,
            vec![MessagePart::Media {
                media_id: 7,
                mime: "image/png".into(),
            }]
        );
    }
}
