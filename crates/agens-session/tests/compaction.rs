use agens_core::compaction::CompactionBudget;
use agens_core::{Message, MessagePart, Role, SessionMetadata};
use agens_diagnostics::{CompactionReason, SafeDiagnosticStore};
use agens_providers::DiagnosticRef;
use agens_session::compaction::{CompactionFailure, CompactionSummarizer, compact_session};
use agens_store::{CompactionStore, SessionStore};

struct Temporary(std::path::PathBuf);

impl Drop for Temporary {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

fn directory(label: &str) -> Temporary {
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the fixture clock is after the epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "agens-session-compaction-{label}-{}-{started}",
        std::process::id()
    ));
    std::fs::remove_dir_all(&path).ok();
    std::fs::create_dir_all(&path).expect("test data directory");
    Temporary(path)
}

const SESSION: i64 = 7;

fn seed_session(path: &std::path::Path) {
    let metadata = SessionMetadata {
        id: SESSION,
        project: "project".into(),
        title: "title".into(),
        active_agent: "primary".into(),
        provider_id: None,
        model_id: None,
        reasoning_effort: None,
        created_at: 10,
        updated_at: 20,
        completed_turn_count: 0,
        resumable: false,
        parent_session_id: None,
        fork_message_count: None,
    };
    let mut store = SessionStore::open(path).expect("the session store opens");
    store
        .begin_session_attempt(&metadata, "retry".into())
        .expect("the session row is created");
}

