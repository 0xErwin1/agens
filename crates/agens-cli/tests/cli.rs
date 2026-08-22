use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use agens::{
    CliDependencies, ExitStatus, ModelSelection, ModelSource, bootstrap, execute, execute_os,
    execute_with_cancellation,
};
use agens_fixtures::{NetworkTripwire, Script, ScriptedDialect, ScriptedProvider, ScriptedTurn};

use agens_core::{
    CompletedSessionTurn, HeadlessPermissionGate, HeadlessPermissionResolver, HeadlessToolCall,
    HeadlessToolDispatcher, HeadlessToolOutput, HeadlessTurnCancellation, HeadlessTurnPortError,
    Message, MessagePart, PermissionDecision, ReasoningEffort, Role, SessionMessage,
    SessionMetadata, TurnEvent, TurnProvider, run_headless_turn,
};
use agens_store::{PermissionGrantStore, SessionStore};
use agens_tools::McpTransport;

fn assert_diagnostic_error(actual: &[u8], expected_without_reference: &str) {
    assert_diagnostic_error_text(&String::from_utf8_lossy(actual), expected_without_reference);
}

fn assert_diagnostic_error_text(actual: &str, expected_without_reference: &str) {
    assert_diagnostic_error_with_detail_text(actual, expected_without_reference, "");
}

/// Like [`assert_diagnostic_error`], but for a diagnostic that also carries failure detail on
/// the lines after the `[ref: ...]` envelope (`CliError::with_failure_detail`). `expected_detail`
/// is matched exactly against everything following the reference's closing bracket and its
/// newline, so this stays as strict as the no-detail case once the detail text is accounted for.
fn assert_diagnostic_error_with_detail(
    actual: &[u8],
    expected_without_reference: &str,
    expected_detail: &str,
) {
    assert_diagnostic_error_with_detail_text(
        &String::from_utf8_lossy(actual),
        expected_without_reference,
        expected_detail,
    );
}

fn assert_diagnostic_error_with_detail_text(
    actual: &str,
    expected_without_reference: &str,
    expected_detail: &str,
) {
    let prefix = expected_without_reference
        .strip_suffix('\n')
        .expect("expected diagnostic should end with a newline");
    let after_prefix = actual
        .strip_prefix(prefix)
        .and_then(|suffix| suffix.strip_prefix(" [ref: "))
        .expect("diagnostic should append a reference");
    let closing_bracket = after_prefix
        .find(']')
        .expect("reference should be closed with ']'");
    let reference = &after_prefix[..closing_bracket];
    assert_eq!(reference.len(), 8);
    assert!(
        reference
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    );
    let remainder = after_prefix[closing_bracket + 1..]
        .strip_prefix('\n')
        .expect("reference should be followed by a newline");
    assert_eq!(remainder, expected_detail);
}

#[test]
fn production_binary_launches_are_centralized_through_isolated_helper() {
    let source = include_str!("cli.rs");
    let direct_launch = ["Command::new", "(env!(\"CARGO_BIN_EXE_agens\"))"].concat();

    assert_eq!(source.matches(&direct_launch).count(), 1);
}

#[test]
fn isolated_commands_use_distinct_temporary_environment_roots() {
    let temporary = std::sync::Arc::new(TemporaryDirectory::new("isolated-command-roots"));
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let first_temporary = std::sync::Arc::clone(&temporary);
    let first_barrier = std::sync::Arc::clone(&barrier);
    let first = thread::spawn(move || {
        first_barrier.wait();
        isolated_agens_command(&first_temporary)
    });
    let second_temporary = std::sync::Arc::clone(&temporary);
    let second_barrier = std::sync::Arc::clone(&barrier);
    let second = thread::spawn(move || {
        second_barrier.wait();
        isolated_agens_command(&second_temporary)
    });
    let first = first.join().expect("first command should be constructed");
    let second = second.join().expect("second command should be constructed");

    for variable in [
        "HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "AGENS_CONFIG_HOME",
    ] {
        let path_for = |command: &IsolatedAgensCommand| {
            command
                .command()
                .get_envs()
                .find_map(|(name, value)| {
                    (name == variable)
                        .then(|| value.map(PathBuf::from))
                        .flatten()
                })
                .unwrap_or_else(|| panic!("{variable} should be isolated"))
        };
        let first_path = path_for(&first);
        let second_path = path_for(&second);

        assert_ne!(first_path, second_path, "{variable} roots must be unique");
        assert!(first_path.starts_with(temporary.path()));
        assert!(second_path.starts_with(temporary.path()));
        if let Some(real_path) = std::env::var_os(variable).map(PathBuf::from) {
            assert_ne!(first_path, real_path, "{variable} inherited the real path");
            assert_ne!(second_path, real_path, "{variable} inherited the real path");
        }
    }
}

#[test]
fn production_command_output_wait_is_bounded() {
    const CHILD_MARKER: &str = "AGENS_CLI_BOUNDED_OUTPUT_CHILD";

    if std::env::var_os(CHILD_MARKER).as_deref() == Some(OsStr::new("sleep")) {
        thread::sleep(Duration::from_secs(1));
        return;
    }

    let result = Command::new(std::env::current_exe().expect("test executable should resolve"))
        .args(["--exact", "production_command_output_wait_is_bounded"])
        .env(CHILD_MARKER, "sleep")
        .bounded_output(Duration::from_millis(20));

    let error = result.expect_err("the bounded wait should terminate the sleeping child");
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
}

#[test]
fn production_command_output_drains_more_than_pipe_capacity() {
    const CHILD_MARKER: &str = "AGENS_CLI_LARGE_OUTPUT_CHILD";
    const OUTPUT_BYTES: usize = 2 * 1024 * 1024;

    if std::env::var_os(CHILD_MARKER).is_some() {
        std::io::stdout()
            .write_all(&vec![0xa5; OUTPUT_BYTES])
            .unwrap();
        std::io::stderr()
            .write_all(&vec![0xa6; OUTPUT_BYTES])
            .unwrap();
        return;
    }

    let output = Command::new(std::env::current_exe().expect("test executable should resolve"))
        .args([
            "--exact",
            "production_command_output_drains_more_than_pipe_capacity",
        ])
        .env(CHILD_MARKER, "1")
        .bounded_output(Duration::from_secs(2))
        .expect("large output should be drained while the child runs");

    assert!(output.status.success());
    assert_eq!(
        output.stdout.iter().filter(|byte| **byte == 0xa5).count(),
        OUTPUT_BYTES
    );
    assert_eq!(
        output.stderr.iter().filter(|byte| **byte == 0xa6).count(),
        OUTPUT_BYTES
    );
}

