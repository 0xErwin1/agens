//! The read plane, as a surface uses it.
//!
//! Every method here reads and none writes, which is the plane's own guarantee:
//! a surface projecting the coordinator cannot cause a state it is drawing.

use tokio_stream::{Stream, StreamExt};
use tonic::transport::Channel;

use crate::ClientError;
use crate::proto;

/// One entry of the coordinator's journal.
///
/// Re-exported under a name of its own rather than handed over as the generated
/// type, so a surface holding it is not holding a wire type it would have to
/// re-import if the transport ever changed.
pub type JournalEntry = proto::Event;

/// A handle on one daemon's read plane.
#[derive(Clone, Debug)]
pub struct FeedClient {
    inner: proto::feed_client::FeedClient<Channel>,
}

impl FeedClient {
    pub(crate) fn new(channel: Channel) -> Self {
        Self {
            inner: proto::feed_client::FeedClient::new(channel),
        }
    }

    /// Follows the coordinator's journal for one repository, live from now.
    ///
    /// The repository is required rather than optional. One daemon serves N
    /// projects, so an unscoped subscription is one that hands a surface
    /// another project's runs.
    pub async fn subscribe(
        &mut self,
        repo_id: &str,
        classes: Vec<String>,
    ) -> Result<impl Stream<Item = Result<JournalEntry, ClientError>> + use<>, ClientError> {
        let events = self
            .inner
            .subscribe(proto::EventFilter {
                repo_id: Some(repo_id.to_owned()),
                run_id: None,
                classes,
            })
            .await?
            .into_inner();

        Ok(events.map(|event| event.map_err(ClientError::Refused)))
    }

    /// Follows one run rather than a whole repository.
    pub async fn subscribe_to_run(
        &mut self,
        run_id: i64,
    ) -> Result<impl Stream<Item = Result<JournalEntry, ClientError>> + use<>, ClientError> {
        let events = self
            .inner
            .subscribe(proto::EventFilter {
                repo_id: None,
                run_id: Some(run_id),
                classes: Vec::new(),
            })
            .await?
            .into_inner();

        Ok(events.map(|event| event.map_err(ClientError::Refused)))
    }

    /// Every repository the daemon holds runs for, in repository-id order.
    ///
    /// How a fleet surface learns which projects exist: a repository is here
    /// because somebody created a run against it, never because a client
    /// configured it in advance. Each one is then read through [`Self::tree`].
    pub async fn repos(&mut self) -> Result<Vec<String>, ClientError> {
        Ok(self
            .inner
            .repos(proto::ReposRequest {})
            .await?
            .into_inner()
            .repo_ids)
    }

    /// The tree of runs one repository has.
    pub async fn tree(&mut self, repo_id: &str) -> Result<proto::TreeSnapshot, ClientError> {
        Ok(self
            .inner
            .tree(proto::TreeRequest {
                repo_id: repo_id.to_owned(),
            })
            .await?
            .into_inner())
    }

    /// Everything one run is: its row, its attempts, its questions, its
    /// findings, its journal and its health.
    pub async fn run_detail(&mut self, run_id: i64) -> Result<proto::RunView, ClientError> {
        Ok(self
            .inner
            .run_detail(proto::RunDetailRequest { run_id })
            .await?
            .into_inner())
    }

    /// What is waiting on a person in one repository.
    pub async fn inbox(&mut self, repo_id: &str) -> Result<proto::InboxView, ClientError> {
        Ok(self
            .inner
            .inbox(proto::InboxRequest {
                repo_id: repo_id.to_owned(),
            })
            .await?
            .into_inner())
    }
}
