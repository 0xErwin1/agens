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

use agens_store::{EventClass, RunRow, RunState, StateChange, TransitionWrite, WorktreeStatus};

use super::{
    AppliedTransition, JournaledMove, PreparedTransition, StateMachines, TransitionOutcome,
    TransitionRejection, event_pair, transition_events,
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

/// The run states in which a run still has a claim on its worktree.
///
/// It lives next to the transition table because it is the same fact the table
/// describes from the other side: a worktree costs the machine a directory and
/// a slot in the ceiling until it reaches `cleaned`, and no run state releases
/// it on its own. `done` and `failed` claim theirs because the reclaim pass has
/// yet to run, and `cancelled` claims one because cancellation moves the run
/// and never touches `worktree_status`.
///
/// Every reader of "which runs hold a worktree" reads this list. Two readers
/// with two lists is what let boot reconciliation report a cancelled run's
/// directory as an orphan while the scheduler was still counting it.
pub const WORKTREE_HOLDING_RUN_STATES: &[RunState] = &[
    RunState::Draft,
    RunState::Queued,
    RunState::Running,
    RunState::AwaitingInput,
    RunState::AwaitingQuota,
    RunState::Interrupted,
    RunState::Done,
    RunState::Failed,
    RunState::Cancelled,
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

/// One worktree transition prepared but not yet written.
pub(super) type PreparedWorktree =
    PreparedTransition<WorktreeStatus, StateChange<WorktreeStatus>, WorktreeEffect>;

/// Prepares the release of a run's directory without writing it.
///
/// `None` when the row is already past `active`, which is the same answer
/// [`StateMachines::apply_worktree`] gives that caller: a branch somebody else
/// landed and released is still one a gate can verify, and refusing the whole
/// settlement over it would refuse the verdict too.
pub(super) fn releasable(
    run: &RunRow,
    now: i64,
    worktree_clean: bool,
) -> Result<Option<PreparedWorktree>, TransitionRejection> {
    let Some(status) = run.worktree_status else {
        return Ok(None);
    };

    let Some(transition) = WORKTREE_TRANSITIONS.iter().find(|candidate| {
        candidate.from == status && candidate.trigger == WorktreeTrigger::MergeDetected
    }) else {
        return Ok(None);
    };

    let facts = WorktreeFacts {
        now,
        merge_re_derived: true,
        worktree_clean,
        manual_disposition_confirmed: false,
    };
    check_worktree_guard(transition, &facts)?;

    let run_id = run
        .id
        .ok_or_else(|| TransitionRejection::Storage("a stored run must carry an id".to_owned()))?;

    let events = transition_events(
        &JournaledMove {
            run_id,
            ts: now,
            class: transition.class,
            machine: "worktree",
            from: status.as_str(),
            to: transition.to.as_str(),
            trigger: WorktreeTrigger::MergeDetected.as_str(),
            domain_event: transition.domain_event,
        },
        &serde_json::json!({ "worktree_path": run.worktree_path }),
    );

    Ok(Some(PreparedWorktree {
        change: StateChange {
            expected: status,
            next: transition.to,
        },
        from: status,
        to: transition.to,
        effects: transition.effects,
        domain_event: transition.domain_event,
        events,
    }))
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
