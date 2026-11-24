//! Detection of disks and volumes on Windows.
//!
//! Basic detection requires no privilege: it only uses property queries
//! (no sector read). With administrator rights, a direct read (read-only)
//! completes the information Windows does not expose: file system
//! and usage of partitions without a letter (EFI, recovery).

use super::rawdisk::{RawDisk, query_geometry, query_sector_sizes};
use super::{Access, Handle, from_wide, ioctl, open_device, u32le, u64le, wide};
use crate::guid::Guid;
use crate::image::partition_infos;
use crate::model::{DiskInfo, DiskKind, MediaKind, PartitionInfo, fingerprint};
use crate::partition::{PartRole, TableKind, gpt_role, gpt_type_label, mbr_role, mbr_type_label};
use crate::plan::analyze_disk;
use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    FindFirstVolumeW, FindNextVolumeW, FindVolumeClose, GetDiskFreeSpaceExW, GetVolumeInformationW, GetVolumePathNamesForVolumeNameW,
    IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
};
use windows_sys::Win32::System::Ioctl::{IOCTL_DISK_GET_DISK_ATTRIBUTES, IOCTL_DISK_GET_DRIVE_LAYOUT_EX, IOCTL_STORAGE_QUERY_PROPERTY};
use windows_sys::Win32::System::SystemInformation::GetSystemWindowsDirectoryW;

/// Maximum number of disk numbers probed.
const MAX_DISKS: u32 = 64;

#[derive(Debug, Clone)]
pub struct VolumeRec {
    /// `\\?\Volume{…}\`
    pub guid_path: String,
    pub mounts: Vec<String>,
    pub label: Option<String>,
    pub fs: Option<String>,
    pub total: Option<u64>,
    pub free: Option<u64>,
    /// (disk number, offset in bytes, length).
    pub extents: Vec<(u32, u64, u64)>,
}

fn cstr_at(buf: &[u8], offset: usize) -> String {
    if offset == 0 || offset >= buf.len() {
        return String::new();
    }
    let end = buf[offset..].iter().position(|b| *b == 0).map(|p| offset + p).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[offset..end]).trim().to_string()
}

/// Some drivers return the ATA serial number in hexadecimal form, with bytes
/// swapped in pairs. It is decoded back when that is clearly the case.
fn normalize_serial(s: &str) -> String {
    let t = s.trim();
    if t.len() >= 16 && t.len().is_multiple_of(4) && t.chars().all(|c| c.is_ascii_hexdigit()) {
        let bytes: Vec<u8> = (0..t.len()).step_by(2).filter_map(|i| u8::from_str_radix(&t[i..i + 2], 16).ok()).collect();
        if bytes.len() * 2 == t.len() && bytes.iter().all(|b| (0x20..0x7F).contains(b)) {
            let mut swapped = bytes.clone();
            for pair in swapped.as_chunks_mut::<2>().0 {
                pair.swap(0, 1);
            }
            return String::from_utf8_lossy(&swapped).trim().to_string();
        }
    }
    t.to_string()
}

fn bus_label(bus: u32) -> &'static str {
    match bus {
        1 => "SCSI",
        2 => "ATAPI",
        3 => "ATA",
        4 => "IEEE 1394",
        6 => "Fibre Channel",
        7 => "USB",
        8 => "RAID",
        9 => "iSCSI",
        10 => "SAS",
        11 => "SATA",
        12 => "SD",
        13 => "MMC",
        14 | 15 => t!("Virtuel", "Virtual"),
        16 => t!("Espaces de stockage", "Storage Spaces"),
        17 => "NVMe",
        19 => "UFS",
        _ => t!("Inconnu", "Unknown"),
    }
}

fn property(h: &Handle, id: u32, out: &mut [u8]) -> Option<usize> {
    let mut q = [0u8; 12];
    q[0..4].copy_from_slice(&id.to_le_bytes());
    ioctl(h, IOCTL_STORAGE_QUERY_PROPERTY, &q, out).ok()
}