#[test]
#[cfg(unix)]
/// The intermediate child intentionally exits without waiting so the outer harness must own and
/// terminate the descendant process that retains its output pipes.
#[allow(clippy::zombie_processes)]
fn production_command_timeout_terminates_descendants_holding_output_pipes() {
    const CHILD_MARKER: &str = "AGENS_CLI_DESCENDANT_OUTPUT_CHILD";

    match std::env::var(CHILD_MARKER).as_deref() {
        Ok("child") => {
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "production_command_timeout_terminates_descendants_holding_output_pipes",
                ])
                .env(CHILD_MARKER, "descendant")
                .spawn()
                .unwrap();
            return;
        }
        Ok("descendant") => {
            thread::sleep(Duration::from_secs(3));
            return;
        }
        _ => {}
    }

    let started = Instant::now();
    let result = Command::new(std::env::current_exe().expect("test executable should resolve"))
        .args([
            "--exact",
            "production_command_timeout_terminates_descendants_holding_output_pipes",
        ])
        .env(CHILD_MARKER, "child")
        .bounded_output(Duration::from_millis(100));

    let error = result.expect_err("a descendant retaining pipes must not defeat the timeout");
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(2));
}

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
fn global_mcp_command_and_environment_fields_expand() {
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
                "[mcp.files]\ntransport = \"stdio\"\ncommand = \"$(printf /bin/sh)\"\nargs = [\"-c\", {script:?}, \"ignored\", \"$(printf configured-argument)\"]\n[mcp.files.env]\nMCP_SENTINEL = \"$(printf 'configured-environment\\\\n')\"\n"
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
    std::fs::create_dir_all(project_root.join(".git")).expect("repository marker should exist");
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
fn a_retired_provider_type_is_rejected_by_name_with_its_replacement() {
    let temporary = TemporaryDirectory::new("retired-provider-type");
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

    let Err(error) = bootstrap(&dependencies) else {
        panic!("a retired setting must not bootstrap");
    };
    let message = error.to_string();

    assert!(message.contains("provider.type"), "{message}");
    assert!(message.contains("provider/model"), "{message}");
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
        "ID\tNAME\tCONTEXT\tPRICE\nopenai-api/gpt-4.1\tGPT-4.1\t1047576\t$2.00/$8.00\nopenai-api/gpt-4.1-mini\tGPT-4.1 mini\t1047576\t$0.40/$1.60\nopenai-api/gpt-4.1-nano\tGPT-4.1 nano\t1047576\t$0.10/$0.40\nopenai-api/gpt-4o\tGPT-4o\t128000\t$2.50/$10.00\nopenai-api/gpt-4o-mini\tGPT-4o mini\t128000\t$0.15/$0.60\nopenai-api/gpt-5.5\tGPT-5.5\t272000\t-/-\nopenai-api/gpt-5.6\tGPT-5.6 (Sol alias)\t1050000\t-/-\nopenai-api/gpt-5.6-luna\tGPT-5.6 Luna\t1050000\t-/-\nopenai-api/gpt-5.6-sol\tGPT-5.6 Sol\t1050000\t-/-\nopenai-api/gpt-5.6-terra\tGPT-5.6 Terra\t1050000\t-/-\nopenai-api/o3\to3\t200000\t$2.00/$8.00\nopenai-api/o4-mini\to4-mini\t200000\t$1.10/$4.40\nopenai-chatgpt/gpt-5.3-codex-spark\tGPT-5.3 Codex Spark\t128000\t-/-\nopenai-chatgpt/gpt-5.4\tGPT-5.4\t272000\t-/-\nopenai-chatgpt/gpt-5.4-mini\tGPT-5.4 mini\t272000\t-/-\nopenai-chatgpt/gpt-5.5\tGPT-5.5\t272000\t-/-\nopenai-chatgpt/gpt-5.6\tGPT-5.6 (Sol alias)\t1050000\t-/-\nopenai-chatgpt/gpt-5.6-luna\tGPT-5.6 Luna\t1050000\t-/-\nopenai-chatgpt/gpt-5.6-sol\tGPT-5.6 Sol\t1050000\t-/-\nopenai-chatgpt/gpt-5.6-terra\tGPT-5.6 Terra\t1050000\t-/-\nmoonshotai/kimi-k2.6\t-\t262144\t-/-\nmoonshotai/kimi-k2.7-code\t-\t262144\t-/-\nmoonshotai/kimi-k2.7-code-highspeed\t-\t262144\t-/-\nmoonshotai/kimi-k3\t-\t1048576\t-/-\n"
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

    let result = execute(["auth", "login", "chatgpt"], &dependencies);

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
    assert!(
        root_help
            .stdout
            .contains("Usage: agens [OPTIONS] [COMMAND]\n")
    );
    assert_eq!(chat_help.status, ExitStatus::Success);
    assert_eq!(
        chat_help.stdout,
        "run a headless agent turn\n\nUsage: agens chat [OPTIONS] [PROMPT]...\n\nArguments:\n  [PROMPT]...  \n\nOptions:\n      --model <MODEL>                    \n      --system <SYSTEM>                  \n      --max-iterations <MAX_ITERATIONS>  \n      --mode <chat|edit>                 \n      --dangerously-allow-all            \n      --attach <PATH>                    \n  -h, --help                             Print help\n"
    );
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

    let browser = execute(["auth", "login", "chatgpt"], &dependencies);
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
        let result =
            execute_with_cancellation(["auth", "login", "chatgpt"], &dependencies, &cancellation);
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

    let login = isolated_agens_command(&temporary)
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
    let status = isolated_agens_command(&temporary)
        .args(["auth", "status", "openai-api"])
        .env("AGENS_CONFIG_HOME", &config_home)
        .output()
        .expect("selected provider status should execute");
    let logout = isolated_agens_command(&temporary)
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

    let mut login = isolated_agens_command(&temporary)
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
        let mut command = isolated_agens_command(&temporary);
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
    assert!(data_directory.join("agens.db").is_file());
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
        provider_id: None,
        model_id: None,
        reasoning_effort: None,
        created_at: 10,
        updated_at: 20,
        completed_turn_count: 0,
        resumable: false,
        parent_session_id: None,
        fork_message_count: None,
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

/// Round-4 verification found `sessions rm -- <id>` unpinned for two
/// members of the ratified W-C family (`--` is only refused as the sole
/// root argument; everywhere else clap's "end of options" handling consumes
/// it and the shape runs for real): a positive identifier and, more
/// pointedly, a negative one that could never reach `rm` any other way
/// (`sessions rm -1` alone is rejected by clap as an unrecognized argument,
/// per ratified W-B). `sessions rm -- 1` genuinely deletes an existing
/// session. `sessions rm -- -1` cannot ever delete a real row: session ids
/// are validated `> 0` at persistence time (`SessionMetadata::validate`),
/// so no session can ever carry a negative id, and `delete_session` is
/// idempotent (it does not error when nothing matched, exactly like
/// removing an already-removed id twice). The exit code for this shape
/// still moved from 2 to 0, which is what this pins: a harmless no-op that
/// reports success, not an actual deletion of anything that could exist.
#[test]
fn sessions_rm_after_a_double_dash_deletes_a_positive_identifier_and_no_ops_on_a_negative_one() {
    let temporary = TemporaryDirectory::new("sessions-rm-double-dash");
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
        id: 1,
        project: "project".into(),
        title: "conversation".into(),
        active_agent: "primary".into(),
        provider_id: None,
        model_id: None,
        reasoning_effort: None,
        created_at: 10,
        updated_at: 20,
        completed_turn_count: 0,
        resumable: false,
        parent_session_id: None,
        fork_message_count: None,
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

    let remove_positive = execute(["sessions", "rm", "--", "1"], &dependencies);
    let remove_negative = execute(["sessions", "rm", "--", "-1"], &dependencies);
    let missing_positive = execute(["sessions", "show", "1"], &dependencies);

    assert_eq!(remove_positive.status, ExitStatus::Success);
    assert_eq!(remove_positive.stdout, "Removed session 1.\n");
    assert_eq!(remove_negative.status, ExitStatus::Success);
    assert_eq!(remove_negative.stdout, "Removed session -1.\n");
    assert_eq!(missing_positive.status, ExitStatus::Failure);
    assert_eq!(
        missing_positive.stderr,
        "error: store: saved session is unavailable\n"
    );
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

    // Each nested subcommand renders its OWN help: clap walks to the
    // deepest matched subcommand for any valid shape, so `config doctor
    // --help` renders `doctor`'s help, not `config`'s. `models` has no
    // nested subcommands of its own, so it renders its own top-level help
    // either way. Each rendering therefore has a known, exact expected text.
    const CONFIG_DOCTOR_HELP: &str = "report the effective configuration and where each setting came from\n\nUsage: agens config doctor\n\nOptions:\n  -h, --help  Print help\n";
    const AUTH_STATUS_HELP: &str = "report authentication status for ChatGPT or an API-key provider\n\nUsage: agens auth status [PROVIDER]\n\nArguments:\n  [PROVIDER]  \n\nOptions:\n  -h, --help  Print help\n";
    const AUTH_LOGIN_HELP: &str = "log in to ChatGPT or an API-key provider\n\nUsage: agens auth login [OPTIONS] [COMMAND]\n\nCommands:\n  chatgpt  log in to a ChatGPT subscription through OAuth\n  api-key  log in with an API key instead of ChatGPT\n\nOptions:\n      --device-auth  Use the device-code flow instead of opening a browser\n  -h, --help         Print help\n";
    const AUTH_LOGOUT_HELP: &str = "remove stored credentials for a provider\n\nUsage: agens auth logout <PROVIDER>\n\nArguments:\n  <PROVIDER>  \n\nOptions:\n  -h, --help  Print help\n";
    const MODELS_HELP: &str =
        "list provider models\n\nUsage: agens models\n\nOptions:\n  -h, --help  Print help\n";
    const SESSIONS_LIST_HELP: &str =
        "list saved sessions\n\nUsage: agens sessions list\n\nOptions:\n  -h, --help  Print help\n";
    const SESSIONS_SHOW_HELP: &str = "show a saved session's details\n\nUsage: agens sessions show <IDENTIFIER>\n\nArguments:\n  <IDENTIFIER>  \n\nOptions:\n  -h, --help  Print help\n";
    const SESSIONS_RM_HELP: &str = "remove a saved session\n\nUsage: agens sessions rm <IDENTIFIER>\n\nArguments:\n  <IDENTIFIER>  \n\nOptions:\n  -h, --help  Print help\n";

    for (arguments, expected) in [
        (
            ["config", "doctor", "--help"].as_slice(),
            CONFIG_DOCTOR_HELP,
        ),
        (["auth", "status", "--help"].as_slice(), AUTH_STATUS_HELP),
        (["auth", "login", "--help"].as_slice(), AUTH_LOGIN_HELP),
        (["auth", "logout", "--help"].as_slice(), AUTH_LOGOUT_HELP),
        (["models", "--help"].as_slice(), MODELS_HELP),
        (
            ["sessions", "list", "--help"].as_slice(),
            SESSIONS_LIST_HELP,
        ),
        (
            ["sessions", "show", "--help"].as_slice(),
            SESSIONS_SHOW_HELP,
        ),
        (["sessions", "rm", "--help"].as_slice(), SESSIONS_RM_HELP),
    ] {
        let result = execute(arguments, &dependencies);

        assert_eq!(result.status, ExitStatus::Success, "{arguments:?}");
        assert_eq!(result.stdout, expected, "{arguments:?}");
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
fn tui_model_selector_applies_verified_api_catalog_and_preserves_state_on_refusal() {
    let mut selector = ModelSelection::new("gpt-4.1");

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
            "gpt-5.6",
            "gpt-5.6-luna",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
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
    let mut api = ModelSelection::for_source("gpt-5.5", ModelSource::OpenAiApi);
    let mut subscription = ModelSelection::for_source("gpt-5.5", ModelSource::ChatGptSubscription);

    assert!(
        api.model_values()
            .expect("API model registry should be available")
            .contains(&"gpt-4o".to_owned())
    );
    assert_eq!(
        subscription
            .model_values()
            .expect("subscription model registry should be available"),
        [
            "gpt-5.3-codex-spark",
            "gpt-5.4",
            "gpt-5.4-mini",
            "gpt-5.5",
            "gpt-5.6",
            "gpt-5.6-luna",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
        ]
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
    let mut selector = ModelSelection::for_source("gpt-5.5", ModelSource::OpenAiApi);

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

    let mut subscription = ModelSelection::for_source("gpt-5.5", ModelSource::ChatGptSubscription);
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

    let non_reasoning = ModelSelection::new("gpt-4.1");
    assert_eq!(non_reasoning.reasoning_effort_values(), ["default"]);

    for source in [ModelSource::OpenAiApi, ModelSource::ChatGptSubscription] {
        let mut gpt_5_6 = ModelSelection::for_source("gpt-5.6", source);
        assert_eq!(gpt_5_6.reasoning_effort_default(), Some("medium"));
        assert_eq!(
            gpt_5_6.reasoning_effort_values(),
            ["default", "none", "low", "medium", "high", "xhigh", "max"]
        );
        assert_eq!(
            gpt_5_6.apply_reasoning_effort("minimal"),
            Err("reasoning effort is unsupported".to_owned())
        );
        gpt_5_6
            .apply_reasoning_effort("max")
            .expect("official maximum effort should apply");
        assert_eq!(
            gpt_5_6.request_config().reasoning_effort(),
            Some(ReasoningEffort::Max)
        );
    }
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
            provider_id: None,
            model_id: None,
            reasoning_effort: None,
            created_at: 10,
            updated_at: 20,
            completed_turn_count: 0,
            resumable: false,
            parent_session_id: None,
            fork_message_count: None,
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
    assert!(!data_directory.join("agens.db").exists());
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
            "[provider]\nmodel = \"openai-api/test-model\"\nbase_url = \"{}\"\n\n[agent]\nparallel_tool_calls = false\n\n[options]\ndata_dir = \"{}\"\n",
            server.base_url(),
            data_directory.display(),
        ),
    )
    .expect("config should be written");

    let chat = isolated_agens_command(&temporary)
        .args(["chat", "hello from production"])
        .env("AGENS_CONFIG_HOME", &config_home)
        .env("OPENAI_API_KEY", "SENTINEL_OPENAI_API_KEY")
        .output()
        .expect("production binary should execute");
    let sessions = isolated_agens_command(&temporary)
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
            // The reviewer agent declares no `permissions:`, so it inherits the
            // parent's full native surface (write/bash/webfetch included) unlike
            // `explore`, which narrows explicitly. Only task nesting and MCP stay
            // excluded from a child's catalog except for bounded task delegation.
            required_body_fragments: vec![
                "child request".into(),
                "You are the isolated reviewer.".into(),
                "Use the review checklist.".into(),
                "gpt-4o".into(),
                "read".into(),
                "write".into(),
                "bash".into(),
                "webfetch".into(),
                "!parent request".into(),
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
            "[provider]\nmodel = \"openai-api/gpt-4.1\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n[permissions]\nallow = [\"task(reviewer)\"]\n",
            server.base_url(),
            data_directory.display(),
        ),
    )
    .expect("configuration should be written");

    let result = isolated_agens_command(&temporary)
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

    let listed = isolated_agens_command(&temporary)
        .args(["sessions", "list"])
        .current_dir(&project_root)
        .env("AGENS_CONFIG_HOME", &config_home)
        .output()
        .expect("sessions should list the parent turn");
    let reopened = isolated_agens_command(&temporary)
        .args(["sessions", "show", "1"])
        .current_dir(&project_root)
        .env("AGENS_CONFIG_HOME", &config_home)
        .output()
        .expect("sessions should reopen the parent turn");
    let removed = isolated_agens_command(&temporary)
        .args(["sessions", "rm", "1"])
        .current_dir(&project_root)
        .env("AGENS_CONFIG_HOME", &config_home)
        .output()
        .expect("sessions should remove the parent turn");
    let empty = isolated_agens_command(&temporary)
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
        &data_directory.join("agens.db"),
        &[
            "SENTINEL_OPENAI_API_KEY",
            "SENTINEL_PROVIDER_ERROR",
            "SENTINEL_PANIC",
            "SENTINEL_HEADER",
        ],
    );
}

#[test]
fn built_in_explore_inherits_the_effective_openai_parent_model_without_agent_files() {
    let temporary = TemporaryDirectory::new("builtin-explore-openai-model");
    let project_root = temporary.path().join("project");
    let config_home = temporary.path().join("config");
    let data_directory = temporary.path().join("data");
    std::fs::create_dir_all(project_root.join(".git")).expect("project marker should exist");
    std::fs::create_dir_all(&config_home).expect("config directory should exist");
    let server = ScriptedNativeOpenAiMockServer::start(vec![
        ScriptedOpenAiResponse {
            required_body_fragments: vec![
                "\"model\":\"gpt-5.6-sol\"".into(),
                "explore".into(),
                "general".into(),
                "parent explore request".into(),
            ],
            response: native_tool_call_response(
                "task-explore",
                "task",
                r#"{"agent":"explore","description":"inspect child"}"#,
            ),
        },
        ScriptedOpenAiResponse {
            required_body_fragments: vec![
                "\"model\":\"gpt-5.6-sol\"".into(),
                "inspect child".into(),
                "read-only exploration subagent".into(),
                "!parent explore request".into(),
            ],
            response: text_response("child explored"),
        },
        ScriptedOpenAiResponse {
            required_body_fragments: vec!["child explored".into()],
            response: text_response("parent complete"),
        },
    ]);
    std::fs::write(
        config_home.join("config.toml"),
        format!(
            "[provider]\nmodel = \"openai-api/gpt-4.1\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n[permissions]\nallow = [\"task(explore)\"]\n",
            server.base_url(),
            data_directory.display(),
        ),
    )
    .expect("configuration should be written");

    let output = isolated_agens_command(&temporary)
        .current_dir(&project_root)
        .args(["chat", "--model", "gpt-5.6-sol", "parent explore request"])
        .env("AGENS_CONFIG_HOME", &config_home)
        .env("OPENAI_API_KEY", "SENTINEL_OPENAI_API_KEY")
        .output()
        .expect("production binary should run");
    server.join();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "parent complete\n");
}

#[test]
fn agents_md_instructions_reach_both_the_parent_and_a_subagents_request_body_end_to_end() {
    let temporary = TemporaryDirectory::new("agents-md-instructions-e2e");
    let project_root = temporary.path().join("project");
    let config_home = temporary.path().join("config");
    let data_directory = temporary.path().join("data");
    std::fs::create_dir_all(project_root.join(".git")).expect("project marker should exist");
    std::fs::create_dir_all(&config_home).expect("config directory should exist");
    std::fs::write(project_root.join("AGENTS.md"), "PROJECT-AGENTS-MD-SENTINEL")
        .expect("project AGENTS.md should be written");
    let server = ScriptedNativeOpenAiMockServer::start(vec![
        // This first request is the top-level `chat` command's own turn. Its `instructions`
        // field comes from `headless_turn_own_system_prompt`
        // (`crates/agens-headless/src/turn.rs`), which re-reads `SessionConfig` directly rather
        // than going through `discover_agent_catalog`'s `primary` agent — but appends this
        // session's own AGENTS.md instructions to whichever base prompt it resolves (here, the
        // hardcoded default, since no `--system` flag or `agent.system_prompt` applies), so it
        // still carries the same sentinel as the `task`-dispatched subagent below. The `@once:`
        // fragment below asserts the sentinel appears exactly once in this request body, i.e.
        // it was appended once and not double-injected.
        ScriptedOpenAiResponse {
            required_body_fragments: vec![
                "\"model\":\"gpt-5.5\"".into(),
                "explore".into(),
                "general".into(),
                "parent general request".into(),
                "You are Agens, a helpful coding agent.".into(),
                "@once:PROJECT-AGENTS-MD-SENTINEL".into(),
            ],
            response: native_tool_call_response(
                "task-general",
                "task",
                r#"{"agent":"general","description":"implement child"}"#,
            ),
        },
        ScriptedOpenAiResponse {
            required_body_fragments: vec![
                "\"model\":\"gpt-5.5\"".into(),
                "implement child".into(),
                "general-purpose subagent".into(),
                "PROJECT-AGENTS-MD-SENTINEL".into(),
                "!parent general request".into(),
            ],
            response: text_response("child implemented"),
        },
        ScriptedOpenAiResponse {
            required_body_fragments: vec!["child implemented".into()],
            response: text_response("parent complete"),
        },
    ]);
    std::fs::write(
        config_home.join("config.toml"),
        format!(
            "[provider]\nmodel = \"openai-chatgpt/gpt-5.4\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n[permissions]\nallow = [\"task(general)\"]\n",
            server.base_url(),
            data_directory.display(),
        ),
    )
    .expect("configuration should be written");
    write_chatgpt_credentials(&config_home, "header.eyJleHAiOjE4OTM0NTYwMDB9.signature");

    let output = isolated_agens_command(&temporary)
        .current_dir(&project_root)
        .args(["chat", "--model", "gpt-5.5", "parent general request"])
        .env("AGENS_CONFIG_HOME", &config_home)
        .env_remove("OPENAI_API_KEY")
        .output()
        .expect("production binary should run");
    server.join();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "parent complete\n");
}

