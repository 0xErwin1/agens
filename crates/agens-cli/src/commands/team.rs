//! The non-interactive fleet view over the daemon's existing control-plane views.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use agens_config::TeamSettings;
use agens_coordinator_client::{
    ChatClient, ClientError, Coordinator, FeedClient, HostedChatEvent, proto,
};
use agens_core::{Message, MessagePart, Role};
use agens_error::CliError;
use agens_store::{QuestionClass, QuestionStore, SessionStore};
use agens_tools::SessionWorktrees;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio_stream::StreamExt;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShowTarget {
    Run(i64),
    Chat(i64),
}

#[derive(Debug, PartialEq, Eq)]
enum TeamAction {
    Answer {
        question_id: i64,
        answer: String,
    },
    AskAnswer {
        session_id: i64,
        prompt_id: u64,
        answer: String,
    },
    Permission {
        session_id: i64,
        prompt_id: u64,
        answer: String,
    },
    Merge {
        question_id: i64,
    },
    Cancel {
        id: i64,
    },
}

pub(crate) fn run_team_action(
    arguments: &[String],
    dependencies: &CliDependencies,
) -> Result<String, CliError> {
    let action = parse_action(arguments)?;
    let bootstrap = bootstrap(dependencies)?;
    let socket = agens_server::socket_path(bootstrap.data_directory());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| {
            CliError::unavailable(format!("the fleet action is unavailable: {error}"))
        })?;

    runtime.block_on(async {
        let coordinator = match Coordinator::attach(&socket).await {
            Ok(coordinator) => coordinator,
            Err(ClientError::NotRunning(_)) => return Ok(no_daemon(false)),
            Err(error) => return Err(client_error(error)),
        };

        match action {
            TeamAction::Answer {
                question_id,
                answer,
            } => {
                let mut team = coordinator.team();
                let answered = team
                    .answer_question(question_id, &answer)
                    .await
                    .map_err(client_error)?;
                Ok(format!(
                    "Answered question {question_id} for run {}.\n",
                    answered.run_id
                ))
            }
            TeamAction::AskAnswer {
                session_id,
                prompt_id,
                answer,
            } => {
                coordinator
                    .chat()
                    .answer_question(session_id, prompt_id, &answer)
                    .await
                    .map_err(client_error)?;
                Ok(format!(
                    "Answered question {prompt_id} for chat {session_id}.\n"
                ))
            }
            TeamAction::Permission {
                session_id,
                prompt_id,
                answer,
            } => {
                coordinator
                    .chat()
                    .answer_question(session_id, prompt_id, &answer)
                    .await
                    .map_err(client_error)?;
                Ok(format!(
                    "Answered prompt {prompt_id} for chat {session_id}.\n"
                ))
            }
            TeamAction::Merge { question_id } => {
                let authorized = coordinator
                    .team()
                    .authorize_merge(proto::AuthorizeMergeRequest {
                        subject: Some(proto::authorize_merge_request::Subject::QuestionId(
                            question_id,
                        )),
                        answer: "merge".to_owned(),
                        expires_at: None,
                    })
                    .await
                    .map_err(client_error)?;
                Ok(format!(
                    "Authorized merge for run {} with question {}.\n",
                    authorized.run_id, authorized.question_id
                ))
            }
            TeamAction::Cancel { id } => {
                let roots = TeamSettings::from(bootstrap.settings())
                    .project_roots
                    .into_iter()
                    .collect::<BTreeSet<_>>();
                let worktrees = SessionWorktrees::new(bootstrap.data_directory());
                let mut feed = coordinator.feed();
                let mut chat = coordinator.chat();

                match locate_target(id, &roots, &worktrees, &mut feed, &mut chat).await? {
                    ShowTarget::Run(run_id) => {
                        coordinator
                            .team()
                            .cancel_run(run_id)
                            .await
                            .map_err(client_error)?;
                        Ok(format!("Cancelled run {run_id}.\n"))
                    }
                    ShowTarget::Chat(session_id) => {
                        chat.cancel(session_id).await.map_err(client_error)?;
                        Ok(format!("Cancelled chat {session_id}.\n"))
                    }
                }
            }
        }
    })
}

