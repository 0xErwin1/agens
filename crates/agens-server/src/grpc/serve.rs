//! Binding the facade, and serving it until the daemon is asked to stop.
//!
//! One address, and it is the daemon's unix socket. The facade authenticates
//! nobody and pins the user's authority to whoever reaches it, so the reach of
//! the socket is the reach of the API — and the only thing keeping that honest
//! is the mode of the directory the single-instance guard created. A loopback
//! TCP listener would carry the same authority with none of that: every local
//! account can route to it. Remote access is an SSH tunnel, which puts identity
//! where it belongs and leaves nothing listening for anyone else.

use std::io;
use std::os::unix::net::UnixListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agens_core::HeadlessTurnCancellation;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;

use super::proto::feed_server::FeedServer;
use super::proto::team_server::TeamServer;
use super::{CoreHandle, FeedFacade, TeamFacade};
use crate::api::ApiCore;
use crate::blocking::BlockingBoundary;
use crate::fsm::Principal;

/// How often the shutdown signal is looked at. The daemon's cancellation is a
/// flag rather than a channel, so serving polls it the same way the rest of the
/// daemon does.
const SHUTDOWN_POLL: Duration = Duration::from_millis(50);

#[derive(Debug)]
pub enum FacadeError {
    /// Nothing to accept on. Serving no address is never what a caller meant.
    NoListener,
    Unavailable(String),
}

impl std::fmt::Display for FacadeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoListener => formatter.write_str("the facade was given no address to serve on"),
            Self::Unavailable(detail) => formatter.write_str(detail),
        }
    }
}

impl std::error::Error for FacadeError {}

impl FacadeError {
    fn unavailable(action: &str, error: impl std::fmt::Display) -> Self {
        Self::Unavailable(format!("{action}: {error}"))
    }
}

/// Where the facade accepts clients.
///
/// One field, and no way to add a second address. The facade pins the user's
/// authority to every caller it accepts, so an address is only admissible when
/// something outside this crate already decides who may reach it — which is
/// true of the daemon's socket directory and of nothing else.
#[derive(Debug, Default)]
pub struct FacadeBinding {
    unix: Option<UnixListener>,
}

impl FacadeBinding {
    #[must_use]
    pub const fn none() -> Self {
        Self { unix: None }
    }

    /// Accepts on an already bound unix socket, so the daemon keeps owning the
    /// socket file it created under its instance lock.
    #[must_use]
    pub fn on_unix_socket(mut self, listener: UnixListener) -> Self {
        self.unix = Some(listener);
        self
    }
}

/// Serves Team and Feed on the bound address until the daemon is asked to
/// stop.
///
/// The principal is pinned to the user here and read from nothing: this is the
/// clients' facade, and the design gives the user's authority to whoever reaches
/// the socket. Narrowing it is a different facade, not a field on a request.
pub async fn serve_until_shutdown(
    core: Arc<Mutex<ApiCore>>,
    blocking: BlockingBoundary,
    binding: FacadeBinding,
    shutdown: &HeadlessTurnCancellation,
) -> Result<(), FacadeError> {
    let Some(listener) = binding.unix else {
        return Err(FacadeError::NoListener);
    };

    listener
        .set_nonblocking(true)
        .map_err(|error| FacadeError::unavailable("prepare the unix socket", error))?;

    let listener = tokio::net::UnixListener::from_std(listener)
        .map_err(|error| FacadeError::unavailable("adopt the unix socket", error))?;

    let handle = CoreHandle::new(core, blocking, Principal::User);

    serve_on(handle, UnixListenerStream::new(listener), shutdown.clone()).await
}

async fn serve_on<S, C>(
    handle: CoreHandle,
    incoming: S,
    shutdown: HeadlessTurnCancellation,
) -> Result<(), FacadeError>
where
    S: tokio_stream::Stream<Item = io::Result<C>>,
    C: tonic::transport::server::Connected + tokio::io::AsyncRead + tokio::io::AsyncWrite,
    C: Send + Unpin + 'static,
    C::ConnectInfo: Clone + Send + Sync + 'static,
{
    Server::builder()
        .add_service(TeamServer::new(TeamFacade::new(handle.clone())))
        .add_service(FeedServer::new(FeedFacade::new(handle)))
        .serve_with_incoming_shutdown(incoming, park_until_shutdown(shutdown))
        .await
        .map_err(|error| FacadeError::unavailable("serve the facade", error))
}

async fn park_until_shutdown(shutdown: HeadlessTurnCancellation) {
    while !shutdown.is_cancelled() {
        tokio::time::sleep(SHUTDOWN_POLL).await;
    }
}
