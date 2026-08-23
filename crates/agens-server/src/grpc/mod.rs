//! The clients' gRPC facade over the service core.
//!
//! It is a transport and nothing else. It holds a [`Principal`] and an
//! [`ApiCore`], and every method it serves does the same three things: shape the
//! request into the core's own type, call the core, shape the answer back. No
//! check lives here that the core does not already make, and none can: the
//! authorization table is the core's, so serving a new wire cannot widen
//! anybody's authority.
//!
//! There is no authentication layer, by design. Identity and network
//! authorization are Custos's territory and are not reinvented here; the reach
//! of the socket is the reach of the facade, which is why the one address it
//! accepts on is a unix socket in the daemon's own data directory, whose mode
//! decides who may connect, and why remote access is an SSH tunnel rather than a
//! listener anything else can route to.
//!
//! Two things the facade decides rather than accepts:
//!
//! - **The clock.** The core reads none, so its caller says what "now" means.
//!   That caller is this facade, reading the machine's clock, never the request:
//!   an approval's expiry is checked against that number, and a client that
//!   supplied it could hand itself an authorization that had already run out.
//! - **The principal.** It is fixed when the facade is built, never read off the
//!   wire. A request cannot name who it is.

mod convert;
mod feed;
mod serve;
mod team;

// Generated from `proto/agens/coordinator/v1/coordinator.proto` at build time.
// The lints are relaxed for this module alone: it is machine output, and
// rewriting it to satisfy a style rule would mean it is no longer generated.
#[allow(
    clippy::all,
    clippy::pedantic,
    clippy::nursery,
    unreachable_pub,
    missing_docs
)]
#[rustfmt::skip]
pub mod proto {
    tonic::include_proto!("agens.coordinator.v1");
}

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use tonic::Status;

use crate::api::{ApiCore, ApiError, CreateRun, CreatedRun};
use crate::blocking::{BlockingBoundary, BlockingError};
use crate::fsm::{Principal, TransitionRejection};

pub use feed::FeedFacade;
pub use serve::{FacadeBinding, FacadeError, serve_until_shutdown};
pub use team::TeamFacade;

/// A facade's handle on the one service core the daemon owns.
///
/// The core is `&mut` per operation and sits on a synchronous SQLite
/// connection, so it is reached under a lock and from the blocking pool. Both
/// halves matter: the lock is what keeps the control plane's single-writer
/// property when several clients call at once, and the boundary is what keeps a
/// slow query off the runtime's worker threads.
#[derive(Clone)]
pub struct CoreHandle {
    core: Arc<Mutex<ApiCore>>,
    blocking: BlockingBoundary,
    principal: Principal,
}

impl CoreHandle {
    #[must_use]
    pub fn new(
        core: Arc<Mutex<ApiCore>>,
        blocking: BlockingBoundary,
        principal: Principal,
    ) -> Self {
        Self {
            core,
            blocking,
            principal,
        }
    }

    #[must_use]
    pub const fn principal(&self) -> Principal {
        self.principal
    }

    /// Runs one core operation and turns whatever came back into a `Status`.
    async fn call<T, F>(&self, work: F) -> Result<T, Status>
    where
        F: FnOnce(&mut ApiCore, Principal, i64) -> Result<T, ApiError> + Send + 'static,
        T: Send + 'static,
    {
        let core = Arc::clone(&self.core);
        let principal = self.principal;
        let now = now();

        let outcome = self
            .blocking
            .run(move || match core.lock() {
                Ok(mut guard) => work(&mut guard, principal, now),
                // A poisoned lock means a previous operation panicked while
                // holding the core. Refusing is the only honest answer: the
                // control plane's invariants were established by code that did
                // not finish.
                Err(_) => Err(ApiError::Storage(
                    "the service core is unusable after a failed operation".to_owned(),
                )),
            })
            .await
            .map_err(status_from_blocking)?;

        outcome.map_err(status_from_api)
    }

    /// Creates a run without holding the core across the provisioning it
    /// implies.
    ///
    /// The core is taken twice — once to decide, once to write the row — and is
    /// released for the step between them, which creates the worktree and runs
    /// whatever the repository declared. That step is bounded by the
    /// repository's own timeouts and may legitimately take minutes; the
    /// admission loop, the timer wheel and every other request wait on this
    /// same lock, so holding it there would stop the daemon for as long as one
    /// caller's hooks felt like running.
    async fn create_run(&self, request: CreateRun) -> Result<CreatedRun, Status> {
        let prepared = self
            .call({
                let request = request.clone();

                move |core, principal, _| core.prepare_run(principal, &request)
            })
            .await?;

        let worktrees = {
            let core = self.core.lock().map_err(|_| poisoned())?;

            Arc::clone(&core.ports().worktrees)
        };

        let provisioned = {
            let prepared = prepared.clone();
            let start_point = request.start_point.clone();

            self.blocking
                .run(move || {
                    worktrees
                        .provision(&prepared.worktree_request(&start_point))
                        .map_err(ApiError::Port)
                })
                .await
                .map_err(status_from_blocking)?
                .map_err(status_from_api)?
        };

        self.call(move |core, _, _| core.open_run(&request, &prepared, provisioned))
            .await
    }
}

/// A poisoned core means a previous operation panicked while holding it, so
/// what the control plane's invariants rest on was established by code that
/// did not finish.
fn poisoned() -> Status {
    Status::internal("the service core is unusable after a failed operation")
}

/// Epoch seconds, from the machine the daemon runs on.
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

fn status_from_blocking(error: BlockingError) -> Status {
    match error {
        BlockingError::Panicked => Status::internal("the operation did not complete"),
        BlockingError::ShuttingDown => Status::unavailable("the daemon is shutting down"),
    }
}

/// Maps a core refusal onto a gRPC code.
///
/// The distinction that matters to a client is between "you may not", "not from
/// this state" and "something here is broken", because only the middle one is
/// worth reacting to by looking at the run again. A refusal keeps its message:
/// the core already journaled it, and hiding the reason from the operator buys
/// nothing when the same text is in the daemon's own journal.
fn status_from_api(error: ApiError) -> Status {
    match error {
        ApiError::Unauthorized { .. } => Status::permission_denied(error.to_string()),
        ApiError::NotFound { .. } => Status::not_found(error.to_string()),
        ApiError::Rejected(TransitionRejection::NoSuchRow { table, id }) => {
            Status::not_found(format!("no {table} with id {id}"))
        }
        ApiError::Rejected(ref rejection) => match rejection {
            TransitionRejection::Storage(_) => Status::internal(error.to_string()),
            _ => Status::failed_precondition(error.to_string()),
        },
        ApiError::Port(_) | ApiError::Storage(_) => Status::internal(error.to_string()),
    }
}
