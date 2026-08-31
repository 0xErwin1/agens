//! Choosing a surface for a headless chat's permission questions.
//!
//! The turn itself lives in `agens-headless` and asks its questions through a
//! `PermissionPrompter` the caller supplies. Picking the terminal one is a
//! composition decision, so it stays here.
//!
//! `agens chat` used to wait on a durable question row that only an external
//! `agens direct` answer could close, which looked like a hang from the
//! terminal that launched it. The surface now follows standard input: a
//! terminal gets the question printed on it, and anything else gets an
//! immediate per-call denial with a printed notice.

use std::io::Write;

use agens_bootstrap::Bootstrap;
use agens_core::{HeadlessTurnCancellation, HeadlessTurnPortError};
use agens_error::CliError;
use agens_headless::{
    HeadlessChatFailure, HeadlessChatRequest, PermissionPrompterFactory,
    run_production_headless_chat_with_progress,
};
use agens_permissions::{PermissionPromptAnswer, PermissionPrompter, sanitize_permission_target};
use agens_providers::ProviderDiagnosticScope;
use agens_tool_runtime::external_permission::unattended_permission_prompter_for_target;
use agens_tools::PermissionPromptContext;
use agens_tui_app::permission_prompt::TtyPermissionPrompter;

pub(crate) fn run_production_headless_chat(
    request: HeadlessChatRequest,
    bootstrap: &Bootstrap,
    cancellation: &HeadlessTurnCancellation,
    stdin_is_terminal: bool,
) -> Result<String, CliError> {
    run_production_headless_chat_with_progress(
        request,
        bootstrap,
        cancellation,
        None,
        chat_permission_prompter_factory(bootstrap, stdin_is_terminal),
        None,
        None,
    )
    .map(|completion| completion.text)
    .map_err(HeadlessChatFailure::into_error)
}

/// Picks the permission surface for a one-shot chat from the input context the
/// command observed through its `stdin_is_terminal` dependency.
///
/// `deny_unattended_permission_prompts` keeps its meaning: an operator who
/// configured immediate denial gets the silent unattended prompter, not the
/// printed notice.
fn chat_permission_prompter_factory(
    bootstrap: &Bootstrap,
    stdin_is_terminal: bool,
) -> PermissionPrompterFactory {
    if stdin_is_terminal {
        return Box::new(|_| Box::new(TtyPermissionPrompter));
    }

    if bootstrap.unattended_permission_settings().deny_immediately {
        return Box::new({
            let bootstrap = bootstrap.clone();
            move |target| {
                unattended_permission_prompter_for_target(
                    &bootstrap,
                    target,
                    ProviderDiagnosticScope::Parent,
                )
            }
        });
    }

    Box::new(|_| Box::new(NonInteractiveDenyPrompter::new(std::io::stderr())))
}

/// Denies each permission question for its one call when the chat has no
/// terminal to ask on, and says so instead of degrading silently.
///
/// A denial rather than a turn failure keeps the existing deny semantics: the
/// model hears the refusal and the turn continues, exactly as when a person
/// answers "deny once".
struct NonInteractiveDenyPrompter<W: Write + Send> {
    notice: W,
}

impl<W: Write + Send> NonInteractiveDenyPrompter<W> {
    fn new(notice: W) -> Self {
        Self { notice }
    }

    #[cfg(test)]
    fn into_notice(self) -> W {
        self.notice
    }
}

impl<W: Write + Send> PermissionPrompter for NonInteractiveDenyPrompter<W> {
    fn prompt(
        &mut self,
        context: &PermissionPromptContext,
        _: &HeadlessTurnCancellation,
    ) -> Result<PermissionPromptAnswer, HeadlessTurnPortError> {
        let tool = agens_core::bare_tool_name(&context.tool_identity);
        let target = sanitize_permission_target(&context.tool_identity, &context.target_identifier);

        // A notice that cannot be written must not turn the denial into a
        // port failure: the call is refused either way.
        let _ = writeln!(
            self.notice,
            "permission required for {tool} on {target}, but standard input is not a terminal: denying this call. Allow it with a [permissions] rule in agens.toml, or run the interactive TUI (agens) to answer."
        );
        let _ = self.notice.flush();

        Ok(PermissionPromptAnswer::DenyOnce)
    }
}

#[cfg(test)]
mod tests {
    use agens_core::{HeadlessTurnCancellation, ToolAccess};
    use agens_permissions::{PermissionPromptAnswer, PermissionPrompter};
    use agens_tools::PermissionPromptContext;

    use super::NonInteractiveDenyPrompter;

    fn context() -> PermissionPromptContext {
        PermissionPromptContext {
            project_id: "project".into(),
            tool_identity: "mcp:6:engram:17:mem_session_start".into(),
            target_identifier: "mem_session_start".into(),
            access: ToolAccess::Write,
            reason: "permission policy requires confirmation".into(),
            denylist: None,
        }
    }

    /// AGN-229: without a terminal, a permission question is denied for that
    /// call immediately instead of waiting on an answer nobody can give.
    #[test]
    fn non_interactive_prompter_denies_the_call_once() {
        let mut prompter = NonInteractiveDenyPrompter::new(Vec::new());

        let answer = prompter
            .prompt(&context(), &HeadlessTurnCancellation::new())
            .expect("denying without a terminal is not a port failure");

        assert_eq!(answer, PermissionPromptAnswer::DenyOnce);
    }

    /// The denial is not silent: the notice names the blocked tool and both
    /// ways forward, so a piped or CI run can be fixed instead of debugged.
    #[test]
    fn non_interactive_prompter_prints_an_actionable_notice() {
        let mut prompter = NonInteractiveDenyPrompter::new(Vec::new());

        prompter
            .prompt(&context(), &HeadlessTurnCancellation::new())
            .expect("denying without a terminal is not a port failure");

        let notice = String::from_utf8(prompter.into_notice()).expect("notice is UTF-8");
        assert!(
            notice.contains("engram::mem_session_start"),
            "the notice must name the blocked tool the way a rule names it, got: {notice}"
        );
        assert!(
            notice.contains("standard input is not a terminal"),
            "the notice must say why the question could not be asked, got: {notice}"
        );
        assert!(
            notice.contains("[permissions]"),
            "the notice must point at an allow rule, got: {notice}"
        );
        assert!(
            notice.contains("agens"),
            "the notice must point at the interactive TUI, got: {notice}"
        );
    }
}
