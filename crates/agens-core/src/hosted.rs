//! Transport-neutral contracts for capabilities owned by a hosted session.

use std::path::{Path, PathBuf};

pub const MAX_WORKSPACE_FILE_ENTRIES: usize = 2_000;
pub const MAX_WORKSPACE_FILE_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogKind {
    Command,
    Skill,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogEntry {
    name: String,
    description: String,
    built_in: bool,
}

impl CatalogEntry {
    #[must_use]
    pub fn new(name: impl Into<String>, description: impl Into<String>, built_in: bool) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            built_in,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn description(&self) -> &str {
        &self.description
    }
    pub const fn built_in(&self) -> bool {
        self.built_in
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogSnapshot {
    revision: String,
    entries: Vec<CatalogEntry>,
}

impl CatalogSnapshot {
    #[must_use]
    pub fn new(revision: impl Into<String>, entries: Vec<CatalogEntry>) -> Self {
        Self {
            revision: revision.into(),
            entries,
        }
    }

    pub fn revision(&self) -> &str {
        &self.revision
    }
    pub fn entries(&self) -> &[CatalogEntry] {
        &self.entries
    }

    #[must_use]
    pub fn resolve(&self, known_revision: Option<&str>) -> CatalogResult {
        match known_revision {
            Some(revision) if revision != self.revision => CatalogResult::Stale {
                current_revision: self.revision.clone(),
            },
            _ => CatalogResult::Current(self.clone()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogResult {
    Current(CatalogSnapshot),
    Stale { current_revision: String },
    Unsupported,
}

pub trait HostedCatalogs: Send + Sync {
    fn catalog(&self, kind: CatalogKind, known_revision: Option<&str>) -> CatalogResult;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceFileKind {
    Text,
    Media,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceFile {
    path: PathBuf,
    byte_len: u64,
    kind: WorkspaceFileKind,
}

impl WorkspaceFile {
    #[must_use]
    pub fn new(path: PathBuf, byte_len: u64, kind: WorkspaceFileKind) -> Self {
        Self {
            path,
            byte_len,
            kind,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
    pub const fn byte_len(&self) -> u64 {
        self.byte_len
    }
    pub const fn kind(&self) -> WorkspaceFileKind {
        self.kind
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceFileContent {
    Text {
        path: PathBuf,
        text: String,
    },
    Media {
        path: PathBuf,
        mime: String,
        bytes: Vec<u8>,
        media_id: Option<i64>,
        kind: WorkspaceFileKind,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileError {
    InvalidSelector,
    OutsideRoot,
    Ignored,
    Missing,
    Unsupported,
    Oversized,
    EntryLimit,
    Unreadable,
}

pub trait HostedWorkspaceFiles: Send + Sync {
    fn list(&self, root: &Path, selector: &Path) -> Result<Vec<WorkspaceFile>, FileError>;
    fn read(&self, root: &Path, selector: &Path) -> Result<WorkspaceFileContent, FileError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostedTaskLimits {
    event_records: usize,
    control_records: usize,
}

impl HostedTaskLimits {
    #[must_use]
    pub const fn with_limits(event_records: usize, control_records: usize) -> Self {
        Self {
            event_records,
            control_records,
        }
    }

    pub const fn event_records(self) -> usize {
        self.event_records
    }
    pub const fn control_records(self) -> usize {
        self.control_records
    }
}

impl Default for HostedTaskLimits {
    fn default() -> Self {
        Self::with_limits(10_000, 10_000)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostedTaskState {
    Running,
    Background,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostedTaskRecord {
    task_id: String,
    state: HostedTaskState,
}

impl HostedTaskRecord {
    #[must_use]
    pub fn new(task_id: impl Into<String>, state: HostedTaskState) -> Self {
        Self {
            task_id: task_id.into(),
            state,
        }
    }

    pub fn task_id(&self) -> &str {
        &self.task_id
    }
    pub const fn state(&self) -> HostedTaskState {
        self.state
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostedTaskEvent {
    cursor: u64,
    task_id: String,
    state: HostedTaskState,
    payload: String,
}

impl HostedTaskEvent {
    #[must_use]
    pub fn new(
        cursor: u64,
        task_id: impl Into<String>,
        state: HostedTaskState,
        payload: impl Into<String>,
    ) -> Self {
        Self {
            cursor,
            task_id: task_id.into(),
            state,
            payload: payload.into(),
        }
    }

    pub const fn cursor(&self) -> u64 {
        self.cursor
    }
    pub fn task_id(&self) -> &str {
        &self.task_id
    }
    pub const fn state(&self) -> HostedTaskState {
        self.state
    }
    pub fn payload(&self) -> &str {
        &self.payload
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostedTaskSnapshot {
    cursor: u64,
    tasks: Vec<HostedTaskRecord>,
    child_turns: Vec<HostedChildTurn>,
}

impl HostedTaskSnapshot {
    #[must_use]
    pub fn new(cursor: u64, tasks: Vec<HostedTaskRecord>) -> Self {
        Self {
            cursor,
            tasks,
            child_turns: Vec::new(),
        }
    }
    #[must_use]
    pub fn with_child_turns(mut self, child_turns: Vec<HostedChildTurn>) -> Self {
        self.child_turns = child_turns;
        self
    }
    pub fn child_turns(&self) -> &[HostedChildTurn] {
        &self.child_turns
    }
    pub const fn cursor(&self) -> u64 {
        self.cursor
    }
    pub fn tasks(&self) -> &[HostedTaskRecord] {
        &self.tasks
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostedTaskReplay {
    Events(Vec<HostedTaskEvent>),
    SnapshotTail {
        snapshot: HostedTaskSnapshot,
        events: Vec<HostedTaskEvent>,
    },
    Gap {
        oldest_cursor: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostedChildTurn {
    task_id: String,
    sequence: u64,
    payload: String,
}

impl HostedChildTurn {
    #[must_use]
    pub fn new(task_id: impl Into<String>, sequence: u64, payload: impl Into<String>) -> Self {
        Self {
            task_id: task_id.into(),
            sequence,
            payload: payload.into(),
        }
    }
    pub fn task_id(&self) -> &str {
        &self.task_id
    }
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }
    pub fn payload(&self) -> &str {
        &self.payload
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostedControlKind {
    Background,
    Cancel,
    CancelAll,
    Message(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostedControlCommand {
    session_id: i64,
    task_id: Option<String>,
    command_id: String,
    kind: HostedControlKind,
}

impl HostedControlCommand {
    #[must_use]
    pub fn new(
        session_id: i64,
        task_id: Option<String>,
        command_id: impl Into<String>,
        kind: HostedControlKind,
    ) -> Self {
        Self {
            session_id,
            task_id,
            command_id: command_id.into(),
            kind,
        }
    }
    pub const fn session_id(&self) -> i64 {
        self.session_id
    }
    pub fn task_id(&self) -> Option<&str> {
        self.task_id.as_deref()
    }
    pub fn command_id(&self) -> &str {
        &self.command_id
    }
    pub const fn kind(&self) -> &HostedControlKind {
        &self.kind
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostedControlResult {
    state: HostedTaskState,
    replayed: bool,
}

impl HostedControlResult {
    #[must_use]
    pub const fn new(state: HostedTaskState, replayed: bool) -> Self {
        Self { state, replayed }
    }
    pub const fn state(&self) -> HostedTaskState {
        self.state
    }
    pub const fn replayed(&self) -> bool {
        self.replayed
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskControlError {
    WrongSession,
    UnknownTask,
    InvalidTransition,
    CommandConflict,
    ControlCapacity,
    InvalidRequest,
    Storage,
}

pub trait HostedTaskJournal: Send {
    fn append_event(
        &mut self,
        session_id: i64,
        task_id: &str,
        state: HostedTaskState,
        payload: &str,
    ) -> Result<HostedTaskEvent, TaskControlError>;
    fn persist_completed_child_turn(
        &mut self,
        session_id: i64,
        task_id: &str,
        sequence: u64,
        payload: &str,
    ) -> Result<(), TaskControlError>;
    fn completed_child_turns(
        &self,
        session_id: i64,
    ) -> Result<Vec<HostedChildTurn>, TaskControlError>;
    fn snapshot_tail(&self, session_id: i64) -> Result<HostedTaskReplay, TaskControlError>;
    fn replay_after(
        &self,
        session_id: i64,
        after_cursor: u64,
    ) -> Result<HostedTaskReplay, TaskControlError>;
    fn apply_control(
        &mut self,
        command: &HostedControlCommand,
    ) -> Result<HostedControlResult, TaskControlError>;
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HostedMcpState {
    Disabled,
    #[default]
    Idle,
    Connecting,
    Ready,
    Degraded,
    Failed,
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostedMcpAction {
    Connect,
    Disconnect,
    Reconnect,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostedMcpServer {
    name: String,
    state: HostedMcpState,
    generation: u64,
    error: Option<String>,
}

impl HostedMcpServer {
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        state: HostedMcpState,
        generation: u64,
        error: Option<String>,
    ) -> Self {
        Self {
            name: name.into(),
            state,
            generation,
            error,
        }
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub const fn state(&self) -> HostedMcpState {
        self.state
    }
    pub const fn generation(&self) -> u64 {
        self.generation
    }
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostedMcpResult {
    servers: Vec<HostedMcpServer>,
    error: Option<String>,
}

impl HostedMcpResult {
    #[must_use]
    pub fn new(servers: Vec<HostedMcpServer>, error: Option<String>) -> Self {
        Self { servers, error }
    }
    pub fn servers(&self) -> &[HostedMcpServer] {
        &self.servers
    }
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

pub trait HostedMcpControl: Send + Sync {
    fn status(&self) -> Vec<HostedMcpServer>;
    fn control(&mut self, server: &str, action: HostedMcpAction) -> HostedMcpResult;
}

/// The authoritative hosted replies to a `/bypass` toggle. A client that
/// mirrors the footer state matches these exactly rather than inferring from
/// free text, so daemon and surface can never disagree about what was said.
pub const BYPASS_ON_REPLY: &str = "Permission bypass: on.";
pub const BYPASS_OFF_REPLY: &str = "Permission bypass: off.";
