use std::sync::Arc;

use agens_tools::{CommandCatalog, CommandDefinition, SkillCatalog};
use agens_tui::{Engine as TuiEngine, PaletteEntry, PaletteEntryKind, Tui};

use crate::bootstrap::Bootstrap;
use crate::error::CliError;

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

pub(crate) fn start_tui_commands<E: TuiEngine>(
    tui: &mut Tui<E>,
    bootstrap: &Bootstrap,
) -> Result<Arc<CommandCatalog>, CliError> {
    let global_root = bootstrap
        .paths
        .global_config
        .parent()
        .ok_or_else(|| CliError::configuration("global command root is unavailable"))?
        .join("commands");
    let project_root = bootstrap
        .paths
        .project_config
        .parent()
        .ok_or_else(|| CliError::configuration("project command root is unavailable"))?
        .join("commands");
    let built_ins = RESERVED_TUI_COMMANDS
        .iter()
        .map(|name| {
            CommandDefinition::new(*name, "Reserved TUI command", *name)
                .expect("reserved TUI command names are valid")
        })
        .collect::<Vec<_>>();
    let discovery = CommandCatalog::discover(&built_ins, global_root, project_root)
        .map_err(CliError::configuration)?;

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
) -> Result<Arc<SkillCatalog>, CliError> {
    let discovery = discover_skill_catalog(bootstrap)?;
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

pub(crate) fn discover_skill_catalog(
    bootstrap: &Bootstrap,
) -> Result<agens_tools::SkillDiscovery, CliError> {
    SkillCatalog::discover(
        bootstrap.paths.global_config.with_file_name("skills"),
        bootstrap.paths.project_config.with_file_name("skills"),
    )
    .map_err(|_| CliError::configuration("skill catalog is unavailable"))
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
