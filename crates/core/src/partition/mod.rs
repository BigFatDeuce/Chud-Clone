//! Partition tables: reading, consistency checks and writing (GPT and MBR).

pub mod gpt;
pub mod mbr;

use crate::device::BlockDevice;
use crate::diag::Finding;
use crate::guid::Guid;
use serde::{Deserialize, Serialize};

pub const MIB: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TableKind {
    Gpt,
    Mbr,
    /// No recognised table (blank disk, "superfloppy", destroyed table).
    None,
}

impl TableKind {
    pub fn label(self) -> &'static str {
        match self {
            TableKind::Gpt => "GPT",
            TableKind::Mbr => "MBR",
            TableKind::None => t!("Aucune table", "No table"),
        }
    }
}

/// Functional role of a partition, deduced from its type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PartRole {
    /// EFI system partition (ESP).
    Esp,
    /// Microsoft reserved partition (MSR).
    Msr,
    /// Windows recovery environment.
    Recovery,
    /// Basic data (NTFS, FAT, exFAT…: Windows, data).
    BasicData,
    /// Windows dynamic disk (LDM) or Storage Spaces.
    Dynamic,
    LinuxFs,
    LinuxSwap,
    LinuxLvm,
    BiosBoot,
    Apple,
    /// Container of logical partitions (MBR extended).
    Extended,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scheme", rename_all = "snake_case")]
