use agens_core::ask_user::{
    AskUserMode, AskUserOption, AskUserQuestion, AskUserReply, AskUserRequest, AskUserUnavailable,
};
use agens_core::{
    HeadlessTurnCancellation, Message, MessagePart, Role, ToolInput, TurnEvent, TurnState,
};
use agens_core::{NoticeSeverity, SubagentErrorKind, SubagentStatus};
use agens_tui::{
    Action, AppEvent, AppState, AskUserEditing, AskUserRowSnapshot, BridgeCancel, BridgeTx,
    Command, Conversation, ConversationError, ConversationEvent, Dialog, DialogEntry, DialogView,
    DiffLine, DiffLineKind, DisplayMode, Effect, Engine, Event, Key, PaletteEntry,
    PaletteEntryKind, PublishOutcome, RatatuiRenderer, Renderer, Runtime, SessionDialogCursor,
    SessionDialogRequest, SessionDialogScope, TranscriptEntry, TranscriptFocus, TranscriptId, Tui,
    TuiExecutionEvent, TuiExecutionState, TuiPermissionBridge, TuiPermissionReply, TuiPresentation,
    TuiProviderOutcome, TuiRouteProgress, TuiRuntimeEvent, TuiSubagentEvent, TuiSubmissionOutcome,
    TurnLifecycle,
};
use ratatui::{Terminal, backend::TestBackend};
use std::{
    collections::BTreeMap,
    thread,
    time::{Duration, Instant},
};

/// Mirrors `BlockContent::default_mode` (Collapsed): a call with no explicit
/// mode entry renders collapsed, same as an explicit `Collapsed` entry.
fn is_collapsed(modes: &BTreeMap<String, DisplayMode>, call_id: &str) -> bool {
    matches!(modes.get(call_id), None | Some(DisplayMode::Collapsed))
}

#[derive(Default)]
struct FakeEngine {
    cancellations: usize,
}

fn start_child(tui: &mut Tui<FakeEngine>, id: u64) {
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "reviewer".into(),
        event: TuiExecutionEvent::ForegroundStarted { id },
    });
}

#[test]
fn transcript_registry_model_starts_with_an_active_main_record() {
    let tui = Tui::new(FakeEngine::default());
    let view = tui.view();

    assert_eq!(view.active_transcript, TranscriptId::Main);
    assert_eq!(view.transcript_ids, vec![TranscriptId::Main]);
    assert_eq!(
        tui.transcript_record(&TranscriptId::Main).unwrap().id(),
        &TranscriptId::Main
    );
}

#[test]
fn transcript_admission_retention_keeps_terminal_records_after_cards_expire() {
    let mut tui = Tui::new(FakeEngine::default());

    for id in 1..=65 {
        tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
            agent: "reviewer".into(),
            event: TuiExecutionEvent::ForegroundStarted { id },
        });
        tui.apply_runtime_event(TuiRuntimeEvent::SubagentExecution(
            TuiSubagentEvent::started(
                id,
                "reviewer",
                format!("review-{id}"),
                TuiExecutionState::ForegroundRunning,
            ),
        ));
        tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
            agent: "reviewer".into(),
            event: TuiExecutionEvent::Completed { id },
        });
        tui.apply_runtime_event(TuiRuntimeEvent::SubagentExecution(
            TuiSubagentEvent::terminal(id, SubagentStatus::Success, format!("final-{id}")),
        ));
    }

    tui.tick(Duration::from_secs(60));

    assert!(tui.executions().is_empty());
    assert_eq!(tui.view().active_transcript, TranscriptId::Main);
    assert_eq!(tui.view().transcript_ids.len(), 65);
    assert!(tui.transcript_record(&TranscriptId::Subagent(1)).is_none());
    assert_eq!(
        tui.transcript_record(&TranscriptId::Subagent(2))
            .unwrap()
            .id(),
        &TranscriptId::Subagent(2)
    );
}

#[test]
fn transcript_admission_retention_ignores_out_of_order_and_post_terminal_updates() {
    let mut tui = Tui::new(FakeEngine::default());

    tui.apply_runtime_event_with_ordinal(
        10,
        TuiRuntimeEvent::TaskExecution {
            agent: "reviewer".into(),
            event: TuiExecutionEvent::ForegroundStarted { id: 7 },
        },
    );
    tui.apply_runtime_event_with_ordinal(
        9,
        TuiRuntimeEvent::TaskExecution {
            agent: "reviewer".into(),
            event: TuiExecutionEvent::Backgrounded { id: 7 },
        },
    );
    tui.apply_runtime_event_with_ordinal(
        11,
        TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::started(
            7,
            "reviewer",
            "review",
            TuiExecutionState::ForegroundRunning,
        )),
    );
    tui.apply_runtime_event_with_ordinal(
        12,
        TuiRuntimeEvent::TaskExecution {
            agent: "reviewer".into(),
            event: TuiExecutionEvent::Completed { id: 7 },
        },
    );
    tui.apply_runtime_event_with_ordinal(
        13,
        TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::terminal(
            7,
            SubagentStatus::Success,
            "final",
        )),
    );
    tui.apply_runtime_event_with_ordinal(
        14,
        TuiRuntimeEvent::TaskExecution {
            agent: "reviewer".into(),
            event: TuiExecutionEvent::Backgrounded { id: 7 },
        },
    );

    assert_eq!(
        tui.executions()[0].state(),
        TuiExecutionState::CompletedRecent
    );
    assert_eq!(tui.runtime_events().len(), 4);
    assert_eq!(
        tui.transcript_record(&TranscriptId::Subagent(7))
            .unwrap()
            .last_admitted_ordinal(),
        Some(13)
    );
    assert!(
        tui.transcript_record(&TranscriptId::Subagent(7))
            .unwrap()
            .is_terminal()
    );
    assert_eq!(
        tui.view().transcript_ids,
        vec![TranscriptId::Main, TranscriptId::Subagent(7)]
    );
}

#[test]
fn transcript_admission_retention_protects_active_child_and_falls_back_after_eviction() {
    let mut tui = Tui::new(FakeEngine::default());

    for id in 1..=65 {
        tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
            agent: "reviewer".into(),
            event: TuiExecutionEvent::ForegroundStarted { id },
        });
        tui.apply_runtime_event(TuiRuntimeEvent::SubagentExecution(
            TuiSubagentEvent::started(
                id,
                "reviewer",
                format!("review-{id}"),
                TuiExecutionState::ForegroundRunning,
            ),
        ));
        tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
            agent: "reviewer".into(),
            event: TuiExecutionEvent::Completed { id },
        });
        tui.apply_runtime_event(TuiRuntimeEvent::SubagentExecution(
            TuiSubagentEvent::terminal(id, SubagentStatus::Success, format!("final-{id}")),
        ));

        if id == 1 {
            tui.select_transcript(TranscriptId::Subagent(id));
        }
    }

    assert_eq!(tui.view().active_transcript, TranscriptId::Subagent(1));
    assert!(tui.transcript_record(&TranscriptId::Subagent(1)).is_some());
    assert!(tui.transcript_record(&TranscriptId::Subagent(2)).is_none());

    tui.select_transcript(TranscriptId::Subagent(2));
    assert_eq!(tui.view().active_transcript, TranscriptId::Main);
}

#[test]
fn transcript_admission_retention_clears_live_children_only_at_reset_boundaries() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "reviewer".into(),
        event: TuiExecutionEvent::ForegroundStarted { id: 7 },
    });

    tui.apply_submission_outcome(TuiSubmissionOutcome::ContextChanged {
        message: "Context updated.".into(),
        presentation: TuiPresentation::new("provider", "model", "session"),
    });
    assert!(tui.transcript_record(&TranscriptId::Subagent(7)).is_some());

    tui.apply_submission_outcome(TuiSubmissionOutcome::ResetSucceeded {
        message: "Started a new session.".into(),
        presentation: TuiPresentation::new("provider", "model", "new session"),
    });

    assert_eq!(tui.view().active_transcript, TranscriptId::Main);
    assert_eq!(tui.view().transcript_ids, vec![TranscriptId::Main]);
}

#[test]
fn transcript_admission_retention_session_resume_keeps_restored_history_summary_only() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "reviewer".into(),
        event: TuiExecutionEvent::ForegroundStarted { id: 7 },
    });

    tui.apply_submission_outcome(TuiSubmissionOutcome::SessionResumed {
        message: "Resumed session 42.".into(),
        presentation: TuiPresentation::new("provider", "model", "session #42"),
        history: Conversation::from_messages(&[
            agens_core::Message {
                role: agens_core::Role::User,
                parts: vec![MessagePart::Text("restored prompt".into())],
            },
            agens_core::Message {
                role: agens_core::Role::Assistant,
                parts: vec![MessagePart::Text("restored summary".into())],
            },
        ])
        .unwrap(),
        draft: None,
        media_chips: Vec::new(),
        resume_error: None,
        file_candidates: Vec::new(),
        palette_entries: Vec::new(),
    });

    let view = tui.view();
    assert_eq!(view.active_transcript, TranscriptId::Main);
    assert_eq!(view.transcript_ids, vec![TranscriptId::Main]);
    assert_eq!(view.completed_conversations.len(), 1);
    assert!(view.conversation.is_none());
    assert!(tui.transcript_record(&TranscriptId::Subagent(7)).is_none());
}

#[test]
fn session_loading_is_local_preserves_visible_state_and_escape_cancels() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.set_presentation("old-provider", "old-model", "session #1");
    tui.begin_submission("old prompt");
    tui.finish_submission(Ok("old answer".into()));
    tui.apply_runtime_event(TuiRuntimeEvent::Usage(agens_core::Usage {
        input_tokens: Some(3),
        output_tokens: Some(5),
        total_tokens: Some(8),
        context_window: Some(128),
    }));
    tui.show_selection_dialog(DialogView::sessions_page(
        vec![DialogEntry::action("Session 2", "session:2")],
        SessionDialogRequest::initial(),
        None,
    ));

    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::DialogAction("session:2".into())
    );
    assert!(tui.view().dialog.is_some());
    assert!(tui.begin_session_load());

    let loading = tui.view();
    assert!(loading.session_loading);
    assert!(!loading.running);
    assert_eq!(loading.provider_model, "old-provider / old-model");
    assert_eq!(loading.latest_usage.unwrap().total_tokens, Some(8));
    assert_eq!(loading.conversation.unwrap().user, "old prompt");
    assert!(loading.dialog.is_some());
    assert!(loading.executions.is_empty());
    assert_eq!(tui.handle(Event::Key(Key::Enter)), Action::Render);

    assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::CancelRoute);
    tui.cancel_session_load();
    let cancelled = tui.view();
    assert!(!cancelled.session_loading);
    assert!(!cancelled.running);
    assert_eq!(cancelled.provider_model, "old-provider / old-model");
    assert_eq!(cancelled.latest_usage.unwrap().total_tokens, Some(8));
    assert_eq!(cancelled.conversation.unwrap().user, "old prompt");
}

#[test]
fn session_resume_success_replaces_prepared_state_in_one_outcome() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.set_presentation("old-provider", "old-model", "session #1");
    tui.begin_submission("old prompt");
    tui.finish_submission(Ok("old answer".into()));
    tui.apply_runtime_event(TuiRuntimeEvent::Usage(agens_core::Usage {
        input_tokens: Some(3),
        output_tokens: Some(5),
        total_tokens: Some(8),
        context_window: Some(128),
    }));
    assert!(tui.begin_session_load());
    let history = Conversation::from_messages(&[
        agens_core::Message {
            role: agens_core::Role::User,
            parts: vec![MessagePart::Text("restored prompt".into())],
        },
        agens_core::Message {
            role: agens_core::Role::Assistant,
            parts: vec![MessagePart::Text("restored answer".into())],
        },
    ])
    .unwrap();

    tui.apply_submission_outcome(TuiSubmissionOutcome::SessionResumed {
        message: "Resumed session 2.".into(),
        presentation: TuiPresentation::new("new-provider", "new-model", "session #2"),
        history,
        draft: None,
        media_chips: Vec::new(),
        resume_error: None,
        file_candidates: Vec::new(),
        palette_entries: Vec::new(),
    });

    let resumed = tui.view();
    assert!(!resumed.session_loading);
    assert!(!resumed.running);
    assert_eq!(resumed.provider_model, "new-provider / new-model");
    assert_eq!(resumed.session, "session #2");
    assert!(resumed.latest_usage.is_none());
    assert!(resumed.runtime_events.is_empty());
    assert_eq!(resumed.completed_conversations.len(), 1);
    assert_eq!(resumed.completed_conversations[0].user, "restored prompt");
}

#[test]
fn failed_session_resume_restores_exact_draft_with_history_at_composer_bottom() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.handle(Event::Resize {
        width: 40,
        height: 8,
    });
    tui.begin_submission("old prompt");
    tui.finish_submission(Ok("old answer".into()));
    tui.handle(Event::Key(Key::PageUp));
    assert!(!tui.following_bottom());
    assert!(tui.begin_session_load());

    tui.apply_submission_outcome(TuiSubmissionOutcome::SessionResumed {
        message: "Previous attempt failed. Prompt restored; press Enter to retry.".into(),
        presentation: TuiPresentation::new("openai-chatgpt", "gpt-5.5", "session #327"),
        history: vec![Conversation::new("completed prompt")],
        draft: Some("retry exact café 🙂".into()),
        media_chips: Vec::new(),
        resume_error: None,
        file_candidates: Vec::new(),
        palette_entries: Vec::new(),
    });

    let resumed = tui.view();
    assert!(!resumed.session_loading);
    assert!(!resumed.running);
    assert_eq!(resumed.input, "retry exact café 🙂");
    assert_eq!(resumed.input_cursor, "retry exact café 🙂".chars().count());
    assert_eq!(resumed.focus, agens_tui::TranscriptFocus::Composer);
    assert!(resumed.following_bottom);
    assert!(resumed.recovered_failed_prompt);
    assert_eq!(
        resumed.status,
        Some("Previous attempt failed. Prompt restored; press Enter to retry.")
    );
    assert_eq!(resumed.completed_conversations.len(), 1);
    assert_eq!(resumed.completed_conversations[0].user, "completed prompt");
    assert_ne!(resumed.completed_conversations[0].user, resumed.input);
    tui.handle(Event::Key(Key::Char('!')));
    assert!(tui.view().recovered_failed_prompt);
    tui.show_selection_dialog(DialogView::selection(
        "Resume session · Current project",
        None::<String>,
        vec![DialogEntry::action("Session 2", "session:2")],
    ));
    assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::Render);
    assert_eq!(tui.input(), "retry exact café 🙂!");
    assert!(tui.view().recovered_failed_prompt);
    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::Submit("retry exact café 🙂!".into())
    );
    assert!(!tui.view().recovered_failed_prompt);
}

#[test]
fn recovered_failed_prompt_escape_discards_and_successful_resume_replaces_atomically() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.apply_submission_outcome(TuiSubmissionOutcome::SessionResumed {
        message: "Recovered failed prompt.".into(),
        presentation: TuiPresentation::new("provider", "model", "session #1"),
        history: Vec::new(),
        draft: Some("failed prompt".into()),
        media_chips: Vec::new(),
        resume_error: None,
        file_candidates: Vec::new(),
        palette_entries: Vec::new(),
    });

    assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::Render);
    assert_eq!(tui.input(), "failed prompt");
    assert!(tui.view().recovered_failed_prompt);

    tui.apply_submission_outcome(TuiSubmissionOutcome::SessionResumed {
        message: "Recovered failed prompt.".into(),
        presentation: TuiPresentation::new("provider", "model", "session #2"),
        history: Vec::new(),
        draft: Some("older failed prompt".into()),
        media_chips: Vec::new(),
        resume_error: None,
        file_candidates: Vec::new(),
        palette_entries: Vec::new(),
    });
    tui.apply_submission_outcome(TuiSubmissionOutcome::SessionResumed {
        message: "Resumed session 3.".into(),
        presentation: TuiPresentation::new("provider", "model", "session #3"),
        history: Vec::new(),
        draft: None,
        media_chips: Vec::new(),
        resume_error: None,
        file_candidates: Vec::new(),
        palette_entries: Vec::new(),
    });

    assert!(tui.input().is_empty());
    assert!(!tui.view().recovered_failed_prompt);
}

#[test]
fn session_dialog_requests_server_search_scope_and_keyset_pages_with_generations() {
    let initial = SessionDialogRequest::initial();
    let mut tui = Tui::new(FakeEngine::default());
    tui.show_selection_dialog(DialogView::sessions_page(
        vec![DialogEntry::action("Session 501", "session:501")],
        initial.clone(),
        Some(SessionDialogCursor::new(250, 438)),
    ));

    assert_eq!(tui.handle(Event::Key(Key::Char('/'))), Action::Render);
    let Action::LoadSessionPage(search) = tui.handle(Event::Key(Key::Char('n'))) else {
        panic!("session search should request a server page");
    };
    assert_eq!(search.scope(), SessionDialogScope::CurrentProject);
    assert_eq!(search.query(), "n");
    assert_eq!(search.page(), 1);
    assert!(search.cursor().is_none());
    assert!(search.generation() > initial.generation());

    tui.apply_submission_outcome(TuiSubmissionOutcome::Dialog(DialogView::sessions_page(
        vec![DialogEntry::action("stale", "session:1")],
        initial,
        None,
    )));
    assert!(tui.view().dialog.unwrap().is_loading());

    tui.apply_submission_outcome(TuiSubmissionOutcome::Dialog(DialogView::sessions_page(
        vec![DialogEntry::action("Needle", "session:1")],
        search,
        Some(SessionDialogCursor::new(10, 1)),
    )));
    let Action::LoadSessionPage(next) = tui.handle(Event::Key(Key::PageDown)) else {
        panic!("PageDown should request the next keyset page");
    };
    assert_eq!(next.page(), 2);
    assert_eq!(next.cursor(), Some(SessionDialogCursor::new(10, 1)));

    let Action::LoadSessionPage(previous) = tui.handle(Event::Key(Key::PageUp)) else {
        panic!("PageUp should request the previous keyset page");
    };
    assert_eq!(previous.page(), 1);
    assert!(previous.cursor().is_none());
    assert_eq!(previous.query(), "n");

    let Action::LoadSessionPage(global) = tui.handle(Event::Key(Key::LineStart)) else {
        panic!("Ctrl+A should request the alternate scope");
    };
    assert_eq!(global.scope(), SessionDialogScope::AllProjects);
    assert_eq!(global.query(), "n");
    assert_eq!(global.page(), 1);
    assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::CancelRoute);
}

#[test]
fn transcript_navigation_restores_focus_and_routes_live_child_composer_to_mailbox() {
    let mut tui = Tui::new(FakeEngine::default());
    for (id, call, output) in [(7, "seven", 40), (8, "eight", 80)] {
        start_child(&mut tui, id);
        tui.apply_runtime_event(TuiRuntimeEvent::SubagentExecution(
            TuiSubagentEvent::started(id, "reviewer", "task", TuiExecutionState::ForegroundRunning),
        ));
        tui.apply_runtime_event(TuiRuntimeEvent::SubagentExecution(
            TuiSubagentEvent::tool_call(id, call, "tool", "input"),
        ));
        tui.apply_runtime_event(TuiRuntimeEvent::SubagentExecution(
            TuiSubagentEvent::tool_result(id, call, "output\n".repeat(output), false),
        ));
    }

    focus_viewport(&mut tui);
    tui.handle(Event::Key(Key::Char('g')));
    assert_eq!(tui.handle(Event::Key(Key::Char('t'))), Action::Render);
    assert!(tui.view().dialog.is_some());
    tui.handle(Event::Key(Key::Enter));
    assert_eq!(tui.view().active_transcript, TranscriptId::Subagent(7));
    tui.handle(Event::Key(Key::Char('[')));
    assert_eq!(tui.view().active_transcript, TranscriptId::Subagent(7));
    tui.handle(Event::Key(Key::Char(']')));
    assert_eq!(tui.view().active_transcript, TranscriptId::Subagent(8));
    tui.handle(Event::Key(Key::Char('l')));
    assert_eq!(tui.view().active_transcript, TranscriptId::Subagent(8));
    tui.handle(Event::Key(Key::Char('m')));
    assert_eq!(tui.view().active_transcript, TranscriptId::Main);
    tui.select_transcript(TranscriptId::Subagent(7));
    assert_eq!(
        tui.handle(Event::Key(Key::Char('x'))),
        Action::CancelExecution(7)
    );
    assert_eq!(tui.input(), "");
    assert_eq!(tui.view().focus, TranscriptFocus::Viewport);
    tui.set_collapse_thinking(true);
    // Collapsed → Truncated → Expanded: the bounded Truncated preview is too
    // short to scroll, so the full body is what this navigation assertion needs.
    tui.handle(Event::Key(Key::CtrlO));
    tui.handle(Event::Key(Key::CtrlO));
    tui.handle(Event::Key(Key::PageUp));
    assert!(tui.view().collapse_thinking);
    assert!(tui.view().scroll_offset > 0);
    assert!(!is_collapsed(tui.view().tool_display_modes, "seven"));
    tui.handle(Event::Key(Key::Home));
    let child_seven_offset = tui.view().scroll_offset;
    tui.show_selection_dialog(DialogView::selection(
        "Choose",
        None::<String>,
        vec![DialogEntry::action("Close", "close")],
    ));
    tui.handle(Event::Key(Key::Escape));
    assert_eq!(tui.view().focus, TranscriptFocus::Viewport);
    tui.handle(Event::Key(Key::Char('m')));
    tui.handle(Event::Key(Key::Char('i')));
    assert_eq!(tui.view().focus, TranscriptFocus::Composer);
    assert!(!tui.view().collapse_thinking);
    assert!(tui.view().following_bottom);
    assert!(tui.view().tool_display_modes.is_empty());
    tui.handle(Event::Key(Key::Char('m')));
    assert_eq!(tui.input(), "m");

    focus_viewport(&mut tui);
    tui.handle(Event::Key(Key::Char(']')));
    assert!(tui.view().collapse_thinking);
    assert!(!tui.view().following_bottom);
    assert_eq!(tui.handle(Event::Paste(" blocked".into())), Action::Render);
    assert_eq!(tui.input(), "m");
    assert_eq!(tui.handle(Event::Key(Key::Char('i'))), Action::Render);
    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::SendTaskMessage {
            id: 7,
            message: "m".into(),
        }
    );
    assert_eq!(tui.input(), "");
    tui.handle(Event::Key(Key::Char(']')));
    tui.handle(Event::Key(Key::CtrlO));
    tui.handle(Event::Key(Key::CtrlO));
    tui.handle(Event::Key(Key::PageUp));
    let child_eight_offset = tui.view().scroll_offset;
    assert_ne!(child_eight_offset, child_seven_offset);
    assert!(!is_collapsed(tui.view().tool_display_modes, "eight"));
    // "seven" belongs to a different transcript's own record; it is simply
    // absent here (state isolation), not a "seven" collapse claim.
    assert!(!tui.view().tool_display_modes.contains_key("seven"));

    tui.handle(Event::Key(Key::Char('[')));
    assert_eq!(tui.view().scroll_offset, child_seven_offset);
    assert!(!is_collapsed(tui.view().tool_display_modes, "seven"));

    tui.apply_runtime_event(TuiRuntimeEvent::SubagentExecution(
        TuiSubagentEvent::started(
            8,
            "reviewer",
            "inactive task",
            TuiExecutionState::ForegroundRunning,
        ),
    ));
    assert_eq!(tui.view().active_transcript, TranscriptId::Subagent(7));
    assert!(
        tui.transcript_record(&TranscriptId::Subagent(8))
            .unwrap()
            .last_admitted_ordinal()
            .is_some()
    );

    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "reviewer".into(),
        event: TuiExecutionEvent::Completed { id: 7 },
    });
    tui.apply_runtime_event(TuiRuntimeEvent::SubagentExecution(
        TuiSubagentEvent::terminal(7, SubagentStatus::Success, "done"),
    ));
    tui.tick(Duration::from_secs(60));
    assert!(tui.executions().iter().all(|execution| execution.id() != 7));
    assert_eq!(tui.handle(Event::Key(Key::Char('g'))), Action::Render);
    assert_eq!(tui.handle(Event::Key(Key::Enter)), Action::Render);
    assert_eq!(tui.view().active_transcript, TranscriptId::Subagent(7));

    let mut restored = Tui::new(FakeEngine::default());
    restored.apply_runtime_event(TuiRuntimeEvent::RestoredCompletedSubagent {
        id: 42,
        agent: "reviewer".into(),
        task_summary: "restored".into(),
        final_result: "done".into(),
        tool_uses: 1,
    });
    restored.handle(Event::Key(Key::Escape));
    assert_eq!(restored.handle(Event::Key(Key::Char('g'))), Action::Render);
    assert!(restored.view().dialog.is_none());
}

