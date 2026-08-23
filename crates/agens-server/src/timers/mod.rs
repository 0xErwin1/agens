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
//! - A **missed first checkpoint** is the same exception, raised for a run that
//!   has never checkpointed at all. It is measured from the run starting rather
//!   than from a promise, because a worker that has said nothing has promised
//!   nothing, and without it such a worker holds its slot and its directory
//!   indefinitely.
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

use std::collections::BTreeMap;
use std::sync::{
    Arc, Mutex,
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

/// The domain event the run machine's admission journals. It is what the first
/// checkpoint's deadline is measured from.
const RUN_STARTED_EVENT: &str = "run_started";

/// The journal entry a refused stage of a tick is recorded as. Its payload
/// names the stage and what refused it.
pub const TIMER_STAGE_REJECTED_EVENT: &str = "timer_stage_rejected";

/// The checkpoint payload field carrying the epoch second the worker promised
/// its next checkpoint by. A checkpoint without it declares no deadline.
const PROMISED_AT_FIELD: &str = "promised_at";

/// The payload field of [`CHECKPOINT_OVERDUE_EVENT`] naming the entry it was
/// raised for: the checkpoint that went past its promise, or the admission a
/// run that has never checkpointed is measured from. It is what keeps the
/// signal to one per entry, across restarts.
const CHECKPOINT_ID_FIELD: &str = "checkpoint_id";

/// Share of the promised span a worker gets before its checkpoint is overdue,
/// as a percentage. The default is one and a half times what was promised:
/// conservative on purpose, since a wheel that cries early costs Praetor a
/// activation for a worker that was only slow.
pub const DEFAULT_CHECKPOINT_GRACE_PERCENT: i64 = 150;

/// How long a run has from the moment it starts executing to its first
/// checkpoint. Conservative on purpose: everything before the first checkpoint
/// is setup a worker cannot report on, and the deadline exists to bound a
/// worker that never reports at all rather than to hurry a slow one.
pub const DEFAULT_FIRST_CHECKPOINT_SECONDS: i64 = 3_600;

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
    /// `team.first_checkpoint_seconds`.
    pub first_checkpoint_seconds: i64,
    /// `team.quota_window_seconds`.
    pub quota_window_seconds: i64,
}

impl Default for TimerSettings {
    fn default() -> Self {
        Self {
            checkpoint_grace_percent: DEFAULT_CHECKPOINT_GRACE_PERCENT,
            first_checkpoint_seconds: DEFAULT_FIRST_CHECKPOINT_SECONDS,
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
    /// The journal entry the deadline was measured from: the checkpoint that
    /// went past its promise, or the admission of a run that has never
    /// checkpointed.
    pub checkpoint_event_id: i64,
    /// `None` for a run that has never checkpointed, which promised nothing.
    pub promised_at: Option<i64>,
    /// `promised_at` plus its share of grace, or the run starting plus the
    /// first-checkpoint span.
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
    /// The stages that were refused, in the order they ran. Empty on an
    /// ordinary tick.
    pub rejections: Vec<RejectedStage>,
}

/// One of the three things a tick does, named so a refusal can say which.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TimerStage {
    QuotaResets,
    ExpiredQuestions,
    OverdueCheckpoints,
}

impl TimerStage {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::QuotaResets => "quota_resets",
            Self::ExpiredQuestions => "expired_questions",
            Self::OverdueCheckpoints => "overdue_checkpoints",
        }
    }
}

/// A stage that was refused, and by what.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RejectedStage {
    pub stage: TimerStage,
    pub rejection: TransitionRejection,
    /// The journal entry that recorded it, or `None` when the journal itself
    /// could not be written.
    pub event_id: Option<i64>,
}

/// What the wheel has already written down about a stage that is not running.
#[derive(Clone, Debug)]
struct StandingRejection {
    reason: String,
    /// The entry that recorded it, or `None` when the journal itself could not
    /// be written.
    event_id: Option<i64>,
}

/// The single wheel.
pub struct TimerWheel {
    clock: TimerClock,
    settings: TimerSettings,
    /// The refusal standing against each stage, so a condition that holds is
    /// journaled when it starts rather than on every tick it keeps holding.
    ///
    /// The one thing the wheel keeps between ticks, and it decides nothing: no
    /// deadline is held here, and losing it to a restart costs one repeated
    /// entry.
    standing: Mutex<BTreeMap<TimerStage, StandingRejection>>,
}

