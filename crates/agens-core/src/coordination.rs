//! What Praetor asks the coordinator for, and what it is told back.
//!
//! This is the contract behind the `team_*` tools. Praetor manages a team; it
//! does not execute one. Every request here is either a read of the control
//! plane or an instruction the coordinator carries out, and the split is
//! enforced by what the payloads can say rather than by what a caller promises:
//!
//! - **No path crosses this seam, in either direction.** No request names a
//!   file, a directory, a repository or a worktree, and no receipt hands one
//!   back. A `team_*` tool therefore has nothing to open, nothing to write and
//!   nowhere to write it, and the repository a request applies to is the one
//!   the implementation is bound to rather than one a caller names.
//! - **No revision, branch or git argument crosses it either.** Merging,
//!   reclaiming and spawning are requests: they say which run, and the
//!   coordinator decides what git that implies. A tool that could name a
//!   revision would be composing a git invocation one layer away from running
//!   it.
//! - **Nothing here authorizes bytes.** [`CoordinationPort::request_merge`]
//!   opens the question the user answers. There is no method that grants one,
//!   because the authority to land code is the person's and a facade cannot
//!   hold what the authorization table never gives it.
//!
//! Nothing reads a clock, the same discipline [`crate::run_introspection`]
//! keeps: timestamps belong to the implementation, so a coordinator replaying
//! or reconciling decides what "now" means.

use crate::run_introspection::{Ask, AskError, AskOption};

pub const MAX_TASK_CHARS: usize = 2_048;
pub const MAX_SCOPE_CHARS: usize = 8_192;
pub const MAX_DOD_CHARS: usize = 8_192;
pub const MAX_ANSWER_CHARS: usize = 4_096;
pub const MAX_GUIDANCE_CHARS: usize = 8_192;
pub const MAX_DIRECTIVE_CHARS: usize = 8_192;
pub const MAX_REASON_CHARS: usize = 2_048;
pub const MAX_SPAWN_CHARS: usize = 16 * 1_024;

/// Why a request was refused before it ever reached the control plane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CoordinationRequestError {
    EmptyField(&'static str),
    FieldTooLong(&'static str),
    ControlCharacter(&'static str),
    /// An identifier that has to name a row, given as zero or a negative
    /// number.
    NotAnIdentifier(&'static str),
    RequestTooLarge,
    /// The escalated question itself did not hold together.
    Question(AskError),
}

impl From<AskError> for CoordinationRequestError {
    fn from(error: AskError) -> Self {
        Self::Question(error)
    }
}

/// Why the coordinator did not carry a request out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CoordinationError {
    /// The session is not managing a team, so there is no control plane to
    /// reach. Its own variant because it is not a failure: it is a `team_*`
    /// tool called outside team mode.
    NoTeam,
    /// The authorization table does not give Praetor this, or does not give it
    /// for this particular subject. Separate from [`Self::Refused`] because a
    /// caller must not retry it: nothing about the request changes the answer.
    Unauthorized(String),
    /// The control plane refused the move — a run that already went somewhere
    /// else, a transition its state has no path for.
    Refused(String),
    NotFound(String),
    /// The control plane could not be reached at all.
    Unavailable,
}

impl std::fmt::Display for CoordinationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoTeam => formatter.write_str("this session is not managing a team"),
            Self::Unauthorized(detail) | Self::Refused(detail) | Self::NotFound(detail) => {
                formatter.write_str(detail)
            }
            Self::Unavailable => formatter.write_str("the control plane is unavailable"),
        }
    }
}

impl std::error::Error for CoordinationError {}

/// One run, as the team board shows it.
///
/// The state is carried as the control plane's own word for it rather than as
/// an enum of this crate's, because the consumer is a model reading a
/// projection and a second vocabulary would be one more thing to keep in step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeamRun {
    pub run_id: i64,
    pub task: String,
    pub state: String,
    pub priority: i64,
    pub worktree_status: Option<String>,
    pub parent_run_id: Option<i64>,
    pub created_at: i64,
}

