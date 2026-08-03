use agens_core::HeadlessTurnCancellation;
use agens_core::ask_user::{
    AskUserAnswer, AskUserMode, AskUserOption, AskUserPort, AskUserQuestion, AskUserReply,
    AskUserReplyError, AskUserRequest, AskUserRequestError, AskUserUnavailable,
    MAX_ASK_USER_CONTEXT_CHARS, MAX_ASK_USER_EXPLANATION_CHARS, MAX_ASK_USER_FREE_TEXT_CHARS,
    MAX_ASK_USER_ID_CHARS, MAX_ASK_USER_LABEL_CHARS, MAX_ASK_USER_NOTE_CHARS, MAX_ASK_USER_OPTIONS,
    MAX_ASK_USER_PROMPT_CHARS, MAX_ASK_USER_QUESTIONS, MAX_ASK_USER_TITLE_CHARS,
    UnavailableAskUserPort,
};

fn option(id: &str, label: &str) -> AskUserOption {
    AskUserOption::new(id, label, None, None)
}

fn single_question(id: &str, options: Vec<AskUserOption>) -> AskUserQuestion {
    AskUserQuestion::new(
        id,
        "Which do you prefer?",
        None,
        AskUserMode::Single,
        options,
        false,
        false,
        false,
    )
}

fn valid_request() -> AskUserRequest {
    AskUserRequest::new(
        Some("Plan review".to_owned()),
        vec![single_question(
            "plan",
            vec![option("a", "Option A"), option("b", "Option B")],
        )],
    )
    .expect("baseline request should be valid")
}

#[test]
fn accepts_a_minimal_valid_request() {
    let request = valid_request();

    assert_eq!(request.questions().len(), 1);
}

#[test]
fn rejects_zero_questions() {
    let result = AskUserRequest::new(None, Vec::new());

    assert_eq!(result.unwrap_err(), AskUserRequestError::NoQuestions);
}

#[test]
fn rejects_more_than_the_maximum_questions() {
    let questions: Vec<AskUserQuestion> = (0..=MAX_ASK_USER_QUESTIONS)
        .map(|index| {
            single_question(
                &format!("q{index}"),
                vec![option("a", "Option A"), option("b", "Option B")],
            )
        })
        .collect();

    let result = AskUserRequest::new(None, questions);

    assert_eq!(result.unwrap_err(), AskUserRequestError::TooManyQuestions);
}

#[test]
fn accepts_exactly_the_maximum_questions() {
    let questions: Vec<AskUserQuestion> = (0..MAX_ASK_USER_QUESTIONS)
        .map(|index| {
            single_question(
                &format!("q{index}"),
                vec![option("a", "Option A"), option("b", "Option B")],
            )
        })
        .collect();

    let result = AskUserRequest::new(None, questions);

    assert!(result.is_ok());
}

#[test]
fn rejects_duplicate_question_ids() {
    let questions = vec![
        single_question("plan", vec![option("a", "A"), option("b", "B")]),
        single_question("plan", vec![option("a", "A"), option("b", "B")]),
    ];

    let result = AskUserRequest::new(None, questions);

    assert_eq!(
        result.unwrap_err(),
        AskUserRequestError::DuplicateQuestionId
    );
}

#[test]
fn rejects_empty_options() {
    let result = AskUserRequest::new(None, vec![single_question("plan", Vec::new())]);

    assert_eq!(result.unwrap_err(), AskUserRequestError::NoOptions);
}

#[test]
fn rejects_more_than_the_maximum_options() {
    let options: Vec<AskUserOption> = (0..=MAX_ASK_USER_OPTIONS)
        .map(|index| option(&format!("o{index}"), "Option"))
        .collect();

    let result = AskUserRequest::new(None, vec![single_question("plan", options)]);

    assert_eq!(result.unwrap_err(), AskUserRequestError::TooManyOptions);
}

