use agens_bootstrap::discover_skill_catalog;
use std::path::Path;
use std::sync::Arc;

use agens_tools::{CommandCatalog, CommandDefinition, SkillCatalog};
use agens_tui::{Engine as TuiEngine, PaletteEntry, PaletteEntryKind, Tui};

use agens_bootstrap::Bootstrap;
use agens_error::CliError;

pub(crate) const RESERVED_TUI_COMMANDS: &[&str] = &[
    "agent",
    "connect",
    "disconnect",
    "diagnostics",
    "effort",
    "help",
    "mcp",
    "model",
    "new",
    "provider",
    "quit",
    "resume",
    "select",
    "sessions",
    "subagent",
    "subagents",
];

const TUI_PALETTE_BUILT_INS: &[(&str, &str, &str, Option<&str>)] = &[
    ("connect", "Connect to ChatGPT", "[--device-auth]", None),
    ("disconnect", "Disconnect ChatGPT credentials", "", None),
    (
        "diagnostics",
        "Show sanitized runtime diagnostics",
        "",
        Some("diagnostics"),
    ),
    ("new", "Start a new session", "", None),
    ("sessions", "List saved sessions", "", None),
    ("resume", "Resume a saved session", "<id>", None),
    ("agent", "List or select the primary agent", "[name]", None),
    (
        "provider",
        "Select runtime provider",
        "[name]",
        Some("provider"),
    ),
    ("model", "List or select the model", "[name]", Some("model")),
    (
        "effort",
        "Show or set reasoning effort",
        "[level]",
        Some("effort"),
    ),
    ("help", "Show commands and skills", "", Some("help")),
    ("mcp", "Show configured MCP servers", "", Some("mcp")),
    ("select", "Select a project file", "", Some("select")),
    ("quit", "Exit Agens", "", None),
];

/// Discovers the command catalog for the given root without surfacing diagnostics to a `Tui`.
///
/// Shared by [`start_tui_commands`] (which adds startup diagnostics) and by a session's
/// post-resume catalog refresh, which has no `Tui` handle to report diagnostics to.
pub(crate) fn discover_tui_command_catalog(
    bootstrap: &Bootstrap,
    project_root: &Path,
) -> Result<agens_tools::CommandDiscovery, CliError> {
    let global_root = bootstrap
        .paths
        .global_config
        .parent()
        .ok_or_else(|| CliError::configuration("global command root is unavailable"))?
        .join("commands");
    let project_command_root = project_root.join(".agens/commands");
    let built_ins = RESERVED_TUI_COMMANDS
        .iter()
        .map(|name| {
            CommandDefinition::new(*name, "Reserved TUI command", *name)
                .expect("reserved TUI command names are valid")
        })
        .collect::<Vec<_>>();
    CommandCatalog::discover(&built_ins, global_root, project_command_root)
        .map_err(CliError::configuration)
}

pub(crate) fn start_tui_commands<E: TuiEngine>(
    tui: &mut Tui<E>,
    bootstrap: &Bootstrap,
    project_root: &Path,
) -> Result<Arc<CommandCatalog>, CliError> {
    let discovery = discover_tui_command_catalog(bootstrap, project_root)?;

    for diagnostic in discovery.diagnostics() {
        tui.add_diagnostic(format!(
            "Command diagnostic ({}): {}",
            diagnostic.path().display(),
            diagnostic.message()
        ));
    }
    for name in discovery.shadowed() {
        tui.add_diagnostic(format!(
            "Command /{name} has multiple definitions; applied source precedence."
        ));
    }

    Ok(Arc::new(discovery.catalog().clone()))
}

pub(crate) fn start_tui_skills<E: TuiEngine>(
    tui: &mut Tui<E>,
    bootstrap: &Bootstrap,
    project_root: &Path,
) -> Result<Arc<SkillCatalog>, CliError> {
    let discovery = discover_skill_catalog(bootstrap, project_root)?;
    for diagnostic in discovery.diagnostics() {
        tui.add_diagnostic(format!(
            "Skill diagnostic ({}): {}",
            diagnostic.path().display(),
            diagnostic.message()
        ));
    }
    for shadow in discovery.shadowed() {
        tui.add_diagnostic(format!(
            "Skill /{} has multiple definitions; applied source precedence.",
            shadow.name()
        ));
    }

    Ok(Arc::new(discovery.catalog().clone()))
}

