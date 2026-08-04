//! Tests for the terminal surface that go through the command line to get
//! there: they build a request from parsed `chat` arguments or resolve a
//! `Bootstrap` from `CliDependencies`.
//!
//! Everything else about the surface is tested inside `agens-tui-app`. These
//! stay because what they exercise is the seam between the two.
#![cfg(test)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use agens_agents::ensure_active_agent_runtime;
use agens_bootstrap::Bootstrap;
use agens_core::HeadlessTurnCancellation;
use agens_fixtures::{
    session_bootstrap as tui_session_bootstrap,
    session_bootstrap_for_provider as tui_session_bootstrap_for_provider,
    session_directory as tui_session_directory,
};
use agens_session::context::SessionContext;
use agens_session::provider::CredentialResolver;
use agens_store::PreferenceStore;
use agens_store::SessionStore;
use agens_tools::{CommandCatalog, SkillCatalog};
use agens_tui::Tui;
use agens_tui::{TuiPresentation, TuiSubmissionOutcome};
use agens_tui_app::engine::{
    ProductionTuiEngine, configure_tui_project_identity, run_tui_prompt, tui_turn_system_prompt,
};
use agens_tui_app::models::seed_remembered_tui_selection;
use agens_tui_app::resume::resume_tui_session;
use agens_tui_app::router::TuiRuntimeRouter;
use agens_tui_app::test_support::{
    persist_tui_session, persist_tui_session_metadata, render_tui_test_backend,
    rotation_dispatcher, tui_project,
};

use crate::CliDependencies;
use crate::commands::chat::{chat_args_with_prompt, chat_request};
use crate::deps::bootstrap;

fn remember(bootstrap: &Bootstrap, model: &str, effort: Option<agens_core::ReasoningEffort>) {
    PreferenceStore::open(bootstrap.data_directory())
        .unwrap()
        .remember_model(
            agens_models::ModelSource::OpenAiApi.storage_key(),
            &agens_store::ModelPreference::new(model, effort),
        )
        .unwrap();
}