#[test]
fn explicit_task_model_selects_a_second_available_openai_model() {
    let temporary = TemporaryDirectory::new("task-explicit-openai-model");
    let project_root = temporary.path().join("project");
    let config_home = temporary.path().join("config");
    let data_directory = temporary.path().join("data");
    std::fs::create_dir_all(project_root.join(".git")).expect("project marker should exist");
    std::fs::create_dir_all(&config_home).expect("config directory should exist");
    let server = ScriptedNativeOpenAiMockServer::start(vec![
        ScriptedOpenAiResponse {
            required_body_fragments: vec![
                "\"model\":\"gpt-5.6-sol\"".into(),
                "\"model\":{\"description\":\"Omit this.".into(),
                "\"enum\":[\"gpt-4.1\"".into(),
                "gpt-4.1".into(),
                "parent chooses child model".into(),
            ],
            response: native_tool_call_response(
                "task-model",
                "task",
                r#"{"agent":"explore","model":"gpt-4.1","description":"inspect with second model"}"#,
            ),
        },
        ScriptedOpenAiResponse {
            required_body_fragments: vec![
                "\"model\":\"gpt-4.1\"".into(),
                "inspect with second model".into(),
            ],
            response: text_response("second model child"),
        },
        ScriptedOpenAiResponse {
            required_body_fragments: vec!["second model child".into()],
            response: text_response("parent complete"),
        },
    ]);
    std::fs::write(
        config_home.join("config.toml"),
        format!(
            "[provider]\nmodel = \"openai-api/gpt-4.1\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n[permissions]\nallow = [\"task(explore)\"]\n",
            server.base_url(),
            data_directory.display(),
        ),
    )
    .expect("configuration should be written");

    let output = isolated_agens_command(&temporary)
        .current_dir(&project_root)
        .args([
            "chat",
            "--model",
            "gpt-5.6-sol",
            "parent chooses child model",
        ])
        .env("AGENS_CONFIG_HOME", &config_home)
        .env("OPENAI_API_KEY", "SENTINEL_OPENAI_API_KEY")
        .output()
        .expect("production binary should run");
    server.join();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "parent complete\n");
}

#[test]
fn built_in_general_inherits_the_effective_chatgpt_parent_model_without_agent_files() {
    let temporary = TemporaryDirectory::new("builtin-general-chatgpt-model");
    let project_root = temporary.path().join("project");
    let config_home = temporary.path().join("config");
    let data_directory = temporary.path().join("data");
    std::fs::create_dir_all(project_root.join(".git")).expect("project marker should exist");
    std::fs::create_dir_all(&config_home).expect("config directory should exist");
    let server = ScriptedNativeOpenAiMockServer::start(vec![
        ScriptedOpenAiResponse {
            required_body_fragments: vec![
                "\"model\":\"gpt-5.5\"".into(),
                "explore".into(),
                "general".into(),
                "parent general request".into(),
            ],
            response: native_tool_call_response(
                "task-general",
                "task",
                r#"{"agent":"general","description":"implement child"}"#,
            ),
        },
        ScriptedOpenAiResponse {
            required_body_fragments: vec![
                "\"model\":\"gpt-5.5\"".into(),
                "implement child".into(),
                "general-purpose subagent".into(),
                "!parent general request".into(),
            ],
            response: text_response("child implemented"),
        },
        ScriptedOpenAiResponse {
            required_body_fragments: vec!["child implemented".into()],
            response: text_response("parent complete"),
        },
    ]);
    std::fs::write(
        config_home.join("config.toml"),
        format!(
            "[provider]\nmodel = \"openai-chatgpt/gpt-5.4\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n[permissions]\nallow = [\"task(general)\"]\n",
            server.base_url(),
            data_directory.display(),
        ),
    )
    .expect("configuration should be written");
    std::fs::write(
        config_home.join("auth.json"),
        r#"{"openai-chatgpt":{"access_token":"header.eyJleHAiOjE4OTM0NTYwMDB9.signature","refresh_token":"SENTINEL_CHATGPT_REFRESH","account_id":"account_123","expires_at":"2030-01-01T00:00:00Z"}}"#,
    )
    .expect("ChatGPT credentials should be written");

    let output = isolated_agens_command(&temporary)
        .current_dir(&project_root)
        .args(["chat", "--model", "gpt-5.5", "parent general request"])
        .env("AGENS_CONFIG_HOME", &config_home)
        .env_remove("OPENAI_API_KEY")
        .output()
        .expect("production binary should run");
    server.join();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "parent complete\n");
}

#[test]
fn unavailable_explicit_child_model_is_diagnosed_once_without_a_child_provider_request() {
    let temporary = TemporaryDirectory::new("task-model-unavailable-diagnostic");
    let project_root = temporary.path().join("project");
    let config_home = temporary.path().join("config");
    let data_directory = temporary.path().join("data");
    std::fs::create_dir_all(project_root.join(".git")).expect("project marker should exist");
    std::fs::create_dir_all(&config_home).expect("config directory should exist");
    let server = ScriptedNativeOpenAiMockServer::start(vec![
        ScriptedOpenAiResponse {
            required_body_fragments: vec!["parent unavailable child".into(), "task".into()],
            response: native_tool_call_response(
                "task-unavailable",
                "task",
                r#"{"agent":"explore","model":"gpt-4o","description":"must not run"}"#,
            ),
        },
        ScriptedOpenAiResponse {
            required_body_fragments: vec!["task: requested model is unavailable".into()],
            response: text_response("parent recovered from unavailable child"),
        },
    ]);
    std::fs::write(
        config_home.join("config.toml"),
        format!(
            "[provider]\nmodel = \"openai-chatgpt/gpt-5.5\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n[permissions]\nallow = [\"task(explore)\"]\n",
            server.base_url(),
            data_directory.display(),
        ),
    )
    .expect("configuration should be written");
    std::fs::write(
        config_home.join("auth.json"),
        r#"{"openai-chatgpt":{"access_token":"header.eyJleHAiOjE4OTM0NTYwMDB9.signature","refresh_token":"SENTINEL_CHATGPT_REFRESH","account_id":"account_123","expires_at":"2030-01-01T00:00:00Z"}}"#,
    )
    .expect("ChatGPT credentials should be written");

    let output = isolated_agens_command(&temporary)
        .current_dir(&project_root)
        .args(["chat", "parent unavailable child"])
        .env("AGENS_CONFIG_HOME", &config_home)
        .env_remove("OPENAI_API_KEY")
        .output()
        .expect("production binary should run");
    server.join();

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "parent recovered from unavailable child\n"
    );
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
    let model_events = diagnostic_json_events(&data_directory)
        .into_iter()
        .filter(|event| {
            event["scope"] == "subagent"
                && event["component"] == "subagent"
                && event["event"] == "terminal"
                && event["class"] == "model_unavailable"
        })
        .collect::<Vec<_>>();
    assert_eq!(model_events.len(), 1);
    assert!(
        model_events[0]["reference"]
            .as_str()
            .is_some_and(|reference| reference.len() == 8)
    );
    assert_eq!(model_events[0]["attempt"], 0);
    assert_eq!(model_events[0]["max_attempts"], 0);
    assert!(
        diagnostic_json_events(&data_directory)
            .into_iter()
            .all(|event| {
                !(event["scope"] == "parent"
                    && event["event"] == "terminal"
                    && event["class"] == "runtime")
            })
    );
}