/// One thing waiting on an answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeamQuestion {
    pub question_id: i64,
    pub run_id: i64,
    /// `question` or `approval`. An approval is the user's alone, and Praetor
    /// sees it here so it knows what it is waiting on rather than to answer it.
    pub kind: String,
    pub blocked_decision: String,
    pub options: Vec<AskOption>,
    pub recommendation: Option<String>,
    pub expires_at: Option<i64>,
}

/// The whole team of the repository this session manages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeamStatus {
    pub repo_id: String,
    pub runs: Vec<TeamRun>,
    pub open_questions: Vec<TeamQuestion>,
}

/// One try at a run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeamAttempt {
    pub attempt: i64,
    pub outcome: Option<String>,
    pub retry_trigger: Option<String>,
    pub started_at: i64,
    pub ended_at: Option<i64>,
}

/// One claim a worker reported, with how well it is backed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeamFinding {
    pub description: String,
    pub evidence_class: String,
    pub causal_disposition: String,
    pub created_at: i64,
}

/// What the coordinator derived about a run's progress.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeamHealth {
    pub noop_turns: i64,
    pub last_progress_turn: Option<i64>,
    pub tokens_since_progress: i64,
}

/// One run in full, as far as the control plane recorded it.
///
/// It carries no worktree path. What a run changed is read through the
/// read-only tools Praetor already has, against the repository it manages, and
/// handing a manager a worker's directory would be handing it the one argument
/// every writing tool needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunReport {
    pub run: TeamRun,
    pub scope: String,
    pub dod: String,
    pub provider: String,
    pub result: Option<String>,
    pub attempts: Vec<TeamAttempt>,
    pub questions: Vec<TeamQuestion>,
    pub findings: Vec<TeamFinding>,
    pub health: Option<TeamHealth>,
}

/// A run proposed for the team.
///
/// It names no repository: one is proposed for the team this session manages,
/// and a caller that could name another would be reaching a project nobody
/// gave it. It names no start point either — where a branch begins is git, and
/// git is the coordinator's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpawnRequest {
    task: String,
    scope: String,
    dod: String,
    priority: i64,
    parent_run_id: Option<i64>,
    dep_run_id: Option<i64>,
}

impl SpawnRequest {
    pub fn new(
        task: String,
        scope: String,
        dod: String,
        priority: i64,
        parent_run_id: Option<i64>,
        dep_run_id: Option<i64>,
    ) -> Result<Self, CoordinationRequestError> {
        check_required(&task, "task", MAX_TASK_CHARS)?;
        check_required(&scope, "scope", MAX_SCOPE_CHARS)?;
        check_required(&dod, "dod", MAX_DOD_CHARS)?;
        check_identifier(parent_run_id, "parent_run_id")?;
        check_identifier(dep_run_id, "dep_run_id")?;

        let aggregate = task.chars().count() + scope.chars().count() + dod.chars().count();

        if aggregate > MAX_SPAWN_CHARS {
            return Err(CoordinationRequestError::RequestTooLarge);
        }

        Ok(Self {
            task,
            scope,
            dod,
            priority,
            parent_run_id,
            dep_run_id,
        })
    }

    #[must_use]
    pub fn task(&self) -> &str {
        &self.task
    }

    #[must_use]
    pub fn scope(&self) -> &str {
        &self.scope
    }

    #[must_use]
    pub fn dod(&self) -> &str {
        &self.dod
    }

    #[must_use]
    pub const fn priority(&self) -> i64 {
        self.priority
    }

    #[must_use]
    pub const fn parent_run_id(&self) -> Option<i64> {
        self.parent_run_id
    }

    #[must_use]
    pub const fn dep_run_id(&self) -> Option<i64> {
        self.dep_run_id
    }
}

/// An answer to a question a run is blocked on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnswerRequest {
    question_id: i64,
    answer: String,
}

impl AnswerRequest {
    pub fn new(question_id: i64, answer: String) -> Result<Self, CoordinationRequestError> {
        check_required_identifier(question_id, "question_id")?;
        check_required(&answer, "answer", MAX_ANSWER_CHARS)?;

        Ok(Self {
            question_id,
            answer,
        })
    }

