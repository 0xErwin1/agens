//! The coordinator's git gates: what has to be true before work lands, and
//! before a worktree is let go.
//!
//! Both gates re-derive the topology from git at the moment they run. Nothing
//! here reads a stored flag and nothing here trusts a caller's claim about the
//! repository, because the whole point of the gate is to be the one place that
//! looks. A branch that existed when the run finished may be gone, a main that
//! was an ancestor may have moved, and a worktree that was clean may have
//! picked up a stray file since.
//!
//! **The approval authorizes bytes, not a run.** A merge authorization is a
//! question of kind `approval` carrying a receipt frozen when it was created:
//! the worktree's `HEAD^{tree}` and a digest of the paths its branch touches.
//! The gate re-derives both and compares. Without that, a worker could commit
//! anything at all between the authorization and the merge and nothing would
//! notice. The cost of it is a real one and is meant to be visible: any commit
//! after the approval, a formatting pass included, makes the receipt stale, so
//! the authorization has to be asked for with the worktree already still.
//!
//! The tree hash carries the whole of that comparison, and the paths digest is
//! checked only while the branch has yet to land; [`receipt_holds`] says why.
//!
//! A stale receipt refuses the gate and leaves the approval where it is. The
//! authorization is bound to the frozen bytes, so it cannot authorize the new
//! tree no matter how long it sits there, and moving it would mean inventing an
//! expiry the user never set. Asking again is the caller's, and
//! [`GateRefusal::ReceiptStale`] carries both sides of the comparison so it can
//! say why.
//!
//! Two ways a merge is reached, and the gate backs both: the coordinator
//! integrates ([`MergePath::Integrate`]), or the user attests the work already
//! landed and the coordinator verifies it without running a merge
//! ([`MergePath::Attested`]).
//!
//! What the gate does **not** do is invoke a sub-agent. A merge that does not
//! apply, and a worktree that is dirty when the reclaim sweep reaches it, both
//! leave as a typed [`SubAgentRequest`] for the caller to act on. The
//! coordinator is deterministic and never invokes a model.

use std::path::Path;

use agens_store::{EventClass, EventRow, QuestionAuthor, QuestionKind, QuestionState, RunRow};
use agens_tools::{GateDerivation, MergeOutcome, SessionWorktrees, WorktreeError};
use sha2::{Digest, Sha256};

use crate::fsm::{
    AppliedWorktreeTransition, StateMachines, TransitionOutcome, TransitionRejection,
    WorktreeFacts, WorktreeTrigger,
};

/// Which of the two doors a merge came through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergePath {
    /// The coordinator integrates the branch itself.
    Integrate,
    /// The user attests the branch already landed. The gate verifies the claim
    /// against git and never runs a merge of its own.
    Attested,
}

impl MergePath {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Integrate => "integrate",
            Self::Attested => "attested",
        }
    }
}

/// One pre-merge gate run.
#[derive(Clone, Debug)]
pub struct PreMergeRequest {
    pub run_id: i64,
    /// The `approval` question this merge is claimed to be authorized by.
    pub approval_id: i64,
    pub path: MergePath,
    /// The branch the work is measured against, as it stands now.
    pub main_ref: String,
    /// How many attempts this run's budget allows. It is configuration, so it
    /// arrives with the request rather than being read from the store.
    pub attempt_cap: i64,
    /// Epoch seconds.
    pub now: i64,
}

/// One reclaim sweep over a single run's worktree.
#[derive(Clone, Debug)]
pub struct ReclaimRequest {
    pub run_id: i64,
    pub main_ref: String,
    /// Epoch seconds.
    pub now: i64,
}

/// The receipt an approval is bound to: the exact bytes the user authorized.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receipt {
    /// `HEAD^{tree}` of the worktree at the moment the approval was created.
    pub tree_hash: String,
    pub paths_digest: String,
}

/// Work the coordinator is not allowed to do itself, handed back for the caller
/// to route to a sub-agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubAgentRequest {
    pub kind: SubAgentKind,
    pub run_id: i64,
    pub worktree_path: String,
    pub branch: Option<String>,
    /// What git reported, or what was found in the way.
    pub detail: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubAgentKind {
    /// The merge did not apply and was rolled back.
    Integration,
    /// The worktree still holds uncommitted work.
    Cleanup,
}

impl SubAgentKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Integration => "integration",
            Self::Cleanup => "cleanup",
        }
    }
}

