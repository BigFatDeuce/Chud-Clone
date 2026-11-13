//! Détection des disques et des volumes sous Windows.
//!
//! La détection de base n'exige aucun privilège : elle n'utilise que des requêtes de
//! propriétés (aucune lecture de secteur). Avec les droits administrateur, une lecture
//! directe (seule) complète les informations que Windows n'expose pas : système de
//! fichiers et occupation des partitions sans lettre (EFI, récupération).

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

/// Nombre maximal de numéros de disque sondés.
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
    /// (numéro de disque, décalage en octets, longueur).
    pub extents: Vec<(u32, u64, u64)>,
}

fn cstr_at(buf: &[u8], offset: usize) -> String {
    if offset == 0 || offset >= buf.len() {
        return String::new();
    }
    let end = buf[offset..].iter().position(|b| *b == 0).map(|p| offset + p).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[offset..end]).trim().to_string()
}

/// Certains pilotes renvoient le numéro de série ATA sous forme hexadécimale, octets
/// permutés deux à deux. On le remet en clair lorsque c'est manifestement le cas.
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

/// Énumère les volumes Windows et leurs emplacements sur les disques.
pub fn list_volumes() -> Vec<VolumeRec> {
    let mut out = Vec::new();
    let mut name = [0u16; 260];
    // SAFETY: tampon de 260 caractères.
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
    // Emplacement sur disque : sans lui (lecteur optique, réseau), le volume ne nous concerne pas.
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

/// Numéros des disques portant le volume qui contient `path`.
fn disks_of_path(volumes: &[VolumeRec], path: &str) -> Vec<u32> {
    let p = path.to_ascii_lowercase();
    // Point de montage le plus long qui préfixe le chemin.
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
    // Volumes portant un fichier d'échange ou d'hibernation ACTIF. Un disque Windows
    // branché en externe en contient aussi, mais inactifs : ce n'est pas un disque
    // système et il doit pouvoir être cloné. Le noyau garde les fichiers actifs ouverts
    // sans aucun partage : leur ouverture en lecture échoue alors par violation de
    // partage, ce qui ne se produit pas pour un fichier inactif.
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

/// Vrai si le fichier existe et qu'il est tenu ouvert sans partage (fichier d'échange
/// ou d'hibernation en service).
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

/// Chemins de volume (`\\?\Volume{…}\`, nom affichable) des volumes d'un disque.
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

/// Décrit le disque `\\.\PhysicalDrive<number>`, ou `None` s'il n'existe pas.
fn describe(number: u32, volumes: &[VolumeRec], system: &[u32], app: &[u32], deep: bool) -> Option<DiskInfo> {
    let path = format!(r"\\.\PhysicalDrive{number}");
    let h = match open_device(&path, Access::Query, 0) {
        Ok(h) => h,
        Err(e) => {
            let code = e.raw_os_error().unwrap_or(0) as u32;
            if code == ERROR_FILE_NOT_FOUND || code == ERROR_PATH_NOT_FOUND {
                return None;
            }
            // Le disque existe mais refuse même les requêtes : on le signale plutôt que de le cacher.
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

    // SSD ou disque dur : pénalité de positionnement (propriété 7).
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

    // Lecture directe (droits administrateur) : complète les partitions sans lettre.
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
    })
}

/// Liste les disques vus par Windows. `deep` active la lecture directe (lecture
/// seule) et n'a d'effet qu'avec les droits administrateur. `app_paths` : dossiers
/// du programme à protéger (données, rapports).
pub fn list_disks(deep: bool, app_paths: &[String]) -> Vec<DiskInfo> {
    let volumes = list_volumes();
    let system = system_disks(&volumes);
    let app = app_disks(&volumes, app_paths);
    let deep = deep && super::is_elevated();
    (0..MAX_DISKS).filter_map(|n| describe(n, &volumes, &system, &app, deep)).collect()
}

/// Décrit un seul disque (revalidation juste avant une opération).
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
        // « WD-WCC4E1234567 » encodé en hexadécimal avec octets permutés.
        let plain = "DWW-CCE4214365 7";
        let hex: String = plain.bytes().map(|b| format!("{b:02x}")).collect();
        assert_eq!(normalize_serial(&hex), "WD-WCC4E1234567");
        assert_eq!(normalize_serial("  S4EWNX0M123456  "), "S4EWNX0M123456");
        assert_eq!(normalize_serial("0025_3856_01A2_B3C4."), "0025_3856_01A2_B3C4.");
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
