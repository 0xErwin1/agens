//! The chat plane, as a surface uses it.

use std::path::Path;

use tokio_stream::{Stream, StreamExt};
use tonic::transport::Channel;

use crate::ClientError;
use crate::decode::{HostedChatEvent, session_event};
use crate::proto;

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
