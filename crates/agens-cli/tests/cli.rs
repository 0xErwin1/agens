use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use agens::{
    CliDependencies, ExitStatus, TuiModelSelector, TuiModelSource, bootstrap, execute, execute_os,
    execute_with_cancellation,
};
use agens_core::{
    CompletedSessionTurn, HeadlessPermissionGate, HeadlessPermissionResolver, HeadlessToolCall,
    HeadlessToolDispatcher, HeadlessToolOutput, HeadlessTurnCancellation, HeadlessTurnPortError,
    Message, MessagePart, PermissionDecision, ReasoningEffort, Role, SessionMessage,
    SessionMetadata, TurnEvent, TurnProvider, run_headless_turn,
};
use agens_store::{PermissionGrantStore, SessionStore};
use agens_tools::McpTransport;

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

#[cfg(unix)]
#[test]
fn bootstrap_factory_builds_configured_stdio_transport_with_fixed_launch_policy() {
    use std::os::unix::fs::symlink;

    let temporary = TemporaryDirectory::new("mcp-transport-factory");
    let config_home = temporary.path().join("config");
    let project_root = temporary.path().join("project");
    let nested_directory = project_root.join("src/nested");
    let symlinked_directory = temporary.path().join("working-directory");
    let launch_record = temporary.path().join("launch-record");
    let launch_complete = temporary.path().join("launch-complete");
    let config_path = config_home.join("config.toml");
    std::fs::create_dir_all(project_root.join(".git")).expect("repository marker should exist");
    std::fs::create_dir_all(&nested_directory).expect("nested directory should exist");
    symlink(&nested_directory, &symlinked_directory)
        .expect("working directory symlink should exist");
    let script = format!(
        "printf '%s|%s|%s' \"$PWD\" \"$1\" \"$MCP_SENTINEL\" > '{}' && : > '{}' && sleep 5",
        launch_record.display(),
        launch_complete.display(),
    );
    let dependencies = CliDependencies::for_test(
        symlinked_directory,
        Some(temporary.path().join("home")),
        BTreeMap::from([
            (
                "AGENS_CONFIG_HOME".to_owned(),
                config_home.display().to_string(),
            ),
            ("PWD".to_owned(), "$PWD".to_owned()),
            ("MCP_SENTINEL".to_owned(), "$MCP_SENTINEL".to_owned()),
        ]),
        BTreeMap::from([(
            config_path,
            format!(
                "[mcp.files]\ntransport = \"stdio\"\ncommand = \"/bin/sh\"\nargs = [\"-c\", {script:?}, \"ignored\", \"configured-argument\"]\ntimeout_ms = 50\n[mcp.files.env]\nMCP_SENTINEL = \"configured-environment\"\n"
            ),
        )]),
    );

    let bootstrap = bootstrap(&dependencies).expect("validated config should bootstrap");
    let mut transports = bootstrap
        .mcp_transports()
        .expect("factory should create stdio transport");

    assert_eq!(transports.len(), 1);
    assert_eq!(transports[0].0, "files");
    assert_eq!(transports[0].2, std::time::Duration::from_millis(50));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    while !launch_complete.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "configured MCP process should complete its launch record"
        );
        thread::sleep(std::time::Duration::from_millis(2));
    }
    assert_eq!(
        std::fs::read_to_string(launch_record).expect("launch policy should be readable"),
        format!(
            "{}|configured-argument|configured-environment",
            project_root.display()
        )
    );
    transports[0]
        .1
        .close(&agens_tools::McpOperationContext::new(
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            std::time::Duration::from_secs(1),
        ))
        .expect("factory transport should close without dispatching chat");
}

#[test]
fn bootstrap_factory_rejects_an_unusable_project_root() {
    let temporary = TemporaryDirectory::new("mcp-transport-outside-root");
    let config_home = temporary.path().join("config");
    let outside_directory = temporary.path().join("outside");
    std::fs::create_dir_all(&outside_directory).expect("outside directory should exist");
    let dependencies = CliDependencies::for_test(
        outside_directory,
        Some(temporary.path().join("home")),
        BTreeMap::from([(
            "AGENS_CONFIG_HOME".to_owned(),
            config_home.display().to_string(),
        )]),
        BTreeMap::from([(
            config_home.join("config.toml"),
            "[mcp.files]\ntransport = \"stdio\"\ncommand = \"server\"\ntimeout_ms = 50\n"
                .to_owned(),
        )]),
    );

    let bootstrap = bootstrap(&dependencies).expect("config should remain valid");

    assert!(bootstrap.mcp_transports().is_err());
}

#[test]
fn disabled_global_mcp_server_is_not_expanded_or_started() {
    let temporary = TemporaryDirectory::new("disabled-mcp-server");
    let config_home = temporary.path().join("config");
    let project_root = temporary.path().join("project");
    let marker = temporary.path().join("must-not-exist");
    std::fs::create_dir_all(project_root.join(".git")).expect("repository marker should exist");

    let dependencies = CliDependencies::for_test(
        project_root,
        Some(temporary.path().join("home")),
        BTreeMap::from([(
            "AGENS_CONFIG_HOME".to_owned(),
            config_home.display().to_string(),
        )]),
        BTreeMap::from([(
            config_home.join("config.toml"),
            format!(
                "[mcp.disabled]\ndisabled = true\ncommand = \"$(touch {})\"\n",
                marker.display(),
            ),
        )]),
    );

    let bootstrap = bootstrap(&dependencies).expect("disabled global server should be accepted");

    assert!(bootstrap.mcp_transports().unwrap().is_empty());
    assert!(!marker.exists());
}

#[test]
fn global_mcp_command_and_environment_fields_expand_without_expanding_system_prompt() {
    let temporary = TemporaryDirectory::new("mcp-command-expansion");
    let config_home = temporary.path().join("config");
    let project_root = temporary.path().join("project");
    let launch_record = temporary.path().join("launch-record");
    std::fs::create_dir_all(project_root.join(".git")).expect("repository marker should exist");
    let script = format!(
        "printf '%s|' \"$1\" > '{}'; printenv MCP_SENTINEL >> '{}'",
        launch_record.display(),
        launch_record.display()
    );
    let dependencies = CliDependencies::for_test(
        project_root,
        Some(temporary.path().join("home")),
        BTreeMap::from([(
            "AGENS_CONFIG_HOME".to_owned(),
            config_home.display().to_string(),
        )]),
        BTreeMap::from([(
            config_home.join("config.toml"),
            format!(
                "[agent]\nsystem_prompt = \"literal $(printf ignored)\"\n\n[mcp.files]\ntransport = \"stdio\"\ncommand = \"$(printf /bin/sh)\"\nargs = [\"-c\", {script:?}, \"ignored\", \"$(printf configured-argument)\"]\n[mcp.files.env]\nMCP_SENTINEL = \"$(printf 'configured-environment\\\\n')\"\n"
            ),
        )]),
    );

    let bootstrap = bootstrap(&dependencies).expect("global MCP substitutions should expand");
    let mut transports = bootstrap
        .mcp_transports()
        .expect("expanded MCP transport should launch");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    while std::fs::read_to_string(&launch_record)
        .map(|contents| !contents.contains("configured-environment"))
        .unwrap_or(true)
    {
        assert!(
            std::time::Instant::now() < deadline,
            "MCP process should launch"
        );
        thread::sleep(std::time::Duration::from_millis(2));
    }

    assert_eq!(
        std::fs::read_to_string(launch_record).expect("launch record should be readable"),
        "configured-argument|configured-environment\n"
    );
    assert_eq!(bootstrap.system_prompt(), Some("literal $(printf ignored)"));
    transports[0]
        .1
        .close(&agens_tools::McpOperationContext::new(
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            std::time::Duration::from_secs(1),
        ))
        .expect("transport should close");
}

#[test]
fn project_mcp_is_rejected_before_its_command_substitution_runs() {
    let temporary = TemporaryDirectory::new("project-mcp-rejection");
    let config_home = temporary.path().join("config");
    let project_root = temporary.path().join("project");
    let marker = temporary.path().join("must-not-exist");
    std::fs::create_dir_all(project_root.join(".agens"))
        .expect("project config directory should exist");

    let dependencies = CliDependencies::for_test(
        project_root.clone(),
        Some(temporary.path().join("home")),
        BTreeMap::from([(
            "AGENS_CONFIG_HOME".to_owned(),
            config_home.display().to_string(),
        )]),
        BTreeMap::from([(
            project_root.join(".agens/config.toml"),
            format!(
                "[mcp.forbidden]\ntransport = \"stdio\"\ncommand = \"$(touch {})\"\n",
                marker.display(),
            ),
        )]),
    );

    let result = execute(["config", "doctor"], &dependencies);

    assert_eq!(result.status, ExitStatus::Configuration);
    assert_eq!(
        result.stderr,
        "error: config: project configuration cannot define MCP servers\n"
    );
    assert!(!marker.exists());
}

#[test]
fn explicit_provider_selection_overrides_credential_inference() {
    let temporary = TemporaryDirectory::new("explicit-provider-selection");
    let config_home = temporary.path().join("config");
    let project_root = temporary.path().join("project");
    let dependencies = CliDependencies::for_test(
        project_root,
        Some(temporary.path().join("home")),
        BTreeMap::from([(
            "AGENS_CONFIG_HOME".to_owned(),
            config_home.display().to_string(),
        )]),
        BTreeMap::from([
            (
                config_home.join("config.toml"),
                "[provider]\ntype = \"openai-chatgpt\"\n".to_owned(),
            ),
            (
                config_home.join("auth.json"),
                r#"{"openai-api":{"api_key":"api-key"},"openai-chatgpt":{"access_token":"access","refresh_token":"refresh","account_id":"account","expires_at":"2099-01-01T00:00:00Z"}}"#.to_owned(),
            ),
        ]),
    );

    let bootstrap = bootstrap(&dependencies).expect("explicit provider should bootstrap");

    assert_eq!(bootstrap.provider_type(), Some("openai-chatgpt"));
    assert_eq!(bootstrap.model(), None);
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
    .with_headless_chat(|request, _, _| Ok(format!("answer:{}", request.prompt)))
    .with_tui_launcher(|_, resume| Ok(format!("tui-selected:{resume:?}")));

    let chat = execute(["chat", "hello"], &dependencies);
    let tui = execute(["--resume", "7"], &dependencies);

    assert_eq!(chat.status, ExitStatus::Success);
    assert_eq!(chat.stdout, "answer:hello\n");
    assert_eq!(tui.status, ExitStatus::Success);
    assert_eq!(tui.stdout, "tui-selected:Some(7)\n");
}

#[test]
fn models_lists_the_bundled_snapshot_deterministically() {
    let dependencies = CliDependencies::for_test(
        PathBuf::from("/project"),
        Some(PathBuf::from("/home/user")),
        BTreeMap::new(),
        BTreeMap::new(),
    );

    let first = execute(["models"], &dependencies);
    let second = execute(["models"], &dependencies);

    assert_eq!(first.status, ExitStatus::Success);
    assert_eq!(
        first.stdout,
        "ID\tNAME\tCONTEXT\tPRICE\ngpt-4.1\tGPT-4.1\t1047576\t$2.00/$8.00\ngpt-4.1-mini\tGPT-4.1 mini\t1047576\t$0.40/$1.60\ngpt-4.1-nano\tGPT-4.1 nano\t1047576\t$0.10/$0.40\ngpt-4o\tGPT-4o\t128000\t$2.50/$10.00\ngpt-4o-mini\tGPT-4o mini\t128000\t$0.15/$0.60\no3\to3\t200000\t$2.00/$8.00\no4-mini\to4-mini\t200000\t$1.10/$4.40\n"
    );
    assert_eq!(first.stderr, "");
    assert_eq!(second.status, ExitStatus::Success);
    assert_eq!(second.stdout, first.stdout);
    assert_eq!(second.stderr, "");
}