struct Fixed(&'static str);

impl CompactionSummarizer for Fixed {
    fn summarize(&self, _prompt: &str) -> Result<String, String> {
        Ok(self.0.to_owned())
    }
}

struct Failing;

impl CompactionSummarizer for Failing {
    fn summarize(&self, _prompt: &str) -> Result<String, String> {
        Err("provider refused".to_owned())
    }
}

/// Captures the prompt the summarizer was handed, so a test can assert what the
/// model was actually asked without a provider in the loop.
struct Recording(std::cell::RefCell<Vec<String>>);

impl CompactionSummarizer for Recording {
    fn summarize(&self, prompt: &str) -> Result<String, String> {
        self.0.borrow_mut().push(prompt.to_owned());
        Ok("summary".to_owned())
    }
}

fn text(role: Role, body: &str) -> Message {
    Message {
        role,
        parts: vec![MessagePart::Text(body.to_owned())],
    }
}

fn history() -> Vec<Message> {
    vec![
        text(Role::User, "the first question"),
        text(Role::Assistant, "the first answer"),
        text(Role::User, "the second question"),
        text(Role::Assistant, "the second answer"),
    ]
}

fn tiny_budget() -> CompactionBudget {
    CompactionBudget {
        keep_recent_tokens: 5,
    }
}

fn recorded_events(path: &std::path::Path) -> Vec<String> {
    let mut events = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(path.join("diagnostics"))
        .expect("the diagnostics directory exists")
        .map(|entry| entry.expect("a diagnostics entry is readable").path())
        .collect();
    entries.sort();
    for entry in entries {
        for line in std::fs::read_to_string(&entry)
            .expect("a diagnostics file is readable")
            .lines()
        {
            let value: serde_json::Value =
                serde_json::from_str(line).expect("a diagnostics line is json");
            if let Some(event) = value["event"].as_str() {
                events.push(event.to_owned());
            }
        }
    }
    events
}

fn reference() -> DiagnosticRef {
    DiagnosticRef::new("abcd1234".to_owned()).expect("the reference is well formed")
}

#[test]
fn a_compaction_replaces_the_head_records_it_and_announces_both_ends() {
    let temporary = directory("applied");
    seed_session(&temporary.0);
    let mut store = CompactionStore::open(&temporary.0).expect("the compaction store opens");
    let diagnostics = SafeDiagnosticStore::with_capture(temporary.0.clone(), true);
    let messages = history();

    let compacted = compact_session(
        &mut store,
        &diagnostics,
        &reference(),
        SESSION,
        &messages,
        tiny_budget(),
        CompactionReason::Overflow,
        &Fixed("what happened earlier"),
    )
    .expect("the history is compactable");

    assert!(compacted.messages.len() < messages.len());
    assert_eq!(compacted.messages[0].role, Role::System);
    assert_eq!(
        compacted.summarized + compacted.kept,
        messages.len(),
        "every message is either summarized or kept",
    );
    assert_eq!(
        store
            .latest(SESSION)
            .expect("the record is readable")
            .expect("a record was appended")
            .summary,
        "what happened earlier"
    );
    assert_eq!(
        recorded_events(&temporary.0),
        vec!["compaction_started", "compaction_ended"]
    );
}

/// The history a caller holds is the only copy of the thread. A summarizing
/// call that fails must leave it whole, or a turn that could still have been
/// retried becomes a session with a hole in it.
#[test]
fn a_failed_summarizing_call_changes_nothing() {
    let temporary = directory("failed-summary");
    seed_session(&temporary.0);
    let mut store = CompactionStore::open(&temporary.0).expect("the compaction store opens");
    let diagnostics = SafeDiagnosticStore::with_capture(temporary.0.clone(), true);
    let messages = history();

    let failure = compact_session(
        &mut store,
        &diagnostics,
        &reference(),
        SESSION,
        &messages,
        tiny_budget(),
        CompactionReason::Overflow,
        &Failing,
    )
    .expect_err("a refused summary refuses the compaction");

    assert_eq!(
        failure,
        CompactionFailure::Summarizer("provider refused".to_owned())
    );
    assert_eq!(messages, history(), "the history is untouched");
    assert_eq!(store.latest(SESSION).expect("the record is readable"), None);
    assert_eq!(
        recorded_events(&temporary.0),
        vec!["compaction_started", "compaction_ended"],
        "a refusal is announced, not swallowed",
    );
}

#[test]
fn an_empty_summary_refuses_the_compaction_and_records_nothing() {
    let temporary = directory("empty-summary");
    seed_session(&temporary.0);
    let mut store = CompactionStore::open(&temporary.0).expect("the compaction store opens");
    let diagnostics = SafeDiagnosticStore::with_capture(temporary.0.clone(), true);
    let messages = history();

    let failure = compact_session(
        &mut store,
        &diagnostics,
        &reference(),
        SESSION,
        &messages,
        tiny_budget(),
        CompactionReason::Overflow,
        &Fixed("   \n  "),
    )
    .expect_err("an empty summary refuses the compaction");

    assert!(matches!(failure, CompactionFailure::Summary(_)));
    assert_eq!(store.latest(SESSION).expect("the record is readable"), None);
}

/// Without this a session compacted twice loses everything older than the last
/// cut: the second summary would describe only the stretch since the first.
#[test]
fn a_second_compaction_folds_the_first_summary_into_its_prompt() {
    let temporary = directory("iterative");
    seed_session(&temporary.0);
    let mut store = CompactionStore::open(&temporary.0).expect("the compaction store opens");
    let diagnostics = SafeDiagnosticStore::with_capture(temporary.0.clone(), true);

    compact_session(
        &mut store,
        &diagnostics,
        &reference(),
        SESSION,
        &history(),
        tiny_budget(),
        CompactionReason::Overflow,
        &Fixed("what happened earlier"),
    )
    .expect("the first compaction succeeds");

    let recording = Recording(std::cell::RefCell::new(Vec::new()));
    compact_session(
        &mut store,
        &diagnostics,
        &reference(),
        SESSION,
        &history(),
        tiny_budget(),
        CompactionReason::Threshold,
        &recording,
    )
    .expect("the second compaction succeeds");

    let prompts = recording.0.borrow();
    assert_eq!(prompts.len(), 1);
    assert!(
        prompts[0].contains("what happened earlier"),
        "the previous summary is carried into the next prompt: {}",
        prompts[0]
    );
}
