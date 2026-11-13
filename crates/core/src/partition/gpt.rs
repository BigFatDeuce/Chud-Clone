//! GUID partition table (GPT): reading with checks of the CRCs and of the backup
//! header, and building a new table for the destination.

use super::{GptMeta, PartInfo, Partition, le32, le64};
use crate::device::{BlockDevice, read_vec};
use crate::diag::Finding;
use crate::guid::Guid;
use std::io;

const SIGNATURE: &[u8; 8] = b"EFI PART";
const HEADER_SIZE: u32 = 92;
const REVISION_1_0: u32 = 0x0001_0000;
/// Safeguard: an entry array larger than 16 MiB is considered corrupt.
const MAX_ENTRIES_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GptHeader {
    pub revision: u32,
    pub header_size: u32,
    pub my_lba: u64,
    pub alternate_lba: u64,
    pub first_usable: u64,
    pub last_usable: u64,
    pub disk_guid: Guid,
    pub entries_lba: u64,
    pub num_entries: u32,
    pub entry_size: u32,
    pub entries_crc: u32,
}

pub struct GptRead {
    pub valid: bool,
    pub partitions: Vec<Partition>,
    pub meta: Option<GptMeta>,
    pub findings: Vec<Finding>,
}

fn crc32(data: &[u8]) -> u32 {
    let mut h = crc32fast::Hasher::new();
    h.update(data);
    h.finalize()
}

fn has_signature(sector: &[u8]) -> bool {
    sector.len() >= 8 && &sector[..8] == SIGNATURE
}

fn parse_header(sector: &[u8], at_lba: u64, disk_sectors: u64) -> Result<GptHeader, String> {
    if !has_signature(sector) {
        return Err(t!("signature « EFI PART » absente", "\"EFI PART\" signature missing").into());
    }
    let header_size = le32(sector, 12);
    if header_size < HEADER_SIZE || header_size as usize > sector.len() {
        return Err(tf!("taille d'en-tête invalide ({header_size})", "invalid header size ({header_size})"));
    }
    let stored_crc = le32(sector, 16);
    let mut copy = sector[..header_size as usize].to_vec();
    copy[16..20].fill(0);
    let computed = crc32(&copy);
    if computed != stored_crc {
        return Err(tf!(
            "CRC de l'en-tête incorrect (lu {stored_crc:08X}, calculé {computed:08X})",
            "wrong header CRC (read {stored_crc:08X}, computed {computed:08X})"
        ));
    }
    let h = GptHeader {
        revision: le32(sector, 8),
        header_size,
        my_lba: le64(sector, 24),
        alternate_lba: le64(sector, 32),
        first_usable: le64(sector, 40),
        last_usable: le64(sector, 48),
        disk_guid: Guid::from_bytes(&sector[56..72]),
        entries_lba: le64(sector, 72),
        num_entries: le32(sector, 80),
        entry_size: le32(sector, 84),
        entries_crc: le32(sector, 88),
    };
    if h.my_lba != at_lba {
        return Err(tf!(
            "l'en-tête se déclare au LBA {} mais se trouve au LBA {at_lba}",
            "the header claims LBA {} but sits at LBA {at_lba}",
            h.my_lba
        ));
    }
    if h.entry_size < 128 || h.entry_size > 4096 || !h.entry_size.is_multiple_of(8) {
        return Err(tf!("taille d'entrée invalide ({})", "invalid entry size ({})", h.entry_size));
    }
    let bytes = h.num_entries as u64 * h.entry_size as u64;
    if h.num_entries == 0 || bytes > MAX_ENTRIES_BYTES {
        return Err(tf!("nombre d'entrées invalide ({})", "invalid entry count ({})", h.num_entries));
    }
    if h.entries_lba < 2 || h.entries_lba >= disk_sectors {
        return Err(tf!("tableau d'entrées hors du disque (LBA {})", "entry array outside the disk (LBA {})", h.entries_lba));
    }
    if h.first_usable > h.last_usable {
        return Err(t!("plage utilisable incohérente", "inconsistent usable range").into());
    }
    Ok(h)
}

