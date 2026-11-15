//! Windows disk (`\\.\PhysicalDriveN`) as a `BlockDevice`, volume locking
//! and preparation of a destination disk.

use super::{Access, Handle, ioctl, open_device, u32le, u64le};
use crate::device::{AlignedBuf, BlockDevice, IoErrorClass, fault};
use crate::model::{DiskInfo, DiskKind, MediaKind};
use std::ffi::c_void;
use std::io;
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_NO_BUFFERING, FILE_FLAG_SEQUENTIAL_SCAN, FlushFileBuffers, ReadFile, WriteFile};
use windows_sys::Win32::System::IO::OVERLAPPED;
use windows_sys::Win32::System::Ioctl::{
    FSCTL_DISMOUNT_VOLUME, FSCTL_LOCK_VOLUME, FSCTL_UNLOCK_VOLUME, IOCTL_DISK_DELETE_DRIVE_LAYOUT, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX,
    IOCTL_DISK_GET_LENGTH_INFO, IOCTL_DISK_UPDATE_PROPERTIES, IOCTL_STORAGE_QUERY_PROPERTY,
};

/// Maximum size of a single I/O (drivers limit transfer sizes).
const MAX_IO: usize = 8 * 1024 * 1024;

pub struct RawDisk {
    handle: Handle,
    path: String,
    size: u64,
    sector: u32,
    physical: u32,
    writable: bool,
    bounce: Option<AlignedBuf>,
    /// Volume locks held as long as the disk is open (clone destination): a locked,
    /// dismounted volume cannot be mounted again by Windows underneath the writes.
    held_locks: Vec<VolumeLock>,
    /// Clone destination: disk number whose volumes are locked again when Windows
    /// refuses a write because a volume appeared on it meanwhile.
    relock_disk: Option<u32>,
    /// Volume paths already locked (see `held_locks`).
    locked_paths: Vec<String>,
}

/// Reads the sector sizes (logical, physical) through the alignment property.
pub(crate) fn query_sector_sizes(h: &Handle) -> Option<(u32, u32)> {
    // STORAGE_PROPERTY_QUERY { PropertyId = StorageAccessAlignmentProperty (6), QueryType = 0 }
    let mut q = [0u8; 12];
    q[0..4].copy_from_slice(&6u32.to_le_bytes());
    let mut out = [0u8; 64];
    let n = ioctl(h, IOCTL_STORAGE_QUERY_PROPERTY, &q, &mut out).ok()?;
    if n < 28 {
        return None;
    }
    let logical = u32le(&out, 16);
    let physical = u32le(&out, 20);
    if logical.is_power_of_two() && logical >= 512 { Some((logical, physical.max(logical))) } else { None }
}

pub(crate) fn query_geometry(h: &Handle) -> Option<(u64, u32)> {
    let mut out = [0u8; 256];
    let n = ioctl(h, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, &[], &mut out).ok()?;
    if n < 32 {
        return None;
    }
    Some((u64le(&out, 24), u32le(&out, 20)))
}

impl RawDisk {
    fn open(path: &str, writable: bool) -> io::Result<Self> {
        let access = if writable { Access::ReadWrite } else { Access::Read };
        let handle = open_device(path, access, FILE_FLAG_NO_BUFFERING | FILE_FLAG_SEQUENTIAL_SCAN)?;
        let geometry = query_geometry(&handle);
        let mut len = [0u8; 8];
        let size = match ioctl(&handle, IOCTL_DISK_GET_LENGTH_INFO, &[], &mut len) {
            Ok(8) => u64::from_le_bytes(len),
            _ => geometry.map(|g| g.0).ok_or_else(|| io::Error::other(t!("taille du disque inconnue", "disk size unknown")))?,
        };
        let (sector, physical) = query_sector_sizes(&handle).or(geometry.map(|g| (g.1, g.1))).unwrap_or((512, 512));
        if !sector.is_power_of_two() || sector < 512 {
            return Err(io::Error::other(tf!("taille de secteur inattendue ({sector})", "unexpected sector size ({sector})")));
        }
        Ok(Self { handle, path: path.to_string(), size: size - size % sector as u64, sector, physical, writable, bounce: None, held_locks: Vec::new(), relock_disk: None, locked_paths: Vec::new() })
    }