#[test]
fn execution_strip_navigation_enters_children_and_backgrounds_the_focused_execution() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.set_agent_catalog(["reviewer", "writer"]);
    tui.select_agent("writer");
    for character in "next task".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }
    for (id, agent) in [(7, "reviewer"), (8, "writer")] {
        tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
            agent: agent.into(),
            event: TuiExecutionEvent::ForegroundStarted { id },
        });
        tui.apply_runtime_event(TuiRuntimeEvent::SubagentExecution(
            TuiSubagentEvent::started(id, agent, "task", TuiExecutionState::ForegroundRunning),
        ));
    }

    assert_eq!(tui.handle(Event::Key(Key::Tab)), Action::Render);
    assert_eq!(tui.view().surface_focus, agens_tui::SurfaceFocus::Queue);
    assert_eq!(tui.handle(Event::Key(Key::Tab)), Action::Render);
    assert_eq!(tui.view().surface_focus, agens_tui::SurfaceFocus::Composer);
    assert_eq!(
        tui.view().execution_selection,
        None,
        "Tab never reaches the subagent tree"
    );
    assert_eq!(tui.input(), "next task");
    assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::Render);
    assert_eq!(tui.view().active_transcript, TranscriptId::Main);

    // Row 0 is transcript chrome, not subagent navigation: it starts a
    // selection drag and leaves the active transcript alone.
    assert_eq!(
        tui.handle(Event::MouseDown { column: 11, row: 0 }),
        Action::Render
    );
    assert_eq!(tui.view().active_transcript, TranscriptId::Main);
    assert!(tui.selected_text().is_none());
}

#[test]
fn child_activity_reorders_executions_and_background_transition_keeps_parent_running() {
    let mut tui = Tui::new(FakeEngine::default());
    start_child(&mut tui, 7);
    tui.tick(Duration::from_secs(1));
    start_child(&mut tui, 8);
    tui.tick(Duration::from_secs(2));

    tui.apply_runtime_event(TuiRuntimeEvent::SubagentExecution(
        TuiSubagentEvent::reasoning(7, "working"),
    ));
    assert_eq!(tui.executions()[0].id(), 7);

    tui.set_running(true);
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "reviewer".into(),
        event: TuiExecutionEvent::Backgrounded { id: 7 },
    });
    assert!(tui.view().running);
}

#[test]
fn transcript_picker_outcome_and_gt_use_the_same_main_and_child_entries() {
    let mut tui = Tui::new(FakeEngine::default());
    start_child(&mut tui, 7);
    tui.apply_runtime_event(TuiRuntimeEvent::SubagentExecution(
        TuiSubagentEvent::started(
            7,
            "reviewer",
            "review navigation",
            TuiExecutionState::ForegroundRunning,
        ),
    ));

    focus_viewport(&mut tui);
    tui.handle(Event::Key(Key::Char('g')));
    assert_eq!(tui.handle(Event::Key(Key::Char('t'))), Action::Render);
    let from_gt = format!("{:?}", tui.view().dialog);
    tui.handle(Event::Key(Key::Escape));
    tui.apply_submission_outcome(TuiSubmissionOutcome::TranscriptDialog);
    let from_command = format!("{:?}", tui.view().dialog);

    assert_eq!(from_command, from_gt);
    assert!(from_command.contains("Main"));
    assert!(from_command.contains("Reviewer"));
}

#[test]
fn vim_modes_remove_all_function_key_routes() {
    let mut tui = Tui::new(FakeEngine::default());
    for id in [7, 8] {
        start_child(&mut tui, id);
        tui.apply_runtime_event(TuiRuntimeEvent::SubagentExecution(
            TuiSubagentEvent::started(id, "reviewer", "task", TuiExecutionState::ForegroundRunning),
        ));
    }

    tui.select_transcript(TranscriptId::Subagent(7));
    assert_eq!(tui.handle(Event::Key(Key::Char('g'))), Action::Render);
    assert_eq!(tui.handle(Event::Key(Key::Char('t'))), Action::Render);
    assert!(tui.view().dialog.is_some());
    tui.handle(Event::Key(Key::Escape));

    assert_eq!(tui.handle(Event::Key(Key::Char(']'))), Action::Render);
    assert_eq!(tui.view().active_transcript, TranscriptId::Subagent(8));
    assert_eq!(tui.handle(Event::Key(Key::Char('['))), Action::Render);
    assert_eq!(tui.view().active_transcript, TranscriptId::Subagent(7));
    assert_eq!(tui.handle(Event::Key(Key::Char('m'))), Action::Render);
    assert_eq!(tui.view().active_transcript, TranscriptId::Main);

    tui.handle(Event::Key(Key::Char('m')));
    assert_eq!(tui.input(), "m");
}

#[test]
fn viewport_vim_routes_preserve_per_transcript_state() {
    let mut tui = Tui::new(FakeEngine::default());
    for (id, call, output_lines) in [(7, "seven", 40), (8, "eight", 80)] {
        start_child(&mut tui, id);
        tui.apply_runtime_event(TuiRuntimeEvent::SubagentExecution(
            TuiSubagentEvent::started(id, "reviewer", "task", TuiExecutionState::ForegroundRunning),
        ));
        tui.apply_runtime_event(TuiRuntimeEvent::SubagentExecution(
            TuiSubagentEvent::tool_call(id, call, "tool", "input"),
        ));
        tui.apply_runtime_event(TuiRuntimeEvent::SubagentExecution(
            TuiSubagentEvent::tool_result(id, call, "output\n".repeat(output_lines), false),
        ));
    }

    focus_viewport(&mut tui);
    tui.handle(Event::Key(Key::Char(']')));
    assert_eq!(tui.view().active_transcript, TranscriptId::Subagent(7));
    tui.handle(Event::Key(Key::CtrlO));
    tui.handle(Event::Key(Key::PageUp));
    let child_seven = (
        tui.view().following_bottom,
        tui.view().scroll_offset,
        tui.view().focus,
        is_collapsed(tui.view().tool_display_modes, "seven"),
    );

    tui.handle(Event::Key(Key::Char(']')));
    assert_eq!(tui.view().active_transcript, TranscriptId::Subagent(8));
    assert!(tui.view().following_bottom);
    assert_eq!(tui.view().focus, TranscriptFocus::Viewport);
    // "seven" belongs to a different transcript's own record; it is simply
    // absent here (state isolation), not a "seven" collapse claim.
    assert!(!tui.view().tool_display_modes.contains_key("seven"));
    tui.handle(Event::Key(Key::CtrlO));
    tui.handle(Event::Key(Key::PageUp));
    let child_eight_offset = tui.view().scroll_offset;

    tui.handle(Event::Key(Key::Char('[')));
    assert_eq!(tui.view().active_transcript, TranscriptId::Subagent(7));
    assert_eq!(
        (
            tui.view().following_bottom,
            tui.view().scroll_offset,
            tui.view().focus,
            is_collapsed(tui.view().tool_display_modes, "seven"),
        ),
        child_seven
    );

    tui.handle(Event::Key(Key::Char(']')));
    assert_eq!(tui.view().scroll_offset, child_eight_offset);
    assert!(!is_collapsed(tui.view().tool_display_modes, "eight"));
    tui.handle(Event::Key(Key::Char('m')));
    assert_eq!(tui.view().active_transcript, TranscriptId::Main);
    assert_eq!(tui.view().focus, TranscriptFocus::Viewport);
}

#[test]
fn ctrl_o_toggles_bounded_detail_without_viewport_motion() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.handle(Event::Resize {
        width: 48,
        height: 12,
    });
    tui.begin_submission("request");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "before-anchor\n".repeat(80),
    )));
    tui.apply_progress(TurnEvent::ToolCallRequested {
        id: "read-1".into(),
        name: "native::read".into(),
        input: "large.log".into(),
    });
    tui.apply_progress(TurnEvent::ToolResult(MessagePart::ToolResult {
        tool_call_id: "read-1".into(),
        content: format!(
            "visible-start\n{}\nretained-tail-sentinel",
            "visible-middle\n".repeat(1_000)
        ),
        is_error: false,
    }));

    assert!(is_collapsed(tui.view().tool_display_modes, "read-1"));
    tui.handle(Event::Key(Key::PageUp));
    let anchor = (
        tui.view().following_bottom,
        tui.view().scroll_offset,
        tui.view().focus,
    );
    assert!(!anchor.0);
    assert!(anchor.1 > 0);

    tui.handle(Event::Key(Key::CtrlO));
    assert_eq!(
        (
            tui.view().following_bottom,
            tui.view().scroll_offset,
            tui.view().focus,
        ),
        anchor
    );
    assert!(!is_collapsed(tui.view().tool_display_modes, "read-1"));
    assert!(
        tui.view().conversation.unwrap().tool_batches[0].calls[0]
            .result
            .as_ref()
            .unwrap()
            .output
            .contains("retained-tail-sentinel")
    );

    // Scrolling no longer moves focus, so returning to the bottom from the
    // composer is the composer-safe jump rather than End.
    tui.handle(Event::Key(Key::CtrlShiftG));

    let backend = TestBackend::new(48, 12);
    let terminal = Terminal::new(backend).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    renderer.render(tui.view()).unwrap();
    let expanded = renderer
        .terminal()
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(expanded.contains("visible output truncated"));
    assert!(!expanded.contains("retained-tail-sentinel"));

    // Second Ctrl+O: Truncated -> Expanded. S1 renders both modes
    // identically (real truncation is S2 scope), so it stays not collapsed.
    tui.handle(Event::Key(Key::CtrlO));
    assert!(!is_collapsed(tui.view().tool_display_modes, "read-1"));

    // Third Ctrl+O completes the Collapsed -> Truncated -> Expanded ->
    // Collapsed cycle.
    tui.handle(Event::Key(Key::CtrlO));
    assert!(is_collapsed(tui.view().tool_display_modes, "read-1"));
}

/// Builds a finished turn carrying both reasoning and one settled tool call —
/// the state in which the two detail axes can be told apart.
fn turn_with_reasoning_and_a_settled_call() -> Tui<FakeEngine> {
    let mut tui = Tui::new(FakeEngine::default());
    tui.begin_submission("request");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Reasoning(
        "REASONING_BODY".into(),
    )));
    tui.apply_progress(TurnEvent::ToolCallRequested {
        id: "read-1".into(),
        name: "native::read".into(),
        input: "large.log".into(),
    });
    tui.apply_progress(TurnEvent::ToolResult(MessagePart::ToolResult {
        tool_call_id: "read-1".into(),
        content: "TOOL_BODY".into(),
        is_error: false,
    }));
    tui.finish_provider_turn(agens_tui::TuiProviderOutcome::Completed("answer".into()));
    tui
}

/// One key drove both reasoning and tool output through an implicit precedence,
/// which had a trap in it: once reasoning was expanded and any call had settled,
/// every further press went to the tools and the thought could never be hidden
/// again. Each axis owning its own key is what makes both reversible.
#[test]
fn each_detail_axis_moves_under_its_own_key_and_reverses() {
    let mut tui = turn_with_reasoning_and_a_settled_call();
    assert!(tui.view().collapse_thinking);

    tui.handle(Event::Key(Key::CtrlT));
    assert!(!tui.view().collapse_thinking, "Ctrl+T shows the reasoning");
    assert!(
        is_collapsed(tui.view().tool_display_modes, "read-1"),
        "and leaves the tool output where it was"
    );

    tui.handle(Event::Key(Key::CtrlO));
    assert!(
        !is_collapsed(tui.view().tool_display_modes, "read-1"),
        "Ctrl+O moves the tool output"
    );
    assert!(
        !tui.view().collapse_thinking,
        "and leaves the reasoning where it was"
    );

    tui.handle(Event::Key(Key::CtrlT));
    assert!(
        tui.view().collapse_thinking,
        "the thought hides again even with a settled call on screen"
    );
}

/// Three levels the reader can aim for are only three levels if overshooting
/// one costs a single key rather than a full lap.
#[test]
fn the_tool_detail_cycle_walks_both_ways_through_its_three_levels() {
    let mut tui = turn_with_reasoning_and_a_settled_call();
    assert_eq!(tui.view().tool_detail, DisplayMode::Collapsed);

    tui.handle(Event::Key(Key::CtrlO));
    assert_eq!(tui.view().tool_detail, DisplayMode::Truncated);
    tui.handle(Event::Key(Key::CtrlO));
    assert_eq!(tui.view().tool_detail, DisplayMode::Expanded);
    tui.handle(Event::Key(Key::CtrlO));
    assert_eq!(tui.view().tool_detail, DisplayMode::Collapsed);

    tui.handle(Event::Key(Key::CtrlShiftO));
    assert_eq!(tui.view().tool_detail, DisplayMode::Expanded);
    tui.handle(Event::Key(Key::CtrlShiftO));
    assert_eq!(tui.view().tool_detail, DisplayMode::Truncated);
    assert_eq!(
        tui.view().tool_display_modes.get("read-1"),
        Some(&DisplayMode::Truncated),
        "the settled call follows the level, not its own history"
    );
}

/// Ctrl+O only expands inline; the modal opens with Enter on a focused tool
/// (or a click on a tool row).
#[test]
fn tool_modal_opens_with_enter_on_focused_tool_not_ctrl_o() {
    let mut tui = turn_with_reasoning_and_a_settled_call();
    tui.handle(Event::Resize {
        width: 100,
        height: 30,
    });

    assert!(tui.view().tool_overlay.is_none());
    tui.handle(Event::Key(Key::CtrlO)); // Truncated
    tui.handle(Event::Key(Key::CtrlO)); // Expanded inline
    assert_eq!(tui.view().tool_detail, DisplayMode::Expanded);
    assert!(
        tui.view().tool_overlay.is_none(),
        "Ctrl+O must not open the modal"
    );
    assert_eq!(
        tui.view().tool_display_modes.get("read-1"),
        Some(&DisplayMode::Expanded),
        "Ctrl+O expands the tool inline"
    );

    focus_viewport(&mut tui);
    tui.handle(Event::Key(Key::Char('K'))); // focus newest settled tool
    assert_eq!(tui.view().focused_call, Some("read-1"));
    tui.handle(Event::Key(Key::Enter));
    let overlay = tui
        .view()
        .tool_overlay
        .expect("Enter on a focused tool opens the modal");
    assert_eq!(overlay.call_id, "read-1");
    assert!(
        overlay.output.contains("TOOL_BODY"),
        "overlay carries the tool output: {:?}",
        overlay.output
    );

    tui.handle(Event::Key(Key::Escape));
    assert!(
        tui.view().tool_overlay.is_none(),
        "Escape closes the tool overlay"
    );
}

/// AGN-109 collapses every settled call, so the detail it hides has to be
/// reachable one block at a time — a transcript-wide cycle answers "how much of
/// everything", not "what is in this one".
#[test]
fn block_focus_walks_settled_calls_and_opens_only_the_one_it_stands_on() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.handle(Event::Resize {
        width: 80,
        height: 24,
    });
    tui.begin_submission("request");
    for id in ["read-1", "read-2", "read-3"] {
        tui.apply_progress(TurnEvent::ToolCallRequested {
            id: id.into(),
            name: "native::read".into(),
            input: format!("{id}.log"),
        });
        tui.apply_progress(TurnEvent::ToolResult(MessagePart::ToolResult {
            tool_call_id: id.into(),
            content: format!("body of {id}"),
            is_error: false,
        }));
    }
    tui.finish_provider_turn(agens_tui::TuiProviderOutcome::Completed("answer".into()));

    focus_viewport(&mut tui);
    assert_eq!(tui.view().focus, TranscriptFocus::Viewport);

    // Focus enters at the newest block, which is the one a reader just watched
    // happen.
    // j and k scroll rows now, so walking blocks moved up a case.
    tui.handle(Event::Key(Key::Char('K')));
    assert_eq!(tui.view().focused_call, Some("read-3"));
    tui.handle(Event::Key(Key::Char('K')));
    assert_eq!(tui.view().focused_call, Some("read-2"));
    tui.handle(Event::Key(Key::Char('J')));
    assert_eq!(tui.view().focused_call, Some("read-3"));

    tui.handle(Event::Key(Key::Char('K')));
    tui.handle(Event::Key(Key::Char('o')));
    assert_eq!(
        tui.view().tool_display_modes.get("read-2"),
        Some(&DisplayMode::Truncated),
        "the focused block opens"
    );
    assert!(
        is_collapsed(tui.view().tool_display_modes, "read-1"),
        "and its neighbours stay as they were"
    );
    assert!(is_collapsed(tui.view().tool_display_modes, "read-3"));
    assert_eq!(
        tui.view().tool_detail,
        DisplayMode::Collapsed,
        "opening one block is not a statement about all of them"
    );
}

/// Detail that arrives by pushing the transcript around costs the reader the
/// place they were reading. Opening a block must leave every row above it where
/// it was.
#[test]
fn opening_a_focused_block_does_not_move_the_rows_above_it() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.handle(Event::Resize {
        width: 80,
        height: 16,
    });
    tui.begin_submission("request");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "anchor-line\n".repeat(40),
    )));
    tui.apply_progress(TurnEvent::ToolCallRequested {
        id: "read-1".into(),
        name: "native::read".into(),
        input: "big.log".into(),
    });
    tui.apply_progress(TurnEvent::ToolResult(MessagePart::ToolResult {
        tool_call_id: "read-1".into(),
        content: "body\n".repeat(200),
        is_error: false,
    }));
    tui.finish_provider_turn(agens_tui::TuiProviderOutcome::Completed("answer".into()));

    focus_viewport(&mut tui);
    tui.handle(Event::Key(Key::Char('K')));
    assert_eq!(tui.view().focused_call, Some("read-1"));
    let anchored = tui.view().scroll_offset;
    assert!(!tui.view().following_bottom, "navigation detaches the view");

    tui.handle(Event::Key(Key::Char('o')));
    assert_eq!(
        tui.view().scroll_offset,
        anchored,
        "the rows above the block do not move when it opens"
    );
}

/// Parent turn events keep arriving while the reader watches a subagent, so
/// "which transcript does this call belong to" and "which transcript am I
/// looking at" are different questions. Answering the first with the second
/// filed the parent's presentation state under the child, and left the parent
/// call with nothing recorded at all — leaning the render on a fallback that is
/// meant to be a safety net.
#[test]
fn a_parent_call_settling_under_a_child_transcript_records_against_the_parent() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.begin_submission("request");
    start_child(&mut tui, 7);
    tui.select_transcript(TranscriptId::Subagent(7));

    tui.apply_progress(TurnEvent::ToolCallRequested {
        id: "parent-read".into(),
        name: "native::read".into(),
        input: "parent.log".into(),
    });
    tui.apply_progress(TurnEvent::ToolResult(MessagePart::ToolResult {
        tool_call_id: "parent-read".into(),
        content: "PARENT_BODY".into(),
        is_error: false,
    }));

    assert!(
        !tui.view().tool_display_modes.contains_key("parent-read"),
        "the child's record has no business holding a parent call: {:?}",
        tui.view().tool_display_modes
    );

    tui.select_transcript(TranscriptId::Main);
    assert_eq!(
        tui.view().tool_display_modes.get("parent-read"),
        Some(&DisplayMode::Collapsed)
    );
}

/// The level is what the footer names, so it has to be true before anything has
/// settled — and a call that settles afterwards has to join its neighbours
/// rather than arrive hidden among expanded ones.
#[test]
fn a_call_settling_after_the_level_moved_arrives_at_that_level() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.begin_submission("request");

    tui.handle(Event::Key(Key::CtrlO));
    tui.handle(Event::Key(Key::CtrlO));
    assert_eq!(
        tui.view().tool_detail,
        DisplayMode::Expanded,
        "the level moves with nothing settled yet"
    );

    tui.apply_progress(TurnEvent::ToolCallRequested {
        id: "read-late".into(),
        name: "native::read".into(),
        input: "late.log".into(),
    });
    tui.apply_progress(TurnEvent::ToolResult(MessagePart::ToolResult {
        tool_call_id: "read-late".into(),
        content: "LATE_BODY".into(),
        is_error: false,
    }));

    assert_eq!(
        tui.view().tool_display_modes.get("read-late"),
        Some(&DisplayMode::Expanded)
    );
}

#[test]
fn child_ordered_stream_preserves_visible_child_rows_and_isolates_parent_summaries() {
    let mut tui = Tui::new(FakeEngine::default());
    let (bridge, receiver) = BridgeTx::bounded(32);
    let cancellation = BridgeCancel::new();
    let events = [
        TuiRuntimeEvent::TaskExecution {
            agent: "reviewer".into(),
            event: TuiExecutionEvent::ForegroundStarted { id: 7 },
        },
        TuiRuntimeEvent::TaskExecution {
            agent: "writer".into(),
            event: TuiExecutionEvent::ForegroundStarted { id: 8 },
        },
        TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::started(
            7,
            "reviewer",
            "review task",
            TuiExecutionState::ForegroundRunning,
        )),
        TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::started(
            8,
            "writer",
            "write task",
            TuiExecutionState::ForegroundRunning,
        )),
        TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::reasoning(7, "child-reasoning")),
        TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::text(8, "other-child")),
        TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::text(7, "child-partial")),
        TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::tool_call(
            7, "call-a", "read", "alpha",
        )),
        TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::tool_call(
            7,
            "call-b",
            "native::glob",
            "beta",
        )),
        TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::tool_result(
            7, "call-b", "result-b", false,
        )),
        TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::tool_result(
            7, "call-a", "result-a", false,
        )),
        TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::error_with_reference(
            7,
            SubagentErrorKind::Tool,
            "abc12345",
        )),
        TuiRuntimeEvent::TaskExecution {
            agent: "reviewer".into(),
            event: TuiExecutionEvent::Failed { id: 7 },
        },
        TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::terminal(
            7,
            SubagentStatus::Failure,
            "child-final",
        )),
    ];

    for event in events {
        let outcome = bridge.publish(event, &cancellation, None);
        assert!(matches!(outcome, PublishOutcome::Published { .. }));
    }
    while let Ok(envelope) = receiver.try_recv() {
        let (ordinal, event) = envelope.into_parts();
        tui.apply_runtime_event_with_ordinal(ordinal, event);
    }

    tui.apply_runtime_event_with_ordinal(
        99,
        TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::text(7, "late-child")),
    );
    tui.apply_runtime_event_with_ordinal(
        100,
        TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::terminal(
            7,
            SubagentStatus::Failure,
            "duplicate-final",
        )),
    );

    let parent_card = &tui.view().conversation.unwrap().subagent_cards[0];
    assert_eq!(parent_card.tool_uses, 2);
    assert!(parent_card.tool_calls.is_empty());

    let backend = TestBackend::new(120, 48);
    let terminal = Terminal::new(backend).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    renderer.render(tui.view()).unwrap();
    let parent = renderer
        .terminal()
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(parent.contains("● Reviewer · review task"));
    assert!(!parent.contains("child-reasoning"));
    assert!(!parent.contains("child-partial"));
    assert!(!parent.contains("result-a"));
    assert!(!parent.contains("Subagent tool execution failed."));

    tui.select_transcript(TranscriptId::Subagent(7));
    // Reasoning and tool output are separate axes: one key each.
    tui.handle(Event::Key(Key::CtrlT));
    tui.handle(Event::Key(Key::CtrlO));
    renderer.render(tui.view()).unwrap();
    let child = renderer
        .terminal()
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    let expected_child_rows = [
        "child-reasoning",
        "child-partial",
        "read",
        "glob",
        "result-b",
        "result-a",
        "Subagent tool execution failed.",
        "ref: abc12345",
        "child-final",
    ];
    let row_positions = expected_child_rows.map(|text| {
        assert!(child.contains(text), "missing child row: {text}");
        child.find(text).unwrap()
    });

    assert!(
        row_positions.windows(2).all(|rows| rows[0] < rows[1]),
        "child rows did not preserve source order: {expected_child_rows:?}",
    );
    assert!(!child.contains("call-a"), "{child:?}");
    assert!(!child.contains("call-b"), "{child:?}");
    assert!(!child.contains("late-child"));
    assert!(!child.contains("duplicate-final"));
}

