use std::{
    cell::Cell,
    collections::BTreeMap,
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

use agens_core::{
    Error, HeadlessTurnCancellationAdapter, PermissionDecision, PermissionPolicy,
    PermissionRequest, PermissionSession, ProjectPermissionGrant, ToolAccess,
};
use globset::{Glob, GlobSet, GlobSetBuilder};
use regex::RegexBuilder;
use serde::Deserialize;
use serde::de::{self, DeserializeSeed, Deserializer, IgnoredAny, MapAccess, Visitor};
use serde_json::Value;

mod http_mcp;
mod stdio_mcp;

pub use http_mcp::McpHttpTransport;
pub use stdio_mcp::{McpStdioTransport, McpStdioTransportConfig};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_PROCESS_OUTPUT: usize = 64 * 1024;
const MAX_PROCESS_OUTPUT_METADATA: usize = 128;
const MAX_CAPTURED_PROCESS_BYTES: usize = MAX_PROCESS_OUTPUT - MAX_PROCESS_OUTPUT_METADATA;
const DEFAULT_BASH_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_WEBFETCH_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_WEBFETCH_BYTES: usize = 100 * 1024;
const MAX_WEBFETCH_REDIRECTS: usize = 5;
const WEBFETCH_TRUNCATED_MARKER: &str = "\n[webfetch output truncated]";
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
    headless_cancellation: Option<HeadlessTurnCancellationAdapter>,
    deadline: Instant,
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
    deadline: Instant,
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
            deadline,
        }
    }

    /// Adapts core's opaque turn cancellation view without exposing its internals.
    pub fn from_headless_adapter(cancellation: HeadlessTurnCancellationAdapter) -> Self {
        let deadline = cancellation
            .deadline()
            .unwrap_or_else(|| Instant::now() + DEFAULT_BASH_TIMEOUT);
        Self {
            cancellation: None,
            headless_cancellation: Some(cancellation),
            deadline,
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
        Instant::now() >= self.deadline
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
        Ok(self.deadline.saturating_duration_since(Instant::now()))
    }

    fn mcp_context(&self) -> McpOperationContext {
        McpOperationContext {
            cancellation: self
                .cancellation
                .as_ref()
                .map(Arc::clone)
                .unwrap_or_else(|| Arc::new(AtomicBool::new(self.is_cancelled()))),
            headless_cancellation: self.headless_cancellation.clone(),
            deadline: self.deadline,
        }
    }
}

