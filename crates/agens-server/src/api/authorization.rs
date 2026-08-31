//! Who may ask for what.
//!
//! This is the reason the coordinator has one service core and not one per
//! transport. Authorization is a property of the operation and the principal,
//! never of the wire a request arrived on, so adding a facade cannot widen
//! anybody's authority: a new transport picks a principal and inherits exactly
//! the table below.
//!
//! The table is data for the same reason the state machines are. Reading which
//! principals reach an operation is reading one row, and adding an operation
//! without deciding its principals does not compile.

use agens_store::{QuestionKind, QuestionRow};

use crate::fsm::Principal;

/// One operation of the in-process API.
///
/// The Team plane moves the control plane; the Feed plane only reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    CreateRun,
    ApprovePlan,
    AnswerQuestion,
    AuthorizeMerge,
    CancelRun,
    Retry,
    Cleaning,
    Takeover,
    PauseAdmissions,
    Stop,
    Tree,
    RunDetail,
    Inbox,
    Subscribe,
}

impl Operation {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CreateRun => "create_run",
            Self::ApprovePlan => "approve_plan",
            Self::AnswerQuestion => "answer_question",
            Self::AuthorizeMerge => "authorize_merge",
            Self::CancelRun => "cancel_run",
            Self::Retry => "retry",
            Self::Cleaning => "cleaning",
            Self::Takeover => "takeover",
            Self::PauseAdmissions => "pause_admissions",
            Self::Stop => "stop",
            Self::Tree => "tree",
            Self::RunDetail => "run_detail",
            Self::Inbox => "inbox",
            Self::Subscribe => "subscribe",
        }
    }

    /// Which principals reach this operation at all.
    ///
    /// An operation missing from the table reaches nobody: the default is
    /// refusal, so forgetting a row closes an operation rather than opening it.
    #[must_use]
    pub fn principals(self) -> &'static [Principal] {
        OPERATION_AUTHORIZATION
            .iter()
            .find(|entry| entry.operation == self)
            .map_or(&[], |entry| entry.principals)
    }

    #[must_use]
    pub fn admits(self, principal: Principal) -> bool {
        self.principals().contains(&principal)
    }
}

/// One row of the authorization table.
pub struct OperationAuthorization {
    pub operation: Operation,
    pub principals: &'static [Principal],
    /// Why the list is what it is, in the terms the design uses.
    pub rationale: &'static str,
}

const USER_ONLY: &[Principal] = &[Principal::User];
const USER_AND_PRAETOR: &[Principal] = &[Principal::User, Principal::Praetor];

/// The authorization table.
///
/// [`Principal::Coordinator`] appears on no Team row. The coordinator's own
/// facts — ingest, the timer wheel, boot reconciliation — reach the state
/// machines directly and need no client surface; letting it in here would give
/// anything able to claim that principal a path around the user's authority.
pub static OPERATION_AUTHORIZATION: &[OperationAuthorization] = &[
    OperationAuthorization {
        operation: Operation::CreateRun,
        principals: USER_AND_PRAETOR,
        rationale: "proposing an execution is planning, and it authorizes nothing on its own: \
                    the run lands in draft and only the user's approval queues it",
    },
    OperationAuthorization {
        operation: Operation::ApprovePlan,
        principals: USER_ONLY,
        rationale: "approving an execution freezes a scope, and only the user does that",
    },
    OperationAuthorization {
        operation: Operation::AnswerQuestion,
        principals: USER_AND_PRAETOR,
        rationale: "Praetor answers detail questions; which ones is the detail-question policy",
    },
    OperationAuthorization {
        operation: Operation::AuthorizeMerge,
        principals: USER_ONLY,
        rationale: "the user approves bytes; nothing else can authorize a merge",
    },
    OperationAuthorization {
        operation: Operation::CancelRun,
        principals: USER_AND_PRAETOR,
        rationale: "stopping work takes no authority a manager does not have",
    },
    OperationAuthorization {
        operation: Operation::Retry,
        principals: USER_AND_PRAETOR,
        rationale: "a retry re-runs an already approved scope, and records who asked",
    },
    OperationAuthorization {
        operation: Operation::Cleaning,
        principals: USER_ONLY,
        rationale: "discarding a worktree is irreversible and needs a person's confirmation",
    },
    OperationAuthorization {
        operation: Operation::Takeover,
        principals: USER_ONLY,
        rationale: "taking over a session hands its authority to whoever holds it",
    },
    OperationAuthorization {
        operation: Operation::PauseAdmissions,
        principals: USER_ONLY,
        rationale: "admission is the operator's control over the machine's capacity",
    },
    OperationAuthorization {
        operation: Operation::Stop,
        principals: USER_ONLY,
        rationale: "stopping the team is the operator's control over the machine's capacity",
    },
    OperationAuthorization {
        operation: Operation::Tree,
        principals: USER_AND_PRAETOR,
        rationale: "the read plane changes nothing",
    },
    OperationAuthorization {
        operation: Operation::RunDetail,
        principals: USER_AND_PRAETOR,
        rationale: "the read plane changes nothing",
    },
    OperationAuthorization {
        operation: Operation::Inbox,
        principals: USER_AND_PRAETOR,
        rationale: "the read plane changes nothing",
    },
    OperationAuthorization {
        operation: Operation::Subscribe,
        principals: USER_AND_PRAETOR,
        rationale: "the read plane changes nothing",
    },
];