pub(crate) fn resolved_tui_palette(
    commands: &CommandCatalog,
    skills: &SkillCatalog,
    has_subagents: bool,
) -> Vec<PaletteEntry> {
    let mut entries = TUI_PALETTE_BUILT_INS
        .iter()
        .map(|(name, description, hint, dialog_id)| {
            let entry = PaletteEntry::new(*name, *description, *hint, PaletteEntryKind::BuiltIn);
            let dialog_id = dialog_id.or(match *name {
                "connect" | "disconnect" | "agent" => Some(*name),
                "sessions" | "resume" => Some("sessions"),
                _ => None,
            });
            dialog_id.map_or(entry.clone(), |route| entry.with_dialog(route))
        })
        .collect::<Vec<_>>();
    if has_subagents {
        entries.push(
            PaletteEntry::new(
                "subagent",
                "Choose an eligible configured subagent",
                "[name]",
                PaletteEntryKind::BuiltIn,
            )
            .with_dialog("subagent"),
        );
        entries.push(PaletteEntry::new(
            "subagents",
            "Inspect current-session subagent transcripts",
            "",
            PaletteEntryKind::BuiltIn,
        ));
    }
    let mut custom_commands = commands
        .iter()
        .filter(|command| !RESERVED_TUI_COMMANDS.contains(&command.name()))
        .collect::<Vec<_>>();
    custom_commands.sort_by_key(|command| command.name());
    entries.extend(custom_commands.into_iter().map(|command| {
        PaletteEntry::new(
            command.name(),
            command.description(),
            "[arguments]",
            PaletteEntryKind::Command,
        )
    }));
    let mut resolved_skills = skills
        .skills()
        .filter(|skill| {
            !RESERVED_TUI_COMMANDS.contains(&skill.name())
                && commands.command(skill.name()).is_none()
        })
        .collect::<Vec<_>>();
    resolved_skills.sort_by_key(|skill| skill.name());
    entries.extend(resolved_skills.into_iter().map(|skill| {
        PaletteEntry::new(
            skill.name(),
            skill.description(),
            "[arguments]",
            PaletteEntryKind::Skill,
        )
    }));
    entries
}

