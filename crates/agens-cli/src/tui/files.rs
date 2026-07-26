use agens_tools::ReadFileInput;

use crate::bootstrap::Bootstrap;
use crate::error::{CliError, ExitStatus};
use crate::tools::runtime::open_native_tools;

const TUI_SELECT_FILE_LIMIT: usize = 100;
/// Hard cap on `@` picker entries: enumeration is one bounded walk of the
/// project root, kept in memory for the whole session so no keystroke and no
/// frame ever touches the filesystem.
const TUI_PICKER_FILE_LIMIT: usize = 2_000;

pub fn tui_file_candidates(bootstrap: &Bootstrap) -> Result<Vec<String>, CliError> {
    tui_file_candidates_with_limit(bootstrap, TUI_SELECT_FILE_LIMIT)
}

pub(crate) fn tui_picker_file_candidates(bootstrap: &Bootstrap) -> Result<Vec<String>, CliError> {
    tui_file_candidates_with_limit(bootstrap, TUI_PICKER_FILE_LIMIT)
}

/// Bounded, ignore-aware project files read through the confined native tools,
/// so no candidate can ever name a path outside the project root.
pub(crate) fn tui_file_candidates_with_limit(
    bootstrap: &Bootstrap,
    limit: usize,
) -> Result<Vec<String>, CliError> {
    let project_root = bootstrap
        .project_root()
        .ok_or_else(|| CliError::configuration("native tools require a project root"))?;
    open_native_tools(project_root, bootstrap.tool_limits())?
        .tui_file_candidates(limit)
        .map_err(|output| CliError::new(ExitStatus::Failure, "file", output.content))
}

pub(crate) fn selected_tui_file(
    bootstrap: &Bootstrap,
    selection: &str,
) -> Result<String, CliError> {
    if selection.is_empty() || selection.chars().count() > 121 {
        return Err(CliError::usage("selected file is invalid"));
    }

    tui_select_candidates(bootstrap)?
        .into_iter()
        .find(|candidate| candidate == selection)
        .ok_or_else(|| CliError::usage("selected file is unavailable"))
}

pub(crate) fn tui_select_candidates(bootstrap: &Bootstrap) -> Result<Vec<String>, CliError> {
    Ok(tui_file_candidates(bootstrap)?
        .into_iter()
        .filter(|path| path.chars().count() <= 121)
        .take(64)
        .collect())
}

pub(crate) fn expand_tui_file_reference(
    bootstrap: &Bootstrap,
    prompt: &str,
) -> Result<String, CliError> {
    let project_root = bootstrap
        .project_root()
        .ok_or_else(|| CliError::configuration("native tools require a project root"))?;
    let tools = open_native_tools(project_root, bootstrap.tool_limits())?;
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

    use agens_tui::Tui;

    use super::*;
    use crate::test_support::{tui_session_bootstrap, tui_session_directory};
    use crate::tui::engine::ProductionTuiEngine;

    #[test]
    fn tui_file_candidates_and_expansion_use_confined_reads() {
        let temporary = tui_session_directory("files");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let project = temporary.join("project");
        std::fs::write(project.join("zeta.txt"), "zeta").unwrap();
        std::fs::write(project.join("alpha.txt"), "alpha").unwrap();
        let oversized = vec![b'x'; 1024 * 1024 + 1];
        std::fs::write(project.join("large.txt"), oversized).unwrap();

        assert_eq!(
            tui_file_candidates(&bootstrap).unwrap(),
            vec!["alpha.txt".to_owned(), "zeta.txt".to_owned()]
        );
        assert_eq!(
            expand_tui_file_reference(&bootstrap, "review @alpha.txt please").unwrap(),
            "review <file path=\"alpha.txt\">\nalpha\n</file> please"
        );
        assert_eq!(
            expand_tui_file_reference(&bootstrap, "@../outside.txt")
                .unwrap_err()
                .to_string(),
            "file: path: traversal is not allowed"
        );
        assert_eq!(
            expand_tui_file_reference(&bootstrap, "@large.txt")
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
        let project = temporary.join("project");
        std::fs::create_dir_all(project.join("nested/deep")).unwrap();
        std::fs::write(project.join("nested/deep/alpha.txt"), "alpha").unwrap();
        std::fs::write(project.join("zeta.txt"), "zeta").unwrap();

        let candidates = tui_picker_file_candidates(&bootstrap).unwrap();
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
            expand_tui_file_reference(&bootstrap, &prompt).unwrap(),
            "review <file path=\"nested/deep/alpha.txt\">\nalpha\n</file>"
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn picker_candidates_stay_capped_and_confined_to_the_project_root() {
        let temporary = tui_session_directory("picker-cap");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let project = temporary.join("project");
        std::fs::write(temporary.join("outside.txt"), "outside").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(temporary.join("outside.txt"), project.join("escape.txt"))
            .unwrap();
        for index in 0..150 {
            std::fs::write(project.join(format!("file-{index:03}.txt")), "body").unwrap();
        }

        let capped = tui_file_candidates_with_limit(&bootstrap, 64).unwrap();

        assert_eq!(capped.len(), 64);
        assert_eq!(capped.first().map(String::as_str), Some("file-000.txt"));
        assert!(
            capped
                .iter()
                .all(|path| path.starts_with("file-") && !path.contains("..")),
            "{capped:?}"
        );
        assert_eq!(tui_picker_file_candidates(&bootstrap).unwrap().len(), 150);

        std::fs::remove_dir_all(temporary).unwrap();
    }
}
