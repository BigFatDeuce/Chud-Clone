//! MBR partition table (including the EBR chain of logical partitions).

use super::{MbrMeta, PartInfo, Partition, le16, le32};
use crate::device::{BlockDevice, read_vec};
use crate::diag::Finding;
use std::io;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MbrKind {
    /// No 0x55AA signature: sector 0 empty or unknown.
    Empty,
    /// Table MBR classique.
    Classic,
    /// GPT protective MBR (a single 0xEE entry).
    Protective,
    /// 0xEE entry together with other partitions.
    Hybrid,
    /// Sector 0 is the boot sector of a file system.
    Superfloppy,
}

pub struct MbrRead {
    pub kind: MbrKind,
    pub partitions: Vec<Partition>,
    pub meta: Option<MbrMeta>,
    pub findings: Vec<Finding>,
}

#[derive(Debug, Clone, Copy)]
struct RawEntry {
    status: u8,
    type_id: u8,
    start: u32,
    sectors: u32,
}

fn entry(sector: &[u8], i: usize) -> RawEntry {
    let o = 446 + i * 16;
    RawEntry { status: sector[o], type_id: sector[o + 4], start: le32(sector, o + 8), sectors: le32(sector, o + 12) }
}

fn is_extended(t: u8) -> bool {
    matches!(t, 0x05 | 0x0F | 0x85)
}

/// Recognises a file system boot sector (NTFS, exFAT, FAT).
pub fn looks_like_fs_boot_sector(s: &[u8]) -> bool {
    if s.len() < 512 {
        return false;
    }
    let oem = &s[3..11];
    if oem == b"NTFS    " || oem == b"EXFAT   " || oem == b"-FVE-FS-" || &s[3..7] == b"ReFS" {
        return true;
    }
    let jump = s[0] == 0xEB || s[0] == 0xE9;
    let bps = le16(s, 11);
    let spc = s[13];
    let fats = s[16];
    let bpb_ok = matches!(bps, 512 | 1024 | 2048 | 4096) && spc.is_power_of_two() && (1..=2).contains(&fats) && s[21] >= 0xF0;
    let fat_label = &s[54..59] == b"FAT12" || &s[54..59] == b"FAT16" || &s[82..87] == b"FAT32";
    jump && bpb_ok && fat_label
}

