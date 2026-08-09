use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use agens_core::{EditMagnitude, Error, FactPath, ToolOutcome, ToolResultFacts};
use agens_tools::{
    BashInput, EditFileInput, GlobInput, GrepInput, ListDirectoryInput, NativeToolCatalog,
    NativeToolLimits, NativeTools, ReadFileInput, SearchInput, ToolExecutionContext, ToolOutput,
    WebfetchInput, WriteFileInput,
};
use serde_json::json;

static NEXT_ROOT: AtomicUsize = AtomicUsize::new(0);

/// Budget for tests that need an operation to run to completion but do not prove anything
/// about the budget itself. Those tests are checking a limit, a diff or an output shape,
/// so the only requirement is that the value comfortably exceeds the operation's real
/// cost; the timeout paths are proven separately and deterministically, by asking for a
/// budget that cannot be met (`Duration::from_nanos(1)`) rather than by hoping a generous
/// one expires.
///
/// This is a floor, not a ceiling: nothing is weakened by raising it. Exceeding it still
/// fails the assertion, because a timed-out operation returns a timeout message instead of
/// the expected result, so the value only decides how long a genuinely stuck operation
/// takes to be reported.
const COMPLETION_BUDGET: Duration = Duration::from_secs(60);

#[test]
fn directly_constructed_native_tools_allow_long_test_commands_by_default() {
    assert_eq!(
        NativeToolLimits::default().bash_timeout,
        Duration::from_secs(2 * 60)
    );
}

/// A temporary project root that deletes itself when the test ends, whether the test
/// returns normally or panics.
///
/// The name mixes three components because none of them is unique on its own: the kernel
/// recycles process ids, the wall-clock stamp repeats across processes that start within
/// the same nanosecond, and the counter only orders roots inside one process. Colliding
/// would require a recycled process id to reach the same counter value in the same
/// nanosecond as the run that owned the id before it.
///
/// Uniqueness alone is not enough: a run that panicked used to leave its root behind for
/// whichever later process drew the same process id, and that leftover then made the next
/// run panic too, so the failures compounded instead of dying out.
struct TemporaryRoot {
    path: std::path::PathBuf,
}

impl TemporaryRoot {
    fn new() -> Self {
        let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        let created = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();

        let path = std::env::temp_dir().join(format!(
            "agens-tools-{}-{created}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();

        Self { path }
    }
}

impl std::ops::Deref for TemporaryRoot {
    type Target = std::path::Path;

    fn deref(&self) -> &Self::Target {
        &self.path
    }
}

impl AsRef<std::path::Path> for TemporaryRoot {
    fn as_ref(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TemporaryRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);

        // A test that swaps the root for a symlink leaves behind a path that
        // `remove_dir_all` refuses to follow.
        let _ = fs::remove_file(&self.path);
    }
}

fn project_root() -> TemporaryRoot {
    TemporaryRoot::new()
}

#[test]
fn rejects_absolute_traversal_and_symlink_escape_paths() {
    let root = project_root();
    let outside = project_root();
    fs::write(outside.join("secret.txt"), "secret").unwrap();
    let tools = NativeTools::open(&root).unwrap();

    assert!(
        tools
            .read_file(ReadFileInput::new(root.join("notes.txt")))
            .unwrap()
            .is_error
    );
    assert!(
        tools
            .read_file(ReadFileInput::new("../secret.txt"))
            .unwrap()
            .is_error
    );

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outside.join("secret.txt"), root.join("escape")).unwrap();
        fs::create_dir(outside.join("nested")).unwrap();
        fs::write(outside.join("nested/result.txt"), "needle").unwrap();
        std::os::unix::fs::symlink(outside.join("nested"), root.join("escape-directory")).unwrap();
        assert_eq!(
            tools.read_file(ReadFileInput::new("escape")).unwrap(),
            ToolOutput::failure("path: outside project root")
        );
        assert_eq!(
            tools
                .write_file(WriteFileInput::new("escape", "overwrite"))
                .unwrap(),
            ToolOutput::failure("path: outside project root")
        );
        std::os::unix::fs::symlink(&outside, root.join("outside-parent")).unwrap();
        assert_eq!(
            tools
                .write_file(WriteFileInput::new("outside-parent/created.txt", "escape"))
                .unwrap(),
            ToolOutput::failure("path: outside project root")
        );
        assert!(!outside.join("created.txt").exists());
        assert_eq!(
            tools
                .list_directory(ListDirectoryInput::new("escape-directory"))
                .unwrap(),
            ToolOutput::failure("path: outside project root")
        );
        assert_eq!(
            tools
                .search(SearchInput::new("escape-directory", "needle"))
                .unwrap(),
            ToolOutput::failure("path: outside project root")
        );
    }
}