#[cfg(unix)]
#[test]
fn production_task_cancellation_prevents_parent_continuation_and_persists_emitted_history() {
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

    let mut server = TaskStalledOpenAiMockServer::start();
    std::fs::write(
        config_home.join("config.toml"),
        format!(
            "[provider]\nmodel = \"openai-api/gpt-4.1\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n[permissions]\nallow = [\"task(reviewer)\"]\n",
            server.base_url(),
            data_directory.display(),
        ),
    )
    .expect("configuration should be written");

    let child = isolated_agens_command(&temporary)
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
    assert_diagnostic_error(
        &output.stderr,
        "error: cancelled: headless turn was cancelled\n",
    );
    assert_interrupted_session_saved(
        &temporary,
        &project_root,
        &config_home,
        "parent task cancellation",
    );
    assert_sqlite_has_partial_turn(&data_directory.join("agens.db"), 3);
    assert_sqlite_has_no_sentinels(
        &data_directory.join("agens.db"),
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
fn production_task_provider_failure_is_sanitized_and_returns_control_to_the_parent() {
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
    let mut responses = vec![ScriptedOpenAiResponse {
        required_body_fragments: vec!["parent provider failure".into()],
        response: native_tool_call_response(
            "task-failure",
            "task",
            r#"{"agent":"reviewer","description":"child provider failure"}"#,
        ),
    }];
    // A permanent rejection rather than a `500`: what this test is about is
    // the parent reading a sanitized child failure, and a transient status
    // would tie the script's length to the provider's retry budget. The
    // exhaustion of that budget is covered where the schedule can be
    // compressed, in `agens-providers`.
    responses.push(ScriptedOpenAiResponse {
        required_body_fragments: vec!["child provider failure".into()],
        response: "HTTP/1.1 400 Bad Request\r\nX-Remote-Secret: SENTINEL_HEADER\r\nContent-Length: 23\r\nConnection: close\r\n\r\nSENTINEL_PROVIDER_ERROR".into(),
    });
    responses.push(ScriptedOpenAiResponse {
        required_body_fragments: vec!["task: provider failure".into()],
        response: text_response("parent recovered from child provider failure"),
    });
    let server = ScriptedNativeOpenAiMockServer::start(responses);
    std::fs::write(
        config_home.join("config.toml"),
        format!(
            "[provider]\nmodel = \"openai-api/gpt-4.1\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n[permissions]\nallow = [\"task(reviewer)\"]\n",
            server.base_url(),
            data_directory.display(),
        ),
    )
    .expect("configuration should be written");

    let output = isolated_agens_command(&temporary)
        .current_dir(&project_root)
        .args(["chat", "parent provider failure"])
        .env("AGENS_CONFIG_HOME", &config_home)
        .env("OPENAI_API_KEY", "SENTINEL_OPENAI_API_KEY")
        .output()
        .expect("production binary should run");
    server.join();

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "parent recovered from child provider failure\n"
    );
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
    assert_interrupted_session_saved(
        &temporary,
        &project_root,
        &config_home,
        "parent provider failure",
    );
    assert_output_and_store_exclude_sentinels(
        &output,
        &data_directory.join("agens.db"),
        &[
            "SENTINEL_OPENAI_API_KEY",
            "SENTINEL_PROVIDER_ERROR",
            "SENTINEL_HEADER",
        ],
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
            "[provider]\nmodel = \"openai-chatgpt/test-model\"\nbase_url = \"{}\"\n\n[agent]\nparallel_tool_calls = true\n\n[options]\ndata_dir = \"{}\"\n",
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

    let chat = isolated_agens_command(&temporary)
        .args(["chat", "hello from subscription"])
        .env("AGENS_CONFIG_HOME", &config_home)
        .env_remove("OPENAI_API_KEY")
        .output()
        .expect("production binary should execute");
    let sessions = isolated_agens_command(&temporary)
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
        &data_directory.join("agens.db"),
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

    let chat = isolated_agens_command(&temporary)
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
        let data_directory = temporary.path().join("data");
        std::fs::create_dir_all(&config_home).expect("config directory should exist");
        std::fs::write(
            config_home.join("config.toml"),
            format!(
                "[provider]\nmodel = \"openai-chatgpt/test-model\"\n\n[options]\ndata_dir = \"{}\"\n",
                data_directory.display(),
            ),
        )
        .expect("config should be written");
        if let Some(credentials) = credentials {
            std::fs::write(config_home.join("auth.json"), credentials)
                .expect("credential fixture should be written");
        }

        let output = isolated_agens_command(&temporary)
            .args(["chat", "reject invalid credentials"])
            .env("AGENS_CONFIG_HOME", &config_home)
            .env_remove("OPENAI_API_KEY")
            .output()
            .expect("production binary should execute");

        assert_eq!(output.status.code(), Some(4), "{name}");
        assert_eq!(String::from_utf8_lossy(&output.stdout), "", "{name}");
        assert_diagnostic_error(
            &output.stderr,
            "error: auth: ChatGPT credentials are unavailable or invalid\n",
        );
        assert!(!format!("{output:?}").contains("SENTINEL"), "{name}");
        // The provider is resolved before any runtime is built now, so an
        // unauthenticated run stops before it would open a sessions database or
        // start an MCP server.
        assert!(!data_directory.join("agens.db").exists(), "{name}");
    }
}

#[test]
fn production_binary_maps_chatgpt_provider_and_auth_failures_without_leaking_credentials() {
    // `rounds` is how many times the endpoint answers, not how many failures
    // the case has: a transient status is attempted three times, and a server
    // that answers only once leaves the remaining attempts to a socket that is
    // no longer there. That is also why the transient cases record the remote
    // detail — the run reaches its retry budget against an endpoint that kept
    // answering, instead of ending on a transport error with nothing to
    // report.
    for (name, rounds, response, expected_exit, expected_stderr, expected_detail) in [
        (
            "forbidden",
            1,
            "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned(),
            Some(4),
            "error: auth: provider credentials are unavailable or invalid\n",
            Some("HTTP 403 rejected model \"test-model\""),
        ),
        (
            "rejected",
            1,
            "HTTP/1.1 422 Unprocessable Content\r\nContent-Length: 27\r\nConnection: close\r\n\r\nSENTINEL_CHATGPT_ERROR_BODY".to_owned(),
            Some(1),
            "error: provider: provider request was rejected\n",
            Some("HTTP 422 rejected model \"test-model\"\nSENTINEL_CHATGPT_ERROR_BODY"),
        ),
        (
            "rate limit",
            3,
            "HTTP/1.1 429 Too Many Requests\r\nContent-Length: 27\r\nConnection: close\r\n\r\nSENTINEL_CHATGPT_ERROR_BODY".to_owned(),
            Some(1),
            "error: provider: provider request was rate limited\n",
            Some("HTTP 429 rejected model \"test-model\"\nSENTINEL_CHATGPT_ERROR_BODY"),
        ),
        (
            "server failure",
            3,
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 27\r\nConnection: close\r\n\r\nSENTINEL_CHATGPT_ERROR_BODY".to_owned(),
            Some(1),
            "error: provider: provider service failed\n",
            Some("HTTP 500 rejected model \"test-model\"\nSENTINEL_CHATGPT_ERROR_BODY"),
        ),
        (
            "protocol failure",
            1,
            sse_response(&[r#"{"type":"response.incomplete","response":{"error":{"message":"SENTINEL_CHATGPT_ERROR_BODY"}}}"#]),
            Some(1),
            "error: provider: provider response protocol failed\n",
            None,
        ),
    ] {
        let temporary = TemporaryDirectory::new(&format!("production-chatgpt-{name}"));
        let config_home = temporary.path().join("config");
        let data_directory = temporary.path().join("data");
        std::fs::create_dir_all(&config_home).expect("config directory should exist");
        let server = ScriptedNativeOpenAiMockServer::start(
            std::iter::repeat_n(response, rounds)
                .map(|response| ScriptedOpenAiResponse {
                    required_body_fragments: vec!["\"store\":false".to_owned()],
                    response,
                })
                .collect(),
        );
        std::fs::write(
            config_home.join("config.toml"),
            format!(
                "[provider]\nmodel = \"openai-chatgpt/test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n",
                server.base_url(),
                data_directory.display(),
            ),
        )
        .expect("config should be written");
        write_chatgpt_credentials(&config_home, "SENTINEL_CHATGPT_ACCESS");

        let output = isolated_agens_command(&temporary)
            .args(["chat", "handle remote failure"])
            .env("AGENS_CONFIG_HOME", &config_home)
            .env_remove("OPENAI_API_KEY")
            .output()
            .expect("production binary should execute");

        assert_eq!(output.status.code(), expected_exit, "{name}");
        assert_eq!(String::from_utf8_lossy(&output.stdout), "", "{name}");
        match expected_detail {
            Some(detail) => assert_diagnostic_error_with_detail(
                &output.stderr,
                expected_stderr,
                &format!("{detail}\n"),
            ),
            None => assert_diagnostic_error(&output.stderr, expected_stderr),
        }
        // The credential values must remain absent regardless of whether this case's
        // recorded detail includes the response body.
        for secret in [
            "SENTINEL_CHATGPT_ACCESS",
            "SENTINEL_CHATGPT_REFRESH",
            "SENTINEL_CHATGPT_REMOTE",
        ] {
            assert!(!format!("{output:?}").contains(secret), "{name}: {secret}");
        }
        // The response body is a message, not a credential: R1/R9 require it to reach
        // stderr wherever it was actually recorded, and it must never reach the
        // diagnostics JSONL regardless.
        let output_contains_body = format!("{output:?}").contains("SENTINEL_CHATGPT_ERROR_BODY");
        let detail_contains_body = expected_detail
            .is_some_and(|detail| detail.contains("SENTINEL_CHATGPT_ERROR_BODY"));
        assert_eq!(
            output_contains_body, detail_contains_body,
            "{name}: SENTINEL_CHATGPT_ERROR_BODY presence should match the recorded detail"
        );
        assert_diagnostics_have_no_sentinels(
            &data_directory,
            &[
                "SENTINEL_CHATGPT_ACCESS",
                "SENTINEL_CHATGPT_REFRESH",
                "SENTINEL_CHATGPT_REMOTE",
                "SENTINEL_CHATGPT_ERROR_BODY",
            ],
        );
        assert!(data_directory.join("agens.db").is_file(), "{name}");

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
                "[provider]\nmodel = \"openai-chatgpt/test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n{setup}",
                server.base_url(),
                data_directory.display(),
            ),
        )
        .expect("config should be written");
        write_chatgpt_credentials(&config_home, "SENTINEL_CHATGPT_TOOL_ACCESS");

        let output = isolated_agens_command(&temporary)
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
                &isolated_agens_command(&temporary)
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
            &data_directory.join("agens.db"),
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
            "[provider]\nmodel = \"openai-chatgpt/test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n",
            server.base_url(),
            data_directory.display(),
        ),
    )
    .expect("config should be written");
    write_chatgpt_credentials(&config_home, "SENTINEL_CHATGPT_CANCEL_ACCESS");

    let child = isolated_agens_command(&temporary)
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
    assert_diagnostic_error(
        &output.stderr,
        "error: cancelled: headless turn was cancelled\n",
    );
    assert_interrupted_session_saved(
        &temporary,
        temporary.path(),
        &config_home,
        "cancel subscription request",
    );
    assert_sqlite_has_interrupted_turn(&data_directory.join("agens.db"));
    assert_sqlite_has_no_sentinels(
        &data_directory.join("agens.db"),
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
            "[provider]\nmodel = \"openai-api/test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n[permissions]\nallow = [\"read(notes.md)\"]\n",
            server.base_url(),
            data_directory.display(),
        ),
    )
    .expect("config should be written");

    let chat = isolated_agens_command(&temporary)
        .args(["chat", "read the native file"])
        .current_dir(&project_root)
        .env("AGENS_CONFIG_HOME", &config_home)
        .env("OPENAI_API_KEY", "SENTINEL_OPENAI_API_KEY")
        .output()
        .expect("production binary should execute");
    let sessions = isolated_agens_command(&temporary)
        .args(["sessions", "list"])
        .current_dir(&project_root)
        .env("AGENS_CONFIG_HOME", &config_home)
        .output()
        .expect("sessions command should execute");
    let resumed = isolated_agens_command(&temporary)
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
fn production_binary_persists_an_empty_native_search_result() {
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
                r#"{"path":".","query":"absent"}"#,
            ),
        },
        ScriptedOpenAiResponse {
            required_body_fragments: vec![
                "\"call_id\":\"call_search\"".to_owned(),
                "\"output\":\"\"".to_owned(),
            ],
            response: text_response("native search completed"),
        },
    ]);
    std::fs::write(
        config_home.join("config.toml"),
        format!(
            "[provider]\nmodel = \"openai-api/test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n",
            server.base_url(),
            data_directory.display(),
        ),
    )
    .expect("config should be written");

    let chat = isolated_agens_command(&temporary)
        .args(["chat", "--dangerously-allow-all", "search for absent text"])
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
    let session = SessionStore::open(&data_directory)
        .expect("session store should open")
        .load_session_for_resume(1)
        .expect("the completed turn should remain resumable");
    let persisted_tool_result = session.messages.iter().find_map(|message| {
        message.parts.iter().find_map(|part| match part {
            MessagePart::ToolResult { content, .. } => Some(content.as_str()),
            _ => None,
        })
    });
    assert_eq!(persisted_tool_result, Some("[tool returned no output]"));
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
                "[provider]\nmodel = \"openai-api/test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n[permissions]\nallow = [{rule:?}]\n",
                server.base_url(),
                data_directory.display(),
            ),
        )
        .expect("config should be written");

        let output = isolated_agens_command(&temporary)
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
                &isolated_agens_command(&temporary)
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
                    "\"output\":\"permission denied\"".to_owned(),
                ],
                response: text_response("static permission denied"),
            },
        ]);
        std::fs::write(
            config_home.join("config.toml"),
            format!(
                "[provider]\nmodel = \"openai-api/test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n[permissions]\ndeny = [{rule:?}]\n",
                server.base_url(),
                data_directory.display(),
            ),
        )
        .expect("config should be written");

        let output = isolated_agens_command(&temporary)
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
                    "\"output\":\"permission denied\"".to_owned(),
                ],
                response: text_response("static ask denial handled"),
            },
        ]);
        std::fs::write(
            config_home.join("config.toml"),
            format!(
                "[provider]\nmodel = \"openai-api/test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n[permissions]\nallow = [{rule:?}]\n",
                server.base_url(),
                data_directory.display(),
            ),
        )
        .expect("config should be written");

        let output = isolated_agens_command(&temporary)
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
                &isolated_agens_command(&temporary)
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
                "\"output\":\"permission denied\"".to_owned(),
            ],
            response: text_response("denial handled"),
        },
    ]);
    std::fs::write(
        config_home.join("config.toml"),
        format!(
            "[provider]\nmodel = \"openai-api/test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n[permissions]\ndeny = [\"read(SENTINEL_DENIED_INPUT.txt)\"]\n",
            server.base_url(),
            data_directory.display(),
        ),
    )
    .expect("config should be written");

    let chat = isolated_agens_command(&temporary)
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
                "\"output\":\"permission denied\"".to_owned(),
                "!SENTINEL_UNRESOLVED_ASK".to_owned(),
            ],
            response: text_response("native ask denial handled"),
        },
    ]);
    std::fs::write(
        config_home.join("config.toml"),
        format!(
            "[provider]\nmodel = \"openai-api/test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n",
            server.base_url(),
            data_directory.display(),
        ),
    )
    .expect("config should be written");

    let output = isolated_agens_command(&temporary)
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
    let sessions = isolated_agens_command(&temporary)
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
    let server = ScriptedNativeOpenAiMockServer::start(vec![
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
                "\"output\":\"permission denied\"".to_owned(),
            ],
            response: text_response("chat mode denial handled"),
        },
    ]);
    std::fs::write(
        config_home.join("config.toml"),
        format!(
            "[provider]\nmodel = \"openai-api/test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n",
            server.base_url(),
            data_directory.display(),
        ),
    )
    .expect("config should be written");

    let output = isolated_agens_command(&temporary)
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
        let server = ScriptedNativeOpenAiMockServer::start(vec![ScriptedOpenAiResponse {
            required_body_fragments: vec!["native::write".to_owned()],
            response,
        }]);
        std::fs::write(
            config_home.join("config.toml"),
            format!(
                "[provider]\nmodel = \"openai-api/test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n",
                server.base_url(),
                data_directory.display(),
            ),
        )
        .expect("config should be written");

        let output = isolated_agens_command(&temporary)
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
        assert_diagnostic_error(
            &output.stderr,
            "error: provider: provider response protocol failed\n",
        );
        assert!(!side_effect.exists(), "{name} call must not dispatch");
        assert_no_saved_sessions(&temporary, &project_root, &config_home);

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
    let server = ScriptedNativeOpenAiMockServer::start(vec![ScriptedOpenAiResponse {
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
            "[provider]\nmodel = \"openai-api/test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n[permissions]\nallow = [\"bash(*)\"]\n",
            server.base_url(),
            data_directory.display(),
        ),
    )
    .expect("config should be written");

    let child = isolated_agens_command(&temporary)
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
    wait_for_path(&ready_marker);

    let signal_status = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .expect("SIGINT command should execute");
    assert!(signal_status.success(), "SIGINT delivery should succeed");

    let output = wait_for_child_output(child, Duration::from_secs(2));
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
    assert_diagnostic_error(
        &output.stderr,
        "error: cancelled: headless turn was cancelled\n",
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
    assert_interrupted_session_saved(
        &temporary,
        &project_root,
        &config_home,
        "run the long native bash command",
    );

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
    let server = ScriptedNativeOpenAiMockServer::start(vec![
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
            "[provider]\nmodel = \"openai-api/test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n[permissions]\nallow = [\"write(execution-count)\"]\n",
            server.base_url(),
            data_directory.display(),
        ),
    )
    .expect("config should be written");

    let output = isolated_agens_command(&temporary)
        .args(["chat", "execute exactly once"])
        .current_dir(&project_root)
        .env("AGENS_CONFIG_HOME", &config_home)
        .env("OPENAI_API_KEY", "SENTINEL_OPENAI_API_KEY")
        .output()
        .expect("production binary should execute");

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
    assert_diagnostic_error(&output.stderr, "error: provider: provider request failed\n");
    assert_eq!(
        std::fs::read_to_string(&side_effect)
            .expect("only the first authorized call should execute"),
        "first execution"
    );
    assert_interrupted_session_saved(
        &temporary,
        &project_root,
        &config_home,
        "execute exactly once",
    );

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
            "[provider]\nmodel = \"openai-api/test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n",
            server.base_url(),
            data_directory.display(),
        ),
    )
    .expect("config should be written");

    let child = isolated_agens_command(&temporary)
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
    assert_diagnostic_error(
        &output.stderr,
        "error: cancelled: headless turn was cancelled\n",
    );
    assert_interrupted_session_saved(
        &temporary,
        temporary.path(),
        &config_home,
        "cancel production request",
    );
    assert_sqlite_has_interrupted_turn(&data_directory.join("agens.db"));

    server.join();
}