pub fn read_mbr(dev: &mut dyn BlockDevice) -> io::Result<MbrRead> {
    let ss = dev.sector_size() as usize;
    let s0 = read_vec(dev, 0, ss)?;
    let sector = &s0[..ss];
    let mut findings = Vec::new();

    if sector[510] != 0x55 || sector[511] != 0xAA {
        return Ok(MbrRead { kind: MbrKind::Empty, partitions: vec![], meta: None, findings });
    }
    if looks_like_fs_boot_sector(sector) {
        return Ok(MbrRead { kind: MbrKind::Superfloppy, partitions: vec![], meta: None, findings });
    }

    let entries: Vec<RawEntry> = (0..4).map(|i| entry(sector, i)).collect();
    let used: Vec<(usize, RawEntry)> = entries.iter().copied().enumerate().filter(|(_, e)| e.type_id != 0 && e.sectors != 0).collect();
    let signature = le32(sector, 440);
    let boot_code_present = sector[..440].iter().any(|b| *b != 0);
    let mut meta = MbrMeta { signature, boot_code_present, extended: None, ebr_lbas: vec![], sector0: sector.to_vec() };

    let has_ee = used.iter().any(|(_, e)| e.type_id == 0xEE);
    if has_ee {
        let kind = if used.len() == 1 { MbrKind::Protective } else { MbrKind::Hybrid };
        return Ok(MbrRead { kind, partitions: vec![], meta: Some(meta), findings });
    }

    let mut partitions = Vec::new();
    let mut extended: Option<(usize, RawEntry)> = None;
    for (i, e) in &used {
        if e.status != 0x00 && e.status != 0x80 {
            findings.push(Finding::warning(
                "MBR_BAD_STATUS",
                t!("Indicateur d'amorçage invalide", "Invalid boot indicator"),
                tf!(
                    "L'entrée {} du MBR porte un indicateur 0x{:02X} (attendu 0x00 ou 0x80).",
                    "MBR entry {} carries indicator 0x{:02X} (expected 0x00 or 0x80).",
                    i + 1,
                    e.status
                ),
            ));
        }
        if is_extended(e.type_id) {
            if extended.is_some() {
                findings.push(Finding::warning(
                    "MBR_MULTIPLE_EXTENDED",
                    t!("Plusieurs partitions étendues", "Several extended partitions"),
                    t!(
                        "Le MBR contient plusieurs partitions étendues ; seule la première est analysée.",
                        "The MBR contains several extended partitions; only the first one is analysed."
                    ),
                ));
            } else {
                extended = Some((*i, *e));
            }
            continue;
        }
        partitions.push(Partition {
            number: *i as u32 + 1,
            start_lba: e.start as u64,
            sectors: e.sectors as u64,
            info: PartInfo::Mbr { type_id: e.type_id, active: e.status == 0x80, logical: false, slot: *i as u32, ebr_lba: None },
        });
    }

    if let Some((_, ext)) = extended {
        let ext_start = ext.start as u64;
        let ext_end = ext_start + ext.sectors as u64;
        meta.extended = Some((ext_start, ext.sectors as u64));
        let disk_sectors = dev.size() / ss as u64;
        let mut ebr_lba = ext_start;
        let mut rank = 0u32;
        loop {
            if rank >= 256 {
                findings.push(Finding::error(
                    "EBR_CHAIN_TOO_LONG",
                    t!("Chaîne de partitions logiques anormale", "Abnormal logical partition chain"),
                    t!(
                        "Plus de 256 maillons EBR : la chaîne est probablement corrompue ou bouclée.",
                        "More than 256 EBR links: the chain is probably corrupted or looping."
                    ),
                ));
                break;
            }
            if ebr_lba >= disk_sectors {
                findings.push(
                    Finding::error(
                        "EBR_BEYOND_DISK",
                        t!("EBR hors du disque", "EBR outside the disk"),
                        tf!(
                            "Un EBR est référencé au LBA {ebr_lba}, au-delà de la fin du disque.",
                            "An EBR is referenced at LBA {ebr_lba}, past the end of the disk."
                        ),
                    )
                    .at_lba(ebr_lba),
                );
                break;
            }
            let raw = read_vec(dev, ebr_lba * ss as u64, ss)?;
            let ebr = &raw[..ss];
            if ebr[510] != 0x55 || ebr[511] != 0xAA {
                // An empty extended partition may not contain an initialised EBR.
                if rank > 0 || ebr.iter().any(|b| *b != 0) {
                    findings.push(
                        Finding::error(
                            "EBR_INVALID",
                            t!("EBR invalide", "Invalid EBR"),
                            tf!(
                                "L'EBR au LBA {ebr_lba} n'a pas de signature valide ; les partitions logiques suivantes sont perdues.",
                                "The EBR at LBA {ebr_lba} has no valid signature; the following logical partitions are lost."
                            ),
                        )
                        .at_lba(ebr_lba),
                    );
                }
                break;
            }
            meta.ebr_lbas.push(ebr_lba);
            let e0 = entry(ebr, 0);
            let e1 = entry(ebr, 1);
            if e0.type_id != 0 && e0.sectors != 0 {
                partitions.push(Partition {
                    number: 5 + rank,
                    start_lba: ebr_lba + e0.start as u64,
                    sectors: e0.sectors as u64,
                    info: PartInfo::Mbr { type_id: e0.type_id, active: e0.status == 0x80, logical: true, slot: rank, ebr_lba: Some(ebr_lba) },
                });
            }
            if is_extended(e1.type_id) && e1.start != 0 {
                let next = ext_start + e1.start as u64;
                if next <= ebr_lba || next >= ext_end {
                    findings.push(
                        Finding::error(
                            "EBR_CHAIN_CORRUPT",
                            t!("Chaîne de partitions logiques corrompue", "Corrupted logical partition chain"),
                            tf!(
                                "L'EBR au LBA {ebr_lba} pointe vers le LBA {next}, hors de la partition étendue ou en arrière.",
                                "The EBR at LBA {ebr_lba} points to LBA {next}, outside the extended partition or backwards."
                            ),
                        )
                        .at_lba(ebr_lba),
                    );
                    break;
                }
                ebr_lba = next;
                rank += 1;
            } else {
                break;
            }
        }
    }

    let actives = partitions.iter().filter(|p| p.is_active()).count();
    if actives > 1 {
        findings.push(Finding::warning(
            "MBR_MULTIPLE_ACTIVE",
            t!("Plusieurs partitions actives", "Several active partitions"),
            t!(
                "Plusieurs partitions sont marquées actives ; un BIOS n'en attend qu'une seule.",
                "Several partitions are marked active; a BIOS expects only one."
            ),
        ));
    }
    if signature == 0 && !partitions.is_empty() {
        findings.push(Finding::info(
            "MBR_ZERO_SIGNATURE",
            t!("Signature de disque nulle", "Zero disk signature"),
            t!(
                "La signature de disque MBR vaut zéro. Windows en attribuera une nouvelle au premier montage.",
                "The MBR disk signature is zero. Windows will assign a new one on first mount."
            ),
        ));
    }

    Ok(MbrRead { kind: MbrKind::Classic, partitions, meta: Some(meta), findings })
}

