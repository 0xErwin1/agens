//! The question state machine.
//!
//! `open → answered → delivered`, with an author recorded on every answer, plus
//! `open | answered → expired` for an authorization that ran out.
//!
//! A merge authorization is a question of kind `approval`, bound to a run, an
//! action and an expiry. Three properties separate it from a plain question,
//! and each is a row or a guard here rather than a convention:
//!
//! - Only the user authorizes it. Praetor answering an approval is refused.
//! - It expires, and silence never authorizes: an expired approval leaves
//!   through `expired`, not through `answered`.
//! - It is not reusable. The table has no transition out of `delivered`, so a
//!   consumed authorization cannot be presented a second time.

use agens_store::{
    EventClass, QuestionAnswer, QuestionAuthor, QuestionChange, QuestionKind, QuestionRow,
    QuestionState, TransitionWrite,
};

use super::{
    AppliedTransition, JournaledMove, StateMachines, TransitionOutcome, TransitionRejection,
    event_pair, transition_events,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuestionTrigger {
    /// Somebody answered, or authorized.
    Answer,
    /// A safe point handed it to the worker.
    Deliver,
    /// The timer wheel found it past its expiry.
    Expire,
}

impl QuestionTrigger {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Answer => "answer",
            Self::Deliver => "deliver",
            Self::Expire => "expire",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuestionGuard {
    /// An answer with an author. Nothing is answered anonymously, because the
    /// answer reaches the worker as authority and the record of whose it was
    /// cannot be reconstructed later.
    AuthorRecorded,
    /// An authorization: the user's, and still in date.
    UserAuthorizationInDate,
    /// Still in date at the moment of delivery. An authorization that expired
    /// between being granted and being handed over authorizes nothing.
    NotExpired,
    /// Past its expiry.
    Expired,
}

impl QuestionGuard {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AuthorRecorded => "author_recorded",
            Self::UserAuthorizationInDate => "user_authorization_in_date",
            Self::NotExpired => "not_expired",
            Self::Expired => "expired",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuestionEffect {
    /// The answer joins the safe-point queue for the run's session.
    EnqueueForDelivery,
    /// The authorization is spent. There is no transition out of `delivered`,
    /// so this is the only time it is ever used.
    ConsumeAuthorization,
    /// The authorization is void and has to be asked for again.
    InvalidateAuthorization,
}

/// One row of the question transition table.
///
/// The kind is part of the key, not a field the guard inspects: an approval and
/// a question move differently enough that sharing rows would mean a guard that
/// silently does nothing for one of them.
pub struct QuestionTransition {
    pub kind: QuestionKind,
    pub from: QuestionState,
    pub trigger: QuestionTrigger,
    pub to: QuestionState,
    pub guard: QuestionGuard,
    pub effects: &'static [QuestionEffect],
    pub domain_event: &'static str,
    pub class: EventClass,
}

/// The question machine, as data.
pub static QUESTION_TRANSITIONS: &[QuestionTransition] = &[
    QuestionTransition {
        kind: QuestionKind::Question,
        from: QuestionState::Open,
        trigger: QuestionTrigger::Answer,
        to: QuestionState::Answered,
        guard: QuestionGuard::AuthorRecorded,
        effects: &[QuestionEffect::EnqueueForDelivery],
        domain_event: "question_answered",
        class: EventClass::Infra,
    },
    QuestionTransition {
        kind: QuestionKind::Question,
        from: QuestionState::Answered,
        trigger: QuestionTrigger::Deliver,
        to: QuestionState::Delivered,
        guard: QuestionGuard::NotExpired,
        effects: &[],
        domain_event: "question_delivered",
        class: EventClass::Infra,
    },
    QuestionTransition {
        kind: QuestionKind::Question,
        from: QuestionState::Open,
        trigger: QuestionTrigger::Expire,
        to: QuestionState::Expired,
        guard: QuestionGuard::Expired,
        effects: &[],
        domain_event: "question_expired",
        class: EventClass::Infra,
    },
    QuestionTransition {
        kind: QuestionKind::Approval,
        from: QuestionState::Open,
        trigger: QuestionTrigger::Answer,
        to: QuestionState::Answered,
        guard: QuestionGuard::UserAuthorizationInDate,
        effects: &[QuestionEffect::EnqueueForDelivery],
        domain_event: "approval_granted",
        class: EventClass::Infra,
    },
    QuestionTransition {
        kind: QuestionKind::Approval,
        from: QuestionState::Answered,
        trigger: QuestionTrigger::Deliver,
        to: QuestionState::Delivered,
        guard: QuestionGuard::NotExpired,
        effects: &[QuestionEffect::ConsumeAuthorization],
        domain_event: "approval_consumed",
        class: EventClass::Infra,
    },
    QuestionTransition {
        kind: QuestionKind::Approval,
        from: QuestionState::Open,
        trigger: QuestionTrigger::Expire,
        to: QuestionState::Expired,
        guard: QuestionGuard::Expired,
        effects: &[QuestionEffect::InvalidateAuthorization],
        domain_event: "approval_expired",
        class: EventClass::Infra,
    },
    QuestionTransition {
        kind: QuestionKind::Approval,
        from: QuestionState::Answered,
        trigger: QuestionTrigger::Expire,
        to: QuestionState::Expired,
        guard: QuestionGuard::Expired,
        effects: &[QuestionEffect::InvalidateAuthorization],
        domain_event: "approval_expired",
        class: EventClass::Infra,
    },
];

/// The answer being given, and when it is being given.
#[derive(Clone, Debug, Default)]
pub struct QuestionFacts {
    /// Epoch seconds.
    pub now: i64,
    pub answer: Option<String>,
    pub author: Option<QuestionAuthor>,
}

/// One applied question transition.
pub type AppliedQuestionTransition = AppliedTransition<QuestionState, QuestionEffect>;

impl StateMachines {
    /// Moves a question, or explains why it did not move.
    ///
    /// A caller that presents an already consumed authorization lands on
    /// [`TransitionRejection::NoSuchTransition`]: the table has no row out of
    /// `delivered`, which is what makes an approval single-use.
    pub fn apply_question(
        &mut self,
        question_id: i64,
        trigger: QuestionTrigger,
        facts: &QuestionFacts,
    ) -> Result<TransitionOutcome<QuestionState, QuestionEffect>, TransitionRejection> {
        let question = self.load_question(question_id)?;

        let transition = QUESTION_TRANSITIONS
            .iter()
            .find(|candidate| {
                candidate.kind == question.kind
                    && candidate.from == question.state
                    && candidate.trigger == trigger
            })
            .ok_or(TransitionRejection::NoSuchTransition {
                machine: "question",
                from: question.state.as_str(),
                trigger: trigger.as_str(),
            })?;

        check_question_guard(transition, &question, facts)?;

        let answer = (transition.to == QuestionState::Answered)
            .then(|| answer_write(facts))
            .transpose()?;

        let events = transition_events(
            &JournaledMove {
                run_id: question.run_id,
                ts: facts.now,
                class: transition.class,
                machine: "question",
                from: question.state.as_str(),
                to: transition.to.as_str(),
                trigger: trigger.as_str(),
                domain_event: transition.domain_event,
            },
            &serde_json::json!({
                "question_id": question_id,
                "kind": question.kind.as_str(),
                "author": facts.author.map(QuestionAuthor::as_str),
            }),
        );

        let outcome = self.store.apply_transition(&TransitionWrite {
            run_id: question.run_id,
            run_state: None,
            worktree_status: None,
            question: Some(QuestionChange {
                question_id,
                expected: question.state,
                next: transition.to,
                answer,
            }),
            new_question: None,
            attempt: None,
            provider: None,
            events: &events,
        })?;

        let (state_changed_event_id, domain_event_id) = event_pair(&outcome.event_ids)?;

        Ok(TransitionOutcome::Applied(AppliedQuestionTransition {
            from: question.state,
            to: transition.to,
            effects: transition.effects,
            domain_event: transition.domain_event,
            state_changed_event_id,
            domain_event_id,
            opened_question_id: None,
        }))
    }
}

fn check_question_guard(
    transition: &QuestionTransition,
    question: &QuestionRow,
    facts: &QuestionFacts,
) -> Result<(), TransitionRejection> {
    let refuse = |detail: String| {
        Err(TransitionRejection::GuardFailed {
            machine: "question",
            guard: transition.guard.as_str(),
            detail,
        })
    };

    let in_date = question.expires_at.is_none_or(|at| at > facts.now);

    match transition.guard {
        QuestionGuard::AuthorRecorded => {
            if answered(facts) {
                Ok(())
            } else {
                refuse("an answer needs both text and an author".to_owned())
            }
        }
        QuestionGuard::UserAuthorizationInDate => {
            if !answered(facts) {
                refuse("an authorization needs both text and an author".to_owned())
            } else if facts.author != Some(QuestionAuthor::User) {
                refuse("only the user authorizes a merge".to_owned())
            } else if in_date {
                Ok(())
            } else {
                refuse(format!(
                    "the authorization expired at {}",
                    question.expires_at.unwrap_or_default()
                ))
            }
        }
        QuestionGuard::NotExpired => {
            if in_date {
                Ok(())
            } else {
                refuse(format!(
                    "expired at {}, and silence never authorizes",
                    question.expires_at.unwrap_or_default()
                ))
            }
        }
        QuestionGuard::Expired => {
            if in_date {
                refuse("it has not expired".to_owned())
            } else {
                Ok(())
            }
        }
    }
}

fn answered(facts: &QuestionFacts) -> bool {
    facts.author.is_some() && facts.answer.as_ref().is_some_and(|text| !text.is_empty())
}

fn answer_write(facts: &QuestionFacts) -> Result<QuestionAnswer, TransitionRejection> {
    match (facts.answer.clone(), facts.author) {
        (Some(answer), Some(author)) => Ok(QuestionAnswer { answer, author }),
        _ => Err(TransitionRejection::GuardFailed {
            machine: "question",
            guard: QuestionGuard::AuthorRecorded.as_str(),
            detail: "an answer needs both text and an author".to_owned(),
        }),
    }
}
