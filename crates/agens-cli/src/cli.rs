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

use clap::error::ErrorKind;
use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "agens",
    bin_name = "agens",
    version,
    no_binary_name = true,
    about = "Agens is a coding agent CLI"
)]
pub(crate) struct Cli {
    /// Resume the most recent session, or the given session id.
    #[arg(long, value_name = "SESSION_ID")]
    pub(crate) resume: Option<Option<i64>>,
    #[command(subcommand)]
    pub(crate) command: Option<Command>,
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
    #[command(about = "inspect completed turns")]
    Sessions {
        #[command(subcommand)]
        action: SessionsAction,
    },
    /// Bare `agens version`; `--version`/`-V` are handled by clap itself.
    #[command(hide = true)]
    Version,
}

#[derive(Subcommand, Debug)]
pub(crate) enum ConfigAction {
    Doctor,
    Init,
}

#[derive(Subcommand, Debug)]
pub(crate) enum AuthAction {
    Status {
        provider: Option<String>,
    },
    Login {
        #[arg(long)]
        device_auth: bool,
        #[command(subcommand)]
        method: Option<LoginMethod>,
    },
    Logout {
        provider: String,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum LoginMethod {
    ApiKey {
        provider: String,
        #[arg(long)]
        api_key: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum SessionsAction {
    List,
    Show { identifier: String },
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
