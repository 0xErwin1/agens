use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use agens_core::{
    Error, HeadlessTurnCancellation,
    ask_user::{
        AskUserAnswer, AskUserMode, AskUserOption, AskUserPort, AskUserQuestion, AskUserReply,
        AskUserRequest, AskUserUnavailable, MAX_ASK_USER_CONTEXT_CHARS,
        MAX_ASK_USER_EXPLANATION_CHARS, MAX_ASK_USER_ID_CHARS, MAX_ASK_USER_LABEL_CHARS,
        MAX_ASK_USER_OPTIONS, MAX_ASK_USER_PROMPT_CHARS, MAX_ASK_USER_QUESTIONS,
        MAX_ASK_USER_TITLE_CHARS,
    },
};
use agens_tools::{AskUserTool, DispatchTool, ToolExecutionContext};
use serde_json::{Value, json};

struct ScriptedPort {
    calls: Arc<AtomicUsize>,
    reply: AskUserReply,
}

impl ScriptedPort {
    fn new(reply: AskUserReply) -> (Self, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Self {
                calls: Arc::clone(&calls),
                reply,
            },
            calls,
        )
    }
}

impl AskUserPort for ScriptedPort {
    fn ask(&self, _: &AskUserRequest, _: &HeadlessTurnCancellation) -> AskUserReply {
        self.calls.fetch_add(1, Ordering::AcqRel);
        self.reply.clone()
    }
}

fn two_question_request() -> AskUserRequest {
    let plan_options = vec![
        AskUserOption::new("a", "Option A", None, None),
        AskUserOption::new("b", "Option B", None, None),
    ];
    let plan_question = AskUserQuestion::new(
        "plan",
        "Which plan?",
        None,
        AskUserMode::Single,
        plan_options,
        true,
        true,
        false,
    );

    let steps_options = vec![
        AskUserOption::new("x", "Step X", None, None),
        AskUserOption::new("y", "Step Y", None, None),
    ];
    let steps_question = AskUserQuestion::new(
        "steps",
        "Which steps?",
        None,
        AskUserMode::Multiple,
        steps_options,
        false,
        false,
        true,
    );

    AskUserRequest::new(None, vec![plan_question, steps_question]).expect("valid request")
}

fn execute_with_port(
    request_value: Value,
    port: impl AskUserPort + 'static,
    context: &ToolExecutionContext,
) -> Result<agens_tools::ToolOutput, Error> {
    let mut tool = AskUserTool::new(Box::new(port));
    tool.execute(context, request_value)
}

fn ready_context() -> ToolExecutionContext {
    ToolExecutionContext::with_timeout(Duration::from_secs(30))
}

#[test]
fn schema_forbids_additional_properties_at_every_object_level() {
    let schema = AskUserTool::input_schema();

    assert_eq!(schema["additionalProperties"], json!(false));
    assert_eq!(schema["properties"]["questions"]["minItems"], json!(1));
    assert_eq!(
        schema["properties"]["questions"]["maxItems"],
        json!(MAX_ASK_USER_QUESTIONS)
    );
    assert_eq!(schema["properties"]["title"]["minLength"], json!(1));
    assert_eq!(
        schema["properties"]["title"]["maxLength"],
        json!(MAX_ASK_USER_TITLE_CHARS)
    );

    let question_schema = &schema["properties"]["questions"]["items"];
    assert_eq!(question_schema["additionalProperties"], json!(false));
    assert_eq!(
        question_schema["required"],
        json!(["id", "prompt", "mode", "options"])
    );
    assert_eq!(
        question_schema["properties"]["id"]["maxLength"],
        json!(MAX_ASK_USER_ID_CHARS)
    );
    assert_eq!(
        question_schema["properties"]["prompt"]["maxLength"],
        json!(MAX_ASK_USER_PROMPT_CHARS)
    );
    assert_eq!(
        question_schema["properties"]["mode"]["enum"],
        json!(["single", "multiple"])
    );
    assert_eq!(
        question_schema["properties"]["options"]["minItems"],
        json!(1)
    );
    assert_eq!(
        question_schema["properties"]["options"]["maxItems"],
        json!(MAX_ASK_USER_OPTIONS)
    );

    let option_schema = &question_schema["properties"]["options"]["items"];
    assert_eq!(option_schema["additionalProperties"], json!(false));
    assert_eq!(option_schema["required"], json!(["id", "label"]));
    assert_eq!(
        option_schema["properties"]["id"]["maxLength"],
        json!(MAX_ASK_USER_ID_CHARS)
    );
    assert_eq!(
        option_schema["properties"]["label"]["maxLength"],
        json!(MAX_ASK_USER_LABEL_CHARS)
    );
    assert_eq!(
        option_schema["properties"]["explanation"]["maxLength"],
        json!(MAX_ASK_USER_EXPLANATION_CHARS)
    );
    assert_eq!(
        option_schema["properties"]["context"]["maxLength"],
        json!(MAX_ASK_USER_CONTEXT_CHARS)
    );
}

