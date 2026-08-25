use std::path::{Path, PathBuf};

use agens_tools::ReadFileInput;

use agens_bootstrap::Bootstrap;
use agens_error::{CliError, ExitStatus};
use agens_session::context::SessionContext;
use agens_store::{
    guess_mime_from_bytes, guess_mime_from_path, ingest_media_bytes, ingest_media_path,
    is_media_mime,
};
use agens_tool_runtime::runtime::open_native_tools;

/// The session's staged media as prompt attachments (durable id + mime pairs).
pub(crate) fn session_staged_media(context: &SessionContext) -> Vec<agens_core::PromptAttachment> {
    context
        .pending_media_ids
        .iter()
        .zip(context.pending_media_mimes.iter())
        .map(|(media_id, mime)| agens_core::PromptAttachment::new(*media_id, mime.clone()))
        .collect()
}

/// What a lookup actually proved about a recorded media id.
///
/// Reachability and permanence are different claims. Only a missing index row or a blob that is
/// provably gone proves an attachment unreachable; every other failure proves nothing, so the
/// attachment must be kept and reported as unchecked instead of being dropped as gone.
pub(crate) enum RestoredMediaCheck {
    Reachable,
    ProvenGone,
    Unverified,
}

/// Classifies a recorded media id for staged-media replacement.
pub(crate) fn check_restored_media(data_directory: &Path, media_id: i64) -> RestoredMediaCheck {
    match agens_store::open_media(data_directory, media_id) {
        Ok((_mime, _path)) => RestoredMediaCheck::Reachable,
        Err(
            agens_store::MediaStoreError::NotFound { .. } | agens_store::MediaStoreError::Io { .. },
        ) => RestoredMediaCheck::ProvenGone,
        Err(_) => RestoredMediaCheck::Unverified,
    }
}

/// Reports what a restore did to attachments it could not stage as recorded.
///
/// `dropped` counts the ones proven unreachable, `unverified` the ones whose lookup failed
/// without proving anything — the latter stay staged, so the two claims must not be merged.
pub(crate) fn restored_attachments_notice(dropped: usize, unverified: usize) -> Option<String> {
    let mut parts = Vec::new();

    if dropped > 0 {
        parts.push(dropped_attachments_notice(dropped));
    }
    if unverified > 0 {
        parts.push(unverified_attachments_notice(unverified));
    }

    (!parts.is_empty()).then(|| parts.join(" "))
}

fn unverified_attachments_notice(unverified: usize) -> String {
    if unverified == 1 {
        "1 restored attachment could not be checked and was kept staged.".to_owned()
    } else {
        format!("{unverified} restored attachments could not be checked and were kept staged.")
    }
}

fn dropped_attachments_notice(dropped: usize) -> String {
    if dropped == 1 {
        "1 restored attachment is no longer available and was dropped.".to_owned()
    } else {
        format!("{dropped} restored attachments are no longer available and were dropped.")
    }
}

const TUI_SELECT_FILE_LIMIT: usize = 100;
/// Hard cap on `@` picker entries: enumeration is one bounded walk of the
/// project root, kept in memory for the whole session so no keystroke and no
/// frame ever touches the filesystem.
const TUI_PICKER_FILE_LIMIT: usize = 2_000;

pub fn tui_file_candidates(
    context: &SessionContext,
    bootstrap: &Bootstrap,
) -> Result<Vec<String>, CliError> {
    tui_file_candidates_with_limit(context, bootstrap, TUI_SELECT_FILE_LIMIT)
}

pub fn tui_picker_file_candidates(
    context: &SessionContext,
    bootstrap: &Bootstrap,
) -> Result<Vec<String>, CliError> {
    tui_file_candidates_with_limit(context, bootstrap, TUI_PICKER_FILE_LIMIT)
}

