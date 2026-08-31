//! Reading the daemon's control plane into the board the terminal draws.
//!
//! The read models the fleet console already consumes carry everything the
//! supervision surface shows, in the shapes the wire uses: a tree of run
//! summaries per repository, a detail view per run, and an inbox of open
//! questions. What is missing from them is only presentation — how long a run
//! has been parked, what it has cost across its attempts, what to call the
//! repository — and that is what this module derives.
//!
//! Every function here is a projection. Nothing decides, retries or writes.

use std::path::Path;
use std::time::Duration;

use agens_coordinator_client::{ClientError, Coordinator, OpenChat, proto};
use agens_tui::team::{
    TeamAttempt, TeamEvent, TeamEventClass, TeamInboxItem, TeamNode, TeamNodeDetail, TeamQuestion,
    TeamRepo, TeamSnapshot, TeamState,
};

/// Runs whose cost and attempt count are worth a detail round trip.
///
/// A settled run keeps its row on the board, but nothing about it will change
/// again, so the tree does not pay a call per run to redraw a finished one.
const LIVE_STATES: [&str; 4] = ["queued", "running", "awaiting_input", "awaiting_quota"];

/// Every repository, run, chat and open question the daemon holds.
///
/// `own_session` is this terminal's own chat, marked on the board so the reader
/// can see the session they are looking from among the ones they are watching.
pub async fn read_fleet(
    coordinator: &Coordinator,
    own_session: Option<i64>,
    now: i64,
) -> Result<TeamSnapshot, ClientError> {
    let mut feed = coordinator.feed();
    let mut chat = coordinator.chat();
    let mut repos = Vec::new();
    let mut inbox = Vec::new();

    for repo_id in feed.repos().await? {
        let tree = feed.tree(&repo_id).await?;
        let items = inbox_items(&repo_id, &feed.inbox(&repo_id).await?, now);
        let mut nodes = Vec::new();
        let mut label = None;

        for summary in &tree.runs {
            let detail = if LIVE_STATES.contains(&summary.state.as_str()) {
                Some(feed.run_detail(summary.run_id).await?)
            } else {
                None
            };
            label = label.or_else(|| {
                detail
                    .as_ref()
                    .and_then(|view| view.run.as_ref())
                    .map(|run| repository_label(&run.repo_root))
            });
            nodes.push(run_node(summary, detail.as_ref(), &items, now));
        }

        inbox.extend(items);
        repos.push(TeamRepo {
            label: label.unwrap_or_else(|| repo_id.clone()),
            id: repo_id,
            nodes,
        });
    }

    let chats: Vec<TeamNode> = chat
        .open_everywhere()
        .await?
        .iter()
        .map(|open| chat_node(open, own_session))
        .collect();
    if !chats.is_empty() {
        repos.push(TeamRepo {
            id: "chats".to_owned(),
            label: "chats".to_owned(),
            nodes: chats,
        });
    }

    Ok(TeamSnapshot { repos, inbox })
}

/// Everything the daemon knows about one run.
pub async fn read_detail(
    coordinator: &Coordinator,
    run_id: i64,
) -> Result<TeamNodeDetail, ClientError> {
    let view = coordinator.feed().run_detail(run_id).await?;

    Ok(node_detail(run_id, &view))
}

/// What to call a repository on a board, given the path the daemon holds it at.
///
/// The daemon's own identifier is a fingerprint, which tells a reader nothing.
fn repository_label(repo_root: &str) -> String {
    Path::new(repo_root).file_name().map_or_else(
        || repo_root.to_owned(),
        |name| name.to_string_lossy().into(),
    )
}

