use crate::HeadlessTurnCancellation;

pub const MAX_ASK_USER_QUESTIONS: usize = 8;
pub const MAX_ASK_USER_OPTIONS: usize = 12;
pub const MAX_ASK_USER_ID_CHARS: usize = 64;
pub const MAX_ASK_USER_LABEL_CHARS: usize = 120;
pub const MAX_ASK_USER_PROMPT_CHARS: usize = 512;
pub const MAX_ASK_USER_EXPLANATION_CHARS: usize = 512;
pub const MAX_ASK_USER_CONTEXT_CHARS: usize = 2_048;
pub const MAX_ASK_USER_FREE_TEXT_CHARS: usize = 1_024;
pub const MAX_ASK_USER_NOTE_CHARS: usize = 512;
pub const MAX_ASK_USER_TITLE_CHARS: usize = 64;
pub const MAX_ASK_USER_REQUEST_CHARS: usize = 64 * 1_024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AskUserMode {
    Single,
    Multiple,
}

/// A single choice within an [`AskUserQuestion`].
///
/// `AskUserOption::new` performs no validation: an option is meaningless in
/// isolation (its bounds and identity uniqueness are checked against its
/// sibling options and the aggregate request size), so the only place this
/// crate treats a value as trustworthy is after it survives
/// [`AskUserRequest::new`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AskUserOption {
    id: String,
    label: String,
    explanation: Option<String>,
    context: Option<String>,
}

impl AskUserOption {
    pub fn new(
        id: impl Into<String>,
        label: impl Into<String>,
        explanation: Option<String>,
        context: Option<String>,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            explanation,
            context,
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn explanation(&self) -> Option<&str> {
        self.explanation.as_deref()
    }

    pub fn context(&self) -> Option<&str> {
        self.context.as_deref()
    }
}

/// One question in an [`AskUserRequest`].
///
/// Like [`AskUserOption`], `AskUserQuestion::new` builds an unchecked value:
/// its own bounds and its options' identity uniqueness can only be judged in
/// the context of the whole request, so it carries no independent guarantee
/// until it has passed through [`AskUserRequest::new`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AskUserQuestion {
    id: String,
    prompt: String,
    explanation: Option<String>,
    mode: AskUserMode,
    options: Vec<AskUserOption>,
    allow_other: bool,
    allow_note: bool,
    allow_discuss: bool,
}

