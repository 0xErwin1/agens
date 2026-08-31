//! The Feed plane: what the control plane looks like from outside.
//!
//! Nothing here writes. Every view is a projection of rows the state machines
//! already wrote, so a reader cannot see a state that never happened and cannot
//! cause one either.
//!
//! Every listing is scoped by repository. One daemon serves N projects, so a
//! view without a repository would hand a client another project's runs.

use agens_store::{
    AttemptRow, EventRow, FindingRow, QuestionKind, QuestionRow, QuestionState, RunHealthRow,
    RunRow, RunState, WorktreeStatus,
};

use super::{ApiCore, ApiError, EventFilter, Operation, Subscription};
use crate::fsm::Principal;

/// One run as the tree shows it.
///
/// Identity on this plane is the project and the branch, never the path: the
/// worktree path is a copyable datum of the detail view, not how a run is
/// recognized.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunSummary {
    pub run_id: i64,
    pub task: String,
    pub state: RunState,
    pub priority: i64,
    pub provider: String,
    pub worktree_status: Option<WorktreeStatus>,
    pub parent_run_id: Option<i64>,
    /// Provenance of an imported task, never followed back to its source.
    pub external_ref: Option<String>,
    pub created_at: i64,
}

impl RunSummary {
    fn of(run: &RunRow) -> Result<Self, ApiError> {
        Ok(Self {
            run_id: run
                .id
                .ok_or_else(|| ApiError::Storage("a stored run has no id".to_owned()))?,
            task: run.task.clone(),
            state: run.state,
            priority: run.priority,
            provider: run.provider.clone(),
            worktree_status: run.worktree_status,
            parent_run_id: run.parent_run_id,
            external_ref: run.external_ref.clone(),
            created_at: run.created_at,
        })
    }
}

/// Every run of one repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeSnapshot {
    pub repo_id: String,
    pub runs: Vec<RunSummary>,
}

/// One run in full: what it is, every try at it, what it is blocked on, what it
/// claimed, and what the journal says happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunView {
    pub run: RunRow,
    pub attempts: Vec<AttemptRow>,
    pub questions: Vec<QuestionRow>,
    pub findings: Vec<FindingRow>,
    pub events: Vec<EventRow>,
    /// Derived signals, absent until ingest has something to derive them from.
    pub health: Option<RunHealthRow>,
}

/// One thing waiting on a person.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboxItem {
    pub run_id: i64,
    pub question_id: i64,
    pub kind: QuestionKind,
    pub blocked_decision: String,
    /// JSON array of the options offered.
    pub options: String,
    pub recommendation: Option<String>,
    pub expires_at: Option<i64>,
    pub created_at: i64,
}

/// Everything open in one repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboxView {
    pub repo_id: String,
    pub items: Vec<InboxItem>,
}

impl ApiCore {
    /// Every repository this daemon holds runs for, in repository-id order.
    ///
    /// The one unscoped listing on this plane: it names repositories rather
    /// than runs, so the operator's fleet board can enumerate what the daemon
    /// actually hosts and then read each repository through [`Self::tree`].
    /// It shares the tree's authorization because it is the tree's preflight.
    pub fn repos(&mut self, principal: Principal, now: i64) -> Result<Vec<String>, ApiError> {
        self.authorize(Operation::Tree, principal, None, now)?;

        Ok(self.machines.store().run_repo_ids()?)
    }

    /// Every run of one repository.
    pub fn tree(
        &mut self,
        principal: Principal,
        repo_id: &str,
        now: i64,
    ) -> Result<TreeSnapshot, ApiError> {
        self.authorize(Operation::Tree, principal, None, now)?;

        let runs = self
            .machines
            .store()
            .runs_for_repo(repo_id)?
            .iter()
            .map(RunSummary::of)
            .collect::<Result<Vec<_>, _>>()?;

        Ok(TreeSnapshot {
            repo_id: repo_id.to_owned(),
            runs,
        })
    }

    /// One run and everything recorded about it.
    pub fn run_detail(
        &mut self,
        principal: Principal,
        run_id: i64,
        now: i64,
    ) -> Result<RunView, ApiError> {
        self.authorize(Operation::RunDetail, principal, Some(run_id), now)?;

        let store = self.machines.store();
        let run = store.load_run(run_id)?.ok_or(ApiError::NotFound {
            subject: "run",
            id: run_id,
        })?;

        Ok(RunView {
            run,
            attempts: store.attempts_for_run(run_id)?,
            questions: store.questions_for_run(run_id)?,
            findings: store.findings_for_run(run_id)?,
            events: store.events_for_run(run_id)?,
            health: store.load_run_health(run_id)?,
        })
    }

    /// Everything of one repository still waiting on an answer.
    ///
    /// Only `open` questions are listed. An expired authorization is not
    /// waiting on anybody — silence never authorizes, so it has to be asked for
    /// again rather than sat in an inbox looking answerable.
    pub fn inbox(
        &mut self,
        principal: Principal,
        repo_id: &str,
        now: i64,
    ) -> Result<InboxView, ApiError> {
        self.authorize(Operation::Inbox, principal, None, now)?;

        let store = self.machines.store();
        let mut items = Vec::new();

        for run in store.runs_for_repo(repo_id)? {
            let Some(run_id) = run.id else {
                continue;
            };

            for question in store.questions_for_run(run_id)? {
                if question.state != QuestionState::Open {
                    continue;
                }

                let Some(question_id) = question.id else {
                    continue;
                };

                items.push(InboxItem {
                    run_id,
                    question_id,
                    kind: question.kind,
                    blocked_decision: question.blocked_decision,
                    options: question.options,
                    recommendation: question.recommendation,
                    expires_at: question.expires_at,
                    created_at: question.created_at,
                });
            }
        }

        Ok(InboxView {
            repo_id: repo_id.to_owned(),
            items,
        })
    }

    /// A live stream of the journal.
    pub fn subscribe(
        &mut self,
        principal: Principal,
        filter: &EventFilter,
        now: i64,
    ) -> Result<Subscription, ApiError> {
        self.authorize(Operation::Subscribe, principal, filter.run_id, now)?;

        Ok(self.ports.feed.subscribe(filter)?)
    }
}
