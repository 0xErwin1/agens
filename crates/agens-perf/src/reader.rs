//! A line-oriented trace reader.
//!
//! `read_trace` never buffers the raw file; it parses one line at a time so a
//! multi-gigabyte trace can be read without holding it whole in memory. Every
//! failure names the file and the reason, because a comparison built on a
//! trace it silently misread would report a fiction.

use std::fmt;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use crate::schema::{Record, SCHEMA_VERSION};

#[derive(Debug)]
pub enum TraceReadError {
    Io {
        file: String,
        reason: String,
    },
    InvalidJson {
        file: String,
        line: usize,
        reason: String,
    },
    UnsupportedSchemaVersion {
        file: String,
        found: u32,
        expected: u32,
    },
}

impl fmt::Display for TraceReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { file, reason } => {
                write!(formatter, "{file}: {reason}")
            }
            Self::InvalidJson { file, line, reason } => {
                write!(formatter, "{file}:{line}: not valid JSON: {reason}")
            }
            Self::UnsupportedSchemaVersion {
                file,
                found,
                expected,
            } => {
                write!(
                    formatter,
                    "{file}: unsupported schema_version {found} (this reader supports {expected})"
                )
            }
        }
    }
}

impl std::error::Error for TraceReadError {}

/// Reads every record from a trace file, in file order.
///
/// The run-metadata record's `schema_version` is checked against
/// [`SCHEMA_VERSION`]; a mismatch is rejected rather than misread. Object keys
/// neither [`RunMetadata`](crate::RunMetadata) nor
/// [`SpanRecord`](crate::SpanRecord) declare are ignored by `serde` and never
/// reach this function as an error.
pub fn read_trace(path: impl AsRef<Path>) -> Result<Vec<Record>, TraceReadError> {
    let path = path.as_ref();
    let file_label = path.display().to_string();

    let file = File::open(path).map_err(|error| TraceReadError::Io {
        file: file_label.clone(),
        reason: error.to_string(),
    })?;

    let mut records = Vec::new();

    for (index, line) in BufReader::new(file).lines().enumerate() {
        let line = line.map_err(|error| TraceReadError::Io {
            file: file_label.clone(),
            reason: error.to_string(),
        })?;

        if line.trim().is_empty() {
            continue;
        }

        let record: Record =
            serde_json::from_str(&line).map_err(|error| TraceReadError::InvalidJson {
                file: file_label.clone(),
                line: index + 1,
                reason: error.to_string(),
            })?;

        if let Record::Run(run) = &record
            && run.schema_version != SCHEMA_VERSION
        {
            return Err(TraceReadError::UnsupportedSchemaVersion {
                file: file_label,
                found: run.schema_version,
                expected: SCHEMA_VERSION,
            });
        }

        records.push(record);
    }

    Ok(records)
}