/// Bounded, ignore-aware project files read through the confined native tools,
/// so no candidate can ever name a path outside the project root.
///
/// Resolves the root through [`agens_session::root::resolve_tui_session_root`]: a resumed
/// session's own recorded root, or the process's own discovered root for a session that has not
/// been created yet. This must never re-derive the process's discovered root directly, or a
/// resumed session confined to a different root than the resuming process's working directory
/// would leak that other root's file listing.
pub fn tui_file_candidates_with_limit(
    context: &SessionContext,
    bootstrap: &Bootstrap,
    limit: usize,
) -> Result<Vec<String>, CliError> {
    let project_root = agens_session::root::resolve_tui_session_root(context, bootstrap)?;
    open_native_tools(&project_root, bootstrap.tool_limits())?
        .tui_file_candidates(limit)
        .map_err(|output| CliError::new(ExitStatus::Failure, "file", output.content))
}

pub fn selected_tui_file(
    context: &SessionContext,
    bootstrap: &Bootstrap,
    selection: &str,
) -> Result<String, CliError> {
    if selection.is_empty() || selection.chars().count() > 121 {
        return Err(CliError::usage("selected file is invalid"));
    }

    tui_select_candidates(context, bootstrap)?
        .into_iter()
        .find(|candidate| candidate == selection)
        .ok_or_else(|| CliError::usage("selected file is unavailable"))
}

pub fn tui_select_candidates(
    context: &SessionContext,
    bootstrap: &Bootstrap,
) -> Result<Vec<String>, CliError> {
    Ok(tui_file_candidates(context, bootstrap)?
        .into_iter()
        .filter(|path| path.chars().count() <= 121)
        .take(64)
        .collect())
}

/// Result of expanding `@` tokens: UTF-8 text files are inlined; media files are ingested.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpandedTuiPrompt {
    pub text: String,
    pub media_ids: Vec<i64>,
    pub media_mimes: Vec<String>,
}

/// Resolves the root the same way [`tui_file_candidates_with_limit`] does; see that function's
/// documentation for why the session's own recorded root must be used instead of re-deriving the
/// process's discovered root.
pub fn expand_tui_file_reference(
    context: &SessionContext,
    bootstrap: &Bootstrap,
    prompt: &str,
) -> Result<String, CliError> {
    Ok(expand_tui_prompt_with_media(context, bootstrap, prompt)?.text)
}

/// Expands `@path` tokens: media paths are ingested (ids returned, token omitted from text);
/// UTF-8 text paths are inlined as before.
///
/// Media paths use the same project-root confinement as text `@` / `read_file`: `..`, absolute
/// paths outside the root, and symlink escapes are rejected.
pub fn expand_tui_prompt_with_media(
    context: &SessionContext,
    bootstrap: &Bootstrap,
    prompt: &str,
) -> Result<ExpandedTuiPrompt, CliError> {
    let project_root = agens_session::root::resolve_tui_session_root(context, bootstrap)?;
    let tools = open_native_tools(&project_root, bootstrap.tool_limits())?;
    let mut expanded = String::with_capacity(prompt.len());
    let mut media_ids = Vec::new();
    let mut media_mimes = Vec::new();

    for segment in prompt.split_inclusive(char::is_whitespace) {
        let token = segment.trim_end_matches(char::is_whitespace);
        let whitespace = &segment[token.len()..];
        if let Some(path) = token.strip_prefix('@').filter(|path| !path.is_empty()) {
            if let Some(mime) = guess_mime_from_path(Path::new(path)).filter(|m| is_media_mime(m)) {
                let absolute = confine_project_path(&project_root, Path::new(path))?;
                let record = ingest_media_path(bootstrap.data_directory(), &absolute, &mime)
                    .map_err(|error| {
                        CliError::new(
                            ExitStatus::Failure,
                            "file",
                            format!("attach failed: {error}"),
                        )
                    })?;
                media_ids.push(record.id);
                media_mimes.push(record.mime);
            } else {
                let output = tools
                    .read_file(ReadFileInput::new(path))
                    .map_err(|_| CliError::new(ExitStatus::Failure, "file", "read failed"))?;
                if output.is_error {
                    return Err(CliError::new(ExitStatus::Failure, "file", output.content));
                }
                expanded.push_str(&format!(
                    "<file path=\"{path}\">\n{}\n</file>",
                    output.content
                ));
            }
        } else {
            expanded.push_str(token);
        }
        expanded.push_str(whitespace);
    }

    Ok(ExpandedTuiPrompt {
        text: expanded,
        media_ids,
        media_mimes,
    })
}