pub enum PartInfo {
    Gpt {
        type_guid: Guid,
        unique_guid: Guid,
        attributes: u64,
        name: String,
        /// Index of the entry in the GPT array (0-based).
        slot: u32,
    },
    Mbr {
        type_id: u8,
        active: bool,
        logical: bool,
        /// Index of the primary entry (0–3); for a logical one, rank in the chain.
        slot: u32,
        /// LBA of the EBR describing this logical partition.
        ebr_lba: Option<u64>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Partition {
    /// Displayed number (1-based), in table order.
    pub number: u32,
    pub start_lba: u64,
    pub sectors: u64,
    pub info: PartInfo,
}

impl Partition {
    pub fn end_lba(&self) -> u64 {
        self.start_lba + self.sectors
    }

    pub fn role(&self) -> PartRole {
        match &self.info {
            PartInfo::Gpt { type_guid, .. } => gpt_role(type_guid),
            PartInfo::Mbr { type_id, .. } => mbr_role(*type_id),
        }
    }

    pub fn type_label(&self) -> String {
        match &self.info {
            PartInfo::Gpt { type_guid, .. } => gpt_type_label(type_guid),
            PartInfo::Mbr { type_id, .. } => mbr_type_label(*type_id),
        }
    }

    pub fn name(&self) -> Option<&str> {
        match &self.info {
            PartInfo::Gpt { name, .. } if !name.is_empty() => Some(name),
            _ => None,
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(&self.info, PartInfo::Mbr { active: true, .. })
    }

    pub fn is_logical(&self) -> bool {
        matches!(&self.info, PartInfo::Mbr { logical: true, .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GptMeta {
    pub disk_guid: Guid,
    pub num_entries: u32,
    pub entry_size: u32,
    pub first_usable_lba: u64,
    pub last_usable_lba: u64,
    pub primary_valid: bool,
    pub backup_valid: bool,
    /// True if the backup header is indeed on the last sector of the disk.
    pub backup_at_end: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MbrMeta {
    pub signature: u32,
    /// True if the boot code area (first 440 bytes) is not empty.
    pub boot_code_present: bool,
    /// Extended partition (starting LBA, number of sectors), if present.
    pub extended: Option<(u64, u64)>,
    /// LBA of each EBR in the chain of logical partitions.
    pub ebr_lbas: Vec<u64>,
    /// Raw copy of sector 0 (boot code + table).
    #[serde(skip)]
    pub sector0: Vec<u8>,
}

/// Complete description of a disk's partitioning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskLayout {
    pub kind: TableKind,
    pub sector_size: u32,
    pub disk_sectors: u64,
    /// Useful partitions (excluding the extended container), sorted by starting LBA.
    pub partitions: Vec<Partition>,
    pub gpt: Option<GptMeta>,
    pub mbr: Option<MbrMeta>,
    pub findings: Vec<Finding>,
}

impl DiskLayout {
    pub fn disk_bytes(&self) -> u64 {
        self.disk_sectors * self.sector_size as u64
    }

    /// End (exclusive, in LBA) of the last partition, or 0.
    pub fn last_partition_end_lba(&self) -> u64 {
        self.partitions.iter().map(|p| p.end_lba()).max().unwrap_or(0)
    }

    pub fn partition(&self, number: u32) -> Option<&Partition> {
        self.partitions.iter().find(|p| p.number == number)
    }

    pub fn has_dynamic(&self) -> bool {
        self.partitions.iter().any(|p| p.role() == PartRole::Dynamic)
    }
}

/// Reads and checks a device's partition table.
///
/// Only returns an error if the device is unreadable; a missing or corrupt table
/// gives a `DiskLayout` with `kind == None` and the corresponding findings.
pub fn read_layout(dev: &mut dyn BlockDevice) -> std::io::Result<DiskLayout> {
    let sector_size = dev.sector_size();
    let disk_sectors = dev.size() / sector_size as u64;
    let mut layout =
        DiskLayout { kind: TableKind::None, sector_size, disk_sectors, partitions: Vec::new(), gpt: None, mbr: None, findings: Vec::new() };
    if disk_sectors < 64 {
        layout.findings.push(Finding::error(
            "DISK_TOO_SMALL",
            t!("Disque trop petit", "Disk too small"),
            t!(
                "Le périphérique fait moins de 64 secteurs : aucune table de partitions possible.",
                "The device has fewer than 64 sectors: no partition table is possible."
            ),
        ));
        return Ok(layout);
    }

    let mbr = mbr::read_mbr(dev)?;
    let gpt = gpt::read_gpt(dev)?;

    match (gpt.valid, &mbr.kind) {
        (true, _) => {
            layout.kind = TableKind::Gpt;
            layout.partitions = gpt.partitions;
            layout.gpt = gpt.meta;
            layout.findings.extend(gpt.findings);
            match mbr.kind {
                mbr::MbrKind::Protective => {}
                mbr::MbrKind::Hybrid => layout.findings.push(Finding::warning(
                    "GPT_HYBRID_MBR",
                    t!("MBR hybride détecté", "Hybrid MBR detected"),
                    t!("Le secteur 0 contient des partitions MBR en plus de l'entrée de protection GPT. \
                     Le clone recevra un MBR de protection standard : un système qui démarrait via le MBR hybride \
                     (certains anciens Mac/Boot Camp) pourrait ne plus démarrer.", "Sector 0 contains MBR partitions in addition to the GPT protective entry. The clone will receive a standard protective MBR: a system that booted through the hybrid MBR (some older Macs / Boot Camp) might no longer boot."),
                )),
                _ => layout.findings.push(Finding::warning(
                    "GPT_NO_PROTECTIVE_MBR",
                    t!("MBR de protection absent", "Protective MBR missing"),
                    t!("Une table GPT valide est présente mais le secteur 0 ne contient pas de MBR de protection. \
                     Le clone recevra un MBR de protection standard.", "A valid GPT is present but sector 0 holds no protective MBR. The clone will receive a standard protective MBR."),
                )),
            }
            // Sector 0 is kept to reuse its boot code.
            layout.mbr = mbr.meta.map(|mut m| {
                m.extended = None;
                m.ebr_lbas.clear();
                m
            });
        }
        (false, mbr::MbrKind::Classic) => {
            layout.kind = TableKind::Mbr;
            layout.partitions = mbr.partitions;
            layout.mbr = mbr.meta;
            layout.findings.extend(mbr.findings);
        }
        (false, mbr::MbrKind::Protective | mbr::MbrKind::Hybrid) => {
            layout.findings.extend(gpt.findings);
            layout.findings.push(
                Finding::error(
                    "GPT_UNREADABLE",
                    t!("Table GPT illisible", "Unreadable GPT"),
                    t!("Le disque porte un MBR de protection GPT, mais ni l'en-tête GPT principal ni l'en-tête de secours \
                     ne sont valides. Les partitions ne peuvent pas être identifiées de façon fiable.", "The disk carries a GPT protective MBR, but neither the primary nor the backup GPT header is valid. The partitions cannot be identified reliably."),
                )
                .scope(t!("table de partitions", "partition table")),
            );
        }
        (false, mbr::MbrKind::Superfloppy) => {
            layout.findings.push(Finding::info(
                "NO_TABLE_SUPERFLOPPY",
                t!("Pas de table de partitions", "No partition table"),
                t!(
                    "Le disque contient directement un système de fichiers, sans table de partitions.",
                    "The disk holds a file system directly, without a partition table."
                ),
            ));
        }
        (false, mbr::MbrKind::Empty) => {
            layout.findings.extend(gpt.findings.into_iter().filter(|f| f.code != "GPT_ABSENT"));
            layout.findings.push(Finding::info(
                "NO_TABLE",
                t!("Aucune table de partitions", "No partition table"),
                t!(
                    "Aucune table GPT ou MBR valide n'a été trouvée (disque vierge ou non initialisé).",
                    "No valid GPT or MBR was found (blank or uninitialised disk)."
                ),
            ));
        }
    }

    layout.partitions.sort_by_key(|p| p.start_lba);
    check_geometry(&mut layout);
    Ok(layout)
}

/// Common checks: partitions outside the disk, overlaps, zero sizes.
fn check_geometry(layout: &mut DiskLayout) {
    let mut findings = Vec::new();
    for p in &layout.partitions {
        if p.sectors == 0 {
            findings.push(
                Finding::error(
                    "PART_EMPTY",
                    t!("Partition de taille nulle", "Zero-size partition"),
                    tf!("La partition {} a une taille nulle.", "Partition {} has a size of zero.", p.number),
                )
                .scope(format!("partition {}", p.number))
                .blocking(),
            );
        }
        if p.end_lba() > layout.disk_sectors {
            findings.push(
                Finding::error(
                    "PART_BEYOND_DISK",
                    t!("Partition dépassant la fin du disque", "Partition past the end of the disk"),
                    tf!("La partition {} se termine au LBA {} alors que le disque ne compte que {} secteurs. \
                         La table de partitions est incohérente (disque tronqué ou table endommagée).", "Partition {} ends at LBA {} while the disk only has {} sectors. The partition table is inconsistent (truncated disk or damaged table).",
                        p.number,
                        p.end_lba(),
                        layout.disk_sectors
                    ),
                )
                .scope(format!("partition {}", p.number))
                .at_lba(p.start_lba)
                .blocking(),
            );
        }
    }
    for pair in layout.partitions.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        if a.end_lba() > b.start_lba {
            findings.push(
                Finding::error(
                    "PART_OVERLAP",
                    t!("Partitions qui se chevauchent", "Overlapping partitions"),
                    tf!(
                        "Les partitions {} (LBA {}–{}) et {} (LBA {}–{}) se chevauchent. \
                         Un clonage par partition serait dangereux.",
                        "Partitions {} (LBA {}–{}) and {} (LBA {}–{}) overlap. Cloning partition by partition would be dangerous.",
                        a.number,
                        a.start_lba,
                        a.end_lba() - 1,
                        b.number,
                        b.start_lba,
                        b.end_lba().saturating_sub(1)
                    ),
                )
                .at_lba(b.start_lba)
                .blocking(),
            );
        }
    }
    if layout.has_dynamic() {
        findings.push(
            Finding::error(
                "DYNAMIC_DISK",
                t!("Disque dynamique ou espace de stockage", "Dynamic disk or Storage Space"),
                t!("Ce disque utilise les disques dynamiques Windows (LDM) ou les espaces de stockage. \
                 Leur structure s'étend sur plusieurs partitions voire plusieurs disques : le clonage intelligent \
                 n'est pas pris en charge. Seule une copie brute secteur par secteur du disque entier est possible, \
                 sans garantie que Windows accepte le clone.", "This disk uses Windows dynamic disks (LDM) or Storage Spaces. Their structure spans several partitions or even several disks: smart cloning is not supported. Only a raw sector-by-sector copy of the whole disk is possible, with no guarantee that Windows will accept the clone."),
            )
            .blocking(),
        );
    }
    layout.findings.extend(findings);
}

// ---------------------------------------------------------------------------
// Types GPT connus
// ---------------------------------------------------------------------------

pub const GPT_ESP: &str = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";
pub const GPT_MSR: &str = "E3C9E316-0B5C-4DB8-817D-F92DF00215AE";
pub const GPT_BASIC_DATA: &str = "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7";
pub const GPT_RECOVERY: &str = "DE94BBA4-06D1-4D40-A16A-BFD50179D6AC";
pub const GPT_LDM_META: &str = "5808C8AA-7E8F-42E0-85D2-E1E90434CFB3";
pub const GPT_LDM_DATA: &str = "AF9B60A0-1431-4F62-BC68-3311714A69AD";
pub const GPT_STORAGE_SPACES: &str = "E75CAF8F-F680-4CEE-AFA3-B001E56EFC2D";
pub const GPT_LINUX_FS: &str = "0FC63DAF-8483-4772-8E79-3D69D8477DE4";
pub const GPT_LINUX_SWAP: &str = "0657FD6D-A4AB-43C4-84E5-0933C84B4F4F";
pub const GPT_LINUX_LVM: &str = "E6D6D379-F507-44C2-A23C-238F2A3DF928";
pub const GPT_BIOS_BOOT: &str = "21686148-6449-6E6F-744E-656564454649";
pub const GPT_APPLE_APFS: &str = "7C3457EF-0000-11AA-AA11-00306543ECAC";
pub const GPT_APPLE_HFS: &str = "48465300-0000-11AA-AA11-00306543ECAC";

fn gpt_known(g: &Guid) -> Option<(PartRole, &'static str)> {
    let table: [(&str, PartRole, &str); 13] = [
        (GPT_ESP, PartRole::Esp, t!("Partition système EFI", "EFI system partition")),
        (GPT_MSR, PartRole::Msr, t!("Réservée Microsoft (MSR)", "Microsoft reserved (MSR)")),
        (GPT_BASIC_DATA, PartRole::BasicData, t!("Données de base", "Basic data")),
        (GPT_RECOVERY, PartRole::Recovery, t!("Récupération Windows", "Windows recovery")),
        (GPT_LDM_META, PartRole::Dynamic, t!("Métadonnées disque dynamique (LDM)", "Dynamic disk metadata (LDM)")),
        (GPT_LDM_DATA, PartRole::Dynamic, t!("Données disque dynamique (LDM)", "Dynamic disk data (LDM)")),
        (GPT_STORAGE_SPACES, PartRole::Dynamic, t!("Espaces de stockage", "Storage Spaces")),
        (GPT_LINUX_FS, PartRole::LinuxFs, t!("Système de fichiers Linux", "Linux file system")),
        (GPT_LINUX_SWAP, PartRole::LinuxSwap, t!("Swap Linux", "Linux swap")),
        (GPT_LINUX_LVM, PartRole::LinuxLvm, t!("LVM Linux", "Linux LVM")),
        (GPT_BIOS_BOOT, PartRole::BiosBoot, t!("Amorçage BIOS (GRUB)", "BIOS boot (GRUB)")),
        (GPT_APPLE_APFS, PartRole::Apple, "Apple APFS"),
        (GPT_APPLE_HFS, PartRole::Apple, "Apple HFS+"),
    ];
    table.iter().find(|(s, _, _)| Guid::must(s) == *g).map(|(_, r, l)| (*r, *l))
}

pub fn gpt_role(g: &Guid) -> PartRole {
    gpt_known(g).map(|(r, _)| r).unwrap_or(PartRole::Other)
}

pub fn gpt_type_label(g: &Guid) -> String {
    gpt_known(g).map(|(_, l)| l.to_string()).unwrap_or_else(|| format!("Type {g}"))
}

pub fn mbr_role(t: u8) -> PartRole {
    match t {
        0x05 | 0x0F | 0x85 => PartRole::Extended,
        0x27 => PartRole::Recovery,
        0x42 => PartRole::Dynamic,
        0x82 => PartRole::LinuxSwap,
        0x83 => PartRole::LinuxFs,
        0x8E => PartRole::LinuxLvm,
        0xEF => PartRole::Esp,
        0xAF | 0xAB => PartRole::Apple,
        0x01 | 0x04 | 0x06 | 0x07 | 0x0B | 0x0C | 0x0E | 0x11 | 0x14 | 0x16 | 0x17 | 0x1B | 0x1C | 0x1E => PartRole::BasicData,
        _ => PartRole::Other,
    }
}

pub fn mbr_type_label(t: u8) -> String {
    let s = match t {
        0x01 => "FAT12",
        0x04 | 0x06 | 0x0E => "FAT16",
        0x07 => "NTFS / exFAT",
        0x0B | 0x0C => "FAT32",
        0x05 | 0x0F | 0x85 => t!("Étendue", "Extended"),
        0x11 | 0x14 | 0x16 | 0x1B | 0x1C | 0x1E => t!("FAT masquée", "Hidden FAT"),
        0x17 => t!("NTFS masquée", "Hidden NTFS"),
        0x12 => t!("Diagnostic constructeur", "Vendor diagnostics"),
        0x27 => t!("Récupération Windows", "Windows recovery"),
        0x42 => t!("Disque dynamique (LDM)", "Dynamic disk (LDM)"),
        0x82 => t!("Swap Linux", "Linux swap"),
        0x83 => "Linux",
        0x8E => t!("LVM Linux", "Linux LVM"),
        0xDE => t!("Utilitaire Dell", "Dell utility"),
        0xEE => t!("Protection GPT", "GPT protective"),
        0xEF => t!("Système EFI", "EFI system"),
        _ => return format!("Type 0x{t:02X}"),
    };
    format!("{s} (0x{t:02X})")
}

pub(crate) fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
pub(crate) fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
pub(crate) fn le64(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(a)
}
