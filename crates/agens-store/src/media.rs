//! Content-addressed media blobs under `{data_directory}/media/{sha256}` plus SQLite index rows.

use std::{
    fmt, fs,
    io::Write,
    path::{Path, PathBuf},
};

use rusqlite::{OptionalExtension, params};
use sha2::{Digest, Sha256};

use crate::database::{self, DatabaseError};

pub const MAX_MEDIA_BYTES: usize = 10 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaRecord {
    pub id: i64,
    pub sha256: String,
    pub mime: String,
    pub byte_len: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MediaStoreError {
    Oversize { byte_len: usize },
    NotFound { media_id: i64 },
    EmptyMime,
    Io { operation: String, detail: String },
    Database { operation: String, detail: String },
}

impl MediaStoreError {
    fn from_database(error: DatabaseError) -> Self {
        Self::Database {
            operation: error.operation().to_owned(),
            detail: error.detail().to_owned(),
        }
    }

    fn io(operation: impl Into<String>, detail: impl fmt::Display) -> Self {
        Self::Io {
            operation: operation.into(),
            detail: detail.to_string(),
        }
    }
}

impl fmt::Display for MediaStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Oversize { byte_len } => {
                write!(
                    formatter,
                    "media exceeds {MAX_MEDIA_BYTES} byte limit ({byte_len} bytes)"
                )
            }
            Self::NotFound { media_id } => write!(formatter, "media {media_id} not found"),
            Self::EmptyMime => formatter.write_str("media mime must be non-empty"),
            Self::Io { operation, detail } => write!(formatter, "media {operation}: {detail}"),
            Self::Database { operation, detail } => {
                write!(formatter, "media database {operation}: {detail}")
            }
        }
    }
}

impl std::error::Error for MediaStoreError {}

/// Ingests raw bytes into the durable media store.
///
/// Rejects payloads larger than [`MAX_MEDIA_BYTES`] before writing. Identical content hashes
/// reuse the existing row and blob path.
pub fn ingest_media_bytes(
    data_directory: &Path,
    bytes: &[u8],
    mime: &str,
) -> Result<MediaRecord, MediaStoreError> {
    if mime.is_empty() {
        return Err(MediaStoreError::EmptyMime);
    }

    if bytes.len() > MAX_MEDIA_BYTES {
        return Err(MediaStoreError::Oversize {
            byte_len: bytes.len(),
        });
    }

    if bytes.is_empty() {
        return Err(MediaStoreError::Io {
            operation: "ingest media".into(),
            detail: "media payload must be non-empty".into(),
        });
    }

    let sha256 = format!("{:x}", Sha256::digest(bytes));
    let media_directory = data_directory.join("media");
    fs::create_dir_all(&media_directory)
        .map_err(|error| MediaStoreError::io("create media directory", error))?;

    let blob_path = media_directory.join(&sha256);
    if !blob_path.exists() {
        let temporary_path = media_directory.join(format!(".{sha256}.tmp"));
        {
            let mut file = fs::File::create(&temporary_path)
                .map_err(|error| MediaStoreError::io("create media blob", error))?;
            file.write_all(bytes)
                .map_err(|error| MediaStoreError::io("write media blob", error))?;
            file.sync_all()
                .map_err(|error| MediaStoreError::io("sync media blob", error))?;
        }
        fs::rename(&temporary_path, &blob_path)
            .map_err(|error| MediaStoreError::io("finalize media blob", error))?;
    }

    let (_, connection) =
        database::open_unified_database(data_directory).map_err(MediaStoreError::from_database)?;

    if let Some(existing) = connection
        .query_row(
            "SELECT id, sha256, mime, byte_len FROM media WHERE sha256 = ?1",
            params![sha256],
            |row| {
                Ok(MediaRecord {
                    id: row.get(0)?,
                    sha256: row.get(1)?,
                    mime: row.get(2)?,
                    byte_len: row.get::<_, i64>(3)? as u64,
                })
            },
        )
        .optional()
        .map_err(|error| MediaStoreError::Database {
            operation: "lookup media by hash".into(),
            detail: error.to_string(),
        })?
    {
        return Ok(existing);
    }

    connection
        .execute(
            "INSERT INTO media (sha256, mime, byte_len, created_at)
             VALUES (?1, ?2, ?3, CAST(strftime('%s','now') AS INTEGER))",
            params![sha256, mime, bytes.len() as i64],
        )
        .map_err(|error| MediaStoreError::Database {
            operation: "insert media row".into(),
            detail: error.to_string(),
        })?;

    Ok(MediaRecord {
        id: connection.last_insert_rowid(),
        sha256,
        mime: mime.to_owned(),
        byte_len: bytes.len() as u64,
    })
}

/// Ingests a filesystem path by reading its bytes and delegating to [`ingest_media_bytes`].
pub fn ingest_media_path(
    data_directory: &Path,
    path: &Path,
    mime: &str,
) -> Result<MediaRecord, MediaStoreError> {
    let metadata =
        fs::metadata(path).map_err(|error| MediaStoreError::io("stat media path", error))?;
    if metadata.len() > MAX_MEDIA_BYTES as u64 {
        return Err(MediaStoreError::Oversize {
            byte_len: metadata.len() as usize,
        });
    }

    let bytes = fs::read(path).map_err(|error| MediaStoreError::io("read media path", error))?;
    ingest_media_bytes(data_directory, &bytes, mime)
}

/// Resolves a durable media id to its mime type and content-addressed blob path.
pub fn open_media(
    data_directory: &Path,
    media_id: i64,
) -> Result<(String, PathBuf), MediaStoreError> {
    let (_, connection) =
        database::open_unified_database(data_directory).map_err(MediaStoreError::from_database)?;

    let row = connection
        .query_row(
            "SELECT sha256, mime FROM media WHERE id = ?1",
            params![media_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(|error| MediaStoreError::Database {
            operation: "open media".into(),
            detail: error.to_string(),
        })?;

    let Some((sha256, mime)) = row else {
        return Err(MediaStoreError::NotFound { media_id });
    };

    let path = data_directory.join("media").join(sha256);
    if !path.is_file() {
        return Err(MediaStoreError::Io {
            operation: "open media blob".into(),
            detail: format!("missing blob for media {media_id}"),
        });
    }

    Ok((mime, path))
}

/// Returns true when `mime` is a durable media attachment type (image or PDF).
pub fn is_media_mime(mime: &str) -> bool {
    mime.starts_with("image/") || mime == "application/pdf"
}

/// Guesses a media MIME from a path extension.
///
/// Returns `None` for unknown extensions — callers treat those as non-media (e.g. UTF-8 `@` text).
pub fn guess_mime_from_path(path: &Path) -> Option<String> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    match extension.as_str() {
        "png" => Some("image/png".into()),
        "jpg" | "jpeg" => Some("image/jpeg".into()),
        "gif" => Some("image/gif".into()),
        "webp" => Some("image/webp".into()),
        "pdf" => Some("application/pdf".into()),
        _ => None,
    }
}

/// Guesses a media MIME from leading magic bytes.
pub fn guess_mime_from_bytes(bytes: &[u8]) -> Option<String> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n']) {
        return Some("image/png".into());
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("image/jpeg".into());
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif".into());
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp".into());
    }
    if bytes.starts_with(b"%PDF") {
        return Some("application/pdf".into());
    }
    None
}

pub use agens_core::media_chip_label;
