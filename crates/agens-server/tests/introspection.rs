//! What a worker's checkpoints and questions leave behind in the control
//! plane.

use std::{
    fs,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use agens_core::run_introspection::{
    Ask, AskOption, CausalDisposition, Checkpoint, EvidenceClaim, EvidenceClass,
    RunIntrospectionError, RunIntrospectionPort,
};
use agens_server::{CHECKPOINT_EVENT, RunIntrospection, StateMachines};
use agens_store::{ControlPlaneStore, QuestionKind, QuestionState, RunRow, RunState};

const NOW: i64 = 1_700_000_500;

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

fn data_directory() -> std::path::PathBuf {
    let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    let directory = std::env::temp_dir().join(format!(
        "agens-server-introspection-{}-{suffix}",
        std::process::id()
    ));
    fs::create_dir_all(&directory).unwrap();
    directory
}

fn run_in(state: RunState) -> RunRow {
    RunRow {
        id: None,
        repo_id: "a1b2c3d4e5f60718".to_owned(),
        repo_root: "/home/dev/agens".to_owned(),
        remote_url: None,
        external_ref: Some("agens/AGN-58".to_owned()),
        parent_run_id: None,
        task: "checkpoint and ask".to_owned(),
        scope: "crates/agens-tools".to_owned(),
        dod: "typed payloads and durable rows".to_owned(),
        genesis_paths: None,
        state,
        priority: 5,
        dep_run_id: None,
        provider: "anthropic".to_owned(),
        budget_tokens: Some(200_000),
        worktree_path: Some("/data/worktrees/agens-a1b2c3d4/agn-58".to_owned()),
        worktree_status: None,
        created_at: 1_700_000_000,
        result: None,
    }
}

/// A run in `state`, its machines, and the introspection surface bound to it.
fn fixture(
    state: RunState,
) -> (
    std::path::PathBuf,
    Arc<Mutex<StateMachines>>,
    i64,
    RunIntrospection,
) {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store.insert_run(&run_in(state)).unwrap();
    let machines = Arc::new(Mutex::new(StateMachines::new(store)));
    let introspection = RunIntrospection::new(Arc::clone(&machines), run_id, Arc::new(|| NOW))
        .for_attempt(Some(11), Some(22));

    (directory, machines, run_id, introspection)
}

fn claim(description: &str, class: EvidenceClass, proofs: &[&str]) -> EvidenceClaim {
    EvidenceClaim::new(
        description,
        proofs.iter().map(|proof| (*proof).to_owned()).collect(),
        class,
        CausalDisposition::CandidateCaused,
    )
}

fn checkpoint(claims: Vec<EvidenceClaim>, touched_paths: Vec<String>) -> Checkpoint {
    Checkpoint::new(
        claims,
        Some("the guard runs before the write".to_owned()),
        "wire the tool into the dispatcher".to_owned(),
        Some(1_800),
        vec!["the schema seam is not merged yet".to_owned()],
        Some(NOW + 3_600),
        touched_paths,
    )
    .expect("the checkpoint is valid")
}

fn payload(machines: &Arc<Mutex<StateMachines>>, run_id: i64) -> serde_json::Value {
    let event = machines
        .lock()
        .unwrap()
        .store()
        .events_for_run(run_id)
        .unwrap()
        .into_iter()
        .find(|event| event.event_type == CHECKPOINT_EVENT)
        .expect("the checkpoint was journaled");

    serde_json::from_str(&event.payload).expect("the journal payload is JSON")
}

#[test]
fn a_checkpoint_journals_one_entry_and_a_finding_for_every_claim() {
    let (directory, machines, run_id, mut introspection) = fixture(RunState::Running);

    let receipt = introspection
        .checkpoint(&checkpoint(
            vec![
                claim(
                    "the parser rejects an empty header",
                    EvidenceClass::Deterministic,
                    &["cargo test -p agens-core rejects_empty_header => 0"],
                ),
                claim(
                    "the caller probably still passes the old shape",
                    EvidenceClass::Inferential,
                    &[],
                ),
            ],
            vec!["crates/agens-core/src/lib.rs".to_owned()],
        ))
        .expect("a running run accepts a checkpoint");

    assert_eq!(receipt.finding_ids.len(), 2);
    assert!(receipt.credited_progress);

    let machines = machines.lock().unwrap();
    let findings = machines.store().findings_for_run(run_id).unwrap();

    assert_eq!(
        findings
            .iter()
            .map(|finding| finding.checkpoint_id)
            .collect::<Vec<_>>(),
        vec![
            Some(receipt.checkpoint_event_id),
            Some(receipt.checkpoint_event_id)
        ],
        "every finding is attributed to the checkpoint that carried it"
    );
    assert_eq!(
        findings
            .iter()
            .map(|finding| finding.evidence_class)
            .collect::<Vec<_>>(),
        vec![
            agens_store::EvidenceClass::Deterministic,
            agens_store::EvidenceClass::Inferential
        ]
    );
    assert_eq!(
        findings[0].proof_refs,
        "[\"cargo test -p agens-core rejects_empty_header => 0\"]"
    );

    drop(machines);
    fs::remove_dir_all(directory).unwrap();
}

/// Run health reads the journal, so the class and the credit it implies are in
/// the payload rather than left for a reader to recompute from the findings.
#[test]
fn the_journal_payload_carries_the_evidence_classes_run_health_consumes() {
    let (directory, machines, run_id, mut introspection) = fixture(RunState::Running);

    introspection
        .checkpoint(&checkpoint(
            vec![
                claim("proved", EvidenceClass::Deterministic, &["exit 0"]),
                claim("reasoned", EvidenceClass::Inferential, &[]),
                claim("not established", EvidenceClass::Insufficient, &[]),
            ],
            Vec::new(),
        ))
        .expect("a running run accepts a checkpoint");

    let payload = payload(&machines, run_id);

    assert_eq!(payload["credits_progress"], serde_json::json!(true));
    assert_eq!(
        payload["evidence_classes"],
        serde_json::json!({"deterministic": 1, "inferential": 1, "insufficient": 1})
    );
    assert_eq!(
        payload["evidence"][0]["evidence_class"],
        serde_json::json!("deterministic")
    );
    assert_eq!(
        payload["evidence"][2]["credits_progress"],
        serde_json::json!(false)
    );

    fs::remove_dir_all(directory).unwrap();
}

/// The genesis-path freeze waits for the first checkpoint with a diff and
/// checks it against the evidence ledger, which is keyed by the physical
/// attempt. Both halves are in the payload or the freeze has nothing to
/// correlate.
#[test]
fn the_journal_payload_correlates_the_diff_with_the_attempt_that_produced_it() {
    let (directory, machines, run_id, mut introspection) = fixture(RunState::Running);

    introspection
        .checkpoint(&checkpoint(
            Vec::new(),
            vec!["crates/agens-tools/src/run_introspection.rs".to_owned()],
        ))
        .expect("a running run accepts a checkpoint");

    let payload = payload(&machines, run_id);

    assert_eq!(payload["carries_diff"], serde_json::json!(true));
    assert_eq!(
        payload["touched_paths"],
        serde_json::json!(["crates/agens-tools/src/run_introspection.rs"])
    );
    assert_eq!(
        payload["attempt"],
        serde_json::json!({"session_id": 11, "session_attempt_id": 22})
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_checkpoint_without_a_proved_claim_credits_no_progress() {
    let (directory, machines, run_id, mut introspection) = fixture(RunState::Running);

    let receipt = introspection
        .checkpoint(&checkpoint(
            vec![
                claim("reasoned", EvidenceClass::Inferential, &[]),
                claim("not established", EvidenceClass::Insufficient, &[]),
            ],
            Vec::new(),
        ))
        .expect("a running run accepts a checkpoint");

    assert!(!receipt.credited_progress);
    assert_eq!(
        payload(&machines, run_id)["credits_progress"],
        serde_json::json!(false)
    );

    fs::remove_dir_all(directory).unwrap();
}

/// A checkpoint reports; it does not move the run. The worker keeps working.
#[test]
fn a_checkpoint_leaves_the_run_where_it_was() {
    let (directory, machines, run_id, mut introspection) = fixture(RunState::Running);

    introspection
        .checkpoint(&checkpoint(Vec::new(), Vec::new()))
        .expect("a running run accepts a checkpoint");

    assert_eq!(
        machines
            .lock()
            .unwrap()
            .store()
            .load_run(run_id)
            .unwrap()
            .unwrap()
            .state,
        RunState::Running
    );

    fs::remove_dir_all(directory).unwrap();
}

fn ask() -> Ask {
    Ask::new(
        "the two schemas disagree about the option column".to_owned(),
        vec![
            AskOption::new(
                "keep",
                "keep the JSON array",
                Some("no migration".to_owned()),
            ),
            AskOption::new("split", "split it into its own table", None),
        ],
        Some("keep".to_owned()),
    )
    .expect("the question is valid")
}

#[test]
fn ask_opens_a_durable_question_and_parks_the_run_on_it() {
    let (directory, machines, run_id, mut introspection) = fixture(RunState::Running);

    let receipt = introspection.ask(&ask()).expect("a running run can ask");

    assert_eq!(receipt.run_id, run_id);

    let machines = machines.lock().unwrap();
    let question = machines
        .store()
        .load_question(receipt.question_id)
        .unwrap()
        .expect("the question is durable");

    assert_eq!(question.run_id, run_id);
    assert_eq!(question.kind, QuestionKind::Question);
    assert_eq!(question.state, QuestionState::Open);
    assert_eq!(question.recommendation.as_deref(), Some("keep"));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&question.options).unwrap(),
        serde_json::json!([
            {"id": "keep", "label": "keep the JSON array", "consequence": "no migration"},
            {"id": "split", "label": "split it into its own table", "consequence": null},
        ])
    );
    assert_eq!(
        machines.store().load_run(run_id).unwrap().unwrap().state,
        RunState::AwaitingInput
    );
    assert_eq!(
        machines
            .store()
            .events_for_run(run_id)
            .unwrap()
            .into_iter()
            .map(|event| event.event_type)
            .collect::<Vec<_>>(),
        vec![
            "run_state_changed".to_owned(),
            "run_awaiting_input".to_owned()
        ],
        "no transition is silent"
    );

    drop(machines);
    fs::remove_dir_all(directory).unwrap();
}

/// A run that is not running has no path to `awaiting_input`, and a refused
/// park leaves no question behind: an open row against a run nobody is waiting
/// on is an inbox entry that cannot be acted on.
#[test]
fn a_refused_ask_writes_no_question() {
    let (directory, machines, run_id, mut introspection) = fixture(RunState::Queued);

    let error = introspection.ask(&ask()).unwrap_err();

    assert!(
        matches!(error, RunIntrospectionError::Refused(_)),
        "{error:?}"
    );

    let machines = machines.lock().unwrap();

    assert!(
        machines
            .store()
            .questions_for_run(run_id)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        machines.store().load_run(run_id).unwrap().unwrap().state,
        RunState::Queued
    );
    assert!(machines.store().events_for_run(run_id).unwrap().is_empty());

    drop(machines);
    fs::remove_dir_all(directory).unwrap();
}