#[test]
fn tui_file_candidates_are_bounded_sorted_and_safe() {
    let root = project_root();
    fs::create_dir(root.join("nested")).unwrap();
    fs::write(root.join("zeta.txt"), "zeta").unwrap();
    fs::write(root.join("alpha.txt"), "alpha").unwrap();
    fs::write(root.join("bravo.txt"), "bravo").unwrap();
    fs::write(root.join("nested/valid.txt"), "valid").unwrap();
    fs::write(root.join("large.txt"), vec![b'x'; 1024 * 1024 + 1]).unwrap();
    fs::hard_link(root.join("alpha.txt"), root.join("linked.txt")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(root.join("zeta.txt"), root.join("symlink.txt")).unwrap();

    let tools = NativeTools::open(&root).unwrap();

    assert_eq!(
        tools.tui_file_candidates(2).unwrap(),
        vec![String::from("bravo.txt"), String::from("nested/valid.txt")]
    );
    assert_eq!(
        tools.tui_file_candidates(8).unwrap(),
        vec![
            String::from("bravo.txt"),
            String::from("nested/valid.txt"),
            String::from("zeta.txt")
        ]
    );
    assert_eq!(
        tools.read_file(ReadFileInput::new("linked.txt")).unwrap(),
        ToolOutput::failure("read: path has multiple hard links")
    );
    assert_eq!(
        tools.read_file(ReadFileInput::new("large.txt")).unwrap(),
        ToolOutput::failure("read: file exceeds 1048576 byte limit")
    );
    assert!(
        tools
            .read_file(ReadFileInput::new("nested"))
            .unwrap()
            .is_error
    );
}

#[cfg(unix)]
#[test]
fn tui_file_reads_reject_root_swaps_and_invalid_utf8() {
    let root = project_root();
    let outside = project_root();
    fs::write(root.join("invalid.txt"), [0xff]).unwrap();
    fs::write(root.join("safe.txt"), "safe").unwrap();
    let tools = NativeTools::open(&root).unwrap();

    assert!(
        tools
            .read_file(ReadFileInput::new("invalid.txt"))
            .unwrap()
            .is_error
    );
    let moved = project_root();
    fs::rename(&root, moved.join("root")).unwrap();
    std::os::unix::fs::symlink(&outside, &root).unwrap();
    assert_eq!(
        tools.read_file(ReadFileInput::new("safe.txt")).unwrap(),
        ToolOutput::failure("path: outside project root")
    );
}

#[test]
fn writes_lists_and_searches_only_within_the_project() {
    let root = project_root();
    fs::create_dir(root.join("logs")).unwrap();
    let tools = NativeTools::open(&root).unwrap();

    assert_eq!(
        tools
            .write_file(WriteFileInput::new("logs/run.txt", "ready\nneedle\n"))
            .unwrap(),
        ToolOutput::success("wrote logs/run.txt")
    );
    assert_eq!(
        tools
            .list_directory(ListDirectoryInput::new("logs"))
            .unwrap(),
        ToolOutput::success("run.txt\n")
    );
    assert_eq!(
        tools.search(SearchInput::new("logs", "needle")).unwrap(),
        ToolOutput::success("logs/run.txt:2:needle\n")
    );
}

#[test]
fn rejects_invalid_typed_inputs_before_running_tools() {
    let root = project_root();
    let tools = NativeTools::open(&root).unwrap();
    assert_eq!(
        tools
            .write_file(WriteFileInput::new("missing/file.txt", "content"))
            .unwrap(),
        ToolOutput::success("wrote missing/file.txt")
    );
    assert_eq!(
        tools
            .write_file(WriteFileInput::new(".", "content"))
            .unwrap(),
        ToolOutput::failure("write: path must name a file")
    );
    assert_eq!(
        tools.search(SearchInput::new(".", "")).unwrap(),
        ToolOutput::failure("search: query is required")
    );
    assert_eq!(
        tools.bash(BashInput::new("   ")).unwrap(),
        ToolOutput::failure("bash: command is required")
    );
}

#[test]
fn bash_uses_the_project_root_and_reports_tool_failures() {
    let root = project_root();
    fs::write(root.join("project.txt"), "project\n").unwrap();
    let tools = NativeTools::open(&root).unwrap();

    assert_eq!(
        tools
            .bash(BashInput::new("pwd; cat project.txt").with_timeout(Duration::from_secs(1)))
            .unwrap(),
        ToolOutput::success(format!(
            "[stdout]\n{}\nproject\n[stderr]\n[exit status: 0]\n",
            root.display()
        ))
    );
    assert_eq!(
        tools
            .bash(BashInput::new("printf stdout; printf stderr >&2; exit 7"))
            .unwrap(),
        ToolOutput::failure("[stdout]\nstdout\n[stderr]\nstderr\n[exit status: 7]\n")
    );
}

#[test]
fn bash_labels_stderr_for_success_and_tool_failures() {
    let root = project_root();
    let tools = NativeTools::open(&root).unwrap();

    let success = tools
        .bash(BashInput::new("printf success-stderr >&2"))
        .unwrap();

    assert_eq!(
        success,
        ToolOutput::success("[stdout]\n[stderr]\nsuccess-stderr\n[exit status: 0]\n")
    );
}

#[test]
fn bash_enforces_one_total_labeled_output_budget_and_reports_timeout() {
    let root = project_root();
    let tools = NativeTools::open(&root).unwrap();

    let timeout = tools
        .bash(BashInput::new("sleep 1").with_timeout(Duration::from_millis(25)))
        .unwrap();
    assert_eq!(
        timeout,
        ToolOutput::failure(
            "[stdout]\n[stderr]\n[bash: timed out after 25ms. If this command is expected to take longer, retry with a larger timeout value in milliseconds (max: 600000ms).]\n[exit status: unavailable]\n"
        )
    );

    let output = tools
        .bash(BashInput::new(
            "printf 'x%.0s' {1..40000}; printf 'y%.0s' {1..40000} >&2",
        ))
        .unwrap();
    assert!(!output.is_error);
    assert!(output.content.starts_with("[stdout]\n"));
    assert!(output.content.contains("\n[stderr]\n"));
    assert!(output.content.contains("[bash output truncated]\n"));
    assert!(output.content.ends_with("[exit status: 0]\n"));
    assert!(output.content.len() <= 64 * 1024);
}

#[test]
fn bash_combines_streams_with_a_deterministic_total_budget() {
    let root = project_root();
    let tools = NativeTools::open(&root).unwrap();
    let command = "printf 'x%.0s' {1..40000}; printf 'y%.0s' {1..40000} >&2";
    let expected = tools.bash(BashInput::new(command)).unwrap();

    assert!(!expected.is_error);
    assert!(expected.content.contains("\n[stderr]\n"));
    assert!(expected.content.contains("yyyy"));
    assert!(expected.content.contains("[bash output truncated]\n"));
    assert!(expected.content.ends_with("[exit status: 0]\n"));
    assert!(expected.content.len() <= 64 * 1024);
    assert_eq!(expected.content.matches('x').count(), 32_705);
    assert_eq!(expected.content.matches('y').count(), 32_704);

    for _ in 0..4 {
        assert_eq!(tools.bash(BashInput::new(command)).unwrap(), expected);
    }
}

#[test]
fn bash_reserves_metadata_and_both_streams_after_lossy_utf8_expansion() {
    let root = project_root();
    let tools = NativeTools::open(&root).unwrap();
    let command = "printf '\\377%.0s' {1..40000}; printf '\\376%.0s' {1..40000} >&2";
    let output = tools.bash(BashInput::new(command)).unwrap();

    assert!(!output.is_error);
    assert!(output.content.starts_with("[stdout]\n\u{fffd}"));
    assert!(output.content.contains("\n[stderr]\n\u{fffd}"));
    assert!(output.content.contains("[bash output truncated]\n"));
    assert!(output.content.ends_with("[exit status: 0]\n"));
    assert!(output.content.len() <= 64 * 1024);
}

#[test]
fn catalog_preserves_the_bounded_bash_result_without_generic_truncation() {
    let root = project_root();
    let catalog = NativeToolCatalog::new(NativeTools::open(&root).unwrap());
    let output = catalog
        .execute(
            "native::bash",
            json!({"command": "printf 'x%.0s' {1..40000}; printf 'y%.0s' {1..40000} >&2"}),
            &ToolExecutionContext::with_timeout(COMPLETION_BUDGET),
        )
        .unwrap();

    assert!(output.content.len() > 16 * 1024);
    assert!(output.content.len() <= 64 * 1024);
    assert!(output.content.contains("\n[stderr]\n"));
    assert!(output.content.contains("yyyy"));
    assert!(output.content.contains("[bash output truncated]\n"));
    assert!(output.content.ends_with("[exit status: 0]\n"));
}

#[test]
fn bash_inherits_environment_and_reports_sanitized_start_failures() {
    let root = project_root();
    let tools = NativeTools::open(&root).unwrap();

    assert_eq!(
        tools
            .bash(BashInput::new("test -n \"$PATH\" && printf inherited"))
            .unwrap(),
        ToolOutput::success("[stdout]\ninherited\n[stderr]\n[exit status: 0]\n")
    );

    fs::remove_dir_all(&root).unwrap();
    assert_eq!(
        tools.bash(BashInput::new("printf should-not-run")).unwrap(),
        ToolOutput::failure("bash: failed to start")
    );
}

#[cfg(unix)]
#[test]
fn bash_timeout_kills_its_process_group_and_descendants() {
    let root = project_root();
    let marker = root.join("timeout-descendant-ran");
    let tools = NativeTools::open(&root).unwrap();
    let command = format!("(sleep 1; touch {}) & wait", marker.display());

    assert_eq!(
        tools
            .bash(BashInput::new(command).with_timeout(Duration::from_millis(25)))
            .unwrap(),
        ToolOutput::failure(
            "[stdout]\n[stderr]\n[bash: timed out after 25ms. If this command is expected to take longer, retry with a larger timeout value in milliseconds (max: 600000ms).]\n[exit status: unavailable]\n"
        )
    );
    thread::sleep(Duration::from_millis(1100));
    assert!(!marker.exists());
}

#[cfg(unix)]
#[test]
fn bash_does_not_wait_for_background_descendant_output() {
    let root = project_root();
    let tools = NativeTools::open(&root).unwrap();
    let started = Instant::now();

    let output = tools
        .bash(BashInput::new("sleep 1 &").with_timeout(Duration::from_secs(2)))
        .unwrap();

    assert_eq!(
        output,
        ToolOutput::success("[stdout]\n[stderr]\n[exit status: 0]\n")
    );
    assert!(started.elapsed() < Duration::from_millis(500));
}

#[cfg(unix)]
#[test]
fn bash_cancellation_kills_its_process_group_and_descendants() {
    let root = project_root();
    let marker = root.join("descendant-ran");
    let tools = NativeTools::open(&root).unwrap();
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancellation = Arc::clone(&cancelled);
    let command = format!("(sleep 1; touch {}) & wait", marker.display());
    let started = Instant::now();
    let worker = thread::spawn(move || {
        tools.bash(
            BashInput::new(command)
                .with_timeout(Duration::from_secs(5))
                .with_cancellation(cancellation),
        )
    });

    thread::sleep(Duration::from_millis(50));
    cancelled.store(true, Ordering::Release);
    let output = worker.join().unwrap().unwrap();

    assert_eq!(
        output,
        ToolOutput::failure("[stdout]\n[stderr]\n[bash: cancelled]\n[exit status: unavailable]\n")
    );
    assert_eq!(output.facts(), None);
    assert!(started.elapsed() < Duration::from_secs(2));
    thread::sleep(Duration::from_millis(1100));
    assert!(!marker.exists());
}

#[cfg(unix)]
#[test]
fn bash_killed_by_a_signal_reports_exit_code_none() {
    let root = project_root();
    let tools = NativeTools::open(&root).unwrap();

    let output = tools
        .bash(BashInput::new("kill -9 $$").with_timeout(Duration::from_secs(5)))
        .unwrap();

    assert!(output.is_error);
    assert_eq!(
        output.facts(),
        Some(&ToolResultFacts::Bash {
            outcome: ToolOutcome::Failed,
            exit_code: None
        })
    );
}

#[test]
fn catalog_returns_turn_cancellation_as_a_runtime_error() {
    let root = project_root();
    let catalog = Arc::new(NativeToolCatalog::new(NativeTools::open(&root).unwrap()));
    let cancelled = Arc::new(AtomicBool::new(false));
    let request_catalog = Arc::clone(&catalog);
    let request_cancellation = Arc::clone(&cancelled);
    let request = thread::spawn(move || {
        request_catalog.execute(
            "native::bash",
            json!({"command": "sleep 1"}),
            &ToolExecutionContext::new(request_cancellation, Duration::from_secs(2)),
        )
    });

    thread::sleep(Duration::from_millis(50));
    cancelled.store(true, Ordering::Release);

    assert!(matches!(request.join().unwrap(), Err(Error::Cancelled)));
}

#[test]
fn catalog_reports_malformed_and_empty_bash_input_as_tool_errors() {
    let root = project_root();
    let catalog = NativeToolCatalog::new(NativeTools::open(&root).unwrap());
    let context = ToolExecutionContext::with_timeout(Duration::from_secs(1));

    assert_eq!(
        catalog
            .execute("native::bash", json!({"command": 1}), &context)
            .unwrap(),
        ToolOutput::failure("bash: command must be a string")
    );
    assert_eq!(
        catalog
            .execute("native::bash", json!({"command": ""}), &context)
            .unwrap(),
        ToolOutput::failure("bash: command is required")
    );
}

#[test]
fn catalog_validates_the_optional_bash_timeout_override() {
    let root = project_root();
    let catalog = NativeToolCatalog::new(NativeTools::open(&root).unwrap());
    let metadata = NativeToolCatalog::metadata();
    let bash = metadata
        .iter()
        .find(|tool| tool.qualified_name == "native::bash")
        .expect("bash metadata");
    let context = ToolExecutionContext::with_timeout(Duration::from_secs(1));

    assert_eq!(bash.input_schema["properties"]["timeout_ms"]["minimum"], 1);
    assert_eq!(
        catalog
            .execute(
                "native::bash",
                json!({"command": "exit 1", "timeout_ms": 0}),
                &context,
            )
            .unwrap(),
        ToolOutput::failure("bash: timeout must be greater than zero")
    );
}

#[test]
fn catalog_applies_a_positive_bash_timeout_override() {
    let root = project_root();
    let catalog = NativeToolCatalog::new(NativeTools::open(&root).unwrap());

    assert_eq!(
        catalog
            .execute(
                "native::bash",
                json!({"command": "sleep 1", "timeout_ms": 25}),
                &ToolExecutionContext::with_timeout(Duration::from_secs(1)),
            )
            .unwrap(),
        ToolOutput::failure(
            "[stdout]\n[stderr]\n[bash: timed out after 25ms. If this command is expected to take longer, retry with a larger timeout value in milliseconds (max: 600000ms).]\n[exit status: unavailable]\n"
        )
    );
}

#[test]
fn catalog_falls_back_to_the_configured_bash_timeout() {
    let root = project_root();
    let limits = NativeToolLimits {
        bash_timeout: Duration::from_millis(25),
        ..NativeToolLimits::default()
    };
    let catalog = NativeToolCatalog::new(NativeTools::open_with_limits(&root, limits).unwrap());

    assert_eq!(
        catalog
            .execute(
                "native::bash",
                json!({"command": "sleep 1"}),
                &ToolExecutionContext::with_timeout(Duration::from_secs(2)),
            )
            .unwrap(),
        ToolOutput::failure(
            "[stdout]\n[stderr]\n[bash: timed out after 25ms. If this command is expected to take longer, retry with a larger timeout value in milliseconds (max: 600000ms).]\n[exit status: unavailable]\n"
        )
    );
}

#[test]
fn rejects_a_zero_bash_timeout() {
    let root = project_root();
    let limits = NativeToolLimits {
        bash_timeout: Duration::ZERO,
        ..NativeToolLimits::default()
    };

    assert!(NativeTools::open_with_limits(&root, limits).is_err());
}

#[test]
fn reads_a_project_relative_file() {
    let root = project_root();
    fs::write(root.join("notes.txt"), "project note").unwrap();
    let tools = NativeTools::open(&root).unwrap();

    let output = tools.read_file(ReadFileInput::new("notes.txt")).unwrap();

    assert_eq!(output, ToolOutput::success("project note"));
}

#[test]
fn a_successful_read_reports_its_path() {
    let root = project_root();
    fs::write(root.join("notes.txt"), "project note").unwrap();
    let tools = NativeTools::open(&root).unwrap();

    let output = tools.read_file(ReadFileInput::new("notes.txt")).unwrap();

    assert!(!output.is_error);
    assert_eq!(
        output.facts(),
        Some(&ToolResultFacts::Read {
            path: FactPath::new("notes.txt"),
            outcome: ToolOutcome::Succeeded,
        })
    );
}

#[test]
fn absolute_paths_under_the_project_root_are_accepted_and_rewritten() {
    let root = project_root();
    let tools = NativeTools::open(&root).unwrap();
    let absolute = root.join("nested/absolute.txt");

    let written = tools
        .write_file(WriteFileInput::new(&absolute, "absolute body\n"))
        .unwrap();
    assert!(
        !written.is_error,
        "absolute path under the root must write: {written:?}"
    );
    assert_eq!(
        fs::read_to_string(root.join("nested/absolute.txt")).unwrap(),
        "absolute body\n"
    );

    let read = tools.read_file(ReadFileInput::new(&absolute)).unwrap();
    assert!(!read.is_error, "absolute read under the root: {read:?}");
    assert!(read.content.contains("absolute body"));

    // Outside the root stays blocked without a post-permission execution context
    // (unit-test / unauthenticated call sites). Authorized execution is tested
    // separately — peers treat out-of-workspace as Allow-after-ask, not hard fail.
    let outside_dir = project_root();
    let outside = outside_dir.join("secret.txt");
    fs::write(&outside, "nope").unwrap();
    assert_eq!(
        tools
            .write_file(WriteFileInput::new(&outside, "escape"))
            .unwrap(),
        ToolOutput::failure("path: outside project root")
    );
    assert_eq!(fs::read_to_string(&outside).unwrap(), "nope");
}

/// After the permission gate Allow's a call (execute path always has a
/// ToolExecutionContext), writes outside the project root must succeed —
/// same shape as OpenCode external_directory + Allow, Claude/Codex
/// workspace-write with approval, and bypass meaning "everything not denied".
#[test]
fn authorized_writes_may_land_outside_the_project_root() {
    let root = project_root();
    let tools = NativeTools::open(&root).unwrap();
    let outside_dir = project_root();
    let outside = outside_dir.join("commit_msg.txt");
    let context = ToolExecutionContext::with_timeout(Duration::from_secs(5));

    let written = tools
        .write_file_with_context(
            WriteFileInput::new(&outside, "peer-style commit message\n"),
            Some(&context),
        )
        .unwrap();
    assert!(
        !written.is_error,
        "authorized external write must succeed: {written:?}"
    );
    assert_eq!(
        fs::read_to_string(&outside).unwrap(),
        "peer-style commit message\n"
    );

    // Unauthenticated call sites still refuse the same path.
    assert!(
        tools
            .write_file(WriteFileInput::new(&outside, "blocked"))
            .unwrap()
            .is_error,
        "without execution context, outside write stays confined"
    );
}

#[test]
fn confined_read_write_creates_parents_and_reads_one_based_ranges() {
    let root = project_root();
    let tools = NativeTools::open(&root).unwrap();

    assert_eq!(
        tools
            .write_file(WriteFileInput::new("nested/notes.txt", "one\ntwo\nthree\n"))
            .unwrap(),
        ToolOutput::success("wrote nested/notes.txt")
    );
    assert_eq!(
        tools
            .read_file(ReadFileInput::new("nested/notes.txt").with_range(2, 1))
            .unwrap(),
        ToolOutput::success("two\n")
    );
    assert_eq!(
        tools
            .read_file(ReadFileInput::new("nested/notes.txt").with_range(3, 8))
            .unwrap(),
        ToolOutput::success("three\n")
    );
}

#[test]
fn exact_edit_replaces_one_match_and_returns_a_unified_diff() {
    let root = project_root();
    fs::write(root.join("notes.txt"), "one\ntwo\nthree\n").unwrap();
    let tools = NativeTools::open(&root).unwrap();

    assert_eq!(
        tools
            .edit_file(EditFileInput::new("notes.txt", "two", "TWO"))
            .unwrap(),
        ToolOutput::success(
            "--- notes.txt\n+++ notes.txt\n@@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n"
        )
    );
    assert_eq!(
        fs::read_to_string(root.join("notes.txt")).unwrap(),
        "one\nTWO\nthree\n"
    );
}

#[test]
fn exact_edit_rejects_invalid_matches_without_changing_the_target() {
    let root = project_root();
    fs::write(root.join("notes.txt"), "repeat repeat").unwrap();
    let tools = NativeTools::open(&root).unwrap();

    for (old, new) in [("missing", "value"), ("repeat", "value"), ("same", "same")] {
        if old == "same" {
            fs::write(root.join("notes.txt"), "same").unwrap();
        }
        assert!(
            tools
                .edit_file(EditFileInput::new("notes.txt", old, new))
                .unwrap()
                .is_error
        );
    }
    assert_eq!(fs::read_to_string(root.join("notes.txt")).unwrap(), "same");

    fs::write(root.join("notes.txt"), "aaa").unwrap();
    assert!(
        tools
            .edit_file(EditFileInput::new("notes.txt", "aa", "b"))
            .unwrap()
            .is_error
    );
    assert_eq!(fs::read_to_string(root.join("notes.txt")).unwrap(), "aaa");
}

#[cfg(unix)]
#[test]
fn exact_edit_fails_closed_for_nonregular_and_linked_targets() {
    use std::os::unix::fs::symlink;

    let root = project_root();
    let outside = project_root();
    fs::write(outside.join("outside.txt"), "old").unwrap();
    symlink(outside.join("outside.txt"), root.join("linked.txt")).unwrap();
    fs::create_dir(root.join("directory.txt")).unwrap();
    fs::write(root.join("original.txt"), "old").unwrap();
    fs::hard_link(root.join("original.txt"), root.join("hard-linked.txt")).unwrap();
    let tools = NativeTools::open(&root).unwrap();

    for path in ["linked.txt", "directory.txt", "original.txt"] {
        assert!(
            tools
                .edit_file(EditFileInput::new(path, "old", "new"))
                .unwrap()
                .is_error
        );
    }
    assert_eq!(
        fs::read_to_string(outside.join("outside.txt")).unwrap(),
        "old"
    );
    assert_eq!(
        fs::read_to_string(root.join("original.txt")).unwrap(),
        "old"
    );
}

#[test]
fn catalog_dispatches_the_separate_edit_schema() {
    let root = project_root();
    fs::write(root.join("notes.txt"), "before").unwrap();
    let catalog = NativeToolCatalog::new(NativeTools::open(&root).unwrap());
    let metadata = NativeToolCatalog::metadata();
    let edit = metadata
        .iter()
        .find(|tool| tool.qualified_name == "native::edit")
        .expect("edit metadata");
    assert_eq!(edit.input_schema["required"], json!(["path", "old", "new"]));

    assert_eq!(
        catalog
            .execute(
                "native::edit",
                json!({"path": "notes.txt", "old": "before", "new": "after"}),
                &ToolExecutionContext::with_timeout(Duration::from_secs(1)),
            )
            .unwrap(),
        ToolOutput::success("--- notes.txt\n+++ notes.txt\n@@ -1,1 +1,1 @@\n-before\n+after\n")
    );
    assert_eq!(fs::read_to_string(root.join("notes.txt")).unwrap(), "after");

    let cancelled = Arc::new(AtomicBool::new(true));
    assert_eq!(
        catalog
            .execute(
                "native::edit",
                json!({"path": "notes.txt", "old": "after", "new": "cancelled"}),
                &ToolExecutionContext::new(cancelled, Duration::from_secs(1)),
            )
            .unwrap(),
        ToolOutput::failure("tool execution cancelled")
    );
    assert_eq!(fs::read_to_string(root.join("notes.txt")).unwrap(), "after");
}

#[test]
fn native_catalog_preserves_edit_facts() {
    let root = project_root();
    fs::write(root.join("notes.txt"), "before").unwrap();
    let catalog = NativeToolCatalog::new(NativeTools::open(&root).unwrap());

    let output = catalog
        .execute(
            "native::edit",
            json!({"path": "notes.txt", "old": "before", "new": "after"}),
            &ToolExecutionContext::with_timeout(COMPLETION_BUDGET),
        )
        .unwrap();

    assert_eq!(
        output,
        ToolOutput::success("--- notes.txt\n+++ notes.txt\n@@ -1,1 +1,1 @@\n-before\n+after\n")
    );
    assert_eq!(
        output.facts(),
        Some(&ToolResultFacts::Edit {
            path: FactPath::new("notes.txt"),
            outcome: ToolOutcome::Succeeded,
            changed: Some(EditMagnitude {
                lines_added: 1,
                lines_removed: 1,
            }),
        })
    );
}

#[cfg(unix)]
#[test]
fn confined_read_write_fails_closed_for_symlinks_and_hardlinks() {
    use std::os::unix::fs::symlink;

    let root = project_root();
    let outside = project_root();
    let outside_target = outside.join("target.txt");
    fs::write(&outside_target, "outside").unwrap();
    symlink(&outside_target, root.join("symlink.txt")).unwrap();
    fs::write(root.join("original.txt"), "original").unwrap();
    fs::hard_link(root.join("original.txt"), root.join("linked.txt")).unwrap();
    let tools = NativeTools::open(&root).unwrap();
    assert!(
        tools
            .write_file(WriteFileInput::new("symlink.txt", "changed"))
            .unwrap()
            .is_error
    );
    assert!(
        tools
            .write_file(WriteFileInput::new("original.txt", "changed"))
            .unwrap()
            .is_error
    );
    assert!(
        tools
            .read_file(ReadFileInput::new("original.txt"))
            .unwrap()
            .is_error
    );
    assert_eq!(fs::read_to_string(&outside_target).unwrap(), "outside");
    assert_eq!(
        fs::read_to_string(root.join("original.txt")).unwrap(),
        "original"
    );
}

#[test]
fn list_and_search_fail_when_configured_work_budgets_are_exhausted() {
    let root = project_root();
    let limits = NativeToolLimits {
        max_list_entries: 2,
        max_search_entries: 3,
        max_search_results: 2,
        max_search_depth: 1,
        operation_timeout: Duration::from_secs(1),
        bash_timeout: Duration::from_secs(1),
    };
    let tools = NativeTools::open_with_limits(&root, limits).unwrap();

    for index in 0..3 {
        fs::write(root.join(format!("entry-{index}")), "content").unwrap();
    }
    assert_eq!(
        tools.list_directory(ListDirectoryInput::new(".")).unwrap(),
        ToolOutput::failure("list: entry limit of 2 exceeded")
    );

    fs::create_dir(root.join("nested")).unwrap();
    fs::create_dir(root.join("nested/deeper")).unwrap();
    fs::create_dir(root.join("nested/deeper/too-deep")).unwrap();
    assert_eq!(
        tools.search(SearchInput::new("nested", "absent")).unwrap(),
        ToolOutput::failure("search: traversal depth limit of 1 exceeded")
    );

    fs::create_dir(root.join("flat")).unwrap();
    for index in 0..4 {
        fs::write(root.join("flat").join(format!("file-{index}")), "absent").unwrap();
    }
    assert_eq!(
        tools.search(SearchInput::new("flat", "needle")).unwrap(),
        ToolOutput::failure("search: entry limit of 3 exceeded")
    );
}

#[test]
fn search_and_grep_report_an_untruncated_match_count() {
    let root = project_root();
    fs::write(root.join("notes.txt"), "needle\nother\nneedle\n").unwrap();
    let tools = NativeTools::open(&root).unwrap();

    let search_output = tools.search(SearchInput::new(".", "needle")).unwrap();
    assert!(!search_output.is_error);
    assert_eq!(
        search_output.facts(),
        Some(&ToolResultFacts::Search {
            outcome: ToolOutcome::Succeeded,
            match_count: 2,
            truncated: false,
        })
    );

    let grep_output = tools.grep(GrepInput::new("needle")).unwrap();
    assert!(!grep_output.is_error);
    assert_eq!(
        grep_output.facts(),
        Some(&ToolResultFacts::Search {
            outcome: ToolOutcome::Succeeded,
            match_count: 2,
            truncated: false,
        })
    );
}

#[test]
fn a_truncated_grep_reports_the_match_count_before_the_truncation_marker() {
    let root = project_root();
    fs::write(root.join("notes.txt"), "needle\nneedle\nneedle\n").unwrap();
    let tools = NativeTools::open_with_limits(
        &root,
        NativeToolLimits {
            max_list_entries: 10,
            max_search_entries: 10,
            max_search_results: 2,
            max_search_depth: 32,
            operation_timeout: Duration::from_secs(1),
            bash_timeout: Duration::from_secs(1),
        },
    )
    .unwrap();

    let output = tools.grep(GrepInput::new("needle")).unwrap();

    assert!(!output.is_error);
    assert!(
        output
            .content
            .contains("[grep output truncated after 2 results]")
    );
    assert_eq!(
        output.facts(),
        Some(&ToolResultFacts::Search {
            outcome: ToolOutcome::Succeeded,
            match_count: 2,
            truncated: true,
        })
    );
}

#[cfg(unix)]
#[test]
fn final_symlink_replacement_never_redirects_a_write_outside_the_project() {
    use std::os::unix::fs::symlink;

    let root = project_root();
    let outside = project_root();
    let victim = root.join("victim");
    let outside_target = outside.join("outside-target");
    fs::write(&outside_target, "original").unwrap();

    let tools = NativeTools::open(&root).unwrap();
    let keep_flipping = Arc::new(AtomicBool::new(true));
    let flipper_running = Arc::clone(&keep_flipping);
    let flipper_victim = victim.clone();
    let flipper_target = outside_target.clone();
    let flipper = thread::spawn(move || {
        while flipper_running.load(Ordering::Acquire) {
            let _ = fs::remove_file(&flipper_victim);
            let _ = symlink(&flipper_target, &flipper_victim);
            thread::yield_now();
        }
        let _ = fs::remove_file(flipper_victim);
    });

    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        let _ = tools.write_file(WriteFileInput::new("victim", "escaped"));
        assert_eq!(fs::read_to_string(&outside_target).unwrap(), "original");
    }

    keep_flipping.store(false, Ordering::Release);
    flipper.join().unwrap();
}