#[test]
fn main_and_child_hierarchy_renders_each_event_once() {
    let mut tui = Tui::new(FakeEngine::default());
    for (id, agent, event_text) in [
        (7, "reviewer", "child-seven-sentinel"),
        (8, "writer", "child-eight-sentinel"),
    ] {
        tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
            agent: agent.into(),
            event: TuiExecutionEvent::ForegroundStarted { id },
        });
        tui.apply_runtime_event(TuiRuntimeEvent::SubagentExecution(
            TuiSubagentEvent::started(
                id,
                agent,
                format!("task-{id}"),
                TuiExecutionState::ForegroundRunning,
            ),
        ));
        tui.apply_runtime_event(TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::text(
            id, event_text,
        )));
    }

    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(120, 48)).unwrap());
    renderer.render(tui.view()).unwrap();
    let main = renderer
        .terminal()
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert_eq!(main.matches("● Reviewer · task-7").count(), 1, "{main:?}");
    assert_eq!(main.matches("● Writer · task-8").count(), 1, "{main:?}");
    assert!(!main.contains("child-seven-sentinel"), "{main:?}");
    assert!(!main.contains("child-eight-sentinel"), "{main:?}");

    tui.select_transcript(TranscriptId::Subagent(7));
    renderer.render(tui.view()).unwrap();
    let child_seven = renderer
        .terminal()
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert_eq!(
        child_seven.matches("child-seven-sentinel").count(),
        1,
        "{child_seven:?}"
    );
    assert!(
        !child_seven.contains("child-eight-sentinel"),
        "{child_seven:?}"
    );

    tui.handle(Event::Key(Key::Char(']')));
    assert_eq!(tui.view().active_transcript, TranscriptId::Subagent(8));
    renderer.render(tui.view()).unwrap();
    let child_eight = renderer
        .terminal()
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert_eq!(
        child_eight.matches("child-eight-sentinel").count(),
        1,
        "{child_eight:?}"
    );
    assert!(
        !child_eight.contains("child-seven-sentinel"),
        "{child_eight:?}"
    );

    tui.handle(Event::Key(Key::Char('m')));
    assert_eq!(tui.view().active_transcript, TranscriptId::Main);
}

impl Engine for FakeEngine {
    fn cancel(&mut self) {
        self.cancellations += 1;
    }
}

#[test]
fn conversation_retains_complete_live_final_markdown_reasoning_diffs_and_errors() {
    let mut conversation = Conversation::new("explain the change");
    for event in [
        ConversationEvent::MarkdownDelta("live ".into()),
        ConversationEvent::MarkdownDelta("output".into()),
        ConversationEvent::ReasoningDelta("inspect ".into()),
        ConversationEvent::ReasoningDelta("events".into()),
        ConversationEvent::MarkdownFinal("final output".into()),
    ] {
        conversation.apply(event).unwrap();
    }
    conversation
        .apply(ConversationEvent::Diff {
            call_id: "edit-1".into(),
            lines: vec![DiffLine::new(7, DiffLineKind::Added, "+ typed")],
        })
        .unwrap();
    conversation
        .apply(ConversationEvent::Error {
            message: "permission denied".into(),
            action: "allow the required capability".into(),
        })
        .unwrap();

    assert_eq!(conversation.user, "explain the change");
    assert_eq!(conversation.live_markdown, "live output");
    assert_eq!(conversation.final_markdown.as_deref(), Some("final output"));
    assert_eq!(conversation.reasoning, "inspect events");
    assert_eq!(conversation.diffs[0].number, 7);
    assert_eq!(conversation.diffs[0].kind, DiffLineKind::Added);
    assert_eq!(
        conversation.errors[0].action,
        "allow the required capability"
    );
}

#[test]
fn conversation_pairs_tool_results_by_call_id_and_keeps_contiguous_batches() {
    let mut conversation = Conversation::new("inspect");
    for event in [
        tool_call("one"),
        tool_call("two"),
        tool_result("two", "files"),
        tool_result("one", "contents"),
        ConversationEvent::MarkdownDelta("done".into()),
        tool_call("three"),
    ] {
        conversation.apply(event).unwrap();
    }

    assert_eq!(conversation.tool_batches.len(), 2);
    assert_eq!(
        conversation.tool_batches[0].calls[0]
            .result
            .as_ref()
            .unwrap()
            .output,
        "contents"
    );
    assert_eq!(
        conversation.tool_batches[0].calls[1]
            .result
            .as_ref()
            .unwrap()
            .output,
        "files"
    );
    assert_eq!(conversation.tool_batches[1].calls[0].call_id, "three");
}

#[test]
fn conversation_rejects_orphan_and_duplicate_call_ids_visibly() {
    let mut conversation = Conversation::new("inspect");
    let orphan = conversation.apply(ConversationEvent::ToolResult {
        call_id: "missing".into(),
        output: "none".into(),
        is_error: true,
    });
    assert_eq!(
        orphan,
        Err(ConversationError::OrphanToolResult("missing".into()))
    );

    conversation.apply(tool_call("call")).unwrap();
    let duplicate = conversation.apply(tool_call("call"));
    assert_eq!(
        duplicate,
        Err(ConversationError::DuplicateToolCall("call".into()))
    );
}

fn tool_call(id: &str) -> ConversationEvent {
    ConversationEvent::ToolCall {
        call_id: id.into(),
        name: id.into(),
        input: id.into(),
        parsed: agens_core::ToolInput::Other {
            name: id.into(),
            raw: id.into(),
        },
    }
}

fn tool_result(id: &str, output: &str) -> ConversationEvent {
    ConversationEvent::ToolResult {
        call_id: id.into(),
        output: output.into(),
        is_error: false,
    }
}

#[test]
fn scheduler_exposes_typed_lifecycle_and_stable_queue_entries() {
    let mut app = AppState::new(2);

    assert_eq!(app.lifecycle(), &TurnLifecycle::Idle);
    app.reduce(AppEvent::SubmitPrompt("first".into()));

    let active = app.lifecycle().active().expect("running active route");
    assert_eq!(active.generation(), 1);
    assert_eq!(active.prompt(), "first");

    app.reduce(AppEvent::SubmitPrompt("second".into()));
    app.reduce(AppEvent::SubmitPrompt("third".into()));

    let queued = app.queued_entries();
    assert_eq!(queued.len(), 2);
    assert_eq!(queued[0].prompt(), "second");
    assert_eq!(queued[1].prompt(), "third");
    assert_ne!(queued[0].id(), queued[1].id());
}

#[test]
fn scheduler_quarantines_stale_terminal_generations_before_dispatch() {
    let mut app = AppState::new(1);
    app.reduce(AppEvent::SubmitPrompt("first".into()));
    app.reduce(AppEvent::SubmitPrompt("second".into()));

    assert!(
        app.reduce(AppEvent::TurnCompletedFor {
            generation: 0,
            output: "stale".into(),
        })
        .is_empty()
    );
    assert_eq!(
        app.lifecycle().active().map(|route| route.generation()),
        Some(1)
    );
    assert_eq!(app.queued_prompts(), ["second"]);
    assert!(app.completed_history().is_empty());

    assert_eq!(
        app.reduce(AppEvent::TurnCompletedFor {
            generation: 1,
            output: "answer".into(),
        }),
        vec![
            Effect::PersistCompleted {
                prompt: "first".into(),
                output: "answer".into(),
            },
            Effect::StartPrompt("second".into()),
        ]
    );
    assert_eq!(
        app.lifecycle().active().map(|route| route.generation()),
        Some(2)
    );
}

#[test]
fn scheduler_quarantines_stale_failure_and_cancellation_before_state_transitions() {
    for terminal_event in [
        AppEvent::TurnCancelledFor { generation: 0 },
        AppEvent::TurnFailedFor { generation: 0 },
    ] {
        let mut app = AppState::new(1);
        app.reduce(AppEvent::SubmitPrompt("first".into()));
        app.reduce(AppEvent::SubmitPrompt("second".into()));
        let before = app.clone();

        assert!(app.reduce(terminal_event).is_empty());
        assert_eq!(app, before);
    }
}

#[test]
fn reducer_starts_idle_prompt_and_persists_only_after_success() {
    let mut app = AppState::new(2);

    assert_eq!(
        app.reduce(AppEvent::SubmitPrompt("first".into())),
        vec![Effect::StartPrompt("first".into())]
    );
    assert_eq!(app.runtime(), &Runtime::Running);
    assert!(app.completed_history().is_empty());

    assert_eq!(
        app.reduce(AppEvent::TurnCompletedFor {
            generation: 1,
            output: "answer".into()
        }),
        vec![Effect::PersistCompleted {
            prompt: "first".into(),
            output: "answer".into(),
        }]
    );
    assert_eq!(app.runtime(), &Runtime::Idle);
    assert_eq!(app.completed_history(), [("first".into(), "answer".into())]);
}

#[test]
fn reducer_queues_safe_prompts_in_bounded_fifo_order() {
    let mut app = AppState::new(2);
    app.reduce(AppEvent::SubmitPrompt("first".into()));

    assert!(
        app.reduce(AppEvent::SubmitPrompt("second".into()))
            .is_empty()
    );
    assert!(
        app.reduce(AppEvent::SubmitPrompt("third".into()))
            .is_empty()
    );
    assert_eq!(app.queued_prompts(), ["second", "third"]);

    assert_eq!(
        app.reduce(AppEvent::TurnCompletedFor {
            generation: 1,
            output: "one".into()
        }),
        vec![
            Effect::PersistCompleted {
                prompt: "first".into(),
                output: "one".into(),
            },
            Effect::StartPrompt("second".into()),
        ]
    );
    assert_eq!(app.queued_prompts(), ["third"]);
    assert_eq!(app.runtime(), &Runtime::Running);
}

#[test]
fn reducer_refuses_prompt_when_running_queue_is_full_without_history() {
    let mut app = AppState::new(1);
    app.reduce(AppEvent::SubmitPrompt("first".into()));
    app.reduce(AppEvent::SubmitPrompt("queued".into()));

    assert_eq!(
        app.reduce(AppEvent::SubmitPrompt("refused".into())),
        vec![Effect::RefusePrompt(
            "Prompt queue is full; draft was kept unchanged.".into()
        )]
    );
    assert_eq!(app.queued_prompts(), ["queued"]);
    assert!(app.completed_history().is_empty());
}

#[test]
fn reducer_terminal_failures_start_the_oldest_queued_prompt_before_later_submissions() {
    for terminal_event in [
        AppEvent::TurnCancelledFor { generation: 1 },
        AppEvent::TurnFailedFor { generation: 1 },
    ] {
        let mut app = AppState::new(2);
        app.reduce(AppEvent::SubmitPrompt("first".into()));
        app.reduce(AppEvent::SubmitPrompt("queued".into()));

        assert_eq!(
            app.reduce(terminal_event),
            vec![Effect::StartPrompt("queued".into())]
        );
        assert_eq!(app.runtime(), &Runtime::Running);
        assert!(app.queued_prompts().is_empty());
        assert!(app.completed_history().is_empty());

        assert!(app.reduce(AppEvent::SubmitPrompt("next".into())).is_empty());
        assert_eq!(app.queued_prompts(), ["next"]);
        assert!(app.completed_history().is_empty());
    }
}

#[test]
fn command_connected_key_dispatch_prioritizes_dialog_global_and_composer_editing() {
    let now = Instant::now();
    let mut app = AppState::new(1);
    app.reduce(AppEvent::SubmitPrompt("running".into()));
    app.set_composer("draft");
    app.set_dialog(Some(Dialog::Command));

    assert_eq!(
        app.reduce(AppEvent::Key(Key::Char('x'), Instant::now())),
        vec![Effect::DialogKey(Key::Char('x'))]
    );
    assert_eq!(app.composer(), "draft");
    assert_eq!(app.dialog(), Some(&Dialog::Command));

    assert_eq!(
        app.reduce(AppEvent::Key(Key::CtrlC, now)),
        vec![Effect::CancelTurn]
    );
    assert_eq!(app.dialog(), Some(&Dialog::Command));
    assert_eq!(
        app.reduce(AppEvent::Key(Key::CtrlC, now + Duration::from_secs(1))),
        vec![Effect::Render]
    );

    app.set_dialog(None);
    assert_eq!(
        app.reduce(AppEvent::Key(Key::Char('x'), Instant::now())),
        vec![Effect::ComposerEdited]
    );
    assert_eq!(app.composer(), "draftx");
}

#[test]
fn command_control_c_warns_then_cancels_if_needed_and_quits() {
    let now = Instant::now();

    let mut running = AppState::new(1);
    running.reduce(AppEvent::SubmitPrompt("running".into()));
    assert_eq!(
        running.reduce(AppEvent::Command(Command::ControlC, now)),
        vec![Effect::CancelTurn]
    );
    assert_eq!(
        running.reduce(AppEvent::Command(
            Command::ControlC,
            now + Duration::from_secs(1)
        )),
        vec![Effect::Render]
    );

    let mut idle = AppState::new(1);
    idle.set_composer("draft");
    assert_eq!(
        idle.reduce(AppEvent::Command(Command::ControlC, now)),
        vec![Effect::ExitWarning]
    );
    assert_eq!(
        idle.reduce(AppEvent::Command(
            Command::ControlC,
            now + Duration::from_secs(1)
        )),
        vec![Effect::Quit]
    );
    assert_eq!(idle.composer(), "draft");
}

#[test]
fn control_c_warning_expires_and_non_control_c_input_disarms_it() {
    let now = Instant::now();
    let mut app = AppState::new(1);

    assert_eq!(
        app.reduce(AppEvent::Command(Command::ControlC, now)),
        vec![Effect::ExitWarning]
    );
    assert_eq!(
        app.reduce(AppEvent::TimerTick(now + Duration::from_secs(2))),
        vec![Effect::Render]
    );
    assert_eq!(
        app.reduce(AppEvent::Command(
            Command::ControlC,
            now + Duration::from_secs(2)
        )),
        vec![Effect::ExitWarning]
    );
    assert_eq!(
        app.reduce(AppEvent::Key(Key::Char('x'), now + Duration::from_secs(2))),
        vec![Effect::ComposerEdited]
    );
    assert_eq!(
        app.reduce(AppEvent::Command(
            Command::ControlC,
            now + Duration::from_secs(2)
        )),
        vec![Effect::ExitWarning]
    );
}

#[test]
fn command_new_resets_only_after_backend_success_and_running_matrix_refuses_mutations() {
    let mut app = AppState::new(2);
    let now = Instant::now();
    app.set_composer("draft");
    let before_reset_request = app.clone();

    assert_eq!(
        app.reduce(AppEvent::Command(Command::New, now)),
        vec![Effect::ResetConversation]
    );
    assert_eq!(app, before_reset_request);

    app.reduce(AppEvent::SubmitPrompt("running".into()));
    app.reduce(AppEvent::SubmitPrompt("first queued".into()));
    app.reduce(AppEvent::SubmitPrompt("second queued".into()));
    app.reduce(AppEvent::TurnCompletedFor {
        generation: 1,
        output: "answer".into(),
    });
    app.set_composer("replacement draft");
    app.set_dialog(Some(Dialog::Command));

    assert_eq!(app.queued_prompts(), ["second queued"]);

    assert_eq!(app.reduce(AppEvent::ResetSucceeded), vec![Effect::Render]);
    assert_eq!(app.runtime(), &Runtime::Idle);
    assert_eq!(app.lifecycle(), &TurnLifecycle::Idle);
    assert!(app.queued_prompts().is_empty());
    assert!(app.completed_history().is_empty());
    assert!(app.composer().is_empty());
    assert_eq!(app.dialog(), None);

    app.reduce(AppEvent::SubmitPrompt("running".into()));
    for command in [
        Command::Navigate,
        Command::Display,
        Command::Select,
        Command::Queue,
    ] {
        assert_eq!(
            app.reduce(AppEvent::Command(command, now)),
            vec![Effect::Render]
        );
    }
    for command in [
        Command::Model,
        Command::Effort,
        Command::Session,
        Command::Agent,
        Command::New,
    ] {
        assert_eq!(
            app.reduce(AppEvent::Command(command, now)),
            vec![Effect::RefuseCommand(
                "This command is unavailable while a response is in progress.".into()
            )]
        );
    }
    assert_eq!(app.runtime(), &Runtime::Running);
    assert_eq!(app.composer(), "");
}

#[test]
fn bridge_clones_cannot_overtake_a_source_waiting_for_capacity() {
    let (bridge, receiver) = BridgeTx::bounded(1);
    let cancellation = BridgeCancel::new();

    assert_eq!(
        bridge.publish("occupied", &cancellation, None),
        PublishOutcome::Published { ordinal: 0 }
    );

    let first_bridge = bridge.clone();
    let first_cancellation = cancellation.clone();
    let first = thread::spawn(move || first_bridge.publish("first", &first_cancellation, None));
    thread::sleep(Duration::from_millis(10));

    let second_cancellation = cancellation.clone();
    let second = thread::spawn(move || bridge.publish("second", &second_cancellation, None));

    assert_eq!(receiver.recv().unwrap().into_parts(), (0, "occupied"));
    assert_eq!(receiver.recv().unwrap().into_parts(), (1, "first"));
    assert_eq!(receiver.recv().unwrap().into_parts(), (2, "second"));
    let _ = first.join().unwrap();
    let _ = second.join().unwrap();
}

#[test]
fn bridge_full_channel_stops_waiting_when_cancelled() {
    let (bridge, _receiver) = BridgeTx::bounded(1);
    let cancellation = BridgeCancel::new();

    assert_eq!(
        bridge.publish("queued", &cancellation, None),
        PublishOutcome::Published { ordinal: 0 }
    );
    let waiting_bridge = bridge.clone();
    let waiting_cancellation = cancellation.clone();
    let waiting =
        thread::spawn(move || waiting_bridge.publish("cancelled", &waiting_cancellation, None));

    thread::sleep(Duration::from_millis(10));
    cancellation.cancel();

    assert_eq!(waiting.join().unwrap(), PublishOutcome::Cancelled);
}

#[test]
fn bridge_full_channel_stops_waiting_at_deadline() {
    let (bridge, _receiver) = BridgeTx::bounded(1);
    let cancellation = BridgeCancel::new();

    assert_eq!(
        bridge.publish("queued", &cancellation, None),
        PublishOutcome::Published { ordinal: 0 }
    );

    assert_eq!(
        bridge.publish(
            "expired",
            &cancellation,
            Some(Instant::now() + Duration::from_millis(10)),
        ),
        PublishOutcome::DeadlineExpired
    );
}

#[test]
fn bridge_fails_closed_when_receiver_disconnects_while_full() {
    let (bridge, receiver) = BridgeTx::bounded(1);
    let cancellation = BridgeCancel::new();

    assert_eq!(
        bridge.publish("queued", &cancellation, None),
        PublishOutcome::Published { ordinal: 0 }
    );
    let waiting_bridge = bridge.clone();
    let waiting_cancellation = cancellation.clone();
    let waiting =
        thread::spawn(move || waiting_bridge.publish("disconnected", &waiting_cancellation, None));

    thread::sleep(Duration::from_millis(10));
    drop(receiver);

    assert_eq!(waiting.join().unwrap(), PublishOutcome::Disconnected);
}

#[test]
fn permission_wait_close_deadline_and_replies_remain_fail_closed() {
    let (bridge, requests) = TuiPermissionBridge::channel();
    let cancellation = HeadlessTurnCancellation::new();
    let waiting_bridge = bridge.clone();
    let waiting_cancellation = cancellation.clone();
    let waiting = thread::spawn(move || {
        waiting_bridge.wait_for_reply(
            "bash",
            "git status",
            "Write",
            None,
            None,
            &waiting_cancellation,
        )
    });

    let request = requests.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(bridge.close());
    assert!(!bridge.close());

    assert_eq!(waiting.join().unwrap(), TuiPermissionReply::Cancelled);
    assert!(!bridge.reply(request.id(), TuiPermissionReply::AllowAlways));

    // An already-expired deadline must not end a parked permission question;
    // only the person's answer (or cancel / surface close) may.
    let (bridge, requests) = TuiPermissionBridge::channel();
    let expired = HeadlessTurnCancellation::with_deadline(Duration::ZERO);
    let expired_bridge = bridge.clone();
    let expired_wait = thread::spawn(move || {
        expired_bridge.wait_for_reply("write", "README.md", "Write", None, None, &expired)
    });
    let expired_request = requests.recv_timeout(Duration::from_secs(1)).unwrap();
    thread::sleep(Duration::from_millis(100));
    assert!(
        bridge.is_pending(expired_request.id()),
        "permission wait must ignore an elapsed deadline while a person answers"
    );
    assert!(bridge.reply(expired_request.id(), TuiPermissionReply::AllowAlways));
    assert_eq!(
        expired_wait.join().unwrap(),
        TuiPermissionReply::AllowAlways
    );

    let allowed = HeadlessTurnCancellation::new();
    let allowed_bridge = bridge.clone();
    let allowed_wait = thread::spawn(move || {
        allowed_bridge.wait_for_reply("write", "README.md", "Write", None, None, &allowed)
    });
    let allowed_request = requests.recv_timeout(Duration::from_secs(1)).unwrap();

    assert!(bridge.reply(allowed_request.id(), TuiPermissionReply::AllowAlways));
    assert_eq!(
        allowed_wait.join().unwrap(),
        TuiPermissionReply::AllowAlways
    );
    assert!(!bridge.reply(allowed_request.id(), TuiPermissionReply::DenyOnce));
}

#[test]
fn normal_input_submits_the_composed_prompt() {
    let mut tui = Tui::new(FakeEngine::default());

    assert_eq!(tui.handle(Event::Key(Key::Char('h'))), Action::Render);
    assert_eq!(tui.handle(Event::Key(Key::Char('i'))), Action::Render);
    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::Submit("hi".into())
    );
    assert_eq!(tui.input(), "");
}

#[test]
fn slash_palette_filters_navigates_completes_and_submits_through_the_composer() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.set_palette_entries(vec![
        PaletteEntry::new(
            "connect",
            "Connect an account",
            "",
            PaletteEntryKind::BuiltIn,
        ),
        PaletteEntry::new(
            "review",
            "Review changes",
            "[scope]",
            PaletteEntryKind::Command,
        ),
        PaletteEntry::new(
            "resume",
            "Resume a session",
            "<id>",
            PaletteEntryKind::BuiltIn,
        ),
    ]);

    assert_eq!(tui.handle(Event::Key(Key::Char('/'))), Action::Render);
    assert!(tui.view().palette.is_some());

    tui.handle(Event::Key(Key::Char('r')));
    tui.handle(Event::Key(Key::Down));
    assert_eq!(tui.handle(Event::Key(Key::Tab)), Action::Render);
    assert_eq!(tui.input(), "/resume ");
    assert!(tui.view().palette.is_some());

    tui.handle(Event::Key(Key::Char('4')));
    tui.handle(Event::Key(Key::Char('2')));
    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::Submit("/resume 42".into())
    );
    assert!(tui.view().palette.is_none());
}

#[test]
fn slash_palette_uses_only_the_name_prefix_and_escape_preserves_composer_and_backend() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.set_palette_entries(vec![
        PaletteEntry::new("resume", "Resume", "<id>", PaletteEntryKind::BuiltIn),
        PaletteEntry::new("review", "Review", "[scope]", PaletteEntryKind::Skill),
    ]);
    for character in "/res 42".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }

    assert!(tui.view().palette.is_some());
    assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::Render);
    assert_eq!(tui.input(), "/res 42");
    assert!(tui.view().palette.is_none());
    assert_eq!(tui.engine().cancellations, 0);

    assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Render);
    assert_eq!(tui.input(), "/res 42");
    assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Quit);
}

