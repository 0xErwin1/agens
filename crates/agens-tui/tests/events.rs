use agens_core::{HeadlessTurnCancellation, MessagePart, TurnEvent, TurnState};
use agens_tui::{
    Action, AppEvent, AppState, BridgeCancel, BridgeTx, Command, Conversation, ConversationError,
    ConversationEvent, Dialog, DialogEntry, DialogView, DiffLine, DiffLineKind, Effect, Engine,
    Event, Key, PaletteEntry, PaletteEntryKind, PublishOutcome, RatatuiRenderer, Renderer, Runtime,
    TranscriptEntry, TranscriptFocus, TranscriptId, Tui, TuiExecutionEvent, TuiExecutionState,
    TuiPermissionBridge, TuiPermissionReply, TuiPresentation, TuiProviderOutcome, TuiRouteProgress,
    TuiRuntimeEvent, TuiSubagentErrorKind, TuiSubagentEvent, TuiSubagentStatus,
    TuiSubmissionOutcome,
};
use ratatui::{Terminal, backend::TestBackend};
use std::{
    thread,
    time::{Duration, Instant},
};

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
            TuiSubagentEvent::terminal(id, TuiSubagentStatus::Success, format!("final-{id}")),
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
            TuiSubagentStatus::Success,
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
            TuiSubagentEvent::terminal(id, TuiSubagentStatus::Success, format!("final-{id}")),
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
        messages: vec![
            agens_core::Message {
                role: agens_core::Role::User,
                parts: vec![MessagePart::Text("restored prompt".into())],
            },
            agens_core::Message {
                role: agens_core::Role::Assistant,
                parts: vec![MessagePart::Text("restored summary".into())],
            },
        ],
    });

    let view = tui.view();
    assert_eq!(view.active_transcript, TranscriptId::Main);
    assert_eq!(view.transcript_ids, vec![TranscriptId::Main]);
    assert_eq!(view.completed_conversations.len(), 1);
    assert!(view.conversation.is_none());
    assert!(tui.transcript_record(&TranscriptId::Subagent(7)).is_none());
}

