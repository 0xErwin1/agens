use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

use serde_yaml::Value;

pub const MAX_MARKDOWN_ROOT_ENTRIES: usize = 1_024;
pub const MAX_MARKDOWN_DEFINITIONS: usize = 128;
pub const MAX_MARKDOWN_FILE_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrontmatterValue {
    Scalar(String),
    List(Vec<String>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedMarkdown {
    frontmatter: BTreeMap<String, FrontmatterValue>,
    body: String,
}

impl ParsedMarkdown {
    pub fn field(&self, name: &str) -> Option<&FrontmatterValue> {
        self.frontmatter.get(name)
    }

    pub fn body(&self) -> &str {
        &self.body
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkdownDocument {
    name: String,
    source: PathBuf,
    parsed: ParsedMarkdown,
}

impl MarkdownDocument {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn source(&self) -> &Path {
        &self.source
    }
    pub fn parsed(&self) -> &ParsedMarkdown {
        &self.parsed
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkdownDiagnostic {
    path: PathBuf,
    message: String,
}

impl MarkdownDiagnostic {
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn message(&self) -> &str {
        &self.message
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MarkdownRoot {
    pub documents: Vec<MarkdownDocument>,
    pub diagnostics: Vec<MarkdownDiagnostic>,
}

pub fn canonical_filename(name: &str) -> Result<String, String> {
    validate_name(name)?;
    Ok(format!("{name}.md"))
}

pub fn parse(contents: &str) -> Result<ParsedMarkdown, String> {
    let (frontmatter, body) = split_frontmatter(contents)?;
    let values = serde_yaml::from_str::<Value>(frontmatter)
        .map_err(|error| format!("invalid frontmatter: {error}"))?;
    let Value::Mapping(values) = values else {
        return Err("frontmatter must be a mapping".into());
    };

    let mut fields = BTreeMap::new();
    for (key, value) in values {
        let Value::String(key) = key else {
            return Err("frontmatter keys must be strings".into());
        };
        let value = match value {
            Value::String(value) => FrontmatterValue::Scalar(value),
            Value::Sequence(values) => FrontmatterValue::List(
                values
                    .into_iter()
                    .map(yaml_string)
                    .collect::<Result<_, _>>()?,
            ),
            _ => {
                return Err(format!(
                    "frontmatter field {key} must be a string or string list"
                ));
            }
        };
        if fields.insert(key.clone(), value).is_some() {
            return Err(format!("duplicate frontmatter field {key}"));
        }
    }

    Ok(ParsedMarkdown {
        frontmatter: fields,
        body: body.to_owned(),
    })
}

pub fn load_root(root: &Path) -> Result<MarkdownRoot, String> {
    load_root_with_definition_limit(root, MAX_MARKDOWN_DEFINITIONS)
}

pub(crate) fn load_root_with_definition_limit(
    root: &Path,
    definition_limit: usize,
) -> Result<MarkdownRoot, String> {
    let root_metadata =
        fs::symlink_metadata(root).map_err(|error| format!("cannot inspect root: {error}"))?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err("markdown root must be a non-symbolic-link directory".into());
    }
    let root =
        fs::canonicalize(root).map_err(|error| format!("cannot canonicalize root: {error}"))?;
    let mut entries = fs::read_dir(&root)
        .map_err(|error| format!("cannot read root: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("cannot read root entry: {error}"))?;
    entries.sort_by_key(|entry| entry.file_name());

    let mut result = MarkdownRoot::default();
    if entries.len() > MAX_MARKDOWN_ROOT_ENTRIES {
        result
            .diagnostics
            .push(diagnostic(&root, "root entry limit exceeded"));
    }
    let mut definition_limit_reported = false;
    let definition_limit = definition_limit.min(MAX_MARKDOWN_ROOT_ENTRIES);
    for entry in entries.into_iter().take(MAX_MARKDOWN_ROOT_ENTRIES) {
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "md") {
            continue;
        }
        match load_file(&root, &path) {
            Ok(document) if result.documents.len() < definition_limit => {
                result.documents.push(document);
            }
            Ok(_) if !definition_limit_reported => {
                result
                    .diagnostics
                    .push(diagnostic(&root, "accepted definition limit exceeded"));
                definition_limit_reported = true;
            }
            Ok(_) => {}
            Err(message) => result.diagnostics.push(diagnostic(&path, message)),
        }
    }
    Ok(result)
}

fn load_file(root: &Path, path: &Path) -> Result<MarkdownDocument, String> {
    let source = canonical_regular_file(path)?;
    if !source.starts_with(root) {
        return Err("definition escapes its root".into());
    }
    let name = path
        .file_stem()
        .and_then(|name| name.to_str())
        .ok_or("filename must be UTF-8")?;
    if canonical_filename(name)?
        != path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or("filename must be UTF-8")?
    {
        return Err("definition filename must be canonical".into());
    }
    let contents = read_capped_utf8(&source)?;
    Ok(MarkdownDocument {
        name: name.into(),
        source,
        parsed: parse(&contents)?,
    })
}

/// Rejects symlinks and non-regular files, returning the canonicalized path
/// of an accepted regular file.
fn canonical_regular_file(path: &Path) -> Result<PathBuf, String> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| format!("cannot inspect file: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("definition must be a regular non-symbolic-link file".into());
    }
    fs::canonicalize(path).map_err(|error| format!("cannot canonicalize file: {error}"))
}

/// Reads `path` as UTF-8 text, rejecting content over `MAX_MARKDOWN_FILE_BYTES`.
fn read_capped_utf8(path: &Path) -> Result<String, String> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .map_err(|error| format!("cannot open file: {error}"))?
        .take(MAX_MARKDOWN_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read file: {error}"))?;
    if bytes.len() > MAX_MARKDOWN_FILE_BYTES {
        return Err("file exceeds byte limit".into());
    }
    String::from_utf8(bytes).map_err(|error| format!("file is not UTF-8: {error}"))
}

/// A single successfully read instruction file (e.g. a project or global
/// `AGENTS.md`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstructionFile {
    source: PathBuf,
    contents: String,
}

impl InstructionFile {
    pub fn source(&self) -> &Path {
        &self.source
    }

    pub fn contents(&self) -> &str {
        &self.contents
    }
}

/// Reads a single instruction file that may or may not exist.
///
/// Returns `Ok(None)` only when `path` does not exist. Every other rejection
/// (symlink, non-regular file, oversized content, or invalid UTF-8) is
/// returned as `Err`; deciding whether to skip a rejected file is the
/// caller's responsibility, not this reader's.
pub fn load_instruction_file(path: &Path) -> Result<Option<InstructionFile>, String> {
    if let Err(error) = fs::symlink_metadata(path) {
        if error.kind() == io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(format!("cannot inspect file: {error}"));
    }
    let source = canonical_regular_file(path)?;
    let contents = read_capped_utf8(&source)?;
    Ok(Some(InstructionFile { source, contents }))
}

fn split_frontmatter(contents: &str) -> Result<(&str, &str), String> {
    let Some(first_end) = contents.find('\n') else {
        return Err("frontmatter must begin with --- followed by a newline".into());
    };
    if contents[..first_end].trim_end_matches('\r') != "---" {
        return Err("frontmatter must begin with ---".into());
    }
    let start = first_end + 1;
    let mut offset = start;
    while offset < contents.len() {
        let end = contents[offset..]
            .find('\n')
            .map_or(contents.len(), |index| offset + index);
        if contents[offset..end].trim_end_matches('\r') == "---" {
            return Ok((
                &contents[start..offset],
                &contents[if end == contents.len() { end } else { end + 1 }..],
            ));
        }
        if end == contents.len() {
            break;
        }
        offset = end + 1;
    }
    Err("frontmatter closing --- is required".into())
}

fn yaml_string(value: Value) -> Result<String, String> {
    if let Value::String(value) = value {
        Ok(value)
    } else {
        Err("frontmatter lists must contain strings".into())
    }
}

fn validate_name(name: &str) -> Result<(), String> {
    let bytes = name.as_bytes();
    if bytes.is_empty()
        || bytes.len() > 64
        || !bytes[0].is_ascii_alphanumeric()
        || !bytes[bytes.len() - 1].is_ascii_alphanumeric()
        || name.contains("--")
        || bytes
            .iter()
            .any(|byte| !byte.is_ascii_lowercase() && !byte.is_ascii_digit() && *byte != b'-')
    {
        return Err(
            "name must use 1-64 lowercase ASCII letters, digits, and internal hyphens".into(),
        );
    }
    Ok(())
}

fn diagnostic(path: &Path, message: impl Into<String>) -> MarkdownDiagnostic {
    MarkdownDiagnostic {
        path: path.to_path_buf(),
        message: message.into(),
    }
}

#[cfg(test)]
mod instruction_file_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_CASE: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir() -> PathBuf {
        let suffix = NEXT_CASE.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agens-instruction-file-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    enum Setup {
        Missing,
        Symlink,
        Directory,
        Oversized,
        InvalidUtf8,
        Unreadable,
    }

    #[test]
    fn rejects_every_unsafe_or_invalid_candidate() {
        let cases = [
            ("missing file", Setup::Missing, true),
            ("symlink", Setup::Symlink, false),
            ("directory", Setup::Directory, false),
            ("oversized file", Setup::Oversized, false),
            ("invalid utf-8", Setup::InvalidUtf8, false),
            ("unreadable file", Setup::Unreadable, false),
        ];

        for (name, setup, expect_ok_none) in cases {
            let dir = temp_dir();
            let path = dir.join("AGENTS.md");

            match setup {
                Setup::Missing => {}
                Setup::Symlink => {
                    let target = dir.join("real.md");
                    fs::write(&target, "hello").unwrap();
                    std::os::unix::fs::symlink(&target, &path).unwrap();
                }
                Setup::Directory => {
                    fs::create_dir_all(&path).unwrap();
                }
                Setup::Oversized => {
                    fs::write(&path, "a".repeat(MAX_MARKDOWN_FILE_BYTES + 1)).unwrap();
                }
                Setup::InvalidUtf8 => {
                    fs::write(&path, [0xff, 0xfe, 0xfd]).unwrap();
                }
                Setup::Unreadable => {
                    use std::os::unix::fs::PermissionsExt;
                    fs::write(&path, "hello").unwrap();
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
                }
            }

            let result = load_instruction_file(&path);

            if matches!(setup, Setup::Unreadable) {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
            }

            match (result, expect_ok_none) {
                (Ok(None), true) => {}
                (Err(_), false) => {}
                (other, _) => panic!("case {name} produced unexpected result: {other:?}"),
            }
        }
    }

    #[test]
    fn accepts_a_valid_file_with_canonical_source_and_contents() {
        let dir = temp_dir();
        let path = dir.join("AGENTS.md");
        fs::write(&path, "hello world").unwrap();

        let file = load_instruction_file(&path).unwrap().unwrap();

        assert_eq!(file.contents(), "hello world");
        assert_eq!(file.source(), fs::canonicalize(&path).unwrap());
    }
}