#[test]
fn accepts_exactly_the_maximum_options() {
    let options: Vec<AskUserOption> = (0..MAX_ASK_USER_OPTIONS)
        .map(|index| option(&format!("o{index}"), "Option"))
        .collect();

    let result = AskUserRequest::new(None, vec![single_question("plan", options)]);

    assert!(result.is_ok());
}

#[test]
fn rejects_duplicate_option_ids_within_one_question() {
    let result = AskUserRequest::new(
        None,
        vec![single_question(
            "plan",
            vec![option("a", "A"), option("a", "A again")],
        )],
    );

    assert_eq!(result.unwrap_err(), AskUserRequestError::DuplicateOptionId);
}

#[test]
fn allows_the_same_option_id_reused_across_different_questions() {
    let result = AskUserRequest::new(
        None,
        vec![
            single_question("plan", vec![option("a", "A"), option("b", "B")]),
            single_question("style", vec![option("a", "A"), option("b", "B")]),
        ],
    );

    assert!(result.is_ok());
}

#[test]
fn rejects_a_title_over_the_bound() {
    let title = "t".repeat(MAX_ASK_USER_TITLE_CHARS + 1);
    let result = AskUserRequest::new(
        Some(title),
        vec![single_question(
            "plan",
            vec![option("a", "A"), option("b", "B")],
        )],
    );

    assert_eq!(
        result.unwrap_err(),
        AskUserRequestError::FieldTooLong("title")
    );
}

#[test]
fn rejects_a_blank_title_when_present() {
    let result = AskUserRequest::new(
        Some(String::new()),
        vec![single_question(
            "plan",
            vec![option("a", "A"), option("b", "B")],
        )],
    );

    assert_eq!(
        result.unwrap_err(),
        AskUserRequestError::EmptyField("title")
    );
}

#[test]
fn rejects_a_question_id_over_the_bound() {
    let id = "q".repeat(MAX_ASK_USER_ID_CHARS + 1);
    let result = AskUserRequest::new(
        None,
        vec![single_question(
            &id,
            vec![option("a", "A"), option("b", "B")],
        )],
    );

    assert_eq!(
        result.unwrap_err(),
        AskUserRequestError::FieldTooLong("question.id")
    );
}

#[test]
fn rejects_a_question_prompt_over_the_bound() {
    let prompt = "p".repeat(MAX_ASK_USER_PROMPT_CHARS + 1);
    let question = AskUserQuestion::new(
        "plan",
        prompt,
        None,
        AskUserMode::Single,
        vec![option("a", "A"), option("b", "B")],
        false,
        false,
        false,
    );

    let result = AskUserRequest::new(None, vec![question]);

    assert_eq!(
        result.unwrap_err(),
        AskUserRequestError::FieldTooLong("question.prompt")
    );
}

#[test]
fn rejects_a_question_explanation_over_the_bound() {
    let explanation = "e".repeat(MAX_ASK_USER_EXPLANATION_CHARS + 1);
    let question = AskUserQuestion::new(
        "plan",
        "Which do you prefer?",
        Some(explanation),
        AskUserMode::Single,
        vec![option("a", "A"), option("b", "B")],
        false,
        false,
        false,
    );

    let result = AskUserRequest::new(None, vec![question]);

    assert_eq!(
        result.unwrap_err(),
        AskUserRequestError::FieldTooLong("question.explanation")
    );
}

#[test]
fn rejects_an_option_label_over_the_bound() {
    let label = "l".repeat(MAX_ASK_USER_LABEL_CHARS + 1);
    let result = AskUserRequest::new(
        None,
        vec![single_question(
            "plan",
            vec![option("a", &label), option("b", "B")],
        )],
    );

    assert_eq!(
        result.unwrap_err(),
        AskUserRequestError::FieldTooLong("option.label")
    );
}

#[test]
fn rejects_an_option_context_over_the_bound() {
    let context = "c".repeat(MAX_ASK_USER_CONTEXT_CHARS + 1);
    let over = AskUserOption::new("a", "A", None, Some(context));
    let result = AskUserRequest::new(
        None,
        vec![single_question("plan", vec![over, option("b", "B")])],
    );

    assert_eq!(
        result.unwrap_err(),
        AskUserRequestError::FieldTooLong("option.context")
    );
}

