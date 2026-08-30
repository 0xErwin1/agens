//! Daemon adapters for hosted catalogs and confined workspace files.

use std::ffi::CString;
use std::fs::{self, File};
use std::io::Read;
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use agens_core::hosted::{
    CatalogKind, CatalogResult, CatalogSnapshot, FileError, HostedCatalogs, HostedWorkspaceFiles,
    MAX_WORKSPACE_FILE_BYTES, MAX_WORKSPACE_FILE_ENTRIES, WorkspaceFile, WorkspaceFileContent,
    WorkspaceFileKind,
};
use ignore::WalkBuilder;

#[derive(Clone, Debug, Default)]
pub struct HostedCatalogSet {
    command: Option<CatalogSnapshot>,
    skill: Option<CatalogSnapshot>,
}

impl HostedCatalogSet {
    #[must_use]
    pub fn new(command: Option<CatalogSnapshot>, skill: Option<CatalogSnapshot>) -> Self {
        Self { command, skill }
    }
}

impl HostedCatalogs for HostedCatalogSet {
    fn catalog(&self, kind: CatalogKind, known_revision: Option<&str>) -> CatalogResult {
        let snapshot = match kind {
            CatalogKind::Command => self.command.as_ref(),
            CatalogKind::Skill => self.skill.as_ref(),
        };
        snapshot.map_or(CatalogResult::Unsupported, |snapshot| {
            snapshot.resolve(known_revision)
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct ConfinedWorkspaceFiles {
    media_store: Option<PathBuf>,
}

impl ConfinedWorkspaceFiles {
    #[must_use]
    pub fn with_media_store(data_directory: PathBuf) -> Self {
        Self {
            media_store: Some(data_directory),
        }
    }
}

impl HostedWorkspaceFiles for ConfinedWorkspaceFiles {
    fn list(&self, root: &Path, selector: &Path) -> Result<Vec<WorkspaceFile>, FileError> {
        let root = canonical_root(root)?;
        let relative = valid_selector(selector)?;
        reject_symlink_components(&root, &relative)?;
        let selected = root.join(&relative);
        let selected = selected.canonicalize().map_err(map_missing)?;
        if !selected.starts_with(&root) {
            return Err(FileError::OutsideRoot);
        }
        if !selected.is_dir() {
            return Err(FileError::InvalidSelector);
        }

        let mut files = Vec::new();
        let mut builder = WalkBuilder::new(&selected);
        configure_walk(&mut builder);
        for entry in builder.build() {
            let entry = entry.map_err(|_| FileError::Unreadable)?;
            if entry.depth() == 0 {
                continue;
            }
            let Some(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() || !kind.is_file() {
                continue;
            }
            if files.len() == MAX_WORKSPACE_FILE_ENTRIES {
                return Err(FileError::EntryLimit);
            }
            let metadata = entry.metadata().map_err(|_| FileError::Unreadable)?;
            let path = entry
                .path()
                .strip_prefix(&root)
                .map_err(|_| FileError::OutsideRoot)?
                .to_path_buf();
            files.push(WorkspaceFile::new(
                path.clone(),
                metadata.len(),
                kind_for_path(&path),
            ));
        }
        Ok(files)
    }

    fn read(&self, root: &Path, selector: &Path) -> Result<WorkspaceFileContent, FileError> {
        let root = canonical_root(root)?;
        let relative = valid_selector(selector)?;
        reject_symlink_components(&root, &relative)?;
        let selected = root.join(&relative);
        let canonical = selected.canonicalize().map_err(map_missing)?;
        if !canonical.starts_with(&root) {
            return Err(FileError::OutsideRoot);
        }
        if !canonical.is_file() {
            return Err(FileError::Unsupported);
        }
        if !is_visible(&root, &canonical)? {
            return Err(FileError::Ignored);
        }

        let file = open_confined(&root, &relative)?;
        let metadata = file.metadata().map_err(|_| FileError::Unreadable)?;
        if metadata.len() > MAX_WORKSPACE_FILE_BYTES as u64 {
            return Err(FileError::Oversized);
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(MAX_WORKSPACE_FILE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| FileError::Unreadable)?;
        if bytes.len() > MAX_WORKSPACE_FILE_BYTES {
            return Err(FileError::Oversized);
        }

        let path = canonical
            .strip_prefix(&root)
            .map_err(|_| FileError::OutsideRoot)?
            .to_path_buf();
        if let Some(mime) = media_mime(&path) {
            let media_id = self
                .media_store
                .as_deref()
                .map(|data_directory| {
                    agens_store::ingest_media_bytes(data_directory, &bytes, mime)
                        .map(|record| record.id)
                        .map_err(|_| FileError::Unreadable)
                })
                .transpose()?;
            return Ok(WorkspaceFileContent::Media {
                path,
                mime: mime.to_owned(),
                bytes,
                media_id,
                kind: WorkspaceFileKind::Media,
            });
        }
        if bytes.contains(&0) {
            return Err(FileError::Unsupported);
        }
        let text = String::from_utf8(bytes).map_err(|_| FileError::Unsupported)?;
        Ok(WorkspaceFileContent::Text { path, text })
    }
}

fn canonical_root(root: &Path) -> Result<PathBuf, FileError> {
    if fs::symlink_metadata(root)
        .map_err(map_missing)?
        .file_type()
        .is_symlink()
    {
        return Err(FileError::OutsideRoot);
    }
    let root = root.canonicalize().map_err(map_missing)?;
    if root.is_dir() {
        Ok(root)
    } else {
        Err(FileError::InvalidSelector)
    }
}

fn valid_selector(selector: &Path) -> Result<PathBuf, FileError> {
    if selector.as_os_str().is_empty() || selector.is_absolute() {
        return Err(FileError::InvalidSelector);
    }
    if selector.to_string_lossy().split_whitespace().count() > 1 {
        return Err(FileError::InvalidSelector);
    }
    let mut clean = PathBuf::new();
    for component in selector.components() {
        match component {
            Component::Normal(part) => clean.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(FileError::OutsideRoot);
            }
        }
    }
    Ok(clean)
}

fn reject_symlink_components(root: &Path, selector: &Path) -> Result<(), FileError> {
    let mut current = root.to_path_buf();
    for component in selector.components() {
        if let Component::Normal(part) = component {
            current.push(part);
            match fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(FileError::OutsideRoot);
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Err(FileError::Missing);
                }
                Err(_) => return Err(FileError::Unreadable),
            }
        }
    }
    Ok(())
}

fn open_confined(root: &Path, selector: &Path) -> Result<File, FileError> {
    let root = CString::new(root.as_os_str().as_bytes()).map_err(|_| FileError::InvalidSelector)?;
    let descriptor = unsafe {
        libc::open(
            root.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        return Err(match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::ELOOP | libc::ENOTDIR) => FileError::OutsideRoot,
            Some(libc::ENOENT) => FileError::Missing,
            _ => FileError::Unreadable,
        });
    }
    let mut directory = unsafe { File::from_raw_fd(descriptor) };
    let components = selector.components().collect::<Vec<_>>();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            return Err(FileError::OutsideRoot);
        };
        let name = CString::new(name.as_bytes()).map_err(|_| FileError::InvalidSelector)?;
        let final_component = index + 1 == components.len();
        let flags = libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | if final_component {
                0
            } else {
                libc::O_DIRECTORY
            };
        // The returned descriptor is owned immediately by `File`; `directory`
        // stays live across `openat`, so every component is resolved beneath it.
        let descriptor = unsafe {
            libc::openat(
                std::os::fd::AsRawFd::as_raw_fd(&directory),
                name.as_ptr(),
                flags,
            )
        };
        if descriptor < 0 {
            let error = std::io::Error::last_os_error();
            return Err(match error.raw_os_error() {
                Some(libc::ELOOP | libc::ENOTDIR) => FileError::OutsideRoot,
                Some(libc::ENOENT) => FileError::Missing,
                _ => FileError::Unreadable,
            });
        }
        // SAFETY: `openat` returned a new owned descriptor and this is its only owner.
        directory = unsafe { File::from_raw_fd(descriptor) };
    }
    Ok(directory)
}

