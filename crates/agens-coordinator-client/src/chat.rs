//! The chat plane, as a surface uses it.

use std::path::{Path, PathBuf};

use tokio_stream::{Stream, StreamExt};
use tonic::transport::Channel;

use agens_core::Message;

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
}

impl ChatClient {
    pub(crate) fn new(channel: Channel) -> Self {
        Self {
            inner: proto::chat_client::ChatClient::new(channel),
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
        self.inner
            .prompt(proto::PromptRequest {
                session_id,
                prompt: prompt.to_owned(),
            })
            .await?;

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
        self.inner
            .answer_permission(proto::AnswerPermissionRequest {
                session_id,
                prompt_id,
                answer: decision.as_str().to_owned(),
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