fn read_entries(dev: &mut dyn BlockDevice, h: &GptHeader) -> io::Result<Result<Vec<u8>, String>> {
    let ss = dev.sector_size() as u64;
    let bytes = h.num_entries as u64 * h.entry_size as u64;
    let rounded = bytes.div_ceil(ss) * ss;
    let off = h.entries_lba * ss;
    if off + rounded > dev.size() {
        return Ok(Err(t!("le tableau d'entrées dépasse la fin du disque", "the entry array runs past the end of the disk").into()));
    }
    let raw = read_vec(dev, off, rounded as usize)?;
    let data = raw[..bytes as usize].to_vec();
    if crc32(&data) != h.entries_crc {
        return Ok(Err(t!("CRC du tableau d'entrées incorrect", "wrong entry array CRC").into()));
    }
    Ok(Ok(data))
}

struct Candidate {
    header: GptHeader,
    entries: Vec<u8>,
}

fn try_read(dev: &mut dyn BlockDevice, lba: u64) -> io::Result<(bool, Result<Candidate, String>)> {
    let ss = dev.sector_size() as u64;
    let disk_sectors = dev.size() / ss;
    if lba >= disk_sectors {
        return Ok((false, Err(t!("LBA hors du disque", "LBA outside the disk").into())));
    }
    let raw = read_vec(dev, lba * ss, ss as usize)?;
    let sector = &raw[..ss as usize];
    let seen = has_signature(sector);
    let header = match parse_header(sector, lba, disk_sectors) {
        Ok(h) => h,
        Err(e) => return Ok((seen, Err(e))),
    };
    match read_entries(dev, &header)? {
        Ok(entries) => Ok((true, Ok(Candidate { header, entries }))),
        Err(e) => Ok((true, Err(e))),
    }
}

fn utf16_name(raw: &[u8]) -> String {
    let units: Vec<u16> = raw.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|u| *u != 0).collect();
    String::from_utf16_lossy(&units)
}

