//! One run of the composed daemon, end to end, with a real agent loop in it.
//!
//! Everything is production except the model. The daemon is the composed one,
//! the client speaks to it over its own socket, the worker is the one `agens
//! serve` supplies, and the turn it takes is a real headless turn against a
//! real streaming endpoint — the endpoint is just scripted, so what the model
//! does is decided by the test instead of by a provider.
//!
//! What it asserts is the whole path the harness exists for: a run is created
//! and its worktree provisioned, the user approves it, the scheduler admits it,
//! the worker's turn writes a file, reports a checkpoint and then asks a
//! question, the run parks on a person, the answer requeues it, and the resumed
//! session finishes the run.
//!
//! It also asserts the health plane the run is measured through, which exists
//! only once the worker actually reports: the physical attempt is correlated so
//! the evidence ledger can be reached from the run, `run_health` is derived
//! from what the turn did, the genesis paths are frozen off that ledger at the
//! first checkpoint, and a checkpoint whose deadline passes reaches the
//! lost-worker detector through the timer wheel.
//!
//! The second journey is the one where the model never speaks at all: the
//! provider refuses the turn for quota, and the run parks on the reset it
//! named until the timer wheel brings it back.

use std::path::Path;
use std::time::{Duration, Instant};

use agens_fixtures::{Script, ScriptedTurn};
use agens_server::grpc::proto::{self, feed_client::FeedClient, team_client::TeamClient};
use agens_server::{CoordinatorSettings, TimerSettings};
use agens_store::{AttemptOutcome, ControlPlaneStore, QuotaState};
use tonic::transport::Channel;

use crate::daemon_fixture::{
    DaemonFixture, PATIENCE, await_reported_state, connect, daemon_settings, journal_of,
};

const ANSWER: &str = "split";

/// The file the worker's turn writes, which is what puts a row in the evidence
/// ledger and therefore what the genesis paths freeze to.
const TOUCHED_PATH: &str = "notes.md";

/// The journal entries the health plane writes.
///
/// Kept out of the lifecycle assertion because ingest drains on its own
/// heartbeat: what it wrote is asserted on its own, and where its entries fall
/// among the run's transitions is not a fact about the run.
const HEALTH_EVENTS: [&str; 8] = [
    "turn_started",
    "turn_ended",
    "tool_result_fact",
    "checkpoint_recorded",
    "genesis_paths_frozen",
    "checkpoint_overdue",
    "checkpoint_expired",
    "worker_lost",
];

/// What the admission loop writes about a queue rather than about a run.
///
/// Filtered out of the run's own story because it is timing, not lifecycle: a
/// run coming back from an answer is offered a slot while the session it was
/// parked in is still ending, so whether that first launch is refused depends
/// on which of the two the scheduler reaches first.
const SCHEDULING_EVENTS: [&str; 2] = ["run_deferred", "admission_failed"];

/// What the health plane has to have written by the time the run is done.
const EXPECTED_HEALTH_EVENTS: [&str; 7] = [
    "turn_started",
    "tool_result_fact",
    "checkpoint_recorded",
    "genesis_paths_frozen",
    "turn_ended",
    "checkpoint_expired",
    "worker_lost",
];

