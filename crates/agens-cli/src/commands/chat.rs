//! The `chat` command: builds a headless chat request from clap-parsed flags
//! and drives it to completion under the configured bootstrap.

use agens_core::{HeadlessTurnCancellation, PermissionMode};

use crate::CliDependencies;
use crate::cli;
use crate::deps::bootstrap;
use agens_error::{CliError, cancellation_result};
use agens_headless::HeadlessChatRequest;
use agens_headless::seed_configured_reasoning_effort;

pub(crate) fn run_chat(
    arguments: cli::ChatArgs,
    dependencies: &CliDependencies,
    cancellation: &HeadlessTurnCancellation,
) -> Result<String, CliError> {
    let mut request = chat_request(arguments)?;
    cancellation_result(cancellation)?;
    let bootstrap = bootstrap(dependencies)?;
    seed_configured_reasoning_effort(&mut request, &bootstrap);
    let output = (dependencies.headless_chat)(request, &bootstrap, cancellation)?;
    cancellation_result(cancellation)?;

    Ok(format!("{output}\n"))
}

/// Builds the headless chat request from clap-parsed flags. clap already
/// owns the shape and type of `--model`/`--system`/`--max-iterations`/
/// `--mode`/`--dangerously-allow-all`; this function keeps the domain
/// validation clap cannot express (arity of the prompt, `--max-iterations`
/// range, `--mode` enum) and reproduces, on `arguments.prompt`, the same
/// left-to-right scan the hand-rolled parser used: the first non-flag,
/// non-blank token becomes the prompt, any further token is rejected, and
/// any leftover token that still looks like a flag (because clap did not
/// recognize it) is rejected as an unknown flag.
pub(crate) fn chat_request(arguments: cli::ChatArgs) -> Result<HeadlessChatRequest, CliError> {
    let max_iterations = match arguments.max_iterations {
        Some(0) => return Err(CliError::usage("chat --max-iterations must be >= 1")),
        other => other,
    };

    let mode = match arguments.mode.as_deref() {
        None | Some("edit") => PermissionMode::Edit,
        Some("chat") => PermissionMode::Chat,
        Some(_) => return Err(CliError::usage("chat --mode must be chat or edit")),
    };

    let mut prompt = String::new();
    for token in &arguments.prompt {
        if token.starts_with('-') {
            return Err(CliError::usage("chat received an unknown flag"));
        }
        if prompt.is_empty() && !token.trim().is_empty() {
            prompt = token.trim().to_owned();
        } else {
            return Err(CliError::usage("chat accepts one prompt argument"));
        }
    }
    if prompt.is_empty() {
        return Err(CliError::usage("chat requires a prompt argument"));
    }

    Ok(HeadlessChatRequest {
        prompt,
        history: Vec::new(),
        model: arguments.model,
        system_prompt: arguments.system,
        max_iterations,
        mode,
        dangerously_allow_all: arguments.dangerously_allow_all,
        dangerous_mode: false,
        request_config: agens_core::RequestConfig::default(),
        session_reasoning_effort: None,
        session: None,
        active_agent: None,
        effective_capabilities: None,
        pending_system_reminder: None,
        skills: None,
    })
}

#[cfg(test)]
pub(crate) fn chat_args_with_prompt(prompt: &str) -> cli::ChatArgs {
    cli::ChatArgs {
        model: None,
        system: None,
        max_iterations: None,
        mode: None,
        dangerously_allow_all: false,
        prompt: vec![prompt.to_owned()],
    }
}
