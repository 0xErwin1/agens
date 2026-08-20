use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::profiles::{AgentProfileStore, ProfileScope};
use agens_core::{
    CompletedSessionTurn, Message, MessagePart, Role, SessionMessage, SessionMetadata,
};
use agens_providers::chatgpt_login::upsert_provider_entry;
use agens_store::{SessionStore, StoredSession};
use agens_tui::{Action, Event, Key, Tui};

use agens_auth::{ChatGptAuthFlow, ChatGptAuthProgress};
use agens_bootstrap::ProviderSource;
use agens_core::HeadlessTurnError;
use agens_models::ModelSelection;
use agens_session::provider::ProviderKind;
use agens_tui::{TuiPresentation, TuiRouteProgress, TuiRouteRequest};

use super::*;
use crate::engine::{ProductionTuiEngine, run_tui_prompt_with};
use crate::extensions::{start_tui_commands, start_tui_skills, tui_builtin_commands};
use crate::resume::resume_tui_session;
use crate::session::session_dialog_entry;
use crate::test_support::{
    bootstrap_from_a_different_working_directory, bootstrap_from_configuration,
    dispatch_tui_dialog_selection, enter_tui_input, open_tui_palette_dialog, persist_tui_session,
    persist_tui_session_metadata, render_tui_test_backend, rotation_dispatcher,
    run_production_batch, submit_tui_command, tui_project, tui_session_bootstrap,
    tui_session_bootstrap_for_provider, tui_session_bootstrap_with_global_bypass,
    tui_session_bootstrap_without_provider, tui_session_directory, tui_session_messages,
};
use agens_agents::ensure_active_agent_runtime;
use agens_headless::HeadlessChatCompletion;
use agens_models::ModelSource;
use agens_session::attempt::active_session_attempts;

fn write_router_test_skill(root: &Path, name: &str, body: &str) {
    let directory = root.join(".agens/skills").join(name);
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {name}\n---\n{body}\n"),
    )
    .unwrap();
}

struct CatalogConfinementFixture {
    origin: std::path::PathBuf,
    elsewhere_root: std::path::PathBuf,
    metadata_id: i64,
    router: TuiRuntimeRouter,
    tui: Tui<ProductionTuiEngine>,
}

/// Builds a session confined to root A and a router whose STARTUP catalogs were discovered
/// from a DIFFERENT root B (matching production, where no `--resume` flag was given), with
/// one skill and one distinctly named file unique to each root.
fn catalog_confinement_fixture(label: &str) -> CatalogConfinementFixture {
    let origin = tui_session_directory(&format!("{label}-origin"));
    let origin_root = origin.join("project");
    write_router_test_skill(&origin_root, "askill", "INSTRUCTIONS-FROM-ROOT-A");
    std::fs::write(origin_root.join("only-in-a.txt"), "a").unwrap();
    let creation_bootstrap = tui_session_bootstrap(&origin, &[]);
    let mut store = SessionStore::open(creation_bootstrap.data_directory()).unwrap();
    let metadata = persist_tui_session(&mut store, &tui_project(&origin), "origin");
    drop(store);

    let resume_bootstrap =
        bootstrap_from_a_different_working_directory(&origin, &format!("{label}-elsewhere"));
    let elsewhere_root =
        agens_bootstrap::session_root::discovered_root_for_tests(&resume_bootstrap);
    write_router_test_skill(&elsewhere_root, "bskill", "INSTRUCTIONS-FROM-ROOT-B");
    std::fs::write(elsewhere_root.join("only-in-elsewhere.txt"), "b").unwrap();

    let cancellation = Arc::new(Mutex::new(None));
    let mut tui = Tui::new(ProductionTuiEngine {
        cancellation: Arc::clone(&cancellation),
    });
    let startup_commands =
        start_tui_commands(&mut tui, &resume_bootstrap, &elsewhere_root).unwrap();
    let startup_skills = start_tui_skills(&mut tui, &resume_bootstrap, &elsewhere_root).unwrap();
    tui.set_palette_entries(resolved_tui_palette(
        &startup_commands,
        &startup_skills,
        false,
    ));
    let session = Arc::new(Mutex::new(SessionContext::fresh()));
    let router = TuiRuntimeRouter::new(
        resume_bootstrap,
        session,
        cancellation,
        startup_commands,
        startup_skills,
    );

    CatalogConfinementFixture {
        origin,
        elsewhere_root,
        metadata_id: metadata.id,
        router,
        tui,
    }
}

impl Drop for CatalogConfinementFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.origin);
        let _ = std::fs::remove_dir_all(self.elsewhere_root.parent().unwrap());
    }
}

fn assert_router_confined_to_root_a(router: &TuiRuntimeRouter) {
    assert!(
        matches!(
            router.route("/bskill args".into()),
            TuiSubmissionOutcome::LocalActionableError { .. }
        ),
        "root B's skill must no longer be reachable once the session is confined to root A"
    );
    let TuiSubmissionOutcome::ProviderTurn { prompt, .. } = router.route("/askill args".into())
    else {
        panic!("root A's own skill must be reachable after resuming into root A");
    };
    assert!(prompt.contains("INSTRUCTIONS-FROM-ROOT-A"), "{prompt:?}");
}

/// Proves C-NEW is closed: a post-startup `/resume <id>` across a differently rooted
/// process re-discovers the router's command/skill catalogs from the RESUMED session's own
/// root, instead of keeping the catalogs the router was constructed with at startup.
///
/// Mirrors the verifier's own probe exactly: a session created under root A is resumed from
/// a router whose STARTUP catalogs came from a different root B (matching production, where
/// no `--resume` flag was given and the process's own root is B). Before the fix, `/bskill`
/// (root B's skill, model-facing instruction text) would remain reachable, and root A's own
/// `/askill` would stay unknown, even though the session is now confined to A.
#[test]
fn a_post_startup_resume_command_refreshes_commands_skills_and_picker_candidates() {
    let fixture = catalog_confinement_fixture("catalog-confinement-resume-command");

    assert!(
        matches!(
            fixture.router.route("/bskill args".into()),
            TuiSubmissionOutcome::ProviderTurn { .. }
        ),
        "root B's skill must be reachable before any resume, matching production startup"
    );

    let resume_outcome = fixture
        .router
        .route(format!("/resume {}", fixture.metadata_id));
    let TuiSubmissionOutcome::SessionResumed {
        file_candidates, ..
    } = resume_outcome
    else {
        panic!("expected a successful resume, got {resume_outcome:?}");
    };

    assert_router_confined_to_root_a(&fixture.router);
    assert_eq!(
        file_candidates,
        vec!["only-in-a.txt".to_owned()],
        "the picker candidates on the resume outcome must enumerate the resumed \
         session's own root, not the resuming process's discovered root"
    );
}

/// Same proof as the `/resume <id>` test above, but through the session-picker dialog action
/// (`session:{id}`) instead of the slash command — the second of the three post-startup
/// resume entry points named by C-NEW.
#[test]
fn a_post_startup_session_picker_action_refreshes_commands_skills_and_picker_candidates() {
    let fixture = catalog_confinement_fixture("catalog-confinement-picker-action");

    let resume_outcome = fixture.router.route_dialog_action(
        &format!("session:{}", fixture.metadata_id),
        std::sync::mpsc::channel().0,
    );
    let TuiSubmissionOutcome::SessionResumed {
        file_candidates, ..
    } = resume_outcome
    else {
        panic!("expected a successful resume, got {resume_outcome:?}");
    };

    assert_router_confined_to_root_a(&fixture.router);
    assert_eq!(file_candidates, vec!["only-in-a.txt".to_owned()]);
}

/// Same proof again, but through interrupted-attempt recovery (`session:recover:<id>:<n>`)
/// — the third of the three post-startup resume entry points named by C-NEW. This path
/// returns `ProviderTurn`, not `SessionResumed`, so it has no picker candidates to check.
#[test]
fn a_post_startup_recovery_action_refreshes_commands_and_skills() {
    let fixture = catalog_confinement_fixture("catalog-confinement-recovery");
    let bootstrap = fixture.router.bootstrap().unwrap();
    let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
    let stored = store.load_session_for_resume(fixture.metadata_id).unwrap();
    let attempt = store
        .begin_session_attempt(&stored.metadata, "recovered prompt".into())
        .unwrap();
    drop(store);

    let outcome = fixture.router.route_dialog_action(
        &format!(
            "session:recover:{}:{}",
            attempt.key().session_id(),
            attempt.key().attempt_id()
        ),
        std::sync::mpsc::channel().0,
    );
    assert!(
        matches!(
            &outcome,
            TuiSubmissionOutcome::ProviderTurn { prompt, .. } if prompt == "recovered prompt"
        ),
        "{outcome:?}"
    );

    assert_router_confined_to_root_a(&fixture.router);
}

/// The router's own palette cache is refreshed on a post-startup `/resume` (proven above by
/// `assert_router_confined_to_root_a`'s `/bskill` check and the file-candidate assertions),
/// but `Tui::set_palette_entries` is only ever called once, at startup. A post-startup resume
/// must still refresh the composer's OWN copy — the one it actually renders from — or root
/// B's skill and command names keep showing up in the composer's autocomplete after the
/// session is confined to root A.
#[test]
fn a_post_startup_resume_refreshes_the_composers_own_palette_not_just_the_routers() {
    let mut fixture = catalog_confinement_fixture("catalog-confinement-composer-palette");

    let palette_before_resume: Vec<String> = fixture
        .tui
        .view()
        .palette
        .map(|palette| {
            palette
                .entries()
                .iter()
                .map(|entry| entry.name().to_owned())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        palette_before_resume.is_empty(),
        "the palette dialog is not open yet, so the view must not surface any entries"
    );

    let resume_outcome = fixture
        .router
        .route(format!("/resume {}", fixture.metadata_id));
    assert!(
        fixture
            .tui
            .apply_submission_outcome(resume_outcome)
            .is_none(),
        "a session resume does not launch a provider turn"
    );

    fixture
        .tui
        .handle(agens_tui::Event::Key(agens_tui::Key::Char('/')));
    let rendered_palette_names: Vec<String> = fixture
        .tui
        .view()
        .palette
        .expect("typing a leading '/' opens the palette")
        .entries()
        .iter()
        .map(|entry| entry.name().to_owned())
        .collect();

    assert!(
        !rendered_palette_names.iter().any(|name| name == "bskill"),
        "the composer's OWN rendered palette must not keep listing root B's skill after the \
         session is confined to root A: {rendered_palette_names:?}"
    );
    assert!(
        rendered_palette_names.iter().any(|name| name == "askill"),
        "root A's own skill must be present in the composer's rendered palette after resume: \
         {rendered_palette_names:?}"
    );
}

#[test]
fn subagent_profile_overlay_renders_origins_and_marks_unavailable_catalog_entries() {
    let bootstrap = bootstrap_from_configuration(
        "subagent-profile-overlay-unavailable",
        Some(
            "[provider]\ntype = \"openai-api\"\nmodel = \"gpt-4.1\"\n\
             \n[agents.explore]\nmodel = \"stored-missing\"\n",
        ),
        None,
    );
    let root = std::env::temp_dir()
        .join(format!(
            "agens-subagent-profile-overlay-unavailable-{}",
            std::process::id()
        ))
        .join("project");
    let agents = bootstrap.paths.global_config.with_file_name("agents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(
        agents.join("bad.md"),
        "---\nname: bad\ndescription: bad\nmode: subagent\nmodel: bad\n---\nbad.",
    )
    .unwrap();
    let mut context = SessionContext::fresh();
    context.confinement_root = Some(root.clone());
    let session = Arc::new(Mutex::new(context));
    let cancellation = Arc::new(Mutex::new(None));
    let mut tui = Tui::new(ProductionTuiEngine {
        cancellation: Arc::clone(&cancellation),
    });
    let router = TuiRuntimeRouter::new(
        bootstrap,
        session,
        cancellation,
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
    );
    let (progress, _) = std::sync::mpsc::channel();

    tui.apply_submission_outcome(router.route_request(
        TuiRouteRequest::OpenDialog("subagent-profiles".into()),
        progress,
    ));
    let overlay = render_tui_test_backend(&tui, 120, 24);

    assert!(
        overlay.contains("explore · stored-missing [profile:global]"),
        "stored profile origin/value must render: {overlay:?}"
    );
    assert!(
        overlay.contains("bad · bad [frontmatter]"),
        "frontmatter-unavailable agent must remain visible: {overlay:?}"
    );
    assert!(
        overlay.matches("(unavailable)").count() >= 2,
        "both unavailable effective models must be marked: {overlay:?}"
    );

    std::fs::remove_dir_all(root.parent().unwrap()).unwrap();
}

struct RecordingProfileStore {
    saved: Mutex<Vec<(ProfileScope, String, agens_config::AgentProfilePatch)>>,
}

impl AgentProfileStore for RecordingProfileStore {
    fn save(
        &self,
        scope: ProfileScope,
        agent: &str,
        patch: &agens_config::AgentProfilePatch,
    ) -> Result<(), String> {
        self.saved
            .lock()
            .unwrap()
            .push((scope, agent.to_owned(), patch.clone()));
        Ok(())
    }
}

#[test]
fn subagent_profile_overlay_autosaves_in_the_selected_scope() {
    let temporary = tui_session_directory("subagent-profile-overlay-autosave");
    let bootstrap = tui_session_bootstrap(&temporary, &[]);
    let session = Arc::new(Mutex::new(SessionContext::fresh()));
    let cancellation = Arc::new(Mutex::new(None));
    let store = Arc::new(RecordingProfileStore {
        saved: Mutex::new(Vec::new()),
    });
    let router = TuiRuntimeRouter::new(
        bootstrap,
        session,
        cancellation,
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
    )
    .with_profile_store(store.clone());
    let progress = std::sync::mpsc::channel().0;

    router.route_request(
        TuiRouteRequest::OpenDialog("subagent-profiles".into()),
        progress.clone(),
    );
    router.route_dialog_action(
        "subagent-profiles:set-model:explore:gpt-4.1",
        progress.clone(),
    );
    router.route_dialog_action("subagent-profiles:scope:toggle", progress.clone());
    router.route_dialog_action("subagent-profiles:set-model:explore:gpt-4.1-mini", progress);

    let saved = store.saved.lock().unwrap();
    assert_eq!(saved.len(), 2);
    assert_eq!(
        saved[0],
        (
            ProfileScope::Global,
            "explore".into(),
            agens_config::AgentProfilePatch {
                model: Some(Some("gpt-4.1".into())),
                effort: None
            }
        )
    );
    assert_eq!(
        saved[1],
        (
            ProfileScope::Project,
            "explore".into(),
            agens_config::AgentProfilePatch {
                model: Some(Some("gpt-4.1-mini".into())),
                effort: None
            }
        )
    );
    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn subagent_profile_cycle_and_reset_autosave_patches() {
    let temporary = tui_session_directory("subagent-profile-overlay-cycle-reset");
    let bootstrap = tui_session_bootstrap(&temporary, &[]);
    let store = Arc::new(RecordingProfileStore {
        saved: Mutex::new(Vec::new()),
    });
    let router = TuiRuntimeRouter::new(
        bootstrap,
        Arc::new(Mutex::new(SessionContext::fresh())),
        Arc::new(Mutex::new(None)),
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
    )
    .with_profile_store(store.clone());
    let progress = std::sync::mpsc::channel().0;

    router.route_request(
        TuiRouteRequest::OpenDialog("subagent-profiles".into()),
        progress.clone(),
    );
    router.route_dialog_action(
        "subagent-profiles:cycle-effort:explore:next",
        progress.clone(),
    );
    router.route_dialog_action("subagent-profiles:reset-row:explore", progress);

    let saved = store.saved.lock().unwrap();
    assert_eq!(saved[0].2.effort, Some(Some("none".into())));
    assert_eq!(
        saved[1],
        (
            ProfileScope::Global,
            "explore".into(),
            agens_config::AgentProfilePatch {
                model: Some(None),
                effort: Some(None)
            }
        )
    );
    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn subagent_profile_root_searches_letters_and_focuses_the_autosaved_row() {
    let temporary = tui_session_directory("subagent-profile-overlay-search");
    let bootstrap = tui_session_bootstrap(&temporary, &[]);
    let store = Arc::new(RecordingProfileStore {
        saved: Mutex::new(Vec::new()),
    });
    let router = TuiRuntimeRouter::new(
        bootstrap,
        Arc::new(Mutex::new(SessionContext::fresh())),
        Arc::new(Mutex::new(None)),
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
    )
    .with_profile_store(store.clone());
    let progress = std::sync::mpsc::channel().0;
    let mut tui = Tui::new(ProductionTuiEngine {
        cancellation: Arc::new(Mutex::new(None)),
    });

    tui.apply_submission_outcome(router.route_request(
        TuiRouteRequest::OpenDialog("subagent-profiles".into()),
        progress.clone(),
    ));
    assert_eq!(tui.handle(Event::Key(Key::Char('/'))), Action::Render);
    for key in [Key::Char('m'), Key::Char('s')] {
        assert_eq!(tui.handle(Event::Key(key)), Action::Render);
    }
    assert!(store.saved.lock().unwrap().is_empty());
    assert!(!render_tui_test_backend(&tui, 120, 24).contains("Choose profile model"));
    for _ in 0..2 {
        assert_eq!(tui.handle(Event::Key(Key::Backspace)), Action::Render);
    }
    for key in [Key::Char('e'), Key::Char('x'), Key::Char('p')] {
        assert_eq!(tui.handle(Event::Key(key)), Action::Render);
    }
    let overlay = render_tui_test_backend(&tui, 120, 24);
    assert!(
        overlay.contains("explore"),
        "query must retain the matching root row: {overlay:?}"
    );
    assert!(!overlay.contains("Choose profile model"));
    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::SafeDialogAction("subagent-profiles:edit:explore".into())
    );

    router.route_dialog_action(
        "subagent-profiles:set-model:explore:gpt-4.1",
        progress.clone(),
    );
    tui.apply_submission_outcome(router.route_request(
        TuiRouteRequest::OpenDialog("subagent-profiles".into()),
        progress.clone(),
    ));
    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::SafeDialogAction("subagent-profiles:edit:explore".into())
    );
    tui.apply_submission_outcome(
        router.route_dialog_action("subagent-profiles:edit:explore", progress.clone()),
    );
    assert_eq!(
        tui.handle(Event::Key(Key::Escape)),
        Action::SafeDialogAction("subagent-profiles:back".into())
    );
    tui.apply_submission_outcome(router.route_request(
        TuiRouteRequest::DialogAction("subagent-profiles:back".into()),
        progress,
    ));
    assert_eq!(store.saved.lock().unwrap().len(), 1);
    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn tui_login_command_palette_and_selector_are_exact() {
    let temporary = tui_session_directory("login-selector");
    let bootstrap = tui_session_bootstrap(&temporary, &[]);
    let router = TuiRuntimeRouter::new(
        bootstrap,
        Arc::new(Mutex::new(SessionContext::fresh())),
        Arc::new(Mutex::new(None)),
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
    );

    let entry = router
        .palette_entries()
        .unwrap()
        .into_iter()
        .find(|entry| entry.name() == "login")
        .expect("/login is in the palette");
    assert_eq!(entry.description(), "Sign in to a provider");
    assert_eq!(entry.argument_hint(), "");
    assert_eq!(
        router.route("/login".into()),
        router.open_dialog("login").unwrap()
    );
    assert!(matches!(
        router.route("/login anything".into()),
        TuiSubmissionOutcome::LocalActionableError { message, .. }
            if message == "usage: unknown TUI command: /login anything"
    ));

    let mut tui = Tui::new(ProductionTuiEngine {
        cancellation: Arc::new(Mutex::new(None)),
    });
    tui.apply_submission_outcome(router.open_dialog("login").unwrap());
    let selector = render_tui_test_backend(&tui, 100, 24);
    for text in [
        "Sign in",
        "Choose a provider",
        "ChatGPT subscription",
        "OpenAI API key",
        "Moonshot AI API key",
    ] {
        assert!(selector.contains(text), "{selector:?}");
    }
    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::DialogAction("login:chatgpt".into())
    );
    tui.apply_submission_outcome(
        router.route_dialog_action("login:chatgpt", std::sync::mpsc::channel().0),
    );
    assert!(render_tui_test_backend(&tui, 100, 24).contains("Connect to ChatGPT"));

    tui.apply_submission_outcome(router.open_dialog("login").unwrap());
    tui.handle(Event::Key(Key::Down));
    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::DialogAction("login:api-key:openai-api".into())
    );
    tui.apply_submission_outcome(router.open_dialog("login").unwrap());
    tui.handle(Event::Key(Key::Down));
    tui.handle(Event::Key(Key::Down));
    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::DialogAction("login:api-key:moonshotai".into())
    );

    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn tui_login_api_key_entries_mask_and_persist_without_runtime_mutation() {
    let temporary = tui_session_directory("login-api-key");
    let config_home = temporary.join("config");
    let credentials = config_home.join("auth.json");
    std::fs::create_dir_all(&config_home).unwrap();
    std::fs::write(
        &credentials,
        r#"{"openai-chatgpt":{"access_token":"kept"},"other":{"sentinel":"kept"}}"#,
    )
    .unwrap();
    let bootstrap = tui_session_bootstrap(&temporary, &[]);
    let session = Arc::new(Mutex::new(SessionContext {
        provider: Some(ProviderKind::OpenAiChatGpt),
        ..SessionContext::fresh()
    }));
    let original_session = session.lock().unwrap().clone();
    let original_bootstrap = (
        bootstrap.provider_type().map(str::to_owned),
        bootstrap.model().map(str::to_owned),
        bootstrap.api_key.clone(),
    );
    let router = TuiRuntimeRouter::new(
        bootstrap,
        Arc::clone(&session),
        Arc::new(Mutex::new(None)),
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
    );
    let mut tui = Tui::new(ProductionTuiEngine {
        cancellation: Arc::new(Mutex::new(None)),
    });

    for (action, title, help, key, success) in [
        (
            "login:api-key:openai-api",
            "Sign in to OpenAI API",
            "Enter your OpenAI API key",
            "  openai-secret  ",
            "Logged in to openai-api.",
        ),
        (
            "login:api-key:moonshotai",
            "Sign in to Moonshot AI",
            "Enter your Moonshot AI API key",
            " moonshot-secret ",
            "Logged in to moonshotai.",
        ),
    ] {
        tui.apply_submission_outcome(
            router.route_dialog_action(action, std::sync::mpsc::channel().0),
        );
        let overlay = render_tui_test_backend(&tui, 100, 24);
        assert!(overlay.contains(title), "{overlay:?}");
        assert!(overlay.contains(help), "{overlay:?}");
        assert_eq!(tui.handle(Event::Paste(key.into())), Action::Render);
        let masked = render_tui_test_backend(&tui, 100, 24);
        assert!(!masked.contains(key.trim()), "{masked:?}");
        let Action::SubmitSecret { action_id, secret } = tui.handle(Event::Key(Key::Enter)) else {
            panic!("secret overlay must submit through the secret route");
        };
        let outcome = router.route_request(
            TuiRouteRequest::SubmitSecret { action_id, secret },
            std::sync::mpsc::channel().0,
        );
        assert!(
            matches!(outcome, TuiSubmissionOutcome::LocalInfo(ref message) if message == success)
        );
        tui.apply_submission_outcome(outcome);
    }

    let stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&credentials).unwrap()).unwrap();
    assert_eq!(stored["openai-api"]["api_key"], "openai-secret");
    assert_eq!(stored["moonshotai"]["api_key"], "moonshot-secret");
    assert_eq!(stored["openai-chatgpt"]["access_token"], "kept");
    assert_eq!(stored["other"]["sentinel"], "kept");
    assert_eq!(*session.lock().unwrap(), original_session);
    let bootstrap = router.bootstrap().unwrap();
    assert_eq!(
        (
            bootstrap.provider_type().map(str::to_owned),
            bootstrap.model().map(str::to_owned),
            bootstrap.api_key.clone(),
        ),
        original_bootstrap
    );

    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn tui_login_rejects_unknown_actions_and_sanitizes_write_failures() {
    let temporary = tui_session_directory("login-errors");
    let mut bootstrap = tui_session_bootstrap(&temporary, &[]);
    bootstrap.paths.credentials = temporary.join("credentials-directory");
    std::fs::create_dir_all(&bootstrap.paths.credentials).unwrap();
    let router = TuiRuntimeRouter::new(
        bootstrap,
        Arc::new(Mutex::new(SessionContext::fresh())),
        Arc::new(Mutex::new(None)),
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
    );
    let mut tui = Tui::new(ProductionTuiEngine {
        cancellation: Arc::new(Mutex::new(None)),
    });

    for (action, expected) in [
        ("login:api-key:unknown", "login action is invalid"),
        ("login:chatgpt", "login action is invalid"),
        ("other-sentinel", "login action is invalid"),
        (
            "login:api-key:openai-api",
            "API-key credentials could not be saved",
        ),
    ] {
        tui.apply_submission_outcome(TuiSubmissionOutcome::SecretEntry(
            agens_tui::SecretEntryView::new("Secret", None::<String>, action),
        ));
        tui.handle(Event::Paste("secret-sentinel".into()));
        let Action::SubmitSecret { action_id, secret } = tui.handle(Event::Key(Key::Enter)) else {
            panic!("secret overlay must submit");
        };
        let outcome = router.route_request(
            TuiRouteRequest::SubmitSecret { action_id, secret },
            std::sync::mpsc::channel().0,
        );
        assert!(
            matches!(outcome, TuiSubmissionOutcome::LocalActionableError { ref message, .. } if message == expected)
        );
        assert!(!format!("{outcome:?}").contains("secret-sentinel"));
    }

    std::fs::remove_dir_all(temporary).unwrap();
}

