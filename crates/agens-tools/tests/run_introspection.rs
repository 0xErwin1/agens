//! The two introspection tools: what they accept, what they refuse, and what
//! reaches the port.

use std::sync::{Arc, Mutex, atomic::AtomicBool};
use std::time::Duration;

use agens_core::run_introspection::{
    Ask, AskReceipt, Checkpoint, CheckpointReceipt, EvidenceClass, RunIntrospectionError,
    RunIntrospectionPort, UnavailableRunIntrospectionPort,
};
use agens_tools::{AskTool, CheckpointTool, DispatchTool, ToolExecutionContext};
use serde_json::{Value, json};

/// Keeps whatever reached it, so a test can assert on the typed payload rather
/// than on the string the tool printed.
#[derive(Default)]
struct Recorded {
    checkpoints: Vec<Checkpoint>,
    asks: Vec<Ask>,
}

#[derive(Clone, Default)]
struct RecordingPort {
    recorded: Arc<Mutex<Recorded>>,
}

impl RunIntrospectionPort for RecordingPort {
    fn checkpoint(
        &mut self,
        checkpoint: &Checkpoint,
    ) -> Result<CheckpointReceipt, RunIntrospectionError> {
        self.recorded
            .lock()
            .unwrap()
            .checkpoints
            .push(checkpoint.clone());

        Ok(CheckpointReceipt {
            checkpoint_event_id: 7,
            finding_ids: vec![11, 12],
            credited_progress: checkpoint.credits_progress(),
        })
    }

    fn ask(&mut self, ask: &Ask) -> Result<AskReceipt, RunIntrospectionError> {
        self.recorded.lock().unwrap().asks.push(ask.clone());

        Ok(AskReceipt {
            question_id: 5,
            run_id: 3,
        })
    }
}

fn context() -> ToolExecutionContext {
    ToolExecutionContext::with_timeout(Duration::from_secs(5))
}

fn cancelled_context() -> ToolExecutionContext {
    let cancellation = Arc::new(AtomicBool::new(true));

    ToolExecutionContext::new(cancellation, Duration::from_secs(5))
}

fn run_checkpoint(port: RecordingPort, arguments: Value) -> String {
    let mut tool = CheckpointTool::new(Box::new(port));
    let output = tool.execute(&context(), arguments).expect("the tool runs");

    output.content
}

fn run_ask(port: RecordingPort, arguments: Value) -> String {
    let mut tool = AskTool::new(Box::new(port));
    let output = tool.execute(&context(), arguments).expect("the tool runs");

    output.content
}

fn full_checkpoint() -> Value {
    json!({
        "evidence": [
            {
                "description": "the header parser rejects an empty name",
                "evidence_class": "deterministic",
                "proof_refs": ["cargo test -p agens-core rejects_empty_header => 0"],
            },
            {
                "description": "the old caller was already broken",
                "evidence_class": "inferential",
                "disposition": "pre_existing",
            },
        ],
        "hypothesis": "the guard runs before the write",
        "next_goal": "wire the rejection into the caller",
        "revised_estimate_seconds": 1_800,
        "blockers": ["the schema seam is not merged yet"],
        "next_checkpoint_at": 1_700_003_600,
        "touched_paths": ["crates/agens-core/src/lib.rs"],
    })
}

#[test]
fn a_checkpoint_reaches_the_port_as_a_typed_payload() {
    let port = RecordingPort::default();
    let recorded = Arc::clone(&port.recorded);

    run_checkpoint(port, full_checkpoint());

    let recorded = recorded.lock().unwrap();
    let checkpoint = recorded.checkpoints.first().expect("one checkpoint");

    assert_eq!(checkpoint.next_goal(), "wire the rejection into the caller");
    assert_eq!(
        checkpoint.hypothesis(),
        Some("the guard runs before the write")
    );
    assert_eq!(checkpoint.revised_estimate_seconds(), Some(1_800));
    assert_eq!(checkpoint.blockers(), ["the schema seam is not merged yet"]);
    assert_eq!(checkpoint.next_checkpoint_at(), Some(1_700_003_600));
    assert_eq!(checkpoint.touched_paths(), ["crates/agens-core/src/lib.rs"]);

    let classes = checkpoint
        .claims()
        .iter()
        .map(|claim| claim.evidence_class())
        .collect::<Vec<_>>();

    assert_eq!(
        classes,
        [EvidenceClass::Deterministic, EvidenceClass::Inferential]
    );
    assert_eq!(
        checkpoint.claims()[0].proof_refs(),
        ["cargo test -p agens-core rejects_empty_header => 0"]
    );
    assert!(checkpoint.credits_progress());
}

#[test]
fn a_recorded_checkpoint_reports_what_it_wrote_and_whether_it_credited_progress() {
    let text = run_checkpoint(RecordingPort::default(), full_checkpoint());
    let envelope: Value = serde_json::from_str(&text).expect("the envelope is JSON");

    assert_eq!(
        envelope,
        json!({
            "status": "recorded",
            "checkpoint_id": 7,
            "finding_ids": [11, 12],
            "credited_progress": true,
        })
    );
}

/// The refusal is the whole point of the class: an unproven claim reported as
/// deterministic would credit progress it did not earn, so the tool says what
/// to do instead rather than only that the payload was wrong.
#[test]
fn a_deterministic_claim_with_no_proof_is_refused_with_the_alternative_named() {
    let text = run_checkpoint(
        RecordingPort::default(),
        json!({
            "next_goal": "keep going",
            "evidence": [{"description": "it works now", "evidence_class": "deterministic"}],
        }),
    );

    assert!(text.contains("proof reference"), "{text}");
    assert!(text.contains("inferential or insufficient"), "{text}");
}

