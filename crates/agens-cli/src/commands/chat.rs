//! The `chat` command: builds a headless chat request from clap-parsed flags
//! and drives it to completion under the configured bootstrap.

use std::path::{Path, PathBuf};

use agens_core::{HeadlessTurnCancellation, PermissionMode};
use agens_error::{CliError, cancellation_result};
use agens_headless::HeadlessChatRequest;
use agens_headless::seed_configured_reasoning_effort;
use agens_store::{guess_mime_from_path, ingest_media_path};

use crate::CliDependencies;
use crate::cli;
use crate::deps::bootstrap;

pub(crate) fn run_chat(
    arguments: cli::ChatArgs,
    dependencies: &CliDependencies,
    cancellation: &HeadlessTurnCancellation,
) -> Result<String, CliError> {
    let attach = arguments.attach.clone();
    let mut request = chat_request(arguments)?;
    cancellation_result(cancellation)?;
    let bootstrap = bootstrap(dependencies)?;
    apply_chat_attachments(
        &mut request,
        bootstrap.data_directory(),
        bootstrap.project_root.as_deref(),
        &attach,
    )?;
    seed_configured_reasoning_effort(&mut request, &bootstrap);
    let output = (dependencies.headless_chat)(request, &bootstrap, cancellation)?;
    cancellation_result(cancellation)?;

    Ok(format!("{output}\n"))
}

/// Builds the headless chat request from clap-parsed flags. clap already
/// owns the shape and type of `--model`/`--system`/`--max-iterations`/
/// `--mode`/`--dangerously-allow-all`/`--attach`; this function keeps the
/// domain validation clap cannot express (arity of the prompt,
/// `--max-iterations` range, `--mode` enum) and reproduces, on
/// `arguments.prompt`, the same left-to-right scan the hand-rolled parser
/// used: the first non-flag, non-blank token becomes the prompt, any further
/// token is rejected, and any leftover token that still looks like a flag
/// (because clap did not recognize it) is rejected as an unknown flag.
///
/// Attachments are not ingested here — that needs the bootstrap data directory
/// and runs in [`apply_chat_attachments`] from [`run_chat`].
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
    // Attach-only turns are allowed: empty/whitespace prompt is fine when --attach is present.
    if prompt.is_empty() && arguments.attach.is_empty() {
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
        media_ids: Vec::new(),
        media_mimes: Vec::new(),
    })
}

/// Ingests each `--attach` path into the durable media store and records ids/mimes on the request.
///
/// Paths are confined to `project_root` (same model as TUI attach): absolute paths outside the
/// project, `..` traversal, and symlink escapes are rejected. Attach requires a project root.
pub(crate) fn apply_chat_attachments(
    request: &mut HeadlessChatRequest,
    data_directory: &Path,
    project_root: Option<&Path>,
    paths: &[PathBuf],
) -> Result<(), CliError> {
    if paths.is_empty() {
        return Ok(());
    }
    let project_root = project_root.ok_or_else(|| {
        CliError::usage("chat --attach requires a project root (run inside a project)")
    })?;

    for path in paths {
        let confined = confine_attach_path(project_root, path)?;
        let mime = guess_mime_from_path(&confined).ok_or_else(|| {
            CliError::usage(format!(
                "chat --attach unsupported media type: {}",
                path.display()
            ))
        })?;
        let record = ingest_media_path(data_directory, &confined, &mime).map_err(|error| {
            CliError::storage(format!(
                "chat --attach failed for {}: {error}",
                path.display()
            ))
        })?;
        request.media_ids.push(record.id);
        request.media_mimes.push(record.mime);
    }
    Ok(())
}

fn confine_attach_path(project_root: &Path, raw: &Path) -> Result<PathBuf, CliError> {
    if raw.as_os_str().is_empty() {
        return Err(CliError::usage("chat --attach path is empty"));
    }

    let candidate = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        if raw.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        }) {
            return Err(CliError::usage(
                "chat --attach path: traversal is not allowed",
            ));
        }
        project_root.join(raw)
    };

    let root = project_root
        .canonicalize()
        .map_err(|error| CliError::usage(format!("chat --attach path: {error}")))?;
    let resolved = candidate
        .canonicalize()
        .map_err(|error| CliError::usage(format!("chat --attach path: {error}")))?;

    if !resolved.starts_with(&root) {
        return Err(CliError::usage("chat --attach path: outside project root"));
    }

    Ok(resolved)
}

