//! The run state machine.
//!
//! `draft → queued → running → awaiting_input | awaiting_quota → done | failed
//! | cancelled`, with `interrupted` as the state boot reconciliation puts a run
//! in when the database says it was running and no session is alive to match.
//!
//! There is no transition back to `draft`. An approved scope is never edited,
//! because a scope that does not move is what makes divergence measurable;
//! replanning opens a new run that inherits the worktree and the lineage.

use agens_store::{
    AttemptClose, AttemptOutcome, AttemptRow, EventClass, ProviderRow, QuestionRow, QuestionState,
    QuotaState, RetryTrigger, RunState, StateChange, TransitionWrite, WorktreeStatus,
};

use super::{
    AppliedTransition, JournaledMove, Principal, StateMachines, TransitionOutcome,
    TransitionRejection, event_pair, transition_events,
};

/// What drives a run out of the state it is in.
///
/// A caller names one of these, never a destination state. Which state it leads
/// to is the table's answer, not the caller's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunTrigger {
    /// The user approved the proposed execution.
    Approve,
    /// The user discarded it during planning.
    Discard,
    /// The scheduler admitted it to a slot.
    Admit,
    /// The worker called `ask`.
    Ask,
    /// The provider reported its quota reached.
    QuotaReached,
    /// The question the run is blocked on has been answered.
    Answered,
    /// The provider's reset time has passed.
    QuotaReset,
    /// The run reported finishing.
    Finished,
    /// The attempt failed.
    AttemptFailed,
    /// A retry was requested with guidance.
    Retry,
    /// Cancellation, from the user or through Praetor.
    Cancel,
    /// Boot reconciliation found a running row with no live session.
    Reconcile,
    /// The reconciled run goes back to the queue.
    Resume,
}

impl RunTrigger {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Discard => "discard",
            Self::Admit => "admit",
            Self::Ask => "ask",
            Self::QuotaReached => "quota_reached",
            Self::Answered => "answered",
            Self::QuotaReset => "quota_reset",
            Self::Finished => "finished",
            Self::AttemptFailed => "attempt_failed",
            Self::Retry => "retry",
            Self::Cancel => "cancel",
            Self::Reconcile => "reconcile",
            Self::Resume => "resume",
        }
    }
}

/// The condition a transition needs beyond being in its source state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunGuard {
    /// Being in the source state is the whole condition.
    None,
    /// Approving an execution is the user's alone.
    UserApproval,
    /// A free slot, a provider that is serving, and a worktree ready to run in.
    SchedulerAdmission,
    /// The run's own lifecycle facts are reported by the coordinator's ingest,
    /// never claimed by a client: without this, a caller could report a run
    /// finished that never ran.
    ReportedByHarness,
    /// The named question belongs to this run and has been answered.
    AnsweredQuestion,
    /// The coordinator reports the `ask`, and it carries the question the run
    /// is about to park on. Without the question half a run reaches
    /// `awaiting_input` with nothing to answer, which is a state nothing can
    /// bring it back out of.
    AskOpensQuestion,
    /// The provider is serving again: either its recorded reset time has
    /// passed, or the cap has already been lifted. Re-derived from the provider
    /// row rather than taken from the caller, because the timer wheel keeps no
    /// state of its own.
    ///
    /// The second half is what lets one elapsed reset wake every run parked on
    /// that provider. The first of them clears the cap as its effect, so a
    /// guard that only accepted a pending reset time would strand every
    /// remaining run in `awaiting_quota` with nothing left to wake it.
    QuotaResetElapsed,
    /// Guidance, a worktree still `active`, and retry budget left. The worktree
    /// half is the one that matters: what already landed is not retried, it is
    /// done again in a new run.
    RetryEligible,
    /// Only boot reconciliation declares a run interrupted.
    BootReconciliation,
}

impl RunGuard {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::UserApproval => "user_approval",
            Self::SchedulerAdmission => "scheduler_admission",
            Self::ReportedByHarness => "reported_by_harness",
            Self::AnsweredQuestion => "answered_question",
            Self::AskOpensQuestion => "ask_opens_question",
            Self::QuotaResetElapsed => "quota_reset_elapsed",
            Self::RetryEligible => "retry_eligible",
            Self::BootReconciliation => "boot_reconciliation",
        }
    }
}

