//! Image files used as disks (source or destination).

use crate::device::FileDevice;
use crate::model::{DiskInfo, DiskKind, MediaKind, PartitionInfo, fingerprint};
use crate::plan::{DiskAnalysis, analyze_disk};
use std::io;
use std::path::Path;

/// Converts the result of an analysis into a description of partitions.
pub fn partition_infos(a: &DiskAnalysis) -> Vec<PartitionInfo> {
    let ss = a.sector_size as u64;
    a.parts
        .iter()
        .map(|sp| PartitionInfo {
            number: sp.part.number,
            offset: sp.part.start_lba * ss,
            size: sp.part.sectors * ss,
            role: sp.part.role(),
            type_label: sp.part.type_label(),
            name: sp.part.name().map(str::to_string),
            fs: match sp.fs.kind {
                crate::fs::FsKind::Unknown | crate::fs::FsKind::Blank => None,
                k => Some(k.label().to_string()),
            },
            label: sp.fs.label.clone(),
            used_bytes: sp.fs.used_bytes,
            mounts: vec![],
            active: sp.part.is_active(),
            volume_path: None,
        })
        .collect()
}

/// Describes an existing image file (read-only) and returns its analysis.
pub fn image_info(path: &Path, sector: u32) -> io::Result<(DiskInfo, DiskAnalysis)> {
    let mut dev = FileDevice::open_read_only(path, sector)?;
    let analysis = analyze_disk(&mut dev)?;
    let size = analysis.size;
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.display().to_string());
    let full = path.to_string_lossy().to_string();
    let partitions = partition_infos(&analysis);
    let used_total = crate::model::estimate_used(&partitions);
    let info = DiskInfo {
        id: crate::job::image_id(path),
        kind: DiskKind::Image,
        path: full.clone(),
        number: None,
        model: name,
        vendor: String::new(),
        serial: String::new(),
        firmware: String::new(),
        bus: t!("Fichier", "File").into(),
        media: MediaKind::Image,
        size,
        sector_size: sector,
        physical_sector_size: sector,
        removable: false,
        read_only: std::fs::metadata(path).map(|m| m.permissions().readonly()).unwrap_or(false),
        offline: false,
        is_system: false,
        hosts_app: false,
        table: analysis.layout.kind,
        used_bytes: used_total,
        partitions,
        fingerprint: fingerprint(&full, "", size, "Fichier", sector),
        deep: true,
        notes: vec![],
    };
    Ok((info, analysis))
}
