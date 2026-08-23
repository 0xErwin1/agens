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
use agens_server::{
    ApiCore, CHECKPOINT_EVENT, CheckpointClaim, Delivery, DeliveryQueue, EventFeed, EventFilter,
    HookTrust, PendingHookTrust, PortError, Ports, ProvisionedWorktree, ReportedCheckpoint,
    RepositoryIdentity, RepositoryPolicy, RunIntrospection, SchedulerPort, SessionControl,
    StateMachines, StopScope, Subscription, TakeoverHandle, TimerSettings, TimerWheel,
    WorktreeDerivation, WorktreeGate, WorktreeRequest,
};
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

/// Ports nothing in this file reaches.
///
/// A worker's checkpoint and its question are written through the state
/// machines alone: neither declares an effect outside the transaction, so a
/// port that answered anything here would be answering a call that never
/// happens.
struct Unreached;

impl SchedulerPort for Unreached {
    fn admissions_paused(&self) -> bool {
        false
    }

    fn set_admissions_paused(&self, _paused: bool) -> Result<bool, PortError> {
        Err(unreached("scheduler"))
    }

    fn queue_changed(&self, _run_id: i64) {}
}

impl WorktreeGate for Unreached {
    fn derive(&self, _run: &RunRow) -> Result<WorktreeDerivation, PortError> {
        Err(unreached("worktrees"))
    }

    fn remove(&self, _run: &RunRow) -> Result<(), PortError> {
        Err(unreached("worktrees"))
    }

    fn identify(&self, _repository: &std::path::Path) -> Result<RepositoryIdentity, PortError> {
        Err(unreached("worktrees"))
    }

    fn provision(&self, _request: &WorktreeRequest<'_>) -> Result<ProvisionedWorktree, PortError> {
        Err(unreached("worktrees"))
    }
}

impl DeliveryQueue for Unreached {
    fn enqueue(&self, _delivery: &Delivery) -> Result<(), PortError> {
        Err(unreached("delivery"))
    }
}

impl SessionControl for Unreached {
    fn cancel(&self, _run_id: i64) -> Result<(), PortError> {
        Err(unreached("sessions"))
    }

    fn take_over(&self, _run_id: i64) -> Result<TakeoverHandle, PortError> {
        Err(unreached("sessions"))
    }

    fn stop(&self, _scope: &StopScope) -> Result<(), PortError> {
        Err(unreached("sessions"))
    }
}

impl EventFeed for Unreached {
    fn subscribe(&self, _filter: &EventFilter) -> Result<Subscription, PortError> {
        Err(unreached("feed"))
    }
}

impl RepositoryPolicy for Unreached {
    fn admits(&self, _repository: &std::path::Path) -> bool {
        false
    }

    fn admission_remedy(&self) -> String {
        "no introspection write names a repository".to_owned()
    }

    fn hook_trust(&self, _repo_id: &str) -> HookTrust {
        HookTrust::Refused
    }

    fn hook_exports(&self) -> Vec<String> {
        Vec::new()
    }

    fn record_pending(&self, _pending: &PendingHookTrust) -> Result<(), PortError> {
        Err(unreached("policy"))
    }

    fn is_pending(&self, _question_id: i64) -> bool {
        false
    }

    fn resolve_pending(&self, _question_id: i64, _granted: bool) -> Result<bool, PortError> {
        Ok(false)
    }
}

fn unreached(port: &'static str) -> PortError {
    PortError::new(port, "no introspection write reaches a port")
}

fn ports() -> Ports {
    let unreached = Arc::new(Unreached);

    Ports {
        scheduler: Arc::clone(&unreached) as Arc<dyn SchedulerPort>,
        worktrees: Arc::clone(&unreached) as Arc<dyn WorktreeGate>,
        delivery: Arc::clone(&unreached) as Arc<dyn DeliveryQueue>,
        sessions: Arc::clone(&unreached) as Arc<dyn SessionControl>,
        feed: unreached as Arc<dyn EventFeed>,
    }
}

/// A run in `state`, its machines, and the introspection surface bound to it.
fn fixture(
    state: RunState,
) -> (
    std::path::PathBuf,
    Arc<Mutex<ApiCore>>,
    i64,
    RunIntrospection,
) {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store.insert_run(&run_in(state)).unwrap();
    let core = Arc::new(Mutex::new(ApiCore::new(
        StateMachines::new(store),
        ports(),
        Arc::new(Unreached) as Arc<dyn RepositoryPolicy>,
    )));
    let introspection = RunIntrospection::new(Arc::clone(&core), run_id, Arc::new(|| NOW))
        .for_attempt(Some(11), Some(22));

    (directory, core, run_id, introspection)
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

fn payload(core: &Arc<Mutex<ApiCore>>, run_id: i64) -> serde_json::Value {
    let event = core
        .lock()
        .unwrap()
        .machines()
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
    let (directory, core, run_id, mut introspection) = fixture(RunState::Running);

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

    let core = core.lock().unwrap();
    let findings = core.machines().store().findings_for_run(run_id).unwrap();

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

    drop(core);
    fs::remove_dir_all(directory).unwrap();
}

/// Run health reads the journal, so the class and the credit it implies are in
/// the payload rather than left for a reader to recompute from the findings.
#[test]
fn the_journal_payload_carries_the_evidence_classes_run_health_consumes() {
    let (directory, core, run_id, mut introspection) = fixture(RunState::Running);

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

    let payload = payload(&core, run_id);

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
    let (directory, core, run_id, mut introspection) = fixture(RunState::Running);

    introspection
        .checkpoint(&checkpoint(
            Vec::new(),
            vec!["crates/agens-tools/src/run_introspection.rs".to_owned()],
        ))
        .expect("a running run accepts a checkpoint");

    let payload = payload(&core, run_id);

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
    let (directory, core, run_id, mut introspection) = fixture(RunState::Running);

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
        payload(&core, run_id)["credits_progress"],
        serde_json::json!(false)
    );

    fs::remove_dir_all(directory).unwrap();
}