#[test]
fn catalog_exposes_strict_schemas_and_cancellation_suppresses_bash_output() {
    let root = project_root();
    let catalog = NativeToolCatalog::new(NativeTools::open(&root).unwrap());
    let metadata = NativeToolCatalog::metadata();
    assert_eq!(metadata.len(), 10);
    assert!(metadata.iter().all(|tool| {
        tool.qualified_name.starts_with("native::")
            && tool.input_schema["type"] == "object"
            && tool.input_schema["additionalProperties"] == false
    }));
    let read = metadata
        .iter()
        .find(|tool| tool.qualified_name == "native::read")
        .unwrap();
    assert_eq!(read.input_schema["properties"]["offset"]["minimum"], 1);
    assert_eq!(read.input_schema["properties"]["limit"]["minimum"], 1);
    let webfetch = metadata
        .iter()
        .find(|tool| tool.qualified_name == "native::webfetch")
        .expect("webfetch metadata");
    assert_eq!(webfetch.access, agens_core::ToolAccess::ReadOnly);
    assert_eq!(webfetch.input_schema["required"], json!(["url"]));
    assert_eq!(
        webfetch.input_schema["properties"]["timeout_ms"]["minimum"],
        1
    );
    let cancellation = Arc::new(AtomicBool::new(true));
    let output = catalog
        .execute(
            "native::bash",
            json!({"command": "printf SECRET_SENTINEL"}),
            &ToolExecutionContext::new(cancellation, Duration::from_secs(1)),
        )
        .unwrap();
    assert_eq!(output, ToolOutput::failure("tool execution cancelled"));
    assert!(!output.content.contains("SECRET_SENTINEL"));
}

