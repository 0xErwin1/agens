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
//! **The authorization is spent by the merge it authorized.** A landed merge
//! moves its approval to `delivered` inside [`Gates::pre_merge`], and the
//! question machine has no transition out of that state. Without it the same
//! `approval_id` would pass the gate again: once the branch has landed the
//! paths digest is no longer compared, so nothing else would tell the second
//! presentation from the first.
//!
//! What the gate does **not** do is invoke a sub-agent. A merge that does not
//! apply, and a worktree that is dirty when the reclaim sweep reaches it, both
//! leave as a typed [`SubAgentRequest`] for the caller to act on, and are
//! journaled beside the verdict that produced them so the request outlives the
//! caller that received it. The coordinator is deterministic and never invokes
//! a model.
//!
//! **This is the one worktree gate.** The daemon reaches it through the sweep
//! its composition root runs, built for the span of each sweep from
//! `ApiCore::machines_mut`. [`crate::GitWorktreeGate`] is not a second one:
//! it is the narrow git seam the service core's own operations derive and
//! dispose through, it forms no verdict and moves no row of its own, and both
//! derive through the same [`SessionWorktrees`] pass so the two sides of a
//! receipt comparison cannot disagree.

use std::path::Path;

use agens_store::{
    EventClass, EventRow, QuestionAuthor, QuestionKind, QuestionState, RunRow, WorktreeStatus,
};
use agens_tools::{GateDerivation, MergeOutcome, SessionWorktrees, WorktreeError};
use sha2::{Digest, Sha256};

use crate::fsm::{
    AppliedWorktreeTransition, MergeSettlement, SettledMerge, StateMachines, TransitionOutcome,
    TransitionRejection, WorktreeFacts, WorktreeTrigger,
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
        /// `None` when the worktree was already past `active`. It is the
        /// attestation path that reaches this: a branch somebody else landed
        /// and released is still a branch the gate can verify.
        worktree: Option<AppliedWorktreeTransition>,
    },
    IntegrationRequired(SubAgentRequest),
    Refused(GateRefusal),
}

/// How a reclaim sweep ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReclaimVerdict {
    Released {
        released: AppliedWorktreeTransition,
        /// The move to `cleaned` the same sweep went on to make. `None` when
        /// the directory could not be removed, which leaves the row
        /// `reclaimable` for the next pass rather than declaring a disposal
        /// that did not happen.
        cleaned: Option<AppliedWorktreeTransition>,
    },
    CleanupRequired(SubAgentRequest),
    Refused(GateRefusal),
}

/// How finishing an already released worktree ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DisposeVerdict {
    /// The directory is gone and the row reached `cleaned`.
    Cleaned(AppliedWorktreeTransition),
    /// The row was not `reclaimable`, so there was nothing to finish.
    NotReleased,
    /// The directory is still on disk. Journaled, and the row stays
    /// `reclaimable` for the next pass to try again.
    Retained,
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
    /// The run never froze its genesis paths, so there is no scope to compare
    /// the diff against. An unfrozen run merges without confinement, which is
    /// the one thing the comparison exists to prevent.
    GenesisUnfrozen,
    /// The diff reaches outside the frozen genesis paths.
    OutsideGenesisPaths {
        paths: Vec<String>,
    },
    /// The attestation says the work landed and git says it did not.
    NotMerged,
    /// Git could not be asked at all, so no rule was evaluated. It is the one
    /// entry here that is journaled beside an error rather than a verdict.
    DerivationFailed,
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
            Self::GenesisUnfrozen => "genesis_unfrozen",
            Self::OutsideGenesisPaths { .. } => "outside_genesis_paths",
            Self::NotMerged => "not_merged",
            Self::DerivationFailed => "derivation_failed",
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
/// It borrows the state machines rather than owning them: the API core is their
/// single owner, and a daemon that let the gates own a second copy would have
/// two writers of the same tables. A gate is therefore built for the span of
/// the sweep it runs, from `ApiCore::machines_mut`.
pub struct Gates<'a> {
    machines: &'a mut StateMachines,
    worktrees: SessionWorktrees,
}

