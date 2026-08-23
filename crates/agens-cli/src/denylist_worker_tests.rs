//! The hard denylist as a run experiences it, through the composed daemon.
//!
//! The worker runs with `dangerously_allow_all`, so nothing in its permission
//! configuration refuses the call this test makes: the escalation has to come
//! from the denylist or from nowhere. What is asserted is the whole round trip
//! — the call is stopped, the run parks on a durable question naming the class,
//! a person answers it, and the resumed session finishes the run.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use agens_core::HeadlessTurnCancellation;
use agens_fixtures::{Script, ScriptedDialect, ScriptedProvider, ScriptedTurn};
use agens_server::grpc::proto::{self, feed_client::FeedClient, team_client::TeamClient};
use agens_server::{CoordinatorSettings, SchedulerLimits};
use tonic::transport::Channel;

use crate::CliDependencies;
use crate::deps::bootstrap;
use crate::worker::run_worker;
use crate::worker_tests::{Stopper, await_reported_state, checkout, connect, journal_of, scratch};

/// The answer a person gives the parked question.
const ANSWER: &str = "refuse";

/// The model's side of both sessions.
///
/// The first turn reaches for the one act a run never takes on its own
/// authority. There is no second turn in that session: the call is stopped, the
/// run parks, and the turn ends. The second session is the resumed one, which
/// reads the answer and finishes.
fn script() -> Script {
    Script::new([
        ScriptedTurn::tool_call("call-push", "bash", r#"{"command":"git push origin head"}"#),
        ScriptedTurn::text("the work is on the branch and was not published"),
    ])
}

/// What the run parked on, as the inbox reports it.
async fn parked_question(
    client: &mut FeedClient<Channel>,
    repo_id: String,
) -> Option<(i64, String)> {
    client
        .inbox(proto::InboxRequest { repo_id })
        .await
        .ok()?
        .into_inner()
        .items
        .into_iter()
        .next()
        .map(|item| (item.question_id, item.blocked_decision))
}

#[test]
fn a_denylisted_call_parks_the_run_on_a_durable_question_instead_of_running() {
    let root = scratch();
    let checkout = checkout(&root);
    let config_home = root.join("config");
    let data_directory = root.join("data");
    std::fs::create_dir_all(&config_home).expect("create the config directory");
    std::fs::create_dir_all(&data_directory).expect("create the data directory");

    let provider = ScriptedProvider::start(ScriptedDialect::Responses, script());
    let base_url = provider.base_url();

    let dependencies = CliDependencies::for_test(
        checkout.clone(),
        Some(root.join("home")),
        BTreeMap::from([
            (
                "AGENS_CONFIG_HOME".to_owned(),
                config_home.display().to_string(),
            ),
            ("OPENAI_API_KEY".to_owned(), "test-key".to_owned()),
        ]),
        BTreeMap::from([
            (
                config_home.join("config.toml"),
                format!(
                    "[provider]\nmodel = \"openai-api/gpt-4.1\"\nbase_url = \"{base_url}\"\n\n\
                     [options]\ndata_dir = \"{}\"\n",
                    data_directory.display()
                ),
            ),
            (
                config_home.join("auth.json"),
                r#"{"openai-api": {"api_key": "fixture"}}"#.to_owned(),
            ),
        ]),
    );
    let bootstrap = bootstrap(&dependencies).expect("the production bootstrap is valid");

    let shutdown = HeadlessTurnCancellation::new();
    let socket = agens_server::socket_path(&data_directory);
    let stopper = Stopper(shutdown.clone());
    let repo_root = checkout.display().to_string();

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
                    task: "finish the change on the branch".to_owned(),
                    scope: "crates/agens-store".to_owned(),
                    dod: "the change is committed".to_owned(),
                    external_ref: None,
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
            let question = parked_question(&mut feed, created.repo_id.clone()).await;

            if let Some((question_id, _)) = question.clone() {
                team.answer_question(proto::AnswerQuestionRequest {
                    question_id,
                    answer: ANSWER.to_owned(),
                })
                .await
                .expect("the user may answer a question");
            }

            let finished = await_reported_state(&mut feed, created.run_id, "done").await;
            let journal = journal_of(&mut feed, created.run_id).await;

            drop(stopper);

            (created, parked, question, finished, journal)
        })
    });

    let report = agens_server::serve_until_shutdown(
        &data_directory,
        &CoordinatorSettings {
            heartbeat: Duration::from_millis(25),
            scheduler: SchedulerLimits {
                max_concurrent: 1,
                available_worktrees: 1,
                provider_capacity: BTreeMap::new(),
                default_provider_capacity: 1,
            },
            ..CoordinatorSettings::default()
        },
        run_worker(&bootstrap),
        None,
        &shutdown,
    )
    .expect("the daemon serves");

    let (created, parked, question, finished, journal) =
        client.join().expect("the client thread finishes");

    assert!(report.is_clean(), "every session ended: {report:?}");
    assert_eq!(
        parked, "awaiting_input",
        "a denylisted call parks the run rather than running or failing it, journal: {journal:?}"
    );

    let (_, blocked_decision) = question.expect("the denylisted call opened a durable question");
    assert!(
        blocked_decision.contains("git_push"),
        "the question names the class that stopped the call: {blocked_decision}"
    );
    assert!(
        blocked_decision.contains("did not run"),
        "the question says the call was stopped rather than taken: {blocked_decision}"
    );
    assert_eq!(
        finished, "done",
        "the answer requeues the run and the resumed session finishes it, journal: {journal:?}"
    );
    assert!(
        !journal.iter().any(|event| event == "run_failed"),
        "parking is not a failed attempt, journal: {journal:?}"
    );

    provider.assert_script_consumed();

    let worktree = PathBuf::from(&created.worktree_path);
    assert!(
        worktree.is_dir(),
        "the run kept its worktree: {created:?}, journal: {journal:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}
