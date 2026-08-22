//! Moving a session's tools to another directory, and the boundary that move
//! is confined by.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use agens_tools::{
    BashInput, ListDirectoryInput, MAX_SESSION_WORKTREES, NativeToolCatalog, NativeTools,
    ReadFileInput, SessionWorktrees, ToolExecutionContext, WorkingDirectory,
};
use serde_json::json;

static NEXT_ROOT: AtomicUsize = AtomicUsize::new(0);

/// A repository, a data directory for its session worktrees, and an unrelated
/// directory beside both, all removed when the test ends.
struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        let created = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "agens-working-directory-{}-{created}-{sequence}",
            std::process::id()
        ));

        fs::create_dir_all(root.join("repository/nested")).expect("create repository");
        fs::create_dir_all(root.join("data")).expect("create data directory");
        fs::create_dir_all(root.join("elsewhere")).expect("create unrelated directory");

        let fixture = Self { root };
        fixture.initialize_repository();
        fixture
    }

    fn initialize_repository(&self) {
        let repository = self.repository();
        fs::write(repository.join("root.txt"), "root file").expect("write root file");
        fs::write(repository.join("nested/inner.txt"), "inner file").expect("write inner file");

        git(&repository, &["init", "--quiet", "--initial-branch=main"]);
        git(&repository, &["config", "user.name", "Agens Test"]);
        git(
            &repository,
            &["config", "user.email", "agens-test@localhost"],
        );
        git(&repository, &["add", "."]);
        git(&repository, &["commit", "--quiet", "-m", "initial"]);
    }

    fn repository(&self) -> PathBuf {
        self.root.join("repository")
    }

    fn data_directory(&self) -> PathBuf {
        self.root.join("data")
    }

    fn elsewhere(&self) -> PathBuf {
        self.root.join("elsewhere")
    }

    fn tools(&self) -> NativeTools {
        NativeTools::open(self.repository())
            .expect("open native tools")
            .with_worktrees(SessionWorktrees::new(self.data_directory()), "fixture")
            .expect("configure session worktrees")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn git(directory: &Path, arguments: &[&str]) {
    let status = Command::new("git")
        .args(arguments)
        .current_dir(directory)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("run git");
    assert!(status.success(), "git {arguments:?} failed");
}

fn canonical(path: impl AsRef<Path>) -> PathBuf {
    fs::canonicalize(path).expect("canonicalize path")
}

#[test]
fn a_relative_path_resolves_against_the_directory_the_session_moved_to() {
    let fixture = Fixture::new();
    let mut tools = fixture.tools();

    assert!(
        tools
            .read_file(ReadFileInput::new("inner.txt"))
            .unwrap()
            .is_error
    );

    let moved = tools.change_directory(Path::new("nested")).unwrap();
    assert!(!moved.is_error, "{}", moved.content);

    let read = tools.read_file(ReadFileInput::new("inner.txt")).unwrap();
    assert!(!read.is_error, "{}", read.content);
    assert!(read.content.contains("inner file"));
}

#[test]
fn a_command_runs_in_the_directory_the_session_moved_to() {
    let fixture = Fixture::new();
    let mut tools = fixture.tools();
    tools
        .change_directory(Path::new("nested"))
        .expect("change directory");

    let output = tools
        .bash(BashInput::new("pwd").with_timeout(Duration::from_secs(30)))
        .unwrap();

    assert!(!output.is_error, "{}", output.content);
    assert!(
        output.content.contains(
            &canonical(fixture.repository().join("nested"))
                .display()
                .to_string()
        ),
        "{}",
        output.content
    );
}

#[test]
fn moving_outside_the_session_root_is_refused_and_leaves_the_session_where_it_was() {
    let fixture = Fixture::new();
    let mut tools = fixture.tools();

    let refused = tools.change_directory(&fixture.elsewhere()).unwrap();
    assert!(refused.is_error);
    assert!(refused.content.contains("outside"), "{}", refused.content);

    let refused = tools.change_directory(Path::new("../elsewhere")).unwrap();
    assert!(refused.is_error);

    let read = tools.read_file(ReadFileInput::new("root.txt")).unwrap();
    assert!(!read.is_error, "{}", read.content);
}

#[test]
fn moving_to_a_path_that_is_not_a_directory_is_refused() {
    let fixture = Fixture::new();
    let mut tools = fixture.tools();

    let refused = tools.change_directory(Path::new("root.txt")).unwrap();
    assert!(refused.is_error);

    let missing = tools.change_directory(Path::new("absent")).unwrap();
    assert!(missing.is_error);
}

#[test]
fn a_created_worktree_becomes_the_directory_the_session_works_in() {
    let fixture = Fixture::new();
    let mut tools = fixture.tools();

    let created = tools
        .create_worktree("feature", "feature-branch", "HEAD")
        .unwrap();
    assert!(!created.is_error, "{}", created.content);

    let listed = tools.list_directory(ListDirectoryInput::new(".")).unwrap();
    assert!(!listed.is_error, "{}", listed.content);
    assert!(listed.content.contains("root.txt"), "{}", listed.content);

    let branch = tools
        .bash(
            BashInput::new("git rev-parse --abbrev-ref HEAD").with_timeout(Duration::from_secs(30)),
        )
        .unwrap();
    assert!(
        branch.content.contains("feature-branch"),
        "{}",
        branch.content
    );
}

#[test]
fn the_session_can_move_back_to_its_own_root_from_a_worktree() {
    let fixture = Fixture::new();
    let mut tools = fixture.tools();
    tools
        .create_worktree("feature", "feature-branch", "HEAD")
        .expect("create worktree");

    let moved = tools.change_directory(&fixture.repository()).unwrap();
    assert!(!moved.is_error, "{}", moved.content);

    let read = tools.read_file(ReadFileInput::new("root.txt")).unwrap();
    assert!(!read.is_error, "{}", read.content);
}

#[test]
fn a_directory_beside_the_session_worktrees_stays_out_of_reach() {
    let fixture = Fixture::new();
    let mut tools = fixture.tools();
    let neighbour = fixture.data_directory().join("worktrees/other");
    fs::create_dir_all(&neighbour).expect("create neighbouring worktree directory");

    let refused = tools.change_directory(&neighbour).unwrap();

    assert!(refused.is_error, "{}", refused.content);
}

#[test]
fn a_session_without_worktrees_configured_cannot_create_one() {
    let fixture = Fixture::new();
    let mut tools = NativeTools::open(fixture.repository()).unwrap();

    let refused = tools
        .create_worktree("feature", "feature-branch", "HEAD")
        .unwrap();

    assert!(refused.is_error);
    assert!(
        refused.content.contains("unavailable"),
        "{}",
        refused.content
    );
}

#[test]
fn every_move_is_published_to_the_shared_working_directory() {
    let fixture = Fixture::new();
    let observed: Arc<Mutex<Vec<PathBuf>>> = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&observed);
    let directory =
        WorkingDirectory::new(fixture.repository()).with_observer(Arc::new(move |path: &Path| {
            recorder
                .lock()
                .expect("record move")
                .push(path.to_path_buf())
        }));
    let mut tools = fixture.tools().with_published_directory(directory.clone());

    tools
        .change_directory(Path::new("nested"))
        .expect("change directory");

    assert_eq!(
        directory.current(),
        canonical(fixture.repository().join("nested"))
    );
    assert_eq!(
        observed.lock().expect("read moves").as_slice(),
        [canonical(fixture.repository().join("nested"))]
    );
}