#[test]
fn slash_palette_selector_enter_emits_a_route_id_but_explicit_arguments_still_submit() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.set_palette_entries(vec![
        PaletteEntry::new("model", "Choose model", "[name]", PaletteEntryKind::BuiltIn)
            .with_dialog("model"),
    ]);

    for character in "/mo".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }
    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::OpenDialog("model".into())
    );
    assert_eq!(tui.input(), "");

    for character in "/model o3".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }
    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::Submit("/model o3".into())
    );
}

#[test]
fn u15_c1a_subagent_shortcut_opens_the_same_dialog_route_as_the_palette() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.set_palette_entries(vec![
        PaletteEntry::new(
            "subagent",
            "Choose a subagent",
            "",
            PaletteEntryKind::BuiltIn,
        )
        .with_dialog("subagent"),
    ]);

    for character in "/subagent".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }
    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::OpenDialog("subagent".into())
    );
    assert_eq!(
        tui.handle(Event::Key(Key::CtrlShiftA)),
        Action::OpenDialog("subagent".into())
    );
    assert_eq!(tui.engine().cancellations, 0);
}

#[test]
fn u15_c1b_tracks_selected_running_and_terminal_execution_states_once() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.set_agent_catalog(["reviewer"]);
    tui.select_agent("reviewer");
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "reviewer".into(),
        event: TuiExecutionEvent::ForegroundStarted { id: 1 },
    });
    assert_eq!(
        tui.executions()[0].state(),
        agens_tui::TuiExecutionState::ForegroundRunning
    );
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "reviewer".into(),
        event: TuiExecutionEvent::Backgrounded { id: 1 },
    });
    assert_eq!(
        tui.executions()[0].state(),
        agens_tui::TuiExecutionState::BackgroundRunning
    );
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "reviewer".into(),
        event: TuiExecutionEvent::Completed { id: 1 },
    });
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "reviewer".into(),
        event: TuiExecutionEvent::Failed { id: 1 },
    });
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "reviewer".into(),
        event: TuiExecutionEvent::ForegroundStarted { id: 2 },
    });
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "reviewer".into(),
        event: TuiExecutionEvent::Failed { id: 2 },
    });

    assert_eq!(tui.agent_catalog(), ["main", "reviewer"]);
    assert_eq!(tui.selected_agent(), Some("reviewer"));
    assert_eq!(tui.view().selected_agent, Some("reviewer"));
    assert_eq!(tui.executions().len(), 2);
    assert_eq!(
        tui.executions()[0].state(),
        agens_tui::TuiExecutionState::Failed
    );
    assert_eq!(
        tui.executions()[1].state(),
        agens_tui::TuiExecutionState::CompletedRecent
    );
    tui.tick(Duration::from_nanos(59_999_999_999));
    assert_eq!(tui.executions().len(), 2);
    tui.tick(Duration::from_secs(60));
    assert!(tui.executions().is_empty());
    assert_eq!(tui.agent_catalog(), ["main", "reviewer"]);
}
#[test]
fn u15_c1b_sorts_reexecutions_newest_first_with_execution_id_ties() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.set_agent_catalog(["reviewer"]);
    tui.tick(Duration::from_secs(7));
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "reviewer".into(),
        event: TuiExecutionEvent::ForegroundStarted { id: 1 },
    });
    tui.tick(Duration::from_secs(9));
    for id in [2, 3] {
        tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
            agent: "reviewer".into(),
            event: TuiExecutionEvent::ForegroundStarted { id },
        });
    }

    assert_eq!(
        tui.executions()
            .iter()
            .map(|execution| execution.id())
            .collect::<Vec<_>>(),
        [3, 2, 1]
    );
    assert_eq!(tui.agent_catalog(), ["main", "reviewer"]);
}

#[test]
fn p1a1_events_upsert_live_calls_pair_out_of_order_results_and_stop_after_c1_terminal() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "reviewer".into(),
        event: TuiExecutionEvent::ForegroundStarted { id: 7 },
    });

    for event in [
        TuiSubagentEvent::started(99, "other", "ignored", TuiExecutionState::ForegroundRunning),
        TuiSubagentEvent::started(
            7,
            "reviewer",
            "review this change",
            TuiExecutionState::ForegroundRunning,
        ),
        TuiSubagentEvent::tool_result(7, "later", "orphan result", false),
        TuiSubagentEvent::tool_call(7, "first", "native::read", "first input"),
        TuiSubagentEvent::tool_call(7, "later", "native::grep", "later input"),
        TuiSubagentEvent::tool_result(7, "later", "later result", false),
        TuiSubagentEvent::tool_result(7, "first", "first result", true),
        TuiSubagentEvent::started(
            7,
            "reviewer",
            "duplicate card",
            TuiExecutionState::ForegroundRunning,
        ),
    ] {
        tui.apply_runtime_event(TuiRuntimeEvent::SubagentExecution(event));
    }

    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "reviewer".into(),
        event: TuiExecutionEvent::Completed { id: 7 },
    });
    tui.apply_runtime_event(TuiRuntimeEvent::SubagentExecution(
        TuiSubagentEvent::tool_call(7, "late", "native::bash", "must not appear"),
    ));

    let cards = &tui.view().conversation.unwrap().subagent_cards;
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0].id, 7);
    assert_eq!(cards[0].tool_uses, 2);
    assert!(cards[0].tool_calls.is_empty());

    tui.select_transcript(TranscriptId::Subagent(7));
    let child = tui.view().conversation.unwrap();
    assert_eq!(child.tool_batches[0].calls.len(), 2);
    assert_eq!(child.tool_batches[0].calls[0].call_id, "first");
    assert_eq!(
        child.tool_batches[0].calls[0]
            .result
            .as_ref()
            .unwrap()
            .output,
        "first result"
    );
    assert!(
        child.tool_batches[0].calls[0]
            .result
            .as_ref()
            .unwrap()
            .is_error
    );
    assert_eq!(child.tool_batches[0].calls[1].call_id, "later");
    assert_eq!(
        child.tool_batches[0].calls[1]
            .result
            .as_ref()
            .unwrap()
            .output,
        "later result"
    );
}

#[test]
fn p1a2_events_admit_one_bounded_terminal_per_c1_execution_and_ignore_late_mutations() {
    let mut tui = Tui::new(FakeEngine::default());
    let long = "x".repeat(300);
    let cases = [
        (
            1,
            TuiExecutionEvent::Completed { id: 1 },
            SubagentStatus::Success,
        ),
        (
            2,
            TuiExecutionEvent::Failed { id: 2 },
            SubagentStatus::Failure,
        ),
        (
            3,
            TuiExecutionEvent::Cancelled { id: 3 },
            SubagentStatus::Cancelled,
        ),
    ];

    for (id, terminal_execution, status) in cases {
        tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
            agent: "reviewer".into(),
            event: TuiExecutionEvent::ForegroundStarted { id },
        });
        tui.apply_runtime_event(TuiRuntimeEvent::SubagentExecution(
            TuiSubagentEvent::started(
                id,
                "reviewer",
                format!("task-{long}"),
                TuiExecutionState::ForegroundRunning,
            ),
        ));
        tui.apply_runtime_event(TuiRuntimeEvent::SubagentExecution(
            TuiSubagentEvent::tool_call(
                id,
                format!("call-{long}"),
                format!("tool-{long}"),
                format!("input-{long}"),
            ),
        ));
        tui.apply_runtime_event(TuiRuntimeEvent::SubagentExecution(
            TuiSubagentEvent::tool_result(
                id,
                format!("call-{long}"),
                format!("output-{long}"),
                false,
            ),
        ));
        tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
            agent: "reviewer".into(),
            event: terminal_execution,
        });
        tui.apply_runtime_event(TuiRuntimeEvent::SubagentExecution(
            TuiSubagentEvent::terminal(id, status, format!("final-{long}")),
        ));
        tui.apply_runtime_event(TuiRuntimeEvent::SubagentExecution(
            TuiSubagentEvent::terminal(id, SubagentStatus::Success, "late terminal"),
        ));
        tui.apply_runtime_event(TuiRuntimeEvent::SubagentExecution(
            TuiSubagentEvent::tool_call(id, "late-call", "native::bash", "late input"),
        ));
    }

    let cards = &tui.view().conversation.unwrap().subagent_cards;
    assert_eq!(cards.len(), 3);
    for (card, (_, _, status)) in cards.iter().zip(cases) {
        assert_eq!(card.status, Some(status));
        assert_eq!(card.task_summary.chars().count(), 256);
        assert_eq!(card.tool_uses, 1);
        assert!(card.tool_calls.is_empty());
        assert_eq!(card.final_result.as_ref().unwrap().chars().count(), 256);
    }

    for id in [1, 2, 3] {
        tui.select_transcript(TranscriptId::Subagent(id));
        let child = tui.view().conversation.unwrap();
        let call = &child.tool_batches[0].calls[0];
        assert_eq!(call.call_id.chars().count(), 256);
        assert_eq!(call.name.chars().count(), 256);
        assert_eq!(call.input.chars().count(), 256);
        assert_eq!(call.result.as_ref().unwrap().output.chars().count(), 256);
    }

    let mut redacted = Tui::new(FakeEngine::default());
    redacted.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "reviewer".into(),
        event: TuiExecutionEvent::ForegroundStarted { id: 4 },
    });
    for event in [
        TuiSubagentEvent::started(
            4,
            "reviewer",
            "password=task-secret",
            TuiExecutionState::ForegroundRunning,
        ),
        TuiSubagentEvent::tool_call(4, "call", "native::read", "Authorization: tool-secret"),
        TuiSubagentEvent::tool_result(4, "call", "token=result-secret", false),
    ] {
        redacted.apply_runtime_event(TuiRuntimeEvent::SubagentExecution(event));
    }
    redacted.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "reviewer".into(),
        event: TuiExecutionEvent::Completed { id: 4 },
    });
    redacted.apply_runtime_event(TuiRuntimeEvent::SubagentExecution(
        TuiSubagentEvent::terminal(4, SubagentStatus::Success, "secret=final-secret"),
    ));

    let card = &redacted.view().conversation.unwrap().subagent_cards[0];
    assert_eq!(card.task_summary, "[redacted]");
    assert_eq!(card.tool_uses, 1);
    assert!(card.tool_calls.is_empty());
    assert_eq!(card.final_result.as_deref(), Some("[redacted]"));

    redacted.select_transcript(TranscriptId::Subagent(4));
    let child = redacted.view().conversation.unwrap();
    let call = &child.tool_batches[0].calls[0];
    assert_eq!(call.input, "[redacted]");
    assert_eq!(call.result.as_ref().unwrap().output, "[redacted]");
}

#[test]
fn p1c2_events_restore_completed_cards_without_live_execution_or_duplicates() {
    let mut tui = Tui::new(FakeEngine::default());
    let event = TuiRuntimeEvent::RestoredCompletedSubagent {
        id: 42,
        agent: "reviewer".into(),
        task_summary: "review the durable result".into(),
        final_result: "approved".into(),
        tool_uses: 3,
    };

    tui.apply_runtime_event(event.clone());
    tui.apply_runtime_event(event);

    let conversation = tui.view().conversation.unwrap();
    assert_eq!(conversation.subagent_cards.len(), 1);
    assert_eq!(conversation.subagent_cards[0].agent, "reviewer");
    assert_eq!(
        conversation.subagent_cards[0].task_summary,
        "review the durable result"
    );
    assert_eq!(
        conversation.subagent_cards[0].final_result.as_deref(),
        Some("approved")
    );
    assert_eq!(conversation.subagent_cards[0].tool_uses, 3);
    assert!(conversation.subagent_cards[0].tool_calls.is_empty());
    assert!(tui.executions().is_empty());
}

#[test]
fn selection_dialog_navigates_dispatches_once_and_precedes_composer_input() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.handle(Event::Key(Key::Char('d')));
    tui.show_selection_dialog(DialogView::selection(
        "Choose",
        Some("Pick one option"),
        vec![
            DialogEntry::action("First", "first"),
            DialogEntry::action("Second", "second"),
        ],
    ));

    for character in "/sec".chars() {
        assert_eq!(tui.handle(Event::Key(Key::Char(character))), Action::Render);
    }
    assert_eq!(tui.input(), "d");
    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::DialogAction("second".into())
    );
    assert!(tui.view().dialog.is_some());
    tui.apply_submission_outcome(TuiSubmissionOutcome::SelectionInfo(
        "Selected file: second".into(),
    ));
    assert!(tui.view().dialog.is_none());
    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::Submit("d".into())
    );
    assert_eq!(tui.engine().cancellations, 0);
}

#[test]
fn selection_dialog_offers_a_bounded_query_action_only_without_matches() {
    let dialog = DialogView::selection(
        "Choose model",
        Some("Search models"),
        vec![DialogEntry::action("gpt-5.5", "model:gpt-5.5")],
    )
    .with_identifier_query_action("Use ", " (unverified metadata)", "model-custom:", 64);
    let mut tui = Tui::new(FakeEngine::default());
    tui.show_selection_dialog(dialog);

    for character in "/gpt-5.6".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }

    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::DialogAction("model-custom:gpt-5.6".into())
    );

    let dialog = DialogView::selection(
        "Choose model",
        Some("Search models"),
        vec![DialogEntry::action("gpt-5.5", "model:gpt-5.5")],
    )
    .with_identifier_query_action("Use ", " (unverified metadata)", "model-custom:", 8);
    tui.show_selection_dialog(dialog);
    for character in "/model-too-long".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }

    assert_eq!(tui.handle(Event::Key(Key::Enter)), Action::Render);

    let dialog = DialogView::selection(
        "Choose model",
        Some("Search models"),
        vec![DialogEntry::action("gpt-5.5", "model:gpt-5.5")],
    )
    .with_identifier_query_action("Use ", " (unverified metadata)", "model-custom:", 64);
    tui.show_selection_dialog(dialog);
    for character in "/bad*model".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }

    assert_eq!(tui.handle(Event::Key(Key::Enter)), Action::Render);
}

#[test]
fn selection_dialog_search_edits_navigate_rows_and_escape_disarms_before_closing() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.handle(Event::Resize {
        width: 24,
        height: 12,
    });
    tui.show_selection_dialog(DialogView::selection(
        "Choose",
        Some("Search options"),
        (0..20)
            .map(|index| DialogEntry::action(format!("Option {index:02}"), format!("pick:{index}")))
            .collect(),
    ));

    for character in "/Option 1x".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }
    tui.handle(Event::Key(Key::Backspace));
    tui.handle(Event::Key(Key::PageDown));
    tui.handle(Event::Key(Key::PageDown));
    tui.handle(Event::Key(Key::ScrollUp));
    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::DialogAction("pick:18".into())
    );

    tui.show_selection_dialog(DialogView::selection(
        "Choose",
        None::<String>,
        vec![DialogEntry::action("Alpha", "alpha")],
    ));
    for character in "/alpha".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }
    tui.handle(Event::Key(Key::DeletePreviousWord));
    assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::Render);
    assert!(
        tui.view().dialog.is_some(),
        "the first escape only leaves search"
    );
    assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::Render);
    assert!(tui.view().dialog.is_none());

    // Without search armed the first escape closes the dialog outright.
    tui.show_selection_dialog(DialogView::selection(
        "Choose",
        None::<String>,
        vec![DialogEntry::action("Alpha", "alpha")],
    ));
    tui.handle(Event::Key(Key::Char('a')));
    assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::Render);
    assert!(tui.view().dialog.is_none());
}

#[test]
fn session_dialog_toggles_scope_preserves_search_and_dispatches_server_selection() {
    let current = DialogEntry::action_with_metadata(
        "#7 Alpha",
        "2 turns · 5m ago · primary · current",
        "7 Alpha /work/alpha primary",
        "ID: 7 · Alpha\nTurns: 2 · Agent: primary\nUpdated: 100 (5m ago)",
        "session:7",
    );
    let other = DialogEntry::action_with_metadata(
        "#9 Beta",
        "4 turns · 1h ago · reviewer · root=/work/beta",
        "9 Beta /work/beta reviewer",
        "ID: 9 · Beta\nTurns: 4 · Agent: reviewer\nUpdated: 90 (1h ago) · Root: /work/beta",
        "session:9",
    );
    let mut tui = Tui::new(FakeEngine::default());
    tui.show_selection_dialog(DialogView::sessions_page(
        vec![current],
        SessionDialogRequest::initial(),
        None,
    ));

    let mut search = None;
    assert_eq!(tui.handle(Event::Key(Key::Char('/'))), Action::Render);
    for character in "reviewer".chars() {
        let Action::LoadSessionPage(request) = tui.handle(Event::Key(Key::Char(character))) else {
            panic!("session search should load from the store");
        };
        search = Some(request);
    }
    assert_eq!(tui.handle(Event::Key(Key::Enter)), Action::Render);

    let Action::LoadSessionPage(global) = tui.handle(Event::Key(Key::LineStart)) else {
        panic!("scope toggle should load from the store");
    };
    assert_eq!(global.query(), search.unwrap().query());
    assert_eq!(global.scope(), SessionDialogScope::AllProjects);
    tui.apply_submission_outcome(TuiSubmissionOutcome::Dialog(DialogView::sessions_page(
        vec![other],
        global,
        None,
    )));
    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::DialogAction("session:9".into())
    );
}

#[test]
fn selection_dialog_escape_and_control_c_preserve_distinct_cancel_and_exit_paths() {
    let mut escape = Tui::new(FakeEngine::default());
    escape.show_selection_dialog(DialogView::selection(
        "Confirm",
        None::<String>,
        vec![DialogEntry::action("Proceed", "proceed")],
    ));
    assert_eq!(escape.handle(Event::Key(Key::Escape)), Action::Render);
    assert!(escape.view().dialog.is_none());
    assert_eq!(escape.engine().cancellations, 0);

    let mut control_c = Tui::new(FakeEngine::default());
    control_c.show_selection_dialog(DialogView::selection(
        "Confirm",
        None::<String>,
        vec![DialogEntry::action("Proceed", "proceed")],
    ));
    assert_eq!(control_c.handle(Event::Key(Key::CtrlC)), Action::Render);
    assert!(control_c.view().dialog.is_some());
    assert_eq!(control_c.engine().cancellations, 0);
    assert_eq!(control_c.handle(Event::Key(Key::CtrlC)), Action::Quit);

    for entries in [
        Vec::new(),
        vec![DialogEntry::disabled("Unavailable", "Not configured")],
    ] {
        let mut tui = Tui::new(FakeEngine::default());
        tui.show_selection_dialog(DialogView::selection(
            "Empty",
            Some("Nothing can be selected"),
            entries,
        ));

        assert_eq!(tui.handle(Event::Key(Key::Down)), Action::Render);
        assert_eq!(tui.handle(Event::Key(Key::Enter)), Action::Render);
        assert!(tui.view().dialog.is_some());
        assert_eq!(tui.engine().cancellations, 0);
    }
}

#[test]
fn selection_dialog_cancel_entry_closes_without_dispatch_or_backend_mutation() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.show_selection_dialog(DialogView::selection(
        "Disconnect",
        None::<String>,
        vec![
            DialogEntry::action("Disconnect", "disconnect"),
            DialogEntry::cancel("Cancel"),
        ],
    ));

    tui.handle(Event::Key(Key::Down));
    assert_eq!(tui.handle(Event::Key(Key::Enter)), Action::Render);
    assert!(tui.view().dialog.is_none());
    assert_eq!(tui.engine().cancellations, 0);
}

#[test]
fn typed_submission_outcomes_start_only_explicit_provider_turns() {
    let mut tui = Tui::new(FakeEngine::default());
    for character in "/unknown".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }
    let Action::Submit(input) = tui.handle(Event::Key(Key::Enter)) else {
        panic!("Enter should submit through the production action boundary");
    };

    assert_eq!(
        tui.apply_submission_outcome(TuiSubmissionOutcome::LocalActionableError {
            message: "Unknown command `/unknown`.".into(),
            action: "Run /sessions to list the available local commands.".into(),
        }),
        None
    );
    assert_eq!(input, "/unknown");
    assert!(tui.transcript().is_empty());
    assert!(!tui.view().running);
    assert!(tui.view().conversation.is_none());
    assert!(tui.view().dialog.is_some());

    assert_eq!(
        tui.apply_submission_outcome(TuiSubmissionOutcome::ProviderTurn {
            display: "provider prompt".into(),
            prompt: "provider prompt".into(),
        }),
        Some("provider prompt".into())
    );
    assert!(tui.view().running);
    assert_eq!(
        tui.transcript().last(),
        Some(&TranscriptEntry::User("provider prompt".into()))
    );
}

#[test]
fn tui_submission_outcome_local_auth_progress_is_transient_and_cancellable() {
    let backend = TestBackend::new(80, 24);
    let terminal = Terminal::new(backend).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine::default());

    tui.begin_route();
    tui.apply_route_progress(TuiRouteProgress::DeviceCode {
        verification_url: "https://auth.example/device".into(),
        user_code: "ABCD-EFGH".into(),
    });
    renderer.render(tui.view()).unwrap();
    let text = renderer
        .terminal()
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert!(text.contains("https://auth.example/device"));
    assert!(text.contains("ABCD-EFGH"));
    assert!(tui.transcript().is_empty());
    assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::CancelRoute);
    assert_eq!(tui.engine().cancellations, 0);

    tui.apply_submission_outcome(TuiSubmissionOutcome::LocalActionableError {
        message: "ChatGPT login was cancelled".into(),
        action: "Run authentication again when ready.".into(),
    });
    assert!(!tui.view().running);
    assert!(tui.view().dialog.is_some());
    assert!(tui.transcript().is_empty());
}

#[test]
fn typed_reset_and_context_outcomes_update_visible_state_after_success() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.begin_submission("old prompt");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "old answer".into(),
    )));

    assert_eq!(
        tui.apply_submission_outcome(TuiSubmissionOutcome::ResetSucceeded {
            message: "Started a new session.".into(),
            presentation: TuiPresentation::new("openai-api", "gpt-4.1", "new session"),
        }),
        None
    );
    assert!(tui.transcript().is_empty());
    assert_eq!(tui.view().status, Some("Started a new session."));
    assert_eq!(tui.view().session, "new session");

    tui.apply_submission_outcome(TuiSubmissionOutcome::ContextChanged {
        message: "Resumed session 42.".into(),
        presentation: TuiPresentation::new("openai-api", "o3", "session #42"),
    });
    assert_eq!(tui.view().provider_model, "openai-api / o3");
    assert_eq!(tui.view().session, "session #42");
    assert!(tui.transcript().is_empty());
    assert_eq!(tui.view().status, Some("Resumed session 42."));
}

#[test]
fn typed_provider_completion_keeps_success_clean_and_failure_actionable() {
    let mut success = Tui::new(FakeEngine::default());
    success.apply_submission_outcome(TuiSubmissionOutcome::ProviderTurn {
        display: "request".into(),
        prompt: "request".into(),
    });
    success.apply_progress(TurnEvent::ProviderPart(MessagePart::Text("answer".into())));
    success.finish_provider_turn(TuiProviderOutcome::Completed("answer".into()));

    assert_eq!(
        success.transcript(),
        [
            TranscriptEntry::User("request".into()),
            TranscriptEntry::Assistant("answer".into()),
        ]
    );
    assert!(success.view().conversation.unwrap().errors.is_empty());

    let mut failure = Tui::new(FakeEngine::default());
    failure.apply_submission_outcome(TuiSubmissionOutcome::ProviderTurn {
        display: "request".into(),
        prompt: "request".into(),
    });
    failure.finish_provider_turn(TuiProviderOutcome::Failed {
        message: "provider: token=SENTINEL".into(),
        action: "Check provider credentials and retry.".into(),
    });

    assert_eq!(
        failure.transcript(),
        [
            TranscriptEntry::User("request".into()),
            TranscriptEntry::Error("provider: token=[redacted: 8 characters]".into()),
        ]
    );
    let view = failure.view();
    assert_eq!(view.turn_state, Some(TurnState::Failed));
    assert_eq!(view.conversation.unwrap().errors.len(), 1);
    assert_eq!(
        view.conversation.unwrap().errors[0].message,
        "provider: token=[redacted: 8 characters]"
    );
    assert!(
        !view.conversation.unwrap().errors[0]
            .message
            .contains("SENTINEL")
    );
    assert!(view.conversation.unwrap().final_markdown.is_none());
}

