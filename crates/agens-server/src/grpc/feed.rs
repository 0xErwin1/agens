//! The Feed plane over the wire: read-only, and scoped by repository.
//!
//! One daemon serves N projects, so `Tree` and `Inbox` take a repository and
//! `Subscribe`'s filter carries one. The design's own sketch predates that and
//! shows all three unscoped; served that way they would hand a client the runs
//! of every project on the machine.

use std::pin::Pin;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use tokio_stream::{Stream, wrappers::ReceiverStream};
use tonic::{Request, Response, Status};

use super::proto::feed_server::Feed;
use super::{CoreHandle, convert, proto};
use crate::api::EventFilter;

/// How many journal entries a slow subscriber may fall behind before the
/// forwarder starts waiting for it.
const SUBSCRIPTION_BUFFER: usize = 256;

/// How long the forwarder waits on a client that has stopped reading before it
/// ends the stream.
///
/// The wait is bounded rather than indefinite: a forwarder blocked forever on
/// one client holds the entries behind it and the thread carrying them, and the
/// core's own fan-out backlog fills up behind that. A client that has not taken
/// a single entry in this long is not reading, and the stream it is not reading
/// is the one thing that has to end for the daemon to reclaim both.
const FORWARD_PATIENCE: Duration = Duration::from_secs(30);

/// How often the forwarder looks again at a client whose buffer is full.
const FORWARD_POLL: Duration = Duration::from_millis(10);

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

/// Hands one entry to the client, waiting at most `patience` for room.
///
/// Written against the synchronous half of the channel rather than awaiting
/// `send_timeout`, because this runs on a thread of its own with no runtime
/// under it: entering one to wait would tie the forwarder's timeout to a
/// runtime it does not belong to.
///
/// `false` means the stream is over — the client hung up, or it has not taken a
/// single entry for longer than the daemon waits.
fn forward(
    sender: &tokio::sync::mpsc::Sender<Result<proto::Event, Status>>,
    event: Result<proto::Event, Status>,
    patience: Duration,
) -> bool {
    use tokio::sync::mpsc::error::TrySendError;

    let deadline = Instant::now() + patience;
    let mut event = event;

    loop {
        match sender.try_send(event) {
            Ok(()) => return true,
            Err(TrySendError::Closed(_)) => return false,
            Err(TrySendError::Full(_)) if Instant::now() >= deadline => return false,
            Err(TrySendError::Full(returned)) => {
                event = returned;
                std::thread::sleep(FORWARD_POLL);
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: i64) -> Result<proto::Event, Status> {
        Ok(proto::Event {
            id,
            run_id: None,
            r#type: "checkpoint".to_owned(),
            class: "agent".to_owned(),
            payload: String::new(),
            ts: 1_700_000_000 + id,
        })
    }

    #[test]
    fn a_client_that_takes_nothing_ends_its_own_stream() {
        const PATIENCE: Duration = Duration::from_millis(50);

        let (sender, _receiver) = tokio::sync::mpsc::channel(1);

        assert!(forward(&sender, entry(1), PATIENCE), "the first entry fits");

        let started = Instant::now();

        assert!(
            !forward(&sender, entry(2), PATIENCE),
            "the entry that does not fit ends the stream rather than waiting forever"
        );
        assert!(started.elapsed() >= PATIENCE, "it waited before giving up");
    }

    #[test]
    fn a_client_that_catches_up_within_the_wait_keeps_its_stream() {
        const PATIENCE: Duration = Duration::from_secs(10);

        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);

        assert!(forward(&sender, entry(1), PATIENCE));

        let reader = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            let taken = receiver.blocking_recv();

            (receiver, taken)
        });

        assert!(
            forward(&sender, entry(2), PATIENCE),
            "room that appears inside the wait is used rather than missed"
        );

        let (_receiver, taken) = reader.join().unwrap();
        assert!(taken.is_some());
    }

    #[test]
    fn a_client_that_hung_up_ends_the_forwarder_at_once() {
        let (sender, receiver) = tokio::sync::mpsc::channel(4);
        drop(receiver);

        let started = Instant::now();

        assert!(!forward(&sender, entry(1), Duration::from_secs(60)));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a closed stream is not something to wait out"
        );
    }
}