impl McpOperationContext {
    pub fn new(cancellation: Arc<AtomicBool>, timeout: Duration) -> Self {
        Self {
            cancellation,
            headless_cancellation: None,
            deadline: Instant::now() + timeout,
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancellation.load(Ordering::Acquire)
            || self
                .headless_cancellation
                .as_ref()
                .is_some_and(HeadlessTurnCancellationAdapter::is_cancelled)
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
    clients: BTreeMap<String, Box<dyn McpCallable>>,
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

    pub fn tools(&self) -> Vec<&RemoteToolMetadata> {
        self.tools.values().collect()
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
        match load_server_client(
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
                    return McpServerReport::Failed {
                        server_name: server_name.into(),
                        message: "mcp server load failed".into(),
                    };
                }

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
            Err(error) => McpServerReport::Failed {
                server_name: server_name.into(),
                message: sanitized_mcp_load_error(&error).into(),
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
        let metadata = self
            .tools
            .get(qualified_name)
            .ok_or_else(|| Error::Tool("unknown MCP tool".into()))?;
        let client = self
            .clients
            .get_mut(&metadata.server_name)
            .ok_or_else(|| Error::Tool("unavailable MCP tool".into()))?;
        client.call(&metadata.tool_name, arguments, context)
    }
}

trait McpCallable: Send {
    fn call(
        &mut self,
        tool_name: &str,
        arguments: Value,
        context: &ToolExecutionContext,
    ) -> Result<ToolOutput, Error>;

    fn close(&mut self);
}

impl<T: McpTransport> McpCallable for McpClient<T> {
    fn call(
        &mut self,
        tool_name: &str,
        arguments: Value,
        context: &ToolExecutionContext,
    ) -> Result<ToolOutput, Error> {
        self.call_tool_with_context(tool_name, arguments, &context.mcp_context())
            .map(sanitize_tool_output)
            .map_err(mcp_call_error)
    }

    fn close(&mut self) {
        Self::close(self);
    }
}

fn mcp_call_error(error: McpTransportError) -> Error {
    match error {
        McpTransportError::Cancelled => Error::Cancelled,
        McpTransportError::TimedOut => Error::Tool("mcp operation timed out".into()),
        McpTransportError::Protocol(_) | McpTransportError::Transport(_) => {
            Error::Extension("mcp tool infrastructure failure".into())
        }
    }
}

fn sanitized_mcp_load_error(_: &McpTransportError) -> &'static str {
    "mcp server load failed"
}

fn load_server_client<T: McpTransport>(
    server_name: &str,
    transport: T,
    initialize: McpInitialize,
    timeouts: McpTimeouts,
    limits: McpLimits,
    cancellation: Arc<AtomicBool>,
) -> Result<(Vec<RemoteToolMetadata>, McpClient<T>), McpTransportError> {
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
    match result {
        Ok(metadata) => Ok((metadata, client)),
        Err(error) => {
            client.close();
            Err(error)
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
            cancellation: Arc::clone(&context.cancellation),
            headless_cancellation: context.headless_cancellation.clone(),
            deadline: context.deadline.min(Instant::now() + self.timeouts.call),
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

fn terminal_mcp_error(error: &Error) -> bool {
    match error {
        Error::Cancelled => true,
        Error::Tool(message) => message == "mcp operation timed out",
        Error::Extension(message) => message == "mcp tool infrastructure failure",
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
    /// Projects the exact execution arguments into the permission target.
    fn permission_target(&self, arguments: &Value) -> Result<String, Error> {
        arguments
            .get("target")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| Error::Tool("tool target is required".into()))
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

#[derive(Debug)]
pub enum ToolEvaluationOutcome {
    Denied,
    PromptRequired(PermissionPromptContext),
    Authorized(AuthorizedToolCall),
}

/// Opaque proof that a specific registered tool was authorized for one request.
/// Its fields are deliberately private: callers cannot construct or alter a call.
#[derive(Debug)]
pub struct AuthorizedToolCall {
    dispatcher_id: u64,
    registration_version: u64,
    qualified_tool_name: String,
    projected_target: String,
    access: ToolAccess,
    arguments: Value,
    arguments_digest: u64,
}

struct RegisteredDispatchTool {
    access: ToolAccess,
    version: u64,
    tool: Box<dyn DispatchTool>,
}

pub struct ToolDispatcher {
    native_tools: BTreeMap<String, RegisteredDispatchTool>,
    mcp_tools: BTreeMap<String, RegisteredDispatchTool>,
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
            native_tools: BTreeMap::new(),
            mcp_tools: BTreeMap::new(),
        }
    }

    pub fn register_native(
        &mut self,
        name: impl Into<String>,
        access: ToolAccess,
        tool: impl DispatchTool + 'static,
    ) -> Result<(), Error> {
        let name = name.into();
        self.ensure_available_name(&name)?;
        let version = self.allocate_version();
        Self::insert(&mut self.native_tools, name, access, version, tool);
        Ok(())
    }

    pub fn register_mcp(
        &mut self,
        metadata: &RemoteToolMetadata,
        tool: impl DispatchTool + 'static,
    ) -> Result<(), Error> {
        self.ensure_available_name(&metadata.qualified_name)?;
        let version = self.allocate_version();
        Self::insert(
            &mut self.mcp_tools,
            metadata.qualified_name.clone(),
            remote_tool_access(metadata.access),
            version,
            tool,
        );
        Ok(())
    }

    /// Replaces an existing native implementation while invalidating prior authorizations.
    pub fn replace_native(
        &mut self,
        name: impl Into<String>,
        access: ToolAccess,
        tool: impl DispatchTool + 'static,
    ) {
        let name = name.into();
        let version = self.allocate_version();
        self.native_tools.insert(
            name,
            RegisteredDispatchTool {
                access,
                version,
                tool: Box::new(tool),
            },
        );
    }

    pub fn evaluate(
        &self,
        policy: &PermissionPolicy,
        grants: &[ProjectPermissionGrant],
        session: &PermissionSession,
        request: ToolDispatchRequest,
    ) -> Result<ToolEvaluationOutcome, Error> {
        let registered = self
            .native_tools
            .get(&request.qualified_tool_name)
            .or_else(|| self.mcp_tools.get(&request.qualified_tool_name))
            .ok_or_else(|| Error::Tool("unknown tool".into()))?;
        let permission = PermissionRequest::new(
            request.project_id,
            request.qualified_tool_name,
            registered.tool.permission_target(&request.arguments)?,
            registered.access,
        );
        let grants = if permission.project.trim().is_empty() {
            &[]
        } else {
            grants
        };

        match policy.evaluate(&permission, grants, session) {
            PermissionDecision::Deny => Ok(ToolEvaluationOutcome::Denied),
            PermissionDecision::Ask => Ok(ToolEvaluationOutcome::PromptRequired(
                PermissionPromptContext::from_request(&permission),
            )),
            PermissionDecision::Allow => {
                Ok(ToolEvaluationOutcome::Authorized(AuthorizedToolCall {
                    dispatcher_id: self.dispatcher_id,
                    registration_version: registered.version,
                    qualified_tool_name: permission.tool,
                    projected_target: permission.target,
                    access: permission.access,
                    arguments_digest: digest_arguments(&request.arguments),
                    arguments: request.arguments,
                }))
            }
        }
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
            .native_tools
            .get_mut(&handle.qualified_tool_name)
            .or_else(|| self.mcp_tools.get_mut(&handle.qualified_tool_name))
            .ok_or_else(|| Error::Tool("stale authorized tool call".into()))?;
        if registered.version != handle.registration_version
            || registered.access != handle.access
            || digest_arguments(&handle.arguments) != handle.arguments_digest
            || handle.projected_target.is_empty() && handle.access == ToolAccess::Write
        {
            return Err(Error::Tool("stale authorized tool call".into()));
        }

        match registered.tool.execute(context, handle.arguments) {
            Ok(output) => {
                if let Err(status) = context.check() {
                    return Ok(sanitized_execution_status(status));
                }
                Ok(sanitize_tool_output(output))
            }
            Err(error) if terminal_mcp_error(&error) => Err(error),
            Err(_) => Ok(ToolOutput::failure("tool infrastructure failure")),
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
        version: u64,
        tool: impl DispatchTool + 'static,
    ) {
        registry.insert(
            name,
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

fn sanitize_tool_output(mut output: ToolOutput) -> ToolOutput {
    const MAX_MODEL_VISIBLE_OUTPUT: usize = 16 * 1024;
    const TRUNCATION_NOTICE: &str = "\n[output truncated]";
    if output.content.len() > MAX_MODEL_VISIBLE_OUTPUT {
        let mut boundary = MAX_MODEL_VISIBLE_OUTPUT.saturating_sub(TRUNCATION_NOTICE.len());
        while !output.content.is_char_boundary(boundary) {
            boundary -= 1;
        }
        output.content.truncate(boundary);
        output.content.push_str(TRUNCATION_NOTICE);
    }
    output
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

#[derive(Debug)]
pub struct NativeTools {
    project_root: PathBuf,
    limits: NativeToolLimits,
    webfetch: Mutex<WebfetchState>,
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
            webfetch: Mutex::new(WebfetchState::default()),
        })
    }

    pub fn read_file(&self, input: ReadFileInput) -> Result<ToolOutput, Error> {
        if let Err(output) = self.validate_relative(&input.path) {
            return Ok(output);
        }
        if input
            .range
            .is_some_and(|(offset, limit)| offset == 0 || limit == 0)
        {
            return Ok(ToolOutput::failure(
                "read: offset and limit must be greater than zero",
            ));
        }

        #[cfg(unix)]
        let result = read_file_confined(&self.project_root_dir, &input);

        #[cfg(not(unix))]
        let result = Err(ToolOutput::failure(
            "read: secure confined reads are unavailable on this platform",
        ));

        Ok(result.unwrap_or_else(|output| output))
    }

    pub fn write_file(&self, input: WriteFileInput) -> Result<ToolOutput, Error> {
        self.write_file_with_context(input, None)
    }

    pub fn edit_file(&self, input: EditFileInput) -> Result<ToolOutput, Error> {
        self.edit_file_with_context(input, None)
    }

    fn write_file_with_context(
        &self,
        input: WriteFileInput,
        context: Option<&ToolExecutionContext>,
    ) -> Result<ToolOutput, Error> {
        if let Err(output) = self.validate_relative(&input.path) {
            return Ok(output);
        }

        #[cfg(unix)]
        let result = write_file_confined(
            &self.project_root_dir,
            &input.path,
            input.content.as_bytes(),
            context,
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

    fn edit_file_with_context(
        &self,
        input: EditFileInput,
        context: Option<&ToolExecutionContext>,
    ) -> Result<ToolOutput, Error> {
        if let Err(output) = self.validate_relative(&input.path) {
            return Ok(output);
        }
        if input.old.is_empty() {
            return Ok(ToolOutput::failure("edit: old text is required"));
        }
        if input.old == input.new {
            return Ok(ToolOutput::failure("edit: old and new text must differ"));
        }

        #[cfg(unix)]
        let result = edit_file_confined(
            &self.project_root_dir,
            &input.path,
            &input.old,
            &input.new,
            context,
        );

        #[cfg(not(unix))]
        let result = Err(ToolOutput::failure(
            "edit: secure confined edits are unavailable on this platform",
        ));

        Ok(result.unwrap_or_else(|output| output))
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
        let mut budget = SearchBudget::new(&self.limits, "search");
        if let Err(output) =
            self.search_directory(&path, &input.query, 0, &mut budget, &mut results)
        {
            return Ok(output);
        }

        Ok(ToolOutput::success(results.join("")))
    }

    pub fn grep(&self, input: GrepInput) -> Result<ToolOutput, Error> {
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
        let directory = match input.path {
            Some(path) => match self.resolve_existing(&path) {
                Ok(path) => path,
                Err(output) => return Ok(output),
            },
            None => self.project_root.clone(),
        };
        if !directory.is_dir() {
            return Ok(ToolOutput::failure("grep: path is not a directory"));
        }

        let mut files = Vec::new();
        let mut budget = SearchBudget::new(&self.limits, "grep");
        if let Err(output) = self.collect_tool_files(&directory, 0, &mut budget, &mut files) {
            return Ok(output);
        }

        let mut results = Vec::new();
        for path in files {
            if let Err(output) = budget.check_deadline() {
                return Ok(output);
            }
            let relative = path
                .strip_prefix(&self.project_root)
                .map_err(|_| Error::Tool("path: outside project root".into()))?;
            if file_glob
                .as_ref()
                .is_some_and(|glob| !glob.is_match(relative))
            {
                continue;
            }
            if fs::metadata(&path).is_ok_and(|metadata| metadata.len() > MAX_FILE_BYTES) {
                continue;
            }
            let content = match fs::read(&path) {
                Ok(content) if !content.contains(&0) => content,
                Ok(_) => continue,
                Err(error) => return Ok(ToolOutput::failure(format!("grep: {error}"))),
            };
            let content = match std::str::from_utf8(&content) {
                Ok(content) => content,
                Err(_) => continue,
            };
            for (line, text) in content.lines().enumerate() {
                if let Err(output) = budget.check_deadline() {
                    return Ok(output);
                }
                if regex.is_match(text) {
                    if results.len() == self.limits.max_search_results {
                        results.push(format!(
                            "[grep output truncated after {} results]\n",
                            self.limits.max_search_results
                        ));
                        return Ok(ToolOutput::success(results.join("")));
                    }
                    results.push(format!("{}:{}:{text}\n", relative.display(), line + 1));
                }
            }
        }

        Ok(ToolOutput::success(results.join("")))
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
        if let Err(output) = self.collect_tool_files(&self.project_root, 0, &mut budget, &mut files)
        {
            return Ok(output);
        }

        let mut matches = files
            .into_iter()
            .filter_map(|path| {
                path.strip_prefix(&self.project_root)
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
        let stdout_reader = read_capped(stdout, ProcessStream::Stdout, Arc::clone(&output));
        let stderr_reader = read_capped(stderr, ProcessStream::Stderr, Arc::clone(&output));
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
                if terminate_process_group(&mut child).is_err()
                    || wait_for_readers(stdout_reader, stderr_reader).is_err()
                {
                    return Ok(ToolOutput::failure("bash: process cleanup failed"));
                }
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
                if terminate_process_group(&mut child).is_err()
                    || wait_for_readers(stdout_reader, stderr_reader).is_err()
                {
                    return Ok(ToolOutput::failure("bash: process cleanup failed"));
                }
                return Ok(render_bash_result(
                    &output,
                    "unavailable",
                    Some(&format!(
                        "bash: timed out after {}ms",
                        input.timeout.as_millis()
                    )),
                ));
            }

            if let Some(status) = child
                .try_wait()
                .map_err(|_| Error::Tool("bash: wait failed".into()))?
            {
                if kill_process_group(child.id()).is_err()
                    || wait_for_readers(stdout_reader, stderr_reader).is_err()
                {
                    return Ok(ToolOutput::failure("bash: process cleanup failed"));
                }
                break status;
            }

            thread::sleep(PROCESS_POLL_INTERVAL);
        };

        if status.success() {
            return Ok(render_bash_result(&output, &exit_code(status), None));
        }

        Ok(render_bash_result(&output, &exit_code(status), None))
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

    fn collect_tool_files(
        &self,
        directory: &Path,
        depth: usize,
        budget: &mut SearchBudget,
        files: &mut Vec<PathBuf>,
    ) -> Result<(), ToolOutput> {
        let directory_entries = fs::read_dir(directory)
            .map_err(|error| ToolOutput::failure(format!("{}: {error}", budget.tool)))?;
        let mut entries = Vec::new();
        for entry in directory_entries {
            budget.consume_entry()?;
            let entry =
                entry.map_err(|error| ToolOutput::failure(format!("{}: {error}", budget.tool)))?;
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            entries.push(entry);
        }
        entries.sort_by_key(|entry| entry.file_name());

        for entry in entries {
            budget.check_deadline()?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)
                .map_err(|error| ToolOutput::failure(format!("{}: {error}", budget.tool)))?;
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                let next_depth = depth + 1;
                if next_depth > self.limits.max_search_depth {
                    return Err(ToolOutput::failure(format!(
                        "{}: traversal depth limit of {} exceeded",
                        budget.tool, self.limits.max_search_depth
                    )));
                }
                self.collect_tool_files(&path, next_depth, budget, files)?;
            } else if metadata.is_file() {
                files.push(path);
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
                "Run a bounded shell command in the project root",
                ToolAccess::Write,
                serde_json::json!({"type":"object","additionalProperties":false,"required":["command"],"properties":{"command":{"type":"string"},"timeout_ms":{"type":"integer","minimum":1}}}),
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
        &self,
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
                if let (Some(offset), Some(limit)) = (
                    arguments.get("offset").and_then(Value::as_u64),
                    arguments.get("limit").and_then(Value::as_u64),
                ) {
                    input = input.with_range(offset as usize, limit as usize);
                } else if arguments.contains_key("offset") || arguments.contains_key("limit") {
                    return Err(Error::Tool("native tool arguments are invalid".into()));
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
            "native::search" => self
                .tools
                .search(SearchInput::new(string("path")?, string("query")?))?,
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
                self.tools.grep(input)?
            }
            "native::glob" => self.tools.glob(GlobInput::new(string("pattern")?))?,
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
                        Some(timeout) => Duration::from_millis(timeout),
                        None => return Ok(ToolOutput::failure("bash: timeout must be an integer")),
                    },
                    None => DEFAULT_BASH_TIMEOUT,
                };
                self.tools.bash(
                    BashInput::new(command)
                        .with_timeout(timeout.min(context.remaining().unwrap_or_default()))
                        .with_execution_context(context.clone()),
                )?
            }
            _ => return Err(Error::Tool("unknown native tool".into())),
        };
        if let Err(status) = context.check() {
            return Ok(sanitized_execution_status(status));
        }
        Ok(sanitize_tool_output(output))
    }
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
        || limits.operation_timeout.is_zero()
    {
        return Err(Error::Tool(
            "native tool limits must be greater than zero".into(),
        ));
    }

    Ok(())
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

#[cfg(unix)]
fn write_file_confined(
    project_root: &fs::File,
    path: &Path,
    content: &[u8],
    context: Option<&ToolExecutionContext>,
) -> Result<(), ToolOutput> {
    use std::{
        ffi::CString,
        os::fd::{AsRawFd, FromRawFd},
    };

    let (directory, file_name) = open_confined_parent(project_root, path, true, "write")?;
    let existing = match open_confined_file(&directory, &file_name, "write") {
        Ok(file) => Some(file_identity(&checked_regular_file(&file, "write")?)),
        Err(output) if output.content.contains("No such file") => None,
        Err(output) => return Err(output),
    };
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
    result
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
    Ok(ToolOutput::success(diff))
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
fn unified_edit_diff(
    path: &Path,
    original: &str,
    replacement: &str,
    old: &str,
    new: &str,
    match_offset: usize,
) -> String {
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
    diff
}

#[cfg(all(test, unix))]
mod native_tool_tests {
    use super::*;
    use std::{
        os::unix::fs::symlink,
        sync::atomic::{AtomicUsize, Ordering},
    };

    static NEXT_ROOT: AtomicUsize = AtomicUsize::new(0);

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
    fn exact_edit_rejects_deterministic_races_and_cleans_up() {
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
    use std::os::fd::{AsRawFd, FromRawFd};
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            file_name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    if descriptor < 0 {
        return Err(confined_open_error(operation, io::Error::last_os_error()));
    }
    Ok(unsafe { fs::File::from_raw_fd(descriptor) })
}

#[cfg(unix)]
fn confined_open_error(operation: &str, error: io::Error) -> ToolOutput {
    if error.raw_os_error() == Some(libc::ELOOP) || error.kind() == io::ErrorKind::NotADirectory {
        return ToolOutput::failure("path: outside project root");
    }
    ToolOutput::failure(format!("{operation}: {error}"))
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
    match (existing, open_confined_file(directory, file_name, "write")) {
        (Some(expected), Ok(file))
            if file_identity(&checked_regular_file(&file, "write")?) == expected =>
        {
            Ok(())
        }
        (Some(_), Ok(_)) => Err(ToolOutput::failure("write: target changed during write")),
        (Some(_), Err(output)) => Err(output),
        (None, Err(output)) if output.content.contains("No such file") => Ok(()),
        (None, _) => Err(ToolOutput::failure("write: target changed during write")),
    }
}

#[derive(Default)]
struct CappedOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    truncated: bool,
}

#[derive(Clone, Copy)]
enum ProcessStream {
    Stdout,
    Stderr,
}

impl CappedOutput {
    fn append(&mut self, stream: ProcessStream, bytes: &[u8]) {
        let remaining =
            MAX_CAPTURED_PROCESS_BYTES.saturating_sub(self.stdout.len() + self.stderr.len());
        let target = match stream {
            ProcessStream::Stdout => &mut self.stdout,
            ProcessStream::Stderr => &mut self.stderr,
        };
        target.extend_from_slice(&bytes[..bytes.len().min(remaining)]);
        self.truncated |= bytes.len() > remaining;
    }

    fn render(&self, exit_status: &str, detail: Option<&str>) -> String {
        let mut output = String::from("[stdout]\n");
        append_bash_stream(&mut output, &self.stdout);
        output.push_str("[stderr]\n");
        append_bash_stream(&mut output, &self.stderr);
        if self.truncated {
            output.push_str("[bash output truncated]\n");
        }
        if let Some(detail) = detail {
            output.push_str(&format!("[{detail}]\n"));
        }
        output.push_str(&format!("[exit status: {exit_status}]\n"));
        output.truncate(output.floor_char_boundary(MAX_PROCESS_OUTPUT));
        output
    }
}

fn append_bash_stream(output: &mut String, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }

    output.push_str(&String::from_utf8_lossy(bytes));
    if !output.ends_with('\n') {
        output.push('\n');
    }
}

fn render_bash_result(
    output: &Arc<Mutex<CappedOutput>>,
    exit_status: &str,
    detail: Option<&str>,
) -> ToolOutput {
    let output = output
        .lock()
        .map(|output| output.render(exit_status, detail))
        .unwrap_or_else(|_| {
            "[stdout]\n[stderr]\n[bash: output collection failed]\n[exit status: unavailable]\n"
                .into()
        });
    if detail.is_some() || exit_status != "0" {
        ToolOutput::failure(output)
    } else {
        ToolOutput::success(output)
    }
}

fn read_capped(
    mut reader: impl Read + Send + 'static,
    stream: ProcessStream,
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
            output.append(stream, &buffer[..count]);
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