    #[must_use]
    pub const fn question_id(&self) -> i64 {
        self.question_id
    }

    #[must_use]
    pub fn answer(&self) -> &str {
        &self.answer
    }
}

/// A decision Praetor is handing to the person, on a run that is waiting for
/// it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EscalateRequest {
    run_id: i64,
    question: Ask,
}

impl EscalateRequest {
    pub fn new(run_id: i64, question: Ask) -> Result<Self, CoordinationRequestError> {
        check_required_identifier(run_id, "run_id")?;

        Ok(Self { run_id, question })
    }

    #[must_use]
    pub const fn run_id(&self) -> i64 {
        self.run_id
    }

    #[must_use]
    pub const fn question(&self) -> &Ask {
        &self.question
    }
}

/// Guidance that moves what a run is doing, delivered at its next safe point.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectRequest {
    run_id: i64,
    directive: String,
}

impl DirectRequest {
    pub fn new(run_id: i64, directive: String) -> Result<Self, CoordinationRequestError> {
        check_required_identifier(run_id, "run_id")?;
        check_required(&directive, "directive", MAX_DIRECTIVE_CHARS)?;

        Ok(Self { run_id, directive })
    }

    #[must_use]
    pub const fn run_id(&self) -> i64 {
        self.run_id
    }

    #[must_use]
    pub fn directive(&self) -> &str {
        &self.directive
    }
}

/// Another attempt at a scope the user already approved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetryRequest {
    run_id: i64,
    guidance: String,
}

impl RetryRequest {
    pub fn new(run_id: i64, guidance: String) -> Result<Self, CoordinationRequestError> {
        check_required_identifier(run_id, "run_id")?;
        check_required(&guidance, "guidance", MAX_GUIDANCE_CHARS)?;

        Ok(Self { run_id, guidance })
    }

    #[must_use]
    pub const fn run_id(&self) -> i64 {
        self.run_id
    }

    #[must_use]
    pub fn guidance(&self) -> &str {
        &self.guidance
    }
}

/// Stopping a run, with why it is being stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CancelRequest {
    run_id: i64,
    reason: String,
}

impl CancelRequest {
    pub fn new(run_id: i64, reason: String) -> Result<Self, CoordinationRequestError> {
        check_required_identifier(run_id, "run_id")?;
        check_required(&reason, "reason", MAX_REASON_CHARS)?;

        Ok(Self { run_id, reason })
    }

    #[must_use]
    pub const fn run_id(&self) -> i64 {
        self.run_id
    }

    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

/// A run whose work Praetor believes is ready to land.
///
/// It carries no receipt, no tree and no branch. What is frozen for the user to
/// authorize is derived from git by the coordinator at the moment the question
/// is opened, so a request cannot describe bytes other than the ones that are
/// actually there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeRequest {
    run_id: i64,
    reason: String,
}

impl MergeRequest {
    pub fn new(run_id: i64, reason: String) -> Result<Self, CoordinationRequestError> {
        check_required_identifier(run_id, "run_id")?;
        check_required(&reason, "reason", MAX_REASON_CHARS)?;

        Ok(Self { run_id, reason })
    }

    #[must_use]
    pub const fn run_id(&self) -> i64 {
        self.run_id
    }

    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

/// A run whose worktree Praetor believes can be released.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReclaimRequest {
    run_id: i64,
}

impl ReclaimRequest {
    pub fn new(run_id: i64) -> Result<Self, CoordinationRequestError> {
        check_required_identifier(run_id, "run_id")?;

        Ok(Self { run_id })
    }

    #[must_use]
    pub const fn run_id(&self) -> i64 {
        self.run_id
    }
}

/// What one run in full is asked for by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReportRequest {
    run_id: i64,
}

impl ReportRequest {
    pub fn new(run_id: i64) -> Result<Self, CoordinationRequestError> {
        check_required_identifier(run_id, "run_id")?;

        Ok(Self { run_id })
    }