#[test]
fn unavailable_surfaces_fail_explicitly_without_claiming_success() {
    let dependencies = CliDependencies::for_test(
        PathBuf::from("/project"),
        Some(PathBuf::from("/home/user")),
        BTreeMap::new(),
        BTreeMap::new(),
    );

    let result = execute(["auth", "login"], &dependencies);

    assert_eq!(result.status, ExitStatus::Unavailable);
    assert_eq!(result.stdout, "");
    assert_eq!(
        result.stderr,
        "error: unavailable: this command is not implemented yet\n"
    );
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
fn auth_login_selects_browser_or_device_flow_and_uses_the_compatible_credentials_path() {
    let temporary = TemporaryDirectory::new("auth-login");
    let config_home = temporary.path().join("config");
    let credentials_path = config_home.join("auth.json");
    let selected_flows = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let dependencies = CliDependencies::for_test(
        temporary.path().join("project"),
        Some(temporary.path().join("home")),
        BTreeMap::from([(
            "AGENS_CONFIG_HOME".to_owned(),
            config_home.display().to_string(),
        )]),
        BTreeMap::new(),
    )
    .with_auth_login({
        let selected_flows = std::sync::Arc::clone(&selected_flows);
        move |path, device_auth, _| {
            selected_flows
                .lock()
                .expect("flow recording lock should be available")
                .push(device_auth);
            assert_eq!(path, credentials_path);
            Ok(String::new())
        }
    });

    let browser = execute(["auth", "login"], &dependencies);
    let device = execute(["auth", "login", "--device-auth"], &dependencies);

    assert_eq!(browser.status, ExitStatus::Success);
    assert_eq!(browser.stdout, "Logged in to ChatGPT.\n");
    assert_eq!(device.status, ExitStatus::Success);
    assert_eq!(device.stdout, "Logged in to ChatGPT.\n");
    assert_eq!(
        *selected_flows
            .lock()
            .expect("flow recording lock should be available"),
        vec![false, true]
    );
}

#[test]
fn auth_login_stops_before_start_for_command_cancellation_or_timeout() {
    let dependencies = CliDependencies::for_test(
        PathBuf::from("/project"),
        Some(PathBuf::from("/home/user")),
        BTreeMap::new(),
        BTreeMap::new(),
    )
    .with_auth_login(|_, _, _| panic!("a stopped login must not reach the provider"));
    let cancelled = HeadlessTurnCancellation::new();
    cancelled.cancel();
    let expired = HeadlessTurnCancellation::with_deadline(Duration::ZERO);

    for (cancellation, expected) in [
        (cancelled, "error: auth: ChatGPT login was cancelled\n"),
        (expired, "error: auth: ChatGPT login timed out\n"),
    ] {
        let result = execute_with_cancellation(["auth", "login"], &dependencies, &cancellation);
        assert_eq!(result.status, ExitStatus::Authentication);
        assert_eq!(result.stderr, expected);
    }
}

#[test]
fn auth_logout_removes_only_chatgpt_credentials_and_reports_absence() {
    let temporary = TemporaryDirectory::new("auth-logout");
    let config_home = temporary.path().join("config");
    std::fs::create_dir_all(&config_home).expect("config directory should be created");
    std::fs::write(
        config_home.join("auth.json"),
        r#"{"openai-chatgpt":{"access_token":"secret-access","refresh_token":"secret-refresh","account_id":"account_123","expires_at":"2099-01-01T00:00:00Z"},"other":{"api_key":"preserved"}}"#,
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

    let removed = execute(["auth", "logout", "openai-chatgpt"], &dependencies);
    let absent = execute(["auth", "logout", "openai-chatgpt"], &dependencies);
    let credentials = std::fs::read_to_string(config_home.join("auth.json"))
        .expect("remaining credentials should be readable");

    assert_eq!(removed.status, ExitStatus::Success);
    assert_eq!(removed.stdout, "Logged out of openai-chatgpt.\n");
    assert_eq!(absent.status, ExitStatus::Success);
    assert_eq!(absent.stdout, "No credentials stored for openai-chatgpt.\n");
    assert!(credentials.contains(r#""other":{"api_key":"preserved"}"#));
    assert!(!credentials.contains("secret-"));
}

#[cfg(unix)]
#[test]
fn api_key_login_flag_updates_only_the_selected_provider_with_private_credentials() {
    use std::os::unix::fs::PermissionsExt;

    let temporary = TemporaryDirectory::new("api-key-login-flag");
    let config_home = temporary.path().join("config");
    let credentials_path = config_home.join("auth.json");
    let sentinel = "SENTINEL_API_KEY_FLAG";
    std::fs::create_dir_all(&config_home).expect("config directory should be created");
    std::fs::write(
        &credentials_path,
        r#"{"openai-chatgpt":{"access_token":"preserved-access","refresh_token":"preserved-refresh","account_id":"account_123","expires_at":"2099-01-01T00:00:00Z"},"other":{"api_key":"preserved"}}"#,
    )
    .expect("credentials should be written");

    let login = Command::new(env!("CARGO_BIN_EXE_agens"))
        .args([
            "auth",
            "login",
            "api-key",
            "openai-api",
            "--api-key",
            sentinel,
        ])
        .env("AGENS_CONFIG_HOME", &config_home)
        .output()
        .expect("API-key login should execute");
    let status = Command::new(env!("CARGO_BIN_EXE_agens"))
        .args(["auth", "status", "openai-api"])
        .env("AGENS_CONFIG_HOME", &config_home)
        .output()
        .expect("selected provider status should execute");
    let logout = Command::new(env!("CARGO_BIN_EXE_agens"))
        .args(["auth", "logout", "openai-api"])
        .env("AGENS_CONFIG_HOME", &config_home)
        .output()
        .expect("selected provider logout should execute");
    let credentials = std::fs::read_to_string(&credentials_path)
        .expect("remaining credentials should be readable");

    assert!(login.status.success());
    assert_eq!(
        String::from_utf8_lossy(&login.stdout),
        "Logged in to openai-api.\n"
    );
    assert_eq!(String::from_utf8_lossy(&login.stderr), "");
    assert_eq!(
        String::from_utf8_lossy(&status.stdout),
        "OpenAI API authentication: ready\n"
    );
    assert!(logout.status.success());
    assert_eq!(
        String::from_utf8_lossy(&logout.stdout),
        "Logged out of openai-api.\n"
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&credentials)
            .expect("remaining credentials should remain valid JSON"),
        serde_json::json!({
            "openai-chatgpt": {
                "access_token": "preserved-access",
                "refresh_token": "preserved-refresh",
                "account_id": "account_123",
                "expires_at": "2099-01-01T00:00:00Z"
            },
            "other": { "api_key": "preserved" }
        })
    );
    assert_eq!(
        std::fs::metadata(&credentials_path)
            .expect("credential metadata should be readable")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(!format!("{login:?}{status:?}{logout:?}").contains(sentinel));
}

#[test]
fn api_key_login_reads_one_non_tty_line_and_rejects_invalid_input_without_persistence() {
    let temporary = TemporaryDirectory::new("api-key-login-stdin");
    let config_home = temporary.path().join("config");
    let credentials_path = config_home.join("auth.json");
    let sentinel = "SENTINEL_API_KEY_STDIN";
    std::fs::create_dir_all(&config_home).expect("config directory should be created");

    let mut login = Command::new(env!("CARGO_BIN_EXE_agens"))
        .args(["auth", "login", "api-key", "openai-api"])
        .env("AGENS_CONFIG_HOME", &config_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("API-key login should start");
    login
        .stdin
        .take()
        .expect("stdin should be piped")
        .write_all(format!("  {sentinel}  \n").as_bytes())
        .expect("stdin should accept one key line");
    let login = login
        .wait_with_output()
        .expect("API-key login should complete");

    assert!(login.status.success());
    assert_eq!(
        String::from_utf8_lossy(&login.stdout),
        "Logged in to openai-api.\n"
    );
    assert_eq!(String::from_utf8_lossy(&login.stderr), "");
    assert!(
        std::fs::read_to_string(&credentials_path)
            .expect("credentials should be readable")
            .contains(&format!(r#""api_key":"{sentinel}""#))
    );
    assert!(!format!("{login:?}").contains(sentinel));

    for (name, arguments, stdin) in [
        (
            "empty flag",
            vec!["auth", "login", "api-key", "openai-api", "--api-key", "   "],
            None,
        ),
        (
            "multiple lines",
            vec!["auth", "login", "api-key", "openai-api"],
            Some("one\ntwo\n"),
        ),
        (
            "empty stdin",
            vec!["auth", "login", "api-key", "openai-api"],
            Some("  \n"),
        ),
        (
            "unsupported provider",
            vec![
                "auth",
                "login",
                "api-key",
                "openai-chatgpt",
                "--api-key",
                sentinel,
            ],
            None,
        ),
    ] {
        let isolated_home = temporary.path().join(name);
        std::fs::create_dir_all(&isolated_home).expect("isolated config directory should exist");
        let mut command = Command::new(env!("CARGO_BIN_EXE_agens"));
        command
            .args(arguments)
            .env("AGENS_CONFIG_HOME", &isolated_home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("invalid login should start");
        if let Some(stdin) = stdin {
            child
                .stdin
                .take()
                .expect("stdin should be piped")
                .write_all(stdin.as_bytes())
                .expect("stdin should accept invalid input");
        }
        let output = child
            .wait_with_output()
            .expect("invalid login should complete");

        assert_eq!(output.status.code(), Some(2), "{name}");
        assert!(!isolated_home.join("auth.json").exists(), "{name}");
        assert!(!format!("{output:?}").contains(sentinel), "{name}");
    }
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

#[test]
fn sessions_crud_uses_normalized_metadata_and_idempotent_removal() {
    let temporary = TemporaryDirectory::new("normalized-sessions");
    let config_home = temporary.path().join("config");
    let data_directory = temporary.path().join("data");
    let dependencies = CliDependencies::for_test(
        temporary.path().join("project"),
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
    let metadata = SessionMetadata {
        id: 7,
        project: "project".into(),
        title: "conversation".into(),
        active_agent: "primary".into(),
        created_at: 10,
        updated_at: 20,
        completed_turn_count: 0,
        resumable: false,
    };
    let turn = CompletedSessionTurn::new(
        vec![
            Message {
                role: Role::User,
                parts: vec![MessagePart::Text("hello".into())],
            },
            Message {
                role: Role::Assistant,
                parts: vec![MessagePart::Text("world".into())],
            },
        ]
        .into_iter()
        .map(SessionMessage::try_from)
        .collect::<Result<_, _>>()
        .expect("session messages should be valid"),
    )
    .expect("completed session turn should be valid");
    let mut store = SessionStore::open(&data_directory).expect("session store should open");
    store
        .persist_completed_session_turn(&metadata, &turn)
        .expect("normalized session should persist");

    let list = execute(["sessions", "list"], &dependencies);
    let show = execute(["sessions", "show", "7"], &dependencies);
    let remove = execute(["sessions", "rm", "7"], &dependencies);
    let remove_again = execute(["sessions", "rm", "7"], &dependencies);
    let missing = execute(["sessions", "show", "7"], &dependencies);
    let empty = execute(["sessions", "list"], &dependencies);

    assert_eq!(list.status, ExitStatus::Success);
    assert_eq!(
        list.stdout,
        "ID\tPROJECT\tTITLE\tAGENT\tTURNS\n7\tproject\tconversation\tprimary\t1\n"
    );
    assert_eq!(show.status, ExitStatus::Success);
    assert_eq!(
        show.stdout,
        "Session 7: project=project title=conversation agent=primary turns=1 messages=2\n"
    );
    assert_eq!(remove.status, ExitStatus::Success);
    assert_eq!(remove.stdout, "Removed session 7.\n");
    assert_eq!(remove_again.status, ExitStatus::Success);
    assert_eq!(remove_again.stdout, "Removed session 7.\n");
    assert_eq!(missing.status, ExitStatus::Failure);
    assert_eq!(
        missing.stderr,
        "error: store: saved session is unavailable\n"
    );
    assert_eq!(empty.status, ExitStatus::Success);
    assert_eq!(empty.stdout, "No saved sessions.\n");
}

#[test]
fn config_doctor_rejects_semantically_invalid_configuration() {
    let temporary = TemporaryDirectory::new("semantic-config");
    let config_home = temporary.path().join("config");
    let dependencies = CliDependencies::for_test(
        temporary.path().join("project"),
        Some(temporary.path().join("home")),
        BTreeMap::from([(
            "AGENS_CONFIG_HOME".to_owned(),
            config_home.display().to_string(),
        )]),
        BTreeMap::from([(
            config_home.join("config.toml"),
            "[provider]\nmodel = 123\nunknown = \"SENTINEL_CONFIG_42\"\n".to_owned(),
        )]),
    );

    let result = execute(["config", "doctor"], &dependencies);

    assert_eq!(result.status, ExitStatus::Configuration);
    assert_eq!(result.stdout, "Agens config doctor\nStatus:  invalid\n");
    assert!(!result.stderr.contains("SENTINEL_CONFIG_42"));
}

#[test]
fn config_doctor_discovers_repository_root_from_nested_directory() {
    let temporary = TemporaryDirectory::new("nested-project-config");
    let config_home = temporary.path().join("config");
    let project_root = temporary.path().join("project");
    let nested_directory = project_root.join("src/nested");
    std::fs::create_dir_all(project_root.join(".git")).expect("repository marker should exist");
    std::fs::create_dir_all(&nested_directory).expect("nested directory should exist");

    let dependencies = CliDependencies::for_test(
        nested_directory,
        Some(temporary.path().join("home")),
        BTreeMap::from([(
            "AGENS_CONFIG_HOME".to_owned(),
            config_home.display().to_string(),
        )]),
        BTreeMap::from([
            (
                config_home.join("config.toml"),
                "[provider]\nmodel = \"global-model\"\n".to_owned(),
            ),
            (
                project_root.join(".agens/config.toml"),
                "[provider]\nmodel = \"project-model\"\n".to_owned(),
            ),
        ]),
    );

    let result = execute(["config", "doctor"], &dependencies);

    assert_eq!(result.status, ExitStatus::Success);
    assert!(result.stdout.contains("Model:   project-model\n"));
    assert!(result.stdout.contains(&format!(
        "Project: {} (loaded)",
        project_root.join(".agens/config.toml").display()
    )));
}

#[cfg(unix)]
#[test]
fn config_doctor_resolves_a_symlinked_working_directory_before_discovery() {
    use std::os::unix::fs::symlink;

    let temporary = TemporaryDirectory::new("symlinked-project-config");
    let config_home = temporary.path().join("config");
    let project_root = temporary.path().join("project");
    let nested_directory = project_root.join("src/nested");
    let symlinked_directory = temporary.path().join("working-directory");
    std::fs::create_dir_all(project_root.join(".git")).expect("repository marker should exist");
    std::fs::create_dir_all(&nested_directory).expect("nested directory should exist");
    symlink(&nested_directory, &symlinked_directory)
        .expect("working directory symlink should exist");

    let dependencies = CliDependencies::for_test(
        symlinked_directory,
        Some(temporary.path().join("home")),
        BTreeMap::from([(
            "AGENS_CONFIG_HOME".to_owned(),
            config_home.display().to_string(),
        )]),
        BTreeMap::from([(
            project_root.join(".agens/config.toml"),
            "[provider]\nmodel = \"project-model\"\n".to_owned(),
        )]),
    );

    let result = execute(["config", "doctor"], &dependencies);

    assert_eq!(result.status, ExitStatus::Success);
    assert!(result.stdout.contains("Model:   project-model\n"));
}

#[test]
fn every_leaf_command_accepts_help_without_bootstrapping_configuration() {
    let dependencies = CliDependencies::for_test(
        PathBuf::from("/project"),
        Some(PathBuf::from("/home/user")),
        BTreeMap::new(),
        BTreeMap::new(),
    );

    for arguments in [
        ["config", "doctor", "--help"].as_slice(),
        ["auth", "status", "--help"].as_slice(),
        ["auth", "login", "--help"].as_slice(),
        ["auth", "logout", "--help"].as_slice(),
        ["models", "--help"].as_slice(),
        ["sessions", "list", "--help"].as_slice(),
        ["sessions", "show", "--help"].as_slice(),
        ["sessions", "rm", "--help"].as_slice(),
    ] {
        let result = execute(arguments, &dependencies);

        assert_eq!(result.status, ExitStatus::Success, "{arguments:?}");
        assert!(result.stdout.starts_with("Usage: agens "), "{arguments:?}");
    }
}

#[test]
fn tui_resume_shapes_reach_the_injected_tui_launcher() {
    let dependencies = CliDependencies::for_test(
        PathBuf::from("/project"),
        Some(PathBuf::from("/home/user")),
        BTreeMap::new(),
        BTreeMap::new(),
    );

    let dependencies = dependencies.with_tui_launcher(|_, resume| {
        Ok(match resume {
            Some(identifier) => format!("resume:{identifier}"),
            None => "new-session".to_owned(),
        })
    });

    for (arguments, expected) in [
        (["--resume"].as_slice(), "new-session\n"),
        (["123"].as_slice(), "resume:123\n"),
    ] {
        let result = execute(arguments, &dependencies);

        assert_eq!(result.status, ExitStatus::Success, "{arguments:?}");
        assert_eq!(result.stdout, expected, "{arguments:?}");
    }
}

#[test]
fn tui_model_selector_applies_only_bundled_models_and_preserves_state_on_refusal() {
    let mut selector = TuiModelSelector::new("gpt-4.1");

    assert_eq!(
        selector
            .model_values()
            .expect("registry should be available"),
        vec![
            "gpt-4.1",
            "gpt-4.1-mini",
            "gpt-4.1-nano",
            "gpt-4o",
            "gpt-4o-mini",
            "gpt-5.5",
            "o3",
            "o4-mini",
        ]
    );
    assert_eq!(selector.model(), "gpt-4.1");

    selector
        .apply_model("o3")
        .expect("bundled model should apply");
    assert_eq!(selector.model(), "o3");

    assert_eq!(
        selector.apply_model("not-a-model"),
        Err("model is unavailable for OpenAI API".to_owned())
    );
    assert_eq!(selector.model(), "o3");
}

#[test]
fn tui_model_selector_exposes_only_models_compatible_with_the_effective_source() {
    let mut api = TuiModelSelector::for_source("gpt-5.5", TuiModelSource::OpenAiApi);
    let mut subscription =
        TuiModelSelector::for_source("gpt-5.5", TuiModelSource::ChatGptSubscription);

    assert!(
        api.model_values()
            .expect("API model registry should be available")
            .contains(&"gpt-4o".to_owned())
    );
    assert_eq!(
        subscription
            .model_values()
            .expect("subscription model registry should be available"),
        ["gpt-5.3-codex-spark", "gpt-5.4", "gpt-5.4-mini", "gpt-5.5"]
    );
    assert_eq!(api.source_label(), "OpenAI API");
    assert_eq!(subscription.source_label(), "ChatGPT subscription");
    assert_eq!(
        subscription.apply_model("gpt-4o"),
        Err("model is unavailable for ChatGPT subscription".to_owned())
    );
    api.apply_model("gpt-4o")
        .expect("API model should remain selectable");
}

#[test]
fn tui_model_selector_applies_typed_effort_and_refuses_unsupported_values_without_mutation() {
    let mut selector = TuiModelSelector::for_source("gpt-5.5", TuiModelSource::OpenAiApi);

    assert_eq!(
        selector.reasoning_effort_values(),
        ["default", "none", "low", "medium", "high", "xhigh"]
    );
    assert_eq!(selector.reasoning_effort(), None);

    selector
        .apply_reasoning_effort("xhigh")
        .expect("supported effort should apply");
    assert_eq!(
        selector.request_config().reasoning_effort(),
        Some(ReasoningEffort::XHigh)
    );
    assert_eq!(selector.reasoning_effort(), Some("xhigh"));

    assert_eq!(
        selector.apply_reasoning_effort("minimal"),
        Err("reasoning effort is unsupported".to_owned())
    );
    assert_eq!(selector.reasoning_effort(), Some("xhigh"));

    let mut subscription =
        TuiModelSelector::for_source("gpt-5.5", TuiModelSource::ChatGptSubscription);
    assert_eq!(
        subscription.reasoning_effort_values(),
        [
            "default", "none", "minimal", "low", "medium", "high", "xhigh"
        ]
    );
    subscription
        .apply_reasoning_effort("minimal")
        .expect("subscription minimal effort should be selectable");
    assert_eq!(subscription.reasoning_effort(), Some("minimal"));
    assert_eq!(
        subscription.request_config().reasoning_effort(),
        Some(ReasoningEffort::Low)
    );

    let non_reasoning = TuiModelSelector::new("gpt-4.1");
    assert_eq!(non_reasoning.reasoning_effort_values(), ["default"]);
}

#[cfg(unix)]
#[test]
fn non_utf8_os_arguments_are_rejected_without_echoing_input() {
    use std::os::unix::ffi::OsStringExt;

    let dependencies = CliDependencies::for_test(
        PathBuf::from("/project"),
        Some(PathBuf::from("/home/user")),
        BTreeMap::new(),
        BTreeMap::new(),
    );
    let result = execute_os(
        [std::ffi::OsString::from_vec(vec![
            b'S', b'E', b'C', b'R', b'E', b'T', 0xff,
        ])],
        &dependencies,
    );

    assert_eq!(result.status, ExitStatus::Usage);
    assert_eq!(result.stdout, "");
    assert_eq!(
        result.stderr,
        "error: usage: command arguments must be valid UTF-8\n"
    );
    assert!(!result.stderr.contains("SECRET"));
}

#[test]
fn headless_chat_bootstraps_config_runs_local_turn_and_supports_session_resume() {
    let temporary = TemporaryDirectory::new("headless-e2e");
    let config_home = temporary.path().join("config");
    let data_directory = temporary.path().join("data");
    let dependencies = CliDependencies::for_test(
        temporary.path().join("project"),
        Some(temporary.path().join("home")),
        BTreeMap::from([(
            "AGENS_CONFIG_HOME".to_owned(),
            config_home.display().to_string(),
        )]),
        BTreeMap::from([(
            config_home.join("config.toml"),
            format!("[options]\ndata_dir = \"{}\"\n", data_directory.display()),
        )]),
    )
    .with_headless_chat(|_, bootstrap, _| {
        let mut provider = LocalProvider {
            iterations: vec![
                Ok(vec![
                    MessagePart::ToolCall {
                        id: "ask".into(),
                        name: "read".into(),
                        input: "notes.md".into(),
                    },
                    MessagePart::ToolCall {
                        id: "deny".into(),
                        name: "write".into(),
                        input: "notes.md".into(),
                    },
                    MessagePart::ToolCall {
                        id: "allow".into(),
                        name: "search".into(),
                        input: "runtime".into(),
                    },
                ]),
                Ok(vec![MessagePart::Text("completed locally".into())]),
            ],
        };
        let mut gate = LocalPermissionGate {
            decisions: vec![
                PermissionDecision::Ask,
                PermissionDecision::Deny,
                PermissionDecision::Allow,
            ],
        };
        let mut resolver = LocalPermissionResolver {
            decisions: vec![PermissionDecision::Allow],
        };
        let mut dispatcher = LocalToolDispatcher {
            outputs: vec![
                Ok(HeadlessToolOutput::success("asked result")),
                Ok(HeadlessToolOutput::success("allowed result")),
            ],
        };
        let mut store = SessionStore::open(bootstrap.data_directory())
            .expect("local session store should open");

        let snapshot = block_on_ready(run_headless_turn(
            &mut provider,
            &mut gate,
            &mut resolver,
            &mut dispatcher,
            &mut store,
            &HeadlessTurnCancellation::new(),
        ))
        .expect("local headless turn should complete");
        let metadata = SessionMetadata {
            id: 1,
            project: "project".into(),
            title: "conversation".into(),
            active_agent: "primary".into(),
            created_at: 10,
            updated_at: 20,
            completed_turn_count: 0,
            resumable: false,
        };
        let turn = CompletedSessionTurn::new(
            [
                Message {
                    role: Role::User,
                    parts: vec![MessagePart::Text("hello".into())],
                },
                Message {
                    role: Role::Assistant,
                    parts: vec![MessagePart::Text("completed locally".into())],
                },
            ]
            .into_iter()
            .map(SessionMessage::try_from)
            .collect::<Result<_, _>>()
            .expect("session messages should be valid"),
        )
        .expect("completed session turn should be valid");
        store
            .persist_completed_session_turn(&metadata, &turn)
            .expect("normalized session should persist");

        Ok(format!("{} events", snapshot.events().len()))
    });

    let chat = execute(["chat", "hello"], &dependencies);
    let sessions = execute(["sessions", "list"], &dependencies);
    let resumed = execute(["sessions", "show", "1"], &dependencies);

    assert_eq!(chat.status, ExitStatus::Success);
    assert_eq!(chat.stdout, "16 events\n");
    assert_eq!(sessions.status, ExitStatus::Success);
    assert_eq!(
        sessions.stdout,
        "ID\tPROJECT\tTITLE\tAGENT\tTURNS\n1\tproject\tconversation\tprimary\t1\n"
    );
    assert_eq!(resumed.status, ExitStatus::Success);
    assert_eq!(
        resumed.stdout,
        "Session 1: project=project title=conversation agent=primary turns=1 messages=2\n"
    );
    assert!(!format!("{}{}{}", chat.stdout, sessions.stdout, resumed.stdout).contains("secret"));
}

#[test]
fn injected_shutdown_cancels_headless_chat_with_deterministic_output_and_no_session() {
    let temporary = TemporaryDirectory::new("cancelled-headless");
    let data_directory = temporary.path().join("data");
    let dependencies = CliDependencies::for_test(
        temporary.path().join("project"),
        Some(temporary.path().join("home")),
        BTreeMap::new(),
        BTreeMap::new(),
    )
    .with_headless_chat(|_, _, cancellation| {
        assert!(cancellation.is_cancelled());
        Ok("must not be emitted".to_owned())
    });
    let cancellation = HeadlessTurnCancellation::new();
    cancellation.cancel();

    let result = execute_with_cancellation(["chat", "cancelled"], &dependencies, &cancellation);

    assert_eq!(result.status, ExitStatus::Failure);
    assert_eq!(result.stdout, "");
    assert_eq!(
        result.stderr,
        "error: cancelled: headless turn was cancelled\n"
    );
    assert!(!data_directory.join("rust-sessions.db").exists());
}

#[test]
fn production_binary_runs_configured_openai_responses_transport_and_persists_the_turn() {
    let temporary = TemporaryDirectory::new("production-headless");
    let config_home = temporary.path().join("config");
    let data_directory = temporary.path().join("data");
    std::fs::create_dir_all(&config_home).expect("config directory should exist");
    let server = ScriptedNativeOpenAiMockServer::start(vec![ScriptedOpenAiResponse {
        required_body_fragments: vec!["\"parallel_tool_calls\":false".to_owned()],
        response: text_response("Hello from OpenAI"),
    }]);
    std::fs::write(
        config_home.join("config.toml"),
        format!(
            "[provider]\ntype = \"openai-api\"\nmodel = \"test-model\"\nbase_url = \"{}\"\n\n[agent]\nparallel_tool_calls = false\n\n[options]\ndata_dir = \"{}\"\n",
            server.base_url(),
            data_directory.display(),
        ),
    )
    .expect("config should be written");

    let chat = Command::new(env!("CARGO_BIN_EXE_agens"))
        .args(["chat", "hello from production"])
        .env("AGENS_CONFIG_HOME", &config_home)
        .env("OPENAI_API_KEY", "SENTINEL_OPENAI_API_KEY")
        .output()
        .expect("production binary should execute");
    let sessions = Command::new(env!("CARGO_BIN_EXE_agens"))
        .args(["sessions", "list"])
        .env("AGENS_CONFIG_HOME", &config_home)
        .output()
        .expect("production binary should list sessions");

    assert!(chat.status.success());
    assert_eq!(String::from_utf8_lossy(&chat.stdout), "Hello from OpenAI\n");
    assert_eq!(String::from_utf8_lossy(&chat.stderr), "");
    assert!(sessions.status.success());
    assert!(String::from_utf8_lossy(&sessions.stdout).ends_with("\tprimary\t1\n"));
    assert!(
        !format!(
            "{}{}",
            String::from_utf8_lossy(&chat.stdout),
            String::from_utf8_lossy(&chat.stderr)
        )
        .contains("SENTINEL_OPENAI_API_KEY")
    );

    server.join();
}

#[test]
fn production_task_consolidates_durable_sessions_catalog_skills_and_isolation() {
    let temporary = TemporaryDirectory::new("production-task-subagent");
    let project_root = temporary.path().join("project");
    let config_home = temporary.path().join("config");
    let data_directory = temporary.path().join("data");
    std::fs::create_dir_all(project_root.join(".git")).expect("project marker should exist");
    std::fs::create_dir_all(config_home.join("agents")).expect("agents directory should exist");
    std::fs::create_dir_all(config_home.join("skills/review-checklist"))
        .expect("skill directory should exist");
    std::fs::write(project_root.join("notes.md"), "child read content")
        .expect("child read fixture should exist");
    std::fs::write(
        config_home.join("agents/reviewer.md"),
        "---\nname: reviewer\ndescription: Review implementation\nmode: subagent\nmodel: gpt-4o\nskills:\n  - review-checklist\npermissions: []\n---\nYou are the isolated reviewer.\n",
    )
    .expect("subagent definition should be written");
    std::fs::write(
        config_home.join("skills/review-checklist/SKILL.md"),
        "---\nname: review-checklist\ndescription: Review checklist\n---\nUse the review checklist.\n",
    )
    .expect("skill manifest should be written");

    let server = ScriptedNativeOpenAiMockServer::start(vec![
        ScriptedOpenAiResponse {
            required_body_fragments: vec!["task".into(), "parent request".into()],
            response: native_tool_call_response(
                "task-call",
                "task",
                r#"{"agent":"reviewer","skills":["review-checklist"],"description":"child request"}"#,
            ),
        },
        ScriptedOpenAiResponse {
            required_body_fragments: vec![
                "child request".into(),
                "You are the isolated reviewer.".into(),
                "Use the review checklist.".into(),
                "gpt-4o".into(),
                "read".into(),
                "!parent request".into(),
                "!task".into(),
                "!write".into(),
                "!bash".into(),
                "!webfetch".into(),
                "!mcp".into(),
            ],
            response: native_tool_call_response(
                "child-read",
                "native::read",
                r#"{"path":"notes.md"}"#,
            ),
        },
        ScriptedOpenAiResponse {
            required_body_fragments: vec![
                "\"call_id\":\"child-read\"".into(),
                "child read content".into(),
            ],
            response: text_response("child answer"),
        },
        ScriptedOpenAiResponse {
            required_body_fragments: vec!["child answer".into()],
            response: text_response("parent answer"),
        },
    ]);
    std::fs::write(
        config_home.join("config.toml"),
        format!(
            "[provider]\ntype = \"openai-api\"\nmodel = \"gpt-4.1\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n[permissions]\nallow = [\"task(reviewer)\"]\n",
            server.base_url(),
            data_directory.display(),
        ),
    )
    .expect("configuration should be written");

    let result = Command::new(env!("CARGO_BIN_EXE_agens"))
        .current_dir(&project_root)
        .args(["chat", "parent request"])
        .env("AGENS_CONFIG_HOME", &config_home)
        .env("OPENAI_API_KEY", "SENTINEL_OPENAI_API_KEY")
        .output()
        .expect("production binary should run");
    server.join();

    assert!(result.status.success());
    assert_eq!(String::from_utf8(result.stdout).unwrap(), "parent answer\n");
    assert_eq!(String::from_utf8(result.stderr).unwrap(), "");

    let listed = Command::new(env!("CARGO_BIN_EXE_agens"))
        .args(["sessions", "list"])
        .current_dir(&project_root)
        .env("AGENS_CONFIG_HOME", &config_home)
        .output()
        .expect("sessions should list the parent turn");
    let reopened = Command::new(env!("CARGO_BIN_EXE_agens"))
        .args(["sessions", "show", "1"])
        .current_dir(&project_root)
        .env("AGENS_CONFIG_HOME", &config_home)
        .output()
        .expect("sessions should reopen the parent turn");
    let removed = Command::new(env!("CARGO_BIN_EXE_agens"))
        .args(["sessions", "rm", "1"])
        .current_dir(&project_root)
        .env("AGENS_CONFIG_HOME", &config_home)
        .output()
        .expect("sessions should remove the parent turn");
    let empty = Command::new(env!("CARGO_BIN_EXE_agens"))
        .args(["sessions", "list"])
        .current_dir(&project_root)
        .env("AGENS_CONFIG_HOME", &config_home)
        .output()
        .expect("sessions should confirm the parent turn was removed");

    assert!(listed.status.success());
    assert_eq!(
        String::from_utf8(listed.stdout).unwrap(),
        format!(
            "ID\tPROJECT\tTITLE\tAGENT\tTURNS\n1\t{}\tparent request\tprimary\t1\n",
            project_root.display()
        )
    );
    assert!(reopened.status.success());
    assert_eq!(
        String::from_utf8(reopened.stdout).unwrap(),
        format!(
            "Session 1: project={} title=parent request agent=primary turns=1 messages=4\n",
            project_root.display()
        )
    );
    assert!(removed.status.success());
    assert_eq!(
        String::from_utf8(removed.stdout).unwrap(),
        "Removed session 1.\n"
    );
    assert!(empty.status.success());
    assert_eq!(
        String::from_utf8(empty.stdout).unwrap(),
        "No saved sessions.\n"
    );
    assert_sqlite_has_no_sentinels(
        &data_directory.join("rust-sessions.db"),
        &[
            "SENTINEL_OPENAI_API_KEY",
            "SENTINEL_PROVIDER_ERROR",
            "SENTINEL_PANIC",
            "SENTINEL_HEADER",
        ],
    );
}

#[cfg(unix)]
#[test]
fn production_task_cancellation_prevents_parent_continuation_and_persistence() {
    let temporary = TemporaryDirectory::new("production-task-cancellation");
    let project_root = temporary.path().join("project");
    let config_home = temporary.path().join("config");
    let data_directory = temporary.path().join("data");
    std::fs::create_dir_all(project_root.join(".git")).expect("project marker should exist");
    std::fs::create_dir_all(config_home.join("agents")).expect("agents directory should exist");
    std::fs::write(
        config_home.join("agents/reviewer.md"),
        "---\nname: reviewer\ndescription: Review implementation\nmode: subagent\nmodel: gpt-4o\npermissions: []\n---\nYou are the isolated reviewer.\n",
    )
    .expect("subagent definition should be written");

    let mut server = TaskStalledOpenAiMockServer::start(Duration::from_secs(1));
    std::fs::write(
        config_home.join("config.toml"),
        format!(
            "[provider]\ntype = \"openai-api\"\nmodel = \"gpt-4.1\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n[permissions]\nallow = [\"task(reviewer)\"]\n",
            server.base_url(),
            data_directory.display(),
        ),
    )
    .expect("configuration should be written");

    let child = Command::new(env!("CARGO_BIN_EXE_agens"))
        .current_dir(&project_root)
        .args(["chat", "parent task cancellation"])
        .env("AGENS_CONFIG_HOME", &config_home)
        .env("OPENAI_API_KEY", "SENTINEL_OPENAI_API_KEY")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("production binary should start");
    server.wait_for_child_request();
    assert!(
        Command::new("kill")
            .args(["-INT", &child.id().to_string()])
            .status()
            .expect("SIGINT delivery should execute")
            .success()
    );
    let output = wait_for_child_output(child, Duration::from_secs(2));

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "error: cancelled: headless turn was cancelled\n"
    );
    assert_no_saved_sessions(&project_root, &config_home);
    assert_sqlite_has_no_sentinels(
        &data_directory.join("rust-sessions.db"),
        &[
            "SENTINEL_OPENAI_API_KEY",
            "SENTINEL_PROVIDER_ERROR",
            "SENTINEL_PANIC",
            "SENTINEL_HEADER",
        ],
    );

    server.join();
}

#[test]
fn production_task_provider_failure_is_sanitized_and_aborts_the_parent_turn() {
    let temporary = TemporaryDirectory::new("production-task-provider-failure");
    let project_root = temporary.path().join("project");
    let config_home = temporary.path().join("config");
    let data_directory = temporary.path().join("data");
    std::fs::create_dir_all(project_root.join(".git")).expect("project marker should exist");
    std::fs::create_dir_all(config_home.join("agents")).expect("agents directory should exist");
    std::fs::write(
        config_home.join("agents/reviewer.md"),
        "---\nname: reviewer\ndescription: Review implementation\nmode: subagent\nmodel: gpt-4o\npermissions: []\n---\nYou are the isolated reviewer.\n",
    )
    .expect("subagent definition should be written");
    let server = BoundedScriptedOpenAiMockServer::start(vec![
        ScriptedOpenAiResponse {
            required_body_fragments: vec!["parent provider failure".into()],
            response: native_tool_call_response(
                "task-failure",
                "task",
                r#"{"agent":"reviewer","description":"child provider failure"}"#,
            ),
        },
        ScriptedOpenAiResponse {
            required_body_fragments: vec!["child provider failure".into()],
            response: "HTTP/1.1 500 Internal Server Error\r\nX-Remote-Secret: SENTINEL_HEADER\r\nContent-Length: 23\r\nConnection: close\r\n\r\nSENTINEL_PROVIDER_ERROR".into(),
        },
    ]);
    std::fs::write(
        config_home.join("config.toml"),
        format!(
            "[provider]\ntype = \"openai-api\"\nmodel = \"gpt-4.1\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n[permissions]\nallow = [\"task(reviewer)\"]\n",
            server.base_url(),
            data_directory.display(),
        ),
    )
    .expect("configuration should be written");

    let output = Command::new(env!("CARGO_BIN_EXE_agens"))
        .current_dir(&project_root)
        .args(["chat", "parent provider failure"])
        .env("AGENS_CONFIG_HOME", &config_home)
        .env("OPENAI_API_KEY", "SENTINEL_OPENAI_API_KEY")
        .output()
        .expect("production binary should run");
    server.join();

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "error: task: provider failure\n"
    );
    assert_no_saved_sessions(&project_root, &config_home);
    assert_output_and_store_exclude_sentinels(
        &output,
        &data_directory.join("rust-sessions.db"),
        &[
            "SENTINEL_OPENAI_API_KEY",
            "SENTINEL_PROVIDER_ERROR",
            "SENTINEL_HEADER",
        ],
    );
}

#[test]
fn production_task_deadline_is_exact_and_aborts_the_parent_turn() {
    let temporary = TemporaryDirectory::new("production-task-deadline");
    let project_root = temporary.path().join("project");
    let config_home = temporary.path().join("config");
    let data_directory = temporary.path().join("data");
    std::fs::create_dir_all(project_root.join(".git")).expect("project marker should exist");
    std::fs::create_dir_all(config_home.join("agents")).expect("agents directory should exist");
    std::fs::write(
        config_home.join("agents/reviewer.md"),
        "---\nname: reviewer\ndescription: Review implementation\nmode: subagent\nmodel: gpt-4o\npermissions: []\n---\nYou are the isolated reviewer.\n",
    )
    .expect("subagent definition should be written");
    let server = TaskStalledOpenAiMockServer::start(Duration::from_secs(40));
    std::fs::write(
        config_home.join("config.toml"),
        format!(
            "[provider]\ntype = \"openai-api\"\nmodel = \"gpt-4.1\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n[permissions]\nallow = [\"task(reviewer)\"]\n",
            server.base_url(),
            data_directory.display(),
        ),
    )
    .expect("configuration should be written");

    let output = Command::new(env!("CARGO_BIN_EXE_agens"))
        .current_dir(&project_root)
        .args(["chat", "parent task deadline"])
        .env("AGENS_CONFIG_HOME", &config_home)
        .env("OPENAI_API_KEY", "SENTINEL_OPENAI_API_KEY")
        .output()
        .expect("production binary should run");
    server.join();

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "error: task: timed out\n"
    );
    assert_no_saved_sessions(&project_root, &config_home);
    assert_output_and_store_exclude_sentinels(
        &output,
        &data_directory.join("rust-sessions.db"),
        &["SENTINEL_OPENAI_API_KEY"],
    );
}

#[test]
fn production_binary_runs_chatgpt_subscription_without_an_api_key_and_persists_the_turn() {
    let temporary = TemporaryDirectory::new("production-chatgpt");
    let config_home = temporary.path().join("config");
    let data_directory = temporary.path().join("data");
    std::fs::create_dir_all(&config_home).expect("config directory should exist");
    let server = ScriptedNativeOpenAiMockServer::start(vec![ScriptedOpenAiResponse {
        required_body_fragments: vec![
            "\"store\":false".to_owned(),
            "\"model\":\"test-model\"".to_owned(),
            "\"parallel_tool_calls\":true".to_owned(),
            "@all-tools-non-strict".to_owned(),
        ],
        response: text_response("Hello from ChatGPT"),
    }]);
    std::fs::write(
        config_home.join("config.toml"),
        format!(
            "[provider]\ntype = \"openai-chatgpt\"\nmodel = \"test-model\"\nbase_url = \"{}\"\n\n[agent]\nparallel_tool_calls = true\n\n[options]\ndata_dir = \"{}\"\n",
            server.base_url(),
            data_directory.display(),
        ),
    )
    .expect("config should be written");
    std::fs::write(
        config_home.join("auth.json"),
        r#"{"openai-chatgpt":{"access_token":"header.eyJleHAiOjE4OTM0NTYwMDB9.signature","refresh_token":"SENTINEL_CHATGPT_REFRESH","account_id":"account_123","expires_at":"2030-01-01T00:00:00Z"}}"#,
    )
    .expect("ChatGPT credentials should be written");

    let chat = Command::new(env!("CARGO_BIN_EXE_agens"))
        .args(["chat", "hello from subscription"])
        .env("AGENS_CONFIG_HOME", &config_home)
        .env_remove("OPENAI_API_KEY")
        .output()
        .expect("production binary should execute");
    let sessions = Command::new(env!("CARGO_BIN_EXE_agens"))
        .args(["sessions", "list"])
        .env("AGENS_CONFIG_HOME", &config_home)
        .output()
        .expect("production binary should list sessions");

    assert!(chat.status.success());
    assert_eq!(
        String::from_utf8_lossy(&chat.stdout),
        "Hello from ChatGPT\n"
    );
    assert_eq!(String::from_utf8_lossy(&chat.stderr), "");
    assert!(String::from_utf8_lossy(&sessions.stdout).ends_with("\tprimary\t1\n"));
    let diagnostics = format!(
        "{}{}",
        String::from_utf8_lossy(&chat.stdout),
        String::from_utf8_lossy(&chat.stderr)
    );
    assert!(!diagnostics.contains("SENTINEL_CHATGPT_REFRESH"));
    assert_sqlite_has_no_sentinels(
        &data_directory.join("rust-sessions.db"),
        &["SENTINEL_CHATGPT_REFRESH"],
    );

    server.join();
}

#[test]
fn production_binary_uses_auth_json_api_key_when_openai_is_inferred_without_environment_key() {
    let temporary = TemporaryDirectory::new("production-auth-json-api-key");
    let config_home = temporary.path().join("config");
    let data_directory = temporary.path().join("data");
    std::fs::create_dir_all(&config_home).expect("config directory should exist");
    let server = OpenAiMockServer::start_with_api_key("SENTINEL_AUTH_JSON_API_KEY");
    std::fs::write(
        config_home.join("config.toml"),
        format!(
            "[provider]\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n",
            server.base_url(),
            data_directory.display(),
        ),
    )
    .expect("config should be written");
    std::fs::write(
        config_home.join("auth.json"),
        r#"{"openai-api":{"api_key":"SENTINEL_AUTH_JSON_API_KEY"}}"#,
    )
    .expect("legacy API credentials should be written");

    let chat = Command::new(env!("CARGO_BIN_EXE_agens"))
        .args(["chat", "hello from auth json"])
        .env("AGENS_CONFIG_HOME", &config_home)
        .env_remove("OPENAI_API_KEY")
        .output()
        .expect("production binary should execute");

    assert!(chat.status.success());
    assert_eq!(String::from_utf8_lossy(&chat.stdout), "Hello from OpenAI\n");
    assert!(!format!("{chat:?}").contains("SENTINEL_AUTH_JSON_API_KEY"));

    server.join();
}

#[test]
fn production_binary_rejects_missing_malformed_and_incomplete_chatgpt_credentials() {
    for (name, credentials) in [
        ("missing", None),
        ("malformed", Some("SENTINEL_MALFORMED_CREDENTIALS")),
        (
            "incomplete",
            Some(r#"{"openai-chatgpt":{"access_token":"SENTINEL_INCOMPLETE_ACCESS"}}"#),
        ),
    ] {
        let temporary = TemporaryDirectory::new(&format!("production-chatgpt-{name}"));
        let config_home = temporary.path().join("config");
        std::fs::create_dir_all(&config_home).expect("config directory should exist");
        std::fs::write(
            config_home.join("config.toml"),
            "[provider]\ntype = \"openai-chatgpt\"\nmodel = \"test-model\"\n",
        )
        .expect("config should be written");
        if let Some(credentials) = credentials {
            std::fs::write(config_home.join("auth.json"), credentials)
                .expect("credential fixture should be written");
        }

        let output = Command::new(env!("CARGO_BIN_EXE_agens"))
            .args(["chat", "reject invalid credentials"])
            .env("AGENS_CONFIG_HOME", &config_home)
            .env_remove("OPENAI_API_KEY")
            .output()
            .expect("production binary should execute");

        assert_eq!(output.status.code(), Some(4), "{name}");
        assert_eq!(String::from_utf8_lossy(&output.stdout), "", "{name}");
        assert_eq!(
            String::from_utf8_lossy(&output.stderr),
            "error: auth: ChatGPT credentials are unavailable or invalid\n",
            "{name}"
        );
        assert!(!format!("{output:?}").contains("SENTINEL"), "{name}");
    }
}

#[test]
fn production_binary_maps_chatgpt_provider_and_auth_failures_without_leaking_credentials() {
    for (name, response, expected_exit, expected_stderr) in [
        (
            "forbidden",
            "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned(),
            Some(4),
            "error: auth: ChatGPT credentials are unavailable or invalid\n",
        ),
        (
            "rejected",
            "HTTP/1.1 422 Unprocessable Content\r\nContent-Length: 27\r\nConnection: close\r\n\r\nSENTINEL_CHATGPT_ERROR_BODY".to_owned(),
            Some(1),
            "error: provider: ChatGPT request was rejected\n",
        ),
        (
            "rate limit",
            "HTTP/1.1 429 Too Many Requests\r\nContent-Length: 27\r\nConnection: close\r\n\r\nSENTINEL_CHATGPT_ERROR_BODY".to_owned(),
            Some(1),
            "error: provider: ChatGPT request was rate limited\n",
        ),
        (
            "server failure",
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 27\r\nConnection: close\r\n\r\nSENTINEL_CHATGPT_ERROR_BODY".to_owned(),
            Some(1),
            "error: provider: ChatGPT service failed\n",
        ),
        (
            "protocol failure",
            sse_response(&[r#"{"type":"response.incomplete","response":{"error":{"message":"SENTINEL_CHATGPT_ERROR_BODY"}}}"#]),
            Some(1),
            "error: provider: ChatGPT response protocol failed\n",
        ),
    ] {
        let temporary = TemporaryDirectory::new(&format!("production-chatgpt-{name}"));
        let config_home = temporary.path().join("config");
        std::fs::create_dir_all(&config_home).expect("config directory should exist");
        let server = ScriptedNativeOpenAiMockServer::start(vec![ScriptedOpenAiResponse {
            required_body_fragments: vec!["\"store\":false".to_owned()],
            response,
        }]);
        std::fs::write(
            config_home.join("config.toml"),
            format!(
                "[provider]\ntype = \"openai-chatgpt\"\nmodel = \"test-model\"\nbase_url = \"{}\"\n",
                server.base_url(),
            ),
        )
        .expect("config should be written");
        write_chatgpt_credentials(&config_home, "SENTINEL_CHATGPT_ACCESS");

        let output = Command::new(env!("CARGO_BIN_EXE_agens"))
            .args(["chat", "handle remote failure"])
            .env("AGENS_CONFIG_HOME", &config_home)
            .env_remove("OPENAI_API_KEY")
            .output()
            .expect("production binary should execute");

        assert_eq!(output.status.code(), expected_exit, "{name}");
        assert_eq!(String::from_utf8_lossy(&output.stdout), "", "{name}");
        assert_eq!(
            String::from_utf8_lossy(&output.stderr),
            expected_stderr,
            "{name}"
        );
        for secret in [
            "SENTINEL_CHATGPT_ACCESS",
            "SENTINEL_CHATGPT_REFRESH",
            "SENTINEL_CHATGPT_REMOTE",
            "SENTINEL_CHATGPT_ERROR_BODY",
        ] {
            assert!(!format!("{output:?}").contains(secret), "{name}: {secret}");
        }

        server.join();
    }
}

#[test]
fn production_binary_replays_chatgpt_native_and_mcp_tool_results_once() {
    for (name, tool, arguments, setup, expected_output) in [
        (
            "native",
            "native::read",
            r#"{"path":"notes.md"}"#,
            "[permissions]\nallow = [\"read(notes.md)\"]\n",
            "native subscription completed",
        ),
        (
            "MCP",
            "files::first",
            "{}",
            "[mcp.files]\ntransport = \"stdio\"\ncommand = \"{fake_mcp}\"\nargs = [\"success\"]\ntimeout_ms = 1000\n",
            "MCP subscription completed",
        ),
    ] {
        let temporary = TemporaryDirectory::new(&format!("production-chatgpt-tool-{name}"));
        let project_root = temporary.path().join("project");
        let config_home = temporary.path().join("config");
        let data_directory = temporary.path().join("data");
        std::fs::create_dir_all(project_root.join(".git")).expect("project marker should exist");
        std::fs::create_dir_all(&config_home).expect("config directory should exist");
        std::fs::write(project_root.join("notes.md"), "subscription native content")
            .expect("native fixture should exist");
        let server = ScriptedNativeOpenAiMockServer::start(vec![
            ScriptedOpenAiResponse {
                required_body_fragments: vec![tool.to_owned(), "\"store\":false".to_owned()],
                response: native_tool_call_response("call_chatgpt_tool", tool, arguments),
            },
            ScriptedOpenAiResponse {
                required_body_fragments: vec![
                    "\"call_id\":\"call_chatgpt_tool\"".to_owned(),
                    "\"store\":false".to_owned(),
                    "!previous_response_id".to_owned(),
                ],
                response: text_response(expected_output),
            },
        ]);
        let setup = setup.replace("{fake_mcp}", env!("CARGO_BIN_EXE_agens-cli-fake-mcp-child"));
        std::fs::write(
            config_home.join("config.toml"),
            format!(
                "[provider]\ntype = \"openai-chatgpt\"\nmodel = \"test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n{setup}",
                server.base_url(),
                data_directory.display(),
            ),
        )
        .expect("config should be written");
        write_chatgpt_credentials(&config_home, "SENTINEL_CHATGPT_TOOL_ACCESS");

        let output = Command::new(env!("CARGO_BIN_EXE_agens"))
            .args([
                "chat",
                "--dangerously-allow-all",
                "call a subscription tool",
            ])
            .current_dir(&project_root)
            .env("AGENS_CONFIG_HOME", &config_home)
            .env_remove("OPENAI_API_KEY")
            .output()
            .expect("production binary should execute");

        assert!(output.status.success(), "{name}: {output:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            format!("{expected_output}\n"),
            "{name}"
        );
        assert_eq!(String::from_utf8_lossy(&output.stderr), "", "{name}");
        assert!(
            String::from_utf8_lossy(
                &Command::new(env!("CARGO_BIN_EXE_agens"))
                    .args(["sessions", "list"])
                    .current_dir(&project_root)
                    .env("AGENS_CONFIG_HOME", &config_home)
                    .output()
                    .expect("sessions command should execute")
                    .stdout,
            )
            .ends_with("\tprimary\t1\n")
        );
        assert_sqlite_has_no_sentinels(
            &data_directory.join("rust-sessions.db"),
            &["SENTINEL_CHATGPT_TOOL_ACCESS", "SENTINEL_CHATGPT_REFRESH"],
        );

        server.join();
    }
}

#[cfg(unix)]
#[test]
fn production_binary_cancels_chatgpt_subscription_without_persisting_a_turn() {
    let temporary = TemporaryDirectory::new("production-chatgpt-cancellation");
    let config_home = temporary.path().join("config");
    let data_directory = temporary.path().join("data");
    std::fs::create_dir_all(&config_home).expect("config directory should exist");
    let mut server = StalledOpenAiMockServer::start();
    std::fs::write(
        config_home.join("config.toml"),
        format!(
            "[provider]\ntype = \"openai-chatgpt\"\nmodel = \"test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n",
            server.base_url(),
            data_directory.display(),
        ),
    )
    .expect("config should be written");
    write_chatgpt_credentials(&config_home, "SENTINEL_CHATGPT_CANCEL_ACCESS");

    let child = Command::new(env!("CARGO_BIN_EXE_agens"))
        .args(["chat", "cancel subscription request"])
        .env("AGENS_CONFIG_HOME", &config_home)
        .env_remove("OPENAI_API_KEY")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("production binary should start");
    server.wait_for_request();
    assert!(
        Command::new("kill")
            .args(["-INT", &child.id().to_string()])
            .status()
            .expect("SIGINT delivery should execute")
            .success()
    );
    let output = child
        .wait_with_output()
        .expect("production binary should exit after cancellation");

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "error: cancelled: headless turn was cancelled\n"
    );
    assert_no_saved_sessions(temporary.path(), &config_home);
    assert_sqlite_has_no_sentinels(
        &data_directory.join("rust-sessions.db"),
        &["SENTINEL_CHATGPT_CANCEL_ACCESS", "SENTINEL_CHATGPT_REFRESH"],
    );

    server.join();
}

#[test]
fn production_binary_executes_allowed_native_read_then_continues_and_persists() {
    let temporary = TemporaryDirectory::new("production-native-read");
    let project_root = temporary.path().join("project");
    let config_home = temporary.path().join("config");
    let data_directory = temporary.path().join("data");
    std::fs::create_dir_all(project_root.join(".git")).expect("project marker should exist");
    std::fs::create_dir_all(&config_home).expect("config directory should exist");
    std::fs::write(project_root.join("notes.md"), "native tool content")
        .expect("native read fixture should exist");

    let server = ScriptedNativeOpenAiMockServer::start(vec![
        ScriptedOpenAiResponse {
            required_body_fragments: vec![
                "\"tools\"".to_owned(),
                "native::read".to_owned(),
                "native::search".to_owned(),
            ],
            response: native_tool_call_response(
                "call_read",
                "native::read",
                r#"{"path":"notes.md"}"#,
            ),
        },
        ScriptedOpenAiResponse {
            required_body_fragments: vec![
                "\"type\":\"function_call_output\"".to_owned(),
                "\"call_id\":\"call_read\"".to_owned(),
                "native tool content".to_owned(),
            ],
            response: text_response("native read completed"),
        },
    ]);
    std::fs::write(
        config_home.join("config.toml"),
        format!(
            "[provider]\ntype = \"openai-api\"\nmodel = \"test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n[permissions]\nallow = [\"read(notes.md)\"]\n",
            server.base_url(),
            data_directory.display(),
        ),
    )
    .expect("config should be written");

    let chat = Command::new(env!("CARGO_BIN_EXE_agens"))
        .args(["chat", "read the native file"])
        .current_dir(&project_root)
        .env("AGENS_CONFIG_HOME", &config_home)
        .env("OPENAI_API_KEY", "SENTINEL_OPENAI_API_KEY")
        .output()
        .expect("production binary should execute");
    let sessions = Command::new(env!("CARGO_BIN_EXE_agens"))
        .args(["sessions", "list"])
        .current_dir(&project_root)
        .env("AGENS_CONFIG_HOME", &config_home)
        .output()
        .expect("sessions command should execute");
    let resumed = Command::new(env!("CARGO_BIN_EXE_agens"))
        .args(["sessions", "show", "1"])
        .current_dir(&project_root)
        .env("AGENS_CONFIG_HOME", &config_home)
        .output()
        .expect("session resume command should execute");

    assert!(chat.status.success());
    assert_eq!(
        String::from_utf8_lossy(&chat.stdout),
        "native read completed\n"
    );
    assert_eq!(String::from_utf8_lossy(&chat.stderr), "");
    assert!(String::from_utf8_lossy(&sessions.stdout).ends_with("\tprimary\t1\n"));
    assert_eq!(
        String::from_utf8_lossy(&resumed.stdout),
        format!(
            "Session 1: project={} title=read the native file agent=primary turns=1 messages=4\n",
            project_root.display(),
        ),
    );

    server.join();
}

#[test]
fn production_binary_executes_allowed_native_search_then_continues() {
    let temporary = TemporaryDirectory::new("production-native-search");
    let project_root = temporary.path().join("project");
    let config_home = temporary.path().join("config");
    let data_directory = temporary.path().join("data");
    std::fs::create_dir_all(project_root.join(".git")).expect("project marker should exist");
    std::fs::create_dir_all(&config_home).expect("config directory should exist");
    std::fs::write(
        project_root.join("notes.md"),
        "needle in the native search fixture",
    )
    .expect("native search fixture should exist");

    let server = ScriptedNativeOpenAiMockServer::start(vec![
        ScriptedOpenAiResponse {
            required_body_fragments: vec!["native::search".to_owned()],
            response: native_tool_call_response(
                "call_search",
                "native::search",
                r#"{"path":".","query":"needle"}"#,
            ),
        },
        ScriptedOpenAiResponse {
            required_body_fragments: vec![
                "\"call_id\":\"call_search\"".to_owned(),
                "needle in the native search fixture".to_owned(),
            ],
            response: text_response("native search completed"),
        },
    ]);
    std::fs::write(
        config_home.join("config.toml"),
        format!(
            "[provider]\ntype = \"openai-api\"\nmodel = \"test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n",
            server.base_url(),
            data_directory.display(),
        ),
    )
    .expect("config should be written");

    let chat = Command::new(env!("CARGO_BIN_EXE_agens"))
        .args(["chat", "--dangerously-allow-all", "search the native file"])
        .current_dir(&project_root)
        .env("AGENS_CONFIG_HOME", &config_home)
        .env("OPENAI_API_KEY", "SENTINEL_OPENAI_API_KEY")
        .output()
        .expect("production binary should execute");

    assert!(
        chat.status.success(),
        "stdout: {} stderr: {}",
        String::from_utf8_lossy(&chat.stdout),
        String::from_utf8_lossy(&chat.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&chat.stdout),
        "native search completed\n"
    );
    assert_eq!(String::from_utf8_lossy(&chat.stderr), "");
    assert!(
        PermissionGrantStore::open(&data_directory)
            .expect("grant store should open")
            .grants_for_project(&project_root.display().to_string())
            .expect("project grants should load")
            .is_empty(),
        "temporary bypass must not persist a grant"
    );

    server.join();
}

#[test]
fn production_binary_applies_static_exact_and_glob_allows_to_native_list_and_search() {
    for (name, tool, path, arguments, rule, expected_output) in [
        (
            "list exact",
            "native::list",
            "list-exact",
            r#"{"path":"list-exact"}"#,
            "list(list-exact)",
            "listed.txt",
        ),
        (
            "list glob",
            "native::list",
            "list-glob",
            r#"{"path":"list-glob"}"#,
            "list(list-*)",
            "listed.txt",
        ),
        (
            "search exact",
            "native::search",
            "search-exact",
            r#"{"path":"search-exact","query":"needle"}"#,
            "search(search-exact)",
            "needle",
        ),
        (
            "search glob",
            "native::search",
            "search-glob",
            r#"{"path":"search-glob","query":"needle"}"#,
            "search(search-*)",
            "needle",
        ),
    ] {
        let temporary = TemporaryDirectory::new(&format!("production-static-{name}"));
        let project_root = temporary.path().join("project");
        let config_home = temporary.path().join("config");
        let data_directory = temporary.path().join("data");
        let fixture_directory = project_root.join(path);
        std::fs::create_dir_all(project_root.join(".git")).expect("project marker should exist");
        std::fs::create_dir_all(&config_home).expect("config directory should exist");
        std::fs::create_dir_all(&fixture_directory).expect("fixture directory should exist");
        std::fs::write(
            fixture_directory.join("listed.txt"),
            "needle in static policy fixture",
        )
        .expect("fixture file should exist");

        let call_id = format!("call_{path}");
        let server = ScriptedNativeOpenAiMockServer::start(vec![
            ScriptedOpenAiResponse {
                required_body_fragments: vec![tool.to_owned()],
                response: native_tool_call_response(&call_id, tool, arguments),
            },
            ScriptedOpenAiResponse {
                required_body_fragments: vec![call_id.clone(), expected_output.to_owned()],
                response: text_response("static permission allowed"),
            },
        ]);
        std::fs::write(
            config_home.join("config.toml"),
            format!(
                "[provider]\ntype = \"openai-api\"\nmodel = \"test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n[permissions]\nallow = [{rule:?}]\n",
                server.base_url(),
                data_directory.display(),
            ),
        )
        .expect("config should be written");

        let output = Command::new(env!("CARGO_BIN_EXE_agens"))
            .args(["chat", "apply static native permission"])
            .current_dir(&project_root)
            .env("AGENS_CONFIG_HOME", &config_home)
            .env("OPENAI_API_KEY", "SENTINEL_OPENAI_API_KEY")
            .output()
            .expect("production binary should execute");

        assert!(output.status.success(), "{name}");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "static permission allowed\n",
            "{name}"
        );
        assert_eq!(String::from_utf8_lossy(&output.stderr), "", "{name}");
        assert!(
            String::from_utf8_lossy(
                &Command::new(env!("CARGO_BIN_EXE_agens"))
                    .args(["sessions", "list"])
                    .current_dir(&project_root)
                    .env("AGENS_CONFIG_HOME", &config_home)
                    .output()
                    .expect("sessions command should execute")
                    .stdout,
            )
            .ends_with("\tprimary\t1\n")
        );
        assert!(
            PermissionGrantStore::open(&data_directory)
                .expect("grant store should open")
                .grants_for_project(&project_root.display().to_string())
                .expect("project grants should load")
                .is_empty(),
            "{name}: non-TTY denial must not persist a grant"
        );

        server.join();
    }
}

#[test]
fn production_binary_static_glob_denies_native_list_and_search_without_execution() {
    for (name, tool, path, arguments, rule) in [
        (
            "list",
            "native::list",
            "denied-list",
            r#"{"path":"denied-list"}"#,
            "list(denied-*)",
        ),
        (
            "search",
            "native::search",
            "denied-search",
            r#"{"path":"denied-search","query":"needle"}"#,
            "search(denied-*)",
        ),
    ] {
        let temporary = TemporaryDirectory::new(&format!("production-static-deny-{name}"));
        let project_root = temporary.path().join("project");
        let config_home = temporary.path().join("config");
        let data_directory = temporary.path().join("data");
        let fixture_directory = project_root.join(path);
        std::fs::create_dir_all(project_root.join(".git")).expect("project marker should exist");
        std::fs::create_dir_all(&config_home).expect("config directory should exist");
        std::fs::create_dir_all(&fixture_directory).expect("fixture directory should exist");
        let protected = fixture_directory.join("protected.txt");
        std::fs::write(&protected, "must remain unchanged").expect("fixture file should exist");

        let call_id = format!("call_denied_{name}");
        let server = ScriptedNativeOpenAiMockServer::start(vec![
            ScriptedOpenAiResponse {
                required_body_fragments: vec![tool.to_owned()],
                response: native_tool_call_response(&call_id, tool, arguments),
            },
            ScriptedOpenAiResponse {
                required_body_fragments: vec![
                    call_id,
                    "\"output\":\"Tool execution failed\"".to_owned(),
                ],
                response: text_response("static permission denied"),
            },
        ]);
        std::fs::write(
            config_home.join("config.toml"),
            format!(
                "[provider]\ntype = \"openai-api\"\nmodel = \"test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n[permissions]\ndeny = [{rule:?}]\n",
                server.base_url(),
                data_directory.display(),
            ),
        )
        .expect("config should be written");

        let output = Command::new(env!("CARGO_BIN_EXE_agens"))
            .args(["chat", "deny static native permission"])
            .current_dir(&project_root)
            .env("AGENS_CONFIG_HOME", &config_home)
            .env("OPENAI_API_KEY", "SENTINEL_OPENAI_API_KEY")
            .output()
            .expect("production binary should execute");

        assert!(output.status.success(), "{name}");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "static permission denied\n",
            "{name}"
        );
        assert_eq!(
            std::fs::read_to_string(&protected).expect("protected fixture should remain readable"),
            "must remain unchanged",
            "{name}"
        );

        server.join();
    }
}

#[test]
fn production_binary_denies_unrelated_static_list_and_search_targets_and_continues() {
    for (name, tool, path, arguments, rule) in [
        (
            "list",
            "native::list",
            "unrelated-list",
            r#"{"path":"unrelated-list"}"#,
            "list(allowed-list)",
        ),
        (
            "search",
            "native::search",
            "unrelated-search",
            r#"{"path":"unrelated-search","query":"needle"}"#,
            "search(allowed-search)",
        ),
    ] {
        let temporary = TemporaryDirectory::new(&format!("production-static-ask-{name}"));
        let project_root = temporary.path().join("project");
        let config_home = temporary.path().join("config");
        let data_directory = temporary.path().join("data");
        let fixture_directory = project_root.join(path);
        std::fs::create_dir_all(project_root.join(".git")).expect("project marker should exist");
        std::fs::create_dir_all(&config_home).expect("config directory should exist");
        std::fs::create_dir_all(&fixture_directory).expect("fixture directory should exist");
        let protected = fixture_directory.join("protected.txt");
        std::fs::write(&protected, "must not be read").expect("fixture file should exist");

        let server = ScriptedNativeOpenAiMockServer::start(vec![
            ScriptedOpenAiResponse {
                required_body_fragments: vec![tool.to_owned()],
                response: native_tool_call_response("call_ask", tool, arguments),
            },
            ScriptedOpenAiResponse {
                required_body_fragments: vec![
                    "\"call_id\":\"call_ask\"".to_owned(),
                    "\"output\":\"Tool execution failed\"".to_owned(),
                ],
                response: text_response("static ask denial handled"),
            },
        ]);
        std::fs::write(
            config_home.join("config.toml"),
            format!(
                "[provider]\ntype = \"openai-api\"\nmodel = \"test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n[permissions]\nallow = [{rule:?}]\n",
                server.base_url(),
                data_directory.display(),
            ),
        )
        .expect("config should be written");

        let output = Command::new(env!("CARGO_BIN_EXE_agens"))
            .args(["chat", "request unrelated native permission"])
            .current_dir(&project_root)
            .env("AGENS_CONFIG_HOME", &config_home)
            .env("OPENAI_API_KEY", "SENTINEL_OPENAI_API_KEY")
            .output()
            .expect("production binary should execute");

        assert!(output.status.success(), "{name}");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "static ask denial handled\n",
            "{name}"
        );
        assert_eq!(String::from_utf8_lossy(&output.stderr), "", "{name}");
        assert_eq!(
            std::fs::read_to_string(&protected).expect("protected fixture should remain readable"),
            "must not be read",
            "{name}"
        );
        assert!(
            String::from_utf8_lossy(
                &Command::new(env!("CARGO_BIN_EXE_agens"))
                    .args(["sessions", "list"])
                    .current_dir(&project_root)
                    .env("AGENS_CONFIG_HOME", &config_home)
                    .output()
                    .expect("sessions command should execute")
                    .stdout,
            )
            .ends_with("\tprimary\t1\n")
        );

        server.join();
    }
}

#[test]
fn production_binary_denies_native_read_without_side_effect_and_continues_safely() {
    let temporary = TemporaryDirectory::new("production-native-deny");
    let project_root = temporary.path().join("project");
    let config_home = temporary.path().join("config");
    let data_directory = temporary.path().join("data");
    std::fs::create_dir_all(project_root.join(".git")).expect("project marker should exist");
    std::fs::create_dir_all(&config_home).expect("config directory should exist");
    let protected = project_root.join("SENTINEL_DENIED_INPUT.txt");
    std::fs::write(&protected, "must not be read").expect("protected fixture should exist");

    let server = ScriptedNativeOpenAiMockServer::start(vec![
        ScriptedOpenAiResponse {
            required_body_fragments: vec!["native::read".to_owned()],
            response: native_tool_call_response(
                "call_denied",
                "native::read",
                r#"{"path":"SENTINEL_DENIED_INPUT.txt"}"#,
            ),
        },
        ScriptedOpenAiResponse {
            required_body_fragments: vec![
                "\"call_id\":\"call_denied\"".to_owned(),
                "\"output\":\"Tool execution failed\"".to_owned(),
            ],
            response: text_response("denial handled"),
        },
    ]);
    std::fs::write(
        config_home.join("config.toml"),
        format!(
            "[provider]\ntype = \"openai-api\"\nmodel = \"test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n[permissions]\ndeny = [\"read(SENTINEL_DENIED_INPUT.txt)\"]\n",
            server.base_url(),
            data_directory.display(),
        ),
    )
    .expect("config should be written");

    let chat = Command::new(env!("CARGO_BIN_EXE_agens"))
        .args([
            "chat",
            "--dangerously-allow-all",
            "attempt denied native read",
        ])
        .current_dir(&project_root)
        .env("AGENS_CONFIG_HOME", &config_home)
        .env("OPENAI_API_KEY", "SENTINEL_OPENAI_API_KEY")
        .output()
        .expect("production binary should execute");

    assert!(chat.status.success());
    assert_eq!(String::from_utf8_lossy(&chat.stdout), "denial handled\n");
    assert_eq!(
        std::fs::read_to_string(&protected).unwrap(),
        "must not be read"
    );
    assert!(
        !format!(
            "{}{}",
            String::from_utf8_lossy(&chat.stdout),
            String::from_utf8_lossy(&chat.stderr)
        )
        .contains("SENTINEL_DENIED_INPUT")
    );

    server.join();
}

#[test]
fn production_binary_denies_unresolved_native_call_without_dispatching_and_continues() {
    let temporary = TemporaryDirectory::new("production-native-ask");
    let project_root = temporary.path().join("project");
    let config_home = temporary.path().join("config");
    let data_directory = temporary.path().join("data");
    std::fs::create_dir_all(project_root.join(".git")).expect("project marker should exist");
    std::fs::create_dir_all(&config_home).expect("config directory should exist");
    let protected = project_root.join("SENTINEL_UNRESOLVED_ASK.txt");
    std::fs::write(&protected, "must not be read").expect("protected fixture should exist");
    let server = ScriptedNativeOpenAiMockServer::start(vec![
        ScriptedOpenAiResponse {
            required_body_fragments: vec!["native::read".to_owned()],
            response: native_tool_call_response(
                "call_ask",
                "native::read",
                r#"{"path":"SENTINEL_UNRESOLVED_ASK.txt"}"#,
            ),
        },
        ScriptedOpenAiResponse {
            required_body_fragments: vec![
                "\"call_id\":\"call_ask\"".to_owned(),
                "\"output\":\"Tool execution failed\"".to_owned(),
                "!SENTINEL_UNRESOLVED_ASK".to_owned(),
            ],
            response: text_response("native ask denial handled"),
        },
    ]);
    std::fs::write(
        config_home.join("config.toml"),
        format!(
            "[provider]\ntype = \"openai-api\"\nmodel = \"test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n",
            server.base_url(),
            data_directory.display(),
        ),
    )
    .expect("config should be written");

    let output = Command::new(env!("CARGO_BIN_EXE_agens"))
        .args(["chat", "request native tool"])
        .current_dir(&project_root)
        .env("AGENS_CONFIG_HOME", &config_home)
        .env("OPENAI_API_KEY", "SENTINEL_OPENAI_API_KEY")
        .output()
        .expect("production binary should execute");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "native ask denial handled\n"
    );
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
    assert_eq!(
        std::fs::read_to_string(&protected).expect("protected fixture should remain readable"),
        "must not be read"
    );
    assert!(
        !format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
        .contains("SENTINEL_UNRESOLVED_ASK")
    );
    let sessions = Command::new(env!("CARGO_BIN_EXE_agens"))
        .args(["sessions", "list"])
        .env("AGENS_CONFIG_HOME", &config_home)
        .output()
        .expect("sessions command should execute");
    assert!(sessions.status.success());
    assert!(String::from_utf8_lossy(&sessions.stdout).ends_with("\tprimary\t1\n"));
    assert!(
        PermissionGrantStore::open(&data_directory)
            .expect("grant store should open")
            .grants_for_project(&project_root.display().to_string())
            .expect("project grants should load")
            .is_empty(),
        "non-TTY denial must not persist a grant"
    );

    server.join();
}

#[test]
fn production_binary_denies_native_write_in_chat_mode_even_with_temporary_bypass() {
    let temporary = TemporaryDirectory::new("production-chat-write-deny");
    let project_root = temporary.path().join("project");
    let config_home = temporary.path().join("config");
    let data_directory = temporary.path().join("data");
    std::fs::create_dir_all(project_root.join(".git")).expect("project marker should exist");
    std::fs::create_dir_all(&config_home).expect("config directory should exist");
    let protected = project_root.join("SENTINEL_CHAT_WRITE.txt");
    let server = BoundedScriptedOpenAiMockServer::start(vec![
        ScriptedOpenAiResponse {
            required_body_fragments: vec!["native::write".to_owned()],
            response: native_tool_call_response(
                "call_chat_write",
                "native::write",
                r#"{"path":"SENTINEL_CHAT_WRITE.txt","content":"must not be written"}"#,
            ),
        },
        ScriptedOpenAiResponse {
            required_body_fragments: vec![
                "\"call_id\":\"call_chat_write\"".to_owned(),
                "\"output\":\"Tool execution failed\"".to_owned(),
            ],
            response: text_response("chat mode denial handled"),
        },
    ]);
    std::fs::write(
        config_home.join("config.toml"),
        format!(
            "[provider]\ntype = \"openai-api\"\nmodel = \"test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n",
            server.base_url(),
            data_directory.display(),
        ),
    )
    .expect("config should be written");

    let output = Command::new(env!("CARGO_BIN_EXE_agens"))
        .args([
            "chat",
            "--mode",
            "chat",
            "--dangerously-allow-all",
            "attempt a native write",
        ])
        .current_dir(&project_root)
        .env("AGENS_CONFIG_HOME", &config_home)
        .env("OPENAI_API_KEY", "SENTINEL_OPENAI_API_KEY")
        .output()
        .expect("production binary should execute");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "chat mode denial handled\n"
    );
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
    assert!(!protected.exists(), "chat mode must block native writes");

    server.join();
}

#[test]
fn production_binary_rejects_duplicate_and_mismatched_tool_call_protocol_items_before_dispatch() {
    for (name, response) in [
        (
            "duplicate",
            sse_response(&[
                r#"{"type":"response.created","response":{"id":"response_duplicate"}}"#,
                r#"{"type":"response.output_item.added","item":{"id":"item_one","type":"function_call","call_id":"call_duplicate","name":"native::write","arguments":""}}"#,
                r#"{"type":"response.output_item.added","item":{"id":"item_two","type":"function_call","call_id":"call_duplicate","name":"native::write","arguments":""}}"#,
            ]),
        ),
        (
            "mismatched",
            sse_response(&[
                r#"{"type":"response.created","response":{"id":"response_mismatched"}}"#,
                r#"{"type":"response.output_item.added","item":{"id":"item_expected","type":"function_call","call_id":"call_mismatched","name":"native::write","arguments":""}}"#,
                r#"{"type":"response.function_call_arguments.done","item_id":"item_other","arguments":"{\"path\":\"should-not-exist\",\"content\":\"must not be written\"}"}"#,
            ]),
        ),
    ] {
        let temporary = TemporaryDirectory::new(&format!("production-{name}-call-id"));
        let project_root = temporary.path().join("project");
        let config_home = temporary.path().join("config");
        let data_directory = temporary.path().join("data");
        std::fs::create_dir_all(project_root.join(".git")).expect("project marker should exist");
        std::fs::create_dir_all(&config_home).expect("config directory should exist");
        let side_effect = project_root.join("should-not-exist");
        let server = BoundedScriptedOpenAiMockServer::start(vec![ScriptedOpenAiResponse {
            required_body_fragments: vec!["native::write".to_owned()],
            response,
        }]);
        std::fs::write(
            config_home.join("config.toml"),
            format!(
                "[provider]\ntype = \"openai-api\"\nmodel = \"test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n",
                server.base_url(),
                data_directory.display(),
            ),
        )
        .expect("config should be written");

        let output = Command::new(env!("CARGO_BIN_EXE_agens"))
            .args([
                "chat",
                "--dangerously-allow-all",
                "reject malformed tool call",
            ])
            .current_dir(&project_root)
            .env("AGENS_CONFIG_HOME", &config_home)
            .env("OPENAI_API_KEY", "SENTINEL_OPENAI_API_KEY")
            .output()
            .expect("production binary should execute");

        assert_eq!(output.status.code(), Some(1), "{name}");
        assert_eq!(String::from_utf8_lossy(&output.stdout), "", "{name}");
        assert_eq!(
            String::from_utf8_lossy(&output.stderr),
            "error: provider: provider request failed\n",
            "{name}"
        );
        assert!(!side_effect.exists(), "{name} call must not dispatch");
        assert_no_saved_sessions(&project_root, &config_home);

        server.join();
    }
}

#[cfg(unix)]
#[test]
fn production_binary_cancellation_kills_native_bash_descendants_without_continuing_or_persisting() {
    let temporary = TemporaryDirectory::new("production-native-bash-cancel");
    let project_root = temporary.path().join("project");
    let config_home = temporary.path().join("config");
    let data_directory = temporary.path().join("data");
    let process_marker = temporary.path().join("processes");
    let ready_marker = temporary.path().join("ready");
    std::fs::create_dir_all(project_root.join(".git")).expect("project marker should exist");
    std::fs::create_dir_all(&config_home).expect("config directory should exist");
    let command = format!(
        "bash -c 'sleep 30 & descendant=$!; printf \"%s %s\\n\" \"$$\" \"$descendant\" > \"$1\"; : > \"$2\"; wait' bash {:?} {:?} & wait",
        process_marker, ready_marker
    );
    let server = BoundedScriptedOpenAiMockServer::start(vec![ScriptedOpenAiResponse {
        required_body_fragments: vec!["native::bash".to_owned()],
        response: native_tool_call_response(
            "call_bash_cancel",
            "native::bash",
            &format!(r#"{{"command":{command:?}}}"#),
        ),
    }]);
    std::fs::write(
        config_home.join("config.toml"),
        format!(
            "[provider]\ntype = \"openai-api\"\nmodel = \"test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n[permissions]\nallow = [\"bash(*)\"]\n",
            server.base_url(),
            data_directory.display(),
        ),
    )
    .expect("config should be written");

    let child = Command::new(env!("CARGO_BIN_EXE_agens"))
        .args([
            "chat",
            "--dangerously-allow-all",
            "run the long native bash command",
        ])
        .current_dir(&project_root)
        .env("AGENS_CONFIG_HOME", &config_home)
        .env("OPENAI_API_KEY", "SENTINEL_OPENAI_API_KEY")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("production binary should start");
    wait_for_path(&ready_marker, Duration::from_secs(2));

    let signal_status = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .expect("SIGINT command should execute");
    assert!(signal_status.success(), "SIGINT delivery should succeed");

    let output = wait_for_child_output(child, Duration::from_secs(2));
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "error: cancelled: headless turn was cancelled\n"
    );

    let process_ids = std::fs::read_to_string(&process_marker)
        .expect("native bash should record its child and descendant process IDs")
        .split_whitespace()
        .map(|process_id| {
            process_id
                .parse::<u32>()
                .expect("process ID should be numeric")
        })
        .collect::<Vec<_>>();
    assert_eq!(process_ids.len(), 2);
    for process_id in process_ids {
        wait_for_process_exit(process_id, Duration::from_secs(2));
    }
    assert_no_saved_sessions(&project_root, &config_home);

    server.join();
}

#[test]
fn production_binary_rejects_replayed_native_call_id_without_second_execution() {
    let temporary = TemporaryDirectory::new("production-native-call-integrity");
    let project_root = temporary.path().join("project");
    let config_home = temporary.path().join("config");
    let data_directory = temporary.path().join("data");
    std::fs::create_dir_all(project_root.join(".git")).expect("project marker should exist");
    std::fs::create_dir_all(&config_home).expect("config directory should exist");
    let side_effect = project_root.join("execution-count");
    let initial_call = native_tool_call_response(
        "call_once",
        "native::write",
        r#"{"path":"execution-count","content":"first execution"}"#,
    );
    let replayed_call = native_tool_call_response(
        "call_once",
        "native::write",
        r#"{"path":"execution-count","content":"second execution"}"#,
    );
    let server = BoundedScriptedOpenAiMockServer::start(vec![
        ScriptedOpenAiResponse {
            required_body_fragments: vec!["native::write".to_owned()],
            response: initial_call,
        },
        ScriptedOpenAiResponse {
            required_body_fragments: vec![
                "\"call_id\":\"call_once\"".to_owned(),
                "wrote execution-count".to_owned(),
            ],
            response: replayed_call,
        },
    ]);
    std::fs::write(
        config_home.join("config.toml"),
        format!(
            "[provider]\ntype = \"openai-api\"\nmodel = \"test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n[permissions]\nallow = [\"write(execution-count)\"]\n",
            server.base_url(),
            data_directory.display(),
        ),
    )
    .expect("config should be written");

    let output = Command::new(env!("CARGO_BIN_EXE_agens"))
        .args(["chat", "execute exactly once"])
        .current_dir(&project_root)
        .env("AGENS_CONFIG_HOME", &config_home)
        .env("OPENAI_API_KEY", "SENTINEL_OPENAI_API_KEY")
        .output()
        .expect("production binary should execute");

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "error: provider: provider request failed\n"
    );
    assert_eq!(
        std::fs::read_to_string(&side_effect)
            .expect("only the first authorized call should execute"),
        "first execution"
    );
    assert_no_saved_sessions(&project_root, &config_home);

    server.join();
}

#[cfg(unix)]
#[test]
fn production_binary_cancellation_has_deterministic_output_exit_and_no_persistence() {
    let temporary = TemporaryDirectory::new("production-cancellation");
    let config_home = temporary.path().join("config");
    let data_directory = temporary.path().join("data");
    std::fs::create_dir_all(&config_home).expect("config directory should exist");
    let mut server = StalledOpenAiMockServer::start();
    std::fs::write(
        config_home.join("config.toml"),
        format!(
            "[provider]\ntype = \"openai-api\"\nmodel = \"test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n",
            server.base_url(),
            data_directory.display(),
        ),
    )
    .expect("config should be written");

    let child = Command::new(env!("CARGO_BIN_EXE_agens"))
        .args(["chat", "cancel production request"])
        .env("AGENS_CONFIG_HOME", &config_home)
        .env("OPENAI_API_KEY", "SENTINEL_OPENAI_API_KEY")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("production binary should start");
    server.wait_for_request();
    let signal_status = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .expect("SIGINT command should execute");
    assert!(signal_status.success(), "SIGINT delivery should succeed");

    let output = child
        .wait_with_output()
        .expect("production binary should exit after cancellation");

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "error: cancelled: headless turn was cancelled\n"
    );
    let sessions = Command::new(env!("CARGO_BIN_EXE_agens"))
        .args(["sessions", "list"])
        .env("AGENS_CONFIG_HOME", &config_home)
        .output()
        .expect("sessions command should execute");
    assert!(sessions.status.success());
    assert_eq!(
        String::from_utf8_lossy(&sessions.stdout),
        "No saved sessions.\n"
    );

    server.join();
}

#[test]
fn production_binary_sanitizes_remote_response_headers_and_body() {
    let temporary = TemporaryDirectory::new("production-remote-error");
    let config_home = temporary.path().join("config");
    std::fs::create_dir_all(&config_home).expect("config directory should exist");
    let server = ErrorOpenAiMockServer::start();
    std::fs::write(
        config_home.join("config.toml"),
        format!(
            "[provider]\ntype = \"openai-api\"\nmodel = \"test-model\"\nbase_url = \"{}\"\n",
            server.base_url(),
        ),
    )
    .expect("config should be written");

    let output = Command::new(env!("CARGO_BIN_EXE_agens"))
        .args(["chat", "remote error"])
        .env("AGENS_CONFIG_HOME", &config_home)
        .env("OPENAI_API_KEY", "SENTINEL_OPENAI_API_KEY")
        .output()
        .expect("production binary should execute");

    let diagnostics = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(diagnostics, "error: provider: provider request failed\n");
    for secret in [
        "SENTINEL_OPENAI_API_KEY",
        "SENTINEL_REMOTE_ERROR_HEADER",
        "SENTINEL_REMOTE_ERROR_BODY",
    ] {
        assert!(!diagnostics.contains(secret), "diagnostics leaked {secret}");
    }

    server.join();
}

#[test]
fn production_binary_sanitizes_config_and_store_error_sources() {
    let temporary = TemporaryDirectory::new("production-config-store-secret-matrix");
    let config_home = temporary.path().join("config");
    std::fs::create_dir_all(&config_home).expect("config directory should exist");
    let malformed_value = "SENTINEL_CONFIG_PARSE_VALUE";
    std::fs::write(
        config_home.join("config.toml"),
        format!("[provider\nmodel = {malformed_value:?}\n"),
    )
    .expect("malformed config should be written");

    let config_output = Command::new(env!("CARGO_BIN_EXE_agens"))
        .args(["chat", "reject malformed config"])
        .env("AGENS_CONFIG_HOME", &config_home)
        .output()
        .expect("production binary should execute");
    assert_eq!(config_output.status.code(), Some(3));
    assert_eq!(String::from_utf8_lossy(&config_output.stdout), "");
    assert_eq!(
        String::from_utf8_lossy(&config_output.stderr),
        "error: config: global configuration is invalid\n"
    );
    assert!(!format!("{config_output:?}").contains(malformed_value));

    let store_config_home = temporary.path().join("store-config");
    let store_path = temporary.path().join("SENTINEL_STORE_PATH");
    std::fs::create_dir_all(&store_config_home).expect("store config directory should exist");
    std::fs::write(&store_path, "not a directory").expect("store error fixture should exist");
    std::fs::write(
        store_config_home.join("config.toml"),
        format!(
            "[provider]\ntype = \"openai-api\"\nmodel = \"test-model\"\nbase_url = \"http://127.0.0.1:1\"\n\n[options]\ndata_dir = \"{}\"\n",
            store_path.display()
        ),
    )
    .expect("store config should be written");

    let store_output = Command::new(env!("CARGO_BIN_EXE_agens"))
        .args(["chat", "reject store path"])
        .env("AGENS_CONFIG_HOME", &store_config_home)
        .env("OPENAI_API_KEY", "SENTINEL_OPENAI_API_KEY")
        .output()
        .expect("production binary should execute");
    assert_eq!(store_output.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&store_output.stdout), "");
    assert_eq!(
        String::from_utf8_lossy(&store_output.stderr),
        "error: store: permission grants are unavailable\n"
    );
    for secret in ["SENTINEL_STORE_PATH", "SENTINEL_OPENAI_API_KEY"] {
        assert!(!format!("{store_output:?}").contains(secret));
    }
}

#[test]
fn production_binary_composes_configured_mcp_tools_with_native_catalog_and_persists() {
    let temporary = TemporaryDirectory::new("production-mcp-composition");
    let project_root = temporary.path().join("project");
    let config_home = temporary.path().join("config");
    let data_directory = temporary.path().join("data");
    std::fs::create_dir_all(project_root.join(".git")).expect("project marker should exist");
    std::fs::create_dir_all(&config_home).expect("config directory should exist");

    let server = ScriptedNativeOpenAiMockServer::start(vec![
        ScriptedOpenAiResponse {
            required_body_fragments: vec![
                "native::read".to_owned(),
                "files::first".to_owned(),
                "files::second".to_owned(),
            ],
            response: native_tool_call_response("call_mcp", "files::first", r#"{}"#),
        },
        ScriptedOpenAiResponse {
            required_body_fragments: vec![
                "\"call_id\":\"call_mcp\"".to_owned(),
                "tool succeeded".to_owned(),
            ],
            response: text_response("MCP tool completed"),
        },
    ]);
    std::fs::write(
        config_home.join("config.toml"),
        format!(
            "[provider]\ntype = \"openai-api\"\nmodel = \"test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n[mcp.broken]\ntransport = \"stdio\"\ncommand = \"{}\"\nargs = [\"malformed\"]\ntimeout_ms = 1000\n[mcp.broken.env]\nFAKE_MCP_PROTOCOL_SECRET = \"SENTINEL_MCP_PROTOCOL\"\nFAKE_MCP_STDERR_SECRET = \"SENTINEL_MCP_STDERR\"\n\n[mcp.crashed]\ntransport = \"stdio\"\ncommand = \"{}\"\nargs = [\"crash\"]\ntimeout_ms = 1000\n[mcp.crashed.env]\nFAKE_MCP_TRANSPORT_SECRET = \"SENTINEL_MCP_TRANSPORT\"\n\n[mcp.files]\ntransport = \"stdio\"\ncommand = \"{}\"\nargs = [\"success\"]\ntimeout_ms = 1000\n",
            server.base_url(),
            data_directory.display(),
            env!("CARGO_BIN_EXE_agens-cli-fake-mcp-child"),
            env!("CARGO_BIN_EXE_agens-cli-fake-mcp-child"),
            env!("CARGO_BIN_EXE_agens-cli-fake-mcp-child"),
        ),
    )
    .expect("config should be written");

    let output = Command::new(env!("CARGO_BIN_EXE_agens"))
        .args([
            "chat",
            "--dangerously-allow-all",
            "call the configured MCP tool",
        ])
        .current_dir(&project_root)
        .env("AGENS_CONFIG_HOME", &config_home)
        .env("OPENAI_API_KEY", "SENTINEL_OPENAI_API_KEY")
        .output()
        .expect("production binary should execute");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "MCP tool completed\n"
    );
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
    let diagnostics = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!diagnostics.contains("SENTINEL_MCP_PROTOCOL"));
    assert!(!diagnostics.contains("SENTINEL_MCP_STDERR"));
    assert!(!diagnostics.contains("SENTINEL_MCP_TRANSPORT"));
    assert!(
        String::from_utf8_lossy(
            &Command::new(env!("CARGO_BIN_EXE_agens"))
                .args(["sessions", "list"])
                .current_dir(&project_root)
                .env("AGENS_CONFIG_HOME", &config_home)
                .output()
                .expect("sessions command should execute")
                .stdout,
        )
        .ends_with("\tprimary\t1\n")
    );
    let session = SessionStore::open(&data_directory)
        .expect("session store should open")
        .load_session_for_resume(1)
        .expect("completed session should be readable");
    for secret in [
        "SENTINEL_OPENAI_API_KEY",
        "SENTINEL_MCP_PROTOCOL",
        "SENTINEL_MCP_STDERR",
        "SENTINEL_MCP_TRANSPORT",
    ] {
        assert!(
            !format!("{session:?}").contains(secret),
            "snapshot leaked {secret}"
        );
    }
    assert_sqlite_has_no_sentinels(
        &data_directory.join("rust-sessions.db"),
        &[
            "SENTINEL_OPENAI_API_KEY",
            "SENTINEL_MCP_PROTOCOL",
            "SENTINEL_MCP_STDERR",
            "SENTINEL_MCP_TRANSPORT",
        ],
    );

    server.join();
}