#[test]
fn grep_uses_regex_filters_and_skips_binary_and_git_files() {
    let root = project_root();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::write(root.join("src/main.rs"), "Needle\nneedle\n").unwrap();
    fs::write(root.join("notes.txt"), "needle\n").unwrap();
    fs::write(root.join(".git/config"), "needle\n").unwrap();
    fs::write(root.join("binary.dat"), b"needle\0ignored").unwrap();
    let tools = NativeTools::open(&root).unwrap();

    assert_eq!(
        tools
            .grep(
                GrepInput::new("^needle$")
                    .with_path(".")
                    .with_file_glob("**/*.rs")
                    .with_case_insensitive(true),
            )
            .unwrap(),
        ToolOutput::success("src/main.rs:1:Needle\nsrc/main.rs:2:needle\n")
    );
    assert_eq!(
        tools
            .grep(
                GrepInput::new("^needle$")
                    .with_path("src/main.rs")
                    .with_case_insensitive(true),
            )
            .unwrap(),
        ToolOutput::success("src/main.rs:1:Needle\nsrc/main.rs:2:needle\n")
    );
    assert_eq!(
        tools.grep(GrepInput::new("[")).unwrap(),
        ToolOutput::failure("grep: invalid regex")
    );
}

#[test]
fn glob_lists_relative_doublestar_matches_and_reports_truncation() {
    let root = project_root();
    fs::create_dir_all(root.join("src/nested")).unwrap();
    fs::write(root.join("src/main.rs"), "main").unwrap();
    fs::write(root.join("src/nested/lib.rs"), "lib").unwrap();
    let tools = NativeTools::open_with_limits(
        &root,
        NativeToolLimits {
            max_list_entries: 1,
            max_search_entries: 10,
            max_search_results: 10,
            max_search_depth: 32,
            operation_timeout: Duration::from_secs(1),
            bash_timeout: Duration::from_secs(1),
        },
    )
    .unwrap();

    assert_eq!(
        tools.glob(GlobInput::new("**/*.rs")).unwrap(),
        ToolOutput::success("src/main.rs\n[glob output truncated after 1 entries]\n")
    );
    assert_eq!(
        tools.glob(GlobInput::new("**/*.toml")).unwrap(),
        ToolOutput::success("")
    );
}