impl<'a> Gates<'a> {
    #[must_use]
    pub const fn new(machines: &'a mut StateMachines, worktrees: SessionWorktrees) -> Self {
        Self {
            machines,
            worktrees,
        }
    }

    /// The state machines, for a caller that also has transitions of its own to
    /// apply.
    #[must_use]
    pub const fn machines(&self) -> &StateMachines {
        self.machines
    }

    #[must_use]
    pub const fn machines_mut(&mut self) -> &mut StateMachines {
        self.machines
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
    /// anything land. Once it has, `gate_result`, the authorization being
    /// spent, `merged` and the release are one write: a subscriber never sees a
    /// released directory without the verdict that released it, never a merge
    /// whose authorization is still standing, and a failure between them is not
    /// a state the store can be left in.
    pub fn pre_merge(&mut self, request: &PreMergeRequest) -> Result<PreMergeVerdict, GateError> {
        let run = self.load_run(request.run_id)?;

        let Some(worktree) = worktree_path(&run).map(ToOwned::to_owned) else {
            return self.refuse_pre_merge(request, None, GateRefusal::NoWorktree);
        };

        let derivation = match self
            .worktrees
            .derive(Path::new(&worktree), &request.main_ref)
        {
            Ok(derivation) => derivation,
            Err(error) => return Err(self.journal_derivation_failure(request, &error)?),
        };

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

        let settled = self.settle(request, &derivation, &branch, commit.as_deref())?;

        Ok(PreMergeVerdict::Merged {
            commit,
            worktree: settled.worktree,
        })
    }

    /// Records everything a landed merge implies, in one write.
    ///
    /// The verdict, the authorization being spent, the `merged` entry and the
    /// release travel together because a merge that has already happened cannot
    /// be undone by a second statement failing. A settlement that never lands
    /// leaves the approval unspent and no `gate_result` naming it, which is
    /// exactly what the next sweep needs in order to present it again against a
    /// branch git now reports as merged.
    fn settle(
        &mut self,
        request: &PreMergeRequest,
        derivation: &GateDerivation,
        branch: &str,
        commit: Option<&str>,
    ) -> Result<SettledMerge, GateError> {
        let verdict = self.gate_event(
            request.run_id,
            request.now,
            &gate_payload(request, Some(derivation), None),
        );
        let merged = EventRow {
            id: None,
            run_id: Some(request.run_id),
            event_type: MERGED_EVENT.to_owned(),
            class: EventClass::Infra,
            payload: serde_json::json!({
                "branch": branch,
                "into": request.main_ref,
                "path": request.path.as_str(),
                "commit": commit,
            })
            .to_string(),
            ts: request.now,
        };

        Ok(self.machines.settle_merge(&MergeSettlement {
            run_id: request.run_id,
            approval_id: request.approval_id,
            now: request.now,
            verdict: &verdict,
            merged: &merged,
            worktree_clean: !derivation.dirty,
        })?)
    }

    /// Sweeps one run's worktree, releasing it only when git says here and now
    /// that its branch landed and that nothing uncommitted is left to lose.
    pub fn reclaim(&mut self, request: &ReclaimRequest) -> Result<ReclaimVerdict, GateError> {
        let run = self.load_run(request.run_id)?;

        let Some(worktree) = worktree_path(&run).map(ToOwned::to_owned) else {
            self.journal_reclaim_result(request, None, Some(&GateRefusal::NoWorktree))?;
            return Ok(ReclaimVerdict::Refused(GateRefusal::NoWorktree));
        };

        let derivation = match self
            .worktrees
            .derive(Path::new(&worktree), &request.main_ref)
        {
            Ok(derivation) => derivation,
            Err(error) => {
                self.journal_reclaim_result(request, None, Some(&GateRefusal::DerivationFailed))?;

                return Err(GateError::Derivation(error));
            }
        };

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

            let work = SubAgentRequest {
                kind: SubAgentKind::Cleanup,
                run_id: request.run_id,
                worktree_path: worktree,
                branch: derivation.branch.clone(),
                detail: "the worktree holds uncommitted changes".to_owned(),
            };
            self.journal_sub_agent_request(&work, request.now)?;

            return Ok(ReclaimVerdict::CleanupRequired(work));
        }

        self.journal_reclaim_result(request, Some(&derivation), None)?;

        let released = self
            .release_worktree(request.run_id, request.now, &derivation)?
            .ok_or_else(|| {
                GateError::Transition(TransitionRejection::NoSuchTransition {
                    machine: "worktree",
                    from: "not active",
                    trigger: WorktreeTrigger::MergeDetected.as_str(),
                })
            })?;

        // The sweep continues into the disposal rather than leaving the row on
        // `reclaimable`: nothing else ever moves it on, and a row that stops
        // there still counts against the worktree ceiling for the rest of the
        // installation's life.
        let cleaned = match self.dispose(request)? {
            DisposeVerdict::Cleaned(applied) => Some(applied),
            DisposeVerdict::NotReleased | DisposeVerdict::Retained => None,
        };

        Ok(ReclaimVerdict::Released { released, cleaned })
    }

