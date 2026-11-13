//! Description of a disk as presented to the user and to the pre-checks.

use crate::partition::{PartRole, TableKind};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskKind {
    /// Disk seen by Windows (`\\.\PhysicalDriveN`), real or virtual (mounted VHD).
    Physical,
    /// Fichier image.
    Image,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Ssd,
    Hdd,
    /// Disque virtuel (VHD/VHDX, machine virtuelle).
    Virtual,
    /// Fichier image.
    Image,
    Unknown,
}

impl MediaKind {
    pub fn label(self) -> &'static str {
        match self {
            MediaKind::Ssd => "SSD",
            MediaKind::Hdd => t!("Disque dur", "Hard disk"),
            MediaKind::Virtual => t!("Disque virtuel", "Virtual disk"),
            MediaKind::Image => t!("Fichier image", "Image file"),
            MediaKind::Unknown => t!("Type inconnu", "Unknown type"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeInfo {
    /// Drive letters or mount points (`C:\`).
    pub mounts: Vec<String>,
    pub label: Option<String>,
    pub fs: Option<String>,
    pub total_bytes: Option<u64>,
    pub free_bytes: Option<u64>,
    /// Nom de volume Windows (`\\?\Volume{…}\`).
    pub guid_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartitionInfo {
    pub number: u32,
    pub offset: u64,
    pub size: u64,
    pub role: PartRole,
    pub type_label: String,
    pub name: Option<String>,
    /// File system identified (by Windows or by direct reading).
    pub fs: Option<String>,
    pub label: Option<String>,
    pub used_bytes: Option<u64>,
    pub mounts: Vec<String>,
    pub active: bool,
    /// Windows volume name (`\?\Volume{…}\`), if a volume matches.
    #[serde(default)]
    pub volume_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskInfo {
    /// Internal identifier, stable during the session (`disk:2`, `image:<path>`).
    pub id: String,
    pub kind: DiskKind,
    /// Path used to open it (`\\.\PhysicalDrive2` or a file path).
    pub path: String,
    pub number: Option<u32>,
    pub model: String,
    pub vendor: String,
    pub serial: String,
    pub firmware: String,
    /// Interface : NVMe, SATA, USB, SAS…
    pub bus: String,
    pub media: MediaKind,
    pub size: u64,
    pub sector_size: u32,
    pub physical_sector_size: u32,
    pub removable: bool,
    pub read_only: bool,
    pub offline: bool,
    /// Holds the running Windows, its boot files or its page file.
    pub is_system: bool,
    /// Holds the program itself or its data folder.
    pub hosts_app: bool,
    pub table: TableKind,
    pub partitions: Vec<PartitionInfo>,
    pub used_bytes: Option<u64>,
    /// Identity fingerprint: used to check, just before writing, that the disk
    /// opened is indeed the one the user chose.
    pub fingerprint: String,
    /// True if the description comes from reading the disk directly (administrator
    /// rights), false if it is limited to what Windows exposes without privilege.
    pub deep: bool,
    /// Notes about detection (access denied, partial information…).
    pub notes: Vec<String>,
    /// SCSI address of the disk: port (storage adapter), path, target, LUN. The bays of
    /// a multi-disk USB dock share the port and differ by target or LUN.
    #[serde(default)]
    pub scsi_address: Option<[u8; 4]>,
}

impl DiskInfo {
    /// Nom court affichable.
    pub fn title(&self) -> String {
        let model = if self.model.trim().is_empty() { t!("Disque sans nom", "Unnamed disk") } else { self.model.trim() };
        match (self.kind, self.number) {
            (DiskKind::Physical, Some(n)) => tf!("Disque {n} — {model}", "Disk {n} — {model}"),
            _ => model.to_string(),
        }
    }

    pub fn has_mounted_volumes(&self) -> bool {
        self.partitions.iter().any(|p| !p.mounts.is_empty())
    }

    pub fn mounts(&self) -> Vec<String> {
        self.partitions.iter().flat_map(|p| p.mounts.iter().cloned()).collect()
    }

    pub fn is_virtual(&self) -> bool {
        matches!(self.media, MediaKind::Virtual | MediaKind::Image)
    }

    #[cfg(test)]
    pub fn test_default() -> DiskInfo {
        DiskInfo {
            id: "disk:99".into(),
            kind: DiskKind::Physical,
            path: String::new(),
            number: Some(99),
            model: String::new(),
            vendor: String::new(),
            serial: String::new(),
            firmware: String::new(),
            bus: "SATA".into(),
            media: MediaKind::Hdd,
            size: 0,
            sector_size: 512,
            physical_sector_size: 512,
            removable: false,
            read_only: false,
            offline: false,
            is_system: false,
            hosts_app: false,
            table: TableKind::None,
            partitions: vec![],
            used_bytes: None,
            fingerprint: String::new(),
            deep: false,
            notes: vec![],
            scsi_address: None,
        }
    }
}

/// Total usage of a disk from that of its partitions.
///
/// Only reports a total if it is representative: if the partitions whose usage is
/// unknown weigh more than 5% of the partitioned space (excluding MSR), the total would
/// be misleading (e.g. "20 MB used" on a full disk where only the ESP is readable).
pub fn estimate_used(parts: &[PartitionInfo]) -> Option<u64> {
    let relevant: Vec<&PartitionInfo> = parts.iter().filter(|p| p.role != PartRole::Msr).collect();
    let known: u64 = relevant.iter().filter_map(|p| p.used_bytes).sum();
    let unknown_size: u64 = relevant.iter().filter(|p| p.used_bytes.is_none()).map(|p| p.size).sum();
    let total: u64 = relevant.iter().map(|p| p.size).sum();
    if relevant.iter().all(|p| p.used_bytes.is_none()) || unknown_size.saturating_mul(20) > total { None } else { Some(known) }
}

/// Identity fingerprint of a disk (model, serial number, size, interface).
pub fn fingerprint(model: &str, serial: &str, size: u64, bus: &str, sector: u32) -> String {
    let text = format!("{}|{}|{}|{}|{}", model.trim(), serial.trim(), size, bus, sector);
    blake3::hash(text.as_bytes()).to_hex()[..16].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(role: PartRole, size: u64, used: Option<u64>) -> PartitionInfo {
        PartitionInfo {
            number: 1,
            offset: 0,
            size,
            role,
            type_label: String::new(),
            name: None,
            fs: None,
            label: None,
            used_bytes: used,
            mounts: vec![],
            active: false,
            volume_path: None,
        }
    }

    #[test]
    fn used_total_is_only_given_when_representative() {
        let g = 1u64 << 30;
        // Readable ESP, unknown main partition: no total.
        assert_eq!(estimate_used(&[p(PartRole::Esp, g / 10, Some(20 << 20)), p(PartRole::BasicData, 900 * g, None)]), None);
        // Only the small recovery partition is unknown: total given.
        assert_eq!(
            estimate_used(&[
                p(PartRole::Esp, g / 10, Some(1)),
                p(PartRole::Msr, g / 64, None),
                p(PartRole::BasicData, 900 * g, Some(2)),
                p(PartRole::Recovery, g, None)
            ]),
            Some(3)
        );
        assert_eq!(estimate_used(&[]), None);
    }
}