#[test]
fn grep_and_glob_reject_escape_patterns_and_skip_external_symlinks() {
    let root = project_root();
    let outside = project_root();
    fs::write(outside.join("secret.txt"), "EXTERNAL_SENTINEL\n").unwrap();
    let tools = NativeTools::open(&root).unwrap();

    assert_eq!(
        tools
            .grep(GrepInput::new("EXTERNAL_SENTINEL").with_path("../"))
            .unwrap(),
        ToolOutput::failure("path: traversal is not allowed")
    );
    assert_eq!(
        tools
            .grep(GrepInput::new("EXTERNAL_SENTINEL").with_path(outside.to_path_buf()))
            .unwrap(),
        ToolOutput::failure("path: outside project root")
    );

    for pattern in ["../**", "/**", r"C:\\**", r"\\\\server\\share\\**"] {
        assert_eq!(
            tools
                .grep(GrepInput::new("EXTERNAL_SENTINEL").with_file_glob(pattern))
                .unwrap(),
            ToolOutput::failure("grep: glob pattern must be relative")
        );
        assert_eq!(
            tools.glob(GlobInput::new(pattern)).unwrap(),
            ToolOutput::failure("glob: glob pattern must be relative")
        );
    }

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outside.join("secret.txt"), root.join("escape.txt")).unwrap();
        fs::create_dir(outside.join("nested")).unwrap();
        std::os::unix::fs::symlink(outside.join("nested"), root.join("escape-directory")).unwrap();
        assert_eq!(
            tools
                .grep(GrepInput::new("EXTERNAL_SENTINEL").with_path("escape-directory"))
                .unwrap(),
            ToolOutput::failure("path: outside project root")
        );
        assert_eq!(
            tools.grep(GrepInput::new("EXTERNAL_SENTINEL")).unwrap(),
            ToolOutput::success("")
        );
        assert_eq!(
            tools.glob(GlobInput::new("**/*.txt")).unwrap(),
            ToolOutput::success("")
        );
    }
}