/// Why the detail-question policy refused an answer from Praetor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DetailQuestionRefusal {
    /// An authorization, which is the user's alone whatever it is worded as.
    IsAuthorization,
    /// The question offered no options, so answering it is a judgment call
    /// rather than a detail.
    OpenEnded,
    /// The answer is not one of the options the question offered.
    OutsideOptions,
    /// The options column did not hold a JSON array of strings, so nothing can
    /// be checked against it and the policy fails closed.
    UnreadableOptions,
}

impl DetailQuestionRefusal {
    #[must_use]
    pub const fn detail(self) -> &'static str {
        match self {
            Self::IsAuthorization => "an authorization is the user's alone",
            Self::OpenEnded => {
                "the question offered no options, so it is a decision rather than a detail"
            }
            Self::OutsideOptions => {
                "praetor answers within the options the question offered, and this answer is not \
                 one of them"
            }
            Self::UnreadableOptions => {
                "the question's options could not be read, and an unreadable question is not a \
                 detail question"
            }
        }
    }
}

/// Which questions Praetor may answer.
///
/// The design gives Praetor `team_answer` "limited to detail questions by
/// policy" without naming the policy, and the schema carries no flag saying
/// which questions those are. So the policy reads what a question already is:
/// a question that enumerated its options has a closed answer set, and picking
/// from it is a detail. A question with no options, or an answer outside the
/// set, is the open decision the design keeps escalating to the person —
/// memory conflicts over architecture, policy or decisions land there — and
/// Praetor does not get to close it.
///
/// The rule fails closed at every unknown, because this is the one seam between
/// a role exposed to third-party text and the authority to unblock a worker.
pub fn praetor_may_answer(
    question: &QuestionRow,
    answer: &str,
) -> Result<(), DetailQuestionRefusal> {
    if question.kind == QuestionKind::Approval {
        return Err(DetailQuestionRefusal::IsAuthorization);
    }

    let Ok(serde_json::Value::Array(options)) =
        serde_json::from_str::<serde_json::Value>(&question.options)
    else {
        return Err(DetailQuestionRefusal::UnreadableOptions);
    };

    if options.is_empty() {
        return Err(DetailQuestionRefusal::OpenEnded);
    }

    let offered = options.iter().any(|option| offers(option, answer));

    if offered {
        Ok(())
    } else {
        Err(DetailQuestionRefusal::OutsideOptions)
    }
}

/// Whether one stored option is the one that was answered.
///
/// An option is stored either as the bare string a client wrote or as the
/// `{id, label, consequence}` object a worker's `ask` writes, and the answer is
/// compared against the identifier in both. Reading only the bare form left
/// every question a worker actually asks with no answerable option at all,
/// which refused Praetor by accident rather than by policy.
///
/// Anything else is not an option this can recognize, and an option nobody can
/// recognize offers nothing.
fn offers(option: &serde_json::Value, answer: &str) -> bool {
    match option {
        serde_json::Value::String(id) => id == answer,
        serde_json::Value::Object(fields) => {
            fields.get("id").and_then(serde_json::Value::as_str) == Some(answer)
        }
        _ => false,
    }
}