/// What an applied transition causes.
///
/// [`RunEffect::OpenAttempt`], [`RunEffect::CloseAttempt`],
/// [`RunEffect::CapProvider`] and [`RunEffect::ClearProviderCap`] are
/// control-plane state, and the machine writes them in the same transaction as
/// the state change. Every other effect names work outside this store — a slot,
/// a session, a queue, a cancellation — and is returned to the caller to
/// perform.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunEffect {
    /// Scope, definition of done, priority and budget stop being editable.
    FreezeApprovedScope,
    /// Opens attempt N+1, with its own cost and duration.
    OpenAttempt,
    /// Ends the attempt the run was executing, with the outcome that says
    /// whether the agent is answerable for how the leg ended. An attempt left
    /// open reads as still running to everything that counts them, and the
    /// retry budget is what pays for it: a run that only ever parked would
    /// exhaust its budget without a single failure.
    CloseAttempt(AttemptOutcome),
    /// Writes the durable question the run parks on. `ask` is the only way a
    /// question comes into being: one buried in a transcript does not exist.
    OpenQuestion,
    LaunchSession,
    /// The worker does not stay resident: an hours-long wait held in memory is
    /// state a restart cannot rebuild.
    SuspendSession,
    ReleaseSlot,
    CapProvider,
    ClearProviderCap,
    /// Resumed runs are admitted before fresh ones.
    ResumePriority,
    /// A parked or interrupted run coming back is not a retry, so the leg it
    /// opens does not count against the retry budget.
    ResumeWithoutChargingRetry,
    /// The next admission opens the attempt this retry asked for, carrying the
    /// trigger that requested it.
    QueueRetryAttempt,
    EnqueueAnswerDelivery,
    EnqueueResumeDirective,
    CancelImmediately,
}

/// One row of the run transition table.
pub struct RunTransition {
    pub from: RunState,
    pub trigger: RunTrigger,
    pub to: RunState,
    pub guard: RunGuard,
    pub effects: &'static [RunEffect],
    pub domain_event: &'static str,
    /// Whether the transition describes agent behavior or the machinery around
    /// it.
    pub class: EventClass,
}