#[cfg(unix)]
#[test]
fn production_binary_cancels_configured_mcp_call_without_continuing_or_persisting() {
    let temporary = TemporaryDirectory::new("production-mcp-cancel");
    let project_root = temporary.path().join("project");
    let config_home = temporary.path().join("config");
    let data_directory = temporary.path().join("data");
    let call_ready = temporary.path().join("mcp-call-ready");
    std::fs::create_dir_all(project_root.join(".git")).expect("project marker should exist");
    std::fs::create_dir_all(&config_home).expect("config directory should exist");

    let server = BoundedScriptedOpenAiMockServer::start(vec![ScriptedOpenAiResponse {
        required_body_fragments: vec!["files::first".to_owned()],
        response: native_tool_call_response("call_mcp_cancel", "files::first", r#"{}"#),
    }]);
    std::fs::write(
        config_home.join("config.toml"),
        format!(
            "[provider]\ntype = \"openai-api\"\nmodel = \"test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n[mcp.files]\ntransport = \"stdio\"\ncommand = \"{}\"\nargs = [\"call-sleep\"]\ntimeout_ms = 1000\n[mcp.files.env]\nFAKE_MCP_CALL_READY = \"{}\"\n",
            server.base_url(),
            data_directory.display(),
            env!("CARGO_BIN_EXE_agens-cli-fake-mcp-child"),
            call_ready.display(),
        ),
    )
    .expect("config should be written");

    let child = Command::new(env!("CARGO_BIN_EXE_agens"))
        .args([
            "chat",
            "--dangerously-allow-all",
            "cancel configured MCP tool",
        ])
        .current_dir(&project_root)
        .env("AGENS_CONFIG_HOME", &config_home)
        .env("OPENAI_API_KEY", "SENTINEL_OPENAI_API_KEY")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("production binary should start");
    wait_for_path(&call_ready, Duration::from_secs(2));

    assert!(
        Command::new("kill")
            .args(["-INT", &child.id().to_string()])
            .status()
            .expect("SIGINT command should execute")
            .success()
    );
    let output = wait_for_child_output(child, Duration::from_secs(2));

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "error: cancelled: headless turn was cancelled\n"
    );
    assert_no_saved_sessions(&project_root, &config_home);

    server.join();
}