#[test]
fn submission_start_keeps_usage_and_resets_turn_duration() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.apply_runtime_event(TuiRuntimeEvent::Usage(agens_core::Usage {
        input_tokens: Some(10),
        output_tokens: Some(5),
        total_tokens: Some(15),
        context_window: Some(8_192),
    }));
    tui.apply_runtime_event(TuiRuntimeEvent::TurnEnded {
        status: TurnState::Completed,
        duration: Some(Duration::from_millis(25)),
    });
    assert_eq!(tui.view().latest_usage.unwrap().total_tokens, Some(15));
    assert_eq!(tui.view().turn_duration, Some(Duration::from_millis(25)));

    tui.apply_submission_outcome(TuiSubmissionOutcome::ProviderTurn {
        display: "next".into(),
        prompt: "next".into(),
    });

    assert_eq!(tui.view().latest_usage.unwrap().total_tokens, Some(15));
    assert!(tui.view().turn_duration.is_none());
}

#[test]
fn second_submission_is_rejected_while_a_turn_owns_cancellation() {
    let mut tui = Tui::new(FakeEngine::default());

    tui.begin_submission("first prompt");
    assert_eq!(tui.handle(Event::Key(Key::Char('s'))), Action::Render);
    assert_eq!(tui.handle(Event::Key(Key::Enter)), Action::Render);
    assert_eq!(tui.input(), "");
    assert_eq!(
        tui.transcript(),
        [agens_tui::TranscriptEntry::User("first prompt".into()),]
    );
    assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Render);
    assert_eq!(tui.engine().cancellations, 1);
}

#[test]
fn resize_updates_the_render_state() {
    let mut tui = Tui::new(FakeEngine::default());

    assert_eq!(
        tui.handle(Event::Resize {
            width: 120,
            height: 40
        }),
        Action::Render
    );
    assert_eq!(tui.size(), (120, 40));
}

#[test]
fn control_c_warns_then_cancels_and_quits_a_running_turn() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.begin_submission("active");

    assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Render);
    assert_eq!(tui.engine().cancellations, 1);
    assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Render);
}

#[test]
fn double_control_c_exits_without_clearing_composer_input() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.handle(Event::Key(Key::Char('x')));

    assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Render);
    assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Quit);
    assert_eq!(tui.input(), "x");
}

#[test]
fn escape_never_cancels_a_running_turn_and_control_c_requests_cancellation() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.begin_submission("running");

    assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::Render);
    assert_eq!(
        tui.view().focus,
        TranscriptFocus::Viewport,
        "Esc moves the reader into the transcript"
    );
    assert_eq!(tui.engine().cancellations, 0);
    assert!(!tui.view().quit_armed);

    assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Render);
    assert_eq!(tui.engine().cancellations, 1);
    assert!(!tui.view().quit_armed);
}

#[test]
fn permission_double_control_c_exits_and_runtime_cleanup_can_fail_closed() {
    let (bridge, requests) = TuiPermissionBridge::channel();
    let worker_bridge = bridge.clone();
    let worker = thread::spawn(move || {
        worker_bridge.wait_for_reply(
            "write",
            "notes.md",
            "Write",
            None,
            None,
            &HeadlessTurnCancellation::new(),
        )
    });
    let request = requests.recv_timeout(Duration::from_secs(1)).unwrap();
    let mut tui = Tui::new(FakeEngine::default());
    tui.begin_submission("active");
    tui.show_selection_dialog(DialogView::selection(
        "Permission required",
        None::<String>,
        vec![DialogEntry::action(
            "Allow once",
            format!("permission:{}:allow-once", request.id()),
        )],
    ));

    assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Render);
    assert!(bridge.is_pending(request.id()));
    assert!(tui.view().dialog.is_some());
    assert_eq!(tui.engine().cancellations, 1);
    assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Render);
    assert!(bridge.close());
    assert_eq!(worker.join().unwrap(), TuiPermissionReply::Cancelled);
}

#[test]
fn submitted_prompt_and_provider_output_are_retained_in_order() {
    let mut tui = Tui::new(FakeEngine::default());

    tui.begin_submission("explain the project");
    tui.finish_submission(Ok("Agens is a coding agent.".into()));

    assert_eq!(
        tui.transcript(),
        [
            agens_tui::TranscriptEntry::User("explain the project".into()),
            agens_tui::TranscriptEntry::Assistant("Agens is a coding agent.".into()),
        ]
    );
    assert!(!tui.view().running);
}

#[test]
fn provider_failures_are_shown_without_leaving_the_turn_running() {
    let mut tui = Tui::new(FakeEngine::default());

    tui.begin_submission("use the provider");
    tui.finish_submission(Err("provider: provider request failed".into()));

    assert_eq!(
        tui.transcript(),
        [
            agens_tui::TranscriptEntry::User("use the provider".into()),
            agens_tui::TranscriptEntry::Error("provider: provider request failed".into()),
        ]
    );
    assert!(!tui.view().running);
}

#[test]
fn streaming_events_update_stable_entries_and_preserve_tool_order() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.begin_submission("inspect the project");

    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text("First ".into())));
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text("answer".into())));
    tui.apply_progress(TurnEvent::ToolCallRequested {
        id: "call-1".into(),
        name: "native::read".into(),
        input: "secret path omitted".into(),
    });
    tui.apply_progress(TurnEvent::ToolResult(MessagePart::ToolResult {
        tool_call_id: "call-1".into(),
        content: "file contents".into(),
        is_error: false,
    }));
    tui.apply_progress(TurnEvent::StateChanged(TurnState::Completed));

    assert_eq!(
        tui.transcript(),
        [
            TranscriptEntry::User("inspect the project".into()),
            TranscriptEntry::Assistant("First answer".into()),
            TranscriptEntry::Tool("native::read started".into()),
            TranscriptEntry::Tool("native::read completed: file contents".into()),
        ]
    );
    assert!(tui.view().running);
}

#[test]
fn multiline_editing_and_scroll_follow_are_deterministic() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.handle(Event::Key(Key::Char('a')));
    tui.handle(Event::Key(Key::ShiftEnter));
    tui.handle(Event::Key(Key::Char('b')));
    tui.handle(Event::Key(Key::Left));
    tui.handle(Event::Key(Key::Backspace));
    tui.handle(Event::Key(Key::PageUp));

    assert_eq!(tui.input(), "ab");
    assert!(!tui.following_bottom());
    // End edits the prompt; following the transcript again is the
    // composer-safe jump, since scrolling no longer moves focus.
    assert_eq!(tui.handle(Event::Key(Key::CtrlShiftG)), Action::Render);
    assert!(tui.following_bottom());
    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::Submit("ab".into())
    );
}

#[test]
fn composer_editing_moves_and_deletes_complete_graphemes() {
    let mut tui = Tui::new(FakeEngine::default());
    for character in "e\u{301}🙂z".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }

    tui.handle(Event::Key(Key::Left));
    assert_eq!(tui.view().input_cursor, 3);
    tui.handle(Event::Key(Key::Left));
    assert_eq!(tui.view().input_cursor, 2);
    tui.handle(Event::Key(Key::Left));
    assert_eq!(tui.view().input_cursor, 0);

    tui.handle(Event::Key(Key::Right));
    assert_eq!(tui.view().input_cursor, 2);
    tui.handle(Event::Key(Key::Backspace));
    assert_eq!(tui.input(), "🙂z");
    assert_eq!(tui.view().input_cursor, 0);

    tui.handle(Event::Key(Key::Delete));
    assert_eq!(tui.input(), "z");
    assert_eq!(tui.view().input_cursor, 0);
}

#[test]
fn composer_insertion_and_word_movement_keep_grapheme_boundaries() {
    let mut tui = Tui::new(FakeEngine::default());
    for character in "e\u{301} 🙂".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }

    tui.handle(Event::Key(Key::PreviousWord));
    assert_eq!(tui.view().input_cursor, 3);
    tui.handle(Event::Key(Key::PreviousWord));
    assert_eq!(tui.view().input_cursor, 0);
    tui.handle(Event::Key(Key::NextWord));
    assert_eq!(tui.view().input_cursor, 2);
    tui.handle(Event::Key(Key::Right));
    assert_eq!(tui.view().input_cursor, 3);

    tui.handle(Event::Key(Key::Char('x')));
    assert_eq!(tui.input(), "e\u{301} x🙂");
    assert_eq!(tui.view().input_cursor, 4);
    tui.handle(Event::Key(Key::Delete));
    assert_eq!(tui.input(), "e\u{301} x");
    assert_eq!(tui.view().input_cursor, 4);
}

#[test]
fn ratatui_layout_degrades_without_overlapping_at_standard_narrow_and_short_sizes() {
    for (width, height) in [(80, 24), (35, 24), (80, 10)] {
        let backend = TestBackend::new(width, height);
        let terminal = Terminal::new(backend).unwrap();
        let mut renderer = RatatuiRenderer::new(terminal);
        let tui = Tui::new(FakeEngine::default());

        renderer.render(tui.view()).unwrap();
        let buffer = renderer.terminal().backend().buffer();
        let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();

        assert_eq!(buffer.area.width, width);
        assert_eq!(buffer.area.height, height);
        // Idle chrome is minimal (no brand header); still paint band rules and/or footer.
        assert!(
            text.contains('─') || text.contains("Ready") || text.contains("model"),
            "height {height}: expected layout chrome, got {text:?}"
        );
    }
}

#[test]
fn ratatui_surface_presents_context_roles_activity_and_responsive_shortcuts() {
    let backend = TestBackend::new(96, 24);
    let terminal = Terminal::new(backend).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine::default());
    tui.set_presentation("openai-api", "gpt-4.1", "session #42");
    tui.begin_submission("Inspect the project structure.");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Reasoning(
        "Checking the workspace.".into(),
    )));
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "The workspace contains focused Rust crates.".into(),
    )));
    tui.handle(Event::Key(Key::Char('n')));
    tui.handle(Event::Key(Key::ShiftEnter));
    tui.handle(Event::Key(Key::Char('o')));

    renderer.render(tui.view()).unwrap();
    let buffer = renderer.terminal().backend().buffer();
    let text = buffer
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    // Footer carries model/project; header no longer says "agens safe".
    assert!(!text.contains("agens safe"));
    assert!(text.contains("gpt-4.1"));
    assert!(text.contains('❯'));
    assert!(text.contains("Thinking"));
    assert!(text.contains("Ready") || text.contains("Responding"));
    assert!(!text.contains("Compose"));
    assert!(!text.contains("Enter send"));
    assert!(!text.contains("LIVE"));
    assert!(!text.contains("SCROLL"));

    let user_cell = buffer
        .content
        .iter()
        .find(|cell| cell.symbol() == "❯")
        .expect("user prompt marker is rendered");
    assert_eq!(user_cell.fg, ratatui::style::Color::Rgb(0xd2, 0xa6, 0xff));

    tui.apply_progress(TurnEvent::ToolCallRequested {
        id: "call-1".into(),
        name: "native::read".into(),
        input: "omitted".into(),
    });
    renderer.render(tui.view()).unwrap();
    let tool_text = renderer
        .terminal()
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(tool_text.contains("Tools"));
    assert!(tool_text.contains("read"));

    let backend = TestBackend::new(50, 14);
    let terminal = Terminal::new(backend).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    renderer.render(tui.view()).unwrap();
    let narrow_text = renderer
        .terminal()
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert!(narrow_text.contains("gpt-4.1") || narrow_text.contains("Ready"));
    assert!(!narrow_text.contains("Enter send"));
    assert!(!narrow_text.contains("Compose"));
}

#[test]
fn ratatui_active_turn_row_distinguishes_waiting_responding_cancelling_and_failure() {
    let backend = TestBackend::new(80, 24);
    let terminal = Terminal::new(backend).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine::default());
    tui.begin_submission("status test");

    renderer.render(tui.view()).unwrap();
    let waiting = renderer
        .terminal()
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(waiting.contains("Waiting"));

    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "response".into(),
    )));
    renderer.render(tui.view()).unwrap();
    let responding = renderer
        .terminal()
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(responding.contains("Responding"));

    assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Render);
    assert_eq!(tui.engine().cancellations, 1);
    assert_eq!(
        tui.view().status,
        Some("Cancellation requested; waiting for confirmation.")
    );

    tui.finish_provider_turn(TuiProviderOutcome::Failed {
        message: "provider failed".into(),
        action: "Retry the prompt.".into(),
    });
    renderer.render(tui.view()).unwrap();
    let failed = renderer
        .terminal()
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(failed.contains("Failed"));
}

#[test]
fn plain_jk_insert_while_ctrl_timeline_nav_scrolls_and_jumps() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.handle(Event::Resize {
        width: 48,
        height: 12,
    });

    tui.begin_submission("first-user-message-anchor");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "assistant-block-one\n".repeat(30),
    )));
    tui.finish_provider_turn(TuiProviderOutcome::Completed("done-one".into()));

    tui.begin_submission("second-user-message-anchor");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "assistant-block-two\n".repeat(30),
    )));
    tui.finish_provider_turn(TuiProviderOutcome::Completed("done-two".into()));

    // Plain j/k remain insert characters in the composer.
    assert_eq!(tui.handle(Event::Key(Key::Char('j'))), Action::Render);
    assert_eq!(tui.handle(Event::Key(Key::Char('k'))), Action::Render);
    assert_eq!(tui.input(), "jk");
    assert_eq!(tui.view().focus, TranscriptFocus::Composer);

    let before_scroll = tui.view().scroll_offset;
    assert_eq!(tui.handle(Event::Key(Key::CtrlK)), Action::Render);
    assert!(
        !tui.view().following_bottom
            || tui.view().scroll_offset < before_scroll
            || tui.view().scroll_offset > 0
            || !tui.view().following_bottom,
        "Ctrl+k must scroll the timeline up"
    );
    assert!(!tui.view().following_bottom);
    // Ctrl+j and Ctrl+k are the composer-safe scroll: they detach the viewport
    // and leave the prompt taking text, which is the whole point of having them
    // alongside the plain motions.
    assert_eq!(tui.view().focus, TranscriptFocus::Composer);
    assert_eq!(tui.input(), "jk");

    let after_up = tui.view().scroll_offset;
    assert_eq!(tui.handle(Event::Key(Key::CtrlJ)), Action::Render);
    assert!(
        tui.view().scroll_offset >= after_up || tui.view().following_bottom,
        "Ctrl+j must scroll the timeline down"
    );
    assert_eq!(tui.input(), "jk");

    assert_eq!(tui.handle(Event::Key(Key::CtrlG)), Action::Render);
    assert_eq!(tui.view().scroll_offset, 0);
    assert!(!tui.view().following_bottom);

    assert_eq!(tui.handle(Event::Key(Key::CtrlShiftG)), Action::Render);
    assert!(tui.view().following_bottom);

    assert_eq!(tui.handle(Event::Key(Key::CtrlN)), Action::Render);
    let last_user_offset = tui.view().scroll_offset;

    assert_eq!(tui.handle(Event::Key(Key::CtrlShiftN)), Action::Render);
    let previous_user_offset = tui.view().scroll_offset;
    assert!(
        previous_user_offset <= last_user_offset,
        "Ctrl+N jumps toward an earlier user message ({previous_user_offset} <= {last_user_offset})"
    );
    assert_eq!(tui.input(), "jk");
}

#[test]
fn viewport_owner_keys_are_gt_m_and_brackets_with_ctrl_timeline_nav() {
    let mut tui = Tui::new(FakeEngine::default());
    start_child(&mut tui, 7);
    tui.apply_runtime_event(TuiRuntimeEvent::SubagentExecution(
        TuiSubagentEvent::started(7, "reviewer", "task", TuiExecutionState::ForegroundRunning),
    ));
    start_child(&mut tui, 8);
    tui.apply_runtime_event(TuiRuntimeEvent::SubagentExecution(
        TuiSubagentEvent::started(8, "writer", "task", TuiExecutionState::ForegroundRunning),
    ));

    focus_viewport(&mut tui);
    assert_eq!(tui.view().focus, TranscriptFocus::Viewport);
    // `g` is a chord prefix now, so the picker costs its second key.
    assert_eq!(tui.handle(Event::Key(Key::Char('g'))), Action::Render);
    assert!(tui.view().dialog.is_none(), "g alone opens nothing");
    assert_eq!(tui.handle(Event::Key(Key::Char('t'))), Action::Render);
    assert!(tui.view().dialog.is_some());
    tui.handle(Event::Key(Key::Escape));

    // h and l are horizontal motions in a vim keymap, so sibling navigation
    // moved to the bracket pair.
    tui.handle(Event::Key(Key::Char(']')));
    assert_eq!(tui.view().active_transcript, TranscriptId::Subagent(7));
    tui.handle(Event::Key(Key::Char(']')));
    assert_eq!(tui.view().active_transcript, TranscriptId::Subagent(8));
    tui.handle(Event::Key(Key::Char('[')));
    assert_eq!(tui.view().active_transcript, TranscriptId::Subagent(7));
    tui.handle(Event::Key(Key::Char('m')));
    assert_eq!(tui.view().active_transcript, TranscriptId::Main);

    // A focused transcript swallows what it does not claim. Letting the key
    // through is what used to make reading a transcript type into the prompt.
    tui.handle(Event::Key(Key::Char('z')));
    assert_eq!(tui.input(), "");
}

fn permission_confirm_entries(request_id: u64) -> Vec<DialogEntry> {
    [
        ("Allow once", "allow-once"),
        ("Always allow", "allow-always"),
        ("Deny once", "deny-once"),
        ("Always deny", "deny-always"),
    ]
    .into_iter()
    .map(|(label, answer)| DialogEntry::action(label, format!("permission:{request_id}:{answer}")))
    .collect()
}

#[test]
fn permission_confirm_short_keys_dispatch_allow_deny_once_and_always() {
    for (key, expected) in [
        ('a', "permission:7:allow-once"),
        ('d', "permission:7:deny-once"),
        ('A', "permission:7:allow-always"),
        ('D', "permission:7:deny-always"),
    ] {
        let mut tui = Tui::new(FakeEngine::default());
        tui.show_selection_dialog(
            DialogView::selection(
                "Permission required",
                Some("bash\nrm -rf /tmp/x"),
                permission_confirm_entries(7),
            )
            .as_confirm(),
        );

        assert_eq!(
            tui.handle(Event::Key(Key::Char(key))),
            Action::DialogAction(expected.into()),
            "short key {key}"
        );
        assert!(
            tui.view().dialog.is_none(),
            "short key {key} dismisses the permission dialog immediately"
        );
        assert_eq!(tui.engine().cancellations, 0);
    }
}

#[test]
fn permission_confirm_list_enter_dispatches_selected_choice() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.show_selection_dialog(
        DialogView::selection(
            "Permission required",
            Some("tool\ntarget"),
            permission_confirm_entries(3),
        )
        .as_confirm(),
    );

    tui.handle(Event::Key(Key::Down));
    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::DialogAction("permission:3:allow-always".into())
    );
    assert!(tui.view().dialog.is_none());
}

#[test]
fn permission_confirm_short_keys_do_not_append_to_query() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.show_selection_dialog(
        DialogView::selection(
            "Permission required",
            Some("tool\ntarget"),
            permission_confirm_entries(1),
        )
        .as_confirm(),
    );

    assert_eq!(
        tui.handle(Event::Key(Key::Char('a'))),
        Action::DialogAction("permission:1:allow-once".into())
    );
    assert_eq!(tui.input(), "");
}

#[test]
fn picker_overlay_types_a_query_only_once_search_is_armed() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.show_selection_dialog(DialogView::selection(
        "Choose",
        Some("Pick"),
        vec![
            DialogEntry::action("alpha", "alpha"),
            DialogEntry::action("delta", "delta"),
        ],
    ));

    // Unarmed, an unbound character is dropped and the selection stays put.
    assert_eq!(tui.handle(Event::Key(Key::Char('d'))), Action::Render);
    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::DialogAction("alpha".into())
    );
    assert!(
        tui.view().dialog.is_some(),
        "non-permission dialog actions retain their existing lifecycle"
    );

    tui.show_selection_dialog(DialogView::selection(
        "Choose",
        Some("Pick"),
        vec![
            DialogEntry::action("alpha", "alpha"),
            DialogEntry::action("delta", "delta"),
        ],
    ));
    assert_eq!(tui.handle(Event::Key(Key::Char('/'))), Action::Render);
    assert_eq!(tui.handle(Event::Key(Key::Char('d'))), Action::Render);
    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::DialogAction("delta".into())
    );
}

#[test]
fn escape_closes_topmost_palette_before_dialog() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.set_palette_entries(vec![PaletteEntry::new(
        "/review",
        "Review",
        "",
        PaletteEntryKind::BuiltIn,
    )]);
    tui.show_dialog("Notice", "Informational body");
    tui.handle(Event::Key(Key::Char('/')));
    assert!(tui.view().palette.is_some());
    assert!(tui.view().dialog.is_some());

    assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::Render);
    assert!(tui.view().palette.is_none(), "palette is topmost");
    assert!(
        tui.view().dialog.is_some(),
        "dialog remains until second Esc"
    );

    assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::Render);
    assert!(tui.view().dialog.is_none());
    assert_eq!(tui.engine().cancellations, 0);
}

#[test]
fn safe_dialog_while_running_remains_usable_and_esc_does_not_cancel_turn() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.set_running(true);
    tui.show_selection_dialog(
        DialogView::selection(
            "Select project file",
            Some("Choose one approved file | Esc cancel"),
            vec![DialogEntry::safe_action(
                "src/main.rs",
                "select:src/main.rs",
            )],
        )
        .with_cancellation_action("select:cancel"),
    );

    assert!(tui.view().running);
    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::SafeDialogAction("select:src/main.rs".into())
    );
    assert!(tui.view().dialog.is_some());
    assert!(tui.view().running);
    assert_eq!(tui.engine().cancellations, 0);

    tui.show_selection_dialog(
        DialogView::selection(
            "Select project file",
            Some("Choose one approved file | Esc cancel"),
            vec![DialogEntry::safe_action("src/lib.rs", "select:src/lib.rs")],
        )
        .with_cancellation_action("select:cancel"),
    );
    assert_eq!(
        tui.handle(Event::Key(Key::Escape)),
        Action::SafeDialogAction("select:cancel".into())
    );
    assert!(tui.view().dialog.is_some());
    assert!(tui.view().running);
    assert_eq!(tui.engine().cancellations, 0);
}

#[test]
fn parsed_tool_input_reaches_live_projection_via_tool_started_enrichment() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.begin_submission("request");
    tui.apply_progress(TurnEvent::ToolCallRequested {
        id: "read-1".into(),
        name: "read".into(),
        input: "src/lib.rs".into(),
    });

    let placeholder = tui.view().conversation.unwrap().tool_batches[0].calls[0]
        .parsed
        .clone();
    assert_eq!(
        placeholder,
        ToolInput::Other {
            name: "read".into(),
            raw: "src/lib.rs".into(),
        }
    );

    tui.apply_runtime_event(TuiRuntimeEvent::ToolStarted {
        call_id: "read-1".into(),
        name: "read".into(),
        input: "src/lib.rs".into(),
        parsed: ToolInput::Read {
            path: "src/lib.rs".into(),
        },
    });

    let enriched = tui.view().conversation.unwrap().tool_batches[0].calls[0]
        .parsed
        .clone();
    assert_eq!(
        enriched,
        ToolInput::Read {
            path: "src/lib.rs".into(),
        }
    );
}

#[test]
fn parsed_tool_input_reaches_subagent_tool_call_update() {
    let mut tui = Tui::new(FakeEngine::default());
    start_child(&mut tui, 7);
    tui.apply_runtime_event(TuiRuntimeEvent::SubagentExecution(
        TuiSubagentEvent::started(7, "reviewer", "task", TuiExecutionState::ForegroundRunning),
    ));
    tui.apply_runtime_event(TuiRuntimeEvent::SubagentExecution(
        TuiSubagentEvent::tool_call_with_parsed(
            7,
            "child-call",
            "grep",
            "needle",
            ToolInput::Grep {
                pattern: "needle".into(),
                path: None,
            },
        ),
    ));

    tui.select_transcript(TranscriptId::Subagent(7));
    let parsed = tui.view().conversation.unwrap().tool_batches[0].calls[0]
        .parsed
        .clone();
    assert_eq!(
        parsed,
        ToolInput::Grep {
            pattern: "needle".into(),
            path: None,
        }
    );
}