#[test]
fn rejects_a_request_over_the_aggregate_bound() {
    let context = "c".repeat(MAX_ASK_USER_CONTEXT_CHARS);
    let questions: Vec<AskUserQuestion> = (0..MAX_ASK_USER_QUESTIONS)
        .map(|question_index| {
            let options: Vec<AskUserOption> = (0..MAX_ASK_USER_OPTIONS)
                .map(|option_index| {
                    AskUserOption::new(
                        format!("o{option_index}"),
                        "Option",
                        None,
                        Some(context.clone()),
                    )
                })
                .collect();

            single_question(&format!("q{question_index}"), options)
        })
        .collect();

    let result = AskUserRequest::new(None, questions);

    assert_eq!(result.unwrap_err(), AskUserRequestError::RequestTooLarge);
}

#[test]
fn rejects_control_characters_in_a_question_id() {
    let result = AskUserRequest::new(
        None,
        vec![single_question(
            "plan\u{7}",
            vec![option("a", "A"), option("b", "B")],
        )],
    );

    assert_eq!(
        result.unwrap_err(),
        AskUserRequestError::ControlCharacter("question.id")
    );
}

#[test]
fn rejects_control_characters_in_an_option_label() {
    let result = AskUserRequest::new(
        None,
        vec![single_question(
            "plan",
            vec![option("a", "A\u{7}"), option("b", "B")],
        )],
    );

    assert_eq!(
        result.unwrap_err(),
        AskUserRequestError::ControlCharacter("option.label")
    );
}

#[test]
fn rejects_control_characters_in_a_prompt() {
    let question = AskUserQuestion::new(
        "plan",
        "Choose\u{7} one",
        None,
        AskUserMode::Single,
        vec![option("a", "A"), option("b", "B")],
        false,
        false,
        false,
    );

    let result = AskUserRequest::new(None, vec![question]);

    assert_eq!(
        result.unwrap_err(),
        AskUserRequestError::ControlCharacter("question.prompt")
    );
}

#[test]
fn allows_newlines_in_explanation_and_context_but_not_other_control_characters() {
    let question = AskUserQuestion::new(
        "plan",
        "Which do you prefer?",
        Some("line one\nline two".to_owned()),
        AskUserMode::Single,
        vec![
            AskUserOption::new(
                "a",
                "A",
                Some("explains\nacross lines".to_owned()),
                Some("context\nacross lines".to_owned()),
            ),
            option("b", "B"),
        ],
        false,
        false,
        false,
    );

    let allowed = AskUserRequest::new(None, vec![question]);
    assert!(allowed.is_ok());

    let disallowed_question = AskUserQuestion::new(
        "plan",
        "Which do you prefer?",
        Some("bell\u{7}here".to_owned()),
        AskUserMode::Single,
        vec![option("a", "A"), option("b", "B")],
        false,
        false,
        false,
    );

    let disallowed = AskUserRequest::new(None, vec![disallowed_question]);
    assert_eq!(
        disallowed.unwrap_err(),
        AskUserRequestError::ControlCharacter("question.explanation")
    );
}

fn answered(question_id: &str, selected: &[&str]) -> AskUserAnswer {
    AskUserAnswer {
        question_id: question_id.to_owned(),
        selected: selected.iter().map(|value| (*value).to_owned()).collect(),
        other: None,
        note: None,
    }
}

#[test]
fn validates_a_correct_answered_reply() {
    let request = valid_request();
    let reply = AskUserReply::Answered(vec![answered("plan", &["b"])]);

    assert!(request.validate_reply(&reply).is_ok());
}

#[test]
fn rejects_a_reply_with_an_unknown_question_id() {
    let request = valid_request();
    let reply = AskUserReply::Answered(vec![answered("unknown", &["b"])]);

    assert_eq!(
        request.validate_reply(&reply).unwrap_err(),
        AskUserReplyError::UnknownQuestion
    );
}

