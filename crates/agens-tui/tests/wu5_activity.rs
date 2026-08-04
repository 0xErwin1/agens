use agens_tui::{
    Action, Engine, Event, Key, SurfaceFocus, Tui, TuiExecutionEvent, TuiExecutionState,
    TuiRuntimeEvent,
};

#[derive(Default)]
struct FakeEngine;

impl Engine for FakeEngine {
    fn cancel(&mut self) {}
}

fn start_execution(tui: &mut Tui<FakeEngine>, id: u64, background: bool) {
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: format!("agent-{id}"),
        event: if background {
            TuiExecutionEvent::BackgroundStarted { id }
        } else {
            TuiExecutionEvent::ForegroundStarted { id }
        },
    });
}

#[test]
fn activity_focus_lists_hidden_work_and_requests_selected_or_all_cancellation() {
    let mut tui = Tui::new(FakeEngine);
    start_execution(&mut tui, 7, false);
    start_execution(&mut tui, 8, true);

    assert_eq!(tui.executions().len(), 2);
    assert_eq!(tui.handle(Event::Key(Key::Tab)), Action::Render);
    assert_eq!(tui.handle(Event::Key(Key::Tab)), Action::Render);
    assert_eq!(tui.view().surface_focus, SurfaceFocus::Activity);

    assert_eq!(tui.handle(Event::Key(Key::Down)), Action::Render);
    assert_eq!(tui.handle(Event::Key(Key::Down)), Action::Render);
    assert_eq!(
        tui.handle(Event::Key(Key::Char('x'))),
        Action::CancelExecution(8)
    );
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "agent-8".into(),
        event: TuiExecutionEvent::CancellationRequested { id: 8 },
    });
    assert_eq!(
        tui.executions()[0].state(),
        TuiExecutionState::CancellationRequested
    );

    assert_eq!(
        tui.handle(Event::Key(Key::Char('X'))),
        Action::CancelAllExecutions
    );
}

#[test]
fn activity_ignores_stale_or_terminal_cancellation_updates() {
    let mut tui = Tui::new(FakeEngine);
    start_execution(&mut tui, 7, false);
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "agent-7".into(),
        event: TuiExecutionEvent::Completed { id: 7 },
    });
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "agent-7".into(),
        event: TuiExecutionEvent::CancellationRequested { id: 7 },
    });
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "unknown".into(),
        event: TuiExecutionEvent::CancellationRequested { id: 99 },
    });

    assert_eq!(
        tui.executions()[0].state(),
        TuiExecutionState::CompletedRecent
    );
    assert_eq!(tui.executions().len(), 1);
}

#[test]
fn activity_marks_only_registry_confirmed_cancel_all_ids_as_pending() {
    let mut tui = Tui::new(FakeEngine);
    start_execution(&mut tui, 7, false);
    start_execution(&mut tui, 8, true);
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "agent-8".into(),
        event: TuiExecutionEvent::Completed { id: 8 },
    });

    tui.apply_confirmed_cancellations([7, 99, 8]);

    assert_eq!(
        tui.executions()
            .iter()
            .find(|execution| execution.id() == 7)
            .unwrap()
            .state(),
        TuiExecutionState::CancellationRequested
    );
    assert_eq!(
        tui.executions()
            .iter()
            .find(|execution| execution.id() == 8)
            .unwrap()
            .state(),
        TuiExecutionState::CompletedRecent
    );
}
