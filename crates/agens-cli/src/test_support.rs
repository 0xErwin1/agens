//! Shared call counters used by the production TUI-resume and tool/provider
//! runtime tests, plus fixture helpers shared by more than one module's test
//! suite. Kept in one place so every consumer reaches a named function
//! instead of duplicating fixture setup or reaching across a module boundary
//! into a `thread_local!`.
#![cfg(test)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::CliDependencies;
use crate::bootstrap::{Bootstrap, bootstrap};

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