/// Enumerates the Windows volumes and their locations on the disks.
pub fn list_volumes() -> Vec<VolumeRec> {
    let mut out = Vec::new();
    let mut name = [0u16; 260];
    // SAFETY: 260-character buffer.
    let find = unsafe { FindFirstVolumeW(name.as_mut_ptr(), name.len() as u32) };
    if find == INVALID_HANDLE_VALUE || find.is_null() {
        return out;
    }
    loop {
        let guid_path = from_wide(&name);
        if let Some(v) = volume_record(&guid_path) {
            out.push(v);
        }
        // SAFETY: handle de recherche valide.
        if unsafe { FindNextVolumeW(find, name.as_mut_ptr(), name.len() as u32) } == 0 {
            break;
        }
    }
    // SAFETY: handle de recherche valide.
    unsafe { FindVolumeClose(find) };
    out
}

fn volume_record(guid_path: &str) -> Option<VolumeRec> {
    // Location on disk: without it (optical drive, network), the volume does not concern us.
    let h = open_device(guid_path.trim_end_matches('\\'), Access::Query, 0).ok()?;
    let mut buf = vec![0u8; 8 + 24 * 32];
    let n = ioctl(&h, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, &[], &mut buf).ok()?;
    if n < 8 {
        return None;
    }
    let count = u32le(&buf, 0) as usize;
    let mut extents = Vec::new();
    for i in 0..count.min(32) {
        let o = 8 + i * 24;
        if o + 24 > n {
            break;
        }
        extents.push((u32le(&buf, o), u64le(&buf, o + 8), u64le(&buf, o + 16)));
    }
    drop(h);

    let wname = wide(guid_path);
    let mut mounts = Vec::new();
    let mut paths = vec![0u16; 2048];
    let mut ret = 0u32;
    // SAFETY: tampons valides.
    if unsafe { GetVolumePathNamesForVolumeNameW(wname.as_ptr(), paths.as_mut_ptr(), paths.len() as u32, &mut ret) } != 0 {
        for part in paths[..(ret as usize).min(paths.len())].split(|c| *c == 0) {
            if !part.is_empty() {
                mounts.push(String::from_utf16_lossy(part));
            }
        }
    }
    let mut label = [0u16; 261];
    let mut fs = [0u16; 261];
    // SAFETY: tampons valides ; pointeurs optionnels nuls.
    let info_ok = unsafe {
        GetVolumeInformationW(
            wname.as_ptr(),
            label.as_mut_ptr(),
            label.len() as u32,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            fs.as_mut_ptr(),
            fs.len() as u32,
        )
    } != 0;
    let (mut total, mut free) = (0u64, 0u64);
    // SAFETY: pointeurs valides.
    let space_ok = unsafe { GetDiskFreeSpaceExW(wname.as_ptr(), std::ptr::null_mut(), &mut total, &mut free) } != 0;
    let label_s = from_wide(&label);
    let fs_s = from_wide(&fs);
    Some(VolumeRec {
        guid_path: guid_path.to_string(),
        mounts,
        label: if info_ok && !label_s.is_empty() { Some(label_s) } else { None },
        fs: if info_ok && !fs_s.is_empty() { Some(fs_s) } else { None },
        total: if space_ok { Some(total) } else { None },
        free: if space_ok { Some(free) } else { None },
        extents,
    })
}

/// Numbers of the disks holding the volume that contains `path`.
fn disks_of_path(volumes: &[VolumeRec], path: &str) -> Vec<u32> {
    let p = path.to_ascii_lowercase();
    // Longest mount point that prefixes the path.
    let mut best: Option<(&VolumeRec, usize)> = None;
    for v in volumes {
        for m in &v.mounts {
            let ml = m.to_ascii_lowercase();
            if p.starts_with(&ml) && best.is_none_or(|(_, l)| ml.len() > l) {
                best = Some((v, ml.len()));
            }
        }
    }
    best.map(|(v, _)| v.extents.iter().map(|e| e.0).collect()).unwrap_or_default()
}