#[test]
fn transcript_navigation_restores_destination_focus_and_disables_child_composer() {
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

    tui.handle(Event::Key(Key::Escape));
    assert_eq!(tui.handle(Event::Key(Key::Char('g'))), Action::Render);
    assert!(tui.view().dialog.is_some());
    tui.handle(Event::Key(Key::Enter));
    assert_eq!(tui.view().active_transcript, TranscriptId::Subagent(7));
    tui.handle(Event::Key(Key::Char('h')));
    assert_eq!(tui.view().active_transcript, TranscriptId::Subagent(7));
    tui.handle(Event::Key(Key::Char('l')));
    assert_eq!(tui.view().active_transcript, TranscriptId::Subagent(8));
    tui.handle(Event::Key(Key::Char('l')));
    assert_eq!(tui.view().active_transcript, TranscriptId::Subagent(8));
    tui.handle(Event::Key(Key::Char('m')));
    assert_eq!(tui.view().active_transcript, TranscriptId::Main);
    tui.select_transcript(TranscriptId::Subagent(7));
    assert_eq!(tui.handle(Event::Key(Key::Char('x'))), Action::Render);
    assert_eq!(tui.input(), "");
    assert_eq!(tui.view().focus, TranscriptFocus::Viewport);
    tui.set_collapse_thinking(true);
    tui.handle(Event::Key(Key::CtrlO));
    tui.handle(Event::Key(Key::PageUp));
    assert!(tui.view().collapse_thinking);
    assert!(tui.view().scroll_offset > 0);
    assert!(!tui.view().collapsed_tool_outputs.contains("seven"));
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
    assert!(tui.view().collapsed_tool_outputs.is_empty());
    tui.handle(Event::Key(Key::Char('m')));
    assert_eq!(tui.input(), "m");

    tui.handle(Event::Key(Key::Escape));
    tui.handle(Event::Key(Key::Char('l')));
    assert!(tui.view().collapse_thinking);
    assert!(!tui.view().following_bottom);
    assert_eq!(tui.handle(Event::Paste(" blocked".into())), Action::Render);
    assert_eq!(tui.input(), "m");
    assert_eq!(tui.handle(Event::Key(Key::Enter)), Action::Render);
    assert_eq!(tui.input(), "m");
    tui.handle(Event::Key(Key::Char('l')));
    tui.handle(Event::Key(Key::CtrlO));
    tui.handle(Event::Key(Key::PageUp));
    let child_eight_offset = tui.view().scroll_offset;
    assert_ne!(child_eight_offset, child_seven_offset);
    assert!(!tui.view().collapsed_tool_outputs.contains("eight"));
    assert!(!tui.view().collapsed_tool_outputs.contains("seven"));

    tui.handle(Event::Key(Key::Char('h')));
    assert_eq!(tui.view().scroll_offset, child_seven_offset);
    assert!(!tui.view().collapsed_tool_outputs.contains("seven"));

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
        TuiSubagentEvent::terminal(7, TuiSubagentStatus::Success, "done"),
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
    assert!(tui.view().dialog.is_some());
    tui.handle(Event::Key(Key::Escape));

    assert_eq!(tui.handle(Event::Key(Key::Char('l'))), Action::Render);
    assert_eq!(tui.view().active_transcript, TranscriptId::Subagent(8));
    assert_eq!(tui.handle(Event::Key(Key::Char('h'))), Action::Render);
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

    tui.handle(Event::Key(Key::Escape));
    tui.handle(Event::Key(Key::Char('l')));
    assert_eq!(tui.view().active_transcript, TranscriptId::Subagent(7));
    tui.handle(Event::Key(Key::CtrlO));
    tui.handle(Event::Key(Key::PageUp));
    let child_seven = (
        tui.view().following_bottom,
        tui.view().scroll_offset,
        tui.view().focus,
        tui.view().collapsed_tool_outputs.contains("seven"),
    );

    tui.handle(Event::Key(Key::Char('l')));
    assert_eq!(tui.view().active_transcript, TranscriptId::Subagent(8));
    assert!(tui.view().following_bottom);
    assert_eq!(tui.view().focus, TranscriptFocus::Viewport);
    assert!(!tui.view().collapsed_tool_outputs.contains("seven"));
    tui.handle(Event::Key(Key::CtrlO));
    tui.handle(Event::Key(Key::PageUp));
    let child_eight_offset = tui.view().scroll_offset;

    tui.handle(Event::Key(Key::Char('h')));
    assert_eq!(tui.view().active_transcript, TranscriptId::Subagent(7));
    assert_eq!(
        (
            tui.view().following_bottom,
            tui.view().scroll_offset,
            tui.view().focus,
            tui.view().collapsed_tool_outputs.contains("seven"),
        ),
        child_seven
    );

    tui.handle(Event::Key(Key::Char('l')));
    assert_eq!(tui.view().scroll_offset, child_eight_offset);
    assert!(!tui.view().collapsed_tool_outputs.contains("eight"));
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

    assert!(tui.view().collapsed_tool_outputs.contains("read-1"));
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
    assert!(!tui.view().collapsed_tool_outputs.contains("read-1"));
    assert!(
        tui.view().conversation.unwrap().tool_batches[0].calls[0]
            .result
            .as_ref()
            .unwrap()
            .output
            .contains("retained-tail-sentinel")
    );

    tui.handle(Event::Key(Key::End));

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

    tui.handle(Event::Key(Key::CtrlO));
    assert!(tui.view().collapsed_tool_outputs.contains("read-1"));
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
            7,
            "call-a",
            "native::read",
            "alpha",
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
        TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::error(7, TuiSubagentErrorKind::Tool)),
        TuiRuntimeEvent::TaskExecution {
            agent: "reviewer".into(),
            event: TuiExecutionEvent::Failed { id: 7 },
        },
        TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::terminal(
            7,
            TuiSubagentStatus::Failure,
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
            TuiSubagentStatus::Failure,
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
    assert!(parent.contains("Subagent 7 · reviewer"));
    assert!(!parent.contains("child-reasoning"));
    assert!(!parent.contains("child-partial"));
    assert!(!parent.contains("result-a"));
    assert!(!parent.contains("Subagent tool execution failed."));

    tui.select_transcript(TranscriptId::Subagent(7));
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
        "call-a",
        "call-b",
        "result-b",
        "result-a",
        "Subagent tool execution failed.",
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
    assert_eq!(main.matches("Subagent 7 · reviewer").count(), 1, "{main:?}");
    assert_eq!(main.matches("Subagent 8 · writer").count(), 1, "{main:?}");
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

    tui.handle(Event::Key(Key::Char('l')));
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
        .apply(ConversationEvent::Diff(vec![DiffLine::new(
            7,
            DiffLineKind::Added,
            "+ typed",
        )]))
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
fn reducer_starts_idle_prompt_and_persists_only_after_success() {
    let mut app = AppState::new(2);

    assert_eq!(
        app.reduce(AppEvent::SubmitPrompt("first".into())),
        vec![Effect::StartPrompt("first".into())]
    );
    assert_eq!(app.runtime(), &Runtime::Running);
    assert!(app.completed_history().is_empty());

    assert_eq!(
        app.reduce(AppEvent::TurnCompleted("answer".into())),
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
        app.reduce(AppEvent::TurnCompleted("one".into())),
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
            "A response is already in progress.".into()
        )]
    );
    assert_eq!(app.queued_prompts(), ["queued"]);
    assert!(app.completed_history().is_empty());
}

