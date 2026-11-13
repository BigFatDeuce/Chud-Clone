//! FAT12/16/32 : géométrie, carte des clusters utilisés et recherche de fichiers.

use super::{Extent, FsAnalysis, FsKind, normalize};
use crate::device::{BlockDevice, DeviceReader, read_vec};
use crate::diag::Finding;
use crate::partition::{le16, le32};
use std::io::Read;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FatGeometry {
    pub kind: FsKind,
    pub bytes_per_sector: u64,
    pub sectors_per_cluster: u64,
    pub reserved_sectors: u64,
    pub fat_count: u64,
    pub fat_sectors: u64,
    pub total_sectors: u64,
    pub first_data_sector: u64,
    pub cluster_count: u64,
    pub label: Option<String>,
}

impl FatGeometry {
    pub fn cluster_size(&self) -> u64 {
        self.bytes_per_sector * self.sectors_per_cluster
    }
    pub fn fs_size(&self) -> u64 {
        self.total_sectors * self.bytes_per_sector
    }
}

/// Analyse le BPB d'un secteur d'amorçage FAT. `None` si ce n'en est pas un.
pub fn parse_bpb(s: &[u8]) -> Option<FatGeometry> {
    if s.len() < 512 || s[510] != 0x55 || s[511] != 0xAA {
        return None;
    }
    if !(s[0] == 0xEB || s[0] == 0xE9) {
        return None;
    }
    let bps = le16(s, 11) as u64;
    let spc = s[13] as u64;
    let reserved = le16(s, 14) as u64;
    let fats = s[16] as u64;
    let root_entries = le16(s, 17) as u64;
    let total16 = le16(s, 19) as u64;
    let fat16 = le16(s, 22) as u64;
    let total32 = le32(s, 32) as u64;
    if !matches!(bps, 512 | 1024 | 2048 | 4096) || spc == 0 || !spc.is_power_of_two() || reserved == 0 || !(1..=2).contains(&fats) {
        return None;
    }
    if s[21] < 0xF0 {
        return None;
    }
    let fat_sectors = if fat16 != 0 { fat16 } else { le32(s, 36) as u64 };
    let total = if total16 != 0 { total16 } else { total32 };
    if fat_sectors == 0 || total == 0 {
        return None;
    }
    let root_dir_sectors = (root_entries * 32).div_ceil(bps);
    let first_data = reserved + fats * fat_sectors + root_dir_sectors;
    if first_data >= total {
        return None;
    }
    let clusters = (total - first_data) / spc;
    let kind = if clusters < 4085 {
        FsKind::Fat12
    } else if clusters < 65525 {
        FsKind::Fat16
    } else {
        FsKind::Fat32
    };
    // Cohérence : une FAT32 n'a pas de répertoire racine fixe ni de champ FAT16.
    if kind == FsKind::Fat32 && (fat16 != 0 || root_entries != 0) {
        return None;
    }
    if kind != FsKind::Fat32 && root_entries == 0 {
        return None;
    }
    // La FAT doit pouvoir décrire tous les clusters.
    let bits = match kind {
        FsKind::Fat12 => 12,
        FsKind::Fat16 => 16,
        _ => 32,
    };
    if ((clusters + 2) * bits).div_ceil(8) > fat_sectors * bps {
        return None;
    }
    let label_off = if kind == FsKind::Fat32 { 71 } else { 43 };
    let raw = String::from_utf8_lossy(&s[label_off..label_off + 11]).trim().to_string();
    let label = if raw.is_empty() || raw == "NO NAME" { None } else { Some(raw) };
    Some(FatGeometry {
        kind,
        bytes_per_sector: bps,
        sectors_per_cluster: spc,
        reserved_sectors: reserved,
        fat_count: fats,
        fat_sectors,
        total_sectors: total,
        first_data_sector: first_data,
        cluster_count: clusters,
        label,
    })
}

pub fn probe(head: &[u8]) -> Option<FsKind> {
    parse_bpb(head).map(|g| g.kind)
}

