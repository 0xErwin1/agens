//! The coordinator's timer wheel.
//!
//! One wheel, and it holds no state of its own. Every tick recomputes what is
//! due by reading SQLite: the promised time of each running run's last
//! checkpoint, the reset time of each capped provider, and the expiry of each
//! question still waiting. Nothing is scheduled in memory, so a restart loses
//! no deadline and a wheel that has just started is indistinguishable from one
//! that has been running for a week.
//!
//! Three kinds of expiry, and they do not mean the same thing when they fire:
//!
//! - A **quota reset** and an **expired approval** are mechanical and exact.
//!   They apply a transition through the state machines and are done; no model
//!   is involved, which is what lets a team recover while every provider is
//!   capped.
//! - An **overdue checkpoint** is an exception raised for Praetor to judge. The
//!   wheel does not decide what to do about it: it journals the signal once and
//!   hands the caller the context to activate Praetor with. Once, not once per
//!   tick — a wheel that appended a row every time it looked would bury the
//!   feed it is trying to draw attention in, and the journal entry is also what
//!   makes the deduplication survive a restart.
//!
//! Time comes from an injected clock so a test can decide what "now" is; the
//! production clock is the system's, in epoch seconds, matching every timestamp
//! the control-plane tables store.

use std::sync::{
    Arc,
    atomic::{AtomicI64, Ordering},
};
use std::time::{SystemTime, UNIX_EPOCH};

use agens_store::{EventClass, EventRow, QuestionKind, QuestionState, RunState};

use crate::fsm::{
    AppliedQuestionTransition, AppliedRunTransition, QUESTION_TRANSITIONS, QuestionFacts,
    QuestionTrigger, RunFacts, RunTrigger, StateMachines, TransitionOutcome, TransitionRejection,
};

/// The journal entry a worker's checkpoint is recorded as.
pub const CHECKPOINT_EVENT: &str = "checkpoint";

/// The journal entry that says a checkpoint's deadline passed without a new
/// one. One per overdue checkpoint, and its payload names which.
pub const CHECKPOINT_OVERDUE_EVENT: &str = "checkpoint_overdue";

/// The checkpoint payload field carrying the epoch second the worker promised
/// its next checkpoint by. A checkpoint without it declares no deadline.
const PROMISED_AT_FIELD: &str = "promised_at";

/// The payload field of [`CHECKPOINT_OVERDUE_EVENT`] naming the checkpoint it
/// was raised for. It is what keeps the signal to one per checkpoint.
const CHECKPOINT_ID_FIELD: &str = "checkpoint_id";

/// Share of the promised span a worker gets before its checkpoint is overdue,
/// as a percentage. The default is one and a half times what was promised:
/// conservative on purpose, since a wheel that cries early costs Praetor a
/// activation for a worker that was only slow.
pub const DEFAULT_CHECKPOINT_GRACE_PERCENT: i64 = 150;

/// How long a cap the provider named no reset for is honoured before the
/// provider is tried again. The run costs no retry budget either way, so being
/// early is paid for in one refused request.
pub const DEFAULT_QUOTA_WINDOW_SECONDS: i64 = 900;

/// What the wheel needs configured. Deliberately not read from configuration
/// here: this crate owns the daemon, and resolving hand-authored TOML belongs
/// to the configuration crate and the composition root that wires the two.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimerSettings {
    /// `team.checkpoint_grace_percent`.
    pub checkpoint_grace_percent: i64,
    /// `team.quota_window_seconds`.
    pub quota_window_seconds: i64,
}

impl Default for TimerSettings {
    fn default() -> Self {
        Self {
            checkpoint_grace_percent: DEFAULT_CHECKPOINT_GRACE_PERCENT,
            quota_window_seconds: DEFAULT_QUOTA_WINDOW_SECONDS,
        }
    }
}

/// Source of "now", in epoch seconds.
///
/// Production is always `System`: the manual variant is unreachable without a
/// [`ManualTimerClock`], and the only way to obtain one is
/// [`TimerWheel::with_manual_clock_for_test`].
#[derive(Clone, Debug, Default)]
enum TimerClock {
    #[default]
    System,
    Manual(Arc<AtomicI64>),
}

impl TimerClock {
    fn now(&self) -> i64 {
        match self {
            Self::System => SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(0)),
            Self::Manual(state) => state.load(Ordering::Acquire),
        }
    }
}

/// Moves the clock of a wheel built by
/// [`TimerWheel::with_manual_clock_for_test`].
#[derive(Clone, Debug)]
pub struct ManualTimerClock {
    state: Arc<AtomicI64>,
}

impl ManualTimerClock {
    pub fn set(&self, epoch_seconds: i64) {
        self.state.store(epoch_seconds, Ordering::Release);
    }

    pub fn advance(&self, seconds: i64) {
        self.state.fetch_add(seconds, Ordering::AcqRel);
    }
}

/// One run woken because its provider is serving again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuotaReset {
    pub provider: String,
    pub run_id: i64,
    pub transition: AppliedRunTransition,
}

