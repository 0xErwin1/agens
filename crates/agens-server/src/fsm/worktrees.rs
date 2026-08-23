//! The worktree state machine.
//!
//! `active → reclaimable → cleaned`, with `active → cleaned` reachable only
//! through a confirmed manual disposition in the cleaning flow.
//!
//! `active → reclaimable` is driven by deterministic merge detection re-derived
//! from git at the moment of the transition, never by a stored flag. The
//! shortcut exists because a person can decide to throw work away, and it is
//! guarded because nothing else should be able to: reaching `cleaned` from
//! `active` discards a worktree whose branch was never shown to be merged.

use agens_store::{EventClass, StateChange, TransitionWrite, WorktreeStatus};

use super::{
    AppliedTransition, JournaledMove, StateMachines, TransitionOutcome, TransitionRejection,
    event_pair, transition_events,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorktreeTrigger {
    /// The gate or the reclaim pass re-derived that the branch is merged.
    MergeDetected,
    /// The reclaim pass is releasing a worktree nothing needs any more.
    Reclaim,
    /// A person confirmed the disposal in the cleaning flow.
    ManualDisposition,
}

impl WorktreeTrigger {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MergeDetected => "merge_detected",
            Self::Reclaim => "reclaim",
            Self::ManualDisposition => "manual_disposition",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorktreeGuard {
    /// Merge state re-derived from git in this moment. A stored flag never
    /// satisfies it, which is why the caller reports the derivation rather than
    /// the machine reading a column.
    MergeReDerived,
    /// Nothing uncommitted is left to lose.
    WorktreeClean,
    /// A person confirmed this disposal. Without it there is no path from
    /// `active` to `cleaned` at all.
    ConfirmedManualDisposition,
}

impl WorktreeGuard {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MergeReDerived => "merge_re_derived",
            Self::WorktreeClean => "worktree_clean",
            Self::ConfirmedManualDisposition => "confirmed_manual_disposition",
        }
    }
}

/// What an applied worktree transition causes. Both are the caller's: this
/// machine records the disposition, it does not touch the filesystem.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorktreeEffect {
    /// The worktree is no longer needed and may be removed.
    ReleaseForReclaim,
    RemoveWorktreeDirectory,
}

/// One row of the worktree transition table.
pub struct WorktreeTransition {
    pub from: WorktreeStatus,
    pub trigger: WorktreeTrigger,
    pub to: WorktreeStatus,
    pub guard: WorktreeGuard,
    pub effects: &'static [WorktreeEffect],
    pub domain_event: &'static str,
    pub class: EventClass,
}

/// The worktree machine, as data.
pub static WORKTREE_TRANSITIONS: &[WorktreeTransition] = &[
    WorktreeTransition {
        from: WorktreeStatus::Active,
        trigger: WorktreeTrigger::MergeDetected,
        to: WorktreeStatus::Reclaimable,
        guard: WorktreeGuard::MergeReDerived,
        effects: &[WorktreeEffect::ReleaseForReclaim],
        domain_event: "worktree_reclaimable",
        class: EventClass::Infra,
    },
    WorktreeTransition {
        from: WorktreeStatus::Reclaimable,
        trigger: WorktreeTrigger::Reclaim,
        to: WorktreeStatus::Cleaned,
        guard: WorktreeGuard::WorktreeClean,
        effects: &[WorktreeEffect::RemoveWorktreeDirectory],
        domain_event: "worktree_cleaned",
        class: EventClass::Infra,
    },
    WorktreeTransition {
        from: WorktreeStatus::Active,
        trigger: WorktreeTrigger::ManualDisposition,
        to: WorktreeStatus::Cleaned,
        guard: WorktreeGuard::ConfirmedManualDisposition,
        effects: &[WorktreeEffect::RemoveWorktreeDirectory],
        domain_event: "worktree_cleaned",
        class: EventClass::Infra,
    },
];

/// What the caller re-derived, and what a person confirmed.
#[derive(Clone, Copy, Debug, Default)]
pub struct WorktreeFacts {
    /// Epoch seconds.
    pub now: i64,
    /// Set only by a caller that just ran the merge derivation.
    pub merge_re_derived: bool,
    pub worktree_clean: bool,
    pub manual_disposition_confirmed: bool,
}

/// One applied worktree transition.
pub type AppliedWorktreeTransition = AppliedTransition<WorktreeStatus, WorktreeEffect>;

impl StateMachines {
    /// Moves the worktree of one run, or explains why it did not move.
    pub fn apply_worktree(
        &mut self,
        run_id: i64,
        trigger: WorktreeTrigger,
        facts: &WorktreeFacts,
    ) -> Result<TransitionOutcome<WorktreeStatus, WorktreeEffect>, TransitionRejection> {
        let run = self.load_run(run_id)?;

        let Some(status) = run.worktree_status else {
            return Err(TransitionRejection::NoSuchTransition {
                machine: "worktree",
                from: "none",
                trigger: trigger.as_str(),
            });
        };

        let transition = WORKTREE_TRANSITIONS
            .iter()
            .find(|candidate| candidate.from == status && candidate.trigger == trigger)
            .ok_or(TransitionRejection::NoSuchTransition {
                machine: "worktree",
                from: status.as_str(),
                trigger: trigger.as_str(),
            })?;

        check_worktree_guard(transition, facts)?;

        let events = transition_events(
            &JournaledMove {
                run_id,
                ts: facts.now,
                class: transition.class,
                machine: "worktree",
                from: status.as_str(),
                to: transition.to.as_str(),
                trigger: trigger.as_str(),
                domain_event: transition.domain_event,
            },
            &serde_json::json!({ "worktree_path": run.worktree_path }),
        );

        let outcome = self.store.apply_transition(&TransitionWrite {
            run_id,
            run_state: None,
            worktree_status: Some(StateChange {
                expected: status,
                next: transition.to,
            }),
            question: None,
            new_question: None,
            attempt: None,
            close_attempt: None,
            provider: None,
            events: &events,
        })?;

        let (state_changed_event_id, domain_event_id) = event_pair(&outcome.event_ids)?;

        Ok(TransitionOutcome::Applied(AppliedWorktreeTransition {
            from: status,
            to: transition.to,
            effects: transition.effects,
            domain_event: transition.domain_event,
            state_changed_event_id,
            domain_event_id,
            opened_question_id: None,
        }))
    }
}

fn check_worktree_guard(
    transition: &WorktreeTransition,
    facts: &WorktreeFacts,
) -> Result<(), TransitionRejection> {
    let holds = match transition.guard {
        WorktreeGuard::MergeReDerived => facts.merge_re_derived,
        WorktreeGuard::WorktreeClean => facts.worktree_clean,
        WorktreeGuard::ConfirmedManualDisposition => facts.manual_disposition_confirmed,
    };

    if holds {
        return Ok(());
    }

    Err(TransitionRejection::GuardFailed {
        machine: "worktree",
        guard: transition.guard.as_str(),
        detail: match transition.guard {
            WorktreeGuard::MergeReDerived => {
                "merge state was not re-derived for this transition".to_owned()
            }
            WorktreeGuard::WorktreeClean => "the worktree still has uncommitted changes".to_owned(),
            WorktreeGuard::ConfirmedManualDisposition => {
                "discarding an active worktree needs a confirmed manual disposition".to_owned()
            }
        },
    })
}
