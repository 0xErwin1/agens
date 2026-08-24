//! The non-interactive fleet view over the daemon's existing control-plane views.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use agens_config::TeamSettings;
use agens_coordinator_client::{ClientError, Coordinator};
use agens_error::CliError;
use agens_store::{QuestionClass, QuestionStore, SessionStore};
use agens_tools::SessionWorktrees;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::CliDependencies;
use crate::deps::bootstrap;

const LIVE_RUN_STATES: &[&str] = &["queued", "running", "awaiting_input", "awaiting_quota"];

#[derive(Serialize)]
struct FleetView {
    daemon: &'static str,
    items: Vec<FleetItem>,
}

#[derive(Serialize)]
struct FleetItem {
    id: String,
    kind: &'static str,
    state: String,
    age_seconds: i64,
    last_event: Option<String>,
    worktree: Option<String>,
    waiting: Option<String>,
    #[serde(skip)]
    last_activity: i64,
}

pub(crate) fn run_team_ls(json: bool, dependencies: &CliDependencies) -> Result<String, CliError> {
    let bootstrap = bootstrap(dependencies)?;
    let socket = agens_server::socket_path(bootstrap.data_directory());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| {
            CliError::unavailable(format!("the fleet view is unavailable: {error}"))
        })?;

    runtime.block_on(async {
        let coordinator = match Coordinator::attach(&socket).await {
            Ok(coordinator) => coordinator,
            Err(ClientError::NotRunning(_)) => return Ok(no_daemon(json)),
            Err(error) => return Err(CliError::unavailable(error.to_string())),
        };

        let roots = TeamSettings::from(bootstrap.settings())
            .project_roots
            .into_iter()
            .collect::<BTreeSet<_>>();
        let sessions = SessionStore::open(bootstrap.data_directory())
            .and_then(|store| store.list_sessions())
            .map_err(|_| CliError::storage("the sessions database is unavailable"))?
            .into_iter()
            .map(|session| (session.id, session))
            .collect::<BTreeMap<_, _>>();
        let waiting_sessions = QuestionStore::open(bootstrap.data_directory())
            .and_then(|store| store.open_questions())
            .map_err(|_| CliError::storage("the questions database is unavailable"))?
            .into_iter()
            .filter_map(|question| {
                question
                    .session_id
                    .map(|session_id| (session_id, question.class))
            })
            .map(|(session_id, class)| (session_id, waiting_label(class).to_owned()))
            .collect::<BTreeMap<_, _>>();
        let worktrees = SessionWorktrees::new(bootstrap.data_directory());
        let now = now_seconds();
        let mut feed = coordinator.feed();
        let mut chat = coordinator.chat();
        let mut items = Vec::new();

        for root in roots {
            let repo_id = repository_id(&worktrees, &root)?;
            let tree = feed.tree(&repo_id).await.map_err(client_error)?;
            let inbox = feed
                .inbox(&repo_id)
                .await
                .map_err(client_error)?
                .items
                .into_iter()
                .map(|item| (item.run_id, item))
                .collect::<BTreeMap<_, _>>();

            for run in tree.runs.into_iter().filter(|run| is_live_run(&run.state)) {
                let detail = feed.run_detail(run.run_id).await.map_err(client_error)?;
                let Some(row) = detail.run else {
                    return Err(CliError::unavailable(
                        "the daemon returned a run without its row",
                    ));
                };
                let last_event = detail.events.last();
                let waiting = detail
                    .attempts
                    .iter()
                    .filter_map(|attempt| attempt.session_id)
                    .find_map(|session_id| waiting_sessions.get(&session_id).cloned())
                    .or_else(|| {
                        inbox.get(&run.run_id).map(|item| match item.kind.as_str() {
                            "approval" => "merge authorization".to_owned(),
                            _ => "question".to_owned(),
                        })
                    });

                items.push(FleetItem {
                    id: run.run_id.to_string(),
                    kind: "run",
                    state: run.state,
                    age_seconds: age_seconds(now, row.created_at),
                    last_event: last_event.map(|event| event.r#type.clone()),
                    worktree: row.worktree_path,
                    waiting,
                    last_activity: last_event.map_or(row.created_at, |event| event.ts),
                });
            }

            for open in chat.open_against(&root).await.map_err(client_error)? {
                let metadata = sessions.get(&open.session_id);
                let created_at = metadata.map_or(0, |session| session.created_at);
                let last_activity = metadata.map_or(created_at, |session| session.updated_at);
                let state = if open.answering { "running" } else { "idle" };

                items.push(FleetItem {
                    id: open.session_id.to_string(),
                    kind: "chat",
                    state: state.to_owned(),
                    age_seconds: age_seconds(now, created_at),
                    last_event: Some(state.to_owned()),
                    worktree: Some(open.checkout.display().to_string()),
                    waiting: waiting_sessions.get(&open.session_id).cloned(),
                    last_activity,
                });
            }
        }

        items.sort_by_key(|item| std::cmp::Reverse(item.last_activity));
        Ok(render(
            FleetView {
                daemon: "running",
                items,
            },
            json,
        ))
    })
}