/// The run machine, as data.
pub static RUN_TRANSITIONS: &[RunTransition] = &[
    RunTransition {
        from: RunState::Draft,
        trigger: RunTrigger::Approve,
        to: RunState::Queued,
        guard: RunGuard::UserApproval,
        effects: &[RunEffect::FreezeApprovedScope],
        domain_event: "run_approved",
        class: EventClass::Infra,
    },
    RunTransition {
        from: RunState::Draft,
        trigger: RunTrigger::Discard,
        to: RunState::Cancelled,
        guard: RunGuard::None,
        effects: &[],
        domain_event: "run_discarded",
        class: EventClass::Infra,
    },
    RunTransition {
        from: RunState::Queued,
        trigger: RunTrigger::Admit,
        to: RunState::Running,
        guard: RunGuard::SchedulerAdmission,
        effects: &[RunEffect::OpenAttempt, RunEffect::LaunchSession],
        domain_event: "run_started",
        class: EventClass::Infra,
    },
    RunTransition {
        from: RunState::Running,
        trigger: RunTrigger::Ask,
        to: RunState::AwaitingInput,
        guard: RunGuard::AskOpensQuestion,
        effects: &[
            RunEffect::OpenQuestion,
            RunEffect::CloseAttempt(AttemptOutcome::Interrupted),
            RunEffect::ReleaseSlot,
            RunEffect::SuspendSession,
        ],
        domain_event: "run_awaiting_input",
        class: EventClass::Agent,
    },
    RunTransition {
        from: RunState::Running,
        trigger: RunTrigger::QuotaReached,
        to: RunState::AwaitingQuota,
        guard: RunGuard::ReportedByHarness,
        effects: &[
            RunEffect::CloseAttempt(AttemptOutcome::Interrupted),
            RunEffect::ReleaseSlot,
            RunEffect::SuspendSession,
            RunEffect::CapProvider,
        ],
        domain_event: "quota_reached",
        class: EventClass::Infra,
    },
    RunTransition {
        from: RunState::AwaitingInput,
        trigger: RunTrigger::Answered,
        to: RunState::Queued,
        guard: RunGuard::AnsweredQuestion,
        effects: &[
            RunEffect::ResumePriority,
            RunEffect::ResumeWithoutChargingRetry,
            RunEffect::EnqueueAnswerDelivery,
        ],
        domain_event: "run_resumed",
        class: EventClass::Infra,
    },
    RunTransition {
        from: RunState::AwaitingQuota,
        trigger: RunTrigger::QuotaReset,
        to: RunState::Queued,
        guard: RunGuard::QuotaResetElapsed,
        effects: &[
            RunEffect::ResumePriority,
            RunEffect::ResumeWithoutChargingRetry,
            RunEffect::ClearProviderCap,
            RunEffect::EnqueueResumeDirective,
        ],
        domain_event: "quota_reset",
        class: EventClass::Infra,
    },
    RunTransition {
        from: RunState::Running,
        trigger: RunTrigger::Finished,
        to: RunState::Done,
        guard: RunGuard::ReportedByHarness,
        effects: &[RunEffect::CloseAttempt(AttemptOutcome::Succeeded)],
        domain_event: "run_finished",
        class: EventClass::Agent,
    },
    RunTransition {
        from: RunState::Running,
        trigger: RunTrigger::AttemptFailed,
        to: RunState::Failed,
        guard: RunGuard::ReportedByHarness,
        effects: &[RunEffect::CloseAttempt(AttemptOutcome::Failed)],
        domain_event: "run_failed",
        class: EventClass::Agent,
    },
    RunTransition {
        from: RunState::Done,
        trigger: RunTrigger::Retry,
        to: RunState::Queued,
        guard: RunGuard::RetryEligible,
        effects: &[RunEffect::QueueRetryAttempt],
        domain_event: "run_retried",
        class: EventClass::Infra,
    },
    RunTransition {
        from: RunState::Failed,
        trigger: RunTrigger::Retry,
        to: RunState::Queued,
        guard: RunGuard::RetryEligible,
        effects: &[RunEffect::QueueRetryAttempt],
        domain_event: "run_retried",
        class: EventClass::Infra,
    },
    RunTransition {
        from: RunState::Running,
        trigger: RunTrigger::Reconcile,
        to: RunState::Interrupted,
        guard: RunGuard::BootReconciliation,
        effects: &[
            RunEffect::CloseAttempt(AttemptOutcome::Interrupted),
            RunEffect::ResumeWithoutChargingRetry,
        ],
        domain_event: "run_interrupted",
        class: EventClass::Infra,
    },
    RunTransition {
        from: RunState::Interrupted,
        trigger: RunTrigger::Resume,
        to: RunState::Queued,
        guard: RunGuard::None,
        effects: &[
            RunEffect::ResumePriority,
            RunEffect::ResumeWithoutChargingRetry,
            RunEffect::EnqueueResumeDirective,
        ],
        domain_event: "run_resumed",
        class: EventClass::Infra,
    },
    cancellation(RunState::Draft),
    cancellation(RunState::Queued),
    cancellation(RunState::Running),
    cancellation(RunState::AwaitingInput),
    cancellation(RunState::AwaitingQuota),
    cancellation(RunState::Interrupted),
];

/// Cancellation reaches every state a run can still be doing something in, and
/// always the same way, so the rows are generated rather than copied.
///
/// Closing the attempt is declared for all of them even though only `running`
/// has one open: closing nothing writes nothing, and a state whose attempt
/// stayed open because its row was the one not listed is the failure this
/// exists to prevent.
const fn cancellation(from: RunState) -> RunTransition {
    RunTransition {
        from,
        trigger: RunTrigger::Cancel,
        to: RunState::Cancelled,
        guard: RunGuard::None,
        effects: &[
            RunEffect::CloseAttempt(AttemptOutcome::Interrupted),
            RunEffect::CancelImmediately,
        ],
        domain_event: "run_cancelled",
        class: EventClass::Infra,
    }
}