/// One run as a row of the tree.
///
/// Cost is the sum of what every attempt spent, because that is what the run
/// has cost; the attempt number is the try it is on now.
fn run_node(
    summary: &proto::RunSummary,
    detail: Option<&proto::RunView>,
    inbox: &[TeamInboxItem],
    now: i64,
) -> TeamNode {
    let state = TeamState::parse(&summary.state);
    let attempts = detail.map(|view| view.attempts.as_slice()).unwrap_or(&[]);
    let cost: i64 = attempts
        .iter()
        .filter_map(|attempt| attempt.cost_micros)
        .sum();
    let waiting = inbox
        .iter()
        .find(|item| item.run_id == summary.run_id)
        .map(|item| item.waiting_label().to_owned());
    let parked_for = state.is_parked().then(|| {
        let since = detail
            .and_then(|view| view.events.last())
            .map_or(summary.created_at, |event| event.ts);

        elapsed(now, since)
    });

    TeamNode {
        parent: summary.parent_run_id,
        attempt: attempts.iter().map(|attempt| attempt.n).max(),
        model: Some(summary.provider.clone()),
        cost_micros: (cost > 0).then_some(cost),
        duration: Some(elapsed(now, summary.created_at)),
        waiting,
        parked_for,
        ..TeamNode::run(summary.run_id, summary.task.clone(), state)
    }
}

/// One hosted chat as a row of the tree.
fn chat_node(open: &OpenChat, own_session: Option<i64>) -> TeamNode {
    let state = if open.answering {
        TeamState::Answering
    } else {
        TeamState::Idle
    };

    TeamNode {
        is_self: own_session == Some(open.session_id),
        ..TeamNode::chat(open.session_id, open.checkout.display().to_string(), state)
    }
}

/// One repository's open questions.
fn inbox_items(repo_id: &str, view: &proto::InboxView, now: i64) -> Vec<TeamInboxItem> {
    view.items
        .iter()
        .map(|item| TeamInboxItem {
            repo_id: repo_id.to_owned(),
            run_id: item.run_id,
            question_id: item.question_id,
            kind: item.kind.clone(),
            blocked_decision: item.blocked_decision.clone(),
            options: parse_options(&item.options),
            recommendation: item.recommendation.clone(),
            age: Some(elapsed(now, item.created_at)),
        })
        .collect()
}

fn node_detail(run_id: i64, view: &proto::RunView) -> TeamNodeDetail {
    let run = view.run.as_ref();

    TeamNodeDetail {
        node_id: run_id,
        task: run.map(|run| run.task.clone()).unwrap_or_default(),
        scope: run.map(|run| run.scope.clone()).unwrap_or_default(),
        definition_of_done: run.map(|run| run.dod.clone()).unwrap_or_default(),
        worktree: run.and_then(|run| run.worktree_path.clone()),
        attempts: view
            .attempts
            .iter()
            .map(|attempt| TeamAttempt {
                n: attempt.n,
                outcome: attempt.outcome.clone(),
                tokens: attempt.tokens,
                cost_micros: attempt.cost_micros,
                duration: attempt
                    .ended_at
                    .map(|ended| elapsed(ended, attempt.started_at)),
            })
            .collect(),
        questions: view
            .questions
            .iter()
            .filter(|question| question.state == "open")
            .map(|question| TeamQuestion {
                question_id: question.question_id,
                run_id: question.run_id,
                kind: question.kind.clone(),
                blocked_decision: question.blocked_decision.clone(),
                options: parse_options(&question.options),
                recommendation: question.recommendation.clone(),
            })
            .collect(),
        events: view
            .events
            .iter()
            .map(|event| TeamEvent {
                class: TeamEventClass::parse(&event.class),
                kind: event.r#type.clone(),
                payload: event.payload.clone(),
                ts: event.ts,
            })
            .collect(),
    }
}

/// The options a question admits, out of the JSON array the daemon carries
/// them in. A payload this build cannot read offers nothing rather than a
/// fabricated choice.
fn parse_options(options: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(options).unwrap_or_default()
}

/// A span between two wall-clock seconds, never negative: a clock that moved
/// backwards is a reading this surface cannot make sense of, and zero is a
/// smaller lie than a span in the future.
fn elapsed(now: i64, then: i64) -> Duration {
    Duration::from_secs(now.saturating_sub(then).max(0).unsigned_abs())
}

#[cfg(test)]
mod tests {
    use agens_tui::team::waiting_label_for_kind;

    use super::*;

    fn summary(run_id: i64, state: &str) -> proto::RunSummary {
        proto::RunSummary {
            run_id,
            task: "ship the api".to_owned(),
            state: state.to_owned(),
            priority: 0,
            provider: "openai".to_owned(),
            worktree_status: None,
            parent_run_id: None,
            external_ref: None,
            created_at: 1_000,
        }
    }

