//! The `team_*` group: what each verb accepts, what reaches the port, and what
//! the group cannot say at all.

use std::sync::{Arc, Mutex, atomic::AtomicBool};
use std::time::Duration;

use agens_core::ToolAccess;
use agens_core::coordination::{
    AnswerReceipt, AnswerRequest, CancelRequest, CoordinationError, CoordinationPort,
    DirectReceipt, DirectRequest, EscalateReceipt, EscalateRequest, MergeRequest,
    MergeRequestReceipt, ReclaimReceipt, ReclaimRequest, ReportRequest, RetryRequest, RunReport,
    RunStateReceipt, SpawnReceipt, SpawnRequest, TeamHealth, TeamQuestion, TeamRun, TeamStatus,
    UnavailableCoordinationPort,
};
use agens_core::run_introspection::AskOption;
use agens_tools::{DispatchTool, TeamTool, TeamVerb, ToolExecutionContext};
use serde_json::{Value, json};

/// Every request the port received, so a test asserts on the typed payload
/// rather than on the string the tool printed.
#[derive(Default)]
struct Recorded {
    spawns: Vec<SpawnRequest>,
    answers: Vec<AnswerRequest>,
    escalations: Vec<EscalateRequest>,
    directives: Vec<DirectRequest>,
    cancellations: Vec<CancelRequest>,
    retries: Vec<RetryRequest>,
    merges: Vec<MergeRequest>,
    reclaims: Vec<ReclaimRequest>,
    reports: Vec<ReportRequest>,
    statuses: usize,
}

#[derive(Clone, Default)]
struct RecordingPort {
    recorded: Arc<Mutex<Recorded>>,
}

impl RecordingPort {
    fn recorded(&self) -> std::sync::MutexGuard<'_, Recorded> {
        self.recorded.lock().unwrap()
    }
}

fn run(state: &str) -> TeamRun {
    TeamRun {
        run_id: 3,
        task: "port the importer".to_owned(),
        state: state.to_owned(),
        priority: 5,
        worktree_status: Some("active".to_owned()),
        parent_run_id: None,
        created_at: 1_700_000_000,
    }
}

fn question() -> TeamQuestion {
    TeamQuestion {
        question_id: 9,
        run_id: 3,
        kind: "question".to_owned(),
        blocked_decision: "which database the importer writes to".to_owned(),
        options: vec![AskOption::new(
            "postgres".to_owned(),
            "write to postgres".to_owned(),
            None,
        )],
        recommendation: Some("postgres".to_owned()),
        expires_at: None,
    }
}

impl CoordinationPort for RecordingPort {
    fn status(&mut self) -> Result<TeamStatus, CoordinationError> {
        self.recorded().statuses += 1;

        Ok(TeamStatus {
            repo_id: "a1b2c3d4".to_owned(),
            runs: vec![run("running")],
            open_questions: vec![question()],
        })
    }

    fn report(&mut self, request: &ReportRequest) -> Result<RunReport, CoordinationError> {
        self.recorded().reports.push(*request);

        Ok(RunReport {
            run: run("running"),
            scope: "the importer only".to_owned(),
            dod: "the suite is green".to_owned(),
            provider: "openai".to_owned(),
            result: None,
            attempts: Vec::new(),
            questions: vec![question()],
            findings: Vec::new(),
            health: Some(TeamHealth {
                noop_turns: 2,
                last_progress_turn: Some(4),
                tokens_since_progress: 900,
            }),
        })
    }

    fn answer(&mut self, request: &AnswerRequest) -> Result<AnswerReceipt, CoordinationError> {
        self.recorded().answers.push(request.clone());

        Ok(AnswerReceipt {
            question_id: request.question_id(),
            run_id: 3,
            run_resumed: true,
        })
    }

    fn escalate(
        &mut self,
        request: &EscalateRequest,
    ) -> Result<EscalateReceipt, CoordinationError> {
        self.recorded().escalations.push(request.clone());

        Ok(EscalateReceipt {
            question_id: 12,
            run_id: request.run_id(),
        })
    }