/// A checkpoint reports; it does not move the run. The worker keeps working.
#[test]
fn a_checkpoint_leaves_the_run_where_it_was() {
    let (directory, core, run_id, mut introspection) = fixture(RunState::Running);

    introspection
        .checkpoint(&checkpoint(Vec::new(), Vec::new()))
        .expect("a running run accepts a checkpoint");

    assert_eq!(
        core.lock()
            .unwrap()
            .machines()
            .store()
            .load_run(run_id)
            .unwrap()
            .unwrap()
            .state,
        RunState::Running
    );

    fs::remove_dir_all(directory).unwrap();
}

/// The wheel reads the promised deadline out of the checkpoint payload under
/// one name. Writing it under any other name is a checkpoint that declares no
/// deadline at all, and nothing would ever say so — the run would simply never
/// be reported overdue.
#[test]
fn the_deadline_a_checkpoint_declares_is_the_one_the_timer_wheel_holds_it_to() {
    let (directory, core, run_id, mut introspection) = fixture(RunState::Running);

    let receipt = introspection
        .checkpoint(&checkpoint(Vec::new(), Vec::new()))
        .expect("a running run accepts a checkpoint");

    let (wheel, clock) = TimerWheel::with_manual_clock_for_test(TimerSettings::default(), NOW);

    let tick = core.lock().unwrap().advance_timers(&wheel).unwrap();
    assert!(
        tick.overdue_checkpoints.is_empty(),
        "a promise that has not come due yet is not overdue: {tick:?}"
    );

    clock.set(NOW + 100_000);
    let tick = core.lock().unwrap().advance_timers(&wheel).unwrap();

    assert_eq!(
        tick.overdue_checkpoints
            .iter()
            .map(|overdue| (
                overdue.run_id,
                overdue.checkpoint_event_id,
                overdue.promised_at
            ))
            .collect::<Vec<_>>(),
        vec![(run_id, receipt.checkpoint_event_id, NOW + 3_600)],
        "the wheel has to find the deadline this checkpoint declared"
    );

    fs::remove_dir_all(directory).unwrap();
}

/// Ingest declared the half of a checkpoint it consumes as a trait rather than
/// depending on this row's shape. Two derivations of one rule drift, so the
/// credit the journal records and the credit health derives are checked against
/// each other here rather than assumed equal.
#[test]
fn what_ingest_reads_off_a_claim_agrees_with_what_the_journal_records() {
    let cases = [
        (
            claim(
                "the parser rejects an empty header",
                EvidenceClass::Deterministic,
                &["cargo test => 0"],
            ),
            true,
        ),
        (
            claim("reasoned to it", EvidenceClass::Inferential, &[]),
            false,
        ),
        (
            claim("not established", EvidenceClass::Insufficient, &[]),
            false,
        ),
        (
            EvidenceClaim::new(
                "the suite was already red on main",
                vec!["git stash && cargo test => 101".to_owned()],
                EvidenceClass::Deterministic,
                CausalDisposition::PreExisting,
            ),
            false,
        ),
    ];

    for (claim, credits) in cases {
        assert_eq!(
            ReportedCheckpoint::from_claim(&claim).credits_progress(),
            credits,
            "ingest disagrees about {:?}",
            claim.description()
        );
        assert_eq!(
            claim.credits_progress(),
            credits,
            "the journal disagrees about {:?}",
            claim.description()
        );
        assert_eq!(
            CheckpointClaim::evidence_class(&claim).as_str(),
            claim.evidence_class().as_str(),
            "the class ingest reads is not the class the claim carries"
        );
    }
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
    let (directory, core, run_id, mut introspection) = fixture(RunState::Running);

    let receipt = introspection.ask(&ask()).expect("a running run can ask");

    assert_eq!(receipt.run_id, run_id);

    let core = core.lock().unwrap();
    let question = core
        .machines()
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
        core.machines()
            .store()
            .load_run(run_id)
            .unwrap()
            .unwrap()
            .state,
        RunState::AwaitingInput
    );
    assert_eq!(
        core.machines()
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

    drop(core);
    fs::remove_dir_all(directory).unwrap();
}

/// A run that is not running has no path to `awaiting_input`, and a refused
/// park leaves no question behind: an open row against a run nobody is waiting
/// on is an inbox entry that cannot be acted on.
#[test]
fn a_refused_ask_writes_no_question() {
    let (directory, core, run_id, mut introspection) = fixture(RunState::Queued);

    let error = introspection.ask(&ask()).unwrap_err();

    assert!(
        matches!(error, RunIntrospectionError::Refused(_)),
        "{error:?}"
    );

    let core = core.lock().unwrap();

    assert!(
        core.machines()
            .store()
            .questions_for_run(run_id)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        core.machines()
            .store()
            .load_run(run_id)
            .unwrap()
            .unwrap()
            .state,
        RunState::Queued
    );
    assert!(
        core.machines()
            .store()
            .events_for_run(run_id)
            .unwrap()
            .is_empty()
    );

    drop(core);
    fs::remove_dir_all(directory).unwrap();
}
