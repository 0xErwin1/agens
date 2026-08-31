//! Praetor's facade onto the service core.
//!
//! This is the implementation half of [`agens_core::coordination`]: the tools
//! crate collects a typed request and knows nothing about the control plane,
//! and this module carries it out through the same [`ApiCore`] the gRPC surface
//! uses. The two differ in exactly two things, the transport and the
//! [`Principal`], which is the whole reason the authorization table lives in
//! the core rather than in either of them.
//!
//! The principal is pinned here and read from nothing. A facade that could name
//! its own would be able to claim the user's authority, and the one thing the
//! table gives the user alone is landing code.
//!
//! Two facts about a request are held rather than taken from it. The repository
//! is the one this session manages, so a manager cannot reach another project's
//! runs; the start point and the provider are the coordinator's, so a manager
//! cannot decide where a branch begins. Neither is a restriction the tool layer
//! promises — there is no argument for either.
//!
//! Nothing here reads a clock. The timestamp comes from the clock the caller
//! installed, the same discipline the machines and the store keep.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agens_core::coordination::{
    AnswerReceipt, AnswerRequest, CancelRequest, CoordinationError, CoordinationPort,
    DirectReceipt, DirectRequest, EscalateReceipt, EscalateRequest, MergeRequest,
    MergeRequestReceipt, ReclaimReceipt, ReclaimRequest, ReportRequest, RetryRequest, RunReport,
    RunStateReceipt, SpawnReceipt, SpawnRequest, TeamAttempt, TeamFinding, TeamHealth,
    TeamQuestion, TeamRun, TeamStatus,
};
use agens_core::run_introspection::{Ask, AskOption};
use agens_store::{AttemptRow, FindingRow, QuestionRow, RunHealthRow, RunRow, RunState};

use crate::api::{
    ApiCore, ApiError, CleaningAction, CleaningDisposition, CreateRun, Direction, Escalation,
    RequestMerge, RunRef,
};
use crate::fsm::Principal;
use crate::introspection::Clock;

/// What a run spawned through this facade is created with, beyond what the
/// request carries.
///
/// Held rather than taken per call, because each of these is a decision about
/// the machine rather than about the task: which checkout the team works in,
/// which provider its workers speak through, where a branch begins, and how
/// many chargeable attempts a retry may spend.
#[derive(Clone, Debug)]
pub struct TeamBinding {
    pub repository: PathBuf,
    pub repo_id: String,
    pub provider: String,
    pub start_point: String,
    pub retry_budget: i64,
}

/// One team's control plane, as its manager reaches it.
pub struct TeamCoordination {
    core: Arc<Mutex<ApiCore>>,
    binding: TeamBinding,
    clock: Clock,
}

impl TeamCoordination {
    #[must_use]
    pub const fn new(core: Arc<Mutex<ApiCore>>, binding: TeamBinding, clock: Clock) -> Self {
        Self {
            core,
            binding,
            clock,
        }
    }

    /// The core, or the refusal a poisoned lock is.
    ///
    /// A poisoned lock means a previous operation panicked while holding the
    /// core, so the invariants the control plane rests on were established by
    /// code that did not finish. Refusing is the only honest answer.
    fn core(&self) -> Result<std::sync::MutexGuard<'_, ApiCore>, CoordinationError> {
        self.core.lock().map_err(|_| CoordinationError::Unavailable)
    }

    fn now(&self) -> i64 {
        (self.clock)()
    }

    /// The state a run is in right now, for a request that moved nothing.
    fn run_state(&self, run_id: i64) -> Result<String, CoordinationError> {
        let core = self.core()?;
        let run = core
            .machines()
            .store()
            .load_run(run_id)
            .map_err(|error| CoordinationError::Refused(error.to_string()))?
            .ok_or_else(|| CoordinationError::NotFound(format!("no run with id {run_id}")))?;

        Ok(run.state.as_str().to_owned())
    }

    /// The disposition a run's worktree is in right now, for a request that
    /// moved nothing.
    fn worktree_status(&self, run_id: i64) -> Result<String, CoordinationError> {
        let core = self.core()?;
        let run = core
            .machines()
            .store()
            .load_run(run_id)
            .map_err(|error| CoordinationError::Refused(error.to_string()))?
            .ok_or_else(|| CoordinationError::NotFound(format!("no run with id {run_id}")))?;

        Ok(run
            .worktree_status
            .map_or_else(|| "none".to_owned(), |status| status.as_str().to_owned()))
    }
}

