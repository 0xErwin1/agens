//! The `config` command: reports the effective configuration with provenance
//! (`doctor`) and writes a documented starter configuration file (`init`).

use std::fs;
use std::io::Write;
use std::path::Path;

use agens_config::{ConfiguredValue, Origin, ResolvedSettings, resolve_paths, starter_document};

use crate::CliDependencies;
use crate::bootstrap::{bootstrap, discover_project_root};
use crate::cli;
use crate::error::CliError;

pub(crate) fn run_config(
    action: cli::ConfigAction,
    dependencies: &CliDependencies,
) -> Result<String, CliError> {
    match action {
        cli::ConfigAction::Init => run_config_init(dependencies),
        cli::ConfigAction::Doctor => {
            let bootstrap = bootstrap(dependencies)?;
            Ok(format!(
                "Agens config doctor\nGlobal:  {} ({})\nProject: {} ({})\nModel:   {}\nStatus:  valid\n\n{}",
                bootstrap.paths.global_config.display(),
                source_status(bootstrap.global_loaded),
                bootstrap.paths.project_config.display(),
                source_status(bootstrap.project_loaded),
                bootstrap.model.as_deref().unwrap_or("-"),
                effective_settings_report(bootstrap.settings())
            ))
        }
    }
}

/// Writes a documented starter configuration for the current project. Refuses
/// to touch an existing file: the command creates configuration, it never
/// rewrites what a user already wrote.
fn run_config_init(dependencies: &CliDependencies) -> Result<String, CliError> {
    let current_directory = (dependencies.current_directory)()?;
    let home_directory = (dependencies.home_directory)();
    let environment = (dependencies.environment)();
    let project_root = discover_project_root(&current_directory).unwrap_or(current_directory);
    let paths = resolve_paths(&project_root, home_directory.as_deref(), &environment);

    if (dependencies.read_file)(&paths.project_config)?.is_some() {
        return Err(CliError::configuration(format!(
            "configuration already exists at {}",
            paths.project_config.display()
        )));
    }

    (dependencies.create_file)(&paths.project_config, &starter_document())?;

    Ok(format!("Wrote {}\n", paths.project_config.display()))
}

pub(crate) fn create_configuration_file(path: &Path, contents: &str) -> Result<(), CliError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|_| CliError::configuration("configuration directory is unavailable"))?;
    }

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|_| CliError::configuration("configuration file cannot be created"))?;
    file.write_all(contents.as_bytes())
        .map_err(|_| CliError::configuration("configuration file cannot be written"))
}

/// Renders every catalog setting with its effective value and the layer that
/// supplied it. Reads only configuration; credentials are never consulted.
fn effective_settings_report(settings: &ResolvedSettings) -> String {
    let width = settings
        .iter()
        .map(|(path, _)| path.chars().count())
        .max()
        .unwrap_or_default();
    let mut report = String::from("Settings:\n");

    for (path, setting) in settings.iter() {
        let value = match &setting.value {
            ConfiguredValue::Bool(value) => value.to_string(),
            ConfiguredValue::Integer(value) => value.to_string(),
            ConfiguredValue::Text(value) => value.clone(),
            ConfiguredValue::Absent => "-".to_owned(),
        };
        let origin = match setting.origin {
            Origin::Default => "default",
            Origin::Global => "global",
            Origin::Project => "project",
            Origin::Environment => "environment",
        };
        report.push_str(&format!("  {path:<width$}  {value:<12}  {origin}\n"));
    }

    report
}

fn source_status(loaded: bool) -> &'static str {
    if loaded { "loaded" } else { "missing" }
}