#[test]
fn a_refused_move_publishes_nothing() {
    let fixture = Fixture::new();
    let directory = WorkingDirectory::new(fixture.repository());
    let mut tools = fixture.tools().with_published_directory(directory.clone());

    let refused = tools.change_directory(&fixture.elsewhere()).unwrap();

    assert!(refused.is_error);
    assert_eq!(directory.current(), fixture.repository());
}

/// Budget for a catalog call that has to run to completion; the timeout paths
/// are proven elsewhere, so this only has to exceed the real cost.
const COMPLETION_BUDGET: Duration = Duration::from_secs(60);

#[test]
fn the_catalog_advertises_moving_and_worktree_creation() {
    let names = NativeToolCatalog::metadata()
        .into_iter()
        .map(|metadata| metadata.qualified_name)
        .collect::<Vec<_>>();

    assert!(names.contains(&"native::cd".to_owned()));
    assert!(names.contains(&"native::worktree".to_owned()));
}

#[test]
fn a_catalog_call_moves_the_session_for_every_later_call() {
    let fixture = Fixture::new();
    let mut catalog = NativeToolCatalog::new(fixture.tools());
    let context = ToolExecutionContext::with_timeout(COMPLETION_BUDGET);

    let moved = catalog
        .execute("native::cd", json!({ "path": "nested" }), &context)
        .unwrap();
    assert!(!moved.is_error, "{}", moved.content);

    let read = catalog
        .execute("native::read", json!({ "path": "inner.txt" }), &context)
        .unwrap();
    assert!(!read.is_error, "{}", read.content);
    assert!(read.content.contains("inner file"));
}

