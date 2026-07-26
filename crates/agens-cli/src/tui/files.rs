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