#[test]
fn grep_and_glob_enforce_exact_default_scan_result_depth_and_timeout_bounds() {
    let root = project_root();

    // Every default this scans for — the entry cap, the result cap, the depth cap — is a
    // count, and a scan that runs out of time reports the timeout instead of the count it
    // was about to hit. The operation timeout is therefore lifted here and pinned by its
    // own assertions below, on the default value and on a budget that cannot be met.
    let tools = NativeTools::open_with_limits(
        &root,
        NativeToolLimits {
            operation_timeout: COMPLETION_BUDGET,
            ..NativeToolLimits::default()
        },
    )
    .unwrap();

    for index in 0..=10_000 {
        fs::write(root.join(format!("entry-{index:05}.txt")), "needle\n").unwrap();
    }

    assert_eq!(
        tools.grep(GrepInput::new("needle")).unwrap(),
        ToolOutput::failure("grep: entry limit of 10000 exceeded")
    );
    assert_eq!(
        tools.glob(GlobInput::new("**/*.txt")).unwrap(),
        ToolOutput::failure("glob: entry limit of 10000 exceeded")
    );

    fs::remove_dir_all(&root).unwrap();
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("results.txt"), "needle\n".repeat(101)).unwrap();
    assert_eq!(
        tools.grep(GrepInput::new("needle")).unwrap(),
        ToolOutput::success(format!(
            "{}[grep output truncated after 100 results]\n",
            (1..=100)
                .map(|line| format!("results.txt:{line}:needle\n"))
                .collect::<String>()
        ))
    );

    let mut directory = root.to_path_buf();
    for _ in 0..=32 {
        directory.push("nested");
        fs::create_dir(&directory).unwrap();
    }
    fs::write(directory.join("leaf.txt"), "needle\n").unwrap();
    assert_eq!(
        tools.grep(GrepInput::new("needle")).unwrap(),
        ToolOutput::failure("grep: traversal depth limit of 32 exceeded")
    );
    assert_eq!(
        tools.glob(GlobInput::new("**/*.txt")).unwrap(),
        ToolOutput::failure("glob: traversal depth limit of 32 exceeded")
    );

    let timed_out = NativeTools::open_with_limits(
        &root,
        NativeToolLimits {
            operation_timeout: Duration::from_nanos(1),
            ..NativeToolLimits::default()
        },
    )
    .unwrap();
    assert_eq!(
        timed_out.grep(GrepInput::new("needle")).unwrap(),
        ToolOutput::failure("grep: operation timed out")
    );
    assert_eq!(
        timed_out.glob(GlobInput::new("**/*.txt")).unwrap(),
        ToolOutput::failure("glob: operation timed out")
    );

    assert_eq!(
        NativeToolLimits::default().operation_timeout,
        Duration::from_secs(5)
    );
}

#[test]
fn glob_excludes_gitignored_trees_before_consuming_the_scan_budget() {
    let root = project_root();
    fs::create_dir(root.join(".git")).unwrap();
    fs::write(root.join(".gitignore"), "/target/\n/references/\n").unwrap();
    fs::write(root.join("README.md"), "visible\n").unwrap();
    fs::write(root.join("package.json"), "{}\n").unwrap();
    fs::write(root.join("notes.md"), "visible\n").unwrap();
    for directory in ["target", "references"] {
        fs::create_dir(root.join(directory)).unwrap();
        for index in 0..8 {
            fs::write(
                root.join(directory).join(format!("ignored-{index}.md")),
                "ignored\n",
            )
            .unwrap();
        }
    }
    let tools = NativeTools::open_with_limits(
        &root,
        NativeToolLimits {
            max_search_entries: 5,
            ..NativeToolLimits::default()
        },
    )
    .unwrap();

    assert_eq!(
        tools.glob(GlobInput::new("**/README*")).unwrap(),
        ToolOutput::success("README.md\n")
    );
    assert_eq!(
        tools.glob(GlobInput::new("**/package.json")).unwrap(),
        ToolOutput::success("package.json\n")
    );
    assert_eq!(
        tools.glob(GlobInput::new("**/*.md")).unwrap(),
        ToolOutput::success("README.md\nnotes.md\n")
    );
}

#[test]
fn glob_applies_local_ignore_rules_safely_outside_git_repositories() {
    let root = project_root();
    fs::write(root.join(".gitignore"), "cache/\n").unwrap();
    fs::write(root.join("README.md"), "visible\n").unwrap();
    fs::create_dir(root.join("cache")).unwrap();
    for index in 0..8 {
        fs::write(
            root.join("cache").join(format!("ignored-{index}.md")),
            "ignored\n",
        )
        .unwrap();
    }
    let tools = NativeTools::open_with_limits(
        &root,
        NativeToolLimits {
            max_search_entries: 2,
            ..NativeToolLimits::default()
        },
    )
    .unwrap();

    assert_eq!(
        tools.glob(GlobInput::new("**/*.md")).unwrap(),
        ToolOutput::success("README.md\n")
    );
}

#[test]
fn glob_and_list_enforce_the_exact_default_entry_cap() {
    let root = project_root();
    for index in 0..=1_000 {
        fs::write(root.join(format!("entry-{index:04}.txt")), "entry\n").unwrap();
    }
    let tools = NativeTools::open(&root).unwrap();

    assert_eq!(
        tools.list_directory(ListDirectoryInput::new(".")).unwrap(),
        ToolOutput::failure("list: entry limit of 1000 exceeded")
    );

    let output = tools.glob(GlobInput::new("**/*.txt")).unwrap();
    assert!(!output.is_error);
    assert_eq!(output.content.lines().count(), 1_001);
    assert_eq!(
        output.content.lines().last(),
        Some("[glob output truncated after 1000 entries]")
    );
}

#[test]
fn catalog_dispatches_grep_and_glob_with_their_own_schemas() {
    let root = project_root();
    fs::write(root.join("notes.txt"), "needle\n").unwrap();
    let catalog = NativeToolCatalog::new(NativeTools::open(&root).unwrap());
    let metadata = NativeToolCatalog::metadata();

    assert_eq!(metadata.len(), 10);
    let grep = metadata
        .iter()
        .find(|tool| tool.qualified_name == "native::grep")
        .expect("grep metadata");
    assert_eq!(grep.input_schema["required"], json!(["pattern"]));
    let glob = metadata
        .iter()
        .find(|tool| tool.qualified_name == "native::glob")
        .expect("glob metadata");
    assert_eq!(glob.input_schema["required"], json!(["pattern"]));

    let context = ToolExecutionContext::with_timeout(Duration::from_secs(1));
    assert_eq!(
        catalog
            .execute("native::grep", json!({"pattern": "needle"}), &context)
            .unwrap(),
        ToolOutput::success("notes.txt:1:needle\n")
    );
    assert_eq!(
        catalog
            .execute("native::glob", json!({"pattern": "**/*.txt"}), &context)
            .unwrap(),
        ToolOutput::success("notes.txt\n")
    );
}

