//! Branch and working-tree size for the footer, collected off the frame path.
//!
//! Every reading is produced by a background thread on its own clock and left
//! in a shared cell; the TUI's probe only reads that cell. A `git status` on a
//! large or cold repository takes far longer than a frame, so it must never be
//! something the renderer waits for.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use agens_tui::{RepositoryProbe, RepositoryStatus};

/// How often the working tree is re-read.
const REFRESH_INTERVAL: Duration = Duration::from_secs(3);

/// Where the next reading is collected from, asked again every cycle so the
/// branch on screen belongs to the directory the session is actually in.
pub type RepositoryDirectory = Arc<dyn Fn() -> PathBuf + Send + Sync>;

/// Starts the collector for `root` and returns the probe the TUI reads.
pub fn start_repository_probe(root: &Path) -> RepositoryProbe {
    let root = root.to_path_buf();

    start_repository_probe_following(Arc::new(move || root.clone()))
}

/// Starts a collector that re-reads `directory` every cycle, and returns the
/// probe the TUI reads.
///
/// The thread outlives nothing: it is detached and stops mattering when the
/// process ends. It holds no lock while running git.
pub fn start_repository_probe_following(directory: RepositoryDirectory) -> RepositoryProbe {
    let cell: Arc<Mutex<Option<RepositoryStatus>>> = Arc::new(Mutex::new(None));

    let writer = Arc::clone(&cell);
    thread::spawn(move || {
        loop {
            let status = collect(&directory());
            if let Ok(mut cell) = writer.lock() {
                *cell = status;
            }
            thread::sleep(REFRESH_INTERVAL);
        }
    });

    Arc::new(move || cell.lock().ok().and_then(|status| status.clone()))
}

/// One reading of the repository at `root`, or `None` when it is not one.
fn collect(root: &Path) -> Option<RepositoryStatus> {
    let branch = git(root, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    let branch = branch.trim();

    let mut status = RepositoryStatus {
        // A detached HEAD has no branch name to show, and `HEAD` is not one.
        branch: (!branch.is_empty() && branch != "HEAD").then(|| branch.to_owned()),
        ..RepositoryStatus::default()
    };

    if let Some(shortstat) = git(root, &["diff", "HEAD", "--shortstat"]) {
        let counts = parse_shortstat(&shortstat);
        status.changed_files = counts.0;
        status.insertions = counts.1;
        status.deletions = counts.2;
    }

    // Files git has never seen are changes too, and `--shortstat` cannot see
    // them: without this an untracked file reads as a clean tree.
    if let Some(untracked) = git(root, &["ls-files", "--others", "--exclude-standard"]) {
        status.changed_files += untracked.lines().filter(|line| !line.is_empty()).count() as u64;
    }

    Some(status)
}

fn git(root: &Path, arguments: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Reads `git diff --shortstat` as (files, insertions, deletions).
///
/// The line omits a section entirely when its count is zero, so each number is
/// found by the word that follows it rather than by position.
fn parse_shortstat(line: &str) -> (u64, u64, u64) {
    let mut files = 0;
    let mut insertions = 0;
    let mut deletions = 0;

    let words: Vec<&str> = line.split_whitespace().collect();
    for (index, word) in words.iter().enumerate() {
        let Some(count) = index
            .checked_sub(1)
            .and_then(|previous| words[previous].parse::<u64>().ok())
        else {
            continue;
        };
        if word.starts_with("file") {
            files = count;
        } else if word.starts_with("insertion") {
            insertions = count;
        } else if word.starts_with("deletion") {
            deletions = count;
        }
    }

    (files, insertions, deletions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shortstat_line_is_read_by_its_words_not_its_positions() {
        assert_eq!(
            parse_shortstat(" 3 files changed, 120 insertions(+), 8 deletions(-)"),
            (3, 120, 8)
        );
        assert_eq!(
            parse_shortstat(" 1 file changed, 4 insertions(+)"),
            (1, 4, 0)
        );
        assert_eq!(
            parse_shortstat(" 2 files changed, 9 deletions(-)"),
            (2, 0, 9)
        );
        assert_eq!(parse_shortstat(""), (0, 0, 0));
    }

    #[test]
    fn a_directory_outside_a_repository_reports_nothing() {
        assert_eq!(collect(Path::new("/")), None);
    }
}