#[test]
fn production_binary_persists_model_visible_mcp_arguments_without_transport_secrets() {
    let temporary = TemporaryDirectory::new("production-mcp-secrets");
    let project_root = temporary.path().join("project");
    let config_home = temporary.path().join("config");
    let data_directory = temporary.path().join("data");
    std::fs::create_dir_all(project_root.join(".git")).expect("project marker should exist");
    std::fs::create_dir_all(&config_home).expect("config directory should exist");

    let server = ScriptedNativeOpenAiMockServer::start(vec![
        ScriptedOpenAiResponse {
            required_body_fragments: vec!["files::first".to_owned()],
            response: native_tool_call_response(
                "call_mcp_error",
                "files::first",
                r#"{"token":"SENTINEL_MCP_ARGUMENT"}"#,
            ),
        },
        ScriptedOpenAiResponse {
            required_body_fragments: vec![
                "\"call_id\":\"call_mcp_error\"".to_owned(),
                "\"output\":\"Tool execution failed\"".to_owned(),
                "!SENTINEL_MCP_ARGUMENT".to_owned(),
                "!SENTINEL_MCP_REMOTE_BODY".to_owned(),
            ],
            response: text_response("MCP failure handled"),
        },
    ]);
    std::fs::write(
        config_home.join("config.toml"),
        format!(
            "[provider]\ntype = \"openai-api\"\nmodel = \"test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n[mcp.files]\ntransport = \"stdio\"\ncommand = \"{}\"\nargs = [\"call-error\"]\ntimeout_ms = 1000\n[mcp.files.env]\nFAKE_MCP_TOOL_ERROR_SECRET = \"SENTINEL_MCP_REMOTE_BODY\"\nFAKE_MCP_STDERR_SECRET = \"SENTINEL_MCP_STDERR\"\n",
            server.base_url(),
            data_directory.display(),
            env!("CARGO_BIN_EXE_agens-cli-fake-mcp-child"),
        ),
    )
    .expect("config should be written");

    let output = Command::new(env!("CARGO_BIN_EXE_agens"))
        .args(["chat", "--dangerously-allow-all", "run failing MCP tool"])
        .current_dir(&project_root)
        .env("AGENS_CONFIG_HOME", &config_home)
        .env("OPENAI_API_KEY", "SENTINEL_OPENAI_API_KEY")
        .output()
        .expect("production binary should execute");
    let diagnostics = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(output.status.success(), "{diagnostics}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "MCP failure handled\n"
    );
    for secret in [
        "SENTINEL_OPENAI_API_KEY",
        "SENTINEL_MCP_ARGUMENT",
        "SENTINEL_MCP_REMOTE_BODY",
        "SENTINEL_MCP_STDERR",
    ] {
        assert!(!diagnostics.contains(secret), "diagnostics leaked {secret}");
    }
    let session = SessionStore::open(&data_directory)
        .expect("session store should open")
        .load_session_for_resume(1)
        .expect("completed session should be readable");
    assert!(
        format!("{session:?}").contains("SENTINEL_MCP_ARGUMENT"),
        "model-visible MCP arguments must remain resumable conversation content"
    );
    assert!(!format!("{session:?}").contains("SENTINEL_MCP_REMOTE_BODY"));
    assert_sqlite_has_no_sentinels(
        &data_directory.join("rust-sessions.db"),
        &[
            "SENTINEL_OPENAI_API_KEY",
            "SENTINEL_MCP_REMOTE_BODY",
            "SENTINEL_MCP_STDERR",
        ],
    );
    assert_sqlite_contains_sentinels(
        &data_directory.join("rust-sessions.db"),
        &["SENTINEL_MCP_ARGUMENT"],
    );

    server.join();
}

