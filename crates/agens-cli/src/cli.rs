//! Pure argv-shape parsing for the `agens` binary.
//!
//! This module owns the SHAPE of the command line end to end: every
//! subcommand (including `auth`'s and `sessions`' own inner subcommands),
//! flag arity, and clap's help/version/error rendering. It performs no I/O
//! and holds no `CliDependencies`. Command bodies in `lib.rs` keep only
//! genuine domain validation that clap cannot express as shape (numeric-id
//! parsing, provider-name validation, the `--device-auth`/`api-key`
//! mutual-exclusion guard) and their own exact `CliError` messages for
//! those cases; every other parse failure carries clap's own wording.

use std::path::PathBuf;

use clap::error::ErrorKind;
use clap::{Args, CommandFactory, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "agens",
    bin_name = "agens",
    version,
    no_binary_name = true,
    about = "Agens is a coding agent CLI",
    disable_help_subcommand = true
)]
pub(crate) struct Cli {
    /// Resume the most recent session, or the given session id.
    #[arg(long, value_name = "SESSION_ID")]
    pub(crate) resume: Option<Option<i64>>,
    // Both modes stay explicit even though attached is the default. This keeps
    // scripts readable and gives daemon startup failures one stable local exit.
    /// Run the turn in this process
    #[arg(long, conflicts_with = "attach")]
    pub(crate) local: bool,
    /// Run the turn in the machine's daemon
    #[arg(long)]
    pub(crate) attach: bool,
    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}

impl Cli {
    /// Where this invocation's turns run.
    ///
    /// Attached unless local execution was explicitly requested.
    pub(crate) const fn tui_mode(&self) -> crate::tui::TuiMode {
        if self.local {
            crate::tui::TuiMode::Local
        } else {
            crate::tui::TuiMode::Attached
        }
    }
}

#[derive(Subcommand, Debug)]
pub(crate) enum Command {
    #[command(about = "inspect configuration")]
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    #[command(about = "inspect supported authentication")]
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
    #[command(about = "run a headless agent turn")]
    Chat(ChatArgs),
    #[command(about = "list provider models")]
    Models,
    #[command(about = "attach the terminal to a chat in the machine daemon")]
    Attach {
        /// The hosted session to resume. Without one, attach to this checkout's latest chat.
        target: Option<i64>,
    },
    #[command(about = "enter team mode or inspect the machine fleet")]
    Team {
        /// An optional first prompt for the attached chat. Fleet operations are `ls [--json]`,
        /// `show <id> [--follow]`, `answer <question-id> <answer>`,
        /// `answer <chat-id> <prompt-id> <option-id>`,
        /// `permission <chat-id> <prompt-id> <answer>`, `merge <approval-question-id>`, and
        /// `cancel <id>`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        prompt: Vec<String>,
    },
    #[command(about = "run the headless daemon for this machine")]
    Serve {
        /// Stay attached to the terminal instead of detaching. This is the
        /// shape a process supervisor starts the daemon in.
        #[arg(long)]
        foreground: bool,
        #[command(subcommand)]
        action: Option<ServeAction>,
    },
    #[command(about = "inspect completed turns")]
    Sessions {
        #[command(subcommand)]
        action: SessionsAction,
    },
    #[command(about = "queue a message for a running session, delivered at its next safe point")]
    Direct {
        /// The session the message is for; a session only ever reads its own.
        #[arg(required_unless_present = "child", conflicts_with = "child")]
        session: Option<String>,
        /// A delegated child turn instead of a session, named by the reference
        /// its `turn_started` diagnostic published. A child holds no session of
        /// its own, so it reads only what names it.
        #[arg(long, conflicts_with = "at_turn_end")]
        child: Option<String>,
        /// Answer the open question with this id instead of sending a prompt.
        #[arg(long, value_name = "QUESTION_ID", conflicts_with = "at_turn_end")]
        answer: Option<String>,
        /// Wait for the turn to end instead of the next tool batch. Use it when
        /// the message changes what the run is doing and the worker has to
        /// replan from a settled plan.
        #[arg(long)]
        at_turn_end: bool,
        /// Send with the narrower authority of an automated supervisor rather
        /// than as the user.
        #[arg(long)]
        as_supervisor: bool,
        /// The message itself.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        message: Vec<String>,
    },
    /// Bare `agens version`; `--version`/`-V` are handled by clap itself.
    #[command(hide = true)]
    Version,
}