fn parse_action(arguments: &[String]) -> Result<TeamAction, CliError> {
    match arguments {
        [action, question_id, answer] if action == "answer" => Ok(TeamAction::Answer {
            question_id: positive_i64(question_id, "team answer requires a positive question id")?,
            answer: nonempty_answer(answer)?,
        }),
        [action, session_id, prompt_id, answer] if action == "answer" => {
            Ok(TeamAction::AskAnswer {
                session_id: positive_i64(session_id, "team answer requires a positive chat id")?,
                prompt_id: positive_u64(prompt_id, "team answer requires a positive prompt id")?,
                answer: nonempty_answer(answer)?,
            })
        }
        [action, session_id, prompt_id, answer] if action == "permission" => {
            Ok(TeamAction::Permission {
                session_id: positive_i64(
                    session_id,
                    "team permission requires a positive chat id",
                )?,
                prompt_id: positive_u64(
                    prompt_id,
                    "team permission requires a positive prompt id",
                )?,
                answer: nonempty_answer(answer)?,
            })
        }
        [action, run_id] if action == "merge" => Ok(TeamAction::Merge {
            question_id: positive_i64(
                run_id,
                "team merge requires a positive approval question id",
            )?,
        }),
        [action, id] if action == "cancel" => Ok(TeamAction::Cancel {
            id: positive_i64(id, "team cancel requires a positive run or chat id")?,
        }),
        [action, ..]
            if matches!(
                action.as_str(),
                "answer" | "permission" | "merge" | "cancel"
            ) =>
        {
            Err(CliError::usage(format!("invalid team {action} arguments")))
        }
        _ => Err(CliError::usage("unknown team action")),
    }
}

fn nonempty_answer(answer: &str) -> Result<String, CliError> {
    if answer.trim().is_empty() {
        return Err(CliError::usage("team answers cannot be empty"));
    }

    Ok(answer.to_owned())
}

fn positive_i64(value: &str, message: &'static str) -> Result<i64, CliError> {
    value
        .parse::<i64>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| CliError::usage(message))
}

fn positive_u64(value: &str, message: &'static str) -> Result<u64, CliError> {
    value
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| CliError::usage(message))
}

pub(crate) fn run_team_show(
    id: &str,
    follow: bool,
    dependencies: &CliDependencies,
) -> Result<String, CliError> {
    let bootstrap = bootstrap(dependencies)?;
    let socket = agens_server::socket_path(bootstrap.data_directory());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| {
            CliError::unavailable(format!("the fleet detail is unavailable: {error}"))
        })?;

    runtime.block_on(async {
        let coordinator = match Coordinator::attach(&socket).await {
            Ok(coordinator) => coordinator,
            Err(ClientError::NotRunning(_)) => return Ok(no_daemon(false)),
            Err(error) => return Err(client_error(error)),
        };
        let roots = TeamSettings::from(bootstrap.settings())
            .project_roots
            .into_iter()
            .collect::<BTreeSet<_>>();
        let worktrees = SessionWorktrees::new(bootstrap.data_directory());
        let numeric_id = positive_i64(id, "team show requires a positive numeric id")?;
        let mut feed = coordinator.feed();
        let mut chat = coordinator.chat();
        let target = locate_target(numeric_id, &roots, &worktrees, &mut feed, &mut chat).await?;

        match target {
            ShowTarget::Run(run_id) => {
                let detail = feed.run_detail(run_id).await.map_err(client_error)?;
                let output = render_run_detail(&detail)?;
                if !follow {
                    return Ok(output);
                }

                write_follow(&output)?;
                let mut events = feed.subscribe_to_run(run_id).await.map_err(client_error)?;
                while let Some(event) = events.next().await {
                    write_follow(&render_run_event(&event.map_err(client_error)?))?;
                }
            }
            ShowTarget::Chat(session_id) => {
                let history = chat.history(session_id).await.map_err(client_error)?;
                let output = render_chat_history(session_id, &history);
                if !follow {
                    return Ok(output);
                }

                write_follow(&output)?;
                let mut events = chat.subscribe(session_id).await.map_err(client_error)?;
                while let Some(event) = events.next().await {
                    let event = event.map_err(client_error)?;
                    write_follow(&render_chat_event(&event))?;
                    if event == HostedChatEvent::Closed {
                        break;
                    }
                }
            }
        }

        Ok(String::new())
    })
}