#[test]
fn reducer_terminal_failures_start_the_oldest_queued_prompt_before_later_submissions() {
    for terminal_event in [AppEvent::TurnCancelled, AppEvent::TurnFailed] {
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
        app.reduce(AppEvent::Key(Key::CtrlC, Instant::now())),
        vec![Effect::CancelTurn]
    );
    assert_eq!(app.dialog(), Some(&Dialog::Command));

    app.set_dialog(None);
    assert_eq!(
        app.reduce(AppEvent::Key(Key::Char('x'), Instant::now())),
        vec![Effect::ComposerEdited]
    );
    assert_eq!(app.composer(), "draftx");
}

#[test]
fn command_control_c_follows_running_composer_warning_exit_and_disarm_states() {
    let mut app = AppState::new(1);
    let now = Instant::now();
    app.reduce(AppEvent::SubmitPrompt("running".into()));
    assert_eq!(
        app.reduce(AppEvent::Command(Command::ControlC, now)),
        vec![Effect::CancelTurn]
    );
    app.reduce(AppEvent::TurnCancelled);
    app.set_composer("draft");
    assert_eq!(
        app.reduce(AppEvent::Command(Command::ControlC, now)),
        vec![Effect::Render]
    );
    assert_eq!(app.composer(), "");
    assert_eq!(
        app.reduce(AppEvent::Command(Command::ControlC, now)),
        vec![Effect::ExitWarning]
    );
    assert_eq!(
        app.reduce(AppEvent::Command(Command::Navigate, now)),
        vec![Effect::Render]
    );
    assert_eq!(
        app.reduce(AppEvent::Command(Command::ControlC, now)),
        vec![Effect::ExitWarning]
    );
    assert_eq!(
        app.reduce(AppEvent::Command(
            Command::ControlC,
            now + Duration::from_secs(1)
        )),
        vec![Effect::Quit]
    );
    assert_eq!(
        app.reduce(AppEvent::Command(
            Command::ControlC,
            now + Duration::from_secs(3)
        )),
        vec![Effect::ExitWarning]
    );
    assert_eq!(
        app.reduce(AppEvent::TimerTick(now + Duration::from_secs(6))),
        vec![Effect::Render]
    );
    assert_eq!(
        app.reduce(AppEvent::Command(
            Command::ControlC,
            now + Duration::from_secs(6)
        )),
        vec![Effect::ExitWarning]
    );
}

