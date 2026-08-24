//! Where an `ask_user` question reaches a person.
//!
//! Like [`crate::permission_prompt`], this lives in a different crate from the
//! domain contract so the port never names a surface type. Unlike a
//! permission prompt, `AskUserPort::ask` cannot fail: every terminal outcome
//! the surface can produce — cancelled, expired, unavailable, discussed, or
//! answered — is already a variant of `AskUserReply`, so this adapter has
//! nothing left to translate.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

use agens_core::ask_user::{AskUserAnswer, AskUserPort, AskUserReply, AskUserRequest};
use agens_core::{HeadlessTurnCancellation, IntraTurnInputSource};
use agens_diagnostics::{SessionLifecycle, next_diagnostic_reference, record_session_lifecycle};
use agens_providers::ProviderDiagnosticScope;
use agens_session::context::SessionContext;
use agens_store::{DirectiveTarget, OpenQuestion, QuestionClass, QuestionStore};
use agens_tui::{
    ExternalAskUserAnswer, PromptOrigin, TuiAskUserBridge, TuiAskUserObserver, TuiAskUserRequest,
};

/// The terminal UI's implementation of the ask-user port. Each surface owns
/// its own, so the engine never chooses between them.
/// The second field names the delegated execution asking, or `None` for the
/// main thread.
pub struct TuiAskUserPort(pub TuiAskUserBridge, pub Option<PromptOrigin>);

impl AskUserPort for TuiAskUserPort {
    fn ask(
        &self,
        request: &AskUserRequest,
        cancellation: &HeadlessTurnCancellation,
    ) -> AskUserReply {
        self.0
            .wait_for_reply(request.clone(), self.1.clone(), cancellation)
    }
}

pub fn production_tui_ask_user_bridge() -> (TuiAskUserBridge, Receiver<TuiAskUserRequest>) {
    TuiAskUserBridge::channel()
}

pub fn externally_answerable_tui_ask_user_bridge(
    bootstrap: agens_bootstrap::Bootstrap,
    session: Arc<Mutex<SessionContext>>,
) -> (TuiAskUserBridge, Receiver<TuiAskUserRequest>) {
    let (bridge, requests) = TuiAskUserBridge::channel();
    let observer = Arc::new(ExternalQuestionObserver {
        data_directory: bootstrap.data_directory().to_path_buf(),
        bootstrap,
        session,
        questions: Mutex::new(BTreeMap::new()),
    });

    (bridge.with_observer(observer), requests)
}

struct ObservedQuestion {
    external_id: String,
    target: Option<DirectiveTarget>,
}

struct ExternalQuestionObserver {
    bootstrap: agens_bootstrap::Bootstrap,
    data_directory: PathBuf,
    session: Arc<Mutex<SessionContext>>,
    questions: Mutex<BTreeMap<u64, ObservedQuestion>>,
}

impl TuiAskUserObserver for ExternalQuestionObserver {
    fn opened(&self, id: u64, request: &AskUserRequest, origin: Option<&PromptOrigin>) {
        let external_id = next_diagnostic_reference();
        let class = question_class(request);
        let admissible_answers = admissible_answers(request);
        let target = origin.map_or_else(
            || {
                self.session
                    .lock()
                    .ok()
                    .and_then(|session| session.identifier)
                    .map(DirectiveTarget::Session)
            },
            |origin| Some(DirectiveTarget::Child(origin.execution.to_string())),
        );
        let target = target.and_then(|target| {
            let question = OpenQuestion {
                target: target.clone(),
                question_id: external_id.clone(),
                class,
                origin: "ask_user".into(),
                admissible_answers: admissible_answers.clone(),
            };
            QuestionStore::open(&self.data_directory)
                .and_then(|mut store| store.open_question(&question))
                .ok()
                .map(|()| target)
        });
        if let Ok(mut questions) = self.questions.lock() {
            questions.insert(
                id,
                ObservedQuestion {
                    external_id: external_id.clone(),
                    target,
                },
            );
        }

        let answers = admissible_answers
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        record_session_lifecycle(
            &self.bootstrap,
            &external_id,
            ProviderDiagnosticScope::Parent,
            SessionLifecycle::QuestionOpened {
                question_id: &external_id,
                class: class.as_str(),
                origin: "ask_user",
                admissible_answers: &answers,
            },
        );
    }