impl TimerWheel {
    #[must_use]
    pub fn new(settings: TimerSettings) -> Self {
        Self {
            clock: TimerClock::System,
            settings,
            standing: Mutex::new(BTreeMap::new()),
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
                standing: Mutex::new(BTreeMap::new()),
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
    /// The three stages are independent, and a stage that is refused does not
    /// take the other two with it. They read different tables and answer to
    /// different deadlines: a run that left `awaiting_quota` between the read
    /// and the apply says nothing about the questions that ran out or the
    /// checkpoints that went overdue, and letting it decide their fate would
    /// suspend every mechanical deadline the machine has for as long as the
    /// row disagreed.
    ///
    /// A refusal is journaled rather than dropped, once per occurrence, and
    /// carried back in [`TimerTick::rejections`]: this is the wheel's only
    /// reader, and a rejection it swallowed would leave a wheel that looks
    /// healthy and applies nothing.
    pub fn tick(&self, machines: &mut StateMachines) -> TimerTick {
        let now = self.clock.now();
        let mut tick = TimerTick {
            now,
            ..TimerTick::default()
        };

        match self.lift_elapsed_quota_caps(machines, now) {
            Ok(resets) => {
                tick.quota_resets = resets;
                self.stage_ran(TimerStage::QuotaResets);
            }
            Err(rejection) => {
                tick.rejections.push(self.record_rejection(
                    machines,
                    TimerStage::QuotaResets,
                    rejection,
                    now,
                ));
            }
        }

        match void_expired_questions(machines, now) {
            Ok(expired) => {
                tick.expired_questions = expired;
                self.stage_ran(TimerStage::ExpiredQuestions);
            }
            Err(rejection) => {
                tick.rejections.push(self.record_rejection(
                    machines,
                    TimerStage::ExpiredQuestions,
                    rejection,
                    now,
                ));
            }
        }

        match self.raise_overdue_checkpoints(machines, now) {
            Ok(overdue) => {
                tick.overdue_checkpoints = overdue;
                self.stage_ran(TimerStage::OverdueCheckpoints);
            }
            Err(rejection) => {
                tick.rejections.push(self.record_rejection(
                    machines,
                    TimerStage::OverdueCheckpoints,
                    rejection,
                    now,
                ));
            }
        }

        tick
    }

    /// Journals one refused stage and returns it for the caller to carry.
    ///
    /// Only what changed reaches the journal. The wheel ticks four times a
    /// second and a condition that refuses a stage refuses it on every one of
    /// those ticks, so an entry per occurrence would bury the feed under the
    /// same sentence. A refusal that already stands is carried back pointing at
    /// the entry that recorded it.
    ///
    /// A journal that cannot be written is recorded as such rather than turned
    /// into a second failure: the rejection is what the caller has to hear
    /// about, and losing it because the record of it could not be stored would
    /// be the same silence this exists to end. Nothing stands after that, so
    /// the next tick tries the entry again.
    fn record_rejection(
        &self,
        machines: &mut StateMachines,
        stage: TimerStage,
        rejection: TransitionRejection,
        now: i64,
    ) -> RejectedStage {
        let reason = rejection.to_string();

        if let Some(standing) = self.standing_rejection(stage, &reason) {
            return RejectedStage {
                stage,
                rejection,
                event_id: standing.event_id,
            };
        }

        let payload = serde_json::json!({
            "stage": stage.as_str(),
            "reason": reason,
        });

        let event_id = machines
            .journal(&[EventRow {
                id: None,
                run_id: None,
                event_type: TIMER_STAGE_REJECTED_EVENT.to_owned(),
                class: EventClass::Infra,
                payload: payload.to_string(),
                ts: now,
            }])
            .ok()
            .and_then(|ids| ids.first().copied());

        if let Ok(mut standing) = self.standing.lock()
            && event_id.is_some()
        {
            standing.insert(stage, StandingRejection { reason, event_id });
        }

        RejectedStage {
            stage,
            rejection,
            event_id,
        }
    }

    /// The refusal already recorded for `stage`, when it is this same one.
    fn standing_rejection(&self, stage: TimerStage, reason: &str) -> Option<StandingRejection> {
        self.standing
            .lock()
            .ok()?
            .get(&stage)
            .filter(|standing| standing.reason == reason)
            .cloned()
    }

    /// A stage that ran ends whatever was standing against it, so the same
    /// condition arriving again is a new one with its own moment.
    fn stage_ran(&self, stage: TimerStage) {
        if let Ok(mut standing) = self.standing.lock() {
            standing.remove(&stage);
        }
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

        let (checkpoint_event_id, promised_at, deadline) = match checkpoints.last() {
            Some(latest) => {
                let (Some(checkpoint_event_id), Some(promised_at)) =
                    (latest.id, promised_at(&latest.payload))
                else {
                    return Ok(None);
                };
                let Some(deadline) = self.deadline(latest.ts, promised_at) else {
                    return Ok(None);
                };

                (checkpoint_event_id, Some(promised_at), deadline)
            }
            None => match self.first_checkpoint_deadline(machines, run_id)? {
                Some((started_event_id, deadline)) => (started_event_id, None, deadline),
                None => return Ok(None),
            },
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
            // A first checkpoint that never arrived is measured from the run
            // starting rather than from a promise, so the entry says which of
            // the two deadlines passed.
            "first_checkpoint": promised_at.is_none(),
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

    /// The deadline a run that has never checkpointed is measured against, and
    /// the entry it is measured from.
    ///
    /// It runs from the moment the run started executing, because that is the
    /// last thing the control plane knows about a worker that has said nothing
    /// since. Without it a worker that never checkpoints is never reported
    /// lost, and goes on holding its slot and its directory for as long as the
    /// daemon lives.
    ///
    /// A run with no `run_started` entry has not executed, so there is nothing
    /// to be late for.
    fn first_checkpoint_deadline(
        &self,
        machines: &StateMachines,
        run_id: i64,
    ) -> Result<Option<(i64, i64)>, TransitionRejection> {
        let started = machines
            .store()
            .events_of_type_for_run(run_id, RUN_STARTED_EVENT)?;

        let Some(latest) = started.last() else {
            return Ok(None);
        };
        let Some(event_id) = latest.id else {
            return Ok(None);
        };
        let Some(deadline) = latest
            .ts
            .checked_add(self.settings.first_checkpoint_seconds)
        else {
            return Ok(None);
        };

        Ok(Some((event_id, deadline)))
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