    #[must_use]
    pub const fn run_id(&self) -> i64 {
        self.run_id
    }
}

/// A run proposed, and the state it landed in.
///
/// The state is carried rather than assumed because it is the whole point of
/// the operation's shape: a proposed run lands in `draft`, and only the user's
/// approval queues it. Praetor reads back what it actually got.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpawnReceipt {
    pub run_id: i64,
    pub state: String,
}

/// A question answered, and whether it unblocked its run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnswerReceipt {
    pub question_id: i64,
    pub run_id: i64,
    pub run_resumed: bool,
}

/// A decision now waiting on the person.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EscalateReceipt {
    pub question_id: i64,
    pub run_id: i64,
}

/// Guidance queued for a run's next safe point.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectReceipt {
    pub run_id: i64,
}

/// A run that moved, and where it went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunStateReceipt {
    pub run_id: i64,
    pub state: String,
    /// Whether this request is what moved it. A run already cancelled reports
    /// its state without having been moved twice.
    pub moved: bool,
}

/// A merge authorization opened for the user, and the bytes it was frozen over.
///
/// The digests are carried because they are what the user is authorizing and
/// what the pre-merge gate re-derives against. They name no path and open
/// nothing: a digest is evidence, not a handle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeRequestReceipt {
    pub question_id: i64,
    pub run_id: i64,
    pub tree_hash: String,
    pub paths_digest: String,
}

/// A worktree released, and the status its row ended in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReclaimReceipt {
    pub run_id: i64,
    pub worktree_status: String,
    pub moved: bool,
}

/// The control plane, as the manager of one team reaches it.
///
/// Every method is either a read or an instruction the coordinator carries
/// out. None of them executes anything itself, and the trait's own vocabulary
/// is what holds that: there is no path, no revision and no command in any
/// signature, so an implementation has nothing to run and a caller has nothing
/// to ask it to run.
pub trait CoordinationPort: Send {
    /// Every run of the managed team, and everything open on it.
    fn status(&mut self) -> Result<TeamStatus, CoordinationError>;

    /// One run and everything recorded about it.
    fn report(&mut self, request: &ReportRequest) -> Result<RunReport, CoordinationError>;

    /// Answers a question a run is blocked on. Which questions are answerable
    /// this way is the control plane's decision, not this trait's: an
    /// authorization never is.
    fn answer(&mut self, request: &AnswerRequest) -> Result<AnswerReceipt, CoordinationError>;

    /// Hands a decision to the person, on a run that is waiting for one.
    fn escalate(&mut self, request: &EscalateRequest)
    -> Result<EscalateReceipt, CoordinationError>;

    /// Queues guidance for a run's next safe point. It does not wait for the
    /// run to read it.
    fn direct(&mut self, request: &DirectRequest) -> Result<DirectReceipt, CoordinationError>;

    /// Stops a run.
    fn cancel(&mut self, request: &CancelRequest) -> Result<RunStateReceipt, CoordinationError>;

    /// Proposes a run. It lands in `draft`; the user's approval is what queues
    /// it.
    fn spawn(&mut self, request: &SpawnRequest) -> Result<SpawnReceipt, CoordinationError>;

    /// Queues another attempt at an already approved scope.
    fn retry(&mut self, request: &RetryRequest) -> Result<RunStateReceipt, CoordinationError>;

    /// Opens the merge authorization the user answers. It grants nothing.
    fn request_merge(
        &mut self,
        request: &MergeRequest,
    ) -> Result<MergeRequestReceipt, CoordinationError>;

    /// Asks the coordinator to release a run's worktree. The coordinator
    /// re-derives from git whether it may be released, and refuses when git
    /// does not agree.
    fn request_reclaim(
        &mut self,
        request: &ReclaimRequest,
    ) -> Result<ReclaimReceipt, CoordinationError>;
}

/// The port a session that manages no team gets.
///
/// Every call answers [`CoordinationError::NoTeam`], which is what the tools
/// report. It exists so the surface can be built and exercised without a
/// coordinator behind it.
pub struct UnavailableCoordinationPort;

