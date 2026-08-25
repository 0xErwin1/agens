//! The chat plane, as a surface uses it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tokio_stream::{Stream, StreamExt};
use tonic::transport::Channel;

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

    /// Opens a chat rooted at `checkout`, or continues the stored session
    /// `resume` names.
    ///
    /// Returns the session's durable id, which is what a client keeps in order
    /// to come back to this conversation after detaching.
    pub async fn open(&mut self, checkout: &Path, resume: Option<i64>) -> Result<i64, ClientError> {
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

        Ok(opened.session_id)
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

#[cfg(test)]
mod tests {
    use agens_core::{Message, MessagePart, Role, SessionMessage};

    use super::*;

    fn session_message(role: Role, parts: Vec<MessagePart>) -> SessionMessage {
        SessionMessage::try_from(Message { role, parts }).unwrap()
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