/// The model's side of both sessions.
///
/// The first is the whole shape of a worker's turn: do some work, report the
/// milestone it produced, keep working, then raise the one decision it cannot
/// make and stop — because the run parks on the question and there is nothing
/// else for the turn to do. The second is the resumed session, which answers
/// and ends, and ending is what reports the run finished.
///
/// The work is a real write, because the genesis-path freeze reads the evidence
/// ledger and never the checkpoint's own list: a turn that only talks leaves
/// that ledger empty and freezes nothing. The command after the checkpoint is
/// what keeps the run executing long enough for the timer wheel to look at the
/// deadline the checkpoint just declared.
fn script(promised_at: i64) -> Script {
    Script::new([
        ScriptedTurn::tool_call(
            "call-write",
            "write",
            serde_json::json!({
                "path": TOUCHED_PATH,
                "content": "the options now live in their own table\n",
            })
            .to_string(),
        ),
        ScriptedTurn::tool_call(
            "call-checkpoint",
            "checkpoint",
            serde_json::json!({
                "next_goal": "split the options into their own table",
                "evidence": [{
                    "description": "cargo test -p agens-store passes",
                    "evidence_class": "deterministic",
                    "proof_refs": ["cargo test -p agens-store"],
                    "disposition": "candidate_caused",
                }],
                "touched_paths": [TOUCHED_PATH],
                "next_checkpoint_at": promised_at,
            })
            .to_string(),
        ),
        ScriptedTurn::tool_call("call-sleep", "bash", r#"{"command":"sleep 1"}"#),
        ScriptedTurn::tool_call(
            "call-ask",
            "ask",
            r#"{"blocked_decision":"keep the options as JSON or split them into a table","options":[{"id":"keep","label":"keep the JSON array"},{"id":"split","label":"split it into its own table"}],"recommendation":"split"}"#,
        ),
        // One turn after the ask, and only one: parking suspends the session
        // the run parked from, so the worker that asked makes no further call
        // and the next one comes from the session the answer resumes.
        ScriptedTurn::text("the options now live in their own table"),
    ])
}

/// How many requests the script spends before the run parks: the write, the
/// checkpoint, the bash call and the `ask`. The next one is the resumed turn's,
/// because parking suspends the session that asked rather than letting it end
/// its turn on one more call.
const fn requests_before_the_answer() -> usize {
    4
}

/// Whether this request carries the answer as its own message.
///
/// An exact match on a string in the request's JSON, because the answer's own
/// word also appears inside the `ask` call's arguments — as an option id, and
/// as the recommendation — where it is part of a longer string rather than a
/// message the model was handed.
fn carries_the_answer(request: &serde_json::Value) -> bool {
    match request {
        serde_json::Value::String(text) => text == ANSWER,
        serde_json::Value::Array(values) => values.iter().any(carries_the_answer),
        serde_json::Value::Object(fields) => fields.values().any(carries_the_answer),
        _ => false,
    }
}

/// Epoch seconds, for the deadline the scripted checkpoint declares.
fn now() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock is past the epoch")
            .as_secs(),
    )
    .expect("epoch seconds fit")
}