#[test]
fn a_catalog_worktree_call_leaves_the_session_inside_the_worktree() {
    let fixture = Fixture::new();
    let mut catalog = NativeToolCatalog::new(fixture.tools());
    let context = ToolExecutionContext::with_timeout(COMPLETION_BUDGET);

    let created = catalog
        .execute(
            "native::worktree",
            json!({ "name": "feature", "branch": "feature-branch" }),
            &context,
        )
        .unwrap();

    assert!(!created.is_error, "{}", created.content);
    assert!(
        created
            .content
            .contains(&fixture.data_directory().display().to_string()),
        "{}",
        created.content
    );
}

#[test]
fn a_worktree_that_resolves_outside_the_session_does_not_move_the_session() {
    let fixture = Fixture::new();
    let mut tools = fixture.tools();
    let home = fixture.data_directory().join("worktrees/fixture");
    fs::create_dir_all(&home).expect("create the session worktree home");
    let planted = fixture.elsewhere().join("planted");
    fs::create_dir_all(&planted).expect("create the directory the symlink points at");
    // git creates a worktree through a symlink whose target is an existing
    // empty directory, so the path under the session's worktree home is not
    // proof of where the checkout landed.
    std::os::unix::fs::symlink(&planted, home.join("feature")).expect("plant the symlink");

    let refused = tools
        .create_worktree("feature", "feature-branch", "HEAD")
        .unwrap();

    assert!(refused.is_error, "{}", refused.content);
    assert!(
        refused.content.starts_with("worktree:"),
        "{}",
        refused.content
    );
    assert_eq!(
        tools.working_directory(),
        canonical(fixture.repository()).as_path()
    );
}

#[test]
fn a_session_at_its_worktree_budget_reclaims_what_is_merged_and_clean() {
    let fixture = Fixture::new();
    let mut tools = fixture.tools();

    for index in 0..MAX_SESSION_WORKTREES {
        let name = format!("session-{index}");
        let created = tools
            .create_worktree(&name, &format!("branch-{index}"), "HEAD")
            .unwrap();
        assert!(!created.is_error, "{}", created.content);
    }

    // Every earlier worktree is still at the commit the session root is on
    // and carries no work, so the budget is met by reclaiming them rather
    // than by refusing.
    let created = tools
        .create_worktree("session-next", "branch-next", "HEAD")
        .unwrap();

    assert!(!created.is_error, "{}", created.content);
    let names = SessionWorktrees::new(fixture.data_directory())
        .names("fixture")
        .expect("list worktrees");
    assert!(names.contains(&"session-next".to_owned()), "{names:?}");
    assert!(
        names.len() < MAX_SESSION_WORKTREES + 1,
        "nothing was reclaimed: {names:?}"
    );
}

#[test]
fn a_session_whose_worktrees_all_carry_work_is_refused_a_new_one() {
    let fixture = Fixture::new();
    let mut tools = fixture.tools();

    for index in 0..MAX_SESSION_WORKTREES {
        let name = format!("session-{index}");
        tools
            .create_worktree(&name, &format!("branch-{index}"), "HEAD")
            .expect("create worktree");
        fs::write(
            fixture
                .data_directory()
                .join("worktrees/fixture")
                .join(&name)
                .join("work.txt"),
            "unfinished\n",
        )
        .expect("leave work in the worktree");
    }

    let refused = tools
        .create_worktree("session-next", "branch-next", "HEAD")
        .unwrap();

    assert!(refused.is_error, "{}", refused.content);
    assert!(
        refused.content.starts_with("worktree:"),
        "{}",
        refused.content
    );
    assert!(
        refused.content.contains("merged and clean"),
        "{}",
        refused.content
    );
}

#[test]
fn a_catalog_move_outside_the_session_root_is_refused() {
    let fixture = Fixture::new();
    let mut catalog = NativeToolCatalog::new(fixture.tools());
    let context = ToolExecutionContext::with_timeout(COMPLETION_BUDGET);

    let refused = catalog
        .execute(
            "native::cd",
            json!({ "path": fixture.elsewhere().display().to_string() }),
            &context,
        )
        .unwrap();

    assert!(refused.is_error, "{}", refused.content);
}
