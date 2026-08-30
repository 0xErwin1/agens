//! Reaching a daemon from outside it.
//!
//! One crate, so that every surface that attaches to a coordinator — the
//! terminal today, whatever else later — speaks to it the same way and none of
//! them has to know that the wire is gRPC over a unix socket.
//!
//! It is a client and nothing else. It opens the connection, calls the methods
//! and turns what comes back into the domain's own types; it decides nothing
//! about what a run or a chat should do, because everything that decides lives
//! behind the socket by design.
//!
//! The chat stream is where that matters most. What a hosted turn produces is
//! [`agens_core::TurnEvent`], and this crate hands it back as exactly that, so a
//! surface renders a turn the daemon ran with the code it already uses to render
//! one it ran itself.

mod chat;
mod decode;
mod feed;
mod team;

use std::path::{Path, PathBuf};

use tonic::transport::{Channel, Endpoint, Uri};

pub use agens_server::grpc::proto;
pub use chat::{ChatClient, OpenChat};
pub use decode::{HostedChatEvent, PermissionDecision, PermissionQuestion};
pub use feed::{FeedClient, JournalEntry};
pub use team::TeamClient;

/// Why a call to the daemon did not answer.
#[derive(Debug)]
pub enum ClientError {
    /// No daemon answered on the socket. Its own variant because it is the one
    /// failure a surface can act on without reading a message: there is nothing
    /// running to talk to, so the answer is to start one.
    NotRunning(String),
    /// The daemon answered, and refused.
    Refused(tonic::Status),
    /// The daemon answered something this client cannot read, which is a
    /// disagreement about the wire rather than about the request.
    Unreadable(String),
    /// The request cannot be represented safely for this daemon.
    InvalidRequest(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotRunning(detail) => write!(formatter, "no daemon is listening: {detail}"),
            Self::Refused(status) => write!(formatter, "the daemon refused: {}", status.message()),
            Self::Unreadable(detail) => {
                write!(
                    formatter,
                    "the daemon answered something unreadable: {detail}"
                )
            }
            Self::InvalidRequest(detail) => formatter.write_str(detail),
        }
    }
}

impl ClientError {
    /// Whether a failed prompt is known not to have crossed the daemon's inbox boundary.
    #[must_use]
    /// Whether the daemon refused because the state the request named is no
    /// longer there — an answer to a question that already resolved, most of
    /// all. The condition failed, not the connection or the wire.
    pub fn refused_precondition(&self) -> bool {
        matches!(
            self,
            Self::Refused(status) if status.code() == tonic::Code::FailedPrecondition
        )
    }

    pub fn definitively_rejected_prompt(&self) -> bool {
        match self {
            Self::InvalidRequest(_) => true,
            Self::Refused(status) => matches!(
                status.code(),
                tonic::Code::InvalidArgument
                    | tonic::Code::NotFound
                    | tonic::Code::FailedPrecondition
            ),
            Self::NotRunning(_) | Self::Unreadable(_) => false,
        }
    }
}

impl std::error::Error for ClientError {}

impl From<tonic::Status> for ClientError {
    fn from(status: tonic::Status) -> Self {
        Self::Refused(status)
    }
}

/// A connection to one daemon.
///
/// Cloning it is cheap and shares the connection, which is what lets the three
/// planes be held separately by whatever needs them without opening a socket
/// apiece.
#[derive(Clone, Debug)]
pub struct Coordinator {
    channel: Channel,
}

impl Coordinator {
    /// Attaches to the daemon accepting on `socket`.
    ///
    /// The address is a unix socket path and cannot be anything else: the facade
    /// authenticates nobody and carries the user's authority to whoever reaches
    /// it, so the reach of the socket is the reach of the API. A client that
    /// could be pointed at a TCP address would be a client that could be
    /// pointed at somebody else's daemon.
    pub async fn attach(socket: &Path) -> Result<Self, ClientError> {
        let socket = socket.to_path_buf();

        // The authority is never used: the connector below hands back a unix
        // stream whatever the URI says, and gRPC still wants a syntactically
        // valid one.
        let channel = Endpoint::try_from("http://localhost")
            .map_err(|error| ClientError::Unreadable(error.to_string()))?
            .connect_with_connector(tower::service_fn(move |_: Uri| {
                connect_unix(socket.clone())
            }))
            .await
            .map_err(|error| ClientError::NotRunning(error.to_string()))?;

        Ok(Self { channel })
    }

    /// The chat plane: the user's own conversation, hosted by the daemon.
    #[must_use]
    pub fn chat(&self) -> ChatClient {
        ChatClient::new(self.channel.clone())
    }

    /// The read plane: the journal, the tree, a run's detail and the inbox.
    #[must_use]
    pub fn feed(&self) -> FeedClient {
        FeedClient::new(self.channel.clone())
    }

    /// The control plane: what a person does to a run.
    #[must_use]
    pub fn team(&self) -> TeamClient {
        TeamClient::new(self.channel.clone())
    }
}

async fn connect_unix(
    socket: PathBuf,
) -> std::io::Result<hyper_util::rt::TokioIo<tokio::net::UnixStream>> {
    let stream = tokio::net::UnixStream::connect(socket).await?;

    Ok(hyper_util::rt::TokioIo::new(stream))
}

#[cfg(test)]
mod tests {
    use super::ClientError;

    #[test]
    fn only_a_failed_precondition_reads_as_a_question_already_resolved() {
        assert!(
            ClientError::Refused(tonic::Status::failed_precondition(
                "this chat is not waiting on that question"
            ))
            .refused_precondition()
        );
        assert!(
            !ClientError::Refused(tonic::Status::unavailable("the daemon went away"))
                .refused_precondition()
        );
        assert!(!ClientError::Unreadable("wire disagreement".to_owned()).refused_precondition());
    }
}