    /// Opens a disk read-only (clone source, analysis).
    pub fn open_read_only(path: &str) -> io::Result<Self> {
        Self::open(path, false)
    }

    /// Opens a disk read/write. Reserved for the destination of a clone,
    /// after the pre-checks and `prepare_destination`.
    pub fn open_read_write(path: &str) -> io::Result<Self> {
        Self::open(path, true)
    }

    pub fn handle(&self) -> &Handle {
        &self.handle
    }

    fn overlapped(offset: u64) -> OVERLAPPED {
        // SAFETY: OVERLAPPED is a plain C structure, valid when zeroed.
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.Anonymous.Anonymous.Offset = offset as u32;
        ov.Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;
        ov
    }

    fn read_raw(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let n = (buf.len() - done).min(MAX_IO);
            let mut ov = Self::overlapped(offset + done as u64);
            let mut got = 0u32;
            // SAFETY: buffer valid for n bytes; synchronous handle with explicit position.
            let ok = unsafe { ReadFile(self.handle.0, buf[done..].as_mut_ptr(), n as u32, &mut got, &mut ov) };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            if got as usize != n {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    tf!("lecture partielle ({got} octets sur {n})", "partial read ({got} of {n} bytes)"),
                ));
            }
            done += n;
        }
        Ok(())
    }

    fn write_raw(&self, offset: u64, buf: &[u8]) -> io::Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let n = (buf.len() - done).min(MAX_IO);
            let mut ov = Self::overlapped(offset + done as u64);
            let mut put = 0u32;
            // SAFETY: buffer valid for n bytes.
            let ok = unsafe { WriteFile(self.handle.0, buf[done..].as_ptr(), n as u32, &mut put, &mut ov) };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            if put as usize != n {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    tf!("écriture partielle ({put} octets sur {n})", "partial write ({put} of {n} bytes)"),
                ));
            }
            done += n;
        }
        Ok(())
    }

    /// Locks and dismounts every volume found on the disk now that is not locked yet,
    /// and keeps it locked while the disk is open. On a removable disk without a
    /// partition table, for instance, Windows exposes a volume covering the whole
    /// disk ("superfloppy") as soon as the old table is gone.
    fn relock_volumes(&mut self) {
        let Some(n) = self.relock_disk else { return };
        for (guid_path, display) in super::enumerate::volumes_of_disk(n) {
            let key = guid_path.trim_end_matches('\\').to_ascii_lowercase();
            if self.locked_paths.contains(&key) {
                continue;
            }
            if let Ok(lock) = lock_volume_tries(&guid_path, &display, 3) {
                self.held_locks.push(lock);
                self.locked_paths.push(key);
            }
        }
    }

    fn write_once(&mut self, offset: u64, buf: &[u8]) -> io::Result<()> {
        if self.aligned(buf.as_ptr()) {
            return self.write_raw(offset, buf);
        }
        let mut b = self.bounce(buf.len());
        b[..buf.len()].copy_from_slice(buf);
        let r = self.write_raw(offset, &b[..buf.len()]);
        self.bounce = Some(b);
        r
    }

    fn aligned(&self, ptr: *const u8) -> bool {
        (ptr as usize).is_multiple_of(self.sector as usize)
    }

    fn bounce(&mut self, len: usize) -> AlignedBuf {
        match self.bounce.take() {
            Some(b) if b.len() >= len => b,
            _ => AlignedBuf::new(len.max(64 * 1024)),
        }
    }
}

