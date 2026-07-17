use std::{
    fs,
    path::PathBuf,
    sync::{Arc, Barrier, Mutex, atomic::AtomicBool},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use agens_tools::{
    ChildCapability, SubagentInvocation, SubagentLimits, SubagentRunner, SubagentTool,
    SubagentTurnRequest, SubagentTurnResult,
};

#[test]
fn runs_a_validated_skill_in_an_isolated_non_recursive_child_context() {
    let temporary = TemporaryDirectory::new();
    let skills_root = temporary.path.join("skills");
    write_skill(
        &skills_root,
        "researcher",
        "---\nname: researcher\ndescription: Research a bounded question\n---\nUse only the supplied context.\n",
    );
    let runner = RecordingRunner::default();
    let observed = Arc::clone(&runner.observed);
    let tool = SubagentTool::discover(
        &skills_root,
        temporary.path.join("missing"),
        runner,
        SubagentLimits::new(1, 2, 64, 64, Duration::from_secs(1)).expect("limits"),
    )
    .expect("discover subagent skill");

    let output = tool.execute(
        SubagentInvocation::new("researcher", "summarize the design")
            .with_context("project facts only")
            .expect("bounded context"),
        Arc::new(AtomicBool::new(false)),
    );

    assert_eq!(output.content, "child result");
    assert!(!output.is_error);

    let request = observed
        .lock()
        .expect("recorded request")
        .clone()
        .expect("runner request");
    assert_eq!(request.skill_name(), "researcher");
    assert_eq!(request.prompt(), "summarize the design");
    assert_eq!(request.context(), "project facts only");
    assert_eq!(
        request.capabilities().allowed(),
        &[ChildCapability::FilesystemRead]
    );
    assert!(!request.capabilities().allows_descendants());
}

#[test]
fn bounds_concurrent_children_without_allowing_descendants() {
    let temporary = TemporaryDirectory::new();
    let skills_root = temporary.path.join("skills");
    write_skill(
        &skills_root,
        "researcher",
        "---\nname: researcher\ndescription: Research a bounded question\n---\nUse only the supplied context.\n",
    );
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let tool = SubagentTool::discover(
        &skills_root,
        temporary.path.join("missing"),
        BlockingRunner {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        },
        SubagentLimits::new(1, 2, 64, 64, Duration::from_secs(1)).expect("limits"),
    )
    .expect("discover subagent skill");
    let child = tool.clone();

    let worker = thread::spawn(move || {
        child.execute(
            SubagentInvocation::new("researcher", "first"),
            Arc::new(AtomicBool::new(false)),
        )
    });
    entered.wait();

    let rejected = tool.execute(
        SubagentInvocation::new("researcher", "second"),
        Arc::new(AtomicBool::new(false)),
    );
    assert!(rejected.is_error);
    assert_eq!(rejected.content, "subagent: concurrent child limit reached");

    release.wait();
    assert_eq!(worker.join().expect("worker").content, "child result");
}

#[test]
fn cancellation_and_child_failures_are_isolated_as_sanitized_tool_results() {
    let temporary = TemporaryDirectory::new();
    let skills_root = temporary.path.join("skills");
    write_skill(
        &skills_root,
        "researcher",
        "---\nname: researcher\ndescription: Research a bounded question\n---\nUse only the supplied context.\n",
    );
    let cancellation = Arc::new(AtomicBool::new(false));
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let tool = SubagentTool::discover(
        &skills_root,
        temporary.path.join("missing"),
        CancellableFailureRunner {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        },
        SubagentLimits::new(1, 2, 64, 64, Duration::from_secs(1)).expect("limits"),
    )
    .expect("discover subagent skill");
    let child = tool.clone();
    let child_cancellation = Arc::clone(&cancellation);

    let worker = thread::spawn(move || {
        child.execute(
            SubagentInvocation::new("researcher", "cancel this child"),
            child_cancellation,
        )
    });
    entered.wait();
    cancellation.store(true, std::sync::atomic::Ordering::Release);
    release.wait();

    let cancelled = worker.join().expect("worker");
    assert!(cancelled.is_error);
    assert_eq!(cancelled.content, "subagent: cancelled");

    let failure = tool.execute(
        SubagentInvocation::new("researcher", "fail without secrets"),
        Arc::new(AtomicBool::new(false)),
    );
    assert!(failure.is_error);
    assert_eq!(failure.content, "subagent: child execution failed");
    assert!(!failure.content.contains("credential"));
}

#[test]
fn rejects_child_results_that_exceed_iteration_or_output_limits() {
    let temporary = TemporaryDirectory::new();
    let skills_root = temporary.path.join("skills");
    write_skill(
        &skills_root,
        "researcher",
        "---\nname: researcher\ndescription: Research a bounded question\n---\nUse only the supplied context.\n",
    );
    let limits = SubagentLimits::new(1, 2, 64, 4, Duration::from_secs(1)).expect("limits");
    let iteration_tool = SubagentTool::discover(
        &skills_root,
        temporary.path.join("missing"),
        FixedResultRunner(SubagentTurnResult::new("ok", 3)),
        limits,
    )
    .expect("discover subagent skill");
    let output_tool = SubagentTool::discover(
        &skills_root,
        temporary.path.join("missing"),
        FixedResultRunner(SubagentTurnResult::new("too long", 1)),
        limits,
    )
    .expect("discover subagent skill");

    let iteration = iteration_tool.execute(
        SubagentInvocation::new("researcher", "bounded"),
        Arc::new(AtomicBool::new(false)),
    );
    let output = output_tool.execute(
        SubagentInvocation::new("researcher", "bounded"),
        Arc::new(AtomicBool::new(false)),
    );

    assert_eq!(iteration.content, "subagent: iteration limit exceeded");
    assert!(iteration.is_error);
    assert_eq!(output.content, "subagent: output limit exceeded");
    assert!(output.is_error);
}

#[test]
fn enforces_the_child_deadline_through_the_injected_runner_context() {
    let temporary = TemporaryDirectory::new();
    let skills_root = temporary.path.join("skills");
    write_skill(
        &skills_root,
        "researcher",
        "---\nname: researcher\ndescription: Research a bounded question\n---\nUse only the supplied context.\n",
    );
    let tool = SubagentTool::discover(
        &skills_root,
        temporary.path.join("missing"),
        DeadlineRunner,
        SubagentLimits::new(1, 2, 64, 64, Duration::from_millis(1)).expect("limits"),
    )
    .expect("discover subagent skill");

    let output = tool.execute(
        SubagentInvocation::new("researcher", "bounded"),
        Arc::new(AtomicBool::new(false)),
    );

    assert!(output.is_error);
    assert_eq!(output.content, "subagent: deadline exceeded");
}

#[test]
fn rejects_oversized_prompt_or_context_before_calling_the_runner() {
    let temporary = TemporaryDirectory::new();
    let skills_root = temporary.path.join("skills");
    write_skill(
        &skills_root,
        "researcher",
        "---\nname: researcher\ndescription: Research a bounded question\n---\nUse only the supplied context.\n",
    );
    let runner = RecordingRunner::default();
    let observed = Arc::clone(&runner.observed);
    let tool = SubagentTool::discover(
        &skills_root,
        temporary.path.join("missing"),
        runner,
        SubagentLimits::new(1, 2, 4, 64, Duration::from_secs(1)).expect("limits"),
    )
    .expect("discover subagent skill");

    let prompt = tool.execute(
        SubagentInvocation::new("researcher", "too long"),
        Arc::new(AtomicBool::new(false)),
    );
    let context = tool.execute(
        SubagentInvocation::new("researcher", "ok")
            .with_context("too long")
            .expect("bounded context"),
        Arc::new(AtomicBool::new(false)),
    );

    assert_eq!(prompt.content, "subagent: input exceeds configured bounds");
    assert_eq!(context.content, "subagent: input exceeds configured bounds");
    assert!(observed.lock().expect("recorded request").is_none());
}

#[derive(Default)]
struct RecordingRunner {
    observed: Arc<Mutex<Option<SubagentTurnRequest>>>,
}

impl SubagentRunner for RecordingRunner {
    fn run(
        &mut self,
        request: SubagentTurnRequest,
        _context: &agens_tools::SubagentRunContext,
    ) -> Result<SubagentTurnResult, agens_tools::SubagentRunnerError> {
        *self.observed.lock().expect("record request") = Some(request);
        Ok(SubagentTurnResult::new("child result", 1))
    }
}

struct BlockingRunner {
    entered: Arc<Barrier>,
    release: Arc<Barrier>,
}

impl SubagentRunner for BlockingRunner {
    fn run(
        &mut self,
        _request: SubagentTurnRequest,
        _context: &agens_tools::SubagentRunContext,
    ) -> Result<SubagentTurnResult, agens_tools::SubagentRunnerError> {
        self.entered.wait();
        self.release.wait();
        Ok(SubagentTurnResult::new("child result", 1))
    }
}

struct CancellableFailureRunner {
    entered: Arc<Barrier>,
    release: Arc<Barrier>,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl SubagentRunner for CancellableFailureRunner {
    fn run(
        &mut self,
        _request: SubagentTurnRequest,
        context: &agens_tools::SubagentRunContext,
    ) -> Result<SubagentTurnResult, agens_tools::SubagentRunnerError> {
        if self.calls.fetch_add(1, std::sync::atomic::Ordering::AcqRel) == 0 {
            self.entered.wait();
            self.release.wait();
        }

        if context.is_cancelled() {
            return Err(agens_tools::SubagentRunnerError::Failed);
        }

        Err(agens_tools::SubagentRunnerError::Failed)
    }
}

struct FixedResultRunner(SubagentTurnResult);

impl SubagentRunner for FixedResultRunner {
    fn run(
        &mut self,
        _request: SubagentTurnRequest,
        _context: &agens_tools::SubagentRunContext,
    ) -> Result<SubagentTurnResult, agens_tools::SubagentRunnerError> {
        Ok(self.0.clone())
    }
}

struct DeadlineRunner;

impl SubagentRunner for DeadlineRunner {
    fn run(
        &mut self,
        _request: SubagentTurnRequest,
        context: &agens_tools::SubagentRunContext,
    ) -> Result<SubagentTurnResult, agens_tools::SubagentRunnerError> {
        thread::sleep(Duration::from_millis(5));
        context.check()?;
        Ok(SubagentTurnResult::new("late", 1))
    }
}

fn write_skill(root: &std::path::Path, name: &str, contents: &str) {
    let directory = root.join(name);
    fs::create_dir_all(&directory).expect("skill directory");
    fs::write(directory.join("SKILL.md"), contents).expect("skill manifest");
}

struct TemporaryDirectory {
    path: PathBuf,
}

impl TemporaryDirectory {
    fn new() -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("agens-subagents-{timestamp}"));
        fs::create_dir_all(&path).expect("temporary directory");
        Self { path }
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