pub fn read_gpt(dev: &mut dyn BlockDevice) -> io::Result<GptRead> {
    let ss = dev.sector_size() as u64;
    let disk_sectors = dev.size() / ss;
    let last = disk_sectors - 1;
    let mut findings = Vec::new();

    // A read error on the primary header must not prevent trying the backup
    // header: that is precisely its role.
    let (seen_p, primary) = match try_read(dev, 1) {
        Ok(r) => r,
        Err(e) => (true, Err(tf!("secteur illisible ({e})", "unreadable sector ({e})"))),
    };
    let alt_hint = primary.as_ref().ok().map(|c| c.header.alternate_lba);
    let mut seen_b = false;
    let mut backup: Result<Candidate, String> = Err(t!("non trouvé", "not found").into());
    let mut backup_lba = last;
    let mut candidates = vec![last];
    if let Some(a) = alt_hint
        && a != last
        && a > 1
    {
        candidates.push(a);
    }
    for lba in candidates {
        let (seen, r) = match try_read(dev, lba) {
            Ok(r) => r,
            Err(e) => (false, Err(tf!("secteur illisible ({e})", "unreadable sector ({e})"))),
        };
        seen_b |= seen;
        if r.is_ok() {
            backup = r;
            backup_lba = lba;
            break;
        } else if backup.is_err() {
            backup = r;
        }
    }

    if !seen_p && !seen_b {
        return Ok(GptRead { valid: false, partitions: vec![], meta: None, findings });
    }

    let backup_at_end = backup.is_ok() && backup_lba == last;
    let (chosen, primary_valid, backup_valid) = match (&primary, &backup) {
        (Ok(p), Ok(b)) => {
            if p.header.disk_guid != b.header.disk_guid || p.header.entries_crc != b.header.entries_crc {
                findings.push(Finding::warning(
                    "GPT_MISMATCH",
                    t!("Table GPT principale et table de secours différentes", "Primary and backup GPT tables differ"),
                    t!("Les deux exemplaires de la table GPT sont individuellement valides mais ne décrivent pas les mêmes partitions. \
                     La table principale est utilisée. Une modification interrompue du partitionnement en est souvent la cause.", "Both copies of the GPT are individually valid but do not describe the same partitions. The primary table is used. An interrupted partitioning change is often the cause."),
                ));
            }
            (p, true, true)
        }
        (Ok(p), Err(e)) => {
            findings.push(Finding::warning(
                "GPT_BACKUP_INVALID",
                t!("Table GPT de secours invalide", "Invalid backup GPT"),
                tf!(
                    "La table GPT principale est valide mais la copie de secours ne l'est pas ({e}). \
                     Le clone recevra deux exemplaires valides.",
                    "The primary GPT is valid but its backup copy is not ({e}). The clone will receive two valid copies."
                ),
            ));
            (p, true, false)
        }
        (Err(e), Ok(b)) => {
            findings.push(Finding::error(
                "GPT_PRIMARY_CORRUPT",
                t!("Table GPT principale corrompue", "Corrupted primary GPT"),
                tf!("L'en-tête GPT principal est invalide ({e}). La table de secours, valide, est utilisée à la place. \
                     C'est le signe d'un disque endommagé ou d'une écriture accidentelle en début de disque.", "The primary GPT header is invalid ({e}). The valid backup table is used instead. This is the sign of a damaged disk or of an accidental write at the start of the disk."
                ),
            ));
            (b, false, true)
        }
        (Err(ep), Err(eb)) => {
            findings.push(Finding::error(
                "GPT_BOTH_INVALID",
                t!("Tables GPT invalides", "Invalid GPT tables"),
                tf!("En-tête principal : {ep}. En-tête de secours : {eb}.", "Primary header: {ep}. Backup header: {eb}."),
            ));
            return Ok(GptRead { valid: false, partitions: vec![], meta: None, findings });
        }
    };
    if backup_valid && !backup_at_end {
        findings.push(Finding::warning(
            "GPT_BACKUP_NOT_AT_END",
            t!("Table GPT de secours mal placée", "Misplaced backup GPT"),
            tf!("L'en-tête GPT de secours se trouve au LBA {backup_lba} et non sur le dernier secteur du disque (LBA {last}). \
                 C'est typique d'une copie brute vers un disque plus grand. Le clone recevra une table correctement placée.", "The backup GPT header sits at LBA {backup_lba}, not on the last sector of the disk (LBA {last}). This is typical of a raw copy to a larger disk. The clone will receive a correctly placed table."
            ),
        ));
    }

    let h = &chosen.header;
    let mut partitions = Vec::new();
    let es = h.entry_size as usize;
    let mut seen_guids: Vec<Guid> = Vec::new();
    for slot in 0..h.num_entries as usize {
        let e = &chosen.entries[slot * es..(slot + 1) * es];
        let type_guid = Guid::from_bytes(&e[0..16]);
        if type_guid.is_zero() {
            continue;
        }
        let unique_guid = Guid::from_bytes(&e[16..32]);
        let first = le64(e, 32);
        let last_lba = le64(e, 40);
        let number = slot as u32 + 1;
        let sectors = if last_lba >= first { last_lba - first + 1 } else { 0 };
        if seen_guids.contains(&unique_guid) {
            findings.push(
                Finding::warning(
                    "GPT_DUPLICATE_GUID",
                    t!("Identifiant de partition en double", "Duplicate partition identifier"),
                    tf!(
                        "La partition {number} porte le même GUID unique ({unique_guid}) qu'une autre partition.",
                        "Partition {number} has the same unique GUID ({unique_guid}) as another partition."
                    ),
                )
                .scope(format!("partition {number}")),
            );
        }
        seen_guids.push(unique_guid);
        if sectors > 0 && (first < h.first_usable || last_lba > h.last_usable) {
            findings.push(
                Finding::warning(
                    "GPT_PART_OUTSIDE_USABLE",
                    t!("Partition hors de la zone utilisable", "Partition outside the usable area"),
                    tf!(
                        "La partition {number} (LBA {first}–{last_lba}) sort de la plage utilisable annoncée par la GPT ({}–{}).",
                        "Partition {number} (LBA {first}–{last_lba}) lies outside the usable range announced by the GPT ({}–{}).",
                        h.first_usable,
                        h.last_usable
                    ),
                )
                .scope(format!("partition {number}")),
            );
        }
        partitions.push(Partition {
            number,
            start_lba: first,
            sectors,
            info: PartInfo::Gpt { type_guid, unique_guid, attributes: le64(e, 48), name: utf16_name(&e[56..128.min(es)]), slot: slot as u32 },
        });
    }

    let meta = GptMeta {
        disk_guid: h.disk_guid,
        num_entries: h.num_entries,
        entry_size: h.entry_size,
        first_usable_lba: h.first_usable,
        last_usable_lba: h.last_usable,
        primary_valid,
        backup_valid,
        backup_at_end,
    };
    Ok(GptRead { valid: true, partitions, meta: Some(meta), findings })
}