fn fat_entry(kind: FsKind, fat: &[u8], n: u64) -> u32 {
    let n = n as usize;
    match kind {
        FsKind::Fat12 => {
            let o = n + n / 2;
            let v = u16::from_le_bytes([fat[o], fat[o + 1]]);
            (if n.is_multiple_of(2) { v & 0x0FFF } else { v >> 4 }) as u32
        }
        FsKind::Fat16 => le16(fat, n * 2) as u32,
        _ => le32(fat, n * 4) & 0x0FFF_FFFF,
    }
}

fn bad_marker(kind: FsKind) -> u32 {
    match kind {
        FsKind::Fat12 => 0xFF7,
        FsKind::Fat16 => 0xFFF7,
        _ => 0x0FFF_FFF7,
    }
}

pub(super) fn analyze(dev: &mut dyn BlockDevice, base: u64, len: u64, out: &mut FsAnalysis) {
    let ss = dev.sector_size() as u64;
    if let Err(e) = analyze_inner(dev, base, len, out) {
        out.used_extents = None;
        out.raw_reason = Some(tf!("structures FAT illisibles ({e})", "unreadable FAT structures ({e})"));
        out.findings.push(
            Finding::error(
                "FAT_UNREADABLE",
                t!("Structures FAT illisibles", "Unreadable FAT structures"),
                tf!(
                    "La table d'allocation n'a pas pu être lue : {e}. La partition sera copiée intégralement, secteur par secteur.",
                    "The allocation table could not be read: {e}. The partition will be copied in full, sector by sector."
                ),
            )
            .at_lba(base / ss),
        );
    }
}

fn analyze_inner(dev: &mut dyn BlockDevice, base: u64, len: u64, out: &mut FsAnalysis) -> Result<(), String> {
    let ss = dev.sector_size() as u64;
    let boot = read_vec(dev, base, ss.max(512) as usize).map_err(|e| e.to_string())?;
    let g = parse_bpb(&boot).ok_or(t!("BPB invalide", "invalid BPB"))?;
    out.kind = g.kind;
    out.label = g.label.clone();
    out.cluster_size = Some(g.cluster_size() as u32);
    out.fs_size = Some(g.fs_size());
    if g.fs_size() > len {
        out.raw_reason = Some(
            t!("le système de fichiers déclare une taille supérieure à sa partition", "the file system claims a size larger than its partition")
                .into(),
        );
        out.findings.push(Finding::error(
            "FAT_LARGER_THAN_PARTITION",
            t!("Volume FAT plus grand que sa partition", "FAT volume larger than its partition"),
            tf!(
                "Le volume FAT déclare {} octets mais la partition n'en contient que {len}.",
                "The FAT volume claims {} bytes but the partition only holds {len}.",
                g.fs_size()
            ),
        ));
        return Ok(());
    }
    if g.bytes_per_sector != ss {
        out.findings.push(Finding::warning(
            "FAT_SECTOR_MISMATCH",
            t!("Taille de secteur FAT différente de celle du disque", "FAT sector size differs from the disk's"),
            tf!(
                "Le volume a été formaté pour des secteurs de {} octets, le disque en annonce {ss}.",
                "The volume was formatted for {}-byte sectors; the disk reports {ss}.",
                g.bytes_per_sector
            ),
        ));
    }

    // Lecture de la première FAT.
    let fat_bytes = g.fat_sectors * g.bytes_per_sector;
    let fat_off = g.reserved_sectors * g.bytes_per_sector;
    let mut fat = vec![0u8; fat_bytes as usize + 4];
    {
        let mut r = DeviceReader::new(dev, base, len);
        std::io::Seek::seek(&mut r, std::io::SeekFrom::Start(fat_off)).map_err(|e| e.to_string())?;
        r.read_exact(&mut fat[..fat_bytes as usize]).map_err(|e| tf!("FAT illisible : {e}", "unreadable FAT: {e}"))?;
    }

    let mut clean = true;
    match g.kind {
        FsKind::Fat16 => clean = fat_entry(g.kind, &fat, 1) & 0x8000 != 0,
        FsKind::Fat32 => clean = le32(&fat, 4) & 0x0800_0000 != 0,
        _ => {}
    }

    let bad = bad_marker(g.kind);
    let cs = g.cluster_size();
    let data_off = g.first_data_sector * g.bytes_per_sector;
    let mut extents = vec![Extent { offset: 0, len: data_off }];
    let mut used = 0u64;
    let mut bad_count = 0u64;
    let mut run: Option<u64> = None;
    for c in 0..g.cluster_count {
        let v = fat_entry(g.kind, &fat, c + 2);
        if v == bad {
            bad_count += 1;
        }
        let is_used = v != 0 && v != bad;
        match (is_used, run) {
            (true, None) => run = Some(c),
            (false, Some(s)) => {
                extents.push(Extent { offset: data_off + s * cs, len: (c - s) * cs });
                run = None;
            }
            _ => {}
        }
        if is_used {
            used += 1;
        }
    }
    if let Some(s) = run {
        extents.push(Extent { offset: data_off + s * cs, len: (g.cluster_count - s) * cs });
    }
    out.used_bytes = Some(data_off + used * cs);
    let extents = normalize(extents);
    out.last_used_end = extents.last().map(|e| e.end());
    if bad_count > 0 {
        out.findings.push(Finding::warning(
            "FAT_BAD_CLUSTERS",
            t!("Clusters marqués défectueux", "Clusters marked bad"),
            tf!(
                "La table d'allocation marque {bad_count} cluster(s) comme défectueux. Ils ne sont pas copiés par le clonage intelligent.",
                "The allocation table marks {bad_count} cluster(s) as bad. They are not copied by smart cloning."
            ),
        ));
    }
    if clean {
        out.used_extents = Some(extents);
    } else {
        out.raw_reason = Some(t!("le volume FAT n'a pas été démonté proprement", "the FAT volume was not unmounted cleanly").into());
        out.findings.push(Finding::info(
            "FAT_DIRTY",
            t!("Volume FAT non démonté proprement", "FAT volume not unmounted cleanly"),
            t!(
                "Le volume porte l'indicateur « non démonté proprement ». Par prudence, la partition sera copiée intégralement.",
                "The volume carries the \"not unmounted cleanly\" flag. To be safe, the partition will be copied in full."
            ),
        ));
    }
    Ok(())
}

