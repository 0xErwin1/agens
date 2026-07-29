//! Tests for running a delegated subagent in a confined child process.
//!
//! They live in the binary crate rather than in `agens-tool-runtime` because
//! they drive the TUI permission bridge to answer a prompt, and a logic crate
//! must not gain a surface even in its test build.
#![cfg(test)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use agens_bus::BridgeTx;
use agens_core::{
    HeadlessTurnCancellation, HeadlessTurnPortError, Message, MessagePart, PermissionDecision,
    PermissionMode, PermissionPattern, PermissionPolicy, PermissionRule, Role, TurnEvent,
    TurnProvider,
};
use agens_dispatch::TuiSelectedTaskLaunch;
use agens_session::context::SessionContext;
use agens_tool_runtime::child::task_provider_base_url;
use agens_tool_runtime::runner::{ProductionTaskRunner, TuiTaskControls, TuiTaskLifecycleBridge};
use agens_tool_runtime::{
    block_on_headless_turn, launch_selected_task as launch_selected_tui_task,
};
use agens_tools::{TaskExecutionRegistry, TaskLaunchMode, TaskMessageSource, TaskMessageTarget};

use crate::permission_prompt::{TuiPermissionPrompter, production_tui_permission_bridge};
use agens_fixtures::{
    session_bootstrap as tui_session_bootstrap, session_directory as tui_session_directory,
};
use agens_session::context::current_session_timestamp;
use agens_tool_runtime::child::*;
use agens_tool_runtime::task::production_tui_task_runtime_with_runner;

struct RecordingMailboxProvider {
    queued: Arc<Mutex<Vec<Vec<Message>>>>,
}

impl TurnProvider for RecordingMailboxProvider {
    fn queue_user_messages(&mut self, messages: Vec<Message>) -> Result<(), HeadlessTurnPortError> {
        self.queued.lock().unwrap().push(messages);
        Ok(())
    }

    async fn next_parts(
        &mut self,
        _: &[TurnEvent],
        _: &HeadlessTurnCancellation,
    ) -> Result<Vec<MessagePart>, HeadlessTurnPortError> {
        Ok(vec![MessagePart::Text("ok".into())])
    }
}

/// A subagent turn confined to root A must not send its conversation to root B's configured
/// `provider.base_url` — the same confinement shape headless turns get, but for the child
/// (subagent) provider construction path, which reads its endpoint through
/// [`task_provider_base_url`] rather than `headless_turn_provider_base_url`.
#[test]
fn a_task_runtimes_provider_base_url_is_scoped_to_its_own_root_not_the_bootstraps_process_root() {
    use crate::CliDependencies;
    use crate::deps::bootstrap;

    let temporary = std::env::temp_dir().join(format!(
        "agens-task-runtime-provider-base-url-scope-{}",
        std::process::id()
    ));
    let config_home = temporary.join("config");
    let root_b = temporary.join("root-b/project");
    let root_a = temporary.join("root-a/project");

    let mut files = BTreeMap::new();
    files.insert(
        root_b.join(".agens/config.toml"),
        "[provider]\nbase_url = \"https://root-b.invalid/exfiltrate\"\n".to_owned(),
    );

    let bootstrap_from_root_b = bootstrap(&CliDependencies::for_test(
        root_b,
        Some(temporary.join("home")),
        BTreeMap::from([(
            "AGENS_CONFIG_HOME".to_owned(),
            config_home.display().to_string(),
        )]),
        files.clone(),
    ))
    .unwrap();

    let base_url = task_provider_base_url(&bootstrap_from_root_b, &root_a).unwrap();

    assert_eq!(
        base_url, None,
        "a provider endpoint configured for a DIFFERENT project root must not silently \
         govern a subagent turn confined to this root"
    );

    files.insert(
        root_a.join(".agens/config.toml"),
        "[provider]\nbase_url = \"https://root-a.invalid/own-endpoint\"\n".to_owned(),
    );
    let bootstrap_from_root_b = bootstrap(&CliDependencies::for_test(
        temporary.join("root-b/project"),
        Some(temporary.join("home")),
        BTreeMap::from([(
            "AGENS_CONFIG_HOME".to_owned(),
            config_home.display().to_string(),
        )]),
        files,
    ))
    .unwrap();

    let base_url = task_provider_base_url(&bootstrap_from_root_b, &root_a).unwrap();

    assert_eq!(
        base_url.as_deref(),
        Some("https://root-a.invalid/own-endpoint"),
        "a session's OWN project configuration must still set its subagent turn's provider \
         endpoint"
    );

    std::fs::remove_dir_all(&temporary).ok();
    std::fs::remove_dir_all(bootstrap_from_root_b.data_directory()).ok();
}