    fn direct(&mut self, request: &DirectRequest) -> Result<DirectReceipt, CoordinationError> {
        self.recorded().directives.push(request.clone());

        Ok(DirectReceipt {
            run_id: request.run_id(),
        })
    }

    fn cancel(&mut self, request: &CancelRequest) -> Result<RunStateReceipt, CoordinationError> {
        self.recorded().cancellations.push(request.clone());

        Ok(RunStateReceipt {
            run_id: request.run_id(),
            state: "cancelled".to_owned(),
            moved: true,
        })
    }

    fn spawn(&mut self, request: &SpawnRequest) -> Result<SpawnReceipt, CoordinationError> {
        self.recorded().spawns.push(request.clone());

        Ok(SpawnReceipt {
            run_id: 7,
            state: "draft".to_owned(),
        })
    }

    fn retry(&mut self, request: &RetryRequest) -> Result<RunStateReceipt, CoordinationError> {
        self.recorded().retries.push(request.clone());

        Ok(RunStateReceipt {
            run_id: request.run_id(),
            state: "queued".to_owned(),
            moved: true,
        })
    }

    fn request_merge(
        &mut self,
        request: &MergeRequest,
    ) -> Result<MergeRequestReceipt, CoordinationError> {
        self.recorded().merges.push(request.clone());

        Ok(MergeRequestReceipt {
            question_id: 21,
            run_id: request.run_id(),
            tree_hash: "cafe".to_owned(),
            paths_digest: "beef".to_owned(),
        })
    }

    fn request_reclaim(
        &mut self,
        request: &ReclaimRequest,
    ) -> Result<ReclaimReceipt, CoordinationError> {
        self.recorded().reclaims.push(*request);

        Ok(ReclaimReceipt {
            run_id: request.run_id(),
            worktree_status: "cleaned".to_owned(),
            moved: true,
        })
    }
}

fn context() -> ToolExecutionContext {
    ToolExecutionContext::with_timeout(Duration::from_secs(5))
}

fn call(port: RecordingPort, verb: TeamVerb, arguments: Value) -> Value {
    let mut tool = TeamTool::new(verb, Box::new(port));
    let output = tool.execute(&context(), arguments).expect("the tool runs");

    serde_json::from_str(&output.content).unwrap_or_else(|_| Value::String(output.content.clone()))
}

fn failure(port: RecordingPort, verb: TeamVerb, arguments: Value) -> String {
    let mut tool = TeamTool::new(verb, Box::new(port));
    let output = tool.execute(&context(), arguments).expect("the tool runs");

    assert!(
        output.is_error,
        "expected a refusal, got {}",
        output.content
    );

    output.content
}

#[test]
fn the_group_is_the_ten_verbs_of_the_first_round() {
    let names: Vec<&str> = TeamVerb::ALL.iter().map(|verb| verb.tool_name()).collect();

    assert_eq!(
        names,
        vec![
            "team_status",
            "team_report",
            "team_answer",
            "team_escalate",
            "team_direct",
            "team_cancel",
            "team_spawn",
            "team_retry",
            "team_merge",
            "team_reclaim",
        ]
    );
}

/// The invariant, read off the surface itself: nothing in this group takes a
/// path, a revision or a branch, so no call can be turned into a filesystem
/// write or a git invocation by choosing its arguments.
#[test]
fn no_verb_accepts_a_path_a_revision_or_a_branch() {
    let forbidden = [
        "path",
        "paths",
        "file",
        "directory",
        "dir",
        "root",
        "repo",
        "repo_root",
        "repository",
        "worktree",
        "revision",
        "rev",
        "ref",
        "branch",
        "commit",
        "start_point",
        "remote",
        "command",
        "argv",
    ];

    for verb in TeamVerb::ALL {
        let schema = verb.input_schema();
        let properties = schema
            .get("properties")
            .and_then(Value::as_object)
            .expect("every verb declares its properties");

        for key in properties.keys() {
            assert!(
                !forbidden.contains(&key.as_str()),
                "{} accepts {key}, which is a handle on the filesystem or on git",
                verb.tool_name()
            );
        }

        assert_eq!(
            schema.get("additionalProperties"),
            Some(&Value::Bool(false)),
            "{} would accept an argument nobody declared",
            verb.tool_name()
        );
    }
}

