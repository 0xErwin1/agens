use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use agens_config::{AgentProfileEditError, AgentProfilePatch, apply_agent_profile_patch};

static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileScope {
    Global,
    Project,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileSnapshot(Option<Vec<u8>>);

#[derive(Debug)]
pub enum ProfileStoreError {
    ConcurrentModification,
    Edit(AgentProfileEditError),
    Io(io::Error),
}

impl fmt::Display for ProfileStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConcurrentModification => {
                formatter.write_str("profile config changed concurrently")
            }
            Self::Edit(error) => error.fmt(formatter),
            Self::Io(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ProfileStoreError {}

impl From<AgentProfileEditError> for ProfileStoreError {
    fn from(error: AgentProfileEditError) -> Self {
        Self::Edit(error)
    }
}

impl From<io::Error> for ProfileStoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub struct AgentProfileStore {
    global_config: PathBuf,
    project_config: PathBuf,
}

impl AgentProfileStore {
    pub fn new(global_config: PathBuf, project_config: PathBuf) -> Self {
        Self {
            global_config,
            project_config,
        }
    }

    pub fn read(&self, scope: ProfileScope) -> Result<ProfileSnapshot, ProfileStoreError> {
        Ok(ProfileSnapshot(read_optional(self.path_for(scope))?))
    }

    pub fn save(
        &self,
        scope: ProfileScope,
        snapshot: &ProfileSnapshot,
        agent: &str,
        patch: &AgentProfilePatch,
    ) -> Result<(), ProfileStoreError> {
        let path = self.path_for(scope);
        let original =
            String::from_utf8(snapshot.0.clone().unwrap_or_default()).map_err(|error| {
                ProfileStoreError::Io(io::Error::new(io::ErrorKind::InvalidData, error))
            })?;
        let replacement = apply_agent_profile_patch(&original, agent, patch)?;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .ok_or_else(|| {
                ProfileStoreError::Io(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "profile config path has no parent",
                ))
            })?;
        ensure_parent(parent)?;
        let (temporary_path, mut temporary) = create_temporary_file(parent, path, snapshot)?;

        let result = (|| {
            temporary.write_all(replacement.as_bytes())?;
            temporary.flush()?;
            temporary.sync_all()?;
            drop(temporary);

            if read_optional(path)? != snapshot.0 {
                return Err(ProfileStoreError::ConcurrentModification);
            }

            fs::rename(&temporary_path, path)?;
            let _ = File::open(parent).and_then(|directory| directory.sync_all());
            Ok(())
        })();

        if result.is_err() {
            let _ = fs::remove_file(&temporary_path);
        }
        result
    }

    fn path_for(&self, scope: ProfileScope) -> &Path {
        match scope {
            ProfileScope::Global => &self.global_config,
            ProfileScope::Project => &self.project_config,
        }
    }
}

fn read_optional(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn ensure_parent(path: &Path) -> io::Result<()> {
    if !path.exists() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
    }
    Ok(())
}

fn create_temporary_file(
    parent: &Path,
    target: &Path,
    snapshot: &ProfileSnapshot,
) -> Result<(PathBuf, File), ProfileStoreError> {
    for _ in 0..128 {
        let sequence = TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(".profile-{}-{sequence}.toml", std::process::id()));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path);
        match file {
            Ok(file) => {
                if snapshot.0.is_some() {
                    fs::set_permissions(&path, fs::metadata(target)?.permissions())?;
                }
                return Ok((path, file));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(ProfileStoreError::Io(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create a unique profile config temporary file",
    )))
}