#[test]
fn exit_warning_is_disarmed_by_composer_edits_and_all_runtime_terminal_events() {
    let now = Instant::now();

    let mut composer = AppState::new(1);
    assert_eq!(
        composer.reduce(AppEvent::Command(Command::ControlC, now)),
        vec![Effect::ExitWarning]
    );
    composer.set_composer("");
    assert_eq!(
        composer.reduce(AppEvent::Command(
            Command::ControlC,
            now + Duration::from_secs(1)
        )),
        vec![Effect::ExitWarning]
    );

    for terminal_event in [
        AppEvent::TurnCompleted("answer".into()),
        AppEvent::TurnCancelled,
        AppEvent::TurnFailed,
    ] {
        let mut app = AppState::new(1);
        assert_eq!(
            app.reduce(AppEvent::Command(Command::ControlC, now)),
            vec![Effect::ExitWarning]
        );
        assert_eq!(
            app.reduce(AppEvent::SubmitPrompt("running".into())),
            vec![Effect::StartPrompt("running".into())]
        );
        app.reduce(terminal_event);

        assert_eq!(
            app.reduce(AppEvent::Command(
                Command::ControlC,
                now + Duration::from_secs(1)
            )),
            vec![Effect::ExitWarning]
        );
    }
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
    app.reduce(AppEvent::TurnCompleted("answer".into()));
    app.set_composer("replacement draft");
    app.set_dialog(Some(Dialog::Command));

    assert_eq!(app.queued_prompts(), ["second queued"]);

    assert_eq!(app.reduce(AppEvent::ResetSucceeded), vec![Effect::Render]);
    assert_eq!(app, AppState::new(2));

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
        waiting_bridge.wait_for_reply("native::bash", "git status", &waiting_cancellation)
    });

    let request = requests.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(bridge.close());
    assert!(!bridge.close());

    assert_eq!(waiting.join().unwrap(), TuiPermissionReply::Cancelled);
    assert!(!bridge.reply(request.id(), TuiPermissionReply::AllowAlways));

    let (bridge, requests) = TuiPermissionBridge::channel();
    let expired = HeadlessTurnCancellation::with_deadline(Duration::from_millis(100));
    let expired_bridge = bridge.clone();
    let expired_wait = thread::spawn(move || {
        expired_bridge.wait_for_reply("native::write", "README.md", &expired)
    });
    let expired_request = requests.recv_timeout(Duration::from_secs(1)).unwrap();

    assert_eq!(
        expired_wait.join().unwrap(),
        TuiPermissionReply::DeadlineExpired
    );
    assert!(!bridge.reply(expired_request.id(), TuiPermissionReply::AllowAlways));

    let allowed = HeadlessTurnCancellation::new();
    let allowed_bridge = bridge.clone();
    let allowed_wait = thread::spawn(move || {
        allowed_bridge.wait_for_reply("native::write", "README.md", &allowed)
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
    assert_eq!(tui.input(), "");
    assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Render);
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
            TuiSubagentStatus::Success,
        ),
        (
            2,
            TuiExecutionEvent::Failed { id: 2 },
            TuiSubagentStatus::Failure,
        ),
        (
            3,
            TuiExecutionEvent::Cancelled { id: 3 },
            TuiSubagentStatus::Cancelled,
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
            TuiSubagentEvent::terminal(id, TuiSubagentStatus::Success, "late terminal"),
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
        TuiSubagentEvent::terminal(4, TuiSubagentStatus::Success, "secret=final-secret"),
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

    for character in "sec".chars() {
        assert_eq!(tui.handle(Event::Key(Key::Char(character))), Action::Render);
    }
    assert_eq!(tui.input(), "d");
    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::DialogAction("second".into())
    );
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

    for character in "gpt-5.6".chars() {
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
    for character in "model-too-long".chars() {
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
    for character in "bad*model".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }

    assert_eq!(tui.handle(Event::Key(Key::Enter)), Action::Render);
}

#[test]
fn selection_dialog_search_edits_navigates_filtered_rows_and_closes_on_first_escape() {
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

    for character in "Option 1x".chars() {
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
    for character in "alpha".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }
    tui.handle(Event::Key(Key::DeletePreviousWord));
    assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::Render);
    assert!(tui.view().dialog.is_none());

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
fn session_dialog_toggles_scope_preserves_search_and_dispatches_the_filtered_selection() {
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
    tui.show_selection_dialog(DialogView::sessions(
        vec![current.clone()],
        vec![current, other],
    ));

    for character in "reviewer".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }
    assert_eq!(tui.handle(Event::Key(Key::Enter)), Action::Render);

    assert_eq!(tui.handle(Event::Key(Key::LineStart)), Action::Render);
    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::DialogAction("session:9".into())
    );
}

