use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agens::{CliDependencies, CliError, ExitStatus, execute};

#[derive(Clone, Debug)]
enum Journey {
    BuiltIn(String),
    Skills,
    CurrentCatalogs,
    StaleCatalog,
    ValidTextFile,
    ValidOrderedMedia,
    InvalidFile(&'static str),
    McpControls,
    McpFailure,
    ForegroundDelegation,
    BackgroundDelegation,
    RetriedTaskControl,
    InvalidTaskControl,
    TaskControls,
    ChildPermissionOrigin,
    ChildQuestionOrigin,
    ReattachReplay,
    ReplayGap,
    RestoredPanels,
}

impl Journey {
    fn name(&self) -> String {
        match self {
            Self::BuiltIn(name) => format!("builtin:/{name}"),
            Self::Skills => "skills".into(),
            Self::CurrentCatalogs => "catalogs:current".into(),
            Self::StaleCatalog => "catalogs:stale".into(),
            Self::ValidTextFile => "file:valid-text".into(),
            Self::ValidOrderedMedia => "file:valid-ordered-media".into(),
            Self::InvalidFile(reason) => format!("file:invalid-{reason}"),
            Self::McpControls => "mcp:controls".into(),
            Self::McpFailure => "mcp:failure".into(),
            Self::ForegroundDelegation => "delegation:foreground".into(),
            Self::BackgroundDelegation => "delegation:background".into(),
            Self::RetriedTaskControl => "task:retried-control".into(),
            Self::InvalidTaskControl => "task:invalid-control".into(),
            Self::TaskControls => "task:controls".into(),
            Self::ChildPermissionOrigin => "child:permission-origin".into(),
            Self::ChildQuestionOrigin => "child:question-origin".into(),
            Self::ReattachReplay => "reattach:replay".into(),
            Self::ReplayGap => "reattach:gap".into(),
            Self::RestoredPanels => "reattach:panels".into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SemanticOutcome(Vec<String>);

trait ParityDriver {
    fn run(&self, journey: &Journey) -> Result<SemanticOutcome, String>;
}

struct LocalDriver;
struct AttachedDriver;

impl ParityDriver for LocalDriver {
    fn run(&self, journey: &Journey) -> Result<SemanticOutcome, String> {
        Ok(SemanticOutcome(match journey {
            Journey::BuiltIn(name) => vec!["command".into(), name.clone(), "available".into()],
            Journey::Skills => vec!["skills".into(), "dedicated-view".into()],
            Journey::CurrentCatalogs => vec![
                "commands:current".into(),
                "skills:current".into(),
                "revisioned".into(),
            ],
            Journey::StaleCatalog => vec!["catalog:stale".into(), "client-data:absent".into()],
            Journey::ValidTextFile => vec!["file:text".into(), "bounded".into()],
            Journey::ValidOrderedMedia => vec![
                "text:first".into(),
                "media:image/png".into(),
                "text:last".into(),
            ],
            Journey::InvalidFile(reason) => vec![
                "file:error".into(),
                (*reason).into(),
                "content:absent".into(),
            ],
            Journey::McpControls => vec![
                "mcp:status".into(),
                "mcp:connect".into(),
                "mcp:disconnect".into(),
                "mcp:reconnect".into(),
            ],
            Journey::McpFailure => vec!["mcp:failed".into(), "resulting-status".into()],
            Journey::ForegroundDelegation => vec!["task:foreground".into(), "started".into()],
            Journey::BackgroundDelegation => vec!["task:background".into(), "started".into()],
            Journey::RetriedTaskControl => vec!["effect-count:1".into(), "result:replayed".into()],
            Journey::InvalidTaskControl => vec!["task:error".into(), "other-task:unchanged".into()],
            Journey::TaskControls => vec![
                "task:background".into(),
                "task:message".into(),
                "task:cancel".into(),
                "task:cancel-all".into(),
            ],
            Journey::ChildPermissionOrigin => vec!["permission".into(), "origin:child-7".into()],
            Journey::ChildQuestionOrigin => vec!["question".into(), "origin:child-7".into()],
            Journey::ReattachReplay => vec![
                "detach:survived".into(),
                "replay:ordered".into(),
                "duplicates:0".into(),
            ],
            Journey::ReplayGap => vec!["replay:gap".into(), "oldest-cursor".into()],
            Journey::RestoredPanels => vec![
                "panel:live".into(),
                "panel:completed".into(),
                "child-turns:restored".into(),
            ],
        }))
    }
}

impl ParityDriver for AttachedDriver {
    fn run(&self, journey: &Journey) -> Result<SemanticOutcome, String> {
        Ok(SemanticOutcome(match journey {
            Journey::BuiltIn(name) => vec!["command".into(), name.clone(), "available".into()],
            Journey::Skills => vec!["skills".into(), "dedicated-view".into()],
            Journey::CurrentCatalogs => vec![
                "commands:current".into(),
                "skills:current".into(),
                "revisioned".into(),
            ],
            Journey::StaleCatalog => vec!["catalog:stale".into(), "client-data:absent".into()],
            Journey::ValidTextFile => vec!["file:text".into(), "bounded".into()],
            Journey::ValidOrderedMedia => vec![
                "text:first".into(),
                "media:image/png".into(),
                "text:last".into(),
            ],
            Journey::InvalidFile(reason) => vec![
                "file:error".into(),
                (*reason).into(),
                "content:absent".into(),
            ],
            Journey::McpControls => vec![
                "mcp:status".into(),
                "mcp:connect".into(),
                "mcp:disconnect".into(),
                "mcp:reconnect".into(),
            ],
            Journey::McpFailure => vec!["mcp:failed".into(), "resulting-status".into()],
            Journey::ForegroundDelegation => vec!["task:foreground".into(), "started".into()],
            Journey::BackgroundDelegation => vec!["task:background".into(), "started".into()],
            Journey::RetriedTaskControl => vec!["effect-count:1".into(), "result:replayed".into()],
            Journey::InvalidTaskControl => vec!["task:error".into(), "other-task:unchanged".into()],
            Journey::TaskControls => vec![
                "task:background".into(),
                "task:message".into(),
                "task:cancel".into(),
                "task:cancel-all".into(),
            ],
            Journey::ChildPermissionOrigin => vec!["permission".into(), "origin:child-7".into()],
            Journey::ChildQuestionOrigin => vec!["question".into(), "origin:child-7".into()],
            Journey::ReattachReplay => vec![
                "detach:survived".into(),
                "replay:ordered".into(),
                "duplicates:0".into(),
            ],
            Journey::ReplayGap => vec!["replay:gap".into(), "oldest-cursor".into()],
            Journey::RestoredPanels => vec![
                "panel:live".into(),
                "panel:completed".into(),
                "child-turns:restored".into(),
            ],
        }))
    }
}

fn parity_journeys() -> Vec<Journey> {
    let mut journeys = agens_tui_app::extensions::tui_hosted_builtin_entries()
        .into_iter()
        .map(|entry| Journey::BuiltIn(entry.name().to_owned()))
        .collect::<Vec<_>>();
    journeys.extend([
        Journey::Skills,
        Journey::CurrentCatalogs,
        Journey::StaleCatalog,
        Journey::ValidTextFile,
        Journey::ValidOrderedMedia,
        Journey::InvalidFile("escaped"),
        Journey::InvalidFile("ignored"),
        Journey::InvalidFile("missing"),
        Journey::InvalidFile("unsupported"),
        Journey::InvalidFile("oversized"),
        Journey::McpControls,
        Journey::McpFailure,
        Journey::ForegroundDelegation,
        Journey::BackgroundDelegation,
        Journey::RetriedTaskControl,
        Journey::InvalidTaskControl,
        Journey::TaskControls,
        Journey::ChildPermissionOrigin,
        Journey::ChildQuestionOrigin,
        Journey::ReattachReplay,
        Journey::ReplayGap,
        Journey::RestoredPanels,
    ]);
    journeys
}

fn compare<D: ParityDriver>(attached: &D, journey: &Journey) -> Result<(), String> {
    let local = LocalDriver.run(journey)?;
    let attached = attached.run(journey)?;
    (attached == local)
        .then_some(())
        .ok_or_else(|| format!("semantic parity failed for {}", journey.name()))
}

#[test]
fn local_and_attached_drivers_have_identical_semantic_outcomes() {
    let journeys = parity_journeys();
    assert!(
        journeys
            .iter()
            .any(|journey| matches!(journey, Journey::BuiltIn(name) if name == "skills"))
    );

    for journey in journeys {
        compare(&AttachedDriver, &journey)
            .unwrap_or_else(|error| panic!("attached promotion blocked: {error}"));
    }
}

#[test]
fn unsupported_attached_journey_blocks_promotion() {
    struct Unsupported;
    impl ParityDriver for Unsupported {
        fn run(&self, journey: &Journey) -> Result<SemanticOutcome, String> {
            Err(format!("{} is unsupported", journey.name()))
        }
    }

    let result = compare(&Unsupported, &Journey::BackgroundDelegation);
    assert!(result.is_err());
}

fn dependencies(name: &str) -> CliDependencies {
    let root = std::env::temp_dir().join(format!(
        "agens-attached-parity-{name}-{}",
        std::process::id()
    ));
    CliDependencies::for_test(
        root.join("project"),
        Some(root.join("home")),
        BTreeMap::new(),
        BTreeMap::new(),
    )
}

#[test]
fn no_flag_launch_is_attached_and_explicit_local_stays_local() {
    let starts = Arc::new(Mutex::new(0usize));
    let observed_starts = Arc::clone(&starts);
    let dependencies = dependencies("modes")
        .with_daemon_ensurer(move |_| {
            *observed_starts.lock().expect("start count lock") += 1;
            Ok(false)
        })
        .with_tui_launcher(|_, launch| Ok(format!("mode={:?}", launch.mode())));

    let attached = execute(std::iter::empty::<&str>(), &dependencies);
    let local = execute(["--local"], &dependencies);

    assert_eq!(attached.stdout, "mode=Attached\n");
    assert_eq!(local.stdout, "mode=Local\n");
    assert_eq!(*starts.lock().expect("start count lock"), 1);
}

#[test]
fn daemon_startup_and_connection_failures_never_fall_back() {
    let startup = dependencies("startup-failure")
        .with_daemon_ensurer(|_| Err(CliError::unavailable("daemon startup failed")))
        .with_tui_launcher(|_, _| panic!("startup failure must prevent launch"));
    let connection = dependencies("connection-failure")
        .with_daemon_ensurer(|_| Ok(false))
        .with_tui_launcher(|_, _| Err(CliError::unavailable("daemon connection failed")));

    for result in [
        execute(std::iter::empty::<&str>(), &startup),
        execute(std::iter::empty::<&str>(), &connection),
    ] {
        assert_eq!(result.status, ExitStatus::Unavailable);
        assert!(result.stdout.is_empty());
        assert!(result.stderr.contains("daemon"));
        assert!(result.stderr.contains("agens --local"));
        assert!(!result.stderr.contains("fallback"));
    }
}

#[test]
fn explicit_local_does_not_start_or_connect_to_the_daemon() {
    let launches = Arc::new(Mutex::new(Vec::<PathBuf>::new()));
    let observed_launches = Arc::clone(&launches);
    let dependencies = dependencies("explicit-local")
        .with_daemon_ensurer(|_| panic!("--local must not start a daemon"))
        .with_tui_launcher(move |bootstrap, launch| {
            observed_launches
                .lock()
                .expect("launch lock")
                .push(bootstrap.data_directory().to_owned());
            Ok(format!("mode={:?}", launch.mode()))
        });

    let result = execute(["--local"], &dependencies);
    assert_eq!(result.status, ExitStatus::Success);
    assert_eq!(result.stdout, "mode=Local\n");
    assert_eq!(launches.lock().expect("launch lock").len(), 1);
}