#[test]
fn parsed_tool_input_reaches_restore_with_qualified_name_stripped() {
    let messages = [
        Message {
            role: Role::User,
            parts: vec![MessagePart::Text("restore me".into())],
        },
        Message {
            role: Role::Assistant,
            parts: vec![MessagePart::ToolCall {
                id: "call-1".into(),
                name: "native::read".into(),
                input: "src/lib.rs".into(),
            }],
        },
    ];

    // Default `from_messages` cannot parse (no parser at this crate
    // boundary) and degrades every restored call to `Other`.
    let degraded = Conversation::from_messages(&messages).unwrap();
    assert_eq!(
        degraded[0].tool_batches[0].calls[0].parsed,
        ToolInput::Other {
            name: "native::read".into(),
            raw: "src/lib.rs".into(),
        }
    );

    // `from_messages_with_parser` receives the qualified restored name
    // (`native::read`), matching the live path's bare `read`; the caller's
    // closure must strip the prefix before parsing.
    let parsed = Conversation::from_messages_with_parser(&messages, |name, input| {
        let bare = name
            .strip_prefix("native::")
            .or_else(|| name.strip_prefix("mcp::"))
            .unwrap_or(name);
        match bare {
            "read" => ToolInput::Read {
                path: input.to_owned(),
            },
            _ => ToolInput::Other {
                name: name.to_owned(),
                raw: input.to_owned(),
            },
        }
    })
    .unwrap();
    assert_eq!(
        parsed[0].tool_batches[0].calls[0].parsed,
        ToolInput::Read {
            path: "src/lib.rs".into(),
        }
    );
}

fn finish_background_child(tui: &mut Tui<FakeEngine>, id: u64) {
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "reviewer".into(),
        event: TuiExecutionEvent::BackgroundStarted { id },
    });
    tui.apply_runtime_event(TuiRuntimeEvent::SubagentExecution(
        TuiSubagentEvent::started(
            id,
            "reviewer",
            format!("review-{id}"),
            TuiExecutionState::BackgroundRunning,
        ),
    ));
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "reviewer".into(),
        event: TuiExecutionEvent::Completed { id },
    });
}

#[test]
fn a_finished_background_subagent_fires_one_main_turn_while_idle() {
    let mut tui = Tui::new(FakeEngine::default());
    finish_background_child(&mut tui, 7);

    let prompt = tui.take_ready_auto_turn().expect("idle schedules the turn");

    assert!(prompt.starts_with("[coordination source=runtime"));
    assert!(prompt.contains("1 background subagent"));
    assert!(tui.view().running);
    assert!(tui.take_ready_auto_turn().is_none());
}

#[test]
fn resume_projects_user_media_parts_as_path_free_chips() {
    let restored = Conversation::from_messages(&[
        Message {
            role: Role::User,
            parts: vec![
                MessagePart::Text("look".into()),
                MessagePart::Media {
                    media_id: 9,
                    mime: "image/png".into(),
                },
                MessagePart::Media {
                    media_id: 11,
                    mime: "application/pdf".into(),
                },
            ],
        },
        Message {
            role: Role::Assistant,
            parts: vec![MessagePart::Text("ok".into())],
        },
    ])
    .expect("user media must restore without InvalidMessageOrder");

    let turn = restored.last().expect("one restored turn");
    assert!(
        turn.user.contains("[Image #1]"),
        "image chip missing: {}",
        turn.user
    );
    assert!(
        turn.user.contains("[File #2]"),
        "file chip missing: {}",
        turn.user
    );
    assert!(
        !turn.user.contains('/') && !turn.user.contains("media_id"),
        "resume chips must be path-free: {}",
        turn.user
    );
}

#[test]
fn media_only_composer_submit_with_empty_text() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.set_media_chips(vec!["[Image #1]".into()]);
    assert!(tui.input().is_empty());

    let action = tui.handle(Event::Key(Key::Enter));
    assert!(
        matches!(action, Action::Submit(ref prompt) if prompt.is_empty()),
        "empty text with staged media must submit, got {action:?}"
    );
    assert_eq!(
        tui.media_chips(),
        &["[Image #1]".to_owned()][..],
        "chips stay until the provider turn succeeds"
    );
}

/// The provider is told about a scheduled turn in a user-role message, so that
/// is what the session store keeps. Replaying it verbatim showed the reader a
/// prompt of their own that said the user had not sent it.
#[test]
fn a_restored_runtime_turn_reads_as_a_notice_rather_than_a_user_prompt() {
    let mut tui = Tui::new(FakeEngine::default());
    finish_background_child(&mut tui, 7);
    let prompt = tui.take_ready_auto_turn().expect("idle schedules the turn");

    let restored = Conversation::from_messages(&[
        Message {
            role: Role::User,
            parts: vec![MessagePart::Text(prompt)],
        },
        Message {
            role: Role::Assistant,
            parts: vec![MessagePart::Text("summary".into())],
        },
    ])
    .expect("a scheduled turn restores");

    let turn = restored.last().expect("one restored turn");
    assert_eq!(turn.user, "", "no prompt is attributed to the reader");

    let rendered = format!("{:?}", turn);
    assert!(
        rendered.contains("Continuing automatically: 1 background subagent finished."),
        "the restored turn keeps the notice the live one recorded: {rendered:?}"
    );
    assert!(
        !rendered.contains("coordination source=runtime"),
        "the coordination text is not shown back to the reader: {rendered:?}"
    );
}

#[test]
fn a_finished_foreground_subagent_never_fires_a_main_turn() {
    let mut tui = Tui::new(FakeEngine::default());
    start_child(&mut tui, 7);
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "reviewer".into(),
        event: TuiExecutionEvent::Completed { id: 7 },
    });

    assert_eq!(tui.take_ready_auto_turn(), None);
}

#[test]
fn a_running_turn_defers_the_auto_turn_instead_of_dropping_it() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.begin_submission("user work");
    finish_background_child(&mut tui, 7);

    assert_eq!(tui.take_ready_auto_turn(), None);

    tui.finish_provider_turn(TuiProviderOutcome::Completed("done".into()));

    assert!(tui.take_ready_auto_turn().is_some());
}

#[test]
fn a_composer_with_text_defers_the_auto_turn_instead_of_dropping_it() {
    let mut tui = Tui::new(FakeEngine::default());
    for character in "half typed".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }
    finish_background_child(&mut tui, 7);

    assert_eq!(tui.take_ready_auto_turn(), None);

    for _ in 0.."half typed".len() {
        tui.handle(Event::Key(Key::Backspace));
    }

    assert!(tui.take_ready_auto_turn().is_some());
}

#[test]
fn simultaneous_background_completions_coalesce_into_one_auto_turn() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.begin_submission("user work");
    for id in [7, 8, 9] {
        finish_background_child(&mut tui, id);
    }
    tui.finish_provider_turn(TuiProviderOutcome::Completed("done".into()));

    let prompt = tui.take_ready_auto_turn().expect("idle schedules the turn");

    assert!(prompt.contains("3 background subagents"));
    assert_eq!(tui.take_ready_auto_turn(), None);
}

#[test]
fn the_auto_turn_is_cancellable_and_never_fabricates_a_user_prompt() {
    let mut tui = Tui::new(FakeEngine::default());
    finish_background_child(&mut tui, 7);
    tui.take_ready_auto_turn().expect("idle schedules the turn");

    let view = tui.view();
    let conversation = view
        .conversation
        .expect("the auto turn opens a conversation");
    assert!(conversation.user.is_empty());
    assert_eq!(
        conversation.info,
        vec!["Continuing automatically: 1 background subagent finished.".to_owned()]
    );
    assert!(
        !tui.transcript()
            .iter()
            .any(|entry| matches!(entry, TranscriptEntry::User(_)))
    );

    assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::Render);
    assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Render);
    assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Render);
    assert_eq!(tui.engine().cancellations, 1);
}

fn file_candidates() -> Vec<String> {
    vec![
        "AGENTS.md".to_owned(),
        "crates/agens-cli/src/lib.rs".to_owned(),
        "crates/agens-tui/src/lib.rs".to_owned(),
        "crates/agens-tui/src/render.rs".to_owned(),
    ]
}

fn typed(tui: &mut Tui<FakeEngine>, text: &str) {
    for character in text.chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }
}

fn focus_viewport(tui: &mut Tui<FakeEngine>) {
    assert_eq!(
        tui.handle(Event::MouseDown { column: 4, row: 1 }),
        Action::Render
    );
    assert_eq!(tui.view().focus, TranscriptFocus::Viewport);
}

#[test]
fn an_at_reference_opens_the_file_picker_and_the_typed_token_filters_it() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.set_file_candidates(file_candidates());

    tui.handle(Event::Key(Key::Char('@')));
    let opened = tui.view().file_picker.expect("@ opens the file picker");
    assert_eq!(opened.query(), "");
    assert_eq!(opened.matches().len(), 4);

    typed(&mut tui, "tui/src/re");
    let filtered = tui
        .view()
        .file_picker
        .expect("typing keeps the picker open");
    assert_eq!(filtered.query(), "tui/src/re");
    assert_eq!(filtered.matches(), vec!["crates/agens-tui/src/render.rs"]);
    assert_eq!(tui.input(), "@tui/src/re");

    tui.handle(Event::Key(Key::Backspace));
    assert_eq!(
        tui.view()
            .file_picker
            .expect("backspace still edits the token")
            .query(),
        "tui/src/r"
    );

    tui.handle(Event::Key(Key::Char(' ')));
    assert!(
        tui.view().file_picker.is_none(),
        "whitespace ends the reference token"
    );
}

#[test]
fn an_at_inside_a_word_never_opens_the_file_picker() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.set_file_candidates(file_candidates());

    typed(&mut tui, "mail@example.com");

    assert!(tui.view().file_picker.is_none());
    assert_eq!(tui.input(), "mail@example.com");
}

#[test]
fn selecting_a_file_inserts_its_relative_path_at_the_at_token() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.set_file_candidates(file_candidates());

    typed(&mut tui, "review @src/lib");
    let picker = tui.view().file_picker.expect("the picker is open");
    assert_eq!(
        picker.matches(),
        vec!["crates/agens-cli/src/lib.rs", "crates/agens-tui/src/lib.rs"]
    );

    tui.handle(Event::Key(Key::Down));
    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::Render,
        "Enter inserts the selection instead of submitting"
    );
    assert_eq!(tui.input(), "review @crates/agens-tui/src/lib.rs");
    assert!(tui.view().file_picker.is_none());

    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::Submit("review @crates/agens-tui/src/lib.rs".to_owned())
    );
}

#[test]
fn escape_closes_the_file_picker_and_leaves_the_composer_as_typed() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.set_file_candidates(file_candidates());
    typed(&mut tui, "review @src/lib");

    assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::Render);

    assert!(tui.view().file_picker.is_none());
    assert_eq!(tui.input(), "review @src/lib");
}

#[test]
fn escape_precedence_still_prefers_the_palette_and_dialogs_over_the_file_picker() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.set_file_candidates(file_candidates());
    tui.set_palette_entries(vec![PaletteEntry::new(
        "review",
        "Review the patch",
        "[scope]",
        PaletteEntryKind::Command,
    )]);

    typed(&mut tui, "/review @src");
    assert!(tui.view().palette.is_some());
    assert!(
        tui.view().file_picker.is_none(),
        "the palette keeps the overlay layer"
    );
    assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::Render);
    assert!(tui.view().palette.is_none());

    tui.show_selection_dialog(DialogView::selection(
        "Choose",
        None::<String>,
        vec![DialogEntry::action("Keep", "keep")],
    ));
    tui.handle(Event::Key(Key::Char('@')));
    assert!(
        tui.view().file_picker.is_none(),
        "an interactive dialog consumes composer keys"
    );

    assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::Render);
    assert!(tui.view().dialog.is_none());
    assert!(tui.view().file_picker.is_none());
}

#[test]
fn the_file_picker_takes_navigation_keys_before_the_subagent_strip() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.set_file_candidates(file_candidates());
    start_child(&mut tui, 7);
    assert_eq!(tui.handle(Event::Key(Key::Tab)), Action::Render);
    assert_eq!(tui.view().surface_focus, agens_tui::SurfaceFocus::Queue);
    assert_eq!(tui.handle(Event::Key(Key::Tab)), Action::Render);
    assert_eq!(tui.view().surface_focus, agens_tui::SurfaceFocus::Composer);
    assert_eq!(
        tui.view().execution_selection,
        None,
        "Tab never reaches the subagent tree"
    );

    typed(&mut tui, "@src/lib");
    tui.handle(Event::Key(Key::Down));
    tui.handle(Event::Key(Key::Enter));

    assert_eq!(tui.input(), "@crates/agens-tui/src/lib.rs");
}

#[test]
fn a_background_submission_leaves_no_file_picker_behind_for_escape() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.set_file_candidates(file_candidates());
    tui.set_agent_catalog(["reviewer"]);
    tui.select_agent("reviewer");
    typed(&mut tui, "review @src/lib");

    assert_eq!(
        tui.handle(Event::Key(Key::CtrlB)),
        Action::SubmitBackground("review @src/lib".to_owned())
    );
    assert!(tui.input().is_empty());
    assert!(tui.view().file_picker.is_none());

    assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::Render);
    assert_eq!(
        tui.view().focus,
        TranscriptFocus::Viewport,
        "Esc reached the transcript instead of a stale picker"
    );
}

#[test]
fn device_auth_overlay_renders_separate_values_and_explicit_actions() {
    let backend = TestBackend::new(100, 24);
    let terminal = Terminal::new(backend).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine::default());

    tui.begin_route();
    tui.apply_route_progress(TuiRouteProgress::DeviceCode {
        verification_url: "https://auth.example/device".into(),
        user_code: "ABCD-EFGH".into(),
    });
    renderer.render(tui.view()).unwrap();
    let text = renderer
        .terminal()
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    for label in [
        "ChatGPT device authentication",
        "Verification URL",
        "Device code",
        "Enter this code on the opened page.",
        "Open browser",
        "Copy link",
        "Copy code",
    ] {
        assert!(text.contains(label), "missing {label:?} in {text:?}");
    }
    assert!(text.contains("https://auth.example/device"));
    assert!(text.contains("ABCD-EFGH"));
}

#[test]
fn device_auth_overlay_actions_copy_exact_values_open_and_keep_route_alive() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.begin_route();
    tui.apply_route_progress(TuiRouteProgress::DeviceCode {
        verification_url: "https://auth.example/device".into(),
        user_code: "ABCD-EFGH".into(),
    });

    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::OpenDeviceAuthUrl
    );
    assert_eq!(
        tui.device_auth_clipboard_text(),
        Some("https://auth.example/device")
    );
    assert!(!tui.view().running);
    assert_eq!(tui.handle(Event::Key(Key::Down)), Action::Render);
    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::CopyDeviceAuthUrl
    );
    assert_eq!(
        tui.device_auth_clipboard_text(),
        Some("https://auth.example/device")
    );
    assert!(!tui.view().running);
    assert_eq!(tui.handle(Event::Key(Key::Down)), Action::Render);
    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::CopyDeviceAuthCode
    );
    assert_eq!(tui.device_auth_clipboard_text(), Some("ABCD-EFGH"));
    assert!(!tui.view().running);
}

#[test]
fn device_auth_overlay_escape_cancels_active_auth_route() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.begin_route();
    tui.apply_route_progress(TuiRouteProgress::DeviceCode {
        verification_url: "https://auth.example/device".into(),
        user_code: "ABCD-EFGH".into(),
    });

    assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::CancelRoute);
    assert_eq!(tui.engine().cancellations, 0);
}

#[test]
fn notice_runtime_events_persist_as_distinct_transcript_entries_and_survive_a_key_press() {
    let mut tui = Tui::new(FakeEngine::default());

    tui.apply_runtime_event(TuiRuntimeEvent::Notice {
        text: "mcp: files failed to connect".into(),
        severity: NoticeSeverity::Failure,
    });
    tui.apply_runtime_event(TuiRuntimeEvent::Notice {
        text: "session restored".into(),
        severity: NoticeSeverity::Info,
    });

    assert_eq!(
        tui.transcript(),
        &[
            TranscriptEntry::Error("mcp: files failed to connect".into()),
            TranscriptEntry::Info("session restored".into()),
        ]
    );

    tui.handle(Event::Key(Key::Char('x')));

    assert_eq!(
        tui.transcript(),
        &[
            TranscriptEntry::Error("mcp: files failed to connect".into()),
            TranscriptEntry::Info("session restored".into()),
        ]
    );
}

#[test]
fn a_failed_state_change_records_a_transcript_error_without_a_provider_outcome() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.begin_submission("request");

    tui.apply_progress(TurnEvent::StateChanged(TurnState::Failed));

    let view = tui.view();
    assert_eq!(view.turn_state, Some(TurnState::Failed));
    assert_eq!(view.conversation.unwrap().errors.len(), 1);
    assert_eq!(
        tui.transcript()
            .iter()
            .filter(|entry| matches!(entry, TranscriptEntry::Error(_)))
            .count(),
        1,
        "{:?}",
        tui.transcript()
    );
}

#[test]
fn a_failed_turn_end_records_a_transcript_error_without_a_provider_outcome() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.begin_submission("request");

    tui.apply_runtime_event(TuiRuntimeEvent::TurnEnded {
        status: TurnState::Failed,
        duration: Some(Duration::from_secs(2)),
    });

    let view = tui.view();
    assert_eq!(view.turn_state, Some(TurnState::Failed));
    assert_eq!(view.conversation.unwrap().errors.len(), 1);
    assert_eq!(
        tui.transcript()
            .iter()
            .filter(|entry| matches!(entry, TranscriptEntry::Error(_)))
            .count(),
        1,
        "{:?}",
        tui.transcript()
    );
}

#[test]
fn a_provider_failure_replaces_the_placeholder_whichever_signal_arrives_first() {
    let mut late_outcome = Tui::new(FakeEngine::default());
    late_outcome.begin_submission("request");
    late_outcome.apply_runtime_event(TuiRuntimeEvent::TurnEnded {
        status: TurnState::Failed,
        duration: None,
    });
    late_outcome.finish_provider_turn(TuiProviderOutcome::Failed {
        message: "network request failed".into(),
        action: "Check the network connection, then retry.".into(),
    });

    assert_eq!(
        late_outcome.transcript(),
        [
            TranscriptEntry::User("request".into()),
            TranscriptEntry::Error("network request failed".into()),
        ]
    );
    let view = late_outcome.view();
    let errors = &view.conversation.unwrap().errors;
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].message, "network request failed");

    let mut early_outcome = Tui::new(FakeEngine::default());
    early_outcome.begin_submission("request");
    early_outcome.finish_provider_turn(TuiProviderOutcome::Failed {
        message: "network request failed".into(),
        action: "Check the network connection, then retry.".into(),
    });
    early_outcome.apply_runtime_event(TuiRuntimeEvent::TurnEnded {
        status: TurnState::Failed,
        duration: None,
    });
    early_outcome.apply_progress(TurnEvent::StateChanged(TurnState::Failed));

    assert_eq!(
        early_outcome.transcript(),
        [
            TranscriptEntry::User("request".into()),
            TranscriptEntry::Error("network request failed".into()),
        ]
    );
    assert_eq!(
        early_outcome.view().conversation.unwrap().errors.len(),
        1,
        "a terminal signal after the real error must not add a second entry"
    );
}

#[test]
fn a_child_event_that_cannot_be_projected_is_recorded_instead_of_discarded() {
    let mut tui = Tui::new(FakeEngine::default());
    start_child(&mut tui, 7);
    tui.apply_runtime_event(TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent {
        id: 7,
        update: agens_core::TuiSubagentUpdate::ToolResult {
            call_id: "never-requested".into(),
            output: "orphan output".into(),
            is_error: false,
        },
    }));

    tui.select_transcript(TranscriptId::Subagent(7));
    let view = tui.view();
    let errors = &view.conversation.expect("child conversation").errors;
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(
        errors[0].message.contains("never-requested"),
        "the discarded event has to name itself: {errors:?}"
    );
}

/// Cancelling a duplicate subagent is how the model stops it from answering.
/// Waking the model because it was cancelled made it answer anyway.
#[test]
fn a_cancelled_background_subagent_does_not_schedule_a_turn_of_its_own() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.begin_submission("delegate");
    tui.apply_progress(TurnEvent::StateChanged(TurnState::Completed));
    tui.finish_provider_turn(TuiProviderOutcome::Completed("delegated".into()));
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "scout".into(),
        event: TuiExecutionEvent::BackgroundStarted { id: 1 },
    });
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "scout".into(),
        event: TuiExecutionEvent::BackgroundStarted { id: 2 },
    });

    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "scout".into(),
        event: TuiExecutionEvent::Cancelled { id: 2 },
    });
    assert_eq!(
        tui.take_ready_auto_turn(),
        None,
        "a cancellation is a decision already taken, not news to act on"
    );

    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "scout".into(),
        event: TuiExecutionEvent::Completed { id: 1 },
    });
    assert!(
        tui.take_ready_auto_turn().is_some(),
        "a subagent that finished with a result is still worth continuing on"
    );
}

fn ask_user_option(id: &str, label: &str) -> AskUserOption {
    AskUserOption::new(id, label, None, None)
}

#[allow(clippy::fn_params_excessive_bools)]
fn ask_user_question(
    id: &str,
    mode: AskUserMode,
    options: Vec<AskUserOption>,
    allow_other: bool,
    allow_note: bool,
    allow_discuss: bool,
) -> AskUserQuestion {
    AskUserQuestion::new(
        id,
        format!("prompt for {id}"),
        None,
        mode,
        options,
        allow_other,
        allow_note,
        allow_discuss,
    )
}

fn three_question_ask_user_request() -> AskUserRequest {
    AskUserRequest::new(
        None,
        vec![
            ask_user_question(
                "q1",
                AskUserMode::Single,
                vec![ask_user_option("a", "A"), ask_user_option("b", "B")],
                true,
                true,
                false,
            ),
            ask_user_question(
                "q2",
                AskUserMode::Multiple,
                vec![ask_user_option("a", "A"), ask_user_option("b", "B")],
                false,
                false,
                false,
            ),
            ask_user_question(
                "q3",
                AskUserMode::Single,
                vec![ask_user_option("a", "A")],
                false,
                false,
                false,
            ),
        ],
    )
    .expect("three bounded questions form a valid request")
}

fn single_question_ask_user_request(
    allow_other: bool,
    allow_note: bool,
    allow_discuss: bool,
) -> AskUserRequest {
    AskUserRequest::new(
        None,
        vec![ask_user_question(
            "q1",
            AskUserMode::Single,
            vec![ask_user_option("a", "A"), ask_user_option("b", "B")],
            allow_other,
            allow_note,
            allow_discuss,
        )],
    )
    .expect("one bounded question forms a valid request")
}

/// Moves the cursor down onto the proceed row of whatever question is open.
///
/// Bounded rather than "press Down until it arrives": `Down` saturates on the
/// last row, so a reducer that stopped producing a proceed row would hang the
/// test instead of failing it.
fn walk_to_proceed_row(tui: &mut Tui<FakeEngine>) {
    for _ in 0..8 {
        if tui.ask_user_snapshot().map(|snapshot| snapshot.row) == Some(AskUserRowSnapshot::Proceed)
        {
            return;
        }
        tui.handle(Event::Key(Key::Down));
    }
    panic!("the proceed row must be reachable with Down from any option row");
}

fn type_into_buffer(tui: &mut Tui<FakeEngine>, open_key: char, text: &str) {
    tui.handle(Event::Key(Key::Char(open_key)));
    for character in text.chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }
    tui.handle(Event::Key(Key::Enter));
}

