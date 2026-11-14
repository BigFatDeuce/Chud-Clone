//! Keeps the computer from going to sleep while a long operation runs (clone,
//! surface scan, formatting). The screen may still turn off.

/// While this guard lives, Windows does not put the computer to sleep. Created and
/// dropped on the same thread (the request is per thread).
pub struct KeepAwake(());

impl KeepAwake {
    pub fn new() -> Self {
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Power::{ES_CONTINUOUS, ES_SYSTEM_REQUIRED, SetThreadExecutionState};
            // SAFETY: plain flag call, no pointer.
            unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED) };
        }
        KeepAwake(())
    }
}

impl Default for KeepAwake {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Power::{ES_CONTINUOUS, SetThreadExecutionState};
            // SAFETY: plain flag call; restores the normal sleep behaviour.
            unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
        }
    }
}

/// Keeps the computer awake for as long as a program wants, switchable at any time
/// from any thread. The request belongs to a thread: the switch owns a parked one.
pub struct AwakeSwitch {
    tx: std::sync::mpsc::Sender<bool>,
}

impl AwakeSwitch {
    pub fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<bool>();
        let _ = std::thread::Builder::new().name("keep-awake".into()).spawn(move || {
            let mut guard: Option<KeepAwake> = None;
            // Ends (and releases the request) when the switch is dropped.
            for on in rx {
                guard = if on { guard.or_else(|| Some(KeepAwake::new())) } else { None };
            }
            drop(guard);
        });
        AwakeSwitch { tx }
    }

    /// `true`: no sleep from now on; `false`: back to the normal Windows behaviour.
    pub fn set(&self, on: bool) {
        let _ = self.tx.send(on);
    }
}

impl Default for AwakeSwitch {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switch_and_guard_can_be_combined() {
        let s = AwakeSwitch::new();
        s.set(true);
        s.set(true);
        {
            let _work = KeepAwake::new();
        }
        s.set(false);
        drop(s);
    }
}
