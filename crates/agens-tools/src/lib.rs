use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    fmt, fs,
    io::{self, Read, Write},
    net::{IpAddr, ToSocketAddrs},
    panic::{self, AssertUnwindSafe, catch_unwind},
    path::{Component, Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use agens_core::mcp_failure::{McpFailure, McpFailureClass};
use agens_core::{
    EditMagnitude, Error, FactPath, HeadlessTaskTerminal, HeadlessTurnCancellationAdapter,
    PermissionAuthority, PermissionDecision, PermissionPolicy, PermissionReach,
    PermissionReadFilter, PermissionRequest, PermissionSession, ProjectPermissionGrant, ToolAccess,
    ToolOutcome, ToolResultFacts, WriteMagnitude,
};
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use regex::RegexBuilder;
use serde::Deserialize;
use serde::de::{self, DeserializeSeed, Deserializer, IgnoredAny, MapAccess, Visitor};
use serde_json::Value;

mod agents;
mod ask_user;
mod capabilities;
mod git_read;
mod http_mcp;
pub mod http_worker;
pub mod markdown;
mod mcp_status;
mod stdio_mcp;
mod task;
mod working_directory;
mod worktrees;

pub use agens_core::{TaskProviderFailure, TaskSkillRejection};
pub use agents::{
    AgentCatalog, AgentDiagnostic, AgentDiscovery, AgentModelValidationError, AgentModelValidator,
    AgentShadow,
};
pub use ask_user::AskUserTool;
pub use capabilities::{EffectiveCapabilityDescriptor, EffectiveCapabilitySet};
pub use git_read::{GitReadInput, GitReadOperation};
pub use http_mcp::{McpHttpTransport, McpSseTransport};
pub use mcp_status::{
    MAX_MCP_STATUS_TOOL_NAMES, McpEndpointSummary, McpErrorCategory, McpLifecycleState,
    McpLoadPhase, McpServerDescriptor, McpServerSource, McpServerStatus, McpServerTransport,
    McpStatusError, McpStatusHandle, McpStatusSnapshot,
};
pub use stdio_mcp::{MAX_MCP_FRAME_BYTES, McpStdioTransport, McpStdioTransportConfig};
pub use task::{
    TaskControlAction, TaskControlTool, TaskDeclarationRejection, TaskExecutionEvent,
    TaskExecutionId, TaskExecutionLifecycle, TaskExecutionLimits, TaskExecutionRegistry,
    TaskExecutionSnapshot, TaskInvocation, TaskLaunchMode, TaskMessage, TaskMessageSource,
    TaskMessageTarget, TaskMessageTool, TaskModelResolutionError, TaskRegistryError,
    TaskRunContext, TaskRunner, TaskRunnerError, TaskSkill, TaskTerminalState, TaskTool,
    TaskTurnRequest, TaskTurnResult,
};
pub use working_directory::{WorkingDirectory, WorkingDirectoryObserver};
pub use worktrees::{SessionWorktrees, WorktreeError, WorktreeStatus};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_PROCESS_OUTPUT: usize = 64 * 1024;
/// The share of an MCP tool result that reaches the model, on the same budget
/// native process output already holds.
const MAX_MCP_TOOL_OUTPUT: usize = 64 * 1024;
const MCP_TRUNCATED_MARKER: &str = "\n[mcp output truncated]";
const MAX_CAPTURED_PROCESS_BYTES: usize = MAX_PROCESS_OUTPUT - 128;
const DEFAULT_BASH_TIMEOUT: Duration = Duration::from_secs(2 * 60);
const MAX_BASH_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const DEFAULT_WEBFETCH_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_WEBFETCH_BYTES: usize = 100 * 1024;
const MAX_WEBFETCH_REDIRECTS: usize = 5;
const WEBFETCH_TRUNCATED_MARKER: &str = "\n[webfetch output truncated]";
/// Told to the caller when a search walked into files a rule refuses to let it
/// report, so the result is not silently passed off as the whole corpus.
///
/// It carries no path and no count deliberately. Either would answer questions
/// about the files being withheld, and a caller that can re-root the same
/// search could turn a count into a way of locating them.
const WITHHELD_FILES_NOTICE: &str =
    "[some files were not read: a permission rule denies reading them]\n";
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(10);
const DEFAULT_MAX_LIST_ENTRIES: usize = 1_000;
const DEFAULT_MAX_SEARCH_ENTRIES: usize = 10_000;
const DEFAULT_MAX_SEARCH_RESULTS: usize = 100;
const DEFAULT_MAX_SEARCH_DEPTH: usize = 32;
const DEFAULT_FILE_OPERATION_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_MAX_MCP_LIST_PAGES: usize = 128;
const DEFAULT_MAX_MCP_TOOLS: usize = 1_000;
const SKILL_MANIFEST_NAME: &str = "SKILL.md";
const MAX_SKILL_DIRECTORIES_PER_ROOT: usize = 128;
const MAX_SKILL_ROOT_ENTRIES: usize = 1_024;
const MAX_SKILL_MANIFEST_BYTES: u64 = 256 * 1024;
const MAX_SKILL_RESOURCE_BYTES: u64 = 256 * 1024;
const MAX_SKILL_NAME_CHARS: usize = 64;
const MAX_SKILL_DESCRIPTION_CHARS: usize = 1_024;
const DEFAULT_MAX_SUBAGENT_CONCURRENCY: usize = 4;
const DEFAULT_MAX_SUBAGENT_INPUT_CHARS: usize = 16 * 1024;
const DEFAULT_MAX_SUBAGENT_OUTPUT_CHARS: usize = 64 * 1024;
const DEFAULT_SUBAGENT_TIMEOUT: Duration = Duration::from_secs(30);
const SUBAGENT_RESULT_POLL_INTERVAL: Duration = Duration::from_millis(1);

#[cfg(unix)]
use std::sync::atomic::AtomicUsize;

#[cfg(unix)]
static TEMP_FILE_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

#[cfg(all(test, unix))]
#[derive(Clone, Copy, PartialEq, Eq)]
enum EditTestHookPoint {
    BeforeTargetRecheck,
    BeforeRename,
}

#[cfg(all(test, unix))]
type EditTestHook = Box<dyn FnOnce(&fs::File, &std::ffi::CString) + Send>;

#[cfg(all(test, unix))]
static EDIT_TEST_HOOK: Mutex<Option<(EditTestHookPoint, EditTestHook)>> = Mutex::new(None);

#[cfg(all(test, unix))]
static GREP_TEST_HOOK: Mutex<Option<Box<dyn FnOnce() + Send>>> = Mutex::new(None);

#[cfg(all(test, unix))]
fn set_grep_test_hook(hook: impl FnOnce() + Send + 'static) {
    *GREP_TEST_HOOK.lock().unwrap() = Some(Box::new(hook));
}

#[cfg(all(test, unix))]
fn run_grep_test_hook() {
    let hook = GREP_TEST_HOOK.lock().unwrap().take();
    if let Some(hook) = hook {
        hook();
    }
}

#[cfg(all(test, unix))]
fn set_edit_test_hook(
    point: EditTestHookPoint,
    hook: impl FnOnce(&fs::File, &std::ffi::CString) + Send + 'static,
) {
    *EDIT_TEST_HOOK.lock().unwrap() = Some((point, Box::new(hook)));
}

#[cfg(all(test, unix))]
fn run_edit_test_hook(
    point: EditTestHookPoint,
    directory: &fs::File,
    temp_name: &std::ffi::CString,
) {
    let hook = {
        let mut hook = EDIT_TEST_HOOK.lock().unwrap();
        hook.take_if(|(expected, _)| *expected == point)
    };
    if let Some((_, hook)) = hook {
        hook(directory, temp_name);
    }
}

static SUBAGENT_PANIC_HOOK_INSTALLED: OnceLock<()> = OnceLock::new();

thread_local! {
    pub(crate) static IS_SUBAGENT_WORKER: Cell<bool> = const { Cell::new(false) };
}

/// Suppress only worker panic payloads because they can contain provider secrets.
pub(crate) fn install_subagent_panic_hook() {
    SUBAGENT_PANIC_HOOK_INSTALLED.get_or_init(|| {
        let default_hook = panic::take_hook();
        panic::set_hook(Box::new(move |panic_info| {
            if !IS_SUBAGENT_WORKER.with(Cell::get) {
                default_hook(panic_info);
            }
        }));
    });
}

#[derive(Clone, Debug)]
pub struct Skill {
    name: String,
    description: String,
    source: PathBuf,
    directory: PathBuf,
    root_directory_descriptor: Arc<fs::File>,
    directory_name: std::ffi::OsString,
}

impl PartialEq for Skill {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.description == other.description
            && self.source == other.source
            && self.directory == other.directory
    }
}

impl Eq for Skill {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkillResourceClass {
    Reference,
    Script,
    Asset,
}

impl SkillResourceClass {
    const fn directory(self) -> &'static str {
        match self {
            Self::Reference => "references",
            Self::Script => "scripts",
            Self::Asset => "assets",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "reference" => Some(Self::Reference),
            "script" => Some(Self::Script),
            "asset" => Some(Self::Asset),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandDefinition {
    name: String,
    description: String,
    template: String,
    busy_policy: CommandBusyPolicy,
    built_in: bool,
}

/// Describes how a catalogued command behaves while a TUI provider turn is active.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandBusyPolicy {
    Local,
    ProviderTurn,
    IdleOnly,
    Quit,
    Invalid,
}

impl CommandDefinition {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        template: impl Into<String>,
    ) -> Result<Self, String> {
        let name = name.into();
        markdown::canonical_filename(&name)?;

        let description = description.into().trim().to_owned();
        if description.is_empty() {
            return Err("command description is required".into());
        }

        let template = template.into().trim().to_owned();
        if template.is_empty() {
            return Err("command markdown body is required".into());
        }

        Ok(Self {
            name,
            description,
            template,
            busy_policy: CommandBusyPolicy::ProviderTurn,
            built_in: false,
        })
    }

