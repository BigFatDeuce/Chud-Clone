//! Disque Windows (`\\.\PhysicalDriveN`) en tant que `BlockDevice`, verrouillage des
//! volumes et préparation d'un disque de destination.

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

/// Taille maximale d'une E/S unique (les pilotes limitent la taille des transferts).
const MAX_IO: usize = 8 * 1024 * 1024;

pub struct RawDisk {
    handle: Handle,
    path: String,
    size: u64,
    sector: u32,
    physical: u32,
    writable: bool,
    bounce: Option<AlignedBuf>,
}

/// Lit les tailles de secteur (logique, physique) via la propriété d'alignement.
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
        Ok(Self { handle, path: path.to_string(), size: size - size % sector as u64, sector, physical, writable, bounce: None })
    }

    /// Ouvre un disque en lecture seule (source d'un clonage, analyse).
    pub fn open_read_only(path: &str) -> io::Result<Self> {
        Self::open(path, false)
    }

    /// Ouvre un disque en lecture/écriture. Réservé à la destination d'un clonage,
    /// après les pré-vérifications et `prepare_destination`.
    pub fn open_read_write(path: &str) -> io::Result<Self> {
        Self::open(path, true)
    }

    pub fn handle(&self) -> &Handle {
        &self.handle
    }

    fn overlapped(offset: u64) -> OVERLAPPED {
        // SAFETY: OVERLAPPED est une structure C simple, valide à zéro.
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
            // SAFETY: tampon valide pour n octets ; handle synchrone avec position explicite.
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
            // SAFETY: tampon valide pour n octets.
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
        // Les E/S non tamponnées exigent un tampon aligné sur le secteur.
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
        if self.aligned(buf.as_ptr()) {
            return self.write_raw(offset, buf);
        }
        let mut b = self.bounce(buf.len());
        b[..buf.len()].copy_from_slice(buf);
        let r = self.write_raw(offset, &b[..buf.len()]);
        self.bounce = Some(b);
        r
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
// Verrouillage des volumes
// ---------------------------------------------------------------------------

/// Volume verrouillé et démonté. Le verrou est rendu à la destruction ; Windows
/// remonte alors le volume à la première demande.
pub struct VolumeLock {
    handle: Handle,
    pub name: String,
}

impl Drop for VolumeLock {
    fn drop(&mut self) {
        let _ = ioctl(&self.handle, FSCTL_UNLOCK_VOLUME, &[], &mut []);
    }
}

/// Verrouille puis démonte un volume (`\\?\Volume{…}\`). Échoue si des fichiers y
/// sont ouverts par un autre programme.
pub fn lock_volume(guid_path: &str, display: &str) -> io::Result<VolumeLock> {
    lock_volume_tries(guid_path, display, 6)
}

fn lock_volume_tries(guid_path: &str, display: &str, tries: u64) -> io::Result<VolumeLock> {
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

/// Verrouille tous les volumes d'un disque. Les volumes sans système de fichiers
/// monté sont ignorés.
pub fn lock_disk_volumes(disk: &DiskInfo, volumes: &[(String, String)]) -> io::Result<Vec<VolumeLock>> {
    let _ = disk;
    let mut locks = Vec::new();
    for (guid_path, display) in volumes {
        locks.push(lock_volume(guid_path, display)?);
    }
    Ok(locks)
}

/// Comme `lock_disk_volumes`, mais en une seule tentative par volume (aperçu : pas
/// d'attente si un programme utilise le volume).
pub fn try_lock_disk_volumes(volumes: &[(String, String)]) -> io::Result<Vec<VolumeLock>> {
    let mut locks = Vec::new();
    for (guid_path, display) in volumes {
        locks.push(lock_volume_tries(guid_path, display, 1)?);
    }
    Ok(locks)
}

/// Refuse l'écriture sur un disque physique réel tant que cette voie n'est pas
/// validée, sauf accord explicite. Les disques virtuels (VHD/VHDX) sont acceptés.
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

/// Prépare un disque de destination : verrouille et démonte ses volumes, supprime sa
/// table de partitions (Windows cesse alors d'y voir des volumes et autorise l'écriture
/// sur tout le disque), puis l'ouvre en lecture/écriture.
///
/// OPÉRATION DESTRUCTRICE : à n'appeler qu'après confirmation de l'utilisateur.
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
    drop(locks);
    Ok(dev)
}

/// Demande à Windows de relire la table de partitions du disque.
pub fn refresh_disk(dev: &RawDisk) -> io::Result<()> {
    ioctl(dev.handle(), IOCTL_DISK_UPDATE_PROPERTIES, &[], &mut []).map(|_| ())
}

#[allow(dead_code)]
fn _assert_send() {
    fn is_send<T: Send>() {}
    is_send::<RawDisk>();
    let _ = std::ptr::null::<c_void>();
}
