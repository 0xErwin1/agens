//! Black-box characterization suite for the crate's public argv contract
//! (`cli-clap-and-module-split`, Phase 0).
//!
//! This file pins what the CURRENT hand-rolled parser in `lib.rs` actually
//! returns, not what it "should" return. Every assertion is a full-equality
//! check on `(status, stdout, stderr)`. If a case here fails after a later
//! phase, the parser's observable behavior changed and that is a defect,
//! unless the case belongs to [`parser_surface_baseline`].

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;

use agens::{CliDependencies, CommandResult, ExitStatus, execute, execute_os};

/// A private, per-case working directory. Real disk is only touched by
/// commands that call `bootstrap` (session storage, credential files); every
/// path handed to those commands lives under this directory so a case can
/// never read or write outside its own sandbox.
struct TemporaryDirectory {
    path: PathBuf,
}

impl TemporaryDirectory {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "agens-cli-contract-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock should be after Unix epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).expect("temporary directory should be created");
        Self { path }
    }

    fn project_root(&self) -> PathBuf {
        self.path.join("project")
    }

    fn home(&self) -> PathBuf {
        self.path.join("home")
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// `CliDependencies::for_test` with no injected files or environment: an
/// empty global/project configuration and no stored credentials.
fn base_dependencies(temporary: &TemporaryDirectory) -> CliDependencies {
    CliDependencies::for_test(
        temporary.project_root(),
        Some(temporary.home()),
        BTreeMap::new(),
        BTreeMap::new(),
    )
}

/// One pinned argv scenario: the exact dependencies it runs against and the
/// exact `CommandResult` it must produce. `_temporary` has no reader; it
/// exists so the case's sandbox directory outlives the call to `execute`.
struct Case {
    name: &'static str,
    argv: Vec<String>,
    dependencies: CliDependencies,
    expected: CommandResult,
    _temporary: TemporaryDirectory,
}

fn run_table_a(cases: Vec<Case>) {
    for case in cases {
        let actual = execute(case.argv.iter().map(String::as_str), &case.dependencies);
        assert_eq!(
            actual, case.expected,
            "case `{}` diverged from the pinned Phase-0 baseline",
            case.name
        );
    }
}

fn argv(words: &[&str]) -> Vec<String> {
    words.iter().map(|word| (*word).to_owned()).collect()
}

fn success(stdout: impl Into<String>) -> CommandResult {
    CommandResult {
        status: ExitStatus::Success,
        stdout: stdout.into(),
        stderr: String::new(),
    }
}

fn failure(status: ExitStatus, stderr: impl Into<String>) -> CommandResult {
    CommandResult {
        status,
        stdout: String::new(),
        stderr: format!("error: {}\n", stderr.into()),
    }
}

/// `config doctor`'s report over an empty global and project configuration.
/// Every catalog setting therefore falls back to its documented default.
const CONFIG_DOCTOR_DEFAULT_SETTINGS: &str = concat!(
    "Settings:\n",
    "  options.debug               true          default\n",
    "  options.data_dir            -             default\n",
    "  provider.type               -             default\n",
    "  provider.model              -             default\n",
    "  provider.base_url           -             default\n",
    "  agent.system_prompt         -             default\n",
    "  agent.max_iterations        -             default\n",
    "  agent.parallel_tool_calls   true          default\n",
    "  agent.default_agent         -             default\n",
    "  agent.reasoning_effort      -             default\n",
    "  ui.collapse_thinking        false         default\n",
    "  tools.max_list_entries      1000          default\n",
    "  tools.max_search_entries    10000         default\n",
    "  tools.max_search_results    100           default\n",
    "  tools.max_search_depth      32            default\n",
    "  tools.operation_timeout_ms  5000          default\n",
    "  tools.bash_timeout_ms       120000        default\n",
    "  subagents.max_iterations    16            default\n",
    "  subagents.max_concurrency   4             default\n",
    "  subagents.max_output_chars  65536         default\n",
    "  mcp_defaults.timeout_ms     10000         default\n",
    "  mcp_defaults.max_retries    0             default\n",
);

fn config_global_config_path(temporary: &TemporaryDirectory) -> PathBuf {
    temporary.home().join(".config/agens/config.toml")
}

fn config_project_config_path(temporary: &TemporaryDirectory) -> PathBuf {
    temporary.project_root().join(".agens/config.toml")
}

#[test]
fn table_a_root_shapes_hold() {
    let cases = vec![
        {
            let temporary = TemporaryDirectory::new("root-empty");
            let dependencies = base_dependencies(&temporary)
                .with_tui_launcher(|_, resume| Ok(format!("resume={resume:?}")));
            Case {
                name: "[] resumes with no session",
                argv: argv(&[]),
                dependencies,
                expected: success("resume=None\n"),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("root-resume-flag-alone");
            let dependencies = base_dependencies(&temporary)
                .with_tui_launcher(|_, resume| Ok(format!("resume={resume:?}")));
            Case {
                name: "--resume alone resumes with no session",
                argv: argv(&["--resume"]),
                dependencies,
                expected: success("resume=None\n"),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("root-resume-flag-with-id");
            let dependencies = base_dependencies(&temporary)
                .with_tui_launcher(|_, resume| Ok(format!("resume={resume:?}")));
            Case {
                name: "--resume 42 resumes session 42",
                argv: argv(&["--resume", "42"]),
                dependencies,
                expected: success("resume=Some(42)\n"),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("root-bare-integer");
            let dependencies = base_dependencies(&temporary)
                .with_tui_launcher(|_, resume| Ok(format!("resume={resume:?}")));
            Case {
                name: "a bare integer resumes that session",
                argv: argv(&["42"]),
                dependencies,
                expected: success("resume=Some(42)\n"),
                _temporary: temporary,
            }
        },
        {
            // R1: `i64::parse` accepts a leading minus, so a bare negative
            // integer resumes a (nonsensical) negative session id today.
            let temporary = TemporaryDirectory::new("root-negative-integer");
            let dependencies = base_dependencies(&temporary)
                .with_tui_launcher(|_, resume| Ok(format!("resume={resume:?}")));
            Case {
                name: "a bare negative integer resumes session -5 today (R1)",
                argv: argv(&["-5"]),
                dependencies,
                expected: success("resume=Some(-5)\n"),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("root-resume-non-numeric");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "--resume abc is an unknown command shape",
                argv: argv(&["--resume", "abc"]),
                dependencies,
                expected: failure(
                    ExitStatus::Usage,
                    format!(
                        "usage: {}",
                        parser_surface_baseline::UNKNOWN_COMMAND_MESSAGE
                    ),
                ),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("root-bare-word");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "a bare non-numeric word is an unknown command",
                argv: argv(&["abc"]),
                dependencies,
                expected: failure(
                    ExitStatus::Usage,
                    format!(
                        "usage: {}",
                        parser_surface_baseline::UNKNOWN_COMMAND_MESSAGE
                    ),
                ),
                _temporary: temporary,
            }
        },
    ];

    run_table_a(cases);
}

#[test]
fn table_a_root_shapes_reject_non_utf8_argv() {
    let temporary = TemporaryDirectory::new("root-non-utf8");
    let dependencies = base_dependencies(&temporary);

    let non_utf8_argument = {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(vec![0xff, 0xfe])
    };
    let actual = execute_os([OsString::from("chat"), non_utf8_argument], &dependencies);

    assert_eq!(
        actual,
        failure(
            ExitStatus::Usage,
            "usage: command arguments must be valid UTF-8"
        )
    );
}

#[test]
fn table_a_config_holds() {
    let cases = vec![
        {
            let temporary = TemporaryDirectory::new("config-doctor-valid");
            let dependencies = base_dependencies(&temporary);
            let expected_stdout = format!(
                "Agens config doctor\nGlobal:  {} (missing)\nProject: {} (missing)\nModel:   -\nStatus:  valid\n\n{}",
                config_global_config_path(&temporary).display(),
                config_project_config_path(&temporary).display(),
                CONFIG_DOCTOR_DEFAULT_SETTINGS,
            );
            Case {
                name: "config doctor over an empty configuration is valid",
                argv: argv(&["config", "doctor"]),
                dependencies,
                expected: success(expected_stdout),
                _temporary: temporary,
            }
        },
        {
            // A project configuration that defines `[mcp]` is rejected by
            // `bootstrap` before the doctor report is built; this is the
            // documented stdout-on-failure special case (`error_result`).
            let temporary = TemporaryDirectory::new("config-doctor-invalid");
            let mut files = BTreeMap::new();
            files.insert(config_project_config_path(&temporary), "[mcp]\n".to_owned());
            let dependencies = CliDependencies::for_test(
                temporary.project_root(),
                Some(temporary.home()),
                BTreeMap::new(),
                files,
            );
            Case {
                name: "config doctor over an invalid configuration reports invalid on stdout too",
                argv: argv(&["config", "doctor"]),
                dependencies,
                expected: CommandResult {
                    status: ExitStatus::Configuration,
                    stdout: "Agens config doctor\nStatus:  invalid\n".to_owned(),
                    stderr: "error: config: project configuration cannot define MCP servers\n"
                        .to_owned(),
                },
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("config-init-no-existing-file");
            let dependencies = base_dependencies(&temporary).with_create_file(|_, _| Ok(()));
            let expected_stdout = format!(
                "Wrote {}\n",
                config_project_config_path(&temporary).display()
            );
            Case {
                name: "config init writes a starter file when none exists",
                argv: argv(&["config", "init"]),
                dependencies,
                expected: success(expected_stdout),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("config-init-existing-file");
            let mut files = BTreeMap::new();
            files.insert(
                config_project_config_path(&temporary),
                "[tools]\n".to_owned(),
            );
            let dependencies = CliDependencies::for_test(
                temporary.project_root(),
                Some(temporary.home()),
                BTreeMap::new(),
                files,
            )
            .with_create_file(|_, _| panic!("init must not overwrite an existing configuration"));
            let expected_stderr = format!(
                "config: configuration already exists at {}",
                config_project_config_path(&temporary).display()
            );
            Case {
                name: "config init refuses to replace an existing file",
                argv: argv(&["config", "init"]),
                dependencies,
                expected: failure(ExitStatus::Configuration, expected_stderr),
                _temporary: temporary,
            }
        },
    ];

    run_table_a(cases);
}

#[test]
fn table_a_auth_holds() {
    let cases = vec![
        {
            let temporary = TemporaryDirectory::new("auth-status-no-credentials");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "auth status with no stored credentials",
                argv: argv(&["auth", "status"]),
                dependencies,
                expected: failure(
                    ExitStatus::Authentication,
                    "auth: ChatGPT credentials are unavailable or invalid",
                ),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("auth-status-openai-api");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "auth status openai-api with no stored credentials",
                argv: argv(&["auth", "status", "openai-api"]),
                dependencies,
                expected: failure(
                    ExitStatus::Authentication,
                    "auth: OpenAI API credentials are unavailable or invalid",
                ),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("auth-status-openai-chatgpt");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "auth status openai-chatgpt with no stored credentials",
                argv: argv(&["auth", "status", "openai-chatgpt"]),
                dependencies,
                expected: failure(
                    ExitStatus::Authentication,
                    "auth: ChatGPT credentials are unavailable or invalid",
                ),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("auth-status-bogus-provider");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "auth status bogus is an unsupported provider",
                argv: argv(&["auth", "status", "bogus"]),
                dependencies,
                expected: failure(ExitStatus::Usage, "usage: auth provider is unsupported"),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("auth-login-browser");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "auth login delegates to the (unavailable) test double",
                argv: argv(&["auth", "login"]),
                dependencies,
                expected: failure(
                    ExitStatus::Unavailable,
                    "unavailable: this command is not implemented yet",
                ),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("auth-login-device-auth");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "auth login --device-auth delegates to the (unavailable) test double",
                argv: argv(&["auth", "login", "--device-auth"]),
                dependencies,
                expected: failure(
                    ExitStatus::Unavailable,
                    "unavailable: this command is not implemented yet",
                ),
                _temporary: temporary,
            }
        },
        {
            // No `--api-key` value and a non-interactive, EOF stdin: the
            // stdin read succeeds with zero bytes, which normalizes to an
            // empty key and is rejected.
            let temporary = TemporaryDirectory::new("auth-login-api-key-empty-stdin");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "auth login api-key openai-api with empty stdin",
                argv: argv(&["auth", "login", "api-key", "openai-api"]),
                dependencies,
                expected: failure(
                    ExitStatus::Usage,
                    "usage: auth login api-key requires a non-empty API key",
                ),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("auth-login-api-key-wrong-provider");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "auth login api-key openai-chatgpt is unsupported",
                argv: argv(&["auth", "login", "api-key", "openai-chatgpt"]),
                dependencies,
                expected: failure(
                    ExitStatus::Usage,
                    "usage: API-key login supports only openai-api",
                ),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("auth-login-api-key-junk-trailer");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "auth login api-key openai-api rejects trailing junk arguments",
                argv: argv(&["auth", "login", "api-key", "openai-api", "junk"]),
                dependencies,
                expected: failure(
                    ExitStatus::Usage,
                    "usage: auth login api-key accepts only an optional --api-key value",
                ),
                _temporary: temporary,
            }
        },
        {
            // D3: `--device-auth` before `api-key` is NOT a recognized
            // shape at all today; it falls through to the generic "auth
            // requires status, login, or logout" usage error. This is
            // load-bearing for Phase 1, which must add an explicit guard
            // to keep clap from silently accepting and ignoring the flag.
            let temporary = TemporaryDirectory::new("auth-login-device-auth-api-key-d3");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "auth login --device-auth api-key openai-api is rejected today (D3)",
                argv: argv(&["auth", "login", "--device-auth", "api-key", "openai-api"]),
                dependencies,
                expected: failure(
                    ExitStatus::Usage,
                    "usage: auth requires status, login, or logout",
                ),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("auth-logout-openai-api");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "auth logout openai-api with nothing stored",
                argv: argv(&["auth", "logout", "openai-api"]),
                dependencies,
                expected: success("No credentials stored for openai-api.\n"),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("auth-logout-openai-chatgpt");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "auth logout openai-chatgpt with nothing stored",
                argv: argv(&["auth", "logout", "openai-chatgpt"]),
                dependencies,
                expected: success("No credentials stored for openai-chatgpt.\n"),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("auth-logout-bogus");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "auth logout bogus is an unsupported provider",
                argv: argv(&["auth", "logout", "bogus"]),
                dependencies,
                expected: failure(ExitStatus::Usage, "usage: auth provider is unsupported"),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("auth-no-subcommand");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "auth with no subcommand",
                argv: argv(&["auth"]),
                dependencies,
                expected: failure(
                    ExitStatus::Usage,
                    "usage: auth requires status, login, or logout",
                ),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("auth-unknown-subcommand");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "auth bogus is an unknown subcommand",
                argv: argv(&["auth", "bogus"]),
                dependencies,
                expected: failure(
                    ExitStatus::Usage,
                    "usage: auth requires status, login, or logout",
                ),
                _temporary: temporary,
            }
        },
    ];

    run_table_a(cases);
}

/// The parser-rendering surface Phase 1 (clap adoption) is explicitly
/// approved to change: help text, version rendering, and parse-error
/// wording, plus the two exit-code deltas recorded in the SDD decisions
/// (`chat foo --help` 2->0, `agens -5` no longer resuming). Every constant
/// here is TODAY's exact hand-rolled output. When Phase 1 lands, this is the
/// ONLY block that gets edited to re-baseline the suite — a single visible
/// diff, not a scatter of edits across Table A.
mod parser_surface_baseline {
    pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");

    pub(crate) fn root_help() -> String {
        format!(
            "Agens is a coding agent CLI\n\nUsage: agens <command>\n\nCommands:\n  auth      inspect supported authentication\n  chat      run a headless agent turn\n  config    inspect configuration\n  models    list provider models\n  sessions  inspect completed turns\n\nVersion: {VERSION}\n"
        )
    }

    pub(crate) fn version_line() -> String {
        format!("agens {VERSION}\n")
    }

    pub(crate) const CONFIG_HELP: &str = "Usage: agens config <doctor|init>\n";
    pub(crate) const CONFIG_MISSING_SUBCOMMAND_MESSAGE: &str =
        "config requires the doctor or init subcommand";
    pub(crate) const AUTH_HELP: &str = "Usage: agens auth <status|login|logout>\n";
    pub(crate) const CHAT_HELP: &str = "Usage: agens chat [flags] <prompt>\n";
    /// D1: `chat foo --help` is Usage(2) today (`--help` here is read as an
    /// unrecognized flag, not as a request for help). Phase 1 accepts this
    /// becoming Success(0) under clap.
    pub(crate) const CHAT_FOO_HELP_MESSAGE: &str = "chat received an unknown flag";
    pub(crate) const MODELS_HELP: &str = "Usage: agens models\n";
    pub(crate) const SESSIONS_HELP: &str = "Usage: agens sessions <list|show|rm>\n";
    pub(crate) const UNKNOWN_COMMAND_MESSAGE: &str = "unknown command; run agens --help";
    pub(crate) const MODELS_EXTRA_MESSAGE: &str = "models does not accept arguments";
    pub(crate) const CHAT_MODEL_MISSING_VALUE_MESSAGE: &str = "chat --model requires a value";
    pub(crate) const CHAT_MISSING_PROMPT_MESSAGE: &str = "chat requires a prompt argument";
}

#[test]
fn table_b_parser_surface_baseline_holds() {
    use parser_surface_baseline as baseline;

    let cases = vec![
        {
            let temporary = TemporaryDirectory::new("baseline-help-word");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "help",
                argv: argv(&["help"]),
                dependencies,
                expected: success(baseline::root_help()),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("baseline-help-long-flag");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "--help",
                argv: argv(&["--help"]),
                dependencies,
                expected: success(baseline::root_help()),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("baseline-help-short-flag");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "-h",
                argv: argv(&["-h"]),
                dependencies,
                expected: success(baseline::root_help()),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("baseline-version-word");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "version",
                argv: argv(&["version"]),
                dependencies,
                expected: success(baseline::version_line()),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("baseline-version-long-flag");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "--version",
                argv: argv(&["--version"]),
                dependencies,
                expected: success(baseline::version_line()),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("baseline-version-short-flag");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "-V",
                argv: argv(&["-V"]),
                dependencies,
                expected: success(baseline::version_line()),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("baseline-config-help");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "config --help",
                argv: argv(&["config", "--help"]),
                dependencies,
                expected: success(baseline::CONFIG_HELP),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("baseline-config-missing-subcommand");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "config with no subcommand",
                argv: argv(&["config"]),
                dependencies,
                expected: failure(
                    ExitStatus::Usage,
                    format!("usage: {}", baseline::CONFIG_MISSING_SUBCOMMAND_MESSAGE),
                ),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("baseline-auth-help");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "auth --help",
                argv: argv(&["auth", "--help"]),
                dependencies,
                expected: success(baseline::AUTH_HELP),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("baseline-chat-help");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "chat --help",
                argv: argv(&["chat", "--help"]),
                dependencies,
                expected: success(baseline::CHAT_HELP),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("baseline-chat-foo-help-d1");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "chat foo --help is Usage(2) today (D1)",
                argv: argv(&["chat", "foo", "--help"]),
                dependencies,
                expected: failure(
                    ExitStatus::Usage,
                    format!("usage: {}", baseline::CHAT_FOO_HELP_MESSAGE),
                ),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("baseline-models-help");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "models --help",
                argv: argv(&["models", "--help"]),
                dependencies,
                expected: success(baseline::MODELS_HELP),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("baseline-sessions-help");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "sessions --help",
                argv: argv(&["sessions", "--help"]),
                dependencies,
                expected: success(baseline::SESSIONS_HELP),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("baseline-unknown-command");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "an unknown top-level command",
                argv: argv(&["frobnicate"]),
                dependencies,
                expected: failure(
                    ExitStatus::Usage,
                    format!("usage: {}", baseline::UNKNOWN_COMMAND_MESSAGE),
                ),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("baseline-models-extra");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "models extra",
                argv: argv(&["models", "extra"]),
                dependencies,
                expected: failure(
                    ExitStatus::Usage,
                    format!("usage: {}", baseline::MODELS_EXTRA_MESSAGE),
                ),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("baseline-chat-model-missing-value");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "chat --model with no value",
                argv: argv(&["chat", "--model"]),
                dependencies,
                expected: failure(
                    ExitStatus::Usage,
                    format!("usage: {}", baseline::CHAT_MODEL_MISSING_VALUE_MESSAGE),
                ),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("baseline-chat-missing-prompt");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "chat with no prompt",
                argv: argv(&["chat"]),
                dependencies,
                expected: failure(
                    ExitStatus::Usage,
                    format!("usage: {}", baseline::CHAT_MISSING_PROMPT_MESSAGE),
                ),
                _temporary: temporary,
            }
        },
    ];

    run_table_a(cases);
}