#[test]
fn only_the_two_reads_are_read_only() {
    for verb in TeamVerb::ALL {
        let expected = match verb {
            TeamVerb::Status | TeamVerb::Report => ToolAccess::ReadOnly,
            _ => ToolAccess::Write,
        };

        assert_eq!(verb.access(), expected, "{}", verb.tool_name());
    }
}

/// A manager that reads `team_merge` as "merge" reports work as landed that
/// nobody authorized, so the description has to say what the call really does.
#[test]
fn the_two_request_verbs_say_they_authorize_nothing() {
    assert!(
        TeamVerb::Merge.description().contains("authorizes nothing"),
        "{}",
        TeamVerb::Merge.description()
    );
    assert!(
        TeamVerb::Spawn.description().contains("draft"),
        "{}",
        TeamVerb::Spawn.description()
    );
}

#[test]
fn status_projects_the_team_and_everything_open_on_it() {
    let port = RecordingPort::default();
    let value = call(port.clone(), TeamVerb::Status, json!({}));

    assert_eq!(port.recorded().statuses, 1);
    assert_eq!(value["repo_id"], json!("a1b2c3d4"));
    assert_eq!(value["runs"][0]["run_id"], json!(3));
    assert_eq!(value["runs"][0]["state"], json!("running"));
    assert_eq!(value["open_questions"][0]["question_id"], json!(9));
    assert_eq!(
        value["open_questions"][0]["options"][0]["id"],
        json!("postgres")
    );
}

/// A worker's directory is the one argument every writing tool needs, so a
/// report never carries one.
#[test]
fn a_report_never_carries_a_worktree_path() {
    let port = RecordingPort::default();
    let value = call(port.clone(), TeamVerb::Report, json!({"run_id": 3}));

    assert_eq!(port.recorded().reports.len(), 1);
    assert_eq!(value["run"]["run_id"], json!(3));
    assert_eq!(value["scope"], json!("the importer only"));
    assert_eq!(value["health"]["noop_turns"], json!(2));

    let rendered = value.to_string();

    assert!(!rendered.contains("worktree_path"), "{rendered}");
    assert!(!rendered.contains("repo_root"), "{rendered}");
}

#[test]
fn an_answer_reaches_the_port_with_the_question_it_answers() {
    let port = RecordingPort::default();
    let value = call(
        port.clone(),
        TeamVerb::Answer,
        json!({"question_id": 9, "answer": "postgres"}),
    );

    let recorded = port.recorded();
    let answer = recorded
        .answers
        .first()
        .expect("the answer reached the port");

    assert_eq!(answer.question_id(), 9);
    assert_eq!(answer.answer(), "postgres");
    assert_eq!(value["status"], json!("answered"));
    assert_eq!(value["run_resumed"], json!(true));
}

#[test]
fn an_escalation_carries_the_options_the_person_is_choosing_between() {
    let port = RecordingPort::default();
    let value = call(
        port.clone(),
        TeamVerb::Escalate,
        json!({
            "run_id": 3,
            "blocked_decision": "which database the importer writes to",
            "options": [
                {"id": "postgres", "label": "write to postgres"},
                {"id": "sqlite", "label": "write to sqlite", "consequence": "no concurrency"}
            ],
            "recommendation": "postgres"
        }),
    );

    let recorded = port.recorded();
    let escalation = recorded
        .escalations
        .first()
        .expect("the escalation reached the port");

    assert_eq!(escalation.run_id(), 3);
    assert_eq!(escalation.question().options().len(), 2);
    assert_eq!(escalation.question().recommendation(), Some("postgres"));
    assert_eq!(value["status"], json!("escalated"));
    assert_eq!(value["question_id"], json!(12));
}