async fn locate_target(
    id: i64,
    roots: &BTreeSet<std::path::PathBuf>,
    worktrees: &SessionWorktrees,
    feed: &mut FeedClient,
    chat: &mut ChatClient,
) -> Result<ShowTarget, CliError> {
    let mut run_found = false;
    let mut chat_found = false;

    for root in roots {
        let repo_id = repository_id(worktrees, root)?;
        run_found |= feed
            .tree(&repo_id)
            .await
            .map_err(client_error)?
            .runs
            .iter()
            .any(|run| run.run_id == id);
        chat_found |= chat
            .open_against(root)
            .await
            .map_err(client_error)?
            .iter()
            .any(|open| open.session_id == id);
    }

    resolve_show_target(&id.to_string(), run_found, chat_found)
}

fn resolve_show_target(
    id: &str,
    run_found: bool,
    chat_found: bool,
) -> Result<ShowTarget, CliError> {
    let id = id
        .parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| CliError::usage("team show requires a positive numeric id"))?;

    match (run_found, chat_found) {
        (true, false) => Ok(ShowTarget::Run(id)),
        (false, true) => Ok(ShowTarget::Chat(id)),
        (false, false) => Err(CliError::usage(format!("no run or chat by id {id}"))),
        (true, true) => Err(CliError::usage(format!(
            "id {id} is ambiguous between a run and a chat"
        ))),
    }
}

fn render_run_detail(detail: &proto::RunView) -> Result<String, CliError> {
    let run = detail
        .run
        .as_ref()
        .ok_or_else(|| CliError::unavailable("the daemon returned a run without its row"))?;
    let mut output = format!(
        "run {}\nstate: {}\ntask: {}\nscope: {}\ndefinition of done: {}\nworktree: {}\n",
        run.run_id,
        run.state,
        run.task,
        run.scope,
        run.dod,
        run.worktree_path.as_deref().unwrap_or("-")
    );

    for attempt in &detail.attempts {
        output.push_str(&format!(
            "attempt {}: {} tokens={}\n",
            attempt.n,
            attempt.outcome.as_deref().unwrap_or("running"),
            attempt
                .tokens
                .map_or_else(|| "-".to_owned(), |tokens| tokens.to_string())
        ));
    }
    for question in &detail.questions {
        output.push_str(&format!(
            "question {} [{}]: {} options={}\n",
            question.question_id, question.state, question.blocked_decision, question.options
        ));
    }
    for finding in &detail.findings {
        output.push_str(&format!(
            "finding [{}]: {} evidence={}\n",
            finding.evidence_class, finding.description, finding.proof_refs
        ));
    }
    for event in &detail.events {
        output.push_str(&render_run_event(event));
    }

    Ok(output)
}

fn render_run_event(event: &proto::Event) -> String {
    format!("event {} {}: {}\n", event.ts, event.r#type, event.payload)
}

fn render_chat_history(session_id: i64, history: &[Message]) -> String {
    let mut output = format!("chat {session_id}\n");
    for message in history {
        for part in &message.parts {
            output.push_str(&render_message_part(message.role, part));
        }
    }
    output
}

fn render_message_part(role: Role, part: &MessagePart) -> String {
    match part {
        MessagePart::Text(text) => format!("{}: {text}\n", role_name(role)),
        MessagePart::Reasoning(text) => format!("reasoning: {text}\n"),
        MessagePart::ToolCall { name, input, .. } => format!("tool call {name}: {input}\n"),
        MessagePart::ToolResult { content, .. } => format!("tool result: {content}\n"),
        MessagePart::Media { media_id, mime } => format!("media {media_id}: {mime}\n"),
    }
}

const fn role_name(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
        Role::Supervisor => "supervisor",
    }
}

fn render_chat_event(event: &HostedChatEvent) -> String {
    match event {
        HostedChatEvent::Progress(progress) => format!("progress: {progress:?}\n"),
        HostedChatEvent::PermissionAsked(question) => format!(
            "permission {}: tool={} target={} access={} reason={}\n",
            question.prompt_id, question.tool, question.target, question.access, question.reason
        ),
        HostedChatEvent::AskUserAsked { prompt_id, request } => {
            let prompts: Vec<String> = request
                .questions()
                .iter()
                .map(|question| {
                    let options: Vec<&str> = question
                        .options()
                        .iter()
                        .map(|option| option.id())
                        .collect();

                    format!("{} [{}]", question.prompt(), options.join("|"))
                })
                .collect();
            format!("question {}: {}\n", prompt_id, prompts.join(" | "))
        }
        HostedChatEvent::TurnCompleted { text } => format!("assistant: {text}\n"),
        HostedChatEvent::TurnFailed { detail } => format!("failed: {detail}\n"),
        HostedChatEvent::Closed => "closed\n".to_owned(),
    }
}