#[test]
fn production_binary_sanitizes_remote_response_headers_and_body() {
    let temporary = TemporaryDirectory::new("production-remote-error");
    let config_home = temporary.path().join("config");
    let data_directory = temporary.path().join("data");
    std::fs::create_dir_all(&config_home).expect("config directory should exist");
    let server = ErrorOpenAiMockServer::start();
    std::fs::write(
        config_home.join("config.toml"),
        format!(
            "[provider]\nmodel = \"openai-api/test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n",
            server.base_url(),
            data_directory.display(),
        ),
    )
    .expect("config should be written");

    let output = isolated_agens_command(&temporary)
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
    // The remote body is a message, not a credential: it reaches stderr as the
    // recorded failure detail, exactly as the ChatGPT matrix beside this pins,
    // while the credential and the remote header never do and none of the three
    // reach the diagnostics file.
    assert_diagnostic_error_with_detail_text(
        &diagnostics,
        "error: provider: provider service failed\n",
        "HTTP 500 rejected model \"test-model\"\nSENTINEL_REMOTE_ERROR_BODY\n",
    );
    for secret in ["SENTINEL_OPENAI_API_KEY", "SENTINEL_REMOTE_ERROR_HEADER"] {
        assert!(!diagnostics.contains(secret), "diagnostics leaked {secret}");
    }
    assert_diagnostics_have_no_sentinels(
        &data_directory,
        &[
            "SENTINEL_OPENAI_API_KEY",
            "SENTINEL_REMOTE_ERROR_HEADER",
            "SENTINEL_REMOTE_ERROR_BODY",
        ],
    );
    assert!(data_directory.join("agens.db").is_file());

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

    let config_output = isolated_agens_command(&temporary)
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
            "[provider]\nmodel = \"openai-api/test-model\"\nbase_url = \"http://127.0.0.1:1\"\n\n[options]\ndata_dir = \"{}\"\n",
            store_path.display()
        ),
    )
    .expect("store config should be written");

    let store_output = isolated_agens_command(&temporary)
        .args(["chat", "reject store path"])
        .env("AGENS_CONFIG_HOME", &store_config_home)
        .env("OPENAI_API_KEY", "SENTINEL_OPENAI_API_KEY")
        .output()
        .expect("production binary should execute");
    assert_eq!(store_output.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&store_output.stdout), "");
    assert_diagnostic_error(
        &store_output.stderr,
        "error: store: permission grants are unavailable\n",
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
            "[provider]\nmodel = \"openai-api/test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n[mcp.broken]\ntransport = \"stdio\"\ncommand = \"{}\"\nargs = [\"malformed\"]\ntimeout_ms = 1000\n[mcp.broken.env]\nFAKE_MCP_PROTOCOL_SECRET = \"SENTINEL_MCP_PROTOCOL\"\nFAKE_MCP_STDERR_SECRET = \"SENTINEL_MCP_STDERR\"\n\n[mcp.crashed]\ntransport = \"stdio\"\ncommand = \"{}\"\nargs = [\"crash\"]\ntimeout_ms = 1000\n[mcp.crashed.env]\nFAKE_MCP_TRANSPORT_SECRET = \"SENTINEL_MCP_TRANSPORT\"\n\n[mcp.files]\ntransport = \"stdio\"\ncommand = \"{}\"\nargs = [\"success\"]\ntimeout_ms = 1000\n",
            server.base_url(),
            data_directory.display(),
            env!("CARGO_BIN_EXE_agens-cli-fake-mcp-child"),
            env!("CARGO_BIN_EXE_agens-cli-fake-mcp-child"),
            env!("CARGO_BIN_EXE_agens-cli-fake-mcp-child"),
        ),
    )
    .expect("config should be written");

    let output = isolated_agens_command(&temporary)
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
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "mcp: broken failed to connect (protocol)\nmcp: crashed failed to connect (transport)\n"
    );
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
            &isolated_agens_command(&temporary)
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
        &data_directory.join("agens.db"),
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

    let server = ScriptedNativeOpenAiMockServer::start(vec![ScriptedOpenAiResponse {
        required_body_fragments: vec!["files::first".to_owned()],
        response: native_tool_call_response("call_mcp_cancel", "files::first", r#"{}"#),
    }]);
    std::fs::write(
        config_home.join("config.toml"),
        format!(
            "[provider]\nmodel = \"openai-api/test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n[mcp.files]\ntransport = \"stdio\"\ncommand = \"{}\"\nargs = [\"call-sleep\"]\ntimeout_ms = 1000\n[mcp.files.env]\nFAKE_MCP_CALL_READY = \"{}\"\n",
            server.base_url(),
            data_directory.display(),
            env!("CARGO_BIN_EXE_agens-cli-fake-mcp-child"),
            call_ready.display(),
        ),
    )
    .expect("config should be written");

    let child = isolated_agens_command(&temporary)
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
    wait_for_path(&call_ready);

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
    assert_diagnostic_error(
        &output.stderr,
        "error: cancelled: headless turn was cancelled\n",
    );
    assert_interrupted_session_saved(
        &temporary,
        &project_root,
        &config_home,
        "cancel configured MCP tool",
    );

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
                "\"output\":\"[redacted: 24 characters]\"".to_owned(),
                "!SENTINEL_MCP_ARGUMENT".to_owned(),
                "!SENTINEL_MCP_REMOTE_BODY".to_owned(),
            ],
            response: text_response("MCP failure handled"),
        },
    ]);
    std::fs::write(
        config_home.join("config.toml"),
        format!(
            "[provider]\nmodel = \"openai-api/test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n[mcp.files]\ntransport = \"stdio\"\ncommand = \"{}\"\nargs = [\"call-error\"]\ntimeout_ms = 1000\n[mcp.files.env]\nFAKE_MCP_TOOL_ERROR_SECRET = \"SENTINEL_MCP_REMOTE_BODY\"\nFAKE_MCP_STDERR_SECRET = \"SENTINEL_MCP_STDERR\"\n",
            server.base_url(),
            data_directory.display(),
            env!("CARGO_BIN_EXE_agens-cli-fake-mcp-child"),
        ),
    )
    .expect("config should be written");

    let output = isolated_agens_command(&temporary)
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
        &data_directory.join("agens.db"),
        &[
            "SENTINEL_OPENAI_API_KEY",
            "SENTINEL_MCP_REMOTE_BODY",
            "SENTINEL_MCP_STDERR",
        ],
    );
    assert_sqlite_contains_sentinels(&data_directory.join("agens.db"), &["SENTINEL_MCP_ARGUMENT"]);

    server.join();
}

#[test]
fn production_binary_persists_model_visible_native_arguments_and_tool_failure_output_in_session() {
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
            // The failing command's own output is what the model needs in order to recover, so
            // the immediate continuation carries the text the dispatcher sanitized. The tool
            // ARGUMENTS are not resent: this dialect refers back to them by response id.
            required_body_fragments: vec![
                "\"call_id\":\"call_native_secret\"".to_owned(),
                "SENTINEL_NATIVE_OUTPUT".to_owned(),
                "!SENTINEL_NATIVE_ARGUMENT".to_owned(),
            ],
            response: text_response("native failure handled"),
        },
    ]);
    std::fs::write(
        config_home.join("config.toml"),
        format!(
            "[provider]\nmodel = \"openai-api/test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n[permissions]\nallow = [\"bash(*)\"]\n",
            server.base_url(),
            data_directory.display(),
        ),
    )
    .expect("config should be written");

    let output = isolated_agens_command(&temporary)
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
    assert!(
        session.messages.iter().flat_map(|message| &message.parts).any(|part| {
            matches!(part, MessagePart::ToolResult { content, .. } if content.contains("SENTINEL_NATIVE_OUTPUT"))
        }),
        "model-visible native tool failure output must remain resumable conversation content"
    );
    assert_sqlite_has_no_sentinels(
        &data_directory.join("agens.db"),
        &["SENTINEL_OPENAI_API_KEY"],
    );
    assert_sqlite_contains_sentinels(
        &data_directory.join("agens.db"),
        &["SENTINEL_NATIVE_ARGUMENT"],
    );

    server.join();
}