    fn attempt(n: i64, cost_micros: Option<i64>) -> proto::Attempt {
        proto::Attempt {
            attempt_id: n,
            run_id: 11,
            n,
            session_id: None,
            session_attempt_id: None,
            started_at: 1_000,
            ended_at: Some(1_600),
            outcome: Some("failed".to_owned()),
            retry_trigger: None,
            tokens: Some(120),
            cost_micros,
        }
    }

    fn view(attempts: Vec<proto::Attempt>, events: Vec<proto::Event>) -> proto::RunView {
        proto::RunView {
            run: Some(proto::Run {
                run_id: 11,
                repo_id: "a1b2c3d4e5f60718".to_owned(),
                repo_root: "/home/someone/dev/agens".to_owned(),
                remote_url: None,
                external_ref: None,
                parent_run_id: None,
                task: "ship the api".to_owned(),
                scope: "crates/agens-api".to_owned(),
                dod: "the gate is green".to_owned(),
                genesis_paths: None,
                state: "running".to_owned(),
                priority: 0,
                dep_run_id: None,
                provider: "openai".to_owned(),
                budget_tokens: None,
                worktree_path: Some("/w/agn-11".to_owned()),
                worktree_status: None,
                created_at: 1_000,
                result: None,
            }),
            attempts,
            questions: Vec::new(),
            findings: Vec::new(),
            events,
            health: None,
        }
    }

    fn event(class: &str, kind: &str, ts: i64) -> proto::Event {
        proto::Event {
            id: ts,
            run_id: Some(11),
            r#type: kind.to_owned(),
            class: class.to_owned(),
            payload: "{}".to_owned(),
            ts,
        }
    }

    #[test]
    fn a_runs_cost_is_what_every_attempt_spent_and_its_number_is_the_current_try() {
        let detail = view(
            vec![attempt(1, Some(180_000)), attempt(2, Some(240_000))],
            Vec::new(),
        );

        let node = run_node(&summary(11, "running"), Some(&detail), &[], 1_500);

        assert_eq!(node.attempt, Some(2));
        assert_eq!(node.cost_micros, Some(420_000));
        assert_eq!(node.model.as_deref(), Some("openai"));
        assert_eq!(node.duration, Some(Duration::from_secs(500)));
    }

    #[test]
    fn a_run_that_has_spent_nothing_reports_no_cost_rather_than_zero() {
        let detail = view(vec![attempt(1, None)], Vec::new());

        let node = run_node(&summary(11, "running"), Some(&detail), &[], 1_500);

        assert_eq!(node.cost_micros, None);
    }

    #[test]
    fn a_parked_run_is_timed_from_the_last_thing_that_happened_to_it() {
        let detail = view(
            vec![attempt(1, None)],
            vec![event("infra", "run_awaiting_input", 1_200)],
        );
        let inbox = vec![TeamInboxItem {
            repo_id: "a1b2c3d4e5f60718".to_owned(),
            run_id: 11,
            question_id: 5,
            kind: "approval".to_owned(),
            blocked_decision: "merge the branch".to_owned(),
            options: vec!["merge".to_owned()],
            recommendation: None,
            age: None,
        }];

        let node = run_node(&summary(11, "awaiting_input"), Some(&detail), &inbox, 1_500);

        assert_eq!(node.parked_for, Some(Duration::from_secs(300)));
        assert_eq!(node.waiting.as_deref(), Some("merge authorization"));
        assert_eq!(
            node.parked_line().as_deref(),
            Some("merge authorization · parked 5m 00s")
        );
    }

    #[test]
    fn a_run_that_is_not_parked_is_not_given_something_to_wait_for() {
        let node = run_node(&summary(11, "running"), None, &[], 1_500);

        assert_eq!(node.parked_for, None);
        assert_eq!(node.parked_line(), None);
    }

    #[test]
    fn a_repository_is_named_by_its_directory_rather_than_its_fingerprint() {
        assert_eq!(repository_label("/home/someone/dev/agens"), "agens");
        assert_eq!(repository_label(""), "");
    }

