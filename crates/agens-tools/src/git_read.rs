//! Bounded read-only git access.
//!
//! The tool never reaches git through a shell and never forwards a model-supplied
//! string as a flag: the argv is fixed per operation and the only free values are
//! revisions, validated as plain ref names. Git also reaches write and execution
//! behaviour through configuration rather than through the subcommand name
//! (`--output=`, `diff.external`, `core.fsmonitor`, the optional index refresh of
//! `status`), so the invocation closes those paths as well; see
//! `HARDENING_ARGUMENTS` for which mechanism stops which behaviour.

use std::{
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use agens_core::Error;

use crate::{
    CappedOutput, PROCESS_POLL_INTERVAL, ToolExecutionContext, ToolOutput, kill_process_group,
    lossy_bash_stream, read_capped, terminate_process_group, wait_for_readers,
};

const DEFAULT_GIT_READ_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_LOG_ENTRIES: usize = 20;
const MAX_LOG_ENTRIES: usize = 500;
const MAX_REVISION_CHARS: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitReadOperation {
    Status,
    Diff,
    Log,
    BranchMerged,
    MergeBase,
}

impl GitReadOperation {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "status" => Some(Self::Status),
            "diff" => Some(Self::Diff),
            "log" => Some(Self::Log),
            "branch_merged" => Some(Self::BranchMerged),
            "merge_base" => Some(Self::MergeBase),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Diff => "diff",
            Self::Log => "log",
            Self::BranchMerged => "branch_merged",
            Self::MergeBase => "merge_base",
        }
    }
}

#[derive(Clone, Debug)]
pub struct GitReadInput {
    operation: GitReadOperation,
    base: Option<String>,
    head: Option<String>,
    staged: bool,
    limit: Option<usize>,
    timeout: Duration,
    execution_context: Option<ToolExecutionContext>,
}

impl GitReadInput {
    pub fn new(operation: GitReadOperation) -> Self {
        Self {
            operation,
            base: None,
            head: None,
            staged: false,
            limit: None,
            timeout: DEFAULT_GIT_READ_TIMEOUT,
            execution_context: None,
        }
    }

    pub fn with_base(mut self, base: impl Into<String>) -> Self {
        self.base = Some(base.into());
        self
    }

    pub fn with_head(mut self, head: impl Into<String>) -> Self {
        self.head = Some(head.into());
        self
    }

    pub fn with_staged(mut self, staged: bool) -> Self {
        self.staged = staged;
        self
    }

    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Lowers the timeout to the caller's remaining budget without ever raising
    /// it above the tool's own bound.
    pub fn capped_at(mut self, remaining: Duration) -> Self {
        self.timeout = self.timeout.min(remaining);
        self
    }

    pub fn with_execution_context(mut self, context: ToolExecutionContext) -> Self {
        self.execution_context = Some(context);
        self
    }
}

impl crate::NativeTools {
    pub fn git_read(&self, input: GitReadInput) -> Result<ToolOutput, Error> {
        if let Err(output) = self.ensure_project_root_is_stable() {
            return Ok(output);
        }
        if input.timeout.is_zero() {
            return Ok(failure(
                input.operation,
                "timeout must be greater than zero",
            ));
        }

        let arguments = match subcommand_arguments(&input) {
            Ok(arguments) => arguments,
            Err(output) => return Ok(output),
        };

        run_git(&self.project_root, &arguments, &input)
    }
}

/// Builds the full argv after the hardening prefix. Revisions reach it only
/// after `validated_revision`, so no element here can be read as a flag.
fn subcommand_arguments(input: &GitReadInput) -> Result<Vec<String>, ToolOutput> {
    let operation = input.operation;
    let base = optional_revision(operation, input.base.as_deref())?;
    let head = optional_revision(operation, input.head.as_deref())?;

    let mut arguments = Vec::new();

    match operation {
        GitReadOperation::Status => {
            arguments.extend(["status".into(), "--porcelain=v1".into(), "--branch".into()]);
        }
        GitReadOperation::Diff => {
            arguments.extend(["diff".into(), "--no-ext-diff".into(), "--no-color".into()]);
            if input.staged {
                arguments.push("--cached".into());
            }
            arguments.extend(base);
            arguments.extend(head);
            arguments.push("--".into());
        }
        GitReadOperation::Log => {
            let limit = input
                .limit
                .unwrap_or(DEFAULT_LOG_ENTRIES)
                .clamp(1, MAX_LOG_ENTRIES);
            arguments.extend([
                "log".into(),
                "--no-color".into(),
                "--oneline".into(),
                "--no-decorate".into(),
                "-n".into(),
                limit.to_string(),
            ]);
            match (base, head) {
                (Some(base), Some(head)) => arguments.push(format!("{base}..{head}")),
                (Some(revision), None) | (None, Some(revision)) => arguments.push(revision),
                (None, None) => {}
            }
            arguments.push("--".into());
        }
        GitReadOperation::BranchMerged => {
            let base = required_revision(operation, base, "base")?;
            arguments.extend([
                "branch".into(),
                "--list".into(),
                "--format=%(refname:short)".into(),
                "--merged".into(),
                base,
            ]);
        }
        GitReadOperation::MergeBase => {
            let base = required_revision(operation, base, "base")?;
            let head = required_revision(operation, head, "head")?;
            arguments.extend(["merge-base".into(), base, head]);
        }
    }

    Ok(arguments)
}