#[test]
fn an_unknown_evidence_class_is_refused() {
    let text = run_checkpoint(
        RecordingPort::default(),
        json!({
            "next_goal": "keep going",
            "evidence": [{"description": "it works", "evidence_class": "probably"}],
        }),
    );

    assert!(
        text.contains("deterministic, inferential or insufficient"),
        "{text}"
    );
}

#[test]
fn a_checkpoint_without_a_next_goal_is_refused() {
    let text = run_checkpoint(RecordingPort::default(), json!({"hypothesis": "maybe"}));

    assert!(text.contains("next_goal is required"), "{text}");
}

/// A key the schema does not name is a payload the worker thinks it is sending
/// and the tool would silently drop.
#[test]
fn an_unknown_key_is_refused_rather_than_ignored() {
    let text = run_checkpoint(
        RecordingPort::default(),
        json!({"next_goal": "keep going", "confidence": "high"}),
    );

    assert!(text.contains("arguments are invalid"), "{text}");
}

#[test]
fn an_ask_reaches_the_port_as_a_typed_payload_and_names_the_parked_run() {
    let port = RecordingPort::default();
    let recorded = Arc::clone(&port.recorded);

    let text = run_ask(
        port,
        json!({
            "blocked_decision": "the two schemas disagree about the option column",
            "options": [
                {"id": "keep", "label": "keep the JSON array", "consequence": "no migration"},
                {"id": "split", "label": "split it into its own table"},
            ],
            "recommendation": "keep",
        }),
    );

    let recorded = recorded.lock().unwrap();
    let ask = recorded.asks.first().expect("one question");

    assert_eq!(
        ask.blocked_decision(),
        "the two schemas disagree about the option column"
    );
    assert_eq!(ask.options().len(), 2);
    assert_eq!(ask.options()[0].consequence(), Some("no migration"));
    assert_eq!(ask.options()[1].consequence(), None);
    assert_eq!(ask.recommendation(), Some("keep"));

    let envelope: Value = serde_json::from_str(&text).expect("the envelope is JSON");
    assert_eq!(
        envelope,
        json!({
            "status": "asked",
            "question_id": 5,
            "run_id": 3,
            "run_state": "awaiting_input",
        })
    );
}

#[test]
fn a_question_with_nothing_to_choose_between_is_refused() {
    let text = run_ask(
        RecordingPort::default(),
        json!({"blocked_decision": "what now", "options": []}),
    );

    assert!(text.contains("options it is choosing between"), "{text}");
}

#[test]
fn a_recommendation_that_names_no_option_is_refused() {
    let text = run_ask(
        RecordingPort::default(),
        json!({
            "blocked_decision": "what now",
            "options": [{"id": "keep", "label": "keep it"}],
            "recommendation": "migrate",
        }),
    );

    assert!(text.contains("must name one of the options"), "{text}");
}

/// Outside team mode the port answers that there is no run, and the worker
/// reads that rather than a tool that silently did nothing.
#[test]
fn a_session_that_is_not_executing_a_run_is_told_so() {
    let mut checkpoint = CheckpointTool::new(Box::new(UnavailableRunIntrospectionPort));
    let output = checkpoint
        .execute(&context(), full_checkpoint())
        .expect("the tool runs");

    assert!(output.is_error);
    assert!(
        output.content.contains("not executing a run"),
        "{}",
        output.content
    );

    let mut ask = AskTool::new(Box::new(UnavailableRunIntrospectionPort));
    let output = ask
        .execute(
            &context(),
            json!({
                "blocked_decision": "what now",
                "options": [{"id": "keep", "label": "keep it"}],
            }),
        )
        .expect("the tool runs");

    assert!(output.is_error);
    assert!(
        output.content.contains("not executing a run"),
        "{}",
        output.content
    );
}

/// A cancelled turn writes nothing: a question filed after the run was
/// cancelled would park a run that is already gone.
#[test]
fn a_cancelled_turn_reaches_neither_port() {
    let port = RecordingPort::default();
    let recorded = Arc::clone(&port.recorded);

    let mut checkpoint = CheckpointTool::new(Box::new(port.clone()));
    checkpoint
        .execute(&cancelled_context(), full_checkpoint())
        .expect("the tool runs");

    let mut ask = AskTool::new(Box::new(port));
    ask.execute(
        &cancelled_context(),
        json!({
            "blocked_decision": "what now",
            "options": [{"id": "keep", "label": "keep it"}],
        }),
    )
    .expect("the tool runs");

    let recorded = recorded.lock().unwrap();
    assert!(recorded.checkpoints.is_empty());
    assert!(recorded.asks.is_empty());
}

/// Neither tool projects a permission target out of its arguments: there is
/// nothing in a checkpoint or a question a rule could usefully match, so both
/// are decided on the tool name alone.
#[test]
fn both_tools_are_decided_on_their_own_name() {
    let checkpoint = CheckpointTool::new(Box::new(UnavailableRunIntrospectionPort));
    let ask = AskTool::new(Box::new(UnavailableRunIntrospectionPort));

    assert_eq!(
        checkpoint.permission_target(&full_checkpoint()).unwrap(),
        "checkpoint"
    );
    assert_eq!(ask.permission_target(&json!({})).unwrap(), "ask");
}