#[test]
fn rejects_a_reply_with_a_question_count_mismatch() {
    let request = AskUserRequest::new(
        None,
        vec![
            single_question("plan", vec![option("a", "A"), option("b", "B")]),
            single_question("style", vec![option("a", "A"), option("b", "B")]),
        ],
    )
    .expect("two-question request should be valid");

    let reply = AskUserReply::Answered(vec![answered("plan", &["a"])]);

    assert_eq!(
        request.validate_reply(&reply).unwrap_err(),
        AskUserReplyError::QuestionCountMismatch
    );
}

#[test]
fn rejects_a_reply_with_out_of_order_questions() {
    let request = AskUserRequest::new(
        None,
        vec![
            single_question("plan", vec![option("a", "A"), option("b", "B")]),
            single_question("style", vec![option("a", "A"), option("b", "B")]),
        ],
    )
    .expect("two-question request should be valid");

    let reply = AskUserReply::Answered(vec![answered("style", &["a"]), answered("plan", &["a"])]);

    assert_eq!(
        request.validate_reply(&reply).unwrap_err(),
        AskUserReplyError::QuestionOutOfOrder
    );
}

#[test]
fn rejects_a_reply_with_an_unknown_option_id() {
    let request = valid_request();
    let reply = AskUserReply::Answered(vec![answered("plan", &["z"])]);

    assert_eq!(
        request.validate_reply(&reply).unwrap_err(),
        AskUserReplyError::UnknownOption
    );
}

#[test]
fn rejects_a_reply_with_a_duplicate_selected_option() {
    let request = valid_request();
    let reply = AskUserReply::Answered(vec![answered("plan", &["a", "a"])]);

    assert_eq!(
        request.validate_reply(&reply).unwrap_err(),
        AskUserReplyError::DuplicateOption
    );
}

#[test]
fn rejects_multiple_selections_in_single_mode() {
    let request = valid_request();
    let reply = AskUserReply::Answered(vec![answered("plan", &["a", "b"])]);

    assert_eq!(
        request.validate_reply(&reply).unwrap_err(),
        AskUserReplyError::MultipleSelectionsInSingleMode
    );
}

#[test]
fn accepts_multiple_selections_in_multiple_mode() {
    let question = AskUserQuestion::new(
        "plan",
        "Which do you prefer?",
        None,
        AskUserMode::Multiple,
        vec![option("a", "A"), option("b", "B")],
        false,
        false,
        false,
    );
    let request = AskUserRequest::new(None, vec![question]).expect("multi-select request valid");
    let reply = AskUserReply::Answered(vec![answered("plan", &["a", "b"])]);

    assert!(request.validate_reply(&reply).is_ok());
}

#[test]
fn rejects_a_note_when_not_allowed() {
    let request = valid_request();
    let mut reply_answer = answered("plan", &["a"]);
    reply_answer.note = Some("prefer the smaller diff".to_owned());
    let reply = AskUserReply::Answered(vec![reply_answer]);

    assert_eq!(
        request.validate_reply(&reply).unwrap_err(),
        AskUserReplyError::NoteNotAllowed
    );
}

#[test]
fn accepts_a_note_when_allowed() {
    let question = AskUserQuestion::new(
        "plan",
        "Which do you prefer?",
        None,
        AskUserMode::Single,
        vec![option("a", "A"), option("b", "B")],
        false,
        true,
        false,
    );
    let request = AskUserRequest::new(None, vec![question]).expect("note-enabled request valid");
    let mut reply_answer = answered("plan", &["a"]);
    reply_answer.note = Some("prefer the smaller diff".to_owned());
    let reply = AskUserReply::Answered(vec![reply_answer]);

    assert!(request.validate_reply(&reply).is_ok());
}

#[test]
fn rejects_free_text_when_other_not_allowed() {
    let request = valid_request();
    let mut reply_answer = answered("plan", &[]);
    reply_answer.other = Some("neither, use C".to_owned());
    let reply = AskUserReply::Answered(vec![reply_answer]);

    assert_eq!(
        request.validate_reply(&reply).unwrap_err(),
        AskUserReplyError::OtherNotAllowed
    );
}

