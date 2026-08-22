//! The Team plane: the operations that move the control plane.
//!
//! Every one of them has the same shape. Check the authorization table, load
//! whatever the subject-level check needs, ask the state machine to move, and
//! perform the effects the machine declared through the ports. Nothing here
//! decides a state: the machine does, and this module reads what it decided.
//!
//! Time arrives in the request. The machines read no clock and neither does
//! this, so a caller replaying or reconciling decides what "now" means.

use agens_store::{
    QuestionAuthor, QuestionKind, QuestionRow, RetryTrigger, RunRow, RunState, WorktreeStatus,
};

use super::{ApiCore, ApiError, Operation, praetor_may_answer};
use crate::api::ports::{Delivery, DeliveryPayload, StopScope, TakeoverHandle};
use crate::fsm::{
    Principal, QuestionEffect, QuestionFacts, QuestionTrigger, RunEffect, RunFacts, RunTrigger,
    TransitionOutcome, WorktreeEffect, WorktreeFacts, WorktreeTrigger,
};

/// Naming a run, and when.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunRef {
    pub run_id: i64,
    /// Epoch seconds.
    pub now: i64,
}

/// The user approving a proposed execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApprovePlan {
    pub run_id: i64,
    pub now: i64,
}

/// An answer to a question that is blocking a run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnswerQuestion {
    pub question_id: i64,
    pub answer: String,
    pub now: i64,
}

/// A question answered, and the run it unblocked when it blocked one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnsweredQuestion {
    pub run_id: i64,
    pub question: TransitionOutcome<agens_store::QuestionState, QuestionEffect>,
    /// Present only when the run was parked on this question. Answering a
    /// question a running worker is not blocked on moves nothing.
    pub run: Option<TransitionOutcome<RunState, RunEffect>>,
}

/// The user granting a merge authorization.
///
/// It is a question of kind `approval`, so granting it is answering it. What
/// separates it is the receipt: the authorization is bound to the bytes frozen
/// when it was created, not to the run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorizeMerge {
    pub question_id: i64,
    /// What the user answered. Recorded with the grant, because the record of
    /// whose authority a merge carried cannot be reconstructed later.
    pub answer: String,
    pub now: i64,
}

/// A retry of an approved scope, with the guidance that makes it a different
/// attempt rather than the same one again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetryRequest {
    pub run_id: i64,
    pub guidance: String,
    /// How many chargeable attempts this run gets.
    pub retry_budget: i64,
    pub now: i64,
}

/// What the cleaning flow is doing to a worktree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CleaningDisposition {
    /// Release a worktree already shown to be merged.
    Reclaim,
    /// Throw away an active worktree whose branch was never shown to be
    /// merged. Only a person's confirmation reaches this.
    Dispose,
}

/// One cleaning action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CleaningAction {
    pub run_id: i64,
    pub disposition: CleaningDisposition,
    /// Set only by a caller relaying a person's confirmation of the disposal.
    pub confirmed: bool,
    pub now: i64,
}

/// Whether admission is paused, and whether this request changed it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdmissionState {
    pub paused: bool,
    pub previously_paused: bool,
}

impl AdmissionState {
    #[must_use]
    pub const fn changed(self) -> bool {
        self.paused != self.previously_paused
    }
}

/// Stopping the team, at some reach.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StopRequest {
    pub scope: StopScope,
    pub now: i64,
}

impl ApiCore {
    /// Freezes a proposed execution's scope and queues it.
    ///
    /// The user's alone, refused for anyone else before the machine is asked.
    /// The run machine's own approval guard refuses it a second time, which is
    /// deliberate: the core is where authority is decided, and the guard is the
    /// backstop that survives a future caller reaching the machine directly.
    pub fn approve_plan(
        &mut self,
        principal: Principal,
        request: &ApprovePlan,
    ) -> Result<TransitionOutcome<RunState, RunEffect>, ApiError> {
        self.authorize(
            Operation::ApprovePlan,
            principal,
            Some(request.run_id),
            request.now,
        )?;

        let outcome = self.machines.apply_run(
            request.run_id,
            RunTrigger::Approve,
            &RunFacts {
                now: request.now,
                principal,
                ..RunFacts::default()
            },
        )?;

        self.announce_queued(&outcome, request.run_id);

        Ok(outcome)
    }