    /// Removes a released worktree's directory and moves its row to `cleaned`.
    ///
    /// This is what makes `cleaned` reachable without a person: the merge gate
    /// and the reclaim sweep both stop at `reclaimable`, and until this runs the
    /// run goes on holding a directory and a slot in the worktree ceiling.
    ///
    /// A directory that is already gone is not an error. The row is the thing
    /// that costs the machine something, and a disposal that refused to finish
    /// because the filesystem had got there first would leave exactly the row
    /// this exists to retire.
    pub fn dispose(&mut self, request: &ReclaimRequest) -> Result<DisposeVerdict, GateError> {
        let run = self.load_run(request.run_id)?;

        if run.worktree_status != Some(WorktreeStatus::Reclaimable) {
            return Ok(DisposeVerdict::NotReleased);
        }

        if let Some(worktree) = worktree_path(&run).map(ToOwned::to_owned)
            && Path::new(&worktree).is_dir()
            && let Some(refusal) = self.remove_directory(&run, Path::new(&worktree))
        {
            self.journal_dispose_result(request, Some(&refusal))?;

            return Ok(DisposeVerdict::Retained);
        }

        // Nothing uncommitted is left to lose: either the directory is gone, or
        // git has just removed it, and `worktree remove` refuses a worktree
        // that still holds work.
        let facts = WorktreeFacts {
            now: request.now,
            merge_re_derived: false,
            worktree_clean: true,
            manual_disposition_confirmed: false,
        };

        match self
            .machines
            .apply_worktree(request.run_id, WorktreeTrigger::Reclaim, &facts)
        {
            Ok(TransitionOutcome::Applied(applied)) => {
                self.journal_dispose_result(request, None)?;

                Ok(DisposeVerdict::Cleaned(applied))
            }
            Ok(TransitionOutcome::AlreadySettled)
            | Err(TransitionRejection::NoSuchTransition { .. }) => Ok(DisposeVerdict::NotReleased),
            Err(rejection) => Err(GateError::Transition(rejection)),
        }
    }

    /// Removes the worktree directory, reporting what git said when it would
    /// not.
    ///
    /// The repository id and the worktree name come from the path rather than
    /// from the run row: the layout is `worktrees/<repository id>/<name>`, and
    /// the directory that exists is the one the removal has to name.
    fn remove_directory(&self, run: &RunRow, worktree: &Path) -> Option<String> {
        let (Some(name), Some(repository_id)) = (
            worktree.file_name().and_then(|name| name.to_str()),
            worktree
                .parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str()),
        ) else {
            return Some(format!(
                "{} is not a session worktree path",
                worktree.display()
            ));
        };

        self.worktrees
            .remove(Path::new(&run.repo_root), repository_id, name)
            .err()
            .map(|error| error.to_string())
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

                let work = SubAgentRequest {
                    kind: SubAgentKind::Integration,
                    run_id: request.run_id,
                    worktree_path: worktree_path(run).unwrap_or_default().to_owned(),
                    branch: Some(branch.to_owned()),
                    detail,
                };
                self.journal_sub_agent_request(&work, request.now)?;

