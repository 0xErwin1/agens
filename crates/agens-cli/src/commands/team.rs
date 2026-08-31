//! The non-interactive fleet view over the daemon's existing control-plane views.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use agens_coordinator_client::{
    ChatClient, ClientError, Coordinator, FeedClient, HostedChatEvent, proto,
};
use agens_core::ask_user::AskUserRequest;
use agens_core::{IntraTurnInputSource, Message, MessagePart, Role, TurnEvent};
use agens_error::CliError;
use agens_store::{OpenQuestionStatus, QuestionClass, QuestionStore, SessionStore};
use serde::Serialize;
use tokio_stream::{Stream, StreamExt};

use crate::CliDependencies;
use crate::deps::bootstrap;

const LIVE_RUN_STATES: &[&str] = &["queued", "running", "awaiting_input", "awaiting_quota"];

/// Every fleet form `agens team` accepts, for the usage error a mistyped one
/// gets instead of being opened as a chat prompt.
pub(crate) const FLEET_USAGE: &str = "team fleet operations are `ls [--json]`, \
    `show <id> [--follow]`, `answer <question-id> <answer>`, \
    `answer <chat-id> <prompt-id> <option-id>`, \
    `permission <chat-id> <prompt-id> <answer>`, `merge <approval-question-id>`, \
    and `cancel <id>`";

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
        let now = now_seconds();
        let mut feed = coordinator.feed();
        let mut chat = coordinator.chat();
        let mut items = Vec::new();

        // The daemon names the projects, never the configuration: a repository
        // exists on this board because somebody created a run against it, and
        // a chat because somebody opened one, wherever either happened.
        for repo_id in feed.repos().await.map_err(client_error)? {
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
        }

        for open in chat.open_everywhere().await.map_err(client_error)? {
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
                let mut feed = coordinator.feed();
                let mut chat = coordinator.chat();

                match locate_target(id, &mut feed, &mut chat).await? {
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
        let numeric_id = positive_i64(id, "team show requires a positive numeric id")?;
        let mut feed = coordinator.feed();
        let mut chat = coordinator.chat();
        let target = locate_target(numeric_id, &mut feed, &mut chat).await?;

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
                let answering = chat
                    .open_everywhere()
                    .await
                    .map_err(client_error)?
                    .into_iter()
                    .find(|open| open.session_id == session_id)
                    .is_some_and(|open| open.answering);
                let questions = open_chat_questions(bootstrap.data_directory(), session_id)?;
                let mut events = chat.subscribe(session_id).await.map_err(client_error)?;

                // A question the turn is stopped on greets every subscriber,
                // which is the only client-visible carrier of its prompt text.
                // A one-shot show drains that greeting; a follow leaves it on
                // the stream it is about to render anyway.
                let greeted = if follow {
                    Vec::new()
                } else {
                    drain_greeting(&mut events).await
                };

                let output =
                    render_chat_detail(session_id, &history, answering, &questions, &greeted);
                if !follow {
                    return Ok(output);
                }

                write_follow(&output)?;
                let mut renderer = ChatFollowRenderer::new(session_id);
                while let Some(event) = events.next().await {
                    let event = event.map_err(client_error)?;
                    write_follow(&renderer.render(&event))?;
                    if event == HostedChatEvent::Closed {
                        break;
                    }
                }
            }
        }

        Ok(String::new())
    })
}

/// The durable open questions addressed to one chat session.
fn open_chat_questions(
    data_directory: &std::path::Path,
    session_id: i64,
) -> Result<Vec<OpenQuestionStatus>, CliError> {
    QuestionStore::open(data_directory)
        .and_then(|store| store.open_questions())
        .map_err(|_| CliError::storage("the questions database is unavailable"))
        .map(|questions| {
            questions
                .into_iter()
                .filter(|question| question.session_id == Some(session_id))
                .collect()
        })
}

/// Collects the ask-user questions the subscription greets a new subscriber
/// with, giving up at the first live event or after a short quiet window.
async fn drain_greeting<S>(events: &mut S) -> Vec<(u64, AskUserRequest)>
where
    S: Stream<Item = Result<HostedChatEvent, ClientError>> + Unpin,
{
    const GREETING_WINDOW: std::time::Duration = std::time::Duration::from_millis(300);

    let deadline = tokio::time::Instant::now() + GREETING_WINDOW;
    let mut greeted = Vec::new();

    while let Ok(Some(Ok(HostedChatEvent::AskUserAsked { prompt_id, request }))) =
        tokio::time::timeout_at(deadline, events.next()).await
    {
        greeted.push((prompt_id, request));
    }

    greeted
}