fn no_daemon(json: bool) -> String {
    if json {
        render(
            FleetView {
                daemon: "not_running",
                items: Vec::new(),
            },
            true,
        )
    } else {
        "No daemon is running.\n".to_owned()
    }
}

fn render(view: FleetView, json: bool) -> String {
    if json {
        return format!(
            "{}\n",
            serde_json::to_string(&view).expect("fleet view contains only serializable values")
        );
    }

    if view.items.is_empty() {
        return "No active runs or chats.\n".to_owned();
    }

    let mut output = String::from("ID\tKIND\tSTATE\tAGE\tLAST EVENT\tWORKTREE\tWAITING\n");
    for item in view.items {
        output.push_str(&format!(
            "{}\t{}\t{}\t{}s\t{}\t{}\t{}\n",
            item.id,
            item.kind,
            item.state,
            item.age_seconds,
            item.last_event.as_deref().unwrap_or("-"),
            item.worktree.as_deref().unwrap_or("-"),
            item.waiting.as_deref().unwrap_or("-")
        ));
    }
    output
}

fn repository_id(worktrees: &SessionWorktrees, root: &Path) -> Result<String, CliError> {
    let identity = worktrees.repository_identity(root).map_err(|error| {
        CliError::configuration(format!("team project root is unavailable: {error}"))
    })?;
    let mut digest = Sha256::new();
    digest.update(identity.common_directory.display().to_string().as_bytes());
    if let Some(remote_url) = identity.remote_url {
        digest.update([0x1f]);
        digest.update(remote_url.as_bytes());
    }

    Ok(digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
        .chars()
        .take(16)
        .collect())
}

fn is_live_run(state: &str) -> bool {
    LIVE_RUN_STATES.contains(&state)
}

fn waiting_label(class: QuestionClass) -> &'static str {
    match class {
        QuestionClass::AskUser => "question",
        QuestionClass::Permission => "permission decision",
        QuestionClass::Consent => "consent",
    }
}

fn age_seconds(now: i64, created_at: i64) -> i64 {
    now.saturating_sub(created_at).max(0)
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(0))
}

fn client_error(error: ClientError) -> CliError {
    CliError::unavailable(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::{FleetItem, FleetView, render};

    #[test]
    fn fleet_rows_keep_machine_readable_waiting_and_activity_facts() {
        let output = render(
            FleetView {
                daemon: "running",
                items: vec![FleetItem {
                    id: "17".to_owned(),
                    kind: "run",
                    state: "awaiting_input".to_owned(),
                    age_seconds: 12,
                    last_event: Some("question_opened".to_owned()),
                    worktree: Some("/worktrees/17".to_owned()),
                    waiting: Some("question".to_owned()),
                    last_activity: 34,
                }],
            },
            true,
        );

        assert_eq!(
            output,
            "{\"daemon\":\"running\",\"items\":[{\"id\":\"17\",\"kind\":\"run\",\"state\":\"awaiting_input\",\"age_seconds\":12,\"last_event\":\"question_opened\",\"worktree\":\"/worktrees/17\",\"waiting\":\"question\"}]}\n"
        );
    }
}