#[test]
fn an_escalation_with_no_options_is_refused_before_it_reaches_the_port() {
    let port = RecordingPort::default();
    let content = failure(
        port.clone(),
        TeamVerb::Escalate,
        json!({"run_id": 3, "blocked_decision": "what now", "options": []}),
    );

    assert!(content.contains("team_escalate"), "{content}");
    assert!(port.recorded().escalations.is_empty());
}

#[test]
fn a_directive_is_queued_rather_than_delivered() {
    let port = RecordingPort::default();
    let value = call(
        port.clone(),
        TeamVerb::Direct,
        json!({"run_id": 3, "directive": "stop widening the scope"}),
    );

    assert_eq!(
        port.recorded()
            .directives
            .first()
            .expect("the directive reached the port")
            .directive(),
        "stop widening the scope"
    );
    assert_eq!(value["status"], json!("queued"));
}

#[test]
fn a_cancellation_records_why() {
    let port = RecordingPort::default();
    let value = call(
        port.clone(),
        TeamVerb::Cancel,
        json!({"run_id": 3, "reason": "the task was withdrawn"}),
    );

    assert_eq!(
        port.recorded()
            .cancellations
            .first()
            .expect("the cancellation reached the port")
            .reason(),
        "the task was withdrawn"
    );
    assert_eq!(value["status"], json!("cancelled"));
    assert_eq!(value["state"], json!("cancelled"));
}

#[test]
fn a_spawn_reads_back_the_draft_it_landed_as() {
    let port = RecordingPort::default();
    let value = call(
        port.clone(),
        TeamVerb::Spawn,
        json!({
            "task": "port the importer",
            "scope": "the importer only",
            "dod": "the suite is green",
            "priority": 5
        }),
    );

    let recorded = port.recorded();
    let spawn = recorded.spawns.first().expect("the spawn reached the port");

    assert_eq!(spawn.task(), "port the importer");
    assert_eq!(spawn.priority(), 5);
    assert_eq!(value["status"], json!("proposed"));
    assert_eq!(value["state"], json!("draft"));
}

#[test]
fn a_retry_carries_the_guidance_that_makes_it_a_different_attempt() {
    let port = RecordingPort::default();
    let value = call(
        port.clone(),
        TeamVerb::Retry,
        json!({"run_id": 3, "guidance": "start from the failing test"}),
    );

    assert_eq!(
        port.recorded()
            .retries
            .first()
            .expect("the retry reached the port")
            .guidance(),
        "start from the failing test"
    );
    assert_eq!(value["status"], json!("queued"));
}

#[test]
fn a_merge_is_an_authorization_request_and_says_so() {
    let port = RecordingPort::default();
    let value = call(
        port.clone(),
        TeamVerb::Merge,
        json!({"run_id": 3, "reason": "the definition of done is met"}),
    );

    assert_eq!(port.recorded().merges.len(), 1);
    assert_eq!(value["status"], json!("authorization_requested"));
    assert_eq!(value["question_id"], json!(21));
    assert_eq!(value["tree_hash"], json!("cafe"));
}

#[test]
fn a_reclaim_reads_back_what_the_coordinator_did_with_the_worktree() {
    let port = RecordingPort::default();
    let value = call(port.clone(), TeamVerb::Reclaim, json!({"run_id": 3}));

    assert_eq!(port.recorded().reclaims.len(), 1);
    assert_eq!(value["status"], json!("released"));
    assert_eq!(value["worktree_status"], json!("cleaned"));
}

#[test]
fn an_undeclared_argument_is_a_malformed_call() {
    let port = RecordingPort::default();
    let content = failure(
        port.clone(),
        TeamVerb::Reclaim,
        json!({"run_id": 3, "worktree_path": "/tmp/somewhere"}),
    );

    assert!(content.contains("arguments are invalid"), "{content}");
    assert!(port.recorded().reclaims.is_empty());
}

