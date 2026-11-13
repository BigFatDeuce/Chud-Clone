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