fn test_chatgpt_credentials(
    access_token: &str,
) -> agens_providers::chatgpt_login::ChatGptCredentials {
    agens_providers::chatgpt_login::ChatGptCredentials {
        access_token: access_token.into(),
        refresh_token: "refresh".into(),
        account_id: "account".into(),
        expires_at: "2099-01-01T00:00:00Z".into(),
    }
}

#[test]
fn tui_enter_routes_unknown_slash_and_local_output_without_provider_history() {
    let temporary = tui_session_directory("enter-local-routing");
    let bootstrap = tui_session_bootstrap(&temporary, &[]);
    let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
    let metadata = persist_tui_session(&mut store, &tui_project(&temporary), "current");
    let session = Arc::new(Mutex::new(SessionContext::fresh()));
    let router = TuiRuntimeRouter::new(
        bootstrap,
        Arc::clone(&session),
        Arc::new(Mutex::new(None)),
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
    );
    let cancellation = Arc::new(Mutex::new(None));
    let mut tui = Tui::new(ProductionTuiEngine { cancellation });
    let input = enter_tui_input(&mut tui, "/unknown");
    let provider_invocations =
        usize::from(tui.apply_submission_outcome(router.route(input)).is_some());
    assert_eq!(provider_invocations, 0);
    assert!(tui.transcript().is_empty());
    assert!(tui.view().dialog.is_some());

    session.lock().unwrap().running = true;
    let input = enter_tui_input(&mut tui, "/new");
    tui.apply_submission_outcome(router.route(input));
    assert!(tui.view().dialog.is_some());

    session.lock().unwrap().running = false;
    let input = enter_tui_input(&mut tui, "/new");
    tui.apply_submission_outcome(router.route(input));
    assert!(tui.transcript().is_empty());
    assert_eq!(tui.view().status, Some("Started a new session."));

    let input = enter_tui_input(&mut tui, &format!("/resume {}", metadata.id));
    tui.apply_submission_outcome(router.route(input));
    assert_eq!(tui.view().session, format!("session #{}", metadata.id));

    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn tui_new_command_reseeds_bypass_permissions_from_configuration_after_a_toggle() {
    let temporary = tui_session_directory("new-command-reseeds-bypass");
    let bootstrap = tui_session_bootstrap_with_global_bypass(&temporary, true);
    let session = Arc::new(Mutex::new(SessionContext::fresh()));
    let router = TuiRuntimeRouter::new(
        bootstrap,
        Arc::clone(&session),
        Arc::new(Mutex::new(None)),
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
    );
    // The user turns bypass off, then starts a new session; configuration re-seeds it to on.
    session.lock().unwrap().bypass_permissions = false;
    let outcome = router.route("/new".into());
    assert!(matches!(
        outcome,
        TuiSubmissionOutcome::ResetSucceeded { .. }
    ));
    assert!(session.lock().unwrap().bypass_permissions);

    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn tui_model_effort_and_help_palette_routes_open_local_overlays_and_dispatch_once() {
    let temporary = tui_session_directory("local-overlays");
    let bootstrap = tui_session_bootstrap(&temporary, &[]);
    let session = Arc::new(Mutex::new(SessionContext::fresh()));
    let cancellation = Arc::new(Mutex::new(None));
    let mut tui = Tui::new(ProductionTuiEngine {
        cancellation: Arc::clone(&cancellation),
    });
    let router = TuiRuntimeRouter::new(
        bootstrap,
        Arc::clone(&session),
        cancellation,
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
    );
    tui.set_palette_entries(router.palette_entries().unwrap());
    let (progress, _) = std::sync::mpsc::channel();

    for (prefix, route_id, expected) in [
        ("/mo", "model", ["Choose model", "gpt-4.1 · OpenAI API"]),
        ("/ef", "effort", ["Choose effort", "Default"]),
        ("/he", "help", ["Commands and skills", "/connect"]),
    ] {
        for character in prefix.chars() {
            tui.handle(agens_tui::Event::Key(agens_tui::Key::Char(character)));
        }
        let agens_tui::Action::OpenDialog(actual_route) =
            tui.handle(agens_tui::Event::Key(agens_tui::Key::Enter))
        else {
            panic!("palette Enter should open the selected overlay");
        };
        assert_eq!(actual_route, route_id);
        let outcome = router.route_request(
            agens_tui::TuiRouteRequest::OpenDialog(actual_route),
            progress.clone(),
        );
        assert!(tui.apply_submission_outcome(outcome).is_none());
        let text = render_tui_test_backend(&tui, 80, 24);
        assert!(text.contains(expected[0]), "{route_id}: {text:?}");
        assert!(text.contains(expected[1]), "{route_id}: {text:?}");

        if route_id == "help" {
            assert_eq!(
                tui.handle(agens_tui::Event::Key(agens_tui::Key::CtrlC)),
                agens_tui::Action::Render
            );
            continue;
        }
        tui.handle(agens_tui::Event::Key(agens_tui::Key::Down));
        let agens_tui::Action::DialogAction(action_id) =
            tui.handle(agens_tui::Event::Key(agens_tui::Key::Enter))
        else {
            panic!("dialog Enter should emit one action ID");
        };
        let outcome = router.route_request(
            agens_tui::TuiRouteRequest::DialogAction(action_id),
            progress.clone(),
        );
        assert!(tui.apply_submission_outcome(outcome).is_none());
        assert!(tui.view().dialog.is_none());
    }

    assert!(session.lock().unwrap().messages.is_empty());
    assert!(
        tui.transcript()
            .iter()
            .all(|entry| !matches!(entry, agens_tui::TranscriptEntry::User(_)))
    );
    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn tui_connect_and_disconnect_overlays_select_flows_and_cancel_without_credentials_mutation() {
    let temporary = tui_session_directory("auth-overlays");
    let config_home = temporary.join("config");
    let credentials_path = config_home.join("auth.json");
    std::fs::create_dir_all(&config_home).unwrap();
    let initial_credentials = r#"{"openai-api":{"api_key":"preserved"}}"#;
    std::fs::write(&credentials_path, initial_credentials).unwrap();
    let flows = Arc::new(Mutex::new(Vec::new()));
    let coordinator = ChatGptAuthCoordinator::with_authenticator({
        let flows = Arc::clone(&flows);
        move |flow, _, publish| {
            flows.lock().unwrap().push(flow);
            publish(ChatGptAuthProgress::BrowserUrl("auth-url".into()));
            Ok(test_chatgpt_credentials("new-access"))
        }
    });
    let session = Arc::new(Mutex::new(SessionContext::fresh()));
    let cancellation = Arc::new(Mutex::new(None));
    let mut tui = Tui::new(ProductionTuiEngine {
        cancellation: Arc::clone(&cancellation),
    });
    let router = TuiRuntimeRouter::with_auth_coordinator(
        tui_session_bootstrap(&temporary, &[]),
        Arc::clone(&session),
        cancellation,
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
        coordinator,
    );
    tui.set_palette_entries(router.palette_entries().unwrap());
    let (progress, _) = std::sync::mpsc::channel();

    for (prefix, down, flow) in [
        ("/co", false, ChatGptAuthFlow::Browser),
        ("/co", true, ChatGptAuthFlow::Device),
    ] {
        open_tui_palette_dialog(&mut tui, &router, prefix, "connect", progress.clone());
        if down {
            tui.handle(Event::Key(Key::Down));
        }
        dispatch_tui_dialog_selection(&mut tui, &router, progress.clone());
        assert_eq!(flows.lock().unwrap().last(), Some(&flow));
    }

    open_tui_palette_dialog(&mut tui, &router, "/di", "disconnect", progress.clone());
    let connected = std::fs::read_to_string(&credentials_path).unwrap();
    assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::Render);
    let after_cancel = std::fs::read_to_string(&credentials_path).unwrap();
    assert_eq!(after_cancel, connected);
    open_tui_palette_dialog(&mut tui, &router, "/di", "disconnect", progress);
    dispatch_tui_dialog_selection(&mut tui, &router, std::sync::mpsc::channel().0);

    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn device_auth_browser_opening_uses_injected_adapter_and_sanitizes_failures() {
    let temporary = tui_session_directory("device-auth-browser-open");
    let opened = Arc::new(Mutex::new(Vec::new()));
    let router = TuiRuntimeRouter::with_auth_coordinator(
        tui_session_bootstrap(&temporary, &[]),
        Arc::new(Mutex::new(SessionContext::fresh())),
        Arc::new(Mutex::new(None)),
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
        ChatGptAuthCoordinator::with_authenticator_and_browser_opener(
            |_, _, _| Ok(test_chatgpt_credentials("unused")),
            {
                let opened = Arc::clone(&opened);
                Arc::new(move |url| {
                    opened.lock().unwrap().push(url.to_owned());
                    Ok(())
                })
            },
        ),
    );

    assert!(matches!(
        router.open_device_auth_url("https://auth.example/device"),
        Ok(message) if message == "Browser opened."
    ));
    assert_eq!(*opened.lock().unwrap(), vec!["https://auth.example/device"]);

    let failing_router = TuiRuntimeRouter::with_auth_coordinator(
        tui_session_bootstrap(&temporary, &[]),
        Arc::new(Mutex::new(SessionContext::fresh())),
        Arc::new(Mutex::new(None)),
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
        ChatGptAuthCoordinator::with_authenticator_and_browser_opener(
            |_, _, _| Ok(test_chatgpt_credentials("unused")),
            Arc::new(|_| {
                Err(std::io::Error::other(
                    "command diagnostic must not reach UI",
                ))
            }),
        ),
    );
    let outcome =
        auth_route_outcome(failing_router.open_device_auth_url("https://auth.example/device"));
    assert!(matches!(
        outcome,
        TuiSubmissionOutcome::LocalActionableError { ref message, ref action }
            if message == "ChatGPT verification page could not be opened"
                && action == "Copy the verification link and open it in a browser."
    ));
    assert!(!format!("{outcome:?}").contains("command diagnostic"));

    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn tui_router_connect_device_disconnect_uses_coordinator_without_provider_history() {
    let temporary = tui_session_directory("auth-router");
    let config_home = temporary.join("config");
    let credentials_path = config_home.join("auth.json");
    std::fs::create_dir_all(&config_home).unwrap();
    std::fs::write(
        &credentials_path,
        r#"{"openai-api":{"api_key":"preserved"},"other":{"value":"kept"}}"#,
    )
    .unwrap();
    let mut bootstrap = tui_session_bootstrap(&temporary, &[]);
    bootstrap.provider_source = ProviderSource::Auto;
    bootstrap.provider_type = Some("openai-api".into());
    bootstrap.api_key = Some("preserved".into());
    let flows = Arc::new(Mutex::new(Vec::new()));
    let coordinator = ChatGptAuthCoordinator::with_authenticator({
        let flows = Arc::clone(&flows);
        move |flow, _, publish| {
            flows.lock().unwrap().push(flow);
            publish(ChatGptAuthProgress::BrowserUrl("auth-url".into()));
            Ok(test_chatgpt_credentials("new-access"))
        }
    });
    let session = Arc::new(Mutex::new(SessionContext::fresh()));
    let router = TuiRuntimeRouter::with_auth_coordinator(
        bootstrap,
        Arc::clone(&session),
        Arc::new(Mutex::new(None)),
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
        coordinator,
    );
    let (progress_tx, progress_rx) = std::sync::mpsc::channel();

    assert!(matches!(
        router.route_with_progress("/connect --device-auth".into(), progress_tx),
        TuiSubmissionOutcome::LocalInfo(_)
    ));
    assert_eq!(progress_rx.try_iter().count(), 1);
    assert_eq!(*flows.lock().unwrap(), vec![ChatGptAuthFlow::Device]);
    let context = session.lock().unwrap();
    assert_eq!(context.provider, Some(ProviderKind::OpenAiChatGpt));
    assert!(context.messages.is_empty());
    drop(context);
    let configured = router.bootstrap().unwrap();
    assert_eq!(configured.provider_type(), Some("openai-api"));
    let connected = std::fs::read_to_string(&credentials_path).unwrap();
    assert!(connected.contains("new-access"));

    assert!(router.disconnect().is_ok());
    assert_eq!(
        session.lock().unwrap().provider,
        Some(ProviderKind::OpenAiApi)
    );
    let stored = std::fs::read_to_string(&credentials_path).unwrap();
    assert!(stored.contains("preserved"));
    assert!(stored.contains("kept"));
    assert!(!stored.contains("new-access"));

    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn runtime_chatgpt_refresh_atomicity_preserves_intervening_unrelated_provider_write() {
    let temporary = tui_session_directory("refresh-rollback");
    let config_home = temporary.join("config");
    let credentials_path = config_home.join("auth.json");
    std::fs::create_dir_all(&config_home).unwrap();
    let before = br#"{"openai-api":{"api_key":"preserved"},"openai-chatgpt":{"access_token":"old-access","refresh_token":"old-refresh","account_id":"old-account","expires_at":"2099-01-01T00:00:00Z"}}"#;
    std::fs::write(&credentials_path, before).unwrap();
    let mut bootstrap = tui_session_bootstrap(&temporary, &[]);
    bootstrap.provider_source = ProviderSource::Auto;
    bootstrap.provider_type = Some("openai-api".into());
    bootstrap.api_key = Some("preserved".into());
    let session = Arc::new(Mutex::new(SessionContext {
        running: true,
        ..SessionContext::fresh()
    }));
    let original_runtime = session.lock().unwrap().clone();
    let router = TuiRuntimeRouter::with_auth_coordinator(
        bootstrap,
        Arc::clone(&session),
        Arc::new(Mutex::new(None)),
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
        ChatGptAuthCoordinator::with_authenticator(|_, _, _| {
            Ok(test_chatgpt_credentials("new-access"))
        }),
    )
    .with_credential_restorer(|path, snapshot| {
        upsert_provider_entry(path, "other-provider", serde_json::json!({"key": "kept"}))
            .map_err(|_| CliError::storage("unrelated provider write failed"))?;
        restore_chatgpt_credentials(path, snapshot)
    });

    assert!(
        router
            .connect(ChatGptAuthFlow::Browser, std::sync::mpsc::channel().0)
            .is_err()
    );
    let mut expected = serde_json::from_slice::<serde_json::Value>(before).unwrap();
    expected
        .as_object_mut()
        .unwrap()
        .insert("other-provider".into(), serde_json::json!({"key": "kept"}));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&std::fs::read(&credentials_path).unwrap())
            .unwrap(),
        expected
    );
    assert_eq!(*session.lock().unwrap(), original_runtime);

    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn runtime_chatgpt_refresh_atomicity_disconnects_explicit_chatgpt_fail_closed() {
    let temporary = tui_session_directory("explicit-disconnect");
    let config_home = temporary.join("config");
    let credentials_path = config_home.join("auth.json");
    std::fs::create_dir_all(&config_home).unwrap();
    std::fs::write(
        &credentials_path,
        r#"{"openai-api":{"api_key":"preserved"},"openai-chatgpt":{"access_token":"old-access","refresh_token":"old-refresh","account_id":"old-account","expires_at":"2099-01-01T00:00:00Z"}}"#,
    )
    .unwrap();
    let mut bootstrap = tui_session_bootstrap(&temporary, &[]);
    bootstrap.provider_source = ProviderSource::ExplicitChatGpt;
    bootstrap.provider_type = Some("openai-chatgpt".into());
    let session = Arc::new(Mutex::new(SessionContext {
        provider: Some(ProviderKind::OpenAiChatGpt),
        ..SessionContext::fresh()
    }));
    ensure_active_agent_runtime(
        &bootstrap,
        &session,
        &Arc::new(Mutex::new(rotation_dispatcher())),
    )
    .unwrap();
    let router = TuiRuntimeRouter::new(
        bootstrap,
        Arc::clone(&session),
        Arc::new(Mutex::new(None)),
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
    );

    assert!(router.disconnect().is_ok());
    assert_eq!(session.lock().unwrap().provider, None);
    assert!(session.lock().unwrap().chatgpt_unavailable);
    assert!(session.lock().unwrap().active_agent.is_none());
    let error = match router.turn_bootstrap() {
        Ok(_) => panic!("disconnected ChatGPT runtime must be unavailable"),
        Err(error) => error,
    };
    assert_eq!(
        error.to_string(),
        "auth: ChatGPT credentials are unavailable; run /connect"
    );
    assert!(
        !std::fs::read_to_string(&credentials_path)
            .unwrap()
            .contains("old-access")
    );

    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn runtime_chatgpt_refresh_atomicity_fails_closed_when_credential_restore_fails() {
    let temporary = tui_session_directory("restore-failure");
    let config_home = temporary.join("config");
    let credentials_path = config_home.join("auth.json");
    std::fs::create_dir_all(&config_home).unwrap();
    std::fs::write(
        &credentials_path,
        r#"{"openai-api":{"api_key":"preserved"}}"#,
    )
    .unwrap();
    let mut bootstrap = tui_session_bootstrap(&temporary, &[]);
    bootstrap.provider_source = ProviderSource::Auto;
    bootstrap.provider_type = Some("openai-api".into());
    bootstrap.api_key = Some("preserved".into());
    let session = Arc::new(Mutex::new(SessionContext {
        running: true,
        ..SessionContext::fresh()
    }));
    let router = TuiRuntimeRouter::with_auth_coordinator(
        bootstrap,
        Arc::clone(&session),
        Arc::new(Mutex::new(None)),
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
        ChatGptAuthCoordinator::with_authenticator(|_, _, _| {
            Ok(test_chatgpt_credentials("new-access"))
        }),
    )
    .with_credential_restorer(|_, _| Err(CliError::storage("injected restore failure")));

    let outcome =
        auth_route_outcome(router.connect(ChatGptAuthFlow::Browser, std::sync::mpsc::channel().0));
    assert!(matches!(
        outcome,
        TuiSubmissionOutcome::LocalActionableError { message, .. }
            if message == "store: ChatGPT credential recovery failed"
    ));
    assert!(session.lock().unwrap().chatgpt_unavailable);

    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn runtime_chatgpt_refresh_atomicity_preserves_runtime_on_credential_write_failures() {
    let temporary = tui_session_directory("credential-write-failures");
    let config_home = temporary.join("config");
    std::fs::create_dir_all(&config_home).unwrap();
    let mut bootstrap = tui_session_bootstrap(&temporary, &[]);
    bootstrap.paths.credentials = config_home.clone();
    let session = Arc::new(Mutex::new(SessionContext {
        provider: Some(ProviderKind::OpenAiApi),
        ..SessionContext::fresh()
    }));
    let original_runtime = session.lock().unwrap().clone();
    let router = TuiRuntimeRouter::with_auth_coordinator(
        bootstrap,
        Arc::clone(&session),
        Arc::new(Mutex::new(None)),
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
        ChatGptAuthCoordinator::with_authenticator(|_, _, _| {
            Ok(test_chatgpt_credentials("new-access"))
        }),
    );

    for outcome in [
        auth_route_outcome(router.connect(ChatGptAuthFlow::Browser, std::sync::mpsc::channel().0)),
        auth_route_outcome(router.disconnect()),
    ] {
        assert!(matches!(
            outcome,
            TuiSubmissionOutcome::LocalActionableError { message, .. }
                if message == "ChatGPT credentials could not be saved"
        ));
        assert_eq!(*session.lock().unwrap(), original_runtime);
    }

    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn runtime_chatgpt_refresh_atomicity_leaves_auto_unavailable_after_disconnect_rebuild_failure() {
    let temporary = tui_session_directory("auto-disconnect-failure");
    let config_home = temporary.join("config");
    let credentials_path = config_home.join("auth.json");
    std::fs::create_dir_all(&config_home).unwrap();
    std::fs::write(
        &credentials_path,
        r#"{"openai-chatgpt":{"access_token":"old-access","refresh_token":"old-refresh","account_id":"old-account","expires_at":"2099-01-01T00:00:00Z"}}"#,
    )
    .unwrap();
    let mut bootstrap = tui_session_bootstrap(&temporary, &[]);
    bootstrap.provider_source = ProviderSource::Auto;
    bootstrap.provider_type = Some("openai-chatgpt".into());
    let session = Arc::new(Mutex::new(SessionContext {
        provider: Some(ProviderKind::OpenAiChatGpt),
        ..SessionContext::fresh()
    }));
    let router = TuiRuntimeRouter::new(
        bootstrap,
        Arc::clone(&session),
        Arc::new(Mutex::new(None)),
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
    );

    assert!(router.disconnect().is_err());
    assert!(session.lock().unwrap().chatgpt_unavailable);
    assert!(
        !std::fs::read_to_string(&credentials_path)
            .unwrap()
            .contains("old-access")
    );

    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn dangerous_mode_is_visible_press_once_and_next_turn_only() {
    let temporary = tui_session_directory("dangerous-mode");
    let bootstrap = tui_session_bootstrap(&temporary, &[]);
    let session = Arc::new(Mutex::new(SessionContext::fresh()));
    let router = TuiRuntimeRouter::new(
        bootstrap.clone(),
        Arc::clone(&session),
        Arc::new(Mutex::new(None)),
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
    );
    let mut tui = Tui::new(ProductionTuiEngine {
        cancellation: Arc::new(Mutex::new(None)),
    });
    tui.set_presentation("openai-api", "gpt-4.1", "new session");

    assert!(!render_tui_test_backend(&tui, 120, 24).contains("agens safe"));

    let Action::OpenDialog(route_id) = tui.handle(Event::Key(Key::CtrlShiftD)) else {
        panic!("Ctrl+Shift+D should route through the dangerous-mode router path");
    };
    assert_eq!(route_id, "dangerous");
    assert!(
        tui.apply_submission_outcome(router.route_request(
            TuiRouteRequest::OpenDialog(route_id),
            std::sync::mpsc::channel().0,
        ))
        .is_none()
    );
    assert!(session.lock().unwrap().dangerous_mode);
    assert!(render_tui_test_backend(&tui, 120, 24).contains("danger"));

    assert!(
        tui.apply_submission_outcome(router.route("/dangerous".into()))
            .is_none()
    );
    assert!(!session.lock().unwrap().dangerous_mode);
    assert!(!render_tui_test_backend(&tui, 120, 24).contains("agens safe"));

    tui.apply_submission_outcome(router.route("/dangerous".into()));
    let result = run_tui_prompt_with(
        &bootstrap,
        "next request",
        &session,
        None,
        None,
        |request| {
            assert!(request.dangerous_mode);
            assert!(matches!(
                router.route("/dangerous".into()),
                TuiSubmissionOutcome::ContextChanged { .. }
            ));
            assert!(request.dangerous_mode);
            Ok(HeadlessChatCompletion {
                text: "captured".into(),
                metadata: SessionMetadata {
                    id: 1,
                    project: "project".into(),
                    title: "captured".into(),
                    active_agent: "primary".into(),
                    provider_id: None,
                    model_id: None,
                    reasoning_effort: None,
                    created_at: 1,
                    updated_at: 1,
                    completed_turn_count: 1,
                    resumable: true,
                    parent_session_id: None,
                    fork_message_count: None,
                },
                messages: Vec::new(),
            })
        },
    );
    assert!(result.is_ok());
    assert!(!session.lock().unwrap().dangerous_mode);

    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn bypass_command_and_keybinding_toggle_both_ways_and_report_state() {
    let temporary = tui_session_directory("bypass-toggle");
    let bootstrap = tui_session_bootstrap(&temporary, &[]);
    let session = Arc::new(Mutex::new(SessionContext::fresh()));
    let router = TuiRuntimeRouter::new(
        bootstrap.clone(),
        Arc::clone(&session),
        Arc::new(Mutex::new(None)),
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
    );
    let mut tui = Tui::new(ProductionTuiEngine {
        cancellation: Arc::new(Mutex::new(None)),
    });
    tui.set_presentation("openai-api", "gpt-4.1", "new session");

    assert!(!session.lock().unwrap().bypass_permissions);

    let Action::OpenDialog(route_id) = tui.handle(Event::Key(Key::CtrlShiftP)) else {
        panic!("Ctrl+Shift+P should route through the bypass-mode router path");
    };
    assert_eq!(route_id, "bypass");
    let outcome = router.route_request(
        TuiRouteRequest::OpenDialog(route_id),
        std::sync::mpsc::channel().0,
    );
    let TuiSubmissionOutcome::ContextChanged { message, .. } = &outcome else {
        panic!("Ctrl+Shift+P must report the resulting bypass state");
    };
    assert_eq!(message, "Permission bypass: on.");
    assert!(tui.apply_submission_outcome(outcome).is_none());
    assert!(session.lock().unwrap().bypass_permissions);

    let outcome = router.route("/bypass".into());
    let TuiSubmissionOutcome::ContextChanged { message, .. } = &outcome else {
        panic!("/bypass must report the resulting bypass state");
    };
    assert_eq!(message, "Permission bypass: off.");
    assert!(!session.lock().unwrap().bypass_permissions);

    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn bypass_toggle_writes_through_once_a_session_row_exists_and_never_mutates_config_files() {
    let temporary = tui_session_directory("bypass-write-through");
    let bootstrap = tui_session_bootstrap(&temporary, &[]);
    let global_config = bootstrap.paths.global_config.clone();
    let global_before = std::fs::read(&global_config).unwrap_or_default();

    let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
    let metadata = persist_tui_session(&mut store, &tui_project(&temporary), "bypass-session");
    drop(store);

    let session = Arc::new(Mutex::new(SessionContext::fresh()));
    session.lock().unwrap().identifier = Some(metadata.id);
    let router = TuiRuntimeRouter::new(
        bootstrap.clone(),
        Arc::clone(&session),
        Arc::new(Mutex::new(None)),
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
    );

    assert!(matches!(
        router.route("/bypass".into()),
        TuiSubmissionOutcome::ContextChanged { .. }
    ));
    assert!(session.lock().unwrap().bypass_permissions);

    let store = SessionStore::open(bootstrap.data_directory()).unwrap();
    assert_eq!(
        store.bypass_permission_prompts(metadata.id).unwrap(),
        Some(true)
    );
    drop(store);

    assert_eq!(
        std::fs::read(&global_config).unwrap_or_default(),
        global_before,
        "toggling bypass must never write configuration to disk"
    );

    std::fs::remove_dir_all(temporary).unwrap();
}

/// Pins the WARNING fix: a write-through failure must not resolve to a stale/re-enabled "on" and
/// must not be silently swallowed the way it was before. The toggle still takes effect in memory
/// (this is the one call site the user directly acted on), but its confirmation message now names
/// the failure instead of reporting success as if the value had actually been persisted.
#[test]
#[cfg(unix)]
fn bypass_toggle_surfaces_a_failed_write_through_without_erroring_or_losing_the_in_memory_toggle() {
    use std::os::unix::fs::PermissionsExt;

    let temporary = tui_session_directory("bypass-write-through-failure");
    let bootstrap = tui_session_bootstrap(&temporary, &[]);

    let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
    let metadata = persist_tui_session(&mut store, &tui_project(&temporary), "bypass-session");
    drop(store);

    let session = Arc::new(Mutex::new(SessionContext::fresh()));
    session.lock().unwrap().identifier = Some(metadata.id);
    let router = TuiRuntimeRouter::new(
        bootstrap.clone(),
        Arc::clone(&session),
        Arc::new(Mutex::new(None)),
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
    );

    let data_directory = bootstrap.data_directory().to_path_buf();
    let original_mode = std::fs::metadata(&data_directory)
        .unwrap()
        .permissions()
        .mode();
    std::fs::set_permissions(&data_directory, std::fs::Permissions::from_mode(0o000)).unwrap();

    let outcome = router.route("/bypass".into());

    std::fs::set_permissions(
        &data_directory,
        std::fs::Permissions::from_mode(original_mode),
    )
    .unwrap();

    let TuiSubmissionOutcome::ContextChanged { message, .. } = &outcome else {
        panic!(
            "a failed write-through must still resolve as a normal toggle outcome, not an error"
        );
    };
    assert!(
        message.contains("could not be saved"),
        "a failed write-through must be surfaced in the toggle confirmation: {message:?}"
    );
    assert!(
        session.lock().unwrap().bypass_permissions,
        "the in-memory toggle must still take effect even when persisting it failed"
    );

    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn u15_c1a_subagent_overlay_and_alias_expose_all_subagent_agents() {
    let temporary = tui_session_directory("u15-c1a-subagents");
    let bootstrap = tui_session_bootstrap(
        &temporary,
        &[
            (
                "all",
                "---\nname: all\ndescription: all\nmode: all\npermissions: []\n---\nAll work.\n",
            ),
            (
                "primary",
                "---\nname: primary\ndescription: primary\nmode: primary\npermissions: []\n---\nPrimary work.\n",
            ),
            (
                "invalid-model",
                "---\nname: invalid-model\ndescription: invalid\nmode: subagent\nmodel: unavailable\npermissions: []\n---\nInvalid work.\n",
            ),
            (
                "reviewer",
                "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
            ),
        ],
    );
    let session = Arc::new(Mutex::new(SessionContext::fresh()));
    let router = TuiRuntimeRouter::new(
        bootstrap,
        Arc::clone(&session),
        Arc::new(Mutex::new(None)),
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
    );
    let mut tui = Tui::new(ProductionTuiEngine {
        cancellation: Arc::new(Mutex::new(None)),
    });

    assert!(
        router
            .palette_entries()
            .unwrap()
            .iter()
            .any(|entry| entry.name() == "subagent")
    );

    assert!(matches!(
        router.route("/subagent".into()),
        TuiSubmissionOutcome::SafeDialog(_)
    ));
    tui.set_running(true);
    assert!(
        tui.apply_submission_outcome(router.route("/subagent".into()))
            .is_none()
    );
    assert!(tui.view().running);
    let overlay = render_tui_test_backend(&tui, 80, 24);
    assert!(!overlay.contains("main"));
    assert!(overlay.contains("explore"));
    assert!(overlay.contains("general"));
    assert!(overlay.contains("reviewer"));
    assert!(!overlay.contains("all"));
    assert!(!overlay.contains("primary"));
    assert!(overlay.contains("invalid-model"));
    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::DialogAction("subagent:explore".into())
    );
    assert!(tui.transcript().is_empty());

    assert!(matches!(
        router.route("/subagent reviewer".into()),
        TuiSubmissionOutcome::ContextChanged { .. }
    ));
    assert_eq!(
        session.lock().unwrap().selected_subagent.as_deref(),
        Some("reviewer")
    );
    assert!(matches!(
        router.route("/subagent all".into()),
        TuiSubmissionOutcome::LocalActionableError { .. }
    ));

    let unavailable_bootstrap = tui_session_bootstrap_without_provider(
        &temporary,
        &[(
            "unavailable-provider",
            "---\nname: unavailable-provider\ndescription: unavailable\nmode: subagent\npermissions: []\n---\nUnavailable work.\n",
        )],
    );
    let unavailable_session = Arc::new(Mutex::new(SessionContext::fresh()));
    let unavailable_router = TuiRuntimeRouter::new(
        unavailable_bootstrap.clone(),
        Arc::clone(&unavailable_session),
        Arc::new(Mutex::new(None)),
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
    );

    assert!(
        !unavailable_router
            .palette_entries()
            .unwrap()
            .iter()
            .any(|entry| entry.name() == "subagent")
    );

    let unavailable_selection = unavailable_router.route("/subagent unavailable-provider".into());
    assert!(matches!(
        &unavailable_selection,
        TuiSubmissionOutcome::LocalActionableError { message, .. }
            if message.contains("No eligible subagents")
    ));
    assert!(
        unavailable_session
            .lock()
            .unwrap()
            .selected_subagent
            .is_none()
    );
    assert!(unavailable_session.lock().unwrap().messages.is_empty());

    let mut unavailable_tui = Tui::new(ProductionTuiEngine {
        cancellation: Arc::new(Mutex::new(None)),
    });
    let captured = Arc::new(Mutex::new(Vec::new()));
    submit_tui_command(
        &mut unavailable_tui,
        &unavailable_router,
        &unavailable_bootstrap,
        "/subagent unavailable-provider",
        &captured,
    );
    assert!(captured.lock().unwrap().is_empty());
    assert!(!unavailable_tui.view().running);

    let empty_selection =
        unavailable_tui.apply_submission_outcome(unavailable_router.route("/subagent".into()));
    assert_eq!(empty_selection, None);
    let unavailable_overlay = render_tui_test_backend(&unavailable_tui, 80, 24);
    assert!(
        unavailable_overlay.contains("No eligible subagents are available."),
        "{unavailable_overlay:?}"
    );
    assert_eq!(
        unavailable_tui.handle(Event::Key(Key::Enter)),
        Action::Render
    );

    unavailable_tui.apply_submission_outcome(unavailable_router.route("/subagent".into()));
    assert_eq!(
        unavailable_tui.handle(Event::Key(Key::Escape)),
        Action::Render
    );
    assert!(unavailable_tui.transcript().is_empty());
    let unavailable_context = unavailable_session.lock().unwrap();
    assert!(unavailable_context.selected_subagent.is_none());
    assert!(unavailable_context.messages.is_empty());

    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn plural_subagents_command_opens_the_transcript_picker_without_changing_next_type() {
    let temporary = tui_session_directory("plural-subagents");
    let bootstrap = tui_session_bootstrap(&temporary, &[]);
    let session = Arc::new(Mutex::new(SessionContext {
        selected_subagent: Some("explore".into()),
        ..SessionContext::fresh()
    }));
    let router = TuiRuntimeRouter::new(
        bootstrap,
        Arc::clone(&session),
        Arc::new(Mutex::new(None)),
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
    );

    assert!(matches!(
        router.route("/subagents".into()),
        TuiSubmissionOutcome::TranscriptDialog
    ));
    assert_eq!(
        session.lock().unwrap().selected_subagent.as_deref(),
        Some("explore")
    );

    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn tui_model_overlay_labels_source_metadata_current_and_compatible_sets() {
    for (provider, source, included, excluded) in [
        ("openai-api", "OpenAI API", "gpt-4o", "gpt-5.4"),
        (
            "openai-chatgpt",
            "ChatGPT subscription",
            "gpt-5.4",
            "gpt-4o",
        ),
    ] {
        let temporary = tui_session_directory(&format!("model-source-{provider}"));
        let bootstrap = tui_session_bootstrap_for_provider(&temporary, &[], provider, "gpt-5.5");
        let session = Arc::new(Mutex::new(SessionContext::fresh()));
        let cancellation = Arc::new(Mutex::new(None));
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::clone(&cancellation),
        });
        let router = TuiRuntimeRouter::new(
            bootstrap,
            Arc::clone(&session),
            cancellation,
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );
        let (progress, _) = std::sync::mpsc::channel();

        assert!(
            tui.apply_submission_outcome(
                router.route_request(TuiRouteRequest::OpenDialog("model".into()), progress)
            )
            .is_none()
        );
        let text = render_tui_test_backend(&tui, 140, 60);

        assert!(
            text.contains(&format!("All providers · current: {source}")),
            "{provider}: {text:?}"
        );
        assert!(
            text.contains(&format!("gpt-5.5 · {source} (current)")),
            "{provider}: {text:?}"
        );
        assert!(text.contains(included), "{provider}: {text:?}");
        assert!(
            !text.contains(&format!("{excluded} · {source}")),
            "{provider}: {text:?}"
        );
        assert!(text.contains("272K context"), "{provider}: {text:?}");
        assert!(text.contains("128K output"), "{provider}: {text:?}");
        assert!(text.contains("reasoning"), "{provider}: {text:?}");

        let source = if provider == "openai-chatgpt" {
            ModelSource::ChatGptSubscription
        } else {
            ModelSource::OpenAiApi
        };
        let models = ModelSelection::for_source("gpt-5.5", source)
            .models()
            .unwrap();
        let family = models
            .iter()
            .filter(|model| model.id.starts_with("gpt-5.6"))
            .map(|model| {
                (
                    model.id.as_str(),
                    model.name.as_deref(),
                    model.context,
                    model.output,
                    model.reasoning,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            family,
            [
                (
                    "gpt-5.6",
                    Some("GPT-5.6 (Sol alias)"),
                    Some(1_050_000),
                    Some(128_000),
                    Some(true)
                ),
                (
                    "gpt-5.6-luna",
                    Some("GPT-5.6 Luna"),
                    Some(1_050_000),
                    Some(128_000),
                    Some(true)
                ),
                (
                    "gpt-5.6-sol",
                    Some("GPT-5.6 Sol"),
                    Some(1_050_000),
                    Some(128_000),
                    Some(true)
                ),
                (
                    "gpt-5.6-terra",
                    Some("GPT-5.6 Terra"),
                    Some(1_050_000),
                    Some(128_000),
                    Some(true)
                ),
            ],
            "official OpenAI GPT-5.6 catalog metadata for {provider}"
        );
        for model in &family {
            assert_eq!(
                models
                    .iter()
                    .filter(|candidate| candidate.id == model.0)
                    .count(),
                1,
                "duplicate {} in {provider}",
                model.0
            );
        }
        assert!(text.contains("gpt-5.6"), "{provider}: {text:?}");
        assert!(text.contains("gpt-5.6-luna"), "{provider}: {text:?}");
        assert!(
            !text.contains("unverified metadata"),
            "{provider}: {text:?}"
        );

        for _ in 0..4 {
            tui.handle(Event::Key(Key::Down));
        }
        let scrolled = render_tui_test_backend(&tui, 80, 24);
        assert!(scrolled.contains("gpt-5.6-sol"), "{provider}: {scrolled:?}");
        assert!(
            scrolled.contains("gpt-5.6-terra"),
            "{provider}: {scrolled:?}"
        );
        tui.handle(Event::Key(Key::Up));
        tui.handle(Event::Key(Key::Up));
        tui.handle(Event::Key(Key::Up));
        let Action::DialogAction(action_id) = tui.handle(Event::Key(Key::Enter)) else {
            panic!("verified gpt-5.6 alias should be selectable");
        };
        let outcome = router.route_request(
            TuiRouteRequest::DialogAction(action_id),
            std::sync::mpsc::channel().0,
        );
        assert!(matches!(
            &outcome,
            TuiSubmissionOutcome::ContextChanged { message, presentation }
                if message == "Model: gpt-5.6."
                    && presentation
                        == &TuiPresentation::new(provider, "gpt-5.6", "new session")
                            .with_effort("medium")
                            .with_context_window(Some(1_050_000))
        ));
        tui.apply_submission_outcome(outcome);
        let selection = session.lock().unwrap().selection.clone().unwrap();
        assert!(selection.metadata_known());
        assert_eq!(selection.reasoning_effort_default(), Some("medium"));
        assert_eq!(
            selection.reasoning_effort_values(),
            ["default", "none", "low", "medium", "high", "xhigh", "max"]
        );

        tui.apply_submission_outcome(router.open_dialog("model").unwrap());
        tui.handle(Event::Key(Key::Char('/')));
        for character in "gpt-5.6-sol".chars() {
            tui.handle(Event::Key(Key::Char(character)));
        }
        let filtered = render_tui_test_backend(&tui, 80, 24);
        assert!(filtered.contains("gpt-5.6-sol"), "{provider}: {filtered:?}");
        assert!(
            !filtered.contains("unverified metadata"),
            "{provider}: {filtered:?}"
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }
}

#[test]
fn tui_provider_overlay_filters_unavailable_entries_and_switches_without_history() {
    let temporary = tui_session_directory("provider-overlay");
    let bootstrap = tui_session_bootstrap_for_provider(&temporary, &[], "openai-api", "gpt-5.5");
    std::fs::write(
        &bootstrap.paths.credentials,
        r#"{"openai-chatgpt":{"access_token":"secret-access","refresh_token":"secret-refresh","account_id":"account","expires_at":"2099-01-01T00:00:00Z"}}"#,
    )
    .unwrap();
    let session = Arc::new(Mutex::new(SessionContext::fresh()));
    let router = TuiRuntimeRouter::with_credential_resolver(
        bootstrap,
        Arc::clone(&session),
        Arc::new(Mutex::new(None)),
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
        CredentialResolver::with_environment(BTreeMap::new()),
    );
    let mut tui = Tui::new(ProductionTuiEngine {
        cancellation: Arc::new(Mutex::new(None)),
    });
    let (progress, _) = std::sync::mpsc::channel();
    tui.apply_submission_outcome(router.route_request(
        TuiRouteRequest::OpenDialog("provider".into()),
        progress.clone(),
    ));
    let overlay = render_tui_test_backend(&tui, 80, 24);
    assert!(
        overlay.contains("Current: OpenAI API · credential required"),
        "{overlay:?}"
    );
    assert!(overlay.contains("▌ ChatGPT subscription"), "{overlay:?}");
    assert!(overlay.contains("ready"), "{overlay:?}");
    assert!(!overlay.contains("OpenAI API (current)"), "{overlay:?}");
    assert!(!overlay.contains("secret-"), "{overlay:?}");

    dispatch_tui_dialog_selection(&mut tui, &router, progress);
    assert_eq!(tui.view().provider_model, "openai-chatgpt / gpt-5.5");
    tui.apply_submission_outcome(router.open_dialog("model").unwrap());
    let model_overlay = render_tui_test_backend(&tui, 140, 40);
    assert!(model_overlay.contains("All providers · current: ChatGPT subscription"));
    assert!(model_overlay.contains("gpt-5.5 · ChatGPT subscription (current)"));
    tui.handle(Event::Key(Key::Char('/')));
    for character in "kimi".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }
    let searched = render_tui_test_backend(&tui, 140, 40);
    assert!(
        searched.contains("kimi-k3 · Moonshot AI"),
        "searching reaches another provider's models: {searched:?}"
    );
    assert!(
        !searched.contains("gpt-5.5 · ChatGPT"),
        "the search filters the active provider out too: {searched:?}"
    );
    assert!(tui.transcript().is_empty());
    assert!(session.lock().unwrap().messages.is_empty());

    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn tui_provider_switch_reconciles_compatible_incompatible_and_busy_state_atomically() {
    let temporary = tui_session_directory("provider-reconcile");
    let bootstrap = tui_session_bootstrap_for_provider(&temporary, &[], "openai-api", "gpt-5.5");
    std::fs::write(
        &bootstrap.paths.credentials,
        r#"{"openai-chatgpt":{"access_token":"access","refresh_token":"refresh","account_id":"account","expires_at":"2099-01-01T00:00:00Z"}}"#,
    )
    .unwrap();
    let session = Arc::new(Mutex::new(SessionContext::fresh()));
    let router = TuiRuntimeRouter::with_credential_resolver(
        bootstrap,
        Arc::clone(&session),
        Arc::new(Mutex::new(None)),
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
        CredentialResolver::with_environment(BTreeMap::from([(
            "OPENAI_API_KEY".into(),
            "api-secret".into(),
        )])),
    );

    let retained = router.route("/provider openai-chatgpt".into());
    assert!(
        matches!(retained, TuiSubmissionOutcome::ContextChanged { ref message, .. } if message.contains("Model retained: gpt-5.5"))
    );
    router.route("/model gpt-5.4".into());
    router.route("/effort high".into());
    let reset = router.route("/provider openai-api".into());
    assert!(
        matches!(reset, TuiSubmissionOutcome::ContextChanged { ref message, .. } if message.contains("Model reset to gpt-4.1") && message.contains("Default"))
    );
    let idle = session.lock().unwrap().clone();
    assert_eq!(idle.selection.as_ref().unwrap().model(), "gpt-4.1");
    assert_eq!(idle.selection.as_ref().unwrap().reasoning_effort(), None);
    let mut context = session.lock().unwrap();
    context.messages = tui_session_messages();
    context.running = true;
    drop(context);
    let busy = session.lock().unwrap().clone();
    assert!(matches!(
        router.route("/provider openai-chatgpt".into()),
        TuiSubmissionOutcome::LocalActionableError { .. }
    ));
    assert_eq!(*session.lock().unwrap(), busy);

    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn tui_provider_switch_to_moonshot_resets_to_kimi_k3_without_panicking() {
    let temporary = tui_session_directory("provider-moonshot-switch");
    let bootstrap = tui_session_bootstrap_for_provider(&temporary, &[], "openai-api", "gpt-5.5");
    let session = Arc::new(Mutex::new(SessionContext::fresh()));
    let router = TuiRuntimeRouter::with_credential_resolver(
        bootstrap,
        Arc::clone(&session),
        Arc::new(Mutex::new(None)),
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
        CredentialResolver::with_environment(BTreeMap::from([(
            "MOONSHOT_API_KEY".into(),
            "moonshot-secret".into(),
        )])),
    );

    let reset = router.route("/provider moonshotai".into());
    assert!(
        matches!(reset, TuiSubmissionOutcome::ContextChanged { ref message, .. } if message.contains("Model reset to kimi-k3")),
        "{reset:?}"
    );
    let idle = session.lock().unwrap().clone();
    assert_eq!(idle.selection.as_ref().unwrap().model(), "kimi-k3");

    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn tui_provider_overlay_lists_moonshot_when_credentials_are_ready() {
    let temporary = tui_session_directory("provider-overlay-moonshot");
    let bootstrap = tui_session_bootstrap_for_provider(&temporary, &[], "openai-api", "gpt-5.5");
    let session = Arc::new(Mutex::new(SessionContext::fresh()));
    let router = TuiRuntimeRouter::with_credential_resolver(
        bootstrap,
        Arc::clone(&session),
        Arc::new(Mutex::new(None)),
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
        CredentialResolver::with_environment(BTreeMap::from([(
            "MOONSHOT_API_KEY".into(),
            "moonshot-secret".into(),
        )])),
    );
    let mut tui = Tui::new(ProductionTuiEngine {
        cancellation: Arc::new(Mutex::new(None)),
    });
    let (progress, _) = std::sync::mpsc::channel();
    tui.apply_submission_outcome(
        router.route_request(TuiRouteRequest::OpenDialog("provider".into()), progress),
    );
    let overlay = render_tui_test_backend(&tui, 80, 24);
    assert!(overlay.contains("Moonshot AI"), "{overlay:?}");
    assert!(!overlay.contains("moonshot-secret"), "{overlay:?}");

    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn tui_turn_bootstrap_resolves_moonshot_api_key_from_environment() {
    let temporary = tui_session_directory("turn-bootstrap-moonshot");
    let bootstrap = tui_session_bootstrap_for_provider(&temporary, &[], "openai-api", "gpt-5.5");
    let session = Arc::new(Mutex::new(SessionContext::fresh()));
    session.lock().unwrap().provider = Some(ProviderKind::Moonshot);
    let router = TuiRuntimeRouter::with_credential_resolver(
        bootstrap,
        Arc::clone(&session),
        Arc::new(Mutex::new(None)),
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
        CredentialResolver::with_environment(BTreeMap::from([(
            "MOONSHOT_API_KEY".into(),
            "moonshot-secret".into(),
        )])),
    );

    let resolved = router.turn_bootstrap().unwrap();
    assert_eq!(resolved.provider_type.as_deref(), Some("moonshotai"));
    assert_eq!(resolved.api_key.as_deref(), Some("moonshot-secret"));

    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn tui_turn_bootstrap_resolves_changed_and_removed_credentials_without_stale_reuse() {
    let temporary = tui_session_directory("fresh-turn-credentials");
    let bootstrap = tui_session_bootstrap(&temporary, &[]);
    let configured_provider = bootstrap.provider_type.clone();
    let credentials = bootstrap.paths.credentials.clone();
    let environment = Arc::new(Mutex::new(BTreeMap::new()));
    let resolver = CredentialResolver::with_environment_resolver({
        let environment = Arc::clone(&environment);
        move || environment.lock().unwrap().clone()
    });
    let session = Arc::new(Mutex::new(SessionContext::fresh()));
    let router = TuiRuntimeRouter::with_credential_resolver(
        bootstrap,
        Arc::clone(&session),
        Arc::new(Mutex::new(None)),
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
        resolver,
    );

    std::fs::write(&credentials, r#"{"openai-api":{"api_key":"file-one"}}"#).unwrap();
    assert_eq!(
        router.turn_bootstrap().unwrap().api_key.as_deref(),
        Some("file-one")
    );
    std::fs::write(&credentials, r#"{"openai-api":{"api_key":"file-two"}}"#).unwrap();
    assert_eq!(
        router.turn_bootstrap().unwrap().api_key.as_deref(),
        Some("file-two")
    );
    environment
        .lock()
        .unwrap()
        .insert("OPENAI_API_KEY".into(), "env-current".into());
    assert_eq!(
        router.turn_bootstrap().unwrap().api_key.as_deref(),
        Some("env-current")
    );
    environment.lock().unwrap().clear();
    std::fs::remove_file(&credentials).unwrap();
    assert!(router.turn_bootstrap().is_err());

    session.lock().unwrap().provider = Some(ProviderKind::OpenAiChatGpt);
    std::fs::write(
        &credentials,
        r#"{"openai-chatgpt":{"access_token":"chat-access","refresh_token":"chat-refresh","account_id":"account","expires_at":"2099-01-01T00:00:00Z"}}"#,
    )
    .unwrap();
    assert_eq!(
        router.turn_bootstrap().unwrap().provider_type(),
        Some("openai-chatgpt")
    );
    std::fs::remove_file(&credentials).unwrap();
    assert!(router.turn_bootstrap().is_err());
    assert!(matches!(
        router.route("/provider openai-chatgpt".into()),
        TuiSubmissionOutcome::LocalActionableError { ref message, .. }
            if message.contains("run /connect")
    ));
    assert_eq!(
        router.bootstrap().unwrap().provider_type,
        configured_provider
    );
    assert!(session.lock().unwrap().messages.is_empty());

    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn tui_model_overlay_selects_exact_future_id_with_unknown_metadata_and_default_effort() {
    let temporary = tui_session_directory("unverified-model");
    let bootstrap = tui_session_bootstrap_for_provider(&temporary, &[], "openai-api", "gpt-5.5");
    let session = Arc::new(Mutex::new(SessionContext::fresh()));
    let cancellation = Arc::new(Mutex::new(None));
    let mut tui = Tui::new(ProductionTuiEngine {
        cancellation: Arc::clone(&cancellation),
    });
    let router = TuiRuntimeRouter::new(
        bootstrap,
        Arc::clone(&session),
        cancellation,
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
    );
    let (progress, _) = std::sync::mpsc::channel();
    assert!(matches!(
        router.route("/effort xhigh".into()),
        TuiSubmissionOutcome::ContextChanged { .. }
    ));
    tui.apply_submission_outcome(router.route_request(
        TuiRouteRequest::OpenDialog("model".into()),
        progress.clone(),
    ));

    tui.handle(Event::Key(Key::Char('/')));
    for character in "gpt-future-1".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }
    let overlay = render_tui_test_backend(&tui, 80, 24);
    assert!(
        overlay.contains("Use gpt-future-1 (unverified metadata)"),
        "{overlay:?}"
    );
    let Action::DialogAction(action_id) = tui.handle(Event::Key(Key::Enter)) else {
        panic!("unverified model should dispatch a local action");
    };
    let outcome = router.route_request(TuiRouteRequest::DialogAction(action_id), progress);
    let TuiSubmissionOutcome::ContextChanged {
        message,
        presentation,
    } = &outcome
    else {
        panic!("unverified model should update session context");
    };
    assert_eq!(
        message,
        "Model: gpt-future-1 (unverified metadata). Reasoning effort reset to Default."
    );
    assert_eq!(
        presentation,
        &TuiPresentation::new("openai-api", "gpt-future-1", "new session")
    );
    tui.apply_submission_outcome(outcome);

    let selection = session.lock().unwrap().selection.clone().unwrap();
    assert_eq!(selection.model(), "gpt-future-1");
    assert!(!selection.metadata_known());
    assert_eq!(selection.reasoning_effort(), None);
    assert_eq!(
        selection.request_config(),
        &agens_core::RequestConfig::default()
    );
    assert!(session.lock().unwrap().messages.is_empty());
    assert!(tui.transcript().is_empty());

    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn tui_effort_overlay_and_model_change_use_grounded_sets_and_atomic_reset() {
    let temporary = tui_session_directory("effort-capabilities");
    let bootstrap = tui_session_bootstrap_for_provider(&temporary, &[], "openai-api", "gpt-5.5");
    let session = Arc::new(Mutex::new(SessionContext::fresh()));
    let cancellation = Arc::new(Mutex::new(None));
    let mut tui = Tui::new(ProductionTuiEngine {
        cancellation: Arc::clone(&cancellation),
    });
    let router = TuiRuntimeRouter::new(
        bootstrap,
        Arc::clone(&session),
        cancellation,
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
    );
    let (progress, _) = std::sync::mpsc::channel();

    assert_eq!(
        router.route("/effort xhigh".into()),
        TuiSubmissionOutcome::ContextChanged {
            message: "Reasoning effort: xhigh.".into(),
            presentation: router.presentation().unwrap(),
        }
    );
    assert!(
        tui.apply_submission_outcome(
            router.route_request(TuiRouteRequest::OpenDialog("effort".into()), progress)
        )
        .is_none()
    );
    let overlay = render_tui_test_backend(&tui, 80, 24);
    assert!(overlay.contains("Default"), "{overlay:?}");
    assert!(overlay.contains("xhigh (current)"), "{overlay:?}");
    assert!(!overlay.contains("minimal"), "{overlay:?}");

    let reset = router.route("/model gpt-4.1".into());
    let TuiSubmissionOutcome::ContextChanged { message, .. } = reset else {
        panic!("model change should be local context information");
    };
    assert_eq!(
        message,
        "Model: gpt-4.1. Reasoning effort reset to Default because xhigh is unsupported."
    );
    let selection = session.lock().unwrap().selection.clone().unwrap();
    assert_eq!(selection.model(), "gpt-4.1");
    assert_eq!(selection.reasoning_effort(), None);
    assert_eq!(selection.request_config().reasoning_effort(), None);

    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn tui_sessions_resume_and_agent_overlays_filter_navigate_cancel_and_apply_typed_outcomes() {
    let temporary = tui_session_directory("session-agent-overlays");
    let bootstrap = tui_session_bootstrap(
        &temporary,
        &[
            (
                "all",
                "---\nname: all\ndescription: all\nmode: all\npermissions: []\n---\nAll work.\n",
            ),
            (
                "reviewer",
                "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
            ),
        ],
    );
    let session = Arc::new(Mutex::new(SessionContext::fresh()));
    let cancellation = Arc::new(Mutex::new(None));
    let mut tui = Tui::new(ProductionTuiEngine {
        cancellation: Arc::clone(&cancellation),
    });
    let router = TuiRuntimeRouter::new(
        bootstrap.clone(),
        Arc::clone(&session),
        cancellation,
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
    );
    tui.set_palette_entries(router.palette_entries().unwrap());
    let (progress, _) = std::sync::mpsc::channel();

    let empty = router.route_request(
        agens_tui::TuiRouteRequest::OpenDialog("sessions".into()),
        progress.clone(),
    );
    tui.apply_submission_outcome(empty);
    assert!(
        render_tui_test_backend(&tui, 80, 24).contains("No resumable sessions in current project.")
    );
    tui.handle(Event::Key(Key::Escape));

    let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
    let current = persist_tui_session(&mut store, &tui_project(&temporary), "current");
    let other = persist_tui_session(
        &mut store,
        &temporary.join("other").display().to_string(),
        "other",
    );
    drop(store);

    open_tui_palette_dialog(&mut tui, &router, "/se", "sessions", progress.clone());
    let sessions = render_tui_test_backend(&tui, 80, 24);
    assert!(sessions.contains(&format!("#{} current", current.id)));
    assert!(!sessions.contains(&format!("#{} other", other.id)));
    let original = session.lock().unwrap().clone();
    assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::Render);
    assert_eq!(*session.lock().unwrap(), original);

    open_tui_palette_dialog(&mut tui, &router, "/re", "sessions", progress.clone());
    dispatch_tui_dialog_selection(&mut tui, &router, progress.clone());
    assert_eq!(tui.view().session, format!("session #{}", current.id));
    assert!(tui.transcript().is_empty());
    assert!(
        tui.view()
            .status
            .is_some_and(|status| status.contains("Resumed session"))
    );

    open_tui_palette_dialog(&mut tui, &router, "/ag", "agent", progress.clone());
    let agents = render_tui_test_backend(&tui, 80, 24);
    assert!(agents.contains("primary (current)"), "{agents:?}");
    tui.handle(Event::Key(Key::Down));
    dispatch_tui_dialog_selection(&mut tui, &router, progress);

    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn dialog_recovery_is_confirmed_private_local_safe_and_retryable() {
    let temporary = tui_session_directory("recovery-dialog");
    let bootstrap = tui_session_bootstrap(&temporary, &[]);
    let session = Arc::new(Mutex::new(SessionContext::fresh()));
    let cancellation = Arc::new(Mutex::new(None));
    let router = TuiRuntimeRouter::new(
        bootstrap.clone(),
        Arc::clone(&session),
        Arc::clone(&cancellation),
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
    );
    let metadata = SessionMetadata {
        id: 1,
        project: tui_project(&temporary),
        title: "Interrupted session".into(),
        active_agent: "primary".into(),
        provider_id: None,
        model_id: None,
        reasoning_effort: None,
        created_at: 1,
        updated_at: 7,
        completed_turn_count: 0,
        resumable: false,
        parent_session_id: None,
        fork_message_count: None,
    };
    let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
    let attempt = store
        .begin_session_attempt(&metadata, "SENTINEL_PRIVATE_RETRY".into())
        .unwrap();
    drop(store);

    let confirmation = router.route_dialog_action("session:1", std::sync::mpsc::channel().0);
    let confirmation_debug = format!("{confirmation:?}");
    let mut tui = Tui::new(ProductionTuiEngine { cancellation });
    assert!(tui.apply_submission_outcome(confirmation).is_none());
    let confirmation_text = render_tui_test_backend(&tui, 100, 24);
    assert!(confirmation_text.contains("Recover interrupted attempt"));
    assert!(confirmation_text.contains("Interrupted session"));
    assert!(confirmation_text.contains("ID: 1"));
    assert!(confirmation_text.contains("Status: running"));
    assert!(confirmation_text.contains("Started: 7"));
    assert!(
        confirmation_debug
            .contains("This may invalidate an attempt still running in another process.")
    );
    assert!(!confirmation_debug.contains("SENTINEL_PRIVATE_RETRY"));

    assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::Render);
    let store = SessionStore::open(bootstrap.data_directory()).unwrap();
    assert_eq!(
        store
            .load_session_for_resume(1)
            .unwrap()
            .latest_attempt
            .unwrap()
            .status(),
        agens_core::SessionAttemptStatus::Running
    );
    drop(store);

    let locally_active_metadata = SessionMetadata {
        id: 2,
        ..metadata.clone()
    };
    let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
    let locally_active = active_session_attempts()
        .begin_and_register(
            &mut store,
            &locally_active_metadata,
            "local private retry".into(),
        )
        .unwrap();
    drop(store);
    let local_refusal = router.route_dialog_action(
        &format!(
            "session:recover:{}:{}",
            locally_active.key().session_id(),
            locally_active.key().attempt_id()
        ),
        std::sync::mpsc::channel().0,
    );
    assert!(matches!(local_refusal, TuiSubmissionOutcome::Dialog(_)));
    let store = SessionStore::open(bootstrap.data_directory()).unwrap();
    let database_path = store.database_path();
    assert_eq!(
        store
            .load_session_for_resume(locally_active.key().session_id())
            .unwrap()
            .latest_attempt
            .unwrap()
            .status(),
        agens_core::SessionAttemptStatus::Running
    );
    drop(store);
    active_session_attempts().unregister(&database_path, locally_active.key());

    let recovered = router.route_dialog_action(
        &format!(
            "session:recover:{}:{}",
            attempt.key().session_id(),
            attempt.key().attempt_id()
        ),
        std::sync::mpsc::channel().0,
    );
    assert!(matches!(
        recovered,
        TuiSubmissionOutcome::ProviderTurn { ref display, ref prompt }
            if display == "Retrying recovered attempt." && prompt == "SENTINEL_PRIVATE_RETRY"
    ));
    assert_eq!(session.lock().unwrap().identifier, Some(1));
    let store = SessionStore::open(bootstrap.data_directory()).unwrap();
    assert_eq!(
        store
            .load_session_for_resume(1)
            .unwrap()
            .latest_attempt
            .unwrap()
            .status(),
        agens_core::SessionAttemptStatus::Interrupted
    );

    let stale = router.route_dialog_action(
        &format!(
            "session:recover:{}:{}",
            attempt.key().session_id(),
            attempt.key().attempt_id()
        ),
        std::sync::mpsc::channel().0,
    );
    assert!(matches!(stale, TuiSubmissionOutcome::Dialog(_)));

    std::fs::remove_dir_all(temporary).unwrap();
}

fn dispatch_tui_session_page(
    tui: &mut Tui<ProductionTuiEngine>,
    router: &TuiRuntimeRouter,
    action: Action,
    progress: std::sync::mpsc::Sender<TuiRouteProgress>,
) {
    let Action::LoadSessionPage(request) = action else {
        panic!("session dialog action should request a page");
    };
    let outcome = router.route_request(TuiRouteRequest::SessionPage(request), progress);
    assert!(tui.apply_submission_outcome(outcome).is_none());
}

#[test]
fn session_overlay_uses_real_metadata_scope_search_sort_clock_and_atomic_failure() {
    let temporary = tui_session_directory("session-metadata-overlay");
    let bootstrap = tui_session_bootstrap(&temporary, &[]);
    let project = tui_project(&temporary);
    let other_project = temporary.join("other-root").display().to_string();
    let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
    let old = persist_tui_session_metadata(&mut store, &project, "Alpha", "primary", 9_900);
    let other = persist_tui_session_metadata(&mut store, &other_project, "Beta", "build", 9_950);
    let mut current =
        persist_tui_session_metadata(&mut store, &project, "Gamma", "reviewer", 9_950);
    current.provider_id = Some("openai-chatgpt".into());
    current.model_id = Some("gpt-5.5".into());
    current.reasoning_effort = Some(agens_core::ReasoningEffort::High);
    store.update_session_selection(&current).unwrap();
    drop(store);

    let session = Arc::new(Mutex::new(SessionContext {
        identifier: Some(current.id),
        ..SessionContext::fresh()
    }));
    let cancellation = Arc::new(Mutex::new(None));
    let mut tui = Tui::new(ProductionTuiEngine {
        cancellation: Arc::clone(&cancellation),
    });
    tui.set_presentation("openai-api", "gpt-4.1", format!("session #{}", current.id));
    tui.replace_history(&tui_session_messages()).unwrap();
    let router = TuiRuntimeRouter::with_clock(
        bootstrap.clone(),
        Arc::clone(&session),
        cancellation,
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
        || 10_000,
    );
    tui.set_palette_entries(router.palette_entries().unwrap());
    let (progress, _) = std::sync::mpsc::channel();
    let original_context = session.lock().unwrap().clone();

    open_tui_palette_dialog(&mut tui, &router, "/se", "sessions", progress.clone());
    let project_rows = render_tui_test_backend(&tui, 100, 26);
    assert!(project_rows.contains("Resume session · Current project"));
    assert!(project_rows.contains(&format!("#{} Gamma", current.id)));
    assert!(project_rows.contains(&format!("#{} Alpha", old.id)));
    assert!(
        project_rows.contains("1 turn · 50s ago"),
        "{project_rows:?}"
    );
    assert!(!project_rows.contains("reviewer"), "{project_rows:?}");
    assert!(!project_rows.contains("Provider:"), "{project_rows:?}");
    assert!(!project_rows.contains("Model:"), "{project_rows:?}");
    assert!(!project_rows.contains("Effort:"), "{project_rows:?}");
    assert!(!project_rows.contains("Updated:"), "{project_rows:?}");
    tui.handle(Event::Key(Key::CtrlO));
    let project_details = render_tui_test_backend(&tui, 100, 26);
    assert!(
        project_details.contains("Provider: openai-chatgpt · Model: gpt-5.5"),
        "{project_details:?}"
    );
    assert!(
        project_details.contains("Effort: high · Updated: 9950 (50s ago)"),
        "{project_details:?}"
    );
    let old_details = format!(
        "{:?}",
        session_dialog_entry(
            &StoredSession {
                metadata: old.clone(),
                messages: Vec::new(),
                latest_attempt: None,
            },
            None,
            false,
            10_000,
        )
    );
    assert!(old_details.contains("Provider: current runtime"));
    assert!(old_details.contains("Model: current runtime"));
    assert!(old_details.contains("Effort: current runtime"));
    assert!(project_rows.find("Gamma").unwrap() < project_rows.find("Alpha").unwrap());
    assert!(!project_rows.contains("Beta"));

    let global_action = tui.handle(Event::Key(Key::LineStart));
    dispatch_tui_session_page(&mut tui, &router, global_action, progress.clone());
    let global_rows = render_tui_test_backend(&tui, 100, 24);
    assert!(global_rows.contains("Resume session · All projects"));
    assert!(global_rows.contains(&format!("#{} Beta", other.id)));
    assert!(!global_rows.contains("root="), "{global_rows:?}");
    assert!(!global_rows.contains("other-root"), "{global_rows:?}");
    assert!(global_rows.find("Gamma").unwrap() < global_rows.find("Beta").unwrap());
    assert!(global_rows.find("Beta").unwrap() < global_rows.find("Alpha").unwrap());

    let mut search_action = Action::Render;
    tui.handle(Event::Key(Key::Char('/')));
    for character in "reviewer".chars() {
        search_action = tui.handle(Event::Key(Key::Char(character)));
    }
    dispatch_tui_session_page(&mut tui, &router, search_action, progress.clone());
    let agent_search = render_tui_test_backend(&tui, 100, 24);
    assert!(agent_search.contains("Gamma"));
    assert!(!agent_search.contains("Alpha"));
    assert!(!agent_search.contains("Beta"));
    tui.handle(Event::Key(Key::Escape));
    tui.handle(Event::Key(Key::Escape));
    tui.apply_submission_outcome(router.open_dialog("sessions").unwrap());
    let global_action = tui.handle(Event::Key(Key::LineStart));
    dispatch_tui_session_page(&mut tui, &router, global_action, progress.clone());
    let mut search_action = Action::Render;
    tui.handle(Event::Key(Key::Char('/')));
    for character in "other-root".chars() {
        search_action = tui.handle(Event::Key(Key::Char(character)));
    }
    dispatch_tui_session_page(&mut tui, &router, search_action, progress.clone());
    let root_search = render_tui_test_backend(&tui, 100, 24);
    assert!(root_search.contains("Beta"));
    assert!(!root_search.contains("Gamma"));
    assert_eq!(*session.lock().unwrap(), original_context);

    tui.handle(Event::Key(Key::Escape));
    tui.handle(Event::Key(Key::Escape));
    tui.apply_submission_outcome(router.open_dialog("sessions").unwrap());
    SessionStore::open(bootstrap.data_directory())
        .unwrap()
        .delete_session(current.id)
        .unwrap();
    let Action::DialogAction(action_id) = tui.handle(Event::Key(Key::Enter)) else {
        panic!("session Enter should dispatch through the router");
    };
    let outcome = router.route_request(TuiRouteRequest::DialogAction(action_id), progress);
    tui.apply_submission_outcome(outcome);
    assert_eq!(tui.view().session, format!("session #{}", current.id));
    assert_eq!(*session.lock().unwrap(), original_context);
    assert!(render_tui_test_backend(&tui, 100, 24).contains("saved session is unavailable"));
    tui.handle(Event::Key(Key::Escape));
    assert!(render_tui_test_backend(&tui, 100, 24).contains("previous request"));

    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn tui_resume_overlay_restores_appends_reopens_and_resets_complete_history() {
    let temporary = tui_session_directory("resume-production-path");
    let bootstrap = tui_session_bootstrap(&temporary, &[]);
    let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
    let first = persist_tui_session(&mut store, &tui_project(&temporary), "history");
    let restored = append_tui_session_turn(&mut store, &first, "second request", "second answer");
    let restored_messages = store.load_session_for_resume(restored.id).unwrap().messages;
    drop(store);

    let session = Arc::new(Mutex::new(SessionContext::fresh()));
    let cancellation = Arc::new(Mutex::new(None));
    let mut tui = Tui::new(ProductionTuiEngine {
        cancellation: Arc::clone(&cancellation),
    });
    let router = TuiRuntimeRouter::new(
        bootstrap.clone(),
        Arc::clone(&session),
        cancellation,
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
    );
    tui.set_palette_entries(router.palette_entries().unwrap());
    let (progress, _) = std::sync::mpsc::channel();

    open_tui_palette_dialog(&mut tui, &router, "/re", "sessions", progress.clone());
    dispatch_tui_dialog_selection(&mut tui, &router, progress.clone());
    let restored_render = render_tui_test_backend(&tui, 120, 50);
    for expected in [
        "previous request",
        "Thought",
        "previous answer",
        "persisted reminder",
        "second request",
        "second answer",
    ] {
        assert!(restored_render.contains(expected), "{restored_render:?}");
        assert_eq!(
            restored_render.matches(expected).count(),
            1,
            "{restored_render:?}"
        );
    }
    // Tool name appears on header and result footer; assert the card chrome once.
    assert!(restored_render.contains("read {}"), "{restored_render:?}");
    assert_eq!(
        restored_render.matches("read {}").count(),
        1,
        "{restored_render:?}"
    );
    // A restored call is hidden by being one row, not by announcing that it is
    // hidden: the "output collapsed" marker was removed with AGN-108. What the
    // assertion owes the reader is that the body is absent, which the
    // "previous result" check below already states.
    // A collapsed thought keeps its opening line as the row's label, which is
    // what tells one from the next; the body below it is what stays hidden.
    assert!(
        restored_render.contains("Thought · previous reasoning"),
        "{restored_render:?}"
    );
    assert!(
        !restored_render.contains("previous reasoning body"),
        "{restored_render:?}"
    );
    assert!(
        !restored_render.contains("previous result"),
        "{restored_render:?}"
    );
    assert!(
        !restored_render.contains("resume-call"),
        "{restored_render:?}"
    );

    tui.handle(Event::Key(Key::PageUp));
    let restored_anchor = (
        tui.view().following_bottom,
        tui.view().scroll_offset,
        tui.view().focus,
    );

    // Ctrl+T owns the reasoning axis; tool bodies stay where they were.
    tui.handle(Event::Key(Key::CtrlT));
    assert_eq!(
        (
            tui.view().following_bottom,
            tui.view().scroll_offset,
            tui.view().focus,
        ),
        restored_anchor
    );
    let thinking_expanded = render_tui_test_backend(&tui, 120, 50);
    assert!(
        thinking_expanded.contains("previous reasoning"),
        "{thinking_expanded:?}"
    );
    assert!(
        !thinking_expanded.contains("previous result"),
        "{thinking_expanded:?}"
    );

    tui.handle(Event::Key(Key::CtrlO));
    assert_eq!(
        (
            tui.view().following_bottom,
            tui.view().scroll_offset,
            tui.view().focus,
        ),
        restored_anchor
    );
    let tools_expanded = render_tui_test_backend(&tui, 120, 50);
    assert!(
        tools_expanded.contains("previous result"),
        "{tools_expanded:?}"
    );

    tui.handle(Event::Key(Key::CtrlO));
    assert_eq!(
        (
            tui.view().following_bottom,
            tui.view().scroll_offset,
            tui.view().focus,
        ),
        restored_anchor
    );
    // Completes the Collapsed -> Truncated -> Expanded -> Collapsed
    // cycle (S1 renders Truncated and Expanded identically).
    tui.handle(Event::Key(Key::CtrlO));
    assert_eq!(
        (
            tui.view().following_bottom,
            tui.view().scroll_offset,
            tui.view().focus,
        ),
        restored_anchor
    );
    assert_eq!(
        tui.view().tool_display_modes.get("resume-call"),
        Some(&agens_tui::DisplayMode::Collapsed)
    );
    tui.handle(Event::Key(Key::End));

    assert_eq!(tui.view().session, format!("session #{}", restored.id));
    assert!(tui.transcript().is_empty());
    assert!(!restored_render.contains("INFO      Resumed session"));

    let before_failure = session.lock().unwrap().clone();
    let input = enter_tui_input(&mut tui, "/resume 999");
    tui.apply_submission_outcome(router.route(input));
    let failed = render_tui_test_backend(&tui, 120, 50);
    assert!(
        failed.contains("saved session is unavailable"),
        "{failed:?}"
    );
    assert!(failed.contains("Action:"), "{failed:?}");
    assert_eq!(tui.view().session, format!("session #{}", restored.id));
    assert_eq!(*session.lock().unwrap(), before_failure);
    assert!(tui.transcript().is_empty());

    tui.handle(Event::Key(Key::Escape));
    let prompt = enter_tui_input(&mut tui, "third request");
    let prompt = tui.apply_submission_outcome(router.route(prompt)).unwrap();
    let result = run_tui_prompt_with(
        &bootstrap,
        &prompt,
        &router.session,
        Some(router.skills().unwrap()),
        None,
        |request| {
            assert_eq!(request.history, restored_messages);
            let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
            let metadata = append_tui_session_turn(
                &mut store,
                request.session.as_ref().unwrap(),
                "third request",
                "third answer",
            );
            let messages = store.load_session_for_resume(metadata.id).unwrap().messages;
            Ok(HeadlessChatCompletion {
                text: "third answer".into(),
                metadata,
                messages,
            })
        },
    );
    tui.finish_provider_turn(tui_provider_outcome(result));
    let reopened = SessionStore::open(bootstrap.data_directory())
        .unwrap()
        .load_session_for_resume(restored.id)
        .unwrap();
    assert_eq!(session.lock().unwrap().messages, reopened.messages);

    open_tui_palette_dialog(&mut tui, &router, "/re", "sessions", progress);
    dispatch_tui_dialog_selection(&mut tui, &router, std::sync::mpsc::channel().0);
    let reopened_render = render_tui_test_backend(&tui, 120, 60);
    for expected in [
        "previous request",
        "second request",
        "third request",
        "third answer",
    ] {
        assert_eq!(
            reopened_render.matches(expected).count(),
            1,
            "{reopened_render:?}"
        );
    }

    for _ in 0..20 {
        tui.handle(Event::Key(Key::PageUp));
    }
    assert!(render_tui_test_backend(&tui, 60, 14).contains("previous request"));

    let input = enter_tui_input(&mut tui, "/new");
    tui.apply_submission_outcome(router.route(input));
    let reset = render_tui_test_backend(&tui, 120, 24);
    assert_eq!(tui.view().session, "new session");
    assert!(!reset.contains("previous request"), "{reset:?}");
    assert!(!reset.contains("INFO"), "{reset:?}");

    std::fs::remove_dir_all(temporary).unwrap();
}

fn append_tui_session_turn(
    store: &mut SessionStore,
    metadata: &SessionMetadata,
    user: &str,
    answer: &str,
) -> SessionMetadata {
    let messages = vec![
        Message {
            role: Role::System,
            parts: vec![MessagePart::Text("persisted reminder".into())],
        },
        Message {
            role: Role::User,
            parts: vec![MessagePart::Text(user.into())],
        },
        Message {
            role: Role::Assistant,
            parts: vec![MessagePart::Text(answer.into())],
        },
    ];
    let turn = CompletedSessionTurn::new(
        messages
            .into_iter()
            .map(SessionMessage::try_from)
            .collect::<Result<_, _>>()
            .unwrap(),
    )
    .unwrap();
    store
        .persist_completed_session_turn(metadata, &turn)
        .unwrap()
}

#[test]
fn tui_native_select_preserves_running_turn_outcomes_and_terminal_cleanup() {
    use std::os::unix::fs::symlink;

    let temporary = tui_session_directory("native-select");
    let bootstrap = tui_session_bootstrap(&temporary, &[]);
    let project = temporary.join("project");
    let outside = temporary.join("outside.txt");
    std::fs::write(project.join("approved.txt"), "approved").unwrap();
    std::fs::create_dir(project.join("directory")).unwrap();
    std::fs::write(project.join("large.txt"), vec![b'x'; 1024 * 1024 + 1]).unwrap();
    std::fs::write(&outside, "outside").unwrap();
    symlink(&outside, project.join("escape.txt")).unwrap();
    let session = Arc::new(Mutex::new(SessionContext::fresh()));
    let router = TuiRuntimeRouter::new(
        bootstrap,
        Arc::clone(&session),
        Arc::new(Mutex::new(None)),
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
    );
    let mut tui = Tui::new(ProductionTuiEngine {
        cancellation: Arc::new(Mutex::new(None)),
    });

    let mut control = TuiTerminalControl::default();
    let mut guard = agens_tui::TerminalModeGuard::enter(&mut control).unwrap();
    let transcript_count = open_running_tui_select(&mut tui, &router);
    assert!(render_tui_test_backend(&tui, 80, 24).contains("Select project file"));
    assert_eq!(
        tui.handle(Event::Key(Key::Escape)),
        Action::SafeDialogAction("select:cancel".into())
    );
    let cancelled = router.route_request(
        TuiRouteRequest::DialogAction("select:cancel".into()),
        std::sync::mpsc::channel().0,
    );
    assert_eq!(cancelled, TuiSubmissionOutcome::SelectionCancelled);
    assert!(tui.apply_submission_outcome(cancelled).is_none());
    assert!(tui.view().dialog.is_none());
    assert!(tui.view().running);
    assert_eq!(tui.transcript().len(), transcript_count);
    assert!(
        tui.apply_submission_outcome(router.route_request(
            TuiRouteRequest::DialogAction("select:cancel".into()),
            std::sync::mpsc::channel().0,
        ))
        .is_none()
    );
    assert_eq!(tui.transcript().len(), transcript_count);
    open_running_tui_select(&mut tui, &router);
    assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Render);
    assert!(!tui.view().quit_armed);
    assert_eq!(
        tui.view().status,
        Some("Cancellation requested; waiting for confirmation.")
    );
    assert!(tui.view().dialog.is_some());
    assert_eq!(
        tui.handle(Event::Key(Key::Escape)),
        Action::SafeDialogAction("select:cancel".into())
    );
    let cancelled = router.route_request(
        TuiRouteRequest::DialogAction("select:cancel".into()),
        std::sync::mpsc::channel().0,
    );
    assert_eq!(cancelled, TuiSubmissionOutcome::SelectionCancelled);
    assert!(tui.apply_submission_outcome(cancelled).is_none());
    assert!(tui.view().dialog.is_none());
    guard.restore(&mut control).unwrap();
    assert_tui_terminal_restored(&control);

    let mut control = TuiTerminalControl::default();
    let mut guard = agens_tui::TerminalModeGuard::enter(&mut control).unwrap();
    let transcript_count = open_running_tui_select(&mut tui, &router);
    let Action::SafeDialogAction(action_id) = tui.handle(Event::Key(Key::Enter)) else {
        panic!("selection Enter should use the safe local action path");
    };
    let selected = router.route_request(
        TuiRouteRequest::DialogAction(action_id),
        std::sync::mpsc::channel().0,
    );
    assert_eq!(
        selected,
        TuiSubmissionOutcome::SelectionInfo("Selected file: approved.txt".into())
    );
    assert!(tui.apply_submission_outcome(selected).is_none());
    assert!(tui.view().running);
    assert_eq!(tui.transcript().len(), transcript_count);
    guard.restore(&mut control).unwrap();
    assert_tui_terminal_restored(&control);

    let mut control = TuiTerminalControl::default();
    let mut guard = agens_tui::TerminalModeGuard::enter(&mut control).unwrap();
    let transcript_count = open_running_tui_select(&mut tui, &router);
    let rejected = router.route_request(
        TuiRouteRequest::DialogAction("select:escape.txt".into()),
        std::sync::mpsc::channel().0,
    );
    assert!(matches!(
        rejected,
        TuiSubmissionOutcome::SelectionError { .. }
    ));
    assert!(tui.apply_submission_outcome(rejected).is_none());
    assert!(tui.view().running);
    assert_eq!(tui.transcript().len(), transcript_count);
    guard.restore(&mut control).unwrap();
    assert_tui_terminal_restored(&control);

    std::fs::remove_dir_all(temporary).unwrap();
}

#[derive(Default)]
struct TuiTerminalControl {
    operations: Vec<agens_tui::TerminalOperation>,
}

impl agens_tui::TerminalControl for TuiTerminalControl {
    fn apply(&mut self, operation: agens_tui::TerminalOperation) -> std::io::Result<()> {
        self.operations.push(operation);
        Ok(())
    }
}

fn assert_tui_terminal_restored(control: &TuiTerminalControl) {
    use agens_tui::TerminalOperation::*;

    assert_eq!(
        control.operations,
        vec![
            EnableRaw,
            EnterAlternate,
            HideCursor,
            EnableMouse,
            EnableKeyboardEnhancement,
            EnablePaste,
            DisablePaste,
            DisableKeyboardEnhancement,
            DisableMouse,
            ShowCursor,
            LeaveAlternate,
            DisableRaw,
        ]
    );
}

fn open_running_tui_select(tui: &mut Tui<ProductionTuiEngine>, router: &TuiRuntimeRouter) -> usize {
    tui.enable_busy_policy_routing();
    tui.begin_submission("running");
    let transcript_count = tui.transcript().len();
    for character in "/select".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }

    let Action::SubmitBusy(input) = tui.handle(Event::Key(Key::Enter)) else {
        panic!("busy /select should route through catalog classification");
    };
    let outcome = router.route_request(
        TuiRouteRequest::BusyInput(input),
        std::sync::mpsc::channel().0,
    );
    assert!(
        matches!(
            outcome,
            TuiSubmissionOutcome::Dialog(_) | TuiSubmissionOutcome::SafeDialog(_)
        ),
        "{outcome:?}"
    );
    assert!(tui.apply_busy_submission_outcome(outcome).is_none());
    assert!(tui.view().running);
    assert_eq!(tui.transcript().len(), transcript_count);

    transcript_count
}

#[test]
fn busy_policy_exhaustively_classifies_builtin_commands_and_catalog_extensions() {
    let temporary = tui_session_directory("busy-policy-catalog");
    let bootstrap = tui_session_bootstrap(&temporary, &[]);
    let project = temporary.join("project");
    let command_root = project.join(".agens/commands");
    std::fs::create_dir_all(&command_root).unwrap();
    std::fs::write(
        command_root.join("summarize.md"),
        "---\nname: summarize\ndescription: summarize\n---\nSummarize $ARGUMENTS\n",
    )
    .unwrap();
    write_router_test_skill(&project, "review", "Review $ARGUMENTS");

    let commands = start_tui_commands(
        &mut Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        }),
        &bootstrap,
        &project,
    )
    .unwrap();
    let skills = start_tui_skills(
        &mut Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        }),
        &bootstrap,
        &project,
    )
    .unwrap();
    let router = TuiRuntimeRouter::new(
        bootstrap,
        Arc::new(Mutex::new(SessionContext::fresh())),
        Arc::new(Mutex::new(None)),
        Arc::clone(&commands),
        skills,
    );

    let built_ins = commands
        .iter()
        .filter(|command| command.is_builtin())
        .collect::<Vec<_>>();
    assert_eq!(built_ins.len(), tui_builtin_commands().len());

    for command in built_ins {
        let input = format!("/{} arguments", command.name());
        assert_eq!(
            router.classify_busy_input(&input),
            BusyPolicy::from_catalog_policy(command.busy_policy()),
            "{input}"
        );
    }

    let cases = [
        ("/agent primary", BusyPolicy::Reject),
        ("/connect --device-auth", BusyPolicy::Reject),
        ("/effort high", BusyPolicy::Reject),
        ("/model gpt-5.6", BusyPolicy::Reject),
        ("/provider openai-api", BusyPolicy::Reject),
        ("/resume 42", BusyPolicy::Reject),
        ("/subagent primary", BusyPolicy::Local),
        ("/subagent-profiles", BusyPolicy::Local),
        ("/subagents", BusyPolicy::Local),
        ("/summarize release", BusyPolicy::Queue),
        ("/review release", BusyPolicy::Queue),
        ("plain provider prompt", BusyPolicy::Queue),
        ("/unknown", BusyPolicy::Invalid),
    ];

    for (input, expected) in cases {
        assert_eq!(router.classify_busy_input(input), expected, "{input}");
    }

    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn busy_routes_execute_local_queue_provider_and_preserve_rejected_drafts() {
    let temporary = tui_session_directory("busy-route-outcomes");
    let bootstrap = tui_session_bootstrap(&temporary, &[]);
    let project = temporary.join("project");
    let command_root = project.join(".agens/commands");
    std::fs::create_dir_all(&command_root).unwrap();
    std::fs::write(
        command_root.join("summarize.md"),
        "---\nname: summarize\ndescription: summarize\n---\nSummarize $ARGUMENTS\n",
    )
    .unwrap();
    let commands = start_tui_commands(
        &mut Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        }),
        &bootstrap,
        &project,
    )
    .unwrap();
    let skills = start_tui_skills(
        &mut Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        }),
        &bootstrap,
        &project,
    )
    .unwrap();
    let router = TuiRuntimeRouter::new(
        bootstrap,
        Arc::new(Mutex::new(SessionContext::fresh())),
        Arc::new(Mutex::new(None)),
        commands,
        skills,
    );
    let mut tui = Tui::with_queue_capacity(
        ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        },
        1,
    );
    tui.enable_busy_policy_routing();
    tui.begin_submission("active");
    let transcript_count = tui.transcript().len();

    for character in "/select".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }
    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::SubmitBusy("/select".into())
    );
    let outcome = router.route_request(
        TuiRouteRequest::BusyInput("/select".into()),
        std::sync::mpsc::channel().0,
    );
    assert!(tui.apply_busy_submission_outcome(outcome).is_none());
    assert!(tui.view().running);
    assert!(tui.input().is_empty());
    assert_eq!(tui.transcript().len(), transcript_count);

    let mut tui = Tui::with_queue_capacity(
        ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        },
        1,
    );
    tui.enable_busy_policy_routing();
    tui.begin_submission("active");
    let transcript_count = tui.transcript().len();
    for character in "/summarize release".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }
    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::SubmitBusy("/summarize release".into())
    );
    let outcome = router.route_request(
        TuiRouteRequest::BusyInput("/summarize release".into()),
        std::sync::mpsc::channel().0,
    );
    assert!(tui.apply_busy_submission_outcome(outcome).is_none());
    assert_eq!(tui.queue_entries().len(), 1);
    assert_eq!(tui.queue_entries()[0].prompt(), "/summarize release");
    assert!(tui.input().is_empty());
    assert_eq!(tui.transcript().len(), transcript_count);

    for character in "/dangerous".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }
    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::SubmitBusy("/dangerous".into())
    );
    let outcome = router.route_request(
        TuiRouteRequest::BusyInput("/dangerous".into()),
        std::sync::mpsc::channel().0,
    );
    assert!(tui.apply_busy_submission_outcome(outcome).is_none());
    assert_eq!(tui.input(), "/dangerous");
    assert!(
        tui.status()
            .is_some_and(|message| message.contains("unavailable while a response is in progress"))
    );
    assert_eq!(tui.transcript().len(), transcript_count);

    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn busy_subagent_commands_run_locally_without_queueing_or_refusing() {
    let temporary = tui_session_directory("busy-subagent-local");
    let bootstrap = tui_session_bootstrap(
        &temporary,
        &[(
            "reviewer",
            "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
        )],
    );
    let project = temporary.join("project");
    let commands = start_tui_commands(
        &mut Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        }),
        &bootstrap,
        &project,
    )
    .unwrap();
    let skills = start_tui_skills(
        &mut Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        }),
        &bootstrap,
        &project,
    )
    .unwrap();
    let session = Arc::new(Mutex::new(SessionContext {
        running: true,
        ..SessionContext::fresh()
    }));
    let router = TuiRuntimeRouter::new(
        bootstrap,
        Arc::clone(&session),
        Arc::new(Mutex::new(None)),
        commands,
        skills,
    );
    let mut tui = Tui::with_queue_capacity(
        ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        },
        2,
    );
    tui.enable_busy_policy_routing();
    tui.begin_submission("active");

    assert_eq!(
        router.classify_busy_input("/subagent reviewer"),
        BusyPolicy::Local
    );
    assert_eq!(router.classify_busy_input("/subagent"), BusyPolicy::Local);
    assert_eq!(
        router.classify_busy_input("/subagent-profiles"),
        BusyPolicy::Local
    );

    for character in "/subagent reviewer".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }
    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::SubmitBusy("/subagent reviewer".into())
    );
    let outcome = router.route_request(
        TuiRouteRequest::BusyInput("/subagent reviewer".into()),
        std::sync::mpsc::channel().0,
    );
    assert!(tui.apply_busy_submission_outcome(outcome).is_none());
    assert!(tui.queue_entries().is_empty());
    assert!(tui.view().running);
    assert_eq!(
        session.lock().unwrap().selected_subagent.as_deref(),
        Some("reviewer")
    );

    let dialog = router.route_request(
        TuiRouteRequest::BusyInput("/subagent".into()),
        std::sync::mpsc::channel().0,
    );
    assert!(
        matches!(
            dialog,
            TuiSubmissionOutcome::Dialog(_) | TuiSubmissionOutcome::SafeDialog(_)
        ),
        "{dialog:?}"
    );

    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn persisted_selection_updates_and_resume_are_atomic_and_credential_fresh() {
    let temporary = tui_session_directory("persisted-selection");
    // This case is about resuming with no usable credentials, so it supplies an
    // empty credentials file before the fixture would write a working one.
    let config_home = temporary.join("config");
    std::fs::create_dir_all(&config_home).unwrap();
    std::fs::write(config_home.join("auth.json"), "{}").unwrap();
    let bootstrap = tui_session_bootstrap(&temporary, &[]);
    let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
    let mut metadata = persist_tui_session(&mut store, &tui_project(&temporary), "selection");
    metadata.provider_id = Some("openai-api".into());
    metadata.model_id = Some("gpt-5.5".into());
    metadata.reasoning_effort = Some(agens_core::ReasoningEffort::High);
    store.update_session_selection(&metadata).unwrap();
    drop(store);
    let resolver = CredentialResolver::with_environment(BTreeMap::from([(
        "OPENAI_API_KEY".into(),
        "fresh-secret".into(),
    )]));
    let resumed = resume_tui_session(&bootstrap, metadata.id, &SkillCatalog::default(), &resolver)
        .unwrap()
        .context;
    assert_eq!(resumed.selection.as_ref().unwrap().model(), "gpt-5.5");
    assert_eq!(
        resumed.selection.as_ref().unwrap().reasoning_effort(),
        Some("high")
    );
    let session = Arc::new(Mutex::new(resumed));
    let router = TuiRuntimeRouter::with_credential_resolver(
        bootstrap.clone(),
        Arc::clone(&session),
        Arc::new(Mutex::new(None)),
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
        resolver,
    );
    assert_eq!(router.turn_bootstrap().unwrap().model(), Some("gpt-5.5"));
    assert_eq!(
        router
            .task_parent_request_config()
            .unwrap()
            .reasoning_effort(),
        Some(agens_core::ReasoningEffort::High)
    );
    assert!(matches!(
        router.route("/model gpt-4.1".into()),
        TuiSubmissionOutcome::ContextChanged { .. }
    ));
    assert_eq!(router.turn_bootstrap().unwrap().model(), Some("gpt-4.1"));
    assert_eq!(
        router
            .task_parent_request_config()
            .unwrap()
            .reasoning_effort(),
        None
    );
    assert_eq!(
        SessionStore::open(bootstrap.data_directory())
            .unwrap()
            .load_session_for_resume(metadata.id)
            .unwrap()
            .metadata
            .model_id
            .as_deref(),
        Some("gpt-4.1")
    );

    let database = SessionStore::open(bootstrap.data_directory())
        .unwrap()
        .database_path();
    rusqlite::Connection::open(database)
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER reject_selection BEFORE UPDATE OF provider_id ON sessions
         BEGIN SELECT RAISE(ABORT, 'reject selection'); END;",
        )
        .unwrap();
    let before = session.lock().unwrap().clone();
    assert!(matches!(
        router.route("/effort default".into()),
        TuiSubmissionOutcome::LocalActionableError { .. }
    ));
    assert_eq!(*session.lock().unwrap(), before);

    let unavailable = resume_tui_session(
        &bootstrap,
        metadata.id,
        &SkillCatalog::default(),
        &CredentialResolver::with_environment(BTreeMap::new()),
    )
    .unwrap()
    .context;
    assert_eq!(unavailable.messages, before.messages);
    assert_eq!(
        unavailable.resume_error.as_deref(),
        Some("connect or choose provider")
    );
    std::fs::remove_dir_all(temporary).unwrap();
}

