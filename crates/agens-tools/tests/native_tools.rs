use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use agens_core::Error;
use agens_tools::{
    BashInput, EditFileInput, GlobInput, GrepInput, ListDirectoryInput, NativeToolCatalog,
    NativeToolLimits, NativeTools, ReadFileInput, SearchInput, ToolExecutionContext, ToolOutput,
    WebfetchInput, WriteFileInput,
};
use serde_json::json;

static NEXT_ROOT: AtomicUsize = AtomicUsize::new(0);

fn project_root() -> std::path::PathBuf {
    let suffix = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("agens-tools-{}-{suffix}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    root
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

    fs::remove_dir_all(root).unwrap();
    fs::remove_dir_all(outside).unwrap();
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

    fs::remove_dir_all(root).unwrap();
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

    fs::remove_dir_all(root).unwrap();
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

    fs::remove_dir_all(root).unwrap();
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
    fs::remove_dir_all(root).unwrap();
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
            "[stdout]\n[stderr]\n[bash: timed out after 25ms]\n[exit status: unavailable]\n"
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

    fs::remove_dir_all(root).unwrap();
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

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn catalog_preserves_the_bounded_bash_result_without_generic_truncation() {
    let root = project_root();
    let catalog = NativeToolCatalog::new(NativeTools::open(&root).unwrap());
    let output = catalog
        .execute(
            "native::bash",
            json!({"command": "printf 'x%.0s' {1..40000}; printf 'y%.0s' {1..40000} >&2"}),
            &ToolExecutionContext::with_timeout(Duration::from_secs(2)),
        )
        .unwrap();

    assert!(output.content.len() > 16 * 1024);
    assert!(output.content.len() <= 64 * 1024);
    assert!(output.content.contains("\n[stderr]\n"));
    assert!(output.content.contains("yyyy"));
    assert!(output.content.contains("[bash output truncated]\n"));
    assert!(output.content.ends_with("[exit status: 0]\n"));

    fs::remove_dir_all(root).unwrap();
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
            "[stdout]\n[stderr]\n[bash: timed out after 25ms]\n[exit status: unavailable]\n"
        )
    );
    thread::sleep(Duration::from_millis(1100));
    assert!(!marker.exists());

    fs::remove_dir_all(root).unwrap();
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
    fs::remove_dir_all(root).unwrap();
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
    assert!(started.elapsed() < Duration::from_secs(2));
    thread::sleep(Duration::from_millis(1100));
    assert!(!marker.exists());
    fs::remove_dir_all(root).unwrap();
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
    fs::remove_dir_all(root).unwrap();
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
    fs::remove_dir_all(root).unwrap();
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
    fs::remove_dir_all(root).unwrap();
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
            "[stdout]\n[stderr]\n[bash: timed out after 25ms]\n[exit status: unavailable]\n"
        )
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn reads_a_project_relative_file() {
    let root = project_root();
    fs::write(root.join("notes.txt"), "project note").unwrap();
    let tools = NativeTools::open(&root).unwrap();

    let output = tools.read_file(ReadFileInput::new("notes.txt")).unwrap();

    assert_eq!(output, ToolOutput::success("project note"));
    fs::remove_dir_all(root).unwrap();
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
    fs::remove_dir_all(root).unwrap();
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

    fs::remove_dir_all(root).unwrap();
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

    fs::remove_dir_all(root).unwrap();
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

    fs::remove_dir_all(root).unwrap();
    fs::remove_dir_all(outside).unwrap();
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

    fs::remove_dir_all(root).unwrap();
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
    fs::remove_dir_all(root).unwrap();
    fs::remove_dir_all(outside).unwrap();
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

    fs::remove_dir_all(root).unwrap();
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
    fs::remove_dir_all(root).unwrap();
    fs::remove_dir_all(outside).unwrap();
}

#[test]
fn catalog_exposes_strict_schemas_and_cancellation_suppresses_bash_output() {
    let root = project_root();
    let catalog = NativeToolCatalog::new(NativeTools::open(&root).unwrap());
    let metadata = NativeToolCatalog::metadata();
    assert_eq!(metadata.len(), 9);
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
    fs::remove_dir_all(root).unwrap();
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
        tools.grep(GrepInput::new("[")).unwrap(),
        ToolOutput::failure("grep: invalid regex")
    );

    fs::remove_dir_all(root).unwrap();
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

    fs::remove_dir_all(root).unwrap();
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
            .grep(GrepInput::new("EXTERNAL_SENTINEL").with_path(&outside))
            .unwrap(),
        ToolOutput::failure("path: must be a non-empty relative path")
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

    fs::remove_dir_all(root).unwrap();
    fs::remove_dir_all(outside).unwrap();
}