async fn locate_target(
    id: i64,
    feed: &mut FeedClient,
    chat: &mut ChatClient,
) -> Result<ShowTarget, CliError> {
    let mut run_found = false;

    for repo_id in feed.repos().await.map_err(client_error)? {
        run_found |= feed
            .tree(&repo_id)
            .await
            .map_err(client_error)?
            .runs
            .iter()
            .any(|run| run.run_id == id);
    }

    let chat_found = chat
        .open_everywhere()
        .await
        .map_err(client_error)?
        .iter()
        .any(|open| open.session_id == id);

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

        if question.state == "open" {
            output.push_str(&format!(
                "answer with: team answer {} <answer>\n",
                question.question_id
            ));
        }
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

/// The full detail one `team show <chat-id>` prints: the persisted transcript,
/// what the chat is doing right now, and any question it is stopped on with
/// the exact command that answers it.
fn render_chat_detail(
    session_id: i64,
    history: &[Message],
    answering: bool,
    questions: &[OpenQuestionStatus],
    greeted: &[(u64, AskUserRequest)],
) -> String {
    let mut output = format!("chat {session_id}\n");

    if history.is_empty() {
        output.push_str("no completed turns yet\n");
    } else {
        for message in history {
            for part in &message.parts {
                output.push_str(&render_message_part(message.role, part));
            }
        }
    }

    output.push_str(if answering {
        "state: answering\n"
    } else {
        "state: idle\n"
    });

    for (prompt_id, request) in greeted {
        output.push_str(&render_ask_user_question(*prompt_id, request));
        output.push_str(&ask_answer_hint(session_id, &prompt_id.to_string()));
    }

    // The durable rows carry no prompt text by design, so a question the
    // daemon already greeted us with renders once, from the greeting.
    for question in questions {
        let greeted_already = greeted
            .iter()
            .any(|(prompt_id, _)| prompt_id.to_string() == question.question_id);
        if greeted_already {
            continue;
        }

        output.push_str(&format!(
            "question {} [{}] from {}: options={}\n",
            question.question_id,
            waiting_label(question.class),
            question.origin,
            question.admissible_answers.join("|")
        ));

        match question.class {
            QuestionClass::AskUser => {
                output.push_str(&ask_answer_hint(session_id, &question.question_id));
            }
            QuestionClass::Permission => {
                output.push_str(&permission_answer_hint(session_id, &question.question_id));
            }
            QuestionClass::Consent => {}
        }
    }

    output
}

fn ask_answer_hint(session_id: i64, prompt_id: &str) -> String {
    format!("answer with: team answer {session_id} {prompt_id} <option-id>\n")
}

fn permission_answer_hint(session_id: i64, prompt_id: &str) -> String {
    format!("answer with: team permission {session_id} {prompt_id} <answer>\n")
}

fn render_ask_user_question(prompt_id: u64, request: &AskUserRequest) -> String {
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

/// Renders one chat's live event stream as prose, one line per fact.
///
/// Assistant text arrives as deltas, so the renderer buffers them and flushes
/// whole lines: on a newline in the stream, and on any event that interrupts
/// the text. A turn whose text was already streamed closes with a marker
/// instead of repeating the full answer.
struct ChatFollowRenderer {
    session_id: i64,
    text: String,
    streamed: bool,
}

impl ChatFollowRenderer {
    const fn new(session_id: i64) -> Self {
        Self {
            session_id,
            text: String::new(),
            streamed: false,
        }
    }

    fn render(&mut self, event: &HostedChatEvent) -> String {
        match event {
            HostedChatEvent::Progress(progress) => self.render_progress(progress),
            HostedChatEvent::PermissionAsked(question) => {
                let mut output = self.flush_text();
                output.push_str(&format!(
                    "permission {}: tool={} target={} access={} reason={}\n",
                    question.prompt_id,
                    question.tool,
                    question.target,
                    question.access,
                    question.reason
                ));
                output.push_str(&permission_answer_hint(
                    self.session_id,
                    &question.prompt_id.to_string(),
                ));
                output
            }
            HostedChatEvent::AskUserAsked { prompt_id, request } => {
                let mut output = self.flush_text();
                output.push_str(&render_ask_user_question(*prompt_id, request));
                output.push_str(&ask_answer_hint(self.session_id, &prompt_id.to_string()));
                output
            }
            HostedChatEvent::TurnCompleted { text } => {
                let mut output = self.flush_text();
                if self.streamed {
                    output.push_str("turn completed\n");
                } else {
                    output.push_str(&format!("assistant: {text}\n"));
                }
                self.streamed = false;
                output
            }
            HostedChatEvent::TurnFailed { detail } => {
                let mut output = self.flush_text();
                output.push_str(&format!("failed: {detail}\n"));
                self.streamed = false;
                output
            }
            HostedChatEvent::Closed => {
                let mut output = self.flush_text();
                output.push_str("closed\n");
                output
            }
        }
    }

    fn render_progress(&mut self, progress: &TurnEvent) -> String {
        match progress {
            TurnEvent::ProviderPart(MessagePart::Text(delta)) => {
                self.streamed = true;
                self.text.push_str(delta);
                self.flush_complete_lines()
            }
            TurnEvent::ToolCallRequested { name, input, .. } => {
                let mut output = self.flush_text();
                output.push_str(&format!("tool {name}: {}\n", one_line(input)));
                output
            }
            TurnEvent::ToolResult(MessagePart::ToolResult {
                content, is_error, ..
            }) => {
                let mut output = self.flush_text();
                let marker = if *is_error { " (error)" } else { "" };
                output.push_str(&format!("tool result{marker}: {}\n", one_line(content)));
                output
            }
            TurnEvent::ProviderRetry {
                attempt,
                max_attempts,
                ..
            } => {
                let mut output = self.flush_text();
                match max_attempts {
                    Some(max) => output.push_str(&format!("retrying (attempt {attempt}/{max})\n")),
                    None => output.push_str(&format!("retrying (attempt {attempt})\n")),
                }
                output
            }
            TurnEvent::IntraTurnInput { source, text } => {
                let mut output = self.flush_text();
                let speaker = match source {
                    IntraTurnInputSource::Human => "user",
                    IntraTurnInputSource::Supervisor => "supervisor",
                };
                output.push_str(&format!("{speaker} (steered): {text}\n"));
                output
            }
            TurnEvent::ProviderPart(_)
            | TurnEvent::StateChanged(_)
            | TurnEvent::Usage(_)
            | TurnEvent::ToolResult(_)
            | TurnEvent::ToolResultFacts { .. } => String::new(),
        }
    }

    /// Emits every whole line buffered so far, keeping the partial tail.
    fn flush_complete_lines(&mut self) -> String {
        let Some(boundary) = self.text.rfind('\n') else {
            return String::new();
        };

        let tail = self.text.split_off(boundary + 1);
        let complete = std::mem::replace(&mut self.text, tail);

        complete
            .lines()
            .map(|line| format!("assistant> {line}\n"))
            .collect()
    }

    /// Emits whatever text is buffered, whole lines or not, because another
    /// event is about to interrupt it.
    fn flush_text(&mut self) -> String {
        let mut output = self.flush_complete_lines();

        if !self.text.is_empty() {
            output.push_str(&format!("assistant> {}\n", self.text));
            self.text.clear();
        }

        output
    }
}

/// Collapses a payload to one bounded line, keeping raw JSON and long output
/// off the follow stream.
fn one_line(payload: &str) -> String {
    let collapsed: String = payload
        .chars()
        .map(|character| {
            if character.is_whitespace() {
                ' '
            } else {
                character
            }
        })
        .collect();
    let mut line: String = collapsed.chars().take(120).collect();

    if collapsed.chars().count() > 120 {
        line.push_str("...");
    }

    line
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
    use agens_core::{Message, MessagePart, Role, TurnEvent, TurnState, Usage};

    use agens_coordinator_client::{HostedChatEvent, PermissionQuestion, proto};
    use agens_core::IntraTurnInputSource;
    use agens_core::ask_user::{AskUserMode, AskUserOption, AskUserQuestion, AskUserRequest};
    use agens_store::{OpenQuestionStatus, QuestionClass};

    use super::{
        ChatFollowRenderer, FleetItem, FleetView, ShowTarget, TeamAction, parse_action, render,
        render_chat_detail, render_run_detail, resolve_show_target,
    };

    fn approval_request() -> AskUserRequest {
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

        AskUserRequest::new(None, vec![question]).expect("the question is valid")
    }

    #[test]
    fn a_chat_with_nothing_persisted_says_so_instead_of_printing_a_bare_header() {
        assert_eq!(
            render_chat_detail(17, &[], false, &[], &[]),
            "chat 17\nno completed turns yet\nstate: idle\n"
        );
    }

    #[test]
    fn a_chat_mid_answer_reports_that_it_is_answering_right_now() {
        let history = [Message {
            role: Role::User,
            parts: vec![MessagePart::Text("inspect this".to_owned())],
        }];

        assert_eq!(
            render_chat_detail(17, &history, true, &[], &[]),
            "chat 17\nuser: inspect this\nstate: answering\n"
        );
    }

    #[test]
    fn a_parked_chat_shows_its_open_question_and_the_exact_answer_command() {
        let duplicate_store_row = OpenQuestionStatus {
            session_id: Some(17),
            child: None,
            question_id: "9".to_owned(),
            class: QuestionClass::AskUser,
            origin: "ask_user".to_owned(),
            admissible_answers: vec!["approve".to_owned(), "decline".to_owned()],
        };

        assert_eq!(
            render_chat_detail(
                17,
                &[],
                true,
                &[duplicate_store_row],
                &[(9, approval_request())],
            ),
            "chat 17\nno completed turns yet\nstate: answering\n\
             question 9: Choose an outcome [approve|decline]\n\
             answer with: team answer 17 9 <option-id>\n"
        );
    }

    #[test]
    fn durable_questions_render_their_domain_and_matching_answer_command() {
        let questions = [
            OpenQuestionStatus {
                session_id: Some(17),
                child: None,
                question_id: "9".to_owned(),
                class: QuestionClass::AskUser,
                origin: "ask_user".to_owned(),
                admissible_answers: vec!["approve".to_owned(), "decline".to_owned()],
            },
            OpenQuestionStatus {
                session_id: Some(17),
                child: None,
                question_id: "12".to_owned(),
                class: QuestionClass::Permission,
                origin: "shell".to_owned(),
                admissible_answers: vec!["allow_once".to_owned(), "deny_once".to_owned()],
            },
            OpenQuestionStatus {
                session_id: Some(17),
                child: None,
                question_id: "14".to_owned(),
                class: QuestionClass::Consent,
                origin: "review".to_owned(),
                admissible_answers: vec!["granted".to_owned(), "declined".to_owned()],
            },
        ];

        assert_eq!(
            render_chat_detail(17, &[], false, &questions, &[]),
            "chat 17\nno completed turns yet\nstate: idle\n\
             question 9 [question] from ask_user: options=approve|decline\n\
             answer with: team answer 17 9 <option-id>\n\
             question 12 [permission decision] from shell: options=allow_once|deny_once\n\
             answer with: team permission 17 12 <answer>\n\
             question 14 [consent] from review: options=granted|declined\n"
        );
    }

    #[test]
    fn followed_text_deltas_coalesce_into_lines_instead_of_debug_dumps() {
        let mut renderer = ChatFollowRenderer::new(17);

        assert_eq!(
            renderer.render(&HostedChatEvent::Progress(TurnEvent::ProviderPart(
                MessagePart::Text("Hel".to_owned())
            ))),
            ""
        );
        assert_eq!(
            renderer.render(&HostedChatEvent::Progress(TurnEvent::ProviderPart(
                MessagePart::Text("lo\nwor".to_owned())
            ))),
            "assistant> Hello\n"
        );
        assert_eq!(
            renderer.render(&HostedChatEvent::Progress(TurnEvent::ToolCallRequested {
                id: "call-1".to_owned(),
                name: "read_file".to_owned(),
                input: "{\"path\":\"src/main.rs\"}".to_owned(),
            })),
            "assistant> wor\ntool read_file: {\"path\":\"src/main.rs\"}\n"
        );
        assert_eq!(
            renderer.render(&HostedChatEvent::TurnCompleted {
                text: "Hello world".to_owned(),
            }),
            "turn completed\n"
        );
        assert_eq!(
            renderer.render(&HostedChatEvent::TurnCompleted {
                text: "A quiet turn".to_owned(),
            }),
            "assistant: A quiet turn\n"
        );
    }

    #[test]
    fn followed_tool_results_are_one_truncated_line_with_failures_marked() {
        let mut renderer = ChatFollowRenderer::new(17);

        assert_eq!(
            renderer.render(&HostedChatEvent::Progress(TurnEvent::ToolResult(
                MessagePart::ToolResult {
                    tool_call_id: "call-1".to_owned(),
                    content: "line one\nline two".to_owned(),
                    is_error: false,
                }
            ))),
            "tool result: line one line two\n"
        );
        assert_eq!(
            renderer.render(&HostedChatEvent::Progress(TurnEvent::ToolResult(
                MessagePart::ToolResult {
                    tool_call_id: "call-2".to_owned(),
                    content: "boom".to_owned(),
                    is_error: true,
                }
            ))),
            "tool result (error): boom\n"
        );

        let long_input = "a".repeat(200);
        let output = renderer.render(&HostedChatEvent::Progress(TurnEvent::ToolCallRequested {
            id: "call-3".to_owned(),
            name: "bash".to_owned(),
            input: long_input,
        }));
        assert_eq!(output, format!("tool bash: {}...\n", "a".repeat(120)));
    }

    #[test]
    fn followed_bookkeeping_events_stay_silent() {
        let mut renderer = ChatFollowRenderer::new(17);

        assert_eq!(
            renderer.render(&HostedChatEvent::Progress(TurnEvent::StateChanged(
                TurnState::Requesting
            ))),
            ""
        );
        assert_eq!(
            renderer.render(&HostedChatEvent::Progress(TurnEvent::Usage(Usage {
                input_tokens: Some(1),
                output_tokens: Some(2),
                total_tokens: Some(3),
                context_window: None,
            }))),
            ""
        );
        assert_eq!(
            renderer.render(&HostedChatEvent::Progress(TurnEvent::ProviderPart(
                MessagePart::Reasoning("thinking".to_owned())
            ))),
            ""
        );
    }

    #[test]
    fn followed_retries_and_steered_input_read_as_prose() {
        let mut renderer = ChatFollowRenderer::new(17);

        assert_eq!(
            renderer.render(&HostedChatEvent::Progress(TurnEvent::ProviderRetry {
                attempt: 2,
                max_attempts: Some(5),
                delay: None,
                reason: agens_core::TurnRetryReason::RateLimited,
            })),
            "retrying (attempt 2/5)\n"
        );
        assert_eq!(
            renderer.render(&HostedChatEvent::Progress(TurnEvent::ProviderRetry {
                attempt: 3,
                max_attempts: None,
                delay: None,
                reason: agens_core::TurnRetryReason::Network,
            })),
            "retrying (attempt 3)\n"
        );
        assert_eq!(
            renderer.render(&HostedChatEvent::Progress(TurnEvent::IntraTurnInput {
                source: IntraTurnInputSource::Human,
                text: "change course".to_owned(),
            })),
            "user (steered): change course\n"
        );
        assert_eq!(
            renderer.render(&HostedChatEvent::Progress(TurnEvent::IntraTurnInput {
                source: IntraTurnInputSource::Supervisor,
                text: "keep going".to_owned(),
            })),
            "supervisor (steered): keep going\n"
        );
    }

    #[test]
    fn followed_questions_name_the_exact_answer_command() {
        let mut renderer = ChatFollowRenderer::new(17);

        assert_eq!(
            renderer.render(&HostedChatEvent::AskUserAsked {
                prompt_id: 3,
                request: approval_request(),
            }),
            "question 3: Choose an outcome [approve|decline]\n\
             answer with: team answer 17 3 <option-id>\n"
        );
        assert_eq!(
            renderer.render(&HostedChatEvent::PermissionAsked(PermissionQuestion {
                prompt_id: 4,
                tool: "bash".to_owned(),
                target: "cargo test".to_owned(),
                access: "execute".to_owned(),
                reason: "runs the suite".to_owned(),
            })),
            "permission 4: tool=bash target=cargo test access=execute reason=runs the suite\n\
             answer with: team permission 17 4 <answer>\n"
        );
    }

    #[test]
    fn an_open_run_question_names_the_exact_answer_command() {
        let detail = proto::RunView {
            run: Some(proto::Run {
                run_id: 23,
                state: "awaiting_input".to_owned(),
                task: "prove the hint".to_owned(),
                scope: "crates/agens-cli".to_owned(),
                dod: "the hint renders".to_owned(),
                ..Default::default()
            }),
            questions: vec![
                proto::Question {
                    question_id: 7,
                    run_id: 23,
                    state: "open".to_owned(),
                    blocked_decision: "which port".to_owned(),
                    options: "8080|9090".to_owned(),
                    ..Default::default()
                },
                proto::Question {
                    question_id: 8,
                    run_id: 23,
                    state: "answered".to_owned(),
                    blocked_decision: "already settled".to_owned(),
                    options: "yes|no".to_owned(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let output = render_run_detail(&detail).expect("the run renders");

        assert!(
            output.contains("answer with: team answer 7 <answer>\n"),
            "{output}"
        );
        assert!(!output.contains("team answer 8"), "{output}");
    }

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
        let output = ChatFollowRenderer::new(17).render(&HostedChatEvent::AskUserAsked {
            prompt_id: 3,
            request: approval_request(),
        });

        assert!(
            output.starts_with("question 3: Choose an outcome [approve|decline]\n"),
            "{output}"
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
        let output = render_chat_detail(
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
            false,
            &[],
            &[],
        );

        assert_eq!(
            output,
            "chat 17\nuser: inspect this\ntool result: done\nstate: idle\n"
        );
    }
}