/// A tool call whose permission target cannot be read is a call that must not
/// run — but the turn survives it. Since AGN's tool-failure recovery the runtime
/// hands the model an actionable refusal instead of aborting, so the agent can
/// correct itself. Which refusal depends on what was wrong: a call whose
/// arguments do not parse is told so, while a call naming no tool this session
/// holds is told that instead, because rewriting its arguments would not help.
/// What stays absolute across all of them is that nothing dispatches and no
/// argument of the rejected call is echoed anywhere.
#[test]
fn an_unreadable_permission_target_dispatches_nothing_and_echoes_nothing() {
    let secret_input = r#"{"command":"SENTINEL_COMMAND","token":"SENTINEL_TOKEN"}"#;
    let missing_tool = "tool call refused: this session has no tool by that name; it was not \
                        denied and its arguments are not at fault, so no rewriting of this call \
                        can reach a tool; call one of the tools you were given instead";
    for (name, input, expected) in [
        ("native::read", "{malformed", "invalid tool arguments"),
        ("native::read", secret_input, "invalid tool arguments"),
        (
            "native::unknown",
            r#"{"path":"SENTINEL_PATH"}"#,
            missing_tool,
        ),
    ] {
        let outcome = run_production_batch(
            "permission-evaluation-invalid",
            Vec::new(),
            vec![MessagePart::ToolCall {
                id: "invalid".into(),
                name: name.into(),
                input: input.into(),
            }],
            None,
            None,
            false,
        );

        assert!(outcome.executions.is_empty(), "{name}: {input}");
        assert!(
            outcome.prompts.is_empty(),
            "an unreadable target is never asked about: {name}"
        );

        let snapshot = outcome
            .result
            .as_ref()
            .expect("the turn recovers rather than aborting");
        assert!(
            snapshot.events().iter().any(|event| matches!(
                event,
                agens_core::TurnEvent::ToolResult(MessagePart::ToolResult {
                    tool_call_id,
                    content,
                    is_error: true,
                }) if tool_call_id == "invalid" && content == expected
            )),
            "the model is told, in words that carry none of the call: {:?}",
            snapshot.events()
        );

        // The call's own arguments travel with the call — that is the
        // conversation, not a leak. What must carry none of them is the
        // rejection the runtime writes itself.
        let rejection = snapshot
            .events()
            .iter()
            .filter_map(|event| match event {
                agens_core::TurnEvent::ToolResult(MessagePart::ToolResult {
                    content,
                    is_error: true,
                    ..
                }) => Some(content.clone()),
                _ => None,
            })
            .collect::<String>();
        for sentinel in ["SENTINEL_COMMAND", "SENTINEL_TOKEN", "SENTINEL_PATH"] {
            assert!(
                !rejection.contains(sentinel),
                "{sentinel} leaked into the rejection: {rejection}"
            );
        }
    }

    for (turn_error, expected) in [
        (
            HeadlessTurnError::Permission,
            "permission: permission evaluation failed",
        ),
        (
            HeadlessTurnError::PermissionRequired,
            "permission: permission approval is required",
        ),
        (
            HeadlessTurnError::PermissionEvaluation,
            "permission: permission target could not be evaluated; correct the tool arguments and retry",
        ),
    ] {
        let error = CliError::runtime(turn_error);
        assert_eq!(error.category, "permission");
        assert_eq!(error.to_string(), expected);
        assert!(!error.to_string().contains("SENTINEL_COMMAND"));
        assert!(!error.to_string().contains("SENTINEL_TOKEN"));

        assert!(matches!(
            tui_provider_outcome(Err(error)),
            TuiProviderOutcome::Failed { message, action }
                if message == expected && action == TUI_ERROR_ACTION
        ));
    }
}

