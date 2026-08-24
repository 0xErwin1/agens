//! Durable permission questions for turns that have no prompt surface.
//!
//! The row is the handoff: a direct caller answers the same `DirectiveTarget`
//! this prompter was built for, and the prompter polls only that row until the
//! configured budget ends.

use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use agens_bootstrap::Bootstrap;
use agens_core::{HeadlessTurnCancellation, HeadlessTurnPortError, IntraTurnInputSource};
use agens_diagnostics::{SessionLifecycle, next_diagnostic_reference, record_session_lifecycle};
use agens_permissions::{PermissionPromptAnswer, PermissionPrompter};
use agens_providers::ProviderDiagnosticScope;
use agens_store::{DirectiveTarget, OpenQuestion, QuestionClass, QuestionStore};
use agens_tools::PermissionPromptContext;

const QUESTION_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Observes the durable question lifecycle without coupling the permission port
/// to a particular reporting surface.
pub trait PermissionQuestionObserver: Send {
    fn opened(&mut self, question_id: &str, context: &PermissionPromptContext);
    fn closed(&mut self, question_id: &str, selected_answer: &str, answered_by: &str);
}

/// A permission prompter that opens one durable question and waits for an
/// external direct answer addressed to its target.
pub struct QuestionPrompter {
    data_directory: PathBuf,
    target: DirectiveTarget,
    wait: Duration,
    observer: Box<dyn PermissionQuestionObserver>,
}

impl QuestionPrompter {
    pub fn new(
        data_directory: impl AsRef<Path>,
        target: DirectiveTarget,
        wait: Duration,
        observer: Box<dyn PermissionQuestionObserver>,
    ) -> Self {
        Self {
            data_directory: data_directory.as_ref().to_path_buf(),
            target,
            wait,
            observer,
        }
    }

    fn close_unanswered(
        &mut self,
        question_id: &str,
        outcome: &str,
    ) -> Result<(), HeadlessTurnPortError> {
        QuestionStore::open(&self.data_directory)
            .and_then(|mut store| store.close_unanswered(&self.target, question_id, outcome))
            .map_err(|_| HeadlessTurnPortError::Permission)?;
        self.observer.closed(question_id, outcome, "system");
        Ok(())
    }
}

impl PermissionPrompter for QuestionPrompter {
    fn prompt(
        &mut self,
        context: &PermissionPromptContext,
        cancellation: &HeadlessTurnCancellation,
    ) -> Result<PermissionPromptAnswer, HeadlessTurnPortError> {
        let question_id = next_diagnostic_reference();
        let question = OpenQuestion {
            target: self.target.clone(),
            question_id: question_id.clone(),
            class: QuestionClass::Permission,
            origin: agens_core::bare_tool_name(&context.tool_identity).into_owned(),
            admissible_answers: vec![
                "allow_once".into(),
                "allow_always".into(),
                "deny_once".into(),
                "deny_always".into(),
            ],
        };
        QuestionStore::open(&self.data_directory)
            .and_then(|mut store| store.open_question(&question))
            .map_err(|_| HeadlessTurnPortError::Permission)?;
        self.observer.opened(&question_id, context);

        let deadline = Instant::now() + self.wait;
        loop {
            if cancellation.is_cancelled() {
                self.close_unanswered(&question_id, "cancelled")?;
                return Ok(PermissionPromptAnswer::Cancel);
            }
            let answer = QuestionStore::open(&self.data_directory)
                .and_then(|mut store| store.take_answer(&self.target, &question_id))
                .map_err(|_| HeadlessTurnPortError::Permission)?;
            if let Some(answer) = answer {
                let (answer, selected, answered_by) = match answer.value.as_str() {
                    "allow_once" => (
                        PermissionPromptAnswer::AllowOnce,
                        "allow_once",
                        answer.source,
                    ),
                    "allow_always" => (
                        PermissionPromptAnswer::AllowAlways,
                        "allow_always",
                        answer.source,
                    ),
                    "deny_once" => (PermissionPromptAnswer::DenyOnce, "deny_once", answer.source),
                    "deny_always" => (
                        PermissionPromptAnswer::DenyAlways,
                        "deny_always",
                        answer.source,
                    ),
                    _ => return Err(HeadlessTurnPortError::Permission),
                };
                self.observer.closed(
                    &question_id,
                    selected,
                    match answered_by {
                        IntraTurnInputSource::Human => "human",
                        IntraTurnInputSource::Supervisor => "supervisor",
                    },
                );
                return Ok(answer);
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                self.close_unanswered(&question_id, "expired")?;
                return Err(HeadlessTurnPortError::PermissionExpired);
            };
            thread::sleep(remaining.min(QUESTION_POLL_INTERVAL));
        }
    }

    fn records_question_lifecycle(&self) -> bool {
        true
    }
}

struct DiagnosticQuestionObserver {
    bootstrap: Bootstrap,
    reference: String,
    scope: ProviderDiagnosticScope,
}

impl PermissionQuestionObserver for DiagnosticQuestionObserver {
    fn opened(&mut self, question_id: &str, context: &PermissionPromptContext) {
        let tool = agens_core::bare_tool_name(&context.tool_identity);
        let answers = ["allow_once", "allow_always", "deny_once", "deny_always"];
        record_session_lifecycle(
            &self.bootstrap,
            &self.reference,
            self.scope,
            SessionLifecycle::QuestionOpened {
                question_id,
                class: "permission",
                origin: tool.as_ref(),
                admissible_answers: &answers,
            },
        );
    }

    fn closed(&mut self, question_id: &str, selected_answer: &str, answered_by: &str) {
        record_session_lifecycle(
            &self.bootstrap,
            &self.reference,
            self.scope,
            SessionLifecycle::QuestionClosed {
                question_id,
                selected_answer,
                answered_by,
            },
        );
    }
}

struct ImmediateDenyPrompter;

impl PermissionPrompter for ImmediateDenyPrompter {
    fn prompt(
        &mut self,
        _: &PermissionPromptContext,
        _: &HeadlessTurnCancellation,
    ) -> Result<PermissionPromptAnswer, HeadlessTurnPortError> {
        Ok(PermissionPromptAnswer::DenyOnce)
    }

    fn records_question_lifecycle(&self) -> bool {
        true
    }
}

/// Builds the unattended port selected by the global runtime policy.
pub fn unattended_permission_prompter(
    bootstrap: &Bootstrap,
    target: DirectiveTarget,
    diagnostic_reference: impl Into<String>,
    scope: ProviderDiagnosticScope,
) -> Box<dyn PermissionPrompter> {
    let settings = bootstrap.unattended_permission_settings();
    if settings.deny_immediately {
        return Box::new(ImmediateDenyPrompter);
    }

    Box::new(QuestionPrompter::new(
        bootstrap.data_directory(),
        target,
        Duration::from_millis(settings.wait_ms),
        Box::new(DiagnosticQuestionObserver {
            bootstrap: bootstrap.clone(),
            reference: diagnostic_reference.into(),
            scope,
        }),
    ))
}

/// Builds a self-contained unattended prompter for a factory that learns its
/// target only after a session attempt begins.
pub fn unattended_permission_prompter_for_target(
    bootstrap: &Bootstrap,
    target: DirectiveTarget,
    scope: ProviderDiagnosticScope,
) -> Box<dyn PermissionPrompter> {
    unattended_permission_prompter(bootstrap, target, next_diagnostic_reference(), scope)
}