    pub fn builtin(
        name: impl Into<String>,
        description: impl Into<String>,
        template: impl Into<String>,
        busy_policy: CommandBusyPolicy,
    ) -> Result<Self, String> {
        let mut command = Self::new(name, description, template)?;
        command.busy_policy = busy_policy;
        command.built_in = true;
        Ok(command)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn busy_policy(&self) -> CommandBusyPolicy {
        self.busy_policy
    }

    pub fn is_builtin(&self) -> bool {
        self.built_in
    }

    pub fn expand(&self, arguments: &str) -> String {
        self.template.replace("$ARGUMENTS", arguments.trim())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommandCatalog {
    commands: Vec<CommandDefinition>,
    positions: BTreeMap<String, usize>,
}

impl CommandCatalog {
    pub fn discover(
        built_ins: &[CommandDefinition],
        global_root: impl AsRef<Path>,
        project_root: impl AsRef<Path>,
    ) -> Result<CommandDiscovery, String> {
        let global_root = global_root.as_ref();
        let project_root = project_root.as_ref();
        let global = load_command_root(global_root)?;
        let project = load_command_root(project_root)?;
        let mut catalog = Self::default();
        let mut diagnostics = global.diagnostics;
        let mut shadowed = Vec::new();
        let reserved = built_ins
            .iter()
            .map(|command| command.name.clone())
            .collect::<BTreeSet<_>>();

        for command in built_ins {
            let mut command = command.clone();
            command.built_in = true;
            catalog.insert(command);
        }
        for command in global.commands {
            if catalog.command(command.name()).is_some() {
                shadowed.push(command.name.clone());
            }
            if reserved.contains(command.name()) {
                continue;
            }
            catalog.insert(command);
        }

        diagnostics.extend(project.diagnostics);
        for command in project.commands {
            if catalog.command(command.name()).is_some() {
                shadowed.push(command.name.clone());
            }
            if reserved.contains(command.name()) {
                continue;
            }
            catalog.insert(command);
        }

        Ok(CommandDiscovery {
            catalog,
            diagnostics,
            shadowed,
        })
    }

    pub fn len(&self) -> usize {
        self.commands.len()
    }

    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    pub fn command(&self, name: &str) -> Option<&CommandDefinition> {
        self.positions
            .get(name)
            .map(|position| &self.commands[*position])
    }

    pub fn iter(&self) -> std::slice::Iter<'_, CommandDefinition> {
        self.commands.iter()
    }

    fn insert(&mut self, command: CommandDefinition) {
        if let Some(position) = self.positions.get(command.name()).copied() {
            self.commands[position] = command;
            return;
        }

        self.positions
            .insert(command.name.clone(), self.commands.len());
        self.commands.push(command);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandDiscovery {
    catalog: CommandCatalog,
    diagnostics: Vec<CommandDiagnostic>,
    shadowed: Vec<String>,
}

impl CommandDiscovery {
    pub fn catalog(&self) -> &CommandCatalog {
        &self.catalog
    }

    pub fn diagnostics(&self) -> &[CommandDiagnostic] {
        &self.diagnostics
    }

    pub fn shadowed(&self) -> &[String] {
        &self.shadowed
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandDiagnostic {
    path: PathBuf,
    message: String,
}

impl CommandDiagnostic {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

#[derive(Default)]
struct CommandRootLoad {
    commands: Vec<CommandDefinition>,
    diagnostics: Vec<CommandDiagnostic>,
}

fn load_command_root(root: &Path) -> Result<CommandRootLoad, String> {
    if matches!(fs::symlink_metadata(root), Err(error) if error.kind() == io::ErrorKind::NotFound) {
        return Ok(CommandRootLoad::default());
    }

    let root =
        markdown::load_root_with_definition_limit(root, markdown::MAX_MARKDOWN_ROOT_ENTRIES)?;
    let mut commands = Vec::new();
    let mut diagnostics = root
        .diagnostics
        .into_iter()
        .map(|diagnostic| CommandDiagnostic {
            path: diagnostic.path().to_owned(),
            message: diagnostic.message().to_owned(),
        })
        .collect::<Vec<_>>();

    let mut definition_limit_reported = false;
    for document in root.documents {
        let description = document
            .parsed()
            .field("description")
            .and_then(|field| match field {
                markdown::FrontmatterValue::Scalar(value) => Some(value),
                markdown::FrontmatterValue::List(_) => None,
            });
        match description {
            Some(description) => {
                match CommandDefinition::new(document.name(), description, document.parsed().body())
                {
                    Ok(command) if commands.len() < markdown::MAX_MARKDOWN_DEFINITIONS => {
                        commands.push(command);
                    }
                    Ok(_) if !definition_limit_reported => {
                        diagnostics.push(CommandDiagnostic {
                            path: document.source().to_owned(),
                            message: "accepted definition limit exceeded".into(),
                        });
                        definition_limit_reported = true;
                    }
                    Ok(_) => {}
                    Err(message) => diagnostics.push(CommandDiagnostic {
                        path: document.source().to_owned(),
                        message,
                    }),
                }
            }
            None => diagnostics.push(CommandDiagnostic {
                path: document.source().to_owned(),
                message: "command description is required".into(),
            }),
        }
    }

    Ok(CommandRootLoad {
        commands,
        diagnostics,
    })
}

impl Skill {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn load_instructions(&self) -> Result<String, String> {
        let directory = self.open_current_directory()?;
        let contents = self.load_current_manifest(&directory)?;

        split_skill_frontmatter(&contents).map(|(_, body)| body.trim().to_owned())
    }

    pub fn load_resource(&self, class: SkillResourceClass, name: &str) -> Result<String, String> {
        if !is_normal_filename(name) {
            return Err("skill resource name must be a single normal filename".into());
        }

        let directory = self.open_current_directory()?;
        self.load_current_manifest(&directory)?;

        let class_directory =
            open_child_directory(&directory, std::ffi::OsStr::new(class.directory()))?
                .ok_or("skill resource class is unavailable")?;
        let mut resource = open_child_file(&class_directory, std::ffi::OsStr::new(name))?
            .ok_or("skill resource is unavailable")?;
        let metadata = resource
            .metadata()
            .map_err(|error| format!("cannot inspect skill resource: {error}"))?;
        ensure_single_link_regular_file(
            &metadata,
            "skill resource must be a regular non-symbolic-link file",
        )?;
        if metadata.len() > MAX_SKILL_RESOURCE_BYTES {
            return Err(format!(
                "skill resource exceeds {MAX_SKILL_RESOURCE_BYTES} byte limit"
            ));
        }

        read_bounded_utf8(&mut resource, MAX_SKILL_RESOURCE_BYTES)
    }

    /// Opens the current skill directory through the audited root descriptor.
    /// This observes child-directory replacements without reopening a root or
    /// ancestor pathname that may have been redirected after discovery.
    fn open_current_directory(&self) -> Result<fs::File, String> {
        open_child_directory(&self.root_directory_descriptor, &self.directory_name)?
            .ok_or_else(|| "skill directory is unavailable".into())
    }

    fn load_current_manifest(&self, directory: &fs::File) -> Result<String, String> {
        let mut manifest = open_manifest(directory)?.ok_or("skill instructions are unavailable")?;
        let metadata = manifest
            .metadata()
            .map_err(|error| format!("cannot inspect opened manifest: {error}"))?;
        ensure_single_link_regular_file(&metadata, "manifest must be a single-link regular file")?;
        let contents = read_bounded_utf8(&mut manifest, MAX_SKILL_MANIFEST_BYTES)?;

        let current = parse_skill_manifest(
            &self.source,
            &self.directory,
            Arc::clone(&self.root_directory_descriptor),
            self.directory_name.clone(),
            &contents,
        )?;
        if current.name != self.name {
            return Err("skill manifest name changed after discovery".into());
        }

        Ok(contents)
    }

    pub fn source(&self) -> &Path {
        &self.source
    }

    /// The directory holding this skill's manifest and its resource classes.
    /// Every file the skill can return is a child of it.
    pub fn directory(&self) -> &Path {
        &self.directory
    }
}

/// Where a project's own skills live, relative to that project's root.
///
/// A catalog records the directory it discovered them in, while a holder of that
/// catalog is handed a project root. The two are only paired with each other
/// when they agree through this, which is what makes the pairing checkable at
/// all rather than merely conventional.
pub const PROJECT_SKILLS_DIRECTORY: &str = ".agens/skills";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SkillCatalog {
    skills: Vec<Skill>,
    positions: BTreeMap<String, usize>,
    /// The root the project half of this catalog was discovered under, kept so
    /// a holder can tell a project skill from a global one by where it came
    /// from rather than by guessing from its path. Absent only from a catalog
    /// that discovered nothing.
    project_root: Option<PathBuf>,
}

impl SkillCatalog {
    pub fn discover(
        global_root: impl AsRef<Path>,
        project_root: impl AsRef<Path>,
    ) -> Result<SkillDiscovery, SkillDiscoveryError> {
        let global = load_skill_root(global_root.as_ref())?;
        let project = load_skill_root(project_root.as_ref())?;
        let mut catalog = Self {
            project_root: Some(project_root.as_ref().to_path_buf()),
            ..Self::default()
        };
        let mut diagnostics = global.diagnostics;
        let mut shadowed = Vec::new();

        for skill in global.skills {
            catalog.insert(skill);
        }

        diagnostics.extend(project.diagnostics);
        for skill in project.skills {
            if let Some(previous) = catalog.skill(skill.name()) {
                shadowed.push(SkillShadow {
                    name: skill.name.clone(),
                    global_source: previous.source.clone(),
                    project_source: skill.source.clone(),
                });
            }
            catalog.insert(skill);
        }

        Ok(SkillDiscovery {
            catalog,
            diagnostics,
            shadowed,
        })
    }

    pub fn len(&self) -> usize {
        self.skills.len()
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    pub fn skill(&self, name: &str) -> Option<&Skill> {
        self.positions
            .get(name)
            .map(|position| &self.skills[*position])
    }

    pub fn skills(&self) -> impl ExactSizeIterator<Item = &Skill> {
        self.skills.iter()
    }

    /// Whether this skill was discovered under the project root rather than
    /// beside the global configuration.
    ///
    /// The distinction decides whether a skill's files have a project-relative
    /// spelling a permission rule can name, which is why it is answered from
    /// where the skill was discovered rather than from where its directory
    /// happens to sit.
    pub fn is_project_skill(&self, skill: &Skill) -> bool {
        self.project_root
            .as_ref()
            .is_some_and(|root| skill.directory().starts_with(root))
    }

    /// Whether this catalog's project half was discovered under `project_root`.
    ///
    /// Answered by equality against the one directory a project's skills are
    /// discovered in, rather than by containment: a root that merely contains
    /// the discovered one still strips off every project skill's path, and
    /// would yield a spelling relative to a root no rule is written against.
    pub fn is_paired_with_project_root(&self, project_root: &Path) -> bool {
        self.project_root
            .as_ref()
            .is_some_and(|root| root == &project_root.join(PROJECT_SKILLS_DIRECTORY))
    }

    fn insert(&mut self, skill: Skill) {
        if let Some(position) = self.positions.get(skill.name()).copied() {
            self.skills[position] = skill;
            return;
        }

        self.positions.insert(skill.name.clone(), self.skills.len());
        self.skills.push(skill);
    }
}

#[derive(Clone, Debug)]
pub struct SkillResourceTool {
    catalog: SkillCatalog,
    project_root: PathBuf,
}

impl SkillResourceTool {
    pub fn new(catalog: SkillCatalog, project_root: impl Into<PathBuf>) -> Self {
        Self {
            catalog,
            project_root: project_root.into(),
        }
    }

    /// The project-relative file one call would return, when the skill it names
    /// has such a spelling.
    ///
    /// A skill discovered under the project root always has one. A skill
    /// discovered beside the global configuration normally does not, and
    /// reports nothing, so no rule written against a path selects the call; it
    /// reports one on the single occasion it has one, which is a global skills
    /// root that happens to sit under the project root.
    ///
    /// The catalog and the root reach this tool as independent arguments and
    /// can therefore disagree. A project skill answered under a root the
    /// catalog was not discovered under is that disagreement, and it is refused
    /// rather than answered. Refusing on the pairing rather than on whether the
    /// root strips off is what covers a root ABOVE the true one: there the
    /// prefix comes off cleanly and the call would be answered with a reach
    /// spelled relative to a root no rule is written against — `deny
    /// skill(**/.agens/**)` would survive on its leading wildcard while `deny
    /// skill(.agens/skills/**)` silently stopped selecting anything.
    ///
    /// The refusal is an [`Error::Permission`] rather than an
    /// [`Error::Tool`]: the arguments are well formed and the caller cannot
    /// repair the disagreement by rewriting them, so it must not arrive as
    /// the argument error that asks it to.
    fn reached_file(&self, arguments: &Value) -> Result<Option<PathBuf>, Error> {
        let Some(skill) = arguments
            .get("skill")
            .and_then(Value::as_str)
            .and_then(|name| self.catalog.skill(name))
        else {
            return Ok(None);
        };
        if self.catalog.is_project_skill(skill)
            && !self.catalog.is_paired_with_project_root(&self.project_root)
        {
            return Err(Error::Permission(
                "skill catalog was discovered under a different project root".into(),
            ));
        }

        let Ok(directory) = skill.directory().strip_prefix(&self.project_root) else {
            return Ok(None);
        };

        let reached = match (
            arguments.get("resource_class").and_then(Value::as_str),
            arguments.get("resource").and_then(Value::as_str),
        ) {
            (None, None) => Some(directory.join(SKILL_MANIFEST_NAME)),
            (Some(class), Some(resource)) if is_normal_filename(resource) => {
                SkillResourceClass::parse(class)
                    .map(|class| directory.join(class.directory()).join(resource))
            }
            _ => None,
        };

        Ok(reached)
    }

    pub fn input_schema() -> Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["skill"],
            "properties": {
                "skill": {"type": "string", "minLength": 1, "maxLength": 64},
                "resource_class": {"type": "string", "enum": ["reference", "script", "asset"]},
                "resource": {"type": "string", "minLength": 1}
            }
        })
    }
}

impl DispatchTool for SkillResourceTool {
    fn permission_target(&self, arguments: &Value) -> Result<String, Error> {
        arguments
            .get("skill")
            .and_then(Value::as_str)
            .filter(|skill| !skill.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| Error::Tool("skill arguments are invalid".into()))
    }

    fn permission_reach(&self, arguments: &Value) -> Result<Vec<PermissionReach>, Error> {
        Ok(self
            .reached_file(arguments)?
            .map(|path| PermissionReach::Path(path.to_string_lossy().into_owned()))
            .into_iter()
            .collect())
    }

    fn execute(&mut self, _: &ToolExecutionContext, arguments: Value) -> Result<ToolOutput, Error> {
        let Some(arguments) = arguments.as_object().filter(|arguments| {
            arguments
                .keys()
                .all(|key| matches!(key.as_str(), "skill" | "resource_class" | "resource"))
        }) else {
            return Ok(ToolOutput::failure("skill arguments are invalid"));
        };
        let Some(skill_name) = arguments
            .get("skill")
            .and_then(Value::as_str)
            .filter(|skill| !skill.is_empty())
        else {
            return Ok(ToolOutput::failure("skill arguments are invalid"));
        };
        let Some(skill) = self.catalog.skill(skill_name) else {
            return Ok(ToolOutput::failure("skill is unavailable"));
        };
        let content = match (
            arguments.get("resource_class").and_then(Value::as_str),
            arguments.get("resource").and_then(Value::as_str),
        ) {
            (None, None) => skill.load_instructions(),
            (Some(class), Some(resource)) => {
                let Some(class) = SkillResourceClass::parse(class) else {
                    return Ok(ToolOutput::failure("skill resource class is invalid"));
                };
                skill.load_resource(class, resource)
            }
            _ => {
                return Ok(ToolOutput::failure(
                    "skill resource arguments are incomplete",
                ));
            }
        };

        Ok(match content {
            Ok(content) => ToolOutput::success(content),
            Err(_) => ToolOutput::failure("skill content is unavailable"),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillDiscovery {
    catalog: SkillCatalog,
    diagnostics: Vec<SkillDiagnostic>,
    shadowed: Vec<SkillShadow>,
}

impl SkillDiscovery {
    pub fn catalog(&self) -> &SkillCatalog {
        &self.catalog
    }

    pub fn diagnostics(&self) -> &[SkillDiagnostic] {
        &self.diagnostics
    }

    pub fn shadowed(&self) -> &[SkillShadow] {
        &self.shadowed
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillDiagnostic {
    path: PathBuf,
    message: String,
}

impl SkillDiagnostic {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillShadow {
    name: String,
    global_source: PathBuf,
    project_source: PathBuf,
}

impl SkillShadow {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn global_source(&self) -> &Path {
        &self.global_source
    }

    pub fn project_source(&self) -> &Path {
        &self.project_source
    }
}

#[derive(Debug)]
pub struct SkillDiscoveryError {
    path: PathBuf,
    operation: &'static str,
    source: io::Error,
}

impl fmt::Display for SkillDiscoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "cannot {} skill root {}: {}",
            self.operation,
            self.path.display(),
            self.source
        )
    }
}

impl std::error::Error for SkillDiscoveryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

#[derive(Default)]
struct SkillRootLoad {
    skills: Vec<Skill>,
    diagnostics: Vec<SkillDiagnostic>,
}

#[cfg(unix)]
fn load_skill_root(root: &Path) -> Result<SkillRootLoad, SkillDiscoveryError> {
    let root_directory = match open_skill_root(root) {
        Ok(directory) => Arc::new(directory),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(SkillRootLoad::default());
        }
        Err(error) => return Err(skill_root_error(root, "open", error)),
    };
    let (entries, truncated) = read_skill_root_entries(&root_directory)
        .map_err(|error| skill_root_error(root, "read", error))?;

    let mut diagnostics = Vec::new();
    if truncated {
        diagnostics.push(skill_diagnostic(
            root,
            format!("skill root entry limit of {MAX_SKILL_ROOT_ENTRIES} exceeded; later entries were skipped"),
        ));
    }

    let mut candidates = Vec::new();
    for entry in entries {
        match load_skill_manifest(root, &root_directory, &entry) {
            Ok(Some(skill)) if candidates.len() < MAX_SKILL_DIRECTORIES_PER_ROOT => {
                candidates.push(skill)
            }
            Ok(Some(_)) => {
                diagnostics.push(skill_diagnostic(
                    root,
                    format!(
                        "skill directory limit of {MAX_SKILL_DIRECTORIES_PER_ROOT} exceeded; later skills were skipped"
                    ),
                ));
                break;
            }
            Ok(None) => {}
            Err(diagnostic) => diagnostics.push(diagnostic),
        }
    }

    let mut ambiguous = BTreeMap::<String, usize>::new();
    for skill in &candidates {
        *ambiguous.entry(skill.name.clone()).or_default() += 1;
    }
    let mut skills = Vec::new();
    for skill in candidates {
        if ambiguous[&skill.name] == 1 {
            skills.push(skill);
        } else {
            diagnostics.push(skill_diagnostic(
                &skill.source,
                format!("duplicate skill name {} in the same root", skill.name),
            ));
        }
    }

    Ok(SkillRootLoad {
        skills,
        diagnostics,
    })
}

#[cfg(not(unix))]
fn load_skill_root(root: &Path) -> Result<SkillRootLoad, SkillDiscoveryError> {
    Err(skill_root_error(
        root,
        "use",
        io::Error::new(
            io::ErrorKind::Unsupported,
            "secure skill discovery is unavailable on this platform",
        ),
    ))
}

#[cfg(unix)]
fn load_skill_manifest(
    root: &Path,
    root_directory: &Arc<fs::File>,
    directory_name: &std::ffi::OsStr,
) -> Result<Option<Skill>, SkillDiagnostic> {
    let directory = root.join(directory_name);
    let manifest = directory.join(SKILL_MANIFEST_NAME);
    let directory_descriptor = match open_child_directory(root_directory, directory_name) {
        Ok(Some(descriptor)) => descriptor,
        Ok(None) => return Ok(None),
        Err(error) => return Err(skill_diagnostic(&directory, error)),
    };
    let mut manifest_descriptor = match open_manifest(&directory_descriptor) {
        Ok(Some(descriptor)) => descriptor,
        Ok(None) => return Ok(None),
        Err(error) => return Err(skill_diagnostic(&manifest, error)),
    };
    let metadata = manifest_descriptor.metadata().map_err(|error| {
        skill_diagnostic(
            &manifest,
            format!("cannot inspect opened manifest: {error}"),
        )
    })?;
    ensure_single_link_regular_file(&metadata, "manifest must be a single-link regular file")
        .map_err(|message| skill_diagnostic(&manifest, message))?;
    if metadata.len() > MAX_SKILL_MANIFEST_BYTES {
        return Err(skill_diagnostic(
            &manifest,
            format!("manifest exceeds {MAX_SKILL_MANIFEST_BYTES} byte limit"),
        ));
    }

    let contents = read_bounded_utf8(&mut manifest_descriptor, MAX_SKILL_MANIFEST_BYTES)
        .map_err(|message| skill_diagnostic(&manifest, message))?;
    parse_skill_manifest(
        &manifest,
        &directory,
        Arc::clone(root_directory),
        directory_name.to_os_string(),
        &contents,
    )
    .map(Some)
    .map_err(|message| skill_diagnostic(&manifest, message))
}

fn parse_skill_manifest(
    source: &Path,
    directory: &Path,
    root_directory_descriptor: Arc<fs::File>,
    directory_name: std::ffi::OsString,
    contents: &str,
) -> Result<Skill, String> {
    let (frontmatter, body) = split_skill_frontmatter(contents)?;
    let fields: SkillFrontmatter = serde_yaml::from_str(frontmatter)
        .map_err(|error| format!("invalid frontmatter: {error}"))?;
    let name = fields.name.trim().to_owned();
    let description = fields.description.trim().to_owned();
    validate_skill_name(&name)?;
    validate_skill_description(&description)?;
    if body.trim().is_empty() {
        return Err("markdown body is required".into());
    }

    Ok(Skill {
        name,
        description,
        source: source.to_path_buf(),
        directory: directory.to_path_buf(),
        root_directory_descriptor,
        directory_name,
    })
}

struct SkillFrontmatter {
    name: String,
    description: String,
}

impl<'de> Deserialize<'de> for SkillFrontmatter {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(SkillFrontmatterVisitor)
    }
}

struct SkillFrontmatterVisitor;

impl<'de> Visitor<'de> for SkillFrontmatterVisitor {
    type Value = SkillFrontmatter;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a skill frontmatter mapping")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut name = None;
        let mut description = None;

        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "name" => {
                    if name.is_some() {
                        return Err(de::Error::duplicate_field("name"));
                    }
                    name = Some(map.next_value_seed(YamlString)?);
                }
                "description" => {
                    if description.is_some() {
                        return Err(de::Error::duplicate_field("description"));
                    }
                    description = Some(map.next_value_seed(YamlString)?);
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }

        Ok(SkillFrontmatter {
            name: name.ok_or_else(|| de::Error::missing_field("name"))?,
            description: description.ok_or_else(|| de::Error::missing_field("description"))?,
        })
    }
}

struct YamlString;

impl<'de> DeserializeSeed<'de> for YamlString {
    type Value = String;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(YamlStringVisitor)
    }
}

struct YamlStringVisitor;

impl Visitor<'_> for YamlStringVisitor {
    type Value = String;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a YAML string scalar")
    }

    fn visit_borrowed_str<E>(self, value: &'_ str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(value.to_owned())
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(value.to_owned())
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(value)
    }
}

fn split_skill_frontmatter(contents: &str) -> Result<(&str, &str), String> {
    let Some(first_end) = contents.find('\n') else {
        return Err("frontmatter must begin with --- followed by a newline".into());
    };
    if contents[..first_end].trim_end_matches('\r') != "---" {
        return Err("frontmatter must begin with ---".into());
    }

    let frontmatter_start = first_end + 1;
    let mut offset = frontmatter_start;
    while offset < contents.len() {
        let line_end = contents[offset..]
            .find('\n')
            .map(|index| offset + index)
            .unwrap_or(contents.len());
        if contents[offset..line_end].trim_end_matches('\r') == "---" {
            let body_start = if line_end == contents.len() {
                line_end
            } else {
                line_end + 1
            };
            return Ok((
                &contents[frontmatter_start..offset],
                &contents[body_start..],
            ));
        }
        if line_end == contents.len() {
            break;
        }
        offset = line_end + 1;
    }

    Err("frontmatter closing --- is required".into())
}

fn validate_skill_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.chars().count() > MAX_SKILL_NAME_CHARS {
        return Err(format!(
            "name must contain 1 to {MAX_SKILL_NAME_CHARS} characters"
        ));
    }
    let bytes = name.as_bytes();
    if !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit()
        || !bytes[bytes.len() - 1].is_ascii_lowercase() && !bytes[bytes.len() - 1].is_ascii_digit()
        || bytes
            .iter()
            .any(|byte| !byte.is_ascii_lowercase() && !byte.is_ascii_digit() && *byte != b'-')
        || name.contains("--")
    {
        return Err("name must use lowercase ASCII letters, digits, and internal hyphens".into());
    }
    Ok(())
}

fn validate_skill_description(description: &str) -> Result<(), String> {
    if description.trim().is_empty() || description.chars().count() > MAX_SKILL_DESCRIPTION_CHARS {
        return Err(format!(
            "description must contain 1 to {MAX_SKILL_DESCRIPTION_CHARS} characters"
        ));
    }
    if description
        .chars()
        .any(|character| character.is_control() && character != '\n')
    {
        return Err("description cannot contain control characters".into());
    }
    Ok(())
}

#[cfg(unix)]
fn open_skill_root(root: &Path) -> io::Result<fs::File> {
    use std::{
        ffi::CString,
        os::{fd::FromRawFd, unix::ffi::OsStrExt},
    };

    let root = CString::new(root.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "root contains a null byte"))?;
    let descriptor = unsafe {
        libc::open(
            root.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(unsafe { fs::File::from_raw_fd(descriptor) })
}

#[cfg(unix)]
fn read_skill_root_entries(root: &fs::File) -> io::Result<(Vec<std::ffi::OsString>, bool)> {
    use std::{
        ffi::CStr,
        os::{fd::IntoRawFd, unix::ffi::OsStrExt},
    };

    let root = root.try_clone()?;
    let directory = unsafe { libc::fdopendir(root.into_raw_fd()) };
    if directory.is_null() {
        return Err(io::Error::last_os_error());
    }

    let result = (|| {
        let mut entries = Vec::new();
        loop {
            unsafe {
                *libc::__errno_location() = 0;
            }
            let entry = unsafe { libc::readdir(directory) };
            if entry.is_null() {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(0) {
                    entries.sort();
                    return Ok((entries, false));
                }
                return Err(error);
            }

            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            if entries.len() == MAX_SKILL_ROOT_ENTRIES {
                entries.sort();
                return Ok((entries, true));
            }
            entries.push(std::ffi::OsStr::from_bytes(name).to_os_string());
        }
    })();
    unsafe {
        libc::closedir(directory);
    }
    result
}

#[cfg(unix)]
fn open_child_directory(
    root: &fs::File,
    directory_name: &std::ffi::OsStr,
) -> Result<Option<fs::File>, String> {
    use std::{
        ffi::CString,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::ffi::OsStrExt,
        },
    };

    let directory_name_c = CString::new(directory_name.as_bytes())
        .map_err(|_| "skill directory name contains a null byte".to_string())?;
    let descriptor = unsafe {
        libc::openat(
            root.as_raw_fd(),
            directory_name_c.as_ptr(),
            libc::O_RDONLY
                | libc::O_DIRECTORY
                | libc::O_NOFOLLOW
                | libc::O_CLOEXEC
                | libc::O_NONBLOCK,
        )
    };
    if descriptor >= 0 {
        return Ok(Some(unsafe { fs::File::from_raw_fd(descriptor) }));
    }

    let error = io::Error::last_os_error();
    if child_is_symlink(root, directory_name)? {
        return Err("symbolic-link skill directories are not allowed".into());
    }
    if error.kind() == io::ErrorKind::NotADirectory {
        return Ok(None);
    }
    Err(format!("cannot open skill directory: {error}"))
}

#[cfg(unix)]
fn child_is_symlink(root: &fs::File, name: &std::ffi::OsStr) -> Result<bool, String> {
    use std::{
        ffi::CString,
        mem::MaybeUninit,
        os::{fd::AsRawFd, unix::ffi::OsStrExt},
    };

    let name = CString::new(name.as_bytes())
        .map_err(|_| "skill directory name contains a null byte".to_string())?;
    let mut metadata = MaybeUninit::<libc::stat>::uninit();
    let result = unsafe {
        libc::fstatat(
            root.as_raw_fd(),
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result < 0 {
        return Err(format!(
            "cannot inspect skill directory: {}",
            io::Error::last_os_error()
        ));
    }

    let metadata = unsafe { metadata.assume_init() };
    Ok(metadata.st_mode & libc::S_IFMT == libc::S_IFLNK)
}

#[cfg(unix)]
fn open_manifest(directory: &fs::File) -> Result<Option<fs::File>, String> {
    use std::{
        ffi::CString,
        os::fd::{AsRawFd, FromRawFd},
    };

    let manifest_name =
        CString::new(SKILL_MANIFEST_NAME).expect("static manifest name has no null byte");
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            manifest_name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    if descriptor >= 0 {
        return Ok(Some(unsafe { fs::File::from_raw_fd(descriptor) }));
    }

    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::NotFound {
        return Ok(None);
    }
    if error.raw_os_error() == Some(libc::ELOOP) {
        return Err("manifest must be a regular non-symbolic-link file".into());
    }
    Err(format!("cannot open manifest: {error}"))
}

#[cfg(unix)]
fn open_child_file(
    directory: &fs::File,
    name: &std::ffi::OsStr,
) -> Result<Option<fs::File>, String> {
    use std::{
        ffi::CString,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::ffi::OsStrExt,
        },
    };

    let name = CString::new(name.as_bytes())
        .map_err(|_| "skill resource name contains a null byte".to_string())?;
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    if descriptor >= 0 {
        return Ok(Some(unsafe { fs::File::from_raw_fd(descriptor) }));
    }

    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::NotFound {
        return Ok(None);
    }
    if error.raw_os_error() == Some(libc::ELOOP) {
        return Err("skill resource must be a regular non-symbolic-link file".into());
    }
    Err(format!("cannot open skill resource: {error}"))
}

#[cfg(unix)]
fn ensure_single_link_regular_file(metadata: &fs::Metadata, message: &str) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;

    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(message.into());
    }

    Ok(())
}

fn is_normal_filename(name: &str) -> bool {
    !name.is_empty()
        && Path::new(name)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
        && Path::new(name).components().count() == 1
}

#[cfg(unix)]
fn read_bounded_utf8(file: &mut fs::File, limit: u64) -> Result<String, String> {
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read opened skill file: {error}"))?;
    if bytes.len() > limit as usize {
        return Err(format!("skill file exceeds {limit} byte limit"));
    }

    String::from_utf8(bytes).map_err(|error| format!("skill file is not UTF-8: {error}"))
}

fn skill_root_error(
    root: &Path,
    operation: &'static str,
    source: io::Error,
) -> SkillDiscoveryError {
    SkillDiscoveryError {
        path: root.to_path_buf(),
        operation,
        source,
    }
}

fn skill_diagnostic(path: &Path, message: String) -> SkillDiagnostic {
    SkillDiagnostic {
        path: path.to_path_buf(),
        message,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChildCapability {
    FilesystemRead,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChildCapabilityRegistry {
    allowed: Vec<ChildCapability>,
}

impl ChildCapabilityRegistry {
    pub fn isolated() -> Self {
        Self {
            allowed: vec![ChildCapability::FilesystemRead],
        }
    }

    pub fn allowed(&self) -> &[ChildCapability] {
        &self.allowed
    }

    pub const fn allows_descendants(&self) -> bool {
        false
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubagentInvocation {
    skill_name: String,
    prompt: String,
    context: String,
}

impl SubagentInvocation {
    pub fn new(skill_name: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            skill_name: skill_name.into(),
            prompt: prompt.into(),
            context: String::new(),
        }
    }

    pub fn with_context(mut self, context: impl Into<String>) -> Result<Self, SubagentInputError> {
        self.context = context.into();
        Ok(self)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SubagentLimits {
    max_concurrent: usize,
    max_input_chars: usize,
    max_output_chars: usize,
    timeout: Duration,
}

impl SubagentLimits {
    pub fn new(
        max_concurrent: usize,
        max_input_chars: usize,
        max_output_chars: usize,
        timeout: Duration,
    ) -> Result<Self, SubagentInputError> {
        if max_concurrent == 0 || max_input_chars == 0 || max_output_chars == 0 || timeout.is_zero()
        {
            return Err(SubagentInputError::InvalidLimits);
        }

        Ok(Self {
            max_concurrent,
            max_input_chars,
            max_output_chars,
            timeout,
        })
    }
}

impl Default for SubagentLimits {
    fn default() -> Self {
        Self {
            max_concurrent: DEFAULT_MAX_SUBAGENT_CONCURRENCY,
            max_input_chars: DEFAULT_MAX_SUBAGENT_INPUT_CHARS,
            max_output_chars: DEFAULT_MAX_SUBAGENT_OUTPUT_CHARS,
            timeout: DEFAULT_SUBAGENT_TIMEOUT,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubagentInputError {
    InvalidLimits,
}

impl fmt::Display for SubagentInputError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimits => formatter.write_str("subagent limits must be greater than zero"),
        }
    }
}

impl std::error::Error for SubagentInputError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubagentTurnRequest {
    skill_name: String,
    skill_description: String,
    instructions: String,
    prompt: String,
    context: String,
    capabilities: ChildCapabilityRegistry,
}

impl SubagentTurnRequest {
    pub fn skill_name(&self) -> &str {
        &self.skill_name
    }

    pub fn skill_description(&self) -> &str {
        &self.skill_description
    }

    pub fn instructions(&self) -> &str {
        &self.instructions
    }

    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    pub fn context(&self) -> &str {
        &self.context
    }

    pub fn capabilities(&self) -> &ChildCapabilityRegistry {
        &self.capabilities
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubagentTurnResult {
    output: String,
}

impl SubagentTurnResult {
    pub fn new(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubagentRunnerError {
    ModelFailure,
    InfrastructureFailure,
}

#[derive(Clone)]
pub struct SubagentRunContext {
    cancellation: Arc<AtomicBool>,
    deadline: Instant,
}

impl SubagentRunContext {
    fn inherit(parent: &ToolExecutionContext, timeout: Duration) -> Self {
        let own_deadline = Instant::now() + timeout;
        Self {
            cancellation: parent.cancellation_handle(),
            deadline: parent
                .deadline()
                .map_or(own_deadline, |deadline| deadline.min(own_deadline)),
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancellation.load(Ordering::Acquire)
    }

    pub fn is_expired(&self) -> bool {
        Instant::now() >= self.deadline
    }

    pub fn check(&self) -> Result<(), SubagentRunnerError> {
        if self.is_cancelled() || self.is_expired() {
            return Err(SubagentRunnerError::ModelFailure);
        }
        Ok(())
    }
}

/// The runner owns provider credentials and must cooperatively check the supplied context.
pub trait SubagentRunner: Send + 'static {
    fn run(
        &mut self,
        request: SubagentTurnRequest,
        context: &SubagentRunContext,
    ) -> Result<SubagentTurnResult, SubagentRunnerError>;
}

/// Not wired into any production runtime; delegation runs through [`TaskTool`].
///
/// Kept because it is the only place the bounded-deadline subagent contract is
/// written down, and its tests still hold that contract. Wiring it as it
/// stands would leak a worker on every expiry: the deadline path abandons the
/// worker while it still holds the runner behind a mutex. Fix that before
/// giving it a runtime, or delete both it and its tests.
pub struct SubagentTool<R> {
    catalog: SkillCatalog,
    runner: Arc<Mutex<R>>,
    limits: SubagentLimits,
    active: Arc<std::sync::atomic::AtomicUsize>,
}

impl<R> Clone for SubagentTool<R> {
    fn clone(&self) -> Self {
        Self {
            catalog: self.catalog.clone(),
            runner: Arc::clone(&self.runner),
            limits: self.limits,
            active: Arc::clone(&self.active),
        }
    }
}

impl<R: SubagentRunner> SubagentTool<R> {
    pub fn discover(
        global_root: impl AsRef<Path>,
        project_root: impl AsRef<Path>,
        runner: R,
        limits: SubagentLimits,
    ) -> Result<Self, SkillDiscoveryError> {
        let discovery = SkillCatalog::discover(global_root, project_root)?;
        Ok(Self::from_catalog(discovery.catalog, runner, limits))
    }

    pub fn from_catalog(catalog: SkillCatalog, runner: R, limits: SubagentLimits) -> Self {
        Self {
            catalog,
            runner: Arc::new(Mutex::new(runner)),
            limits,
            active: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    pub fn execute(
        &self,
        invocation: SubagentInvocation,
        cancellation: Arc<AtomicBool>,
    ) -> ToolOutput {
        let context = ToolExecutionContext::new(cancellation, self.limits.timeout);
        self.execute_with_context(invocation, &context)
    }

    pub fn execute_with_context(
        &self,
        invocation: SubagentInvocation,
        parent: &ToolExecutionContext,
    ) -> ToolOutput {
        let Some(skill) = self.catalog.skill(&invocation.skill_name) else {
            return ToolOutput::failure("subagent: requested skill is unavailable");
        };
        let Ok(instructions) = skill.load_instructions() else {
            return ToolOutput::failure("subagent: requested skill is unavailable");
        };
        if invocation.prompt.is_empty()
            || invocation
                .prompt
                .chars()
                .count()
                .saturating_add(invocation.context.chars().count())
                > self.limits.max_input_chars
        {
            return ToolOutput::failure("subagent: input exceeds configured bounds");
        }
        if parent.is_cancelled() {
            return ToolOutput::failure("subagent: cancelled");
        }
        if parent.is_expired() {
            return ToolOutput::failure("subagent: deadline exceeded");
        }

        let Some(permit) = SubagentPermit::acquire(&self.active, self.limits.max_concurrent) else {
            return ToolOutput::failure("subagent: concurrent child limit reached");
        };
        let context = SubagentRunContext::inherit(parent, self.limits.timeout);
        if context.is_cancelled() {
            return ToolOutput::failure("subagent: cancelled");
        }
        if context.is_expired() {
            return ToolOutput::failure("subagent: deadline exceeded");
        }
        let request = SubagentTurnRequest {
            skill_name: skill.name.clone(),
            skill_description: skill.description.clone(),
            instructions,
            prompt: invocation.prompt,
            context: invocation.context,
            capabilities: ChildCapabilityRegistry::isolated(),
        };

        let (sender, receiver) = mpsc::channel();
        let runner = Arc::clone(&self.runner);
        let worker_context = context.clone();

        thread::spawn(move || {
            let result = {
                let _permit = permit;
                install_subagent_panic_hook();
                IS_SUBAGENT_WORKER.with(|is_worker| is_worker.set(true));
                let result = catch_unwind(AssertUnwindSafe(|| {
                    let mut runner = runner
                        .lock()
                        .map_err(|_| SubagentRunnerError::InfrastructureFailure)?;
                    runner.run(request, &worker_context)
                }))
                .unwrap_or(Err(SubagentRunnerError::InfrastructureFailure));
                IS_SUBAGENT_WORKER.with(|is_worker| is_worker.set(false));

                result
            };

            let _ = sender.send(result);
        });

        loop {
            if context.is_cancelled() {
                return ToolOutput::failure("subagent: cancelled");
            }
            if context.is_expired() {
                return ToolOutput::failure("subagent: deadline exceeded");
            }

            let remaining = context.deadline.saturating_duration_since(Instant::now());
            let wait = remaining.min(SUBAGENT_RESULT_POLL_INTERVAL);

            match receiver.recv_timeout(wait) {
                Ok(result) => {
                    if context.is_cancelled() {
                        return ToolOutput::failure("subagent: cancelled");
                    }
                    if context.is_expired() {
                        return ToolOutput::failure("subagent: deadline exceeded");
                    }

                    return self.result_output(result);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return ToolOutput::failure("subagent: infrastructure failure");
                }
            }
        }
    }

    fn result_output(&self, result: Result<SubagentTurnResult, SubagentRunnerError>) -> ToolOutput {
        match result {
            Ok(result) if result.output.chars().count() <= self.limits.max_output_chars => {
                ToolOutput::success(result.output)
            }
            Ok(_) => ToolOutput::failure("subagent: output limit exceeded"),
            Err(SubagentRunnerError::ModelFailure) => {
                ToolOutput::failure("subagent: child execution failed")
            }
            Err(SubagentRunnerError::InfrastructureFailure) => {
                ToolOutput::failure("subagent: infrastructure failure")
            }
        }
    }
}

struct SubagentPermit {
    active: Arc<std::sync::atomic::AtomicUsize>,
}

impl SubagentPermit {
    fn acquire(
        active: &Arc<std::sync::atomic::AtomicUsize>,
        max_concurrent: usize,
    ) -> Option<Self> {
        let mut current = active.load(Ordering::Acquire);
        loop {
            if current >= max_concurrent {
                return None;
            }
            match active.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(Self {
                        active: Arc::clone(active),
                    });
                }
                Err(next) => current = next,
            }
        }
    }
}

impl Drop for SubagentPermit {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Protocol version agens requests during MCP initialize.
pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

/// Protocol versions agens accepts from an MCP server's initialize response.
///
/// A server is free to answer with a different version than the one agens
/// requested, as long as it is one this build knows how to speak. Any other
/// answer fails the connection with `McpErrorCategory::Protocol`.
///
/// `2024-11-05` is still what a large share of published servers answer with,
/// and every request and response shape agens speaks is unchanged between it
/// and the versions above, so refusing it only cost reachable servers.
pub const SUPPORTED_MCP_PROTOCOL_VERSIONS: [&str; 4] =
    ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

#[derive(Clone, Debug, PartialEq)]
pub struct McpInitialize {
    pub protocol_version: String,
    pub capabilities: Value,
    pub client_info_name: String,
    pub client_info_version: String,
}

impl McpInitialize {
    pub fn new(
        protocol_version: impl Into<String>,
        capabilities: Value,
        client_info_name: impl Into<String>,
        client_info_version: impl Into<String>,
    ) -> Self {
        Self {
            protocol_version: protocol_version.into(),
            capabilities,
            client_info_name: client_info_name.into(),
            client_info_version: client_info_version.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum McpRequest {
    Initialize(McpInitialize),
    Initialized,
    ListTools { cursor: Option<String> },
    CallTool { name: String, arguments: Value },
}

#[derive(Clone, Debug, PartialEq)]
pub struct McpInitializeResult {
    pub protocol_version: String,
    pub capabilities: Value,
}

impl McpInitializeResult {
    pub fn new(protocol_version: impl Into<String>, capabilities: Value) -> Self {
        Self {
            protocol_version: protocol_version.into(),
            capabilities,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct McpToolsPage {
    pub tools: Vec<McpToolDefinition>,
    pub next_cursor: Option<String>,
}

impl McpToolsPage {
    pub fn new(tools: Vec<McpToolDefinition>, next_cursor: Option<String>) -> Self {
        Self { tools, next_cursor }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpProtocolError {
    pub code: i64,
    pub message: String,
}

impl McpProtocolError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// One block of a `tools/call` result, as much of it as a text-only tool
/// output can carry.
///
/// The provider surface agens speaks to takes a tool result as a single
/// string, so an image, audio, or binary-resource block has nothing to be
/// forwarded as. It becomes a [`McpContentBlock::NonText`] description of what
/// the server returned instead — the model still learns a screenshot came
/// back, and the call no longer fails the whole turn.
#[derive(Clone, Debug, PartialEq)]
pub enum McpContentBlock {
    Text(String),
    /// An agens-authored description of a block that carries no text, such as
    /// `[mcp image content: image/png]`. Never interpolates server text
    /// beyond a bounded, sanitized media type.
    NonText(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct McpCallResult {
    pub content: Vec<McpContentBlock>,
    pub is_error: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum McpResponse {
    Initialized(McpInitializeResult),
    ToolsListed(McpToolsPage),
    ToolCalled(McpCallResult),
    ProtocolError(McpProtocolError),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpTransportError {
    Cancelled,
    TimedOut,
    RetriesExhausted,
    Protocol(String),
    Transport(String),
    HttpStatus(u16),
}

impl fmt::Display for McpTransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("mcp operation cancelled"),
            Self::TimedOut => formatter.write_str("mcp operation timed out"),
            Self::RetriesExhausted => formatter.write_str("mcp HTTP retries exhausted"),
            Self::Protocol(message) => write!(formatter, "mcp protocol error: {message}"),
            Self::Transport(message) => write!(formatter, "mcp transport error: {message}"),
            Self::HttpStatus(status) => write!(formatter, "mcp http status {status}"),
        }
    }
}

impl std::error::Error for McpTransportError {}

pub struct McpOperationContext {
    cancellation: Option<Arc<AtomicBool>>,
    headless_cancellation: Option<HeadlessTurnCancellationAdapter>,
    deadline: Option<Instant>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolExecutionStatus {
    Cancelled,
    TimedOut,
}

impl fmt::Display for ToolExecutionStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("tool execution cancelled"),
            Self::TimedOut => formatter.write_str("tool execution timed out"),
        }
    }
}

/// Shared cancellation and absolute-deadline contract for every callable tool.
#[derive(Clone, Debug)]
pub struct ToolExecutionContext {
    cancellation: Option<Arc<AtomicBool>>,
    headless_cancellation: Option<HeadlessTurnCancellationAdapter>,
    deadline: Option<Instant>,
    read_filter: Option<PermissionReadFilter>,
    authority: PermissionAuthority,
}

impl ToolExecutionContext {
    pub fn new(cancellation: Arc<AtomicBool>, timeout: Duration) -> Self {
        Self::with_deadline(cancellation, Instant::now() + timeout)
    }

    pub fn with_timeout(timeout: Duration) -> Self {
        Self::new(Arc::new(AtomicBool::new(false)), timeout)
    }

    pub fn with_deadline(cancellation: Arc<AtomicBool>, deadline: Instant) -> Self {
        Self {
            cancellation: Some(cancellation),
            headless_cancellation: None,
            deadline: Some(deadline),
            read_filter: None,
            authority: PermissionAuthority::Decided,
        }
    }

    /// Carries the per-file decision for one authorized call to the tool that
    /// performs it. Set by [`ToolDispatcher::execute`], which is where a call
    /// stops being a decision and starts reading files.
    #[must_use]
    pub fn with_read_filter(mut self, filter: PermissionReadFilter) -> Self {
        self.read_filter = Some(filter);
        self
    }

    /// Carries who authorized this call to the tool that performs it. Set by
    /// [`ToolDispatcher::execute`] from the authorized call's own authority.
    #[must_use]
    pub fn with_authority(mut self, authority: PermissionAuthority) -> Self {
        self.authority = authority;
        self
    }

    /// Whether this call's authorization can widen a write past the project
    /// root once the person or the policy has already allowed it.
    ///
    /// Dangerous mode's fallback cannot: nobody decided that call, so the
    /// confinement floor stays where it is. A context built without any
    /// authority at all — a direct test of a tool — keeps the behavior it had
    /// before there was one.
    pub fn permits_write_outside_root(&self) -> bool {
        self.authority == PermissionAuthority::Decided
    }

    /// Whether the call may report what the project-relative `path` holds.
    ///
    /// A context built without a filter permits everything: it belongs to a
    /// caller that never went through a permission decision at all, such as a
    /// direct test of a tool, and inventing a refusal there would withhold
    /// files no rule was ever consulted about. Every dispatched call carries
    /// one.
    pub fn permits_read(&self, path: &str) -> bool {
        self.read_filter
            .as_ref()
            .is_none_or(|filter| filter.permits(path))
    }

    /// Whether any file this call reads can be withheld at all.
    ///
    /// A tool that has to spend work — a second process, a second walk —
    /// before it can ask [`Self::permits_read`] anything asks this first, so
    /// the caller that carries no filter pays none of it and behaves exactly
    /// as it did before there was one.
    pub fn filters_reads(&self) -> bool {
        self.read_filter.is_some()
    }

    /// Adapts core's opaque turn cancellation view without exposing its internals.
    pub fn from_headless_adapter(cancellation: HeadlessTurnCancellationAdapter) -> Self {
        let deadline = cancellation.deadline();
        Self {
            cancellation: None,
            headless_cancellation: Some(cancellation),
            deadline,
            read_filter: None,
            authority: PermissionAuthority::Decided,
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancellation
            .as_ref()
            .is_some_and(|cancellation| cancellation.load(Ordering::Acquire))
            || self
                .headless_cancellation
                .as_ref()
                .is_some_and(HeadlessTurnCancellationAdapter::is_cancelled)
    }

    pub fn is_expired(&self) -> bool {
        self.deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
    }

    pub fn check(&self) -> Result<(), ToolExecutionStatus> {
        if self.is_cancelled() {
            return Err(ToolExecutionStatus::Cancelled);
        }
        if self.is_expired() {
            return Err(ToolExecutionStatus::TimedOut);
        }
        Ok(())
    }

    pub fn remaining(&self) -> Result<Duration, ToolExecutionStatus> {
        self.check()?;
        Ok(self.deadline.map_or(Duration::MAX, |deadline| {
            deadline.saturating_duration_since(Instant::now())
        }))
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub fn cancellation_handle(&self) -> Arc<AtomicBool> {
        self.cancellation
            .as_ref()
            .map(Arc::clone)
            .unwrap_or_else(|| {
                self.headless_cancellation
                    .as_ref()
                    .expect("tool execution context has a cancellation source")
                    .cancellation_handle()
            })
    }

    fn mcp_context(&self) -> McpOperationContext {
        McpOperationContext {
            cancellation: self.cancellation.as_ref().map(Arc::clone),
            headless_cancellation: self.headless_cancellation.clone(),
            deadline: self.deadline,
        }
    }
}

impl McpOperationContext {
    pub fn new(cancellation: Arc<AtomicBool>, timeout: Duration) -> Self {
        Self {
            cancellation: Some(cancellation),
            headless_cancellation: None,
            deadline: Some(Instant::now() + timeout),
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancellation
            .as_ref()
            .is_some_and(|cancellation| cancellation.load(Ordering::Acquire))
            || self
                .headless_cancellation
                .as_ref()
                .is_some_and(HeadlessTurnCancellationAdapter::is_cancelled)
    }

    pub fn is_expired(&self) -> bool {
        self.deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
    }

    pub fn check(&self) -> Result<(), McpTransportError> {
        if self.is_cancelled() {
            return Err(McpTransportError::Cancelled);
        }
        if self.is_expired() {
            return Err(McpTransportError::TimedOut);
        }
        Ok(())
    }

    pub fn remaining(&self) -> Result<Duration, McpTransportError> {
        self.check()?;
        Ok(self.deadline.map_or(Duration::MAX, |deadline| {
            deadline.saturating_duration_since(Instant::now())
        }))
    }

    pub fn from_headless_adapter(cancellation: HeadlessTurnCancellationAdapter) -> Self {
        let deadline = cancellation.deadline();
        Self {
            cancellation: None,
            headless_cancellation: Some(cancellation),
            deadline,
        }
    }

    pub(crate) fn cancellation_probe(&self) -> crate::http_worker::HttpWorkerCancellationProbe {
        let cancellation = self.cancellation.as_ref().map(Arc::clone);
        let headless_cancellation = self.headless_cancellation.clone();
        Arc::new(move || {
            cancellation
                .as_ref()
                .is_some_and(|cancellation| cancellation.load(Ordering::Acquire))
                || headless_cancellation
                    .as_ref()
                    .is_some_and(HeadlessTurnCancellationAdapter::is_cancelled)
        })
    }

    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.deadline
    }
}

/// Implementations must cooperatively observe the context and must not block past its deadline.
pub trait McpTransport: Send {
    fn execute(
        &mut self,
        request: McpRequest,
        context: &McpOperationContext,
    ) -> Result<McpResponse, McpTransportError>;
    fn notify(
        &mut self,
        request: McpRequest,
        context: &McpOperationContext,
    ) -> Result<(), McpTransportError>;
    fn close(&mut self, context: &McpOperationContext) -> Result<(), McpTransportError>;

    /// Whether this transport can still carry a request.
    ///
    /// Answering without a round trip is the point: a per-call ping costs
    /// every tool call a full exchange, while a transport that owns a process
    /// or a stream usually knows locally that it is gone. A transport with no
    /// such signal keeps the default and lets the call itself surface the
    /// failure, which the caller then recovers from by reconnecting.
    fn is_alive(&mut self) -> bool {
        true
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct McpTimeouts {
    pub connect: Duration,
    pub list: Duration,
    pub call: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct McpLimits {
    pub max_list_pages: usize,
    pub max_tools: usize,
}

impl McpLimits {
    pub fn new(max_list_pages: usize, max_tools: usize) -> Result<Self, McpTransportError> {
        if max_list_pages == 0 || max_tools == 0 {
            return Err(McpTransportError::Protocol(
                "MCP list limits must be greater than zero".into(),
            ));
        }
        Ok(Self {
            max_list_pages,
            max_tools,
        })
    }
}

impl Default for McpLimits {
    fn default() -> Self {
        Self {
            max_list_pages: DEFAULT_MAX_MCP_LIST_PAGES,
            max_tools: DEFAULT_MAX_MCP_TOOLS,
        }
    }
}

impl McpTimeouts {
    pub fn new(
        connect: Duration,
        list: Duration,
        call: Duration,
    ) -> Result<Self, McpTransportError> {
        if connect.is_zero() || list.is_zero() || call.is_zero() {
            return Err(McpTransportError::Protocol(
                "mcp timeouts must be greater than zero".into(),
            ));
        }

        Ok(Self {
            connect,
            list,
            call,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpToolAnnotations {
    pub read_only_hint: Option<bool>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct McpToolDefinition {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
    pub annotations: McpToolAnnotations,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteToolAccess {
    ReadOnly,
    Write,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RemoteToolMetadata {
    pub qualified_name: String,
    pub server_name: String,
    pub tool_name: String,
    pub description: Option<String>,
    pub input_schema: Value,
    pub access: RemoteToolAccess,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpServerReport {
    Loaded {
        server_name: String,
        tool_count: usize,
    },
    Failed {
        server_name: String,
        message: String,
    },
}

impl McpServerReport {
    pub fn loaded(server_name: impl Into<String>, tool_count: usize) -> Self {
        Self::Loaded {
            server_name: server_name.into(),
            tool_count,
        }
    }

    pub fn is_failed(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }
}

#[derive(Default)]
pub struct McpRegistry {
    tools: BTreeMap<String, RemoteToolMetadata>,
    clients: BTreeMap<String, Box<dyn McpCallable>>,
    configured: BTreeMap<String, ConfiguredMcpServer>,
    diagnostics: BTreeMap<String, McpServerDiagnostic>,
    attempted: std::collections::BTreeSet<String>,
    /// Server names this registry has claimed on the shared status handle.
    ///
    /// A name is claimed exactly once, even across repeated
    /// `configure_server*` calls for the same name, so that `close()` releases
    /// exactly one claim per name regardless of how many times it was
    /// reconfigured during this registry's lifetime.
    claimed: std::collections::BTreeSet<String>,
    closed: bool,
    status: McpStatusHandle,
    discovery_cancellation: Arc<AtomicBool>,
}

/// A registry built once and kept for as long as the session that built it.
///
/// A registry used to be rebuilt for every prompt, which meant connecting to
/// every configured server before the model could start, and killing every one
/// of them when the turn ended. With several stdio servers configured that is
/// the dominant cost of a turn and a fresh chance to fail once per prompt,
/// none of which buys anything: the connections are identical each time.
///
/// The slot is filled on first use and shared by cloning, so the per-turn
/// bootstrap clone reaches the same connections the session already has.
/// Everything closes when the last holder drops it, which is the end of the
/// session rather than the end of a turn.
#[derive(Clone, Default)]
pub struct SharedMcpRegistry {
    slot: Arc<Mutex<Option<Arc<Mutex<McpRegistry>>>>>,
}

impl fmt::Debug for SharedMcpRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SharedMcpRegistry")
    }
}

impl SharedMcpRegistry {
    /// Returns this session's registry, building it with `build` the first
    /// time it is asked for.
    ///
    /// `None` only when the slot's lock is poisoned, which the caller reports
    /// as MCP being unavailable rather than silently connecting a second set
    /// of servers nobody would ever close.
    pub fn get_or_init(
        &self,
        build: impl FnOnce() -> McpRegistry,
    ) -> Option<Arc<Mutex<McpRegistry>>> {
        let mut slot = self.slot.lock().ok()?;
        Some(Arc::clone(
            slot.get_or_insert_with(|| Arc::new(Mutex::new(build()))),
        ))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpServerDiagnostic {
    pub server_name: String,
    pub message: String,
}

type McpTransportFactory =
    Box<dyn FnMut() -> Result<Box<dyn McpTransport>, McpTransportError> + Send>;

struct ConfiguredMcpServer {
    factory: Option<McpTransportFactory>,
    timeouts: McpTimeouts,
    limits: McpLimits,
}

impl McpRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_status_handle(status: McpStatusHandle) -> Self {
        Self {
            tools: BTreeMap::new(),
            clients: BTreeMap::new(),
            configured: BTreeMap::new(),
            diagnostics: BTreeMap::new(),
            attempted: BTreeSet::new(),
            claimed: BTreeSet::new(),
            closed: false,
            status,
            discovery_cancellation: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn set_discovery_cancellation(&mut self, cancellation: Arc<AtomicBool>) {
        self.discovery_cancellation = cancellation;
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn tool(&self, qualified_name: &str) -> Option<&RemoteToolMetadata> {
        self.tools.get(qualified_name)
    }

    pub fn tools(&self) -> Vec<&RemoteToolMetadata> {
        self.tools.values().collect()
    }

    pub fn diagnostics(&self) -> Vec<&McpServerDiagnostic> {
        self.diagnostics.values().collect()
    }

    pub fn configured_server_names(&self) -> Vec<String> {
        self.configured.keys().cloned().collect()
    }

    pub fn status_handle(&self) -> McpStatusHandle {
        self.status.clone()
    }

    pub fn register_disabled_server(
        &mut self,
        descriptor: McpServerDescriptor,
    ) -> Result<(), McpTransportError> {
        if descriptor.enabled() {
            return Err(McpTransportError::Protocol(
                "disabled MCP server must be disabled".into(),
            ));
        }
        validate_server_name(descriptor.name())?;
        let first_claim = self.claimed.insert(descriptor.name().to_owned());
        self.status.register(descriptor, first_claim);
        Ok(())
    }

    /// Records a server as `Failed` without ever configuring it for connect
    /// attempts.
    ///
    /// Some failures are known before any transport can be built — an
    /// invalid timeout, a rejected server name — so there is no factory to
    /// register and no later `discover_server` call will resolve them. Without
    /// this, such a server would never appear in the shared status handle and
    /// would silently vanish from `/mcp` instead of surfacing as failed.
    ///
    /// No transport exists yet, so the failure is recorded against the connect
    /// phase.
    pub fn register_failed_server(
        &mut self,
        descriptor: McpServerDescriptor,
        category: McpErrorCategory,
        message: &str,
    ) -> Result<(), McpTransportError> {
        if !descriptor.enabled() {
            return Err(McpTransportError::Protocol(
                "enabled MCP server must be enabled".into(),
            ));
        }
        validate_server_name(descriptor.name())?;
        let server_name = descriptor.name().to_owned();
        let first_claim = self.claimed.insert(server_name.clone());
        self.status.register(descriptor, first_claim);
        let report = McpServerReport::Failed {
            server_name,
            message: message.to_owned(),
        };
        self.record_report(&report, Some((category, McpLoadPhase::Connect)));
        Ok(())
    }

    pub fn configure_server<F>(
        &mut self,
        server_name: &str,
        factory: F,
        timeouts: McpTimeouts,
        limits: McpLimits,
    ) -> Result<(), McpTransportError>
    where
        F: FnMut() -> Result<Box<dyn McpTransport>, McpTransportError> + Send + 'static,
    {
        self.configure_server_with_descriptor(
            McpServerDescriptor::new(
                server_name,
                McpServerSource::Global,
                McpServerTransport::Stdio,
                true,
                timeouts.call,
                None,
            ),
            factory,
            timeouts,
            limits,
        )
    }

    pub fn configure_server_with_descriptor<F>(
        &mut self,
        descriptor: McpServerDescriptor,
        factory: F,
        timeouts: McpTimeouts,
        limits: McpLimits,
    ) -> Result<(), McpTransportError>
    where
        F: FnMut() -> Result<Box<dyn McpTransport>, McpTransportError> + Send + 'static,
    {
        if !descriptor.enabled() {
            return Err(McpTransportError::Protocol(
                "enabled MCP server must be enabled".into(),
            ));
        }
        self.insert_configuration(descriptor, Some(Box::new(factory)), timeouts, limits)
    }

    fn insert_configuration(
        &mut self,
        descriptor: McpServerDescriptor,
        factory: Option<McpTransportFactory>,
        timeouts: McpTimeouts,
        limits: McpLimits,
    ) -> Result<(), McpTransportError> {
        validate_server_name(descriptor.name())?;
        let name = descriptor.name().to_owned();
        let first_claim = self.claimed.insert(name.clone());
        self.status.register(descriptor, first_claim);
        self.configured.insert(
            name.clone(),
            ConfiguredMcpServer {
                factory,
                timeouts,
                limits,
            },
        );
        self.attempted.remove(&name);
        Ok(())
    }

    pub fn discover_server(&mut self, server_name: &str) -> McpServerReport {
        if self.closed {
            return self.failed(server_name);
        }
        if self.attempted.contains(server_name) {
            if let Some(diagnostic) = self.diagnostics.get(server_name) {
                return McpServerReport::Failed {
                    server_name: diagnostic.server_name.clone(),
                    message: diagnostic.message.clone(),
                };
            }
            return McpServerReport::loaded(
                server_name,
                self.tools
                    .values()
                    .filter(|tool| tool.server_name == server_name)
                    .count(),
            );
        }
        self.attempted.insert(server_name.into());
        self.discover_or_reload(server_name)
    }

    pub fn reload_server(&mut self, server_name: &str) -> McpServerReport {
        if self.closed {
            return self.failed(server_name);
        }
        self.attempted.insert(server_name.into());
        self.discover_or_reload(server_name)
    }

    pub fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.tools.clear();

        for (_, mut client) in std::mem::take(&mut self.clients) {
            client.close();
        }

        let claimed: Vec<String> = std::mem::take(&mut self.claimed).into_iter().collect();
        self.status
            .close_servers(claimed.iter().map(String::as_str));
    }

    pub fn load_server<T: McpTransport + 'static>(
        &mut self,
        server_name: &str,
        transport: T,
        initialize: &McpInitialize,
        timeouts: McpTimeouts,
        limits: McpLimits,
        cancellation: Arc<AtomicBool>,
    ) -> McpServerReport {
        let mut failure = None;
        let report = match load_server_client(
            server_name,
            transport,
            initialize.clone(),
            timeouts,
            limits,
            cancellation,
        ) {
            Ok((metadata, mut client)) => {
                let conflicts = metadata.iter().any(|tool| {
                    self.tools
                        .get(&tool.qualified_name)
                        .is_some_and(|existing| existing.server_name != server_name)
                });
                if conflicts || has_duplicate_qualified_name(&metadata) {
                    client.close();
                    failure = Some((McpErrorCategory::Protocol, McpLoadPhase::ListTools));
                    McpServerReport::Failed {
                        server_name: server_name.into(),
                        message: MCP_DUPLICATE_TOOL_NAMES_REASON.into(),
                    }
                } else {
                    let tool_count = metadata.len();
                    self.tools.retain(|_, tool| tool.server_name != server_name);
                    for tool in metadata {
                        self.tools.insert(tool.qualified_name.clone(), tool);
                    }
                    if let Some(mut previous) =
                        self.clients.insert(server_name.into(), Box::new(client))
                    {
                        previous.close();
                    }
                    McpServerReport::loaded(server_name, tool_count)
                }
            }
            Err(McpLoadFailure { phase, error }) => {
                let resolved_category = McpErrorCategory::from(&error);
                failure = Some((resolved_category, phase));
                McpServerReport::Failed {
                    server_name: server_name.into(),
                    message: sanitized_mcp_load_error(resolved_category, phase, &error),
                }
            }
        };
        self.record_report(&report, failure);
        report
    }

    pub fn load_servers<T: McpTransport + 'static>(
        &mut self,
        servers: impl IntoIterator<Item = (String, T)>,
        initialize: &McpInitialize,
        timeouts: McpTimeouts,
        limits: McpLimits,
        cancellation: Arc<AtomicBool>,
    ) -> Vec<McpServerReport> {
        servers
            .into_iter()
            .map(|(name, transport)| {
                self.load_server(
                    &name,
                    transport,
                    initialize,
                    timeouts,
                    limits,
                    Arc::clone(&cancellation),
                )
            })
            .collect()
    }

    pub fn call_tool(
        &mut self,
        qualified_name: &str,
        arguments: Value,
        context: &ToolExecutionContext,
    ) -> Result<ToolOutput, Error> {
        if !self.tools.contains_key(qualified_name)
            && let Some((server_name, _)) = qualified_name.split_once("::")
            && self.configured.contains_key(server_name)
        {
            let previous = Arc::clone(&self.discovery_cancellation);
            self.discovery_cancellation = context.cancellation_handle();
            let _ = self.discover_server(server_name);
            self.discovery_cancellation = previous;
        }
        let metadata = self
            .tools
            .get(qualified_name)
            .cloned()
            .ok_or_else(|| Error::Tool("unknown MCP tool".into()))?;
        let server_name = metadata.server_name.clone();

        // A connection this registry has held since an earlier turn can be
        // gone without anything having asked it: the process exited, the
        // stream was retired. Noticing before the call keeps the model from
        // paying for a round trip that was never going to arrive.
        if !self.server_is_alive(&server_name) {
            self.reconnect_for(&server_name, context);
        }

        match self.dispatch(
            &server_name,
            &metadata.tool_name,
            arguments.clone(),
            context,
        ) {
            Ok(output) => Ok(output),
            Err(McpTransportError::Cancelled) => Err(Error::Cancelled),
            Err(error) if is_recoverable_call_failure(&error) => {
                if !self.reconnect_for(&server_name, context) {
                    // Recorded after the failed reconnect so the status keeps
                    // the call that actually failed, rather than the connect
                    // attempt made trying to recover from it.
                    self.record_runtime_failure(&server_name, &error);
                    // Distinct from the call's own failure: the connection was
                    // gone and could not be rebuilt, which is the difference
                    // between a server that dropped one call and a server that
                    // is no longer there at all.
                    return Err(McpFailure::new(
                        McpFailureClass::Transport,
                        "server did not restart",
                    )
                    .map_or_else(
                        || mcp_call_error(error),
                        |failure| Error::Extension(failure.error_message()),
                    ));
                }
                match self.dispatch(&server_name, &metadata.tool_name, arguments, context) {
                    Ok(output) => Ok(output),
                    Err(McpTransportError::Cancelled) => Err(Error::Cancelled),
                    Err(error) => {
                        self.record_runtime_failure(&server_name, &error);
                        Err(mcp_call_error(error))
                    }
                }
            }
            Err(error) => {
                self.record_runtime_failure(&server_name, &error);
                Err(mcp_call_error(error))
            }
        }
    }

    fn dispatch(
        &mut self,
        server_name: &str,
        tool_name: &str,
        arguments: Value,
        context: &ToolExecutionContext,
    ) -> Result<ToolOutput, McpTransportError> {
        let client = self
            .clients
            .get_mut(server_name)
            .ok_or_else(|| McpTransportError::Transport("MCP server is not connected".into()))?;
        client.call(tool_name, arguments, context)
    }

    /// Whether this server still holds a connection that can carry a call.
    ///
    /// A server with no client at all counts as alive: it has nothing to
    /// reconnect, and the call fails with `unavailable MCP tool` as before.
    fn server_is_alive(&mut self, server_name: &str) -> bool {
        self.clients
            .get_mut(server_name)
            .is_none_or(|client| client.is_alive())
    }

    /// Rebuilds this server's connection in place and reports whether it came
    /// back.
    ///
    /// The dead client is dropped first so `reload_server` never reports the
    /// failure as `Degraded` on the strength of a connection that no longer
    /// works, and so a successful reload leaves the status handle at `Ready`
    /// rather than stranding a server that recovered on a stale error.
    fn reconnect(&mut self, server_name: &str) -> bool {
        if self.closed || !self.configured.contains_key(server_name) {
            return false;
        }
        if let Some(mut client) = self.clients.remove(server_name) {
            client.close();
        }
        !self.reload_server(server_name).is_failed()
    }

    /// Rebuilds a connection on behalf of one tool call, under that call's
    /// cancellation rather than the registry's own.
    ///
    /// Discovery reads `discovery_cancellation`, a handle that lives as long
    /// as the daemon. A reconnect made for a call spends the caller's budget
    /// on connect and on every page of `tools/list`, so it has to answer to
    /// the same Esc the call does. The previous handle is restored so a later
    /// discovery outside any call keeps its own.
    fn reconnect_for(&mut self, server_name: &str, context: &ToolExecutionContext) -> bool {
        let previous = Arc::clone(&self.discovery_cancellation);
        self.discovery_cancellation = context.cancellation_handle();

        let reconnected = self.reconnect(server_name);

        self.discovery_cancellation = previous;
        reconnected
    }

    /// Records a post-discovery liveness failure without treating a tool
    /// application error as a dead server.
    fn record_runtime_failure(&mut self, server_name: &str, error: &McpTransportError) {
        let category = McpErrorCategory::from(error);
        let report = McpServerReport::Failed {
            server_name: server_name.into(),
            message: sanitized_mcp_load_error(category, McpLoadPhase::Call, error),
        };
        self.record_report(&report, Some((category, McpLoadPhase::Call)));
    }

    fn discover_or_reload(&mut self, server_name: &str) -> McpServerReport {
        self.status.update(server_name, |status| {
            status.state = McpLifecycleState::Connecting
        });
        let Some(configured) = self.configured.get_mut(server_name) else {
            return self.failed(server_name);
        };
        let timeouts = configured.timeouts;
        let limits = configured.limits;
        let transport = configured.factory.as_mut().map_or_else(
            || {
                Err(McpTransportError::Transport(
                    "MCP server is disabled".into(),
                ))
            },
            |factory| factory(),
        );
        match transport {
            Ok(transport) => self.load_server(
                server_name,
                transport,
                &McpInitialize::new(
                    MCP_PROTOCOL_VERSION,
                    Value::Object(Default::default()),
                    "agens",
                    "0.1.0",
                ),
                timeouts,
                limits,
                Arc::clone(&self.discovery_cancellation),
            ),
            Err(error) => {
                let category = McpErrorCategory::from(&error);
                let report = McpServerReport::Failed {
                    server_name: server_name.into(),
                    message: sanitized_mcp_load_error(category, McpLoadPhase::Connect, &error),
                };
                self.record_report(&report, Some((category, McpLoadPhase::Connect)));
                report
            }
        }
    }

    fn failed(&mut self, server_name: &str) -> McpServerReport {
        let report = McpServerReport::Failed {
            server_name: server_name.into(),
            message: "mcp server is unavailable".into(),
        };
        self.record_report(
            &report,
            Some((McpErrorCategory::Unavailable, McpLoadPhase::Connect)),
        );
        report
    }

    fn record_report(
        &mut self,
        report: &McpServerReport,
        failure: Option<(McpErrorCategory, McpLoadPhase)>,
    ) {
        match report {
            McpServerReport::Loaded {
                server_name,
                tool_count,
            } => {
                self.diagnostics.remove(server_name);
                let names = self
                    .tools
                    .values()
                    .filter(|tool| tool.server_name == *server_name)
                    .map(|tool| tool.tool_name.clone())
                    .take(MAX_MCP_STATUS_TOOL_NAMES)
                    .collect::<Vec<_>>();
                self.status.update(server_name, |status| {
                    status.state = McpLifecycleState::Ready;
                    status.tool_count = *tool_count;
                    status.tool_names = names;
                    status.last_error = None;
                });
            }
            McpServerReport::Failed {
                server_name,
                message,
            } => {
                self.diagnostics.insert(
                    server_name.clone(),
                    McpServerDiagnostic {
                        server_name: server_name.clone(),
                        message: message.clone(),
                    },
                );
                // A server keeps the tools a successful discovery registered
                // even once its connection is gone, and those tools are what
                // `degraded` means: still callable once it reconnects, as
                // against a server that never got that far.
                let degraded = self.clients.contains_key(server_name)
                    || self
                        .tools
                        .values()
                        .any(|tool| tool.server_name == *server_name);
                let (category, phase) =
                    failure.unwrap_or((McpErrorCategory::Unavailable, McpLoadPhase::Connect));
                self.status.update(server_name, |status| {
                    status.state = if degraded {
                        McpLifecycleState::Degraded
                    } else {
                        McpLifecycleState::Failed
                    };
                    status.last_error = Some(McpStatusError {
                        category,
                        phase,
                        message: message.clone(),
                    });
                });
            }
        }
    }
}

impl Drop for McpRegistry {
    fn drop(&mut self) {
        self.close();
    }
}

trait McpCallable: Send {
    fn call(
        &mut self,
        tool_name: &str,
        arguments: Value,
        context: &ToolExecutionContext,
    ) -> Result<ToolOutput, McpTransportError>;

    fn is_alive(&mut self) -> bool;

    fn close(&mut self);
}

impl McpTransport for Box<dyn McpTransport> {
    fn execute(
        &mut self,
        request: McpRequest,
        context: &McpOperationContext,
    ) -> Result<McpResponse, McpTransportError> {
        (**self).execute(request, context)
    }

    fn notify(
        &mut self,
        request: McpRequest,
        context: &McpOperationContext,
    ) -> Result<(), McpTransportError> {
        (**self).notify(request, context)
    }

    fn close(&mut self, context: &McpOperationContext) -> Result<(), McpTransportError> {
        (**self).close(context)
    }

    fn is_alive(&mut self) -> bool {
        (**self).is_alive()
    }
}

impl<T: McpTransport> McpCallable for McpClient<T> {
    fn call(
        &mut self,
        tool_name: &str,
        arguments: Value,
        context: &ToolExecutionContext,
    ) -> Result<ToolOutput, McpTransportError> {
        self.call_tool_with_context(tool_name, arguments, &context.mcp_context())
    }

    fn is_alive(&mut self) -> bool {
        self.transport.is_alive()
    }

    fn close(&mut self) {
        Self::close(self);
    }
}

/// Whether a failed tool call is worth one transparent reconnect.
///
/// Only failures of the connection qualify. A protocol error is the server
/// answering badly, not the connection being gone, and reconnecting would
/// replay a call that fails the same way. Cancellation and timeouts belong to
/// the caller's budget, and retrying either would spend it twice.
fn is_recoverable_call_failure(error: &McpTransportError) -> bool {
    matches!(
        error,
        McpTransportError::Transport(_) | McpTransportError::RetriesExhausted
    )
}

fn mcp_call_error(error: McpTransportError) -> Error {
    match error {
        McpTransportError::Cancelled => Error::Cancelled,
        McpTransportError::TimedOut => Error::Tool("mcp operation timed out".into()),
        McpTransportError::RetriesExhausted
        | McpTransportError::Protocol(_)
        | McpTransportError::Transport(_)
        | McpTransportError::HttpStatus(_) => {
            Error::Extension(mcp_call_failure(&error).map_or_else(
                || agens_core::mcp_failure::FIXED_ERROR_MESSAGE.to_owned(),
                |failure| failure.error_message(),
            ))
        }
    }
}

/// The cause of a failed call, in the vocabulary every reader downstream
/// shares.
///
/// Built from the transport error's own category rather than from its message:
/// a `Transport` error carries text agens did not author, and the model-visible
/// result and the diagnostics record are both sinks that text may not reach.
fn mcp_call_failure(error: &McpTransportError) -> Option<McpFailure> {
    let (class, detail) = match error {
        McpTransportError::RetriesExhausted => (
            McpFailureClass::RetriesExhausted,
            "server did not respond".to_owned(),
        ),
        McpTransportError::Protocol(_) => (
            McpFailureClass::Protocol,
            "server response rejected".to_owned(),
        ),
        McpTransportError::HttpStatus(status) => {
            (McpFailureClass::HttpStatus, format!("http status {status}"))
        }
        McpTransportError::Transport(_) => (McpFailureClass::Transport, "call failed".to_owned()),
        McpTransportError::Cancelled | McpTransportError::TimedOut => return None,
    };

    McpFailure::new(class, &detail)
}

const MCP_DUPLICATE_TOOL_NAMES_REASON: &str = "protocol: duplicate tool names";

/// Renders a load failure as a sanitized, agens-authored reason.
///
/// The result never interpolates remote text (bodies, headers, or messages
/// from the MCP server) — only the error category, the phase agens itself
/// observed the failure in, and, for HTTP status failures, the numeric status
/// code seen on the wire.
///
/// Timeouts name their phase because connect and tool listing hold separate
/// budgets: a `tools/list` timeout reported as a connect timeout would send an
/// operator to verify a handshake that already succeeded.
fn sanitized_mcp_load_error(
    category: McpErrorCategory,
    phase: McpLoadPhase,
    error: &McpTransportError,
) -> String {
    match (category, phase, error) {
        (McpErrorCategory::Cancelled, McpLoadPhase::Connect, _) => {
            "cancelled: connect cancelled".into()
        }
        (McpErrorCategory::Cancelled, McpLoadPhase::ListTools, _) => {
            "cancelled: tool listing cancelled".into()
        }
        (McpErrorCategory::Cancelled, McpLoadPhase::Call, _) => "cancelled: call cancelled".into(),
        (McpErrorCategory::Timeout, McpLoadPhase::Connect, _) => {
            "timeout: connect timed out".into()
        }
        (McpErrorCategory::Timeout, McpLoadPhase::ListTools, _) => {
            "timeout: tool listing timed out; raise timeout_ms".into()
        }
        (McpErrorCategory::Timeout, McpLoadPhase::Call, _) => "timeout: tool call timed out".into(),
        (McpErrorCategory::RetriesExhausted, _, _) => {
            "retries_exhausted: server did not respond".into()
        }
        (McpErrorCategory::Protocol, _, _) => "protocol: server response rejected".into(),
        (McpErrorCategory::Transport, _, McpTransportError::HttpStatus(status)) => {
            format!("transport: http status {status}")
        }
        (McpErrorCategory::Transport, McpLoadPhase::Connect, _) => {
            "transport: connection failed".into()
        }
        (McpErrorCategory::Transport, McpLoadPhase::ListTools, _) => {
            "transport: tool listing failed".into()
        }
        (McpErrorCategory::Transport, McpLoadPhase::Call, _) => "transport: call failed".into(),
        (McpErrorCategory::Unavailable, _, _) => "mcp server load failed; reload to retry".into(),
    }
}

#[cfg(test)]
mod mcp_sanitized_error_tests {
    use super::*;

    #[test]
    fn sanitized_mcp_load_error_covers_the_closed_reason_set_without_leaking_remote_text() {
        let cases = [
            (
                McpErrorCategory::Cancelled,
                McpLoadPhase::Connect,
                McpTransportError::Cancelled,
                "cancelled: connect cancelled",
            ),
            (
                McpErrorCategory::Cancelled,
                McpLoadPhase::ListTools,
                McpTransportError::Cancelled,
                "cancelled: tool listing cancelled",
            ),
            (
                McpErrorCategory::Cancelled,
                McpLoadPhase::Call,
                McpTransportError::Cancelled,
                "cancelled: call cancelled",
            ),
            (
                McpErrorCategory::Timeout,
                McpLoadPhase::Connect,
                McpTransportError::TimedOut,
                "timeout: connect timed out",
            ),
            (
                McpErrorCategory::Timeout,
                McpLoadPhase::ListTools,
                McpTransportError::TimedOut,
                "timeout: tool listing timed out; raise timeout_ms",
            ),
            (
                McpErrorCategory::Timeout,
                McpLoadPhase::Call,
                McpTransportError::TimedOut,
                "timeout: tool call timed out",
            ),
            (
                McpErrorCategory::RetriesExhausted,
                McpLoadPhase::Connect,
                McpTransportError::RetriesExhausted,
                "retries_exhausted: server did not respond",
            ),
            (
                McpErrorCategory::Protocol,
                McpLoadPhase::ListTools,
                McpTransportError::Protocol("SENTINEL_SECRET body".into()),
                "protocol: server response rejected",
            ),
            (
                McpErrorCategory::Transport,
                McpLoadPhase::Connect,
                McpTransportError::Transport("SENTINEL_SECRET body".into()),
                "transport: connection failed",
            ),
            (
                McpErrorCategory::Transport,
                McpLoadPhase::ListTools,
                McpTransportError::Transport("SENTINEL_SECRET body".into()),
                "transport: tool listing failed",
            ),
            (
                McpErrorCategory::Transport,
                McpLoadPhase::Call,
                McpTransportError::Transport("SENTINEL_SECRET body".into()),
                "transport: call failed",
            ),
            (
                McpErrorCategory::Transport,
                McpLoadPhase::Connect,
                McpTransportError::HttpStatus(406),
                "transport: http status 406",
            ),
        ];

        for (category, phase, error, expected) in cases {
            let message = sanitized_mcp_load_error(category, phase, &error);
            assert_eq!(message, expected);
            assert!(!message.contains("SENTINEL_SECRET"));
        }
    }
}

/// A load failure together with the startup phase it happened in.
///
/// The two phases hold independent budgets, so collapsing them would report a
/// `tools/list` failure as a connect failure and point the operator at the
/// wrong knob.
struct McpLoadFailure {
    phase: McpLoadPhase,
    error: McpTransportError,
}

impl McpLoadFailure {
    fn connect(error: McpTransportError) -> Self {
        Self {
            phase: McpLoadPhase::Connect,
            error,
        }
    }

    fn list_tools(error: McpTransportError) -> Self {
        Self {
            phase: McpLoadPhase::ListTools,
            error,
        }
    }
}

fn load_server_client<T: McpTransport>(
    server_name: &str,
    transport: T,
    initialize: McpInitialize,
    timeouts: McpTimeouts,
    limits: McpLimits,
    cancellation: Arc<AtomicBool>,
) -> Result<(Vec<RemoteToolMetadata>, McpClient<T>), McpLoadFailure> {
    validate_server_name(server_name).map_err(McpLoadFailure::connect)?;

    let mut client = McpClient::new(transport, timeouts, limits);

    let result = client
        .connect(initialize, &cancellation)
        .map_err(McpLoadFailure::connect)
        .and_then(|_| {
            client
                .list_tools(&cancellation)
                .map_err(McpLoadFailure::list_tools)
        })
        .and_then(|tools| {
            tools
                .into_iter()
                .map(|tool| remote_tool_metadata(server_name, tool))
                .collect::<Result<Vec<_>, _>>()
                .map_err(McpLoadFailure::list_tools)
        });

    match result {
        Ok(metadata) => Ok((metadata, client)),
        Err(failure) => {
            client.close();
            Err(failure)
        }
    }
}

pub struct McpClient<T: McpTransport> {
    transport: T,
    timeouts: McpTimeouts,
    limits: McpLimits,
}

impl<T: McpTransport> McpClient<T> {
    pub fn new(transport: T, timeouts: McpTimeouts, limits: McpLimits) -> Self {
        Self {
            transport,
            timeouts,
            limits,
        }
    }
    pub fn into_transport(self) -> T {
        self.transport
    }

    pub fn connect(
        &mut self,
        initialize: McpInitialize,
        cancellation: &Arc<AtomicBool>,
    ) -> Result<(), McpTransportError> {
        let context = McpOperationContext::new(Arc::clone(cancellation), self.timeouts.connect);
        let initialized = expect_initialized(
            self.request(McpRequest::Initialize(initialize.clone()), &context)?,
        )?;
        if !SUPPORTED_MCP_PROTOCOL_VERSIONS.contains(&initialized.protocol_version.as_str()) {
            return Err(McpTransportError::Protocol(
                "MCP protocol version negotiation failed".into(),
            ));
        }
        if !initialized.capabilities.is_object()
            || !initialized
                .capabilities
                .get("tools")
                .is_some_and(Value::is_object)
        {
            return Err(McpTransportError::Protocol(
                "MCP server does not advertise tools capability".into(),
            ));
        }
        self.notify(McpRequest::Initialized, &context)
    }

    pub fn list_tools(
        &mut self,
        cancellation: &Arc<AtomicBool>,
    ) -> Result<Vec<McpToolDefinition>, McpTransportError> {
        let context = McpOperationContext::new(Arc::clone(cancellation), self.timeouts.list);
        let mut cursor = None;
        let mut seen = std::collections::BTreeSet::new();
        let mut tools = Vec::new();
        for _ in 0..self.limits.max_list_pages {
            let McpResponse::ToolsListed(page) = self.request(
                McpRequest::ListTools {
                    cursor: cursor.clone(),
                },
                &context,
            )?
            else {
                return Err(McpTransportError::Protocol(
                    "expected tools/list result".into(),
                ));
            };
            if tools.len().saturating_add(page.tools.len()) > self.limits.max_tools {
                return Err(McpTransportError::Protocol(
                    "MCP tools/list tool limit exceeded".into(),
                ));
            }
            tools.extend(page.tools);
            match page.next_cursor {
                Some(next) if next.is_empty() || !seen.insert(next.clone()) => {
                    return Err(McpTransportError::Protocol(
                        "MCP tools/list cursor loop detected".into(),
                    ));
                }
                Some(next) => cursor = Some(next),
                None => return Ok(tools),
            }
        }
        Err(McpTransportError::Protocol(
            "MCP tools/list page limit exceeded".into(),
        ))
    }

    pub fn call_tool(
        &mut self,
        name: impl Into<String>,
        arguments: Value,
        cancellation: &Arc<AtomicBool>,
    ) -> Result<ToolOutput, McpTransportError> {
        if !arguments.is_object() {
            return Ok(ToolOutput::failure(
                "mcp: tool arguments must be a JSON object",
            ));
        }
        let context = McpOperationContext::new(Arc::clone(cancellation), self.timeouts.call);
        match self.request(
            McpRequest::CallTool {
                name: name.into(),
                arguments,
            },
            &context,
        )? {
            McpResponse::ToolCalled(result) => Ok(map_call_result(result)),
            McpResponse::ProtocolError(_) => Ok(ToolOutput::failure("mcp protocol failure")),
            _ => Err(McpTransportError::Protocol(
                "expected tools/call result".into(),
            )),
        }
    }

    fn call_tool_with_context(
        &mut self,
        name: impl Into<String>,
        arguments: Value,
        context: &McpOperationContext,
    ) -> Result<ToolOutput, McpTransportError> {
        if !arguments.is_object() {
            return Ok(ToolOutput::failure(
                "mcp: tool arguments must be a JSON object",
            ));
        }
        let context = McpOperationContext {
            cancellation: context.cancellation.as_ref().map(Arc::clone),
            headless_cancellation: context.headless_cancellation.clone(),
            deadline: Some(
                context
                    .deadline
                    .map_or(Instant::now() + self.timeouts.call, |deadline| {
                        deadline.min(Instant::now() + self.timeouts.call)
                    }),
            ),
        };
        match self.request(
            McpRequest::CallTool {
                name: name.into(),
                arguments,
            },
            &context,
        )? {
            McpResponse::ToolCalled(result) => Ok(map_call_result(result)),
            McpResponse::ProtocolError(_) => Ok(ToolOutput::failure("mcp protocol failure")),
            _ => Err(McpTransportError::Protocol(
                "expected tools/call result".into(),
            )),
        }
    }

    fn request(
        &mut self,
        request: McpRequest,
        context: &McpOperationContext,
    ) -> Result<McpResponse, McpTransportError> {
        if let Err(error @ (McpTransportError::Cancelled | McpTransportError::TimedOut)) =
            context.remaining()
        {
            return self.abort(context, error);
        }
        match self.transport.execute(request, context) {
            Ok(response) => match context.check() {
                Ok(()) => Ok(response),
                Err(error @ (McpTransportError::Cancelled | McpTransportError::TimedOut)) => {
                    self.abort(context, error)
                }
                Err(error) => Err(error),
            },
            Err(error @ (McpTransportError::Cancelled | McpTransportError::TimedOut)) => {
                self.abort(context, error)
            }
            Err(error) => Err(error),
        }
    }

    fn notify(
        &mut self,
        request: McpRequest,
        context: &McpOperationContext,
    ) -> Result<(), McpTransportError> {
        if let Err(error @ (McpTransportError::Cancelled | McpTransportError::TimedOut)) =
            context.remaining()
        {
            return self.abort_notification(context, error);
        }
        match self.transport.notify(request, context) {
            Ok(()) => match context.check() {
                Ok(()) => Ok(()),
                Err(error @ (McpTransportError::Cancelled | McpTransportError::TimedOut)) => {
                    self.abort_notification(context, error)
                }
                Err(error) => Err(error),
            },
            Err(error @ (McpTransportError::Cancelled | McpTransportError::TimedOut)) => {
                self.abort_notification(context, error)
            }
            Err(error) => Err(error),
        }
    }

    /// Ends an operation the caller stopped waiting for.
    ///
    /// Only a deadline takes the connection down with it: a server that did
    /// not answer inside its budget is a server the next call should not be
    /// handed. Cancellation says nothing about the server, so it keeps the
    /// connection and the transport abandons the pending request on its own.
    fn abort(
        &mut self,
        context: &McpOperationContext,
        primary: McpTransportError,
    ) -> Result<McpResponse, McpTransportError> {
        self.close_on_deadline(context, &primary);
        Err(primary)
    }

    fn abort_notification(
        &mut self,
        context: &McpOperationContext,
        primary: McpTransportError,
    ) -> Result<(), McpTransportError> {
        self.close_on_deadline(context, &primary);
        Err(primary)
    }

    fn close_on_deadline(&mut self, context: &McpOperationContext, primary: &McpTransportError) {
        if matches!(primary, McpTransportError::TimedOut) {
            let _ = self.transport.close(context);
        }
    }
    /// Shuts the transport down, budgeting the shutdown with the connect
    /// timeout: `McpTimeouts` carries no separate close budget, and a server
    /// slow to start is the same server slow to stop.
    fn close(&mut self) {
        let context =
            McpOperationContext::new(Arc::new(AtomicBool::new(false)), self.timeouts.connect);
        let _ = self.transport.close(&context);
    }
}

fn terminal_mcp_error(error: &Error) -> bool {
    match error {
        Error::Cancelled => true,
        Error::Tool(message) => message == "mcp operation timed out",
        Error::Extension(message) => {
            message == agens_core::mcp_failure::FIXED_ERROR_MESSAGE
                || McpFailure::from_error_message(message).is_some()
        }
        _ => false,
    }
}

fn expect_initialized(response: McpResponse) -> Result<McpInitializeResult, McpTransportError> {
    match response {
        McpResponse::Initialized(result) => Ok(result),
        McpResponse::ProtocolError(_) => Err(McpTransportError::Protocol(
            "MCP initialize protocol failure".into(),
        )),
        _ => Err(McpTransportError::Protocol(
            "expected initialize result".into(),
        )),
    }
}

fn map_call_result(result: McpCallResult) -> ToolOutput {
    let content = truncate_mcp_tool_content(
        result
            .content
            .into_iter()
            .map(|block| match block {
                McpContentBlock::Text(text) | McpContentBlock::NonText(text) => text,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    );

    if result.is_error {
        ToolOutput::failure(content)
    } else {
        ToolOutput::success(content)
    }
}

/// Bounds what a single MCP tool result can put in front of the model.
///
/// A server is free to answer with megabytes — a whole file, a base64 asset,
/// a full page of search results — and failing the call over its size loses
/// the answer entirely. Native tools already truncate on the same budget
/// (`MAX_PROCESS_OUTPUT`), and a truncated answer with a marker is worth more
/// to the model than an infrastructure failure.
fn truncate_mcp_tool_content(mut content: String) -> String {
    if content.len() <= MAX_MCP_TOOL_OUTPUT {
        return content;
    }

    let mut end = MAX_MCP_TOOL_OUTPUT - MCP_TRUNCATED_MARKER.len();
    while !content.is_char_boundary(end) {
        end -= 1;
    }

    content.truncate(end);
    content.push_str(MCP_TRUNCATED_MARKER);
    content
}

fn remote_tool_metadata(
    server_name: &str,
    tool: McpToolDefinition,
) -> Result<RemoteToolMetadata, McpTransportError> {
    if tool.name.is_empty() {
        return Err(McpTransportError::Protocol(
            "MCP tool name is required".into(),
        ));
    }
    if !tool.input_schema.is_object()
        || tool.input_schema.get("type") != Some(&Value::String("object".into()))
    {
        return Err(McpTransportError::Protocol(format!(
            "MCP tool {} inputSchema must be a JSON Schema object with type object",
            tool.name
        )));
    }

    let qualified_name = format!("{server_name}::{}", tool.name);
    Ok(RemoteToolMetadata {
        qualified_name,
        server_name: server_name.into(),
        tool_name: tool.name,
        description: tool.description,
        input_schema: tool.input_schema,
        access: if tool.annotations.read_only_hint == Some(true) {
            RemoteToolAccess::ReadOnly
        } else {
            RemoteToolAccess::Write
        },
    })
}

fn validate_server_name(server_name: &str) -> Result<(), McpTransportError> {
    if server_name.is_empty() || server_name.contains("::") {
        return Err(McpTransportError::Protocol(
            "MCP server name must be non-empty and cannot contain ::".into(),
        ));
    }
    Ok(())
}

fn has_duplicate_qualified_name(metadata: &[RemoteToolMetadata]) -> bool {
    metadata.iter().enumerate().any(|(index, tool)| {
        metadata[index + 1..]
            .iter()
            .any(|other| other.qualified_name == tool.qualified_name)
    })
}

#[derive(Clone, Debug)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    task_terminal: Option<HeadlessTaskTerminal>,
    facts: Option<ToolResultFacts>,
}

impl PartialEq for ToolOutput {
    fn eq(&self, other: &Self) -> bool {
        self.content == other.content && self.is_error == other.is_error
    }
}

impl Eq for ToolOutput {}

impl ToolOutput {
    pub fn task_terminal(terminal: HeadlessTaskTerminal) -> Self {
        Self {
            content: terminal.message().to_owned(),
            is_error: true,
            task_terminal: Some(terminal),
            facts: None,
        }
    }

    pub fn terminal(&self) -> Option<HeadlessTaskTerminal> {
        self.task_terminal
    }

    #[must_use]
    pub fn with_facts(mut self, facts: ToolResultFacts) -> Self {
        self.facts = Some(facts);
        self
    }

    pub fn facts(&self) -> Option<&ToolResultFacts> {
        self.facts.as_ref()
    }
}

/// A tool as the dispatcher holds it: what a call projects onto for a
/// permission decision, and how it runs once that decision allows it.
///
/// # Which error a projection fails with
///
/// The two projection methods carry a reserved distinction, because the variant
/// they return decides what the model is told and therefore what it does next:
///
/// - [`Error::Tool`] means **the arguments are malformed**. The model reads
///   this as `invalid tool arguments`, which invites it to rewrite them and
///   call again. Use it whenever rewriting the arguments could produce a call
///   that works.
/// - [`Error::Permission`] means **the arguments are well formed and no
///   permission decision can be made for this call at all** — the tool is
///   misconfigured in this session, not misused. The model is told so in its
///   own words, and told that repeating the call will not change it. Nothing
///   the model can write repairs this, so it must never arrive wearing the
///   wording that asks it to try.
///
/// Every other error variant is classified with [`Error::Tool`]. A projection
/// that cannot decide must fail one of these two ways rather than answer: an
/// empty target or an empty reach is a positive claim that the call touches
/// nothing, and every rule written against it stops selecting the call while
/// the call goes on doing what it does.
pub trait DispatchTool: Send {
    /// Projects the exact execution arguments into the permission target.
    ///
    /// See the trait's own documentation for which error variant to fail with:
    /// the choice is part of this method's contract, and the default body's
    /// [`Error::Tool`] is the malformed-arguments case rather than the general
    /// one.
    fn permission_target(&self, arguments: &Value) -> Result<String, Error> {
        arguments
            .get("target")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| Error::Tool("tool target is required".into()))
    }

    /// Projects what the exact execution arguments let the call reach beyond
    /// [`Self::permission_target`]. A tool named by the one thing it touches
    /// reaches nothing else, which is why this defaults to empty.
    ///
    /// Fails under the same contract as [`Self::permission_target`].
    fn permission_reach(&self, _arguments: &Value) -> Result<Vec<PermissionReach>, Error> {
        Ok(Vec::new())
    }

    fn execute(
        &mut self,
        context: &ToolExecutionContext,
        arguments: Value,
    ) -> Result<ToolOutput, Error>;
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolDispatchRequest {
    project_id: String,
    qualified_tool_name: String,
    arguments: Value,
}

impl ToolDispatchRequest {
    pub fn new(
        project_id: impl Into<String>,
        qualified_tool_name: impl Into<String>,
        arguments: Value,
    ) -> Self {
        Self {
            project_id: project_id.into(),
            qualified_tool_name: qualified_tool_name.into(),
            arguments,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionPromptContext {
    pub project_id: String,
    /// The dispatcher's own identity for the tool (`native:4:bash`), which is
    /// what policy compares against and what a grant is stored under.
    ///
    /// It is deliberately not a qualified name: it equals no spelling a rule or
    /// a person writes, so anything deciding on it — a redaction, a display —
    /// has to reduce it through [`agens_core::bare_tool_name`] first.
    pub tool_identity: String,
    pub target_identifier: String,
    pub access: ToolAccess,
    pub reason: String,
}

impl PermissionPromptContext {
    fn from_request(request: &PermissionRequest) -> Self {
        Self {
            project_id: request.project.clone(),
            tool_identity: request.tool.clone(),
            target_identifier: request.target.clone(),
            access: request.access,
            reason: "permission policy requires confirmation".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolDispatchOutcome {
    Denied,
    PromptRequired(PermissionPromptContext),
    Executed(ToolOutput),
}

#[derive(Debug)]
pub enum ToolEvaluationOutcome {
    Denied,
    PromptRequired(PermissionPromptContext),
    Authorized(AuthorizedToolCall),
}

/// Shared identity / target projection for policy evaluate and post-prompt allow.
struct PreparedEvaluation {
    dispatcher_id: u64,
    registration_version: u64,
    identity: ToolIdentity,
    access: ToolAccess,
    arguments: serde_json::Value,
    policy: PermissionPolicy,
    grants: Vec<ProjectPermissionGrant>,
    permission: PermissionRequest,
}

impl PreparedEvaluation {
    fn into_authorized_call(self, authority: PermissionAuthority) -> AuthorizedToolCall {
        let read_filter = PermissionReadFilter::new(
            self.policy,
            self.grants,
            self.permission.project.clone(),
            self.identity.0.clone(),
            self.permission.access,
        );

        AuthorizedToolCall {
            dispatcher_id: self.dispatcher_id,
            registration_version: self.registration_version,
            identity: self.identity,
            projected_target: self.permission.target,
            access: self.access,
            arguments_digest: digest_arguments(&self.arguments),
            arguments: self.arguments,
            read_filter,
            authority,
        }
    }
}

/// Opaque proof that a specific registered tool was authorized for one request.
/// Its fields are deliberately private: callers cannot construct or alter a call.
#[derive(Debug)]
pub struct AuthorizedToolCall {
    dispatcher_id: u64,
    registration_version: u64,
    identity: ToolIdentity,
    projected_target: String,
    access: ToolAccess,
    arguments: Value,
    arguments_digest: u64,
    /// The rules that authorized this call, kept so a tool that reads a whole
    /// file set can ask them about each file. See [`PermissionReadFilter`].
    read_filter: PermissionReadFilter,
    /// Whether a decision authorized this call or dangerous mode's fallback
    /// did. See [`PermissionAuthority`].
    authority: PermissionAuthority,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ToolIdentity(String);

impl ToolIdentity {
    fn native(name: &str) -> Self {
        Self(format!("native:{}:{name}", name.len()))
    }

    fn mcp(server: &str, tool: &str) -> Self {
        Self(format!(
            "mcp:{}:{server}:{}:{tool}",
            server.len(),
            tool.len()
        ))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

struct RegisteredDispatchTool {
    access: ToolAccess,
    version: u64,
    tool: Box<dyn DispatchTool>,
}

pub struct ToolDispatcher {
    tools: BTreeMap<ToolIdentity, RegisteredDispatchTool>,
    aliases: BTreeMap<String, ToolIdentity>,
    declared_mcp_servers: BTreeSet<String>,
    dispatcher_id: u64,
    next_version: u64,
}

impl ToolDispatcher {
    pub fn new() -> Self {
        static NEXT_DISPATCHER_ID: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(1);
        static PROCESS_NONCE: std::sync::LazyLock<u64> = std::sync::LazyLock::new(|| {
            use std::hash::{BuildHasher, Hasher};

            std::collections::hash_map::RandomState::new()
                .build_hasher()
                .finish()
        });
        Self {
            dispatcher_id: *PROCESS_NONCE ^ NEXT_DISPATCHER_ID.fetch_add(1, Ordering::AcqRel),
            next_version: 1,
            tools: BTreeMap::new(),
            aliases: BTreeMap::new(),
            declared_mcp_servers: BTreeSet::new(),
        }
    }

    pub fn register_native(
        &mut self,
        name: impl Into<String>,
        access: ToolAccess,
        tool: impl DispatchTool + 'static,
    ) -> Result<(), Error> {
        let name = name.into();
        let native_name = name
            .strip_prefix("native::")
            .filter(|name| !name.is_empty())
            .ok_or_else(|| Error::Tool("native tool name is invalid".into()))?
            .to_owned();
        let version = self.allocate_version();
        self.insert(
            ToolIdentity::native(&native_name),
            [name, native_name],
            access,
            version,
            tool,
        );
        Ok(())
    }

    pub fn register_mcp(
        &mut self,
        metadata: &RemoteToolMetadata,
        tool: impl DispatchTool + 'static,
    ) -> Result<(), Error> {
        let version = self.allocate_version();
        self.insert(
            ToolIdentity::mcp(&metadata.server_name, &metadata.tool_name),
            [
                metadata.qualified_name.clone(),
                format!("{}_{}", metadata.server_name, metadata.tool_name),
            ],
            remote_tool_access(metadata.access),
            version,
            tool,
        );
        Ok(())
    }

    /// Removes a server's MCP registrations and invalidates their outstanding handles.
    pub fn remove_mcp_server(&mut self, server_name: &str) {
        let identities = self
            .tools
            .keys()
            .filter(|identity| {
                identity
                    .0
                    .starts_with(&format!("mcp:{}:{server_name}:", server_name.len()))
            })
            .cloned()
            .collect::<Vec<_>>();

        for identity in identities {
            self.aliases.retain(|_, current| current != &identity);
            self.tools.remove(&identity);
        }
    }

    /// Replaces an existing native implementation while invalidating prior authorizations.
    pub fn replace_native(
        &mut self,
        name: impl Into<String>,
        access: ToolAccess,
        tool: impl DispatchTool + 'static,
    ) {
        let name = name.into();
        let Some(native_name) = name
            .strip_prefix("native::")
            .filter(|name| !name.is_empty())
            .map(ToOwned::to_owned)
        else {
            return;
        };
        let version = self.allocate_version();
        self.insert(
            ToolIdentity::native(&native_name),
            [name, native_name],
            access,
            version,
            tool,
        );
    }

    pub fn canonical_identity(&self, alias: &str) -> Option<&ToolIdentity> {
        self.aliases.get(alias)
    }

    /// Records the MCP servers this session's configuration names, whether or
    /// not any of them was ever reached.
    ///
    /// A configured rule may name a remote tool by either of the two names it
    /// answers to, and only one of them says so on its own. `<server>::<tool>`
    /// is self-identifying; `<server>_<tool>` — the name the model is actually
    /// advertised, and the one `register_mcp` installs alongside it — is shaped
    /// exactly like a bare native name. Nothing in `engram_mem_save`
    /// distinguishes it from a misspelt `webfetc`, so a caller resolving it has
    /// to ask what this session set out to run rather than what it reached.
    pub fn declare_mcp_servers(&mut self, servers: impl IntoIterator<Item = String>) {
        self.declared_mcp_servers
            .extend(servers.into_iter().filter(|server| !server.is_empty()));
    }

    /// Whether this session's configuration names `server` at all.
    pub fn declares_mcp_server(&self, server: &str) -> bool {
        self.declared_mcp_servers.contains(server)
    }

    /// Every server name this session's configuration declares.
    ///
    /// A caller resolving a `<server>_<tool>` name has no separator to split on
    /// and cannot recover the server from the name alone — a server called `a`
    /// serving `b_c` and a server called `a_b` serving `c` are advertised under
    /// the same name — so it matches the name against the declared set instead.
    pub fn declared_mcp_servers(&self) -> impl Iterator<Item = &str> {
        self.declared_mcp_servers.iter().map(String::as_str)
    }

    /// Whether this dispatcher holds any tool served by `server`.
    ///
    /// A caller resolving a configured rule that names a remote tool needs to
    /// tell a misspelt tool name — which a live server's own surface can refuse
    /// — from a name for a server that is not here at all, which nothing here
    /// can answer for. Discovery is what puts a server's tools in this map, so a
    /// server absent from it is a server this session never reached.
    pub fn holds_mcp_server(&self, server: &str) -> bool {
        let prefix = format!("mcp:{}:{server}:", server.len());

        self.tools
            .keys()
            .any(|identity| identity.0.starts_with(&prefix))
    }

    /// The qualified names of every native tool registered here.
    ///
    /// A dispatcher is the only authority on what it holds. Deriving the native
    /// surface from a catalog instead misses every tool registered directly
    /// beside one — which is exactly how a tool arrives on the surface without
    /// being classified against the rules that are supposed to reach it.
    pub fn registered_native_names(&self) -> Vec<String> {
        self.tools
            .keys()
            .filter_map(|identity| {
                identity
                    .0
                    .strip_prefix("native:")
                    .and_then(|rest| rest.split_once(':'))
                    .filter(|(length, name)| {
                        length
                            .parse::<usize>()
                            .is_ok_and(|length| length == name.len())
                    })
                    .map(|(_, name)| format!("native::{name}"))
            })
            .collect()
    }

    /// The access class one registered native was registered under.
    ///
    /// Paired with [`Self::registered_native_names`], this describes a
    /// dispatcher's native surface without reference to any catalog — which is
    /// what a harness modelling the production surface needs in order to model
    /// it rather than remember it.
    pub fn native_access(&self, qualified_name: &str) -> Option<ToolAccess> {
        let identity = self.canonical_identity(qualified_name)?;

        self.tools.get(identity).map(|registered| registered.access)
    }

    pub(crate) fn capability_snapshot(&self) -> capabilities::CapabilitySnapshot {
        capabilities::CapabilitySnapshot {
            identities: self
                .tools
                .keys()
                .map(|identity| identity.0.clone())
                .collect(),
            aliases: self
                .aliases
                .iter()
                .map(|(alias, identity)| (alias.clone(), identity.0.clone()))
                .collect(),
        }
    }

    pub fn evaluate(
        &self,
        policy: &PermissionPolicy,
        grants: &[ProjectPermissionGrant],
        session: &PermissionSession,
        request: ToolDispatchRequest,
    ) -> Result<ToolEvaluationOutcome, Error> {
        self.evaluate_with_policy_override(policy, grants, session, request, false)
    }

    /// Authorizes a call the person already approved at the interactive prompt.
    ///
    /// Peer agents treat human Allow as the decision: they do not re-run policy
    /// and re-ask. Agens still applies hard safety (worktree escape, chat-mode
    /// write ban, configured global deny). Soft `ask` rules, configured floors,
    /// and grant-matching quirks must not turn an explicit Allow into
    /// `PromptRequired` again — that is what freezes the agent on "approval
    /// could not be completed" after the person already said yes.
    pub fn authorize_after_human_approval(
        &self,
        policy: &PermissionPolicy,
        request: ToolDispatchRequest,
    ) -> Result<ToolEvaluationOutcome, Error> {
        let prepared = self.prepare_evaluation(policy, &[], request)?;
        if !prepared.policy.hard_safety_allows(&prepared.permission) {
            return Ok(ToolEvaluationOutcome::Denied);
        }

        Ok(ToolEvaluationOutcome::Authorized(
            prepared.into_authorized_call(PermissionAuthority::Decided),
        ))
    }

    /// Evaluates identity, arguments, target projection, and hard safety
    /// before consulting policy.
    ///
    /// `unmatched_allow` never overrides a matched rule or grant decision —
    /// a declared `deny` or `ask` stays exactly that, even when this flag is
    /// set. It only decides what happens when nothing matched at all, which
    /// is the same fallback role `temporary_bypass` plays in the policy
    /// layer.
    pub fn evaluate_with_policy_override(
        &self,
        policy: &PermissionPolicy,
        grants: &[ProjectPermissionGrant],
        session: &PermissionSession,
        request: ToolDispatchRequest,
        unmatched_allow: bool,
    ) -> Result<ToolEvaluationOutcome, Error> {
        let prepared = self.prepare_evaluation(policy, grants, request)?;

        if !prepared.policy.hard_safety_allows(&prepared.permission) {
            return Ok(ToolEvaluationOutcome::Denied);
        }

        let (decision, authority) = prepared.policy.evaluate_with_unmatched_authority(
            &prepared.permission,
            &prepared.grants,
            &[],
            session,
            unmatched_allow,
        );

        match decision {
            PermissionDecision::Deny => Ok(ToolEvaluationOutcome::Denied),
            PermissionDecision::Ask => Ok(ToolEvaluationOutcome::PromptRequired(
                PermissionPromptContext::from_request(&prepared.permission),
            )),
            PermissionDecision::Allow => Ok(ToolEvaluationOutcome::Authorized(
                prepared.into_authorized_call(authority),
            )),
        }
    }

    fn prepare_evaluation(
        &self,
        policy: &PermissionPolicy,
        grants: &[ProjectPermissionGrant],
        request: ToolDispatchRequest,
    ) -> Result<PreparedEvaluation, Error> {
        let identity = self
            .aliases
            .get(&request.qualified_tool_name)
            .ok_or_else(|| Error::Tool("unknown tool".into()))?
            .clone();
        let registered = self
            .tools
            .get(&identity)
            .ok_or_else(|| Error::Tool("unknown tool".into()))?;
        let policy = policy.normalized_tool_aliases(|name| {
            self.aliases.get(name).map(|identity| identity.0.clone())
        });
        let grants = agens_core::normalize_project_permission_grants(grants, |name| {
            self.aliases.get(name).map(|identity| identity.0.clone())
        });
        let target = registered.tool.permission_target(&request.arguments)?;
        let reach = registered.tool.permission_reach(&request.arguments)?;
        let permission = PermissionRequest::reaching(
            request.project_id,
            identity.0.clone(),
            target,
            registered.access,
            &reach,
        );
        let grants = if permission.project.trim().is_empty() {
            Vec::new()
        } else {
            grants
        };

        Ok(PreparedEvaluation {
            dispatcher_id: self.dispatcher_id,
            registration_version: registered.version,
            identity,
            access: registered.access,
            arguments: request.arguments,
            policy,
            grants,
            permission,
        })
    }

    pub fn execute(
        &mut self,
        handle: AuthorizedToolCall,
        context: &ToolExecutionContext,
    ) -> Result<ToolOutput, Error> {
        if handle.dispatcher_id != self.dispatcher_id {
            return Err(Error::Tool("invalid authorized tool call".into()));
        }
        if let Err(status) = context.check() {
            return Ok(sanitized_execution_status(status));
        }

        let registered = self
            .tools
            .get_mut(&handle.identity)
            .ok_or_else(|| Error::Tool("stale authorized tool call".into()))?;
        if registered.version != handle.registration_version
            || registered.access != handle.access
            || digest_arguments(&handle.arguments) != handle.arguments_digest
            || handle.projected_target.is_empty() && handle.access == ToolAccess::Write
        {
            return Err(Error::Tool("stale authorized tool call".into()));
        }

        let context = context
            .clone()
            .with_read_filter(handle.read_filter)
            .with_authority(handle.authority);

        match registered.tool.execute(&context, handle.arguments) {
            Ok(output) => {
                // A deadline that expired while the tool ran does not unmake
                // its result: the work is already paid for and the caller is
                // better served by it than by a failure. Reporting a timeout
                // over a finished call is how a long subagent came back as
                // "tool execution timed out" and got launched a second time.
                // Cancellation is different — it is a decision, and its result
                // must not be acted on.
                if context.is_cancelled() {
                    return Ok(sanitized_execution_status(ToolExecutionStatus::Cancelled));
                }
                Ok(output)
            }
            Err(error) if terminal_mcp_error(&error) => Err(error),
            Err(_) => Ok(ToolOutput::failure("tool infrastructure failure")),
        }
    }

    fn insert(
        &mut self,
        identity: ToolIdentity,
        aliases: impl IntoIterator<Item = String>,
        access: ToolAccess,
        version: u64,
        tool: impl DispatchTool + 'static,
    ) {
        let aliases = aliases
            .into_iter()
            .filter(|alias| !alias.is_empty())
            .collect::<Vec<_>>();
        let displaced = aliases
            .iter()
            .filter_map(|alias| self.aliases.get(alias))
            .filter(|current| *current != &identity)
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();

        for displaced_identity in displaced {
            let replacement_version = self.allocate_version();
            if let Some(displaced_tool) = self.tools.get_mut(&displaced_identity) {
                displaced_tool.version = replacement_version;
            }
        }

        self.aliases.retain(|_, current| current != &identity);
        self.aliases
            .extend(aliases.into_iter().map(|alias| (alias, identity.clone())));
        self.tools.insert(
            identity,
            RegisteredDispatchTool {
                access,
                version,
                tool: Box::new(tool),
            },
        );
    }

    fn allocate_version(&mut self) -> u64 {
        let version = self.next_version;
        self.next_version = self.next_version.saturating_add(1);
        version
    }
}

fn digest_arguments(arguments: &Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    arguments.to_string().hash(&mut hasher);
    hasher.finish()
}

fn sanitized_execution_status(status: ToolExecutionStatus) -> ToolOutput {
    match status {
        ToolExecutionStatus::Cancelled => ToolOutput::failure("tool execution cancelled"),
        ToolExecutionStatus::TimedOut => ToolOutput::failure("tool execution timed out"),
    }
}

impl Default for ToolDispatcher {
    fn default() -> Self {
        Self::new()
    }
}

fn remote_tool_access(access: RemoteToolAccess) -> ToolAccess {
    match access {
        RemoteToolAccess::ReadOnly => ToolAccess::ReadOnly,
        RemoteToolAccess::Write => ToolAccess::Write,
    }
}

impl ToolOutput {
    pub fn success(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            task_terminal: None,
            facts: None,
        }
    }

    pub fn failure(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            task_terminal: None,
            facts: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadFileInput {
    path: PathBuf,
    range: Option<(usize, usize)>,
}

impl ReadFileInput {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            range: None,
        }
    }

    pub fn with_range(mut self, offset: usize, limit: usize) -> Self {
        self.range = Some((offset, limit));
        self
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
pub struct EditFileInput {
    path: PathBuf,
    old: String,
    new: String,
}

impl EditFileInput {
    pub fn new(path: impl Into<PathBuf>, old: impl Into<String>, new: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            old: old.into(),
            new: new.into(),
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
pub struct GrepInput {
    pattern: String,
    path: Option<PathBuf>,
    file_glob: Option<String>,
    case_insensitive: bool,
}

impl GrepInput {
    pub fn new(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            path: None,
            file_glob: None,
            case_insensitive: false,
        }
    }

    pub fn with_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.path = Some(path.into());
        self
    }

    pub fn with_file_glob(mut self, file_glob: impl Into<String>) -> Self {
        self.file_glob = Some(file_glob.into());
        self
    }

    pub fn with_case_insensitive(mut self, case_insensitive: bool) -> Self {
        self.case_insensitive = case_insensitive;
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GlobInput {
    pattern: String,
}

#[derive(Clone, Debug)]
pub struct WebfetchInput {
    url: String,
    timeout: Duration,
    cancellation: Option<Arc<AtomicBool>>,
    execution_context: Option<ToolExecutionContext>,
}

impl WebfetchInput {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            timeout: DEFAULT_WEBFETCH_TIMEOUT,
            cancellation: None,
            execution_context: None,
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

    fn with_execution_context(mut self, context: ToolExecutionContext) -> Self {
        self.execution_context = Some(context);
        self
    }

    fn cancelled(&self) -> bool {
        self.cancellation
            .as_ref()
            .is_some_and(|cancellation| cancellation.load(Ordering::Acquire))
            || self
                .execution_context
                .as_ref()
                .is_some_and(ToolExecutionContext::is_cancelled)
    }
}

impl GlobInput {
    pub fn new(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
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
    pub bash_timeout: Duration,
}

impl Default for NativeToolLimits {
    fn default() -> Self {
        Self {
            max_list_entries: DEFAULT_MAX_LIST_ENTRIES,
            max_search_entries: DEFAULT_MAX_SEARCH_ENTRIES,
            max_search_results: DEFAULT_MAX_SEARCH_RESULTS,
            max_search_depth: DEFAULT_MAX_SEARCH_DEPTH,
            operation_timeout: DEFAULT_FILE_OPERATION_TIMEOUT,
            bash_timeout: DEFAULT_BASH_TIMEOUT,
        }
    }
}

#[derive(Clone, Debug)]
pub struct BashInput {
    command: String,
    timeout: Duration,
    cancellation: Option<Arc<AtomicBool>>,
    execution_context: Option<ToolExecutionContext>,
}

impl BashInput {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            timeout: DEFAULT_BASH_TIMEOUT,
            cancellation: None,
            execution_context: None,
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

    fn with_execution_context(mut self, context: ToolExecutionContext) -> Self {
        self.execution_context = Some(context);
        self
    }
}

/// The most worktrees one session may hold at once. A budget rather than a
/// limit on the work: every worktree is a full checkout on disk and a branch
/// in the repository, and a session that keeps creating them without ever
/// finishing one is leaking both.
pub const MAX_SESSION_WORKTREES: usize = 8;

/// Removes a worktree that was created but could not be used, and describes
/// what happened to it, so a failure never leaves an unexplained checkout
/// behind.
fn discard_worktree(
    worktrees: &SessionWorktrees,
    repository: &Path,
    repository_id: &str,
    name: &str,
) -> String {
    match worktrees.remove(repository, repository_id, name) {
        Ok(()) => "; the worktree was removed".to_owned(),
        Err(error) => format!("; the worktree could not be removed: {error}"),
    }
}

/// Where one session's worktrees live, and what to call the repository they
/// belong to.
#[derive(Debug)]
struct ConfiguredWorktrees {
    worktrees: SessionWorktrees,
    repository_id: String,
    /// This repository's session-worktree directory. Reachable as a whole, so
    /// a worktree an earlier session created is somewhere this one may return
    /// to.
    home: PathBuf,
}

#[derive(Debug)]
pub struct NativeTools {
    /// Where the session is working: what a relative path resolves against,
    /// what a command runs in, and what a confined open starts from. This
    /// moves; `session_root` does not.
    working_directory: PathBuf,
    /// The root the session was opened on. Every directory the session may
    /// move to lies under this one or under its own worktree home, so moving
    /// is bounded by the same rule a path is.
    session_root: PathBuf,
    worktrees: Option<ConfiguredWorktrees>,
    published_directory: Option<WorkingDirectory>,
    limits: NativeToolLimits,
    webfetch: Mutex<WebfetchState>,
    #[cfg(unix)]
    working_directory_dir: fs::File,
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
            working_directory_dir: fs::File::open(&project_root)
                .map_err(|error| Error::Tool(format!("cannot open project root: {error}")))?,
            session_root: project_root.clone(),
            working_directory: project_root,
            worktrees: None,
            published_directory: None,
            limits,
            webfetch: Mutex::new(WebfetchState::default()),
        })
    }

    /// Lets this session create worktrees under `worktrees`, and reach the
    /// ones already there, for the repository named by `repository_id`.
    pub fn with_worktrees(
        mut self,
        worktrees: SessionWorktrees,
        repository_id: impl Into<String>,
    ) -> Result<Self, Error> {
        let repository_id = repository_id.into();
        let home = worktrees
            .repository_directory(&repository_id)
            .map_err(|error| Error::Tool(error.to_string()))?;

        self.worktrees = Some(ConfiguredWorktrees {
            worktrees,
            repository_id,
            home,
        });
        Ok(self)
    }

    /// Publishes every later move to `directory`, so a surface can report
    /// where the session is without asking the tools.
    pub fn with_published_directory(mut self, directory: WorkingDirectory) -> Self {
        self.published_directory = Some(directory);
        self
    }

    /// Where the session is working right now.
    pub fn working_directory(&self) -> &Path {
        &self.working_directory
    }

    /// Moves the session to `path`, so later calls resolve against it.
    ///
    /// `path` is read the way every other tool path is: relative to where the
    /// session is now, or absolute. What it may name is wider than a file
    /// path's confinement by exactly one thing, this session's own worktree
    /// home, because a worktree the session creates is useless if the session
    /// cannot work in it.
    pub fn change_directory(&mut self, path: &Path) -> Result<ToolOutput, Error> {
        if path.as_os_str().is_empty() {
            return Ok(ToolOutput::failure("cd: path is required"));
        }

        let requested = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.working_directory.join(path)
        };

        let Ok(target) = fs::canonicalize(&requested) else {
            return Ok(ToolOutput::failure("cd: no such directory"));
        };

        if !target.is_dir() {
            return Ok(ToolOutput::failure("cd: not a directory"));
        }

        if !self.is_reachable(&target) {
            return Ok(ToolOutput::failure(
                "cd: outside the session root and its worktrees",
            ));
        }

        self.enter(target, "cd")
    }

    /// Creates `branch` in a worktree named `name` and moves the session into
    /// it, so the work the model asked for starts where it asked for it.
    pub fn create_worktree(
        &mut self,
        name: &str,
        branch: &str,
        start_point: &str,
    ) -> Result<ToolOutput, Error> {
        self.create_worktree_with_context(name, branch, start_point, None)
    }

    /// Same as [`Self::create_worktree`], with the calling turn's cancellation
    /// and deadline carried into git, so a cancelled turn does not leave a
    /// checkout running.
    pub fn create_worktree_with_context(
        &mut self,
        name: &str,
        branch: &str,
        start_point: &str,
        context: Option<&ToolExecutionContext>,
    ) -> Result<ToolOutput, Error> {
        let Some(configured) = self.worktrees.as_ref() else {
            return Ok(ToolOutput::failure(
                "worktree: session worktrees are unavailable",
            ));
        };
        let repository_id = configured.repository_id.clone();
        let worktrees = match context {
            Some(context) => configured
                .worktrees
                .clone()
                .with_execution_context(context.clone()),
            None => configured.worktrees.clone(),
        };

        if let Some(output) = self.enforce_worktree_budget(&worktrees, &repository_id) {
            return Ok(output);
        }

        let created = worktrees.create(
            &self.session_root,
            &repository_id,
            name,
            branch,
            start_point,
        );
        let path = match created {
            Ok(path) => path,
            Err(error) => return Ok(ToolOutput::failure(format!("worktree: {error}"))),
        };

        let Ok(path) = fs::canonicalize(&path) else {
            return Ok(ToolOutput::failure(format!(
                "worktree: the created worktree is unreadable{}",
                discard_worktree(&worktrees, &self.session_root, &repository_id, name)
            )));
        };

        // The worktree was created at a path under this session's own worktree
        // home, but what that path resolves to is only known now: a symlink
        // planted there before the session started would otherwise confine
        // every later tool call to a directory outside the session.
        if !self.is_reachable(&path) {
            return Ok(ToolOutput::failure(format!(
                "worktree: {} resolves outside the session root and its worktrees; \
                 it was left in place for inspection",
                path.display()
            )));
        }

        let entered = self.enter(path, "worktree")?;
        if entered.is_error {
            return Ok(ToolOutput::failure(format!(
                "{}{}",
                entered.content,
                discard_worktree(&worktrees, &self.session_root, &repository_id, name)
            )));
        }

        Ok(entered)
    }

    /// Holds one session to [`MAX_SESSION_WORKTREES`], reclaiming the
    /// worktrees whose work is already merged and whose tree is clean before
    /// refusing. Nothing is reclaimed while the session is below the budget:
    /// a merged worktree is still somewhere the model may be reading.
    fn enforce_worktree_budget(
        &self,
        worktrees: &SessionWorktrees,
        repository_id: &str,
    ) -> Option<ToolOutput> {
        let names = match worktrees.names(repository_id) {
            Ok(names) => names,
            Err(error) => return Some(ToolOutput::failure(format!("worktree: {error}"))),
        };
        if names.len() < MAX_SESSION_WORKTREES {
            return None;
        }

        let reclaimed = self.reclaim_worktrees(worktrees, repository_id, &names);
        if names.len() - reclaimed < MAX_SESSION_WORKTREES {
            return None;
        }

        Some(ToolOutput::failure(format!(
            "worktree: this session already holds {} worktrees and none of them is \
             both merged and clean; finish or remove one before creating another",
            names.len()
        )))
    }

    /// Removes every worktree whose branch is already contained in the
    /// session repository's `HEAD` and whose tree carries no work, and
    /// answers how many went.
    fn reclaim_worktrees(
        &self,
        worktrees: &SessionWorktrees,
        repository_id: &str,
        names: &[String],
    ) -> usize {
        let Ok(merge_target) = worktrees.head_revision(&self.session_root) else {
            return 0;
        };

        names
            .iter()
            .filter(|name| {
                let path = worktrees
                    .repository_directory(repository_id)
                    .map(|directory| directory.join(name))
                    .unwrap_or_default();
                // Never reclaim the directory the session is standing in, nor
                // one it is standing under.
                if self.working_directory.starts_with(&path) {
                    return false;
                }

                let reclaimable = worktrees
                    .status(repository_id, name, &merge_target)
                    .is_ok_and(|status| status.merged && !status.dirty);

                reclaimable
                    && worktrees
                        .remove(&self.session_root, repository_id, name)
                        .is_ok()
            })
            .count()
    }

    /// Opens `target` as the directory the session works in, reporting a
    /// failure under the name of the tool that asked for the move.
    fn enter(&mut self, target: PathBuf, tool: &str) -> Result<ToolOutput, Error> {
        #[cfg(unix)]
        {
            let Ok(opened) = fs::File::open(&target) else {
                return Ok(ToolOutput::failure(format!(
                    "{tool}: the directory cannot be opened"
                )));
            };
            self.working_directory_dir = opened;
        }

        self.working_directory = target;
        if let Some(published) = self.published_directory.as_ref() {
            published.moved_to(&self.working_directory);
        }

        Ok(ToolOutput::success(format!(
            "working directory: {}",
            self.working_directory.display()
        )))
    }

    /// Whether the session may work in `directory`, which must already be
    /// resolved.
    fn is_reachable(&self, directory: &Path) -> bool {
        if directory.starts_with(&self.session_root) {
            return true;
        }

        self.worktrees.as_ref().is_some_and(|configured| {
            let home =
                fs::canonicalize(&configured.home).unwrap_or_else(|_| configured.home.clone());
            directory.starts_with(&home)
        })
    }

    pub fn read_file(&self, input: ReadFileInput) -> Result<ToolOutput, Error> {
        if let Err(output) = self.ensure_working_directory_is_stable() {
            return Ok(output);
        }
        let path = match self.resolve_confined_path(&input.path) {
            Ok(path) => path,
            Err(output) => return Ok(output),
        };
        let mut input = input;
        input.path = path;
        if input
            .range
            .is_some_and(|(offset, limit)| offset == 0 || limit == 0)
        {
            return Ok(ToolOutput::failure(
                "read: offset and limit must be greater than zero",
            ));
        }

        #[cfg(unix)]
        let result = read_file_confined(&self.working_directory_dir, &input);

        #[cfg(not(unix))]
        let result = Err(ToolOutput::failure(
            "read: secure confined reads are unavailable on this platform",
        ));

        match result {
            Ok(output) => Ok(output.with_facts(ToolResultFacts::Read {
                path: FactPath::new(&input.path.display().to_string()),
                outcome: ToolOutcome::Succeeded,
            })),
            Err(output) => Ok(output),
        }
    }

    /// Lists bounded, readable project files for the TUI `@file` picker.
    pub fn tui_file_candidates(&self, limit: usize) -> Result<Vec<String>, ToolOutput> {
        if limit == 0 {
            return Err(ToolOutput::failure(
                "file picker: limit must be greater than zero",
            ));
        }
        self.ensure_working_directory_is_stable()?;

        let mut files = Vec::new();
        let mut budget = SearchBudget::new(&self.limits, "file picker");
        self.collect_tool_files(&self.working_directory, &mut budget, &mut files)?;
        self.ensure_working_directory_is_stable()?;

        let mut candidates = files
            .into_iter()
            .filter_map(|path| {
                path.strip_prefix(&self.working_directory)
                    .ok()
                    .map(Path::to_path_buf)
            })
            .filter_map(|path| path.to_str().map(str::to_owned))
            .filter(|path| {
                self.read_file(ReadFileInput::new(path))
                    .is_ok_and(|output| !output.is_error)
            })
            .collect::<Vec<_>>();
        candidates.sort();
        candidates.truncate(limit);
        Ok(candidates)
    }

    pub fn write_file(&self, input: WriteFileInput) -> Result<ToolOutput, Error> {
        self.write_file_with_context(input, None)
    }

    pub fn edit_file(&self, input: EditFileInput) -> Result<ToolOutput, Error> {
        self.edit_file_with_context(input, None)
    }

    /// Writes after the permission gate has decided (when `context` is set).
    ///
    /// Out-of-workspace paths are allowed only with a context — that is the
    /// peer "Allow on external path" path. Direct [`Self::write_file`] still
    /// hard-fails outside the root.
    pub fn write_file_with_context(
        &self,
        input: WriteFileInput,
        context: Option<&ToolExecutionContext>,
    ) -> Result<ToolOutput, Error> {
        // Peer agents (OpenCode, Claude, Codex) treat out-of-workspace paths as
        // a permission decision, not a hard tool failure. By the time this
        // runs with a ToolExecutionContext, the permission gate has already
        // Allow'd the call (or bypass converted Ask → Allow). Unconfined
        // absolute writes therefore only open after that decision.
        let target = match self.resolve_authorized_write_path(&input.path, context) {
            Ok(target) => target,
            Err(output) => return Ok(Self::failed_write_facts(&input.path, output)),
        };

        let result = match &target {
            AuthorizedWritePath::Confined(relative) => {
                #[cfg(unix)]
                {
                    write_file_confined(
                        &self.working_directory_dir,
                        relative,
                        input.content.as_bytes(),
                        context,
                    )
                }
                #[cfg(not(unix))]
                {
                    let _ = relative;
                    Err(ToolOutput::failure(
                        "write: secure confined writes are unavailable on this platform",
                    ))
                }
            }
            AuthorizedWritePath::External(absolute) => {
                write_file_external(absolute, input.content.as_bytes(), context)
            }
        };

        let display_path = target.display_path();
        match result {
            Ok(is_new_file) => {
                let bytes_written = input.content.len();
                let lines_written = input.content.lines().count();

                Ok(
                    ToolOutput::success(format!("wrote {display_path}")).with_facts(
                        ToolResultFacts::Write {
                            path: FactPath::new(&display_path),
                            outcome: ToolOutcome::Succeeded,
                            written: Some(WriteMagnitude {
                                is_new_file,
                                bytes_written,
                                lines_written,
                            }),
                        },
                    ),
                )
            }
            Err(output) => Ok(Self::failed_write_facts(Path::new(&display_path), output)),
        }
    }

    /// Attaches `Write` failure facts to an already-sanitized failure output,
    /// so a write that never completed is still visible to the mechanical
    /// half of the divergence detector rather than leaving no trace at all.
    fn failed_write_facts(path: &Path, output: ToolOutput) -> ToolOutput {
        output.with_facts(ToolResultFacts::Write {
            path: FactPath::new(&path.display().to_string()),
            outcome: ToolOutcome::Failed,
            written: None,
        })
    }

    fn edit_file_with_context(
        &self,
        input: EditFileInput,
        context: Option<&ToolExecutionContext>,
    ) -> Result<ToolOutput, Error> {
        let target = match self.resolve_authorized_write_path(&input.path, context) {
            Ok(target) => target,
            Err(output) => return Ok(Self::failed_edit_facts(&input.path, output)),
        };
        if input.old.is_empty() {
            return Ok(Self::failed_edit_facts(
                Path::new(&target.display_path()),
                ToolOutput::failure("edit: old text is required"),
            ));
        }
        if input.old == input.new {
            return Ok(Self::failed_edit_facts(
                Path::new(&target.display_path()),
                ToolOutput::failure("edit: old and new text must differ"),
            ));
        }

        let result = match &target {
            AuthorizedWritePath::Confined(relative) => {
                #[cfg(unix)]
                {
                    edit_file_confined(
                        &self.working_directory_dir,
                        relative,
                        &input.old,
                        &input.new,
                        context,
                    )
                }
                #[cfg(not(unix))]
                {
                    let _ = relative;
                    Err(ToolOutput::failure(
                        "edit: secure confined edits are unavailable on this platform",
                    ))
                }
            }
            AuthorizedWritePath::External(absolute) => {
                edit_file_external(absolute, &input.old, &input.new, context)
            }
        };

        let display = Path::new(&target.display_path()).to_path_buf();
        Ok(result.unwrap_or_else(|output| Self::failed_edit_facts(&display, output)))
    }

    /// Attaches `Edit` failure facts to an already-sanitized failure output,
    /// for the same reason [`Self::failed_write_facts`] exists for writes.
    fn failed_edit_facts(path: &Path, output: ToolOutput) -> ToolOutput {
        output.with_facts(ToolResultFacts::Edit {
            path: FactPath::new(&path.display().to_string()),
            outcome: ToolOutcome::Failed,
            changed: None,
        })
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
        self.search_with_context(input, None)
    }

    pub fn search_with_context(
        &self,
        input: SearchInput,
        context: Option<&ToolExecutionContext>,
    ) -> Result<ToolOutput, Error> {
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

        let mut walk = SearchWalk {
            query: &input.query,
            context,
            budget: SearchBudget::new(&self.limits, "search"),
            results: Vec::new(),
            withheld: false,
        };
        if let Err(output) = self.search_directory(&path, 0, &mut walk) {
            return Ok(output);
        }

        let SearchWalk {
            mut results,
            withheld,
            ..
        } = walk;
        let match_count = results.len();
        if withheld {
            results.push(WITHHELD_FILES_NOTICE.to_owned());
        }

        Ok(
            ToolOutput::success(results.join("")).with_facts(ToolResultFacts::Search {
                outcome: ToolOutcome::Succeeded,
                match_count,
                truncated: false,
            }),
        )
    }

    pub fn grep(&self, input: GrepInput) -> Result<ToolOutput, Error> {
        self.grep_with_context(input, None)
    }

    pub fn grep_with_context(
        &self,
        input: GrepInput,
        context: Option<&ToolExecutionContext>,
    ) -> Result<ToolOutput, Error> {
        if input.pattern.is_empty() {
            return Ok(ToolOutput::failure("grep: pattern is required"));
        }
        let regex = match RegexBuilder::new(&input.pattern)
            .case_insensitive(input.case_insensitive)
            .build()
        {
            Ok(regex) => regex,
            Err(_) => return Ok(ToolOutput::failure("grep: invalid regex")),
        };
        let file_glob = match input.file_glob.as_deref() {
            Some(pattern) => match build_glob_set(pattern, "grep") {
                Ok(glob) => Some(glob),
                Err(output) => return Ok(output),
            },
            None => None,
        };
        let target = match input.path {
            Some(path) => match self.resolve_existing(&path) {
                Ok(path) => path,
                Err(output) => return Ok(output),
            },
            None => self.working_directory.clone(),
        };

        let mut files = Vec::new();
        let mut budget = SearchBudget::new(&self.limits, "grep");
        let single_file = target.is_file();
        if single_file {
            files.push(target);
        } else if target.is_dir() {
            if let Err(output) = self.collect_tool_files(&target, &mut budget, &mut files) {
                return Ok(output);
            }
        } else {
            return Ok(ToolOutput::failure("grep: path is not a file or directory"));
        }

        let mut results = Vec::new();
        let mut withheld = false;
        for path in files {
            if let Err(output) = budget.check_deadline() {
                return Ok(output);
            }
            let relative = path
                .strip_prefix(&self.working_directory)
                .map_err(|_| Error::Tool("path: outside project root".into()))?;
            if !permits_read(context, relative) {
                withheld = true;
                continue;
            }
            if file_glob
                .as_ref()
                .is_some_and(|glob| !glob.is_match(relative))
            {
                continue;
            }
            if fs::metadata(&path).is_ok_and(|metadata| metadata.len() > MAX_FILE_BYTES) {
                continue;
            }
            let content = if single_file {
                #[cfg(all(test, unix))]
                run_grep_test_hook();

                #[cfg(unix)]
                match read_grep_file_confined(&self.working_directory_dir, relative) {
                    Ok(Some(content)) => content,
                    Ok(None) => continue,
                    Err(output) => return Ok(output),
                }

                #[cfg(not(unix))]
                return Ok(ToolOutput::failure(
                    "grep: secure confined reads are unavailable on this platform",
                ));
            } else {
                let content = match fs::read(&path) {
                    Ok(content) if !content.contains(&0) => content,
                    Ok(_) => continue,
                    Err(error) => return Ok(ToolOutput::failure(format!("grep: {error}"))),
                };
                match String::from_utf8(content) {
                    Ok(content) => content,
                    Err(_) => continue,
                }
            };
            for (line, text) in content.lines().enumerate() {
                if let Err(output) = budget.check_deadline() {
                    return Ok(output);
                }
                if regex.is_match(text) {
                    if results.len() == self.limits.max_search_results {
                        let match_count = results.len();
                        if withheld {
                            results.push(WITHHELD_FILES_NOTICE.to_owned());
                        }
                        results.push(format!(
                            "[grep output truncated after {} results]\n",
                            self.limits.max_search_results
                        ));
                        return Ok(ToolOutput::success(results.join("")).with_facts(
                            ToolResultFacts::Search {
                                outcome: ToolOutcome::Succeeded,
                                match_count,
                                truncated: true,
                            },
                        ));
                    }
                    results.push(format!("{}:{}:{text}\n", relative.display(), line + 1));
                }
            }
        }

        let match_count = results.len();
        if withheld {
            results.push(WITHHELD_FILES_NOTICE.to_owned());
        }

        Ok(
            ToolOutput::success(results.join("")).with_facts(ToolResultFacts::Search {
                outcome: ToolOutcome::Succeeded,
                match_count,
                truncated: false,
            }),
        )
    }

    pub fn glob(&self, input: GlobInput) -> Result<ToolOutput, Error> {
        if input.pattern.is_empty() {
            return Ok(ToolOutput::failure("glob: pattern is required"));
        }
        let pattern = match build_glob_set(&input.pattern, "glob") {
            Ok(pattern) => pattern,
            Err(output) => return Ok(output),
        };
        let mut files = Vec::new();
        let mut budget = SearchBudget::new(&self.limits, "glob");
        if let Err(output) =
            self.collect_tool_files(&self.working_directory, &mut budget, &mut files)
        {
            return Ok(output);
        }

        let mut matches = files
            .into_iter()
            .filter_map(|path| {
                path.strip_prefix(&self.working_directory)
                    .ok()
                    .map(Path::to_path_buf)
            })
            .filter(|path| pattern.is_match(path))
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>();
        matches.sort();
        let truncated = matches.len() > self.limits.max_list_entries;
        matches.truncate(self.limits.max_list_entries);

        let mut output = matches.join("\n");
        if !output.is_empty() {
            output.push('\n');
        }
        if truncated {
            output.push_str(&format!(
                "[glob output truncated after {} entries]\n",
                self.limits.max_list_entries
            ));
        }
        Ok(ToolOutput::success(output))
    }

    pub fn webfetch(&self, input: WebfetchInput) -> Result<ToolOutput, Error> {
        if input.url.trim().is_empty() {
            return Ok(ToolOutput::failure("webfetch: URL is required"));
        }
        if input.timeout.is_zero() {
            return Ok(ToolOutput::failure(
                "webfetch: timeout must be greater than zero",
            ));
        }
        if input.cancelled() {
            return Ok(ToolOutput::failure("webfetch: cancelled"));
        }

        if !self.begin_webfetch() {
            return Ok(ToolOutput::failure("webfetch: request busy"));
        }

        let result = self.webfetch_with_admission(input);
        self.finish_webfetch();
        result
    }

    fn webfetch_with_admission(&self, input: WebfetchInput) -> Result<ToolOutput, Error> {
        let mut url = match webfetch_url(&input.url) {
            Ok(url) => url,
            Err(output) => return Ok(output),
        };

        for redirects in 0..=MAX_WEBFETCH_REDIRECTS {
            if input.cancelled() {
                return Ok(ToolOutput::failure("webfetch: cancelled"));
            }
            let addresses = match webfetch_addresses(&url) {
                Ok(addresses) => addresses,
                Err(output) => return Ok(output),
            };
            let host = url.host_str().expect("validated URL host");
            let timeout = match input.execution_context.as_ref() {
                Some(context) => match context.remaining() {
                    Ok(remaining) => remaining,
                    Err(ToolExecutionStatus::Cancelled) => {
                        return Ok(ToolOutput::failure("webfetch: cancelled"));
                    }
                    Err(ToolExecutionStatus::TimedOut) => {
                        return Ok(ToolOutput::failure("webfetch: timed out"));
                    }
                },
                None => input.timeout,
            }
            .min(input.timeout);
            let client = reqwest::blocking::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(timeout)
                .user_agent("agens-webfetch/1")
                .resolve_to_addrs(host, &addresses)
                .build()
                .map_err(|_| Error::Tool("webfetch client setup failed".into()))?;
            self.start_webfetch_request(client, url.clone());
            let response = loop {
                match self.wait_for_webfetch_request() {
                    Ok(Ok(response)) => break response,
                    Ok(Err(WebfetchRequestError::TimedOut)) => {
                        return Ok(ToolOutput::failure("webfetch: timed out"));
                    }
                    Ok(Err(WebfetchRequestError::Failed)) => {
                        return Ok(ToolOutput::failure("webfetch: request failed"));
                    }
                    Ok(Err(WebfetchRequestError::ReadFailed)) => {
                        return Ok(ToolOutput::failure("webfetch: response read failed"));
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) if input.cancelled() => {
                        return Ok(ToolOutput::failure("webfetch: cancelled"));
                    }
                    Err(mpsc::RecvTimeoutError::Timeout)
                        if input
                            .execution_context
                            .as_ref()
                            .is_some_and(ToolExecutionContext::is_expired) =>
                    {
                        return Ok(ToolOutput::failure("webfetch: timed out"));
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        return Ok(ToolOutput::failure("webfetch: request failed"));
                    }
                }
            };
            if response.status.is_redirection() {
                if redirects == MAX_WEBFETCH_REDIRECTS {
                    return Ok(ToolOutput::failure("webfetch: redirect limit exceeded"));
                }
                let Some(location) = response.location else {
                    return Ok(ToolOutput::failure("webfetch: redirect has no location"));
                };
                url = match url.join(&location) {
                    Ok(url) => match webfetch_url(url.as_str()) {
                        Ok(url) => url,
                        Err(output) => return Ok(output),
                    },
                    Err(_) => {
                        return Ok(ToolOutput::failure("webfetch: invalid redirect location"));
                    }
                };
                continue;
            }
            if !response.status.is_success() {
                return Ok(ToolOutput::failure(format!(
                    "webfetch: HTTP status {}",
                    response.status
                )));
            }
            let mut content = String::from_utf8_lossy(&response.bytes).into_owned();
            if response.html {
                content = visible_html_text(&content);
            }
            return Ok(ToolOutput::success(truncate_webfetch_content(
                content,
                response.truncated,
            )));
        }
        unreachable!("redirect loop always returns")
    }

    fn begin_webfetch(&self) -> bool {
        let mut state = self.webfetch.lock().expect("webfetch state lock poisoned");
        if state.active || !state.reap_completed_worker() {
            return false;
        }
        state.active = true;
        true
    }

    fn finish_webfetch(&self) {
        self.webfetch
            .lock()
            .expect("webfetch state lock poisoned")
            .active = false;
    }

    fn start_webfetch_request(&self, client: reqwest::blocking::Client, url: reqwest::Url) {
        let (sender, receiver) = mpsc::sync_channel(1);
        let handle = thread::spawn(move || {
            let _ = sender.send(webfetch_request(client, url));
        });
        self.webfetch
            .lock()
            .expect("webfetch state lock poisoned")
            .worker = Some(WebfetchWorker { receiver, handle });
    }

    fn wait_for_webfetch_request(
        &self,
    ) -> Result<Result<WebfetchResponse, WebfetchRequestError>, mpsc::RecvTimeoutError> {
        let result = self
            .webfetch
            .lock()
            .expect("webfetch state lock poisoned")
            .worker
            .as_ref()
            .expect("webfetch admission owns request worker")
            .receiver
            .recv_timeout(PROCESS_POLL_INTERVAL);

        if result.is_ok() || matches!(&result, Err(mpsc::RecvTimeoutError::Disconnected)) {
            self.join_webfetch_worker();
        }

        result
    }

    fn join_webfetch_worker(&self) {
        if let Some(worker) = self
            .webfetch
            .lock()
            .expect("webfetch state lock poisoned")
            .worker
            .take()
        {
            let _ = worker.handle.join();
        }
    }

    /// Runs a shell command with its working directory set to the project
    /// root; nothing else confines it. Unlike `read`/`write`/`edit`/`grep`,
    /// which resolve through a `*_confined` helper before touching disk, a
    /// granted `bash` call can read or write anywhere the OS process can
    /// reach — an absolute path, `cd ..`, or a relative `../` escapes the
    /// project root exactly as it would in an interactive shell. This is a
    /// deliberate, accepted property of granting `bash`, not an oversight.
    ///
    /// The declared guardrail against this is a target-pattern rule on the
    /// command text, e.g. `deny bash rm*`, evaluated by `PermissionPolicy`
    /// before this method ever runs. That guardrail is pattern matching over
    /// a raw shell string, not a security boundary: it does not stop
    /// `/bin/rm`, `sudo rm`, `cd foo && rm`, `xargs rm`, or any other way to
    /// reach the same effect through different text. Unlike a path target,
    /// a `bash` target is classified `PermissionTargetKind::FreeFormText`
    /// (see `permission_target_kind_for_tool`), so a bare `*` crosses `/`
    /// there: `deny bash rm*` already catches `rm -rf /tmp/x`, not just a
    /// slash-free `rm` command.
    pub fn bash(&self, input: BashInput) -> Result<ToolOutput, Error> {
        if input.command.trim().is_empty() {
            return Ok(ToolOutput::failure("bash: command is required"));
        }

        if input.timeout.is_zero() {
            return Ok(ToolOutput::failure(
                "bash: timeout must be greater than zero",
            ));
        }

        let mut command = Command::new("bash");
        command
            .arg("-c")
            .arg(&input.command)
            .current_dir(&self.working_directory)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        #[cfg(unix)]
        command.process_group(0);

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(_) => return Ok(ToolOutput::failure("bash: failed to start")),
        };
        let Some(stdout) = child.stdout.take() else {
            let _ = terminate_process_group(&mut child);
            return Ok(ToolOutput::failure("bash: output setup failed"));
        };
        let Some(stderr) = child.stderr.take() else {
            let _ = terminate_process_group(&mut child);
            return Ok(ToolOutput::failure("bash: output setup failed"));
        };
        let stdout_reader = read_capped(stdout);
        let stderr_reader = read_capped(stderr);
        let deadline = Instant::now() + input.timeout;

        let status = loop {
            if input
                .execution_context
                .as_ref()
                .is_some_and(ToolExecutionContext::is_cancelled)
                || input
                    .cancellation
                    .as_ref()
                    .is_some_and(|cancellation| cancellation.load(Ordering::Acquire))
            {
                if terminate_process_group(&mut child).is_err() {
                    return Ok(ToolOutput::failure("bash: process cleanup failed"));
                }
                let output = match wait_for_readers(stdout_reader, stderr_reader) {
                    Ok(output) => output,
                    Err(_) => return Ok(ToolOutput::failure("bash: process cleanup failed")),
                };
                if input.execution_context.is_some() {
                    return Err(Error::Cancelled);
                }
                return Ok(render_bash_result(
                    &output,
                    "unavailable",
                    Some("bash: cancelled"),
                ));
            }

            if Instant::now() >= deadline {
                if terminate_process_group(&mut child).is_err() {
                    return Ok(ToolOutput::failure("bash: process cleanup failed"));
                }
                let output = match wait_for_readers(stdout_reader, stderr_reader) {
                    Ok(output) => output,
                    Err(_) => return Ok(ToolOutput::failure("bash: process cleanup failed")),
                };
                return Ok(render_bash_result(
                    &output,
                    "unavailable",
                    Some(&format!(
                        "bash: timed out after {}ms. If this command is expected to take longer, retry with a larger timeout value in milliseconds (max: {}ms).",
                        input.timeout.as_millis(),
                        MAX_BASH_TIMEOUT.as_millis()
                    )),
                ));
            }

            let status = match child.try_wait() {
                Ok(status) => status,
                Err(_) => {
                    let _ = terminate_process_group(&mut child);
                    let _ = wait_for_readers(stdout_reader, stderr_reader);
                    return Ok(ToolOutput::failure("bash: wait failed"));
                }
            };
            if let Some(status) = status {
                if kill_process_group(child.id()).is_err() {
                    return Ok(ToolOutput::failure("bash: process cleanup failed"));
                }
                break status;
            }

            thread::sleep(PROCESS_POLL_INTERVAL);
        };

        let output = wait_for_readers(stdout_reader, stderr_reader)
            .map_err(|_| Error::Tool("bash: output reader failed".into()))?;

        let outcome = if status.success() {
            ToolOutcome::Succeeded
        } else {
            ToolOutcome::Failed
        };
        let facts = ToolResultFacts::Bash {
            outcome,
            exit_code: status.code(),
        };

        Ok(render_bash_result(&output, &exit_code(status), None).with_facts(facts))
    }

    fn resolve_existing(&self, path: &Path) -> Result<PathBuf, ToolOutput> {
        let relative = self.resolve_confined_path(path)?;

        let path = fs::canonicalize(self.working_directory.join(relative))
            .map_err(|error| ToolOutput::failure(format!("path: {error}")))?;

        if path.starts_with(&self.working_directory) {
            Ok(path)
        } else {
            Err(ToolOutput::failure("path: outside project root"))
        }
    }

    fn search_directory(
        &self,
        directory: &Path,
        depth: usize,
        walk: &mut SearchWalk<'_>,
    ) -> Result<(), ToolOutput> {
        let directory_entries = fs::read_dir(directory)
            .map_err(|error| ToolOutput::failure(format!("search: {error}")))?;
        let mut entries = Vec::new();

        for entry in directory_entries {
            walk.budget.consume_entry()?;
            entries.push(entry.map_err(|error| ToolOutput::failure(format!("search: {error}")))?);
        }
        entries.sort_by_key(|entry| entry.file_name());

        for entry in entries {
            walk.budget.check_deadline()?;

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
                self.search_directory(&path, next_depth, walk)?;
                continue;
            }

            if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
                continue;
            }

            let relative = path
                .strip_prefix(&self.working_directory)
                .map_err(|_| ToolOutput::failure("path: outside project root"))?;

            if !permits_read(walk.context, relative) {
                walk.withheld = true;
                continue;
            }

            let content = fs::read_to_string(&path)
                .map_err(|error| ToolOutput::failure(format!("search: {error}")))?;

            for (line, text) in content.lines().enumerate() {
                walk.budget.check_deadline()?;
                if text.contains(walk.query) {
                    if walk.results.len() == self.limits.max_search_results {
                        return Err(ToolOutput::failure(format!(
                            "search: result limit of {} exceeded",
                            self.limits.max_search_results
                        )));
                    }
                    walk.results
                        .push(format!("{}:{}:{text}\n", relative.display(), line + 1));
                }
            }
        }

        Ok(())
    }

    fn collect_tool_files(
        &self,
        directory: &Path,
        budget: &mut SearchBudget,
        files: &mut Vec<PathBuf>,
    ) -> Result<(), ToolOutput> {
        let mut builder = WalkBuilder::new(directory);
        builder
            .hidden(true)
            .ignore(false)
            .git_ignore(true)
            .git_exclude(true)
            .git_global(true)
            .parents(true)
            .require_git(false)
            .follow_links(false)
            .sort_by_file_name(|left, right| left.cmp(right));

        for entry in builder.build() {
            let entry = entry
                .map_err(|_| ToolOutput::failure(format!("{}: traversal failed", budget.tool)))?;
            if entry.depth() == 0 {
                continue;
            }

            budget.consume_entry()?;
            budget.check_deadline()?;
            let Some(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                if entry.depth() > self.limits.max_search_depth {
                    return Err(ToolOutput::failure(format!(
                        "{}: traversal depth limit of {} exceeded",
                        budget.tool, self.limits.max_search_depth
                    )));
                }
            } else if file_type.is_file() {
                files.push(entry.into_path());
            }
        }
        Ok(())
    }

    /// Resolves a path for a write/edit that has already cleared the permission
    /// gate (when `context` is present).
    ///
    /// - Under the project root → confined openat path (relative).
    /// - Outside the root after a decided authorization → unrestricted absolute
    ///   path.
    /// - Outside the root without a context (unit tests / unauthenticated call
    ///   sites) → still a hard confinement failure, same as before.
    /// - Outside the root under dangerous mode's fallback → the same hard
    ///   confinement failure: path confinement is a floor that an authorization
    ///   nobody gave cannot lift. See [`PermissionAuthority`].
    ///
    /// Peers treat out-of-workspace paths as permission decisions; bypass/Allow
    /// must not die at openat. Explicit policy Deny never reaches execute.
    fn resolve_authorized_write_path(
        &self,
        path: &Path,
        context: Option<&ToolExecutionContext>,
    ) -> Result<AuthorizedWritePath, ToolOutput> {
        match self.resolve_confined_path(path) {
            Ok(relative) => Ok(AuthorizedWritePath::Confined(relative)),
            Err(error)
                if context.is_some_and(ToolExecutionContext::permits_write_outside_root)
                    && is_path_confinement_refusal(&error) =>
            {
                let absolute = if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    self.working_directory.join(path)
                };
                let absolute = normalize_external_path(&absolute)?;
                Ok(AuthorizedWritePath::External(absolute))
            }
            Err(error) => Err(error),
        }
    }

    /// Resolves a user/tool path into a project-relative path safe for confined openat.
    ///
    /// Absolute paths are accepted when they lie under the confinement root
    /// (future multi-workspace roots will extend the same rewrite). Outside the
    /// root still fails as outside project root for unauthenticated call sites.
    /// After permission Allow (see [`Self::resolve_authorized_write_path`]),
    /// outside paths may proceed unconfined.
    fn resolve_confined_path(&self, path: &Path) -> Result<PathBuf, ToolOutput> {
        if path.as_os_str().is_empty() {
            return Err(ToolOutput::failure("path: must be a non-empty path"));
        }

        let relative = if path.is_absolute() {
            self.absolute_path_under_root(path)?
        } else {
            path.to_path_buf()
        };

        if relative.as_os_str().is_empty() {
            return Err(ToolOutput::failure("path: path must name a file"));
        }

        if relative
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
        {
            Ok(relative)
        } else {
            Err(ToolOutput::failure("path: traversal is not allowed"))
        }
    }

    /// Maps an absolute path under the confinement root to a relative path.
    fn absolute_path_under_root(&self, path: &Path) -> Result<PathBuf, ToolOutput> {
        let root = &self.working_directory;
        if let Ok(relative) = path.strip_prefix(root) {
            if relative.as_os_str().is_empty() {
                return Err(ToolOutput::failure("path: path must name a file"));
            }
            return Ok(relative.to_path_buf());
        }

        // Lexical strip failed (symlink components, non-canonical spelling).
        // Resolve an existing path, or its parent for not-yet-created files.
        let resolved = if path.exists() {
            fs::canonicalize(path).map_err(|_| ToolOutput::failure("path: outside project root"))?
        } else {
            let parent = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .ok_or_else(|| ToolOutput::failure("path: outside project root"))?;
            let file_name = path
                .file_name()
                .ok_or_else(|| ToolOutput::failure("path: path must name a file"))?;
            let parent = fs::canonicalize(parent)
                .map_err(|_| ToolOutput::failure("path: outside project root"))?;
            parent.join(file_name)
        };

        resolved
            .strip_prefix(root)
            .map(Path::to_path_buf)
            .map_err(|_| ToolOutput::failure("path: outside project root"))
    }

    fn ensure_working_directory_is_stable(&self) -> Result<(), ToolOutput> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;

            let root = fs::canonicalize(&self.working_directory)
                .map_err(|_| ToolOutput::failure("path: outside project root"))?;
            let metadata = fs::symlink_metadata(&self.working_directory)
                .map_err(|_| ToolOutput::failure("path: outside project root"))?;
            let opened = self
                .working_directory_dir
                .metadata()
                .map_err(|_| ToolOutput::failure("path: outside project root"))?;
            if root != self.working_directory
                || metadata.file_type().is_symlink()
                || (metadata.dev(), metadata.ino()) != (opened.dev(), opened.ino())
            {
                return Err(ToolOutput::failure("path: outside project root"));
            }
        }

        Ok(())
    }
}

impl Drop for NativeTools {
    fn drop(&mut self) {
        let worker = self
            .webfetch
            .get_mut()
            .expect("webfetch state lock poisoned")
            .worker
            .take();
        if let Some(worker) = worker {
            let _ = worker.handle.join();
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct NativeToolMetadata {
    pub qualified_name: String,
    pub description: String,
    pub input_schema: Value,
    pub access: ToolAccess,
}

struct WebfetchResponse {
    status: reqwest::StatusCode,
    location: Option<String>,
    html: bool,
    bytes: Vec<u8>,
    truncated: bool,
}

#[derive(Debug)]
struct WebfetchWorker {
    receiver: mpsc::Receiver<Result<WebfetchResponse, WebfetchRequestError>>,
    handle: thread::JoinHandle<()>,
}

#[derive(Debug, Default)]
struct WebfetchState {
    active: bool,
    worker: Option<WebfetchWorker>,
}

impl WebfetchState {
    fn reap_completed_worker(&mut self) -> bool {
        let Some(worker) = self.worker.as_ref() else {
            return true;
        };
        if matches!(worker.receiver.try_recv(), Err(mpsc::TryRecvError::Empty)) {
            return false;
        }
        let worker = self.worker.take().expect("webfetch worker must exist");
        let _ = worker.handle.join();
        true
    }
}

enum WebfetchRequestError {
    TimedOut,
    Failed,
    ReadFailed,
}

fn webfetch_url(value: &str) -> Result<reqwest::Url, ToolOutput> {
    let url = match reqwest::Url::parse(value) {
        Ok(url) if matches!(url.scheme(), "http" | "https") => url,
        _ => return Err(ToolOutput::failure("webfetch: URL must use http or https")),
    };
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ToolOutput::failure(
            "webfetch: URL credentials are not allowed",
        ));
    }
    Ok(url)
}

fn webfetch_request(
    client: reqwest::blocking::Client,
    url: reqwest::Url,
) -> Result<WebfetchResponse, WebfetchRequestError> {
    let response = client.get(url).send().map_err(|error| {
        if error.is_timeout() {
            WebfetchRequestError::TimedOut
        } else {
            WebfetchRequestError::Failed
        }
    })?;
    let status = response.status();
    let location = response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let html = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/html"));
    let mut bytes = Vec::new();
    if status.is_success()
        && response
            .take(MAX_WEBFETCH_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .is_err()
    {
        return Err(WebfetchRequestError::ReadFailed);
    }
    let truncated = bytes.len() > MAX_WEBFETCH_BYTES;
    bytes.truncate(MAX_WEBFETCH_BYTES);
    Ok(WebfetchResponse {
        status,
        location,
        html,
        bytes,
        truncated,
    })
}

fn truncate_webfetch_content(mut content: String, truncated: bool) -> String {
    if !truncated && content.len() <= MAX_WEBFETCH_BYTES {
        return content;
    }
    let mut end = MAX_WEBFETCH_BYTES - WEBFETCH_TRUNCATED_MARKER.len();
    while !content.is_char_boundary(end) {
        end -= 1;
    }
    content.truncate(end);
    content.push_str(WEBFETCH_TRUNCATED_MARKER);
    content
}

fn webfetch_addresses(url: &reqwest::Url) -> Result<Vec<std::net::SocketAddr>, ToolOutput> {
    let host = url
        .host_str()
        .ok_or_else(|| ToolOutput::failure("webfetch: URL host is required"))?
        .trim_matches(['[', ']']);
    let port = url
        .port_or_known_default()
        .ok_or_else(|| ToolOutput::failure("webfetch: URL port is required"))?;
    if let Ok(address) = host.parse::<IpAddr>() {
        if blocked_webfetch_address(address) {
            return Err(ToolOutput::failure("webfetch: blocked network address"));
        }
        return Ok(vec![std::net::SocketAddr::new(address, port)]);
    }
    let addresses = (host, port)
        .to_socket_addrs()
        .map_err(|_| ToolOutput::failure("webfetch: host resolution failed"))?
        .collect::<Vec<_>>();
    let addresses = permitted_webfetch_addresses(addresses);
    if addresses.is_empty() {
        return Err(ToolOutput::failure("webfetch: blocked network address"));
    }
    Ok(addresses)
}

fn permitted_webfetch_addresses(
    addresses: impl IntoIterator<Item = std::net::SocketAddr>,
) -> Vec<std::net::SocketAddr> {
    addresses
        .into_iter()
        .filter(|address| !blocked_webfetch_address(address.ip()))
        .collect()
}

fn blocked_webfetch_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => address.is_link_local() || address.octets() == [100, 100, 100, 200],
        IpAddr::V6(address) => {
            address.is_unicast_link_local()
                || address.segments() == [0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x254]
        }
    }
}

fn visible_html_text(html: &str) -> String {
    let mut text = String::new();
    let mut hidden = None;
    for part in html.split('<') {
        let Some((tag, rest)) = part.split_once('>') else {
            if hidden.is_none() {
                text.push_str(part);
            }
            continue;
        };
        let name = tag
            .trim_start_matches('/')
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if matches!(name.as_str(), "script" | "style") {
            hidden = if tag.trim_start().starts_with('/') {
                None
            } else {
                Some(name)
            };
        }
        if hidden.is_none() {
            text.push_str(rest);
            text.push(' ');
        }
    }
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The native tools registered beside [`NativeToolCatalog::metadata`] rather
/// than out of it.
///
/// Each is constructed from something the catalog holds no handle on — the
/// skill catalog, the agent catalog, the registry a live delegation is
/// coordinated through, the port an interactive surface answers a question on —
/// so the runtime that owns those constructs and registers the tool itself.
/// Which of them a given dispatcher ends up holding depends on what that
/// runtime is for and on how the session is configured.
///
/// They are enumerated here all the same, because anything resolving a name
/// against "the native tools" resolves it against a surface these belong to. A
/// name left out survives as a pattern that never matches a dispatcher
/// identity: the rule reads as enforced and decides nothing.
/// Native tools that move the session itself rather than act inside it.
///
/// A delegated child runs its own tools on its own root and reports through
/// the execution that launched it, so a child holding these would move a
/// directory the thread that delegated to it never asked to move, and would
/// move it where none of the session's surfaces can see. They stay with the
/// thread that owns the session.
pub const SESSION_SCOPED_NATIVE_TOOLS: [&str; 2] = ["native::cd", "native::worktree"];

/// Whether `name` is one of the natives that only the session's own thread
/// holds. The name is reduced through [`agens_core::bare_tool_name`], so the
/// answer cannot depend on which spelling of the tool reached it.
pub fn is_session_scoped_native_tool(name: &str) -> bool {
    let bare = agens_core::bare_tool_name(name);

    SESSION_SCOPED_NATIVE_TOOLS
        .iter()
        .filter_map(|registered| registered.strip_prefix("native::"))
        .any(|registered| bare == registered)
}

pub const NATIVE_TOOLS_REGISTERED_OUTSIDE_THE_CATALOG: [&str; 5] = [
    "native::ask_user",
    "native::skill",
    "native::task",
    "native::task_control",
    "native::task_message",
];

/// Canonical production catalog for the built-in project-confined tools.
#[derive(Debug)]
pub struct NativeToolCatalog {
    tools: NativeTools,
}

impl NativeToolCatalog {
    pub fn new(tools: NativeTools) -> Self {
        Self { tools }
    }

    pub fn metadata() -> Vec<NativeToolMetadata> {
        vec![
            native_metadata(
                "native::read",
                "Read a UTF-8 file beneath the project root",
                ToolAccess::ReadOnly,
                serde_json::json!({"type":"object","additionalProperties":false,"required":["path"],"properties":{"path":{"type":"string"},"offset":{"type":"integer","minimum":1},"limit":{"type":"integer","minimum":1}}}),
            ),
            native_metadata(
                "native::write",
                "Write a file beneath the project root",
                ToolAccess::Write,
                serde_json::json!({"type":"object","additionalProperties":false,"required":["path","content"],"properties":{"path":{"type":"string"},"content":{"type":"string"}}}),
            ),
            native_metadata(
                "native::edit",
                "Replace exactly one text match beneath the project root",
                ToolAccess::Write,
                serde_json::json!({"type":"object","additionalProperties":false,"required":["path","old","new"],"properties":{"path":{"type":"string"},"old":{"type":"string"},"new":{"type":"string"}}}),
            ),
            native_metadata(
                "native::list",
                "List a directory beneath the project root",
                ToolAccess::ReadOnly,
                serde_json::json!({"type":"object","additionalProperties":false,"required":["path"],"properties":{"path":{"type":"string"}}}),
            ),
            native_metadata(
                "native::search",
                "Search text beneath the project root",
                ToolAccess::ReadOnly,
                serde_json::json!({"type":"object","additionalProperties":false,"required":["path","query"],"properties":{"path":{"type":"string"},"query":{"type":"string"}}}),
            ),
            native_metadata(
                "native::grep",
                "Search project files with a regular expression",
                ToolAccess::ReadOnly,
                serde_json::json!({"type":"object","additionalProperties":false,"required":["pattern"],"properties":{"pattern":{"type":"string"},"path":{"type":"string"},"glob":{"type":"string"},"case_insensitive":{"type":"boolean"}}}),
            ),
            native_metadata(
                "native::glob",
                "List project files matching a doublestar glob",
                ToolAccess::ReadOnly,
                serde_json::json!({"type":"object","additionalProperties":false,"required":["pattern"],"properties":{"pattern":{"type":"string"}}}),
            ),
            native_metadata(
                "native::bash",
                "Run a bounded shell command in the project root. Default timeout: 2 minutes. Maximum timeout: 10 minutes. Pass timeout_ms to override.",
                ToolAccess::Write,
                serde_json::json!({"type":"object","additionalProperties":false,"required":["command"],"properties":{"command":{"type":"string"},"timeout_ms":{"type":"integer","minimum":1,"maximum":600000}}}),
            ),
            native_metadata(
                "native::git_read",
                "Inspect git state in the project root without any write surface",
                ToolAccess::ReadOnly,
                serde_json::json!({"type":"object","additionalProperties":false,"required":["operation"],"properties":{"operation":{"type":"string","enum":["status","diff","log","branch_merged","merge_base"]},"base":{"type":"string"},"head":{"type":"string"},"staged":{"type":"boolean"},"limit":{"type":"integer","minimum":1}}}),
            ),
            native_metadata(
                "native::cd",
                "Move the session's working directory, so later calls resolve relative paths and run commands there. Accepts a path under the session root, or a worktree this session can reach.",
                ToolAccess::Write,
                serde_json::json!({"type":"object","additionalProperties":false,"required":["path"],"properties":{"path":{"type":"string"}}}),
            ),
            native_metadata(
                "native::worktree",
                "Create a git worktree for this session on a new branch and move the session into it. Branch defaults to the worktree name and start point to HEAD. This checks the repository out: repository hooks are disabled, but content filters the repository configures still run, so it executes what the repository configures the same way a checkout does.",
                ToolAccess::Write,
                serde_json::json!({"type":"object","additionalProperties":false,"required":["name"],"properties":{"name":{"type":"string"},"branch":{"type":"string"},"start_point":{"type":"string"}}}),
            ),
            native_metadata(
                "native::webfetch",
                "Fetch an HTTP or HTTPS URL without credentials",
                ToolAccess::ReadOnly,
                serde_json::json!({"type":"object","additionalProperties":false,"required":["url"],"properties":{"url":{"type":"string"},"timeout_ms":{"type":"integer","minimum":1}}}),
            ),
        ]
    }

    pub fn execute(
        &mut self,
        name: &str,
        arguments: Value,
        context: &ToolExecutionContext,
    ) -> Result<ToolOutput, Error> {
        if let Err(status) = context.check() {
            return Ok(sanitized_execution_status(status));
        }
        let arguments = arguments
            .as_object()
            .ok_or_else(|| Error::Tool("native tool arguments must be an object".into()))?;
        let string = |key: &str| {
            arguments
                .get(key)
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Tool("native tool arguments are invalid".into()))
        };
        let output = match name {
            "native::read" => {
                let mut input = ReadFileInput::new(string("path")?);
                let offset = nonzero_read_bound(arguments, "offset")?;
                let limit = nonzero_read_bound(arguments, "limit")?;
                if offset.is_some() || limit.is_some() {
                    input = input.with_range(offset.unwrap_or(1), limit.unwrap_or(usize::MAX));
                }
                self.tools.read_file(input)?
            }
            "native::write" => self.tools.write_file_with_context(
                WriteFileInput::new(string("path")?, string("content")?),
                Some(context),
            )?,
            "native::edit" => self.tools.edit_file_with_context(
                EditFileInput::new(string("path")?, string("old")?, string("new")?),
                Some(context),
            )?,
            "native::list" => self
                .tools
                .list_directory(ListDirectoryInput::new(string("path")?))?,
            "native::search" => self.tools.search_with_context(
                SearchInput::new(string("path")?, string("query")?),
                Some(context),
            )?,
            "native::grep" => {
                let mut input = GrepInput::new(string("pattern")?);
                if let Some(path) = arguments.get("path").and_then(Value::as_str) {
                    input = input.with_path(path);
                }
                if let Some(glob) = arguments.get("glob").and_then(Value::as_str) {
                    input = input.with_file_glob(glob);
                }
                if let Some(case_insensitive) =
                    arguments.get("case_insensitive").and_then(Value::as_bool)
                {
                    input = input.with_case_insensitive(case_insensitive);
                }
                self.tools.grep_with_context(input, Some(context))?
            }
            "native::glob" => self.tools.glob(GlobInput::new(string("pattern")?))?,
            "native::cd" => self.tools.change_directory(Path::new(string("path")?))?,
            "native::worktree" => {
                let name = string("name")?;
                let branch = arguments
                    .get("branch")
                    .and_then(Value::as_str)
                    .unwrap_or(name);
                let start_point = arguments
                    .get("start_point")
                    .and_then(Value::as_str)
                    .unwrap_or("HEAD");

                self.tools
                    .create_worktree_with_context(name, branch, start_point, Some(context))?
            }
            "native::git_read" => {
                let Some(operation) = GitReadOperation::parse(string("operation")?) else {
                    return Ok(ToolOutput::failure("git_read: unknown operation"));
                };
                let mut input =
                    GitReadInput::new(operation).with_execution_context(context.clone());
                if let Some(base) = arguments.get("base").and_then(Value::as_str) {
                    input = input.with_base(base);
                }
                if let Some(head) = arguments.get("head").and_then(Value::as_str) {
                    input = input.with_head(head);
                }
                if let Some(staged) = arguments.get("staged").and_then(Value::as_bool) {
                    input = input.with_staged(staged);
                }
                if let Some(limit) = arguments.get("limit").and_then(Value::as_u64) {
                    input = input.with_limit(limit as usize);
                }
                if let Ok(remaining) = context.remaining() {
                    input = input.capped_at(remaining);
                }
                self.tools.git_read(input)?
            }
            "native::webfetch" => {
                let mut input = WebfetchInput::new(string("url")?);
                if let Some(timeout) = arguments.get("timeout_ms").and_then(Value::as_u64) {
                    input = input.with_timeout(Duration::from_millis(timeout));
                } else if arguments.contains_key("timeout_ms") {
                    return Err(Error::Tool("native tool arguments are invalid".into()));
                }
                self.tools
                    .webfetch(input.with_execution_context(context.clone()))?
            }
            "native::bash" => {
                let Some(command) = arguments.get("command").and_then(Value::as_str) else {
                    return Ok(ToolOutput::failure("bash: command must be a string"));
                };
                let timeout = match arguments.get("timeout_ms") {
                    Some(timeout) => match timeout.as_u64() {
                        Some(ms) => {
                            let requested = Duration::from_millis(ms);
                            if requested > MAX_BASH_TIMEOUT {
                                return Ok(ToolOutput::failure(format!(
                                    "bash: timeout {}ms exceeds maximum allowed {}ms",
                                    ms,
                                    MAX_BASH_TIMEOUT.as_millis()
                                )));
                            }
                            requested
                        }
                        None => return Ok(ToolOutput::failure("bash: timeout must be an integer")),
                    },
                    None => self.tools.limits.bash_timeout,
                };
                self.tools.bash(
                    BashInput::new(command)
                        .with_timeout(timeout)
                        .with_execution_context(context.clone()),
                )?
            }
            _ => return Err(Error::Tool("unknown native tool".into())),
        };
        if let Err(status) = context.check() {
            return Ok(sanitized_execution_status(status));
        }
        Ok(output)
    }
}

fn nonzero_read_bound(
    arguments: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<usize>, Error> {
    let Some(value) = arguments.get(key) else {
        return Ok(None);
    };
    let Some(value) = value.as_u64().filter(|value| *value > 0) else {
        return Err(Error::Tool("native tool arguments are invalid".into()));
    };

    usize::try_from(value)
        .map(Some)
        .map_err(|_| Error::Tool("native tool arguments are invalid".into()))
}

fn native_metadata(
    qualified_name: &str,
    description: &str,
    access: ToolAccess,
    input_schema: Value,
) -> NativeToolMetadata {
    NativeToolMetadata {
        qualified_name: qualified_name.into(),
        description: description.into(),
        input_schema,
        access,
    }
}

fn validate_limits(limits: &NativeToolLimits) -> Result<(), Error> {
    if limits.max_list_entries == 0
        || limits.max_search_entries == 0
        || limits.max_search_results == 0
        || limits.max_search_depth == 0
        || limits.operation_timeout.is_zero()
        || limits.bash_timeout.is_zero()
    {
        return Err(Error::Tool(
            "native tool limits must be greater than zero".into(),
        ));
    }

    Ok(())
}

/// Asks the rules that authorized a search whether it may report what one of
/// the files it walked into holds.
///
/// A search is authorized on its pattern and on the root it was given, neither
/// of which names the files under that root, so this is asked once per file
/// actually read. A caller holding no context — a direct use of the tool
/// outside any permission decision — reads everything.
fn permits_read(context: Option<&ToolExecutionContext>, relative: &Path) -> bool {
    context.is_none_or(|context| context.permits_read(&relative.to_string_lossy()))
}

fn build_glob_set(pattern: &str, tool: &str) -> Result<GlobSet, ToolOutput> {
    validate_relative_glob_pattern(pattern, tool)?;

    let glob = Glob::new(pattern)
        .map_err(|_| ToolOutput::failure(format!("{tool}: invalid glob pattern")))?;
    let mut builder = GlobSetBuilder::new();
    builder.add(glob);
    builder
        .build()
        .map_err(|_| ToolOutput::failure(format!("{tool}: invalid glob pattern")))
}

fn validate_relative_glob_pattern(pattern: &str, tool: &str) -> Result<(), ToolOutput> {
    let path = Path::new(pattern);
    let has_windows_prefix = pattern.starts_with('\\')
        || pattern.as_bytes().get(..3).is_some_and(|prefix| {
            prefix[0].is_ascii_alphabetic()
                && prefix[1] == b':'
                && matches!(prefix[2], b'/' | b'\\')
        });

    if path.is_absolute()
        || has_windows_prefix
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(ToolOutput::failure(format!(
            "{tool}: glob pattern must be relative"
        )));
    }

    Ok(())
}

/// What one `search` call carries through its traversal: what it looks for,
/// what it is allowed to read, and what it has found so far.
struct SearchWalk<'a> {
    query: &'a str,
    context: Option<&'a ToolExecutionContext>,
    budget: SearchBudget,
    results: Vec<String>,
    withheld: bool,
}

struct SearchBudget {
    deadline: Instant,
    entries_seen: usize,
    max_entries: usize,
    tool: &'static str,
}

impl SearchBudget {
    fn new(limits: &NativeToolLimits, tool: &'static str) -> Self {
        Self {
            deadline: Instant::now() + limits.operation_timeout,
            entries_seen: 0,
            max_entries: limits.max_search_entries,
            tool,
        }
    }

    fn check_deadline(&self) -> Result<(), ToolOutput> {
        if Instant::now() >= self.deadline {
            return Err(ToolOutput::failure(format!(
                "{}: operation timed out",
                self.tool
            )));
        }

        Ok(())
    }

    fn consume_entry(&mut self) -> Result<(), ToolOutput> {
        self.check_deadline()?;
        if self.entries_seen == self.max_entries {
            return Err(ToolOutput::failure(format!(
                "{}: entry limit of {} exceeded",
                self.tool, self.max_entries
            )));
        }

        self.entries_seen += 1;
        Ok(())
    }
}

#[cfg(unix)]
fn read_grep_file_confined(
    project_root: &fs::File,
    path: &Path,
) -> Result<Option<String>, ToolOutput> {
    let (directory, file_name) = open_confined_parent(project_root, path, false, "grep")?;
    let mut file = open_confined_file(&directory, &file_name, "grep")?;
    let metadata = checked_regular_file(&file, "grep")?;
    if metadata.len() > MAX_FILE_BYTES {
        return Ok(None);
    }

    let mut content = Vec::new();
    std::io::Read::by_ref(&mut file)
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut content)
        .map_err(|error| ToolOutput::failure(format!("grep: {error}")))?;
    if content.len() > MAX_FILE_BYTES as usize || content.contains(&0) {
        return Ok(None);
    }
    Ok(String::from_utf8(content).ok())
}

#[cfg(unix)]
fn read_file_confined(
    project_root: &fs::File,
    input: &ReadFileInput,
) -> Result<ToolOutput, ToolOutput> {
    let (directory, file_name) = open_confined_parent(project_root, &input.path, false, "read")?;
    let mut file = open_confined_file(&directory, &file_name, "read")?;
    let metadata = checked_regular_file(&file, "read")?;
    if metadata.len() > MAX_FILE_BYTES {
        return Err(ToolOutput::failure("read: file exceeds 1048576 byte limit"));
    }
    let mut content = String::new();
    file.read_to_string(&mut content)
        .map_err(|error| ToolOutput::failure(format!("read: {error}")))?;
    Ok(ToolOutput::success(read_range(&content, input.range)))
}

#[cfg(unix)]
fn read_range(content: &str, range: Option<(usize, usize)>) -> String {
    let (offset, limit) = range.unwrap_or((1, usize::MAX));
    content
        .split_inclusive('\n')
        .skip(offset - 1)
        .take(limit)
        .collect()
}

/// Where an authorized write/edit should land.
enum AuthorizedWritePath {
    /// Project-relative path for confined openat.
    Confined(PathBuf),
    /// Absolute path outside the confinement root (permission already Allow'd).
    External(PathBuf),
}

impl AuthorizedWritePath {
    fn display_path(&self) -> String {
        match self {
            Self::Confined(path) | Self::External(path) => path.display().to_string(),
        }
    }
}

fn is_path_confinement_refusal(output: &ToolOutput) -> bool {
    output.is_error
        && (output.content.contains("outside project root")
            || output.content.contains("traversal is not allowed"))
}

/// Collapses `.` / `..` without requiring the leaf to exist yet.
fn normalize_external_path(path: &Path) -> Result<PathBuf, ToolOutput> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(ToolOutput::failure("path: outside project root"));
                }
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    if normalized.as_os_str().is_empty() {
        return Err(ToolOutput::failure("path: path must name a file"));
    }
    Ok(normalized)
}

/// Unconfined write used only after the permission gate Allow'd an external path.
fn write_file_external(
    path: &Path,
    content: &[u8],
    context: Option<&ToolExecutionContext>,
) -> Result<bool, ToolOutput> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| ToolOutput::failure("path: path must name a file"))?;
    fs::create_dir_all(parent)
        .map_err(|error| ToolOutput::failure(format!("write: cannot create parent: {error}")))?;
    let is_new_file = !path.exists();
    let temp = parent.join(format!(
        ".agens-write-{}-{}",
        std::process::id(),
        TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        fs::write(&temp, content)
            .map_err(|error| ToolOutput::failure(format!("write: {error}")))?;
        if context.is_some_and(ToolExecutionContext::is_cancelled) {
            return Err(ToolOutput::failure("tool execution cancelled"));
        }
        fs::rename(&temp, path)
            .map_err(|error| ToolOutput::failure(format!("write: cannot commit file: {error}")))?;
        Ok(is_new_file)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Unconfined edit used only after the permission gate Allow'd an external path.
fn edit_file_external(
    path: &Path,
    old: &str,
    new: &str,
    context: Option<&ToolExecutionContext>,
) -> Result<ToolOutput, ToolOutput> {
    let content =
        fs::read_to_string(path).map_err(|error| ToolOutput::failure(format!("edit: {error}")))?;
    let matches = content.matches(old).count();
    if matches == 0 {
        return Err(ToolOutput::failure("edit: old text not found"));
    }
    if matches > 1 {
        return Err(ToolOutput::failure(
            "edit: old text matches more than once; make it unique",
        ));
    }
    let updated = content.replacen(old, new, 1);
    write_file_external(path, updated.as_bytes(), context)?;
    Ok(ToolOutput::success(format!("edited {}", path.display())))
}

#[cfg(unix)]
fn write_file_confined(
    project_root: &fs::File,
    path: &Path,
    content: &[u8],
    context: Option<&ToolExecutionContext>,
) -> Result<bool, ToolOutput> {
    use std::{
        ffi::CString,
        os::fd::{AsRawFd, FromRawFd},
    };

    let (directory, file_name) = open_confined_parent(project_root, path, true, "write")?;
    let existing = open_confined_file_optional(&directory, &file_name, "write")?
        .map(|file| checked_regular_file(&file, "write").map(|metadata| file_identity(&metadata)))
        .transpose()?;
    let temp_name = CString::new(format!(
        ".agens-write-{}-{}",
        std::process::id(),
        TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ))
    .expect("generated temporary name has no null byte");
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            temp_name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if descriptor < 0 {
        return Err(ToolOutput::failure(format!(
            "write: cannot create temporary file: {}",
            io::Error::last_os_error()
        )));
    }
    let mut temp = unsafe { fs::File::from_raw_fd(descriptor) };
    let result = (|| {
        temp.write_all(content)
            .map_err(|error| ToolOutput::failure(format!("write: {error}")))?;
        temp.sync_all()
            .map_err(|error| ToolOutput::failure(format!("write: {error}")))?;
        if context.is_some_and(ToolExecutionContext::is_cancelled) {
            return Err(ToolOutput::failure("tool execution cancelled"));
        }
        recheck_write_target(&directory, &file_name, existing)?;
        let renamed = unsafe {
            libc::renameat(
                directory.as_raw_fd(),
                temp_name.as_ptr(),
                directory.as_raw_fd(),
                file_name.as_ptr(),
            )
        };
        if renamed != 0 {
            return Err(ToolOutput::failure(format!(
                "write: cannot commit temporary file: {}",
                io::Error::last_os_error()
            )));
        }
        directory
            .sync_all()
            .map_err(|error| ToolOutput::failure(format!("write: {error}")))
    })();
    if result.is_err() {
        unsafe {
            libc::unlinkat(directory.as_raw_fd(), temp_name.as_ptr(), 0);
        }
    }
    result.map(|()| existing.is_none())
}

#[cfg(unix)]
fn edit_file_confined(
    project_root: &fs::File,
    path: &Path,
    old: &str,
    new: &str,
    context: Option<&ToolExecutionContext>,
) -> Result<ToolOutput, ToolOutput> {
    use std::os::unix::fs::MetadataExt;

    let (directory, file_name) = open_confined_parent(project_root, path, false, "edit")?;
    let mut file = open_confined_file(&directory, &file_name, "edit")?;
    let metadata = checked_regular_file(&file, "edit")?;
    if metadata.len() > MAX_FILE_BYTES {
        return Err(ToolOutput::failure("edit: file exceeds 1048576 byte limit"));
    }

    let mut original = String::new();
    file.read_to_string(&mut original)
        .map_err(|error| ToolOutput::failure(format!("edit: {error}")))?;
    let Some(match_offset) = original.find(old) else {
        return Err(ToolOutput::failure("edit: old text was not found"));
    };
    let next_start = match_offset + original[match_offset..].chars().next().unwrap().len_utf8();
    if original[next_start..].contains(old) {
        return Err(ToolOutput::failure(
            "edit: old text matched multiple locations",
        ));
    }

    let replacement = original.replacen(old, new, 1);
    let original_identity = (metadata.dev(), metadata.ino());
    let diff = unified_edit_diff(path, &original, &replacement, old, new, match_offset);
    write_edit_temp(
        &directory,
        &file_name,
        replacement.as_bytes(),
        original_identity,
        context,
    )?;
    Ok(
        ToolOutput::success(diff.text).with_facts(ToolResultFacts::Edit {
            path: FactPath::new(&path.display().to_string()),
            outcome: ToolOutcome::Succeeded,
            changed: Some(EditMagnitude {
                lines_added: diff.lines_added,
                lines_removed: diff.lines_removed,
            }),
        }),
    )
}

#[cfg(unix)]
fn write_edit_temp(
    directory: &fs::File,
    file_name: &std::ffi::CString,
    content: &[u8],
    expected: (u64, u64),
    context: Option<&ToolExecutionContext>,
) -> Result<(), ToolOutput> {
    use std::{
        ffi::CString,
        os::fd::{AsRawFd, FromRawFd},
    };

    let temp_name = CString::new(format!(
        ".agens-edit-{}-{}",
        std::process::id(),
        TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ))
    .expect("generated temporary name has no null byte");
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            temp_name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if descriptor < 0 {
        return Err(ToolOutput::failure(format!(
            "edit: cannot create temporary file: {}",
            io::Error::last_os_error()
        )));
    }

    let mut temp = unsafe { fs::File::from_raw_fd(descriptor) };
    let result = (|| {
        temp.write_all(content)
            .map_err(|error| ToolOutput::failure(format!("edit: {error}")))?;
        temp.sync_all()
            .map_err(|error| ToolOutput::failure(format!("edit: {error}")))?;
        if context.is_some_and(ToolExecutionContext::is_cancelled) {
            return Err(ToolOutput::failure("tool execution cancelled"));
        }
        #[cfg(test)]
        run_edit_test_hook(
            EditTestHookPoint::BeforeTargetRecheck,
            directory,
            &temp_name,
        );
        let target = open_confined_file(directory, file_name, "edit")?;
        if file_identity(&checked_regular_file(&target, "edit")?) != expected {
            return Err(ToolOutput::failure("edit: target changed during edit"));
        }
        #[cfg(test)]
        run_edit_test_hook(EditTestHookPoint::BeforeRename, directory, &temp_name);
        if context.is_some_and(ToolExecutionContext::is_cancelled) {
            return Err(ToolOutput::failure("tool execution cancelled"));
        }
        if unsafe {
            libc::renameat(
                directory.as_raw_fd(),
                temp_name.as_ptr(),
                directory.as_raw_fd(),
                file_name.as_ptr(),
            )
        } != 0
        {
            return Err(ToolOutput::failure(format!(
                "edit: cannot commit temporary file: {}",
                io::Error::last_os_error()
            )));
        }
        directory
            .sync_all()
            .map_err(|error| ToolOutput::failure(format!("edit: {error}")))
    })();
    if result.is_err() {
        unsafe { libc::unlinkat(directory.as_raw_fd(), temp_name.as_ptr(), 0) };
    }
    result
}

#[cfg(unix)]
struct EditDiff {
    text: String,
    lines_added: usize,
    lines_removed: usize,
}

#[cfg(unix)]
fn unified_edit_diff(
    path: &Path,
    original: &str,
    replacement: &str,
    old: &str,
    new: &str,
    match_offset: usize,
) -> EditDiff {
    const CONTEXT_LINES: usize = 3;
    let old_lines: Vec<_> = original.lines().collect();
    let new_lines: Vec<_> = replacement.lines().collect();
    let changed = original[..match_offset]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count();
    let old_end =
        (changed + old.bytes().filter(|byte| *byte == b'\n').count() + 1).min(old_lines.len());
    let new_end =
        (changed + new.bytes().filter(|byte| *byte == b'\n').count() + 1).min(new_lines.len());
    let start = changed.saturating_sub(CONTEXT_LINES);
    let old_tail_end = old_end.saturating_add(CONTEXT_LINES).min(old_lines.len());
    let new_tail_end = new_end.saturating_add(CONTEXT_LINES).min(new_lines.len());
    let mut diff = format!(
        "--- {}\n+++ {}\n@@ -{},{} +{},{} @@\n",
        path.display(),
        path.display(),
        start + 1,
        old_tail_end - start,
        start + 1,
        new_tail_end - start
    );
    for line in &old_lines[start..changed] {
        diff.push_str(&format!(" {line}\n"));
    }
    for line in &old_lines[changed..old_end] {
        diff.push_str(&format!("-{line}\n"));
    }
    for line in &new_lines[changed..new_end] {
        diff.push_str(&format!("+{line}\n"));
    }
    for line in &new_lines[new_end..new_tail_end] {
        diff.push_str(&format!(" {line}\n"));
    }

    EditDiff {
        text: diff,
        lines_added: new_end - changed,
        lines_removed: old_end - changed,
    }
}

#[cfg(all(test, unix))]
mod native_tool_tests {
    use super::*;
    use std::{
        os::unix::ffi::OsStrExt,
        os::unix::fs::symlink,
        sync::atomic::{AtomicUsize, Ordering},
    };

    static NEXT_ROOT: AtomicUsize = AtomicUsize::new(0);

    /// `write_edit_temp` names its temporary file from the process-global
    /// `TEMP_FILE_SEQUENCE`, and some tests predict that name ahead of time.
    /// Any concurrently running edit that reaches `write_edit_temp` also
    /// advances the same counter, invalidating that prediction, so tests
    /// relying on it must not run concurrently with each other.
    static SEQUENTIAL_EDIT_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn project_root() -> PathBuf {
        let suffix = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("agens-tools-unit-{}-{suffix}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn temp_name(sequence: usize) -> PathBuf {
        PathBuf::from(format!(".agens-edit-{}-{sequence}", std::process::id()))
    }

    #[test]
    fn single_file_grep_rejects_a_symlink_swap_after_path_validation() {
        let root = project_root();
        let outside = root.with_extension("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(root.join("target.txt"), "safe").unwrap();
        fs::write(outside.join("secret.txt"), "SENTINEL_OUTSIDE").unwrap();
        let tools = NativeTools::open(&root).unwrap();
        let target = root.join("target.txt");
        let outside_target = outside.join("secret.txt");
        set_grep_test_hook(move || {
            fs::remove_file(&target).unwrap();
            symlink(outside_target, target).unwrap();
        });

        let failure = tools
            .grep(GrepInput::new("SENTINEL_OUTSIDE").with_path("target.txt"))
            .expect("grep failures should remain tool results");
        assert_eq!(failure, ToolOutput::failure("path: outside project root"));
        assert!(!failure.content.contains("SENTINEL_OUTSIDE"));

        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn write_reports_a_comparable_magnitude_for_a_new_file() {
        let _sequential_edit_guard = SEQUENTIAL_EDIT_TEST_LOCK.lock().unwrap();
        let root = project_root();
        let tools = NativeTools::open(&root).unwrap();
        let body = "café \u{1F600}\nsecond line";

        let output = tools
            .write_file(WriteFileInput::new("notes.txt", body))
            .unwrap();

        assert!(!output.is_error);
        assert_eq!(
            output.facts(),
            Some(&ToolResultFacts::Write {
                path: FactPath::new("notes.txt"),
                outcome: ToolOutcome::Succeeded,
                written: Some(WriteMagnitude {
                    is_new_file: true,
                    bytes_written: body.len(),
                    lines_written: 2,
                }),
            })
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn overwriting_an_existing_file_is_distinguishable_from_creating_one() {
        let _sequential_edit_guard = SEQUENTIAL_EDIT_TEST_LOCK.lock().unwrap();
        let root = project_root();
        fs::write(root.join("notes.txt"), "before").unwrap();
        let tools = NativeTools::open(&root).unwrap();

        let output = tools
            .write_file(WriteFileInput::new("notes.txt", "after"))
            .unwrap();

        assert!(!output.is_error);
        match output.facts() {
            Some(ToolResultFacts::Write { written, .. }) => {
                assert_eq!(written.map(|written| written.is_new_file), Some(false));
            }
            other => panic!("expected write facts, got {other:?}"),
        }

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn edit_reports_line_counts_matching_its_own_diff() {
        let _sequential_edit_guard = SEQUENTIAL_EDIT_TEST_LOCK.lock().unwrap();
        let root = project_root();
        fs::write(root.join("notes.txt"), "one\ntwo\nthree\n").unwrap();
        let tools = NativeTools::open(&root).unwrap();

        let output = tools
            .edit_file(EditFileInput::new("notes.txt", "two", "alpha\nbeta\ngamma"))
            .unwrap();

        assert!(!output.is_error);
        let diff_added = output
            .content
            .lines()
            .filter(|line| line.starts_with('+') && !line.starts_with("+++"))
            .count();
        let diff_removed = output
            .content
            .lines()
            .filter(|line| line.starts_with('-') && !line.starts_with("---"))
            .count();
        assert_eq!(
            output.facts(),
            Some(&ToolResultFacts::Edit {
                path: FactPath::new("notes.txt"),
                outcome: ToolOutcome::Succeeded,
                changed: Some(EditMagnitude {
                    lines_added: 3,
                    lines_removed: 1,
                }),
            })
        );
        assert_eq!(diff_added, 3);
        assert_eq!(diff_removed, 1);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bash_reports_its_exit_code_and_outcome() {
        let root = project_root();
        let tools = NativeTools::open(&root).unwrap();

        for (command, expected_code, expected_outcome) in [
            ("exit 0", 0, ToolOutcome::Succeeded),
            ("exit 3", 3, ToolOutcome::Failed),
        ] {
            let output = tools
                .bash(BashInput::new(command).with_timeout(Duration::from_secs(5)))
                .unwrap();

            assert_eq!(
                output.facts(),
                Some(&ToolResultFacts::Bash {
                    outcome: expected_outcome,
                    exit_code: Some(expected_code)
                })
            );
        }

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bash_timeout_reports_no_facts() {
        let root = project_root();
        let tools = NativeTools::open(&root).unwrap();

        let output = tools
            .bash(BashInput::new("sleep 5").with_timeout(Duration::from_millis(50)))
            .unwrap();

        assert!(output.is_error);
        assert_eq!(output.facts(), None);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bash_timeout_message_suggests_retry() {
        let root = project_root();
        let tools = NativeTools::open(&root).unwrap();

        let output = tools
            .bash(BashInput::new("sleep 5").with_timeout(Duration::from_millis(50)))
            .unwrap();

        assert!(output.is_error);
        assert!(output.content.contains("timed out after"));
        assert!(output.content.contains("retry with a larger timeout"));
        assert!(output.content.contains("max:"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bash_rejects_timeout_above_max() {
        let root = project_root();
        let tools = NativeTools::open(&root).unwrap();
        let mut catalog = NativeToolCatalog::new(tools);
        let context = ToolExecutionContext::with_timeout(Duration::from_secs(60));

        let result = catalog.execute(
            "native::bash",
            serde_json::json!({"command": "echo test", "timeout_ms": 999999999}),
            &context,
        );

        let output = result.unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("exceeds maximum allowed"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bash_accepts_timeout_up_to_max() {
        let root = project_root();
        let tools = NativeTools::open(&root).unwrap();
        let mut catalog = NativeToolCatalog::new(tools);
        let context = ToolExecutionContext::with_timeout(Duration::from_secs(60));

        let result = catalog.execute(
            "native::bash",
            serde_json::json!({"command": "echo test", "timeout_ms": 600000}),
            &context,
        );

        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(!output.is_error);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failing_writes_and_edits_report_failure_facts() {
        let _sequential_edit_guard = SEQUENTIAL_EDIT_TEST_LOCK.lock().unwrap();

        let root = project_root();
        fs::write(root.join("notes.txt"), "old").unwrap();
        let tools = NativeTools::open(&root).unwrap();

        let rejected_write = tools
            .write_file(WriteFileInput::new("../outside.txt", "x"))
            .unwrap();
        assert!(rejected_write.is_error);
        match rejected_write.facts() {
            Some(ToolResultFacts::Write {
                path,
                outcome,
                written,
            }) => {
                assert!(!path.is_representable());
                assert_eq!(*outcome, ToolOutcome::Failed);
                assert_eq!(*written, None);
            }
            other => panic!("expected write failure facts, got {other:?}"),
        }

        let unnameable_write = tools
            .write_file(WriteFileInput::new(".", "content"))
            .unwrap();
        assert!(unnameable_write.is_error);
        match unnameable_write.facts() {
            Some(ToolResultFacts::Write {
                path,
                outcome,
                written,
            }) => {
                assert_eq!(path.relative(), Some("."));
                assert_eq!(*outcome, ToolOutcome::Failed);
                assert_eq!(*written, None);
            }
            other => panic!("expected write failure facts, got {other:?}"),
        }

        let rejected_edit = tools
            .edit_file(EditFileInput::new("../outside.txt", "old", "new"))
            .unwrap();
        assert!(rejected_edit.is_error);
        match rejected_edit.facts() {
            Some(ToolResultFacts::Edit {
                path,
                outcome,
                changed,
            }) => {
                assert!(!path.is_representable());
                assert_eq!(*outcome, ToolOutcome::Failed);
                assert_eq!(*changed, None);
            }
            other => panic!("expected edit failure facts, got {other:?}"),
        }

        let empty_old_edit = tools
            .edit_file(EditFileInput::new("notes.txt", "", "new"))
            .unwrap();
        assert!(empty_old_edit.is_error);
        match empty_old_edit.facts() {
            Some(ToolResultFacts::Edit {
                path,
                outcome,
                changed,
            }) => {
                assert_eq!(path.relative(), Some("notes.txt"));
                assert_eq!(*outcome, ToolOutcome::Failed);
                assert_eq!(*changed, None);
            }
            other => panic!("expected edit failure facts, got {other:?}"),
        }

        let identical_edit = tools
            .edit_file(EditFileInput::new("notes.txt", "old", "old"))
            .unwrap();
        assert!(identical_edit.is_error);
        match identical_edit.facts() {
            Some(ToolResultFacts::Edit {
                path,
                outcome,
                changed,
            }) => {
                assert_eq!(path.relative(), Some("notes.txt"));
                assert_eq!(*outcome, ToolOutcome::Failed);
                assert_eq!(*changed, None);
            }
            other => panic!("expected edit failure facts, got {other:?}"),
        }

        let missing_edit = tools
            .edit_file(EditFileInput::new("notes.txt", "missing", "new"))
            .unwrap();
        assert!(missing_edit.is_error);
        match missing_edit.facts() {
            Some(ToolResultFacts::Edit {
                path,
                outcome,
                changed,
            }) => {
                assert_eq!(path.relative(), Some("notes.txt"));
                assert_eq!(*outcome, ToolOutcome::Failed);
                assert_eq!(*changed, None);
            }
            other => panic!("expected edit failure facts, got {other:?}"),
        }

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn exact_edit_rejects_deterministic_races_and_cleans_up() {
        let _sequential_edit_guard = SEQUENTIAL_EDIT_TEST_LOCK.lock().unwrap();
        let root = project_root();
        let outside = project_root();
        let target = root.join("notes.txt");
        let outside_target = outside.join("outside.txt");
        fs::write(&target, "old").unwrap();
        fs::write(&outside_target, "outside").unwrap();
        let tools = NativeTools::open(&root).unwrap();

        let collision = temp_name(TEMP_FILE_SEQUENCE.load(Ordering::Relaxed));
        symlink(&outside_target, root.join(&collision)).unwrap();
        assert!(
            tools
                .edit_file(EditFileInput::new("notes.txt", "old", "new"))
                .unwrap()
                .is_error
        );
        assert_eq!(fs::read_to_string(&target).unwrap(), "old");
        assert_eq!(fs::read_to_string(&outside_target).unwrap(), "outside");
        assert!(
            fs::symlink_metadata(root.join(&collision))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        fs::remove_file(root.join(&collision)).unwrap();

        let replacement = root.join("replacement.txt");
        fs::write(&replacement, "swapped").unwrap();
        let swapped_target = target.clone();
        set_edit_test_hook(EditTestHookPoint::BeforeTargetRecheck, move |_, _| {
            fs::rename(&replacement, &swapped_target).unwrap();
        });
        let swap_temp = temp_name(TEMP_FILE_SEQUENCE.load(Ordering::Relaxed));
        assert_eq!(
            tools
                .edit_file(EditFileInput::new("notes.txt", "old", "new"))
                .unwrap(),
            ToolOutput::failure("edit: target changed during edit")
        );
        assert_eq!(
            fs::read_to_string(root.join("notes.txt")).unwrap(),
            "swapped"
        );
        assert!(!root.join(swap_temp).exists());

        let cancelled = Arc::new(AtomicBool::new(false));
        let cancellation = Arc::clone(&cancelled);
        set_edit_test_hook(EditTestHookPoint::BeforeRename, move |_, _| {
            cancellation.store(true, Ordering::Release);
        });
        let cancellation_temp = temp_name(TEMP_FILE_SEQUENCE.load(Ordering::Relaxed));
        assert_eq!(
            NativeToolCatalog::new(tools)
                .execute(
                    "native::edit",
                    serde_json::json!({"path": "notes.txt", "old": "swapped", "new": "new"}),
                    &ToolExecutionContext::new(cancelled, Duration::from_secs(1)),
                )
                .unwrap(),
            ToolOutput::failure("tool execution cancelled")
        );
        assert_eq!(
            fs::read_to_string(root.join("notes.txt")).unwrap(),
            "swapped"
        );
        assert!(!root.join(cancellation_temp).exists());

        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn native_read_independent_ranges_preserve_split_inclusive_semantics() {
        let root = project_root();
        let outside = project_root();
        fs::write(root.join("AGENTS.md"), "first\nsecond\nthird\nlast").unwrap();
        fs::write(outside.join("outside.txt"), "outside").unwrap();
        fs::create_dir(root.join("directory")).unwrap();
        symlink(outside.join("outside.txt"), root.join("link.txt")).unwrap();
        unsafe {
            assert_eq!(
                libc::mkfifo(
                    root.join("pipe").as_os_str().as_bytes().as_ptr().cast(),
                    0o600
                ),
                0
            );
        }
        fs::write(
            root.join("large.txt"),
            vec![b'x'; MAX_FILE_BYTES as usize + 1],
        )
        .unwrap();

        let mut catalog = NativeToolCatalog::new(NativeTools::open(&root).unwrap());
        let context = ToolExecutionContext::with_timeout(Duration::from_secs(1));
        let mut read = |arguments| catalog.execute("native::read", arguments, &context);

        assert_eq!(
            read(serde_json::json!({"path":"AGENTS.md","limit":200}))
                .unwrap()
                .content,
            "first\nsecond\nthird\nlast"
        );
        assert_eq!(
            read(serde_json::json!({"path":"AGENTS.md","offset":3}))
                .unwrap()
                .content,
            "third\nlast"
        );
        assert_eq!(
            read(serde_json::json!({"path":"AGENTS.md","offset":2,"limit":1}))
                .unwrap()
                .content,
            "second\n"
        );
        assert_eq!(
            read(serde_json::json!({"path":"AGENTS.md"}))
                .unwrap()
                .content,
            "first\nsecond\nthird\nlast"
        );
        assert_eq!(
            read(serde_json::json!({"path":"AGENTS.md","offset":9}))
                .unwrap()
                .content,
            ""
        );

        for arguments in [
            serde_json::json!({"path":"AGENTS.md","offset":0}),
            serde_json::json!({"path":"AGENTS.md","limit":0}),
            serde_json::json!({"path":"AGENTS.md","offset":-1}),
            serde_json::json!({"path":"AGENTS.md","limit":"1"}),
            serde_json::from_str(r#"{"path":"AGENTS.md","offset":18446744073709551616}"#).unwrap(),
        ] {
            assert!(read(arguments).is_err());
        }

        for path in [
            "../outside.txt",
            "link.txt",
            "directory",
            "pipe",
            "large.txt",
        ] {
            assert!(
                read(serde_json::json!({"path":path})).unwrap().is_error,
                "{path} must remain rejected"
            );
        }

        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn webfetch_address_policy_covers_literals_and_resolved_addresses() {
        assert_eq!(DEFAULT_WEBFETCH_TIMEOUT, Duration::from_secs(30));
        for address in ["169.254.1.1", "100.100.100.200", "fe80::1", "fd00:ec2::254"] {
            assert!(blocked_webfetch_address(address.parse().unwrap()));
        }
        for address in ["127.0.0.1", "10.0.0.1", "::1"] {
            assert!(!blocked_webfetch_address(address.parse().unwrap()));
        }
        let resolved = permitted_webfetch_addresses([
            "127.0.0.1:80".parse().unwrap(),
            "169.254.1.1:80".parse().unwrap(),
            "[fe80::1]:80".parse().unwrap(),
        ]);
        assert_eq!(resolved, vec!["127.0.0.1:80".parse().unwrap()]);
    }
}

#[cfg(unix)]
fn open_confined_parent(
    project_root: &fs::File,
    path: &Path,
    create: bool,
    operation: &str,
) -> Result<(fs::File, std::ffi::CString), ToolOutput> {
    use std::{
        ffi::CString,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::ffi::OsStrExt,
        },
    };

    let file_name = path
        .file_name()
        .ok_or_else(|| ToolOutput::failure(format!("{operation}: path must name a file")))?;
    let file_name = CString::new(file_name.as_bytes())
        .map_err(|_| ToolOutput::failure(format!("{operation}: invalid path component")))?;
    let mut directory = project_root
        .try_clone()
        .map_err(|error| ToolOutput::failure(format!("{operation}: {error}")))?;
    for component in path.parent().unwrap_or_else(|| Path::new("")).components() {
        let Component::Normal(component) = component else {
            continue;
        };
        let component = CString::new(component.as_bytes())
            .map_err(|_| ToolOutput::failure(format!("{operation}: invalid path component")))?;
        let mut descriptor = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                component.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if descriptor < 0 && create && io::Error::last_os_error().kind() == io::ErrorKind::NotFound
        {
            let created =
                unsafe { libc::mkdirat(directory.as_raw_fd(), component.as_ptr(), 0o755) };
            if created != 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) {
                return Err(ToolOutput::failure(format!(
                    "{operation}: cannot create parent directory: {}",
                    io::Error::last_os_error()
                )));
            }
            descriptor = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    component.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
        }
        if descriptor < 0 {
            return Err(confined_open_error(operation, io::Error::last_os_error()));
        }
        directory = unsafe { fs::File::from_raw_fd(descriptor) };
    }
    Ok((directory, file_name))
}

#[cfg(unix)]
fn open_confined_file(
    directory: &fs::File,
    file_name: &std::ffi::CString,
    operation: &str,
) -> Result<fs::File, ToolOutput> {
    open_confined_file_optional(directory, file_name, operation)?
        .ok_or_else(|| confined_open_error(operation, io::Error::from(io::ErrorKind::NotFound)))
}

/// Reports an absent target as `Ok(None)`, because the write path has to tell "nothing is there
/// yet" apart from a real failure and a rendered error message is not a dependable signal for it.
#[cfg(unix)]
fn open_confined_file_optional(
    directory: &fs::File,
    file_name: &std::ffi::CString,
    operation: &str,
) -> Result<Option<fs::File>, ToolOutput> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            file_name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    if descriptor < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(confined_open_error(operation, error));
    }
    Ok(Some(unsafe { fs::File::from_raw_fd(descriptor) }))
}

/// The closed set of reasons a confined filesystem failure may carry to the model. Each names
/// neither a path nor any file content, so the dispatcher forwards them verbatim; any failure
/// outside this set still degrades to the generic message.
pub const NATIVE_FILESYSTEM_FAILURE_REASONS: [&str; 4] = [
    "file not found",
    "permission denied",
    "path is a directory",
    "path is not a regular file",
];

#[cfg(unix)]
fn confined_open_error(operation: &str, error: io::Error) -> ToolOutput {
    if error.raw_os_error() == Some(libc::ELOOP) || error.kind() == io::ErrorKind::NotADirectory {
        return ToolOutput::failure("path: outside project root");
    }
    match canonical_filesystem_reason(&error) {
        Some(reason) => ToolOutput::failure(format!("{operation}: {reason}")),
        None => ToolOutput::failure(format!("{operation}: {error}")),
    }
}

/// Maps the kinds worth telling the model apart, so it stops retrying a file that is not there
/// instead of reading an errno string it cannot act on.
#[cfg(unix)]
fn canonical_filesystem_reason(error: &io::Error) -> Option<&'static str> {
    match error.kind() {
        io::ErrorKind::NotFound => Some("file not found"),
        io::ErrorKind::PermissionDenied => Some("permission denied"),
        io::ErrorKind::IsADirectory => Some("path is a directory"),
        _ => None,
    }
}

#[cfg(unix)]
fn checked_regular_file(file: &fs::File, operation: &str) -> Result<fs::Metadata, ToolOutput> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file
        .metadata()
        .map_err(|error| ToolOutput::failure(format!("{operation}: {error}")))?;
    if !metadata.is_file() {
        return Err(ToolOutput::failure(format!(
            "{operation}: path is not a regular file"
        )));
    }
    if metadata.nlink() != 1 {
        return Err(ToolOutput::failure(format!(
            "{operation}: path has multiple hard links"
        )));
    }
    Ok(metadata)
}

#[cfg(unix)]
fn file_identity(metadata: &fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (metadata.dev(), metadata.ino())
}

#[cfg(unix)]
fn recheck_write_target(
    directory: &fs::File,
    file_name: &std::ffi::CString,
    existing: Option<(u64, u64)>,
) -> Result<(), ToolOutput> {
    match (
        existing,
        open_confined_file_optional(directory, file_name, "write"),
    ) {
        (Some(expected), Ok(Some(file)))
            if file_identity(&checked_regular_file(&file, "write")?) == expected =>
        {
            Ok(())
        }
        (Some(_), Ok(_)) => Err(ToolOutput::failure("write: target changed during write")),
        (Some(_), Err(output)) => Err(output),
        (None, Ok(None)) => Ok(()),
        (None, _) => Err(ToolOutput::failure("write: target changed during write")),
    }
}

struct CappedOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    truncated: bool,
}

#[derive(Default)]
struct StreamCapture {
    bytes: Vec<u8>,
    truncated: bool,
}

impl CappedOutput {
    fn combine(stdout: StreamCapture, stderr: StreamCapture) -> Self {
        let half = MAX_CAPTURED_PROCESS_BYTES / 2;
        let stdout_reserved = stdout.bytes.len().min(half);
        let stderr_reserved = stderr.bytes.len().min(half);
        let mut remaining = MAX_CAPTURED_PROCESS_BYTES - stdout_reserved - stderr_reserved;
        let stdout_len = stdout_reserved + (stdout.bytes.len() - stdout_reserved).min(remaining);
        remaining -= stdout_len - stdout_reserved;
        let stderr_len = stderr_reserved + (stderr.bytes.len() - stderr_reserved).min(remaining);

        Self {
            truncated: stdout.truncated
                || stderr.truncated
                || stdout.bytes.len() > stdout_len
                || stderr.bytes.len() > stderr_len,
            stdout: stdout.bytes[..stdout_len].to_vec(),
            stderr: stderr.bytes[..stderr_len].to_vec(),
        }
    }

    fn render(&self, exit_status: &str, detail: Option<&str>) -> String {
        let stdout = lossy_bash_stream(&self.stdout);
        let stderr = lossy_bash_stream(&self.stderr);
        let detail = detail
            .map(|detail| format!("[{detail}]\n"))
            .unwrap_or_default();
        let exit_status = format!("[exit status: {exit_status}]\n");
        let labels = "[stdout]\n[stderr]\n";
        let marker = "[bash output truncated]\n";
        let mut truncated = self.truncated;

        if !truncated
            && labels.len() + stdout.len() + stderr.len() + detail.len() + exit_status.len()
                > MAX_PROCESS_OUTPUT
        {
            truncated = true;
        }

        let metadata_len =
            labels.len() + detail.len() + exit_status.len() + usize::from(truncated) * marker.len();
        let stream_budget = MAX_PROCESS_OUTPUT.saturating_sub(metadata_len);
        let (stdout, stderr, streams_truncated) =
            allocate_bash_streams(stdout, stderr, stream_budget);
        truncated |= streams_truncated;

        let mut output = String::with_capacity(MAX_PROCESS_OUTPUT);
        output.push_str("[stdout]\n");
        output.push_str(&stdout);
        output.push_str("[stderr]\n");
        output.push_str(&stderr);
        if truncated {
            output.push_str(marker);
        }
        if !detail.is_empty() {
            output.push_str(&detail);
        }
        output.push_str(&exit_status);
        debug_assert!(output.len() <= MAX_PROCESS_OUTPUT);
        output
    }
}

fn lossy_bash_stream(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }

    let mut output = String::from_utf8_lossy(bytes).into_owned();
    if !output.ends_with('\n') {
        output.push('\n');
    }
    output
}

fn allocate_bash_streams(
    stdout: String,
    stderr: String,
    stream_budget: usize,
) -> (String, String, bool) {
    let stdout_minimum = bash_stream_minimum_bytes(&stdout);
    let stderr_minimum = bash_stream_minimum_bytes(&stderr);
    let remaining = stream_budget.saturating_sub(stdout_minimum + stderr_minimum);
    let half = remaining / 2;
    let stdout_budget = stdout_minimum + (stdout.len() - stdout_minimum).min(half);
    let stderr_budget = stderr_minimum + (stderr.len() - stderr_minimum).min(half);
    let remaining = stream_budget - stdout_budget - stderr_budget;
    let stdout_budget = stdout_budget + (stdout.len() - stdout_budget).min(remaining);
    let truncated = stdout_budget < stdout.len() || stderr_budget < stderr.len();
    let stdout = truncate_bash_stream(&stdout, stdout_budget);
    let stderr = truncate_bash_stream(&stderr, stderr_budget);

    (stdout, stderr, truncated)
}

fn bash_stream_minimum_bytes(stream: &str) -> usize {
    match stream.chars().next() {
        None => 0,
        Some('\n') => 1,
        Some(character) => character.len_utf8() + 1,
    }
}

fn truncate_bash_stream(stream: &str, budget: usize) -> String {
    if stream.len() <= budget {
        return stream.to_owned();
    }

    let mut end = stream.floor_char_boundary(budget);
    if stream[..end].ends_with('\n') {
        return stream[..end].to_owned();
    }

    end = stream.floor_char_boundary(end.saturating_sub(1));
    let mut truncated = stream[..end].to_owned();
    truncated.push('\n');
    truncated
}

fn render_bash_result(
    output: &CappedOutput,
    exit_status: &str,
    detail: Option<&str>,
) -> ToolOutput {
    let output = output.render(exit_status, detail);
    if detail.is_some() || exit_status != "0" {
        ToolOutput::failure(output)
    } else {
        ToolOutput::success(output)
    }
}

fn read_capped(
    mut reader: impl Read + Send + 'static,
) -> thread::JoinHandle<Result<StreamCapture, io::Error>> {
    thread::spawn(move || {
        let mut output = StreamCapture::default();
        let mut buffer = [0; 8192];
        loop {
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                return Ok(output);
            }

            let remaining = MAX_CAPTURED_PROCESS_BYTES.saturating_sub(output.bytes.len());
            output
                .bytes
                .extend_from_slice(&buffer[..count.min(remaining)]);
            output.truncated |= count > remaining;
        }
    })
}

fn wait_for_readers(
    stdout_reader: thread::JoinHandle<Result<StreamCapture, io::Error>>,
    stderr_reader: thread::JoinHandle<Result<StreamCapture, io::Error>>,
) -> Result<CappedOutput, Error> {
    let stdout = stdout_reader
        .join()
        .map_err(|_| Error::Tool("bash: output reader failed".into()))?
        .map_err(|_| Error::Tool("bash: output reader failed".into()))?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| Error::Tool("bash: output reader failed".into()))?
        .map_err(|_| Error::Tool("bash: output reader failed".into()))?;

    Ok(CappedOutput::combine(stdout, stderr))
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