#[test]
fn webfetch_rejects_unsafe_urls_and_honors_cancellation_before_network_access() {
    let root = project_root();
    let tools = NativeTools::open(&root).unwrap();

    for url in [
        "",
        "ftp://example.test",
        "http://user:secret@example.test/",
        "http://169.254.169.254/latest/meta-data/",
        "http://[fe80::1]/",
    ] {
        assert!(tools.webfetch(WebfetchInput::new(url)).unwrap().is_error);
    }

    let cancelled = Arc::new(AtomicBool::new(true));
    assert_eq!(
        tools
            .webfetch(WebfetchInput::new("http://127.0.0.1/").with_cancellation(cancelled),)
            .unwrap(),
        ToolOutput::failure("webfetch: cancelled")
    );
}

const FIXTURE_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a test waits for a condition another thread is expected to publish. Only a
/// hung fixture ever spends it, so it is sized to outlast scheduler starvation rather
/// than to bound the happy path.
const FIXTURE_WAIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Execution budget handed to the cancellation tests. An unhonored cancellation burns
/// all of it, because the fixture holds the connection open and never answers.
const EXECUTION_BUDGET: Duration = Duration::from_secs(2);

/// How long a cancelled webfetch may take to return once cancellation is published.
///
/// The real cost is one `PROCESS_POLL_INTERVAL` pickup (10ms) plus a thread join, so the
/// bound is loose enough that CPU contention cannot trip it. What it must keep proving is
/// that the call returned promptly instead of waiting out `EXECUTION_BUDGET`, so it has to
/// stay far below that; do not raise it towards two seconds.
const CANCELLATION_BUDGET: Duration = Duration::from_millis(500);

/// Blocks until another thread makes `condition` hold, failing loudly instead of hanging
/// if it never does. Preferred over a fixed sleep, whose margin evaporates under load.
fn wait_for(condition: impl Fn() -> bool, message: &str) {
    let deadline = Instant::now() + FIXTURE_WAIT_TIMEOUT;

    while !condition() {
        assert!(Instant::now() < deadline, "{message}");
        thread::sleep(Duration::from_millis(1));
    }
}

/// Binds a loopback listener that fixture workers poll instead of parking inside
/// `accept`, so a worker can still notice that its test is over.
fn bind_pollable_listener() -> (TcpListener, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();

    (listener, port)
}

/// Waits for one client connection while honoring the stop flag.
///
/// Under CPU contention a webfetch budget can expire before hyper's connector issues
/// its `connect`, so the client may never open a socket at all; a bare `accept` would
/// then park the worker forever and deadlock the `join` that ends the test. Accepted
/// streams are handed back in blocking mode with a read timeout, because platforms
/// differ on whether they inherit the listener's nonblocking flag and a client that
/// connects without sending would otherwise park the worker just as hard.
fn accept_until_stopped(listener: &TcpListener, stop: &AtomicBool) -> Option<TcpStream> {
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                stream.set_read_timeout(Some(FIXTURE_READ_TIMEOUT)).unwrap();

                return Some(stream);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => panic!("fixture listener should accept: {error}"),
        }
    }

    None
}

/// A fixture listener thread whose `join` cannot block indefinitely: it first tells the
/// worker to stop waiting for a connection the client may never open, then surfaces any
/// expectation the worker failed as a test failure.
struct FixtureServer {
    stop: Arc<AtomicBool>,
    worker: thread::JoinHandle<()>,
}

impl FixtureServer {
    fn spawn<F>(body: F) -> Self
    where
        F: FnOnce(&AtomicBool) + Send + 'static,
    {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = thread::spawn(move || body(&worker_stop));

        Self { stop, worker }
    }

    fn join(self) {
        self.stop.store(true, Ordering::Release);
        self.worker.join().expect("fixture server should finish");
    }
}

fn webfetch_fixture(
    responses: Vec<String>,
    request: Arc<Mutex<String>>,
) -> (String, FixtureServer) {
    let (listener, port) = bind_pollable_listener();
    let url = format!("http://localhost:{port}/");
    let worker = FixtureServer::spawn(move |stop| {
        for response in responses {
            let mut stream = accept_until_stopped(&listener, stop)
                .expect("fixture client should connect for every queued response");
            let mut bytes = [0; 4096];
            let read = stream.read(&mut bytes).unwrap();
            *request.lock().unwrap() = String::from_utf8_lossy(&bytes[..read]).into_owned();
            stream.write_all(response.as_bytes()).unwrap();
        }
    });
    (url, worker)
}

#[test]
fn webfetch_enforces_redirects_response_contract_and_headers() {
    let root = project_root();
    let tools = NativeTools::open(&root).unwrap();
    let request = Arc::new(Mutex::new(String::new()));
    let (url, worker) = webfetch_fixture(
        vec![
            "HTTP/1.1 302 Found\r\nLocation: /one\r\nContent-Length: 0\r\n\r\n".into(),
            "HTTP/1.1 302 Found\r\nLocation: /two\r\nContent-Length: 0\r\n\r\n".into(),
            "HTTP/1.1 302 Found\r\nLocation: /three\r\nContent-Length: 0\r\n\r\n".into(),
            "HTTP/1.1 302 Found\r\nLocation: /four\r\nContent-Length: 0\r\n\r\n".into(),
            "HTTP/1.1 302 Found\r\nLocation: /five\r\nContent-Length: 0\r\n\r\n".into(),
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<p>visible</p><script>secret</script><style>hidden</style>text".into(),
        ],
        Arc::clone(&request),
    );

    assert_eq!(
        tools.webfetch(WebfetchInput::new(url)).unwrap(),
        ToolOutput::success("visible text")
    );
    worker.join();

    let (url, worker) = webfetch_fixture(
        vec![
            "HTTP/1.1 302 Found\r\nLocation: ftp://example.test/\r\nContent-Length: 0\r\n\r\n"
                .into(),
        ],
        Arc::new(Mutex::new(String::new())),
    );
    assert_eq!(
        tools.webfetch(WebfetchInput::new(url)).unwrap(),
        ToolOutput::failure("webfetch: URL must use http or https")
    );
    worker.join();
    let request = request.lock().unwrap();
    let request = request.to_ascii_lowercase();
    assert!(request.contains("user-agent: agens-webfetch/1"));
    assert!(!request.contains("authorization:"));
    assert!(!request.contains("cookie:"));

    let (url, worker) = webfetch_fixture(
        vec!["HTTP/1.1 302 Found\r\nLocation: http://user:secret@example.test/\r\nContent-Length: 0\r\n\r\n".into()],
        Arc::new(Mutex::new(String::new())),
    );
    assert_eq!(
        tools.webfetch(WebfetchInput::new(url)).unwrap(),
        ToolOutput::failure("webfetch: URL credentials are not allowed")
    );
    worker.join();

    let (url, worker) = webfetch_fixture(
        vec![
            "HTTP/1.1 302 Found\r\nLocation: http://169.254.169.254/\r\nContent-Length: 0\r\n\r\n"
                .into(),
        ],
        Arc::new(Mutex::new(String::new())),
    );
    assert_eq!(
        tools.webfetch(WebfetchInput::new(url)).unwrap(),
        ToolOutput::failure("webfetch: blocked network address")
    );
    worker.join();

    let (url, worker) = webfetch_fixture(
        vec!["HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\r\nraw\0body".into()],
        Arc::new(Mutex::new(String::new())),
    );
    assert_eq!(
        tools.webfetch(WebfetchInput::new(url)).unwrap(),
        ToolOutput::success("raw\0body")
    );
    worker.join();

    let (url, worker) = webfetch_fixture(
        vec![format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\n{}",
            "x".repeat(100 * 1024 + 1)
        )],
        Arc::new(Mutex::new(String::new())),
    );
    let output = tools.webfetch(WebfetchInput::new(url)).unwrap();
    assert!(!output.is_error);
    assert!(output.content.ends_with("[webfetch output truncated]"));
    assert!(output.content.len() <= 100 * 1024);
    worker.join();

    let (url, worker) = webfetch_fixture(
        vec!["HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\n\r\nmissing".into()],
        Arc::new(Mutex::new(String::new())),
    );
    assert_eq!(
        tools.webfetch(WebfetchInput::new(url)).unwrap(),
        ToolOutput::failure("webfetch: HTTP status 404 Not Found")
    );
    worker.join();

    let (listener, port) = bind_pollable_listener();
    let url = format!("http://127.0.0.1:{port}/");

    // The one-millisecond budget can expire before the connector even dials, so a
    // connection that never arrives is a legitimate outcome here: the assertion is
    // about the timeout, not about the server observing a request.
    let worker = FixtureServer::spawn(move |stop| {
        if let Some(_stream) = accept_until_stopped(&listener, stop) {
            thread::sleep(Duration::from_millis(20));
        }
    });
    assert_eq!(
        tools
            .webfetch(WebfetchInput::new(url).with_timeout(Duration::from_millis(1)))
            .unwrap(),
        ToolOutput::failure("webfetch: timed out")
    );
    worker.join();
}