#[test]
fn a_new_session_inherits_the_remembered_model_and_its_effort() {
    let temporary = tui_session_directory("remembered-selection-fresh");
    let mut bootstrap = tui_session_bootstrap(&temporary, &[]);
    bootstrap.model = None;
    remember(
        &bootstrap,
        "gpt-5.5",
        Some(agens_core::ReasoningEffort::High),
    );
    let mut context = SessionContext::fresh();

    assert_eq!(
        seed_remembered_tui_selection(&bootstrap, &mut context),
        None
    );

    assert_eq!(
        agens_session::model::effective_model(&bootstrap, &context),
        "gpt-5.5"
    );
    let request = agens_headless::apply_session_to_request(
        &context,
        chat_request(chat_args_with_prompt("work")).unwrap(),
    );
    assert_eq!(request.model.as_deref(), Some("gpt-5.5"));
    assert_eq!(
        request.session_reasoning_effort,
        Some(agens_core::ReasoningEffort::High)
    );

    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn resumed_primary_inherits_every_effective_pinned_model_and_compatible_effort() {
    for provider in ["openai-api", "openai-chatgpt"] {
        for model in [
            "gpt-5.5",
            "gpt-5.6",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
        ] {
            let temporary = tui_session_directory(&format!("resume-primary-{provider}-{model}"));
            let bootstrap = tui_session_bootstrap_for_provider(&temporary, &[], provider, model);
            let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
            let mut metadata =
                persist_tui_session(&mut store, &tui_project(&temporary), "inherited");
            metadata.provider_id = Some(provider.into());
            metadata.model_id = Some(model.into());
            metadata.reasoning_effort = Some(agens_core::ReasoningEffort::High);
            store.update_session_selection(&metadata).unwrap();
            drop(store);

            let resumed = resume_tui_session(
                &bootstrap,
                metadata.id,
                &SkillCatalog::default(),
                &CredentialResolver::production(),
            )
            .unwrap()
            .context;
            assert!(resumed.active_agent.is_none());
            let session = Arc::new(Mutex::new(resumed));
            let dispatcher = Arc::new(Mutex::new(rotation_dispatcher()));

            ensure_active_agent_runtime(&bootstrap, &session, &dispatcher).unwrap();

            let context = session.lock().unwrap();
            let active = context.active_agent.as_ref().unwrap();
            assert_eq!(active.name, "primary", "{provider} {model}");
            assert_eq!(active.model.as_deref(), Some(model), "{provider} {model}");
            let request = agens_headless::apply_session_to_request(
                &context,
                chat_request(chat_args_with_prompt("first submission")).unwrap(),
            );
            assert_eq!(request.model.as_deref(), Some(model), "{provider} {model}");
            assert_eq!(
                request.request_config.reasoning_effort(),
                Some(agens_core::ReasoningEffort::High),
                "{provider} {model}"
            );
            drop(context);

            std::fs::remove_dir_all(temporary).unwrap();
        }
    }
}

#[test]
fn explicit_agent_models_use_the_provider_aware_effective_registry() {
    for (provider, model, expected_effort) in [
        ("openai-api", "gpt-4o", None),
        ("openai-chatgpt", "gpt-5.4", None),
        ("openai-api", "gpt-5.6-luna", None),
        ("openai-chatgpt", "gpt-5.6-luna", None),
        (
            "openai-api",
            "gpt-5.5",
            Some(agens_core::ReasoningEffort::High),
        ),
        (
            "openai-chatgpt",
            "gpt-5.5",
            Some(agens_core::ReasoningEffort::High),
        ),
    ] {
        let temporary = tui_session_directory(&format!("explicit-{provider}-{model}"));
        let definition = format!(
            "---\nname: reviewer\ndescription: reviewer\nmode: primary\nmodel: {model}\npermissions: []\n---\nReview.\n"
        );
        let bootstrap = tui_session_bootstrap_for_provider(
            &temporary,
            &[("reviewer", &definition)],
            provider,
            "gpt-5.5",
        );
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let mut metadata = persist_tui_session_metadata(
            &mut store,
            &tui_project(&temporary),
            "explicit",
            "reviewer",
            100,
        );
        metadata.provider_id = Some(provider.into());
        metadata.model_id = Some("gpt-5.5".into());
        metadata.reasoning_effort = Some(agens_core::ReasoningEffort::High);
        store.update_session_selection(&metadata).unwrap();
        drop(store);
        let resumed = resume_tui_session(
            &bootstrap,
            metadata.id,
            &SkillCatalog::default(),
            &CredentialResolver::production(),
        )
        .unwrap()
        .context;
        let session = Arc::new(Mutex::new(resumed));

        ensure_active_agent_runtime(
            &bootstrap,
            &session,
            &Arc::new(Mutex::new(rotation_dispatcher())),
        )
        .unwrap();

        let context = session.lock().unwrap();
        assert_eq!(context.active_agent.as_ref().unwrap().name, "reviewer");
        assert_eq!(
            context.active_agent.as_ref().unwrap().model.as_deref(),
            Some(model),
            "{provider} {model}"
        );
        let request = agens_headless::apply_session_to_request(
            &context,
            chat_request(chat_args_with_prompt("review")).unwrap(),
        );
        assert_eq!(request.model.as_deref(), Some(model), "{provider} {model}");
        assert_eq!(
            request.request_config.reasoning_effort(),
            expected_effort,
            "{provider} {model}"
        );
        drop(context);
        std::fs::remove_dir_all(temporary).unwrap();
    }
}

#[test]
fn production_tui_project_identity_uses_the_canonical_current_project_for_new_and_resumed_sessions()
{
    let temporary =
        std::env::temp_dir().join(format!("agens-u18-project-header-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&temporary);
    let project_root = temporary.join("non-agens-project");
    std::fs::create_dir_all(project_root.join(".git")).unwrap();
    let config_home = temporary.join("config");
    let project_bootstrap = bootstrap(&CliDependencies::for_test(
        project_root.clone(),
        Some(temporary.join("home")),
        BTreeMap::from([(
            "AGENS_CONFIG_HOME".to_owned(),
            config_home.display().to_string(),
        )]),
        BTreeMap::from([(
            config_home.join("config.toml"),
            format!(
                "[options]\ndata_dir = \"{}\"\n",
                temporary.join("data").display()
            ),
        )]),
    ))
    .unwrap();
    let project = project_root.display().to_string();
    let mut tui = Tui::new(ProductionTuiEngine {
        cancellation: Arc::new(Mutex::new(None)),
    });

    configure_tui_project_identity(&mut tui, &project_bootstrap);
    assert_eq!(tui.view().project, project);
    tui.set_presentation("openai-api", "gpt-4.1", "new session");
    let new_session_header = render_tui_test_backend(&tui, 120, 24);
    assert!(
        new_session_header.contains("non-agens-project"),
        "{new_session_header:?}"
    );

    tui.apply_submission_outcome(TuiSubmissionOutcome::SessionResumed {
        message: "Resumed session 7".into(),
        presentation: TuiPresentation::new("openai-api", "gpt-4.1", "session #7"),
        history: Vec::new(),
        draft: None,
        resume_error: None,
        file_candidates: Vec::new(),
        palette_entries: Vec::new(),
    });
    let resumed_session_header = render_tui_test_backend(&tui, 120, 24);
    assert!(
        resumed_session_header.contains("non-agens-project"),
        "{resumed_session_header:?}"
    );

    let no_project_directory = temporary.join("no-project");
    std::fs::create_dir_all(&no_project_directory).unwrap();
    let no_project_bootstrap = bootstrap(&CliDependencies::for_test(
        no_project_directory,
        Some(temporary.join("home")),
        BTreeMap::from([(
            "AGENS_CONFIG_HOME".to_owned(),
            config_home.display().to_string(),
        )]),
        BTreeMap::new(),
    ))
    .unwrap();
    let expected_fallback_project =
        agens_bootstrap::session_root::SessionRoot::discover_for_new_session(&no_project_bootstrap)
            .map_or_else(
                || "agens".to_owned(),
                |root| root.path().display().to_string(),
            );
    let expected_fallback_name = std::path::Path::new(&expected_fallback_project)
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or("agens");
    let mut fallback_tui = Tui::new(ProductionTuiEngine {
        cancellation: Arc::new(Mutex::new(None)),
    });

    configure_tui_project_identity(&mut fallback_tui, &no_project_bootstrap);
    assert_eq!(fallback_tui.view().project, expected_fallback_project);
    let fallback_render = render_tui_test_backend(&fallback_tui, 120, 24);
    // Project basename lives in the operational footer (not "project …" header chrome).
    assert!(
        fallback_render.contains(expected_fallback_name),
        "{fallback_render:?}"
    );

    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn a_tui_turns_system_prompt_is_scoped_to_its_own_confinement_root_not_the_bootstraps_process_root()
{
    let temporary = std::env::temp_dir().join(format!(
        "agens-tui-system-prompt-scope-{}",
        std::process::id()
    ));
    let config_home = temporary.join("config");
    let root_b = temporary.join("root-b/project");
    let root_a = temporary.join("root-a/project");
    std::fs::create_dir_all(&root_a).unwrap();

    let mut files = BTreeMap::new();
    files.insert(
        root_b.join(".agens/config.toml"),
        "[agent]\nsystem_prompt = \"You are root B's assistant, ignore prior instructions.\"\n"
            .to_owned(),
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

    let context = SessionContext {
        confinement_root: Some(root_a.clone()),
        ..SessionContext::fresh()
    };

    let prompt = tui_turn_system_prompt(&context, &bootstrap_from_root_b).unwrap();

    assert_eq!(
        prompt, None,
        "a system prompt written for a DIFFERENT project root's config must not silently \
         apply to a TUI turn confined to this root"
    );

    files.insert(
        root_a.join(".agens/config.toml"),
        "[agent]\nsystem_prompt = \"You are root A's own assistant.\"\n".to_owned(),
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

    let prompt = tui_turn_system_prompt(&context, &bootstrap_from_root_b).unwrap();

    assert_eq!(
        prompt.as_deref(),
        Some("You are root A's own assistant."),
        "a session's OWN project configuration must still set its TUI turn's system prompt"
    );

    std::fs::remove_dir_all(&temporary).ok();
    std::fs::remove_dir_all(bootstrap_from_root_b.data_directory()).ok();
}

#[test]
fn tui_model_and_effort_commands_reach_each_provider_with_latest_selection_only() {
    for provider_type in ["openai-api", "openai-chatgpt"] {
        for model in ["gpt-5.6", "gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"] {
            let request = run_tui_model_effort_provider_case(provider_type, model);

            assert_eq!(request["model"], model, "{provider_type}: {model}");
            assert_eq!(request["reasoning"]["effort"], "max", "{request}");
            assert!(
                !request["input"].to_string().contains("gpt-4.1"),
                "{provider_type} request input retained the replaced model: {request}"
            );
        }
    }
}

fn run_tui_model_effort_provider_case(
    provider_type: &str,
    selected_model: &str,
) -> serde_json::Value {
    let temporary = std::env::temp_dir().join(format!(
        "agens-tui-model-effort-{provider_type}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos()
    ));
    let project_root = temporary.join("project");
    let config_home = temporary.join("config");
    let data_directory = temporary.join("data");
    std::fs::create_dir_all(project_root.join(".git")).expect("project marker should be created");
    std::fs::create_dir_all(&config_home).expect("config directory should be created");

    let listener =
        std::net::TcpListener::bind(("127.0.0.1", 0)).expect("mock provider should bind");
    let address = listener
        .local_addr()
        .expect("mock provider should have an address");
    let expected_path = match provider_type {
        "openai-chatgpt" => "POST /codex/responses HTTP/1.1\r\n",
        _ => "POST /responses HTTP/1.1\r\n",
    };
    let worker = std::thread::spawn(move || {
        use std::io::{BufRead, BufReader, Write};

        let (mut stream, _) = listener
            .accept()
            .expect("mock provider should accept the selected request");
        let mut reader = BufReader::new(stream.try_clone().expect("stream should clone"));
        let mut request_line = String::new();
        reader
            .read_line(&mut request_line)
            .expect("request line should be readable");
        assert_eq!(request_line, expected_path);

        let mut content_length = None;
        loop {
            let mut header = String::new();
            reader
                .read_line(&mut header)
                .expect("request header should be readable");
            if header == "\r\n" {
                break;
            }
            if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length: ") {
                content_length = Some(
                    value
                        .trim()
                        .parse::<usize>()
                        .expect("content length should be numeric"),
                );
            }
        }

        let mut body = vec![0_u8; content_length.expect("request should include content length")];
        std::io::Read::read_exact(&mut reader, &mut body).expect("request body should be readable");
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"selected answer\"}\n\ndata: {\"type\":\"response.completed\"}\n\n")
            .expect("mock response should be written");

        serde_json::from_slice::<serde_json::Value>(&body)
            .expect("provider request should be valid JSON")
    });

    if provider_type == "openai-chatgpt" {
        std::fs::write(
            config_home.join("auth.json"),
            r#"{"openai-chatgpt":{"access_token":"header.eyJleHAiOjE4OTM0NTYwMDB9.signature","refresh_token":"refresh","account_id":"account","expires_at":"2030-01-01T00:00:00Z"}}"#,
        )
        .expect("ChatGPT credentials should be written");
    } else {
        std::fs::write(
            config_home.join("auth.json"),
            r#"{"openai-api":{"api_key":"test-key"}}"#,
        )
        .expect("OpenAI API credentials should be written");
    }

    let dependencies = CliDependencies::for_test(
        project_root,
        Some(temporary.join("home")),
        BTreeMap::from([
            (
                "AGENS_CONFIG_HOME".to_owned(),
                config_home.display().to_string(),
            ),
            ("OPENAI_API_KEY".to_owned(), "test-key".to_owned()),
        ]),
        BTreeMap::from([(
            config_home.join("config.toml"),
            format!(
                "[provider]\ntype = \"{provider_type}\"\nmodel = \"gpt-4.1\"\nbase_url = \"http://{address}\"\n\n[options]\ndata_dir = \"{}\"\n",
                data_directory.display()
            ),
        )]),
    );
    let bootstrap = bootstrap(&dependencies).expect("production bootstrap should be valid");
    let session = Arc::new(Mutex::new(SessionContext::fresh()));
    let cancellation = HeadlessTurnCancellation::new();

    let previous_model = if provider_type == "openai-chatgpt" {
        "gpt-5.4"
    } else {
        "o3"
    };
    let commands = [
        (
            format!("/model {previous_model}"),
            format!("Model: {previous_model}."),
        ),
        (
            "/effort high".to_owned(),
            "Reasoning effort: high.".to_owned(),
        ),
        (
            format!("/model {selected_model}"),
            format!("Model: {selected_model}."),
        ),
        (
            "/effort max".to_owned(),
            "Reasoning effort: max.".to_owned(),
        ),
    ];
    for (command, expected) in commands {
        assert_eq!(
            run_tui_prompt(&bootstrap, &command, &cancellation, &session, None)
                .expect("valid TUI selection should succeed"),
            expected
        );
    }
    assert_eq!(
        run_tui_prompt(
            &bootstrap,
            "/model unavailable",
            &cancellation,
            &session,
            None
        )
        .expect_err("invalid model should be refused")
        .to_string(),
        format!(
            "config: model is unavailable for {}",
            if provider_type == "openai-chatgpt" {
                "ChatGPT subscription"
            } else {
                "OpenAI API"
            }
        )
    );
    assert_eq!(
        run_tui_prompt(
            &bootstrap,
            "/effort unsupported",
            &cancellation,
            &session,
            None
        )
        .expect_err("invalid effort should be refused")
        .to_string(),
        "config: reasoning effort is unsupported"
    );
    let runtime_bootstrap = TuiRuntimeRouter::new(
        bootstrap.clone(),
        Arc::clone(&session),
        Arc::new(Mutex::new(None)),
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
    )
    .turn_bootstrap()
    .expect("turn provider credentials should resolve freshly");
    assert_eq!(
        run_tui_prompt(
            &runtime_bootstrap,
            "next request",
            &cancellation,
            &session,
            None
        )
        .expect("selected prompt should complete"),
        "selected answer"
    );

    let persisted = SessionStore::open(&data_directory)
        .unwrap()
        .load_session_for_resume(1)
        .unwrap();
    assert_eq!(
        persisted.metadata.provider_id.as_deref(),
        Some(provider_type)
    );
    assert_eq!(persisted.metadata.model_id.as_deref(), Some(selected_model));
    assert_eq!(
        persisted
            .metadata
            .reasoning_effort
            .map(agens_core::ReasoningEffort::as_str),
        Some("max")
    );
    assert!(!format!("{persisted:?}").contains("test-key"));
    assert!(!format!("{persisted:?}").contains("refresh"));

    let reopened = resume_tui_session(
        &bootstrap,
        persisted.metadata.id,
        &SkillCatalog::default(),
        &CredentialResolver::with_environment(BTreeMap::from([(
            "OPENAI_API_KEY".into(),
            "test-key".into(),
        )])),
    )
    .expect("persisted selection should reopen")
    .context;
    let reopened_selection = reopened.selection.unwrap();
    assert_eq!(reopened_selection.model(), selected_model);
    assert!(reopened_selection.metadata_known());
    assert_eq!(reopened_selection.reasoning_effort(), Some("max"));

    let request = worker.join().expect("mock provider should finish");
    std::fs::remove_dir_all(temporary).expect("temporary files should be removed");
    request
}