    #[test]
    fn a_detail_carries_the_run_its_attempts_and_its_journal() {
        let mut detail = view(
            vec![attempt(1, Some(180_000))],
            vec![
                event("agent", "tool_call", 1_100),
                event("infra", "quota_reached", 1_200),
            ],
        );
        detail.questions = vec![
            proto::Question {
                question_id: 5,
                run_id: 11,
                kind: "approval".to_owned(),
                blocked_decision: "merge the branch".to_owned(),
                options: "[\"merge\",\"reject\"]".to_owned(),
                recommendation: Some("merge".to_owned()),
                answer: None,
                author: None,
                expires_at: None,
                tree_hash: None,
                paths_digest: None,
                state: "open".to_owned(),
                created_at: 1_100,
            },
            proto::Question {
                state: "answered".to_owned(),
                ..proto::Question {
                    question_id: 4,
                    run_id: 11,
                    kind: "question".to_owned(),
                    blocked_decision: "which store".to_owned(),
                    options: "[]".to_owned(),
                    recommendation: None,
                    answer: Some("sessions".to_owned()),
                    author: None,
                    expires_at: None,
                    tree_hash: None,
                    paths_digest: None,
                    state: "answered".to_owned(),
                    created_at: 1_000,
                }
            },
        ];

        let projected = node_detail(11, &detail);

        assert_eq!(projected.worktree.as_deref(), Some("/w/agn-11"));
        assert_eq!(projected.definition_of_done, "the gate is green");
        assert_eq!(projected.attempts.len(), 1);
        assert_eq!(
            projected.attempts.first().and_then(|first| first.duration),
            Some(Duration::from_secs(600))
        );
        assert_eq!(projected.questions.len(), 1);
        assert_eq!(
            projected.questions.first().map(|first| first.options.len()),
            Some(2)
        );
        assert_eq!(projected.events_of(TeamEventClass::Infra).len(), 1);
        assert_eq!(projected.events_of(TeamEventClass::Agent).len(), 1);
    }

    #[test]
    fn an_options_payload_this_build_cannot_read_offers_nothing_it_invented() {
        assert_eq!(parse_options("[\"merge\"]"), ["merge"]);
        assert_eq!(parse_options("not json"), Vec::<String>::new());
        assert_eq!(parse_options("{}"), Vec::<String>::new());
    }

    #[test]
    fn the_inbox_carries_how_long_each_question_has_been_waiting() {
        let items = inbox_items(
            "a1b2c3d4e5f60718",
            &proto::InboxView {
                repo_id: "a1b2c3d4e5f60718".to_owned(),
                items: vec![proto::InboxItem {
                    run_id: 11,
                    question_id: 5,
                    kind: "approval".to_owned(),
                    blocked_decision: "merge the branch".to_owned(),
                    options: "[\"merge\",\"reject\"]".to_owned(),
                    recommendation: None,
                    expires_at: None,
                    created_at: 1_260,
                }],
            },
            1_500,
        );
        let first = items.first().expect("the inbox carries its one item");

        assert_eq!(first.age, Some(Duration::from_secs(240)));
        assert_eq!(first.options, ["merge", "reject"]);
        assert!(first.is_approval());
        assert_eq!(first.waiting_label(), waiting_label_for_kind("approval"));
    }

    #[test]
    fn this_terminals_own_chat_is_marked_and_the_others_are_not() {
        let mine = OpenChat {
            session_id: 90,
            checkout: std::path::PathBuf::from("/home/someone/dev/agens"),
            answering: true,
        };
        let theirs = OpenChat {
            session_id: 91,
            answering: false,
            ..mine.clone()
        };

        let mine = chat_node(&mine, Some(90));
        let theirs = chat_node(&theirs, Some(90));

        assert!(mine.is_self);
        assert_eq!(mine.state, TeamState::Answering);
        assert!(!theirs.is_self);
        assert_eq!(theirs.state, TeamState::Idle);
    }

    #[test]
    fn a_clock_that_moved_backwards_reads_as_no_time_rather_than_a_future_span() {
        assert_eq!(elapsed(1_000, 1_500), Duration::from_secs(0));
        assert_eq!(elapsed(1_500, 1_000), Duration::from_secs(500));
    }
}