#[test]
fn selection_dialog_escape_control_c_empty_and_disabled_states_never_dispatch() {
    for cancel_key in [Key::Escape, Key::CtrlC] {
        let mut tui = Tui::new(FakeEngine::default());
        tui.show_selection_dialog(DialogView::selection(
            "Confirm",
            None::<String>,
            vec![DialogEntry::action("Proceed", "proceed")],
        ));

        assert_eq!(tui.handle(Event::Key(cancel_key)), Action::Render);
        assert!(tui.view().dialog.is_none());
        assert_eq!(tui.engine().cancellations, 0);
    }

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
    assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Cancel);
    assert_eq!(tui.engine().cancellations, 1);

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
            TranscriptEntry::Error("[redacted]".into()),
        ]
    );
    let view = failure.view();
    assert_eq!(view.turn_state, Some(TurnState::Failed));
    assert_eq!(view.conversation.unwrap().errors.len(), 1);
    assert_eq!(view.conversation.unwrap().errors[0].message, "[redacted]");
    assert!(view.conversation.unwrap().final_markdown.is_none());
}

#[test]
fn submission_start_resets_footer_metrics() {
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

    assert!(tui.view().latest_usage.is_none());
    assert!(tui.view().turn_duration.is_none());
}

#[test]
fn second_submission_is_rejected_while_a_turn_owns_cancellation() {
    let mut tui = Tui::new(FakeEngine::default());

    tui.begin_submission("first prompt");
    assert_eq!(tui.handle(Event::Key(Key::Char('s'))), Action::Render);
    assert_eq!(tui.handle(Event::Key(Key::Enter)), Action::Render);
    assert_eq!(tui.input(), "s");
    assert_eq!(
        tui.transcript(),
        [
            agens_tui::TranscriptEntry::User("first prompt".into()),
            agens_tui::TranscriptEntry::Info("A response is already in progress.".into()),
        ]
    );
    assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Cancel);
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
fn control_c_cancels_a_running_turn_before_quitting() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.set_running(true);

    assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Cancel);
    assert_eq!(tui.engine().cancellations, 1);
    assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Cancel);
    assert_eq!(tui.engine().cancellations, 2);
}

#[test]
fn repeated_control_c_quits_only_when_idle_and_input_is_empty() {
    let mut tui = Tui::new(FakeEngine::default());

    assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Render);
    assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Quit);
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
    assert!(!tui.view().running);
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
    assert_eq!(tui.handle(Event::Key(Key::End)), Action::Render);
    assert!(tui.following_bottom());
    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::Submit("ab".into())
    );
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

        assert_eq!(buffer.area.width, width);
        assert_eq!(buffer.area.height, height);
        assert!(buffer.content.iter().any(|cell| cell.symbol() == "a"));
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

    assert!(text.contains("agens"));
    assert!(text.contains("openai-api / gpt-4.1"));
    assert!(text.contains("session #42"));
    assert!(text.contains("You"));
    assert!(text.contains("Thinking"));
    assert!(text.contains("Compose"));
    assert!(text.contains("2 lines"));
    assert!(text.contains("Shift+Enter"));
    assert!(text.contains("LIVE"));

    let user_cell = buffer
        .content
        .iter()
        .find(|cell| cell.symbol() == "Y")
        .expect("user role label is rendered");
    assert_eq!(user_cell.fg, ratatui::style::Color::Cyan);

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
    assert!(tool_text.contains("native::read"));

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

    assert!(narrow_text.contains("agens"));
    assert!(narrow_text.contains("Enter"));
    assert!(!narrow_text.contains("Shift+Enter"));
    assert!(narrow_text.contains("Compose"));
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

    assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Cancel);
    renderer.render(tui.view()).unwrap();
    let cancelling = renderer
        .terminal()
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(cancelling.contains("Cancelling"));

    tui.apply_progress(TurnEvent::StateChanged(TurnState::Failed));
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