fn write_follow(output: &str) -> Result<(), CliError> {
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(output.as_bytes())
        .and_then(|()| stdout.flush())
        .map_err(|error| CliError::unavailable(format!("standard output is unavailable: {error}")))
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
    use agens_core::{Message, MessagePart, Role};

    use agens_core::ask_user::{AskUserMode, AskUserOption, AskUserQuestion, AskUserRequest};
    use agens_coordinator_client::HostedChatEvent;

    use super::{
        FleetItem, FleetView, ShowTarget, TeamAction, parse_action, render, render_chat_event,
        render_chat_history, resolve_show_target,
    };

    #[test]
    fn action_parser_preserves_answer_domains_and_rejects_empty_answers() {
        let arguments = ["permission", "17", "9", "allow_once"]
            .map(str::to_owned)
            .to_vec();

        assert_eq!(
            parse_action(&arguments).unwrap(),
            TeamAction::Permission {
                session_id: 17,
                prompt_id: 9,
                answer: "allow_once".to_owned(),
            }
        );
        assert!(
            parse_action(&["answer", "23", " "].map(str::to_owned))
                .unwrap_err()
                .to_string()
                .contains("cannot be empty")
        );
    }

    #[test]
    fn a_three_argument_answer_names_a_chat_question_rather_than_a_run() {
        assert_eq!(
            parse_action(&["answer", "23", "approve"].map(str::to_owned)).unwrap(),
            TeamAction::Answer {
                question_id: 23,
                answer: "approve".to_owned(),
            }
        );
        assert_eq!(
            parse_action(&["answer", "17", "9", "approve"].map(str::to_owned)).unwrap(),
            TeamAction::AskAnswer {
                session_id: 17,
                prompt_id: 9,
                answer: "approve".to_owned(),
            }
        );
        assert!(
            parse_action(&["answer", "17", "9", " "].map(str::to_owned))
                .unwrap_err()
                .to_string()
                .contains("cannot be empty")
        );
    }

    #[test]
    fn a_followed_question_shows_the_option_ids_an_answer_must_come_from() {
        let question = AskUserQuestion::new(
            "approval",
            "Choose an outcome",
            None,
            AskUserMode::Single,
            vec![
                AskUserOption::new("approve", "Approve", None, None),
                AskUserOption::new("decline", "Decline", None, None),
            ],
            false,
            false,
            false,
        );
        let request =
            AskUserRequest::new(None, vec![question]).expect("the question is valid");

        assert_eq!(
            render_chat_event(&HostedChatEvent::AskUserAsked {
                prompt_id: 3,
                request,
            }),
            "question 3: Choose an outcome [approve|decline]\n"
        );
    }

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

    #[test]
    fn show_target_rejects_invalid_missing_and_ambiguous_identifiers() {
        assert_eq!(
            resolve_show_target("abc", false, false)
                .unwrap_err()
                .to_string(),
            "usage: team show requires a positive numeric id"
        );
        assert_eq!(
            resolve_show_target("17", false, false)
                .unwrap_err()
                .to_string(),
            "usage: no run or chat by id 17"
        );
        assert_eq!(
            resolve_show_target("17", true, true)
                .unwrap_err()
                .to_string(),
            "usage: id 17 is ambiguous between a run and a chat"
        );
        assert_eq!(
            resolve_show_target("17", true, false).unwrap(),
            ShowTarget::Run(17)
        );
        assert_eq!(
            resolve_show_target("17", false, true).unwrap(),
            ShowTarget::Chat(17)
        );
    }

    #[test]
    fn chat_history_is_plain_text_for_non_tty_consumers() {
        let output = render_chat_history(
            17,
            &[Message {
                role: Role::User,
                parts: vec![
                    MessagePart::Text("inspect this".to_owned()),
                    MessagePart::ToolResult {
                        tool_call_id: "call-1".to_owned(),
                        content: "done".to_owned(),
                        is_error: false,
                    },
                ],
            }],
        );

        assert_eq!(output, "chat 17\nuser: inspect this\ntool result: done\n");
    }
}
