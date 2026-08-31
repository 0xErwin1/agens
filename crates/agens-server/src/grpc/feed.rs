//! The Feed plane over the wire: read-only, and scoped by repository.
//!
//! One daemon serves N projects, so `Tree` and `Inbox` take a repository and
//! `Subscribe`'s filter carries one. The design's own sketch predates that and
//! shows all three unscoped; served that way they would hand a client the runs
//! of every project on the machine.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::mpsc::RecvTimeoutError;

use tokio_stream::{Stream, wrappers::ReceiverStream};
use tonic::{Request, Response, Status};

use super::proto::feed_server::Feed;
use super::subscriptions::{
    FORWARD_PATIENCE, LIVE_SUBSCRIPTIONS, SUBSCRIPTION_BUFFER, SubscriptionSlots, forward,
};
use super::{CoreHandle, convert, proto};
use crate::api::EventFilter;

pub struct FeedFacade {
    core: CoreHandle,
    slots: Arc<SubscriptionSlots>,
}

impl FeedFacade {
    #[must_use]
    pub fn new(core: CoreHandle) -> Self {
        Self::with_subscription_ceiling(core, LIVE_SUBSCRIPTIONS)
    }

    /// A facade that forwards at most `ceiling` subscriptions at once.
    ///
    /// Exists for the tests that drive the ceiling itself: reaching the
    /// production one over the wire would mean opening sixty-four streams to
    /// assert about the sixty-fifth.
    #[must_use]
    pub fn with_subscription_ceiling(core: CoreHandle, ceiling: usize) -> Self {
        Self {
            core,
            slots: Arc::new(SubscriptionSlots::new(ceiling)),
        }
    }
}

type EventStream = Pin<Box<dyn Stream<Item = Result<proto::Event, Status>> + Send>>;

#[tonic::async_trait]
impl Feed for FeedFacade {
    type SubscribeStream = EventStream;

    /// Opens a subscription and forwards it to the client.
    ///
    /// The core's end of the fan-out is a synchronous channel, so one thread
    /// per subscriber moves entries across. It is a thread of its own rather
    /// than a task on the blocking pool: the forwarder spends its whole life
    /// parked on a channel, and the blocking pool is the same pool every core
    /// operation crosses into, so enough idle subscribers there would leave the
    /// facade with nowhere to run a query.
    ///
    /// It ends when any of three things happens: the coordinator drops its
    /// sender, the client hangs up, or the client stops reading for longer than
    /// the forwarder waits. The hang-up is noticed on the same wait rather than
    /// on the next journal entry, so a subscription to a quiet filter does not
    /// keep a thread alive until something unrelated happens to be published.
    async fn subscribe(
        &self,
        request: Request<proto::EventFilter>,
    ) -> Result<Response<Self::SubscribeStream>, Status> {
        let request = request.into_inner();
        let classes = convert::event_classes(&request.classes)?;

        // Before the subscription is opened rather than after: a subscription
        // the core registered and nothing forwards is an entry queued for a
        // reader that will never come, held against the fan-out's backlog.
        let Some(slot) = self.slots.take() else {
            return Err(Status::resource_exhausted(
                "this daemon is forwarding as many subscriptions as it can; \
                 close one before opening another",
            ));
        };

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
        let patience = FORWARD_PATIENCE;

        std::thread::spawn(move || {
            // Moved into the forwarder so the slot is released by the thread
            // ending, whichever of the three ways it ends.
            let _slot = slot;

            loop {
                match subscription.recv_timeout(patience) {
                    Ok(event) if forward(&sender, convert::event(&event), patience) => {}
                    Ok(_) => return,
                    Err(RecvTimeoutError::Timeout) if sender.is_closed() => return,
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }

    async fn repos(
        &self,
        _request: Request<proto::ReposRequest>,
    ) -> Result<Response<proto::RepoList>, Status> {
        let repo_ids = self
            .core
            .call(move |core, principal, now| core.repos(principal, now))
            .await?;

        Ok(Response::new(proto::RepoList { repo_ids }))
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