/// What the caller knows that the store does not.
///
/// Everything the guards read either comes from here or is re-derived from the
/// store; nothing is inferred from the trigger alone.
#[derive(Clone, Debug, Default)]
pub struct RunFacts {
    /// Epoch seconds. The machine reads no clock.
    pub now: i64,
    pub principal: Principal,
    pub slot_available: bool,
    pub provider_serving: bool,
    pub worktree_ready: bool,
    /// The answered question this run was blocked on.
    pub answered_question_id: Option<i64>,
    /// The question `ask` opens, written in the same transaction as the park.
    pub opened_question: Option<QuestionRow>,
    /// When the capped provider says it will serve again. `None` means it named
    /// no reset, and what wakes the parked runs then is
    /// [`RunFacts::quota_window_seconds`].
    pub quota_reset_at: Option<i64>,
    /// How long a cap that named no reset is honoured for before the provider
    /// is tried again. Configuration rather than state, which is why it comes
    /// from the caller. `None` refuses to lift such a cap on a timer at all.
    pub quota_window_seconds: Option<i64>,
    /// Required for a retry: a retry without it is the same attempt again.
    pub guidance: Option<String>,
    /// Who asked for the retry the next admission will open an attempt for.
    pub retry_trigger: Option<RetryTrigger>,
    /// How many chargeable attempts a run gets.
    pub retry_budget: i64,
    /// Set only by boot reconciliation.
    pub boot_reconciliation: bool,
    /// The physical execution the admitted attempt runs as, correlating the run
    /// with the harness's evidence ledger.
    pub session_attempt_id: Option<i64>,
    pub session_id: Option<i64>,
}

/// One applied run transition.
pub type AppliedRunTransition = AppliedTransition<RunState, RunEffect>;

impl StateMachines {
    /// Moves a run, or explains why it did not move.
    ///
    /// The guard is evaluated against the run as stored plus [`RunFacts`], and
    /// nothing is written until it holds.
    pub fn apply_run(
        &mut self,
        run_id: i64,
        trigger: RunTrigger,
        facts: &RunFacts,
    ) -> Result<TransitionOutcome<RunState, RunEffect>, TransitionRejection> {
        let run = self.load_run(run_id)?;

        if trigger == RunTrigger::Cancel && run.state == RunState::Cancelled {
            return Ok(TransitionOutcome::AlreadySettled);
        }

        let transition = RUN_TRANSITIONS
            .iter()
            .find(|candidate| candidate.from == run.state && candidate.trigger == trigger)
            .ok_or(TransitionRejection::NoSuchTransition {
                machine: "run",
                from: run.state.as_str(),
                trigger: trigger.as_str(),
            })?;

        self.check_run_guard(
            transition,
            run_id,
            &run.provider,
            run.worktree_status,
            facts,
        )?;

        let attempt = transition
            .effects
            .contains(&RunEffect::OpenAttempt)
            .then(|| self.next_attempt(run_id, facts))
            .transpose()?;

        let close_attempt = closed_outcome(transition.effects).map(|outcome| AttemptClose {
            ended_at: facts.now,
            outcome,
        });

        let provider = self.provider_write(transition, &run.provider, facts);

        let events = transition_events(
            &JournaledMove {
                run_id,
                ts: facts.now,
                class: transition.class,
                machine: "run",
                from: run.state.as_str(),
                to: transition.to.as_str(),
                trigger: trigger.as_str(),
                domain_event: transition.domain_event,
            },
            &serde_json::json!({
                "principal": facts.principal.as_str(),
                "retry_trigger": facts.retry_trigger.map(RetryTrigger::as_str),
                "question_id": facts.answered_question_id,
            }),
        );

        let outcome = self.store.apply_transition(&TransitionWrite {
            run_id,
            run_state: Some(StateChange {
                expected: run.state,
                next: transition.to,
            }),
            worktree_status: None,
            question: None,
            new_question: transition
                .effects
                .contains(&RunEffect::OpenQuestion)
                .then_some(facts.opened_question.as_ref())
                .flatten(),
            attempt: attempt.as_ref(),
            close_attempt,
            provider: provider.as_ref(),
            events: &events,
        })?;

        let (state_changed_event_id, domain_event_id) = event_pair(&outcome.event_ids)?;

        Ok(TransitionOutcome::Applied(AppliedRunTransition {
            from: run.state,
            to: transition.to,
            effects: transition.effects,
            domain_event: transition.domain_event,
            state_changed_event_id,
            domain_event_id,
            opened_question_id: outcome.question_id,
        }))
    }

