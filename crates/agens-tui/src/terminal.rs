use std::{
    collections::BTreeMap,
    io,
    sync::mpsc::{self, Receiver, Sender},
    time::{Duration, Instant},
};

use crate::bridge::{BridgeCancel, BridgeTx};
pub fn teardown<T>(
    bridge: &BridgeTx<T>,
    cancellation: &BridgeCancel,
    permissions: &mut PendingPermissions,
    deadline: Instant,
    wait_for_worker: impl FnOnce(Duration) -> bool,
) -> bool {
    cancellation.cancel();
    bridge.close();
    permissions.drain(PermissionReply::Cancelled);
    wait_for_worker(deadline.saturating_duration_since(Instant::now()))
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PermissionReply {
    Cancelled,
    DeadlineExpired,
}
#[derive(Default)]
pub struct PendingPermissions {
    pending: BTreeMap<u64, Sender<PermissionReply>>,
}
impl PendingPermissions {
    pub fn register(&mut self, id: u64) -> Receiver<PermissionReply> {
        let (sender, receiver) = mpsc::channel();
        self.pending.insert(id, sender);
        receiver
    }
    pub fn reply(&mut self, id: u64, reply: PermissionReply) -> bool {
        self.pending
            .remove(&id)
            .is_some_and(|sender| sender.send(reply).is_ok())
    }
    pub fn drain(&mut self, reply: PermissionReply) -> usize {
        let pending = std::mem::take(&mut self.pending);
        let count = pending.len();
        for sender in pending.into_values() {
            let _ = sender.send(reply);
        }
        count
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalOperation {
    EnableRaw,
    DisableRaw,
    EnterAlternate,
    LeaveAlternate,
    EnableMouse,
    DisableMouse,
    EnableKeyboardEnhancement,
    DisableKeyboardEnhancement,
    EnablePaste,
    DisablePaste,
}
pub trait TerminalControl {
    fn apply(&mut self, operation: TerminalOperation) -> io::Result<()>;
}
pub struct TerminalModeGuard {
    raw: bool,
    alternate: bool,
    mouse: bool,
    keyboard_enhancement: bool,
    paste: bool,
}
impl TerminalModeGuard {
    pub fn enter(control: &mut impl TerminalControl) -> io::Result<Self> {
        control.apply(TerminalOperation::EnableRaw)?;
        let mut guard = Self {
            raw: true,
            alternate: false,
            mouse: false,
            keyboard_enhancement: false,
            paste: false,
        };
        if let Err(error) = control.apply(TerminalOperation::EnterAlternate) {
            let _ = guard.restore(control);
            return Err(error);
        }
        guard.alternate = true;
        if let Err(error) = control.apply(TerminalOperation::EnableMouse) {
            let _ = guard.restore(control);
            return Err(error);
        }
        guard.mouse = true;
        match control.apply(TerminalOperation::EnableKeyboardEnhancement) {
            Ok(()) => guard.keyboard_enhancement = true,
            Err(error) if error.kind() == io::ErrorKind::Unsupported => {}
            Err(error) => {
                let _ = guard.restore(control);
                return Err(error);
            }
        }
        match control.apply(TerminalOperation::EnablePaste) {
            Ok(()) => guard.paste = true,
            Err(error) if error.kind() == io::ErrorKind::Unsupported => {}
            Err(error) => {
                let _ = guard.restore(control);
                return Err(error);
            }
        }
        Ok(guard)
    }
    pub fn restore(&mut self, control: &mut impl TerminalControl) -> io::Result<()> {
        let mut first_error = None;
        if self.paste {
            self.paste = false;
            if let Err(error) = control.apply(TerminalOperation::DisablePaste) {
                first_error = Some(error);
            }
        }
        if self.keyboard_enhancement {
            self.keyboard_enhancement = false;
            if let Err(error) = control.apply(TerminalOperation::DisableKeyboardEnhancement) {
                first_error.get_or_insert(error);
            }
        }
        if self.mouse {
            self.mouse = false;
            if let Err(error) = control.apply(TerminalOperation::DisableMouse) {
                first_error.get_or_insert(error);
            }
        }
        if self.alternate {
            self.alternate = false;
            if let Err(error) = control.apply(TerminalOperation::LeaveAlternate) {
                first_error.get_or_insert(error);
            }
        }
        if self.raw {
            self.raw = false;
            if let Err(error) = control.apply(TerminalOperation::DisableRaw) {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}