impl CoordinationPort for UnavailableCoordinationPort {
    fn status(&mut self) -> Result<TeamStatus, CoordinationError> {
        Err(CoordinationError::NoTeam)
    }

    fn report(&mut self, _: &ReportRequest) -> Result<RunReport, CoordinationError> {
        Err(CoordinationError::NoTeam)
    }

    fn answer(&mut self, _: &AnswerRequest) -> Result<AnswerReceipt, CoordinationError> {
        Err(CoordinationError::NoTeam)
    }

    fn escalate(&mut self, _: &EscalateRequest) -> Result<EscalateReceipt, CoordinationError> {
        Err(CoordinationError::NoTeam)
    }

    fn direct(&mut self, _: &DirectRequest) -> Result<DirectReceipt, CoordinationError> {
        Err(CoordinationError::NoTeam)
    }

    fn cancel(&mut self, _: &CancelRequest) -> Result<RunStateReceipt, CoordinationError> {
        Err(CoordinationError::NoTeam)
    }

    fn spawn(&mut self, _: &SpawnRequest) -> Result<SpawnReceipt, CoordinationError> {
        Err(CoordinationError::NoTeam)
    }

    fn retry(&mut self, _: &RetryRequest) -> Result<RunStateReceipt, CoordinationError> {
        Err(CoordinationError::NoTeam)
    }

    fn request_merge(
        &mut self,
        _: &MergeRequest,
    ) -> Result<MergeRequestReceipt, CoordinationError> {
        Err(CoordinationError::NoTeam)
    }

    fn request_reclaim(&mut self, _: &ReclaimRequest) -> Result<ReclaimReceipt, CoordinationError> {
        Err(CoordinationError::NoTeam)
    }
}

fn check_required(
    value: &str,
    field: &'static str,
    max_chars: usize,
) -> Result<(), CoordinationRequestError> {
    if value.is_empty() {
        return Err(CoordinationRequestError::EmptyField(field));
    }

    if value.chars().count() > max_chars {
        return Err(CoordinationRequestError::FieldTooLong(field));
    }

    if value
        .chars()
        .any(|character| character.is_control() && character != '\n')
    {
        return Err(CoordinationRequestError::ControlCharacter(field));
    }

    Ok(())
}

fn check_required_identifier(
    value: i64,
    field: &'static str,
) -> Result<(), CoordinationRequestError> {
    if value <= 0 {
        return Err(CoordinationRequestError::NotAnIdentifier(field));
    }

    Ok(())
}