impl CoordinationPort for TeamCoordination {
    fn status(&mut self) -> Result<TeamStatus, CoordinationError> {
        let now = self.now();
        let mut core = self.core()?;

        let tree = core.tree(Principal::Praetor, &self.binding.repo_id, now)?;
        let inbox = core.inbox(Principal::Praetor, &self.binding.repo_id, now)?;

        Ok(TeamStatus {
            repo_id: tree.repo_id,
            runs: tree
                .runs
                .into_iter()
                .map(|run| TeamRun {
                    run_id: run.run_id,
                    task: run.task,
                    state: run.state.as_str().to_owned(),
                    priority: run.priority,
                    worktree_status: run.worktree_status.map(|status| status.as_str().to_owned()),
                    parent_run_id: run.parent_run_id,
                    created_at: run.created_at,
                })
                .collect(),
            open_questions: inbox
                .items
                .into_iter()
                .map(|item| TeamQuestion {
                    question_id: item.question_id,
                    run_id: item.run_id,
                    kind: item.kind.as_str().to_owned(),
                    blocked_decision: item.blocked_decision,
                    options: parse_options(&item.options),
                    recommendation: item.recommendation,
                    expires_at: item.expires_at,
                })
                .collect(),
        })
    }

    fn report(&mut self, request: &ReportRequest) -> Result<RunReport, CoordinationError> {
        let now = self.now();
        let mut core = self.core()?;
        let view = core.run_detail(Principal::Praetor, request.run_id(), now)?;

        Ok(RunReport {
            run: team_run(&view.run, request.run_id()),
            scope: view.run.scope,
            dod: view.run.dod,
            provider: view.run.provider,
            result: view.run.result,
            attempts: view.attempts.iter().map(team_attempt).collect(),
            questions: view.questions.iter().map(team_question).collect(),
            findings: view.findings.iter().map(team_finding).collect(),
            health: view.health.as_ref().map(team_health),
        })
    }

    fn answer(&mut self, request: &AnswerRequest) -> Result<AnswerReceipt, CoordinationError> {
        let now = self.now();
        let mut core = self.core()?;

        let answered = core.answer_question(
            Principal::Praetor,
            &crate::api::AnswerQuestion {
                question_id: request.question_id(),
                answer: request.answer().to_owned(),
                now,
            },
        )?;

        Ok(AnswerReceipt {
            question_id: request.question_id(),
            run_id: answered.run_id,
            run_resumed: answered
                .run
                .as_ref()
                .is_some_and(|outcome| outcome.applied().is_some()),
        })
    }

    fn escalate(
        &mut self,
        request: &EscalateRequest,
    ) -> Result<EscalateReceipt, CoordinationError> {
        let now = self.now();
        let mut core = self.core()?;

        let question_id = core.escalate(
            Principal::Praetor,
            &Escalation {
                run_id: request.run_id(),
                blocked_decision: request.question().blocked_decision().to_owned(),
                options: encode_options(request.question()),
                recommendation: request.question().recommendation().map(str::to_owned),
                now,
            },
        )?;

        Ok(EscalateReceipt {
            question_id,
            run_id: request.run_id(),
        })
    }

    fn direct(&mut self, request: &DirectRequest) -> Result<DirectReceipt, CoordinationError> {
        let now = self.now();
        let mut core = self.core()?;

        core.direct(
            Principal::Praetor,
            &Direction {
                run_id: request.run_id(),
                directive: request.directive().to_owned(),
                now,
            },
        )?;

        Ok(DirectReceipt {
            run_id: request.run_id(),
        })
    }