#[cfg(test)]
pub(crate) fn chat_args_with_prompt(prompt: &str) -> cli::ChatArgs {
    cli::ChatArgs {
        model: None,
        system: None,
        max_iterations: None,
        mode: None,
        dangerously_allow_all: false,
        attach: Vec::new(),
        prompt: vec![prompt.to_owned()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agens_store::SessionStore;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    fn temp_data_dir() -> PathBuf {
        let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "agens-cli-chat-attach-{}-{suffix}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn chat_request_starts_with_empty_media() {
        let request = chat_request(chat_args_with_prompt("hello")).unwrap();
        assert!(request.media_ids.is_empty());
        assert!(request.media_mimes.is_empty());
        assert_eq!(request.prompt, "hello");
    }

    #[test]
    fn apply_chat_attachments_ingests_repeatable_paths_in_order() {
        let directory = temp_data_dir();
        let project = directory.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let _store = SessionStore::open(&directory).unwrap();
        let png = project.join("a.png");
        let jpg = project.join("b.jpg");
        std::fs::write(&png, b"png-a").unwrap();
        std::fs::write(&jpg, b"jpg-b").unwrap();

        let mut request = chat_request(chat_args_with_prompt("look")).unwrap();
        apply_chat_attachments(&mut request, &directory, Some(&project), &[png, jpg]).unwrap();

        assert_eq!(request.media_ids.len(), 2);
        assert_eq!(request.media_mimes, vec!["image/png", "image/jpeg"]);
        assert!(request.media_ids[0] > 0);
        assert_ne!(request.media_ids[0], request.media_ids[1]);

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn apply_chat_attachments_rejects_unknown_extension() {
        let directory = temp_data_dir();
        let project = directory.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let _store = SessionStore::open(&directory).unwrap();
        let path = project.join("notes.txt");
        std::fs::write(&path, b"hello").unwrap();

        let mut request = chat_request(chat_args_with_prompt("look")).unwrap();
        let error =
            apply_chat_attachments(&mut request, &directory, Some(&project), &[path]).unwrap_err();
        assert!(
            error.to_string().contains("unsupported media type"),
            "{error}"
        );

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn apply_chat_attachments_confines_paths_to_project_root() {
        let directory = temp_data_dir();
        let project = directory.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let _store = SessionStore::open(&directory).unwrap();
        let outside = directory.join("escape.png");
        std::fs::write(&outside, b"outside").unwrap();
        std::fs::write(project.join("inside.png"), b"inside").unwrap();

        let mut request = chat_request(chat_args_with_prompt("look")).unwrap();
        let err = apply_chat_attachments(&mut request, &directory, Some(&project), &[outside])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("outside project root") || err.contains("traversal"),
            "{err}"
        );

        let mut request = chat_request(chat_args_with_prompt("look")).unwrap();
        apply_chat_attachments(
            &mut request,
            &directory,
            Some(&project),
            &[PathBuf::from("inside.png")],
        )
        .unwrap();
        assert_eq!(request.media_ids.len(), 1);

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn chat_request_allows_empty_prompt_when_attach_present() {
        let args = cli::ChatArgs {
            model: None,
            system: None,
            max_iterations: None,
            mode: None,
            dangerously_allow_all: false,
            attach: vec![PathBuf::from("shot.png")],
            prompt: vec![],
        };
        let request = chat_request(args).unwrap();
        assert!(request.prompt.is_empty());
    }

    #[test]
    fn chat_args_parse_repeatable_attach_flags() {
        use clap::Parser;
        // Cli is configured with `no_binary_name = true`.
        let cli = crate::cli::Cli::try_parse_from([
            "chat", "--attach", "one.png", "--attach", "two.jpg", "describe",
        ])
        .unwrap();
        let crate::cli::Command::Chat(args) = cli.command.unwrap() else {
            panic!("expected chat");
        };
        assert_eq!(
            args.attach,
            vec![PathBuf::from("one.png"), PathBuf::from("two.jpg")]
        );
        assert_eq!(args.prompt, vec!["describe".to_owned()]);
    }
}