#[test]
fn unknown_top_level_property_is_rejected_without_calling_the_port() {
    let (port, calls) = ScriptedPort::new(AskUserReply::Cancelled);
    let arguments = json!({
        "questions": [{
            "id": "plan",
            "prompt": "Which plan?",
            "mode": "single",
            "options": [{"id": "a", "label": "Option A"}]
        }],
        "unexpected": true
    });

    let result = execute_with_port(arguments, port, &ready_context());

    assert!(matches!(result, Err(Error::Tool(_))));
    assert_eq!(calls.load(Ordering::Acquire), 0);
}

#[test]
fn unknown_nested_question_property_is_rejected_without_calling_the_port() {
    let (port, calls) = ScriptedPort::new(AskUserReply::Cancelled);
    let arguments = json!({
        "questions": [{
            "id": "plan",
            "prompt": "Which plan?",
            "mode": "single",
            "options": [{"id": "a", "label": "Option A"}],
            "unexpected": true
        }]
    });

    let result = execute_with_port(arguments, port, &ready_context());

    assert!(matches!(result, Err(Error::Tool(_))));
    assert_eq!(calls.load(Ordering::Acquire), 0);
}

#[test]
fn unknown_nested_option_property_is_rejected_without_calling_the_port() {
    let (port, calls) = ScriptedPort::new(AskUserReply::Cancelled);
    let arguments = json!({
        "questions": [{
            "id": "plan",
            "prompt": "Which plan?",
            "mode": "single",
            "options": [{"id": "a", "label": "Option A", "unexpected": true}]
        }]
    });

    let result = execute_with_port(arguments, port, &ready_context());

    assert!(matches!(result, Err(Error::Tool(_))));
    assert_eq!(calls.load(Ordering::Acquire), 0);
}

fn request_value() -> Value {
    json!({
        "questions": [{
            "id": "plan",
            "prompt": "Which plan?",
            "mode": "single",
            "allow_other": true,
            "allow_note": true,
            "options": [
                {"id": "a", "label": "Option A"},
                {"id": "b", "label": "Option B"}
            ]
        }, {
            "id": "steps",
            "prompt": "Which steps?",
            "mode": "multiple",
            "allow_discuss": true,
            "options": [
                {"id": "x", "label": "Step X"},
                {"id": "y", "label": "Step Y"}
            ]
        }]
    })
}

#[test]
fn answered_envelope_has_the_exact_documented_bytes() {
    let reply = AskUserReply::Answered(vec![
        AskUserAnswer {
            question_id: "plan".into(),
            selected: vec!["b".into()],
            other: None,
            note: Some("prefer the smaller diff".into()),
        },
        AskUserAnswer {
            question_id: "steps".into(),
            selected: vec!["x".into(), "y".into()],
            other: None,
            note: None,
        },
    ]);
    let (port, _) = ScriptedPort::new(reply);

    let output = execute_with_port(request_value(), port, &ready_context()).expect("executed");

    assert!(!output.is_error);
    assert_eq!(
        output.content,
        "{\"status\":\"answered\",\"answers\":[\
         {\"question_id\":\"plan\",\"selected\":[\"b\"],\"other\":null,\"note\":\"prefer the smaller diff\"},\
         {\"question_id\":\"steps\",\"selected\":[\"x\",\"y\"],\"other\":null,\"note\":null}\
         ]}"
    );
}