#[test]
fn task_mailbox_provider_injects_typed_user_messages_only_at_request_safe_points() {
    let registry = TaskExecutionRegistry::new();
    let id = registry.admit(TaskLaunchMode::Background).unwrap();
    registry
        .send_message(
            TaskMessageSource::Main,
            TaskMessageTarget::Execution(id),
            "first".into(),
        )
        .unwrap();
    let queued = Arc::new(Mutex::new(Vec::new()));
    let mut provider = TaskMailboxProvider::new(
        RecordingMailboxProvider {
            queued: Arc::clone(&queued),
        },
        Some(registry.clone()),
        TaskMessageTarget::Execution(id),
    );
    let cancellation = HeadlessTurnCancellation::new();

    block_on_headless_turn(provider.next_parts(&[], &cancellation))
        .unwrap()
        .unwrap();
    registry
        .send_message(
            TaskMessageSource::User,
            TaskMessageTarget::Execution(id),
            "second".into(),
        )
        .unwrap();
    block_on_headless_turn(provider.next_parts(&[], &cancellation))
        .unwrap()
        .unwrap();

    let queued = queued.lock().unwrap();
    assert_eq!(queued.len(), 2);
    assert_eq!(queued[0][0].role, Role::User);
    assert_eq!(
        queued[0][0].parts,
        [MessagePart::Text(
            "[coordination source=main untrusted=true]\nfirst".into()
        )]
    );
    assert_eq!(
        queued[1][0].parts,
        [MessagePart::Text(
            "[coordination source=user untrusted=true]\nsecond".into()
        )]
    );
}

#[test]
fn p1c3_completed_background_subagent_notifies_the_next_main_turn() {
    let temporary = tui_session_directory("subagent-completion-notice");
    let bootstrap = tui_session_bootstrap(
        &temporary,
        &[(
            "reviewer",
            "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
        )],
    );
    let (events, _receiver) = BridgeTx::bounded(16);
    let controls = TuiTaskControls::default();
    let session = Arc::new(Mutex::new(SessionContext {
        selected_subagent: Some("reviewer".into()),
        ..SessionContext::fresh()
    }));
    let lifecycle_bridge = TuiTaskLifecycleBridge::new(events, controls.clone())
        .with_session_writer(bootstrap.clone(), Arc::clone(&session));
    let mut runtime = production_tui_task_runtime_with_runner(
        &bootstrap,
        &agens_bootstrap::session_root::discovered_root_for_tests(&bootstrap),
        &agens_tools::SkillCatalog::default(),
        Box::new(TuiPermissionPrompter(production_tui_permission_bridge().0)),
        ProductionTaskRunner::with_progress_probe(
            bootstrap.clone(),
            agens_bootstrap::session_root::discovered_root_for_tests(&bootstrap),
            Arc::new(Mutex::new(Vec::new())),
            Vec::new(),
        )
        .with_lifecycle_bridge(lifecycle_bridge),
    )
    .unwrap();
    runtime.authorized.gate.policy = PermissionPolicy::new(
        PermissionMode::Edit,
        vec![PermissionRule::global(
            PermissionDecision::Allow,
            PermissionPattern::Exact("native::task".into()),
            PermissionPattern::Any,
        )],
    );
    let cancellation = HeadlessTurnCancellation::new();
    let launched_at = current_session_timestamp();

    assert_eq!(
        launch_selected_tui_task(&mut runtime, &session, "review task", true, &cancellation),
        Ok(TuiSelectedTaskLaunch::Dispatched)
    );
    crate::test_support::wait_for(
        "a completed background subagent to persist one durable turn",
        || session.lock().unwrap().identifier,
    );

    let queued = Arc::new(Mutex::new(Vec::new()));
    let mut provider = TaskMailboxProvider::new(
        RecordingMailboxProvider {
            queued: Arc::clone(&queued),
        },
        Some(controls.0.clone()),
        TaskMessageTarget::Main,
    );
    // The notice is posted after the turn is persisted, so the identifier the
    // launch waits on is set strictly earlier. Drain until the notice lands
    // rather than assuming one drain is enough.
    crate::test_support::wait_for("the completed subagent's mailbox notice", || {
        block_on_headless_turn(provider.next_parts(&[], &cancellation))
            .unwrap()
            .unwrap();
        queued
            .lock()
            .unwrap()
            .iter()
            .any(|batch| !batch.is_empty())
            .then_some(())
    });
    // Draining again must add nothing: the notice is delivered once, which is
    // the property the old single-drain assertion was standing in for.
    block_on_headless_turn(provider.next_parts(&[], &cancellation))
        .unwrap()
        .unwrap();

    let queued = queued.lock().unwrap();
    let delivered = queued
        .iter()
        .filter(|batch| !batch.is_empty())
        .collect::<Vec<_>>();
    assert_eq!(delivered.len(), 1, "{queued:?}");
    assert_eq!(delivered[0].len(), 1);
    assert_eq!(delivered[0][0].role, Role::User);
    let [MessagePart::Text(notice)] = delivered[0][0].parts.as_slice() else {
        panic!("a mailbox notice is text: {:?}", delivered[0][0].parts)
    };
    let (label, detail) = notice
        .split_once('\n')
        .expect("mailbox notices are labelled untrusted");
    assert_eq!(label, "[coordination source=subagent:1 untrusted=true]");
    let completed_at = detail
        .split_once("completed_at=")
        .and_then(|(_, tail)| tail.split_whitespace().next())
        .and_then(|value| value.parse::<i64>().ok())
        .expect("the notice states when the subagent finished");
    assert!(completed_at >= launched_at);
    assert_eq!(
        detail,
        format!(
            "subagent #1 (reviewer) finished with state=completed completed_at={completed_at} \
             (unix seconds). The full result is recorded in this session history; run \
             task_control action=status id=1 for the recorded outcome."
        )
    );

    drop(queued);
    std::fs::remove_dir_all(temporary).unwrap();
}