/// How a pre-merge gate ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreMergeVerdict {
    Merged {
        /// The merge commit, or `None` when the branch had already landed and
        /// nothing was executed.
        commit: Option<String>,
        /// `None` when the worktree was already past `active`, so a gate that
        /// runs twice reports the same verdict rather than failing the second
        /// time.
        worktree: Option<AppliedWorktreeTransition>,
    },
    IntegrationRequired(SubAgentRequest),
    Refused(GateRefusal),
}

/// How a reclaim sweep ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReclaimVerdict {
    Released(AppliedWorktreeTransition),
    CleanupRequired(SubAgentRequest),
    Refused(GateRefusal),
}

/// Why a gate refused. Nothing was merged and no worktree moved in any of these
/// cases, and each one is journaled as a `gate_result`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GateRefusal {
    /// The run has no worktree recorded, so there is nothing to derive from.
    NoWorktree,
    /// The worktree is on a detached head: no branch to merge, none to reclaim.
    DetachedHead,
    /// The worktree and the target share no history, so there is no merge base.
    UnrelatedHistory,
    WorktreeDirty,
    /// No approval with that id, or it belongs to another run, or it is a plain
    /// question.
    ApprovalMissing,
    /// It exists but authorizes nothing: never answered, already consumed, or
    /// answered by someone other than the user.
    NotAuthorized {
        state: &'static str,
    },
    ApprovalExpired {
        expired_at: i64,
    },
    /// The approval carries no receipt at all. An approval without one
    /// authorizes a run rather than bytes, which is the thing the receipt
    /// exists to prevent.
    ReceiptMissing,
    /// The worktree moved after the authorization. The approval stands, bound
    /// to bytes that are no longer there, and a new one has to be asked for.
    ReceiptStale {
        frozen: Receipt,
        derived: Receipt,
    },
    AttemptsExhausted {
        charged: i64,
        cap: i64,
    },
    /// The diff reaches outside the frozen genesis paths.
    OutsideGenesisPaths {
        paths: Vec<String>,
    },
    /// The attestation says the work landed and git says it did not.
    NotMerged,
}

impl GateRefusal {
    /// The stable name this refusal is journaled under.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::NoWorktree => "no_worktree",
            Self::DetachedHead => "detached_head",
            Self::UnrelatedHistory => "unrelated_history",
            Self::WorktreeDirty => "worktree_dirty",
            Self::ApprovalMissing => "approval_missing",
            Self::NotAuthorized { .. } => "not_authorized",
            Self::ApprovalExpired { .. } => "approval_expired",
            Self::ReceiptMissing => "receipt_missing",
            Self::ReceiptStale { .. } => "receipt_stale",
            Self::AttemptsExhausted { .. } => "attempts_exhausted",
            Self::OutsideGenesisPaths { .. } => "outside_genesis_paths",
            Self::NotMerged => "not_merged",
        }
    }
}

/// A gate that could not reach a verdict at all, as opposed to one that reached
/// a refusal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GateError {
    /// The run the gate was asked about does not exist.
    NoSuchRun(i64),
    /// Git could not be reached, timed out, or refused an invocation.
    Derivation(WorktreeError),
    /// The store refused, or the row moved under the gate.
    Transition(TransitionRejection),
    /// A stored value could not be read back in the shape it was written.
    Malformed(String),
}

impl std::fmt::Display for GateError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSuchRun(id) => write!(formatter, "no run with id {id}"),
            Self::Derivation(error) => write!(formatter, "git derivation failed: {error}"),
            Self::Transition(rejection) => write!(formatter, "{rejection}"),
            Self::Malformed(detail) => write!(formatter, "{detail}"),
        }
    }
}

impl std::error::Error for GateError {}

impl From<WorktreeError> for GateError {
    fn from(error: WorktreeError) -> Self {
        Self::Derivation(error)
    }
}

impl From<TransitionRejection> for GateError {
    fn from(rejection: TransitionRejection) -> Self {
        Self::Transition(rejection)
    }
}

/// The two git gates over one control plane.
///
/// It holds the state machines rather than a store handle of its own: a gate
/// both journals its verdict and moves a worktree, and routing both through the
/// machines keeps them the only writer of the control-plane tables.
pub struct Gates {
    machines: StateMachines,
    worktrees: SessionWorktrees,
}

impl Gates {
    #[must_use]
    pub const fn new(machines: StateMachines, worktrees: SessionWorktrees) -> Self {
        Self {
            machines,
            worktrees,
        }
    }

