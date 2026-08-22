//! Binding the facade, and serving it until the daemon is asked to stop.
//!
//! Two addresses, both local. The unix socket lives in the daemon's own data
//! directory, whose mode the single-instance guard already restricts; the TCP
//! listener is loopback and refused if it is not, because the facade carries no
//! authentication and the reach of the socket is the reach of the API. Remote
//! access is an SSH tunnel, which puts identity where it belongs and leaves
//! nothing listening on the world.

use std::io;
use std::net::{SocketAddr, TcpListener};
use std::os::unix::net::UnixListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agens_core::HeadlessTurnCancellation;
use tokio_stream::wrappers::{TcpListenerStream, UnixListenerStream};
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
    /// A TCP listener that is not on loopback. The facade authenticates
    /// nobody, so binding it anywhere reachable hands the control plane to
    /// whoever can route to it.
    NotLoopback(SocketAddr),
    Unavailable(String),
}

impl std::fmt::Display for FacadeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoListener => formatter.write_str("the facade was given no address to serve on"),
            Self::NotLoopback(address) => write!(
                formatter,
                "{address} is not loopback, and the facade authenticates nobody"
            ),
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
#[derive(Debug, Default)]
pub struct FacadeBinding {
    unix: Option<UnixListener>,
    localhost: Option<TcpListener>,
}

impl FacadeBinding {
    #[must_use]
    pub const fn none() -> Self {
        Self {
            unix: None,
            localhost: None,
        }
    }

    /// Accepts on an already bound unix socket, so the daemon keeps owning the
    /// socket file it created under its instance lock.
    #[must_use]
    pub fn on_unix_socket(mut self, listener: UnixListener) -> Self {
        self.unix = Some(listener);
        self
    }

    /// Accepts on an already bound TCP listener, which has to be on loopback.
    pub fn on_localhost(mut self, listener: TcpListener) -> Result<Self, FacadeError> {
        let address = listener
            .local_addr()
            .map_err(|error| FacadeError::unavailable("read the listener's address", error))?;

        if !address.ip().is_loopback() {
            return Err(FacadeError::NotLoopback(address));
        }

        self.localhost = Some(listener);
        Ok(self)
    }

    /// Binds loopback on the given port. Port zero asks the operating system for
    /// one, which the caller reads back with [`FacadeBinding::localhost_address`].
    pub fn bind_localhost(self, port: u16) -> Result<Self, FacadeError> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
            .map_err(|error| FacadeError::unavailable("bind loopback", error))?;

        self.on_localhost(listener)
    }

    /// The loopback address actually bound, once one is.
    pub fn localhost_address(&self) -> Option<SocketAddr> {
        self.localhost
            .as_ref()
            .and_then(|listener| listener.local_addr().ok())
    }

    fn is_empty(&self) -> bool {
        self.unix.is_none() && self.localhost.is_none()
    }
}

/// Serves Team and Feed on every bound address until the daemon is asked to
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
    if binding.is_empty() {
        return Err(FacadeError::NoListener);
    }

    let handle = CoreHandle::new(core, blocking, Principal::User);
    let mut serving = Vec::new();

    if let Some(listener) = binding.unix {
        listener
            .set_nonblocking(true)
            .map_err(|error| FacadeError::unavailable("prepare the unix socket", error))?;

        let listener = tokio::net::UnixListener::from_std(listener)
            .map_err(|error| FacadeError::unavailable("adopt the unix socket", error))?;

        serving.push(tokio::spawn(serve_on(
            handle.clone(),
            UnixListenerStream::new(listener),
            shutdown.clone(),
        )));
    }

    if let Some(listener) = binding.localhost {
        listener
            .set_nonblocking(true)
            .map_err(|error| FacadeError::unavailable("prepare the loopback socket", error))?;

        let listener = tokio::net::TcpListener::from_std(listener)
            .map_err(|error| FacadeError::unavailable("adopt the loopback socket", error))?;

        serving.push(tokio::spawn(serve_on(
            handle,
            TcpListenerStream::new(listener),
            shutdown.clone(),
        )));
    }

    let mut failure = None;

    for task in serving {
        match task.await {
            Ok(Ok(())) => {}
            // One address failing does not release the others: they are still
            // joined, so the daemon never leaves a listener accepting behind a
            // shutdown that already reported.
            Ok(Err(error)) => failure = failure.or(Some(error)),
            Err(_) => {
                failure = failure.or_else(|| {
                    Some(FacadeError::Unavailable(
                        "a facade listener stopped without reporting".to_owned(),
                    ))
                });
            }
        }
    }

    failure.map_or(Ok(()), Err)
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