/// One question or authorization voided by its own expiry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpiredQuestion {
    pub question_id: i64,
    pub run_id: i64,
    pub kind: QuestionKind,
    pub transition: AppliedQuestionTransition,
}

/// One exception signal for Praetor: a run that promised a checkpoint by a
/// moment that has passed.
///
/// The wheel raises it and stops there. Deciding what an overdue worker needs
/// is judgment over the run's own words, and the coordinator applies no
/// judgment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverdueCheckpoint {
    pub run_id: i64,
    /// The journal entry of the checkpoint that went overdue.
    pub checkpoint_event_id: i64,
    pub promised_at: i64,
    /// `promised_at` plus its share of grace.
    pub deadline: i64,
    /// The journal entry the wheel wrote, which is also what keeps this signal
    /// from being raised again for the same checkpoint.
    pub signal_event_id: i64,
}

/// Everything one tick found due, and what it did about it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TimerTick {
    /// The instant the whole tick was computed against. One reading of the
    /// clock per tick, so two deadlines a second apart cannot disagree about
    /// which side of "now" they fell on.
    pub now: i64,
    pub quota_resets: Vec<QuotaReset>,
    pub expired_questions: Vec<ExpiredQuestion>,
    pub overdue_checkpoints: Vec<OverdueCheckpoint>,
}

/// The single wheel.
pub struct TimerWheel {
    clock: TimerClock,
    settings: TimerSettings,
}

impl TimerWheel {
    #[must_use]
    pub fn new(settings: TimerSettings) -> Self {
        Self {
            clock: TimerClock::System,
            settings,
        }
    }

    /// A wheel whose clock the caller drives. Test-only by construction: the
    /// manual clock variant has no other constructor.
    #[must_use]
    pub fn with_manual_clock_for_test(
        settings: TimerSettings,
        start: i64,
    ) -> (Self, ManualTimerClock) {
        let state = Arc::new(AtomicI64::new(start));

        (
            Self {
                clock: TimerClock::Manual(Arc::clone(&state)),
                settings,
            },
            ManualTimerClock { state },
        )
    }

    #[must_use]
    pub fn now(&self) -> i64 {
        self.clock.now()
    }

    /// Recomputes every deadline from the database and applies what is due.
    ///
    /// A rejection is propagated rather than skipped. The wheel re-derives each
    /// guard's facts from the same rows the guard reads, and the coordinator is
    /// the only writer of those rows, so a refused transition here means an
    /// invariant broke and the next tick would refuse it again.
    pub fn tick(&self, machines: &mut StateMachines) -> Result<TimerTick, TransitionRejection> {
        let now = self.clock.now();

        Ok(TimerTick {
            now,
            quota_resets: self.lift_elapsed_quota_caps(machines, now)?,
            expired_questions: void_expired_questions(machines, now)?,
            overdue_checkpoints: self.raise_overdue_checkpoints(machines, now)?,
        })
    }

    /// Requeues every run parked on a provider whose reset has arrived.
    ///
    /// The first run of a provider clears its cap as a transition effect, and
    /// the guard accepts a provider that is already serving, so the whole group
    /// wakes in the tick that found the reset rather than one run per tick.
    fn lift_elapsed_quota_caps(
        &self,
        machines: &mut StateMachines,
        now: i64,
    ) -> Result<Vec<QuotaReset>, TransitionRejection> {
        let due: Vec<String> = machines
            .store()
            .providers_due(now, Some(self.settings.quota_window_seconds))?
            .into_iter()
            .map(|provider| provider.provider)
            .collect();

        if due.is_empty() {
            return Ok(Vec::new());
        }

        let parked = machines.store().runs_in_state(RunState::AwaitingQuota)?;
        let mut resets = Vec::new();

        for run in parked {
            let Some(run_id) = run.id else {
                continue;
            };

            if !due.contains(&run.provider) {
                continue;
            }

            let facts = RunFacts {
                now,
                quota_window_seconds: Some(self.settings.quota_window_seconds),
                ..RunFacts::default()
            };

            if let TransitionOutcome::Applied(transition) =
                machines.apply_run(run_id, RunTrigger::QuotaReset, &facts)?
            {
                resets.push(QuotaReset {
                    provider: run.provider,
                    run_id,
                    transition,
                });
            }
        }

        Ok(resets)
    }

    /// Journals an exception signal for every running run whose last checkpoint
    /// promised a moment that has passed.
    fn raise_overdue_checkpoints(
        &self,
        machines: &mut StateMachines,
        now: i64,
    ) -> Result<Vec<OverdueCheckpoint>, TransitionRejection> {
        let working = machines.store().runs_in_state(RunState::Running)?;
        let mut overdue = Vec::new();

        for run in working {
            let Some(run_id) = run.id else {
                continue;
            };

            let Some(signal) = self.overdue_signal(machines, run_id, now)? else {
                continue;
            };

            overdue.push(signal);
        }

        Ok(overdue)
    }