/// Ingests a path into the durable media store (surface calls ingest only).
pub fn ingest_tui_media_path(
    bootstrap: &Bootstrap,
    path: &Path,
) -> Result<(i64, String), CliError> {
    let mime = guess_mime_from_path(path)
        .filter(|mime| is_media_mime(mime))
        .ok_or_else(|| CliError::usage(format!("unsupported media type: {}", path.display())))?;
    let record = ingest_media_path(bootstrap.data_directory(), path, &mime).map_err(|error| {
        CliError::storage(format!("attach failed for {}: {error}", path.display()))
    })?;
    Ok((record.id, record.mime))
}

/// Ingests raw image/PDF bytes (clipboard paste path).
pub fn ingest_tui_media_bytes(
    bootstrap: &Bootstrap,
    bytes: &[u8],
    mime_hint: Option<&str>,
) -> Result<(i64, String), CliError> {
    let mime = mime_hint
        .map(str::to_owned)
        .or_else(|| guess_mime_from_bytes(bytes))
        .filter(|mime| is_media_mime(mime))
        .ok_or_else(|| CliError::usage("clipboard image type is unsupported"))?;
    let record = ingest_media_bytes(bootstrap.data_directory(), bytes, &mime)
        .map_err(|error| CliError::storage(format!("clipboard attach failed: {error}")))?;
    Ok((record.id, record.mime))
}

/// Resolves `/attach PATH` relative to the session project root when not absolute.
///
/// Absolute paths must still resolve under the session project root; `..` and symlink escapes are
/// rejected the same way as `@` media expansion.
pub fn resolve_attach_path(
    context: &SessionContext,
    bootstrap: &Bootstrap,
    raw: &str,
) -> Result<PathBuf, CliError> {
    let project_root = agens_session::root::resolve_tui_session_root(context, bootstrap)?;
    confine_project_path(&project_root, Path::new(raw.trim()))
}

