use agens_tools::{TaskExecutionEvent, TaskExecutionId, TaskLaunchMode};
use agens_tui::{
    BridgeCancel, BridgeTx, PendingPermissions, PermissionReply, PublishOutcome, TerminalControl,
    TerminalModeGuard, TerminalOperation, teardown,
};
use std::{
    io, thread,
    time::{Duration, Instant},
};
#[derive(Default)]
struct Control {
    calls: Vec<&'static str>,
    fail: Option<&'static str>,
}
impl Control {
    fn call(&mut self, operation: &'static str) -> io::Result<()> {
        self.calls.push(operation);
        (self.fail != Some(operation)).then_some(()).ok_or_else(|| {
            let kind = if operation == "keyboard-on" {
                io::ErrorKind::Unsupported
            } else {
                io::ErrorKind::Other
            };
            io::Error::new(kind, "injected")
        })
    }
}
impl TerminalControl for Control {
    fn apply(&mut self, operation: TerminalOperation) -> io::Result<()> {
        self.call(match operation {
            TerminalOperation::EnableRaw => "raw-on",
            TerminalOperation::DisableRaw => "raw-off",
            TerminalOperation::EnterAlternate => "alternate-on",
            TerminalOperation::LeaveAlternate => "alternate-off",
            TerminalOperation::EnableMouse => "mouse-on",
            TerminalOperation::DisableMouse => "mouse-off",
            TerminalOperation::EnableKeyboardEnhancement => "keyboard-on",
            TerminalOperation::DisableKeyboardEnhancement => "keyboard-off",
            TerminalOperation::EnablePaste => "paste-on",
            TerminalOperation::DisablePaste => "paste-off",
        })
    }
}
fn assert_calls(control: &Control, expected: &str) {
    assert_eq!(control.calls.join(","), expected);
}
#[test]
fn teardown_guards_reverse_activated_modes_and_clean_partial_setup() {
    let mut control = Control::default();
    let mut guard = TerminalModeGuard::enter(&mut control).unwrap();
    control.fail = Some("mouse-off");
    assert!(guard.restore(&mut control).is_err());
    assert_calls(
        &control,
        "raw-on,alternate-on,mouse-on,keyboard-on,paste-on,paste-off,keyboard-off,mouse-off,alternate-off,raw-off",
    );

    let mut control = Control {
        calls: Vec::new(),
        fail: Some("mouse-on"),
    };
    assert!(TerminalModeGuard::enter(&mut control).is_err());
    assert_calls(
        &control,
        "raw-on,alternate-on,mouse-on,alternate-off,raw-off",
    );

    let mut control = Control {
        calls: Vec::new(),
        fail: Some("paste-on"),
    };
    assert!(TerminalModeGuard::enter(&mut control).is_err());
    assert_calls(
        &control,
        "raw-on,alternate-on,mouse-on,keyboard-on,paste-on,keyboard-off,mouse-off,alternate-off,raw-off",
    );
}

#[test]
fn unsupported_keyboard_enhancement_does_not_break_startup_or_restoration() {
    let mut control = Control {
        calls: Vec::new(),
        fail: Some("keyboard-on"),
    };

    let mut guard = TerminalModeGuard::enter(&mut control).unwrap();
    guard.restore(&mut control).unwrap();

    assert_calls(
        &control,
        "raw-on,alternate-on,mouse-on,keyboard-on,paste-on,paste-off,mouse-off,alternate-off,raw-off",
    );
}
#[test]
fn teardown_wakes_blocked_publishers_after_receiver_invalidation() {
    let (bridge, _receiver) = BridgeTx::bounded(1);
    let cancellation = BridgeCancel::new();
    assert_eq!(
        bridge.publish("occupied", &cancellation, None),
        PublishOutcome::Published { ordinal: 0 }
    );
    let sender = bridge.clone();
    let cancel = cancellation.clone();
    let waiting = thread::spawn(move || sender.publish("blocked", &cancel, None));
    thread::sleep(Duration::from_millis(10));
    bridge.close();
    assert_eq!(waiting.join().unwrap(), PublishOutcome::Closed);
}
#[test]
fn teardown_drains_permissions_fail_closed_once_and_bounds_the_worker_wait() {
    let (bridge, _receiver) = BridgeTx::<()>::bounded(1);
    let cancellation = BridgeCancel::new();
    let mut pending = PendingPermissions::default();
    let cancelled = pending.register(1);
    let expired = pending.register(2);
    assert_eq!(pending.drain(PermissionReply::DeadlineExpired), 2);
    assert_eq!(expired.recv().unwrap(), PermissionReply::DeadlineExpired);
    assert_eq!(pending.drain(PermissionReply::Cancelled), 0);
    assert!(!pending.reply(2, PermissionReply::Cancelled));
    let pending_reply = pending.register(3);
    let deadline = Instant::now() + Duration::from_millis(20);
    assert!(!teardown(
        &bridge,
        &cancellation,
        &mut pending,
        deadline,
        |remaining| {
            assert!(remaining <= Duration::from_millis(20));
            assert_eq!(
                pending_reply.try_recv().unwrap(),
                PermissionReply::Cancelled
            );
            false
        }
    ));
    assert_eq!(cancelled.recv().unwrap(), PermissionReply::DeadlineExpired);
    assert_eq!(
        bridge.publish((), &BridgeCancel::new(), None),
        PublishOutcome::Closed
    );
}

#[test]
fn u15_c1b_terminal_execution_expires_at_sixty_seconds_without_removing_the_catalog() {
    struct Engine;
    impl agens_tui::Engine for Engine {
        fn cancel(&mut self) {}
    }

    let mut tui = agens_tui::Tui::new(Engine);
    tui.set_agent_catalog(["reviewer"]);
    tui.apply_runtime_event(agens_tui::TuiRuntimeEvent::TaskExecution {
        agent: "reviewer".into(),
        event: TaskExecutionEvent::Admitted(
            TaskExecutionId::from_u64(1),
            TaskLaunchMode::Foreground,
        ),
    });
    tui.apply_runtime_event(agens_tui::TuiRuntimeEvent::TaskExecution {
        agent: "reviewer".into(),
        event: TaskExecutionEvent::Cancelled(TaskExecutionId::from_u64(1)),
    });

    tui.tick(Duration::from_nanos(59_999_999_999));
    assert_eq!(tui.executions().len(), 1);
    tui.tick(Duration::from_secs(60));

    assert!(tui.executions().is_empty());
    assert_eq!(tui.agent_catalog(), ["main", "reviewer"]);
}
