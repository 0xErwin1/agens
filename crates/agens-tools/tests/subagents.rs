use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc, Barrier, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use agens_core::HeadlessTurnCancellation;
use agens_tools::{
    ChildCapability, SubagentInvocation, SubagentLimits, SubagentRunner, SubagentRunnerError,
    SubagentTool, SubagentTurnRequest, SubagentTurnResult, ToolExecutionContext, ToolOutput,
};

/// How long [`DelayedThenSuccessfulRunner`] ignores the supplied context on its
/// first call.
///
/// Every other constant here is expressed relative to this one; it is long
/// enough that a loaded scheduler cannot make a child that is still sleeping
/// look like a child that already returned.
const NON_COOPERATIVE_RUNNER_DELAY: Duration = Duration::from_millis(1_200);

/// Child deadline used against a non-cooperative runner.
///
/// It must stay an order of magnitude below [`NON_COOPERATIVE_RUNNER_DELAY`] so
/// the deadline — not the runner returning — is what ends a blocked call, and
/// far above the cost of spawning the child thread and handing its result back
/// over a channel, because the tool applies the same deadline to calls that are
/// expected to succeed. A few milliseconds satisfies the first constraint but
/// not the second, and turns a successful child into a spurious timeout under
/// load.
const NON_COOPERATIVE_CHILD_DEADLINE: Duration = Duration::from_millis(100);

/// Upper bound on how long a call bounded by a deadline or a cancellation may
/// take to come back.
///
/// It sits between the two constants above: high enough above
/// [`NON_COOPERATIVE_CHILD_DEADLINE`] to absorb scheduler jitter and the
/// result-polling interval, and low enough below
/// [`NON_COOPERATIVE_RUNNER_DELAY`] that it still proves the call returned on
/// its own bound instead of waiting for the runner.
const PROMPT_RETURN_BOUND: Duration = Duration::from_millis(500);

/// Overall budget for waiting on the permit a non-cooperative child holds.
///
/// Comfortably above [`NON_COOPERATIVE_RUNNER_DELAY`] so only a permit that is
/// genuinely never released fails the test, and bounded so such a regression
/// fails loudly instead of hanging.
const PERMIT_RELEASE_TIMEOUT: Duration = Duration::from_secs(5);

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
        SubagentLimits::new(1, 64, 64, Duration::from_secs(1)).expect("limits"),
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
        SubagentLimits::new(1, 64, 64, Duration::from_secs(1)).expect("limits"),
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
        SubagentLimits::new(1, 64, 64, Duration::from_secs(1)).expect("limits"),
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

    let still_active = tool.execute(
        SubagentInvocation::new("researcher", "while cancelled child completes"),
        Arc::new(AtomicBool::new(false)),
    );
    assert_eq!(
        still_active.content,
        "subagent: concurrent child limit reached"
    );

    thread::sleep(Duration::from_millis(80));
    let failure = tool.execute(
        SubagentInvocation::new("researcher", "fail without secrets"),
        Arc::new(AtomicBool::new(false)),
    );
    assert!(failure.is_error);
    assert_eq!(failure.content, "subagent: child execution failed");
    assert!(!failure.content.contains("credential"));
}

