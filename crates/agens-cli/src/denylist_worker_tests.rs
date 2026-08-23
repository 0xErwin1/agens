//! The hard denylist as a run experiences it, through the composed daemon.
//!
//! The worker runs with `dangerously_allow_all`, so nothing in its permission
//! configuration refuses the call this test makes: the escalation has to come
//! from the denylist or from nowhere. What is asserted is the whole round trip
//! — the call is stopped, the run parks on a durable question naming the class,
//! a person answers it, and the resumed session finishes the run.

use std::path::PathBuf;

use agens_fixtures::{Script, ScriptedTurn};
use agens_server::grpc::proto::{self, feed_client::FeedClient, team_client::TeamClient};
use tonic::transport::Channel;

use crate::daemon_fixture::{
    DaemonFixture, await_reported_state, connect, daemon_settings, journal_of,
};

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
    let daemon = DaemonFixture::start(script(), daemon_settings());

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

    let report = daemon.serve();

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

    daemon.provider.assert_script_consumed();

    let worktree = PathBuf::from(&created.worktree_path);
    assert!(
        worktree.is_dir(),
        "the run kept its worktree: {created:?}, journal: {journal:?}"
    );

    let _ = std::fs::remove_dir_all(&daemon.root);
}
