//! Pure argv-shape parsing for the `agens` binary.
//!
//! This module owns only the SHAPE of the command line: subcommand names,
//! flag arity, and clap's own help/version/error rendering. It performs no
//! I/O and holds no `CliDependencies`. Every command body keeps its own
//! domain validation (numeric-id checks, provider names, prompt arity) and
//! its own exact `CliError` messages; only genuine shape errors are allowed
//! to carry clap's wording.
//!
//! `Auth` and `Sessions` deliberately do NOT model their subcommands as a
//! typed clap `Subcommand` enum. A typed enum makes clap itself reject a
//! missing or unrecognized inner subcommand before the command body ever
//! runs, which would replace several already-pinned domain messages (e.g.
//! `auth requires status, login, or logout`, produced by the D3 guard
//! today) with clap's own wording. Capturing the trailing tokens as a raw
//! `Vec<String>` instead lets clap own only the top-level command name,
//! while the command body keeps parsing and validating its own arguments
//! exactly as it did before clap existed.

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
        #[arg(allow_hyphen_values = true)]
        arguments: Vec<String>,
    },
    #[command(about = "run a headless agent turn")]
    Chat(ChatArgs),
    #[command(about = "list provider models")]
    Models,
    #[command(about = "inspect completed turns")]
    Sessions {
        #[arg(allow_hyphen_values = true)]
        arguments: Vec<String>,
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
    // Every trailing token that clap does not recognize as one of the
    // flags above: the prompt itself, any extra positional, and any
    // unrecognized flag. Arity and unknown-flag detection stay in the
    // command body so their exact wording survives clap adoption. This is
    // a plain comment, not a doc comment: a doc comment here becomes the
    // rendered `--help` text for this argument.
    #[arg(allow_hyphen_values = true)]
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
