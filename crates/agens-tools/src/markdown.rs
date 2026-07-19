use std::{
    collections::BTreeMap,
    fs,
    io::Read,
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
    let metadata =
        fs::symlink_metadata(path).map_err(|error| format!("cannot inspect file: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("definition must be a regular non-symbolic-link file".into());
    }
    let source =
        fs::canonicalize(path).map_err(|error| format!("cannot canonicalize file: {error}"))?;
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
    let mut bytes = Vec::new();
    fs::File::open(&source)
        .map_err(|error| format!("cannot open file: {error}"))?
        .take(MAX_MARKDOWN_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read file: {error}"))?;
    if bytes.len() > MAX_MARKDOWN_FILE_BYTES {
        return Err("file exceeds byte limit".into());
    }
    let contents =
        String::from_utf8(bytes).map_err(|error| format!("file is not UTF-8: {error}"))?;
    Ok(MarkdownDocument {
        name: name.into(),
        source,
        parsed: parse(&contents)?,
    })
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