#[test]
fn production_binary_persists_model_visible_native_arguments_without_error_output() {
    let temporary = TemporaryDirectory::new("production-native-secret-matrix");
    let project_root = temporary.path().join("project");
    let config_home = temporary.path().join("config");
    let data_directory = temporary.path().join("data");
    std::fs::create_dir_all(project_root.join(".git")).expect("project marker should exist");
    std::fs::create_dir_all(&config_home).expect("config directory should exist");

    let command = ": SENTINEL_NATIVE_ARGUMENT; printf SENTINEL_NATIVE_OUTPUT >&2; exit 1";
    let server = ScriptedNativeOpenAiMockServer::start(vec![
        ScriptedOpenAiResponse {
            required_body_fragments: vec!["native::bash".to_owned()],
            response: native_tool_call_response(
                "call_native_secret",
                "native::bash",
                &format!(r#"{{"command":{command:?}}}"#),
            ),
        },
        ScriptedOpenAiResponse {
            required_body_fragments: vec![
                "\"call_id\":\"call_native_secret\"".to_owned(),
                "\"output\":\"Tool execution failed\"".to_owned(),
                "!SENTINEL_NATIVE_ARGUMENT".to_owned(),
                "!SENTINEL_NATIVE_OUTPUT".to_owned(),
            ],
            response: text_response("native failure handled"),
        },
    ]);
    std::fs::write(
        config_home.join("config.toml"),
        format!(
            "[provider]\ntype = \"openai-api\"\nmodel = \"test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n[permissions]\nallow = [\"bash(*)\"]\n",
            server.base_url(),
            data_directory.display(),
        ),
    )
    .expect("config should be written");

    let output = Command::new(env!("CARGO_BIN_EXE_agens"))
        .args(["chat", "run failing native command"])
        .current_dir(&project_root)
        .env("AGENS_CONFIG_HOME", &config_home)
        .env("OPENAI_API_KEY", "SENTINEL_OPENAI_API_KEY")
        .output()
        .expect("production binary should execute");
    let diagnostics = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(output.status.success(), "{diagnostics}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "native failure handled\n"
    );
    for secret in [
        "SENTINEL_OPENAI_API_KEY",
        "SENTINEL_NATIVE_OUTPUT",
        "SENTINEL_NATIVE_ARGUMENT",
    ] {
        assert!(!diagnostics.contains(secret), "diagnostics leaked {secret}");
    }
    let session = SessionStore::open(&data_directory)
        .expect("session store should open")
        .load_session_for_resume(1)
        .expect("completed session should be readable");
    assert!(
        format!("{session:?}").contains("SENTINEL_NATIVE_ARGUMENT"),
        "model-visible native arguments must remain resumable conversation content"
    );
    assert!(session.messages.iter().flat_map(|message| &message.parts).all(|part| {
        !matches!(part, MessagePart::ToolResult { content, .. } if content.contains("SENTINEL_NATIVE_OUTPUT"))
    }));
    assert_sqlite_has_no_sentinels(
        &data_directory.join("rust-sessions.db"),
        &["SENTINEL_OPENAI_API_KEY"],
    );
    assert_sqlite_contains_sentinels(
        &data_directory.join("rust-sessions.db"),
        &["SENTINEL_NATIVE_ARGUMENT"],
    );

    server.join();
}

#[test]
fn production_binary_stops_on_mcp_infrastructure_failures_without_continuation_or_persistence() {
    for (name, mode, timeout_ms) in [
        ("timeout", "call-sleep", 20),
        ("crash", "call-crash", 1_000),
        ("malformed protocol", "call-malformed", 1_000),
    ] {
        let temporary = TemporaryDirectory::new(&format!("production-mcp-{name}"));
        let project_root = temporary.path().join("project");
        let config_home = temporary.path().join("config");
        let data_directory = temporary.path().join("data");
        std::fs::create_dir_all(project_root.join(".git")).expect("project marker should exist");
        std::fs::create_dir_all(&config_home).expect("config directory should exist");

        let server = BoundedScriptedOpenAiMockServer::start(vec![ScriptedOpenAiResponse {
            required_body_fragments: vec!["files::first".to_owned()],
            response: native_tool_call_response("call_mcp_infrastructure", "files::first", r#"{}"#),
        }]);
        std::fs::write(
            config_home.join("config.toml"),
            format!(
                "[provider]\ntype = \"openai-api\"\nmodel = \"test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n[mcp.files]\ntransport = \"stdio\"\ncommand = \"{}\"\nargs = [{mode:?}]\ntimeout_ms = {timeout_ms}\n",
                server.base_url(),
                data_directory.display(),
                env!("CARGO_BIN_EXE_agens-cli-fake-mcp-child"),
            ),
        )
        .expect("config should be written");

        let output = Command::new(env!("CARGO_BIN_EXE_agens"))
            .args(["chat", "--dangerously-allow-all", "run broken MCP tool"])
            .current_dir(&project_root)
            .env("AGENS_CONFIG_HOME", &config_home)
            .env("OPENAI_API_KEY", "SENTINEL_OPENAI_API_KEY")
            .output()
            .expect("production binary should execute");

        assert_eq!(output.status.code(), Some(1), "{name}");
        assert_eq!(String::from_utf8_lossy(&output.stdout), "", "{name}");
        assert_no_saved_sessions(&project_root, &config_home);
        assert_sqlite_has_no_rows(&data_directory.join("rust-sessions.db"));

        server.join();
    }
}

#[test]
fn production_binary_static_deny_blocks_mcp_write_without_a_child_call() {
    let temporary = TemporaryDirectory::new("production-mcp-static-deny");
    let project_root = temporary.path().join("project");
    let config_home = temporary.path().join("config");
    let data_directory = temporary.path().join("data");
    let call_marker = temporary.path().join("mcp-child-call");
    std::fs::create_dir_all(project_root.join(".git")).expect("project marker should exist");
    std::fs::create_dir_all(&config_home).expect("config directory should exist");

    let server = ScriptedNativeOpenAiMockServer::start(vec![
        ScriptedOpenAiResponse {
            required_body_fragments: vec!["files::second".to_owned()],
            response: native_tool_call_response("call_mcp_deny", "files::second", r#"{}"#),
        },
        ScriptedOpenAiResponse {
            required_body_fragments: vec![
                "\"call_id\":\"call_mcp_deny\"".to_owned(),
                "\"output\":\"Tool execution failed\"".to_owned(),
            ],
            response: text_response("MCP denial handled"),
        },
    ]);
    std::fs::write(
        config_home.join("config.toml"),
        format!(
            "[provider]\ntype = \"openai-api\"\nmodel = \"test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n[permissions]\ndeny = [\"files_second(*)\"]\n\n[mcp.files]\ntransport = \"stdio\"\ncommand = \"{}\"\nargs = [\"success\"]\ntimeout_ms = 1000\n[mcp.files.env]\nFAKE_MCP_CALL_READY = \"{}\"\n",
            server.base_url(),
            data_directory.display(),
            env!("CARGO_BIN_EXE_agens-cli-fake-mcp-child"),
            call_marker.display(),
        ),
    )
    .expect("config should be written");

    let output = Command::new(env!("CARGO_BIN_EXE_agens"))
        .args(["chat", "deny configured MCP write"])
        .current_dir(&project_root)
        .env("AGENS_CONFIG_HOME", &config_home)
        .env("OPENAI_API_KEY", "SENTINEL_OPENAI_API_KEY")
        .output()
        .expect("production binary should execute");

    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {} stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "MCP denial handled\n"
    );
    assert!(!call_marker.exists(), "denied MCP tool must not execute");

    server.join();
}

#[test]
fn production_binary_enforces_mcp_permission_matrix_and_executes_allowed_calls_once() {
    for (name, tool, rule, arguments, flags, expected_exit, expected_output, executes, persists) in [
        (
            "read only static allow",
            "files::first",
            Some("allow = [\"files_first(*)\"]"),
            r#"{}"#,
            vec![],
            Some(0),
            "MCP permission handled\n",
            true,
            true,
        ),
        (
            "write non-TTY ask denial",
            "files::second",
            None,
            r#"{}"#,
            vec![],
            Some(0),
            "MCP permission handled\n",
            false,
            true,
        ),
        (
            "explicit deny",
            "files::second",
            Some("deny = [\"files_second(*)\"]"),
            r#"{}"#,
            vec![],
            Some(0),
            "MCP permission handled\n",
            false,
            true,
        ),
        (
            "bypass ordinary write",
            "files::second",
            None,
            r#"{}"#,
            vec!["--dangerously-allow-all"],
            Some(0),
            "MCP permission handled\n",
            true,
            true,
        ),
        (
            "bypass explicit deny",
            "files::second",
            Some("deny = [\"files_second(*)\"]"),
            r#"{}"#,
            vec!["--dangerously-allow-all"],
            Some(0),
            "MCP permission handled\n",
            false,
            true,
        ),
        (
            "chat mode write restriction",
            "files::second",
            None,
            r#"{}"#,
            vec!["--mode", "chat", "--dangerously-allow-all"],
            Some(0),
            "MCP permission handled\n",
            false,
            true,
        ),
    ] {
        let temporary = TemporaryDirectory::new(&format!("production-mcp-permission-{name}"));
        let project_root = temporary.path().join("project");
        let config_home = temporary.path().join("config");
        let data_directory = temporary.path().join("data");
        let call_marker = temporary.path().join("mcp-call-count");
        std::fs::create_dir_all(project_root.join(".git")).expect("project marker should exist");
        std::fs::create_dir_all(&config_home).expect("config directory should exist");

        let first_response = ScriptedOpenAiResponse {
            required_body_fragments: vec![tool.to_owned()],
            response: native_tool_call_response("call_mcp_permission", tool, arguments),
        };
        let server = BoundedScriptedOpenAiMockServer::start(if persists {
            vec![
                first_response,
                ScriptedOpenAiResponse {
                    required_body_fragments: vec![
                        "\"call_id\":\"call_mcp_permission\"".to_owned(),
                        if executes {
                            "tool succeeded".to_owned()
                        } else {
                            "\"output\":\"Tool execution failed\"".to_owned()
                        },
                    ],
                    response: text_response("MCP permission handled"),
                },
            ]
        } else {
            vec![first_response]
        });
        let permissions =
            rule.map_or_else(String::new, |rule| format!("\n[permissions]\n{rule}\n"));
        std::fs::write(
            config_home.join("config.toml"),
            format!(
                "[provider]\ntype = \"openai-api\"\nmodel = \"test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n{permissions}\n[mcp.files]\ntransport = \"stdio\"\ncommand = \"{}\"\nargs = [\"success\"]\ntimeout_ms = 1000\n[mcp.files.env]\nFAKE_MCP_CALL_READY = \"{}\"\n",
                server.base_url(),
                data_directory.display(),
                env!("CARGO_BIN_EXE_agens-cli-fake-mcp-child"),
                call_marker.display(),
            ),
        )
        .expect("config should be written");

        let mut command = Command::new(env!("CARGO_BIN_EXE_agens"));
        command.arg("chat");
        command.args(flags);
        let output = command
            .arg("exercise MCP permission policy")
            .current_dir(&project_root)
            .env("AGENS_CONFIG_HOME", &config_home)
            .env("OPENAI_API_KEY", "SENTINEL_OPENAI_API_KEY")
            .output()
            .expect("production binary should execute");

        assert_eq!(output.status.code(), expected_exit, "{name}");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            expected_output,
            "{name}"
        );
        if !persists {
            assert_eq!(
                String::from_utf8_lossy(&output.stderr),
                "error: permission: permission approval is required\n",
                "{name}"
            );
        }
        assert_eq!(call_marker.exists(), executes, "{name}");
        if executes {
            assert_eq!(
                std::fs::read_to_string(&call_marker).expect("MCP marker should be readable"),
                "1",
                "{name}"
            );
        }
        if persists {
            assert!(
                String::from_utf8_lossy(
                    &Command::new(env!("CARGO_BIN_EXE_agens"))
                        .args(["sessions", "list"])
                        .current_dir(&project_root)
                        .env("AGENS_CONFIG_HOME", &config_home)
                        .output()
                        .expect("sessions command should execute")
                        .stdout,
                )
                .ends_with("\tprimary\t1\n")
            );
        } else {
            assert_no_saved_sessions(&project_root, &config_home);
        }
        assert!(
            PermissionGrantStore::open(&data_directory)
                .expect("grant store should open")
                .grants_for_project(&project_root.display().to_string())
                .expect("project grants should load")
                .is_empty(),
            "{name}: temporary bypass must not persist a grant"
        );

        server.join();
    }
}

#[test]
fn production_binary_fails_closed_for_mcp_duplicate_replay_and_mismatched_call_items() {
    for (name, responses, expected_calls) in [
        (
            "duplicate provider call ID replay",
            vec![
                ScriptedOpenAiResponse {
                    required_body_fragments: vec!["files::first".to_owned()],
                    response: native_tool_call_response(
                        "call_mcp_integrity",
                        "files::first",
                        r#"{}"#,
                    ),
                },
                ScriptedOpenAiResponse {
                    required_body_fragments: vec![
                        "\"call_id\":\"call_mcp_integrity\"".to_owned(),
                        "tool succeeded".to_owned(),
                    ],
                    response: native_tool_call_response(
                        "call_mcp_integrity",
                        "files::second",
                        r#"{}"#,
                    ),
                },
            ],
            Some("1"),
        ),
        (
            "mismatched item arguments",
            vec![ScriptedOpenAiResponse {
                required_body_fragments: vec!["files::first".to_owned()],
                response: sse_response(&[
                    r#"{"type":"response.created","response":{"id":"response_mcp_mismatch"}}"#,
                    r#"{"type":"response.output_item.added","item":{"id":"item_mcp_expected","type":"function_call","call_id":"call_mcp_mismatch","name":"files::first","arguments":""}}"#,
                    r#"{"type":"response.function_call_arguments.done","item_id":"item_mcp_other","arguments":"{}"}"#,
                ]),
            }],
            None,
        ),
    ] {
        let temporary = TemporaryDirectory::new(&format!("production-mcp-integrity-{name}"));
        let project_root = temporary.path().join("project");
        let config_home = temporary.path().join("config");
        let data_directory = temporary.path().join("data");
        let call_marker = temporary.path().join("mcp-call-count");
        std::fs::create_dir_all(project_root.join(".git")).expect("project marker should exist");
        std::fs::create_dir_all(&config_home).expect("config directory should exist");
        let server = BoundedScriptedOpenAiMockServer::start(responses);
        std::fs::write(
            config_home.join("config.toml"),
            format!(
                "[provider]\ntype = \"openai-api\"\nmodel = \"test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n[mcp.files]\ntransport = \"stdio\"\ncommand = \"{}\"\nargs = [\"success\"]\ntimeout_ms = 1000\n[mcp.files.env]\nFAKE_MCP_CALL_READY = \"{}\"\n",
                server.base_url(),
                data_directory.display(),
                env!("CARGO_BIN_EXE_agens-cli-fake-mcp-child"),
                call_marker.display(),
            ),
        )
        .expect("config should be written");

        let output = Command::new(env!("CARGO_BIN_EXE_agens"))
            .args(["chat", "--dangerously-allow-all", "reject MCP replay"])
            .current_dir(&project_root)
            .env("AGENS_CONFIG_HOME", &config_home)
            .env("OPENAI_API_KEY", "SENTINEL_OPENAI_API_KEY")
            .output()
            .expect("production binary should execute");

        assert_eq!(output.status.code(), Some(1), "{name}");
        assert_eq!(String::from_utf8_lossy(&output.stdout), "", "{name}");
        assert_eq!(
            String::from_utf8_lossy(&output.stderr),
            "error: provider: provider request failed\n",
            "{name}"
        );
        assert_eq!(
            call_marker
                .exists()
                .then(|| std::fs::read_to_string(&call_marker)
                    .expect("MCP marker should be readable"))
                .as_deref(),
            expected_calls,
            "{name}"
        );
        assert_no_saved_sessions(&project_root, &config_home);

        server.join();
    }
}

struct LocalProvider {
    iterations: Vec<Result<Vec<MessagePart>, HeadlessTurnPortError>>,
}

impl TurnProvider for LocalProvider {
    fn next_parts(
        &mut self,
        _events: &[TurnEvent],
        _cancellation: &HeadlessTurnCancellation,
    ) -> impl std::future::Future<Output = Result<Vec<MessagePart>, HeadlessTurnPortError>> + Send
    {
        std::future::ready(self.iterations.remove(0))
    }
}

struct LocalPermissionGate {
    decisions: Vec<PermissionDecision>,
}

impl HeadlessPermissionGate for LocalPermissionGate {
    fn evaluate(
        &mut self,
        _call: &HeadlessToolCall,
        _cancellation: &HeadlessTurnCancellation,
    ) -> impl std::future::Future<Output = Result<PermissionDecision, HeadlessTurnPortError>> + Send
    {
        std::future::ready(Ok(self.decisions.remove(0)))
    }
}

struct LocalPermissionResolver {
    decisions: Vec<PermissionDecision>,
}

impl HeadlessPermissionResolver for LocalPermissionResolver {
    fn resolve(
        &mut self,
        _call: &HeadlessToolCall,
        _cancellation: &HeadlessTurnCancellation,
    ) -> impl std::future::Future<Output = Result<PermissionDecision, HeadlessTurnPortError>> + Send
    {
        std::future::ready(Ok(self.decisions.remove(0)))
    }
}

struct LocalToolDispatcher {
    outputs: Vec<Result<HeadlessToolOutput, HeadlessTurnPortError>>,
}

impl HeadlessToolDispatcher for LocalToolDispatcher {
    fn dispatch(
        &mut self,
        _call: HeadlessToolCall,
        _cancellation: &HeadlessTurnCancellation,
    ) -> impl std::future::Future<Output = Result<HeadlessToolOutput, HeadlessTurnPortError>> + Send
    {
        std::future::ready(self.outputs.remove(0))
    }
}

fn block_on_ready<T>(future: impl std::future::Future<Output = T>) -> T {
    let mut future = std::pin::pin!(future);
    let context = &mut std::task::Context::from_waker(std::task::Waker::noop());

    match future.as_mut().poll(context) {
        std::task::Poll::Ready(value) => value,
        std::task::Poll::Pending => panic!("local test ports must complete immediately"),
    }
}

fn write_chatgpt_credentials(config_home: &std::path::Path, access_token: &str) {
    std::fs::write(
        config_home.join("auth.json"),
        format!(
            r#"{{"openai-chatgpt":{{"access_token":{access_token:?},"refresh_token":"SENTINEL_CHATGPT_REFRESH","account_id":"account_123","expires_at":"2030-01-01T00:00:00Z"}}}}"#
        ),
    )
    .expect("ChatGPT credentials should be written");
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

struct OpenAiMockServer {
    address: std::net::SocketAddr,
    worker: thread::JoinHandle<()>,
}

struct ScriptedOpenAiResponse {
    required_body_fragments: Vec<String>,
    response: String,
}

struct ScriptedNativeOpenAiMockServer {
    address: std::net::SocketAddr,
    worker: thread::JoinHandle<()>,
}

struct BoundedScriptedOpenAiMockServer {
    address: std::net::SocketAddr,
    worker: thread::JoinHandle<()>,
}

impl ScriptedNativeOpenAiMockServer {
    fn start(responses: Vec<ScriptedOpenAiResponse>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("mock server should bind");
        let address = listener
            .local_addr()
            .expect("mock server should have an address");
        let worker = thread::spawn(move || {
            for scripted in responses {
                let (mut stream, _) = listener
                    .accept()
                    .expect("mock server should accept a request");
                let body = read_openai_request_body(&stream);
                for fragment in scripted.required_body_fragments {
                    if fragment == "@all-tools-non-strict" {
                        let payload: serde_json::Value = serde_json::from_str(&body)
                            .expect("production provider payload should be JSON");
                        let tools = payload["tools"]
                            .as_array()
                            .expect("production provider should advertise tools");
                        assert!(
                            !tools.is_empty(),
                            "production provider should advertise tools"
                        );
                        for tool in tools {
                            assert_eq!(tool["type"], "function");
                            assert_eq!(tool["strict"], false, "tool was strict: {tool}");
                            assert!(tool["name"].as_str().is_some_and(|name| !name.is_empty()));
                            assert!(
                                tool["description"]
                                    .as_str()
                                    .is_some_and(|description| !description.is_empty())
                            );
                            assert_eq!(tool["parameters"]["type"], "object");
                        }
                        continue;
                    }
                    if let Some(forbidden) = fragment.strip_prefix('!') {
                        assert!(
                            !body.contains(forbidden),
                            "request body leaked {forbidden:?}: {body}"
                        );
                        continue;
                    }
                    let visible = model_visible_fragment(&fragment);
                    assert!(
                        body.contains(&visible),
                        "request body should contain {visible:?}: {body}"
                    );
                }
                stream
                    .write_all(scripted.response.as_bytes())
                    .expect("scripted response should be written");
            }
        });

        Self { address, worker }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }

    fn join(self) {
        self.worker.join().expect("mock server should finish");
    }
}

impl BoundedScriptedOpenAiMockServer {
    fn start(responses: Vec<ScriptedOpenAiResponse>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("mock server should bind");
        let address = listener
            .local_addr()
            .expect("mock server should have an address");
        let worker = thread::spawn(move || {
            for scripted in responses {
                let (mut stream, _) = listener
                    .accept()
                    .expect("mock server should accept a request");
                let body = read_openai_request_body(&stream);
                for fragment in scripted.required_body_fragments {
                    if let Some(forbidden) = fragment.strip_prefix('!') {
                        assert!(
                            !body.contains(forbidden),
                            "request body leaked {forbidden:?}: {body}"
                        );
                        continue;
                    }
                    let visible = model_visible_fragment(&fragment);
                    assert!(
                        body.contains(&visible),
                        "request body should contain {visible:?}: {body}"
                    );
                }
                stream
                    .write_all(scripted.response.as_bytes())
                    .expect("scripted response should be written");
            }

            listener
                .set_nonblocking(true)
                .expect("mock server should enable bounded probe mode");
            let deadline = std::time::Instant::now() + Duration::from_millis(250);
            while std::time::Instant::now() < deadline {
                match listener.accept() {
                    Ok((_stream, _)) => {
                        panic!("unexpected provider continuation request");
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("mock server probe failed: {error}"),
                }
            }
        });

        Self { address, worker }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }

    fn join(self) {
        self.worker.join().expect("mock server should finish");
    }
}

fn model_visible_fragment(fragment: &str) -> String {
    if let Some(name) = fragment.strip_prefix("native::") {
        return name.to_owned();
    }
    if let Some((server, tool)) = fragment.split_once("::") {
        return format!("{server}_{tool}");
    }
    fragment.to_owned()
}

impl OpenAiMockServer {
    fn start_with_api_key(api_key: &str) -> Self {
        let expected_authorization = format!("authorization: Bearer {api_key}\r\n");
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("mock server should bind");
        let address = listener
            .local_addr()
            .expect("mock server should have an address");
        let worker = thread::spawn(move || {
            let (stream, _) = listener
                .accept()
                .expect("mock server should accept a request");
            let mut reader = BufReader::new(stream.try_clone().expect("stream should clone"));
            let mut request = String::new();
            reader
                .read_line(&mut request)
                .expect("request line should be readable");
            assert_eq!(request, "POST /responses HTTP/1.1\r\n");

            let mut authorization = String::new();
            loop {
                let mut header = String::new();
                reader
                    .read_line(&mut header)
                    .expect("header should be readable");
                if header == "\r\n" {
                    break;
                }
                if header.to_ascii_lowercase().starts_with("authorization:") {
                    authorization = header;
                }
            }
            assert_eq!(authorization, expected_authorization);

            let mut stream = stream;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello from OpenAI\"}\n\ndata: {\"type\":\"response.completed\"}\n\n")
                .expect("mock response should be written");
        });

        Self { address, worker }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }

    fn join(self) {
        self.worker.join().expect("mock server should finish");
    }
}

struct StalledOpenAiMockServer {
    address: std::net::SocketAddr,
    observed_request: mpsc::Receiver<()>,
    worker: thread::JoinHandle<()>,
}

struct TaskStalledOpenAiMockServer {
    address: std::net::SocketAddr,
    observed_child_request: mpsc::Receiver<()>,
    worker: thread::JoinHandle<()>,
}

impl StalledOpenAiMockServer {
    fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("mock server should bind");
        let address = listener
            .local_addr()
            .expect("mock server should have an address");
        let (observed_sender, observed_request) = mpsc::channel();
        let worker = thread::spawn(move || {
            let (stream, _) = listener
                .accept()
                .expect("mock server should accept a request");
            read_openai_request(&stream);
            observed_sender
                .send(())
                .expect("test should receive the request observation");
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .expect("client-close timeout should be configured");
            let mut byte = [0_u8; 1];
            let _ = std::io::Read::read(
                &mut stream.try_clone().expect("stream should clone"),
                &mut byte,
            );
        });

        Self {
            address,
            observed_request,
            worker,
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }

    fn wait_for_request(&mut self) {
        self.observed_request
            .recv_timeout(Duration::from_secs(1))
            .expect("production request should reach the local server");
    }

    fn join(self) {
        self.worker.join().expect("mock server should finish");
    }
}

impl TaskStalledOpenAiMockServer {
    fn start(stall_timeout: Duration) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("mock server should bind");
        let address = listener
            .local_addr()
            .expect("mock server should have an address");
        let (observed_sender, observed_child_request) = mpsc::channel();
        let worker = thread::spawn(move || {
            let (mut parent, _) = listener
                .accept()
                .expect("mock server should accept the parent request");
            let parent_body = read_openai_request_body(&parent);
            assert!(parent_body.contains("parent task"));
            parent
                .write_all(
                    native_tool_call_response(
                        "task-cancel",
                        "task",
                        r#"{"agent":"reviewer","description":"child cancellation request"}"#,
                    )
                    .as_bytes(),
                )
                .expect("parent response should be written");
            drop(parent);

            let (child, _) = listener
                .accept()
                .expect("mock server should accept the child request");
            let child_body = read_openai_request_body(&child);
            observed_sender
                .send(())
                .expect("test should receive the child request observation");
            for forbidden in [
                "parent task cancellation",
                "task",
                "write",
                "bash",
                "webfetch",
                "mcp",
            ] {
                assert!(
                    !child_body.contains(forbidden),
                    "child request leaked {forbidden:?}: {child_body}"
                );
            }
            child
                .set_read_timeout(Some(stall_timeout))
                .expect("child close timeout should be configured");
            let mut byte = [0_u8; 1];
            let _ = std::io::Read::read(
                &mut child.try_clone().expect("child stream should clone"),
                &mut byte,
            );

            listener
                .set_nonblocking(true)
                .expect("mock server should enable continuation probe");
            let deadline = std::time::Instant::now() + Duration::from_millis(250);
            while std::time::Instant::now() < deadline {
                match listener.accept() {
                    Ok(_) => panic!("parent continued after child cancellation"),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("mock server probe failed: {error}"),
                }
            }
        });

        Self {
            address,
            observed_child_request,
            worker,
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }

    fn wait_for_child_request(&mut self) {
        self.observed_child_request
            .recv_timeout(Duration::from_secs(1))
            .expect("production child request should reach the local server");
    }

    fn join(self) {
        self.worker.join().expect("mock server should finish");
    }
}

struct ErrorOpenAiMockServer {
    address: std::net::SocketAddr,
    worker: thread::JoinHandle<()>,
}

impl ErrorOpenAiMockServer {
    fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("mock server should bind");
        let address = listener
            .local_addr()
            .expect("mock server should have an address");
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener
                .accept()
                .expect("mock server should accept a request");
            read_openai_request(&stream);
            stream
                .write_all(
                    b"HTTP/1.1 500 Internal Server Error\r\nX-Remote-Secret: SENTINEL_REMOTE_ERROR_HEADER\r\nContent-Length: 26\r\nConnection: close\r\n\r\nSENTINEL_REMOTE_ERROR_BODY",
                )
                .expect("error response should be written");
        });

        Self { address, worker }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }

    fn join(self) {
        self.worker.join().expect("mock server should finish");
    }
}

