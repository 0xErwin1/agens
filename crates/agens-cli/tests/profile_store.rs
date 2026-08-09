use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agens::profile_store::{AgentProfileStore, ProfileScope};
use agens_agents::{AgentProfileResolver, TaskModelValidator};
use agens_bootstrap::session_config::ScopedAgentProfiles;
use agens_config::{AgentProfilePatch, parse_agent_profiles, parse_toml_document};
use agens_core::{ReasoningEffort, RequestConfig};
use agens_tools::{
    AgentCatalog, AgentModelValidationError, AgentModelValidator, DispatchTool, SkillCatalog,
    TaskRunContext, TaskRunner, TaskRunnerError, TaskTool, TaskTurnRequest, TaskTurnResult,
    ToolExecutionContext,
};

type CapturedDelegations = Vec<(String, Option<ReasoningEffort>)>;

struct AnyModel;

impl AgentModelValidator for AnyModel {
    fn validate_model(&self, _: &str) -> Result<(), AgentModelValidationError> {
        Ok(())
    }
}

struct CapturingRunner(Arc<Mutex<CapturedDelegations>>);

impl TaskRunner for CapturingRunner {
    fn run(
        &self,
        request: TaskTurnRequest,
        _: &TaskRunContext,
    ) -> Result<TaskTurnResult, TaskRunnerError> {
        self.0.lock().unwrap().push((
            request.model().to_owned(),
            request.request_config().reasoning_effort(),
        ));
        Ok(TaskTurnResult {
            output: "delegated".to_owned(),
        })
    }
}

/// A fresh root for one test.
///
/// The key carries a timestamp and a per-process sequence on top of the PID
/// because PIDs wrap: two concurrent runs can share one, and a root that only a
/// PID distinguishes would then be the same directory. Cleaning on entry is not
/// a substitute — on a reused PID it deletes the other run's live state.
fn temporary_directory(label: &str) -> std::path::PathBuf {
    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    let path = std::env::temp_dir().join(format!(
        "agens-profile-store-{label}-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the system clock must be after the Unix epoch")
            .as_nanos(),
        NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
    ));

    fs::create_dir_all(&path).expect("temporary directory must be created");
    path
}

#[test]
fn saves_to_the_selected_scope_and_creates_missing_profile_tables() {
    let root = temporary_directory("scope");
    let global = root.join("global/config.toml");
    let project = root.join("project/.agens/config.toml");
    fs::create_dir_all(global.parent().expect("global parent")).expect("global parent must exist");
    fs::create_dir_all(project.parent().expect("project parent"))
        .expect("project parent must exist");
    fs::write(
        &global,
        "# global comment\n[provider]\nmodel = \"global-model\"\n",
    )
    .expect("global fixture must be written");
    fs::write(&project, "[provider]\nmodel = \"project-model\"\n")
        .expect("project fixture must be written");

    let store = AgentProfileStore::new(global.clone(), project.clone());
    let snapshot = store
        .read(ProfileScope::Global)
        .expect("global snapshot must load");
    store
        .save(
            ProfileScope::Global,
            &snapshot,
            "explore",
            &AgentProfilePatch {
                model: Some(Some("gpt-5".to_owned())),
                effort: None,
            },
        )
        .expect("global profile must save");

    assert_eq!(
        fs::read_to_string(&global).expect("global config must remain readable"),
        "# global comment\n[provider]\nmodel = \"global-model\"\n\n[agents.explore]\nmodel = \"gpt-5\"\n"
    );
    assert_eq!(
        fs::read_to_string(&project).expect("project config must remain unchanged"),
        "[provider]\nmodel = \"project-model\"\n"
    );

    fs::remove_dir_all(root).expect("temporary directory must be removed");
}