    /// Answers a question, and resumes the run if it was parked on it.
    ///
    /// Praetor reaches this operation, but not every question: the
    /// detail-question policy decides which, and an authorization is never one
    /// of them.
    pub fn answer_question(
        &mut self,
        principal: Principal,
        request: &AnswerQuestion,
    ) -> Result<AnsweredQuestion, ApiError> {
        let question = self.load_question(request.question_id)?;

        self.authorize(
            Operation::AnswerQuestion,
            principal,
            Some(question.run_id),
            request.now,
        )?;

        if question.kind == QuestionKind::Approval {
            return Err(self.refuse(
                Operation::AnswerQuestion,
                principal,
                Some(question.run_id),
                request.now,
                "an authorization is granted through authorize_merge, not answered".to_owned(),
            ));
        }

        if principal == Principal::Praetor
            && let Err(refusal) = praetor_may_answer(&question, &request.answer)
        {
            return Err(self.refuse(
                Operation::AnswerQuestion,
                principal,
                Some(question.run_id),
                request.now,
                refusal.detail().to_owned(),
            ));
        }

        let author = self.author_for(
            Operation::AnswerQuestion,
            principal,
            Some(question.run_id),
            request.now,
        )?;

        let answered = self.machines.apply_question(
            request.question_id,
            QuestionTrigger::Answer,
            &QuestionFacts {
                now: request.now,
                answer: Some(request.answer.clone()),
                author: Some(author),
            },
        )?;

        self.perform_question_effects(&answered, &question, &request.answer)?;

        let run = self.resume_run_waiting_on(&question, request.question_id, request.now)?;

        Ok(AnsweredQuestion {
            run_id: question.run_id,
            question: answered,
            run,
        })
    }

    /// Grants a merge authorization.
    ///
    /// The user's alone. Praetor is refused by the table, and the question
    /// machine refuses a non-user author on top of that.
    pub fn authorize_merge(
        &mut self,
        principal: Principal,
        request: &AuthorizeMerge,
    ) -> Result<TransitionOutcome<agens_store::QuestionState, QuestionEffect>, ApiError> {
        let question = self.load_question(request.question_id)?;

        self.authorize(
            Operation::AuthorizeMerge,
            principal,
            Some(question.run_id),
            request.now,
        )?;

        if question.kind != QuestionKind::Approval {
            return Err(self.refuse(
                Operation::AuthorizeMerge,
                principal,
                Some(question.run_id),
                request.now,
                "only an approval authorizes a merge".to_owned(),
            ));
        }

        // The schema refuses to store an approval without a receipt, so this
        // is the second lock on the same door. It stays because the receipt is
        // what the gate compares against, and a grant that reached it with
        // nothing frozen would pass a comparison built to fail.
        if question.tree_hash.is_none() || question.paths_digest.is_none() {
            return Err(self.refuse(
                Operation::AuthorizeMerge,
                principal,
                Some(question.run_id),
                request.now,
                "an approval without a receipt authorizes no bytes, and the gate would compare \
                 against nothing"
                    .to_owned(),
            ));
        }

        let author = self.author_for(
            Operation::AuthorizeMerge,
            principal,
            Some(question.run_id),
            request.now,
        )?;

        let granted = self.machines.apply_question(
            request.question_id,
            QuestionTrigger::Answer,
            &QuestionFacts {
                now: request.now,
                answer: Some(request.answer.clone()),
                author: Some(author),
            },
        )?;

        self.perform_question_effects(&granted, &question, &request.answer)?;

        Ok(granted)
    }

