//! The Team plane over the wire.
//!
//! Each method shapes a request into the core's own type, calls the core, and
//! shapes the answer back. Nothing decides anything: which principal reaches
//! which operation is the core's table, and every guard behind it is the state
//! machine's. A refusal arrives here already journaled and is only given a gRPC
//! code.

use tonic::{Request, Response, Status};

use super::proto::team_server::Team;
use super::{CoreHandle, convert, proto};
use crate::api::{
    AnswerQuestion, ApprovePlan, AuthorizeMerge, CleaningAction, CleaningDisposition, CreateRun,
    RetryRequest, RunRef, StopRequest, StopScope,
};

/// The Team service, bound to one principal for its whole life.
pub struct TeamFacade {
    core: CoreHandle,
}

impl TeamFacade {
    #[must_use]
    pub const fn new(core: CoreHandle) -> Self {
        Self { core }
    }
}

/// The disposition names the wire accepts.
///
/// Spelled out rather than derived, because widening what a client may ask for
/// should mean writing a new name down. `dispose` throws away work that was
/// never shown to be merged, so an unrecognized name has to land on neither.
fn disposition(name: &str) -> Result<CleaningDisposition, Status> {
    match name {
        "reclaim" => Ok(CleaningDisposition::Reclaim),
        "dispose" => Ok(CleaningDisposition::Dispose),
        other => Err(Status::invalid_argument(format!(
            "no cleaning disposition named {other}"
        ))),
    }
}

/// How far a stop reaches.
///
/// An absent scope is refused rather than defaulted. The widest scope is every
/// session on the machine, and a default that reaches it would make an
/// incomplete request the most destructive one.
fn stop_scope(request: proto::StopRequest) -> Result<StopScope, Status> {
    match request.scope {
        Some(proto::stop_request::Scope::RunId(run_id)) => Ok(StopScope::Run(run_id)),
        Some(proto::stop_request::Scope::RepoId(repo_id)) => Ok(StopScope::Repo(repo_id)),
        Some(proto::stop_request::Scope::Machine(true)) => Ok(StopScope::Machine),
        Some(proto::stop_request::Scope::Machine(false)) => Err(Status::invalid_argument(
            "a machine scope set to false names nothing to stop",
        )),
        None => Err(Status::invalid_argument("a stop names how far it reaches")),
    }
}

/// The commit a run's branch starts from.
///
/// An empty field means the checkout's own `HEAD`, which is what a client that
/// has nothing particular in mind wants. It is spelled out here rather than in
/// the core, because a default is a wire convenience and the core takes what it
/// is given.
fn start_point(named: &str) -> String {
    if named.trim().is_empty() {
        "HEAD".to_owned()
    } else {
        named.to_owned()
    }
}

#[tonic::async_trait]
impl Team for TeamFacade {
    async fn create_run(
        &self,
        request: Request<proto::CreateRunRequest>,
    ) -> Result<Response<proto::CreateRunResponse>, Status> {
        let request = request.into_inner();

        let created = self
            .core
            .create_run(CreateRun {
                repo_root: std::path::PathBuf::from(request.repo_root),
                task: request.task,
                scope: request.scope,
                dod: request.dod,
                external_ref: request.external_ref,
                parent_run_id: request.parent_run_id,
                dep_run_id: request.dep_run_id,
                provider: request.provider,
                priority: request.priority,
                budget_tokens: request.budget_tokens,
                start_point: start_point(&request.start_point),
                now: super::now(),
            })
            .await?;

        Ok(Response::new(proto::CreateRunResponse {
            run_id: created.run_id,
            repo_id: created.repo_id,
            worktree_path: created.worktree_path.display().to_string(),
            hook_failures: created.hook_failures,
            hooks_ran: created.hooks_ran,
            hook_authorization_question: created.hook_authorization_question,
        }))
    }

    async fn approve_plan(
        &self,
        request: Request<proto::ApprovePlanRequest>,
    ) -> Result<Response<proto::Ack>, Status> {
        let run_id = request.into_inner().run_id;

        let outcome = self
            .core
            .call(move |core, principal, now| {
                core.approve_plan(principal, &ApprovePlan { run_id, now })
            })
            .await?;

        Ok(Response::new(proto::Ack {
            transition: Some(convert::transition(&outcome)),
        }))
    }