    /// The state machines, for a caller that also has transitions of its own to
    /// apply.
    #[must_use]
    pub const fn machines(&self) -> &StateMachines {
        &self.machines
    }

    #[must_use]
    pub const fn machines_mut(&mut self) -> &mut StateMachines {
        &mut self.machines
    }

    /// Freezes the receipt an approval is bound to.
    ///
    /// The gate re-derives through the same code path, so the two sides of the
    /// comparison cannot drift into disagreeing about what a digest of the same
    /// worktree is.
    pub fn freeze_receipt(&self, run_id: i64, main_ref: &str) -> Result<Receipt, GateError> {
        let run = self.load_run(run_id)?;
        let worktree = worktree_path(&run).ok_or(GateError::Malformed(format!(
            "run {run_id} has no worktree to freeze a receipt from"
        )))?;

        freeze_receipt(&self.worktrees, Path::new(worktree), main_ref)
    }

    /// Runs the pre-merge gate, and integrates only if every rule holds.
    ///
    /// The order is the one the rules depend on: the topology is re-derived
    /// first because everything after it is a comparison against what git says
    /// now, the authorization is checked against that derivation, the
    /// transaction is checked against the frozen scope, and only then does
    /// anything land. `gate_result` and `merged` are journaled before the
    /// worktree becomes reclaimable, so a subscriber never sees a worktree
    /// released without the verdict that released it.
    pub fn pre_merge(&mut self, request: &PreMergeRequest) -> Result<PreMergeVerdict, GateError> {
        let run = self.load_run(request.run_id)?;

        let Some(worktree) = worktree_path(&run).map(ToOwned::to_owned) else {
            return self.refuse_pre_merge(request, None, GateRefusal::NoWorktree);
        };

        let derivation = self
            .worktrees
            .derive(Path::new(&worktree), &request.main_ref)?;

        if let Some(refusal) = topology_refusal(&derivation) {
            return self.refuse_pre_merge(request, Some(&derivation), refusal);
        }
        if let Some(refusal) = self.authorization_refusal(request, &derivation)? {
            return self.refuse_pre_merge(request, Some(&derivation), refusal);
        }
        if let Some(refusal) = self.transaction_refusal(request, &run, &derivation)? {
            return self.refuse_pre_merge(request, Some(&derivation), refusal);
        }

        let branch = derivation.branch.clone().unwrap_or_default();
        let commit = match self.integrate(request, &run, &derivation, &branch)? {
            Integration::Landed(commit) => commit,
            Integration::Refused(verdict) => return Ok(verdict),
        };

        self.journal_gate_result(
            request.run_id,
            request.now,
            &gate_payload(request, Some(&derivation), None),
        )?;
        self.journal_merged(request, &branch, commit.as_deref())?;

        let worktree = self.release_worktree(request.run_id, request.now, &derivation)?;

        Ok(PreMergeVerdict::Merged { commit, worktree })
    }

    /// Sweeps one run's worktree, releasing it only when git says here and now
    /// that its branch landed and that nothing uncommitted is left to lose.
    pub fn reclaim(&mut self, request: &ReclaimRequest) -> Result<ReclaimVerdict, GateError> {
        let run = self.load_run(request.run_id)?;

        let Some(worktree) = worktree_path(&run).map(ToOwned::to_owned) else {
            self.journal_reclaim_result(request, None, Some(&GateRefusal::NoWorktree))?;
            return Ok(ReclaimVerdict::Refused(GateRefusal::NoWorktree));
        };

        let derivation = self
            .worktrees
            .derive(Path::new(&worktree), &request.main_ref)?;

        if derivation.branch.is_none() {
            self.journal_reclaim_result(
                request,
                Some(&derivation),
                Some(&GateRefusal::DetachedHead),
            )?;
            return Ok(ReclaimVerdict::Refused(GateRefusal::DetachedHead));
        }
        if !derivation.merged {
            self.journal_reclaim_result(request, Some(&derivation), Some(&GateRefusal::NotMerged))?;
            return Ok(ReclaimVerdict::Refused(GateRefusal::NotMerged));
        }

        if derivation.dirty {
            self.journal_reclaim_result(
                request,
                Some(&derivation),
                Some(&GateRefusal::WorktreeDirty),
            )?;

            return Ok(ReclaimVerdict::CleanupRequired(SubAgentRequest {
                kind: SubAgentKind::Cleanup,
                run_id: request.run_id,
                worktree_path: worktree,
                branch: derivation.branch.clone(),
                detail: "the worktree holds uncommitted changes".to_owned(),
            }));
        }

        self.journal_reclaim_result(request, Some(&derivation), None)?;

        let applied = self
            .release_worktree(request.run_id, request.now, &derivation)?
            .ok_or_else(|| {
                GateError::Transition(TransitionRejection::NoSuchTransition {
                    machine: "worktree",
                    from: "not active",
                    trigger: WorktreeTrigger::MergeDetected.as_str(),
                })
            })?;

        Ok(ReclaimVerdict::Released(applied))
    }