    /// Cancels a run. Idempotent: a run already cancelled reports settled and
    /// nothing is signalled twice.
    pub fn cancel_run(
        &mut self,
        principal: Principal,
        request: &RunRef,
    ) -> Result<TransitionOutcome<RunState, RunEffect>, ApiError> {
        self.authorize(
            Operation::CancelRun,
            principal,
            Some(request.run_id),
            request.now,
        )?;

        let outcome = self.machines.apply_run(
            request.run_id,
            RunTrigger::Cancel,
            &RunFacts {
                now: request.now,
                principal,
                ..RunFacts::default()
            },
        )?;

        if let Some(applied) = outcome.applied()
            && applied.effects.contains(&RunEffect::CancelImmediately)
        {
            self.ports.sessions.cancel(request.run_id)?;
        }

        Ok(outcome)
    }

    /// Queues a new attempt at an already approved scope, recording who asked.
    pub fn retry(
        &mut self,
        principal: Principal,
        request: &RetryRequest,
    ) -> Result<TransitionOutcome<RunState, RunEffect>, ApiError> {
        self.authorize(
            Operation::Retry,
            principal,
            Some(request.run_id),
            request.now,
        )?;

        let retry_trigger = match principal {
            Principal::User => RetryTrigger::User,
            Principal::Praetor => RetryTrigger::Praetor,
            Principal::Coordinator => RetryTrigger::Coordinator,
        };

        let outcome = self.machines.apply_run(
            request.run_id,
            RunTrigger::Retry,
            &RunFacts {
                now: request.now,
                principal,
                guidance: Some(request.guidance.clone()),
                retry_trigger: Some(retry_trigger),
                retry_budget: request.retry_budget,
                ..RunFacts::default()
            },
        )?;

        self.announce_queued(&outcome, request.run_id);

        Ok(outcome)
    }

    /// Releases or discards a run's worktree.
    ///
    /// Both dispositions take their facts from a live git derivation rather
    /// than from the caller: a merge a request claims and git does not is the
    /// state the derivation exists to refuse.
    pub fn cleaning(
        &mut self,
        principal: Principal,
        request: &CleaningAction,
    ) -> Result<TransitionOutcome<WorktreeStatus, WorktreeEffect>, ApiError> {
        self.authorize(
            Operation::Cleaning,
            principal,
            Some(request.run_id),
            request.now,
        )?;

        let run = self.load_run(request.run_id)?;
        let derivation = self.ports.worktrees.derive(&run)?;

        let (trigger, facts) = match request.disposition {
            CleaningDisposition::Reclaim => (
                WorktreeTrigger::Reclaim,
                WorktreeFacts {
                    now: request.now,
                    merge_re_derived: derivation.branch_merged,
                    worktree_clean: derivation.worktree_clean,
                    manual_disposition_confirmed: false,
                },
            ),
            CleaningDisposition::Dispose => (
                WorktreeTrigger::ManualDisposition,
                WorktreeFacts {
                    now: request.now,
                    merge_re_derived: derivation.branch_merged,
                    worktree_clean: derivation.worktree_clean,
                    manual_disposition_confirmed: request.confirmed,
                },
            ),
        };

        let outcome = self
            .machines
            .apply_worktree(request.run_id, trigger, &facts)?;

        if let Some(applied) = outcome.applied()
            && applied
                .effects
                .contains(&WorktreeEffect::RemoveWorktreeDirectory)
        {
            self.ports.worktrees.remove(&run)?;
        }

        Ok(outcome)
    }

    /// Hands a run's live session to the user.
    pub fn takeover(
        &mut self,
        principal: Principal,
        request: &RunRef,
    ) -> Result<TakeoverHandle, ApiError> {
        self.authorize(
            Operation::Takeover,
            principal,
            Some(request.run_id),
            request.now,
        )?;

        Ok(self.ports.sessions.take_over(request.run_id)?)
    }

    /// Stops or resumes admission of queued runs.
    pub fn pause_admissions(
        &mut self,
        principal: Principal,
        paused: bool,
        now: i64,
    ) -> Result<AdmissionState, ApiError> {
        self.authorize(Operation::PauseAdmissions, principal, None, now)?;

        let previously_paused = self.ports.scheduler.set_admissions_paused(paused)?;

        Ok(AdmissionState {
            paused,
            previously_paused,
        })
    }