#[test]
fn selected_is_reprojected_into_declared_option_order() {
    let reply = AskUserReply::Answered(vec![
        AskUserAnswer {
            question_id: "plan".into(),
            selected: vec!["a".into()],
            other: None,
            note: None,
        },
        AskUserAnswer {
            question_id: "steps".into(),
            selected: vec!["y".into(), "x".into()],
            other: None,
            note: None,
        },
    ]);
    let (port, _) = ScriptedPort::new(reply);

    let output = execute_with_port(request_value(), port, &ready_context()).expect("executed");

    assert!(output.content.contains("\"selected\":[\"x\",\"y\"]"));
}

#[test]
fn discuss_envelope_has_the_exact_documented_bytes() {
    let (port, _) = ScriptedPort::new(AskUserReply::Discuss {
        question_id: "steps".into(),
        note: None,
    });

    let output = execute_with_port(request_value(), port, &ready_context()).expect("executed");

    assert!(!output.is_error);
    assert_eq!(
        output.content,
        "{\"status\":\"discuss\",\"question_id\":\"steps\",\"note\":null}"
    );
}

#[test]
fn cancelled_envelope_has_the_exact_documented_bytes() {
    let (port, _) = ScriptedPort::new(AskUserReply::Cancelled);

    let output = execute_with_port(request_value(), port, &ready_context()).expect("executed");

    assert!(!output.is_error);
    assert_eq!(output.content, "{\"status\":\"cancelled\"}");
}

#[test]
fn unavailable_envelope_has_the_exact_documented_bytes() {
    let (port, _) = ScriptedPort::new(AskUserReply::Unavailable(
        AskUserUnavailable::NoInteractiveSurface,
    ));

    let output = execute_with_port(request_value(), port, &ready_context()).expect("executed");

    assert!(!output.is_error);
    assert_eq!(
        output.content,
        "{\"status\":\"unavailable\",\"reason\":\"no interactive surface\"}"
    );
}

#[test]
fn surface_closed_unavailable_envelope_has_the_exact_documented_bytes() {
    let (port, _) = ScriptedPort::new(AskUserReply::Unavailable(AskUserUnavailable::SurfaceClosed));

    let output = execute_with_port(request_value(), port, &ready_context()).expect("executed");

    assert!(!output.is_error);
    assert_eq!(
        output.content,
        "{\"status\":\"unavailable\",\"reason\":\"interactive surface closed\"}"
    );
}

#[test]
fn expired_envelope_has_the_exact_documented_bytes() {
    let (port, _) = ScriptedPort::new(AskUserReply::Expired);

    let output = execute_with_port(request_value(), port, &ready_context()).expect("executed");

    assert!(!output.is_error);
    assert_eq!(output.content, "{\"status\":\"expired\"}");
}

#[test]
fn already_cancelled_context_short_circuits_before_the_port_is_invoked() {
    let (port, calls) = ScriptedPort::new(AskUserReply::Answered(vec![]));
    let context =
        ToolExecutionContext::new(Arc::new(AtomicBool::new(true)), Duration::from_secs(30));

    let output = execute_with_port(request_value(), port, &context).expect("executed");

    assert!(output.is_error);
    assert_eq!(calls.load(Ordering::Acquire), 0);
}

#[test]
fn expired_deadline_context_short_circuits_before_the_port_is_invoked() {
    let (port, calls) = ScriptedPort::new(AskUserReply::Answered(vec![]));
    let context = ToolExecutionContext::with_timeout(Duration::from_millis(1));
    thread::sleep(Duration::from_millis(10));

    let output = execute_with_port(request_value(), port, &context).expect("executed");

    assert!(output.is_error);
    assert_eq!(calls.load(Ordering::Acquire), 0);
}

#[test]
fn a_lying_port_reply_never_produces_an_answered_envelope() {
    let reply = AskUserReply::Answered(vec![AskUserAnswer {
        question_id: "plan".into(),
        selected: vec!["not-a-real-option".into()],
        other: None,
        note: None,
    }]);
    let (port, calls) = ScriptedPort::new(reply);

    let output = execute_with_port(request_value(), port, &ready_context()).expect("executed");

    assert!(output.is_error);
    assert!(!output.content.contains("\"status\":\"answered\""));
    assert_eq!(calls.load(Ordering::Acquire), 1);
}