fn read_openai_request(stream: &std::net::TcpStream) {
    let mut reader = BufReader::new(stream.try_clone().expect("stream should clone"));
    let mut request = String::new();
    reader
        .read_line(&mut request)
        .expect("request line should be readable");
    assert!(
        request == "POST /responses HTTP/1.1\r\n"
            || request == "POST /codex/responses HTTP/1.1\r\n"
    );

    loop {
        let mut header = String::new();
        reader
            .read_line(&mut header)
            .expect("header should be readable");
        if header == "\r\n" {
            return;
        }
    }
}

fn read_openai_request_body(stream: &std::net::TcpStream) -> String {
    let mut reader = BufReader::new(stream.try_clone().expect("stream should clone"));
    let mut request = String::new();
    reader
        .read_line(&mut request)
        .expect("request line should be readable");
    assert!(
        request == "POST /responses HTTP/1.1\r\n"
            || request == "POST /codex/responses HTTP/1.1\r\n"
    );

    let mut content_length = None;
    loop {
        let mut header = String::new();
        reader
            .read_line(&mut header)
            .expect("header should be readable");
        if header == "\r\n" {
            break;
        }
        if let Some(value) = header.strip_prefix("content-length: ") {
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
    String::from_utf8(body).expect("request body should be UTF-8")
}

fn native_tool_call_response(call_id: &str, name: &str, arguments: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {{\"type\":\"response.output_item.added\",\"item\":{{\"id\":\"item_{call_id}\",\"type\":\"function_call\",\"call_id\":\"{call_id}\",\"name\":\"{name}\",\"arguments\":\"\"}}}}\n\ndata: {{\"type\":\"response.function_call_arguments.done\",\"item_id\":\"item_{call_id}\",\"arguments\":{arguments:?}}}\n\ndata: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"response_{call_id}\"}}}}\n\n"
    )
}

fn text_response(text: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {{\"type\":\"response.output_text.delta\",\"delta\":{text:?}}}\n\ndata: {{\"type\":\"response.completed\"}}\n\n"
    )
}

fn sse_response(events: &[&str]) -> String {
    let body = events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();
    format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}")
}

fn wait_for_path(path: &std::path::Path, timeout: Duration) {
    let deadline = std::time::Instant::now() + timeout;
    while !path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(5));
    }
}

fn wait_for_child_output(
    mut child: std::process::Child,
    timeout: Duration,
) -> std::process::Output {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if child
            .try_wait()
            .expect("production binary status should remain observable")
            .is_some()
        {
            return child
                .wait_with_output()
                .expect("production binary output should remain readable");
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for production binary cancellation"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(unix)]
fn wait_for_process_exit(process_id: u32, timeout: Duration) {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let status = Command::new("kill")
            .args(["-0", &process_id.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("process probe should execute");
        if !status.success() {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "process {process_id} survived cancellation"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn assert_no_saved_sessions(project_root: &std::path::Path, config_home: &std::path::Path) {
    let sessions = Command::new(env!("CARGO_BIN_EXE_agens"))
        .args(["sessions", "list"])
        .current_dir(project_root)
        .env("AGENS_CONFIG_HOME", config_home)
        .output()
        .expect("sessions command should execute");

    assert!(sessions.status.success());
    assert_eq!(
        String::from_utf8_lossy(&sessions.stdout),
        "No saved sessions.\n"
    );
}

fn assert_sqlite_has_no_sentinels(database: &std::path::Path, sentinels: &[&str]) {
    for (location, value) in sqlite_text_values(database) {
        for sentinel in sentinels {
            assert!(!value.contains(sentinel), "{location} leaked {sentinel}");
        }
    }
}

fn assert_output_and_store_exclude_sentinels(
    output: &std::process::Output,
    database: &std::path::Path,
    sentinels: &[&str],
) {
    let visible_output = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    for sentinel in sentinels {
        assert!(
            !visible_output.contains(sentinel),
            "output leaked {sentinel}"
        );
    }

    assert_sqlite_has_no_sentinels(database, sentinels);
}

fn assert_sqlite_contains_sentinels(database: &std::path::Path, sentinels: &[&str]) {
    let values = sqlite_text_values(database);

    for sentinel in sentinels {
        assert!(
            values.iter().any(|(_, value)| value.contains(sentinel)),
            "persisted SQLite content omitted {sentinel}"
        );
    }
}

fn sqlite_text_values(database: &std::path::Path) -> Vec<(String, String)> {
    let connection = rusqlite::Connection::open(database).expect("session database should open");
    let mut tables = connection
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
        .expect("tables should be queryable");
    let tables = tables
        .query_map([], |row| row.get::<_, String>(0))
        .expect("table query should run")
        .collect::<Result<Vec<_>, _>>()
        .expect("table names should be readable");

    let mut sqlite_values = Vec::new();

    for table in tables {
        let quoted_table = table.replace('"', "\"\"");
        let mut columns = connection
            .prepare(&format!("PRAGMA table_info(\"{quoted_table}\")"))
            .expect("table metadata should be queryable");
        let columns = columns
            .query_map([], |row| {
                Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?))
            })
            .expect("column query should run")
            .collect::<Result<Vec<_>, _>>()
            .expect("column metadata should be readable");

        for (column, declared_type) in columns {
            let declared_type = declared_type.to_ascii_uppercase();
            if !declared_type.contains("TEXT") && !declared_type.contains("BLOB") {
                continue;
            }
            let quoted_column = column.replace('"', "\"\"");
            let mut values = connection
                .prepare(&format!(
                    "SELECT CAST(\"{quoted_column}\" AS TEXT) FROM \"{quoted_table}\""
                ))
                .expect("serialized values should be queryable");
            let values = values
                .query_map([], |row| row.get::<_, Option<String>>(0))
                .expect("serialized value query should run")
                .collect::<Result<Vec<_>, _>>()
                .expect("serialized values should be readable");

            for value in values.into_iter().flatten() {
                sqlite_values.push((format!("{table}.{column}"), value));
            }
        }
    }

    sqlite_values
}

fn assert_sqlite_has_no_rows(database: &std::path::Path) {
    assert!(database.exists(), "session database should exist");

    let connection = rusqlite::Connection::open(database).expect("session database should open");
    let mut tables = connection
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'")
        .expect("tables should be queryable");
    let tables = tables
        .query_map([], |row| row.get::<_, String>(0))
        .expect("table query should run")
        .collect::<Result<Vec<_>, _>>()
        .expect("table names should be readable");

    for table in tables {
        let quoted_table = table.replace('"', "\"\"");
        let row_count = connection
            .query_row(
                &format!("SELECT COUNT(*) FROM \"{quoted_table}\""),
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("table row count should be readable");
        assert_eq!(row_count, 0, "{table} should have no persisted rows");
    }
}