#[test]
fn production_binary_records_a_completed_native_call_in_the_evidence_ledger() {
    let temporary = TemporaryDirectory::new("production-evidence-ledger");
    let project_root = temporary.path().join("project");
    let config_home = temporary.path().join("config");
    let data_directory = temporary.path().join("data");
    std::fs::create_dir_all(project_root.join(".git")).expect("project marker should exist");
    std::fs::create_dir_all(&config_home).expect("config directory should exist");

    let server = ScriptedNativeOpenAiMockServer::start(vec![
        ScriptedOpenAiResponse {
            required_body_fragments: vec!["native::bash".to_owned()],
            response: native_tool_call_response(
                "call_ledger_bash",
                "native::bash",
                r#"{"command":"exit 0"}"#,
            ),
        },
        ScriptedOpenAiResponse {
            required_body_fragments: vec!["\"call_id\":\"call_ledger_bash\"".to_owned()],
            response: text_response("ledger recorded"),
        },
    ]);
    std::fs::write(
        config_home.join("config.toml"),
        format!(
            "[provider]\nmodel = \"openai-api/test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n[permissions]\nallow = [\"bash(*)\"]\n",
            server.base_url(),
            data_directory.display(),
        ),
    )
    .expect("config should be written");

    let output = isolated_agens_command(&temporary)
        .args(["chat", "run a native bash command"])
        .current_dir(&project_root)
        .env("AGENS_CONFIG_HOME", &config_home)
        .env("OPENAI_API_KEY", "SENTINEL_OPENAI_API_KEY")
        .output()
        .expect("production binary should execute");

    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "ledger recorded\n");

    let connection = rusqlite::Connection::open(data_directory.join("agens.db"))
        .expect("unified database should open");
    let (session_id, attempt_id): (i64, i64) = connection
        .query_row(
            "SELECT id, (SELECT id FROM session_attempts WHERE session_id = sessions.id)
             FROM sessions",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("a completed session and attempt should exist");
    let (tool, outcome, exit_code, recorded_session_id, recorded_attempt_id): (
        String,
        String,
        Option<i64>,
        i64,
        i64,
    ) = connection
        .query_row(
            "SELECT tool, outcome, exit_code, session_id, attempt_id FROM tool_result_facts",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .expect("exactly one fact row should be recorded for the bash call");

    assert_eq!(tool, "bash");
    assert_eq!(outcome, "succeeded");
    assert_eq!(exit_code, Some(0));
    assert_eq!(recorded_session_id, session_id);
    assert_eq!(recorded_attempt_id, attempt_id);

    server.join();
}

/// A hardcoded, defaulted, or zeroed identity would leave `production_binary_records_a_completed_native_call_in_the_evidence_ledger`
/// green, because a fresh database allocates `session_id = attempt_id = 1` for
/// its one call. This test runs two independent sessions against the SAME
/// data directory and asserts the ledger carries the two DIFFERENT identities
/// SQLite actually allocated, in allocation order, so it fails under any of
/// those mutations.
#[test]
fn production_binary_records_each_sessions_own_identity_in_the_evidence_ledger() {
    let temporary = TemporaryDirectory::new("production-evidence-ledger-identity");
    let project_root = temporary.path().join("project");
    let config_home = temporary.path().join("config");
    let data_directory = temporary.path().join("data");
    std::fs::create_dir_all(project_root.join(".git")).expect("project marker should exist");
    std::fs::create_dir_all(&config_home).expect("config directory should exist");

    let server = ScriptedNativeOpenAiMockServer::start(vec![
        ScriptedOpenAiResponse {
            required_body_fragments: vec!["native::bash".to_owned()],
            response: native_tool_call_response(
                "call_probe_one",
                "native::bash",
                r#"{"command":"exit 0"}"#,
            ),
        },
        ScriptedOpenAiResponse {
            required_body_fragments: vec!["\"call_id\":\"call_probe_one\"".to_owned()],
            response: text_response("first session recorded"),
        },
        ScriptedOpenAiResponse {
            required_body_fragments: vec!["native::bash".to_owned()],
            response: native_tool_call_response(
                "call_probe_two",
                "native::bash",
                r#"{"command":"exit 0"}"#,
            ),
        },
        ScriptedOpenAiResponse {
            required_body_fragments: vec!["\"call_id\":\"call_probe_two\"".to_owned()],
            response: text_response("second session recorded"),
        },
    ]);
    std::fs::write(
        config_home.join("config.toml"),
        format!(
            "[provider]\nmodel = \"openai-api/test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n[permissions]\nallow = [\"bash(*)\"]\n",
            server.base_url(),
            data_directory.display(),
        ),
    )
    .expect("config should be written");

    for prompt in ["run the first probe", "run the second probe"] {
        let output = isolated_agens_command(&temporary)
            .args(["chat", prompt])
            .current_dir(&project_root)
            .env("AGENS_CONFIG_HOME", &config_home)
            .env("OPENAI_API_KEY", "SENTINEL_OPENAI_API_KEY")
            .output()
            .expect("production binary should execute");

        assert!(output.status.success());
    }

    let connection = rusqlite::Connection::open(data_directory.join("agens.db"))
        .expect("unified database should open");
    let mut statement = connection
        .prepare("SELECT session_id, attempt_id, tool_call_id FROM tool_result_facts ORDER BY id")
        .expect("query should prepare");
    let rows: Vec<(i64, i64, String)> = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("query should run")
        .collect::<Result<_, _>>()
        .expect("rows should be readable");

    assert_eq!(
        rows,
        vec![
            (1, 1, "call_probe_one".to_owned()),
            (2, 2, "call_probe_two".to_owned()),
        ]
    );

    server.join();
}

#[test]
fn production_binary_recovers_from_mcp_infrastructure_failures_and_persists_completed_history() {
    // `timeout_ms` budgets the connect and the list as well as the call, so the
    // timeout case is bounded on both sides. Below, it must clear spawning the
    // child and completing the MCP handshake on a loaded machine: 200ms already
    // fails there, so do not tighten this. Above, it must stay well under the
    // five seconds `call-sleep` blocks for, or the call would answer in time and
    // the case would stop proving that a timeout is what produced the error.
    for (name, mode, timeout_ms, expected_tool_error) in [
        ("timeout", "call-sleep", 1_000, "tool operation timed out"),
        ("crash", "call-crash", 1_000, "tool infrastructure failure"),
        (
            "malformed protocol",
            "call-malformed",
            1_000,
            "tool infrastructure failure",
        ),
    ] {
        let temporary = TemporaryDirectory::new(&format!("production-mcp-{name}"));
        let project_root = temporary.path().join("project");
        let config_home = temporary.path().join("config");
        let data_directory = temporary.path().join("data");
        std::fs::create_dir_all(project_root.join(".git")).expect("project marker should exist");
        std::fs::create_dir_all(&config_home).expect("config directory should exist");

        let server = ScriptedNativeOpenAiMockServer::start(vec![
            ScriptedOpenAiResponse {
                required_body_fragments: vec!["files::first".to_owned()],
                response: native_tool_call_response(
                    "call_mcp_infrastructure",
                    "files::first",
                    r#"{}"#,
                ),
            },
            ScriptedOpenAiResponse {
                required_body_fragments: vec![
                    "\"call_id\":\"call_mcp_infrastructure\"".to_owned(),
                    format!("\"output\":{expected_tool_error:?}"),
                    "!SENTINEL_MCP_TRANSPORT".to_owned(),
                    "!SENTINEL_MCP_STDERR".to_owned(),
                ],
                response: text_response("MCP infrastructure failure handled"),
            },
        ]);
        std::fs::write(
            config_home.join("config.toml"),
            format!(
                "[provider]\nmodel = \"openai-api/test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n[mcp.files]\ntransport = \"stdio\"\ncommand = \"{}\"\nargs = [{mode:?}]\ntimeout_ms = {timeout_ms}\n[mcp.files.env]\nFAKE_MCP_TRANSPORT_SECRET = \"SENTINEL_MCP_TRANSPORT\"\nFAKE_MCP_STDERR_SECRET = \"SENTINEL_MCP_STDERR\"\n",
                server.base_url(),
                data_directory.display(),
                env!("CARGO_BIN_EXE_agens-cli-fake-mcp-child"),
            ),
        )
        .expect("config should be written");

        let output = isolated_agens_command(&temporary)
            .args(["chat", "--dangerously-allow-all", "run broken MCP tool"])
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

        assert!(output.status.success(), "{name}: {diagnostics}");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "MCP infrastructure failure handled\n",
            "{name}"
        );
        assert_eq!(String::from_utf8_lossy(&output.stderr), "", "{name}");
        for secret in [
            "SENTINEL_OPENAI_API_KEY",
            "SENTINEL_MCP_TRANSPORT",
            "SENTINEL_MCP_STDERR",
        ] {
            assert!(!diagnostics.contains(secret), "{name}: leaked {secret}");
        }

        let session = SessionStore::open(&data_directory)
            .expect("session store should open")
            .load_session_for_resume(1)
            .expect("completed session should be readable");
        assert_eq!(
            session.messages,
            vec![
                Message {
                    role: Role::User,
                    parts: vec![MessagePart::Text("run broken MCP tool".to_owned())],
                },
                Message {
                    role: Role::Assistant,
                    parts: vec![MessagePart::ToolCall {
                        id: "call_mcp_infrastructure".to_owned(),
                        name: "files::first".to_owned(),
                        input: r#"{}"#.to_owned(),
                    }],
                },
                Message {
                    role: Role::Tool,
                    parts: vec![MessagePart::ToolResult {
                        tool_call_id: "call_mcp_infrastructure".to_owned(),
                        content: expected_tool_error.to_owned(),
                        is_error: true,
                    }],
                },
                Message {
                    role: Role::Assistant,
                    parts: vec![MessagePart::Text(
                        "MCP infrastructure failure handled".to_owned(),
                    )],
                },
            ],
            "{name}"
        );
        assert_eq!(
            session
                .latest_attempt
                .as_ref()
                .map(agens_core::SessionAttemptSummary::status),
            Some(agens_core::SessionAttemptStatus::Completed),
            "{name}"
        );
        assert_sqlite_has_no_sentinels(
            &data_directory.join("agens.db"),
            &[
                "SENTINEL_OPENAI_API_KEY",
                "SENTINEL_MCP_TRANSPORT",
                "SENTINEL_MCP_STDERR",
            ],
        );

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
                "\"output\":\"permission denied\"".to_owned(),
            ],
            response: text_response("MCP denial handled"),
        },
    ]);
    std::fs::write(
        config_home.join("config.toml"),
        format!(
            "[provider]\nmodel = \"openai-api/test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n[permissions]\ndeny = [\"files_second(*)\"]\n\n[mcp.files]\ntransport = \"stdio\"\ncommand = \"{}\"\nargs = [\"success\"]\ntimeout_ms = 1000\n[mcp.files.env]\nFAKE_MCP_CALL_READY = \"{}\"\n",
            server.base_url(),
            data_directory.display(),
            env!("CARGO_BIN_EXE_agens-cli-fake-mcp-child"),
            call_marker.display(),
        ),
    )
    .expect("config should be written");

    let output = isolated_agens_command(&temporary)
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
        let server = ScriptedNativeOpenAiMockServer::start(if persists {
            vec![
                first_response,
                ScriptedOpenAiResponse {
                    required_body_fragments: vec![
                        "\"call_id\":\"call_mcp_permission\"".to_owned(),
                        if executes {
                            "tool succeeded".to_owned()
                        } else {
                            "\"output\":\"permission denied\"".to_owned()
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
                "[provider]\nmodel = \"openai-api/test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n{permissions}\n[mcp.files]\ntransport = \"stdio\"\ncommand = \"{}\"\nargs = [\"success\"]\ntimeout_ms = 1000\n[mcp.files.env]\nFAKE_MCP_CALL_READY = \"{}\"\n",
                server.base_url(),
                data_directory.display(),
                env!("CARGO_BIN_EXE_agens-cli-fake-mcp-child"),
                call_marker.display(),
            ),
        )
        .expect("config should be written");

        let mut command = isolated_agens_command(&temporary);
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
                    &isolated_agens_command(&temporary)
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
            assert_no_saved_sessions(&temporary, &project_root, &config_home);
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
    for (name, responses, expected_calls, expected_error) in [
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
            "error: provider: provider request failed\n",
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
            "error: provider: provider response protocol failed\n",
        ),
    ] {
        let temporary = TemporaryDirectory::new(&format!("production-mcp-integrity-{name}"));
        let project_root = temporary.path().join("project");
        let config_home = temporary.path().join("config");
        let data_directory = temporary.path().join("data");
        let call_marker = temporary.path().join("mcp-call-count");
        std::fs::create_dir_all(project_root.join(".git")).expect("project marker should exist");
        std::fs::create_dir_all(&config_home).expect("config directory should exist");
        let server = ScriptedNativeOpenAiMockServer::start(responses);
        std::fs::write(
            config_home.join("config.toml"),
            format!(
                "[provider]\nmodel = \"openai-api/test-model\"\nbase_url = \"{}\"\n\n[options]\ndata_dir = \"{}\"\n\n[mcp.files]\ntransport = \"stdio\"\ncommand = \"{}\"\nargs = [\"success\"]\ntimeout_ms = 1000\n[mcp.files.env]\nFAKE_MCP_CALL_READY = \"{}\"\n",
                server.base_url(),
                data_directory.display(),
                env!("CARGO_BIN_EXE_agens-cli-fake-mcp-child"),
                call_marker.display(),
            ),
        )
        .expect("config should be written");

        let output = isolated_agens_command(&temporary)
            .args(["chat", "--dangerously-allow-all", "reject MCP replay"])
            .current_dir(&project_root)
            .env("AGENS_CONFIG_HOME", &config_home)
            .env("OPENAI_API_KEY", "SENTINEL_OPENAI_API_KEY")
            .output()
            .expect("production binary should execute");

        assert_eq!(output.status.code(), Some(1), "{name}");
        assert_eq!(String::from_utf8_lossy(&output.stdout), "", "{name}");
        assert_diagnostic_error(&output.stderr, expected_error);
        assert_eq!(
            call_marker
                .exists()
                .then(|| std::fs::read_to_string(&call_marker)
                    .expect("MCP marker should be readable"))
                .as_deref(),
            expected_calls,
            "{name}"
        );
        if expected_calls.is_some() {
            assert_interrupted_session_saved(
                &temporary,
                &project_root,
                &config_home,
                "reject MCP replay",
            );
        } else {
            assert_no_saved_sessions(&temporary, &project_root, &config_home);
        }

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

const PRODUCTION_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

trait BoundedCommandOutput {
    fn bounded_output(&mut self, timeout: Duration) -> std::io::Result<Output>;
}

impl BoundedCommandOutput for Command {
    fn bounded_output(&mut self, timeout: Duration) -> std::io::Result<Output> {
        #[cfg(unix)]
        self.process_group(0);

        let mut child = self
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let process_id = child.id();
        let stdout_reader = spawn_output_reader(
            child
                .stdout
                .take()
                .expect("bounded command stdout should be piped"),
        );
        let stderr_reader = spawn_output_reader(
            child
                .stderr
                .take()
                .expect("bounded command stderr should be piped"),
        );
        let deadline = Instant::now() + timeout;
        let mut status = None;

        loop {
            if status.is_none() {
                match child.try_wait() {
                    Ok(observed) => status = observed,
                    Err(error) => {
                        return match cleanup_bounded_command(
                            &mut child,
                            process_id,
                            false,
                            stdout_reader,
                            stderr_reader,
                        ) {
                            Ok(()) => Err(error),
                            Err(cleanup) => Err(std::io::Error::new(
                                cleanup.kind(),
                                format!(
                                    "bounded command wait failed: {error}; cleanup failed: {cleanup}"
                                ),
                            )),
                        };
                    }
                }
            }
            if let Some(status) = status
                && stdout_reader.is_finished()
                && stderr_reader.is_finished()
            {
                let (stdout, stderr) = join_output_readers(stdout_reader, stderr_reader)?;
                return Ok(Output {
                    status,
                    stdout,
                    stderr,
                });
            }
            if Instant::now() >= deadline {
                cleanup_bounded_command(
                    &mut child,
                    process_id,
                    status.is_some(),
                    stdout_reader,
                    stderr_reader,
                )?;

                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("production command exceeded {timeout:?}"),
                ));
            }

            thread::sleep(Duration::from_millis(5));
        }
    }
}

fn cleanup_bounded_command(
    child: &mut Child,
    process_id: u32,
    child_reaped: bool,
    stdout_reader: thread::JoinHandle<std::io::Result<Vec<u8>>>,
    stderr_reader: thread::JoinHandle<std::io::Result<Vec<u8>>>,
) -> std::io::Result<()> {
    let termination = terminate_process_group(child, process_id);
    let wait = if child_reaped {
        Ok(())
    } else {
        child.wait().map(|_| ())
    };
    let readers = join_output_readers(stdout_reader, stderr_reader).map(|_| ());

    termination?;
    wait?;
    readers
}

fn spawn_output_reader<R>(mut reader: R) -> thread::JoinHandle<std::io::Result<Vec<u8>>>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut output = Vec::new();
        reader.read_to_end(&mut output)?;
        Ok(output)
    })
}

fn join_output_reader(
    reader: thread::JoinHandle<std::io::Result<Vec<u8>>>,
) -> std::io::Result<Vec<u8>> {
    reader
        .join()
        .map_err(|_| std::io::Error::other("bounded command output reader panicked"))?
}