#[test]
fn creates_new_profile_files_with_private_permissions() {
    let root = temporary_directory("permissions");
    let global = root.join("global/config.toml");
    let project = root.join("project/.agens/config.toml");
    let store = AgentProfileStore::new(global, project.clone());
    let snapshot = store
        .read(ProfileScope::Project)
        .expect("missing snapshot must load");

    store
        .save(
            ProfileScope::Project,
            &snapshot,
            "explore",
            &AgentProfilePatch {
                model: Some(Some("gpt-5".to_owned())),
                effort: None,
            },
        )
        .expect("project profile must save");

    assert_eq!(
        fs::metadata(&project)
            .expect("project config must exist")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    fs::remove_dir_all(root).expect("temporary directory must be removed");
}

#[test]
fn saves_then_freshly_resolves_and_delegates_the_profile_model() {
    let root = temporary_directory("save-resolve-delegate");
    let global = root.join("global/config.toml");
    let project = root.join("project/.agens/config.toml");
    fs::create_dir_all(global.parent().expect("global parent")).expect("global parent must exist");
    fs::write(&global, "[provider]\nmodel = \"session-model\"\n").expect("fixture must be written");

    let store = AgentProfileStore::new(global.clone(), project);
    let snapshot = store
        .read(ProfileScope::Global)
        .expect("snapshot must load");
    store
        .save(
            ProfileScope::Global,
            &snapshot,
            "worker",
            &AgentProfilePatch {
                model: Some(Some("worker-model".to_owned())),
                effort: None,
            },
        )
        .expect("profile must save");

    let document =
        parse_toml_document(&fs::read_to_string(&global).expect("saved config must be readable"))
            .expect("saved config must parse");
    let profiles = ScopedAgentProfiles::new(
        parse_agent_profiles(&document).expect("saved profile must validate"),
        Default::default(),
    );
    let resolved = AgentProfileResolver::new(&profiles).resolve(
        "worker",
        None,
        None,
        "session-model",
        Some(ReasoningEffort::High),
    );
    assert_eq!(resolved.model.value, "worker-model");
    assert_eq!(resolved.effort.value, None);

    let agents_root = root.join("agents");
    fs::create_dir_all(&agents_root).expect("agents directory must be created");
    fs::write(
        agents_root.join("worker.md"),
        "---\nname: worker\ndescription: worker\nmode: subagent\n---\nworker instructions\n",
    )
    .expect("agent fixture must be written");
    let missing = root.join("missing");
    let agents = AgentCatalog::discover(&[], &agents_root, &missing)
        .expect("agent catalog must load")
        .catalog()
        .clone()
        .map_agents(|agent| {
            let mut agent = agent.clone();
            agent.model = Some(resolved.model.value.clone());
            agent.reasoning_effort = resolved.effort.value;
            agent
        });
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut task = TaskTool::from_catalogs_with_parent_config(
        agents,
        SkillCatalog::default(),
        "session-model",
        RequestConfig::with_reasoning_effort_value(ReasoningEffort::High),
        vec!["session-model".to_owned(), "worker-model".to_owned()],
        AnyModel,
        CapturingRunner(Arc::clone(&calls)),
    );
    let output = task
        .execute(
            &ToolExecutionContext::with_timeout(Duration::from_secs(1)),
            serde_json::json!({"agent":"worker","description":"delegate"}),
        )
        .expect("delegation must execute");

    assert_eq!(output.content, "delegated");
    assert_eq!(
        *calls.lock().unwrap(),
        vec![("worker-model".to_owned(), None)]
    );

    fs::remove_dir_all(root).expect("temporary directory must be removed");
}

#[test]
fn fixing_an_unavailable_stored_profile_removes_the_fallback_warning() {
    let root = temporary_directory("fix-unavailable");
    let global = root.join("global/config.toml");
    let project = root.join("project/.agens/config.toml");
    fs::create_dir_all(global.parent().expect("global parent")).expect("global parent must exist");
    fs::write(
        &global,
        "[provider]\nmodel = \"session-model\"\n\n[agents.worker]\nmodel = \"unavailable-model\"\n",
    )
    .expect("fixture must be written");
    let store = AgentProfileStore::new(global.clone(), project);
    let resolve = || {
        let document = parse_toml_document(
            &fs::read_to_string(&global).expect("saved config must be readable"),
        )
        .expect("saved config must parse");
        AgentProfileResolver::new(&ScopedAgentProfiles::new(
            parse_agent_profiles(&document).expect("saved profile must validate"),
            Default::default(),
        ))
        .resolve(
            "worker",
            None,
            None,
            "session-model",
            Some(ReasoningEffort::High),
        )
    };
    let delegate = |model: String, diagnostics: Arc<Mutex<Vec<()>>>| {
        let agents_root = root.join("agents");
        fs::create_dir_all(&agents_root).expect("agents directory must be created");
        fs::write(
            agents_root.join("worker.md"),
            "---\nname: worker\ndescription: worker\nmode: subagent\n---\nworker instructions\n",
        )
        .expect("agent fixture must be written");
        let missing = root.join("missing");
        let agents = AgentCatalog::discover(&[], &agents_root, &missing)
            .expect("agent catalog must load")
            .catalog()
            .clone()
            .map_agents(|agent| {
                let mut agent = agent.clone();
                agent.model = Some(model.clone());
                agent
            });
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut task = TaskTool::from_catalogs_with_parent_config(
            agents,
            SkillCatalog::default(),
            "session-model",
            RequestConfig::with_reasoning_effort_value(ReasoningEffort::High),
            vec!["session-model".to_owned(), "worker-model".to_owned()],
            TaskModelValidator::new(&["session-model".to_owned(), "worker-model".to_owned()]),
            CapturingRunner(Arc::clone(&calls)),
        )
        .with_model_resolution_diagnostics(move |_| {
            diagnostics.lock().unwrap().push(());
            Some("abc12345".to_owned())
        });
        let output = task
            .execute(
                &ToolExecutionContext::with_timeout(Duration::from_secs(1)),
                serde_json::json!({"agent":"worker","description":"delegate"}),
            )
            .expect("delegation must execute");
        (output, calls)
    };

    let diagnostics = Arc::new(Mutex::new(Vec::new()));
    let unavailable = resolve();
    assert_eq!(unavailable.model.value, "unavailable-model");
    let (unavailable_output, unavailable_calls) =
        delegate(unavailable.model.value, Arc::clone(&diagnostics));
    assert_eq!(
        unavailable_output.content,
        "warning: agent worker requested unavailable model unavailable-model; using session-model [ref: abc12345]\ndelegated"
    );
    assert_eq!(
        *unavailable_calls.lock().unwrap(),
        vec![("session-model".to_owned(), Some(ReasoningEffort::High))]
    );
    assert_eq!(diagnostics.lock().unwrap().len(), 1);

    let snapshot = store
        .read(ProfileScope::Global)
        .expect("stored profile snapshot must load");
    store
        .save(
            ProfileScope::Global,
            &snapshot,
            "worker",
            &AgentProfilePatch {
                model: Some(Some("worker-model".to_owned())),
                effort: None,
            },
        )
        .expect("overlay-equivalent patch must save");

    let fixed = resolve();
    assert_eq!(fixed.model.value, "worker-model");
    let (fixed_output, fixed_calls) = delegate(fixed.model.value, Arc::clone(&diagnostics));
    assert_eq!(fixed_output.content, "delegated");
    assert_eq!(
        *fixed_calls.lock().unwrap(),
        vec![("worker-model".to_owned(), None)]
    );
    assert_eq!(diagnostics.lock().unwrap().len(), 1);

    fs::remove_dir_all(root).expect("temporary directory must be removed");
}

#[test]
fn leaves_the_original_file_intact_when_validation_fails_before_write() {
    let root = temporary_directory("validation-failure");
    let global = root.join("global/config.toml");
    let project = root.join("project/.agens/config.toml");
    fs::create_dir_all(global.parent().expect("global parent")).expect("global parent must exist");
    let original = "# preserve\n[provider]\nmodel = \"before\"\n";
    fs::write(&global, original).expect("fixture must be written");

    let store = AgentProfileStore::new(global.clone(), project);
    let snapshot = store
        .read(ProfileScope::Global)
        .expect("snapshot must load");
    let error = store
        .save(
            ProfileScope::Global,
            &snapshot,
            "explore",
            &AgentProfilePatch {
                model: None,
                effort: Some(Some("ludicrous".to_owned())),
            },
        )
        .expect_err("invalid replacement must be rejected before write");

    assert_eq!(
        error.to_string(),
        "invalid configuration field agents.explore.effort; allowed values: none, minimal, low, medium, high, xhigh, max"
    );
    assert_eq!(
        fs::read_to_string(&global).expect("original config must remain readable"),
        original
    );

    fs::remove_dir_all(root).expect("temporary directory must be removed");
}

#[test]
fn rejects_a_concurrent_change_without_replacing_the_original_file() {
    let root = temporary_directory("cas");
    let global = root.join("global/config.toml");
    let project = root.join("project/.agens/config.toml");
    fs::create_dir_all(global.parent().expect("global parent")).expect("global parent must exist");
    fs::write(&global, "[provider]\nmodel = \"before\"\n").expect("fixture must be written");

    let store = AgentProfileStore::new(global.clone(), project);
    let snapshot = store
        .read(ProfileScope::Global)
        .expect("snapshot must load");
    fs::write(&global, "[provider]\nmodel = \"external-change\"\n")
        .expect("concurrent change must be written");

    let error = store
        .save(
            ProfileScope::Global,
            &snapshot,
            "explore",
            &AgentProfilePatch {
                model: Some(Some("gpt-5".to_owned())),
                effort: None,
            },
        )
        .expect_err("concurrent modification must be rejected");

    assert_eq!(error.to_string(), "profile config changed concurrently");
    assert_eq!(
        fs::read_to_string(&global).expect("externally changed config must remain readable"),
        "[provider]\nmodel = \"external-change\"\n"
    );

    fs::remove_dir_all(root).expect("temporary directory must be removed");
}