#[test]
fn webfetch_rejects_six_redirects_and_cancels_delayed_headers_and_bodies() {
    let root = project_root();
    let tools = NativeTools::open(&root).unwrap();
    let (url, worker) = webfetch_fixture(
        vec!["HTTP/1.1 302 Found\r\nLocation: /again\r\nContent-Length: 0\r\n\r\n".into(); 6],
        Arc::new(Mutex::new(String::new())),
    );
    assert_eq!(
        tools.webfetch(WebfetchInput::new(url)).unwrap(),
        ToolOutput::failure("webfetch: redirect limit exceeded")
    );
    worker.join();

    for response in [
        None,
        Some("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 8\r\n\r\n"),
    ] {
        let (listener, port) = bind_pollable_listener();
        let url = format!("http://127.0.0.1:{port}/");
        let cancellation = Arc::new(AtomicBool::new(false));
        let in_flight = Arc::new(AtomicBool::new(false));
        let server_in_flight = Arc::clone(&in_flight);
        let worker = FixtureServer::spawn(move |stop| {
            let mut stream = accept_until_stopped(&listener, stop)
                .expect("cancelled request should still reach the server");
            let mut request = [0; 4096];
            stream.read_exact(&mut request[..1]).unwrap();
            if let Some(response) = response {
                stream.write_all(response.as_bytes()).unwrap();
            }
            server_in_flight.store(true, Ordering::Release);

            // Holding the connection open until the test is done is what gives the
            // budget below its meaning: the request can only finish by being cancelled.
            while !stop.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(1));
            }
        });
        let cancelled = Arc::clone(&cancellation);
        let catalog = Arc::new(NativeToolCatalog::new(NativeTools::open(&root).unwrap()));
        let request_catalog = Arc::clone(&catalog);
        let request = thread::spawn(move || {
            request_catalog
                .execute(
                    "native::webfetch",
                    json!({"url": url}),
                    &ToolExecutionContext::new(cancelled, EXECUTION_BUDGET),
                )
                .unwrap()
        });

        wait_for(
            || in_flight.load(Ordering::Acquire),
            "the request should reach the server before it is cancelled",
        );
        let cancelled_at = Instant::now();
        cancellation.store(true, Ordering::Release);
        assert_eq!(
            request.join().unwrap(),
            ToolOutput::failure("tool execution cancelled")
        );

        assert!(
            cancelled_at.elapsed() < CANCELLATION_BUDGET,
            "cancellation took {:?}, which no longer distinguishes a prompt return from \
             waiting out the {EXECUTION_BUDGET:?} execution budget",
            cancelled_at.elapsed()
        );

        worker.join();
        drop(catalog);
    }
}

#[test]
fn webfetch_bounds_cancelled_request_workers_and_reuses_the_admission_slot() {
    let root = project_root();
    let tools = Arc::new(NativeTools::open(&root).unwrap());
    let (listener, port) = bind_pollable_listener();
    let url = format!("http://localhost:{port}/");
    let accepted = Arc::new(AtomicUsize::new(0));
    let server_accepted = Arc::clone(&accepted);
    let release_first = Arc::new(AtomicBool::new(false));
    let server_release = Arc::clone(&release_first);
    let server = FixtureServer::spawn(move |stop| {
        let mut first = accept_until_stopped(&listener, stop)
            .expect("the cancelled request should reach the server");
        server_accepted.fetch_add(1, Ordering::Release);
        let mut request = [0; 4096];
        first.read_exact(&mut request[..1]).unwrap();

        // The orphaned request worker keeps the admission slot until this response
        // lands, so the test decides when that happens instead of racing a sleep.
        while !server_release.load(Ordering::Acquire) {
            if stop.load(Ordering::Acquire) {
                return;
            }
            thread::sleep(Duration::from_millis(1));
        }
        first
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nfirst")
            .unwrap();

        let mut second = accept_until_stopped(&listener, stop)
            .expect("the readmitted request should reach the server");
        server_accepted.fetch_add(1, Ordering::Release);
        second.read_exact(&mut request[..1]).unwrap();
        second
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\nsecond")
            .unwrap();
    });
    let cancellation = Arc::new(AtomicBool::new(false));
    let request_tools = Arc::clone(&tools);
    let request_url = url.clone();
    let request_cancellation = Arc::clone(&cancellation);
    let request = thread::spawn(move || {
        request_tools
            .webfetch(WebfetchInput::new(request_url).with_cancellation(request_cancellation))
            .unwrap()
    });

    wait_for(
        || accepted.load(Ordering::Acquire) == 1,
        "request worker did not start",
    );
    cancellation.store(true, Ordering::Release);
    assert_eq!(
        request.join().unwrap(),
        ToolOutput::failure("webfetch: cancelled")
    );

    for _ in 0..4 {
        assert_eq!(
            tools.webfetch(WebfetchInput::new(&url)).unwrap(),
            ToolOutput::failure("webfetch: request busy")
        );
    }
    assert_eq!(accepted.load(Ordering::Acquire), 1);

    release_first.store(true, Ordering::Release);

    let deadline = Instant::now() + FIXTURE_WAIT_TIMEOUT;
    let readmitted = loop {
        let output = tools.webfetch(WebfetchInput::new(&url)).unwrap();
        if output != ToolOutput::failure("webfetch: request busy") {
            break output;
        }

        assert!(
            Instant::now() < deadline,
            "the admission slot was never reused after the orphaned request worker finished"
        );
        thread::sleep(Duration::from_millis(5));
    };

    assert_eq!(readmitted, ToolOutput::success("second"));
    assert_eq!(accepted.load(Ordering::Acquire), 2);
    server.join();
    drop(tools);
}

#[cfg(unix)]
#[test]
fn confined_open_reports_canonical_filesystem_reasons() {
    let root = project_root();
    fs::create_dir(root.join("nested")).unwrap();
    fs::write(root.join("locked.txt"), "body").unwrap();
    fs::set_permissions(
        root.join("locked.txt"),
        std::os::unix::fs::PermissionsExt::from_mode(0o000),
    )
    .unwrap();
    let tools = NativeTools::open(&root).unwrap();

    assert_eq!(
        tools.read_file(ReadFileInput::new("missing.json")).unwrap(),
        ToolOutput::failure("read: file not found")
    );
    assert_eq!(
        tools.read_file(ReadFileInput::new("locked.txt")).unwrap(),
        ToolOutput::failure("read: permission denied")
    );
    assert_eq!(
        tools.read_file(ReadFileInput::new("nested")).unwrap(),
        ToolOutput::failure("read: path is not a regular file")
    );
    assert_eq!(
        tools
            .edit_file(EditFileInput::new("missing.json", "a", "b"))
            .unwrap(),
        ToolOutput::failure("edit: file not found")
    );

    drop(tools);
}