    fn answer(&self, id: u64, request: &AskUserRequest) -> Option<ExternalAskUserAnswer> {
        let questions = self.questions.lock().ok()?;
        let observed = questions.get(&id)?;
        let target = observed.target.as_ref()?;
        let answer = QuestionStore::open(&self.data_directory)
            .ok()?
            .take_answer(target, &observed.external_id)
            .ok()??;
        let question = request.questions().first()?;
        if request.questions().len() != 1 {
            return None;
        }
        let reply = AskUserReply::Answered(vec![AskUserAnswer {
            question_id: question.id().to_owned(),
            selected: vec![answer.value],
            other: None,
            note: None,
        }]);
        request.validate_reply(&reply).ok()?;

        Some(ExternalAskUserAnswer {
            reply,
            answered_by: match answer.source {
                IntraTurnInputSource::Human => "human",
                IntraTurnInputSource::Supervisor => "supervisor",
            },
        })
    }

    fn closed(&self, id: u64, reply: &AskUserReply, answered_by: &'static str) {
        let Some(observed) = self
            .questions
            .lock()
            .ok()
            .and_then(|mut questions| questions.remove(&id))
        else {
            return;
        };
        let selected = selected_answer(reply);
        let source = if answered_by == "supervisor" {
            IntraTurnInputSource::Supervisor
        } else {
            IntraTurnInputSource::Human
        };
        if let Some(target) = observed.target.as_ref() {
            let _ = QuestionStore::open(&self.data_directory).and_then(|mut store| {
                store.close(target, &observed.external_id, &selected, source)
            });
        }
        record_session_lifecycle(
            &self.bootstrap,
            &observed.external_id,
            ProviderDiagnosticScope::Parent,
            SessionLifecycle::QuestionClosed {
                question_id: &observed.external_id,
                selected_answer: &selected,
                answered_by,
            },
        );
    }
}

fn question_class(request: &AskUserRequest) -> QuestionClass {
    let consent_title = request
        .title()
        .is_some_and(|title| title.to_ascii_lowercase().contains("consent"));
    let consent_domain = admissible_answers(request).iter().any(|answer| {
        matches!(
            answer.as_str(),
            "granted" | "declined" | "report_and_continue" | "stop_here"
        )
    });
    if consent_title || consent_domain {
        QuestionClass::Consent
    } else {
        QuestionClass::AskUser
    }
}

fn admissible_answers(request: &AskUserRequest) -> Vec<String> {
    request
        .questions()
        .iter()
        .flat_map(|question| {
            question
                .options()
                .iter()
                .map(|option| option.id().to_owned())
        })
        .collect()
}