#[test]
fn provider_failures_render_sanitized_specific_actions() {
    for (turn_error, expected_message, expected_action) in [
        (
            HeadlessTurnError::ProviderContext,
            "provider: request exceeds the model context window",
            "Start a new session or shorten the prompt, then retry.",
        ),
        (
            HeadlessTurnError::ProviderNetwork,
            "provider: network request failed",
            "Check the network connection, then retry.",
        ),
        (
            HeadlessTurnError::ProviderProtocol,
            "provider: provider response protocol failed",
            "Open /diagnostics for the referenced event, then retry.",
        ),
    ] {
        let error = CliError::runtime(turn_error).with_diagnostic_reference("deadbeef");
        let expected_message = format!("{expected_message} [ref: deadbeef]");

        assert!(matches!(
            tui_provider_outcome(Err(error)),
            TuiProviderOutcome::Failed { message, action }
                if message == expected_message && action == expected_action
        ));
    }
}

#[test]
fn provider_specific_actions_reject_malformed_diagnostic_references() {
    let exact = CliError::runtime(HeadlessTurnError::ProviderProtocol);
    assert!(matches!(
        tui_provider_outcome(Err(exact)),
        TuiProviderOutcome::Failed { action, .. }
            if action == "Open /diagnostics for the referenced event, then retry."
    ));

    for reference in [
        "DEADBEEF",
        "deadbee",
        "deadbeef0",
        "deadbeef] trailing",
        "dead] [ref: beef",
    ] {
        let error = CliError::runtime(HeadlessTurnError::ProviderProtocol)
            .with_diagnostic_reference(reference);
        assert!(matches!(
            tui_provider_outcome(Err(error)),
            TuiProviderOutcome::Failed { action, .. } if action == TUI_ERROR_ACTION
        ));
    }
}