fn is_visible(root: &Path, selected: &Path) -> Result<bool, FileError> {
    let mut builder = WalkBuilder::new(root);
    configure_walk(&mut builder);
    for entry in builder.build() {
        let entry = entry.map_err(|_| FileError::Unreadable)?;
        if entry.path() == selected {
            return Ok(true);
        }
    }
    Ok(false)
}

fn configure_walk(builder: &mut WalkBuilder) {
    builder
        .hidden(true)
        .ignore(true)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .parents(true)
        .require_git(false)
        .follow_links(false)
        .sort_by_file_name(|left, right| left.cmp(right));
}

fn kind_for_path(path: &Path) -> WorkspaceFileKind {
    if media_mime(path).is_some() {
        WorkspaceFileKind::Media
    } else {
        WorkspaceFileKind::Text
    }
}

fn media_mime(path: &Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => Some("image/png"),
        Some("jpg" | "jpeg") => Some("image/jpeg"),
        Some("gif") => Some("image/gif"),
        Some("webp") => Some("image/webp"),
        Some("pdf") => Some("application/pdf"),
        Some("mp3") => Some("audio/mpeg"),
        Some("wav") => Some("audio/wav"),
        Some("mp4") => Some("video/mp4"),
        _ => None,
    }
}

fn map_missing(error: std::io::Error) -> FileError {
    if error.kind() == std::io::ErrorKind::NotFound {
        FileError::Missing
    } else {
        FileError::Unreadable
    }
}
