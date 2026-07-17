use std::{
    fs,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use agens_tools::{
    BashInput, ListDirectoryInput, NativeTools, ReadFileInput, SearchInput, ToolOutput,
    WriteFileInput,
};

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
        ToolOutput::failure("write: parent directory does not exist")
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
        ToolOutput::success(format!("{}\nproject\n", root.display()))
    );
    let failure = tools.bash(BashInput::new("exit 7")).unwrap();
    assert!(failure.is_error);
    assert!(failure.content.contains("exit status: 7"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bash_bounds_timeout_and_captured_output() {
    let root = project_root();
    let tools = NativeTools::open(&root).unwrap();

    let timeout = tools
        .bash(BashInput::new("sleep 1").with_timeout(Duration::from_millis(25)))
        .unwrap();
    assert_eq!(timeout, ToolOutput::failure("bash: timed out after 25ms"));

    let output = tools
        .bash(BashInput::new("printf 'x%.0s' {1..70000}"))
        .unwrap();
    assert!(!output.is_error);
    assert!(output.content.ends_with("\n[bash output truncated]\n"));
    assert!(output.content.len() <= 64 * 1024 + "\n[bash output truncated]\n".len());

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

    assert_eq!(output, ToolOutput::success("(no output; exit status 0)"));
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

    assert_eq!(output, ToolOutput::failure("bash: cancelled"));
    assert!(started.elapsed() < Duration::from_secs(2));
    thread::sleep(Duration::from_millis(1100));
    assert!(!marker.exists());
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