/// The run's journal once every entry named has appeared, or once patience runs
/// out and the assertion can say what was missing.
///
/// Ingest drains on its own heartbeat, so a fact reported during the turn is
/// journaled shortly after rather than with it. Waiting is what keeps the
/// assertion about whether the health plane produced the entry rather than
/// about how fast it got there.
async fn await_journal_containing(
    client: &mut FeedClient<Channel>,
    run_id: i64,
    wanted: &[&str],
) -> Vec<String> {
    let deadline = Instant::now() + PATIENCE;

    loop {
        let journal = journal_of(client, run_id).await;
        let complete = wanted
            .iter()
            .all(|event| journal.iter().any(|entry| entry == event));

        if complete || Instant::now() >= deadline {
            return journal;
        }

        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// What the run recorded as evidence, which is what a checkpoint that reached
/// the control plane looks like from outside.
async fn findings_of(client: &mut FeedClient<Channel>, run_id: i64) -> Vec<String> {
    client
        .run_detail(proto::RunDetailRequest { run_id })
        .await
        .map(|view| {
            view.into_inner()
                .findings
                .into_iter()
                .map(|finding| finding.evidence_class)
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn the_daemon_executes_a_run_through_a_real_turn_that_checkpoints_asks_and_finishes() {
    // Comfortably ahead of the checkpoint, so the deadline the wheel derives is
    // a real one: a checkpoint promising a moment already past declares no
    // deadline at all.
    let promised_at = now() + 3600;

    let daemon = DaemonFixture::start(
        script(promised_at),
        CoordinatorSettings {
            // No grace at all, so the deadline is the moment the worker
            // promised and the wheel raises the exception on its next tick.
            // The default share of the promised span is measured in minutes,
            // which is not a thing a test can wait for.
            timers: TimerSettings {
                checkpoint_grace_percent: 0,
                ..TimerSettings::default()
            },
            ..daemon_settings()
        },
    );

    let socket = daemon.socket.clone();
    let stopper = daemon.stopper();
    let repo_root = daemon.repo_root();

    // The daemon takes its own runtime with it, so the client drives another.
    let client = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        let stopper = stopper;

        runtime.block_on(async move {
            let channel = connect(socket).await;
            let mut team = TeamClient::new(channel.clone());
            let mut feed = FeedClient::new(channel);

            let created = team
                .create_run(proto::CreateRunRequest {
                    repo_root,
                    task: "move the question options into their own table".to_owned(),
                    scope: "crates/agens-store".to_owned(),
                    dod: "the options are a table and the tests pass".to_owned(),
                    external_ref: Some("agens/AGN-181".to_owned()),
                    parent_run_id: None,
                    dep_run_id: None,
                    provider: "openai-api".to_owned(),
                    priority: 5,
                    budget_tokens: None,
                    start_point: String::new(),
                })
                .await
                .expect("a client may propose an execution")
                .into_inner();

            team.approve_plan(proto::ApprovePlanRequest {
                run_id: created.run_id,
            })
            .await
            .expect("the user may approve a proposed run");

            let parked = await_reported_state(&mut feed, created.run_id, "awaiting_input").await;
            let findings = findings_of(&mut feed, created.run_id).await;

            let inbox = feed
                .inbox(proto::InboxRequest {
                    repo_id: created.repo_id.clone(),
                })
                .await
                .expect("the inbox is readable")
                .into_inner();
            let question = inbox.items.first().map(|item| item.question_id);

            if let Some(question_id) = question {
                team.answer_question(proto::AnswerQuestionRequest {
                    question_id,
                    answer: ANSWER.to_owned(),
                })
                .await
                .expect("the user may answer a question");
            }

            let finished = await_reported_state(&mut feed, created.run_id, "done").await;
            let journal =
                await_journal_containing(&mut feed, created.run_id, &EXPECTED_HEALTH_EVENTS).await;

            drop(stopper);

            (created, parked, findings, question, finished, journal)
        })
    });

    let report = daemon.serve();

    let (created, parked, findings, question, finished, journal) =
        client.join().expect("the client thread finishes");

    // The answer has to reach the turn that resumed, not only the journal: a
    // run that comes back without what it parked for is a run whose question
    // decided nothing. It arrives as a message of its own, so the assertion is
    // an exact string in the request rather than a substring of one.
    let carrying: Vec<usize> = daemon
        .provider
        .requests()
        .iter()
        .enumerate()
        .filter(|(_, request)| carries_the_answer(&request.json()))
        .map(|(index, _)| index)
        .collect();

    assert_eq!(
        carrying,
        vec![requests_before_the_answer()],
        "the answer reached the resumed turn and no earlier one, journal: {journal:?}"
    );

    assert!(report.is_clean(), "every session ended: {report:?}");
    assert!(
        Path::new(&created.worktree_path).is_dir(),
        "creating a run provisions the worktree it works in: {created:?}"
    );
    assert_eq!(
        parked, "awaiting_input",
        "the worker's own `ask` parks the run on a person, journal: {journal:?}"
    );
    assert_eq!(
        findings,
        vec!["deterministic".to_owned()],
        "the worker's checkpoint reached the control plane as evidence, journal: {journal:?}"
    );
    assert!(
        question.is_some(),
        "the question the run parked on is in the inbox, journal: {journal:?}"
    );
    assert_eq!(
        finished, "done",
        "the answer requeues the run and the resumed session finishes it, journal: {journal:?}"
    );
    assert_eq!(
        journal
            .iter()
            .filter(|event| {
                *event != "run_state_changed"
                    && !HEALTH_EVENTS.contains(&event.as_str())
                    && !SCHEDULING_EVENTS.contains(&event.as_str())
            })
            .cloned()
            .collect::<Vec<_>>(),
        [
            "run_created",
            "run_approved",
            "run_started",
            "checkpoint",
            "run_awaiting_input",
            "question_answered",
            "run_resumed",
            "run_started",
            "run_finished",
        ],
        "the journal is the whole of what the run did"
    );

    for event in EXPECTED_HEALTH_EVENTS {
        assert!(
            journal.iter().any(|entry| entry == event),
            "the worker's turn produced `{event}` for ingest, journal: {journal:?}"
        );
    }

    let control_plane =
        ControlPlaneStore::open(&daemon.data_directory).expect("the control plane opens");
    let health = control_plane
        .load_run_health(created.run_id)
        .expect("run health is readable")
        .expect("the worker's facts derived a health row");

    assert_eq!(
        health.last_progress_turn,
        Some(1),
        "the turn's own work credited progress against the attempt it ran as: {health:?}"
    );

    let run = control_plane
        .load_run(created.run_id)
        .expect("the run is readable")
        .expect("the run is still there");

    assert_eq!(
        run.genesis_paths.as_deref(),
        Some(r#"["notes.md"]"#),
        "the first checkpoint froze the paths the evidence ledger recorded for this run"
    );

    daemon.provider.assert_script_consumed();

    let _ = std::fs::remove_dir_all(&daemon.root);
}

/// The model never speaks in a run that reaches its provider's cap.
///
/// One refusal per attempt the client's retry budget allows, each naming a
/// reset a second away, and then the text the resumed session answers with.
/// The turn never reaches a model, so nothing about the work decides what
/// happens to the run.
fn quota_script() -> Script {
    let refusals = agens_providers::RetryPolicy::default().max_attempts();

    Script::new(
        std::iter::repeat_with(|| ScriptedTurn::rate_limited(QUOTA_RESET_SECONDS))
            .take(refusals)
            .chain([ScriptedTurn::text("the provider served again")]),
    )
}

/// What the scripted provider names in `Retry-After`.
///
/// A second, so the wheel's resume is something a test can wait for. It is
/// also the run's whole reason to come back: nothing else here lifts the cap,
/// and the configured window that eventually lifts a cap naming no reset is
/// left at its default quarter of an hour.
const QUOTA_RESET_SECONDS: u32 = 1;

/// Reaching a subscription's cap is a wall with a time on it, and retrying
/// against it only spends the run's budget on refusals.
///
/// The whole park is asserted through the composed daemon: the run leaves
/// `running` and therefore its slot, its leg closes without being charged, the
/// provider is capped with the reset it named, and the timer wheel alone brings
/// the run back, with no person and no model involved in any of it.
#[test]
fn a_provider_that_refuses_for_quota_parks_the_run_and_the_wheel_brings_it_back() {
    let daemon = DaemonFixture::start(quota_script(), daemon_settings());

    let socket = daemon.socket.clone();
    let stopper = daemon.stopper();
    let repo_root = daemon.repo_root();

    let client = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        let stopper = stopper;

        runtime.block_on(async move {
            let channel = connect(socket).await;
            let mut team = TeamClient::new(channel.clone());
            let mut feed = FeedClient::new(channel);

            let created = team
                .create_run(proto::CreateRunRequest {
                    repo_root,
                    task: "move the question options into their own table".to_owned(),
                    scope: "crates/agens-store".to_owned(),
                    dod: "the options are a table and the tests pass".to_owned(),
                    external_ref: Some("agens/AGN-61".to_owned()),
                    parent_run_id: None,
                    dep_run_id: None,
                    provider: "openai-api".to_owned(),
                    priority: 5,
                    budget_tokens: None,
                    start_point: String::new(),
                })
                .await
                .expect("a client may propose an execution")
                .into_inner();

            team.approve_plan(proto::ApprovePlanRequest {
                run_id: created.run_id,
            })
            .await
            .expect("the user may approve a proposed run");

            let parked = await_reported_state(&mut feed, created.run_id, "awaiting_quota").await;
            let findings = findings_of(&mut feed, created.run_id).await;
            let inbox = feed
                .inbox(proto::InboxRequest {
                    repo_id: created.repo_id.clone(),
                })
                .await
                .expect("the inbox is readable")
                .into_inner();

            let resumed = await_reported_state(&mut feed, created.run_id, "done").await;
            let journal = journal_of(&mut feed, created.run_id).await;

            drop(stopper);

            (
                created,
                parked,
                findings,
                inbox.items.len(),
                resumed,
                journal,
            )
        })
    });

    let report = daemon.serve();

    let (created, parked, findings, waiting, resumed, journal) =
        client.join().expect("the client thread finishes");

    assert!(report.is_clean(), "every session ended: {report:?}");
    assert_eq!(
        parked, "awaiting_quota",
        "the provider's refusal parks the run instead of failing it, journal: {journal:?}"
    );
    assert_eq!(
        findings,
        vec!["insufficient".to_owned()],
        "the forced checkpoint says what stopped the run without claiming progress for it, \
         journal: {journal:?}"
    );
    assert_eq!(
        waiting, 0,
        "nothing is asked of a person: the reset is what brings the run back, journal: {journal:?}"
    );
    assert_eq!(
        resumed, "done",
        "the timer wheel requeued the run at the reset the provider named, journal: {journal:?}"
    );

    let control_plane =
        ControlPlaneStore::open(&daemon.data_directory).expect("the control plane opens");
    let attempts = control_plane
        .attempts_for_run(created.run_id)
        .expect("the attempts are readable");

    assert_eq!(
        attempts
            .iter()
            .map(|attempt| (attempt.n, attempt.outcome))
            .collect::<Vec<_>>(),
        vec![
            (1, Some(AttemptOutcome::Interrupted)),
            (2, Some(AttemptOutcome::Succeeded)),
        ],
        "the parked leg closed without being charged to the retry budget"
    );
    assert_eq!(
        control_plane
            .load_provider("openai-api")
            .expect("the provider row is readable")
            .expect("parking recorded the provider")
            .quota_state,
        QuotaState::Ok,
        "the run that came back cleared the cap as it went"
    );

    daemon.provider.assert_script_consumed();

    let _ = std::fs::remove_dir_all(&daemon.root);
}