#[test]
fn rejects_a_blank_other_answer() {
    let question = AskUserQuestion::new(
        "plan",
        "Which do you prefer?",
        None,
        AskUserMode::Single,
        vec![option("a", "A"), option("b", "B")],
        true,
        false,
        false,
    );
    let request = AskUserRequest::new(None, vec![question]).expect("other-enabled request valid");
    let mut reply_answer = answered("plan", &[]);
    reply_answer.other = Some(String::new());
    let reply = AskUserReply::Answered(vec![reply_answer]);

    assert_eq!(
        request.validate_reply(&reply).unwrap_err(),
        AskUserReplyError::BlankOther
    );
}

#[test]
fn rejects_a_blank_note() {
    let question = AskUserQuestion::new(
        "plan",
        "Which do you prefer?",
        None,
        AskUserMode::Single,
        vec![option("a", "A"), option("b", "B")],
        false,
        true,
        false,
    );
    let request = AskUserRequest::new(None, vec![question]).expect("note-enabled request valid");
    let mut reply_answer = answered("plan", &["a"]);
    reply_answer.note = Some(String::new());
    let reply = AskUserReply::Answered(vec![reply_answer]);

    assert_eq!(
        request.validate_reply(&reply).unwrap_err(),
        AskUserReplyError::BlankNote
    );
}

#[test]
fn accepts_false_like_non_blank_other_text() {
    let question = AskUserQuestion::new(
        "plan",
        "Which do you prefer?",
        None,
        AskUserMode::Single,
        vec![option("a", "A"), option("b", "B")],
        true,
        false,
        false,
    );
    let request = AskUserRequest::new(None, vec![question]).expect("other-enabled request valid");

    for candidate in ["0", "false"] {
        let mut reply_answer = answered("plan", &[]);
        reply_answer.other = Some(candidate.to_owned());
        let reply = AskUserReply::Answered(vec![reply_answer]);

        assert!(
            request.validate_reply(&reply).is_ok(),
            "{candidate} should be accepted"
        );
    }
}

#[test]
fn rejects_a_missing_question_in_the_reply() {
    let request = AskUserRequest::new(
        None,
        vec![
            single_question("plan", vec![option("a", "A"), option("b", "B")]),
            single_question("style", vec![option("a", "A"), option("b", "B")]),
        ],
    )
    .expect("two-question request should be valid");

    let reply = AskUserReply::Answered(vec![answered("plan", &["a"]), answered("plan", &["b"])]);

    assert_eq!(
        request.validate_reply(&reply).unwrap_err(),
        AskUserReplyError::QuestionOutOfOrder
    );
}

#[test]
fn discuss_reply_requires_a_valid_question_that_allows_discuss() {
    let question = AskUserQuestion::new(
        "plan",
        "Which do you prefer?",
        None,
        AskUserMode::Single,
        vec![option("a", "A"), option("b", "B")],
        false,
        false,
        true,
    );
    let request = AskUserRequest::new(None, vec![question]).expect("discuss-enabled request valid");

    let allowed = AskUserReply::Discuss {
        question_id: "plan".to_owned(),
        note: None,
    };
    assert!(request.validate_reply(&allowed).is_ok());

    let disallowed = AskUserReply::Discuss {
        question_id: "unknown".to_owned(),
        note: None,
    };
    assert_eq!(
        request.validate_reply(&disallowed).unwrap_err(),
        AskUserReplyError::UnknownQuestion
    );
}

#[test]
fn discuss_reply_rejected_when_the_question_does_not_allow_it() {
    let request = valid_request();
    let reply = AskUserReply::Discuss {
        question_id: "plan".to_owned(),
        note: None,
    };

    assert_eq!(
        request.validate_reply(&reply).unwrap_err(),
        AskUserReplyError::DiscussNotAllowed
    );
}