// --- WU4: /history and /stash slash builtins open Tui-owned overlays ---

#[test]
fn history_stash_palette_and_open_dialog_open_prompt_overlays() {
    let temporary = tui_session_directory("history-stash-overlays");
    let bootstrap = tui_session_bootstrap(&temporary, &[]);
    let session = Arc::new(Mutex::new(SessionContext::fresh()));
    let cancellation = Arc::new(Mutex::new(None));
    let mut tui = Tui::new(ProductionTuiEngine {
        cancellation: Arc::clone(&cancellation),
    });
    tui.set_prompt_memory(Box::new(agens_core::EphemeralPromptMemory::new()));
    let router = TuiRuntimeRouter::new(
        bootstrap,
        Arc::clone(&session),
        cancellation,
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
    );
    tui.set_palette_entries(router.palette_entries().unwrap());
    let (progress, _) = std::sync::mpsc::channel();

    // Seed stores via the same key paths users use (submit + Ctrl+S).
    for character in "history-seed".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }
    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::Submit("history-seed".into())
    );
    for character in "stash-seed".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }
    assert_eq!(tui.handle(Event::Key(Key::CtrlS)), Action::Render);

    open_tui_palette_dialog(&mut tui, &router, "/hi", "history", progress.clone());
    let history_text = render_tui_test_backend(&tui, 80, 24);
    assert!(
        history_text.contains("Prompt history"),
        "palette /history must open history overlay: {history_text:?}"
    );
    assert!(
        history_text.contains("history-seed"),
        "history overlay must list seeded entry: {history_text:?}"
    );
    assert!(tui.view().dialog.is_some());
    assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::Render);

    open_tui_palette_dialog(&mut tui, &router, "/sta", "stash", progress.clone());
    let stash_text = render_tui_test_backend(&tui, 80, 24);
    assert!(
        stash_text.contains("Prompt stash"),
        "palette /stash must open stash overlay: {stash_text:?}"
    );
    assert!(
        stash_text.contains("stash-seed"),
        "stash overlay must list seeded entry: {stash_text:?}"
    );
    assert!(tui.view().dialog.is_some());
    assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::Render);

    // OpenDialog route ids also open Tui-owned builders (no router-held stores).
    let outcome = router.route_request(
        TuiRouteRequest::OpenDialog("history".into()),
        progress.clone(),
    );
    assert!(tui.apply_submission_outcome(outcome).is_none());
    assert!(
        render_tui_test_backend(&tui, 80, 24).contains("Prompt history"),
        "OpenDialog(history) must open the history overlay"
    );
    assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::Render);

    let outcome = router.route_request(TuiRouteRequest::OpenDialog("stash".into()), progress);
    assert!(tui.apply_submission_outcome(outcome).is_none());
    assert!(
        render_tui_test_backend(&tui, 80, 24).contains("Prompt stash"),
        "OpenDialog(stash) must open the stash overlay"
    );

    assert!(session.lock().unwrap().messages.is_empty());
    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn history_stash_typed_slash_resolve_opens_overlays_without_provider_turn() {
    let temporary = tui_session_directory("history-stash-resolve");
    let bootstrap = tui_session_bootstrap(&temporary, &[]);
    let session = Arc::new(Mutex::new(SessionContext::fresh()));
    let cancellation = Arc::new(Mutex::new(None));
    let mut tui = Tui::new(ProductionTuiEngine {
        cancellation: Arc::clone(&cancellation),
    });
    tui.set_prompt_memory(Box::new(agens_core::EphemeralPromptMemory::new()));
    let project_root = temporary.join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    let commands = start_tui_commands(&mut tui, &bootstrap, &project_root).unwrap();
    let skills = start_tui_skills(&mut tui, &bootstrap, &project_root).unwrap();
    let router = TuiRuntimeRouter::new(
        bootstrap,
        Arc::clone(&session),
        cancellation,
        commands,
        skills,
    );
    tui.set_palette_entries(router.palette_entries().unwrap());

    for character in "typed-history".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }
    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::Submit("typed-history".into())
    );

    let history_outcome = router.route("/history".into());
    assert!(
        !matches!(
            history_outcome,
            TuiSubmissionOutcome::ProviderTurn { .. }
                | TuiSubmissionOutcome::BusyProviderTurn { .. }
        ),
        "typed /history must not become a provider turn: {history_outcome:?}"
    );
    assert!(tui.apply_submission_outcome(history_outcome).is_none());
    let history_text = render_tui_test_backend(&tui, 80, 24);
    assert!(
        history_text.contains("Prompt history"),
        "typed /history must open history overlay: {history_text:?}"
    );
    assert!(
        history_text.contains("typed-history") || history_text.contains("/history"),
        "overlay or recorded history should be visible: {history_text:?}"
    );
    assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::Render);

    for character in "typed-stash".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }
    assert_eq!(tui.handle(Event::Key(Key::CtrlS)), Action::Render);

    let stash_outcome = router.route("/stash".into());
    assert!(
        !matches!(
            stash_outcome,
            TuiSubmissionOutcome::ProviderTurn { .. }
                | TuiSubmissionOutcome::BusyProviderTurn { .. }
        ),
        "typed /stash must not become a provider turn: {stash_outcome:?}"
    );
    assert!(tui.apply_submission_outcome(stash_outcome).is_none());
    let stash_text = render_tui_test_backend(&tui, 80, 24);
    assert!(
        stash_text.contains("Prompt stash"),
        "typed /stash must open stash overlay: {stash_text:?}"
    );
    assert!(
        stash_text.contains("typed-stash"),
        "stash overlay must list seeded entry: {stash_text:?}"
    );

    // Builtins must be registered in the palette catalog.
    let names: Vec<_> = router
        .palette_entries()
        .unwrap()
        .iter()
        .map(|entry| entry.name().to_owned())
        .collect();
    assert!(names.iter().any(|name| name == "history"), "{names:?}");
    assert!(names.iter().any(|name| name == "stash"), "{names:?}");

    assert!(session.lock().unwrap().messages.is_empty());
    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn attach_command_ingests_media_and_returns_path_free_chips() {
    let temporary = tui_session_directory("attach-command");
    let bootstrap = tui_session_bootstrap(&temporary, &[]);
    let _store = SessionStore::open(bootstrap.data_directory()).unwrap();
    let project = temporary.join("project");
    std::fs::write(project.join("photo.png"), b"photo-bytes").unwrap();

    let session = Arc::new(Mutex::new(SessionContext::fresh()));
    let router = TuiRuntimeRouter::new(
        bootstrap,
        Arc::clone(&session),
        Arc::new(Mutex::new(None)),
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
    );
    let (progress, _) = std::sync::mpsc::channel();
    let outcome =
        router.route_request(TuiRouteRequest::Input("/attach photo.png".into()), progress);

    let TuiSubmissionOutcome::MediaAttached {
        message,
        staged_media,
    } = outcome
    else {
        panic!("expected MediaAttached, got {outcome:?}");
    };
    assert!(message.contains("[Image #1]"), "{message}");
    assert_eq!(staged_media.len(), 1);
    assert_eq!(staged_media[0].mime, "image/png");
    assert_eq!(session.lock().unwrap().pending_media_ids.len(), 1);
    assert_eq!(
        session.lock().unwrap().pending_media_mimes,
        vec!["image/png".to_owned()]
    );
    // Path-free: attachments and message must not echo the source path.
    assert!(!message.contains("photo.png"));
    assert!(
        !staged_media
            .iter()
            .any(|media| media.mime.contains("photo"))
    );

    std::fs::remove_dir_all(temporary).unwrap();
}

