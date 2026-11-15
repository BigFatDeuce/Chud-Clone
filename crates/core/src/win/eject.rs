//! Safe removal of a disk ("Eject" in File Explorer), and shutting the computer down.

use super::{Access, from_wide, ioctl, open_device};
use std::mem::size_of;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_Parent, CM_Request_Device_EjectW, CR_SUCCESS, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, PNP_VETO_TYPE, SP_DEVICE_INTERFACE_DATA,
    SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW,
};
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::System::Ioctl::GUID_DEVINTERFACE_DISK;

const IOCTL_STORAGE_GET_DEVICE_NUMBER: u32 = 0x002D_1080;

/// Device instance (PnP) of the disk `\\.\PhysicalDrive<number>`.
fn disk_devinst(number: u32) -> Option<u32> {
    // SAFETY: standard SetupAPI enumeration; every structure has its cbSize set and
    // the device information set is destroyed before returning.
    unsafe {
        let set = SetupDiGetClassDevsW(&GUID_DEVINTERFACE_DISK, std::ptr::null(), std::ptr::null_mut(), DIGCF_PRESENT | DIGCF_DEVICEINTERFACE);
        if set as isize == INVALID_HANDLE_VALUE as isize {
            return None;
        }
        let mut found = None;
        let mut index = 0;
        loop {
            let mut iface: SP_DEVICE_INTERFACE_DATA = std::mem::zeroed();
            iface.cbSize = size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
            if SetupDiEnumDeviceInterfaces(set, std::ptr::null(), &GUID_DEVINTERFACE_DISK, index, &mut iface) == 0 {
                break;
            }
            index += 1;
            // Detail: a u32 cbSize followed by the device path (UTF-16).
            let mut buf = vec![0u64; 512];
            let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
            (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            let mut info: SP_DEVINFO_DATA = std::mem::zeroed();
            info.cbSize = size_of::<SP_DEVINFO_DATA>() as u32;
            if SetupDiGetDeviceInterfaceDetailW(set, &iface, detail, (buf.len() * 8) as u32, std::ptr::null_mut(), &mut info) == 0 {
                continue;
            }
            let path_ptr = std::ptr::addr_of!((*detail).DevicePath) as *const u16;
            let path = from_wide(std::slice::from_raw_parts(path_ptr, 1024));
            let Ok(h) = open_device(&path, Access::Query, 0) else { continue };
            let mut out = [0u8; 12];
            if ioctl(&h, IOCTL_STORAGE_GET_DEVICE_NUMBER, &[], &mut out).is_ok() && u32::from_le_bytes([out[4], out[5], out[6], out[7]]) == number {
                found = Some(info.DevInst);
                break;
            }
        }
        SetupDiDestroyDeviceInfoList(set);
        found
    }
}

/// Asks Windows to remove the disk safely (like "Eject" in File Explorer). Works for
/// USB disks, keys and docks; an internal disk is refused by Windows.
///
/// Right after a clone, Windows mounts the new volumes and File Explorer, the
/// antivirus or the indexer open them at once, which vetoes the removal ("in use").
/// The volumes are therefore flushed and dismounted just before each attempt, and
/// the request is repeated for a few seconds.
pub fn eject_disk(number: u32) -> Result<(), String> {
    let mut last = String::new();
    for attempt in 0..6u64 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(1000 + 500 * attempt));
        }
        dismount_volumes(number);
        match request_eject(number) {
            Ok(()) => return Ok(()),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// Locks and dismounts every volume of the disk, then lets go of them: the volumes
/// stay dismounted (nothing is cached or open on them any more) until something
/// accesses them again. Failures are ignored: the removal request then reports why.
fn dismount_volumes(number: u32) {
    for (guid_path, display) in super::enumerate::volumes_of_disk(number) {
        drop(super::rawdisk::lock_volume_tries(&guid_path, &display, 2));
    }
}

/// One removal request: the disk itself, then its parents (USB bridge, dock port).
fn request_eject(number: u32) -> Result<(), String> {
    let mut inst = disk_devinst(number).ok_or_else(|| tf!("disque {number} introuvable pour l'éjection", "disk {number} not found for ejection"))?;
    let mut last = String::new();
    // The disk itself, then its parents (USB bridge, dock port), as File Explorer does.
    for _ in 0..3 {
        let mut veto: PNP_VETO_TYPE = 0;
        let mut name = [0u16; 260];
        // SAFETY: valid device instance, buffer length given in characters.
        let r = unsafe { CM_Request_Device_EjectW(inst, &mut veto, name.as_mut_ptr(), name.len() as u32, 0) };
        if r == CR_SUCCESS && veto == 0 {
            return Ok(());
        }
        last = if veto != 0 {
            tf!("refusé par Windows (veto {veto} : {})", "refused by Windows (veto {veto}: {})", from_wide(&name))
        } else {
            tf!("code {r}", "code {r}")
        };
        let mut parent = 0u32;
        // SAFETY: plain query on a valid device instance.
        if unsafe { CM_Get_Parent(&mut parent, inst, 0) } != CR_SUCCESS {
            break;
        }
        inst = parent;
    }
    Err(tf!(
        "le disque {number} n'a pas pu être éjecté ({last}). Un disque interne ne s'éjecte pas ; sinon fermez ce qui l'utilise.",
        "disk {number} could not be ejected ({last}). An internal disk cannot be ejected; otherwise close what is using it."
    ))
}

/// Shuts the computer down now (`shutdown /s /t 0`; `wpeutil shutdown` under WinPE).
pub fn shutdown_now() -> Result<(), String> {
    let winpe = std::env::var("SystemDrive").map(|d| d.eq_ignore_ascii_case("X:")).unwrap_or(false);
    let status = if winpe {
        std::process::Command::new("wpeutil").arg("shutdown").status()
    } else {
        std::process::Command::new("shutdown").args(["/s", "/t", "0"]).status()
    };
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(tf!("arrêt refusé (code {})", "shutdown refused (code {})", s.code().unwrap_or(-1))),
        Err(e) => Err(tf!("arrêt impossible : {e}", "cannot shut down: {e}")),
    }
}