    fn cancel(&mut self, request: &CancelRequest) -> Result<RunStateReceipt, CoordinationError> {
        let now = self.now();
        let outcome = {
            let mut core = self.core()?;

            core.cancel_run(
                Principal::Praetor,
                &RunRef {
                    run_id: request.run_id(),
                    now,
                },
            )?
        };

        match outcome.applied() {
            Some(applied) => Ok(RunStateReceipt {
                run_id: request.run_id(),
                state: applied.to.as_str().to_owned(),
                moved: true,
            }),
            None => Ok(RunStateReceipt {
                run_id: request.run_id(),
                state: self.run_state(request.run_id())?,
                moved: false,
            }),
        }
    }

    /// Proposes a run, releasing the core across the provisioning it implies.
    ///
    /// The core is taken twice — once to decide, once to write the row — and is
    /// released for the step between them, which creates the worktree and runs
    /// whatever the repository declared. That step is bounded by the
    /// repository's own timeouts and may take minutes, and every other request
    /// waits on this same lock.
    fn spawn(&mut self, request: &SpawnRequest) -> Result<SpawnReceipt, CoordinationError> {
        let create = CreateRun {
            repo_root: self.binding.repository.clone(),
            task: request.task().to_owned(),
            scope: request.scope().to_owned(),
            dod: request.dod().to_owned(),
            external_ref: None,
            parent_run_id: request.parent_run_id(),
            dep_run_id: request.dep_run_id(),
            provider: self.binding.provider.clone(),
            priority: request.priority(),
            budget_tokens: None,
            start_point: self.binding.start_point.clone(),
            now: self.now(),
        };

        let (prepared, worktrees) = {
            let mut core = self.core()?;
            let prepared = core.prepare_run(Principal::Praetor, &create)?;
            let worktrees = Arc::clone(&core.ports().worktrees);

            (prepared, worktrees)
        };

        let provisioned = worktrees
            .provision(&prepared.worktree_request(&create.start_point))
            .map_err(ApiError::Port)?;

        let created = {
            let mut core = self.core()?;

            core.open_run(&create, &prepared, provisioned)?
        };

        Ok(SpawnReceipt {
            run_id: created.run_id,
            // Read from the machine's own vocabulary rather than written as a
            // literal: a creation lands in draft, and if that ever stopped
            // being true the receipt would still say what happened.
            state: RunState::Draft.as_str().to_owned(),
        })
    }

    fn retry(&mut self, request: &RetryRequest) -> Result<RunStateReceipt, CoordinationError> {
        let now = self.now();
        let outcome = {
            let mut core = self.core()?;

            core.retry(
                Principal::Praetor,
                &crate::api::RetryRequest {
                    run_id: request.run_id(),
                    guidance: request.guidance().to_owned(),
                    retry_budget: self.binding.retry_budget,
                    now,
                },
            )?
        };

        match outcome.applied() {
            Some(applied) => Ok(RunStateReceipt {
                run_id: request.run_id(),
                state: applied.to.as_str().to_owned(),
                moved: true,
            }),
            None => Ok(RunStateReceipt {
                run_id: request.run_id(),
                state: self.run_state(request.run_id())?,
                moved: false,
            }),
        }
    }

    fn request_merge(
        &mut self,
        request: &MergeRequest,
    ) -> Result<MergeRequestReceipt, CoordinationError> {
        let now = self.now();
        let mut core = self.core()?;

        let opened = core.request_merge(
            Principal::Praetor,
            &RequestMerge {
                run_id: request.run_id(),
                reason: Some(request.reason().to_owned()),
                expires_at: None,
                now,
            },
        )?;

        Ok(MergeRequestReceipt {
            question_id: opened.question_id,
            run_id: opened.run_id,
            tree_hash: opened.tree_hash,
            paths_digest: opened.paths_digest,
        })
    }

    fn request_reclaim(
        &mut self,
        request: &ReclaimRequest,
    ) -> Result<ReclaimReceipt, CoordinationError> {
        let now = self.now();
        let mut core = self.core()?;

        let outcome = core.cleaning(
            Principal::Praetor,
            &CleaningAction {
                run_id: request.run_id(),
                disposition: CleaningDisposition::Reclaim,
                // Never set here. A confirmed disposition is a person relaying
                // their own confirmation, and this facade is not a person.
                confirmed: false,
                now,
            },
        )?;

        match outcome.applied() {
            Some(applied) => Ok(ReclaimReceipt {
                run_id: request.run_id(),
                worktree_status: applied.to.as_str().to_owned(),
                moved: true,
            }),
            None => Ok(ReclaimReceipt {
                run_id: request.run_id(),
                worktree_status: self.worktree_status(request.run_id())?,
                moved: false,
            }),
        }
    }
}

