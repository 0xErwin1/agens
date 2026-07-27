//! Shared call counters used by the production TUI-resume and tool/provider
//! runtime tests, plus fixture helpers shared by more than one module's test
//! suite. Kept in one place so every consumer reaches a named function
//! instead of duplicating fixture setup or reaching across a module boundary
//! into a `thread_local!`.
#![cfg(test)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agens_core::{
    AgentDefinition, AgentMode, CompletedSessionTurn, CompletedTurnRepository,
    CompletedTurnSnapshot, Error as ToolError, HeadlessTurnCancellation, HeadlessTurnError,
    HeadlessTurnPortError, Message, MessagePart, PermissionDecision, PermissionMode,
    PermissionPattern, PermissionPolicy, PermissionRule, PermissionSession, Role, SessionMessage,
    SessionMetadata, ToolAccess, TurnEvent, TurnProgressSink, TurnProvider,
};
use agens_store::{PermissionGrantStore, SessionStore};
use agens_tools::{DispatchTool, ToolDispatcher, ToolExecutionContext, ToolOutput};
use agens_tui::{
    Action, BridgeCancel, BridgeTx, Event, Key, Tui, TuiRouteProgress, TuiRouteRequest,
    TuiRuntimeEvent,
};

use crate::CliDependencies;
use crate::bootstrap::{Bootstrap, bootstrap};
use crate::dispatch::ProductionToolDispatcher;
use crate::error::CliError;
use crate::headless::HeadlessChatCompletion;
use crate::permissions::{
    NativePermissionTarget, PermissionPromptAnswer, PermissionPrompter, ProductionPermissionGate,
    ProductionPermissionResolver, ProductionPromptAuthorization,
};
use crate::tui::engine::{ProductionTuiEngine, run_tui_prompt_with};
use crate::tui::metrics::{TuiMetricsPublisher, finish_tui_metrics};
use crate::tui::router::{TuiRuntimeRouter, tui_provider_outcome};