fn optional_revision(
    operation: GitReadOperation,
    revision: Option<&str>,
) -> Result<Option<String>, ToolOutput> {
    revision
        .map(|revision| validated_revision(operation, revision))
        .transpose()
}

fn required_revision(
    operation: GitReadOperation,
    revision: Option<String>,
    field: &str,
) -> Result<String, ToolOutput> {
    revision.ok_or_else(|| failure(operation, &format!("{field} revision is required")))
}

/// Accepts only plain ref names. Revision expressions, ranges, options and
/// pathspecs are rejected instead of being escaped, because the argv positions
/// that accept them also accept flags.
fn validated_revision(operation: GitReadOperation, revision: &str) -> Result<String, ToolOutput> {
    let invalid = |reason: &str| failure(operation, &format!("revision {reason}"));

    if revision.is_empty() {
        return Err(invalid("must not be empty"));
    }
    if revision.len() > MAX_REVISION_CHARS {
        return Err(invalid("is too long"));
    }
    if revision.starts_with('-') || revision.starts_with('/') || revision.starts_with('.') {
        return Err(invalid("must start with a name character"));
    }
    if revision.ends_with('/') || revision.ends_with(".lock") {
        return Err(invalid("is not a valid ref name"));
    }
    if revision.contains("..") || revision.contains("//") {
        return Err(invalid("must not contain a range or an empty component"));
    }
    if !revision
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "._/-".contains(character))
    {
        return Err(invalid(
            "must only contain letters, digits, '.', '_', '/' or '-'",
        ));
    }

    Ok(revision.to_owned())
}

fn run_git(
    project_root: &std::path::Path,
    arguments: &[String],
    input: &GitReadInput,
) -> Result<ToolOutput, Error> {
    let operation = input.operation;
    let mut command = Command::new("git");

    command
        .args(HARDENING_ARGUMENTS)
        .args(arguments)
        .current_dir(project_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    harden_environment(&mut command);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    let Ok(mut child) = command.spawn() else {
        return Ok(failure(operation, "failed to start git"));
    };
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        let _ = terminate_process_group(&mut child);
        return Ok(failure(operation, "output setup failed"));
    };
    let stdout_reader = read_capped(stdout);
    let stderr_reader = read_capped(stderr);
    let deadline = Instant::now() + input.timeout;

    let status = loop {
        let cancelled = input
            .execution_context
            .as_ref()
            .is_some_and(ToolExecutionContext::is_cancelled);

        if cancelled || Instant::now() >= deadline {
            if terminate_process_group(&mut child).is_err() {
                return Ok(failure(operation, "process cleanup failed"));
            }
            if wait_for_readers(stdout_reader, stderr_reader).is_err() {
                return Ok(failure(operation, "process cleanup failed"));
            }
            if cancelled {
                return Err(Error::Cancelled);
            }
            return Ok(failure(
                operation,
                &format!("timed out after {}ms", input.timeout.as_millis()),
            ));
        }

        let status = match child.try_wait() {
            Ok(status) => status,
            Err(_) => {
                let _ = terminate_process_group(&mut child);
                let _ = wait_for_readers(stdout_reader, stderr_reader);
                return Ok(failure(operation, "wait failed"));
            }
        };
        if let Some(status) = status {
            if kill_process_group(child.id()).is_err() {
                return Ok(failure(operation, "process cleanup failed"));
            }
            break status;
        }

        thread::sleep(PROCESS_POLL_INTERVAL);
    };

    let output = wait_for_readers(stdout_reader, stderr_reader)
        .map_err(|_| Error::Tool("git_read: output reader failed".into()))?;
    let rendered = render_streams(&output);

    if status.success() {
        Ok(ToolOutput::success(rendered))
    } else {
        Ok(ToolOutput::failure(format!(
            "git_read {}: git exited with {}\n{rendered}",
            operation.label(),
            output_status(status.code())
        )))
    }
}

/// Options that hold the read to a read, each one measured against the
/// behaviour it is supposed to stop: without `--no-optional-locks`, `status`
/// rewrites the index whenever it refreshes stat information, and without
/// `core.fsmonitor` emptied, `status` executes the program the repository
/// configured. `diff.external` is *not* neutralized here: emptying it makes git
/// try to execute the empty string rather than skip the external diff, so the
/// diff argv carries `--no-ext-diff` instead.
const HARDENING_ARGUMENTS: [&str; 4] =
    ["--no-pager", "--no-optional-locks", "-c", "core.fsmonitor="];

fn harden_environment(command: &mut Command) {
    for variable in [
        "GIT_EXTERNAL_DIFF",
        "GIT_PAGER",
        "PAGER",
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_ASKPASS",
        "SSH_ASKPASS",
        "GIT_SSH",
        "GIT_SSH_COMMAND",
        "GIT_CONFIG",
        "GIT_CONFIG_COUNT",
    ] {
        command.env_remove(variable);
    }

    command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0");
}

fn render_streams(output: &CappedOutput) -> String {
    let stdout = lossy_bash_stream(&output.stdout);
    let stderr = lossy_bash_stream(&output.stderr);
    let truncated = if output.truncated {
        "[git_read output truncated]\n"
    } else {
        ""
    };

    format!("{truncated}[stdout]\n{stdout}[stderr]\n{stderr}")
}

fn output_status(code: Option<i32>) -> String {
    code.map(|code| code.to_string())
        .unwrap_or_else(|| "a signal".into())
}

fn failure(operation: GitReadOperation, detail: &str) -> ToolOutput {
    ToolOutput::failure(format!("git_read {}: {detail}", operation.label()))
}