#[test]
fn accepts_long_child_results_but_still_enforces_the_output_limit() {
    let temporary = TemporaryDirectory::new();
    let skills_root = temporary.path.join("skills");
    write_skill(
        &skills_root,
        "researcher",
        "---\nname: researcher\ndescription: Research a bounded question\n---\nUse only the supplied context.\n",
    );
    let limits = SubagentLimits::new(1, 64, 4, Duration::from_secs(1)).expect("limits");
    let iteration_tool = SubagentTool::discover(
        &skills_root,
        temporary.path.join("missing"),
        FixedResultRunner(SubagentTurnResult::new("ok")),
        limits,
    )
    .expect("discover subagent skill");
    let output_tool = SubagentTool::discover(
        &skills_root,
        temporary.path.join("missing"),
        FixedResultRunner(SubagentTurnResult::new("too long")),
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

    assert_eq!(iteration.content, "ok");
    assert!(!iteration.is_error);
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
        SubagentLimits::new(1, 64, 64, Duration::from_millis(1)).expect("limits"),
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
fn inherits_the_live_parent_cancellation_before_child_admission() {
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
        SubagentLimits::new(1, 64, 64, Duration::from_secs(1)).expect("limits"),
    )
    .expect("discover subagent skill");
    let cancellation = HeadlessTurnCancellation::new();
    cancellation.cancel();
    let parent = ToolExecutionContext::from_headless_adapter(cancellation.adapter_view());

    let output =
        tool.execute_with_context(SubagentInvocation::new("researcher", "bounded"), &parent);

    assert_eq!(output.content, "subagent: cancelled");
    assert!(output.is_error);
    assert!(observed.lock().expect("recorded request").is_none());
}

#[test]
fn rejects_a_late_child_success_after_the_parent_cancels() {
    let temporary = TemporaryDirectory::new();
    let skills_root = temporary.path.join("skills");
    write_skill(
        &skills_root,
        "researcher",
        "---\nname: researcher\ndescription: Research a bounded question\n---\nUse only the supplied context.\n",
    );
    let runner = DelayedThenSuccessfulRunner::default();
    let entered = runner.entered();
    let tool = SubagentTool::discover(
        &skills_root,
        temporary.path.join("missing"),
        runner,
        SubagentLimits::new(1, 64, 64, Duration::from_secs(1)).expect("limits"),
    )
    .expect("discover subagent skill");
    let cancellation = HeadlessTurnCancellation::new();
    let parent = ToolExecutionContext::from_headless_adapter(cancellation.adapter_view());
    let child = tool.clone();
    let child_parent = parent.clone();

    let worker = thread::spawn(move || {
        child.execute_with_context(
            SubagentInvocation::new("researcher", "bounded"),
            &child_parent,
        )
    });
    wait_until_the_child_started(&entered);
    cancellation.cancel();

    let output = worker.join().expect("child caller");
    assert_eq!(output.content, "subagent: cancelled");
    assert!(output.is_error);

    let next_parent = ToolExecutionContext::with_timeout(Duration::from_secs(1));
    let rejected = tool.execute_with_context(
        SubagentInvocation::new("researcher", "second"),
        &next_parent,
    );
    assert_eq!(rejected.content, "subagent: concurrent child limit reached");
    assert!(rejected.is_error);

    let recovered = execute_once_the_permit_is_released(&tool, "third");
    assert_eq!(recovered.content, "child result");
}

#[test]
fn uses_the_earlier_parent_deadline_for_the_child() {
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
        SubagentLimits::new(1, 64, 64, Duration::from_secs(1)).expect("limits"),
    )
    .expect("discover subagent skill");
    let parent = ToolExecutionContext::from_headless_adapter(
        HeadlessTurnCancellation::with_deadline(Duration::from_millis(1)).adapter_view(),
    );

    let output =
        tool.execute_with_context(SubagentInvocation::new("researcher", "bounded"), &parent);

    assert_eq!(output.content, "subagent: deadline exceeded");
    assert!(output.is_error);
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
        SubagentLimits::new(1, 4, 64, Duration::from_secs(1)).expect("limits"),
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

#[test]
fn returns_by_deadline_and_retains_the_permit_until_a_non_cooperative_runner_finishes() {
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
        DelayedThenSuccessfulRunner::default(),
        SubagentLimits::new(1, 64, 64, NON_COOPERATIVE_CHILD_DEADLINE).expect("limits"),
    )
    .expect("discover subagent skill");

    let started = Instant::now();
    let timed_out = tool.execute(
        SubagentInvocation::new("researcher", "first"),
        Arc::new(AtomicBool::new(false)),
    );

    assert!(started.elapsed() < PROMPT_RETURN_BOUND);
    assert_eq!(timed_out.content, "subagent: deadline exceeded");
    assert!(timed_out.is_error);

    let rejected = tool.execute(
        SubagentInvocation::new("researcher", "second"),
        Arc::new(AtomicBool::new(false)),
    );
    assert_eq!(rejected.content, "subagent: concurrent child limit reached");

    let recovered = execute_once_the_permit_is_released(&tool, "third");
    assert_eq!(recovered.content, "child result");
    assert!(!recovered.is_error);
}