#[test]
fn a_lying_port_reply_with_wrong_question_count_never_produces_an_answered_envelope() {
    let reply = AskUserReply::Answered(vec![AskUserAnswer {
        question_id: "plan".into(),
        selected: vec!["a".into()],
        other: None,
        note: None,
    }]);
    let (port, _) = ScriptedPort::new(reply);

    let output = execute_with_port(request_value(), port, &ready_context()).expect("executed");

    assert!(output.is_error);
    assert!(!output.content.contains("\"status\":\"answered\""));
}

#[test]
fn a_lying_port_reply_with_out_of_order_questions_never_produces_an_answered_envelope() {
    // `request_value()` declares `plan` first and `steps` second; this reply
    // supplies both real, valid question ids but in the wrong order, which
    // `AskUserRequest::validate_reply` rejects as `QuestionOutOfOrder` rather
    // than silently reordering it into the declared sequence.
    let reply = AskUserReply::Answered(vec![
        AskUserAnswer {
            question_id: "steps".into(),
            selected: vec!["x".into()],
            other: None,
            note: None,
        },
        AskUserAnswer {
            question_id: "plan".into(),
            selected: vec!["a".into()],
            other: None,
            note: None,
        },
    ]);
    let (port, calls) = ScriptedPort::new(reply);

    let output = execute_with_port(request_value(), port, &ready_context()).expect("executed");

    assert!(output.is_error);
    assert!(!output.content.contains("\"status\":\"answered\""));
    assert_eq!(calls.load(Ordering::Acquire), 1);
}

#[test]
fn permission_target_is_a_constant_independent_of_arguments() {
    let (port, _) = ScriptedPort::new(AskUserReply::Cancelled);
    let tool = AskUserTool::new(Box::new(port));

    assert_eq!(
        tool.permission_target(&json!({"anything": "goes"}))
            .expect("permission target"),
        "ask_user"
    );
}

#[test]
fn duplicate_question_ids_are_rejected_by_domain_validation_before_calling_the_port() {
    let (port, calls) = ScriptedPort::new(AskUserReply::Cancelled);
    let arguments = json!({
        "questions": [{
            "id": "plan",
            "prompt": "Which plan?",
            "mode": "single",
            "options": [{"id": "a", "label": "Option A"}]
        }, {
            "id": "plan",
            "prompt": "Which plan again?",
            "mode": "single",
            "options": [{"id": "a", "label": "Option A"}]
        }]
    });

    let result = execute_with_port(arguments, port, &ready_context());

    assert!(matches!(result, Err(Error::Tool(_))));
    assert_eq!(calls.load(Ordering::Acquire), 0);
}

#[test]
fn wrong_top_level_type_is_rejected_without_calling_the_port() {
    let (port, calls) = ScriptedPort::new(AskUserReply::Cancelled);

    let result = execute_with_port(json!("not an object"), port, &ready_context());

    assert!(matches!(result, Err(Error::Tool(_))));
    assert_eq!(calls.load(Ordering::Acquire), 0);
}

#[test]
fn parses_a_realistic_two_question_request_end_to_end() {
    let expected_request = two_question_request();
    let reply = AskUserReply::Answered(vec![
        AskUserAnswer {
            question_id: "plan".into(),
            selected: vec!["a".into()],
            other: None,
            note: None,
        },
        AskUserAnswer {
            question_id: "steps".into(),
            selected: vec!["x".into()],
            other: None,
            note: None,
        },
    ]);
    let (port, _) = ScriptedPort::new(reply);
    let arguments = json!({
        "questions": [{
            "id": "plan",
            "prompt": "Which plan?",
            "mode": "single",
            "allow_other": true,
            "allow_note": true,
            "options": [
                {"id": "a", "label": "Option A"},
                {"id": "b", "label": "Option B"}
            ]
        }, {
            "id": "steps",
            "prompt": "Which steps?",
            "mode": "multiple",
            "allow_discuss": true,
            "options": [
                {"id": "x", "label": "Step X"},
                {"id": "y", "label": "Step Y"}
            ]
        }]
    });

    let output = execute_with_port(arguments, port, &ready_context()).expect("executed");

    assert!(!output.is_error);
    assert_eq!(
        expected_request.questions().len(),
        2,
        "fixture stays in sync with the arguments above"
    );
}