                Ok(Integration::Refused(PreMergeVerdict::IntegrationRequired(
                    work,
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
            return Ok(Some(GateRefusal::GenesisUnfrozen));
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

    /// Records what the disposal did, whichever way it went.
    ///
    /// A disposal that could not remove the directory is journaled for the same
    /// reason a refused gate is: the row stays `reclaimable` and the next sweep
    /// tries again, and without an entry a directory git will never let go of
    /// is retried every interval with nothing to show for it.
    fn journal_dispose_result(
        &mut self,
        request: &ReclaimRequest,
        refusal: Option<&str>,
    ) -> Result<(), GateError> {
        let payload = serde_json::json!({
            "gate": "dispose",
            "passed": refusal.is_none(),
            "main_ref": request.main_ref,
            "reason": refusal,
        });

        self.journal_gate_result(request.run_id, request.now, &payload)
    }

    /// Records work the coordinator is not allowed to do itself.
    ///
    /// It lands in the journal rather than only in the return value because the
    /// caller that receives it has nowhere durable to put it: the coordinator
    /// never reaches a model, so the request waits here for the surface that
    /// does.
    fn journal_sub_agent_request(
        &mut self,
        work: &SubAgentRequest,
        now: i64,
    ) -> Result<(), GateError> {
        let payload = serde_json::json!({
            "kind": work.kind.as_str(),
            "worktree_path": work.worktree_path,
            "branch": work.branch,
            "detail": work.detail,
        });

        self.machines.journal(&[EventRow {
            id: None,
            run_id: Some(work.run_id),
            event_type: SUB_AGENT_EVENT.to_owned(),
            class: EventClass::Infra,
            payload: payload.to_string(),
            ts: now,
        }])?;

        Ok(())
    }

    fn journal_gate_result(
        &mut self,
        run_id: i64,
        now: i64,
        payload: &serde_json::Value,
    ) -> Result<(), GateError> {
        let event = self.gate_event(run_id, now, payload);
        self.machines.journal(&[event])?;

        Ok(())
    }

    /// Records that git itself could not be asked, and carries the failure on.
    ///
    /// Without the entry the sweep re-runs the same invocation every interval
    /// and leaves nothing behind that says it did: a repository the daemon
    /// cannot reach looks identical to one it never looked at.
    fn journal_derivation_failure(
        &mut self,
        request: &PreMergeRequest,
        error: &WorktreeError,
    ) -> Result<GateError, GateError> {
        let mut payload = gate_payload(request, None, Some(GateRefusal::DerivationFailed.as_str()));
        if let Some(object) = payload.as_object_mut() {
            object.insert(
                "detail".to_owned(),
                serde_json::Value::from(error.to_string()),
            );
        }

        self.journal_gate_result(request.run_id, request.now, &payload)?;

        Ok(GateError::Derivation(error.clone()))
    }

    fn gate_event(&self, run_id: i64, now: i64, payload: &serde_json::Value) -> EventRow {
        EventRow {
            id: None,
            run_id: Some(run_id),
            event_type: GATE_RESULT_EVENT.to_owned(),
            class: EventClass::Infra,
            payload: payload.to_string(),
            ts: now,
        }
    }

    fn load_run(&self, run_id: i64) -> Result<RunRow, GateError> {
        self.machines
            .store()
            .load_run(run_id)
            .map_err(|error| GateError::Malformed(error.to_string()))?
            .ok_or(GateError::NoSuchRun(run_id))
    }
}

/// The journal entry a gate's sub-agent request becomes.
pub(crate) const SUB_AGENT_EVENT: &str = "sub_agent_requested";

/// The journal entry every gate verdict becomes.
///
/// Declared here, where it is written, and imported by the sweep that reads it
/// back: an approval is a candidate only until a `gate_result` names it, and the
/// producer and the consumer disagreeing about the name would make every
/// approval a candidate forever.
pub(crate) const GATE_RESULT_EVENT: &str = "gate_result";

/// The journal entry a landed merge becomes.
pub(crate) const MERGED_EVENT: &str = "merged";

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
pub(crate) fn receipt_of(derivation: &GateDerivation) -> Receipt {
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