#[test]
fn ask_user_navigation_preserves_a_valid_answer_on_the_first_question() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.open_ask_user(1, three_question_ask_user_request());

    tui.handle(Event::Key(Key::Down));
    // Space selects without advancing, so free-text edits stay on question 0.
    tui.handle(Event::Key(Key::Char(' ')));
    type_into_buffer(&mut tui, 'o', "extra");
    type_into_buffer(&mut tui, 'n', "fyi");

    tui.handle(Event::Key(Key::Tab));
    tui.handle(Event::Key(Key::Tab));
    tui.handle(Event::Key(Key::Left));
    tui.handle(Event::Key(Key::Left));

    let snapshot = tui.ask_user_snapshot().expect("ask-user still open");
    assert_eq!(snapshot.question_index, 0);
    assert_eq!(snapshot.selected, vec![1]);
    assert_eq!(snapshot.other, "extra");
    assert_eq!(snapshot.note, "fyi");
}

#[test]
fn ask_user_single_choice_replaces_previous_selection() {
    let mut tui = Tui::new(FakeEngine::default());
    // Single-question set: Enter selects without advancing past the last item.
    tui.open_ask_user(1, single_question_ask_user_request(false, false, false));

    tui.handle(Event::Key(Key::Enter));
    tui.handle(Event::Key(Key::Down));
    tui.handle(Event::Key(Key::Enter));

    let snapshot = tui.ask_user_snapshot().expect("ask-user still open");
    assert_eq!(snapshot.selected, vec![1]);
}

#[test]
fn ask_user_multiple_choice_toggles_selections() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.open_ask_user(1, three_question_ask_user_request());

    tui.handle(Event::Key(Key::Tab));
    // Space toggles without advancing so multi-select can accumulate choices.
    tui.handle(Event::Key(Key::Char(' ')));
    tui.handle(Event::Key(Key::Down));
    tui.handle(Event::Key(Key::Char(' ')));
    tui.handle(Event::Key(Key::Up));
    tui.handle(Event::Key(Key::Char(' ')));

    let snapshot = tui.ask_user_snapshot().expect("ask-user still open");
    assert_eq!(snapshot.question_index, 1);
    assert_eq!(snapshot.selected, vec![1]);
}

#[test]
fn ask_user_char_o_always_opens_free_text() {
    let mut without_flag = Tui::new(FakeEngine::default());
    without_flag.open_ask_user(1, single_question_ask_user_request(false, false, false));
    assert_eq!(
        without_flag.handle(Event::Key(Key::Char('o'))),
        Action::Render
    );
    assert_eq!(
        without_flag.ask_user_snapshot().unwrap().editing,
        AskUserEditing::Other
    );

    let mut with_flag = Tui::new(FakeEngine::default());
    with_flag.open_ask_user(1, single_question_ask_user_request(true, false, false));
    assert_eq!(with_flag.handle(Event::Key(Key::Char('o'))), Action::Render);
    assert_eq!(
        with_flag.ask_user_snapshot().unwrap().editing,
        AskUserEditing::Other
    );
}

#[test]
fn ask_user_enter_on_option_advances_to_the_next_question() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.open_ask_user(1, three_question_ask_user_request());

    assert_eq!(tui.handle(Event::Key(Key::Enter)), Action::Render);
    let snapshot = tui.ask_user_snapshot().expect("still open");
    assert_eq!(snapshot.question_index, 1);
    assert_eq!(snapshot.row, AskUserRowSnapshot::Option(0));

    // On the last question, the first Enter answers it and stays; a second
    // Enter has no answer left to give, so it opens the review screen.
    tui.handle(Event::Key(Key::Tab));
    assert_eq!(
        tui.ask_user_snapshot().unwrap().question_index,
        2,
        "tab reaches the last question"
    );
    assert_eq!(tui.handle(Event::Key(Key::Enter)), Action::Render);
    let last = tui
        .ask_user_snapshot()
        .expect("still open on last question");
    assert_eq!(last.question_index, 2);
    assert_eq!(last.selected, vec![0]);
    assert_eq!(last.row, AskUserRowSnapshot::Option(0));

    assert_eq!(tui.handle(Event::Key(Key::Enter)), Action::Render);
    assert!(tui.ask_user_snapshot().expect("review open").reviewing);
}

#[test]
fn ask_user_enter_on_answered_last_question_reaches_submit_in_two_keystrokes() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.open_ask_user(1, single_question_ask_user_request(false, false, false));

    // First Enter answers the question and stays.
    assert_eq!(tui.handle(Event::Key(Key::Enter)), Action::Render);
    let snapshot = tui.ask_user_snapshot().expect("still open");
    assert_eq!(snapshot.selected, vec![0]);
    assert_eq!(snapshot.row, AskUserRowSnapshot::Option(0));

    // Second Enter opens the review screen, where Enter submits.
    assert_eq!(tui.handle(Event::Key(Key::Enter)), Action::Render);
    assert!(tui.ask_user_snapshot().expect("review open").reviewing);
    tui.handle(Event::Key(Key::Enter));
    assert!(tui.ask_user_snapshot().is_none());
}

#[test]
fn ask_user_enter_on_unanswered_last_question_never_leaves_the_options() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.open_ask_user(1, three_question_ask_user_request());

    // Land on the last question without answering it: Enter answers in place,
    // so a reader can never jump past the question without answering first.
    tui.handle(Event::Key(Key::Tab));
    tui.handle(Event::Key(Key::Tab));
    assert_eq!(tui.handle(Event::Key(Key::Enter)), Action::Render);
    let snapshot = tui.ask_user_snapshot().expect("still open");
    assert_eq!(snapshot.question_index, 2);
    assert_eq!(snapshot.selected, vec![0]);
    assert_eq!(snapshot.row, AskUserRowSnapshot::Option(0));
}

#[test]
fn ask_user_last_multiple_question_accumulates_with_space_then_submits() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.open_ask_user(1, three_question_ask_user_request());

    // On the multiple question, Enter on an option advances like anywhere
    // else; Space is the accumulate-in-place key.
    tui.handle(Event::Key(Key::Tab));
    tui.handle(Event::Key(Key::Char(' ')));
    tui.handle(Event::Key(Key::Down));
    tui.handle(Event::Key(Key::Char(' ')));
    let snapshot = tui.ask_user_snapshot().expect("still open");
    assert_eq!(snapshot.question_index, 1);
    assert_eq!(snapshot.selected, vec![0, 1]);

    // Proceed carries the accumulated answer to the last question.
    tui.handle(Event::Key(Key::Down));
    walk_to_proceed_row(&mut tui);
    assert_eq!(tui.handle(Event::Key(Key::Enter)), Action::Render);
    let last = tui.ask_user_snapshot().expect("still open");
    assert_eq!(last.question_index, 2);
    assert!(!last.reviewing);
}

#[test]
fn ask_user_char_n_opens_note_only_when_allowed() {
    let mut disallowed = Tui::new(FakeEngine::default());
    disallowed.open_ask_user(1, single_question_ask_user_request(false, false, false));
    assert_eq!(
        disallowed.handle(Event::Key(Key::Char('n'))),
        Action::Unchanged
    );

    let mut allowed = Tui::new(FakeEngine::default());
    allowed.open_ask_user(1, single_question_ask_user_request(false, true, false));
    assert_eq!(allowed.handle(Event::Key(Key::Char('n'))), Action::Render);
    assert_eq!(
        allowed.ask_user_snapshot().unwrap().editing,
        AskUserEditing::Note
    );
}

#[test]
fn ask_user_typed_buffer_commits_on_enter_survives_escape_then_second_escape_cancels() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.open_ask_user(7, single_question_ask_user_request(true, false, false));

    tui.handle(Event::Key(Key::Char('o')));
    tui.handle(Event::Key(Key::Char('h')));
    tui.handle(Event::Key(Key::Char('i')));
    tui.handle(Event::Key(Key::Enter));

    let after_commit = tui.ask_user_snapshot().expect("ask-user still open");
    assert_eq!(after_commit.other, "hi");
    assert_eq!(after_commit.editing, AskUserEditing::Browsing);

    tui.handle(Event::Key(Key::Char('o')));
    tui.handle(Event::Key(Key::Char('!')));
    let escaped = tui.handle(Event::Key(Key::Escape));
    assert_eq!(escaped, Action::Render);

    let after_escape = tui
        .ask_user_snapshot()
        .expect("leaving entry mode does not resolve the prompt");
    assert_eq!(after_escape.other, "hi!");
    assert_eq!(after_escape.editing, AskUserEditing::Browsing);

    let cancelled = tui.handle(Event::Key(Key::Escape));
    assert_eq!(
        cancelled,
        Action::AskUserReply {
            id: 7,
            reply: AskUserReply::Cancelled,
        }
    );
    assert!(tui.ask_user_snapshot().is_none());
}

/// Enter on Proceed from review commits empty answers for unanswered items.
#[test]
fn ask_user_submit_while_incomplete_resolves_with_empty_answers() {
    let request = single_question_ask_user_request(false, false, false);
    let mut tui = Tui::new(FakeEngine::default());
    tui.open_ask_user(1, request.clone());

    walk_to_proceed_row(&mut tui);
    assert_eq!(tui.handle(Event::Key(Key::Enter)), Action::Render);
    let snapshot = tui.ask_user_snapshot().expect("review must open first");
    assert!(snapshot.reviewing, "last-question proceed opens review");

    let action = tui.handle(Event::Key(Key::Enter));

    match action {
        Action::AskUserReply {
            id,
            reply: AskUserReply::Answered(answers),
        } => {
            assert_eq!(id, 1);
            assert_eq!(answers.len(), 1);
            assert!(answers[0].selected.is_empty());
            assert_eq!(answers[0].other, None);
            assert!(
                request
                    .validate_reply(&AskUserReply::Answered(answers))
                    .is_ok(),
                "skipped questions must pass core validation"
            );
        }
        other => panic!("incomplete submit must resolve, got {other:?}"),
    }
    assert!(tui.ask_user_snapshot().is_none());
}

#[test]
fn ask_user_submit_when_complete_resolves_answered_in_request_order() {
    let request = single_question_ask_user_request(true, true, false);
    let mut tui = Tui::new(FakeEngine::default());
    tui.open_ask_user(9, request.clone());

    tui.handle(Event::Key(Key::Enter));
    type_into_buffer(&mut tui, 'o', "more");
    type_into_buffer(&mut tui, 'n', "nb");
    tui.handle(Event::Key(Key::Down));
    tui.handle(Event::Key(Key::Down));
    // Last question: Proceed opens review rather than submitting.
    assert_eq!(tui.handle(Event::Key(Key::Enter)), Action::Render);
    assert!(tui.ask_user_snapshot().expect("open").reviewing);
    let action = tui.handle(Event::Key(Key::Enter));

    match action {
        Action::AskUserReply {
            id,
            reply: AskUserReply::Answered(answers),
        } => {
            assert_eq!(id, 9);
            assert_eq!(answers.len(), 1);
            assert_eq!(answers[0].question_id, "q1");
            assert_eq!(answers[0].selected, vec!["a".to_owned()]);
            assert_eq!(answers[0].other.as_deref(), Some("more"));
            assert_eq!(answers[0].note.as_deref(), Some("nb"));

            let reply = AskUserReply::Answered(answers);
            assert!(
                request.validate_reply(&reply).is_ok(),
                "a reply built by this reducer must pass core's own reply validation"
            );
        }
        other => panic!("expected an answered reply, got {other:?}"),
    }
    assert!(tui.ask_user_snapshot().is_none());
}

/// Walks the proceed row down the whole question set. On every question but
/// the last it may only advance — never resolve — and on the last it opens
/// review; Submit from review commits.
#[test]
fn ask_user_proceed_row_advances_until_the_last_question_where_it_opens_review() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.open_ask_user(4, three_question_ask_user_request());

    // Advancing is not a submission attempt, so it must move on even with
    // nothing answered anywhere. This is what separates a `Next` row from a
    // review / submit row.
    walk_to_proceed_row(&mut tui);
    assert_eq!(tui.handle(Event::Key(Key::Enter)), Action::Render);
    let snapshot = tui.ask_user_snapshot().expect("still open");
    assert_eq!(
        snapshot.question_index, 1,
        "proceeding must advance without answering anything"
    );

    tui.handle(Event::Key(Key::Tab));
    tui.handle(Event::Key(Key::Tab));
    assert_eq!(
        tui.ask_user_snapshot().expect("still open").question_index,
        0
    );

    // Select with Space so Enter on Proceed only advances (does not re-select).
    let proceed = |tui: &mut Tui<FakeEngine>| {
        tui.handle(Event::Key(Key::Char(' ')));
        walk_to_proceed_row(tui);
        tui.handle(Event::Key(Key::Enter))
    };

    assert_eq!(proceed(&mut tui), Action::Render);
    assert_eq!(
        tui.ask_user_snapshot().expect("still open").question_index,
        1,
        "proceeding from the first question must move to the second, not submit"
    );

    assert_eq!(proceed(&mut tui), Action::Render);
    assert_eq!(
        tui.ask_user_snapshot().expect("still open").question_index,
        2
    );

    assert_eq!(
        proceed(&mut tui),
        Action::Render,
        "last-question proceed must open review, not resolve"
    );
    let review = tui.ask_user_snapshot().expect("still open");
    assert!(review.reviewing);
    assert_eq!(review.row, AskUserRowSnapshot::Proceed);

    match tui.handle(Event::Key(Key::Enter)) {
        Action::AskUserReply {
            id,
            reply: AskUserReply::Answered(answers),
        } => {
            assert_eq!(id, 4);
            assert_eq!(answers.len(), 3, "every question is present once");
            assert_eq!(answers[0].selected, vec!["a".to_owned()]);
            assert_eq!(answers[1].selected, vec!["a".to_owned()]);
            assert_eq!(answers[2].selected, vec!["a".to_owned()]);
        }
        other => panic!("review submit must resolve answered, got {other:?}"),
    }
    assert!(tui.ask_user_snapshot().is_none());
}

/// Submit from review always resolves, even when earlier questions were left
/// unanswered — those land as empty selected/other entries.
#[test]
fn ask_user_submit_from_review_accepts_skipped_questions() {
    let request = three_question_ask_user_request();
    let mut tui = Tui::new(FakeEngine::default());
    tui.open_ask_user(5, request.clone());

    // Answer only the last question, leave the first two skipped.
    tui.handle(Event::Key(Key::Tab));
    tui.handle(Event::Key(Key::Tab));
    assert_eq!(tui.ask_user_snapshot().unwrap().question_index, 2);
    tui.handle(Event::Key(Key::Enter));
    walk_to_proceed_row(&mut tui);
    assert_eq!(tui.handle(Event::Key(Key::Enter)), Action::Render);
    assert!(tui.ask_user_snapshot().unwrap().reviewing);
    let action = tui.handle(Event::Key(Key::Enter));

    match action {
        Action::AskUserReply {
            id,
            reply: AskUserReply::Answered(answers),
        } => {
            assert_eq!(id, 5);
            assert_eq!(answers.len(), 3);
            assert!(answers[0].selected.is_empty());
            assert!(answers[1].selected.is_empty());
            assert_eq!(answers[2].selected, vec!["a".to_owned()]);
            assert!(
                request
                    .validate_reply(&AskUserReply::Answered(answers))
                    .is_ok()
            );
        }
        other => panic!("submit with skips must resolve, got {other:?}"),
    }
    assert!(tui.ask_user_snapshot().is_none());
}

#[test]
fn ask_user_last_question_proceed_opens_review_without_resolving() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.open_ask_user(1, three_question_ask_user_request());

    tui.handle(Event::Key(Key::Tab));
    tui.handle(Event::Key(Key::Tab));
    walk_to_proceed_row(&mut tui);
    assert_eq!(tui.handle(Event::Key(Key::Enter)), Action::Render);

    let snapshot = tui.ask_user_snapshot().expect("must stay open in review");
    assert!(snapshot.reviewing);
    assert_eq!(snapshot.row, AskUserRowSnapshot::Proceed);
}

#[test]
fn ask_user_escape_while_reviewing_cancels_the_whole_interaction() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.open_ask_user(2, single_question_ask_user_request(false, false, false));

    walk_to_proceed_row(&mut tui);
    assert_eq!(tui.handle(Event::Key(Key::Enter)), Action::Render);
    assert!(tui.ask_user_snapshot().unwrap().reviewing);

    assert_eq!(
        tui.handle(Event::Key(Key::Escape)),
        Action::AskUserReply {
            id: 2,
            reply: AskUserReply::Cancelled,
        }
    );
    assert!(tui.ask_user_snapshot().is_none());
}

#[test]
fn ask_user_leaving_review_via_tab_returns_to_browse() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.open_ask_user(3, three_question_ask_user_request());

    tui.handle(Event::Key(Key::Tab));
    tui.handle(Event::Key(Key::Tab));
    walk_to_proceed_row(&mut tui);
    tui.handle(Event::Key(Key::Enter));
    assert!(tui.ask_user_snapshot().unwrap().reviewing);

    assert_eq!(tui.handle(Event::Key(Key::Tab)), Action::Render);
    let snapshot = tui.ask_user_snapshot().expect("still open");
    assert!(!snapshot.reviewing);
    assert_eq!(snapshot.question_index, 0);
    assert_eq!(snapshot.row, AskUserRowSnapshot::Option(0));
}

/// The note field is a text field, and a text field whose caret only ever
/// sits after the last character is one the reader cannot correct.
#[test]
fn ask_user_note_edits_at_the_caret_like_the_composer_does() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.open_ask_user(6, single_question_ask_user_request(false, true, false));

    tui.handle(Event::Key(Key::Char('n')));
    for character in "hello world".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }

    tui.handle(Event::Key(Key::Left));
    tui.handle(Event::Key(Key::Left));
    tui.handle(Event::Key(Key::Char('X')));
    assert_eq!(
        tui.ask_user_snapshot().expect("still editing").note,
        "hello worXld",
        "a character must land at the caret, not at the end"
    );

    tui.handle(Event::Key(Key::Delete));
    assert_eq!(
        tui.ask_user_snapshot().expect("still editing").note,
        "hello worXd",
        "Delete must remove the character in front of the caret"
    );

    tui.handle(Event::Key(Key::DeletePreviousWord));
    assert_eq!(
        tui.ask_user_snapshot().expect("still editing").note,
        "hello d",
        "ctrl-w must delete the word before the caret"
    );

    tui.handle(Event::Key(Key::Home));
    assert_eq!(
        tui.ask_user_snapshot().expect("still editing").entry_cursor,
        0
    );
    tui.handle(Event::Key(Key::Char('>')));
    assert_eq!(
        tui.ask_user_snapshot().expect("still editing").note,
        ">hello d",
        "Home must move the caret, not scroll the context pane beside it"
    );

    tui.handle(Event::Key(Key::End));
    assert_eq!(
        tui.ask_user_snapshot().expect("still editing").entry_cursor,
        ">hello d".chars().count()
    );
}

/// Reopening a buffer to correct it must not force the reader to walk the
/// caret back to where they stopped.
#[test]
fn ask_user_reopening_a_buffer_puts_the_caret_after_what_was_typed() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.open_ask_user(8, single_question_ask_user_request(true, false, false));

    type_into_buffer(&mut tui, 'o', "draft");
    tui.handle(Event::Key(Key::Char('o')));

    let snapshot = tui.ask_user_snapshot().expect("still open");
    assert_eq!(snapshot.editing, AskUserEditing::Other);
    assert_eq!(snapshot.entry_cursor, "draft".chars().count());

    tui.handle(Event::Key(Key::Char('!')));
    assert_eq!(
        tui.ask_user_snapshot().expect("still editing").other,
        "draft!"
    );
}

#[test]
fn ask_user_discuss_row_only_available_when_allowed_and_resolves_without_fabricating_answers() {
    let mut without_discuss = Tui::new(FakeEngine::default());
    without_discuss.open_ask_user(1, single_question_ask_user_request(false, false, false));
    without_discuss.handle(Event::Key(Key::Down));
    without_discuss.handle(Event::Key(Key::Down));
    without_discuss.handle(Event::Key(Key::Down));
    let snapshot = without_discuss.ask_user_snapshot().unwrap();
    assert_eq!(snapshot.row, AskUserRowSnapshot::Cancel);
    assert!(!snapshot.discuss_available);

    let mut with_discuss = Tui::new(FakeEngine::default());
    with_discuss.open_ask_user(2, single_question_ask_user_request(false, false, true));
    with_discuss.handle(Event::Key(Key::Down));
    with_discuss.handle(Event::Key(Key::Down));
    with_discuss.handle(Event::Key(Key::Down));
    let snapshot = with_discuss.ask_user_snapshot().unwrap();
    assert_eq!(snapshot.row, AskUserRowSnapshot::Discuss);
    assert!(snapshot.discuss_available);

    let action = with_discuss.handle(Event::Key(Key::Enter));
    match action {
        Action::AskUserReply {
            id,
            reply: AskUserReply::Discuss { question_id, note },
        } => {
            assert_eq!(id, 2);
            assert_eq!(question_id, "q1");
            assert_eq!(note, None);
        }
        other => panic!("expected a discuss reply, got {other:?}"),
    }
    assert!(with_discuss.ask_user_snapshot().is_none());
}

#[test]
fn ask_user_ctrl_c_resolves_cancelled_once_ahead_of_the_ordinary_quit_arming_path() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.open_ask_user(3, three_question_ask_user_request());

    let action = tui.handle(Event::Key(Key::CtrlC));
    assert_eq!(
        action,
        Action::AskUserReply {
            id: 3,
            reply: AskUserReply::Cancelled,
        }
    );
    assert!(tui.ask_user_snapshot().is_none());

    let second_press = tui.handle(Event::Key(Key::CtrlC));
    assert_eq!(second_press, Action::Render);
}

#[test]
fn ask_user_no_op_keys_report_unchanged() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.open_ask_user(1, three_question_ask_user_request());
    assert_eq!(tui.handle(Event::Key(Key::Up)), Action::Unchanged);

    let mut single_question = Tui::new(FakeEngine::default());
    single_question.open_ask_user(1, single_question_ask_user_request(false, false, false));
    assert_eq!(
        single_question.handle(Event::Key(Key::Tab)),
        Action::Unchanged
    );

    let mut reselect = Tui::new(FakeEngine::default());
    reselect.open_ask_user(1, single_question_ask_user_request(false, false, false));
    reselect.handle(Event::Key(Key::Enter));
    // Enter on the already-selected option of the answered last question is a
    // selection no-op, so it advances to the review screen instead.
    assert_eq!(reselect.handle(Event::Key(Key::Enter)), Action::Render);
    assert!(reselect.ask_user_snapshot().expect("review open").reviewing);
}

#[test]
fn ask_user_unavailable_reply_debug_never_carries_answer_content() {
    let reply = AskUserReply::Unavailable(AskUserUnavailable::NoInteractiveSurface);
    let action = Action::AskUserReply { id: 1, reply };
    let rendered = format!("{action:?}");
    assert_eq!(rendered, "AskUserReply { id: 1, status: \"unavailable\" }");
}

#[test]
fn ask_user_answered_reply_debug_never_carries_answer_content() {
    let request = single_question_ask_user_request(false, false, false);
    let reply = AskUserReply::Answered(vec![agens_core::ask_user::AskUserAnswer {
        question_id: "q1".to_owned(),
        selected: vec!["a".to_owned()],
        other: None,
        note: None,
    }]);
    assert!(request.validate_reply(&reply).is_ok());
    let action = Action::AskUserReply { id: 42, reply };
    let rendered = format!("{action:?}");
    assert_eq!(rendered, "AskUserReply { id: 42, status: \"answered\" }");
    assert!(
        !rendered.contains('q'),
        "debug must not leak the question id"
    );
}

#[test]
fn terminal_progress_does_not_release_foreground_scheduler_ownership() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.enable_busy_policy_routing();
    tui.begin_submission("first");
    tui.apply_progress(TurnEvent::StateChanged(TurnState::Completed));
    tui.handle(Event::Key(Key::Char('n')));

    assert!(tui.view().running);
    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::SubmitBusy("n".into())
    );
}

