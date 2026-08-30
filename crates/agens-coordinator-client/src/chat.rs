//! The chat plane, as a surface uses it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tokio_stream::{Stream, StreamExt};
use tonic::transport::Channel;

use agens_core::ask_user::{AskUserReply, AskUserUnavailable};
use agens_core::hosted::{
    CatalogEntry, CatalogKind, CatalogResult, CatalogSnapshot, FileError, HostedChildTurn,
    HostedControlCommand, HostedControlKind, HostedControlResult, HostedMcpAction, HostedMcpResult,
    HostedMcpServer, HostedMcpState, HostedTaskEvent, HostedTaskRecord, HostedTaskReplay,
    HostedTaskSnapshot, HostedTaskState, TaskControlError, WorkspaceFile, WorkspaceFileContent,
    WorkspaceFileKind,
};
use agens_core::{Message, MessagePart, Role, SessionMessage};

use crate::ClientError;
use crate::decode::{HostedChatEvent, PermissionDecision, message, session_event};
use crate::proto;

/// One chat the daemon is hosting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenChat {
    pub session_id: i64,
    pub checkout: PathBuf,
    /// Whether a turn is running right now, so a client that comes back
    /// mid-answer can say so rather than looking idle.
    pub answering: bool,
}

/// A chat the daemon just opened for this client, and how it described it.
///
/// The description is what dresses the surface before the first prompt: the
/// model the session actually speaks to, its reasoning effort, and the window
/// its context gauge measures against. Absent fields mean the daemon predates
/// the description, never that the session has no configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenedChat {
    pub session_id: i64,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub context_window: Option<u64>,
}

/// The daemon's account of what it is, as the attach handshake carries it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DaemonBuild {
    /// The client/daemon contract revision. Equality is compatibility.
    pub wire_revision: u64,
    /// The build the daemon was compiled from, compared for equality only.
    pub build: String,
    /// Chats whose turn is running right now, machine-wide.
    pub answering_chats: i64,
}

/// A handle on one daemon's chat plane.
#[derive(Clone, Debug)]
pub struct ChatClient {
    inner: proto::chat_client::ChatClient<Channel>,
    prompt_parts: Arc<Mutex<BTreeMap<i64, bool>>>,
}