fn selected_answer(reply: &AskUserReply) -> String {
    match reply {
        AskUserReply::Answered(answers) => answers
            .iter()
            .flat_map(|answer| answer.selected.iter().cloned())
            .collect::<Vec<_>>()
            .join(","),
        AskUserReply::Discuss { .. } => "discuss".into(),
        AskUserReply::Cancelled => "cancelled".into(),
        AskUserReply::Unavailable(_) => "unavailable".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agens_core::ask_user::{AskUserAnswer, AskUserMode, AskUserOption, AskUserQuestion};

    fn single_question_request() -> AskUserRequest {
        let options = vec![
            AskUserOption::new("a", "Option A", None, None),
            AskUserOption::new("b", "Option B", None, None),
        ];
        let question = AskUserQuestion::new(
            "plan",
            "Which plan?",
            None,
            AskUserMode::Single,
            options,
            false,
            false,
            false,
        );
        AskUserRequest::new(None, vec![question]).expect("valid request")
    }

    #[test]
    fn the_port_forwards_the_bridges_reply_verbatim() {
        let (bridge, requests) = production_tui_ask_user_bridge();
        let port = TuiAskUserPort(bridge.clone(), None);
        let cancellation = HeadlessTurnCancellation::new();

        let request = single_question_request();
        let answering_thread = std::thread::spawn(move || {
            let parked = requests
                .recv()
                .expect("the parked request should reach the receiver");
            let answer = AskUserAnswer {
                question_id: "plan".into(),
                selected: vec!["a".into()],
                other: None,
                note: None,
            };
            bridge.reply(parked.id(), AskUserReply::Answered(vec![answer]))
        });

        let reply = port.ask(&request, &cancellation);

        assert!(
            answering_thread
                .join()
                .expect("answering thread should not panic")
        );
        assert_eq!(
            reply,
            AskUserReply::Answered(vec![AskUserAnswer {
                question_id: "plan".into(),
                selected: vec!["a".into()],
                other: None,
                note: None,
            }])
        );
    }

    #[test]
    fn the_port_reports_unavailable_once_the_surface_is_closed() {
        let (bridge, _requests) = production_tui_ask_user_bridge();
        let port = TuiAskUserPort(bridge.clone(), None);
        let cancellation = HeadlessTurnCancellation::new();

        assert!(!bridge.close());

        let reply = port.ask(&single_question_request(), &cancellation);

        assert_eq!(
            reply,
            AskUserReply::Unavailable(agens_core::ask_user::AskUserUnavailable::SurfaceClosed)
        );
    }

    mod same_turn_integration {
        use std::collections::BTreeMap;
        use std::sync::{Arc, Mutex};
        use std::thread;

        use agens_core::{
            CompletedTurnRepository, CompletedTurnSnapshot, HeadlessTurnCancellation,
            HeadlessTurnPortError, MessagePart, PermissionMode, PermissionPolicy,
            PermissionSession, TurnEvent, TurnProvider,
        };
        use agens_dispatch::ProductionToolDispatcher;
        use agens_permissions::{
            PermissionPromptAnswer, PermissionPrompter, ProductionPermissionGate,
            ProductionPermissionResolver, ProductionPromptAuthorization,
        };
        use agens_store::PermissionGrantStore;
        use agens_tools::{AskUserTool, ToolDispatcher};

        use super::*;
        use agens_core::ask_user::AskUserAnswer;

        /// A scripted provider that emits one `ask_user` call and then, once
        /// this turn's events carry that call's result, a closing text part —
        /// the observable proof that the SAME turn continued past the
        /// interactive prompt rather than being restarted.
        struct ScriptedAskUserProvider {
            iterations: Vec<Result<Vec<MessagePart>, HeadlessTurnPortError>>,
        }

        impl TurnProvider for ScriptedAskUserProvider {
            fn next_parts(
                &mut self,
                _: &[TurnEvent],
                _: &HeadlessTurnCancellation,
            ) -> impl std::future::Future<Output = Result<Vec<MessagePart>, HeadlessTurnPortError>> + Send
            {
                std::future::ready(self.iterations.remove(0))
            }
        }

        struct NoopRepository;

        impl CompletedTurnRepository for NoopRepository {
            fn persist_completed_turn(
                &mut self,
                _: CompletedTurnSnapshot,
            ) -> impl std::future::Future<Output = Result<(), agens_core::CompletedTurnStoreError>> + Send
            {
                std::future::ready(Ok(()))
            }
        }

        struct NeverPrompt;

        impl PermissionPrompter for NeverPrompt {
            fn prompt(
                &mut self,
                _: &agens_tools::PermissionPromptContext,
                _: &HeadlessTurnCancellation,
            ) -> Result<PermissionPromptAnswer, HeadlessTurnPortError> {
                panic!("a bypassed session must never reach the permission prompter");
            }
        }

        fn run_ready<T>(
            future: impl std::future::Future<Output = Result<T, agens_core::HeadlessTurnError>>,
        ) -> Result<T, agens_core::HeadlessTurnError> {
            let mut future = std::pin::pin!(future);
            let context = &mut std::task::Context::from_waker(std::task::Waker::noop());

            match future.as_mut().poll(context) {
                std::task::Poll::Ready(result) => result,
                std::task::Poll::Pending => {
                    panic!("this fixture's ports must complete synchronously")
                }
            }
        }

        #[test]
        fn a_provider_ask_user_call_resolves_in_the_same_turn_as_its_tool_result() {
            let directory = std::env::temp_dir()
                .join(format!("agens-ask-user-same-turn-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&directory);

            let (bridge, requests) = production_tui_ask_user_bridge();
            let dispatcher = Arc::new(Mutex::new(ToolDispatcher::new()));
            dispatcher
                .lock()
                .expect("dispatcher lock should be available")
                .register_native(
                    "native::ask_user",
                    agens_core::ToolAccess::ReadOnly,
                    AskUserTool::new(Box::new(TuiAskUserPort(bridge.clone(), None))),
                )
                .expect("ask_user tool should register");

            let grants = Arc::new(Mutex::new(Vec::new()));
            let allowed = Arc::new(Mutex::new(BTreeMap::new()));
            let pending_prompts = Arc::new(Mutex::new(BTreeMap::new()));
            let policy = PermissionPolicy::new(PermissionMode::Edit, vec![]);
            let mut gate = ProductionPermissionGate::new(
                policy.clone(),
                Arc::clone(&grants),
                PermissionSession::with_temporary_bypass(),
                "project".into(),
                Arc::clone(&dispatcher),
                Arc::clone(&allowed),
                Arc::clone(&pending_prompts),
            );
            let mut resolver = ProductionPermissionResolver::new(
                NeverPrompt,
                PermissionGrantStore::open(&directory).expect("grant store should open"),
                grants,
                pending_prompts,
                ProductionPromptAuthorization {
                    policy,
                    session: PermissionSession::new(),
                    project: "project".into(),
                    dispatcher: Arc::clone(&dispatcher),
                    allowed: Arc::clone(&allowed),
                },
            );
            let mut tool_dispatcher = ProductionToolDispatcher::new(dispatcher, allowed);

            let ask_user_call = MessagePart::ToolCall {
                id: "call-1".into(),
                name: "native::ask_user".into(),
                input: serde_json::json!({
                    "questions": [{
                        "id": "q",
                        "prompt": "Pick one",
                        "mode": "single",
                        "options": [
                            {"id": "a", "label": "Option A"},
                            {"id": "b", "label": "Option B"}
                        ]
                    }]
                })
                .to_string(),
            };
            let mut provider = ScriptedAskUserProvider {
                iterations: vec![
                    Ok(vec![ask_user_call]),
                    Ok(vec![MessagePart::Text(
                        "thanks, continuing this turn".into(),
                    )]),
                ],
            };

            let answering_thread = thread::spawn(move || {
                let request = requests
                    .recv()
                    .expect("the ask_user request should reach the bridge");
                let answer = AskUserAnswer {
                    question_id: "q".into(),
                    selected: vec!["a".into()],
                    other: None,
                    note: None,
                };
                bridge.reply(request.id(), AskUserReply::Answered(vec![answer]))
            });

            let cancellation = HeadlessTurnCancellation::new();
            let result = run_ready(agens_core::run_headless_turn_with_progress(
                &mut provider,
                &mut gate,
                &mut resolver,
                &mut tool_dispatcher,
                &mut NoopRepository,
                &cancellation,
                None,
                None,
            ));

            assert!(
                answering_thread
                    .join()
                    .expect("the answering thread should not panic")
            );
            let snapshot = result.expect("the turn should complete");

            let (tool_result_content, tool_result_is_error) = snapshot
                .events()
                .iter()
                .find_map(|event| match event {
                    TurnEvent::ToolResult(MessagePart::ToolResult {
                        tool_call_id,
                        content,
                        is_error,
                    }) if tool_call_id == "call-1" => Some((content.clone(), *is_error)),
                    _ => None,
                })
                .expect("the ask_user tool result should be one of this turn's events");
            assert!(!tool_result_is_error);
            assert_eq!(
                tool_result_content,
                "{\"status\":\"answered\",\"answers\":[{\"question_id\":\"q\",\"answered\":true,\"selected\":[\"a\"],\"other\":null,\"note\":null}]}"
            );

            let turn_continued = snapshot.events().iter().any(|event| {
                matches!(
                    event,
                    TurnEvent::ProviderPart(MessagePart::Text(text))
                        if text == "thanks, continuing this turn"
                )
            });
            assert!(
                turn_continued,
                "the provider's closing text should be part of the SAME turn, proving the \
                 interactive completion did not restart it"
            );

            std::fs::remove_dir_all(&directory).ok();
        }
    }
}