    /// The signal one run owes, if its last checkpoint is past its deadline and
    /// nothing has said so yet.
    fn overdue_signal(
        &self,
        machines: &mut StateMachines,
        run_id: i64,
        now: i64,
    ) -> Result<Option<OverdueCheckpoint>, TransitionRejection> {
        let checkpoints = machines
            .store()
            .events_of_type_for_run(run_id, CHECKPOINT_EVENT)?;

        let Some(latest) = checkpoints.last() else {
            return Ok(None);
        };
        let Some(checkpoint_event_id) = latest.id else {
            return Ok(None);
        };
        let Some(promised_at) = promised_at(&latest.payload) else {
            return Ok(None);
        };
        let Some(deadline) = self.deadline(latest.ts, promised_at) else {
            return Ok(None);
        };

        if now < deadline {
            return Ok(None);
        }

        if already_signalled(machines, run_id, checkpoint_event_id)? {
            return Ok(None);
        }

        let payload = serde_json::json!({
            CHECKPOINT_ID_FIELD: checkpoint_event_id,
            "promised_at": promised_at,
            "deadline": deadline,
            "grace_percent": self.settings.checkpoint_grace_percent,
        });

        let journaled = machines.journal(&[EventRow {
            id: None,
            run_id: Some(run_id),
            event_type: CHECKPOINT_OVERDUE_EVENT.to_owned(),
            class: EventClass::Infra,
            payload: payload.to_string(),
            ts: now,
        }])?;

        let Some(signal_event_id) = journaled.first().copied() else {
            return Err(TransitionRejection::Storage(
                "the overdue signal was journaled without an id".to_owned(),
            ));
        };

        Ok(Some(OverdueCheckpoint {
            run_id,
            checkpoint_event_id,
            promised_at,
            deadline,
            signal_event_id,
        }))
    }

    /// When a checkpoint stops being merely late and becomes overdue.
    ///
    /// The grace is a share of what the worker itself promised, not a fixed
    /// span: a worker that promised five minutes and a worker that promised an
    /// hour are not equally late at the same absolute delay. `None` when the
    /// checkpoint promised nothing measurable, which leaves the run without a
    /// mechanical deadline rather than giving it one it never agreed to.
    fn deadline(&self, checkpoint_ts: i64, promised_at: i64) -> Option<i64> {
        let span = i128::from(promised_at.checked_sub(checkpoint_ts)?);
        if span <= 0 {
            return None;
        }

        let granted = span
            .checked_mul(i128::from(self.settings.checkpoint_grace_percent))?
            .checked_div(100)?;

        i64::try_from(i128::from(checkpoint_ts).checked_add(granted)?).ok()
    }
}

/// Voids every question and authorization whose expiry has arrived.
///
/// Silence never authorizes: an approval that ran out leaves through `expired`,
/// with no answer and nothing consumed. A row the transition table has no way
/// to expire — an answered plain question, say — is left exactly where it is
/// rather than asked to move and refused.
fn void_expired_questions(
    machines: &mut StateMachines,
    now: i64,
) -> Result<Vec<ExpiredQuestion>, TransitionRejection> {
    let mut expired = Vec::new();

    for question in machines.store().questions_past_expiry(now)? {
        let Some(question_id) = question.id else {
            continue;
        };

        if !expirable(question.kind, question.state) {
            continue;
        }

        let facts = QuestionFacts {
            now,
            ..QuestionFacts::default()
        };

        if let TransitionOutcome::Applied(transition) =
            machines.apply_question(question_id, QuestionTrigger::Expire, &facts)?
        {
            expired.push(ExpiredQuestion {
                question_id,
                run_id: question.run_id,
                kind: question.kind,
                transition,
            });
        }
    }

    Ok(expired)
}

/// Whether the question transition table has a row that expires this kind out
/// of this state. Read from the table rather than restated, so a row added
/// there is honoured here without a second list to keep in step.
fn expirable(kind: QuestionKind, state: QuestionState) -> bool {
    QUESTION_TRANSITIONS.iter().any(|transition| {
        transition.kind == kind
            && transition.from == state
            && transition.trigger == QuestionTrigger::Expire
    })
}

fn already_signalled(
    machines: &StateMachines,
    run_id: i64,
    checkpoint_event_id: i64,
) -> Result<bool, TransitionRejection> {
    let raised = machines
        .store()
        .events_of_type_for_run(run_id, CHECKPOINT_OVERDUE_EVENT)?;

    Ok(raised
        .iter()
        .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.payload).ok())
        .filter_map(|payload| payload.get(CHECKPOINT_ID_FIELD).and_then(|id| id.as_i64()))
        .any(|id| id == checkpoint_event_id))
}

/// The promised moment a checkpoint declared, if it declared one.
///
/// The payload is written by the harness, so a missing or malformed field is
/// ordinary input rather than a defect: it leaves the run without a mechanical
/// deadline, which is the same place a run sits before its first checkpoint.
fn promised_at(payload: &str) -> Option<i64> {
    serde_json::from_str::<serde_json::Value>(payload)
        .ok()?
        .get(PROMISED_AT_FIELD)?
        .as_i64()
}
