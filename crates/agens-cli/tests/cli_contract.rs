//! Black-box characterization suite for the crate's public argv contract
//! (`cli-clap-and-module-split`, Phase 0).
//!
//! This file pins what the CURRENT hand-rolled parser in `lib.rs` actually
//! returns, not what it "should" return. Every assertion is a full-equality
//! check on `(status, stdout, stderr)`. If a case here fails after a later
//! phase, the parser's observable behavior changed and that is a defect,
//! unless the case belongs to [`parser_surface_baseline`].

use std::collections::BTreeMap;
use std::path::PathBuf;

use agens::{CliDependencies, CommandResult, ExitStatus, execute};

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