impl From<ApiError> for CoordinationError {
    fn from(error: ApiError) -> Self {
        match &error {
            ApiError::Unauthorized { .. } => Self::Unauthorized(error.to_string()),
            ApiError::NotFound { .. } => Self::NotFound(error.to_string()),
            ApiError::Rejected(_) | ApiError::Port(_) | ApiError::Storage(_) => {
                Self::Refused(error.to_string())
            }
        }
    }
}

fn team_run(run: &RunRow, run_id: i64) -> TeamRun {
    TeamRun {
        run_id,
        task: run.task.clone(),
        state: run.state.as_str().to_owned(),
        priority: run.priority,
        worktree_status: run.worktree_status.map(|status| status.as_str().to_owned()),
        parent_run_id: run.parent_run_id,
        created_at: run.created_at,
    }
}

fn team_attempt(attempt: &AttemptRow) -> TeamAttempt {
    TeamAttempt {
        attempt: attempt.n,
        outcome: attempt.outcome.map(|outcome| outcome.as_str().to_owned()),
        retry_trigger: attempt
            .retry_trigger
            .map(|trigger| trigger.as_str().to_owned()),
        started_at: attempt.started_at,
        ended_at: attempt.ended_at,
    }
}

fn team_question(question: &QuestionRow) -> TeamQuestion {
    TeamQuestion {
        question_id: question.id.unwrap_or_default(),
        run_id: question.run_id,
        kind: question.kind.as_str().to_owned(),
        blocked_decision: question.blocked_decision.clone(),
        options: parse_options(&question.options),
        recommendation: question.recommendation.clone(),
        expires_at: question.expires_at,
    }
}

fn team_finding(finding: &FindingRow) -> TeamFinding {
    TeamFinding {
        description: finding.description.clone(),
        evidence_class: finding.evidence_class.as_str().to_owned(),
        causal_disposition: finding.causal_disposition.as_str().to_owned(),
        created_at: finding.created_at,
    }
}

fn team_health(health: &RunHealthRow) -> TeamHealth {
    TeamHealth {
        noop_turns: health.noop_turns,
        last_progress_turn: health.last_progress_turn,
        tokens_since_progress: health.tokens_since_progress,
    }
}

/// The options a question offered, in whichever of the two shapes the column
/// holds.
///
/// A worker's `ask` writes `{id, label, consequence}` objects; a client may
/// write bare strings. An option in neither shape is dropped rather than
/// guessed at: what the identifier would be is exactly what the answer is
/// checked against.
fn parse_options(stored: &str) -> Vec<AskOption> {
    let Ok(serde_json::Value::Array(options)) = serde_json::from_str::<serde_json::Value>(stored)
    else {
        return Vec::new();
    };

    options
        .iter()
        .filter_map(|option| match option {
            serde_json::Value::String(id) => Some(AskOption::new(id.clone(), id.clone(), None)),
            serde_json::Value::Object(fields) => {
                let id = fields.get("id").and_then(serde_json::Value::as_str)?;
                let label = fields
                    .get("label")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(id);

                Some(AskOption::new(
                    id.to_owned(),
                    label.to_owned(),
                    fields
                        .get("consequence")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                ))
            }
            _ => None,
        })
        .collect()
}

/// The options an escalation carries, in the shape a worker's question is
/// already stored in, so a client renders both the same way and the
/// detail-question policy reads both the same way.
fn encode_options(question: &Ask) -> String {
    let options: Vec<serde_json::Value> = question
        .options()
        .iter()
        .map(|option| {
            serde_json::json!({
                "id": option.id(),
                "label": option.label(),
                "consequence": option.consequence(),
            })
        })
        .collect();

    serde_json::Value::Array(options).to_string()
}