pub(crate) fn render_tui_help(entries: &[PaletteEntry]) -> String {
    let surface = entries
        .iter()
        .map(|entry| {
            format!(
                "/{} {}  [{}] {}",
                entry.name(),
                entry.argument_hint(),
                entry.kind().label(),
                entry.description()
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("Available commands and skills:\n{surface}")
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Mutex;

    use agens_core::{
        PermissionDecision, PermissionMode, PermissionPattern, PermissionPolicy, PermissionRule,
        PermissionSession,
    };
    use agens_tools::{ToolDispatchRequest, ToolEvaluationOutcome, ToolExecutionContext};
    use agens_tui::{TuiProviderOutcome, TuiSubmissionOutcome};

    use super::*;
    use crate::test_support::{
        enter_tui_input, submit_tui_command, tui_session_bootstrap, tui_session_directory,
    };
    use crate::tools::runtime::production_tool_runtime;
    use crate::tui::engine::{ProductionTuiEngine, report_tui_extension_collisions};
    use crate::tui::router::TuiRuntimeRouter;
    use agens_session::context::SessionContext;

    fn write_tui_command(root: &Path, name: &str, description: &str, template: &str) {
        std::fs::write(
            root.join(format!("{name}.md")),
            format!("---\ndescription: {description}\n---\n{template}\n"),
        )
        .unwrap();
    }

    fn write_tui_skill(root: &Path, name: &str, description: &str, body: &str) {
        let directory = root.join(name);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n{body}\n"),
        )
        .unwrap();
    }

    #[test]
    fn startup_commands_and_skills_read_the_given_root_not_the_bootstrap_process_root() {
        let temporary = tui_session_directory("extensions-root-confinement");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let bootstrap_root = agens_bootstrap::session_root::discovered_root_for_tests(&bootstrap);

        let elsewhere = tui_session_directory("extensions-root-confinement-elsewhere");
        let elsewhere_root = elsewhere.join("project");
        std::fs::create_dir_all(elsewhere_root.join(".agens/commands")).unwrap();
        write_tui_command(
            &elsewhere_root.join(".agens/commands"),
            "elsewhere",
            "elsewhere command",
            "ELSEWHERE:$ARGUMENTS",
        );
        std::fs::create_dir_all(elsewhere_root.join(".agens/skills")).unwrap();
        write_tui_skill(
            &elsewhere_root.join(".agens/skills"),
            "elsewhere-skill",
            "elsewhere skill",
            "ELSEWHERE_SKILL_BODY",
        );

        let cancellation = Arc::new(Mutex::new(None));
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::clone(&cancellation),
        });
        let commands = start_tui_commands(&mut tui, &bootstrap, &elsewhere_root).unwrap();
        let skills = start_tui_skills(&mut tui, &bootstrap, &elsewhere_root).unwrap();

        assert!(
            commands.command("elsewhere").is_some(),
            "the given root's commands must be discovered"
        );
        assert!(
            skills.skill("elsewhere-skill").is_some(),
            "the given root's skills must be discovered"
        );
        assert_ne!(bootstrap_root, elsewhere_root);

        std::fs::remove_dir_all(temporary).unwrap();
        std::fs::remove_dir_all(elsewhere).unwrap();
    }

    #[test]
    fn tui_startup_commands_route_real_enter_to_captured_provider_requests() {
        let temporary = tui_session_directory("declarative-commands");
        let config_home = temporary.join("config");
        let global_commands = config_home.join("commands");
        let project_commands = temporary.join("project/.agens/commands");
        std::fs::create_dir_all(&global_commands).unwrap();
        std::fs::create_dir_all(&project_commands).unwrap();
        for (root, name, description, template) in [
            (&global_commands, "shared", "global", "global:$ARGUMENTS"),
            (
                &global_commands,
                "global-only",
                "global only",
                "Keep literal text [$ARGUMENTS]",
            ),
            (
                &global_commands,
                "slash-template",
                "literal slash",
                "/literal $ARGUMENTS",
            ),
            (
                &global_commands,
                "connect",
                "collision",
                "must not run $ARGUMENTS",
            ),
            (&project_commands, "shared", "project", "project:$ARGUMENTS"),
        ] {
            write_tui_command(root, name, description, template);
        }
        std::fs::write(
            project_commands.join("broken.md"),
            "---\ndescription: [invalid\n---\nbroken\n",
        )
        .unwrap();

        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let session = Arc::new(Mutex::new(SessionContext::fresh()));
        let cancellation = Arc::new(Mutex::new(None));
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::clone(&cancellation),
        });
        let project_root = agens_bootstrap::session_root::discovered_root_for_tests(&bootstrap);
        let commands = start_tui_commands(&mut tui, &bootstrap, &project_root).unwrap();
        assert!(tui.view().dialog.is_some());
        assert!(tui.transcript().is_empty());
        let router = TuiRuntimeRouter::new(
            bootstrap.clone(),
            Arc::clone(&session),
            cancellation,
            commands,
            Arc::new(SkillCatalog::default()),
        );
        let captured = Arc::new(Mutex::new(Vec::new()));

        submit_tui_command(
            &mut tui,
            &router,
            &bootstrap,
            "/shared   hello world   ",
            &captured,
        );
        assert!(tui.transcript().contains(&agens_tui::TranscriptEntry::User(
            "/shared   hello world   ".into()
        )));
        submit_tui_command(
            &mut tui,
            &router,
            &bootstrap,
            "/global-only   value   ",
            &captured,
        );
        submit_tui_command(
            &mut tui,
            &router,
            &bootstrap,
            "/slash-template text",
            &captured,
        );

        let requests = captured.lock().unwrap();
        assert_eq!(
            requests
                .iter()
                .map(|request| request.prompt.as_str())
                .collect::<Vec<_>>(),
            vec![
                "project:hello world",
                "Keep literal text [value]",
                "/literal text",
            ]
        );
        drop(requests);

        for input in ["/connect custom", "/unknown"] {
            submit_tui_command(&mut tui, &router, &bootstrap, input, &captured);
        }
        assert_eq!(captured.lock().unwrap().len(), 3);
        assert!(tui.view().dialog.is_some());
        assert!(session.lock().unwrap().messages.is_empty());

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_startup_skills_reach_parent_context_and_tool_with_builtin_subagents() {
        let temporary = tui_session_directory("parent-skills");
        let config_home = temporary.join("config");
        let global_skills = config_home.join("skills");
        let project_skills = temporary.join("project/.agens/skills");
        write_tui_skill(
            &global_skills,
            "alpha",
            "global alpha",
            "GLOBAL_ALPHA_BODY_SENTINEL",
        );
        write_tui_skill(
            &global_skills,
            "shared",
            "global shared",
            "GLOBAL_SHARED_BODY_SENTINEL",
        );
        write_tui_skill(
            &global_skills,
            "invoke",
            "global invoke",
            "GLOBAL_INVOKE_BODY_SENTINEL",
        );
        write_tui_skill(
            &project_skills,
            "shared",
            "project shared",
            "PROJECT_SHARED_BODY_SENTINEL",
        );
        write_tui_skill(
            &project_skills,
            "invoke",
            "project invoke",
            "PROJECT_INVOKE_BODY_SENTINEL",
        );
        write_tui_skill(
            &project_skills,
            "broken",
            "broken after startup",
            "BROKEN_BODY_SENTINEL",
        );
        let global_commands = config_home.join("commands");
        std::fs::create_dir_all(&global_commands).unwrap();
        write_tui_command(
            &global_commands,
            "shared",
            "command wins",
            "COMMAND:$ARGUMENTS",
        );
        std::fs::create_dir_all(project_skills.join("shared/references")).unwrap();
        std::fs::write(
            project_skills.join("shared/references/guide.md"),
            "RESOURCE_SENTINEL",
        )
        .unwrap();

        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let session = Arc::new(Mutex::new(SessionContext::fresh()));
        let cancellation = Arc::new(Mutex::new(None));
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::clone(&cancellation),
        });
        let project_root = agens_bootstrap::session_root::discovered_root_for_tests(&bootstrap);
        let commands = start_tui_commands(&mut tui, &bootstrap, &project_root).unwrap();
        let skills = start_tui_skills(&mut tui, &bootstrap, &project_root).unwrap();
        report_tui_extension_collisions(&mut tui, &commands, &skills);
        assert!(tui.view().dialog.is_some());
        assert!(tui.transcript().is_empty());
        let router = TuiRuntimeRouter::new(
            bootstrap.clone(),
            session,
            cancellation,
            commands,
            Arc::clone(&skills),
        );
        let captured = Arc::new(Mutex::new(Vec::new()));

        submit_tui_command(&mut tui, &router, &bootstrap, "normal prompt", &captured);

        let request = captured.lock().unwrap()[0].clone();
        let context = request.system_prompt.unwrap();
        assert_eq!(context.matches("## Available skills").count(), 1);
        assert!(context.contains("- alpha: global alpha"));
        assert!(context.contains("- invoke: project invoke"));
        assert!(context.contains("- shared: project shared"));
        for secret in [
            "GLOBAL_ALPHA_BODY_SENTINEL",
            "GLOBAL_SHARED_BODY_SENTINEL",
            "GLOBAL_INVOKE_BODY_SENTINEL",
            "PROJECT_SHARED_BODY_SENTINEL",
            "PROJECT_INVOKE_BODY_SENTINEL",
            "BROKEN_BODY_SENTINEL",
            "RESOURCE_SENTINEL",
        ] {
            assert!(!context.contains(secret));
        }

        let (tools, dispatcher) = production_tool_runtime(
            &bootstrap,
            &agens_bootstrap::session_root::discovered_root_for_tests(&bootstrap),
            Some(skills.as_ref()),
        )
        .unwrap();
        assert!(tools.iter().any(|tool| tool.name() == "skill"));
        assert!(tools.iter().any(|tool| tool.name() == "task"));
        assert!(
            dispatcher
                .lock()
                .unwrap()
                .canonical_identity("skill")
                .is_some()
        );
        let policy = PermissionPolicy::new(
            PermissionMode::Edit,
            vec![PermissionRule::global(
                PermissionDecision::Allow,
                PermissionPattern::Exact("native::skill".into()),
                PermissionPattern::Any,
            )],
        );
        let mut dispatcher = dispatcher.lock().unwrap();
        let ToolEvaluationOutcome::Authorized(call) = dispatcher
            .evaluate(
                &policy,
                &[],
                &PermissionSession::new(),
                ToolDispatchRequest::new("project", "skill", serde_json::json!({"skill":"shared"})),
            )
            .unwrap()
        else {
            panic!("skill tool should pass normal authorization");
        };
        assert_eq!(
            dispatcher
                .execute(
                    call,
                    &ToolExecutionContext::with_timeout(std::time::Duration::from_secs(1)),
                )
                .unwrap()
                .content,
            "PROJECT_SHARED_BODY_SENTINEL"
        );
        drop(dispatcher);

        submit_tui_command(
            &mut tui,
            &router,
            &bootstrap,
            "/invoke   explicit arguments   ",
            &captured,
        );
        submit_tui_command(
            &mut tui,
            &router,
            &bootstrap,
            "/shared command arguments",
            &captured,
        );
        std::fs::remove_file(project_skills.join("broken/SKILL.md")).unwrap();
        submit_tui_command(&mut tui, &router, &bootstrap, "/broken args", &captured);

        let requests = captured.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests[1].prompt,
            "## Skill: invoke\nPROJECT_INVOKE_BODY_SENTINEL\n\n## User arguments\nexplicit arguments"
        );
        assert_eq!(requests[2].prompt, "COMMAND:command arguments");
        assert!(tui.transcript().contains(&agens_tui::TranscriptEntry::User(
            "/invoke   explicit arguments   ".into()
        )));
        assert!(tui.view().dialog.is_some());
        drop(requests);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_palette_uses_the_resolved_surface_and_renders_inside_a_narrow_resize() {
        let temporary = tui_session_directory("resolved-palette");
        let config_home = temporary.join("config");
        let global_commands = config_home.join("commands");
        let project_commands = temporary.join("project/.agens/commands");
        let global_skills = config_home.join("skills");
        let project_skills = temporary.join("project/.agens/skills");
        std::fs::create_dir_all(&global_commands).unwrap();
        std::fs::create_dir_all(&project_commands).unwrap();
        write_tui_command(&global_commands, "shared", "global command", "global");
        write_tui_command(&project_commands, "shared", "project command", "project");
        write_tui_command(
            &project_commands,
            "review",
            "review changes",
            "review:$ARGUMENTS",
        );
        write_tui_command(&project_commands, "connect", "reserved collision", "wrong");
        write_tui_skill(&global_skills, "shared", "shadowed skill", "wrong");
        write_tui_skill(&project_skills, "inspect", "inspect code", "INSPECT");
        std::fs::write(
            project_commands.join("broken.md"),
            "---\ndescription: [invalid\n---\nbroken\n",
        )
        .unwrap();

        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let session = Arc::new(Mutex::new(SessionContext::fresh()));
        let cancellation = Arc::new(Mutex::new(None));
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::clone(&cancellation),
        });
        let project_root = agens_bootstrap::session_root::discovered_root_for_tests(&bootstrap);
        let commands = start_tui_commands(&mut tui, &bootstrap, &project_root).unwrap();
        let skills = start_tui_skills(&mut tui, &bootstrap, &project_root).unwrap();
        report_tui_extension_collisions(&mut tui, &commands, &skills);
        let router = TuiRuntimeRouter::new(
            bootstrap,
            Arc::clone(&session),
            cancellation,
            commands,
            skills,
        );
        let entries = router.palette_entries().unwrap();

        assert_eq!(
            entries.iter().map(|entry| entry.name()).collect::<Vec<_>>(),
            vec![
                "connect",
                "disconnect",
                "diagnostics",
                "new",
                "sessions",
                "resume",
                "agent",
                "provider",
                "model",
                "effort",
                "help",
                "mcp",
                "select",
                "quit",
                "subagent",
                "subagents",
                "review",
                "shared",
                "inspect",
            ]
        );
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.name() == "shared")
                .count(),
            1
        );
        assert_eq!(
            entries
                .iter()
                .find(|entry| entry.name() == "shared")
                .unwrap()
                .kind(),
            agens_tui::PaletteEntryKind::Command
        );
        assert_eq!(
            entries
                .iter()
                .find(|entry| entry.name() == "shared")
                .unwrap()
                .description(),
            "project command"
        );
        assert!(entries.iter().any(|entry| entry.name() == "subagent"));
        assert!(tui.transcript().is_empty());
        assert!(tui.view().dialog.is_some());

        tui.set_palette_entries(entries.to_vec());
        for character in "/sha".chars() {
            tui.handle(agens_tui::Event::Key(agens_tui::Key::Char(character)));
        }
        tui.handle(agens_tui::Event::Resize {
            width: 20,
            height: 6,
        });
        let terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(20, 6)).unwrap();
        let mut renderer = agens_tui::RatatuiRenderer::new(terminal);
        agens_tui::Renderer::render(&mut renderer, tui.view()).unwrap();
        let text = renderer
            .terminal()
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("commands"), "{text:?}");
        assert!(text.contains("/shared"), "{text:?}");
        assert!(!text.contains("inspect"), "{text:?}");

        let original = session.lock().unwrap().clone();
        assert_eq!(
            tui.handle(agens_tui::Event::Key(agens_tui::Key::Escape)),
            agens_tui::Action::Render
        );
        assert_eq!(tui.input(), "/sha");
        assert_eq!(*session.lock().unwrap(), original);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_palette_enter_routes_built_in_command_skill_help_quit_and_unknown_once() {
        let temporary = tui_session_directory("palette-routing");
        let config_home = temporary.join("config");
        let project_commands = temporary.join("project/.agens/commands");
        let project_skills = temporary.join("project/.agens/skills");
        std::fs::create_dir_all(config_home.join("commands")).unwrap();
        std::fs::create_dir_all(&project_commands).unwrap();
        write_tui_command(
            &project_commands,
            "review",
            "review changes",
            "REVIEW:$ARGUMENTS",
        );
        write_tui_skill(&project_skills, "inspect", "inspect code", "INSPECT_BODY");

        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let session = Arc::new(Mutex::new(SessionContext::fresh()));
        let cancellation = Arc::new(Mutex::new(None));
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::clone(&cancellation),
        });
        let project_root = agens_bootstrap::session_root::discovered_root_for_tests(&bootstrap);
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
        let mut provider_prompts = Vec::new();

        for (input, expected) in [
            ("/review target", "REVIEW:target"),
            (
                "/inspect src",
                "## Skill: inspect\nINSPECT_BODY\n\n## User arguments\nsrc",
            ),
        ] {
            let input = enter_tui_input(&mut tui, input);
            let prompt = tui.apply_submission_outcome(router.route(input)).unwrap();
            provider_prompts.push(prompt.clone());
            tui.finish_provider_turn(TuiProviderOutcome::Completed("captured".into()));
            assert_eq!(prompt, expected);
        }

        let sessions = router.open_dialog("sessions").unwrap();
        assert!(matches!(sessions, TuiSubmissionOutcome::Dialog(_)));
        assert!(matches!(
            router.route("/help".into()),
            TuiSubmissionOutcome::Dialog(_)
        ));
        assert!(matches!(
            router.route("/mouse".into()),
            TuiSubmissionOutcome::LocalActionableError { .. }
        ));

        let unknown = enter_tui_input(&mut tui, "/unknown");
        assert!(
            tui.apply_submission_outcome(router.route(unknown))
                .is_none()
        );
        assert_eq!(provider_prompts.len(), 2);
        assert!(session.lock().unwrap().messages.is_empty());

        let quit = enter_tui_input(&mut tui, "/quit");
        assert_eq!(router.route(quit), TuiSubmissionOutcome::Quit);

        std::fs::remove_dir_all(temporary).unwrap();
    }
}