    /// Stops the team, pausing admission first.
    ///
    /// The order is the whole point: a team that keeps admitting runs while it
    /// is being stopped is not stopped, so the toggle goes down before any
    /// session is asked to end.
    pub fn stop(
        &mut self,
        principal: Principal,
        request: &StopRequest,
    ) -> Result<AdmissionState, ApiError> {
        self.authorize(Operation::Stop, principal, None, request.now)?;

        let previously_paused = self.ports.scheduler.set_admissions_paused(true)?;
        self.ports.sessions.stop(&request.scope)?;

        Ok(AdmissionState {
            paused: true,
            previously_paused,
        })
    }

    /// Tells the scheduler when a transition left the run queued.
    ///
    /// Which effects a queued run declares — resume priority, a retry attempt,
    /// a frozen scope — are all the scheduler's to act on, and it reads them
    /// from the row. The core only says that the queue moved.
    fn announce_queued(&self, outcome: &TransitionOutcome<RunState, RunEffect>, run_id: i64) {
        if outcome
            .applied()
            .is_some_and(|applied| applied.to == RunState::Queued)
        {
            self.ports.scheduler.queue_changed(run_id);
        }
    }

    /// Performs a question transition's effects.
    ///
    /// The answer is enqueued once, here. The run machine declares the same
    /// delivery from its own side when it resumes, and enqueueing on both would
    /// hand the worker its answer twice.
    fn perform_question_effects(
        &self,
        outcome: &TransitionOutcome<agens_store::QuestionState, QuestionEffect>,
        question: &QuestionRow,
        answer: &str,
    ) -> Result<(), ApiError> {
        let Some(applied) = outcome.applied() else {
            return Ok(());
        };

        if applied
            .effects
            .contains(&QuestionEffect::EnqueueForDelivery)
        {
            let question_id = question
                .id
                .ok_or_else(|| ApiError::Storage("an answered question has no id".to_owned()))?;

            self.ports.delivery.enqueue(&Delivery::new(
                question.run_id,
                DeliveryPayload::Answer {
                    question_id,
                    text: answer.to_owned(),
                },
            ))?;
        }

        Ok(())
    }

    /// Moves the run back to the queue when it was parked on this question.
    ///
    /// A question that blocked nothing moves nothing, so the run machine is
    /// only asked when the run is actually waiting.
    fn resume_run_waiting_on(
        &mut self,
        question: &QuestionRow,
        question_id: i64,
        now: i64,
    ) -> Result<Option<TransitionOutcome<RunState, RunEffect>>, ApiError> {
        let run = self.load_run(question.run_id)?;

        if run.state != RunState::AwaitingInput {
            return Ok(None);
        }

        let outcome = self.machines.apply_run(
            question.run_id,
            RunTrigger::Answered,
            &RunFacts {
                now,
                principal: Principal::Coordinator,
                answered_question_id: Some(question_id),
                ..RunFacts::default()
            },
        )?;

        self.announce_queued(&outcome, question.run_id);

        Ok(Some(outcome))
    }

    /// The author an answer is recorded under.
    ///
    /// The coordinator is not one: it reaches no Team operation, and an answer
    /// with no human or managerial author behind it is exactly what the
    /// question machine refuses to record.
    fn author_for(
        &mut self,
        operation: Operation,
        principal: Principal,
        run_id: Option<i64>,
        now: i64,
    ) -> Result<QuestionAuthor, ApiError> {
        match principal {
            Principal::User => Ok(QuestionAuthor::User),
            Principal::Praetor => Ok(QuestionAuthor::Praetor),
            Principal::Coordinator => Err(self.refuse(
                operation,
                principal,
                run_id,
                now,
                "nothing is answered anonymously, and the coordinator is not an author".to_owned(),
            )),
        }
    }

    fn load_run(&self, run_id: i64) -> Result<RunRow, ApiError> {
        self.machines
            .store()
            .load_run(run_id)?
            .ok_or(ApiError::NotFound {
                subject: "run",
                id: run_id,
            })
    }

    fn load_question(&self, question_id: i64) -> Result<QuestionRow, ApiError> {
        self.machines
            .store()
            .load_question(question_id)?
            .ok_or(ApiError::NotFound {
                subject: "question",
                id: question_id,
            })
    }
}