    /// Executes the merge, or reports why nothing was executed.
    fn integrate(
        &mut self,
        request: &PreMergeRequest,
        run: &RunRow,
        derivation: &GateDerivation,
        branch: &str,
    ) -> Result<Integration, GateError> {
        if derivation.merged {
            return Ok(Integration::Landed(None));
        }

        if request.path == MergePath::Attested {
            let verdict =
                self.refuse_pre_merge(request, Some(derivation), GateRefusal::NotMerged)?;
            return Ok(Integration::Refused(verdict));
        }

        match self.worktrees.merge(Path::new(&run.repo_root), branch)? {
            MergeOutcome::Merged { commit } => Ok(Integration::Landed(Some(commit))),
            MergeOutcome::Conflicted { detail } => {
                self.journal_gate_result(
                    request.run_id,
                    request.now,
                    &gate_payload(request, Some(derivation), Some("integration_required")),
                )?;

                Ok(Integration::Refused(PreMergeVerdict::IntegrationRequired(
                    SubAgentRequest {
                        kind: SubAgentKind::Integration,
                        run_id: request.run_id,
                        worktree_path: worktree_path(run).unwrap_or_default().to_owned(),
                        branch: Some(branch.to_owned()),
                        detail,
                    },
                )))
            }
        }
    }

    /// Whether the authorization presented is the user's, still in date, and
    /// still bound to the bytes in front of the gate.
    fn authorization_refusal(
        &self,
        request: &PreMergeRequest,
        derivation: &GateDerivation,
    ) -> Result<Option<GateRefusal>, GateError> {
        let question = self
            .machines
            .store()
            .load_question(request.approval_id)
            .map_err(|error| GateError::Malformed(error.to_string()))?;

        let Some(question) = question else {
            return Ok(Some(GateRefusal::ApprovalMissing));
        };
        if question.run_id != request.run_id || question.kind != QuestionKind::Approval {
            return Ok(Some(GateRefusal::ApprovalMissing));
        }
        if question.state != QuestionState::Answered
            || question.author != Some(QuestionAuthor::User)
        {
            return Ok(Some(GateRefusal::NotAuthorized {
                state: question.state.as_str(),
            }));
        }
        if let Some(expires_at) = question.expires_at
            && expires_at <= request.now
        {
            return Ok(Some(GateRefusal::ApprovalExpired {
                expired_at: expires_at,
            }));
        }

        let (Some(tree_hash), Some(paths_digest)) = (question.tree_hash, question.paths_digest)
        else {
            return Ok(Some(GateRefusal::ReceiptMissing));
        };

        let frozen = Receipt {
            tree_hash,
            paths_digest,
        };
        let derived = receipt_of(derivation);

        if receipt_holds(&frozen, &derived, derivation.merged) {
            Ok(None)
        } else {
            Ok(Some(GateRefusal::ReceiptStale { frozen, derived }))
        }
    }

    /// Whether the run stayed inside the budget and the scope it was approved
    /// with.
    fn transaction_refusal(
        &self,
        request: &PreMergeRequest,
        run: &RunRow,
        derivation: &GateDerivation,
    ) -> Result<Option<GateRefusal>, GateError> {
        let attempts = self
            .machines
            .store()
            .attempts_for_run(request.run_id)
            .map_err(|error| GateError::Malformed(error.to_string()))?;
        let charged = charged_attempts(&attempts);

        if charged > request.attempt_cap {
            return Ok(Some(GateRefusal::AttemptsExhausted {
                charged,
                cap: request.attempt_cap,
            }));
        }

        let Some(genesis) = run.genesis_paths.as_deref() else {
            return Ok(None);
        };
        let genesis = parse_genesis_paths(genesis)?;
        let outside = paths_outside(&derivation.changed_paths, &genesis);

        if outside.is_empty() {
            Ok(None)
        } else {
            Ok(Some(GateRefusal::OutsideGenesisPaths { paths: outside }))
        }
    }