fn check_identifier(
    value: Option<i64>,
    field: &'static str,
) -> Result<(), CoordinationRequestError> {
    match value {
        None => Ok(()),
        Some(value) => check_required_identifier(value, field),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spawn(task: &str, scope: &str, dod: &str) -> Result<SpawnRequest, CoordinationRequestError> {
        SpawnRequest::new(
            task.to_owned(),
            scope.to_owned(),
            dod.to_owned(),
            0,
            None,
            None,
        )
    }

    #[test]
    fn a_spawn_needs_a_task_a_scope_and_a_definition_of_done() {
        assert_eq!(
            spawn("", "scope", "dod"),
            Err(CoordinationRequestError::EmptyField("task"))
        );
        assert_eq!(
            spawn("task", "", "dod"),
            Err(CoordinationRequestError::EmptyField("scope"))
        );
        assert_eq!(
            spawn("task", "scope", ""),
            Err(CoordinationRequestError::EmptyField("dod"))
        );

        let request = spawn("task", "scope", "dod").expect("a complete spawn is accepted");

        assert_eq!(request.task(), "task");
        assert_eq!(request.priority(), 0);
        assert_eq!(request.parent_run_id(), None);
    }

    #[test]
    fn a_spawn_refuses_an_identifier_that_names_no_row() {
        assert_eq!(
            SpawnRequest::new(
                "task".to_owned(),
                "scope".to_owned(),
                "dod".to_owned(),
                0,
                Some(0),
                None,
            ),
            Err(CoordinationRequestError::NotAnIdentifier("parent_run_id"))
        );
        assert_eq!(
            SpawnRequest::new(
                "task".to_owned(),
                "scope".to_owned(),
                "dod".to_owned(),
                0,
                None,
                Some(-1),
            ),
            Err(CoordinationRequestError::NotAnIdentifier("dep_run_id"))
        );
    }

    #[test]
    fn every_bounded_field_refuses_a_control_character() {
        assert_eq!(
            spawn("ta\u{7}sk", "scope", "dod"),
            Err(CoordinationRequestError::ControlCharacter("task"))
        );
        assert_eq!(
            DirectRequest::new(1, "keep \u{1b}[31mgoing".to_owned()),
            Err(CoordinationRequestError::ControlCharacter("directive"))
        );
        assert_eq!(
            AnswerRequest::new(1, "ye\u{0}s".to_owned()),
            Err(CoordinationRequestError::ControlCharacter("answer"))
        );
    }

    #[test]
    fn a_newline_is_not_a_control_character_a_request_refuses() {
        let request = DirectRequest::new(1, "first\nsecond".to_owned())
            .expect("guidance is written in more than one line");

        assert_eq!(request.directive(), "first\nsecond");
    }

    #[test]
    fn every_request_that_names_a_run_refuses_one_that_is_not_an_identifier() {
        assert_eq!(
            DirectRequest::new(0, "go".to_owned()),
            Err(CoordinationRequestError::NotAnIdentifier("run_id"))
        );
        assert_eq!(
            RetryRequest::new(0, "try again".to_owned()),
            Err(CoordinationRequestError::NotAnIdentifier("run_id"))
        );
        assert_eq!(
            CancelRequest::new(0, "no longer needed".to_owned()),
            Err(CoordinationRequestError::NotAnIdentifier("run_id"))
        );
        assert_eq!(
            MergeRequest::new(0, "the work is done".to_owned()),
            Err(CoordinationRequestError::NotAnIdentifier("run_id"))
        );
        assert_eq!(
            ReclaimRequest::new(0),
            Err(CoordinationRequestError::NotAnIdentifier("run_id"))
        );
        assert_eq!(
            ReportRequest::new(0),
            Err(CoordinationRequestError::NotAnIdentifier("run_id"))
        );
        assert_eq!(
            AnswerRequest::new(0, "yes".to_owned()),
            Err(CoordinationRequestError::NotAnIdentifier("question_id"))
        );
    }

    #[test]
    fn an_oversized_field_is_refused_by_name() {
        let long = "a".repeat(MAX_TASK_CHARS + 1);

        assert_eq!(
            spawn(&long, "scope", "dod"),
            Err(CoordinationRequestError::FieldTooLong("task"))
        );
    }

    #[test]
    fn a_spawn_whose_fields_are_each_bounded_can_still_be_too_large() {
        let scope = "a".repeat(MAX_SCOPE_CHARS);
        let dod = "b".repeat(MAX_DOD_CHARS);
        let task = "c".repeat(MAX_TASK_CHARS);

        assert_eq!(
            spawn(&task, &scope, &dod),
            Err(CoordinationRequestError::RequestTooLarge)
        );
    }

    #[test]
    fn an_escalation_carries_the_question_it_is_raising() {
        let question = Ask::new(
            "which database the importer writes to".to_owned(),
            vec![AskOption::new(
                "postgres".to_owned(),
                "write to postgres".to_owned(),
                None,
            )],
            None,
        )
        .expect("the question holds together");

        let request = EscalateRequest::new(4, question).expect("an escalation names its run");

        assert_eq!(request.run_id(), 4);
        assert_eq!(request.question().options().len(), 1);
    }

    #[test]
    fn a_session_managing_no_team_says_so_rather_than_failing() {
        let mut port = UnavailableCoordinationPort;

        assert_eq!(port.status().unwrap_err(), CoordinationError::NoTeam);
        assert_eq!(
            port.cancel(&CancelRequest::new(1, "stop".to_owned()).unwrap())
                .unwrap_err(),
            CoordinationError::NoTeam
        );
    }
}
