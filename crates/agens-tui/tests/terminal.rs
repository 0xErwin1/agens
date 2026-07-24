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
            TerminalOperation::HideCursor => "cursor-hide",
            TerminalOperation::ShowCursor => "cursor-show",
            TerminalOperation::EnableMouse => "mouse-on",
            TerminalOperation::DisableMouse => "mouse-off",
            TerminalOperation::EnableKeyboardEnhancement => "keyboard-on",
            TerminalOperation::DisableKeyboardEnhancement => "keyboard-off",
            TerminalOperation::EnablePaste => "paste-on",
            TerminalOperation::DisablePaste => "paste-off",
        })
    }
}

#[derive(Default)]
struct CleanupFailureControl;

impl TerminalControl for CleanupFailureControl {
    fn apply(&mut self, operation: TerminalOperation) -> io::Result<()> {
        match operation {
            TerminalOperation::DisablePaste => Err(io::Error::other("paste cleanup")),
            TerminalOperation::ShowCursor => Err(io::Error::other("cursor cleanup")),
            _ => Ok(()),
        }
    }
}
fn assert_calls(control: &Control, expected: &str) {
    assert_eq!(control.calls.join(","), expected);
}
#[test]
fn teardown_guards_reverse_activated_modes_and_clean_partial_setup() {
    let mut control = Control::default();
    let mut guard = TerminalModeGuard::enter(&mut control).unwrap();
    guard.restore(&mut control).unwrap();
    assert_calls(
        &control,
        "raw-on,alternate-on,cursor-hide,mouse-on,keyboard-on,paste-on,paste-off,keyboard-off,mouse-off,cursor-show,alternate-off,raw-off",
    );

    let mut control = Control {
        calls: Vec::new(),
        fail: Some("cursor-hide"),
    };
    assert!(TerminalModeGuard::enter(&mut control).is_err());
    assert_calls(
        &control,
        "raw-on,alternate-on,cursor-hide,cursor-show,alternate-off,raw-off",
    );

    let mut control = Control {
        calls: Vec::new(),
        fail: Some("paste-on"),
    };
    assert!(TerminalModeGuard::enter(&mut control).is_err());
    assert_calls(
        &control,
        "raw-on,alternate-on,cursor-hide,mouse-on,keyboard-on,paste-on,keyboard-off,mouse-off,cursor-show,alternate-off,raw-off",
    );
}

#[test]
fn mouse_capture_is_enabled_at_startup_and_disabled_once_during_cleanup() {
    let mut control = Control::default();
    let mut guard = TerminalModeGuard::enter(&mut control).unwrap();

    guard.restore(&mut control).unwrap();
    guard.restore(&mut control).unwrap();

    assert_calls(
        &control,
        "raw-on,alternate-on,cursor-hide,mouse-on,keyboard-on,paste-on,paste-off,keyboard-off,mouse-off,cursor-show,alternate-off,raw-off",
    );
}

#[test]
fn failed_mouse_enable_restores_prior_modes_without_disabling_inactive_capture() {
    let mut control = Control {
        calls: Vec::new(),
        fail: Some("mouse-on"),
    };

    assert!(TerminalModeGuard::enter(&mut control).is_err());

    assert_calls(
        &control,
        "raw-on,alternate-on,cursor-hide,mouse-on,cursor-show,alternate-off,raw-off",
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
        "raw-on,alternate-on,cursor-hide,mouse-on,keyboard-on,paste-on,paste-off,mouse-off,cursor-show,alternate-off,raw-off",
    );
}

#[test]
fn cursor_is_restored_exactly_once_after_repeated_cleanup() {
    let mut control = Control::default();
    let mut guard = TerminalModeGuard::enter(&mut control).unwrap();

    guard.restore(&mut control).unwrap();
    guard.restore(&mut control).unwrap();

    assert_eq!(
        control
            .calls
            .iter()
            .filter(|operation| **operation == "cursor-show")
            .count(),
        1
    );
}

#[test]
fn cleanup_preserves_the_first_error_while_restoring_later_modes() {
    let mut control = CleanupFailureControl;
    let mut guard = TerminalModeGuard::enter(&mut control).unwrap();

    let error = guard.restore(&mut control).unwrap_err();

    assert_eq!(error.to_string(), "paste cleanup");
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