    /// Moves the worktree to `reclaimable`, reporting `None` when it was
    /// already past `active`.
    fn release_worktree(
        &mut self,
        run_id: i64,
        now: i64,
        derivation: &GateDerivation,
    ) -> Result<Option<AppliedWorktreeTransition>, GateError> {
        let facts = WorktreeFacts {
            now,
            merge_re_derived: true,
            worktree_clean: !derivation.dirty,
            manual_disposition_confirmed: false,
        };

        match self
            .machines
            .apply_worktree(run_id, WorktreeTrigger::MergeDetected, &facts)
        {
            Ok(TransitionOutcome::Applied(applied)) => Ok(Some(applied)),
            Ok(TransitionOutcome::AlreadySettled) => Ok(None),
            Err(TransitionRejection::NoSuchTransition { .. }) => Ok(None),
            Err(rejection) => Err(GateError::Transition(rejection)),
        }
    }

    fn refuse_pre_merge(
        &mut self,
        request: &PreMergeRequest,
        derivation: Option<&GateDerivation>,
        refusal: GateRefusal,
    ) -> Result<PreMergeVerdict, GateError> {
        self.journal_gate_result(
            request.run_id,
            request.now,
            &gate_payload(request, derivation, Some(refusal.as_str())),
        )?;

        Ok(PreMergeVerdict::Refused(refusal))
    }

    fn journal_reclaim_result(
        &mut self,
        request: &ReclaimRequest,
        derivation: Option<&GateDerivation>,
        refusal: Option<&GateRefusal>,
    ) -> Result<(), GateError> {
        let mut payload = serde_json::json!({
            "gate": "reclaim",
            "passed": refusal.is_none(),
            "main_ref": request.main_ref,
        });
        merge_derivation(&mut payload, derivation);
        if let (Some(object), Some(refusal)) = (payload.as_object_mut(), refusal) {
            object.insert(
                "reason".to_owned(),
                serde_json::Value::from(refusal.as_str()),
            );
        }

        self.journal_gate_result(request.run_id, request.now, &payload)
    }

    fn journal_gate_result(
        &mut self,
        run_id: i64,
        now: i64,
        payload: &serde_json::Value,
    ) -> Result<(), GateError> {
        self.machines.journal(&[EventRow {
            id: None,
            run_id: Some(run_id),
            event_type: "gate_result".to_owned(),
            class: EventClass::Infra,
            payload: payload.to_string(),
            ts: now,
        }])?;

        Ok(())
    }

    fn journal_merged(
        &mut self,
        request: &PreMergeRequest,
        branch: &str,
        commit: Option<&str>,
    ) -> Result<(), GateError> {
        let payload = serde_json::json!({
            "branch": branch,
            "into": request.main_ref,
            "path": request.path.as_str(),
            "commit": commit,
        });

        self.machines.journal(&[EventRow {
            id: None,
            run_id: Some(request.run_id),
            event_type: "merged".to_owned(),
            class: EventClass::Infra,
            payload: payload.to_string(),
            ts: request.now,
        }])?;

        Ok(())
    }

    fn load_run(&self, run_id: i64) -> Result<RunRow, GateError> {
        self.machines
            .store()
            .load_run(run_id)
            .map_err(|error| GateError::Malformed(error.to_string()))?
            .ok_or(GateError::NoSuchRun(run_id))
    }
}

/// Freezes the receipt an approval over `worktree` is bound to.
///
/// It takes the worktree rather than a run because an approval is created
/// before there is a gate to run, and because binding it to the same derivation
/// the gate re-runs is what keeps the two sides of the comparison from drifting
/// into disagreeing about what a digest of one worktree is.
pub fn freeze_receipt(
    worktrees: &SessionWorktrees,
    worktree: &Path,
    main_ref: &str,
) -> Result<Receipt, GateError> {
    Ok(receipt_of(&worktrees.derive(worktree, main_ref)?))
}

/// Whether the merge ran, or why the gate stopped instead.
enum Integration {
    Landed(Option<String>),
    Refused(PreMergeVerdict),
}

fn worktree_path(run: &RunRow) -> Option<&str> {
    run.worktree_path.as_deref().filter(|path| !path.is_empty())
}

fn topology_refusal(derivation: &GateDerivation) -> Option<GateRefusal> {
    if derivation.branch.is_none() {
        return Some(GateRefusal::DetachedHead);
    }
    if derivation.merge_base.is_none() {
        return Some(GateRefusal::UnrelatedHistory);
    }
    if derivation.dirty {
        return Some(GateRefusal::WorktreeDirty);
    }

    None
}

