use std::path::PathBuf;
use std::time::Duration;

use agens_core::{HeadlessTurnCancellation, HeadlessTurnPortError, ToolAccess};
use agens_permissions::{PermissionPromptAnswer, PermissionPrompter};
use agens_store::{DirectiveTarget, QuestionStore};
use agens_tool_runtime::external_permission::{PermissionQuestionObserver, QuestionPrompter};
use agens_tools::PermissionPromptContext;

fn temporary_directory(label: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "agens-external-permission-{label}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::remove_dir_all(&directory).ok();
    std::fs::create_dir_all(&directory).expect("test directory must exist");
    directory
}

fn prompt_context() -> PermissionPromptContext {
    PermissionPromptContext {
        project_id: "project".into(),
        tool_identity: "native:4:write".into(),
        target_identifier: "notes.md".into(),
        access: ToolAccess::Write,
        reason: "permission policy requires confirmation".into(),
        denylist: None,
    }
}

struct AnswerOnOpen {
    data_directory: PathBuf,
    target: DirectiveTarget,
}

impl PermissionQuestionObserver for AnswerOnOpen {
    fn opened(&mut self, question_id: &str, _: &PermissionPromptContext) {
        QuestionStore::open(&self.data_directory)
            .and_then(|mut store| {
                store.answer(
                    &self.target,
                    question_id,
                    "allow_once",
                    agens_core::IntraTurnInputSource::Supervisor,
                )
            })
            .expect("a direct answer must reach the question that just opened");
    }

    fn closed(&mut self, _: &str, _: &str, _: &str) {}
}

struct NoopObserver;

impl PermissionQuestionObserver for NoopObserver {
    fn opened(&mut self, _: &str, _: &PermissionPromptContext) {}

    fn closed(&mut self, _: &str, _: &str, _: &str) {}
}

#[test]
fn an_unattended_permission_question_accepts_a_direct_answer_at_its_bound_target() {
    let data_directory = temporary_directory("answer");
    let target = DirectiveTarget::Child("c0ffee00".into());
    let mut prompter = QuestionPrompter::new(
        &data_directory,
        target.clone(),
        Duration::from_secs(1),
        Box::new(AnswerOnOpen {
            data_directory: data_directory.clone(),
            target,
        }),
    );

    let answer = prompter
        .prompt(&prompt_context(), &HeadlessTurnCancellation::new())
        .expect("the direct answer must become the permission decision");

    assert_eq!(answer, PermissionPromptAnswer::AllowOnce);
    std::fs::remove_dir_all(data_directory).ok();
}

#[test]
fn an_unattended_permission_question_refuses_with_a_named_expiry() {
    let data_directory = temporary_directory("expiry");
    let mut prompter = QuestionPrompter::new(
        &data_directory,
        DirectiveTarget::Session(42),
        Duration::ZERO,
        Box::new(NoopObserver),
    );

    let error = prompter
        .prompt(&prompt_context(), &HeadlessTurnCancellation::new())
        .expect_err("an unanswered zero-budget question must expire without sleeping");

    assert_eq!(error, HeadlessTurnPortError::PermissionExpired);
    std::fs::remove_dir_all(data_directory).ok();
}