fn join_output_readers(
    stdout: thread::JoinHandle<std::io::Result<Vec<u8>>>,
    stderr: thread::JoinHandle<std::io::Result<Vec<u8>>>,
) -> std::io::Result<(Vec<u8>, Vec<u8>)> {
    let stdout = join_output_reader(stdout);
    let stderr = join_output_reader(stderr);

    Ok((stdout?, stderr?))
}

#[cfg(unix)]
fn terminate_process_group(child: &mut Child, process_id: u32) -> std::io::Result<()> {
    let process_group = i32::try_from(process_id)
        .map_err(|_| std::io::Error::other("bounded command process ID is invalid"))?;

    // SAFETY: the child was spawned as the leader of a new process group, and the negative PID
    // targets that group without granting access to unrelated processes.
    let result = unsafe { libc::kill(-process_group, libc::SIGKILL) };
    if result == 0 {
        return Ok(());
    }

    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        let _ = child.kill();
        Err(error)
    }
}

#[cfg(not(unix))]
fn terminate_process_group(child: &mut Child, _process_id: u32) -> std::io::Result<()> {
    child.kill()
}

struct IsolatedAgensCommand {
    command: Command,
}

impl IsolatedAgensCommand {
    fn command(&self) -> &Command {
        &self.command
    }

    fn arg<S>(&mut self, argument: S) -> &mut Self
    where
        S: AsRef<OsStr>,
    {
        self.command.arg(argument);
        self
    }

    fn args<I, S>(&mut self, arguments: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.command.args(arguments);
        self
    }

    fn current_dir<P>(&mut self, directory: P) -> &mut Self
    where
        P: AsRef<Path>,
    {
        self.command.current_dir(directory);
        self
    }

    fn env<K, V>(&mut self, key: K, value: V) -> &mut Self
    where
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.command.env(key, value);
        self
    }

    fn env_remove<K>(&mut self, key: K) -> &mut Self
    where
        K: AsRef<OsStr>,
    {
        self.command.env_remove(key);
        self
    }

    fn stdin<T>(&mut self, configuration: T) -> &mut Self
    where
        T: Into<Stdio>,
    {
        self.command.stdin(configuration);
        self
    }

    fn stdout<T>(&mut self, configuration: T) -> &mut Self
    where
        T: Into<Stdio>,
    {
        self.command.stdout(configuration);
        self
    }

    fn stderr<T>(&mut self, configuration: T) -> &mut Self
    where
        T: Into<Stdio>,
    {
        self.command.stderr(configuration);
        self
    }

    fn spawn(&mut self) -> std::io::Result<Child> {
        self.command.spawn()
    }

    fn output(&mut self) -> std::io::Result<Output> {
        self.command.bounded_output(PRODUCTION_COMMAND_TIMEOUT)
    }
}

fn isolated_agens_command(temporary: &TemporaryDirectory) -> IsolatedAgensCommand {
    static NEXT_COMMAND: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    let sequence = NEXT_COMMAND.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let process_root = temporary.path().join(format!("command-{sequence}"));
    let home = process_root.join("home");
    let xdg_config_home = process_root.join("xdg-config");
    let xdg_data_home = process_root.join("xdg-data");
    let agens_config_home = process_root.join("agens-config");

    for directory in [&home, &xdg_config_home, &xdg_data_home, &agens_config_home] {
        std::fs::create_dir_all(directory).expect("isolated command root should be created");
    }

    let mut command = Command::new(env!("CARGO_BIN_EXE_agens"));
    command
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", xdg_config_home)
        .env("XDG_DATA_HOME", xdg_data_home)
        .env("AGENS_CONFIG_HOME", agens_config_home);

    // Isolating the configuration roots leaves the inherited environment
    // untouched, so a developer machine with a real key exported still lets a
    // run authenticate for real. Removing the credentials and routing every
    // non-loopback request into a tripwire is what makes "this test cannot
    // reach a real provider" true rather than assumed. A test that needs a key
    // sets its own afterwards, and that value wins.
    for variable in agens_fixtures::PROVIDER_CREDENTIAL_VARIABLES {
        command.env_remove(variable);
    }
    for (variable, value) in agens_fixtures::NetworkTripwire::shared().environment() {
        command.env(variable, value);
    }

    IsolatedAgensCommand { command }
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

/// A `/responses` endpoint that answers one turn and reports the credential
/// the run authenticated with.
struct OpenAiMockServer {
    provider: ScriptedProvider,
    expected_authorization: String,
}

struct ScriptedOpenAiResponse {
    required_body_fragments: Vec<String>,
    response: String,
}

/// A scripted `/responses` endpoint over the shared journey fake.
///
/// The fragment expectations are checked in `join`, against the request each
/// scripted round actually answered, rather than inside the server thread: a
/// mismatch is then the test's own failure with the whole conversation in
/// hand, not an opaque worker panic.
struct ScriptedNativeOpenAiMockServer {
    provider: ScriptedProvider,
    expectations: Vec<Vec<String>>,
}

impl ScriptedNativeOpenAiMockServer {
    fn start(responses: Vec<ScriptedOpenAiResponse>) -> Self {
        let mut expectations = Vec::with_capacity(responses.len());
        let mut turns = Vec::with_capacity(responses.len());
        for scripted in responses {
            expectations.push(scripted.required_body_fragments);
            turns.push(ScriptedTurn::raw(scripted.response));
        }

        Self {
            provider: ScriptedProvider::start(ScriptedDialect::Responses, Script::new(turns)),
            expectations,
        }
    }

    fn base_url(&self) -> String {
        self.provider.base_url()
    }

    fn join(self) {
        let requests = self.provider.wait_for_requests(self.expectations.len());
        // An unscripted request is what the bounded variant of this server used
        // to look for by waiting a quarter of a second after the last scripted
        // round. The fake records it instead, and every caller joins after the
        // run has already exited, so the same guarantee costs no wall clock.
        self.provider.assert_script_consumed();

        for (round, (fragments, request)) in self.expectations.iter().zip(&requests).enumerate() {
            assert_scripted_request_fragments(round, request.body(), fragments);
        }
    }
}

/// Checks one scripted round's expectations against the request it answered.
///
/// A bare fragment must appear, `!` requires its absence, `@once:` pins a
/// single occurrence, and `@all-tools-non-strict` checks the declared tool
/// surface as a whole.
fn assert_scripted_request_fragments(round: usize, body: &str, fragments: &[String]) {
    for fragment in fragments {
        if fragment == "@all-tools-non-strict" {
            let payload: serde_json::Value =
                serde_json::from_str(body).expect("production provider payload should be JSON");
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
                "round {round} request body leaked {forbidden:?}: {body}"
            );
            continue;
        }
        if let Some(once) = fragment.strip_prefix("@once:") {
            let visible = model_visible_fragment(once);
            let occurrences = body.matches(&visible).count();
            assert_eq!(
                occurrences, 1,
                "round {round} request body should contain {visible:?} exactly once, found {occurrences}: {body}"
            );
            continue;
        }
        let visible = model_visible_fragment(fragment);
        assert!(
            body.contains(&visible),
            "round {round} request body should contain {visible:?}: {body}"
        );
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
        Self {
            provider: ScriptedProvider::start(
                ScriptedDialect::Responses,
                Script::new([ScriptedTurn::text("Hello from OpenAI")]),
            ),
            expected_authorization: format!("Bearer {api_key}"),
        }
    }

    fn base_url(&self) -> String {
        self.provider.base_url()
    }

    fn join(self) {
        let requests = self.provider.wait_for_requests(1);
        self.provider.assert_script_consumed();
        assert_eq!(requests[0].target(), "/responses");
        assert_eq!(
            requests[0].header("authorization"),
            Some(self.expected_authorization.as_str())
        );
    }
}

struct StalledOpenAiMockServer {
    provider: ScriptedProvider,
}

/// The same, one level down: the parent delegates and the child's turn is the
/// one left hanging.
struct TaskStalledOpenAiMockServer {
    provider: ScriptedProvider,
}

impl StalledOpenAiMockServer {
    fn start() -> Self {
        Self {
            provider: ScriptedProvider::start(
                ScriptedDialect::Responses,
                Script::new([ScriptedTurn::stall(STALLED_TURN)]),
            ),
        }
    }

    fn base_url(&self) -> String {
        self.provider.base_url()
    }

    fn wait_for_request(&mut self) {
        let requests = self.provider.wait_for_requests(1);
        assert!(
            matches!(requests[0].target(), "/responses" | "/codex/responses"),
            "unexpected stalled target: {}",
            requests[0].target()
        );
    }

    fn join(self) {
        self.provider.assert_script_consumed();
    }
}

impl TaskStalledOpenAiMockServer {
    fn start() -> Self {
        Self {
            provider: ScriptedProvider::start(
                ScriptedDialect::Responses,
                Script::new([ScriptedTurn::raw(native_tool_call_response(
                    "task-cancel",
                    "task",
                    r#"{"agent":"reviewer","description":"child cancellation request"}"#,
                ))])
                .with_child(
                    "child cancellation request",
                    [ScriptedTurn::stall(STALLED_TURN)],
                ),
            ),
        }
    }

    fn base_url(&self) -> String {
        self.provider.base_url()
    }

    fn wait_for_child_request(&mut self) {
        self.provider.wait_for_requests(2);
    }

    /// Checks the delegation the parent sent and the scope the child ran in.
    ///
    /// The reviewer agent declares no `permissions:`, so it inherits the
    /// parent's full native surface (write/bash/webfetch included) unlike
    /// `explore`, which narrows explicitly. It also carries `task`: a child may
    /// delegate one level further, and the chain stops at the grandchild —
    /// which this request cannot observe, so the depth limit itself is pinned
    /// in `delegation_reaches_a_grandchild_and_stops_there`.
    fn join(self) {
        let requests = self.provider.requests();
        // A parent that continued past the cancelled child would have sent a
        // third request, and the fake would have recorded it as unscripted.
        self.provider.assert_script_consumed();
        assert!(requests[0].body().contains("parent task"));

        let child = requests[1].body();
        assert!(
            requests[1].is_child(),
            "the child ran its own turn: {child}"
        );
        for forbidden in ["parent task cancellation", "mcp"] {
            assert!(
                !child.contains(forbidden),
                "child request leaked {forbidden:?}: {child}"
            );
        }
        for expected in ["write", "bash", "webfetch", "\"name\":\"task\""] {
            assert!(
                child.contains(expected),
                "child request should inherit the parent's full native surface, missing {expected:?}: {child}"
            );
        }
        assert_eq!(
            child.matches("\"name\":\"task_control\"").count(),
            1,
            "the child's own execution-scoped task_control must not be joined by \
             the main-scoped one: {child}"
        );
    }
}

struct ErrorOpenAiMockServer {
    provider: ScriptedProvider,
}

impl ErrorOpenAiMockServer {
    fn start() -> Self {
        Self {
            provider: ScriptedProvider::start(
                ScriptedDialect::Responses,
                // A server failure is transient, so the run spends its whole
                // retry budget against an endpoint that keeps failing.
                Script::new(std::iter::repeat_n(
                    ScriptedTurn::raw(concat!(
                        "HTTP/1.1 500 Internal Server Error\r\n",
                        "X-Remote-Secret: SENTINEL_REMOTE_ERROR_HEADER\r\n",
                        "Content-Length: 26\r\nConnection: close\r\n\r\n",
                        "SENTINEL_REMOTE_ERROR_BODY"
                    )),
                    3,
                )),
            ),
        }
    }

    fn base_url(&self) -> String {
        self.provider.base_url()
    }

    fn join(self) {
        let requests = self.provider.wait_for_requests(3);
        self.provider.assert_script_consumed();
        assert_eq!(requests[0].target(), "/responses");
    }
}

/// How long a stalled endpoint holds a turn open. The run under test
/// interrupts it well before this; the bound only keeps a run that never
/// interrupts from hanging the suite.
const STALLED_TURN: Duration = Duration::from_secs(5);

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