/// Whether the receipt still binds the bytes in front of the gate.
///
/// The tree hash is the whole of it: it is the identity of every byte the
/// worktree holds, so a commit made after the authorization moves it and no
/// commit that leaves the tree identical can hide behind it.
///
/// The paths digest is checked only while the branch has yet to land. Once it
/// has, the merge base is the branch's own head and the diff against the target
/// is empty by construction, so the derived digest describes an empty set
/// rather than the branch's scope. Comparing it there would refuse every
/// attestation and every second run of the gate, and it would refuse them for a
/// property of git's ancestry rather than for anything about the approved
/// bytes.
fn receipt_holds(frozen: &Receipt, derived: &Receipt, merged: bool) -> bool {
    frozen.tree_hash == derived.tree_hash && (merged || frozen.paths_digest == derived.paths_digest)
}

/// The receipt a derivation implies.
fn receipt_of(derivation: &GateDerivation) -> Receipt {
    Receipt {
        tree_hash: derivation.head_tree.clone(),
        paths_digest: paths_digest(&derivation.changed_paths),
    }
}

/// A digest over the paths a branch touches, sorted, unique, and separated by a
/// byte no path can contain.
///
/// The separator matters: joining on any character a filename may hold would
/// let two different path sets produce the same digest, which is the one way a
/// receipt could pass for a tree it was not frozen from.
fn paths_digest(paths: &[String]) -> String {
    let mut hasher = Sha256::new();
    for path in paths {
        hasher.update(path.as_bytes());
        hasher.update([0]);
    }

    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Attempts that count against the run's budget.
///
/// An `interrupted` attempt does not: a quota park, a wait on a person and a
/// reboot are not the agent's failures, and charging them would spend a budget
/// on infrastructure.
fn charged_attempts(attempts: &[agens_store::AttemptRow]) -> i64 {
    attempts
        .iter()
        .filter(|attempt| attempt.outcome != Some(agens_store::AttemptOutcome::Interrupted))
        .count()
        .try_into()
        .unwrap_or(i64::MAX)
}

fn parse_genesis_paths(stored: &str) -> Result<Vec<String>, GateError> {
    serde_json::from_str::<Vec<String>>(stored).map_err(|error| {
        GateError::Malformed(format!("genesis_paths is not a path array: {error}"))
    })
}

/// The changed paths no genesis path covers.
///
/// A genesis entry covers a path when it is that path, or when it is a
/// directory the path sits under. The prefix is compared at a component
/// boundary, so `crates/agens-server` never covers `crates/agens-server-extra`.
fn paths_outside(changed: &[String], genesis: &[String]) -> Vec<String> {
    changed
        .iter()
        .filter(|path| {
            !genesis.iter().any(|allowed| {
                *path == allowed
                    || path
                        .strip_prefix(allowed.as_str())
                        .is_some_and(|rest| rest.starts_with('/'))
            })
        })
        .cloned()
        .collect()
}

fn gate_payload(
    request: &PreMergeRequest,
    derivation: Option<&GateDerivation>,
    refusal: Option<&str>,
) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "gate": "pre_merge",
        "passed": refusal.is_none(),
        "path": request.path.as_str(),
        "main_ref": request.main_ref,
        "approval_id": request.approval_id,
    });
    merge_derivation(&mut payload, derivation);
    if let (Some(object), Some(refusal)) = (payload.as_object_mut(), refusal) {
        object.insert("reason".to_owned(), serde_json::Value::from(refusal));
    }

    payload
}

fn merge_derivation(payload: &mut serde_json::Value, derivation: Option<&GateDerivation>) {
    let (Some(object), Some(derivation)) = (payload.as_object_mut(), derivation) else {
        return;
    };

    object.insert("branch".to_owned(), serde_json::json!(derivation.branch));
    object.insert(
        "merge_base".to_owned(),
        serde_json::json!(derivation.merge_base),
    );
    object.insert(
        "head_tree".to_owned(),
        serde_json::json!(derivation.head_tree),
    );
    object.insert("merged".to_owned(), serde_json::json!(derivation.merged));
    object.insert("dirty".to_owned(), serde_json::json!(derivation.dirty));
    object.insert(
        "changed_paths".to_owned(),
        serde_json::json!(derivation.changed_paths),
    );
}