impl BlockDevice for RawDisk {
    fn size(&self) -> u64 {
        self.size
    }
    fn sector_size(&self) -> u32 {
        self.sector
    }
    fn physical_sector_size(&self) -> u32 {
        self.physical
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        crate::device::validate_io(self, offset, buf.len())?;
        if buf.is_empty() {
            return Ok(());
        }
        // Unbuffered I/O requires a sector-aligned buffer.
        if self.aligned(buf.as_ptr()) {
            return self.read_raw(offset, buf);
        }
        let mut b = self.bounce(buf.len());
        let r = self.read_raw(offset, &mut b[..buf.len()]);
        if r.is_ok() {
            buf.copy_from_slice(&b[..buf.len()]);
        }
        self.bounce = Some(b);
        r
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()> {
        if !self.writable {
            return Err(fault(IoErrorClass::WriteProtected));
        }
        crate::device::validate_io(self, offset, buf.len())?;
        if buf.is_empty() {
            return Ok(());
        }
        match self.write_once(offset, buf) {
            // Windows refuses writes inside a mounted volume: nothing written ("partial
            // write, 0 bytes") or access denied. A volume that appeared during the
            // operation is locked, then the write is made again, once.
            Err(e) if self.relock_disk.is_some() && blocked_by_volume(&e) => {
                self.relock_volumes();
                self.write_once(offset, buf)
            }
            r => r,
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        if !self.writable {
            return Ok(());
        }
        // SAFETY: handle valide.
        if unsafe { FlushFileBuffers(self.handle.0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    fn is_read_only(&self) -> bool {
        !self.writable
    }
    fn label(&self) -> String {
        self.path.clone()
    }
}

// ---------------------------------------------------------------------------
// Volume locking
// ---------------------------------------------------------------------------

/// Locked and dismounted volume. The lock is released on drop; Windows
/// then remounts the volume on the first request.
pub struct VolumeLock {
    handle: Handle,
    pub name: String,
}

impl Drop for VolumeLock {
    fn drop(&mut self) {
        let _ = ioctl(&self.handle, FSCTL_UNLOCK_VOLUME, &[], &mut []);
    }
}

/// Locks then dismounts a volume (`\\?\Volume{…}\`). Fails if files on it
/// are open in another program.
pub fn lock_volume(guid_path: &str, display: &str) -> io::Result<VolumeLock> {
    lock_volume_tries(guid_path, display, 6)
}

pub(crate) fn lock_volume_tries(guid_path: &str, display: &str, tries: u64) -> io::Result<VolumeLock> {
    let path = guid_path.trim_end_matches('\\');
    let handle = open_device(path, Access::Read, 0)?;
    let mut last = None;
    for attempt in 0..tries {
        match ioctl(&handle, FSCTL_LOCK_VOLUME, &[], &mut []) {
            Ok(_) => {
                last = None;
                break;
            }
            Err(e) => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(250 * (attempt + 1)));
            }
        }
    }
    if let Some(e) = last {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            tf!(
                "le volume {display} est en cours d'utilisation et n'a pas pu être verrouillé ({e}). \
                 Fermez les programmes et fenêtres de l'Explorateur qui l'utilisent, puis réessayez.",
                "volume {display} is in use and could not be locked ({e}). Close the programs and File Explorer windows using it, then try again."
            ),
        ));
    }
    ioctl(&handle, FSCTL_DISMOUNT_VOLUME, &[], &mut [])
        .map_err(|e| io::Error::other(tf!("impossible de démonter le volume {display} : {e}", "cannot dismount volume {display}: {e}")))?;
    Ok(VolumeLock { handle, name: display.to_string() })
}

/// Locks all the volumes of a disk. Volumes without a mounted file
/// system are ignored.
pub fn lock_disk_volumes(disk: &DiskInfo, volumes: &[(String, String)]) -> io::Result<Vec<VolumeLock>> {
    let _ = disk;
    let mut locks = Vec::new();
    for (guid_path, display) in volumes {
        locks.push(lock_volume(guid_path, display)?);
    }
    Ok(locks)
}

/// Like `lock_disk_volumes`, but with a single attempt per volume (preview: no
/// waiting if a program is using the volume).
pub fn try_lock_disk_volumes(volumes: &[(String, String)]) -> io::Result<Vec<VolumeLock>> {
    let mut locks = Vec::new();
    for (guid_path, display) in volumes {
        locks.push(lock_volume_tries(guid_path, display, 1)?);
    }
    Ok(locks)
}

/// Refuses writing to a real physical disk while that path is not
/// validated, unless explicitly agreed. Virtual disks (VHD/VHDX) are accepted.
pub fn check_write_allowed(disk: &DiskInfo, allow_unvalidated: bool) -> Result<(), String> {
    if disk.kind != DiskKind::Physical {
        return Ok(());
    }
    if disk.is_system || disk.hosts_app {
        return Err(t!(
            "écriture refusée : ce disque héberge le système en cours d'exécution ou le programme",
            "write refused: this disk holds the running system or the program itself"
        )
        .into());
    }
    if disk.media == MediaKind::Virtual || super::PHYSICAL_WRITE_VALIDATED || allow_unvalidated {
        return Ok(());
    }
    Err(t!("l'écriture sur un disque physique réel n'a pas encore été validée dans cette version. \
         Elle doit être activée explicitement dans les réglages avancés, en connaissance de cause.", "writing to a real physical disk has not been validated yet in this version. It must be enabled explicitly in the advanced settings, knowingly.")
        .into())
}

/// Prepares a destination disk: locks and dismounts its volumes, deletes its
/// partition table (Windows then stops seeing volumes on it and allows writing
/// to the whole disk), then opens it read/write.
///
/// The locks stay held by the returned disk until it is closed: on a USB key,
/// Windows may otherwise mount an old volume again before removing it, and every
/// write inside a mounted volume is refused ("partial write, 0 bytes").
///
/// DESTRUCTIVE OPERATION: call only after the user's confirmation.
pub fn prepare_destination(disk: &DiskInfo, volumes: &[(String, String)], allow_unvalidated: bool) -> io::Result<RawDisk> {
    check_write_allowed(disk, allow_unvalidated).map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e))?;
    let locks = lock_disk_volumes(disk, volumes)?;
    let dev = RawDisk::open_read_write(&disk.path)?;
    ioctl(dev.handle(), IOCTL_DISK_DELETE_DRIVE_LAYOUT, &[], &mut []).map_err(|e| {
        io::Error::other(tf!(
            "impossible de supprimer l'ancienne table de partitions de la destination : {e}",
            "cannot delete the destination's old partition table: {e}"
        ))
    })?;
    let _ = ioctl(dev.handle(), IOCTL_DISK_UPDATE_PROPERTIES, &[], &mut []);
    let mut dev = dev;
    dev.locked_paths = volumes.iter().map(|(g, _)| g.trim_end_matches('\\').to_ascii_lowercase()).collect();
    dev.held_locks = locks;
    dev.relock_disk = disk.number;
    // Without a table, a removable disk shows a whole-disk volume a moment later.
    std::thread::sleep(std::time::Duration::from_millis(400));
    dev.relock_volumes();
    Ok(dev)
}

/// Error of a write refused because the sectors belong to a mounted volume.
fn blocked_by_volume(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::WriteZero || e.raw_os_error() == Some(5)
}

/// Asks Windows to re-read the disk's partition table.
pub fn refresh_disk(dev: &RawDisk) -> io::Result<()> {
    ioctl(dev.handle(), IOCTL_DISK_UPDATE_PROPERTIES, &[], &mut []).map(|_| ())
}

#[allow(dead_code)]
fn _assert_send() {
    fn is_send<T: Send>() {}
    is_send::<RawDisk>();
    let _ = std::ptr::null::<c_void>();
}