impl AskUserQuestion {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: impl Into<String>,
        prompt: impl Into<String>,
        explanation: Option<String>,
        mode: AskUserMode,
        options: Vec<AskUserOption>,
        allow_other: bool,
        allow_note: bool,
        allow_discuss: bool,
    ) -> Self {
        Self {
            id: id.into(),
            prompt: prompt.into(),
            explanation,
            mode,
            options,
            allow_other,
            allow_note,
            allow_discuss,
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    pub fn explanation(&self) -> Option<&str> {
        self.explanation.as_deref()
    }

    pub const fn mode(&self) -> AskUserMode {
        self.mode
    }

    pub fn options(&self) -> &[AskUserOption] {
        &self.options
    }

    pub const fn allow_other(&self) -> bool {
        self.allow_other
    }

    pub const fn allow_note(&self) -> bool {
        self.allow_note
    }

    pub const fn allow_discuss(&self) -> bool {
        self.allow_discuss
    }
}

/// A bounded, structured question set a caller can hand to an
/// [`AskUserPort`].
///
/// This is the crate's single validated value in the request tree: every
/// field on `AskUserRequest`, `AskUserQuestion`, and `AskUserOption` is
/// private, and [`AskUserRequest::new`] is the only constructor that returns
/// a value with those private fields already populated. `AskUserQuestion`
/// and `AskUserOption` still expose their own unchecked `new` so a caller
/// (typically a tool layer parsing untrusted JSON) can assemble the tree
/// before it is validated, but that intermediate tree has no way to reach a
/// port: only `AskUserRequest::new` performs bound, uniqueness, and
/// control-character checks and only its `Ok` value is an `AskUserRequest`.
/// There is no other path that produces one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AskUserRequest {
    title: Option<String>,
    questions: Vec<AskUserQuestion>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AskUserRequestError {
    NoQuestions,
    TooManyQuestions,
    DuplicateQuestionId,
    NoOptions,
    TooManyOptions,
    DuplicateOptionId,
    EmptyField(&'static str),
    FieldTooLong(&'static str),
    ControlCharacter(&'static str),
    RequestTooLarge,
}

/// One question's answer inside an [`AskUserReply::Answered`].
///
/// Its fields are public rather than validated at construction, because an
/// `AskUserAnswer` has no invariant it can check on its own: whether an
/// unknown option, a disallowed note, or an over-long free-text value is
/// legal depends entirely on the [`AskUserQuestion`] it answers, and that
/// question is only known to the caller holding the matching
/// [`AskUserRequest`]. It is built freely by whichever surface collects the
/// answer (a TUI, a scripted test double) and is only judged for validity
/// by [`AskUserRequest::validate_reply`], after the fact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AskUserAnswer {
    pub question_id: String,
    pub selected: Vec<String>,
    pub other: Option<String>,
    pub note: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AskUserUnavailable {
    NoInteractiveSurface,
    SurfaceClosed,
}

/// Every way an ask-user interaction can end.
///
/// There is deliberately no elapsed-time outcome. A question put to a person
/// blocks until that person answers it, walks away from it, or loses the
/// surface it was drawn on; a deadline that resolved it on their behalf would
/// be inventing an answer nobody gave. Cancellation still ends it, because
/// that is the user acting, not a clock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AskUserReply {
    Answered(Vec<AskUserAnswer>),
    Discuss {
        question_id: String,
        note: Option<String>,
    },
    Cancelled,
    Unavailable(AskUserUnavailable),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AskUserReplyError {
    QuestionCountMismatch,
    UnknownQuestion,
    QuestionOutOfOrder,
    UnknownOption,
    DuplicateOption,
    MultipleSelectionsInSingleMode,
    BlankOther,
    OtherTooLong,
    OtherControlCharacter,
    NoteNotAllowed,
    BlankNote,
    NoteTooLong,
    NoteControlCharacter,
    DiscussNotAllowed,
}

/// Rejects a required non-blank field: empty, over its character bound, or
/// carrying a control character other than newline.
fn check_required_field(
    value: &str,
    field: &'static str,
    max_chars: usize,
    allow_newline: bool,
) -> Result<(), AskUserRequestError> {
    if value.is_empty() {
        return Err(AskUserRequestError::EmptyField(field));
    }

    check_field_bounds(value, field, max_chars, allow_newline)
}

fn check_field_bounds(
    value: &str,
    field: &'static str,
    max_chars: usize,
    allow_newline: bool,
) -> Result<(), AskUserRequestError> {
    if value.chars().count() > max_chars {
        return Err(AskUserRequestError::FieldTooLong(field));
    }

    let has_disallowed_control = value
        .chars()
        .any(|character| character.is_control() && !(allow_newline && character == '\n'));

    if has_disallowed_control {
        return Err(AskUserRequestError::ControlCharacter(field));
    }

    Ok(())
}

impl AskUserRequest {
    pub fn new(
        title: Option<String>,
        questions: Vec<AskUserQuestion>,
    ) -> Result<Self, AskUserRequestError> {
        if let Some(title) = title.as_deref() {
            check_required_field(title, "title", MAX_ASK_USER_TITLE_CHARS, false)?;
        }

        if questions.is_empty() {
            return Err(AskUserRequestError::NoQuestions);
        }

        if questions.len() > MAX_ASK_USER_QUESTIONS {
            return Err(AskUserRequestError::TooManyQuestions);
        }

        for question in &questions {
            validate_question(question)?;
        }

        let mut seen_question_ids = std::collections::BTreeSet::new();
        for question in &questions {
            if !seen_question_ids.insert(question.id.as_str()) {
                return Err(AskUserRequestError::DuplicateQuestionId);
            }
        }

        let aggregate_chars = title.as_deref().map_or(0, str::chars_count)
            + questions.iter().map(question_char_count).sum::<usize>();

        if aggregate_chars > MAX_ASK_USER_REQUEST_CHARS {
            return Err(AskUserRequestError::RequestTooLarge);
        }

        Ok(Self { title, questions })
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub fn questions(&self) -> &[AskUserQuestion] {
        &self.questions
    }

    pub fn validate_reply(&self, reply: &AskUserReply) -> Result<(), AskUserReplyError> {
        match reply {
            AskUserReply::Answered(answers) => self.validate_answered_reply(answers),
            AskUserReply::Discuss { question_id, note } => {
                self.validate_discuss_reply(question_id, note.as_deref())
            }
            AskUserReply::Cancelled | AskUserReply::Unavailable(_) => Ok(()),
        }
    }

    fn validate_answered_reply(&self, answers: &[AskUserAnswer]) -> Result<(), AskUserReplyError> {
        if answers.len() != self.questions.len() {
            return Err(AskUserReplyError::QuestionCountMismatch);
        }

        for (question, answer) in self.questions.iter().zip(answers.iter()) {
            if answer.question_id != question.id {
                let question_exists_elsewhere = self
                    .questions
                    .iter()
                    .any(|other| other.id == answer.question_id);

                return Err(if question_exists_elsewhere {
                    AskUserReplyError::QuestionOutOfOrder
                } else {
                    AskUserReplyError::UnknownQuestion
                });
            }

            validate_answer(question, answer)?;
        }

        Ok(())
    }

    fn validate_discuss_reply(
        &self,
        question_id: &str,
        note: Option<&str>,
    ) -> Result<(), AskUserReplyError> {
        let question = self
            .questions
            .iter()
            .find(|question| question.id == question_id)
            .ok_or(AskUserReplyError::UnknownQuestion)?;

        if !question.allow_discuss {
            return Err(AskUserReplyError::DiscussNotAllowed);
        }

        if let Some(note) = note {
            validate_note(question, note)?;
        }

        Ok(())
    }
}

fn validate_question(question: &AskUserQuestion) -> Result<(), AskUserRequestError> {
    check_required_field(&question.id, "question.id", MAX_ASK_USER_ID_CHARS, false)?;
    check_required_field(
        &question.prompt,
        "question.prompt",
        MAX_ASK_USER_PROMPT_CHARS,
        false,
    )?;

    if let Some(explanation) = question.explanation.as_deref() {
        check_field_bounds(
            explanation,
            "question.explanation",
            MAX_ASK_USER_EXPLANATION_CHARS,
            true,
        )?;
    }

    if question.options.is_empty() {
        return Err(AskUserRequestError::NoOptions);
    }

    if question.options.len() > MAX_ASK_USER_OPTIONS {
        return Err(AskUserRequestError::TooManyOptions);
    }

    for option in &question.options {
        validate_option(option)?;
    }

    let mut seen_option_ids = std::collections::BTreeSet::new();
    for option in &question.options {
        if !seen_option_ids.insert(option.id.as_str()) {
            return Err(AskUserRequestError::DuplicateOptionId);
        }
    }

    Ok(())
}

fn validate_option(option: &AskUserOption) -> Result<(), AskUserRequestError> {
    check_required_field(&option.id, "option.id", MAX_ASK_USER_ID_CHARS, false)?;
    check_required_field(
        &option.label,
        "option.label",
        MAX_ASK_USER_LABEL_CHARS,
        false,
    )?;

    if let Some(explanation) = option.explanation.as_deref() {
        check_field_bounds(
            explanation,
            "option.explanation",
            MAX_ASK_USER_EXPLANATION_CHARS,
            true,
        )?;
    }

    if let Some(context) = option.context.as_deref() {
        check_field_bounds(context, "option.context", MAX_ASK_USER_CONTEXT_CHARS, true)?;
    }

    Ok(())
}

fn validate_answer(
    question: &AskUserQuestion,
    answer: &AskUserAnswer,
) -> Result<(), AskUserReplyError> {
    let mut seen_option_ids = std::collections::BTreeSet::new();
    for selected_id in &answer.selected {
        if !question
            .options
            .iter()
            .any(|option| option.id == *selected_id)
        {
            return Err(AskUserReplyError::UnknownOption);
        }

        if !seen_option_ids.insert(selected_id.as_str()) {
            return Err(AskUserReplyError::DuplicateOption);
        }
    }

    if matches!(question.mode, AskUserMode::Single) && answer.selected.len() > 1 {
        return Err(AskUserReplyError::MultipleSelectionsInSingleMode);
    }

    // Free-text "other" is always accepted when present: the interactive surface
    // always offers it, independent of the agent's `allow_other` flag. A blank
    // `Some` is still rejected so callers cannot smuggle empty strings as
    // answers; an omitted `other` with no selection is a skipped question.
    if let Some(other) = answer.other.as_deref() {
        validate_free_text(
            other,
            MAX_ASK_USER_FREE_TEXT_CHARS,
            AskUserReplyError::BlankOther,
            AskUserReplyError::OtherTooLong,
            AskUserReplyError::OtherControlCharacter,
        )?;
    }

    if let Some(note) = answer.note.as_deref() {
        validate_note(question, note)?;
    }

    Ok(())
}

/// A note is only meaningful next to an allowed answer, so its `allow_note`
/// check is repeated at both call sites (an answered reply and a discuss
/// reply) rather than hoisted, since a discuss reply has no `AskUserAnswer`
/// to hang the check off.
fn validate_note(question: &AskUserQuestion, note: &str) -> Result<(), AskUserReplyError> {
    if !question.allow_note {
        return Err(AskUserReplyError::NoteNotAllowed);
    }

    validate_free_text(
        note,
        MAX_ASK_USER_NOTE_CHARS,
        AskUserReplyError::BlankNote,
        AskUserReplyError::NoteTooLong,
        AskUserReplyError::NoteControlCharacter,
    )
}

/// Whitespace-only text carries no answer even though it is not the empty
/// string; blank is judged by trimmed content, not by length, so intentional
/// non-blank values such as `"0"` or `"false"` remain valid.
fn is_blank(value: &str) -> bool {
    value.trim().is_empty()
}

fn validate_free_text(
    value: &str,
    max_chars: usize,
    blank_error: AskUserReplyError,
    too_long_error: AskUserReplyError,
    control_character_error: AskUserReplyError,
) -> Result<(), AskUserReplyError> {
    if is_blank(value) {
        return Err(blank_error);
    }

    if value.chars().count() > max_chars {
        return Err(too_long_error);
    }

    if value.chars().any(char::is_control) {
        return Err(control_character_error);
    }

    Ok(())
}

trait CharsCount {
    fn chars_count(&self) -> usize;
}

impl CharsCount for str {
    fn chars_count(&self) -> usize {
        self.chars().count()
    }
}

fn question_char_count(question: &AskUserQuestion) -> usize {
    question.id.chars().count()
        + question.prompt.chars().count()
        + question.explanation.as_deref().map_or(0, str::chars_count)
        + question
            .options
            .iter()
            .map(option_char_count)
            .sum::<usize>()
}

fn option_char_count(option: &AskUserOption) -> usize {
    option.id.chars().count()
        + option.label.chars().count()
        + option.explanation.as_deref().map_or(0, str::chars_count)
        + option.context.as_deref().map_or(0, str::chars_count)
}

pub trait AskUserPort: Send {
    fn ask(
        &self,
        request: &AskUserRequest,
        cancellation: &HeadlessTurnCancellation,
    ) -> AskUserReply;
}

impl AskUserPort for Box<dyn AskUserPort> {
    fn ask(
        &self,
        request: &AskUserRequest,
        cancellation: &HeadlessTurnCancellation,
    ) -> AskUserReply {
        (**self).ask(request, cancellation)
    }
}

pub struct UnavailableAskUserPort;

impl AskUserPort for UnavailableAskUserPort {
    fn ask(
        &self,
        _request: &AskUserRequest,
        _cancellation: &HeadlessTurnCancellation,
    ) -> AskUserReply {
        AskUserReply::Unavailable(AskUserUnavailable::NoInteractiveSurface)
    }
}