// ---------------------------------------------------------------------------
// Construction
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GptEntrySpec {
    pub slot: u32,
    pub type_guid: Guid,
    pub unique_guid: Guid,
    pub first_lba: u64,
    /// Dernier LBA, inclus.
    pub last_lba: u64,
    pub attributes: u64,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct GptSpec {
    pub sector_size: u32,
    pub disk_sectors: u64,
    pub disk_guid: Guid,
    pub num_entries: u32,
    pub entry_size: u32,
    pub entries: Vec<GptEntrySpec>,
}

/// Sectors to write to install a complete GPT.
#[derive(Debug, Clone)]
pub struct GptImage {
    /// Primary header (LBA 1) followed by the entry array.
    pub primary: Vec<u8>,
    pub primary_lba: u64,
    /// Backup entry array followed by the backup header (last LBA).
    pub backup: Vec<u8>,
    pub backup_lba: u64,
    pub first_usable_lba: u64,
    pub last_usable_lba: u64,
}

/// (first usable LBA, last usable LBA, sectors of the entry array).
pub fn geometry(sector_size: u32, disk_sectors: u64, num_entries: u32, entry_size: u32) -> Option<(u64, u64, u64)> {
    let ss = sector_size as u64;
    let entries_sectors = (num_entries as u64 * entry_size as u64).div_ceil(ss);
    let first_usable = 2 + entries_sectors;
    // End of disk: [backup array][backup header].
    let last_usable = disk_sectors.checked_sub(entries_sectors + 2)?;
    if last_usable < first_usable {
        return None;
    }
    Some((first_usable, last_usable, entries_sectors))
}

pub fn build_gpt(spec: &GptSpec) -> Result<GptImage, String> {
    let ss = spec.sector_size as usize;
    if spec.entry_size < 128 || !spec.entry_size.is_multiple_of(8) {
        return Err(t!("taille d'entrée GPT invalide", "invalid GPT entry size").into());
    }
    let (first_usable, last_usable, entries_sectors) = geometry(spec.sector_size, spec.disk_sectors, spec.num_entries, spec.entry_size)
        .ok_or(t!("disque trop petit pour une table GPT", "disk too small for a GPT"))?;

    let es = spec.entry_size as usize;
    let mut array = vec![0u8; entries_sectors as usize * ss];
    let mut ranges: Vec<(u64, u64, u32)> = Vec::new();
    for e in &spec.entries {
        if e.slot >= spec.num_entries {
            return Err(tf!("entrée {} hors du tableau ({} entrées)", "entry {} outside the array ({} entries)", e.slot + 1, spec.num_entries));
        }
        if e.type_guid.is_zero() {
            return Err(tf!("entrée {} : type de partition nul", "entry {}: null partition type", e.slot + 1));
        }
        if e.first_lba > e.last_lba || e.first_lba < first_usable || e.last_lba > last_usable {
            return Err(tf!(
                "entrée {} : LBA {}–{} hors de la zone utilisable {}–{}",
                "entry {}: LBA {}–{} outside the usable area {}–{}",
                e.slot + 1,
                e.first_lba,
                e.last_lba,
                first_usable,
                last_usable
            ));
        }
        if ranges.iter().any(|(_, _, s)| *s == e.slot) {
            return Err(tf!("entrée {} définie deux fois", "entry {} defined twice", e.slot + 1));
        }
        if let Some((a, b, s)) = ranges.iter().find(|(a, b, _)| e.first_lba <= *b && *a <= e.last_lba) {
            return Err(tf!("les entrées {} et {} se chevauchent (LBA {a}–{b})", "entries {} and {} overlap (LBA {a}–{b})", s + 1, e.slot + 1));
        }
        ranges.push((e.first_lba, e.last_lba, e.slot));

        let o = e.slot as usize * es;
        let rec = &mut array[o..o + es];
        rec[0..16].copy_from_slice(&e.type_guid.0);
        rec[16..32].copy_from_slice(&e.unique_guid.0);
        rec[32..40].copy_from_slice(&e.first_lba.to_le_bytes());
        rec[40..48].copy_from_slice(&e.last_lba.to_le_bytes());
        rec[48..56].copy_from_slice(&e.attributes.to_le_bytes());
        let units: Vec<u16> = e.name.encode_utf16().collect();
        if units.len() > 36 {
            return Err(tf!("entrée {} : nom trop long", "entry {}: name too long", e.slot + 1));
        }
        for (i, u) in units.iter().enumerate() {
            rec[56 + i * 2..58 + i * 2].copy_from_slice(&u.to_le_bytes());
        }
    }
    let entries_bytes = spec.num_entries as usize * es;
    let entries_crc = crc32(&array[..entries_bytes]);

    let backup_header_lba = spec.disk_sectors - 1;
    let backup_entries_lba = backup_header_lba - entries_sectors;
    let header = |my_lba: u64, alternate: u64, entries_lba: u64| -> Vec<u8> {
        let mut h = vec![0u8; ss];
        h[0..8].copy_from_slice(SIGNATURE);
        h[8..12].copy_from_slice(&REVISION_1_0.to_le_bytes());
        h[12..16].copy_from_slice(&HEADER_SIZE.to_le_bytes());
        h[24..32].copy_from_slice(&my_lba.to_le_bytes());
        h[32..40].copy_from_slice(&alternate.to_le_bytes());
        h[40..48].copy_from_slice(&first_usable.to_le_bytes());
        h[48..56].copy_from_slice(&last_usable.to_le_bytes());
        h[56..72].copy_from_slice(&spec.disk_guid.0);
        h[72..80].copy_from_slice(&entries_lba.to_le_bytes());
        h[80..84].copy_from_slice(&spec.num_entries.to_le_bytes());
        h[84..88].copy_from_slice(&spec.entry_size.to_le_bytes());
        h[88..92].copy_from_slice(&entries_crc.to_le_bytes());
        let crc = crc32(&h[..HEADER_SIZE as usize]);
        h[16..20].copy_from_slice(&crc.to_le_bytes());
        h
    };

    let mut primary = header(1, backup_header_lba, 2);
    primary.extend_from_slice(&array);
    let mut backup = array;
    backup.extend_from_slice(&header(backup_header_lba, 1, backup_entries_lba));

    Ok(GptImage { primary, primary_lba: 1, backup, backup_lba: backup_entries_lba, first_usable_lba: first_usable, last_usable_lba: last_usable })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::MemDevice;
    use crate::partition::{GPT_BASIC_DATA, GPT_ESP, mbr};

    fn sample_spec(sector: u32, disk_sectors: u64) -> GptSpec {
        GptSpec {
            sector_size: sector,
            disk_sectors,
            disk_guid: Guid::must("11111111-2222-3333-4444-555555555555"),
            num_entries: 128,
            entry_size: 128,
            entries: vec![
                GptEntrySpec {
                    slot: 0,
                    type_guid: Guid::must(GPT_ESP),
                    unique_guid: Guid::must("AAAAAAAA-0000-0000-0000-000000000001"),
                    first_lba: 2048,
                    last_lba: 4095,
                    attributes: 0x8000_0000_0000_0001,
                    name: "EFI system partition".into(),
                },
                GptEntrySpec {
                    slot: 3,
                    type_guid: Guid::must(GPT_BASIC_DATA),
                    unique_guid: Guid::must("AAAAAAAA-0000-0000-0000-000000000004"),
                    first_lba: 4096,
                    last_lba: 20479,
                    attributes: 0,
                    name: "Données é".into(),
                },
            ],
        }
    }

    fn install(dev: &mut MemDevice, img: &GptImage) {
        let ss = dev.sector_size() as u64;
        let disk_sectors = dev.size() / ss;
        let pm = mbr::build_protective(None, ss as u32, disk_sectors);
        dev.write_at(0, &pm).unwrap();
        dev.write_at(img.primary_lba * ss, &img.primary).unwrap();
        dev.write_at(img.backup_lba * ss, &img.backup).unwrap();
    }

    #[test]
    fn build_then_read_512_and_4096() {
        for sector in [512u32, 4096] {
            let disk_sectors = (32u64 << 20) / sector as u64;
            let mut spec = sample_spec(sector, disk_sectors);
            if sector == 4096 {
                spec.entries[0].first_lba = 256;
                spec.entries[0].last_lba = 511;
                spec.entries[1].first_lba = 512;
                spec.entries[1].last_lba = 2047;
            }
            let img = build_gpt(&spec).unwrap();
            let mut dev = MemDevice::new(32 << 20, sector);
            install(&mut dev, &img);
            let r = read_gpt(&mut dev).unwrap();
            assert!(r.valid);
            assert!(r.findings.is_empty(), "{:?}", r.findings);
            let meta = r.meta.unwrap();
            assert_eq!(meta.disk_guid, spec.disk_guid);
            assert!(meta.primary_valid && meta.backup_valid && meta.backup_at_end);
            assert_eq!(r.partitions.len(), 2);
            assert_eq!(r.partitions[1].number, 4);
            assert_eq!(r.partitions[1].name(), Some("Données é"));
            match &r.partitions[0].info {
                PartInfo::Gpt { attributes, unique_guid, .. } => {
                    assert_eq!(*attributes, 0x8000_0000_0000_0001);
                    assert_eq!(unique_guid.to_string(), "AAAAAAAA-0000-0000-0000-000000000001");
                }
                _ => panic!(),
            }
        }
    }

    #[test]
    fn independent_parser_agrees() {
        // Cross-check with an independent implementation (gptman crate).
        let spec = sample_spec(512, 65536);
        let img = build_gpt(&spec).unwrap();
        let mut dev = MemDevice::new(32 << 20, 512);
        install(&mut dev, &img);
        let mut cur = std::io::Cursor::new(dev.data.clone());
        let g = gptman::GPT::read_from(&mut cur, 512).expect("gptman must accept our table");
        assert_eq!(g.header.disk_guid, spec.disk_guid.0);
        assert_eq!(g[1].starting_lba, 2048);
        assert_eq!(g[1].ending_lba, 4095);
        assert_eq!(g[4].starting_lba, 4096);
        assert_eq!(g[4].unique_partition_guid, spec.entries[1].unique_guid.0);
        assert_eq!(g.header.first_usable_lba, img.first_usable_lba);
        assert_eq!(g.header.last_usable_lba, img.last_usable_lba);
        // And the backup header alone must be enough for gptman.
        let mut broken = dev.data.clone();
        broken[512..1024].fill(0);
        let mut cur = std::io::Cursor::new(broken);
        let g2 = gptman::GPT::find_from(&mut cur).expect("table de secours lisible par gptman");
        assert_eq!(g2[4].ending_lba, 20479);
    }

    #[test]
    fn corrupt_primary_falls_back_to_backup() {
        let spec = sample_spec(512, 65536);
        let img = build_gpt(&spec).unwrap();
        let mut dev = MemDevice::new(32 << 20, 512);
        install(&mut dev, &img);
        dev.data[512 + 60] ^= 0xFF; // in the disk GUID → wrong CRC
        let r = read_gpt(&mut dev).unwrap();
        assert!(r.valid);
        assert!(r.findings.iter().any(|f| f.code == "GPT_PRIMARY_CORRUPT"));
        assert_eq!(r.partitions.len(), 2);
        assert!(!r.meta.unwrap().primary_valid);
    }

    #[test]
    fn corrupt_entries_and_backup_are_reported() {
        let spec = sample_spec(512, 65536);
        let img = build_gpt(&spec).unwrap();
        let mut dev = MemDevice::new(32 << 20, 512);
        install(&mut dev, &img);
        // Corrupt the backup array.
        let o = img.backup_lba as usize * 512 + 40;
        dev.data[o] ^= 1;
        let r = read_gpt(&mut dev).unwrap();
        assert!(r.valid);
        assert!(r.findings.iter().any(|f| f.code == "GPT_BACKUP_INVALID"));
        // Corrupt the primary array too: nothing reliable left.
        dev.data[2 * 512 + 40] ^= 1;
        let r = read_gpt(&mut dev).unwrap();
        assert!(!r.valid);
        assert!(r.findings.iter().any(|f| f.code == "GPT_BOTH_INVALID"));
    }

    #[test]
    fn backup_not_at_end_after_raw_copy_to_larger_disk() {
        let spec = sample_spec(512, 65536);
        let img = build_gpt(&spec).unwrap();
        let mut small = MemDevice::new(32 << 20, 512);
        install(&mut small, &img);
        let mut big = MemDevice::new(48 << 20, 512);
        big.data[..32 << 20].copy_from_slice(&small.data);
        let r = read_gpt(&mut big).unwrap();
        assert!(r.valid);
        assert!(r.findings.iter().any(|f| f.code == "GPT_BACKUP_NOT_AT_END"));
        assert!(!r.meta.unwrap().backup_at_end);
    }

    #[test]
    fn build_rejects_overlap_and_out_of_range() {
        let mut spec = sample_spec(512, 65536);
        spec.entries[1].first_lba = 4000;
        assert!(build_gpt(&spec).unwrap_err().contains("chevauchent"));
        let mut spec = sample_spec(512, 65536);
        spec.entries[1].last_lba = 65535;
        assert!(build_gpt(&spec).is_err());
        let mut spec = sample_spec(512, 65536);
        spec.entries[0].first_lba = 10;
        assert!(build_gpt(&spec).is_err());
    }

    #[test]
    fn absent_gpt_is_silent() {
        let mut dev = MemDevice::new(8 << 20, 512);
        let r = read_gpt(&mut dev).unwrap();
        assert!(!r.valid && r.findings.is_empty());
    }
}