/// Rebuilds sector 0 for the destination from the source's.
/// `resized`: (primary entry index, new number of sectors).
pub fn build_sector0(src_sector0: &[u8], resized: &[(u32, u64)]) -> Result<Vec<u8>, String> {
    if src_sector0.len() < 512 {
        return Err(t!("secteur 0 source incomplet", "incomplete source sector 0").into());
    }
    let mut out = src_sector0.to_vec();
    for (slot, sectors) in resized {
        if *slot > 3 {
            return Err(t!("seule une partition primaire peut être redimensionnée", "only a primary partition can be resized").into());
        }
        let o = 446 + *slot as usize * 16;
        let start = le32(&out, o + 8) as u64;
        if start + sectors > u32::MAX as u64 {
            return Err(t!(
                "une partition MBR ne peut pas dépasser 2 Tio (limite de 2^32 secteurs)",
                "an MBR partition cannot exceed 2 TiB (2^32-sector limit)"
            )
            .into());
        }
        out[o + 12..o + 16].copy_from_slice(&(*sectors as u32).to_le_bytes());
        // Ending CHS: conventional "beyond the CHS limit" value.
        out[o + 5] = 0xFE;
        out[o + 6] = 0xFF;
        out[o + 7] = 0xFF;
    }
    Ok(out)
}

