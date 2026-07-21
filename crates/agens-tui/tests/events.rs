use agens_core::{HeadlessTurnCancellation, MessagePart, TurnEvent, TurnState};
use agens_tui::{
    Action, AppEvent, AppState, BridgeCancel, BridgeTx, Command, Conversation, ConversationError,
    ConversationEvent, Dialog, DialogEntry, DialogView, DiffLine, DiffLineKind, Effect, Engine,
    Event, Key, PaletteEntry, PaletteEntryKind, PublishOutcome, RatatuiRenderer, Renderer, Runtime,
    TranscriptEntry, Tui, TuiExecutionEvent, TuiPermissionBridge, TuiPermissionReply,
    TuiPresentation, TuiProviderOutcome, TuiRouteProgress, TuiRuntimeEvent, TuiSubmissionOutcome,
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
fn selection_dialog_search_edits_navigates_filtered_rows_and_clears_before_closing() {
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
    assert!(tui.view().dialog.is_some());
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
