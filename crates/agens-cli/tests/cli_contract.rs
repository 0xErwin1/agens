//! Black-box characterization suite for the crate's public argv contract
//! (`cli-clap-and-module-split`, Phase 0).
//!
//! This file pins what the CURRENT hand-rolled parser in `lib.rs` actually
//! returns, not what it "should" return. Every assertion is a full-equality
//! check on `(status, stdout, stderr)`. If a case here fails after a later
//! phase, the parser's observable behavior changed and that is a defect,
//! unless the case belongs to [`parser_surface_baseline`] or to
//! `table_b_ratified_deltas_hold`, whose cases the maintainer has explicitly
//! accepted as deliberate behavior deltas from the pre-clap parser (not
//! invariants to preserve).

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

    fn path(&self) -> &std::path::Path {
        &self.path
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

/// A failure whose stderr is already clap's own fully rendered error text
/// (its own `error: ` prefix and usage block). Unlike [`failure`], this does
/// NOT add another `error: ` wrapper.
fn preformatted_failure(status: ExitStatus, stderr: impl Into<String>) -> CommandResult {
    CommandResult {
        status,
        stdout: String::new(),
        stderr: stderr.into(),
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
                // clap rejects the `--resume` value itself (a value-validation
                // error), which is a different rendering shape than an
                // unrecognized subcommand; sourced from the same Table B
                // module as `parser_surface_baseline::unrecognized_subcommand_message`
                // so it still updates only via that module.
                expected: preformatted_failure(
                    ExitStatus::Usage,
                    parser_surface_baseline::resume_invalid_value_message("abc"),
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
                expected: preformatted_failure(
                    ExitStatus::Usage,
                    parser_surface_baseline::unrecognized_subcommand_message(
                        "abc",
                        "agens [OPTIONS] [COMMAND]",
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
            let temporary = TemporaryDirectory::new("config-init-global-no-existing-file");
            let dependencies = base_dependencies(&temporary).with_create_file(|_, _| Ok(()));
            let expected_stdout = format!(
                "Wrote {}\n",
                config_global_config_path(&temporary).display()
            );
            Case {
                name: "config init --global writes a starter file to the global path",
                argv: argv(&["config", "init", "--global"]),
                dependencies,
                expected: success(expected_stdout),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("config-init-global-existing-file");
            let mut files = BTreeMap::new();
            files.insert(
                config_global_config_path(&temporary),
                "[tools]\n".to_owned(),
            );
            let dependencies = CliDependencies::for_test(
                temporary.project_root(),
                Some(temporary.home()),
                BTreeMap::new(),
                files,
            )
            .with_create_file(|_, _| panic!("init --global must not overwrite an existing file"));
            let expected_stderr = format!(
                "config: configuration already exists at {}",
                config_global_config_path(&temporary).display()
            );
            Case {
                name: "config init --global refuses to replace an existing file",
                argv: argv(&["config", "init", "--global"]),
                dependencies,
                expected: failure(ExitStatus::Configuration, expected_stderr),
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
    ];

    run_table_a(cases);
}

/// Echoes every field of the parsed `HeadlessChatRequest` so the assertion
/// exercises actual argument parsing, not just a stubbed success path.
fn echoing_chat_dependencies(temporary: &TemporaryDirectory) -> CliDependencies {
    base_dependencies(temporary).with_headless_chat(|request, _, _| {
        Ok(format!(
            "model={:?} system={:?} max_iterations={:?} mode={:?} dangerously_allow_all={} prompt={:?}",
            request.model,
            request.system_prompt,
            request.max_iterations,
            request.mode,
            request.dangerously_allow_all,
            request.prompt
        ))
    })
}

#[test]
fn table_a_chat_holds() {
    let cases = vec![
        {
            let temporary = TemporaryDirectory::new("chat-flag-model");
            let dependencies = echoing_chat_dependencies(&temporary);
            Case {
                name: "chat --model is threaded through to the request",
                argv: argv(&["chat", "--model", "gpt-4", "hi"]),
                dependencies,
                expected: success(
                    "model=Some(\"gpt-4\") system=None max_iterations=None mode=Edit dangerously_allow_all=false prompt=\"hi\"\n",
                ),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("chat-flag-system");
            let dependencies = echoing_chat_dependencies(&temporary);
            Case {
                name: "chat --system is threaded through to the request",
                argv: argv(&["chat", "--system", "sys", "hi"]),
                dependencies,
                expected: success(
                    "model=None system=Some(\"sys\") max_iterations=None mode=Edit dangerously_allow_all=false prompt=\"hi\"\n",
                ),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("chat-flag-max-iterations");
            let dependencies = echoing_chat_dependencies(&temporary);
            Case {
                name: "chat --max-iterations is threaded through to the request",
                argv: argv(&["chat", "--max-iterations", "3", "hi"]),
                dependencies,
                expected: success(
                    "model=None system=None max_iterations=Some(3) mode=Edit dangerously_allow_all=false prompt=\"hi\"\n",
                ),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("chat-flag-mode-chat");
            let dependencies = echoing_chat_dependencies(&temporary);
            Case {
                name: "chat --mode chat overrides the default Edit mode",
                argv: argv(&["chat", "--mode", "chat", "hi"]),
                dependencies,
                expected: success(
                    "model=None system=None max_iterations=None mode=Chat dangerously_allow_all=false prompt=\"hi\"\n",
                ),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("chat-flag-dangerously-allow-all");
            let dependencies = echoing_chat_dependencies(&temporary);
            Case {
                name: "chat --dangerously-allow-all is threaded through to the request",
                argv: argv(&["chat", "--dangerously-allow-all", "hi"]),
                dependencies,
                expected: success(
                    "model=None system=None max_iterations=None mode=Edit dangerously_allow_all=true prompt=\"hi\"\n",
                ),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("chat-all-flags-together");
            let dependencies = echoing_chat_dependencies(&temporary);
            Case {
                name: "chat with every flag combined",
                argv: argv(&[
                    "chat",
                    "--model",
                    "X",
                    "--system",
                    "S",
                    "--max-iterations",
                    "3",
                    "--mode",
                    "chat",
                    "--dangerously-allow-all",
                    "hi",
                ]),
                dependencies,
                expected: success(
                    "model=Some(\"X\") system=Some(\"S\") max_iterations=Some(3) mode=Chat dangerously_allow_all=true prompt=\"hi\"\n",
                ),
                _temporary: temporary,
            }
        },
        {
            // A single whitespace-only argument is NOT treated as an empty
            // prompt: `prompt.trim().is_empty()` guards the "set the
            // prompt" arm, so the argument instead falls into the
            // catch-all "one prompt argument" arm. This diverges from the
            // spec's assumption that this case reads as a missing prompt;
            // pin the OBSERVED behavior.
            let temporary = TemporaryDirectory::new("chat-whitespace-only-prompt");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "chat with a single whitespace-only argument (observed, not `requires a prompt`)",
                argv: argv(&["chat", "  "]),
                dependencies,
                expected: failure(ExitStatus::Usage, "usage: chat accepts one prompt argument"),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("chat-max-iterations-zero");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "chat --max-iterations 0 is rejected",
                argv: argv(&["chat", "--max-iterations", "0", "x"]),
                dependencies,
                expected: failure(
                    ExitStatus::Usage,
                    "usage: chat --max-iterations must be >= 1",
                ),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("chat-mode-bogus");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "chat --mode bogus is rejected",
                argv: argv(&["chat", "--mode", "bogus", "x"]),
                dependencies,
                expected: failure(ExitStatus::Usage, "usage: chat --mode must be chat or edit"),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("chat-two-positionals");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "chat rejects a second positional argument",
                argv: argv(&["chat", "a", "b"]),
                dependencies,
                expected: failure(ExitStatus::Usage, "usage: chat accepts one prompt argument"),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("chat-flag-model-after-prompt");
            let dependencies = echoing_chat_dependencies(&temporary);
            Case {
                name: "chat --model after the prompt is still threaded through",
                argv: argv(&["chat", "hi", "--model", "gpt-4"]),
                dependencies,
                expected: success(
                    "model=Some(\"gpt-4\") system=None max_iterations=None mode=Edit dangerously_allow_all=false prompt=\"hi\"\n",
                ),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("chat-flag-system-after-prompt");
            let dependencies = echoing_chat_dependencies(&temporary);
            Case {
                name: "chat --system after the prompt is still threaded through",
                argv: argv(&["chat", "hi", "--system", "sys"]),
                dependencies,
                expected: success(
                    "model=None system=Some(\"sys\") max_iterations=None mode=Edit dangerously_allow_all=false prompt=\"hi\"\n",
                ),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("chat-flag-max-iterations-after-prompt");
            let dependencies = echoing_chat_dependencies(&temporary);
            Case {
                name: "chat --max-iterations after the prompt is still threaded through",
                argv: argv(&["chat", "hi", "--max-iterations", "3"]),
                dependencies,
                expected: success(
                    "model=None system=None max_iterations=Some(3) mode=Edit dangerously_allow_all=false prompt=\"hi\"\n",
                ),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("chat-flag-mode-after-prompt");
            let dependencies = echoing_chat_dependencies(&temporary);
            Case {
                name: "chat --mode after the prompt is still threaded through",
                argv: argv(&["chat", "hi", "--mode", "chat"]),
                dependencies,
                expected: success(
                    "model=None system=None max_iterations=None mode=Chat dangerously_allow_all=false prompt=\"hi\"\n",
                ),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("chat-flag-dangerously-allow-all-after-prompt");
            let dependencies = echoing_chat_dependencies(&temporary);
            Case {
                name: "chat --dangerously-allow-all after the prompt is still threaded through",
                argv: argv(&["chat", "hi", "--dangerously-allow-all"]),
                dependencies,
                expected: success(
                    "model=None system=None max_iterations=None mode=Edit dangerously_allow_all=true prompt=\"hi\"\n",
                ),
                _temporary: temporary,
            }
        },
    ];

    run_table_a(cases);
}

#[test]
fn table_a_models_and_sessions_hold() {
    let cases = vec![
        {
            let temporary = TemporaryDirectory::new("models-default");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "models lists the bundled snapshot",
                argv: argv(&["models"]),
                dependencies,
                expected: success(concat!(
                    "ID\tNAME\tCONTEXT\tPRICE\n",
                    "gpt-4.1\tGPT-4.1\t1047576\t$2.00/$8.00\n",
                    "gpt-4.1-mini\tGPT-4.1 mini\t1047576\t$0.40/$1.60\n",
                    "gpt-4.1-nano\tGPT-4.1 nano\t1047576\t$0.10/$0.40\n",
                    "gpt-4o\tGPT-4o\t128000\t$2.50/$10.00\n",
                    "gpt-4o-mini\tGPT-4o mini\t128000\t$0.15/$0.60\n",
                    "o3\to3\t200000\t$2.00/$8.00\n",
                    "o4-mini\to4-mini\t200000\t$1.10/$4.40\n",
                )),
                _temporary: temporary,
            }
        },
        {
            // Full triple pinned here in Table A; the wording is sourced
            // from the Table B constant so a Phase 1 re-baseline of that
            // constant updates this expectation automatically.
            let temporary = TemporaryDirectory::new("models-extra-argument");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "models rejects any argument",
                argv: argv(&["models", "extra"]),
                dependencies,
                expected: preformatted_failure(
                    ExitStatus::Usage,
                    parser_surface_baseline::MODELS_EXTRA_MESSAGE,
                ),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("sessions-list-empty");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "sessions list with no saved sessions",
                argv: argv(&["sessions", "list"]),
                dependencies,
                expected: success("No saved sessions.\n"),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("sessions-show-missing-id");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "sessions show of a numeric id that does not exist",
                argv: argv(&["sessions", "show", "1"]),
                dependencies,
                expected: failure(ExitStatus::Failure, "store: saved session is unavailable"),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("sessions-show-non-numeric");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "sessions show requires a numeric id",
                argv: argv(&["sessions", "show", "abc"]),
                dependencies,
                expected: failure(
                    ExitStatus::Usage,
                    "usage: sessions show requires a numeric id",
                ),
                _temporary: temporary,
            }
        },
        {
            // Deletion is idempotent: removing a session id that was never
            // stored still reports success (confirmed by
            // `sessions_crud_uses_normalized_metadata_and_idempotent_removal`
            // in `tests/cli.rs`, not a Phase-0 discovery).
            let temporary = TemporaryDirectory::new("sessions-rm-missing-id");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "sessions rm of a numeric id that does not exist is idempotent",
                argv: argv(&["sessions", "rm", "1"]),
                dependencies,
                expected: success("Removed session 1.\n"),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("sessions-rm-non-numeric");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "sessions rm requires a numeric id",
                argv: argv(&["sessions", "rm", "abc"]),
                dependencies,
                expected: failure(
                    ExitStatus::Usage,
                    "usage: sessions rm requires a numeric id",
                ),
                _temporary: temporary,
            }
        },
        {
            // A store-open failure: `data_dir` names a path whose parent
            // segment is an ordinary file, so `create_dir_all` fails before
            // any session table is touched.
            let temporary = TemporaryDirectory::new("sessions-store-open-failure");
            let blocked_by_file = temporary.path().join("blocked-file");
            std::fs::write(&blocked_by_file, b"not a directory")
                .expect("blocking file should be created");
            let data_directory = blocked_by_file.join("data");
            let mut files = BTreeMap::new();
            files.insert(
                config_project_config_path(&temporary),
                format!("[options]\ndata_dir = \"{}\"\n", data_directory.display()),
            );
            let dependencies = CliDependencies::for_test(
                temporary.project_root(),
                Some(temporary.home()),
                BTreeMap::new(),
                files,
            );
            Case {
                name: "sessions list reports a store-open failure",
                argv: argv(&["sessions", "list"]),
                dependencies,
                expected: failure(
                    ExitStatus::Failure,
                    "store: sessions database is unavailable",
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

    pub(crate) const ROOT_HELP: &str = "Agens is a coding agent CLI\n\nUsage: agens [OPTIONS] [COMMAND]\n\nCommands:\n  config    inspect configuration\n  auth      inspect supported authentication\n  chat      run a headless agent turn\n  models    list provider models\n  sessions  inspect completed turns\n  help      Print this message or the help of the given subcommand(s)\n\nOptions:\n      --resume [<SESSION_ID>]  Resume the most recent session, or the given session id\n  -h, --help                   Print help\n  -V, --version                Print version\n";

    pub(crate) fn version_line() -> String {
        format!("agens {VERSION}\n")
    }

    pub(crate) const CONFIG_HELP: &str = "inspect configuration\n\nUsage: agens config <COMMAND>\n\nCommands:\n  doctor  report the effective configuration and where each setting came from\n  init    write a starter configuration file\n  help    Print this message or the help of the given subcommand(s)\n\nOptions:\n  -h, --help  Print help\n";
    /// `config` with no subcommand renders byte-identical to `config --help`:
    /// clap treats a required subcommand group with nothing supplied the
    /// same as an explicit help request.
    pub(crate) const CONFIG_MISSING_SUBCOMMAND_MESSAGE: &str = CONFIG_HELP;
    pub(crate) const CONFIG_DOCTOR_HELP: &str = "report the effective configuration and where each setting came from\n\nUsage: agens config doctor\n\nOptions:\n  -h, --help  Print help\n";
    pub(crate) const CONFIG_INIT_HELP: &str = "write a starter configuration file\n\nUsage: agens config init [OPTIONS]\n\nOptions:\n      --global  Write the starter configuration to the global path instead of the project path\n  -h, --help    Print help\n";
    pub(crate) const AUTH_HELP: &str = "inspect supported authentication\n\nUsage: agens auth <COMMAND>\n\nCommands:\n  status  report authentication status for ChatGPT or an API-key provider\n  login   log in to ChatGPT or an API-key provider\n  logout  remove stored credentials for a provider\n  help    Print this message or the help of the given subcommand(s)\n\nOptions:\n  -h, --help  Print help\n";
    /// `auth` with no subcommand renders byte-identical to `auth --help`.
    pub(crate) const AUTH_MISSING_SUBCOMMAND_MESSAGE: &str = AUTH_HELP;
    pub(crate) const AUTH_STATUS_HELP: &str = "report authentication status for ChatGPT or an API-key provider\n\nUsage: agens auth status [PROVIDER]\n\nArguments:\n  [PROVIDER]  \n\nOptions:\n  -h, --help  Print help\n";
    pub(crate) const AUTH_LOGIN_HELP: &str = "log in to ChatGPT or an API-key provider\n\nUsage: agens auth login [OPTIONS] [COMMAND]\n\nCommands:\n  api-key  log in with an API key instead of ChatGPT\n  help     Print this message or the help of the given subcommand(s)\n\nOptions:\n      --device-auth  Use the device-code flow instead of opening a browser\n  -h, --help         Print help\n";
    pub(crate) const AUTH_LOGIN_API_KEY_HELP: &str = "log in with an API key instead of ChatGPT\n\nUsage: agens auth login api-key [OPTIONS] <PROVIDER>\n\nArguments:\n  <PROVIDER>  \n\nOptions:\n      --api-key <API_KEY>  \n  -h, --help               Print help\n";
    pub(crate) const CHAT_HELP: &str = "run a headless agent turn\n\nUsage: agens chat [OPTIONS] [PROMPT]...\n\nArguments:\n  [PROMPT]...  \n\nOptions:\n      --model <MODEL>                    \n      --system <SYSTEM>                  \n      --max-iterations <MAX_ITERATIONS>  \n      --mode <chat|edit>                 \n      --dangerously-allow-all            \n  -h, --help                             Print help\n";
    /// D1 (ratified): `chat foo --help` goes from Usage(2) to Success(0).
    /// `ChatArgs.prompt` no longer sets `allow_hyphen_values`, so clap keeps
    /// matching `--help` as a flag no matter where it appears relative to
    /// the prompt token, and renders `chat`'s own help exactly as `chat
    /// --help` does. This is the ratified delta, not "no delta observed" —
    /// an earlier revision of this baseline was stale.
    pub(crate) const CHAT_UNKNOWN_FLAG_MESSAGE: &str = "error: unexpected argument '--bogus' found\n\n  tip: to pass '--bogus' as a value, use '-- --bogus'\n\nUsage: agens chat [OPTIONS] [PROMPT]...\n\nFor more information, try '--help'.\n";
    pub(crate) const MODELS_HELP: &str =
        "list provider models\n\nUsage: agens models\n\nOptions:\n  -h, --help  Print help\n";
    pub(crate) const SESSIONS_HELP: &str = "inspect completed turns\n\nUsage: agens sessions <COMMAND>\n\nCommands:\n  list  list saved sessions\n  show  show a saved session's details\n  rm    remove a saved session\n  help  Print this message or the help of the given subcommand(s)\n\nOptions:\n  -h, --help  Print help\n";
    /// `sessions` with no subcommand renders byte-identical to `sessions --help`.
    pub(crate) const SESSIONS_MISSING_SUBCOMMAND_MESSAGE: &str = SESSIONS_HELP;
    pub(crate) const SESSIONS_LIST_HELP: &str =
        "list saved sessions\n\nUsage: agens sessions list\n\nOptions:\n  -h, --help  Print help\n";

    /// clap's rendering embeds both the offending token and the usage line
    /// of whichever command group rejected it, so this can no longer be a
    /// single shared literal the way the hand-rolled parser's one static
    /// message was.
    pub(crate) fn unrecognized_subcommand_message(token: &str, usage: &str) -> String {
        format!(
            "error: unrecognized subcommand '{token}'\n\nUsage: {usage}\n\nFor more information, try '--help'.\n"
        )
    }

    /// `--resume <non-numeric>` fails clap's own value parsing for the
    /// `--resume` option; this is a distinct clap error shape (no `Usage:`
    /// line) from an unrecognized subcommand, so it cannot share
    /// `unrecognized_subcommand_message`.
    pub(crate) fn resume_invalid_value_message(token: &str) -> String {
        format!(
            "error: invalid value '{token}' for '--resume [<SESSION_ID>]': invalid digit found in string\n\nFor more information, try '--help'.\n"
        )
    }

    pub(crate) const MODELS_EXTRA_MESSAGE: &str = "error: unexpected argument 'extra' found\n\nUsage: agens models\n\nFor more information, try '--help'.\n";

    /// W2: `--help`/`-h`/`--version`/`-V`/`help` are recognized as a
    /// root-level request ONLY when they are the sole argument; clap itself
    /// would otherwise honor the flag regardless of a trailing token, which
    /// is exactly the regression this message closes. `cli::root_shape_conflict`
    /// manufactures this via `Command::error`, so it carries clap's own
    /// rendering even though clap never actually parsed this shape.
    pub(crate) fn root_shape_conflict_message(token: &str) -> String {
        format!(
            "error: unrecognized argument '{token}'\n\nUsage: agens [OPTIONS] [COMMAND]\n\nFor more information, try '--help'.\n"
        )
    }
    pub(crate) const CHAT_MODEL_MISSING_VALUE_MESSAGE: &str = "error: a value is required for '--model <MODEL>' but none was supplied\n\nFor more information, try '--help'.\n";
    /// Unlike the other clap-owned cases above, this stays a body-owned
    /// message: `ChatArgs.prompt` is an unbounded `Vec<String>` (see
    /// `cli.rs`), so clap itself never errors on zero prompt tokens; the
    /// domain check in `chat_request` still produces this exact text.
    pub(crate) const CHAT_MISSING_PROMPT_MESSAGE: &str = "chat requires a prompt argument";

    /// `AuthAction::Login`'s typed `LoginMethod::ApiKey { provider, api_key
    /// }` only defines a required `provider` positional and an optional
    /// `--api-key` flag, so a trailing `junk` argument is a clap-owned
    /// shape error now, not the old hand-rolled arity message.
    pub(crate) const AUTH_LOGIN_API_KEY_JUNK_MESSAGE: &str = "error: unexpected argument 'junk' found\n\nUsage: agens auth login api-key [OPTIONS] <PROVIDER>\n\nFor more information, try '--help'.\n";

    /// D3: a plain `#[arg(long)]` cannot `conflicts_with` a nested
    /// `#[command(subcommand)]` field (clap's own derive debug assertion
    /// rejects that at startup — verified, not assumed), so
    /// `--device-auth` combined with `api-key ...` is rejected by an
    /// explicit body guard in `run_auth`, not by clap. The guard reuses the
    /// pre-clap message; that message is body-owned so it is NOT
    /// clap-preformatted (no `error: ` double-prefix concern here).
    pub(crate) const AUTH_DEVICE_AUTH_API_KEY_MESSAGE: &str =
        "auth requires status, login, or logout";
}

/// C1-R2: a bare `help` token (not `--help`/`-h`) at a nested position must
/// still win over whatever String-typed positional it would otherwise bind
/// to. Every leaf command with a plain `String`/`Option<String>` positional
/// (as opposed to a nested `#[command(subcommand)]`, where clap's own
/// auto-generated `help` pseudo-subcommand already handles this) is covered
/// here as one family, not as isolated shapes — that is exactly the
/// distinction that let this regression land twice.
#[test]
fn table_b_bare_help_word_at_every_nesting_depth_holds() {
    use parser_surface_baseline as baseline;

    let cases = vec![
        {
            let temporary = TemporaryDirectory::new("bare-help-auth-status");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "auth status help (bare word) still renders auth's help",
                argv: argv(&["auth", "status", "help"]),
                dependencies,
                expected: success(baseline::AUTH_HELP),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("bare-help-auth-logout");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "auth logout help (bare word) still renders auth's help",
                argv: argv(&["auth", "logout", "help"]),
                dependencies,
                expected: success(baseline::AUTH_HELP),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("bare-help-auth-login-api-key");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "auth login api-key help (bare word) still renders auth's help",
                argv: argv(&["auth", "login", "api-key", "help"]),
                dependencies,
                expected: success(baseline::AUTH_HELP),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("bare-help-sessions-show");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "sessions show help (bare word) still renders sessions's help",
                argv: argv(&["sessions", "show", "help"]),
                dependencies,
                expected: success(baseline::SESSIONS_HELP),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("bare-help-sessions-rm");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "sessions rm help (bare word) still renders sessions's help",
                argv: argv(&["sessions", "rm", "help"]),
                dependencies,
                expected: success(baseline::SESSIONS_HELP),
                _temporary: temporary,
            }
        },
        // Regression guards: leaves with NO plain String positional resolve
        // the bare `help` token through clap's own auto-generated `help`
        // pseudo-subcommand already, and must keep doing so unaffected by
        // the fix for the shapes above.
        {
            let temporary = TemporaryDirectory::new("bare-help-config-init");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "config init help (bare word) renders config's help via clap's own subcommand",
                argv: argv(&["config", "init", "help"]),
                dependencies,
                expected: success(baseline::CONFIG_HELP),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("bare-help-config-doctor");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "config doctor help (bare word) renders config's help via clap's own subcommand",
                argv: argv(&["config", "doctor", "help"]),
                dependencies,
                expected: success(baseline::CONFIG_HELP),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("bare-help-auth-login");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "auth login help (bare word) renders auth's help via clap's own subcommand",
                argv: argv(&["auth", "login", "help"]),
                dependencies,
                expected: success(baseline::AUTH_HELP),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("bare-help-sessions-list");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "sessions list help (bare word) renders sessions's help via clap's own subcommand",
                argv: argv(&["sessions", "list", "help"]),
                dependencies,
                expected: success(baseline::SESSIONS_HELP),
                _temporary: temporary,
            }
        },
    ];

    run_table_a(cases);
}

/// C2-R2: `agens chat help` is the one shape where the bare `help` token
/// must win over `ChatArgs.prompt` (a `Vec<String>`, so clap itself would
/// otherwise happily bind `"help"` as the prompt and run a real turn). Only
/// the single-argument shape is special-cased, matching the pre-clap
/// `is_help` precedent: `chat foo help` and `chat help foo` are ordinary
/// two-token prompts, not a help request.
#[test]
fn table_b_chat_bare_help_word_holds() {
    use parser_surface_baseline as baseline;

    let cases = vec![
        {
            let temporary = TemporaryDirectory::new("bare-help-chat-alone");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "chat help (bare word, sole argument) renders chat's help",
                argv: argv(&["chat", "help"]),
                dependencies,
                expected: success(baseline::CHAT_HELP),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("bare-help-chat-not-sole-argument");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "chat foo help is an ordinary two-token prompt, not a help request",
                argv: argv(&["chat", "foo", "help"]),
                dependencies,
                expected: failure(ExitStatus::Usage, "usage: chat accepts one prompt argument"),
                _temporary: temporary,
            }
        },
    ];

    run_table_a(cases);
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
                expected: success(baseline::ROOT_HELP),
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
                expected: success(baseline::ROOT_HELP),
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
                expected: success(baseline::ROOT_HELP),
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
                expected: preformatted_failure(
                    ExitStatus::Usage,
                    baseline::CONFIG_MISSING_SUBCOMMAND_MESSAGE,
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
            let temporary = TemporaryDirectory::new("baseline-auth-missing-subcommand");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "auth with no subcommand",
                argv: argv(&["auth"]),
                dependencies,
                expected: preformatted_failure(
                    ExitStatus::Usage,
                    baseline::AUTH_MISSING_SUBCOMMAND_MESSAGE,
                ),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("baseline-auth-unknown-subcommand");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "auth bogus is an unknown subcommand",
                argv: argv(&["auth", "bogus"]),
                dependencies,
                expected: preformatted_failure(
                    ExitStatus::Usage,
                    baseline::unrecognized_subcommand_message("bogus", "agens auth <COMMAND>"),
                ),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("baseline-auth-login-api-key-junk-trailer");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "auth login api-key openai-api rejects trailing junk arguments",
                argv: argv(&["auth", "login", "api-key", "openai-api", "junk"]),
                dependencies,
                expected: preformatted_failure(
                    ExitStatus::Usage,
                    baseline::AUTH_LOGIN_API_KEY_JUNK_MESSAGE,
                ),
                _temporary: temporary,
            }
        },
        {
            // D3: `--device-auth` before `api-key` is silently accepted by
            // clap's grammar (a flag cannot `conflicts_with` a nested
            // subcommand); `run_auth`'s explicit guard is what rejects it,
            // reusing the pre-clap message verbatim.
            let temporary = TemporaryDirectory::new("baseline-auth-login-device-auth-api-key-d3");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "auth login --device-auth api-key openai-api is rejected (D3)",
                argv: argv(&["auth", "login", "--device-auth", "api-key", "openai-api"]),
                dependencies,
                expected: failure(
                    ExitStatus::Usage,
                    format!("usage: {}", baseline::AUTH_DEVICE_AUTH_API_KEY_MESSAGE),
                ),
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
                name: "chat foo --help is Success(0), the ratified D1 delta",
                argv: argv(&["chat", "foo", "--help"]),
                dependencies,
                expected: success(baseline::CHAT_HELP),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("baseline-chat-unknown-flag");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "chat rejects an unrecognized flag with clap's own rendering",
                argv: argv(&["chat", "--bogus", "hi"]),
                dependencies,
                expected: preformatted_failure(
                    ExitStatus::Usage,
                    baseline::CHAT_UNKNOWN_FLAG_MESSAGE,
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
            let temporary = TemporaryDirectory::new("baseline-sessions-missing-subcommand");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "sessions with no subcommand",
                argv: argv(&["sessions"]),
                dependencies,
                expected: preformatted_failure(
                    ExitStatus::Usage,
                    baseline::SESSIONS_MISSING_SUBCOMMAND_MESSAGE,
                ),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("baseline-sessions-unknown-subcommand");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "sessions bogus is an unknown subcommand",
                argv: argv(&["sessions", "bogus"]),
                dependencies,
                expected: preformatted_failure(
                    ExitStatus::Usage,
                    baseline::unrecognized_subcommand_message("bogus", "agens sessions <COMMAND>"),
                ),
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
                expected: preformatted_failure(
                    ExitStatus::Usage,
                    baseline::unrecognized_subcommand_message(
                        "frobnicate",
                        "agens [OPTIONS] [COMMAND]",
                    ),
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
                expected: preformatted_failure(ExitStatus::Usage, baseline::MODELS_EXTRA_MESSAGE),
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
                expected: preformatted_failure(
                    ExitStatus::Usage,
                    baseline::CHAT_MODEL_MISSING_VALUE_MESSAGE,
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
        // W1: `config`/`auth`/`models`/`sessions` treat a help token
        // ANYWHERE in their own arguments as a request for that
        // subcommand's help, even alongside an otherwise-invalid shape.
        {
            let temporary = TemporaryDirectory::new("baseline-models-help-word");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "models help (bare word, non-flag) still renders help",
                argv: argv(&["models", "help"]),
                dependencies,
                expected: success(baseline::MODELS_HELP),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("baseline-models-extra-then-help");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "models extra --help renders help despite the invalid extra argument",
                argv: argv(&["models", "extra", "--help"]),
                dependencies,
                expected: success(baseline::MODELS_HELP),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("baseline-config-extra-then-help");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "config extra --help renders help despite the invalid extra subcommand",
                argv: argv(&["config", "extra", "--help"]),
                dependencies,
                expected: success(baseline::CONFIG_HELP),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("baseline-auth-extra-then-help");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "auth extra --help renders help despite the invalid extra subcommand",
                argv: argv(&["auth", "extra", "--help"]),
                dependencies,
                expected: success(baseline::AUTH_HELP),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("baseline-sessions-extra-then-help");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "sessions extra --help renders help despite the invalid extra subcommand",
                argv: argv(&["sessions", "extra", "--help"]),
                dependencies,
                expected: success(baseline::SESSIONS_HELP),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("baseline-config-doctor-extra-then-help");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "config doctor extra --help renders help despite the invalid trailing argument",
                argv: argv(&["config", "doctor", "extra", "--help"]),
                dependencies,
                expected: success(baseline::CONFIG_HELP),
                _temporary: temporary,
            }
        },
        // Nested help: clap resolves a VALID nested shape's help itself,
        // walking to the deepest matched subcommand, rather than falling
        // back to the top-level subcommand's help the way the W1 cases
        // above do for an invalid shape.
        {
            let temporary = TemporaryDirectory::new("baseline-config-init-help");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "config init --help renders init's own help, not config's",
                argv: argv(&["config", "init", "--help"]),
                dependencies,
                expected: success(baseline::CONFIG_INIT_HELP),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("baseline-config-doctor-help");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "config doctor --help renders doctor's own help, not config's",
                argv: argv(&["config", "doctor", "--help"]),
                dependencies,
                expected: success(baseline::CONFIG_DOCTOR_HELP),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("baseline-auth-login-help");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "auth login --help renders login's own help, not auth's",
                argv: argv(&["auth", "login", "--help"]),
                dependencies,
                expected: success(baseline::AUTH_LOGIN_HELP),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("baseline-auth-status-help");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "auth status --help renders status's own help, not auth's",
                argv: argv(&["auth", "status", "--help"]),
                dependencies,
                expected: success(baseline::AUTH_STATUS_HELP),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("baseline-auth-login-api-key-help");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "auth login api-key --help renders api-key's own help, not auth's",
                argv: argv(&["auth", "login", "api-key", "--help"]),
                dependencies,
                expected: success(baseline::AUTH_LOGIN_API_KEY_HELP),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("baseline-sessions-list-help");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "sessions list --help renders list's own help, not sessions's",
                argv: argv(&["sessions", "list", "--help"]),
                dependencies,
                expected: success(baseline::SESSIONS_LIST_HELP),
                _temporary: temporary,
            }
        },
        // W2: help/version tokens are recognized as a root-level request
        // ONLY when they are the sole argument; a trailing token is a
        // usage error, matching the historical single-argument precedent.
        {
            let temporary = TemporaryDirectory::new("baseline-help-flag-with-extra");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "--help extra is a usage error, not a help render",
                argv: argv(&["--help", "extra"]),
                dependencies,
                expected: preformatted_failure(
                    ExitStatus::Usage,
                    baseline::root_shape_conflict_message("--help"),
                ),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("baseline-help-short-flag-with-extra");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "-h x is a usage error, not a help render",
                argv: argv(&["-h", "x"]),
                dependencies,
                expected: preformatted_failure(
                    ExitStatus::Usage,
                    baseline::root_shape_conflict_message("-h"),
                ),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("baseline-version-short-flag-with-extra");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "-V x is a usage error, not a version render",
                argv: argv(&["-V", "x"]),
                dependencies,
                expected: preformatted_failure(
                    ExitStatus::Usage,
                    baseline::root_shape_conflict_message("-V"),
                ),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("baseline-version-flag-with-extra");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "--version x is a usage error, not a version render",
                argv: argv(&["--version", "x"]),
                dependencies,
                expected: preformatted_failure(
                    ExitStatus::Usage,
                    baseline::root_shape_conflict_message("--version"),
                ),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("baseline-help-word-with-extra");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "help chat is a usage error, not subcommand help",
                argv: argv(&["help", "chat"]),
                dependencies,
                expected: preformatted_failure(
                    ExitStatus::Usage,
                    baseline::root_shape_conflict_message("help"),
                ),
                _temporary: temporary,
            }
        },
        // W3: a bare `--` must not fall through to the TUI launcher; it is
        // not a recognized invocation shape.
        {
            let temporary = TemporaryDirectory::new("baseline-bare-option-terminator");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "a bare -- is a usage error, not a TUI launch",
                argv: argv(&["--"]),
                dependencies,
                expected: preformatted_failure(
                    ExitStatus::Usage,
                    baseline::root_shape_conflict_message("--"),
                ),
                _temporary: temporary,
            }
        },
    ];

    run_table_a(cases);
}

/// Ratified behavior deltas from `766bc0b` (round-2 verification, families
/// W-A/W-B/W-C/W-D/W6). The maintainer accepted every case below as
/// deliberate, NOT as an invariant to restore: this test pins the CURRENT
/// behavior so it cannot drift again unnoticed, exactly the way
/// [`table_b_parser_surface_baseline_holds`] pins the parser's rendering
/// surface. Do not "improve" any of these without a new, explicit
/// ratification — that is precisely what turned each of them into a defect
/// the first time.
#[test]
fn table_b_ratified_deltas_hold() {
    let cases = vec![
        // W-A: clap rejects a repeated chat flag outright. The hand-rolled
        // parser took last-wins and ran the turn; every later occurrence of
        // the same flag is a shape error now, not a value override.
        {
            let temporary = TemporaryDirectory::new("ratified-chat-repeated-model");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "W-A: chat --model twice is rejected, not last-wins",
                argv: argv(&["chat", "--model", "a", "--model", "b", "hi"]),
                dependencies,
                expected: preformatted_failure(
                    ExitStatus::Usage,
                    "error: the argument '--model <MODEL>' cannot be used multiple times\n\nUsage: agens chat [OPTIONS] [PROMPT]...\n\nFor more information, try '--help'.\n",
                ),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("ratified-chat-repeated-system");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "W-A: chat --system twice is rejected, not last-wins",
                argv: argv(&["chat", "--system", "a", "--system", "b", "hi"]),
                dependencies,
                expected: preformatted_failure(
                    ExitStatus::Usage,
                    "error: the argument '--system <SYSTEM>' cannot be used multiple times\n\nUsage: agens chat [OPTIONS] [PROMPT]...\n\nFor more information, try '--help'.\n",
                ),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("ratified-chat-repeated-max-iterations");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "W-A: chat --max-iterations twice is rejected, not last-wins",
                argv: argv(&[
                    "chat",
                    "--max-iterations",
                    "1",
                    "--max-iterations",
                    "2",
                    "hi",
                ]),
                dependencies,
                expected: preformatted_failure(
                    ExitStatus::Usage,
                    "error: the argument '--max-iterations <MAX_ITERATIONS>' cannot be used multiple times\n\nUsage: agens chat [OPTIONS] [PROMPT]...\n\nFor more information, try '--help'.\n",
                ),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("ratified-chat-repeated-mode");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "W-A: chat --mode twice is rejected, not last-wins",
                argv: argv(&["chat", "--mode", "chat", "--mode", "edit", "hi"]),
                dependencies,
                expected: preformatted_failure(
                    ExitStatus::Usage,
                    "error: the argument '--mode <chat|edit>' cannot be used multiple times\n\nUsage: agens chat [OPTIONS] [PROMPT]...\n\nFor more information, try '--help'.\n",
                ),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("ratified-chat-repeated-dangerously-allow-all");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "W-A: chat --dangerously-allow-all twice is rejected, not last-wins",
                argv: argv(&[
                    "chat",
                    "--dangerously-allow-all",
                    "--dangerously-allow-all",
                    "hi",
                ]),
                dependencies,
                expected: preformatted_failure(
                    ExitStatus::Usage,
                    "error: the argument '--dangerously-allow-all' cannot be used multiple times\n\nUsage: agens chat [OPTIONS] [PROMPT]...\n\nFor more information, try '--help'.\n",
                ),
                _temporary: temporary,
            }
        },
        // W-B: clap treats a leading `-` on a value as a flag by default, so
        // a negative numeric argument is rejected instead of reaching the
        // domain's own numeric parsing.
        {
            let temporary = TemporaryDirectory::new("ratified-resume-negative");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "W-B: --resume -5 is rejected by clap, not resolved to session -5",
                argv: argv(&["--resume", "-5"]),
                dependencies,
                expected: preformatted_failure(
                    ExitStatus::Usage,
                    "error: unexpected argument '-5' found\n\nUsage: agens [OPTIONS] [COMMAND]\n\nFor more information, try '--help'.\n",
                ),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("ratified-sessions-show-negative");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "W-B: sessions show -1 is rejected by clap, not a numeric-id error",
                argv: argv(&["sessions", "show", "-1"]),
                dependencies,
                expected: preformatted_failure(
                    ExitStatus::Usage,
                    "error: unexpected argument '-1' found\n\n  tip: to pass '-1' as a value, use '-- -1'\n\nUsage: agens sessions show <IDENTIFIER>\n\nFor more information, try '--help'.\n",
                ),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("ratified-sessions-rm-negative");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "W-B: sessions rm -1 is rejected by clap, not a no-op success",
                argv: argv(&["sessions", "rm", "-1"]),
                dependencies,
                expected: preformatted_failure(
                    ExitStatus::Usage,
                    "error: unexpected argument '-1' found\n\n  tip: to pass '-1' as a value, use '-- -1'\n\nUsage: agens sessions rm <IDENTIFIER>\n\nFor more information, try '--help'.\n",
                ),
                _temporary: temporary,
            }
        },
        // W-C: `--` is only rejected as the SOLE root argument
        // (`root_shape_conflict`); everywhere else clap's own "end of
        // options" handling consumes it and the shape runs for real. The
        // hand-rolled parser refused `--` at any position.
        {
            let temporary = TemporaryDirectory::new("ratified-models-double-dash");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "W-C: models -- runs, it is not rejected like a bare --",
                argv: argv(&["models", "--"]),
                dependencies,
                expected: success(concat!(
                    "ID\tNAME\tCONTEXT\tPRICE\n",
                    "gpt-4.1\tGPT-4.1\t1047576\t$2.00/$8.00\n",
                    "gpt-4.1-mini\tGPT-4.1 mini\t1047576\t$0.40/$1.60\n",
                    "gpt-4.1-nano\tGPT-4.1 nano\t1047576\t$0.10/$0.40\n",
                    "gpt-4o\tGPT-4o\t128000\t$2.50/$10.00\n",
                    "gpt-4o-mini\tGPT-4o mini\t128000\t$0.15/$0.60\n",
                    "o3\to3\t200000\t$2.00/$8.00\n",
                    "o4-mini\to4-mini\t200000\t$1.10/$4.40\n",
                )),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("ratified-config-init-double-dash");
            let dependencies = base_dependencies(&temporary).with_create_file(|_, _| Ok(()));
            let expected_stdout = format!(
                "Wrote {}\n",
                config_project_config_path(&temporary).display()
            );
            Case {
                name: "W-C: config init -- writes the file, it does not refuse the shape",
                argv: argv(&["config", "init", "--"]),
                dependencies,
                expected: success(expected_stdout),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("ratified-auth-logout-double-dash");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "W-C: auth logout -- openai-api performs the logout, it does not refuse the shape",
                argv: argv(&["auth", "logout", "--", "openai-api"]),
                dependencies,
                expected: success("No credentials stored for openai-api.\n"),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("ratified-chat-double-dash-before-prompt");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "W-C: chat -- hi reaches the headless turn, it does not refuse the shape",
                argv: argv(&["chat", "--", "hi"]),
                dependencies,
                expected: failure(
                    ExitStatus::Unavailable,
                    "unavailable: this command is not implemented yet",
                ),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("ratified-chat-double-dash-after-prompt");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "W-C: chat hi -- reaches the headless turn, it does not refuse the shape",
                argv: argv(&["chat", "hi", "--"]),
                dependencies,
                expected: failure(
                    ExitStatus::Unavailable,
                    "unavailable: this command is not implemented yet",
                ),
                _temporary: temporary,
            }
        },
        // W-D / W6: help in a non-first root position, or repeated on
        // `chat`, now wins where the hand-rolled parser rejected it —
        // `root_shape_conflict` only refuses a help/version alias that is
        // the FIRST token with a trailing argument, and clap itself always
        // honors `--help` as a flag regardless of position on `chat`.
        {
            let temporary = TemporaryDirectory::new("ratified-resume-then-help");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "W-D: --resume --help renders root help, it is not a usage error",
                argv: argv(&["--resume", "--help"]),
                dependencies,
                expected: success(parser_surface_baseline::ROOT_HELP),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("ratified-chat-help-twice");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "W-D: chat --help --help renders chat's help, it is not a usage error",
                argv: argv(&["chat", "--help", "--help"]),
                dependencies,
                expected: success(parser_surface_baseline::CHAT_HELP),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("ratified-chat-help-before-prompt");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "W6: chat --help hi renders chat's help, it is not a usage error",
                argv: argv(&["chat", "--help", "hi"]),
                dependencies,
                expected: success(parser_surface_baseline::CHAT_HELP),
                _temporary: temporary,
            }
        },
        {
            let temporary = TemporaryDirectory::new("ratified-chat-help-after-prompt");
            let dependencies = base_dependencies(&temporary);
            Case {
                name: "W6: chat hi --help renders chat's help, it is not a usage error",
                argv: argv(&["chat", "hi", "--help"]),
                dependencies,
                expected: success(parser_surface_baseline::CHAT_HELP),
                _temporary: temporary,
            }
        },
    ];

    run_table_a(cases);
}