impl ChatClient {
    pub(crate) fn new(channel: Channel) -> Self {
        Self {
            inner: proto::chat_client::ChatClient::new(channel),
            prompt_parts: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Asks the daemon what it is, before this client commits to it.
    ///
    /// `None` is a daemon too old to say: it predates the handshake, so the
    /// method comes back `Unimplemented` rather than answered.
    pub async fn build_info(&mut self) -> Result<Option<DaemonBuild>, ClientError> {
        daemon_build(self.inner.build_info(proto::BuildInfoRequest {}).await)
    }

    pub async fn catalog(
        &mut self,
        kind: CatalogKind,
        known_revision: Option<&str>,
    ) -> Result<CatalogResult, ClientError> {
        let kind = match kind {
            CatalogKind::Command => proto::HostedCatalogKind::Command,
            CatalogKind::Skill => proto::HostedCatalogKind::Skill,
        };
        let result = self
            .inner
            .catalog(proto::HostedCatalogRequest {
                kind: kind as i32,
                known_revision: known_revision.map(str::to_owned),
            })
            .await?
            .into_inner();
        decode_catalog(result)
    }

    pub async fn list_workspace_files(
        &mut self,
        checkout: &Path,
        selector: &Path,
    ) -> Result<Result<Vec<WorkspaceFile>, FileError>, ClientError> {
        let result = self
            .inner
            .list_workspace_files(proto::WorkspaceFilesRequest {
                checkout: checkout.display().to_string(),
                selector: selector.display().to_string(),
            })
            .await?
            .into_inner();
        decode_file_list(result)
    }

    pub async fn read_workspace_file(
        &mut self,
        checkout: &Path,
        selector: &Path,
    ) -> Result<Result<WorkspaceFileContent, FileError>, ClientError> {
        let result = self
            .inner
            .read_workspace_file(proto::WorkspaceFileRequest {
                checkout: checkout.display().to_string(),
                selector: selector.display().to_string(),
            })
            .await?
            .into_inner();
        decode_file(result)
    }

    pub async fn mcp_status(&mut self) -> Result<HostedMcpResult, ClientError> {
        let result = self
            .inner
            .mcp_status(proto::HostedMcpStatusRequest {})
            .await?
            .into_inner();
        decode_mcp_result(result)
    }

    pub async fn mcp_control(
        &mut self,
        server: &str,
        action: HostedMcpAction,
    ) -> Result<HostedMcpResult, ClientError> {
        let action = match action {
            HostedMcpAction::Connect => proto::HostedMcpAction::Connect,
            HostedMcpAction::Disconnect => proto::HostedMcpAction::Disconnect,
            HostedMcpAction::Reconnect => proto::HostedMcpAction::Reconnect,
        };
        let result = self
            .inner
            .mcp_control(proto::HostedMcpControlRequest {
                server: server.to_owned(),
                action: action as i32,
            })
            .await?
            .into_inner();
        decode_mcp_result(result)
    }

    pub async fn task_snapshot(
        &mut self,
        session_id: i64,
    ) -> Result<Result<HostedTaskReplay, TaskControlError>, ClientError> {
        let result = self
            .inner
            .task_snapshot(proto::HostedTaskSnapshotRequest { session_id })
            .await?
            .into_inner();
        decode_task_replay(result)
    }

    pub async fn task_replay(
        &mut self,
        session_id: i64,
        after_cursor: u64,
    ) -> Result<Result<HostedTaskReplay, TaskControlError>, ClientError> {
        let result = self
            .inner
            .task_replay(proto::HostedTaskReplayRequest {
                session_id,
                after_cursor,
            })
            .await?
            .into_inner();
        decode_task_replay(result)
    }

    pub async fn task_control(
        &mut self,
        command: &HostedControlCommand,
    ) -> Result<Result<HostedControlResult, TaskControlError>, ClientError> {
        let (kind, message) = match command.kind() {
            HostedControlKind::Background => {
                (proto::HostedTaskControlKind::Background, String::new())
            }
            HostedControlKind::Cancel => (proto::HostedTaskControlKind::Cancel, String::new()),
            HostedControlKind::CancelAll => {
                (proto::HostedTaskControlKind::CancelAll, String::new())
            }
            HostedControlKind::Message(message) => {
                (proto::HostedTaskControlKind::Message, message.clone())
            }
        };
        let result = self
            .inner
            .task_control(proto::HostedTaskControlRequest {
                session_id: command.session_id(),
                task_id: command.task_id().unwrap_or_default().to_owned(),
                command_id: command.command_id().to_owned(),
                kind: kind as i32,
                message,
            })
            .await?
            .into_inner();
        decode_task_control(result)
    }

    /// Opens a chat rooted at `checkout`, or continues the stored session
    /// `resume` names.
    ///
    /// Returns the session's durable id — what a client keeps in order to come
    /// back to this conversation after detaching — together with the
    /// configuration the daemon described the chat as holding. A daemon that
    /// predates the description leaves every described field absent.
    pub async fn open(
        &mut self,
        checkout: &Path,
        resume: Option<i64>,
    ) -> Result<OpenedChat, ClientError> {
        let opened = self
            .inner
            .open(proto::OpenChatRequest {
                checkout: checkout.display().to_string(),
                resume,
            })
            .await?
            .into_inner();

        self.prompt_parts
            .lock()
            .map_err(|_| ClientError::Unreadable("chat capabilities are unavailable".into()))?
            .insert(opened.session_id, opened.supports_prompt_parts);

        Ok(OpenedChat {
            session_id: opened.session_id,
            provider: opened.provider.filter(|provider| !provider.is_empty()),
            model: opened.model.filter(|model| !model.is_empty()),
            reasoning_effort: opened.reasoning_effort.filter(|effort| !effort.is_empty()),
            context_window: opened.context_window.filter(|window| *window > 0),
        })
    }

    /// What is already open for `checkout`, newest first.
    ///
    /// This is how a terminal that detached comes back to its conversation
    /// rather than starting a second one beside it, without anybody having
    /// written a session id down.
    pub async fn open_against(&mut self, checkout: &Path) -> Result<Vec<OpenChat>, ClientError> {
        let chats = self
            .inner
            .list(proto::ListChatsRequest {
                checkout: checkout.display().to_string(),
            })
            .await?
            .into_inner();

        Ok(chats
            .chats
            .into_iter()
            .map(|chat| OpenChat {
                session_id: chat.session_id,
                checkout: PathBuf::from(chat.checkout),
                answering: chat.answering,
            })
            .collect())
    }

    /// Sends a prompt and returns as soon as the daemon has somewhere to run it.
    ///
    /// What the turn does arrives on [`Self::subscribe`], never here. A client
    /// that waited for the turn on this call would be a client whose going away
    /// ends the turn, which is the whole thing a hosted chat exists to avoid.
    pub async fn prompt(&mut self, session_id: i64, prompt: &str) -> Result<(), ClientError> {
        let message = SessionMessage::try_from(Message {
            role: Role::User,
            parts: vec![MessagePart::Text(prompt.to_owned())],
        })
        .map_err(|_| ClientError::InvalidRequest("the prompt is empty or invalid".into()))?;
        self.prompt_message(session_id, &message).await
    }

    /// Sends canonical ordered user content. Media is refused locally unless the
    /// opened daemon advertised structured prompt parts.
    pub async fn prompt_message(
        &mut self,
        session_id: i64,
        message: &SessionMessage,
    ) -> Result<(), ClientError> {
        let supports_parts = self
            .prompt_parts
            .lock()
            .map_err(|_| ClientError::Unreadable("chat capabilities are unavailable".into()))?
            .get(&session_id)
            .copied()
            .unwrap_or(false);
        let request = prompt_request(session_id, message, supports_parts)?;
        self.inner.prompt(request).await?;
        Ok(())
    }

    /// Executes a slash command against the daemon-owned chat state.
    pub async fn command(&mut self, session_id: i64, command: &str) -> Result<String, ClientError> {
        let result = self
            .inner
            .command(proto::ChatCommandRequest {
                session_id,
                command: command.to_owned(),
            })
            .await?
            .into_inner();

        Ok(result.message)
    }

    /// Answers a question the chat's turn is stopped on.
    ///
    /// A question that already resolved is refused rather than ignored: an
    /// answer to something the person is no longer looking at should not be
    /// applied to whatever they are.
    pub async fn answer_permission(
        &mut self,
        session_id: i64,
        prompt_id: u64,
        decision: PermissionDecision,
    ) -> Result<(), ClientError> {
        self.answer_question(session_id, prompt_id, decision.as_str())
            .await
    }

    /// Answers any bounded question through the chat's existing decision wire.
    pub async fn answer_question(
        &mut self,
        session_id: i64,
        prompt_id: u64,
        answer: &str,
    ) -> Result<(), ClientError> {
        self.inner
            .answer_permission(proto::AnswerPermissionRequest {
                session_id,
                prompt_id,
                answer: answer.to_owned(),
            })
            .await?;

        Ok(())
    }

    pub async fn answer_ask_user(
        &mut self,
        session_id: i64,
        prompt_id: u64,
        answer: AskUserReply,
    ) -> Result<(), ClientError> {
        self.inner
            .answer_ask_user(proto::AnswerAskUserRequest {
                session_id,
                prompt_id,
                reply: Some(ask_user_reply(answer)),
            })
            .await?;

        Ok(())
    }

    /// Ends the turn the chat is running, leaving the session open.
    pub async fn cancel(&mut self, session_id: i64) -> Result<(), ClientError> {
        self.inner.cancel(proto::ChatRef { session_id }).await?;

        Ok(())
    }

    /// Ends the session, letting the turn it may be running finish first.
    pub async fn close(&mut self, session_id: i64) -> Result<(), ClientError> {
        self.inner.close(proto::ChatRef { session_id }).await?;

        Ok(())
    }

    /// What the chat has said so far.
    ///
    /// A snapshot: this is what a terminal coming back needs in order to draw
    /// the conversation it left, and what happens next arrives on
    /// [`Self::subscribe`].
    pub async fn history(&mut self, session_id: i64) -> Result<Vec<Message>, ClientError> {
        let history = self
            .inner
            .history(proto::ChatRef { session_id })
            .await?
            .into_inner();

        history.messages.into_iter().map(message).collect()
    }

    /// Follows what the chat is doing, live from now.
    ///
    /// Not a replay: a client coming back to a chat it detached from reads the
    /// session's stored history for what it missed, and this for what happens
    /// next.
    pub async fn subscribe(
        &mut self,
        session_id: i64,
    ) -> Result<impl Stream<Item = Result<HostedChatEvent, ClientError>> + use<>, ClientError> {
        let events = self
            .inner
            .subscribe(proto::ChatRef { session_id })
            .await?
            .into_inner();

        Ok(events.map(|event| match event {
            Ok(event) => session_event(event),
            Err(status) => Err(ClientError::Refused(status)),
        }))
    }
}

fn ask_user_reply(reply: AskUserReply) -> proto::AskUserReply {
    use proto::ask_user_reply::Reply;

    let reply = match reply {
        AskUserReply::Answered(answers) => Reply::Answered(proto::AskUserAnswered {
            answers: answers
                .into_iter()
                .map(|answer| proto::AskUserAnswer {
                    question_id: answer.question_id,
                    selected: answer.selected,
                    other: answer.other,
                    note: answer.note,
                })
                .collect(),
        }),
        AskUserReply::Discuss { question_id, note } => {
            Reply::Discuss(proto::AskUserDiscuss { question_id, note })
        }
        AskUserReply::Cancelled => Reply::Cancelled(proto::AskUserCancelled {}),
        AskUserReply::Unavailable(reason) => Reply::Unavailable(proto::AskUserUnavailable {
            reason: match reason {
                AskUserUnavailable::NoInteractiveSurface => "no_interactive_surface",
                AskUserUnavailable::SurfaceClosed => "surface_closed",
            }
            .to_owned(),
        }),
    };

    proto::AskUserReply { reply: Some(reply) }
}

fn decode_catalog(result: proto::HostedCatalogResult) -> Result<CatalogResult, ClientError> {
    use proto::hosted_catalog_result::Result;
    match result.result {
        Some(Result::Current(snapshot)) => Ok(CatalogResult::Current(CatalogSnapshot::new(
            snapshot.revision,
            snapshot
                .entries
                .into_iter()
                .map(|entry| CatalogEntry::new(entry.name, entry.description, entry.built_in))
                .collect(),
        ))),
        Some(Result::Stale(stale)) => Ok(CatalogResult::Stale {
            current_revision: stale.current_revision,
        }),
        Some(Result::Unsupported(_)) => Ok(CatalogResult::Unsupported),
        None => Err(ClientError::Unreadable(
            "daemon returned no catalog outcome".into(),
        )),
    }
}

fn decode_file_list(
    result: proto::HostedWorkspaceFilesResult,
) -> Result<Result<Vec<WorkspaceFile>, FileError>, ClientError> {
    use proto::hosted_workspace_files_result::Result;
    match result.result {
        Some(Result::Files(list)) => list
            .files
            .into_iter()
            .map(|file| {
                let kind = decode_file_kind(file.kind)?;
                Ok(WorkspaceFile::new(
                    PathBuf::from(file.path),
                    file.byte_len,
                    kind,
                ))
            })
            .collect::<std::result::Result<Vec<_>, ClientError>>()
            .map(Ok),
        Some(Result::Error(error)) => decode_file_error(error).map(Err),
        None => Err(ClientError::Unreadable(
            "daemon returned no file-list outcome".into(),
        )),
    }
}

fn decode_file(
    result: proto::HostedWorkspaceFileResult,
) -> Result<Result<WorkspaceFileContent, FileError>, ClientError> {
    use proto::hosted_workspace_file_result::Result;
    match result.result {
        Some(Result::Text(text)) => Ok(Ok(WorkspaceFileContent::Text {
            path: PathBuf::from(text.path),
            text: text.text,
        })),
        Some(Result::Media(media)) => Ok(Ok(WorkspaceFileContent::Media {
            path: PathBuf::from(media.path),
            mime: media.mime,
            bytes: media.bytes,
            media_id: media.media_id,
            kind: WorkspaceFileKind::Media,
        })),
        Some(Result::Error(error)) => decode_file_error(error).map(Err),
        None => Err(ClientError::Unreadable(
            "daemon returned no file outcome".into(),
        )),
    }
}

fn decode_file_kind(kind: i32) -> Result<WorkspaceFileKind, ClientError> {
    match proto::HostedFileKind::try_from(kind) {
        Ok(proto::HostedFileKind::Text) => Ok(WorkspaceFileKind::Text),
        Ok(proto::HostedFileKind::Media) => Ok(WorkspaceFileKind::Media),
        _ => Err(ClientError::Unreadable(
            "daemon returned an unknown file kind".into(),
        )),
    }
}

fn decode_file_error(error: i32) -> Result<FileError, ClientError> {
    match proto::HostedFileError::try_from(error) {
        Ok(proto::HostedFileError::InvalidSelector) => Ok(FileError::InvalidSelector),
        Ok(proto::HostedFileError::OutsideRoot) => Ok(FileError::OutsideRoot),
        Ok(proto::HostedFileError::Ignored) => Ok(FileError::Ignored),
        Ok(proto::HostedFileError::Missing) => Ok(FileError::Missing),
        Ok(proto::HostedFileError::Unsupported) => Ok(FileError::Unsupported),
        Ok(proto::HostedFileError::Oversized) => Ok(FileError::Oversized),
        Ok(proto::HostedFileError::EntryLimit) => Ok(FileError::EntryLimit),
        Ok(proto::HostedFileError::Unreadable) => Ok(FileError::Unreadable),
        _ => Err(ClientError::Unreadable(
            "daemon returned an unknown file error".into(),
        )),
    }
}

fn decode_mcp_result(result: proto::HostedMcpResult) -> Result<HostedMcpResult, ClientError> {
    let servers = result
        .servers
        .into_iter()
        .map(decode_mcp_server)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(HostedMcpResult::new(servers, result.error))
}

fn decode_mcp_server(server: proto::HostedMcpServer) -> Result<HostedMcpServer, ClientError> {
    let state = match proto::HostedMcpState::try_from(server.state) {
        Ok(proto::HostedMcpState::Disabled) => HostedMcpState::Disabled,
        Ok(proto::HostedMcpState::Idle) => HostedMcpState::Idle,
        Ok(proto::HostedMcpState::Connecting) => HostedMcpState::Connecting,
        Ok(proto::HostedMcpState::Ready) => HostedMcpState::Ready,
        Ok(proto::HostedMcpState::Degraded) => HostedMcpState::Degraded,
        Ok(proto::HostedMcpState::Failed) => HostedMcpState::Failed,
        Ok(proto::HostedMcpState::Closed) => HostedMcpState::Closed,
        _ => {
            return Err(ClientError::Unreadable(
                "daemon returned an unknown MCP state".into(),
            ));
        }
    };
    if server.name.is_empty() {
        return Err(ClientError::Unreadable(
            "daemon returned an unnamed MCP server".into(),
        ));
    }
    Ok(HostedMcpServer::new(
        server.name,
        state,
        server.generation,
        server.error,
    ))
}

fn decode_task_replay(
    result: proto::HostedTaskReplayResult,
) -> Result<Result<HostedTaskReplay, TaskControlError>, ClientError> {
    use proto::hosted_task_replay_result::Result as Wire;
    match result.result {
        Some(Wire::Events(events)) => {
            decode_task_events(events.events).map(|events| Ok(HostedTaskReplay::Events(events)))
        }
        Some(Wire::SnapshotTail(tail)) => {
            let snapshot = tail.snapshot.ok_or_else(|| {
                ClientError::Unreadable("daemon returned no task snapshot".into())
            })?;
            let tasks = snapshot
                .tasks
                .into_iter()
                .map(|task| {
                    decode_task_state(task.state)
                        .map(|state| HostedTaskRecord::new(task.task_id, state))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let child_turns = snapshot
                .child_turns
                .into_iter()
                .map(|turn| HostedChildTurn::new(turn.task_id, turn.sequence, turn.payload))
                .collect();
            let snapshot =
                HostedTaskSnapshot::new(snapshot.cursor, tasks).with_child_turns(child_turns);
            let events = decode_task_events(tail.events)?;
            Ok(Ok(HostedTaskReplay::SnapshotTail { snapshot, events }))
        }
        Some(Wire::Gap(gap)) => Ok(Ok(HostedTaskReplay::Gap {
            oldest_cursor: gap.oldest_cursor,
        })),
        Some(Wire::Error(error)) => decode_task_error(error).map(Err),
        None => Err(ClientError::Unreadable(
            "daemon returned no task replay outcome".into(),
        )),
    }
}

fn decode_task_events(
    events: Vec<proto::HostedTaskEvent>,
) -> Result<Vec<HostedTaskEvent>, ClientError> {
    events
        .into_iter()
        .map(|event| {
            decode_task_state(event.state).map(|state| {
                HostedTaskEvent::new(event.cursor, event.task_id, state, event.payload)
            })
        })
        .collect()
}

fn decode_task_control(
    result: proto::HostedTaskControlResult,
) -> Result<Result<HostedControlResult, TaskControlError>, ClientError> {
    use proto::hosted_task_control_result::Result as Wire;
    match result.result {
        Some(Wire::Applied(applied)) => decode_task_state(applied.state)
            .map(|state| Ok(HostedControlResult::new(state, applied.replayed))),
        Some(Wire::Error(error)) => decode_task_error(error).map(Err),
        None => Err(ClientError::Unreadable(
            "daemon returned no task control outcome".into(),
        )),
    }
}

fn decode_task_state(state: i32) -> Result<HostedTaskState, ClientError> {
    match proto::HostedTaskState::try_from(state) {
        Ok(proto::HostedTaskState::Running) => Ok(HostedTaskState::Running),
        Ok(proto::HostedTaskState::Background) => Ok(HostedTaskState::Background),
        Ok(proto::HostedTaskState::Completed) => Ok(HostedTaskState::Completed),
        Ok(proto::HostedTaskState::Cancelled) => Ok(HostedTaskState::Cancelled),
        Ok(proto::HostedTaskState::Failed) => Ok(HostedTaskState::Failed),
        _ => Err(ClientError::Unreadable(
            "daemon returned an unknown task state".into(),
        )),
    }
}

fn decode_task_error(error: i32) -> Result<TaskControlError, ClientError> {
    match proto::HostedTaskError::try_from(error) {
        Ok(proto::HostedTaskError::WrongSession) => Ok(TaskControlError::WrongSession),
        Ok(proto::HostedTaskError::UnknownTask) => Ok(TaskControlError::UnknownTask),
        Ok(proto::HostedTaskError::InvalidTransition) => Ok(TaskControlError::InvalidTransition),
        Ok(proto::HostedTaskError::CommandConflict) => Ok(TaskControlError::CommandConflict),
        Ok(proto::HostedTaskError::ControlCapacity) => Ok(TaskControlError::ControlCapacity),
        Ok(proto::HostedTaskError::InvalidRequest) => Ok(TaskControlError::InvalidRequest),
        Ok(proto::HostedTaskError::Storage) => Ok(TaskControlError::Storage),
        _ => Err(ClientError::Unreadable(
            "daemon returned an unknown task error".into(),
        )),
    }
}

fn prompt_request(
    session_id: i64,
    message: &SessionMessage,
    supports_parts: bool,
) -> Result<proto::PromptRequest, ClientError> {
    if message.as_message().role != Role::User {
        return Err(ClientError::InvalidRequest(
            "attached chat accepts only user messages".into(),
        ));
    }

    let has_media = message
        .as_message()
        .parts
        .iter()
        .any(|part| matches!(part, MessagePart::Media { .. }));
    if has_media && !supports_parts {
        return Err(ClientError::InvalidRequest(
            "the attached daemon does not support media prompts".into(),
        ));
    }

    let prompt = message
        .as_message()
        .parts
        .iter()
        .filter_map(|part| match part {
            MessagePart::Text(text) => Some(text.as_str()),
            MessagePart::Media { .. } => None,
            _ => None,
        })
        .collect::<String>();
    let parts = if supports_parts {
        message
            .as_message()
            .parts
            .iter()
            .map(|part| {
                let part = match part {
                    MessagePart::Text(text) => proto::message_part::Part::Text(text.clone()),
                    MessagePart::Media { media_id, mime } => {
                        proto::message_part::Part::Media(proto::Media {
                            media_id: *media_id,
                            mime: mime.clone(),
                        })
                    }
                    _ => {
                        return Err(ClientError::InvalidRequest(
                            "attached chat accepts only text and media parts".into(),
                        ));
                    }
                };
                Ok(proto::MessagePart { part: Some(part) })
            })
            .collect::<Result<Vec<_>, _>>()?
    } else {
        Vec::new()
    };

    Ok(proto::PromptRequest {
        session_id,
        prompt,
        parts,
    })
}

/// Reads a handshake answer, telling an older daemon apart from a failure.
fn daemon_build(
    result: Result<tonic::Response<proto::DaemonBuild>, tonic::Status>,
) -> Result<Option<DaemonBuild>, ClientError> {
    match result {
        Ok(response) => {
            let answered = response.into_inner();

            Ok(Some(DaemonBuild {
                wire_revision: answered.wire_revision,
                build: answered.build,
                answering_chats: answered.answering_chats,
            }))
        }
        Err(status) if status.code() == tonic::Code::Unimplemented => Ok(None),
        Err(status) => Err(status.into()),
    }
}

#[cfg(test)]
mod tests {
    use agens_core::{Message, MessagePart, Role, SessionMessage};

    use super::*;

    fn session_message(role: Role, parts: Vec<MessagePart>) -> SessionMessage {
        SessionMessage::try_from(Message { role, parts }).unwrap()
    }

    #[test]
    fn a_served_handshake_reads_as_the_daemon_describing_itself() {
        let answered = daemon_build(Ok(tonic::Response::new(proto::DaemonBuild {
            wire_revision: 4,
            build: "0.1.0+abc123".to_owned(),
            answering_chats: 2,
        })))
        .expect("the handshake is readable")
        .expect("the daemon described itself");

        assert_eq!(answered.wire_revision, 4);
        assert_eq!(answered.build, "0.1.0+abc123");
        assert_eq!(answered.answering_chats, 2);
    }

    /// A daemon that predates the handshake refuses the method, not the
    /// client: `Unimplemented` means an older daemon, never an error.
    #[test]
    fn a_daemon_without_the_handshake_reads_as_older_not_as_failed() {
        let absent = daemon_build(Err(tonic::Status::unimplemented("unknown method")))
            .expect("an older daemon is an answer, not a failure");

        assert_eq!(absent, None);
    }

    #[test]
    fn any_other_refusal_of_the_handshake_stays_an_error() {
        let refused = daemon_build(Err(tonic::Status::internal("broken")));

        assert!(refused.is_err());
    }

    #[test]
    fn structured_prompt_encoding_preserves_order_and_projects_text_only() {
        let message = session_message(
            Role::User,
            vec![
                MessagePart::Media {
                    media_id: 7,
                    mime: "image/png".into(),
                },
                MessagePart::Text("between".into()),
                MessagePart::Media {
                    media_id: 9,
                    mime: "application/pdf".into(),
                },
            ],
        );

        let request = prompt_request(3, &message, true).unwrap();
        assert_eq!(request.prompt, "between");
        assert!(matches!(
            request.parts[0].part,
            Some(proto::message_part::Part::Media(ref media)) if media.media_id == 7
        ));
        assert!(matches!(
            request.parts[1].part,
            Some(proto::message_part::Part::Text(ref text)) if text == "between"
        ));
        assert!(matches!(
            request.parts[2].part,
            Some(proto::message_part::Part::Media(ref media)) if media.media_id == 9
        ));
    }

    #[test]
    fn media_fails_locally_without_prompt_parts_capability() {
        let message = session_message(
            Role::User,
            vec![MessagePart::Media {
                media_id: 7,
                mime: "image/png".into(),
            }],
        );

        assert!(matches!(
            prompt_request(3, &message, false),
            Err(ClientError::InvalidRequest(_))
        ));
    }

    #[test]
    fn text_for_a_capability_absent_daemon_uses_the_legacy_request_shape() {
        let message = session_message(Role::User, vec![MessagePart::Text("legacy text".into())]);

        let request = prompt_request(3, &message, false).unwrap();
        assert_eq!(request.prompt, "legacy text");
        assert!(request.parts.is_empty());
    }

    #[test]
    fn non_user_messages_are_rejected_before_rpc() {
        let reasoning = session_message(Role::Assistant, vec![MessagePart::Reasoning("no".into())]);
        let tool_result = session_message(
            Role::Tool,
            vec![MessagePart::ToolResult {
                tool_call_id: "call".into(),
                content: "no".into(),
                is_error: false,
            }],
        );

        assert!(prompt_request(3, &reasoning, true).is_err());
        assert!(prompt_request(3, &tool_result, true).is_err());
    }

    #[test]
    fn hosted_decoders_reject_missing_and_unknown_typed_results() {
        assert!(decode_task_replay(proto::HostedTaskReplayResult { result: None }).is_err());
        assert!(decode_task_control(proto::HostedTaskControlResult { result: None }).is_err());
        assert!(
            decode_mcp_result(proto::HostedMcpResult {
                servers: Vec::new(),
                error: None
            })
            .is_ok()
        );
        assert!(
            decode_mcp_server(proto::HostedMcpServer {
                name: "files".into(),
                state: 999,
                generation: 1,
                error: None,
            })
            .is_err()
        );
    }

    #[test]
    fn prompt_error_classification_restores_only_proven_pre_admission_refusals() {
        for code in [
            tonic::Code::InvalidArgument,
            tonic::Code::NotFound,
            tonic::Code::FailedPrecondition,
        ] {
            assert!(
                ClientError::Refused(tonic::Status::new(code, "rejected"))
                    .definitively_rejected_prompt()
            );
        }
        for code in [
            tonic::Code::Unavailable,
            tonic::Code::Unknown,
            tonic::Code::DeadlineExceeded,
            tonic::Code::Cancelled,
            tonic::Code::Internal,
        ] {
            assert!(
                !ClientError::Refused(tonic::Status::new(code, "uncertain"))
                    .definitively_rejected_prompt(),
                "{code:?} is ambiguous after RPC start"
            );
        }
        assert!(
            ClientError::InvalidRequest("local capability check".into())
                .definitively_rejected_prompt()
        );
    }
}