#[test]
fn returns_promptly_when_cancellation_reaches_a_non_cooperative_runner() {
    let temporary = TemporaryDirectory::new();
    let skills_root = temporary.path.join("skills");
    write_skill(
        &skills_root,
        "researcher",
        "---\nname: researcher\ndescription: Research a bounded question\n---\nUse only the supplied context.\n",
    );
    let runner = DelayedThenSuccessfulRunner::default();
    let entered = runner.entered();
    let tool = SubagentTool::discover(
        &skills_root,
        temporary.path.join("missing"),
        runner,
        SubagentLimits::new(1, 64, 64, Duration::from_secs(1)).expect("limits"),
    )
    .expect("discover subagent skill");
    let cancellation = Arc::new(AtomicBool::new(false));
    let trigger = Arc::clone(&cancellation);
    let cancellation_worker = thread::spawn(move || {
        wait_until_the_child_started(&entered);
        trigger.store(true, Ordering::Release);
    });

    let started = Instant::now();
    let cancelled = tool.execute(SubagentInvocation::new("researcher", "first"), cancellation);

    cancellation_worker.join().expect("cancellation worker");
    assert!(started.elapsed() < PROMPT_RETURN_BOUND);
    assert_eq!(cancelled.content, "subagent: cancelled");

    let rejected = tool.execute(
        SubagentInvocation::new("researcher", "second"),
        Arc::new(AtomicBool::new(false)),
    );
    assert_eq!(rejected.content, "subagent: concurrent child limit reached");

    let recovered = execute_once_the_permit_is_released(&tool, "third");
    assert_eq!(recovered.content, "child result");
}

#[test]
fn converts_panic_and_infrastructure_failures_to_distinct_sanitized_results() {
    let temporary = TemporaryDirectory::new();
    let skills_root = temporary.path.join("skills");
    write_skill(
        &skills_root,
        "researcher",
        "---\nname: researcher\ndescription: Research a bounded question\n---\nUse only the supplied context.\n",
    );
    let panic_tool = SubagentTool::discover(
        &skills_root,
        temporary.path.join("missing"),
        PanicThenSuccessfulRunner::default(),
        SubagentLimits::new(1, 64, 64, Duration::from_secs(1)).expect("limits"),
    )
    .expect("discover subagent skill");
    let infrastructure_tool = SubagentTool::discover(
        &skills_root,
        temporary.path.join("missing"),
        InfrastructureFailureRunner,
        SubagentLimits::new(1, 64, 64, Duration::from_secs(1)).expect("limits"),
    )
    .expect("discover subagent skill");

    let panic = panic_tool.execute(
        SubagentInvocation::new("researcher", "panic"),
        Arc::new(AtomicBool::new(false)),
    );
    assert_eq!(panic.content, "subagent: infrastructure failure");
    assert!(!panic.content.contains("PARENT_PROVIDER_SECRET_SENTINEL"));

    let recovered = panic_tool.execute(
        SubagentInvocation::new("researcher", "recover"),
        Arc::new(AtomicBool::new(false)),
    );
    assert_eq!(recovered.content, "subagent: infrastructure failure");
    assert_ne!(
        recovered.content,
        "subagent: concurrent child limit reached"
    );

    let infrastructure = infrastructure_tool.execute(
        SubagentInvocation::new("researcher", "internal failure"),
        Arc::new(AtomicBool::new(false)),
    );
    assert_eq!(infrastructure.content, "subagent: infrastructure failure");
    assert!(infrastructure.is_error);
}

#[test]
fn releases_the_permit_before_publishing_a_panic_result() {
    for _ in 0..128 {
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
            PanicThenSuccessfulRunner::default(),
            SubagentLimits::new(1, 64, 64, Duration::from_secs(1)).expect("limits"),
        )
        .expect("discover subagent skill");

        let panic = tool.execute(
            SubagentInvocation::new("researcher", "panic"),
            Arc::new(AtomicBool::new(false)),
        );
        assert_eq!(panic.content, "subagent: infrastructure failure");
        assert!(!panic.content.contains("PARENT_PROVIDER_SECRET_SENTINEL"));

        let next = tool.execute(
            SubagentInvocation::new("researcher", "immediate next"),
            Arc::new(AtomicBool::new(false)),
        );
        assert_eq!(next.content, "subagent: infrastructure failure");
        assert_ne!(next.content, "subagent: concurrent child limit reached");
    }
}

