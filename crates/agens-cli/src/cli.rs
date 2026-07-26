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
use clap::{Args, CommandFactory, Parser, Subcommand};

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
    #[command(about = "log in with an API key instead of ChatGPT")]
    ApiKey {
        provider: String,
        #[arg(long)]
        api_key: Option<String>,
    },
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

fn is_help(argument: &str) -> bool {
    matches!(argument, "--help" | "-h" | "help")
}

fn is_version(argument: &str) -> bool {
    matches!(argument, "--version" | "-V" | "version")
}

/// clap honors `--help`/`-h`/`--version`/`-V`/`help`/`version` (and treats a
/// bare `--` as "no more options", falling through to the TUI) regardless of
/// any trailing token, which silently discards garbage that the historical
/// single-argument matcher rejected. Detects those two shapes ahead of
/// parsing and returns the `clap::Error` that reproduces the historical
/// Usage(2) outcome; `None` means clap should parse `arguments` normally.
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

/// `config`/`auth`/`models`/`sessions` treat a help token ANYWHERE in their
/// own arguments as a request for help, even alongside an otherwise-invalid
/// shape — the historical `.any(is_help)` precedent from the hand-rolled
/// parser. `chat` has its own narrower rule, [`chat_help_override`].
///
/// clap already resolves help correctly for every VALID shape on its own,
/// including deeply nested subcommands (`config init --help`, `auth login
/// api-key --help`), by walking to the deepest matched subcommand, and it
/// also auto-generates a `help` pseudo-subcommand for any command that has
/// nested subcommands of its own (`config init help`, `auth login help`).
/// This override must stay out of clap's way in both of those cases, or it
/// clobbers that correct resolution with the top-level subcommand's help
/// instead.
///
/// Where clap's own resolution is NOT enough is a leaf that takes a plain
/// `String`/`Option<String>` positional rather than a nested subcommand
/// (`auth status <PROVIDER>`, `auth logout <PROVIDER>`, `auth login api-key
/// <PROVIDER>`, `sessions show <IDENTIFIER>`, `sessions rm <IDENTIFIER>`):
/// clap has no way to know the bare word `"help"` is special there, so it
/// happily binds it as the positional's value and parses the shape
/// successfully (`auth status help`, `sessions show help`). This override
/// re-checks those successful parses for a literal `"help"` token and wins
/// over them, reproducing the pre-clap precedent. It also still fires, as
/// before, when clap outright REJECTS the shape because of a token it does
/// not recognize alongside a help alias (`config extra --help`, `models
/// help`), falling back to the top-level subcommand's help for those
/// unrecognized shapes.
///
/// Returns `None` when clap should parse `arguments` normally: either it
/// resolves the shape itself (with or without help, including via its own
/// `help` pseudo-subcommand) and no bare `"help"` positional is present, or
/// the parse failure has nothing to do with a help token.
pub(crate) fn subcommand_help_override(
    arguments: &[String],
) -> Option<Result<String, crate::CliError>> {
    let [name, rest @ ..] = arguments else {
        return None;
    };
    if name == "chat" {
        return chat_help_override(rest);
    }
    if !matches!(name.as_str(), "config" | "auth" | "models" | "sessions") {
        return None;
    }
    if !rest.iter().any(|argument| is_help(argument)) {
        return None;
    }
    let bare_help_word_present = rest.iter().any(|argument| argument == "help");
    if !bare_help_word_present && clap_resolves_natively(arguments) {
        return None;
    }

    let canonical = [name.clone(), "--help".to_owned()];
    Some(match Cli::try_parse_from(canonical.iter()) {
        Err(error) => clap_outcome(error),
        Ok(_) => unreachable!("`--help` always yields a clap DisplayHelp error"),
    })
}

/// `chat`'s prompt is an unbounded `Vec<String>`, so clap has no shape that
/// ever rejects a bare `"help"` token there: it always parses successfully
/// as a one-word prompt. The historical hand-rolled parser special-cased
/// exactly one shape — a single argument that is a help alias — and nothing
/// wider, so `chat foo help` and `chat help foo` remain ordinary prompts,
/// not a help request; there is no way to pass a literal one-word prompt of
/// `"help"` without this ambiguity, and that trade-off is the restored
/// pre-clap behavior, not a gap introduced here. `--help`/`-h` are already
/// handled natively by clap regardless of position and never reach this
/// function.
fn chat_help_override(rest: &[String]) -> Option<Result<String, crate::CliError>> {
    if !matches!(rest, [only] if only == "help") {
        return None;
    }

    let canonical = ["chat".to_owned(), "--help".to_owned()];
    Some(match Cli::try_parse_from(canonical.iter()) {
        Err(error) => clap_outcome(error),
        Ok(_) => unreachable!("`--help` always yields a clap DisplayHelp error"),
    })
}

/// True when clap either accepts `arguments` outright or rejects them with
/// its own `DisplayHelp`/`DisplayVersion` outcome — the two cases where
/// clap's native resolution must be left untouched.
fn clap_resolves_natively(arguments: &[String]) -> bool {
    match Cli::try_parse_from(arguments.iter()) {
        Ok(_) => true,
        Err(error) => matches!(
            error.kind(),
            ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
        ),
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