    /// The guard the transition names, run against the facts it reads.
    ///
    /// Dispatch only. Every arm that reads more than the facts it was handed is
    /// a check of its own below, so what this reads as is the table of which
    /// guard governs which trigger.
    fn check_run_guard(
        &self,
        transition: &RunTransition,
        run_id: i64,
        provider: &str,
        worktree_status: Option<WorktreeStatus>,
        facts: &RunFacts,
    ) -> Result<(), TransitionRejection> {
        match transition.guard {
            RunGuard::None => Ok(()),
            RunGuard::UserApproval => check_principal(
                transition,
                facts.principal,
                Principal::User,
                "approve an execution",
            ),
            RunGuard::ReportedByHarness => check_principal(
                transition,
                facts.principal,
                Principal::Coordinator,
                "report a run's own lifecycle facts",
            ),
            RunGuard::SchedulerAdmission => check_admission(transition, facts),
            RunGuard::AnsweredQuestion => self.check_answered_question(transition, run_id, facts),
            RunGuard::AskOpensQuestion => check_ask_opens_question(transition, run_id, facts),
            RunGuard::QuotaResetElapsed => self.check_quota_reset(transition, provider, facts),
            RunGuard::RetryEligible => {
                self.check_retry_eligible(transition, run_id, worktree_status, facts)
            }
            RunGuard::BootReconciliation => {
                if facts.boot_reconciliation {
                    Ok(())
                } else {
                    Err(guard_refused(
                        transition,
                        "only boot reconciliation interrupts a run".to_owned(),
                    ))
                }
            }
        }
    }

    /// The question the run is coming back on is one of its own, and it has
    /// been answered.
    fn check_answered_question(
        &self,
        transition: &RunTransition,
        run_id: i64,
        facts: &RunFacts,
    ) -> Result<(), TransitionRejection> {
        let Some(question_id) = facts.answered_question_id else {
            return Err(guard_refused(
                transition,
                "no question was named".to_owned(),
            ));
        };

        let question = self.load_question(question_id)?;

        if question.run_id != run_id {
            return Err(guard_refused(
                transition,
                format!("question {question_id} belongs to another run"),
            ));
        }

        if matches!(
            question.state,
            QuestionState::Answered | QuestionState::Delivered
        ) {
            Ok(())
        } else {
            Err(guard_refused(
                transition,
                format!("question {question_id} is {}", question.state.as_str()),
            ))
        }
    }

    /// The provider this run was capped on is serving again, either because the
    /// cap is already lifted or because the reset it named has passed.
    fn check_quota_reset(
        &self,
        transition: &RunTransition,
        provider: &str,
        facts: &RunFacts,
    ) -> Result<(), TransitionRejection> {
        let Some(row) = self.store.load_provider(provider)? else {
            return Err(guard_refused(
                transition,
                format!("nothing recorded for provider {provider}"),
            ));
        };

        if row.quota_state == QuotaState::Ok {
            return Ok(());
        }

        // A cap the provider named no reset for is due a window after it was
        // recorded: the only thing that lifts such a cap is a run reaching that
        // provider, and every run parked on it is barred from starting.
        let due_at = row.reset_at.or_else(|| {
            facts
                .quota_window_seconds
                .map(|window| row.updated_at.saturating_add(window))
        });

        match due_at {
            Some(reset_at) if reset_at <= facts.now => Ok(()),
            Some(reset_at) => Err(guard_refused(
                transition,
                format!(
                    "{provider} resets at {reset_at}, which is after {}",
                    facts.now
                ),
            )),
            None => Err(guard_refused(
                transition,
                format!("{provider} named no reset time"),
            )),
        }
    }

    /// There is something left to retry, something new to try, and budget to
    /// try it with.
    fn check_retry_eligible(
        &self,
        transition: &RunTransition,
        run_id: i64,
        worktree_status: Option<WorktreeStatus>,
        facts: &RunFacts,
    ) -> Result<(), TransitionRejection> {
        if worktree_status != Some(WorktreeStatus::Active) {
            return Err(guard_refused(
                transition,
                format!(
                    "worktree is {}, and what already landed is redone in a new run rather \
                     than retried",
                    worktree_status.map_or("absent", WorktreeStatus::as_str)
                ),
            ));
        }

        if facts.guidance.as_ref().is_none_or(|text| text.is_empty()) {
            return Err(guard_refused(
                transition,
                "a retry without guidance is the same attempt again".to_owned(),
            ));
        }

        let chargeable = self.chargeable_attempts(run_id)?;

        if chargeable >= facts.retry_budget {
            return Err(guard_refused(
                transition,
                format!(
                    "{chargeable} chargeable attempts against a budget of {}",
                    facts.retry_budget
                ),
            ));
        }

        Ok(())
    }

