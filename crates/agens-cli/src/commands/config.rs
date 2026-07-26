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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use agens_config::{parse_toml_document, validate_toml_document};

    use super::*;
    use crate::ExitStatus;

    #[test]
    fn config_init_writes_a_starter_file_the_validator_accepts() {
        let temporary =
            std::env::temp_dir().join(format!("agens-config-init-{}", std::process::id()));
        let written = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&written);
        let dependencies = CliDependencies::for_test(
            temporary.join("project"),
            Some(temporary.join("home")),
            BTreeMap::new(),
            BTreeMap::new(),
        )
        .with_create_file(move |path, contents| {
            recorder
                .lock()
                .unwrap()
                .push((path.to_path_buf(), contents.to_owned()));
            Ok(())
        });

        let report = run_config(cli::ConfigAction::Init, &dependencies).expect("init should run");

        let written = written.lock().unwrap();
        let (path, contents) = written.first().expect("init should write exactly one file");
        assert_eq!(written.len(), 1);
        assert!(path.ends_with(".agens/config.toml"));
        assert!(report.contains(".agens/config.toml"));
        validate_toml_document(&parse_toml_document(contents).expect("starter file must parse"))
            .expect("starter file must validate");
    }

    #[test]
    fn config_init_refuses_to_replace_an_existing_configuration() {
        let temporary =
            std::env::temp_dir().join(format!("agens-config-init-existing-{}", std::process::id()));
        let project_root = temporary.join("project");
        let dependencies = CliDependencies::for_test(
            project_root.clone(),
            Some(temporary.join("home")),
            BTreeMap::new(),
            BTreeMap::from([(
                project_root.join(".agens/config.toml"),
                "[tools]\nmax_search_depth = 4\n".to_owned(),
            )]),
        )
        .with_create_file(|_, _| panic!("init must not write over an existing configuration"));

        let error = run_config(cli::ConfigAction::Init, &dependencies)
            .expect_err("init must refuse an existing file");

        assert_eq!(error.status(), ExitStatus::Configuration);
        assert!(error.message.contains("already exists"));
    }

    #[test]
    fn config_doctor_reports_effective_values_with_their_origin() {
        let temporary =
            std::env::temp_dir().join(format!("agens-doctor-report-{}", std::process::id()));
        let config_home = temporary.join("config");
        let project_root = temporary.join("project");
        let dependencies = CliDependencies::for_test(
            project_root.clone(),
            Some(temporary.join("home")),
            BTreeMap::from([(
                "AGENS_CONFIG_HOME".to_owned(),
                config_home.display().to_string(),
            )]),
            BTreeMap::from([
                (
                    config_home.join("config.toml"),
                    "[tools]\nmax_search_depth = 8\n".to_owned(),
                ),
                (
                    project_root.join(".agens/config.toml"),
                    "[tools]\nmax_search_results = 25\n".to_owned(),
                ),
            ]),
        );

        let report =
            run_config(cli::ConfigAction::Doctor, &dependencies).expect("doctor should run");

        assert!(report.contains("Status:  valid\n"));
        assert!(report.contains("tools.max_search_depth      8             global\n"));
        assert!(report.contains("tools.max_search_results    25            project\n"));
        assert!(report.contains("tools.max_list_entries      1000          default\n"));
        assert!(report.contains("agent.default_agent         -             default\n"));
    }
}
