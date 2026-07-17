use std::{
    cell::Cell,
    collections::BTreeMap,
    fmt, fs,
    io::{self, Read, Write},
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

use agens_core::{
    Error, PermissionDecision, PermissionPolicy, PermissionRequest, PermissionSession,
    ProjectPermissionGrant, ToolAccess,
};
use serde::Deserialize;
use serde::de::{self, DeserializeSeed, Deserializer, IgnoredAny, MapAccess, Visitor};
use serde_json::Value;

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
const DEFAULT_MAX_MCP_LIST_PAGES: usize = 128;
const DEFAULT_MAX_MCP_TOOLS: usize = 1_000;
const SKILL_MANIFEST_NAME: &str = "SKILL.md";
const MAX_SKILL_DIRECTORIES_PER_ROOT: usize = 128;
const MAX_SKILL_ROOT_ENTRIES: usize = 1_024;
const MAX_SKILL_MANIFEST_BYTES: u64 = 256 * 1024;
const MAX_SKILL_NAME_CHARS: usize = 64;
const MAX_SKILL_DESCRIPTION_CHARS: usize = 1_024;
const DEFAULT_MAX_SUBAGENT_CONCURRENCY: usize = 4;
const DEFAULT_MAX_SUBAGENT_ITERATIONS: usize = 16;
const DEFAULT_MAX_SUBAGENT_INPUT_CHARS: usize = 16 * 1024;
const DEFAULT_MAX_SUBAGENT_OUTPUT_CHARS: usize = 64 * 1024;
const DEFAULT_SUBAGENT_TIMEOUT: Duration = Duration::from_secs(30);
const SUBAGENT_RESULT_POLL_INTERVAL: Duration = Duration::from_millis(1);

static SUBAGENT_PANIC_HOOK_INSTALLED: OnceLock<()> = OnceLock::new();

thread_local! {
    static IS_SUBAGENT_WORKER: Cell<bool> = const { Cell::new(false) };
}

/// Suppress only worker panic payloads because they can contain provider secrets.
fn install_subagent_panic_hook() {
    SUBAGENT_PANIC_HOOK_INSTALLED.get_or_init(|| {
        let default_hook = panic::take_hook();
        panic::set_hook(Box::new(move |panic_info| {
            if !IS_SUBAGENT_WORKER.with(Cell::get) {
                default_hook(panic_info);
            }
        }));
    });
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skill {
    name: String,
    description: String,
    body: String,
    source: PathBuf,
}

impl Skill {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn body(&self) -> &str {
        &self.body
    }

    pub fn source(&self) -> &Path {
        &self.source
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SkillCatalog {
    skills: Vec<Skill>,
    positions: BTreeMap<String, usize>,
}

impl SkillCatalog {
    pub fn discover(
        global_root: impl AsRef<Path>,
        project_root: impl AsRef<Path>,
    ) -> Result<SkillDiscovery, SkillDiscoveryError> {
        let global = load_skill_root(global_root.as_ref())?;
        let project = load_skill_root(project_root.as_ref())?;
        let mut catalog = Self::default();
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

    fn insert(&mut self, skill: Skill) {
        if let Some(position) = self.positions.get(skill.name()).copied() {
            self.skills[position] = skill;
            return;
        }

        self.positions.insert(skill.name.clone(), self.skills.len());
        self.skills.push(skill);
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
        Ok(directory) => directory,
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
    root_directory: &fs::File,
    directory_name: &std::ffi::OsStr,
) -> Result<Option<Skill>, SkillDiagnostic> {
    use std::os::unix::fs::MetadataExt;

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
    if !metadata.is_file() || metadata.nlink() > 1 {
        return Err(skill_diagnostic(
            &manifest,
            "manifest must be a single-link regular file".into(),
        ));
    }
    if metadata.len() > MAX_SKILL_MANIFEST_BYTES {
        return Err(skill_diagnostic(
            &manifest,
            format!("manifest exceeds {MAX_SKILL_MANIFEST_BYTES} byte limit"),
        ));
    }

    let contents = read_bounded_utf8(&mut manifest_descriptor)
        .map_err(|message| skill_diagnostic(&manifest, message))?;
    parse_skill_manifest(&manifest, &contents)
        .map(Some)
        .map_err(|message| skill_diagnostic(&manifest, message))
}

fn parse_skill_manifest(source: &Path, contents: &str) -> Result<Skill, String> {
    let (frontmatter, body) = split_skill_frontmatter(contents)?;
    let fields: SkillFrontmatter = serde_yaml::from_str(frontmatter)
        .map_err(|error| format!("invalid frontmatter: {error}"))?;
    let name = fields.name.trim().to_owned();
    let description = fields.description.trim().to_owned();
    validate_skill_name(&name)?;
    validate_skill_description(&description)?;
    let body = body.trim().to_owned();
    if body.is_empty() {
        return Err("markdown body is required".into());
    }

    Ok(Skill {
        name,
        description,
        body,
        source: source.to_path_buf(),
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
fn read_bounded_utf8(file: &mut fs::File) -> Result<String, String> {
    let mut bytes = Vec::new();
    file.take(MAX_SKILL_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read opened manifest: {error}"))?;
    if bytes.len() > MAX_SKILL_MANIFEST_BYTES as usize {
        return Err(format!(
            "manifest exceeds {MAX_SKILL_MANIFEST_BYTES} byte limit"
        ));
    }

    String::from_utf8(bytes).map_err(|error| format!("cannot read UTF-8 manifest: {error}"))
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
    max_iterations: usize,
    max_input_chars: usize,
    max_output_chars: usize,
    timeout: Duration,
}

impl SubagentLimits {
    pub fn new(
        max_concurrent: usize,
        max_iterations: usize,
        max_input_chars: usize,
        max_output_chars: usize,
        timeout: Duration,
    ) -> Result<Self, SubagentInputError> {
        if max_concurrent == 0
            || max_iterations == 0
            || max_input_chars == 0
            || max_output_chars == 0
            || timeout.is_zero()
        {
            return Err(SubagentInputError::InvalidLimits);
        }

        Ok(Self {
            max_concurrent,
            max_iterations,
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
            max_iterations: DEFAULT_MAX_SUBAGENT_ITERATIONS,
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
    iterations: usize,
}

impl SubagentTurnResult {
    pub fn new(output: impl Into<String>, iterations: usize) -> Self {
        Self {
            output: output.into(),
            iterations,
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
    fn new(cancellation: Arc<AtomicBool>, timeout: Duration) -> Self {
        Self {
            cancellation,
            deadline: Instant::now() + timeout,
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
        let Some(skill) = self.catalog.skill(&invocation.skill_name) else {
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
        if cancellation.load(Ordering::Acquire) {
            return ToolOutput::failure("subagent: cancelled");
        }

        let Some(permit) = SubagentPermit::acquire(&self.active, self.limits.max_concurrent) else {
            return ToolOutput::failure("subagent: concurrent child limit reached");
        };
        let context = SubagentRunContext::new(cancellation, self.limits.timeout);
        let request = SubagentTurnRequest {
            skill_name: skill.name.clone(),
            skill_description: skill.description.clone(),
            instructions: skill.body.clone(),
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
            Ok(result)
                if result.iterations <= self.limits.max_iterations
                    && result.output.chars().count() <= self.limits.max_output_chars =>
            {
                ToolOutput::success(result.output)
            }
            Ok(result) if result.iterations > self.limits.max_iterations => {
                ToolOutput::failure("subagent: iteration limit exceeded")
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

#[derive(Clone, Debug, PartialEq)]
pub enum McpContentBlock {
    Text(String),
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
    Protocol(String),
    Transport(String),
}

impl fmt::Display for McpTransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("mcp operation cancelled"),
            Self::TimedOut => formatter.write_str("mcp operation timed out"),
            Self::Protocol(message) => write!(formatter, "mcp protocol error: {message}"),
            Self::Transport(message) => write!(formatter, "mcp transport error: {message}"),
        }
    }
}

impl std::error::Error for McpTransportError {}

pub struct McpOperationContext {
    cancellation: Arc<AtomicBool>,
    deadline: Instant,
}

impl McpOperationContext {
    pub fn new(cancellation: Arc<AtomicBool>, timeout: Duration) -> Self {
        Self {
            cancellation,
            deadline: Instant::now() + timeout,
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancellation.load(Ordering::Acquire)
    }

    pub fn is_expired(&self) -> bool {
        Instant::now() >= self.deadline
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
        Ok(self.deadline.saturating_duration_since(Instant::now()))
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
}

impl McpRegistry {
    pub fn new() -> Self {
        Self::default()
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

    pub fn load_server<T: McpTransport>(
        &mut self,
        server_name: &str,
        transport: T,
        initialize: &McpInitialize,
        timeouts: McpTimeouts,
        limits: McpLimits,
        cancellation: Arc<AtomicBool>,
    ) -> McpServerReport {
        match load_server_metadata(
            server_name,
            transport,
            initialize.clone(),
            timeouts,
            limits,
            cancellation,
        ) {
            Ok(metadata)
                if !metadata
                    .iter()
                    .any(|tool| self.tools.contains_key(&tool.qualified_name))
                    && !has_duplicate_qualified_name(&metadata) =>
            {
                let tool_count = metadata.len();
                for tool in metadata {
                    self.tools.insert(tool.qualified_name.clone(), tool);
                }
                McpServerReport::loaded(server_name, tool_count)
            }
            Ok(_) => McpServerReport::Failed {
                server_name: server_name.into(),
                message: "mcp protocol error: duplicate qualified MCP tool name".into(),
            },
            Err(error) => McpServerReport::Failed {
                server_name: server_name.into(),
                message: error.to_string(),
            },
        }
    }

    pub fn load_servers<T: McpTransport + 'static>(
        &mut self,
        servers: impl IntoIterator<Item = (String, T)>,
        initialize: &McpInitialize,
        timeouts: McpTimeouts,
        limits: McpLimits,
        cancellation: Arc<AtomicBool>,
    ) -> Vec<McpServerReport> {
        let mut workers = servers
            .into_iter()
            .enumerate()
            .map(|(index, (name, transport))| {
                let initialize = initialize.clone();
                let cancellation = Arc::clone(&cancellation);
                thread::spawn(move || {
                    (
                        index,
                        name.clone(),
                        load_server_metadata(
                            &name,
                            transport,
                            initialize,
                            timeouts,
                            limits,
                            cancellation,
                        ),
                    )
                })
            })
            .collect::<Vec<_>>();
        let mut results = workers
            .drain(..)
            .map(|worker| {
                worker
                    .join()
                    .expect("cooperative MCP worker must not panic")
            })
            .collect::<Vec<_>>();
        results.sort_by_key(|(index, _, _)| *index);
        results
            .into_iter()
            .map(|(_, name, result)| match result {
                Ok(metadata)
                    if !metadata
                        .iter()
                        .any(|tool| self.tools.contains_key(&tool.qualified_name))
                        && !has_duplicate_qualified_name(&metadata) =>
                {
                    let tool_count = metadata.len();
                    for tool in metadata {
                        self.tools.insert(tool.qualified_name.clone(), tool);
                    }
                    McpServerReport::loaded(name, tool_count)
                }
                Ok(_) => McpServerReport::Failed {
                    server_name: name,
                    message: "mcp protocol error: duplicate qualified MCP tool name".into(),
                },
                Err(error) => McpServerReport::Failed {
                    server_name: name,
                    message: error.to_string(),
                },
            })
            .collect()
    }
}

fn load_server_metadata<T: McpTransport>(
    server_name: &str,
    transport: T,
    initialize: McpInitialize,
    timeouts: McpTimeouts,
    limits: McpLimits,
    cancellation: Arc<AtomicBool>,
) -> Result<Vec<RemoteToolMetadata>, McpTransportError> {
    validate_server_name(server_name)?;
    let mut client = McpClient::new(transport, timeouts, limits);
    let result = client
        .connect(initialize, &cancellation)
        .and_then(|_| client.list_tools(&cancellation))
        .and_then(|tools| {
            tools
                .into_iter()
                .map(|tool| remote_tool_metadata(server_name, tool))
                .collect()
        });
    client.close();
    result
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
        if initialized.protocol_version != initialize.protocol_version {
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
            McpResponse::ProtocolError(error) => Ok(ToolOutput::failure(format!(
                "mcp protocol error {}: {}",
                error.code, error.message
            ))),
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

    fn abort(
        &mut self,
        context: &McpOperationContext,
        primary: McpTransportError,
    ) -> Result<McpResponse, McpTransportError> {
        let _ = self.transport.close(context);
        Err(primary)
    }

    fn abort_notification(
        &mut self,
        context: &McpOperationContext,
        primary: McpTransportError,
    ) -> Result<(), McpTransportError> {
        let _ = self.transport.close(context);
        Err(primary)
    }
    fn close(&mut self) {
        let context =
            McpOperationContext::new(Arc::new(AtomicBool::new(false)), self.timeouts.connect);
        let _ = self.transport.close(&context);
    }
}

fn expect_initialized(response: McpResponse) -> Result<McpInitializeResult, McpTransportError> {
    match response {
        McpResponse::Initialized(result) => Ok(result),
        McpResponse::ProtocolError(error) => Err(protocol_error(error)),
        _ => Err(McpTransportError::Protocol(
            "expected initialize result".into(),
        )),
    }
}

fn protocol_error(error: McpProtocolError) -> McpTransportError {
    McpTransportError::Protocol(format!("{}: {}", error.code, error.message))
}

fn map_call_result(result: McpCallResult) -> ToolOutput {
    let content = result
        .content
        .into_iter()
        .map(|block| match block {
            McpContentBlock::Text(text) => text,
        })
        .collect::<Vec<_>>()
        .join("\n");

    if result.is_error {
        ToolOutput::failure(content)
    } else {
        ToolOutput::success(content)
    }
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

pub trait DispatchTool: Send {
    fn execute(&mut self, arguments: Value) -> Result<ToolOutput, Error>;
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolDispatchRequest {
    permission: PermissionRequest,
    arguments: Value,
}

impl ToolDispatchRequest {
    pub fn new(permission: PermissionRequest, arguments: Value) -> Self {
        Self {
            permission,
            arguments,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionPromptContext {
    pub project_id: String,
    pub qualified_tool_name: String,
    pub target_identifier: String,
    pub access: ToolAccess,
    pub reason: String,
}

impl PermissionPromptContext {
    fn from_request(request: &PermissionRequest) -> Self {
        Self {
            project_id: request.project.clone(),
            qualified_tool_name: request.tool.clone(),
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

struct RegisteredDispatchTool {
    access: ToolAccess,
    tool: Box<dyn DispatchTool>,
}

#[derive(Default)]
pub struct ToolDispatcher {
    native_tools: BTreeMap<String, RegisteredDispatchTool>,
    mcp_tools: BTreeMap<String, RegisteredDispatchTool>,
}

impl ToolDispatcher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_native(
        &mut self,
        name: impl Into<String>,
        access: ToolAccess,
        tool: impl DispatchTool + 'static,
    ) -> Result<(), Error> {
        let name = name.into();
        self.ensure_available_name(&name)?;
        Self::insert(&mut self.native_tools, name, access, tool);
        Ok(())
    }

    pub fn register_mcp(
        &mut self,
        metadata: &RemoteToolMetadata,
        tool: impl DispatchTool + 'static,
    ) -> Result<(), Error> {
        self.ensure_available_name(&metadata.qualified_name)?;
        Self::insert(
            &mut self.mcp_tools,
            metadata.qualified_name.clone(),
            remote_tool_access(metadata.access),
            tool,
        );
        Ok(())
    }

    pub fn dispatch(
        &mut self,
        policy: &PermissionPolicy,
        grants: &[ProjectPermissionGrant],
        session: &PermissionSession,
        request: ToolDispatchRequest,
    ) -> Result<ToolDispatchOutcome, Error> {
        let registered = self
            .native_tools
            .get_mut(&request.permission.tool)
            .or_else(|| self.mcp_tools.get_mut(&request.permission.tool))
            .ok_or_else(|| Error::Tool(format!("unknown tool: {}", request.permission.tool)))?;
        let mut permission = request.permission;
        permission.access = registered.access;
        let grants = if permission.project.trim().is_empty() {
            &[]
        } else {
            grants
        };

        match policy.evaluate(&permission, grants, session) {
            PermissionDecision::Deny => Ok(ToolDispatchOutcome::Denied),
            PermissionDecision::Ask => Ok(ToolDispatchOutcome::PromptRequired(
                PermissionPromptContext::from_request(&permission),
            )),
            PermissionDecision::Allow => registered
                .tool
                .execute(request.arguments)
                .map(ToolDispatchOutcome::Executed),
        }
    }

    fn ensure_available_name(&self, name: &str) -> Result<(), Error> {
        if name.is_empty()
            || self.native_tools.contains_key(name)
            || self.mcp_tools.contains_key(name)
        {
            return Err(Error::Tool("tool name must be unique and non-empty".into()));
        }

        Ok(())
    }

    fn insert(
        registry: &mut BTreeMap<String, RegisteredDispatchTool>,
        name: String,
        access: ToolAccess,
        tool: impl DispatchTool + 'static,
    ) {
        registry.insert(
            name,
            RegisteredDispatchTool {
                access,
                tool: Box::new(tool),
            },
        );
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