#[test]
fn terminal_replies_other_than_answered_and_discuss_are_always_valid() {
    let request = valid_request();

    assert!(request.validate_reply(&AskUserReply::Cancelled).is_ok());
    assert!(request.validate_reply(&AskUserReply::Expired).is_ok());
    assert!(
        request
            .validate_reply(&AskUserReply::Unavailable(
                AskUserUnavailable::NoInteractiveSurface
            ))
            .is_ok()
    );
}

#[test]
fn unavailable_port_never_touches_io_and_reports_no_interactive_surface() {
    let port = UnavailableAskUserPort;
    let request = valid_request();
    let cancellation = HeadlessTurnCancellation::new();

    let reply = port.ask(&request, &cancellation);

    assert_eq!(
        reply,
        AskUserReply::Unavailable(AskUserUnavailable::NoInteractiveSurface)
    );
}

#[test]
fn boxed_ask_user_port_forwards_to_the_inner_port() {
    let boxed: Box<dyn AskUserPort> = Box::new(UnavailableAskUserPort);
    let request = valid_request();
    let cancellation = HeadlessTurnCancellation::new();

    let reply = boxed.ask(&request, &cancellation);

    assert_eq!(
        reply,
        AskUserReply::Unavailable(AskUserUnavailable::NoInteractiveSurface)
    );
}

#[test]
fn free_text_and_note_bounds_are_the_documented_values() {
    assert_eq!(MAX_ASK_USER_FREE_TEXT_CHARS, 1_024);
    assert_eq!(MAX_ASK_USER_NOTE_CHARS, 512);
}

fn other_enabled_request() -> AskUserRequest {
    let question = AskUserQuestion::new(
        "plan",
        "Which do you prefer?",
        None,
        AskUserMode::Single,
        vec![option("a", "A"), option("b", "B")],
        true,
        false,
        false,
    );

    AskUserRequest::new(None, vec![question]).expect("other-enabled request valid")
}

fn note_enabled_request() -> AskUserRequest {
    let question = AskUserQuestion::new(
        "plan",
        "Which do you prefer?",
        None,
        AskUserMode::Single,
        vec![option("a", "A"), option("b", "B")],
        false,
        true,
        false,
    );

    AskUserRequest::new(None, vec![question]).expect("note-enabled request valid")
}

fn discuss_enabled_with_note_request() -> AskUserRequest {
    let question = AskUserQuestion::new(
        "plan",
        "Which do you prefer?",
        None,
        AskUserMode::Single,
        vec![option("a", "A"), option("b", "B")],
        false,
        true,
        true,
    );

    AskUserRequest::new(None, vec![question]).expect("discuss+note-enabled request valid")
}

#[test]
fn accepts_other_text_exactly_at_the_bound() {
    let request = other_enabled_request();
    let mut reply_answer = answered("plan", &[]);
    reply_answer.other = Some("o".repeat(MAX_ASK_USER_FREE_TEXT_CHARS));
    let reply = AskUserReply::Answered(vec![reply_answer]);

    assert!(request.validate_reply(&reply).is_ok());
}

#[test]
fn rejects_other_text_over_the_bound() {
    let request = other_enabled_request();
    let mut reply_answer = answered("plan", &[]);
    reply_answer.other = Some("o".repeat(MAX_ASK_USER_FREE_TEXT_CHARS + 1));
    let reply = AskUserReply::Answered(vec![reply_answer]);

    assert_eq!(
        request.validate_reply(&reply).unwrap_err(),
        AskUserReplyError::OtherTooLong
    );
}

#[test]
fn accepts_note_text_exactly_at_the_bound() {
    let request = note_enabled_request();
    let mut reply_answer = answered("plan", &["a"]);
    reply_answer.note = Some("n".repeat(MAX_ASK_USER_NOTE_CHARS));
    let reply = AskUserReply::Answered(vec![reply_answer]);

    assert!(request.validate_reply(&reply).is_ok());
}

