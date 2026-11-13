//! Windows layer: disk access, detection, volume locking, SMART.
//!
//! The rest of the engine is platform-independent and tested on images.
//! This layer is deliberately thin: it translates Win32 calls into
//! `BlockDevice` and `DiskInfo`.

pub mod enumerate;
pub mod rawdisk;
pub mod smart_io;

use std::ffi::c_void;
use std::io;
use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
use windows_sys::Win32::System::IO::DeviceIoControl;

/// Writing to a real physical disk has not been validated on hardware yet
/// (only on images and, through the provided script, on VHDX virtual disks).
/// While this constant is `false`, opening a non-virtual disk for writing
/// requires explicit consent from the caller.
pub const PHYSICAL_WRITE_VALIDATED: bool = false;

/// Win32 handle closed automatically.
pub struct Handle(pub HANDLE);

// SAFETY: a file/device HANDLE can be used from any thread.
unsafe impl Send for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            // SAFETY: valid handle, owned by this structure.
            unsafe { CloseHandle(self.0) };
        }
    }
}

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Property queries only (no privilege required).
    Query,
    Read,
    ReadWrite,
}

pub fn open_device(path: &str, access: Access, flags: u32) -> io::Result<Handle> {
    let w = wide(path);
    let desired = match access {
        Access::Query => 0,
        Access::Read => GENERIC_READ,
        Access::ReadWrite => GENERIC_READ | GENERIC_WRITE,
    };
    // SAFETY: zero-terminated UTF-16 string, optional pointers null.
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            desired,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            flags,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE || h.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(Handle(h))
}

/// `DeviceIoControl` with input and output buffers. Returns the number of bytes received.
pub fn ioctl(h: &Handle, code: u32, input: &[u8], output: &mut [u8]) -> io::Result<usize> {
    let mut returned = 0u32;
    // SAFETY: the buffers are valid for the stated lengths.
    let ok = unsafe {
        DeviceIoControl(
            h.0,
            code,
            if input.is_empty() { std::ptr::null() } else { input.as_ptr() as *const c_void },
            input.len() as u32,
            if output.is_empty() { std::ptr::null_mut() } else { output.as_mut_ptr() as *mut c_void },
            output.len() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(returned as usize)
}

#[allow(dead_code)]
pub(crate) fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
pub(crate) fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
pub(crate) fn u64le(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(a)
}

/// True if the process has an elevated administrator token.
pub fn is_elevated() -> bool {
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: standard Win32 calls; the token is closed by `Handle`.
    unsafe {
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let token = Handle(token);
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token.0,
            TokenElevation,
            &mut elevation as *mut _ as *mut c_void,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        );
        ok != 0 && elevation.TokenIsElevated != 0
    }
}

/// Restarts the current program with elevation (UAC prompt). Returns `Ok(())` if
/// Windows agreed to start the new process.
pub fn relaunch_elevated(args: &str) -> io::Result<()> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let exe = std::env::current_exe()?;
    let exe_w = wide(&exe.to_string_lossy());
    let verb = wide("runas");
    let args_w = wide(args);
    // SAFETY: valid, terminated UTF-16 strings.
    let r = unsafe { ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), exe_w.as_ptr(), args_w.as_ptr(), std::ptr::null(), SW_SHOWNORMAL) };
    if (r as isize) > 32 { Ok(()) } else { Err(io::Error::other(t!("élévation refusée ou annulée", "elevation refused or cancelled"))) }
}
