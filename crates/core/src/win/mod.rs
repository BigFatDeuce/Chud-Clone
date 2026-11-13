//! Couche Windows : accès aux disques, détection, verrouillage des volumes, SMART.
//!
//! Tout le reste du moteur est indépendant de la plateforme et testé sur des images.
//! Cette couche est volontairement mince : elle traduit les appels Win32 en
//! `BlockDevice` et en `DiskInfo`.

pub mod enumerate;
pub mod rawdisk;
pub mod smart_io;

use std::ffi::c_void;
use std::io;
use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
use windows_sys::Win32::System::IO::DeviceIoControl;

/// L'écriture sur disque physique réel n'a pas encore été validée sur du matériel
/// (seulement sur des images et, via le script fourni, sur des disques virtuels VHDX).
/// Tant que cette constante vaut `false`, l'ouverture en écriture d'un disque non
/// virtuel exige un accord explicite de l'appelant.
pub const PHYSICAL_WRITE_VALIDATED: bool = false;

/// Handle Win32 fermé automatiquement.
pub struct Handle(pub HANDLE);

// SAFETY: un HANDLE de fichier/périphérique peut être utilisé depuis n'importe quel thread.
unsafe impl Send for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            // SAFETY: handle valide, possédé par cette structure.
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
    /// Requêtes de propriétés uniquement (aucun privilège requis).
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
    // SAFETY: chaîne UTF-16 terminée par zéro, pointeurs optionnels nuls.
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

/// `DeviceIoControl` avec tampons d'entrée et de sortie. Renvoie le nombre d'octets reçus.
pub fn ioctl(h: &Handle, code: u32, input: &[u8], output: &mut [u8]) -> io::Result<usize> {
    let mut returned = 0u32;
    // SAFETY: les tampons sont valides pour les longueurs annoncées.
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

/// Vrai si le processus dispose d'un jeton administrateur élevé.
pub fn is_elevated() -> bool {
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: appels Win32 standards ; le jeton est fermé par `Handle`.
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

/// Relance le programme courant avec élévation (invite UAC). Renvoie `Ok(())` si
/// Windows a accepté de démarrer le nouveau processus.
pub fn relaunch_elevated(args: &str) -> io::Result<()> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let exe = std::env::current_exe()?;
    let exe_w = wide(&exe.to_string_lossy());
    let verb = wide("runas");
    let args_w = wide(args);
    // SAFETY: chaînes UTF-16 valides et terminées.
    let r = unsafe { ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), exe_w.as_ptr(), args_w.as_ptr(), std::ptr::null(), SW_SHOWNORMAL) };
    if (r as isize) > 32 { Ok(()) } else { Err(io::Error::other(t!("élévation refusée ou annulée", "elevation refused or cancelled"))) }
}