    /// Attempts that spend the retry budget.
    ///
    /// A leg that ended `interrupted` does not: parking for quota, waiting on a
    /// person and being interrupted by a restart are none of them the agent's
    /// failure, and charging them would let a run exhaust its budget without
    /// ever having been retried.
    fn chargeable_attempts(&self, run_id: i64) -> Result<i64, TransitionRejection> {
        let chargeable = self
            .store
            .attempts_for_run(run_id)?
            .into_iter()
            .filter(|attempt| attempt.outcome != Some(agens_store::AttemptOutcome::Interrupted))
            .count();

        i64::try_from(chargeable).map_err(|_| {
            TransitionRejection::Storage(format!("run {run_id} has more attempts than countable"))
        })
    }

    fn next_attempt(
        &self,
        run_id: i64,
        facts: &RunFacts,
    ) -> Result<AttemptRow, TransitionRejection> {
        let next_number = self
            .store
            .attempts_for_run(run_id)?
            .iter()
            .map(|attempt| attempt.n)
            .max()
            .unwrap_or(0)
            + 1;

        Ok(AttemptRow {
            id: None,
            run_id,
            n: next_number,
            session_id: facts.session_id,
            session_attempt_id: facts.session_attempt_id,
            started_at: facts.now,
            ended_at: None,
            outcome: None,
            retry_trigger: facts.retry_trigger,
            tokens: None,
            cost_micros: None,
        })
    }

    fn provider_write(
        &self,
        transition: &RunTransition,
        provider: &str,
        facts: &RunFacts,
    ) -> Option<ProviderRow> {
        let (quota_state, reset_at) = if transition.effects.contains(&RunEffect::CapProvider) {
            (QuotaState::Capped, facts.quota_reset_at)
        } else if transition.effects.contains(&RunEffect::ClearProviderCap) {
            (QuotaState::Ok, None)
        } else {
            return None;
        };

        Some(ProviderRow {
            provider: provider.to_owned(),
            quota_state,
            reset_at,
            updated_at: facts.now,
        })
    }
}

/// The refusal a failed guard produces, so every check below names the guard it
/// is rather than restating the machine.
fn guard_refused(transition: &RunTransition, detail: String) -> TransitionRejection {
    TransitionRejection::GuardFailed {
        machine: "run",
        guard: transition.guard.as_str(),
        detail,
    }
}

/// A move only the named principal may report.
fn check_principal(
    transition: &RunTransition,
    principal: Principal,
    admitted: Principal,
    act: &str,
) -> Result<(), TransitionRejection> {
    if principal == admitted {
        Ok(())
    } else {
        Err(guard_refused(
            transition,
            format!("{} cannot {act}", principal.as_str()),
        ))
    }
}

/// Everything a run needs at once for the scheduler to start it.
fn check_admission(
    transition: &RunTransition,
    facts: &RunFacts,
) -> Result<(), TransitionRejection> {
    if facts.slot_available && facts.provider_serving && facts.worktree_ready {
        Ok(())
    } else {
        Err(guard_refused(
            transition,
            format!(
                "slot_available={}, provider_serving={}, worktree_ready={}",
                facts.slot_available, facts.provider_serving, facts.worktree_ready
            ),
        ))
    }
}

/// A run parks on a question it opened for itself, and the harness is the only
/// party that opens one.
fn check_ask_opens_question(
    transition: &RunTransition,
    run_id: i64,
    facts: &RunFacts,
) -> Result<(), TransitionRejection> {
    check_principal(
        transition,
        facts.principal,
        Principal::Coordinator,
        "report a run's own lifecycle facts",
    )?;

    match facts.opened_question.as_ref() {
        Some(question) if question.run_id == run_id => Ok(()),
        Some(question) => Err(guard_refused(
            transition,
            format!(
                "the question carries run {} rather than {run_id}",
                question.run_id
            ),
        )),
        None => Err(guard_refused(
            transition,
            "a run cannot park on awaiting_input without the question it parks on".to_owned(),
        )),
    }
}

/// The outcome a transition ends the run's open attempt with, if it ends one.
///
/// Read out of the declared effects rather than restated per transition, so the
/// table stays the only place that says which legs end an attempt and how.
fn closed_outcome(effects: &[RunEffect]) -> Option<AttemptOutcome> {
    effects.iter().find_map(|effect| match effect {
        RunEffect::CloseAttempt(outcome) => Some(*outcome),
        _ => None,
    })
}