/// A restored id proven unreachable — its blob deleted from the store, or no row at all — must be
/// dropped, not staged: staging it would fail the preflight of every later submit with no way to
/// take the chip back off the composer.
#[test]
fn restoring_staged_media_drops_ids_with_a_deleted_blob_and_ids_with_no_row() {
    let temporary = tui_session_directory("restore-missing-blob");
    let bootstrap = tui_session_bootstrap(&temporary, &[]);
    let _store = SessionStore::open(bootstrap.data_directory()).unwrap();
    let project = temporary.join("project");
    std::fs::write(project.join("photo.png"), b"photo-bytes").unwrap();
    std::fs::write(project.join("scan.png"), b"scan-bytes").unwrap();

    let session = Arc::new(Mutex::new(SessionContext::fresh()));
    let router = TuiRuntimeRouter::new(
        bootstrap.clone(),
        Arc::clone(&session),
        Arc::new(Mutex::new(None)),
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
    );

    let (progress, _) = std::sync::mpsc::channel();
    let attached =
        router.route_request(TuiRouteRequest::Input("/attach photo.png".into()), progress);
    let TuiSubmissionOutcome::MediaAttached { staged_media, .. } = attached else {
        panic!("expected MediaAttached, got {attached:?}");
    };
    let reachable = staged_media[0].clone();

    let (progress, _) = std::sync::mpsc::channel();
    let attached =
        router.route_request(TuiRouteRequest::Input("/attach scan.png".into()), progress);
    let TuiSubmissionOutcome::MediaAttached { staged_media, .. } = attached else {
        panic!("expected MediaAttached, got {attached:?}");
    };
    let blob_deleted = staged_media
        .iter()
        .find(|attachment| attachment.media_id != reachable.media_id)
        .expect("the second attach must record its own media id")
        .clone();

    let (_mime, blob_path) =
        agens_store::open_media(bootstrap.data_directory(), blob_deleted.media_id).unwrap();
    std::fs::remove_file(&blob_path).unwrap();

    let (progress, _) = std::sync::mpsc::channel();
    let outcome = router.route_request(
        TuiRouteRequest::ReplaceStagedMedia {
            attachments: vec![
                reachable.clone(),
                blob_deleted,
                agens_core::PromptAttachment::new(987_654, "application/pdf"),
            ],
        },
        progress,
    );

    let TuiSubmissionOutcome::StagedMediaReplaced {
        staged_media,
        notice,
    } = outcome
    else {
        panic!("expected StagedMediaReplaced, got {outcome:?}");
    };
    assert_eq!(staged_media, vec![reachable.clone()]);
    assert_eq!(
        notice.as_deref(),
        Some("2 restored attachments are no longer available and were dropped.")
    );
    assert_eq!(
        session.lock().unwrap().pending_media_ids,
        vec![reachable.media_id],
        "an unreachable id must never reach the session staging"
    );

    std::fs::remove_dir_all(temporary).unwrap();
}

