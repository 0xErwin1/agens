use std::{
    fs,
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use agens_core::Error;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_PROCESS_OUTPUT: usize = 64 * 1024;
const DEFAULT_BASH_TIMEOUT: Duration = Duration::from_secs(120);
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(10);
const DEFAULT_MAX_LIST_ENTRIES: usize = 1_000;
const DEFAULT_MAX_SEARCH_ENTRIES: usize = 10_000;
const DEFAULT_MAX_SEARCH_RESULTS: usize = 100;
const DEFAULT_MAX_SEARCH_DEPTH: usize = 32;
const DEFAULT_FILE_OPERATION_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn success(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    pub fn failure(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadFileInput {
    path: PathBuf,
}

impl ReadFileInput {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteFileInput {
    path: PathBuf,
    content: String,
}

impl WriteFileInput {
    pub fn new(path: impl Into<PathBuf>, content: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            content: content.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListDirectoryInput {
    path: PathBuf,
}

impl ListDirectoryInput {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchInput {
    path: PathBuf,
    query: String,
}

impl SearchInput {
    pub fn new(path: impl Into<PathBuf>, query: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            query: query.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeToolLimits {
    pub max_list_entries: usize,
    pub max_search_entries: usize,
    pub max_search_results: usize,
    pub max_search_depth: usize,
    pub operation_timeout: Duration,
}

impl Default for NativeToolLimits {
    fn default() -> Self {
        Self {
            max_list_entries: DEFAULT_MAX_LIST_ENTRIES,
            max_search_entries: DEFAULT_MAX_SEARCH_ENTRIES,
            max_search_results: DEFAULT_MAX_SEARCH_RESULTS,
            max_search_depth: DEFAULT_MAX_SEARCH_DEPTH,
            operation_timeout: DEFAULT_FILE_OPERATION_TIMEOUT,
        }
    }
}

#[derive(Clone, Debug)]
pub struct BashInput {
    command: String,
    timeout: Duration,
    cancellation: Option<Arc<AtomicBool>>,
}

impl BashInput {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            timeout: DEFAULT_BASH_TIMEOUT,
            cancellation: None,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_cancellation(mut self, cancellation: Arc<AtomicBool>) -> Self {
        self.cancellation = Some(cancellation);
        self
    }
}

#[derive(Debug)]
pub struct NativeTools {
    project_root: PathBuf,
    limits: NativeToolLimits,
    #[cfg(unix)]
    project_root_dir: fs::File,
}

impl NativeTools {
    pub fn open(project_root: impl AsRef<Path>) -> Result<Self, Error> {
        Self::open_with_limits(project_root, NativeToolLimits::default())
    }

    pub fn open_with_limits(
        project_root: impl AsRef<Path>,
        limits: NativeToolLimits,
    ) -> Result<Self, Error> {
        validate_limits(&limits)?;
        let project_root = fs::canonicalize(project_root)
            .map_err(|error| Error::Tool(format!("cannot resolve project root: {error}")))?;

        if !project_root.is_dir() {
            return Err(Error::Tool("project root is not a directory".into()));
        }

        Ok(Self {
            #[cfg(unix)]
            project_root_dir: fs::File::open(&project_root)
                .map_err(|error| Error::Tool(format!("cannot open project root: {error}")))?,
            project_root,
            limits,
        })
    }

    pub fn read_file(&self, input: ReadFileInput) -> Result<ToolOutput, Error> {
        let path = match self.resolve_existing(&input.path) {
            Ok(path) => path,
            Err(output) => return Ok(output),
        };

        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => return Ok(ToolOutput::failure(format!("read: {error}"))),
        };

        if !metadata.is_file() {
            return Ok(ToolOutput::failure("read: path is not a file"));
        }

        if metadata.len() > MAX_FILE_BYTES {
            return Ok(ToolOutput::failure("read: file exceeds 1048576 byte limit"));
        }

        match fs::read_to_string(path) {
            Ok(content) => Ok(ToolOutput::success(content)),
            Err(error) => Ok(ToolOutput::failure(format!("read: {error}"))),
        }
    }

    pub fn write_file(&self, input: WriteFileInput) -> Result<ToolOutput, Error> {
        if let Err(output) = self.validate_relative(&input.path) {
            return Ok(output);
        }

        #[cfg(unix)]
        let result = write_file_confined(
            &self.project_root_dir,
            &input.path,
            input.content.as_bytes(),
        );

        #[cfg(not(unix))]
        let result = Err(ToolOutput::failure(
            "write: secure confined writes are unavailable on this platform",
        ));

        match result {
            Ok(()) => Ok(ToolOutput::success(format!(
                "wrote {}",
                input.path.display()
            ))),
            Err(output) => Ok(output),
        }
    }

    pub fn list_directory(&self, input: ListDirectoryInput) -> Result<ToolOutput, Error> {
        let path = match self.resolve_existing(&input.path) {
            Ok(path) => path,
            Err(output) => return Ok(output),
        };

        if !path.is_dir() {
            return Ok(ToolOutput::failure("list: path is not a directory"));
        }

        let deadline = Instant::now() + self.limits.operation_timeout;
        let directory = match fs::read_dir(path) {
            Ok(directory) => directory,
            Err(error) => return Ok(ToolOutput::failure(format!("list: {error}"))),
        };
        let mut entries = Vec::new();

        for entry in directory {
            if Instant::now() >= deadline {
                return Ok(ToolOutput::failure("list: operation timed out"));
            }
            if entries.len() == self.limits.max_list_entries {
                return Ok(ToolOutput::failure(format!(
                    "list: entry limit of {} exceeded",
                    self.limits.max_list_entries
                )));
            }

            let entry = entry.map_err(|error| Error::Tool(format!("list: {error}")))?;
            entries.push(entry.file_name().to_string_lossy().into_owned());
        }
        entries.sort();

        Ok(ToolOutput::success(entries.join("\n") + "\n"))
    }

    pub fn search(&self, input: SearchInput) -> Result<ToolOutput, Error> {
        if input.query.is_empty() {
            return Ok(ToolOutput::failure("search: query is required"));
        }

        let path = match self.resolve_existing(&input.path) {
            Ok(path) => path,
            Err(output) => return Ok(output),
        };

        if !path.is_dir() {
            return Ok(ToolOutput::failure("search: path is not a directory"));
        }

        let mut results = Vec::new();
        let mut budget = SearchBudget::new(&self.limits);
        if let Err(output) =
            self.search_directory(&path, &input.query, 0, &mut budget, &mut results)
        {
            return Ok(output);
        }

        Ok(ToolOutput::success(results.join("")))
    }

    pub fn bash(&self, input: BashInput) -> Result<ToolOutput, Error> {
        if input.command.trim().is_empty() {
            return Ok(ToolOutput::failure("bash: command is required"));
        }

        if input.timeout.is_zero() {
            return Ok(ToolOutput::failure(
                "bash: timeout must be greater than zero",
            ));
        }

        let output = Arc::new(Mutex::new(CappedOutput::default()));
        let mut command = Command::new("bash");
        command
            .arg("-c")
            .arg(&input.command)
            .current_dir(&self.project_root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        #[cfg(unix)]
        command.process_group(0);

        let mut child = command
            .spawn()
            .map_err(|error| Error::Tool(format!("bash: failed to start: {error}")))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Tool("bash: stdout pipe unavailable".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| Error::Tool("bash: stderr pipe unavailable".into()))?;
        let stdout_reader = read_capped(stdout, Arc::clone(&output));
        let stderr_reader = read_capped(stderr, Arc::clone(&output));
        let deadline = Instant::now() + input.timeout;

        let status = loop {
            if input
                .cancellation
                .as_ref()
                .is_some_and(|cancellation| cancellation.load(Ordering::Acquire))
            {
                terminate_process_group(&mut child)?;
                wait_for_readers(stdout_reader, stderr_reader)?;
                return Ok(ToolOutput::failure("bash: cancelled"));
            }

            if Instant::now() >= deadline {
                terminate_process_group(&mut child)?;
                wait_for_readers(stdout_reader, stderr_reader)?;
                return Ok(ToolOutput::failure(format!(
                    "bash: timed out after {}ms",
                    input.timeout.as_millis()
                )));
            }

            if let Some(status) = child
                .try_wait()
                .map_err(|error| Error::Tool(format!("bash: wait failed: {error}")))?
            {
                kill_process_group(child.id())?;
                wait_for_readers(stdout_reader, stderr_reader)?;
                break status;
            }

            thread::sleep(PROCESS_POLL_INTERVAL);
        };

        let output = output
            .lock()
            .map_err(|_| Error::Tool("bash: output collector unavailable".into()))?
            .render();

        if status.success() {
            return Ok(ToolOutput::success(if output.is_empty() {
                "(no output; exit status 0)".into()
            } else {
                output
            }));
        }

        Ok(ToolOutput::failure(format!(
            "{output}bash: exit status: {}",
            exit_code(status)
        )))
    }

    fn resolve_existing(&self, path: &Path) -> Result<PathBuf, ToolOutput> {
        self.validate_relative(path)?;

        let path = fs::canonicalize(self.project_root.join(path))
            .map_err(|error| ToolOutput::failure(format!("path: {error}")))?;

        if path.starts_with(&self.project_root) {
            Ok(path)
        } else {
            Err(ToolOutput::failure("path: outside project root"))
        }
    }

    fn search_directory(
        &self,
        directory: &Path,
        query: &str,
        depth: usize,
        budget: &mut SearchBudget,
        results: &mut Vec<String>,
    ) -> Result<(), ToolOutput> {
        let directory_entries = fs::read_dir(directory)
            .map_err(|error| ToolOutput::failure(format!("search: {error}")))?;
        let mut entries = Vec::new();

        for entry in directory_entries {
            budget.consume_entry()?;
            entries.push(entry.map_err(|error| ToolOutput::failure(format!("search: {error}")))?);
        }
        entries.sort_by_key(|entry| entry.file_name());

        for entry in entries {
            budget.check_deadline()?;

            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)
                .map_err(|error| ToolOutput::failure(format!("search: {error}")))?;

            if metadata.file_type().is_symlink() {
                continue;
            }

            if metadata.is_dir() {
                let next_depth = depth + 1;
                if next_depth > self.limits.max_search_depth {
                    return Err(ToolOutput::failure(format!(
                        "search: traversal depth limit of {} exceeded",
                        self.limits.max_search_depth
                    )));
                }
                self.search_directory(&path, query, next_depth, budget, results)?;
                continue;
            }

            if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
                continue;
            }

            let content = fs::read_to_string(&path)
                .map_err(|error| ToolOutput::failure(format!("search: {error}")))?;
            let relative = path
                .strip_prefix(&self.project_root)
                .map_err(|_| ToolOutput::failure("path: outside project root"))?;

            for (line, text) in content.lines().enumerate() {
                budget.check_deadline()?;
                if text.contains(query) {
                    if results.len() == self.limits.max_search_results {
                        return Err(ToolOutput::failure(format!(
                            "search: result limit of {} exceeded",
                            self.limits.max_search_results
                        )));
                    }
                    results.push(format!("{}:{}:{text}\n", relative.display(), line + 1));
                }
            }
        }

        Ok(())
    }

    fn validate_relative(&self, path: &Path) -> Result<(), ToolOutput> {
        if path.as_os_str().is_empty() || path.is_absolute() {
            return Err(ToolOutput::failure(
                "path: must be a non-empty relative path",
            ));
        }

        if path
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
        {
            Ok(())
        } else {
            Err(ToolOutput::failure("path: traversal is not allowed"))
        }
    }
}

fn validate_limits(limits: &NativeToolLimits) -> Result<(), Error> {
    if limits.max_list_entries == 0
        || limits.max_search_entries == 0
        || limits.max_search_results == 0
        || limits.operation_timeout.is_zero()
    {
        return Err(Error::Tool(
            "native tool limits must be greater than zero".into(),
        ));
    }

    Ok(())
}

struct SearchBudget {
    deadline: Instant,
    entries_seen: usize,
    max_entries: usize,
}

impl SearchBudget {
    fn new(limits: &NativeToolLimits) -> Self {
        Self {
            deadline: Instant::now() + limits.operation_timeout,
            entries_seen: 0,
            max_entries: limits.max_search_entries,
        }
    }

    fn check_deadline(&self) -> Result<(), ToolOutput> {
        if Instant::now() >= self.deadline {
            return Err(ToolOutput::failure("search: operation timed out"));
        }

        Ok(())
    }

    fn consume_entry(&mut self) -> Result<(), ToolOutput> {
        self.check_deadline()?;
        if self.entries_seen == self.max_entries {
            return Err(ToolOutput::failure(format!(
                "search: entry limit of {} exceeded",
                self.max_entries
            )));
        }

        self.entries_seen += 1;
        Ok(())
    }
}

#[cfg(unix)]
fn write_file_confined(
    project_root: &fs::File,
    path: &Path,
    content: &[u8],
) -> Result<(), ToolOutput> {
    use std::{
        ffi::CString,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::ffi::OsStrExt,
        },
    };

    let file_name = path
        .file_name()
        .ok_or_else(|| ToolOutput::failure("write: path must name a file"))?;
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    let mut directory = project_root
        .try_clone()
        .map_err(|error| ToolOutput::failure(format!("write: {error}")))?;

    // Each component is opened beneath an already-open directory descriptor, so renames and
    // symlink substitutions cannot redirect the final open outside the canonical root.
    for component in parent.components() {
        let Component::Normal(component) = component else {
            continue;
        };
        let component = CString::new(component.as_bytes())
            .map_err(|_| ToolOutput::failure("write: invalid path component"))?;
        let descriptor = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                component.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if descriptor < 0 {
            let error = io::Error::last_os_error();
            return Err(write_open_error(error, true));
        }
        directory = unsafe { fs::File::from_raw_fd(descriptor) };
    }

    let file_name = CString::new(file_name.as_bytes())
        .map_err(|_| ToolOutput::failure("write: invalid path component"))?;
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            file_name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            0o666,
        )
    };
    if descriptor < 0 {
        return Err(write_open_error(io::Error::last_os_error(), false));
    }

    let mut file = unsafe { fs::File::from_raw_fd(descriptor) };
    let metadata = file
        .metadata()
        .map_err(|error| ToolOutput::failure(format!("write: {error}")))?;
    if !metadata.is_file() {
        return Err(ToolOutput::failure("write: path is not a regular file"));
    }
    use std::os::unix::fs::MetadataExt;
    if metadata.nlink() > 1 {
        return Err(ToolOutput::failure("write: path has multiple hard links"));
    }

    file.set_len(0)
        .map_err(|error| ToolOutput::failure(format!("write: {error}")))?;
    file.write_all(content)
        .map_err(|error| ToolOutput::failure(format!("write: {error}")))
}

#[cfg(unix)]
fn write_open_error(error: io::Error, parent: bool) -> ToolOutput {
    if error.raw_os_error() == Some(libc::ELOOP)
        || (parent && error.kind() == io::ErrorKind::NotADirectory)
    {
        return ToolOutput::failure("path: outside project root");
    }
    if parent && error.kind() == io::ErrorKind::NotFound {
        return ToolOutput::failure("write: parent directory does not exist");
    }

    ToolOutput::failure(format!("write: {error}"))
}

#[derive(Default)]
struct CappedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

impl CappedOutput {
    fn append(&mut self, bytes: &[u8]) {
        let remaining = MAX_PROCESS_OUTPUT.saturating_sub(self.bytes.len());
        self.bytes
            .extend_from_slice(&bytes[..bytes.len().min(remaining)]);
        self.truncated |= bytes.len() > remaining;
    }

    fn render(&self) -> String {
        let mut output = String::from_utf8_lossy(&self.bytes).into_owned();
        if self.truncated {
            output.push_str("\n[bash output truncated]\n");
        }
        output
    }
}

fn read_capped(
    mut reader: impl Read + Send + 'static,
    output: Arc<Mutex<CappedOutput>>,
) -> thread::JoinHandle<Result<(), io::Error>> {
    thread::spawn(move || {
        let mut buffer = [0; 8192];
        loop {
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                return Ok(());
            }

            let mut output = output
                .lock()
                .map_err(|_| io::Error::other("output collector unavailable"))?;
            output.append(&buffer[..count]);
        }
    })
}

fn wait_for_readers(
    stdout_reader: thread::JoinHandle<Result<(), io::Error>>,
    stderr_reader: thread::JoinHandle<Result<(), io::Error>>,
) -> Result<(), Error> {
    for reader in [stdout_reader, stderr_reader] {
        reader
            .join()
            .map_err(|_| Error::Tool("bash: output reader panicked".into()))?
            .map_err(|error| Error::Tool(format!("bash: output reader failed: {error}")))?;
    }
    Ok(())
}

fn terminate_process_group(child: &mut std::process::Child) -> Result<(), Error> {
    kill_process_group(child.id())?;

    #[cfg(not(unix))]
    child
        .kill()
        .map_err(|error| Error::Tool(format!("bash: failed to terminate process: {error}")))?;

    child
        .wait()
        .map_err(|error| Error::Tool(format!("bash: wait failed: {error}")))?;
    Ok(())
}

fn kill_process_group(process_id: u32) -> Result<(), Error> {
    #[cfg(unix)]
    {
        let result = unsafe { libc::kill(-(process_id as i32), libc::SIGKILL) };
        if result != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(Error::Tool(format!(
                    "bash: failed to terminate process group: {error}"
                )));
            }
        }
    }

    Ok(())
}

fn exit_code(status: ExitStatus) -> String {
    status
        .code()
        .map_or_else(|| "terminated by signal".into(), |code| code.to_string())
}