#[derive(Subcommand, Debug)]
pub(crate) enum ConfigAction {
    #[command(about = "report the effective configuration and where each setting came from")]
    Doctor,
    #[command(about = "write a starter configuration file")]
    Init {
        /// Write the starter configuration to the global path instead of
        /// the project path.
        #[arg(long)]
        global: bool,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum AuthAction {
    #[command(about = "report authentication status for ChatGPT or an API-key provider")]
    Status { provider: Option<String> },
    #[command(about = "log in to ChatGPT or an API-key provider")]
    Login {
        /// Use the device-code flow instead of opening a browser.
        #[arg(long)]
        device_auth: bool,
        #[command(subcommand)]
        method: Option<LoginMethod>,
    },
    #[command(about = "remove stored credentials for a provider")]
    Logout { provider: String },
}

#[derive(Subcommand, Debug)]
pub(crate) enum LoginMethod {
    #[command(about = "log in to a ChatGPT subscription through OAuth")]
    Chatgpt {
        /// Use the device-code flow instead of opening a browser.
        #[arg(long)]
        device_auth: bool,
    },
    #[command(about = "log in with an API key instead of ChatGPT")]
    ApiKey {
        provider: String,
        #[arg(long)]
        api_key: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum ServeAction {
    #[command(
        about = "authorize a repository's provisioning hooks, which run with the daemon's environment"
    )]
    Trust {
        /// The checkout to trust. It has to be one the daemon serves, which is
        /// what `team.project_roots` says.
        repository: PathBuf,
    },
    #[command(about = "stop the daemon running for this machine")]
    Stop,
    #[command(about = "report the running daemon's pid, socket, uptime and active runs")]
    Status,
}

#[derive(Subcommand, Debug)]
pub(crate) enum SessionsAction {
    #[command(about = "list saved sessions")]
    List,
    #[command(about = "show a saved session's details")]
    Show { identifier: String },
    #[command(about = "remove a saved session")]
    Rm { identifier: String },
}

#[derive(Args, Debug)]
pub(crate) struct ChatArgs {
    #[arg(long)]
    pub(crate) model: Option<String>,
    #[arg(long)]
    pub(crate) system: Option<String>,
    #[arg(long)]
    pub(crate) max_iterations: Option<usize>,
    #[arg(long, value_name = "chat|edit")]
    pub(crate) mode: Option<String>,
    #[arg(long)]
    pub(crate) dangerously_allow_all: bool,
    // Repeatable media paths; ingest happens in `commands/chat` after bootstrap.
    #[arg(long = "attach", value_name = "PATH", action = clap::ArgAction::Append)]
    pub(crate) attach: Vec<std::path::PathBuf>,
    // Every trailing token clap does not recognize as one of the flags
    // above: the prompt itself, any extra positional, and any
    // unrecognized flag. `allow_hyphen_values` is intentionally NOT set
    // here: without it, clap keeps matching `--model`/`--system`/etc. as
    // flags no matter where they appear relative to the prompt, and only
    // genuinely unrecognized hyphen-prefixed tokens fall through to this
    // positional for the command body to reject. Arity and unknown-flag
    // detection stay in the command body so their exact wording survives
    // clap adoption. This is a plain comment, not a doc comment: a doc
    // comment here becomes the rendered `--help` text for this argument.
    pub(crate) prompt: Vec<String>,
}

/// `agens 42` resumes session 42. A bare positional integer cannot coexist
/// with a subcommand enum in clap's grammar, so it is intercepted before
/// parsing.
pub(crate) fn resume_shorthand(arguments: &[String]) -> Option<i64> {
    match arguments {
        [identifier] => identifier.parse::<i64>().ok(),
        _ => None,
    }
}

/// `--resume=<value>` bypasses clap's flag/value disambiguation: an
/// `=`-joined value is assigned directly and never mistaken for another
/// flag, unlike a space-separated value. That disambiguation is exactly why
/// `--resume -5` is a usage error instead of resolving to session -5, so
/// without this normalization the `=` spelling would resume a negative
/// session id for real. Splitting `--resume=-<digits>` into the equivalent
/// two-token `--resume -<digits>` before parsing routes it through the
/// identical disambiguation the space-separated form already goes through.
/// Non-numeric or non-negative `=` values (`--resume=abc`, `--resume=5`) are
/// left untouched: those either already fail with clap's own `invalid
/// value` message, or are accepted deliberately.
pub(crate) fn normalize_resume_equals_negative(arguments: &[String]) -> Vec<String> {
    let mut normalized = Vec::with_capacity(arguments.len());
    for argument in arguments {
        match argument.strip_prefix("--resume=") {
            Some(value) if is_negative_integer(value) => {
                normalized.push("--resume".to_owned());
                normalized.push(value.to_owned());
            }
            _ => normalized.push(argument.clone()),
        }
    }
    normalized
}

fn is_negative_integer(value: &str) -> bool {
    value.strip_prefix('-').is_some_and(|digits| {
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn is_help(argument: &str) -> bool {
    matches!(argument, "--help" | "-h")
}

fn is_version(argument: &str) -> bool {
    matches!(argument, "--version" | "-V" | "version")
}

/// Help is available ONLY as `--help`/`-h`; there is no bare-word `help`
/// path anywhere in this CLI (clap's auto-generated `help` pseudo-subcommand
/// is disabled via `disable_help_subcommand` on [`Cli`]). clap still honors
/// `--help`/`-h`/`--version`/`-V` (and treats a bare `--` as "no more
/// options", falling through to the TUI) regardless of any trailing token,
/// which silently discards garbage that the historical single-argument
/// matcher rejected. Detects those two shapes ahead of parsing and returns
/// the `clap::Error` that reproduces the historical Usage(2) outcome; `None`
/// means clap should parse `arguments` normally.
pub(crate) fn root_shape_conflict(arguments: &[String]) -> Option<clap::Error> {
    if matches!(arguments, [only] if only == "--") {
        return Some(unrecognized_argument_error("--"));
    }
    if let [first, ..] = arguments
        && arguments.len() > 1
        && (is_help(first) || is_version(first))
    {
        return Some(unrecognized_argument_error(first));
    }
    None
}

fn unrecognized_argument_error(token: &str) -> clap::Error {
    Cli::command().error(
        ErrorKind::UnknownArgument,
        format!("unrecognized argument '{token}'"),
    )
}

/// clap's rendered text is emitted verbatim: it already carries its own
/// `error: ` prefix and usage block, so re-wrapping it in the
/// `error: {category}: {message}` envelope would double the prefix.
pub(crate) fn clap_outcome(error: clap::Error) -> Result<String, crate::CliError> {
    match error.kind() {
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => Ok(rendered(&error)),
        _ => Err(crate::CliError::preformatted_usage(rendered(&error))),
    }
}

fn rendered(error: &clap::Error) -> String {
    let mut text = error.render().to_string();
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}