/// Blocks until the run under test writes the marker saying it has reached the
/// work the caller is about to interrupt.
///
/// The bound proves nothing by itself: the signal, the exit status, and the
/// absence of a persisted turn are what prove cancellation. It is here so that a
/// run which never reaches that work fails loudly instead of hanging, so it must
/// stay far above spawning the binary, resolving configuration, and completing
/// the provider round trip on a loaded machine — two seconds did not, and was
/// the largest single source of failures in this file under load.
///
/// Widening it does not widen the window between the marker and the interrupt,
/// which is what the cancellation cases are actually sensitive to: the caller
/// signals as soon as the marker appears, and the shared helper polls more
/// tightly than this one used to.
fn wait_for_path(path: &std::path::Path) {
    agens_fixtures::wait_for(&path.display().to_string(), || path.exists().then_some(()));
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

fn assert_no_saved_sessions(
    temporary: &TemporaryDirectory,
    project_root: &std::path::Path,
    config_home: &std::path::Path,
) {
    let sessions = isolated_agens_command(temporary)
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

fn assert_interrupted_session_saved(
    temporary: &TemporaryDirectory,
    project_root: &std::path::Path,
    config_home: &std::path::Path,
    title: &str,
) {
    let sessions = isolated_agens_command(temporary)
        .args(["sessions", "list"])
        .current_dir(project_root)
        .env("AGENS_CONFIG_HOME", config_home)
        .output()
        .expect("sessions command should execute");

    assert!(sessions.status.success());
    let stdout = String::from_utf8_lossy(&sessions.stdout);
    let listed = stdout.lines().collect::<Vec<_>>();
    assert_eq!(listed.len(), 2, "{stdout:?}");
    assert!(
        listed[1].ends_with(&format!("\t{title}\tprimary\t1")),
        "{stdout:?}"
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
    if let Some(data_directory) = database.parent() {
        assert_diagnostics_have_no_sentinels(data_directory, sentinels);
    }
}

fn assert_diagnostics_have_no_sentinels(data_directory: &std::path::Path, sentinels: &[&str]) {
    let diagnostics = data_directory.join("diagnostics");
    let Ok(entries) = std::fs::read_dir(diagnostics) else {
        return;
    };
    for entry in entries {
        let entry = entry.expect("diagnostic entry should be readable");
        let metadata = entry
            .metadata()
            .expect("diagnostic metadata should be readable");
        if !metadata.is_file() {
            continue;
        }
        let content = std::fs::read(entry.path()).expect("diagnostic file should be readable");
        for sentinel in sentinels {
            assert!(
                !content
                    .windows(sentinel.len())
                    .any(|window| window == sentinel.as_bytes()),
                "diagnostics leaked {sentinel}"
            );
        }
    }
}

fn diagnostic_json_events(data_directory: &std::path::Path) -> Vec<serde_json::Value> {
    let diagnostics = data_directory.join("diagnostics");
    let Ok(entries) = std::fs::read_dir(diagnostics) else {
        return Vec::new();
    };
    let mut events = Vec::new();
    for entry in entries {
        let entry = entry.expect("diagnostic entry should be readable");
        if !entry
            .metadata()
            .expect("diagnostic metadata should be readable")
            .is_file()
        {
            continue;
        }
        let content = std::fs::read_to_string(entry.path())
            .expect("diagnostic JSONL should be readable text");
        events.extend(
            content.lines().map(|line| {
                serde_json::from_str(line).expect("diagnostic line should be valid JSON")
            }),
        );
    }
    events
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

fn assert_sqlite_has_partial_turn(database: &std::path::Path, expected_messages: i64) {
    assert!(database.exists(), "session database should exist");

    let connection = rusqlite::Connection::open(database).expect("session database should open");
    let counts = connection
        .query_row(
            "SELECT
                 (SELECT COUNT(*) FROM session_attempts WHERE status != 'running'),
                 (SELECT COUNT(*) FROM session_attempts WHERE retry_prompt IS NOT NULL),
                 (SELECT COUNT(*) FROM turns),
                 (SELECT COUNT(*) FROM messages)",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .expect("partial attempt should be queryable");

    assert_eq!(counts, (1, 0, 1, expected_messages));
}

fn assert_sqlite_has_interrupted_turn(database: &std::path::Path) {
    assert!(database.exists(), "session database should exist");

    let connection = rusqlite::Connection::open(database).expect("session database should open");
    let counts = connection
        .query_row(
            "SELECT
                 (SELECT COUNT(*) FROM session_attempts WHERE status != 'running'),
                 (SELECT COUNT(*) FROM session_attempts WHERE retry_prompt IS NOT NULL),
                 (SELECT COUNT(*) FROM turns),
                 (SELECT COUNT(*) FROM messages)",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .expect("interrupted attempt should be queryable");

    assert_eq!(counts, (1, 0, 1, 2));
}

// Journeys: the whole loop against a scripted model.
//
// Everything below runs the production binary against `ScriptedProvider`, so
// the turn machinery, the tools, the session store and the process boundary
// are all real and only the model is a fixture. Each journey asserts on the
// requests the agent sent, not only on what it printed, and finishes by
// checking that the script was consumed and that nothing left the fake.

/// A journey's isolated project, configuration and data roots.
struct Journey {
    temporary: TemporaryDirectory,
    project_root: PathBuf,
    config_home: PathBuf,
    data_directory: PathBuf,
}

impl Journey {
    fn new(name: &str) -> Self {
        let temporary = TemporaryDirectory::new(name);
        let project_root = temporary.path().join("project");
        let config_home = temporary.path().join("config");
        let data_directory = temporary.path().join("data");

        std::fs::create_dir_all(project_root.join(".git")).expect("project marker should exist");
        std::fs::create_dir_all(&config_home).expect("config directory should exist");

        Self {
            temporary,
            project_root,
            config_home,
            data_directory,
        }
    }

    fn write_project_file(&self, name: &str, contents: &str) {
        std::fs::write(self.project_root.join(name), contents)
            .expect("journey project file should be written");
    }

    /// Runs `agens chat <prompt>` against the scripted provider.
    fn chat(&self, provider: &ScriptedProvider, prompt: &str, extra_configuration: &str) -> Output {
        provider.write_configuration(&self.config_home, &self.data_directory, extra_configuration);

        isolated_agens_command(&self.temporary)
            .current_dir(&self.project_root)
            .args(["chat", prompt])
            .env("AGENS_CONFIG_HOME", &self.config_home)
            .output()
            .expect("production binary should run the journey")
    }
}

/// The journey's stdout, reporting the run's own output and the conversation
/// the scripted provider observed when it failed.
///
/// A journey fails inside a process whose only report is an exit status, so
/// the requests are the difference between "the loop went wrong" and knowing
/// which turn it went wrong on.
fn journey_stdout(result: &Output, provider: &ScriptedProvider) -> String {
    assert!(
        result.status.success(),
        "journey failed: {}{}\nobserved conversation: {:#?}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr),
        provider
            .requests()
            .iter()
            .map(|request| {
                (
                    if request.is_child() { "child" } else { "main" },
                    request.body().chars().take(240).collect::<String>(),
                )
            })
            .collect::<Vec<_>>()
    );

    String::from_utf8_lossy(&result.stdout).into_owned()
}

#[test]
fn journey_tool_loop_returns_the_tool_result_to_the_model_and_closes() {
    let journey = Journey::new("journey-tool-loop");
    journey.write_project_file("notes.md", "the note the model asked for");
    let provider = ScriptedProvider::start(
        ScriptedDialect::Responses,
        Script::new([
            ScriptedTurn::tool_call("call-read", "read", r#"{"path":"notes.md"}"#),
            ScriptedTurn::text("summarised"),
        ]),
    );

    let result = journey.chat(
        &provider,
        "summarise the notes",
        "\n[permissions]\nallow = [\"read(*)\"]\n",
    );

    assert_eq!(journey_stdout(&result, &provider), "summarised\n");
    provider.assert_script_consumed();

    let requests = provider.requests();
    assert_eq!(requests.len(), 2, "a tool loop is two requests");
    assert!(
        requests[1].body().contains("the note the model asked for"),
        "the tool result should reach the model: {}",
        requests[1].body()
    );
    assert!(
        requests[1].body().contains("call-read"),
        "the tool result should be tied to the call it answers: {}",
        requests[1].body()
    );
    NetworkTripwire::shared().assert_no_connections();
}

/// AGN-102: a failure the model cannot read is a failure it cannot recover
/// from, so the tool's own text has to survive to the next request rather than
/// being flattened into a generic "tool failed".
#[test]
fn journey_tool_failure_reaches_the_model_with_the_failure_text_intact() {
    let journey = Journey::new("journey-tool-failure");
    let provider = ScriptedProvider::start(
        ScriptedDialect::Responses,
        Script::new([
            ScriptedTurn::tool_call("call-missing", "read", r#"{"path":"absent.md"}"#),
            ScriptedTurn::text("reported the failure"),
        ]),
    );

    let result = journey.chat(
        &provider,
        "read a file that is not there",
        "\n[permissions]\nallow = [\"read(*)\"]\n",
    );

    assert_eq!(journey_stdout(&result, &provider), "reported the failure\n");
    provider.assert_script_consumed();

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let failure = requests[1].json()["input"][0]["output"]
        .as_str()
        .expect("the continuation should carry the tool result")
        .to_owned();
    assert_eq!(
        failure, "read: file not found",
        "the tool's own failure text should reach the model, naming both the \
         operation and the reason, rather than a flattened generic failure"
    );
    NetworkTripwire::shared().assert_no_connections();
}

/// AGN-105: a delegated child runs its own conversation, and the point of the
/// journey is that the child's scope is its own — it neither inherits the
/// parent's prompt nor widens past the toolset its agent declares.
#[test]
fn journey_delegation_serves_the_child_its_own_script_within_its_own_scope() {
    let journey = Journey::new("journey-delegation");
    journey.write_project_file("notes.md", "the note the child read");
    std::fs::create_dir_all(journey.config_home.join("agents"))
        .expect("agents directory should exist");
    std::fs::write(
        journey.config_home.join("agents/reviewer.md"),
        "---\nname: reviewer\ndescription: Review implementation\nmode: subagent\nmodel: gpt-4o\npermissions: []\n---\nYou are the isolated reviewer.\n",
    )
    .expect("subagent definition should be written");

    let provider = ScriptedProvider::start(
        ScriptedDialect::Responses,
        Script::new([
            ScriptedTurn::tool_call(
                "call-task",
                "task",
                r#"{"agent":"reviewer","description":"child request"}"#,
            ),
            ScriptedTurn::text("parent answer"),
        ])
        .with_child(
            "child request",
            [
                ScriptedTurn::tool_call("call-child-read", "read", r#"{"path":"notes.md"}"#),
                ScriptedTurn::text("child answer"),
            ],
        ),
    );

    let result = journey.chat(
        &provider,
        "parent request",
        "\n[permissions]\nallow = [\"task(reviewer)\", \"read(*)\"]\n",
    );

    assert_eq!(journey_stdout(&result, &provider), "parent answer\n");
    provider.assert_script_consumed();

    let child_requests = provider.child_requests();
    assert_eq!(
        child_requests.len(),
        2,
        "the child ran its own two-turn loop"
    );
    let opening = child_requests[0].body();
    assert!(
        opening.contains("You are the isolated reviewer."),
        "the child should run under its own agent's prompt: {opening}"
    );
    assert!(
        !opening.contains("parent request"),
        "the child should not inherit the parent's prompt: {opening}"
    );
    assert!(
        !opening.contains("\"name\":\"mcp"),
        "the child should not reach the parent's MCP surface: {opening}"
    );
    assert!(
        child_requests[1].body().contains("the note the child read"),
        "the child's own tool result should reach it: {}",
        child_requests[1].body()
    );
    NetworkTripwire::shared().assert_no_connections();
}

/// AGN-104: a stream cut mid-turn should be retried within a visible budget
/// rather than surfacing as a bare failure. Ignored until that budget exists;
/// the journey is written against the behaviour AGN-104 defines so the fix can
/// turn it on.
#[test]
#[ignore = "AGN-104"]
fn journey_retry_mid_stream_resumes_within_its_budget_and_reports_the_attempt() {
    let journey = Journey::new("journey-retry-mid-stream");
    let provider = ScriptedProvider::start(
        ScriptedDialect::Responses,
        Script::new([ScriptedTurn::truncate(), ScriptedTurn::text("recovered")]),
    );

    let result = journey.chat(&provider, "answer through a cut stream", "");

    assert_eq!(journey_stdout(&result, &provider), "recovered\n");
    provider.assert_script_consumed();
    assert_eq!(
        provider.requests().len(),
        2,
        "the cut stream should cost exactly one retry"
    );
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("retry"),
        "the retry should be visible rather than silent: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    NetworkTripwire::shared().assert_no_connections();
}

/// A journey run with the credential variables still exported is a journey
/// that can bill a real provider, so the isolation itself is asserted rather
/// than assumed.
#[test]
fn journey_runs_without_any_real_credential_in_its_environment() {
    let journey = Journey::new("journey-credential-isolation");
    let provider = ScriptedProvider::start(
        ScriptedDialect::Responses,
        Script::new([ScriptedTurn::text("answered from the fake")]),
    );
    provider.write_configuration(&journey.config_home, &journey.data_directory, "");

    let mut command = isolated_agens_command(&journey.temporary);
    // An inherited variable is absent from `get_envs` entirely, so checking that
    // none is set would pass on a machine that leaks one. What proves the
    // isolation is the removal entry itself: a `None` value is the command
    // saying it will unset that variable whatever the parent exported.
    let mut removed: Vec<&str> = command
        .command()
        .get_envs()
        .filter_map(|(name, value)| {
            let name = name.to_str()?;
            let removed = agens_fixtures::PROVIDER_CREDENTIAL_VARIABLES
                .into_iter()
                .find(|credential| *credential == name)?;
            value.is_none().then_some(removed)
        })
        .collect();
    let mut expected = Vec::from(agens_fixtures::PROVIDER_CREDENTIAL_VARIABLES);
    removed.sort_unstable();
    expected.sort_unstable();
    let result = command
        .current_dir(&journey.project_root)
        .args(["chat", "answer from the fake only"])
        .env("AGENS_CONFIG_HOME", &journey.config_home)
        .output()
        .expect("production binary should run the journey");

    assert_eq!(
        removed, expected,
        "an isolated journey must unset every provider credential variable"
    );
    assert_eq!(
        journey_stdout(&result, &provider),
        "answered from the fake\n"
    );
    provider.assert_script_consumed();
    NetworkTripwire::shared().assert_no_connections();
}