    async fn answer_question(
        &self,
        request: Request<proto::AnswerQuestionRequest>,
    ) -> Result<Response<proto::AnswerAck>, Status> {
        let request = request.into_inner();

        let answered = self
            .core
            .call(move |core, principal, now| {
                core.answer_question(
                    principal,
                    &AnswerQuestion {
                        question_id: request.question_id,
                        answer: request.answer,
                        now,
                    },
                )
            })
            .await?;

        Ok(Response::new(convert::answer_ack(&answered)))
    }

    async fn authorize_merge(
        &self,
        request: Request<proto::AuthorizeMergeRequest>,
    ) -> Result<Response<proto::Ack>, Status> {
        let request = request.into_inner();

        let outcome = self
            .core
            .call(move |core, principal, now| {
                core.authorize_merge(
                    principal,
                    &AuthorizeMerge {
                        question_id: request.question_id,
                        answer: request.answer,
                        now,
                    },
                )
            })
            .await?;

        Ok(Response::new(proto::Ack {
            transition: Some(convert::transition(&outcome)),
        }))
    }

    async fn cancel_run(
        &self,
        request: Request<proto::CancelRunRequest>,
    ) -> Result<Response<proto::Ack>, Status> {
        let run_id = request.into_inner().run_id;

        let outcome = self
            .core
            .call(move |core, principal, now| core.cancel_run(principal, &RunRef { run_id, now }))
            .await?;

        Ok(Response::new(proto::Ack {
            transition: Some(convert::transition(&outcome)),
        }))
    }

    async fn retry(
        &self,
        request: Request<proto::RetryRequest>,
    ) -> Result<Response<proto::Ack>, Status> {
        let request = request.into_inner();

        let outcome = self
            .core
            .call(move |core, principal, now| {
                core.retry(
                    principal,
                    &RetryRequest {
                        run_id: request.run_id,
                        guidance: request.guidance,
                        retry_budget: request.retry_budget,
                        now,
                    },
                )
            })
            .await?;

        Ok(Response::new(proto::Ack {
            transition: Some(convert::transition(&outcome)),
        }))
    }

    async fn cleaning(
        &self,
        request: Request<proto::CleaningRequest>,
    ) -> Result<Response<proto::Ack>, Status> {
        let request = request.into_inner();
        let disposition = disposition(&request.disposition)?;

        let outcome = self
            .core
            .call(move |core, principal, now| {
                core.cleaning(
                    principal,
                    &CleaningAction {
                        run_id: request.run_id,
                        disposition,
                        confirmed: request.confirmed,
                        now,
                    },
                )
            })
            .await?;

        Ok(Response::new(proto::Ack {
            transition: Some(convert::transition(&outcome)),
        }))
    }

    async fn takeover(
        &self,
        request: Request<proto::TakeoverRequest>,
    ) -> Result<Response<proto::TakeoverResponse>, Status> {
        let run_id = request.into_inner().run_id;

        let handle = self
            .core
            .call(move |core, principal, now| core.takeover(principal, &RunRef { run_id, now }))
            .await?;

        Ok(Response::new(proto::TakeoverResponse {
            run_id: handle.run_id,
            session_id: handle.session_id,
        }))
    }

    async fn pause_admissions(
        &self,
        request: Request<proto::PauseAdmissionsRequest>,
    ) -> Result<Response<proto::AdmissionState>, Status> {
        let paused = request.into_inner().paused;

        let state = self
            .core
            .call(move |core, principal, now| core.pause_admissions(principal, paused, now))
            .await?;

        Ok(Response::new(convert::admission_state(state)))
    }

    async fn stop(
        &self,
        request: Request<proto::StopRequest>,
    ) -> Result<Response<proto::AdmissionState>, Status> {
        let scope = stop_scope(request.into_inner())?;

        let state = self
            .core
            .call(move |core, principal, now| core.stop(principal, &StopRequest { scope, now }))
            .await?;

        Ok(Response::new(convert::admission_state(state)))
    }
}