/// A blob the store could not even stat proves nothing, so the attachment must survive the
/// restore: the stash pop feeding this path has already deleted the durable row.
#[test]
#[cfg(unix)]
fn restoring_staged_media_keeps_ids_whose_blob_could_not_be_stated() {
    use std::os::unix::fs::PermissionsExt;

    let temporary = tui_session_directory("restore-unstatable-blob");
    let bootstrap = tui_session_bootstrap(&temporary, &[]);
    let _store = SessionStore::open(bootstrap.data_directory()).unwrap();
    let project = temporary.join("project");
    std::fs::write(project.join("photo.png"), b"photo-bytes").unwrap();

    let session = Arc::new(Mutex::new(SessionContext::fresh()));
    let router = TuiRuntimeRouter::new(
        bootstrap.clone(),
        Arc::clone(&session),
        Arc::new(Mutex::new(None)),
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
    );

    let (progress, _) = std::sync::mpsc::channel();
    let attached =
        router.route_request(TuiRouteRequest::Input("/attach photo.png".into()), progress);
    let TuiSubmissionOutcome::MediaAttached { staged_media, .. } = attached else {
        panic!("expected MediaAttached, got {attached:?}");
    };
    let recorded = staged_media[0].clone();

    let media_directory = bootstrap.data_directory().join("media");
    let original_mode = std::fs::metadata(&media_directory)
        .unwrap()
        .permissions()
        .mode();
    std::fs::set_permissions(&media_directory, std::fs::Permissions::from_mode(0o000)).unwrap();

    let (progress, _) = std::sync::mpsc::channel();
    let outcome = router.route_request(
        TuiRouteRequest::ReplaceStagedMedia {
            attachments: vec![recorded.clone()],
        },
        progress,
    );

    std::fs::set_permissions(
        &media_directory,
        std::fs::Permissions::from_mode(original_mode),
    )
    .unwrap();

    let TuiSubmissionOutcome::StagedMediaReplaced {
        staged_media,
        notice,
    } = outcome
    else {
        panic!("expected StagedMediaReplaced, got {outcome:?}");
    };
    assert_eq!(
        staged_media,
        vec![recorded.clone()],
        "a blob that could not be stated proves nothing, so the attachment must stay staged"
    );
    assert_eq!(
        notice.as_deref(),
        Some("1 restored attachment could not be checked and was kept staged.")
    );
    assert_eq!(
        session.lock().unwrap().pending_media_ids,
        vec![recorded.media_id]
    );

    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn restoring_staged_media_keeps_ids_whose_lookup_failed() {
    let temporary = tui_session_directory("restore-unverifiable");
    let bootstrap = tui_session_bootstrap(&temporary, &[]);
    let store = SessionStore::open(bootstrap.data_directory()).unwrap();
    let project = temporary.join("project");
    std::fs::write(project.join("photo.png"), b"photo-bytes").unwrap();

    let session = Arc::new(Mutex::new(SessionContext::fresh()));
    let router = TuiRuntimeRouter::new(
        bootstrap.clone(),
        Arc::clone(&session),
        Arc::new(Mutex::new(None)),
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
    );

    let (progress, _) = std::sync::mpsc::channel();
    let attached =
        router.route_request(TuiRouteRequest::Input("/attach photo.png".into()), progress);
    let TuiSubmissionOutcome::MediaAttached { staged_media, .. } = attached else {
        panic!("expected MediaAttached, got {attached:?}");
    };
    let recorded = staged_media[0].clone();

    drop(store);
    let database_path = bootstrap.data_directory().join("agens.db");
    std::fs::remove_file(&database_path).unwrap();
    std::fs::create_dir(&database_path).unwrap();

    let (progress, _) = std::sync::mpsc::channel();
    let outcome = router.route_request(
        TuiRouteRequest::ReplaceStagedMedia {
            attachments: vec![recorded.clone()],
        },
        progress,
    );

    let TuiSubmissionOutcome::StagedMediaReplaced {
        staged_media,
        notice,
    } = outcome
    else {
        panic!("expected StagedMediaReplaced, got {outcome:?}");
    };
    assert_eq!(
        staged_media,
        vec![recorded.clone()],
        "a lookup failure proves nothing, so the attachment must stay staged"
    );
    assert_eq!(
        notice.as_deref(),
        Some("1 restored attachment could not be checked and was kept staged.")
    );
    assert_eq!(
        session.lock().unwrap().pending_media_ids,
        vec![recorded.media_id]
    );

    std::fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn clipboard_image_route_ingests_bytes() {
    let temporary = tui_session_directory("clipboard-image");
    let bootstrap = tui_session_bootstrap(&temporary, &[]);
    let _store = SessionStore::open(bootstrap.data_directory()).unwrap();
    let session = Arc::new(Mutex::new(SessionContext::fresh()));
    let router = TuiRuntimeRouter::new(
        bootstrap,
        Arc::clone(&session),
        Arc::new(Mutex::new(None)),
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
    );
    let png = vec![0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n', 9, 9, 9];
    let (progress, _) = std::sync::mpsc::channel();
    let outcome = router.route_request(
        TuiRouteRequest::AttachClipboardImage {
            bytes: png,
            mime: None,
        },
        progress,
    );
    let TuiSubmissionOutcome::MediaAttached { staged_media, .. } = outcome else {
        panic!("expected MediaAttached, got {outcome:?}");
    };
    assert_eq!(staged_media.len(), 1);
    assert_eq!(staged_media[0].mime, "image/png");
    assert_eq!(session.lock().unwrap().pending_media_ids.len(), 1);

    std::fs::remove_dir_all(temporary).unwrap();
}

/// Two turns, so a fork can cut between them.
fn persist_two_turn_tui_session(
    store: &mut SessionStore,
    project: &str,
    title: &str,
) -> SessionMetadata {
    let first = persist_tui_session(store, project, title);
    let second = CompletedSessionTurn::new(
        vec![
            Message {
                role: Role::User,
                parts: vec![MessagePart::Text("second request".into())],
            },
            Message {
                role: Role::Assistant,
                parts: vec![MessagePart::Text("second answer".into())],
            },
        ]
        .into_iter()
        .map(SessionMessage::try_from)
        .collect::<Result<_, _>>()
        .unwrap(),
    )
    .unwrap();

    store
        .persist_completed_session_turn(&first, &second)
        .unwrap()
}

/// The whole feature end to end: the reader stands on the first turn, forks, and lands in a new
/// session holding only that turn — with the session they forked from left exactly as it was.
#[test]
fn forking_the_active_session_enters_a_copy_of_the_prefix_and_leaves_the_source_alone() {
    let temporary = tui_session_directory("fork-enters-the-copy");
    let bootstrap = tui_session_bootstrap(&temporary, &[]);
    let project = tui_project(&temporary);
    let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
    let metadata = persist_two_turn_tui_session(&mut store, &project, "Forked");
    let source_before = store.load_session_for_resume(metadata.id).unwrap();
    assert_eq!(source_before.messages.len(), 5);
    drop(store);

    let session = Arc::new(Mutex::new(SessionContext {
        identifier: Some(metadata.id),
        messages: source_before.messages.clone(),
        ..SessionContext::fresh()
    }));
    let router = TuiRuntimeRouter::with_clock(
        bootstrap.clone(),
        Arc::clone(&session),
        Arc::new(Mutex::new(None)),
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
        || 10_000,
    );

    let outcome = router.route_request(
        TuiRouteRequest::ForkSession(agens_tui::SessionForkRequest::from_active_transcript(1)),
        std::sync::mpsc::channel().0,
    );

    assert!(
        matches!(outcome, TuiSubmissionOutcome::SessionResumed { .. }),
        "a fork ends attached to the fork: {outcome:?}"
    );
    let fork_id = session.lock().unwrap().identifier.unwrap();
    assert_ne!(fork_id, metadata.id);

    let store = SessionStore::open(bootstrap.data_directory()).unwrap();
    let fork = store.load_session_for_resume(fork_id).unwrap();
    assert_eq!(
        fork.messages,
        source_before.messages[..3].to_vec(),
        "the fork holds the first turn and nothing of the second"
    );
    assert_eq!(fork.metadata.parent_session_id, Some(metadata.id));
    assert_eq!(fork.metadata.fork_message_count, Some(3));
    assert_eq!(
        store.load_session_for_resume(metadata.id).unwrap(),
        source_before,
        "the session that was forked from is untouched"
    );
    assert_eq!(session.lock().unwrap().messages, fork.messages);

    std::fs::remove_dir_all(temporary).unwrap();
}

/// A fork point the reader never chose is refused rather than guessed at.
#[test]
fn forking_without_a_turn_to_cut_at_is_refused() {
    let temporary = tui_session_directory("fork-without-a-turn");
    let bootstrap = tui_session_bootstrap(&temporary, &[]);
    let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
    let metadata = persist_two_turn_tui_session(&mut store, &tui_project(&temporary), "Forked");
    drop(store);

    let session = Arc::new(Mutex::new(SessionContext {
        identifier: Some(metadata.id),
        ..SessionContext::fresh()
    }));
    let router = TuiRuntimeRouter::with_clock(
        bootstrap.clone(),
        Arc::clone(&session),
        Arc::new(Mutex::new(None)),
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
        || 10_000,
    );

    let refused = router.route_request(
        TuiRouteRequest::ForkSession(agens_tui::SessionForkRequest::from_active_transcript(0)),
        std::sync::mpsc::channel().0,
    );
    assert!(
        matches!(refused, TuiSubmissionOutcome::LocalActionableError { .. }),
        "{refused:?}"
    );

    let unsaved = Arc::new(Mutex::new(SessionContext::fresh()));
    let unsaved_router = TuiRuntimeRouter::with_clock(
        bootstrap.clone(),
        unsaved,
        Arc::new(Mutex::new(None)),
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
        || 10_000,
    );
    let unsaved_refusal = unsaved_router.route_request(
        TuiRouteRequest::ForkSession(agens_tui::SessionForkRequest::from_active_transcript(1)),
        std::sync::mpsc::channel().0,
    );
    assert!(
        matches!(
            unsaved_refusal,
            TuiSubmissionOutcome::LocalActionableError { .. }
        ),
        "{unsaved_refusal:?}"
    );

    let store = SessionStore::open(bootstrap.data_directory()).unwrap();
    assert_eq!(
        store.list_session_children(metadata.id).unwrap(),
        Vec::new(),
        "a refused fork writes no session"
    );

    std::fs::remove_dir_all(temporary).unwrap();
}

/// The overlay is opened from anywhere in a lineage and always shows the whole of it, rooted at
/// the session nothing was forked from and indented by how deep each fork sits.
#[test]
fn the_session_tree_lists_the_whole_lineage_from_whichever_fork_it_is_opened_in() {
    let temporary = tui_session_directory("session-tree-lineage");
    let bootstrap = tui_session_bootstrap(&temporary, &[]);
    let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
    let root = persist_two_turn_tui_session(&mut store, &tui_project(&temporary), "Root");
    let fork = store.fork_session(root.id, 3).unwrap();
    let sibling = store.fork_session(root.id, 5).unwrap();
    let grandchild = store.fork_session(fork, 3).unwrap();
    drop(store);

    let session = Arc::new(Mutex::new(SessionContext {
        identifier: Some(grandchild),
        ..SessionContext::fresh()
    }));
    let router = TuiRuntimeRouter::with_clock(
        bootstrap.clone(),
        Arc::clone(&session),
        Arc::new(Mutex::new(None)),
        Arc::new(CommandCatalog::default()),
        Arc::new(SkillCatalog::default()),
        || 10_000,
    );

    let outcome = router.route_request(
        TuiRouteRequest::SessionTree(agens_tui::SessionTreeRequest::active()),
        std::sync::mpsc::channel().0,
    );

    let TuiSubmissionOutcome::Dialog(dialog) = outcome else {
        panic!("the lineage browser answers with a dialog");
    };
    let mut tui = Tui::new(ProductionTuiEngine {
        cancellation: Arc::new(Mutex::new(None)),
    });
    tui.show_selection_dialog(dialog);
    let rendered = render_tui_test_backend(&tui, 100, 26);

    assert!(rendered.contains("Session tree"), "{rendered:?}");
    for id in [root.id, fork, sibling, grandchild] {
        assert!(rendered.contains(&format!("#{id}")), "{id} in {rendered:?}");
    }
    assert!(
        rendered.contains(&format!("└ #{fork}")),
        "a fork is indented under the session it came from: {rendered:?}"
    );
    assert!(
        rendered.contains(&format!("└ #{grandchild}")),
        "a fork of a fork is indented deeper: {rendered:?}"
    );

    std::fs::remove_dir_all(temporary).unwrap();
}