#[test]
fn a_run_identifier_that_names_no_row_never_reaches_the_port() {
    let port = RecordingPort::default();
    let content = failure(
        port.clone(),
        TeamVerb::Direct,
        json!({"run_id": 0, "directive": "go"}),
    );

    assert!(content.contains("run_id"), "{content}");
    assert!(port.recorded().directives.is_empty());
}

/// A session with no team behind it is told the call cannot work rather than
/// being invited to try again.
#[test]
fn a_session_managing_no_team_is_told_retrying_changes_nothing() {
    let mut tool = TeamTool::new(TeamVerb::Status, Box::new(UnavailableCoordinationPort));
    let output = tool.execute(&context(), json!({})).expect("the tool runs");

    assert!(output.is_error);
    assert!(
        output.content.contains("manages no team"),
        "{}",
        output.content
    );
    assert!(
        output.content.contains("will not change that"),
        "{}",
        output.content
    );
}

#[test]
fn an_unauthorized_call_is_reported_as_a_dead_end() {
    struct Refusing;

    impl CoordinationPort for Refusing {
        fn status(&mut self) -> Result<TeamStatus, CoordinationError> {
            unreachable!("this test only calls cancel")
        }

        fn report(&mut self, _: &ReportRequest) -> Result<RunReport, CoordinationError> {
            unreachable!("this test only calls cancel")
        }

        fn answer(&mut self, _: &AnswerRequest) -> Result<AnswerReceipt, CoordinationError> {
            unreachable!("this test only calls cancel")
        }

        fn escalate(&mut self, _: &EscalateRequest) -> Result<EscalateReceipt, CoordinationError> {
            unreachable!("this test only calls cancel")
        }

        fn direct(&mut self, _: &DirectRequest) -> Result<DirectReceipt, CoordinationError> {
            unreachable!("this test only calls cancel")
        }

        fn cancel(&mut self, _: &CancelRequest) -> Result<RunStateReceipt, CoordinationError> {
            Err(CoordinationError::Unauthorized(
                "praetor may not cancel_run".to_owned(),
            ))
        }

        fn spawn(&mut self, _: &SpawnRequest) -> Result<SpawnReceipt, CoordinationError> {
            unreachable!("this test only calls cancel")
        }

        fn retry(&mut self, _: &RetryRequest) -> Result<RunStateReceipt, CoordinationError> {
            unreachable!("this test only calls cancel")
        }

        fn request_merge(
            &mut self,
            _: &MergeRequest,
        ) -> Result<MergeRequestReceipt, CoordinationError> {
            unreachable!("this test only calls cancel")
        }

        fn request_reclaim(
            &mut self,
            _: &ReclaimRequest,
        ) -> Result<ReclaimReceipt, CoordinationError> {
            unreachable!("this test only calls cancel")
        }
    }

    let mut tool = TeamTool::new(TeamVerb::Cancel, Box::new(Refusing));
    let output = tool
        .execute(
            &context(),
            json!({"run_id": 3, "reason": "no longer needed"}),
        )
        .expect("the tool runs");

    assert!(output.is_error);
    assert!(
        output.content.contains("will not change that"),
        "{}",
        output.content
    );
}

#[test]
fn a_cancelled_call_never_reaches_the_port() {
    let port = RecordingPort::default();
    let cancellation = Arc::new(AtomicBool::new(true));
    let mut tool = TeamTool::new(TeamVerb::Status, Box::new(port.clone()));
    let output = tool
        .execute(
            &ToolExecutionContext::new(cancellation, Duration::from_secs(5)),
            json!({}),
        )
        .expect("the tool runs");

    assert!(output.is_error);
    assert_eq!(port.recorded().statuses, 0);
}

#[test]
fn the_permission_target_is_the_operation_itself() {
    for verb in TeamVerb::ALL {
        let tool = TeamTool::new(verb, Box::new(UnavailableCoordinationPort));

        assert_eq!(
            tool.permission_target(&json!({})).unwrap(),
            verb.tool_name()
        );
    }
}