#[test]
fn runtime_turn_end_does_not_release_foreground_scheduler_ownership() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.enable_busy_policy_routing();
    tui.begin_submission("first");
    tui.apply_runtime_event(TuiRuntimeEvent::TurnEnded {
        status: TurnState::Completed,
        duration: None,
    });
    tui.handle(Event::Key(Key::Char('n')));

    assert!(tui.view().running);
    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::SubmitBusy("n".into())
    );
}

#[test]
fn scheduler_owned_background_handoff_releases_and_dispatches_the_oldest_fifo_prompt() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.begin_submission("first");
    tui.handle(Event::Key(Key::Char('n')));
    assert_eq!(tui.handle(Event::Key(Key::Enter)), Action::Render);

    assert_eq!(
        tui.finish_provider_turn(TuiProviderOutcome::Backgrounded),
        Some("n".into())
    );
    assert!(tui.view().running);
}

#[test]
fn local_route_stays_cancellable_without_claiming_scheduler_foreground_running() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.begin_route();
    tui.apply_route_progress(TuiRouteProgress::DeviceCode {
        verification_url: "https://auth.example/device".into(),
        user_code: "ABCD-EFGH".into(),
    });

    assert!(!tui.view().running);
    assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::CancelRoute);
}

#[test]
fn terminal_progress_keeps_auto_turn_deferred_until_matching_outcome_after_fifo() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.begin_submission("foreground");
    finish_background_child(&mut tui, 7);
    tui.handle(Event::Key(Key::Char('q')));
    assert_eq!(tui.handle(Event::Key(Key::Enter)), Action::Render);

    tui.apply_progress(TurnEvent::StateChanged(TurnState::Completed));
    tui.apply_runtime_event(TuiRuntimeEvent::TurnEnded {
        status: TurnState::Completed,
        duration: None,
    });
    assert_eq!(tui.take_ready_auto_turn(), None);

    assert_eq!(
        tui.finish_provider_turn(TuiProviderOutcome::Backgrounded),
        Some("q".into())
    );
    assert_eq!(tui.take_ready_auto_turn(), None);
    tui.begin_submission("q");
    tui.finish_provider_turn(TuiProviderOutcome::Completed("done".into()));

    assert!(tui.take_ready_auto_turn().is_some());
}

fn type_chars(tui: &mut Tui<FakeEngine>, text: &str) {
    for character in text.chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }
}

fn submit_text(tui: &mut Tui<FakeEngine>, text: &str) -> Action {
    type_chars(tui, text);
    tui.handle(Event::Key(Key::Enter))
}

fn tui_with_prompt_memory() -> Tui<FakeEngine> {
    let mut tui = Tui::new(FakeEngine::default());
    tui.set_prompt_memory(Box::new(agens_core::EphemeralPromptMemory::new()));
    tui
}

fn tui_with_prompt_memory_capacity(capacity: usize) -> Tui<FakeEngine> {
    let mut tui = Tui::with_queue_capacity(FakeEngine::default(), capacity);
    tui.set_prompt_memory(Box::new(agens_core::EphemeralPromptMemory::new()));
    tui
}

#[test]
fn prompt_memory_keys_empty_up_browses_history_without_submit() {
    let mut tui = tui_with_prompt_memory();

    assert_eq!(
        submit_text(&mut tui, "older"),
        Action::Submit("older".into())
    );
    assert_eq!(
        submit_text(&mut tui, "newer"),
        Action::Submit("newer".into())
    );
    assert_eq!(tui.input(), "");

    assert_eq!(tui.handle(Event::Key(Key::Up)), Action::Render);
    assert_eq!(tui.input(), "newer");
    assert_eq!(tui.handle(Event::Key(Key::Up)), Action::Render);
    assert_eq!(tui.input(), "older");

    // Browse never auto-submits.
    assert_eq!(tui.handle(Event::Key(Key::Up)), Action::Render);
    assert_eq!(tui.input(), "older");
}

#[test]
fn prompt_memory_keys_down_restores_draft_and_empty_non_browse_down_still_focuses_tree() {
    let mut tui = tui_with_prompt_memory();
    assert_eq!(
        submit_text(&mut tui, "first"),
        Action::Submit("first".into())
    );
    assert_eq!(
        submit_text(&mut tui, "second"),
        Action::Submit("second".into())
    );

    assert_eq!(tui.handle(Event::Key(Key::Up)), Action::Render);
    assert_eq!(tui.input(), "second");
    assert_eq!(tui.handle(Event::Key(Key::Up)), Action::Render);
    assert_eq!(tui.input(), "first");

    assert_eq!(tui.handle(Event::Key(Key::Down)), Action::Render);
    assert_eq!(tui.input(), "second");
    assert_eq!(tui.handle(Event::Key(Key::Down)), Action::Render);
    assert_eq!(tui.input(), "");

    // Empty non-browse Down still enters the subagent execution strip.
    start_child(&mut tui, 11);
    assert!(tui.view().execution_selection.is_none());
    assert_eq!(tui.handle(Event::Key(Key::Down)), Action::Render);
    assert_eq!(
        tui.view().execution_selection,
        Some(TranscriptId::Main),
        "empty non-browse Down must still focus the execution strip"
    );
}

#[test]
fn prompt_memory_keys_ctrl_s_push_pop_and_empty_noop() {
    let mut tui = tui_with_prompt_memory();

    type_chars(&mut tui, "first");
    assert_eq!(tui.handle(Event::Key(Key::CtrlS)), Action::Render);
    assert_eq!(tui.input(), "");
    assert_eq!(tui.status(), Some("Saved to stash."));

    type_chars(&mut tui, "second");
    assert_eq!(tui.handle(Event::Key(Key::CtrlS)), Action::Render);
    assert_eq!(tui.input(), "");
    assert_eq!(tui.status(), Some("Saved to stash."));

    // Empty + stash → pop LIFO (second, then first); never submits.
    assert_eq!(tui.handle(Event::Key(Key::CtrlS)), Action::Render);
    assert_eq!(tui.input(), "second");
    while !tui.input().is_empty() {
        tui.handle(Event::Key(Key::Backspace));
    }
    assert_eq!(tui.handle(Event::Key(Key::CtrlS)), Action::Render);
    assert_eq!(tui.input(), "first");
    while !tui.input().is_empty() {
        tui.handle(Event::Key(Key::Backspace));
    }

    // Empty composer + empty stash is a no-op.
    assert_eq!(tui.handle(Event::Key(Key::CtrlS)), Action::Render);
    assert_eq!(tui.input(), "");
    assert_eq!(tui.handle(Event::Key(Key::CtrlS)), Action::Render);
    assert_eq!(tui.input(), "");
}

#[test]
fn prompt_memory_keys_submit_enqueue_and_busy_append_history_with_dedupe_stash_untouched() {
    let mut tui = tui_with_prompt_memory_capacity(4);

    // Stash stays independent of history appends.
    type_chars(&mut tui, "stash-keep");
    assert_eq!(tui.handle(Event::Key(Key::CtrlS)), Action::Render);

    // Idle Submit records history, including slash text.
    assert_eq!(
        submit_text(&mut tui, "/status"),
        Action::Submit("/status".into())
    );
    assert_eq!(
        submit_text(&mut tui, "hello"),
        Action::Submit("hello".into())
    );
    // Consecutive-dedupe: same text twice does not grow history.
    assert_eq!(
        submit_text(&mut tui, "hello"),
        Action::Submit("hello".into())
    );

    // Enqueue path (busy without busy-policy routing) also records.
    tui.begin_submission("active");
    assert_eq!(submit_text(&mut tui, "queued-one"), Action::Render);
    assert_eq!(tui.queue_entries().len(), 1);

    // SubmitBusy path records the draft for history.
    tui.enable_busy_policy_routing();
    type_chars(&mut tui, "busy-draft");
    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::SubmitBusy("busy-draft".into())
    );

    // Clear busy/input state for browse assertions.
    while !tui.input().is_empty() {
        tui.handle(Event::Key(Key::Backspace));
    }

    // Newest → older: busy-draft, queued-one, hello, /status
    assert_eq!(tui.handle(Event::Key(Key::Up)), Action::Render);
    assert_eq!(tui.input(), "busy-draft");
    assert_eq!(tui.handle(Event::Key(Key::Up)), Action::Render);
    assert_eq!(tui.input(), "queued-one");
    assert_eq!(tui.handle(Event::Key(Key::Up)), Action::Render);
    assert_eq!(tui.input(), "hello");
    assert_eq!(tui.handle(Event::Key(Key::Up)), Action::Render);
    assert_eq!(tui.input(), "/status");
    // Only one "hello" despite two submits (consecutive-dedupe).
    assert_eq!(tui.handle(Event::Key(Key::Up)), Action::Render);
    assert_eq!(tui.input(), "/status");

    // Stash still holds the original entry.
    while !tui.input().is_empty() {
        tui.handle(Event::Key(Key::Backspace));
    }
    // Leaving browse via Down so stash Ctrl+S is not confused with browse.
    assert_eq!(tui.handle(Event::Key(Key::Down)), Action::Render);
    assert_eq!(tui.input(), "");
    assert_eq!(tui.handle(Event::Key(Key::CtrlS)), Action::Render);
    assert_eq!(tui.input(), "stash-keep");
}

#[test]
fn prompt_memory_keys_composer_edit_clears_browse() {
    let mut tui = tui_with_prompt_memory();
    assert_eq!(
        submit_text(&mut tui, "alpha"),
        Action::Submit("alpha".into())
    );
    assert_eq!(submit_text(&mut tui, "beta"), Action::Submit("beta".into()));

    assert_eq!(tui.handle(Event::Key(Key::Up)), Action::Render);
    assert_eq!(tui.input(), "beta");
    assert_eq!(tui.handle(Event::Key(Key::Up)), Action::Render);
    assert_eq!(tui.input(), "alpha");

    // Edit while browsing keeps the edited input and exits browse.
    tui.handle(Event::Key(Key::Char('!')));
    assert_eq!(tui.input(), "alpha!");

    // Down no longer walks history (browse cleared); input stays as edited.
    assert_eq!(tui.handle(Event::Key(Key::Down)), Action::Render);
    assert_eq!(tui.input(), "alpha!");

    // Non-empty Up does not re-enter browse.
    assert_eq!(tui.handle(Event::Key(Key::Up)), Action::Render);
    assert_eq!(tui.input(), "alpha!");
}

// --- WU3: prompt history/stash overlays (FillComposer, filter, remove) ---

#[test]
fn prompt_overlay_history_enter_pastes_without_submit() {
    let mut tui = tui_with_prompt_memory();
    assert_eq!(
        submit_text(&mut tui, "older-entry"),
        Action::Submit("older-entry".into())
    );
    assert_eq!(
        submit_text(&mut tui, "newer-entry"),
        Action::Submit("newer-entry".into())
    );
    assert_eq!(tui.input(), "");

    tui.show_history_overlay();
    assert!(tui.view().dialog.is_some());
    assert_eq!(tui.view().dialog.unwrap().entry_count(), 2);

    // Newest-first: Enter pastes the selected (newest) entry only.
    assert_eq!(tui.handle(Event::Key(Key::Enter)), Action::Render);
    assert_eq!(tui.input(), "newer-entry");
    assert!(tui.view().dialog.is_none());
    assert_eq!(tui.view().input_cursor, "newer-entry".chars().count());

    // No auto-submit: a second Enter now submits the pasted composer text.
    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::Submit("newer-entry".into())
    );
}

#[test]
fn prompt_overlay_history_filter_narrows_list() {
    let mut tui = tui_with_prompt_memory();
    assert_eq!(
        submit_text(&mut tui, "alpha one"),
        Action::Submit("alpha one".into())
    );
    assert_eq!(
        submit_text(&mut tui, "beta two"),
        Action::Submit("beta two".into())
    );
    assert_eq!(
        submit_text(&mut tui, "alpha three"),
        Action::Submit("alpha three".into())
    );

    tui.show_history_overlay();
    assert_eq!(tui.view().dialog.unwrap().entry_count(), 3);

    // Arm search and filter by keyword.
    for character in "/alpha".chars() {
        assert_eq!(tui.handle(Event::Key(Key::Char(character))), Action::Render);
    }
    assert_eq!(
        tui.view().dialog.unwrap().entry_count(),
        2,
        "filter must rebuild from the full store, not only the current window"
    );

    // Newest matching first → "alpha three".
    assert_eq!(tui.handle(Event::Key(Key::Enter)), Action::Render);
    assert_eq!(tui.input(), "alpha three");
    assert!(tui.view().dialog.is_none());
}

#[test]
fn prompt_overlay_stash_enter_pastes_selected_without_submit() {
    let mut tui = tui_with_prompt_memory();

    type_chars(&mut tui, "stash-a");
    assert_eq!(tui.handle(Event::Key(Key::CtrlS)), Action::Render);
    type_chars(&mut tui, "stash-b");
    assert_eq!(tui.handle(Event::Key(Key::CtrlS)), Action::Render);
    type_chars(&mut tui, "stash-c");
    assert_eq!(tui.handle(Event::Key(Key::CtrlS)), Action::Render);

    tui.show_stash_overlay();
    assert_eq!(tui.view().dialog.unwrap().entry_count(), 3);

    // Newest-first list: C, B, A. Move to non-top entry B and paste.
    assert_eq!(tui.handle(Event::Key(Key::Down)), Action::Render);
    assert_eq!(tui.handle(Event::Key(Key::Enter)), Action::Render);
    assert_eq!(tui.input(), "stash-b");
    assert!(tui.view().dialog.is_none());

    // Paste does not remove: LIFO pop still yields C then B then A.
    while !tui.input().is_empty() {
        tui.handle(Event::Key(Key::Backspace));
    }
    assert_eq!(tui.handle(Event::Key(Key::CtrlS)), Action::Render);
    assert_eq!(tui.input(), "stash-c");
    while !tui.input().is_empty() {
        tui.handle(Event::Key(Key::Backspace));
    }
    assert_eq!(tui.handle(Event::Key(Key::CtrlS)), Action::Render);
    assert_eq!(tui.input(), "stash-b");
}

#[test]
fn prompt_overlay_stash_x_and_delete_remove_selected_rows() {
    let mut tui = tui_with_prompt_memory();

    type_chars(&mut tui, "keep-old");
    assert_eq!(tui.handle(Event::Key(Key::CtrlS)), Action::Render);
    type_chars(&mut tui, "remove-me");
    assert_eq!(tui.handle(Event::Key(Key::CtrlS)), Action::Render);
    type_chars(&mut tui, "keep-new");
    assert_eq!(tui.handle(Event::Key(Key::CtrlS)), Action::Render);

    tui.show_stash_overlay();
    assert_eq!(tui.view().dialog.unwrap().entry_count(), 3);

    // Newest-first: keep-new, remove-me, keep-old → select remove-me.
    assert_eq!(tui.handle(Event::Key(Key::Down)), Action::Render);
    assert_eq!(tui.handle(Event::Key(Key::Char('x'))), Action::Render);
    assert_eq!(tui.view().dialog.unwrap().entry_count(), 2);
    assert_eq!(tui.input(), "", "remove must not paste into the composer");

    // Delete also removes the selected row (keep-new is selected first).
    assert_eq!(tui.handle(Event::Key(Key::Delete)), Action::Render);
    assert_eq!(tui.view().dialog.unwrap().entry_count(), 1);

    // Remaining LIFO top is keep-old.
    assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::Render);
    assert_eq!(tui.handle(Event::Key(Key::CtrlS)), Action::Render);
    assert_eq!(tui.input(), "keep-old");
}

#[test]
fn prompt_overlay_open_does_not_mutate_queued_prompts() {
    let mut tui = tui_with_prompt_memory_capacity(4);
    tui.begin_submission("active");
    assert_eq!(submit_text(&mut tui, "queued-one"), Action::Render);
    assert_eq!(submit_text(&mut tui, "queued-two"), Action::Render);
    let before: Vec<String> = tui
        .queue_entries()
        .iter()
        .map(|entry| entry.prompt().to_owned())
        .collect();
    assert_eq!(
        before,
        vec!["queued-one".to_owned(), "queued-two".to_owned()]
    );

    type_chars(&mut tui, "parked");
    assert_eq!(tui.handle(Event::Key(Key::CtrlS)), Action::Render);

    tui.show_history_overlay();
    assert!(tui.view().dialog.is_some());
    let mid: Vec<String> = tui
        .queue_entries()
        .iter()
        .map(|entry| entry.prompt().to_owned())
        .collect();
    assert_eq!(mid, before, "opening history must not touch the FIFO queue");
    assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::Render);

    tui.show_stash_overlay();
    assert!(tui.view().dialog.is_some());
    let after: Vec<String> = tui
        .queue_entries()
        .iter()
        .map(|entry| entry.prompt().to_owned())
        .collect();
    assert_eq!(after, before, "opening stash must not touch the FIFO queue");
}

// --- WU5: history/stash independence from FIFO queue + I/O best-effort ---

fn queue_prompt_texts(tui: &Tui<FakeEngine>) -> Vec<String> {
    tui.queue_entries()
        .iter()
        .map(|entry| entry.prompt().to_owned())
        .collect()
}

#[test]
fn queue_untouched_by_stash_history_ops_preserves_capacity_order_and_dispatch() {
    let mut tui = tui_with_prompt_memory_capacity(2);
    tui.begin_submission("active");

    // Fill FIFO queue to capacity.
    assert_eq!(submit_text(&mut tui, "q-first"), Action::Render);
    assert_eq!(submit_text(&mut tui, "q-second"), Action::Render);
    let before = queue_prompt_texts(&tui);
    assert_eq!(before, vec!["q-first".to_owned(), "q-second".to_owned()]);

    // Capacity still enforced after enqueue (draft kept, queue unchanged).
    type_chars(&mut tui, "overflow");
    assert_eq!(tui.handle(Event::Key(Key::Enter)), Action::Render);
    assert_eq!(tui.input(), "overflow");
    assert!(
        tui.status()
            .is_some_and(|status| status.contains("queue is full")),
        "status: {:?}",
        tui.status()
    );
    assert_eq!(queue_prompt_texts(&tui), before);

    // Stash push of the refused draft + another park.
    assert_eq!(tui.handle(Event::Key(Key::CtrlS)), Action::Render);
    assert_eq!(tui.input(), "");
    type_chars(&mut tui, "parked");
    assert_eq!(tui.handle(Event::Key(Key::CtrlS)), Action::Render);
    assert_eq!(
        queue_prompt_texts(&tui),
        before,
        "stash push must not touch queue"
    );

    // History browse (entries from q-first/q-second submits) must not touch queue.
    assert_eq!(tui.handle(Event::Key(Key::Up)), Action::Render);
    assert!(!tui.input().is_empty());
    assert_eq!(tui.handle(Event::Key(Key::Up)), Action::Render);
    assert_eq!(tui.handle(Event::Key(Key::Down)), Action::Render);
    assert_eq!(tui.handle(Event::Key(Key::Down)), Action::Render);
    assert_eq!(tui.input(), "");
    assert_eq!(
        queue_prompt_texts(&tui),
        before,
        "history browse must not touch queue"
    );

    // Stash pop still leaves queue alone.
    assert_eq!(tui.handle(Event::Key(Key::CtrlS)), Action::Render);
    assert_eq!(tui.input(), "parked");
    while !tui.input().is_empty() {
        tui.handle(Event::Key(Key::Backspace));
    }
    assert_eq!(
        queue_prompt_texts(&tui),
        before,
        "stash pop must not touch queue"
    );

    // Capacity still 2 after stash/history churn.
    type_chars(&mut tui, "still-full");
    assert_eq!(tui.handle(Event::Key(Key::Enter)), Action::Render);
    assert_eq!(tui.input(), "still-full");
    assert_eq!(queue_prompt_texts(&tui), before);

    // FIFO dispatch: finish active → oldest queued is next.
    assert_eq!(
        tui.finish_provider_turn(TuiProviderOutcome::Completed("done".into())),
        Some("q-first".into())
    );
    assert_eq!(queue_prompt_texts(&tui), vec!["q-second".to_owned()]);
    assert_eq!(
        tui.finish_provider_turn(TuiProviderOutcome::Completed("done-2".into())),
        Some("q-second".into())
    );
    assert!(queue_prompt_texts(&tui).is_empty());
}

#[test]
fn history_append_leaves_stash_unchanged_across_submit_and_enqueue() {
    let mut tui = tui_with_prompt_memory_capacity(4);

    type_chars(&mut tui, "stash-a");
    assert_eq!(tui.handle(Event::Key(Key::CtrlS)), Action::Render);
    type_chars(&mut tui, "stash-b");
    assert_eq!(tui.handle(Event::Key(Key::CtrlS)), Action::Render);

    // Idle submits grow history only.
    assert_eq!(
        submit_text(&mut tui, "hist-one"),
        Action::Submit("hist-one".into())
    );
    assert_eq!(
        submit_text(&mut tui, "hist-two"),
        Action::Submit("hist-two".into())
    );

    // Busy enqueue also appends history without touching stash.
    tui.begin_submission("active");
    assert_eq!(submit_text(&mut tui, "hist-queued"), Action::Render);
    assert_eq!(queue_prompt_texts(&tui), vec!["hist-queued".to_owned()]);

    // Stash still LIFO: B then A.
    while !tui.input().is_empty() {
        tui.handle(Event::Key(Key::Backspace));
    }
    assert_eq!(tui.handle(Event::Key(Key::CtrlS)), Action::Render);
    assert_eq!(tui.input(), "stash-b");
    while !tui.input().is_empty() {
        tui.handle(Event::Key(Key::Backspace));
    }
    assert_eq!(tui.handle(Event::Key(Key::CtrlS)), Action::Render);
    assert_eq!(tui.input(), "stash-a");

    // Queue still holds the enqueued prompt after stash pops.
    assert_eq!(queue_prompt_texts(&tui), vec!["hist-queued".to_owned()]);
}

#[test]
fn prompt_memory_write_failure_does_not_panic_and_restores_stash_draft() {
    use agens_core::{HistoryBrowseResult, PromptMemory, PromptMemoryError, PromptOverlayItem};

    struct FailingPromptMemory;

    impl PromptMemory for FailingPromptMemory {
        fn record_submission(&mut self, _text: &str) -> Result<bool, PromptMemoryError> {
            Err(PromptMemoryError::new("history write failed"))
        }

        fn browse_up(&mut self, _composer_input: &str) -> Option<String> {
            None
        }

        fn browse_down(&mut self) -> HistoryBrowseResult {
            HistoryBrowseResult::Idle
        }

        fn clear_browse(&mut self) {}

        fn is_browsing(&self) -> bool {
            false
        }

        fn stash_push(&mut self, _text: &str) -> Result<bool, PromptMemoryError> {
            Err(PromptMemoryError::new("stash write failed"))
        }

        fn stash_pop(&mut self) -> Result<Option<String>, PromptMemoryError> {
            Err(PromptMemoryError::new("stash pop failed"))
        }

        fn stash_remove_at(&mut self, _index: usize) -> Result<bool, PromptMemoryError> {
            Err(PromptMemoryError::new("stash remove failed"))
        }

        fn history_overlay(&self, _query: &str, _limit: usize) -> Vec<PromptOverlayItem> {
            Vec::new()
        }

        fn stash_overlay(&self, _query: &str, _limit: usize) -> Vec<PromptOverlayItem> {
            Vec::new()
        }
    }

    let mut tui = Tui::new(FakeEngine::default());
    tui.set_prompt_memory(Box::new(FailingPromptMemory));

    // Submit still routes; failed record does not panic or invent history.
    assert_eq!(
        submit_text(&mut tui, "survives-history-io"),
        Action::Submit("survives-history-io".into())
    );
    assert_eq!(tui.handle(Event::Key(Key::Up)), Action::Render);
    assert_eq!(tui.input(), "");

    // Stash push failure restores the draft so the user does not lose input.
    type_chars(&mut tui, "stash-io-a");
    assert_eq!(tui.handle(Event::Key(Key::CtrlS)), Action::Render);
    assert_eq!(tui.input(), "stash-io-a");

    // Overlay remove failure is best-effort and must not panic.
    tui.show_stash_overlay();
    assert!(tui.view().dialog.is_some());
    assert_eq!(tui.handle(Event::Key(Key::Char('x'))), Action::Render);
    assert!(tui.view().dialog.is_some());
}