#[test]
fn rejects_note_text_over_the_bound() {
    let request = note_enabled_request();
    let mut reply_answer = answered("plan", &["a"]);
    reply_answer.note = Some("n".repeat(MAX_ASK_USER_NOTE_CHARS + 1));
    let reply = AskUserReply::Answered(vec![reply_answer]);

    assert_eq!(
        request.validate_reply(&reply).unwrap_err(),
        AskUserReplyError::NoteTooLong
    );
}

#[test]
fn rejects_whitespace_only_other_as_blank() {
    let request = other_enabled_request();
    let mut reply_answer = answered("plan", &[]);
    reply_answer.other = Some("   ".to_owned());
    let reply = AskUserReply::Answered(vec![reply_answer]);

    assert_eq!(
        request.validate_reply(&reply).unwrap_err(),
        AskUserReplyError::BlankOther
    );
}

#[test]
fn rejects_whitespace_only_note_as_blank() {
    let request = note_enabled_request();
    let mut reply_answer = answered("plan", &["a"]);
    reply_answer.note = Some("   ".to_owned());
    let reply = AskUserReply::Answered(vec![reply_answer]);

    assert_eq!(
        request.validate_reply(&reply).unwrap_err(),
        AskUserReplyError::BlankNote
    );
}

#[test]
fn rejects_control_characters_in_other_text() {
    let request = other_enabled_request();
    let mut reply_answer = answered("plan", &[]);
    reply_answer.other = Some("bell\u{7}here".to_owned());
    let reply = AskUserReply::Answered(vec![reply_answer]);

    assert_eq!(
        request.validate_reply(&reply).unwrap_err(),
        AskUserReplyError::OtherControlCharacter
    );
}

#[test]
fn rejects_control_characters_in_note_text() {
    let request = note_enabled_request();
    let mut reply_answer = answered("plan", &["a"]);
    reply_answer.note = Some("bell\u{7}here".to_owned());
    let reply = AskUserReply::Answered(vec![reply_answer]);

    assert_eq!(
        request.validate_reply(&reply).unwrap_err(),
        AskUserReplyError::NoteControlCharacter
    );
}

#[test]
fn discuss_reply_note_is_rejected_when_the_question_does_not_allow_notes() {
    let discuss_only_question = AskUserQuestion::new(
        "plan",
        "Which do you prefer?",
        None,
        AskUserMode::Single,
        vec![option("a", "A"), option("b", "B")],
        false,
        false,
        true,
    );
    let request =
        AskUserRequest::new(None, vec![discuss_only_question]).expect("discuss-only request valid");

    let reply = AskUserReply::Discuss {
        question_id: "plan".to_owned(),
        note: Some("no notes here".to_owned()),
    };

    assert_eq!(
        request.validate_reply(&reply).unwrap_err(),
        AskUserReplyError::NoteNotAllowed
    );
}

#[test]
fn discuss_reply_note_is_validated_against_blank_length_and_control_character_rules() {
    let request = discuss_enabled_with_note_request();

    let blank_note = AskUserReply::Discuss {
        question_id: "plan".to_owned(),
        note: Some("   ".to_owned()),
    };
    assert_eq!(
        request.validate_reply(&blank_note).unwrap_err(),
        AskUserReplyError::BlankNote
    );

    let too_long_note = AskUserReply::Discuss {
        question_id: "plan".to_owned(),
        note: Some("n".repeat(MAX_ASK_USER_NOTE_CHARS + 1)),
    };
    assert_eq!(
        request.validate_reply(&too_long_note).unwrap_err(),
        AskUserReplyError::NoteTooLong
    );

    let control_character_note = AskUserReply::Discuss {
        question_id: "plan".to_owned(),
        note: Some("bell\u{7}here".to_owned()),
    };
    assert_eq!(
        request.validate_reply(&control_character_note).unwrap_err(),
        AskUserReplyError::NoteControlCharacter
    );

    let valid_note = AskUserReply::Discuss {
        question_id: "plan".to_owned(),
        note: Some("prefer the smaller diff".to_owned()),
    };
    assert!(request.validate_reply(&valid_note).is_ok());
}