/// Taille d'un fichier d'un volume FAT, ou `None` s'il n'existe pas.
/// `path` utilise `/` comme séparateur. Lecture seule garantie par `DeviceReader`.
pub fn file_size(dev: &mut dyn BlockDevice, base: u64, len: u64, path: &str) -> Result<Option<u64>, String> {
    let reader = DeviceReader::new(dev, base, len);
    let fs = fatfs::FileSystem::new(reader, fatfs::FsOptions::new().update_accessed_date(false)).map_err(|e| e.to_string())?;
    let root = fs.root_dir();
    let (dir_path, name) = match path.rsplit_once('/') {
        Some((d, n)) => (d, n),
        None => ("", path),
    };
    let dir = if dir_path.is_empty() {
        root
    } else {
        match root.open_dir(dir_path) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.to_string()),
        }
    };
    for entry in dir.iter() {
        let entry = entry.map_err(|e| e.to_string())?;
        if entry.file_name().eq_ignore_ascii_case(name) {
            return Ok(Some(entry.len()));
        }
    }
    Ok(None)
}

/// Lit un fichier complet d'un volume FAT (tests et vérifications).
pub fn read_file(dev: &mut dyn BlockDevice, base: u64, len: u64, path: &str) -> Result<Option<Vec<u8>>, String> {
    let reader = DeviceReader::new(dev, base, len);
    let fs = fatfs::FileSystem::new(reader, fatfs::FsOptions::new().update_accessed_date(false)).map_err(|e| e.to_string())?;
    let root = fs.root_dir();
    let mut f = match root.open_file(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    let mut data = Vec::new();
    f.read_to_end(&mut data).map_err(|e| e.to_string())?;
    Ok(Some(data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::MemDevice;
    use std::io::{Cursor, Write};

    /// Formate un volume FAT avec une implémentation indépendante (crate fatfs).
    fn make_fat(size: usize, fat_type: fatfs::FatType) -> Vec<u8> {
        let mut cur = Cursor::new(vec![0u8; size]);
        fatfs::format_volume(&mut cur, fatfs::FormatVolumeOptions::new().fat_type(fat_type).volume_label(*b"TESTVOL    ")).unwrap();
        {
            let fs = fatfs::FileSystem::new(&mut cur, fatfs::FsOptions::new()).unwrap();
            let root = fs.root_dir();
            root.create_dir("EFI").unwrap();
            let mut f = root.create_file("EFI/a.bin").unwrap();
            f.write_all(&vec![0x5Au8; 300_000]).unwrap();
            let mut g = root.create_file("b.txt").unwrap();
            g.write_all(b"bonjour").unwrap();
            let mut h = root.create_file("c.bin").unwrap();
            h.write_all(&vec![0x11u8; 100_000]).unwrap();
            drop((f, g, h));
            root.remove("b.txt").unwrap();
        }
        cur.into_inner()
    }

    #[test]
    fn analyze_fat_types_against_independent_formatter() {
        for (size, ty, kind) in [
            (2 << 20, fatfs::FatType::Fat12, FsKind::Fat12),
            (16 << 20, fatfs::FatType::Fat16, FsKind::Fat16),
            (64 << 20, fatfs::FatType::Fat32, FsKind::Fat32),
        ] {
            let img = make_fat(size, ty);
            let mut dev = MemDevice::from_vec(img.clone(), 512);
            let a = crate::fs::analyze(&mut dev, 0, size as u64);
            assert_eq!(a.kind, kind);
            assert_eq!(a.label.as_deref(), Some("TESTVOL"));
            let ext = a.used_extents.clone().expect("volume propre : étendues disponibles");
            let used: u64 = ext.iter().map(|e| e.len).sum();
            assert_eq!(Some(used), a.used_bytes);
            assert!(used < size as u64 / 2, "seule une fraction du volume est utilisée");

            // Preuve de suffisance : une image ne contenant QUE les étendues utilisées
            // doit redonner exactement les mêmes fichiers avec le lecteur indépendant.
            let mut sparse = vec![0xEEu8; size];
            for e in &ext {
                let (o, l) = (e.offset as usize, e.len as usize);
                sparse[o..o + l].copy_from_slice(&img[o..o + l]);
            }
            let mut dev2 = MemDevice::from_vec(sparse, 512);
            let data = read_file(&mut dev2, 0, size as u64, "EFI/a.bin").unwrap().unwrap();
            assert_eq!(data, vec![0x5Au8; 300_000]);
            assert_eq!(file_size(&mut dev2, 0, size as u64, "efi/A.BIN").unwrap(), Some(300_000));
            assert_eq!(file_size(&mut dev2, 0, size as u64, "b.txt").unwrap(), None);
            assert_eq!(file_size(&mut dev2, 0, size as u64, "nope/x").unwrap(), None);
            assert_eq!(dev2.writes, 0, "aucune écriture pendant l'analyse");
        }
    }

    #[test]
    fn dirty_fat32_falls_back_to_raw() {
        let mut img = make_fat(64 << 20, fatfs::FatType::Fat32);
        let g = parse_bpb(&img).unwrap();
        // Efface le bit « démonté proprement » de FAT[1].
        let o = (g.reserved_sectors * g.bytes_per_sector) as usize + 4;
        let v = le32(&img, o) & !0x0800_0000;
        img[o..o + 4].copy_from_slice(&v.to_le_bytes());
        let mut dev = MemDevice::from_vec(img, 512);
        let a = crate::fs::analyze(&mut dev, 0, 64 << 20);
        assert!(a.used_extents.is_none());
        assert!(a.findings.iter().any(|f| f.code == "FAT_DIRTY"));
        assert!(a.used_bytes.is_some());
    }

    #[test]
    fn garbage_is_not_fat() {
        let mut s = vec![0u8; 512];
        s[510] = 0x55;
        s[511] = 0xAA;
        assert!(parse_bpb(&s).is_none());
    }
}
