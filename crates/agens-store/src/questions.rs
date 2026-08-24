//! Open interactive questions shared by a running session and `agens direct`.
//!
//! The row contains only the closed answer domain and sanitized origin. Prompt
//! text, tool arguments, permission targets, and free-form explanations never
//! enter this store.

use std::fmt;
use std::path::{Path, PathBuf};

use agens_core::IntraTurnInputSource;
use rusqlite::{Connection, OptionalExtension, params};

use crate::database;
use crate::directives::DirectiveTarget;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuestionClass {
    AskUser,
    Permission,
    Consent,
}

impl QuestionClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AskUser => "ask_user",
            Self::Permission => "permission",
            Self::Consent => "consent",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenQuestion {
    pub target: DirectiveTarget,
    pub question_id: String,
    pub class: QuestionClass,
    pub origin: String,
    pub admissible_answers: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalQuestionAnswer {
    pub value: String,
    pub source: IntraTurnInputSource,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuestionStoreError {
    message: String,
}

impl QuestionStoreError {
    fn operation(operation: &str, path: &Path, error: impl fmt::Display) -> Self {
        Self {
            message: format!("questions {operation} at {}: {error}", path.display()),
        }
    }

    fn detail(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for QuestionStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for QuestionStoreError {}

pub struct QuestionStore {
    database_path: PathBuf,
    connection: Connection,
}

impl QuestionStore {
    pub fn open(data_directory: impl AsRef<Path>) -> Result<Self, QuestionStoreError> {
        let (database_path, connection) = database::open_unified_database(data_directory.as_ref())
            .map_err(|error| {
                QuestionStoreError::operation(error.operation(), error.path(), error.detail())
            })?;

        Ok(Self {
            database_path,
            connection,
        })
    }

    pub fn open_question(&mut self, question: &OpenQuestion) -> Result<(), QuestionStoreError> {
        if question.question_id.trim().is_empty()
            || question.origin.trim().is_empty()
            || question.admissible_answers.is_empty()
            || question
                .admissible_answers
                .iter()
                .any(|answer| answer.trim().is_empty())
        {
            return Err(QuestionStoreError::detail("an open question is incomplete"));
        }

        let (session_id, child) = target_columns(&question.target);
        let answers = serde_json::to_string(&question.admissible_answers)
            .map_err(|error| QuestionStoreError::operation("encode", &self.database_path, error))?;

        self.connection
            .execute(
                "INSERT OR REPLACE INTO open_questions
                    (session_id, child, question_id, class, origin, admissible_answers, opened_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    session_id,
                    child,
                    question.question_id,
                    question.class.as_str(),
                    question.origin,
                    answers,
                    timestamp(),
                ],
            )
            .map_err(|error| QuestionStoreError::operation("open", &self.database_path, error))?;

        Ok(())
    }

    pub fn answer(
        &mut self,
        target: &DirectiveTarget,
        question_id: &str,
        value: &str,
        source: IntraTurnInputSource,
    ) -> Result<(), QuestionStoreError> {
        let (session_id, child) = target_columns(target);
        let domain = self
            .connection
            .query_row(
                "SELECT admissible_answers FROM open_questions
                 WHERE question_id = ?1
                   AND session_id IS ?2 AND child IS ?3
                   AND answer IS NULL",
                params![question_id, session_id, child],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| QuestionStoreError::operation("read", &self.database_path, error))?
            .ok_or_else(|| QuestionStoreError::detail("no open question by that id"))?;
        let domain: Vec<String> = serde_json::from_str(&domain)
            .map_err(|error| QuestionStoreError::operation("decode", &self.database_path, error))?;

        if !domain.iter().any(|answer| answer == value) {
            return Err(QuestionStoreError::detail(format!(
                "`{value}` is not admissible for question {question_id}"
            )));
        }

        let changed = self
            .connection
            .execute(
                "UPDATE open_questions
                 SET answer = ?4, answered_by = ?5, answered_at = ?6
                 WHERE question_id = ?1
                   AND session_id IS ?2 AND child IS ?3
                   AND answer IS NULL",
                params![
                    question_id,
                    session_id,
                    child,
                    value,
                    source.as_str(),
                    timestamp(),
                ],
            )
            .map_err(|error| QuestionStoreError::operation("answer", &self.database_path, error))?;

        if changed == 0 {
            return Err(QuestionStoreError::detail("the question is no longer open"));
        }

        Ok(())
    }

    pub fn close(
        &mut self,
        target: &DirectiveTarget,
        question_id: &str,
        value: &str,
        source: IntraTurnInputSource,
    ) -> Result<(), QuestionStoreError> {
        let (session_id, child) = target_columns(target);
        self.connection
            .execute(
                "UPDATE open_questions
                 SET answer = COALESCE(answer, ?4),
                     answered_by = COALESCE(answered_by, ?5),
                     answered_at = COALESCE(answered_at, ?6),
                     delivered_at = COALESCE(delivered_at, ?6)
                 WHERE question_id = ?1
                   AND session_id IS ?2 AND child IS ?3",
                params![
                    question_id,
                    session_id,
                    child,
                    value,
                    source.as_str(),
                    timestamp(),
                ],
            )
            .map_err(|error| QuestionStoreError::operation("close", &self.database_path, error))?;
        Ok(())
    }

    pub fn take_answer(
        &mut self,
        target: &DirectiveTarget,
        question_id: &str,
    ) -> Result<Option<ExternalQuestionAnswer>, QuestionStoreError> {
        let (session_id, child) = target_columns(target);
        let transaction = self.connection.transaction().map_err(|error| {
            QuestionStoreError::operation("deliver", &self.database_path, error)
        })?;
        let answer = transaction
            .query_row(
                "SELECT answer, answered_by FROM open_questions
                 WHERE question_id = ?1
                   AND session_id IS ?2 AND child IS ?3
                   AND answer IS NOT NULL AND delivered_at IS NULL",
                params![question_id, session_id, child],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(|error| {
                QuestionStoreError::operation("deliver", &self.database_path, error)
            })?;

        let Some((value, answered_by)) = answer else {
            return Ok(None);
        };
        let source = match answered_by.as_str() {
            "human" => IntraTurnInputSource::Human,
            "supervisor" => IntraTurnInputSource::Supervisor,
            _ => return Err(QuestionStoreError::detail("unknown question answer source")),
        };

        transaction
            .execute(
                "UPDATE open_questions SET delivered_at = ?4
                 WHERE question_id = ?1
                   AND session_id IS ?2 AND child IS ?3",
                params![question_id, session_id, child, timestamp()],
            )
            .map_err(|error| {
                QuestionStoreError::operation("deliver", &self.database_path, error)
            })?;
        transaction.commit().map_err(|error| {
            QuestionStoreError::operation("deliver", &self.database_path, error)
        })?;

        Ok(Some(ExternalQuestionAnswer { value, source }))
    }
}

fn target_columns(target: &DirectiveTarget) -> (Option<i64>, Option<&str>) {
    match target {
        DirectiveTarget::Session(id) => (Some(*id), None),
        DirectiveTarget::Child(reference) => (None, Some(reference.as_str())),
    }
}

fn timestamp() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis().to_string())
        .unwrap_or_default()
}