#[test]
fn grep_and_glob_enforce_exact_default_scan_result_depth_and_timeout_bounds() {
    let root = project_root();
    let tools = NativeTools::open(&root).unwrap();

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

    let mut directory = root.clone();
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
    fs::remove_dir_all(root).unwrap();
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

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn catalog_dispatches_grep_and_glob_with_their_own_schemas() {
    let root = project_root();
    fs::write(root.join("notes.txt"), "needle\n").unwrap();
    let catalog = NativeToolCatalog::new(NativeTools::open(&root).unwrap());
    let metadata = NativeToolCatalog::metadata();

    assert_eq!(metadata.len(), 9);
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

    fs::remove_dir_all(root).unwrap();
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

    fs::remove_dir_all(root).unwrap();
}

fn webfetch_fixture(
    responses: Vec<String>,
    request: Arc<Mutex<String>>,
) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "http://localhost:{}/",
        listener.local_addr().unwrap().port()
    );
    let worker = thread::spawn(move || {
        for response in responses {
            let (mut stream, _) = listener.accept().unwrap();
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
    let previous_proxy = std::env::var_os("HTTP_PROXY");
    unsafe { std::env::set_var("HTTP_PROXY", "http://127.0.0.1:1") };
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
    worker.join().unwrap();

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
    worker.join().unwrap();
    let request = request.lock().unwrap();
    let request = request.to_ascii_lowercase();
    assert!(request.contains("user-agent: agens-webfetch/1"));
    assert!(!request.contains("authorization:"));
    assert!(!request.contains("cookie:"));

    unsafe {
        match previous_proxy {
            Some(proxy) => std::env::set_var("HTTP_PROXY", proxy),
            None => std::env::remove_var("HTTP_PROXY"),
        }
    }

    let (url, worker) = webfetch_fixture(
        vec!["HTTP/1.1 302 Found\r\nLocation: http://user:secret@example.test/\r\nContent-Length: 0\r\n\r\n".into()],
        Arc::new(Mutex::new(String::new())),
    );
    assert_eq!(
        tools.webfetch(WebfetchInput::new(url)).unwrap(),
        ToolOutput::failure("webfetch: URL credentials are not allowed")
    );
    worker.join().unwrap();

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
    worker.join().unwrap();

    let (url, worker) = webfetch_fixture(
        vec!["HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\r\nraw\0body".into()],
        Arc::new(Mutex::new(String::new())),
    );
    assert_eq!(
        tools.webfetch(WebfetchInput::new(url)).unwrap(),
        ToolOutput::success("raw\0body")
    );
    worker.join().unwrap();

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
    worker.join().unwrap();

    let (url, worker) = webfetch_fixture(
        vec!["HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\n\r\nmissing".into()],
        Arc::new(Mutex::new(String::new())),
    );
    assert_eq!(
        tools.webfetch(WebfetchInput::new(url)).unwrap(),
        ToolOutput::failure("webfetch: HTTP status 404 Not Found")
    );
    worker.join().unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "http://127.0.0.1:{}/",
        listener.local_addr().unwrap().port()
    );
    let worker = thread::spawn(move || {
        let _stream = listener.accept().unwrap().0;
        thread::sleep(Duration::from_millis(20));
    });
    assert_eq!(
        tools
            .webfetch(WebfetchInput::new(url).with_timeout(Duration::from_millis(1)))
            .unwrap(),
        ToolOutput::failure("webfetch: timed out")
    );
    worker.join().unwrap();

    fs::remove_dir_all(root).unwrap();
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
    worker.join().unwrap();

    for response in [
        None,
        Some("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 8\r\n\r\n"),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://127.0.0.1:{}/",
            listener.local_addr().unwrap().port()
        );
        let cancellation = Arc::new(AtomicBool::new(false));
        let started = Instant::now();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            stream.read_exact(&mut request[..1]).unwrap();
            if let Some(response) = response {
                stream.write_all(response.as_bytes()).unwrap();
            }
            thread::sleep(Duration::from_millis(100));
        });
        let cancelled = Arc::clone(&cancellation);
        let catalog = Arc::new(NativeToolCatalog::new(NativeTools::open(&root).unwrap()));
        let request_catalog = Arc::clone(&catalog);
        let request = thread::spawn(move || {
            request_catalog
                .execute(
                    "native::webfetch",
                    json!({"url": url}),
                    &ToolExecutionContext::new(cancelled, Duration::from_secs(2)),
                )
                .unwrap()
        });
        thread::sleep(Duration::from_millis(20));
        cancellation.store(true, Ordering::Release);
        assert_eq!(
            request.join().unwrap(),
            ToolOutput::failure("tool execution cancelled")
        );
        assert!(started.elapsed() < Duration::from_millis(100));
        worker.join().unwrap();
        drop(catalog);
    }

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn webfetch_bounds_cancelled_request_workers_and_reuses_the_admission_slot() {
    let root = project_root();
    let tools = Arc::new(NativeTools::open(&root).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "http://localhost:{}/",
        listener.local_addr().unwrap().port()
    );
    let accepted = Arc::new(AtomicUsize::new(0));
    let server_accepted = Arc::clone(&accepted);
    let server = thread::spawn(move || {
        let (mut first, _) = listener.accept().unwrap();
        server_accepted.fetch_add(1, Ordering::Release);
        let mut request = [0; 4096];
        first.read_exact(&mut request[..1]).unwrap();
        thread::sleep(Duration::from_millis(300));
        first
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nfirst")
            .unwrap();

        let (mut second, _) = listener.accept().unwrap();
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

    let deadline = Instant::now() + Duration::from_secs(1);
    while accepted.load(Ordering::Acquire) == 0 {
        assert!(Instant::now() < deadline, "request worker did not start");
        thread::sleep(Duration::from_millis(1));
    }
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

    thread::sleep(Duration::from_millis(320));
    assert_eq!(
        tools.webfetch(WebfetchInput::new(url)).unwrap(),
        ToolOutput::success("second")
    );
    assert_eq!(accepted.load(Ordering::Acquire), 2);
    server.join().unwrap();
    drop(tools);
    fs::remove_dir_all(root).unwrap();
}