fn system_disks(volumes: &[VolumeRec]) -> Vec<u32> {
    let mut out = Vec::new();
    let mut buf = [0u16; 260];
    // SAFETY: tampon valide.
    let n = unsafe { GetSystemWindowsDirectoryW(buf.as_mut_ptr(), buf.len() as u32) };
    if n > 0 {
        out.extend(disks_of_path(volumes, &from_wide(&buf)));
    }
    // Volumes holding an ACTIVE page or hibernation file. A Windows disk
    // connected externally also contains them, but inactive: it is not a system
    // disk and it must be clonable. The kernel keeps active files open
    // with no sharing at all: opening them for reading then fails with a sharing
    // violation, which does not happen for an inactive file.
    for v in volumes {
        for m in &v.mounts {
            for f in ["pagefile.sys", "swapfile.sys", "hiberfil.sys"] {
                if m.len() <= 3 && file_held_by_system(&format!("{m}{f}")) {
                    out.extend(v.extents.iter().map(|e| e.0));
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// True if the file exists and is held open without sharing (page or
/// hibernation file in use).
fn file_held_by_system(path: &str) -> bool {
    use std::os::windows::fs::OpenOptionsExt;
    const ERROR_SHARING_VIOLATION: i32 = 32;
    // FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE
    match std::fs::OpenOptions::new().read(true).share_mode(7).open(path) {
        Ok(_) => false,
        Err(e) => e.raw_os_error() == Some(ERROR_SHARING_VIOLATION),
    }
}

fn app_disks(volumes: &[VolumeRec], extra_paths: &[String]) -> Vec<u32> {
    let mut out = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        out.extend(disks_of_path(volumes, &exe.to_string_lossy()));
    }
    for p in extra_paths {
        out.extend(disks_of_path(volumes, p));
    }
    out.sort();
    out.dedup();
    out
}

/// Numbers of the disks holding the file (or folder) `path`; for a file that does not
/// exist yet, the disks of its nearest existing parent folder. The volume is found from
/// the opened handle (`GetFinalPathNameByHandleW`, volume GUID form), not from the
/// spelling of the path: drive letter, mount folder, `subst`, short names all lead to
/// the same answer. Empty if the volume is not on a local disk (network share).
pub fn disks_of_file(path: &std::path::Path) -> Vec<u32> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, GetFinalPathNameByHandleW, VOLUME_NAME_GUID};
    let mut cur = Some(path);
    let file = loop {
        let Some(p) = cur else { return Vec::new() };
        // No access right, every sharing mode; FILE_FLAG_BACKUP_SEMANTICS opens folders.
        match std::fs::OpenOptions::new().access_mode(0).share_mode(7).custom_flags(FILE_FLAG_BACKUP_SEMANTICS).open(p) {
            Ok(f) => break f,
            Err(_) => cur = p.parent().filter(|q| !q.as_os_str().is_empty()),
        }
    };
    let mut buf = vec![0u16; 1024];
    // SAFETY: valid handle owned by `file`; buffer of the announced length.
    let n = unsafe { GetFinalPathNameByHandleW(file.as_raw_handle(), buf.as_mut_ptr(), buf.len() as u32, VOLUME_NAME_GUID) } as usize;
    if n == 0 || n >= buf.len() {
        return Vec::new();
    }
    let full = String::from_utf16_lossy(&buf[..n]).to_ascii_lowercase();
    list_volumes()
        .into_iter()
        .filter(|v| full.starts_with(&v.guid_path.to_ascii_lowercase()))
        .flat_map(|v| v.extents.into_iter().map(|e| e.0))
        .collect()
}

/// Volume paths (`\\?\Volume{…}\`, display name) of the volumes of a disk.
pub fn volumes_of_disk(number: u32) -> Vec<(String, String)> {
    list_volumes()
        .into_iter()
        .filter(|v| v.extents.iter().any(|e| e.0 == number))
        .map(|v| {
            let display = v.mounts.first().cloned().unwrap_or_else(|| v.label.clone().unwrap_or_else(|| t!("sans lettre", "no letter").into()));
            (v.guid_path, display)
        })
        .collect()
}

fn layout_partitions(h: &Handle, number: u32, sector: u32, volumes: &[VolumeRec]) -> (TableKind, Vec<PartitionInfo>, Vec<String>) {
    let mut notes = Vec::new();
    let mut buf = vec![0u8; 48 + 144 * 256];
    let n = match ioctl(h, IOCTL_DISK_GET_DRIVE_LAYOUT_EX, &[], &mut buf) {
        Ok(n) => n,
        Err(e) => {
            notes.push(tf!("Table de partitions non communiquée par Windows ({e}).", "Partition table not reported by Windows ({e})."));
            return (TableKind::None, vec![], notes);
        }
    };
    if n < 48 {
        return (TableKind::None, vec![], notes);
    }
    let style = u32le(&buf, 0);
    let count = u32le(&buf, 4) as usize;
    let table = match style {
        0 => TableKind::Mbr,
        1 => TableKind::Gpt,
        _ => TableKind::None,
    };
    let mut parts = Vec::new();
    for i in 0..count {
        let o = 48 + i * 144;
        if o + 144 > n {
            break;
        }
        let e = &buf[o..o + 144];
        let offset = u64le(e, 8);
        let size = u64le(e, 16);
        let pnum = u32le(e, 24);
        if size == 0 || pnum == 0 {
            continue;
        }
        let (role, type_label, name, active) = match table {
            TableKind::Gpt => {
                let ty = Guid::from_bytes(&e[32..48]);
                let units: Vec<u16> =
                    e[72..144].as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|u| *u != 0).collect();
                let name = String::from_utf16_lossy(&units);
                (gpt_role(&ty), gpt_type_label(&ty), if name.is_empty() { None } else { Some(name) }, false)
            }
            TableKind::Mbr => {
                let ty = e[32];
                if ty == 0 || mbr_role(ty) == PartRole::Extended {
                    continue;
                }
                (mbr_role(ty), mbr_type_label(ty), None, e[33] != 0)
            }
            TableKind::None => continue,
        };
        let vol = volumes.iter().find(|v| v.extents.iter().any(|x| x.0 == number && x.1 == offset));
        let used = vol.and_then(|v| match (v.total, v.free) {
            (Some(t), Some(f)) if t >= f => Some(t - f),
            _ => None,
        });
        parts.push(PartitionInfo {
            number: pnum,
            offset,
            size,
            role,
            type_label,
            name,
            fs: vol.and_then(|v| v.fs.clone()),
            label: vol.and_then(|v| v.label.clone()),
            used_bytes: used,
            mounts: vol.map(|v| v.mounts.clone()).unwrap_or_default(),
            active,
            volume_path: vol.map(|v| v.guid_path.clone()),
        });
    }
    let _ = sector;
    parts.sort_by_key(|p| p.offset);
    (table, parts, notes)
}

/// Describes the disk `\\.\PhysicalDrive<number>`, or `None` if it does not exist.
fn describe(number: u32, volumes: &[VolumeRec], system: &[u32], app: &[u32], deep: bool) -> Option<DiskInfo> {
    let path = format!(r"\\.\PhysicalDrive{number}");
    let h = match open_device(&path, Access::Query, 0) {
        Ok(h) => h,
        Err(e) => {
            let code = e.raw_os_error().unwrap_or(0) as u32;
            if code == ERROR_FILE_NOT_FOUND || code == ERROR_PATH_NOT_FOUND {
                return None;
            }
            // The disk exists but refuses even queries: report it rather than hide it.
            return Some(DiskInfo {
                id: format!("disk:{number}"),
                kind: DiskKind::Physical,
                path,
                number: Some(number),
                model: t!("Disque inaccessible", "Inaccessible disk").into(),
                vendor: String::new(),
                serial: String::new(),
                firmware: String::new(),
                bus: t!("Inconnu", "Unknown").into(),
                media: MediaKind::Unknown,
                size: 0,
                sector_size: 512,
                physical_sector_size: 512,
                removable: false,
                read_only: false,
                offline: false,
                is_system: system.contains(&number),
                hosts_app: app.contains(&number),
                table: TableKind::None,
                partitions: vec![],
                used_bytes: None,
                fingerprint: String::new(),
                deep: false,
                notes: vec![tf!("Windows refuse l'accès à ce disque ({e}).", "Windows denies access to this disk ({e}).")],
                scsi_address: None,
            });
        }
    };
    let mut notes = Vec::new();
    let mut desc = vec![0u8; 2048];
    let (mut vendor, mut model, mut firmware, mut serial, mut bus, mut removable) =
        (String::new(), String::new(), String::new(), String::new(), 0u32, false);
    match property(&h, 0, &mut desc) {
        Some(n) if n >= 36 => {
            let d = &desc[..n];
            removable = d[10] != 0;
            vendor = cstr_at(d, u32le(d, 12) as usize);
            model = cstr_at(d, u32le(d, 16) as usize);
            firmware = cstr_at(d, u32le(d, 20) as usize);
            serial = normalize_serial(&cstr_at(d, u32le(d, 24) as usize));
            bus = u32le(d, 28);
        }
        _ => notes.push(t!("Identité du disque non communiquée par Windows.", "Disk identity not reported by Windows.").into()),
    }
    let geometry = query_geometry(&h);
    let size = geometry.map(|g| g.0).unwrap_or(0);
    let (sector, physical) = query_sector_sizes(&h).or(geometry.map(|g| (g.1, g.1))).unwrap_or((512, 512));
    if size == 0 {
        notes.push(t!("Aucun support détecté (taille nulle).", "No media detected (zero size).").into());
    }

    // SSD or hard disk: seek penalty (property 7).
    let mut seek = [0u8; 16];
    let seek_penalty = match property(&h, 7, &mut seek) {
        Some(n) if n >= 9 => Some(seek[8] != 0),
        _ => None,
    };
    let media = match (bus, seek_penalty) {
        (14 | 15, _) => MediaKind::Virtual,
        (17, _) => MediaKind::Ssd,
        (_, Some(false)) => MediaKind::Ssd,
        (_, Some(true)) => MediaKind::Hdd,
        _ => MediaKind::Unknown,
    };

    let mut attrs = [0u8; 16];
    let (offline, read_only) = match ioctl(&h, IOCTL_DISK_GET_DISK_ATTRIBUTES, &[], &mut attrs) {
        Ok(n) if n >= 16 => {
            let a = u64le(&attrs, 8);
            (a & 1 != 0, a & 2 != 0)
        }
        _ => (false, false),
    };

    let scsi_address = scsi_address(&h);
    let (table, mut partitions, lnotes) = layout_partitions(&h, number, sector, volumes);
    notes.extend(lnotes);
    drop(h);

    let display_model = match (vendor.is_empty(), model.is_empty()) {
        (false, false) if !model.to_ascii_lowercase().starts_with(&vendor.to_ascii_lowercase()) => {
            format!("{vendor} {model}")
        }
        (_, false) => model.clone(),
        (false, true) => vendor.clone(),
        _ => String::new(),
    };

    // Direct read (administrator rights): completes the partitions without a letter.
    let mut is_deep = false;
    let mut table = table;
    if deep && size > 0 {
        match RawDisk::open_read_only(&path) {
            Ok(mut dev) => match analyze_disk(&mut dev) {
                Ok(a) => {
                    is_deep = true;
                    table = a.layout.kind;
                    let direct = partition_infos(&a);
                    for d in direct {
                        match partitions.iter_mut().find(|p| p.offset == d.offset) {
                            Some(p) => {
                                if p.fs.is_none() || p.fs.as_deref() == Some("RAW") {
                                    p.fs = d.fs.clone();
                                }
                                if p.used_bytes.is_none() {
                                    p.used_bytes = d.used_bytes;
                                }
                                if p.label.is_none() {
                                    p.label = d.label.clone();
                                }
                            }
                            None => partitions.push(d),
                        }
                    }
                    partitions.sort_by_key(|p| p.offset);
                }
                Err(e) => notes.push(tf!("Lecture directe impossible ({e}).", "Direct read impossible ({e}).")),
            },
            Err(e) => notes.push(tf!("Ouverture en lecture impossible ({e}).", "Cannot open for reading ({e}).")),
        }
    }

    let used_total = crate::model::estimate_used(&partitions);
    let bus_s = bus_label(bus).to_string();
    Some(DiskInfo {
        id: format!("disk:{number}"),
        kind: DiskKind::Physical,
        fingerprint: fingerprint(&display_model, &serial, size, &bus_s, sector),
        path,
        number: Some(number),
        model: display_model,
        vendor,
        serial,
        firmware,
        bus: bus_s,
        media,
        size,
        sector_size: sector,
        physical_sector_size: physical,
        removable,
        read_only,
        offline,
        is_system: system.contains(&number),
        hosts_app: app.contains(&number),
        table,
        used_bytes: used_total,
        partitions,
        deep: is_deep,
        notes,
        scsi_address,
    })
}

/// SCSI address of the disk (port, path, target, LUN), from `IOCTL_SCSI_GET_ADDRESS`.
fn scsi_address(h: &Handle) -> Option<[u8; 4]> {
    // CTL_CODE(IOCTL_SCSI_BASE, 0x0406, METHOD_BUFFERED, FILE_ANY_ACCESS)
    const IOCTL_SCSI_GET_ADDRESS: u32 = 0x0004_1018;
    // SCSI_ADDRESS: Length (u32), PortNumber, PathId, TargetId, Lun (u8 each).
    let mut a = [0u8; 8];
    match ioctl(h, IOCTL_SCSI_GET_ADDRESS, &[], &mut a) {
        Ok(n) if n >= 8 => Some([a[4], a[5], a[6], a[7]]),
        _ => None,
    }
}

/// Lists the disks seen by Windows. `deep` enables direct reading (read-only)
/// and only has an effect with administrator rights. `app_paths`: program
/// folders to protect (data, reports).
pub fn list_disks(deep: bool, app_paths: &[String]) -> Vec<DiskInfo> {
    let volumes = list_volumes();
    let system = system_disks(&volumes);
    let app = app_disks(&volumes, app_paths);
    let deep = deep && super::is_elevated();
    (0..MAX_DISKS).filter_map(|n| describe(n, &volumes, &system, &app, deep)).collect()
}

/// Describes a single disk (re-validation just before an operation).
pub fn disk_by_number(number: u32, deep: bool, app_paths: &[String]) -> Option<DiskInfo> {
    let volumes = list_volumes();
    let system = system_disks(&volumes);
    let app = app_disks(&volumes, app_paths);
    describe(number, &volumes, &system, &app, deep && super::is_elevated())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serial_normalisation() {
        // "WD-WCC4E1234567" encoded in hexadecimal with swapped bytes.
        let plain = "DWW-CCE4214365 7";
        let hex: String = plain.bytes().map(|b| format!("{b:02x}")).collect();
        assert_eq!(normalize_serial(&hex), "WD-WCC4E1234567");
        assert_eq!(normalize_serial("  S4EWNX0M123456  "), "S4EWNX0M123456");
        assert_eq!(normalize_serial("0025_3856_01A2_B3C4."), "0025_3856_01A2_B3C4.");
    }

    /// Read-only: locates this test's own files. Whatever the spelling (drive letter,
    /// `\\?\` prefix, file not created yet), the answer is the disk of the volume.
    #[test]
    fn file_location_follows_the_volume_not_the_spelling() {
        let dir = std::env::temp_dir().join(format!("chudclone-where-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("x.bin");
        std::fs::write(&file, b"x").unwrap();
        let disks = disks_of_file(&file);
        let prefixed = disks_of_file(&std::path::PathBuf::from(format!(r"\\?\{}", file.display())));
        let missing = disks_of_file(&dir.join("not-yet").join("y.img"));
        let of_dir = disks_of_file(&dir);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(!disks.is_empty(), "the temporary folder is on a local disk");
        assert_eq!(prefixed, disks);
        assert_eq!(missing, disks, "a file to be created is located by its folder");
        assert_eq!(of_dir, disks);
    }

    #[test]
    fn mount_point_resolution_prefers_longest_prefix() {
        let v = vec![
            VolumeRec {
                guid_path: "a".into(),
                mounts: vec!["C:\\".into()],
                label: None,
                fs: None,
                total: None,
                free: None,
                extents: vec![(0, 0, 1)],
            },
            VolumeRec {
                guid_path: "b".into(),
                mounts: vec!["C:\\Montage\\".into()],
                label: None,
                fs: None,
                total: None,
                free: None,
                extents: vec![(3, 0, 1)],
            },
        ];
        assert_eq!(disks_of_path(&v, "c:\\windows"), vec![0]);
        assert_eq!(disks_of_path(&v, "C:\\Montage\\x\\y.exe"), vec![3]);
        assert!(disks_of_path(&v, "D:\\x").is_empty());
    }
}