/// Confines an attach path to `project_root`: rejects empty paths, `..` traversal, targets outside
/// the root, and symlink escapes (via canonicalize). Relative paths resolve under the root;
/// absolute paths are accepted only when they stay under the root.
pub(crate) fn confine_project_path(project_root: &Path, raw: &Path) -> Result<PathBuf, CliError> {
    if raw.as_os_str().is_empty() {
        return Err(CliError::usage("attach path is empty"));
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
            return Err(CliError::new(
                ExitStatus::Failure,
                "file",
                "path: traversal is not allowed",
            ));
        }
        project_root.join(raw)
    };

    let root = project_root
        .canonicalize()
        .map_err(|error| CliError::new(ExitStatus::Failure, "file", format!("path: {error}")))?;
    let resolved = candidate
        .canonicalize()
        .map_err(|error| CliError::new(ExitStatus::Failure, "file", format!("path: {error}")))?;

    if !resolved.starts_with(&root) {
        return Err(CliError::new(
            ExitStatus::Failure,
            "file",
            "path: outside project root",
        ));
    }

    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use agens_store::SessionStore;
    use agens_tools::SkillCatalog;
    use agens_tui::Tui;

    use super::*;
    use crate::engine::ProductionTuiEngine;
    use crate::resume::resume_tui_session;
    use crate::test_support::{
        bootstrap_from_a_different_working_directory, persist_tui_session, tui_project,
        tui_session_bootstrap, tui_session_directory,
    };
    use agens_session::provider::CredentialResolver;

    #[test]
    fn a_resumed_session_confines_the_picker_and_at_file_expansion_to_its_own_recorded_root() {
        let origin = tui_session_directory("files-confinement-origin");
        let creation_bootstrap = tui_session_bootstrap(&origin, &[]);
        let mut store = SessionStore::open(creation_bootstrap.data_directory()).unwrap();
        let metadata = persist_tui_session(&mut store, &tui_project(&origin), "origin");
        drop(store);
        std::fs::write(origin.join("project/secret.txt"), "origin-secret").unwrap();

        let resume_bootstrap =
            bootstrap_from_a_different_working_directory(&origin, "files-confinement-elsewhere");
        let elsewhere_root =
            agens_bootstrap::session_root::discovered_root_for_tests(&resume_bootstrap);
        // Deliberately a DIFFERENT filename than origin's, not just different content: a picker
        // leak must be provable by listing alone, without relying on the expansion assertion
        // below to carry the whole test.
        std::fs::write(
            elsewhere_root.join("only-in-elsewhere.txt"),
            "elsewhere-secret",
        )
        .unwrap();

        let context = resume_tui_session(
            &resume_bootstrap,
            metadata.id,
            &SkillCatalog::default(),
            &CredentialResolver::production(),
        )
        .unwrap()
        .context;

        assert_eq!(
            tui_file_candidates_with_limit(&context, &resume_bootstrap, 50).unwrap(),
            vec!["secret.txt".to_owned()],
            "the picker must enumerate the session's own recorded root, not the resuming \
             process's discovered root"
        );
        assert_eq!(
            expand_tui_file_reference(&context, &resume_bootstrap, "look at @secret.txt please")
                .unwrap(),
            "look at <file path=\"secret.txt\">\norigin-secret\n</file> please",
            "@file expansion must inline content from the session's own recorded root"
        );

        std::fs::remove_dir_all(origin).unwrap();
        std::fs::remove_dir_all(elsewhere_root.parent().unwrap()).unwrap();
    }

    #[test]
    fn tui_file_candidates_and_expansion_use_confined_reads() {
        let temporary = tui_session_directory("files");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let context = SessionContext::fresh();
        let project = temporary.join("project");
        std::fs::write(project.join("zeta.txt"), "zeta").unwrap();
        std::fs::write(project.join("alpha.txt"), "alpha").unwrap();
        let oversized = vec![b'x'; 1024 * 1024 + 1];
        std::fs::write(project.join("large.txt"), oversized).unwrap();

        assert_eq!(
            tui_file_candidates(&context, &bootstrap).unwrap(),
            vec!["alpha.txt".to_owned(), "zeta.txt".to_owned()]
        );
        assert_eq!(
            expand_tui_file_reference(&context, &bootstrap, "review @alpha.txt please").unwrap(),
            "review <file path=\"alpha.txt\">\nalpha\n</file> please"
        );
        assert_eq!(
            expand_tui_file_reference(&context, &bootstrap, "@../outside.txt")
                .unwrap_err()
                .to_string(),
            "file: path: traversal is not allowed"
        );
        assert_eq!(
            expand_tui_file_reference(&context, &bootstrap, "@large.txt")
                .unwrap_err()
                .to_string(),
            "file: read: file exceeds 1048576 byte limit"
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn at_media_path_ingests_instead_of_utf8_inline() {
        let temporary = tui_session_directory("files-media-at");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let _store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let context = SessionContext::fresh();
        let project = temporary.join("project");
        std::fs::write(project.join("shot.png"), b"png-bytes").unwrap();
        std::fs::write(project.join("notes.txt"), "hello notes").unwrap();

        let expanded =
            expand_tui_prompt_with_media(&context, &bootstrap, "see @shot.png and @notes.txt")
                .unwrap();
        assert_eq!(expanded.media_ids.len(), 1);
        assert_eq!(expanded.media_mimes, vec!["image/png".to_owned()]);
        assert!(
            !expanded.text.contains("png-bytes"),
            "media must not be inlined as text: {}",
            expanded.text
        );
        assert!(
            expanded.text.contains("<file path=\"notes.txt\">"),
            "UTF-8 @ text expansion must remain: {}",
            expanded.text
        );
        assert!(expanded.text.contains("hello notes"));

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn at_media_path_rejects_parent_traversal_and_outside_absolute_paths() {
        let temporary = tui_session_directory("files-media-confine");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let _store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let context = SessionContext::fresh();
        let project = temporary.join("project");
        std::fs::write(project.join("inside.png"), b"inside-png").unwrap();
        std::fs::write(temporary.join("outside.png"), b"outside-png").unwrap();

        let err = expand_tui_prompt_with_media(&context, &bootstrap, "see @../outside.png")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("traversal") || err.contains("outside project root"),
            "{err}"
        );

        let outside_absolute = temporary.join("outside.png");
        let err = expand_tui_prompt_with_media(
            &context,
            &bootstrap,
            &format!("see @{}", outside_absolute.display()),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("outside project root") || err.contains("traversal"),
            "{err}"
        );

        let ok = expand_tui_prompt_with_media(&context, &bootstrap, "see @inside.png").unwrap();
        assert_eq!(ok.media_ids.len(), 1);
        assert_eq!(ok.media_mimes, vec!["image/png".to_owned()]);

        let confined =
            resolve_attach_path(&context, &bootstrap, outside_absolute.to_str().unwrap())
                .unwrap_err()
                .to_string();
        assert!(
            confined.contains("outside project root") || confined.contains("traversal"),
            "{confined}"
        );

        let attach_ok = resolve_attach_path(&context, &bootstrap, "inside.png").unwrap();
        assert_eq!(
            attach_ok.canonicalize().unwrap(),
            project.join("inside.png").canonicalize().unwrap()
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn ingest_tui_media_path_and_bytes_produce_durable_ids() {
        let temporary = tui_session_directory("files-ingest");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let _store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let path = temporary.join("project/clip.png");
        std::fs::write(&path, b"clip-png").unwrap();

        let (id, mime) = ingest_tui_media_path(&bootstrap, &path).unwrap();
        assert!(id > 0);
        assert_eq!(mime, "image/png");

        let png = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n', 1, 2, 3];
        let (id2, mime2) = ingest_tui_media_bytes(&bootstrap, &png, None).unwrap();
        assert!(id2 > 0);
        assert_eq!(mime2, "image/png");

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn the_file_picker_inserts_a_relative_path_the_confined_expansion_resolves() {
        let temporary = tui_session_directory("picker");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let context = SessionContext::fresh();
        let project = temporary.join("project");
        std::fs::create_dir_all(project.join("nested/deep")).unwrap();
        std::fs::write(project.join("nested/deep/alpha.txt"), "alpha").unwrap();
        std::fs::write(project.join("zeta.txt"), "zeta").unwrap();

        let candidates = tui_picker_file_candidates(&context, &bootstrap).unwrap();
        assert_eq!(
            candidates,
            vec!["nested/deep/alpha.txt".to_owned(), "zeta.txt".to_owned()]
        );

        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        });
        tui.set_file_candidates(candidates);
        for character in "review @alpha".chars() {
            tui.handle(agens_tui::Event::Key(agens_tui::Key::Char(character)));
        }
        assert_eq!(
            tui.view().file_picker.unwrap().matches(),
            vec!["nested/deep/alpha.txt"]
        );

        tui.handle(agens_tui::Event::Key(agens_tui::Key::Enter));
        let prompt = tui.input().to_owned();

        assert_eq!(prompt, "review @nested/deep/alpha.txt");
        assert_eq!(
            expand_tui_file_reference(&context, &bootstrap, &prompt).unwrap(),
            "review <file path=\"nested/deep/alpha.txt\">\nalpha\n</file>"
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn picker_candidates_stay_capped_and_confined_to_the_project_root() {
        let temporary = tui_session_directory("picker-cap");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let context = SessionContext::fresh();
        let project = temporary.join("project");
        std::fs::write(temporary.join("outside.txt"), "outside").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(temporary.join("outside.txt"), project.join("escape.txt"))
            .unwrap();
        for index in 0..150 {
            std::fs::write(project.join(format!("file-{index:03}.txt")), "body").unwrap();
        }

        let capped = tui_file_candidates_with_limit(&context, &bootstrap, 64).unwrap();

        assert_eq!(capped.len(), 64);
        assert_eq!(capped.first().map(String::as_str), Some("file-000.txt"));
        assert!(
            capped
                .iter()
                .all(|path| path.starts_with("file-") && !path.contains("..")),
            "{capped:?}"
        );
        assert_eq!(
            tui_picker_file_candidates(&context, &bootstrap)
                .unwrap()
                .len(),
            150
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }
}