#[test]
fn caps_combined_prompt_and_context_before_calling_the_runner() {
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
        SubagentLimits::new(1, 4, 64, Duration::from_secs(1)).expect("limits"),
    )
    .expect("discover subagent skill");

    let output = tool.execute(
        SubagentInvocation::new("researcher", "abc")
            .with_context("de")
            .expect("bounded context"),
        Arc::new(AtomicBool::new(false)),
    );

    assert_eq!(output.content, "subagent: input exceeds configured bounds");
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
        Ok(SubagentTurnResult::new("child result"))
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
        Ok(SubagentTurnResult::new("child result"))
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
            thread::sleep(Duration::from_millis(50));
        }

        if context.is_cancelled() {
            return Err(SubagentRunnerError::ModelFailure);
        }

        Err(SubagentRunnerError::ModelFailure)
    }
}

#[derive(Default)]
struct DelayedThenSuccessfulRunner {
    calls: AtomicUsize,
    entered: Arc<AtomicBool>,
}

impl DelayedThenSuccessfulRunner {
    /// Observes the start of the first, deadline-ignoring call.
    ///
    /// The tool acquires the concurrency permit before spawning the child
    /// thread, so this flag being set implies the permit is already held.
    fn entered(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.entered)
    }
}

impl SubagentRunner for DelayedThenSuccessfulRunner {
    fn run(
        &mut self,
        _request: SubagentTurnRequest,
        _context: &agens_tools::SubagentRunContext,
    ) -> Result<SubagentTurnResult, SubagentRunnerError> {
        if self.calls.fetch_add(1, Ordering::AcqRel) == 0 {
            self.entered.store(true, Ordering::Release);
            thread::sleep(NON_COOPERATIVE_RUNNER_DELAY);
        }

        Ok(SubagentTurnResult::new("child result"))
    }
}

/// Blocks until the non-cooperative child is actually running, so a test that
/// wants to observe the held permit cannot race the child's admission.
fn wait_until_the_child_started(entered: &Arc<AtomicBool>) {
    let started = Instant::now();

    while !entered.load(Ordering::Acquire) {
        assert!(
            started.elapsed() < PERMIT_RELEASE_TIMEOUT,
            "the child never reached the runner"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

/// Retries `prompt` until the tool stops rejecting it for the concurrency
/// limit, and returns the output of the call that was finally admitted.
///
/// This waits on the condition the test cares about — the non-cooperative child
/// releasing its permit — instead of guessing how long that takes.
fn execute_once_the_permit_is_released(
    tool: &SubagentTool<DelayedThenSuccessfulRunner>,
    prompt: &str,
) -> ToolOutput {
    let started = Instant::now();

    loop {
        let output = tool.execute(
            SubagentInvocation::new("researcher", prompt),
            Arc::new(AtomicBool::new(false)),
        );
        if output.content != "subagent: concurrent child limit reached" {
            return output;
        }

        assert!(
            started.elapsed() < PERMIT_RELEASE_TIMEOUT,
            "the non-cooperative child never released its permit"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

#[derive(Default)]
struct PanicThenSuccessfulRunner {
    calls: AtomicUsize,
}

impl SubagentRunner for PanicThenSuccessfulRunner {
    fn run(
        &mut self,
        _request: SubagentTurnRequest,
        _context: &agens_tools::SubagentRunContext,
    ) -> Result<SubagentTurnResult, SubagentRunnerError> {
        if self.calls.fetch_add(1, Ordering::AcqRel) == 0 {
            panic!("PARENT_PROVIDER_SECRET_SENTINEL");
        }

        Ok(SubagentTurnResult::new("child result"))
    }
}

struct InfrastructureFailureRunner;

impl SubagentRunner for InfrastructureFailureRunner {
    fn run(
        &mut self,
        _request: SubagentTurnRequest,
        _context: &agens_tools::SubagentRunContext,
    ) -> Result<SubagentTurnResult, SubagentRunnerError> {
        Err(SubagentRunnerError::InfrastructureFailure)
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
        Ok(SubagentTurnResult::new("late"))
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
