//! The control plane, as a surface uses it.
//!
//! Thin on purpose. Every method here is one call and no logic: what a
//! transition is allowed to do is the coordinator's decision, and a client that
//! pre-judged it would be a second, quieter copy of a rule that already exists
//! behind the socket.

use tonic::transport::Channel;

use crate::ClientError;
use crate::proto;

/// A handle on one daemon's control plane.
#[derive(Clone, Debug)]
pub struct TeamClient {
    inner: proto::team_client::TeamClient<Channel>,
}

impl TeamClient {
    pub(crate) fn new(channel: Channel) -> Self {
        Self {
            inner: proto::team_client::TeamClient::new(channel),
        }
    }

    /// Proposes a run against a checkout.
    pub async fn create_run(
        &mut self,
        request: proto::CreateRunRequest,
    ) -> Result<proto::CreateRunResponse, ClientError> {
        Ok(self.inner.create_run(request).await?.into_inner())
    }

    /// Approves a run's plan, which is what makes it eligible to be admitted.
    pub async fn approve_plan(&mut self, run_id: i64) -> Result<proto::Ack, ClientError> {
        Ok(self
            .inner
            .approve_plan(proto::ApprovePlanRequest { run_id })
            .await?
            .into_inner())
    }

    /// Answers a question a run parked on.
    pub async fn answer_question(
        &mut self,
        question_id: i64,
        answer: &str,
    ) -> Result<proto::AnswerAck, ClientError> {
        Ok(self
            .inner
            .answer_question(proto::AnswerQuestionRequest {
                question_id,
                answer: answer.to_owned(),
            })
            .await?
            .into_inner())
    }

    /// Grants a merge on an approval, opening one over the run's worktree as it
    /// stands when the request names a run rather than an approval.
    pub async fn authorize_merge(
        &mut self,
        request: proto::AuthorizeMergeRequest,
    ) -> Result<proto::AuthorizeMergeAck, ClientError> {
        Ok(self.inner.authorize_merge(request).await?.into_inner())
    }

    pub async fn cancel_run(&mut self, run_id: i64) -> Result<proto::Ack, ClientError> {
        Ok(self
            .inner
            .cancel_run(proto::CancelRunRequest { run_id })
            .await?
            .into_inner())
    }

    /// Sends a run back with guidance and a fresh budget of chargeable
    /// attempts.
    pub async fn retry(
        &mut self,
        run_id: i64,
        guidance: &str,
        retry_budget: i64,
    ) -> Result<proto::Ack, ClientError> {
        Ok(self
            .inner
            .retry(proto::RetryRequest {
                run_id,
                guidance: guidance.to_owned(),
                retry_budget,
            })
            .await?
            .into_inner())
    }

    /// Releases a merged worktree, or throws away an active one.
    ///
    /// `confirmed` is the person's confirmation relayed, never a client's own:
    /// throwing away work nobody agreed to lose is the one thing this plane
    /// will not do on a surface's say-so.
    pub async fn cleaning(
        &mut self,
        run_id: i64,
        disposition: &str,
        confirmed: bool,
    ) -> Result<proto::Ack, ClientError> {
        Ok(self
            .inner
            .cleaning(proto::CleaningRequest {
                run_id,
                disposition: disposition.to_owned(),
                confirmed,
            })
            .await?
            .into_inner())
    }

    /// Where a run's live session is, for a person who wants to drive it
    /// themselves.
    pub async fn takeover(&mut self, run_id: i64) -> Result<proto::TakeoverResponse, ClientError> {
        Ok(self
            .inner
            .takeover(proto::TakeoverRequest { run_id })
            .await?
            .into_inner())
    }

    /// Stops admitting new runs, or starts again.
    pub async fn pause_admissions(
        &mut self,
        paused: bool,
    ) -> Result<proto::AdmissionState, ClientError> {
        Ok(self
            .inner
            .pause_admissions(proto::PauseAdmissionsRequest { paused })
            .await?
            .into_inner())
    }

    /// Stops one run, one repository's runs, or every session on the machine.
    pub async fn stop(
        &mut self,
        scope: proto::stop_request::Scope,
    ) -> Result<proto::AdmissionState, ClientError> {
        Ok(self
            .inner
            .stop(proto::StopRequest { scope: Some(scope) })
            .await?
            .into_inner())
    }
}
