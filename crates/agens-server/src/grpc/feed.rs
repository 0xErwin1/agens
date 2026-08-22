//! The Feed plane over the wire: read-only, and scoped by repository.
//!
//! One daemon serves N projects, so `Tree` and `Inbox` take a repository and
//! `Subscribe`'s filter carries one. The design's own sketch predates that and
//! shows all three unscoped; served that way they would hand a client the runs
//! of every project on the machine.

use std::pin::Pin;

use tokio_stream::{Stream, wrappers::ReceiverStream};
use tonic::{Request, Response, Status};

use super::proto::feed_server::Feed;
use super::{CoreHandle, convert, proto};
use crate::api::EventFilter;

/// How many journal entries a slow subscriber may fall behind before the
/// forwarder waits for it.
///
/// The forwarder blocks rather than dropping, so a client that stops reading
/// stalls its own stream and nobody else's. Dropping instead would give a
/// subscriber a gap in an append-only journal with no way to notice it.
const SUBSCRIPTION_BUFFER: usize = 256;

pub struct FeedFacade {
    core: CoreHandle,
}

impl FeedFacade {
    #[must_use]
    pub const fn new(core: CoreHandle) -> Self {
        Self { core }
    }
}

type EventStream = Pin<Box<dyn Stream<Item = Result<proto::Event, Status>> + Send>>;

#[tonic::async_trait]
impl Feed for FeedFacade {
    type SubscribeStream = EventStream;

    /// Opens a subscription and forwards it to the client.
    ///
    /// The core's end of the fan-out is a synchronous channel, so one blocking
    /// task per subscriber moves entries across. It ends when either side goes:
    /// the coordinator dropping its sender, or the client hanging up and the
    /// forward failing.
    async fn subscribe(
        &self,
        request: Request<proto::EventFilter>,
    ) -> Result<Response<Self::SubscribeStream>, Status> {
        let request = request.into_inner();
        let classes = convert::event_classes(&request.classes)?;

        let filter = EventFilter {
            repo_id: request.repo_id,
            run_id: request.run_id,
            classes,
        };

        let subscription = self
            .core
            .call(move |core, principal, now| core.subscribe(principal, &filter, now))
            .await?;

        let (sender, receiver) = tokio::sync::mpsc::channel(SUBSCRIPTION_BUFFER);

        tokio::task::spawn_blocking(move || {
            while let Ok(event) = subscription.recv() {
                if sender.blocking_send(convert::event(&event)).is_err() {
                    return;
                }
            }
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }

    async fn tree(
        &self,
        request: Request<proto::TreeRequest>,
    ) -> Result<Response<proto::TreeSnapshot>, Status> {
        let repo_id = repository(request.into_inner().repo_id)?;

        let snapshot = self
            .core
            .call(move |core, principal, now| core.tree(principal, &repo_id, now))
            .await?;

        Ok(Response::new(convert::tree_snapshot(&snapshot)))
    }

    async fn run_detail(
        &self,
        request: Request<proto::RunDetailRequest>,
    ) -> Result<Response<proto::RunView>, Status> {
        let run_id = request.into_inner().run_id;

        let view = self
            .core
            .call(move |core, principal, now| core.run_detail(principal, run_id, now))
            .await?;

        Ok(Response::new(convert::run_view(&view)?))
    }

    async fn inbox(
        &self,
        request: Request<proto::InboxRequest>,
    ) -> Result<Response<proto::InboxView>, Status> {
        let repo_id = repository(request.into_inner().repo_id)?;

        let view = self
            .core
            .call(move |core, principal, now| core.inbox(principal, &repo_id, now))
            .await?;

        Ok(Response::new(convert::inbox_view(&view)))
    }
}

/// The repository a listing is scoped to.
///
/// Empty is refused rather than read as "every repository": proto3 cannot tell
/// an unset string from an empty one, so a client that forgot the field would
/// otherwise get exactly the cross-project listing the scope exists to prevent.
fn repository(repo_id: String) -> Result<String, Status> {
    if repo_id.is_empty() {
        return Err(Status::invalid_argument(
            "a listing names the repository it is scoped to",
        ));
    }

    Ok(repo_id)
}
