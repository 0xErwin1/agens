//! The fleet view against a live daemon that has nothing configured under
//! `[team]`.
//!
//! What this pins is where the view's knowledge comes from: the daemon knows
//! which projects exist because somebody opened a chat or created a run
//! against them, so `team ls` lists exactly that, with no pre-declared
//! project roots anywhere in the client's configuration.

use agens_fixtures::{Script, ScriptedTurn};
use agens_server::grpc::proto::{self, chat_client::ChatClient, team_client::TeamClient};

use crate::commands::team::{run_team_ls, run_team_show};
use crate::daemon_fixture::{DaemonFixture, connect, daemon_settings};

#[test]
fn showing_a_chat_without_completed_turns_reports_its_state_instead_of_a_bare_header() {
    let daemon = DaemonFixture::start(
        Script::new([ScriptedTurn::text("never consumed")]),
        daemon_settings(),
    );
    let socket = daemon.socket.clone();
    let checkout = daemon.checkout.display().to_string();
    let seed = daemon.dependency_seed.clone();
    let stopper = daemon.stopper();

    let client = std::thread::spawn(move || {
        let _stopper = stopper;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("the client runtime builds");

        let session_id = runtime.block_on(async {
            let channel = connect(socket).await;
            let mut chat = ChatClient::new(channel);

            chat.open(proto::OpenChatRequest {
                checkout,
                resume: None,
            })
            .await
            .expect("a chat opens")
            .into_inner()
            .session_id
        });

        drop(runtime);

        let output = run_team_show(&session_id.to_string(), false, &seed.dependencies())
            .expect("the chat detail renders");

        (session_id, output)
    });

    daemon.serve();

    let (session_id, output) = client.join().expect("the client thread finishes");

    assert!(
        output.starts_with(&format!("chat {session_id}\nno completed turns yet\n")),
        "a chat with nothing persisted says so:\n{output}"
    );
    assert!(
        output.contains("state: idle\n"),
        "the chat reports whether it is answering:\n{output}"
    );
}

#[test]
fn the_fleet_view_lists_what_the_daemon_hosts_without_configured_roots() {
    let daemon = DaemonFixture::start(
        Script::new([ScriptedTurn::text("never consumed")]),
        daemon_settings(),
    );
    let socket = daemon.socket.clone();
    let repo_root = daemon.repo_root();
    let checkout = daemon.checkout.display().to_string();
    let seed = daemon.dependency_seed.clone();
    let stopper = daemon.stopper();

    let client = std::thread::spawn(move || {
        let _stopper = stopper;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("the client runtime builds");

        let (session_id, run_id) = runtime.block_on(async {
            let channel = connect(socket).await;

            let mut team = TeamClient::new(channel.clone());

            // Paused before the run exists, so the run stays `queued` — a live
            // state the view must list — instead of racing the scheduler into
            // whatever the scripted provider would make of it.
            team.pause_admissions(proto::PauseAdmissionsRequest { paused: true })
                .await
                .expect("admissions pause");

            let created = team
                .create_run(proto::CreateRunRequest {
                    repo_root,
                    task: "prove the fleet view".to_owned(),
                    scope: "crates/agens-cli".to_owned(),
                    dod: "the view lists this run".to_owned(),
                    external_ref: None,
                    parent_run_id: None,
                    dep_run_id: None,
                    provider: "openai-api".to_owned(),
                    priority: 5,
                    budget_tokens: None,
                    start_point: String::new(),
                })
                .await
                .expect("a run is created")
                .into_inner();

            team.approve_plan(proto::ApprovePlanRequest {
                run_id: created.run_id,
            })
            .await
            .expect("the plan is approved");

            let mut chat = ChatClient::new(channel);
            let opened = chat
                .open(proto::OpenChatRequest {
                    checkout,
                    resume: None,
                })
                .await
                .expect("a chat opens")
                .into_inner();

            (opened.session_id, created.run_id)
        });

        // The command builds its own current-thread runtime, so the client's
        // is dropped first rather than nested under it.
        drop(runtime);

        let output = run_team_ls(false, &seed.dependencies()).expect("the fleet view renders");

        (session_id, run_id, output)
    });

    daemon.serve();

    let (session_id, run_id, output) = client.join().expect("the client thread finishes");

    assert!(
        output.contains(&format!("{run_id}\trun\tqueued")),
        "the run the daemon hosts is listed without configured roots:\n{output}"
    );
    assert!(
        output.contains(&format!("{session_id}\tchat\t")),
        "the chat the daemon hosts is listed without configured roots:\n{output}"
    );
}
