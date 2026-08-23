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
use std::sync::Arc;
use std::sync::mpsc::RecvTimeoutError;

use tokio_stream::{Stream, wrappers::ReceiverStream};
use tonic::{Request, Response, Status};

use super::proto::chat_server::Chat;
use super::subscriptions::{
    FORWARD_PATIENCE, LIVE_SUBSCRIPTIONS, SUBSCRIPTION_BUFFER, SubscriptionSlots, forward,
};
use super::{proto, turn};
use crate::blocking::{BlockingBoundary, BlockingError};
use crate::chat::{ChatError, ChatSessionRequest, ChatSessions};
use crate::sessions::SessionId;

pub struct ChatFacade {
    chats: Arc<ChatSessions>,
    blocking: BlockingBoundary,
    slots: Arc<SubscriptionSlots>,
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
        }
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
            session_id: session.value(),
        }))
    }

    async fn prompt(
        &self,
        request: Request<proto::PromptRequest>,
    ) -> Result<Response<proto::ChatAck>, Status> {
        let request = request.into_inner();
        let session = SessionId::new(request.session_id);
        let prompt = request.prompt;

        self.off_runtime(move |chats| chats.prompt(session, prompt))
            .await?;

        Ok(Response::new(proto::ChatAck {}))
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

    /// What is already open for a checkout, newest first.
    ///
    /// The checkout is required for the reason every listing on this facade
    /// requires one: a daemon serves N projects, and a terminal offered another
    /// project's conversation is a terminal that can attach to the wrong one.
    async fn list(
        &self,
        request: Request<proto::ListChatsRequest>,
    ) -> Result<Response<proto::OpenChats>, Status> {
        let checkout = checkout(request.into_inner().checkout)?;

        let chats = self
            .off_runtime(move |chats| chats.open_against(&checkout))
            .await?;

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
        ChatError::Unavailable(detail) => Status::unavailable(detail),
    }
}

fn unavailable(error: BlockingError) -> Status {
    match error {
        BlockingError::Panicked => Status::internal("the chat plane failed to answer"),
        BlockingError::ShuttingDown => Status::unavailable("the daemon is shutting down"),
    }
}