/// GPT protective MBR. Any boot code of the source is kept.
pub fn build_protective(src_sector0: Option<&[u8]>, sector_size: u32, disk_sectors: u64) -> Vec<u8> {
    let mut out = vec![0u8; sector_size as usize];
    if let Some(src) = src_sector0
        && src.len() >= 446
    {
        // Boot code and disk signature (bytes 0–445), table reset to zero.
        out[..446].copy_from_slice(&src[..446]);
    }
    let o = 446;
    out[o] = 0x00;
    out[o + 1] = 0x00;
    out[o + 2] = 0x02;
    out[o + 3] = 0x00;
    out[o + 4] = 0xEE;
    out[o + 5] = 0xFF;
    out[o + 6] = 0xFF;
    out[o + 7] = 0xFF;
    out[o + 8..o + 12].copy_from_slice(&1u32.to_le_bytes());
    let size = (disk_sectors - 1).min(u32::MAX as u64) as u32;
    out[o + 12..o + 16].copy_from_slice(&size.to_le_bytes());
    out[510] = 0x55;
    out[511] = 0xAA;
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::MemDevice;

    fn put_entry(s: &mut [u8], i: usize, status: u8, t: u8, start: u32, sectors: u32) {
        let o = 446 + i * 16;
        s[o] = status;
        s[o + 4] = t;
        s[o + 8..o + 12].copy_from_slice(&start.to_le_bytes());
        s[o + 12..o + 16].copy_from_slice(&sectors.to_le_bytes());
    }

    fn sign(s: &mut [u8]) {
        s[510] = 0x55;
        s[511] = 0xAA;
    }

    #[test]
    fn empty_disk() {
        let mut d = MemDevice::new(1 << 20, 512);
        let r = read_mbr(&mut d).unwrap();
        assert_eq!(r.kind, MbrKind::Empty);
    }

    #[test]
    fn classic_with_logical_chain() {
        let mut d = MemDevice::new(64 << 20, 512);
        {
            let s = &mut d.data[..512];
            s[440..444].copy_from_slice(&0xDEADBEEFu32.to_le_bytes());
            s[0] = 0x33;
            put_entry(s, 0, 0x80, 0x07, 2048, 20480);
            put_entry(s, 1, 0x00, 0x05, 22528, 100000);
            sign(s);
        }
        // EBR 1 at the start of the extended partition: logical at +2048, then next EBR at +40000.
        {
            let o = 22528 * 512;
            let s = &mut d.data[o..o + 512];
            put_entry(s, 0, 0, 0x07, 2048, 30000);
            put_entry(s, 1, 0, 0x05, 40000, 50000);
            sign(s);
        }
        {
            let o = (22528 + 40000) * 512;
            let s = &mut d.data[o..o + 512];
            put_entry(s, 0, 0, 0x0B, 2048, 40000);
            sign(s);
        }
        let r = read_mbr(&mut d).unwrap();
        assert_eq!(r.kind, MbrKind::Classic);
        assert_eq!(r.partitions.len(), 3);
        let meta = r.meta.unwrap();
        assert_eq!(meta.signature, 0xDEADBEEF);
        assert!(meta.boot_code_present);
        assert_eq!(meta.extended, Some((22528, 100000)));
        assert_eq!(meta.ebr_lbas, vec![22528, 62528]);
        assert_eq!(r.partitions[1].number, 5);
        assert_eq!(r.partitions[1].start_lba, 22528 + 2048);
        assert_eq!(r.partitions[2].number, 6);
        assert_eq!(r.partitions[2].start_lba, 62528 + 2048);
        assert!(r.partitions[2].is_logical());
        assert!(r.findings.is_empty(), "{:?}", r.findings);
    }

    #[test]
    fn looping_ebr_chain_is_reported_not_followed() {
        let mut d = MemDevice::new(32 << 20, 512);
        {
            let s = &mut d.data[..512];
            put_entry(s, 0, 0x00, 0x0F, 2048, 60000);
            sign(s);
        }
        {
            let o = 2048 * 512;
            let s = &mut d.data[o..o + 512];
            put_entry(s, 0, 0, 0x07, 63, 1000);
            // Points to itself (offset 0 forbidden), then backwards.
            put_entry(s, 1, 0, 0x05, 70000, 100);
            sign(s);
        }
        let r = read_mbr(&mut d).unwrap();
        assert!(r.findings.iter().any(|f| f.code == "EBR_CHAIN_CORRUPT"));
        assert_eq!(r.partitions.len(), 1);
    }

    #[test]
    fn protective_and_superfloppy() {
        let mut d = MemDevice::new(1 << 20, 512);
        {
            let s = &mut d.data[..512];
            put_entry(s, 0, 0, 0xEE, 1, 2047);
            sign(s);
        }
        assert_eq!(read_mbr(&mut d).unwrap().kind, MbrKind::Protective);
        let mut d = MemDevice::new(1 << 20, 512);
        {
            let s = &mut d.data[..512];
            s[3..11].copy_from_slice(b"NTFS    ");
            sign(s);
        }
        assert_eq!(read_mbr(&mut d).unwrap().kind, MbrKind::Superfloppy);
    }

    #[test]
    fn rebuild_sector0_resizes_only_requested_entry() {
        let mut s = vec![0u8; 512];
        put_entry(&mut s, 0, 0x80, 0x07, 2048, 1000);
        put_entry(&mut s, 1, 0, 0x07, 4096, 2000);
        sign(&mut s);
        let out = build_sector0(&s, &[(1, 9000)]).unwrap();
        assert_eq!(entry(&out, 0).sectors, 1000);
        assert_eq!(entry(&out, 1).sectors, 9000);
        assert_eq!(entry(&out, 1).start, 4096);
        assert!(build_sector0(&s, &[(1, u32::MAX as u64)]).is_err());
    }
}
