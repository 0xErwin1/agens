use std::collections::BTreeMap;
use std::path::PathBuf;

use agens::{CliDependencies, ExitStatus, execute};

#[test]
fn config_doctor_merges_compatible_paths_and_reports_loaded_sources() {
    let temporary = TemporaryDirectory::new("config-doctor");
    let config_home = temporary.path().join("config");
    let project_root = temporary.path().join("project");
    let global_path = config_home.join("config.toml");
    let project_path = project_root.join(".agens/config.toml");

    let dependencies = CliDependencies::for_test(
        project_root,
        Some(temporary.path().join("home")),
        BTreeMap::from([(
            "AGENS_CONFIG_HOME".to_owned(),
            config_home.display().to_string(),
        )]),
        BTreeMap::from([
            (
                global_path,
                "[provider]\nmodel = \"global-model\"\n".to_owned(),
            ),
            (
                project_path,
                "[provider]\nmodel = \"project-model\"\n".to_owned(),
            ),
        ]),
    );

    let result = execute(["config", "doctor"], &dependencies);

    assert_eq!(result.status, ExitStatus::Success);
    assert!(result.stdout.contains("Agens config doctor\n"));
    assert!(result.stdout.contains("Global:  "));
    assert!(result.stdout.contains("Project: "));
    assert!(result.stdout.contains("Status:  valid\n"));
    assert!(result.stdout.contains("Model:   project-model\n"));
}

#[test]
fn invalid_config_is_a_sanitized_configuration_failure() {
    let temporary = TemporaryDirectory::new("invalid-config");
    let config_home = temporary.path().join("config");
    let project_root = temporary.path().join("project");
    let dependencies = CliDependencies::for_test(
        project_root,
        Some(temporary.path().join("home")),
        BTreeMap::from([(
            "AGENS_CONFIG_HOME".to_owned(),
            config_home.display().to_string(),
        )]),
        BTreeMap::from([(config_home.join("config.toml"), "[provider\n".to_owned())]),
    );

    let result = execute(["config", "doctor"], &dependencies);

    assert_eq!(result.status, ExitStatus::Configuration);
    assert_eq!(result.stdout, "Agens config doctor\nStatus:  invalid\n");
    assert!(
        result
            .stderr
            .starts_with("error: config: global configuration is invalid\n")
    );
    assert!(!result.stderr.contains("[provider"));
}

#[test]
fn command_boundaries_invoke_injected_headless_and_tui_services_without_network() {
    let temporary = TemporaryDirectory::new("services");
    let dependencies = CliDependencies::for_test(
        temporary.path().join("project"),
        Some(temporary.path().join("home")),
        BTreeMap::new(),
        BTreeMap::new(),
    )
    .with_headless_chat(|request, _| Ok(format!("answer:{}", request.prompt)))
    .with_tui_launcher(|_| Ok("tui-selected".to_owned()));

    let chat = execute(["chat", "hello"], &dependencies);
    let tui = execute(std::iter::empty::<&str>(), &dependencies);

    assert_eq!(chat.status, ExitStatus::Success);
    assert_eq!(chat.stdout, "answer:hello\n");
    assert_eq!(tui.status, ExitStatus::Success);
    assert_eq!(tui.stdout, "tui-selected\n");
}

#[test]
fn unavailable_surfaces_fail_explicitly_without_claiming_success() {
    let dependencies = CliDependencies::for_test(
        PathBuf::from("/project"),
        Some(PathBuf::from("/home/user")),
        BTreeMap::new(),
        BTreeMap::new(),
    );

    for arguments in [
        ["models"].as_slice(),
        ["auth", "login"].as_slice(),
        ["sessions", "rm", "1"].as_slice(),
    ] {
        let result = execute(arguments, &dependencies);

        assert_eq!(result.status, ExitStatus::Unavailable);
        assert_eq!(result.stdout, "");
        assert_eq!(
            result.stderr,
            "error: unavailable: this command is not implemented yet\n"
        );
    }
}

#[test]
fn help_and_version_are_successful_without_bootstrapping_configuration() {
    let dependencies = CliDependencies::for_test(
        PathBuf::from("/project"),
        Some(PathBuf::from("/home/user")),
        BTreeMap::new(),
        BTreeMap::new(),
    );

    let root_help = execute(["--help"], &dependencies);
    let chat_help = execute(["chat", "--help"], &dependencies);
    let version = execute(["--version"], &dependencies);

    assert_eq!(root_help.status, ExitStatus::Success);
    assert!(root_help.stdout.contains("Usage: agens <command>\n"));
    assert_eq!(chat_help.status, ExitStatus::Success);
    assert_eq!(chat_help.stdout, "Usage: agens chat [flags] <prompt>\n");
    assert_eq!(version.status, ExitStatus::Success);
    assert_eq!(version.stdout, "agens 0.1.0\n");
}

#[test]
fn auth_status_uses_the_compatible_credentials_path_without_exposing_tokens() {
    let temporary = TemporaryDirectory::new("auth-status");
    let config_home = temporary.path().join("config");
    std::fs::create_dir_all(&config_home).expect("config directory should be created");
    std::fs::write(
        config_home.join("auth.json"),
        r#"{
            "openai-chatgpt": {
                "access_token": "secret-access-token",
                "refresh_token": "secret-refresh-token",
                "account_id": "account_123",
                "expires_at": "2099-01-01T00:00:00Z"
            }
        }"#,
    )
    .expect("credentials should be written");

    let dependencies = CliDependencies::for_test(
        temporary.path().join("project"),
        Some(temporary.path().join("home")),
        BTreeMap::from([(
            "AGENS_CONFIG_HOME".to_owned(),
            config_home.display().to_string(),
        )]),
        BTreeMap::new(),
    );

    let result = execute(["auth", "status"], &dependencies);

    assert_eq!(result.status, ExitStatus::Success);
    assert_eq!(result.stdout, "ChatGPT authentication: ready\n");
    assert!(!result.stdout.contains("secret-"));
}

#[test]
fn sessions_list_uses_configured_data_directory_and_reports_empty_store() {
    let temporary = TemporaryDirectory::new("sessions-list");
    let config_home = temporary.path().join("config");
    let project_root = temporary.path().join("project");
    let data_directory = temporary.path().join("data");
    let dependencies = CliDependencies::for_test(
        project_root,
        Some(temporary.path().join("home")),
        BTreeMap::from([(
            "AGENS_CONFIG_HOME".to_owned(),
            config_home.display().to_string(),
        )]),
        BTreeMap::from([(
            config_home.join("config.toml"),
            format!("[options]\ndata_dir = \"{}\"\n", data_directory.display()),
        )]),
    );

    let result = execute(["sessions", "list"], &dependencies);

    assert_eq!(result.status, ExitStatus::Success);
    assert_eq!(result.stdout, "No saved sessions.\n");
    assert!(data_directory.join("rust-sessions.db").is_file());
}

struct TemporaryDirectory {
    path: PathBuf,
}

impl TemporaryDirectory {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "agens-cli-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock should be after Unix epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).expect("temporary directory should be created");

        Self { path }
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