thread_local! {
    static TUI_RESUME_LOAD_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static TUI_RESUME_PROJECTION_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static PRODUCTION_TOOL_RUNTIME_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static PRODUCTION_PROVIDER_RUNTIME_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(crate) fn note_tui_resume_load() {
    TUI_RESUME_LOAD_CALLS.with(|calls| calls.set(calls.get() + 1));
}

pub(crate) fn note_tui_resume_projection() {
    TUI_RESUME_PROJECTION_CALLS.with(|calls| calls.set(calls.get() + 1));
}

pub(crate) fn note_production_tool_runtime() {
    PRODUCTION_TOOL_RUNTIME_CALLS.with(|calls| calls.set(calls.get() + 1));
}

pub(crate) fn note_production_provider_runtime() {
    PRODUCTION_PROVIDER_RUNTIME_CALLS.with(|calls| calls.set(calls.get() + 1));
}

pub(crate) fn reset_tui_resume_test_counters() {
    TUI_RESUME_LOAD_CALLS.with(|calls| calls.set(0));
    TUI_RESUME_PROJECTION_CALLS.with(|calls| calls.set(0));
    PRODUCTION_TOOL_RUNTIME_CALLS.with(|calls| calls.set(0));
    PRODUCTION_PROVIDER_RUNTIME_CALLS.with(|calls| calls.set(0));
}

pub(crate) fn tui_resume_test_counters() -> (usize, usize, usize, usize) {
    (
        TUI_RESUME_LOAD_CALLS.with(std::cell::Cell::get),
        TUI_RESUME_PROJECTION_CALLS.with(std::cell::Cell::get),
        PRODUCTION_TOOL_RUNTIME_CALLS.with(std::cell::Cell::get),
        PRODUCTION_PROVIDER_RUNTIME_CALLS.with(std::cell::Cell::get),
    )
}

/// Bootstraps a `Bootstrap` fixture from optional global/project TOML
/// fragments, isolated under a unique temporary directory named after
/// `label`. Shared by `bootstrap.rs`'s own tests and by test clusters in
/// other modules that need a configured `Bootstrap` without repeating its
/// setup.
pub(crate) fn bootstrap_from_configuration(
    label: &str,
    global: Option<&str>,
    project: Option<&str>,
) -> Bootstrap {
    let temporary = std::env::temp_dir().join(format!("agens-{label}-{}", std::process::id()));
    let config_home = temporary.join("config");
    let project_root = temporary.join("project");
    let mut files = BTreeMap::new();
    if let Some(global) = global {
        files.insert(config_home.join("config.toml"), global.to_owned());
    }
    if let Some(project) = project {
        files.insert(project_root.join(".agens/config.toml"), project.to_owned());
    }

    let dependencies = CliDependencies::for_test(
        project_root,
        Some(temporary.join("home")),
        BTreeMap::from([(
            "AGENS_CONFIG_HOME".to_owned(),
            config_home.display().to_string(),
        )]),
        files,
    );

    bootstrap(&dependencies).expect("configuration fixture should be valid")
}

/// A second bootstrap sharing `origin`'s data directory (and therefore its sessions
/// database) but discovering its own project root from a completely different, unrelated
/// working directory — simulating a process restart from elsewhere on disk.
pub(crate) fn bootstrap_from_a_different_working_directory(
    origin: &Path,
    label: &str,
) -> Bootstrap {
    let elsewhere = tui_session_directory(label);
    let config_home = origin.join("config");
    let data_directory = origin.join("data");
    bootstrap(&CliDependencies::for_test(
        elsewhere.join("project"),
        Some(elsewhere.join("home")),
        BTreeMap::from([(
            "AGENS_CONFIG_HOME".to_owned(),
            config_home.display().to_string(),
        )]),
        BTreeMap::from([(
            config_home.join("config.toml"),
            format!(
                "[provider]\ntype = \"openai-api\"\nmodel = \"gpt-4.1\"\n\n[options]\ndata_dir = \"{}\"\n",
                data_directory.display()
            ),
        )]),
    ))
    .unwrap()
}

/// A fresh, uniquely named temporary directory with a project marker
/// (`project/.git`) already created, isolating one test's filesystem state
/// from every other test running concurrently.
pub(crate) fn tui_session_directory(label: &str) -> PathBuf {
    let temporary = std::env::temp_dir().join(format!(
        "agens-tui-session-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(temporary.join("project/.git")).unwrap();
    temporary
}

/// A `Bootstrap` fixture wired for the OpenAI API provider, with the given
/// agent definitions written under the fixture's config directory.
pub(crate) fn tui_session_bootstrap(temporary: &Path, agents: &[(&str, &str)]) -> Bootstrap {
    tui_session_bootstrap_for_provider(temporary, agents, "openai-api", "gpt-4.1")
}

/// A `Bootstrap` fixture wired for the given provider and model, with the
/// given agent definitions written under the fixture's config directory.
pub(crate) fn tui_session_bootstrap_for_provider(
    temporary: &Path,
    agents: &[(&str, &str)],
    provider: &str,
    model: &str,
) -> Bootstrap {
    let config_home = temporary.join("config");
    let data_directory = temporary.join("data");
    let agents_directory = config_home.join("agents");
    std::fs::create_dir_all(&agents_directory).unwrap();
    for (name, contents) in agents {
        std::fs::write(agents_directory.join(format!("{name}.md")), contents).unwrap();
    }
    bootstrap(&CliDependencies::for_test(
        temporary.join("project"),
        Some(temporary.join("home")),
        BTreeMap::from([(
            "AGENS_CONFIG_HOME".to_owned(),
            config_home.display().to_string(),
        )]),
        BTreeMap::from([(
            config_home.join("config.toml"),
            format!(
                "[provider]\ntype = \"{provider}\"\nmodel = \"{model}\"\n\n[options]\ndata_dir = \"{}\"\n",
                data_directory.display()
            ),
        )]),
    ))
    .unwrap()
}

/// A native tool that records every path it acts on into a shared log and can
/// be told, per call, to inject a permission-evaluator failure or a
/// tool-execution failure. Backs the production-turn permission-batch
/// harness below.
pub(crate) struct BatchTool {
    pub(crate) name: String,
    pub(crate) calls: Arc<Mutex<Vec<String>>>,
    pub(crate) cancellation: Option<HeadlessTurnCancellation>,
}

impl DispatchTool for BatchTool {
    fn permission_target(
        &self,
        arguments: &serde_json::Value,
    ) -> Result<String, agens_core::Error> {
        if arguments
            .get("_inject_permission_evaluator_failure")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            return Err(agens_core::Error::Tool(
                "injected permission evaluator failure".into(),
            ));
        }

        NativePermissionTarget::parse(&self.name, arguments)
            .map(NativePermissionTarget::into_value)
            .map_err(|error| agens_core::Error::Tool(error.to_string()))
    }

    fn execute(
        &mut self,
        _: &ToolExecutionContext,
        arguments: serde_json::Value,
    ) -> Result<ToolOutput, agens_core::Error> {
        let path = self.permission_target(&arguments)?;
        self.calls
            .lock()
            .expect("tool calls should be available")
            .push(path.clone());
        if let Some(message) = arguments
            .get("_inject_tool_failure")
            .and_then(serde_json::Value::as_str)
        {
            return Ok(ToolOutput::failure(message));
        }
        if let Some(cancellation) = &self.cancellation {
            cancellation.cancel();
        }
        Ok(ToolOutput::success(format!("executed {path}")))
    }
}

struct BatchProvider {
    iterations: Vec<Result<Vec<MessagePart>, HeadlessTurnPortError>>,
}

impl TurnProvider for BatchProvider {
    fn next_parts(
        &mut self,
        _: &[TurnEvent],
        _: &HeadlessTurnCancellation,
    ) -> impl std::future::Future<Output = Result<Vec<MessagePart>, HeadlessTurnPortError>> + Send
    {
        std::future::ready(self.iterations.remove(0))
    }
}

struct BatchRepository {
    fail_persistence: bool,
}

impl CompletedTurnRepository for BatchRepository {
    fn persist_completed_turn(
        &mut self,
        _: CompletedTurnSnapshot,
    ) -> impl std::future::Future<Output = Result<(), agens_core::CompletedTurnStoreError>> + Send
    {
        if self.fail_persistence {
            std::future::ready(Err(agens_core::CompletedTurnStoreError::new(
                "database unavailable",
            )))
        } else {
            std::future::ready(Ok(()))
        }
    }
}

pub(crate) struct RecordingPrompt {
    pub(crate) answers: Vec<PermissionPromptAnswer>,
    pub(crate) calls: Arc<Mutex<Vec<String>>>,
}

impl PermissionPrompter for RecordingPrompt {
    fn prompt(
        &mut self,
        context: &agens_tools::PermissionPromptContext,
        _: &HeadlessTurnCancellation,
    ) -> Result<PermissionPromptAnswer, HeadlessTurnPortError> {
        self.calls
            .lock()
            .expect("prompt calls should be available")
            .push(context.target_identifier.clone());
        Ok(self.answers.remove(0))
    }
}

pub(crate) fn batch_call(id: &str, path: &str) -> MessagePart {
    MessagePart::ToolCall {
        id: id.into(),
        name: "native::read".into(),
        input: format!(r#"{{"path":"{path}"}}"#),
    }
}

pub(crate) fn native_batch_call(id: &str, name: &str, arguments: serde_json::Value) -> MessagePart {
    MessagePart::ToolCall {
        id: id.into(),
        name: name.into(),
        input: serde_json::to_string(&arguments).expect("native test arguments should encode"),
    }
}

fn batch_policy() -> PermissionPolicy {
    PermissionPolicy::new(
        PermissionMode::Edit,
        vec![PermissionRule::global(
            PermissionDecision::Ask,
            PermissionPattern::Exact("native::read".into()),
            PermissionPattern::Any,
        )],
    )
}

/// The full outcome of one production-turn permission batch: the headless
/// turn result plus every prompt, tool execution, turn-progress event, and
/// TUI metrics envelope it produced. Shared by the permission-gate,
/// dispatcher, and TUI-metrics test clusters that all drive the same
/// production headless-turn wiring end to end.
pub(crate) struct BatchOutcome {
    pub(crate) result: Result<CompletedTurnSnapshot, HeadlessTurnError>,
    pub(crate) prompts: Vec<String>,
    pub(crate) executions: Vec<String>,
    pub(crate) progress: Vec<TurnEvent>,
    pub(crate) metrics: Vec<TuiRuntimeEvent>,
}

pub(crate) struct ProductionBatchInput<'a> {
    directory_name: &'a str,
    answers: Vec<PermissionPromptAnswer>,
    calls: Vec<MessagePart>,
    cancellation: Option<HeadlessTurnCancellation>,
    provider_error: Option<HeadlessTurnPortError>,
    fail_persistence: bool,
    policy: PermissionPolicy,
    bypass: bool,
    dangerous_override: bool,
}

impl<'a> ProductionBatchInput<'a> {
    pub(crate) fn new(
        directory_name: &'a str,
        answers: Vec<PermissionPromptAnswer>,
        calls: Vec<MessagePart>,
    ) -> Self {
        Self {
            directory_name,
            answers,
            calls,
            cancellation: None,
            provider_error: None,
            fail_persistence: false,
            policy: batch_policy(),
            bypass: false,
            dangerous_override: false,
        }
    }

    pub(crate) fn with_runtime(
        mut self,
        cancellation: Option<HeadlessTurnCancellation>,
        provider_error: Option<HeadlessTurnPortError>,
        fail_persistence: bool,
    ) -> Self {
        self.cancellation = cancellation;
        self.provider_error = provider_error;
        self.fail_persistence = fail_persistence;
        self
    }

    pub(crate) fn with_policy(mut self, policy: PermissionPolicy) -> Self {
        self.policy = policy;
        self
    }

    pub(crate) fn with_bypass(mut self) -> Self {
        self.bypass = true;
        self
    }

    pub(crate) fn with_dangerous_override(mut self) -> Self {
        self.dangerous_override = true;
        self
    }
}

fn run_ready<T>(
    future: impl std::future::Future<Output = Result<T, HeadlessTurnError>>,
) -> Result<T, HeadlessTurnError> {
    let mut future = std::pin::pin!(future);
    let context = &mut std::task::Context::from_waker(std::task::Waker::noop());

    match future.as_mut().poll(context) {
        std::task::Poll::Ready(result) => result,
        std::task::Poll::Pending => panic!("batch ports must complete synchronously"),
    }
}

/// Drives one production headless turn through the real permission gate,
/// resolver, and tool dispatcher wiring, using the given directory name to
/// isolate the grant store, and returns everything the batch produced.
pub(crate) fn run_production_batch(
    directory_name: &str,
    answers: Vec<PermissionPromptAnswer>,
    calls: Vec<MessagePart>,
    cancellation: Option<HeadlessTurnCancellation>,
    provider_error: Option<HeadlessTurnPortError>,
    fail_persistence: bool,
) -> BatchOutcome {
    run_production_batch_with_policy(
        ProductionBatchInput::new(directory_name, answers, calls).with_runtime(
            cancellation,
            provider_error,
            fail_persistence,
        ),
    )
}

pub(crate) fn run_production_batch_with_policy(input: ProductionBatchInput<'_>) -> BatchOutcome {
    let ProductionBatchInput {
        directory_name,
        answers,
        calls,
        cancellation,
        provider_error,
        fail_persistence,
        policy,
        bypass,
        dangerous_override,
    } = input;
    let directory =
        std::env::temp_dir().join(format!("agens-{directory_name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let executions = Arc::new(Mutex::new(Vec::new()));
    let dispatcher = Arc::new(Mutex::new(ToolDispatcher::new()));
    let mut dispatcher_guard = dispatcher.lock().expect("dispatcher should be available");
    for name in [
        "native::read",
        "native::list",
        "native::glob",
        "native::grep",
        "native::webfetch",
        "native::write",
    ] {
        dispatcher_guard
            .register_native(
                name,
                if name == "native::write" {
                    agens_core::ToolAccess::Write
                } else {
                    agens_core::ToolAccess::ReadOnly
                },
                BatchTool {
                    name: name.into(),
                    calls: Arc::clone(&executions),
                    cancellation: cancellation.clone(),
                },
            )
            .expect("batch tool should register");
    }
    drop(dispatcher_guard);
    let grants = Arc::new(Mutex::new(Vec::new()));
    let allowed = Arc::new(Mutex::new(BTreeMap::new()));
    let pending_prompts = Arc::new(Mutex::new(BTreeMap::new()));
    let mut gate = ProductionPermissionGate::new(
        policy.clone(),
        Arc::clone(&grants),
        if bypass {
            PermissionSession::with_temporary_bypass()
        } else {
            PermissionSession::new()
        },
        "project".into(),
        Arc::clone(&dispatcher),
        Arc::clone(&allowed),
        Arc::clone(&pending_prompts),
    )
    .with_dangerous_override(dangerous_override);
    let mut resolver = ProductionPermissionResolver::new(
        RecordingPrompt {
            answers,
            calls: Arc::clone(&prompts),
        },
        PermissionGrantStore::open(&directory).expect("grant store should open"),
        grants,
        pending_prompts,
        ProductionPromptAuthorization {
            policy,
            session: PermissionSession::new(),
            project: "project".into(),
            dispatcher: Arc::clone(&dispatcher),
            allowed: Arc::clone(&allowed),
        },
    );
    let mut tool_dispatcher = ProductionToolDispatcher::new(dispatcher, allowed);
    let mut provider = BatchProvider {
        iterations: provider_error
            .map(Err)
            .into_iter()
            .chain(std::iter::once(Ok(calls)))
            .chain(std::iter::once(Ok(vec![MessagePart::Text(
                "complete".into(),
            )])))
            .collect(),
    };
    let progress_events = Arc::new(Mutex::new(Vec::new()));
    let (metrics_sender, metrics_receiver) = BridgeTx::bounded(16);
    let metrics = Arc::new(Mutex::new(TuiMetricsPublisher::new(
        metrics_sender,
        BridgeCancel::new(),
        "test-model",
    )));
    let progress: TurnProgressSink = {
        let progress_events = Arc::clone(&progress_events);
        let metrics = Arc::clone(&metrics);
        Arc::new(move |event| {
            metrics.lock().unwrap().observe(&event);
            progress_events.lock().unwrap().push(event);
        })
    };
    let cancellation = cancellation.unwrap_or_default();
    let result = run_ready(agens_core::run_headless_turn_with_progress(
        &mut provider,
        &mut gate,
        &mut resolver,
        &mut tool_dispatcher,
        &mut BatchRepository { fail_persistence },
        &cancellation,
        Some(&progress),
        None,
    ));
    let terminal = result
        .as_ref()
        .map(|_| ())
        .map_err(|error| CliError::runtime(*error));
    finish_tui_metrics(&metrics, &terminal);
    std::fs::remove_dir_all(&directory).expect("temporary grant directory should be removed");

    BatchOutcome {
        result,
        prompts: prompts.lock().unwrap().clone(),
        executions: executions.lock().unwrap().clone(),
        progress: progress_events.lock().unwrap().clone(),
        metrics: std::iter::from_fn(|| metrics_receiver.try_recv().ok())
            .map(|envelope| envelope.into_parts().1)
            .collect(),
    }
}

pub(crate) fn render_tui_test_backend(
    tui: &Tui<ProductionTuiEngine>,
    width: u16,
    height: u16,
) -> String {
    let terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
    let mut renderer = agens_tui::RatatuiRenderer::new(terminal);
    agens_tui::Renderer::render(&mut renderer, tui.view()).unwrap();
    renderer
        .terminal()
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

pub(crate) struct RotationTool;

impl DispatchTool for RotationTool {
    fn execute(
        &mut self,
        _: &ToolExecutionContext,
        _: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::success("unused"))
    }
}

pub(crate) fn rotation_agent(name: &str, model: Option<&str>, allow_read: bool) -> AgentDefinition {
    AgentDefinition {
        name: name.into(),
        description: format!("{name} agent"),
        mode: AgentMode::Primary,
        model: model.map(str::to_owned),
        system_prompt: format!("You are {name}."),
        permission_rules: allow_read
            .then(|| {
                PermissionRule::global(
                    PermissionDecision::Allow,
                    PermissionPattern::glob("native::read").unwrap(),
                    PermissionPattern::Any,
                )
            })
            .into_iter()
            .collect(),
        skills: Vec::new(),
    }
}

pub(crate) fn rotation_dispatcher() -> ToolDispatcher {
    let mut dispatcher = ToolDispatcher::new();
    dispatcher
        .register_native("native::read", ToolAccess::ReadOnly, RotationTool)
        .unwrap();
    dispatcher
}

pub(crate) fn enter_tui_input(tui: &mut Tui<ProductionTuiEngine>, input: &str) -> String {
    for character in input.chars() {
        tui.handle(agens_tui::Event::Key(agens_tui::Key::Char(character)));
    }
    let agens_tui::Action::Submit(input) = tui.handle(agens_tui::Event::Key(agens_tui::Key::Enter))
    else {
        panic!("Enter should submit through the production TUI path");
    };
    input
}

pub(crate) fn tui_project(temporary: &Path) -> String {
    temporary.join("project").display().to_string()
}

pub(crate) fn tui_session_messages() -> Vec<Message> {
    vec![
        Message {
            role: Role::User,
            parts: vec![MessagePart::Text("previous request".into())],
        },
        Message {
            role: Role::Assistant,
            parts: vec![
                MessagePart::Reasoning("previous reasoning".into()),
                MessagePart::ToolCall {
                    id: "resume-call".into(),
                    name: "read".into(),
                    input: "{}".into(),
                },
                MessagePart::Text("previous answer".into()),
            ],
        },
        Message {
            role: Role::Tool,
            parts: vec![MessagePart::ToolResult {
                tool_call_id: "resume-call".into(),
                content: "previous result".into(),
                is_error: false,
            }],
        },
    ]
}

pub(crate) fn persist_tui_session(
    store: &mut SessionStore,
    project: &str,
    title: &str,
) -> SessionMetadata {
    let turn = CompletedSessionTurn::new(
        tui_session_messages()
            .into_iter()
            .map(SessionMessage::try_from)
            .collect::<Result<_, _>>()
            .unwrap(),
    )
    .unwrap();
    store
        .persist_completed_session_turn(
            &SessionMetadata {
                id: 0,
                project: project.into(),
                title: title.into(),
                active_agent: "primary".into(),
                provider_id: None,
                model_id: None,
                reasoning_effort: None,
                created_at: 1,
                updated_at: 1,
                completed_turn_count: 0,
                resumable: false,
            },
            &turn,
        )
        .unwrap()
}

pub(crate) fn persist_tui_session_metadata(
    store: &mut SessionStore,
    project: &str,
    title: &str,
    active_agent: &str,
    updated_at: i64,
) -> SessionMetadata {
    let mut metadata = persist_tui_session(store, project, title);
    metadata.active_agent = active_agent.into();
    metadata.updated_at = updated_at;
    store.update_session(&metadata).unwrap();
    metadata
}

pub(crate) fn open_tui_palette_dialog(
    tui: &mut Tui<ProductionTuiEngine>,
    router: &TuiRuntimeRouter,
    prefix: &str,
    expected_route: &str,
    progress: std::sync::mpsc::Sender<TuiRouteProgress>,
) {
    for character in prefix.chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }
    let Action::OpenDialog(route_id) = tui.handle(Event::Key(Key::Enter)) else {
        panic!("palette Enter should open a dialog");
    };
    assert_eq!(route_id, expected_route);
    let outcome = router.route_request(TuiRouteRequest::OpenDialog(route_id), progress);
    assert!(tui.apply_submission_outcome(outcome).is_none());
}

pub(crate) fn dispatch_tui_dialog_selection(
    tui: &mut Tui<ProductionTuiEngine>,
    router: &TuiRuntimeRouter,
    progress: std::sync::mpsc::Sender<TuiRouteProgress>,
) {
    let Action::DialogAction(action_id) = tui.handle(Event::Key(Key::Enter)) else {
        panic!("dialog Enter should dispatch an action");
    };
    let outcome = router.route_request(TuiRouteRequest::DialogAction(action_id), progress);
    assert!(tui.apply_submission_outcome(outcome).is_none());
}

pub(crate) fn submit_tui_command(
    tui: &mut Tui<ProductionTuiEngine>,
    router: &TuiRuntimeRouter,
    bootstrap: &Bootstrap,
    input: &str,
    captured: &Arc<Mutex<Vec<crate::headless::HeadlessChatRequest>>>,
) {
    let input = enter_tui_input(tui, input);
    let Some(prompt) = tui.apply_submission_outcome(router.route(input)) else {
        return;
    };
    let result = run_tui_prompt_with(
        bootstrap,
        &prompt,
        &router.session,
        Some(router.skills().unwrap()),
        {
            let captured = Arc::clone(captured);
            move |request| {
                captured.lock().unwrap().push(request);
                Ok(HeadlessChatCompletion {
                    text: "captured".into(),
                    metadata: SessionMetadata {
                        id: 1,
                        project: "project".into(),
                        title: "captured".into(),
                        active_agent: "build".into(),
                        provider_id: None,
                        model_id: None,
                        reasoning_effort: None,
                        created_at: 1,
                        updated_at: 1,
                        completed_turn_count: 1,
                        resumable: true,
                    },
                    messages: Vec::new(),
                })
            }
        },
    );
    tui.finish_provider_turn(tui_provider_outcome(result));
}
