use agens_tools::ReadFileInput;

use crate::bootstrap::Bootstrap;
use crate::error::{CliError, ExitStatus};
use crate::session::context::SessionContext;
use crate::tools::runtime::open_native_tools;

const TUI_SELECT_FILE_LIMIT: usize = 100;
/// Hard cap on `@` picker entries: enumeration is one bounded walk of the
/// project root, kept in memory for the whole session so no keystroke and no
/// frame ever touches the filesystem.
const TUI_PICKER_FILE_LIMIT: usize = 2_000;

pub(crate) fn tui_file_candidates(
    context: &SessionContext,
    bootstrap: &Bootstrap,
) -> Result<Vec<String>, CliError> {
    tui_file_candidates_with_limit(context, bootstrap, TUI_SELECT_FILE_LIMIT)
}

pub(crate) fn tui_picker_file_candidates(
    context: &SessionContext,
    bootstrap: &Bootstrap,
) -> Result<Vec<String>, CliError> {
    tui_file_candidates_with_limit(context, bootstrap, TUI_PICKER_FILE_LIMIT)
}

/// Bounded, ignore-aware project files read through the confined native tools,
/// so no candidate can ever name a path outside the project root.
///
/// Resolves the root through [`crate::session_root::resolve_tui_session_root`]: a resumed
/// session's own recorded root, or the process's own discovered root for a session that has not
/// been created yet. This must never re-derive the process's discovered root directly, or a
/// resumed session confined to a different root than the resuming process's working directory
/// would leak that other root's file listing.
pub(crate) fn tui_file_candidates_with_limit(
    context: &SessionContext,
    bootstrap: &Bootstrap,
    limit: usize,
) -> Result<Vec<String>, CliError> {
    let project_root = crate::session_root::resolve_tui_session_root(context, bootstrap)?;
    open_native_tools(&project_root, bootstrap.tool_limits())?
        .tui_file_candidates(limit)
        .map_err(|output| CliError::new(ExitStatus::Failure, "file", output.content))
}

pub(crate) fn selected_tui_file(
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

pub(crate) fn tui_select_candidates(
    context: &SessionContext,
    bootstrap: &Bootstrap,
) -> Result<Vec<String>, CliError> {
    Ok(tui_file_candidates(context, bootstrap)?
        .into_iter()
        .filter(|path| path.chars().count() <= 121)
        .take(64)
        .collect())
}

/// Resolves the root the same way [`tui_file_candidates_with_limit`] does; see that function's
/// documentation for why the session's own recorded root must be used instead of re-deriving the
/// process's discovered root.
pub(crate) fn expand_tui_file_reference(
    context: &SessionContext,
    bootstrap: &Bootstrap,
    prompt: &str,
) -> Result<String, CliError> {
    let project_root = crate::session_root::resolve_tui_session_root(context, bootstrap)?;
    let tools = open_native_tools(&project_root, bootstrap.tool_limits())?;
    let mut expanded = String::with_capacity(prompt.len());

    for segment in prompt.split_inclusive(char::is_whitespace) {
        let token = segment.trim_end_matches(char::is_whitespace);
        let whitespace = &segment[token.len()..];
        if let Some(path) = token.strip_prefix('@').filter(|path| !path.is_empty()) {
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
        } else {
            expanded.push_str(token);
        }
        expanded.push_str(whitespace);
    }

    Ok(expanded)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use agens_store::SessionStore;
    use agens_tools::SkillCatalog;
    use agens_tui::Tui;

    use super::*;
    use crate::test_support::{
        bootstrap_from_a_different_working_directory, persist_tui_session, tui_project,
        tui_session_bootstrap, tui_session_directory,
    };
    use crate::tui::engine::ProductionTuiEngine;
    use crate::tui::provider::TuiCredentialResolver;
    use crate::tui::resume::resume_tui_session;

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
        let elsewhere_root = crate::session_root::discovered_root_for_tests(&resume_bootstrap);
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
            &TuiCredentialResolver::production(),
        )
        .unwrap();

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
