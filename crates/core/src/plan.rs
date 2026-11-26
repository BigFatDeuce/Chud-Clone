//! Clone planning: analysis of the source, placement of the partitions on the
//! destination and list of the ranges to copy.
//!
//! The plan is computed without writing anything. It is shown to the user, then run
//! as is by the engine: what is displayed is what is done.

use crate::device::BlockDevice;
use crate::diag::{Finding, human_size};
use crate::fs::{self, Extent, FsAnalysis, FsKind};
use crate::partition::gpt::{self, GptEntrySpec, GptSpec};
use crate::partition::{DiskLayout, MIB, PartInfo, PartRole, Partition, TableKind, read_layout};
use serde::{Deserialize, Serialize};
use std::io;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloneMode {
    /// Per partition; only the used blocks are copied when the file system allows it
    /// safely, otherwise the partition is copied in full.
    Smart,
    /// Per partition, every sector of each partition.
    RawPartitions,
    /// Whole disk, sector by sector, at the same positions.
    RawDisk,
}

impl CloneMode {
    pub fn label(self) -> &'static str {
        match self {
            CloneMode::Smart => t!("Intelligent (blocs utilisés)", "Smart (used blocks)"),
            CloneMode::RawPartitions => t!("Partitions complètes (secteur par secteur)", "Whole partitions (sector by sector)"),
            CloneMode::RawDisk => t!("Disque entier (secteur par secteur)", "Whole disk (sector by sector)"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanOptions {
    pub mode: CloneMode,
    /// On a larger destination (GPT): move the partitions after the main partition to
    /// the end of the disk, so that the free space is contiguous with the main
    /// partition and it can be extended afterwards.
    pub optimize_layout: bool,
    /// Automatically resize the main NTFS partition: grow it over the free space
    /// following it on a larger destination, or shrink it (moving the data beyond
    /// the new end) when the destination is too small for the partitions as they are.
    #[serde(default = "yes")]
    pub grow_main: bool,
}

fn yes() -> bool {
    true
}

impl Default for PlanOptions {
    fn default() -> Self {
        Self { mode: CloneMode::Smart, optimize_layout: true, grow_main: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourcePartition {
    pub part: Partition,
    pub fs: FsAnalysis,
}

/// Result of analysing a disk (source or plain inspection).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskAnalysis {
    pub size: u64,
    pub sector_size: u32,
    pub layout: DiskLayout,
    pub parts: Vec<SourcePartition>,
}

impl DiskAnalysis {
    /// Known used bytes, all partitions together (opaque partitions count for
    /// their whole size).
    pub fn used_bytes_estimate(&self) -> u64 {
        self.parts
            .iter()
            .map(|p| match p.part.role() {
                PartRole::Msr => 0,
                _ => p.fs.used_bytes.unwrap_or(p.part.sectors * self.sector_size as u64),
            })
            .sum()
    }

    pub fn all_findings(&self) -> Vec<Finding> {
        let mut v = self.layout.findings.clone();
        for p in &self.parts {
            for f in &p.fs.findings {
                let mut f = f.clone();
                if f.scope.is_none() {
                    f.scope = Some(format!("partition {}", p.part.number));
                }
                v.push(f);
            }
        }
        v
    }
}

/// Reads the partition table and analyses each file system. Read-only.
pub fn analyze_disk(dev: &mut dyn BlockDevice) -> io::Result<DiskAnalysis> {
    let layout = read_layout(dev)?;
    let ss = layout.sector_size as u64;
    let mut parts = Vec::new();
    for p in &layout.partitions {
        let in_bounds = p.sectors > 0 && p.end_lba() <= layout.disk_sectors;
        let analysis = if !in_bounds || p.role() == PartRole::Msr {
            FsAnalysis::new(FsKind::Unknown)
        } else {
            fs::analyze(dev, p.start_lba * ss, p.sectors * ss)
        };
        parts.push(SourcePartition { part: p.clone(), fs: analysis });
    }
    Ok(DiskAnalysis { size: dev.size(), sector_size: layout.sector_size, layout, parts })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CopyMethod {
    /// Used blocks only.
    Smart,
    /// Every sector.
    Raw,
    /// Nothing to copy (reserved partition without useful content).
    Skip,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedPartition {
    pub number: u32,
    pub role: PartRole,
    pub type_label: String,
    pub name: Option<String>,
    pub fs: FsKind,
    pub fs_label: Option<String>,
    pub used_bytes: Option<u64>,
    pub src_start: u64,
    pub src_len: u64,
    pub dst_start: u64,
    pub dst_len: u64,
    pub method: CopyMethod,
    /// Bytes actually copied for this partition.
    pub copy_bytes: u64,
    pub moved: bool,
    /// Explanation when the smart copy is not used.
    pub method_reason: Option<String>,
}

/// Planned growth of an NTFS partition (absolute bytes on the destination).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrowPlan {
    pub number: u32,
    pub dst_start: u64,
    /// Size of the partition (and of the volume) on the source.
    pub old_len: u64,
    /// Size of the partition on the destination.
    pub new_len: u64,
}

/// Planned shrink of an NTFS partition (absolute bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShrinkPlan {
    pub number: u32,
    pub src_start: u64,
    pub dst_start: u64,
    /// Size of the partition on the source.
    pub old_len: u64,
    /// Size of the partition on the destination.
    pub new_len: u64,
    /// Estimated bytes moved below the new end of the volume.
    pub relocated_bytes: u64,
}

/// Range to copy, in absolute bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyTask {
    pub src: u64,
    pub dst: u64,
    pub len: u64,
}

/// Partition table to install on the destination, once the data has been copied.
#[derive(Debug, Clone, Default)]
pub enum TablePlan {
    /// Rebuilt GPT (primary + backup) and protective MBR.
    Gpt { spec: GptSpec, src_sector0: Option<Vec<u8>> },
    /// MBR sector 0 taken from the source.
    Mbr { sector0: Vec<u8> },
    /// Nothing to write (raw copy with no GPT table to relocate).
    #[default]
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LayoutStrategy {
    /// Same positions as on the source.
    Identical,
    /// Partitions packed together to fit on a smaller destination.
    Compacted,
    /// Trailing partitions moved to the end of the disk; free space contiguous with the
    /// main partition.
    TrailingMoved,
    /// Raw copy of the whole disk.
    WholeDisk,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClonePlan {
    pub requested_mode: CloneMode,
    pub mode: CloneMode,
    pub table: TableKind,
    pub strategy: LayoutStrategy,
    pub src_size: u64,
    pub dst_size: u64,
    pub partitions: Vec<PlannedPartition>,
    pub total_copy_bytes: u64,
    /// Unallocated space left right after the main partition.
    pub free_after_main: u64,
    pub main_partition: Option<u32>,
    /// NTFS partition grown after the copy, if any.
    #[serde(default)]
    pub grow: Option<GrowPlan>,
    /// NTFS partition shrunk while it is copied, if any.
    #[serde(default)]
    pub shrink: Option<ShrinkPlan>,
    pub findings: Vec<Finding>,
    #[serde(skip)]
    pub tasks: Vec<CopyTask>,
    /// Ranges copied last (areas holding the table in a raw copy).
    #[serde(skip)]
    pub late_tasks: Vec<CopyTask>,
    #[serde(skip)]
    pub table_plan: TablePlan,
}

impl ClonePlan {
    pub fn is_blocked(&self) -> bool {
        crate::diag::has_blocking(&self.findings)
    }
}

/// Gaps smaller than this threshold between two used extents are copied with
/// them: fewer head movements, for a negligible extra cost.
const MERGE_GAP: u64 = 128 * 1024;

fn align_up(v: u64, a: u64) -> u64 {
    v.div_ceil(a) * a
}
fn align_down(v: u64, a: u64) -> u64 {
    v - v % a
}

fn merge_gaps(extents: &[Extent], sector: u64, limit: u64) -> Vec<Extent> {
    let mut out: Vec<Extent> = Vec::new();
    for e in extents {
        let start = align_down(e.offset, sector);
        let end = align_up(e.end(), sector).min(limit);
        if end <= start {
            continue;
        }
        match out.last_mut() {
            Some(last) if start <= last.end() + MERGE_GAP => {
                let new_end = last.end().max(end);
                last.len = new_end - last.offset;
            }
            _ => out.push(Extent { offset: start, len: end - start }),
        }
    }
    out
}

/// Why the main partition cannot be grown automatically (`None`:
/// it can).
fn grow_blocker(sp: &SourcePartition, opts: &PlanOptions) -> Option<String> {
    if !opts.grow_main {
        return Some(t!("l'agrandissement automatique est désactivé dans les options", "automatic growing is disabled in the options").into());
    }
    if sp.fs.kind != FsKind::Ntfs {
        return Some(tf!(
            "le volume est de type {}, seul NTFS non chiffré peut être agrandi",
            "the volume is {}; only unencrypted NTFS can be grown",
            sp.fs.kind.label()
        ));
    }
    if !sp.fs.grow_ready {
        return Some(sp.fs.grow_issue.clone().unwrap_or_else(|| t!("structures NTFS non contrôlées", "NTFS structures not checked").into()));
    }
    None
}

fn grow_finding(g: &GrowPlan) -> Finding {
    Finding::info(
        "NTFS_GROW_PLANNED",
        tf!("Partition {} agrandie automatiquement", "Partition {} grown automatically", g.number),
        tf!(
            "La partition {} passera de {} à {} (+{}). Après la copie et sa vérification, son système de fichiers NTFS est \
             étendu sur place, sans déplacer aucune donnée (mêmes modifications que l'outil de référence ntfsresize). \
             Le volume est ensuite marqué « à vérifier » : au premier démarrage du clone, Windows le contrôle une fois \
             (chkdsk) avant l'ouverture de session. C'est normal et peut prendre quelques minutes.",
            "Partition {} will go from {} to {} (+{}). After the copy and its verification, its NTFS file system is extended in place, without moving any data (the same changes as the reference tool ntfsresize). The volume is then flagged \"needs checking\": on the clone's first boot, Windows checks it once (chkdsk) before sign-in. This is normal and can take a few minutes.",
            g.number,
            human_size(g.old_len),
            human_size(g.new_len),
            human_size(g.new_len - g.old_len)
        ),
    )
}

fn no_grow_finding(number: u32, free: u64, reason: &str) -> Finding {
    Finding::info(
        "GROW_NOT_AUTOMATIC",
        t!("Espace supplémentaire à récupérer", "Extra space to reclaim"),
        tf!(
            "{} non alloués se trouveront juste après la partition {number}. Agrandissement automatique impossible : {reason}. \
             Une fois le clone en service, « Gestion des disques » → clic droit sur la partition → « Étendre le volume » \
             permet de récupérer cet espace.",
            "{} of unallocated space will sit right after partition {number}. Automatic growing is not possible: {reason}. Once the clone is in use, \"Disk Management\" → right-click the partition → \"Extend Volume\" reclaims this space.",
            human_size(free)
        ),
    )
}

/// Checks that the main partition can be shrunk to `new_sectors` while copied.
/// Returns the estimated bytes moved, or why it cannot.
fn shrink_check(sp: &SourcePartition, new_sectors: u64, ss: u64, opts: &PlanOptions, mode: CloneMode) -> Result<u64, String> {
    if !opts.grow_main {
        return Err(t!("l'ajustement automatique de la partition Windows est désactivé dans les options", "automatic resizing of the Windows partition is disabled in the options").into());
    }
    if mode != CloneMode::Smart {
        return Err(t!("elle exige le mode intelligent (blocs utilisés)", "it requires the smart mode (used blocks)").into());
    }
    if sp.fs.kind != FsKind::Ntfs {
        return Err(tf!(
            "le volume est de type {}, seul NTFS non chiffré peut être réduit",
            "the volume is {}; only unencrypted NTFS can be shrunk",
            sp.fs.kind.label()
        ));
    }
    let Some(ext) = &sp.fs.used_extents else {
        return Err(sp.fs.raw_reason.clone().unwrap_or_else(|| t!("carte d'allocation non lue", "allocation map not read").into()));
    };
    if !sp.fs.grow_ready {
        return Err(sp.fs.grow_issue.clone().unwrap_or_else(|| t!("structures NTFS non contrôlées", "NTFS structures not checked").into()));
    }
    let (Some(cluster), Some(fs_size)) = (sp.fs.cluster_size, sp.fs.fs_size) else {
        return Err(t!("géométrie NTFS inconnue", "unknown NTFS geometry").into());
    };
    if new_sectors == 0 || new_sectors >= sp.part.sectors {
        return Err(t!("la partition est plus petite que l'espace manquant", "the partition is smaller than the missing space").into());
    }
    fs::ntfs_shrink::estimate(ext, cluster as u64, fs_size, ss, new_sectors * ss).map(|e| e.relocated_bytes)
}

/// End of the shrunk volume's clusters, relative to the partition start.
fn shrunk_volume_end(sp: &SourcePartition, new_len: u64, ss: u64) -> u64 {
    let cl = sp.fs.cluster_size.map(u64::from).unwrap_or(ss).max(ss);
    (new_len / ss).saturating_sub(1) * ss / cl * cl
}

fn shrink_finding(s: &ShrinkPlan) -> Finding {
    Finding::info(
        "NTFS_SHRINK_PLANNED",
        tf!("Partition {} réduite pour tenir sur la destination", "Partition {} shrunk to fit the destination", s.number),
        tf!(
            "La partition {} passera de {} à {}. Pendant la copie, les données situées au-delà de la nouvelle fin ({}) \
             sont recopiées plus bas dans l'espace libre et le système de fichiers NTFS est mis à jour sur la destination ; \
             la source n'est pas modifiée. Le volume est ensuite marqué « à vérifier » : au premier démarrage du clone, \
             Windows le contrôle une fois (chkdsk) avant l'ouverture de session. C'est normal et peut prendre quelques minutes.",
            "Partition {} will go from {} to {}. During the copy, the data located beyond the new end ({}) is copied lower down into the free space and the NTFS file system is updated on the destination; the source is not modified. The volume is then flagged \"needs checking\": on the clone's first boot, Windows checks it once (chkdsk) before sign-in. This is normal and can take a few minutes.",
            s.number,
            human_size(s.old_len),
            human_size(s.new_len),
            human_size(s.relocated_bytes)
        ),
    )
}

/// "Main" partition: the largest NTFS or BitLocker data partition.
fn main_partition(src: &DiskAnalysis) -> Option<&SourcePartition> {
    src.parts
        .iter()
        .filter(|p| p.part.role() == PartRole::BasicData && matches!(p.fs.kind, FsKind::Ntfs | FsKind::Bitlocker))
        .max_by_key(|p| p.part.sectors)
}

fn blocked_plan(src: &DiskAnalysis, dst_size: u64, opts: &PlanOptions, mode: CloneMode, findings: Vec<Finding>) -> ClonePlan {
    ClonePlan {
        requested_mode: opts.mode,
        mode,
        table: src.layout.kind,
        strategy: LayoutStrategy::Identical,
        src_size: src.size,
        dst_size,
        partitions: describe_identical(src),
        total_copy_bytes: 0,
        free_after_main: 0,
        main_partition: main_partition(src).map(|p| p.part.number),
        grow: None,
        shrink: None,
        findings,
        tasks: vec![],
        late_tasks: vec![],
        table_plan: TablePlan::None,
    }
}

fn describe_identical(src: &DiskAnalysis) -> Vec<PlannedPartition> {
    let ss = src.sector_size as u64;
    src.parts
        .iter()
        .map(|p| PlannedPartition {
            number: p.part.number,
            role: p.part.role(),
            type_label: p.part.type_label(),
            name: p.part.name().map(str::to_string),
            fs: p.fs.kind,
            fs_label: p.fs.label.clone(),
            used_bytes: p.fs.used_bytes,
            src_start: p.part.start_lba * ss,
            src_len: p.part.sectors * ss,
            dst_start: p.part.start_lba * ss,
            dst_len: p.part.sectors * ss,
            method: CopyMethod::Raw,
            copy_bytes: p.part.sectors * ss,
            moved: false,
            method_reason: None,
        })
        .collect()
}

/// Builds the clone plan. Neither reads nor writes any disk.
pub fn build_plan(src: &DiskAnalysis, dst_size: u64, dst_sector: u32, opts: &PlanOptions) -> ClonePlan {
    let ss = src.sector_size as u64;
    let mut findings: Vec<Finding> = Vec::new();

    if dst_sector != src.sector_size {
        findings.push(
            Finding::error(
                "SECTOR_SIZE_MISMATCH",
                t!("Tailles de secteur différentes", "Different sector sizes"),
                tf!("La source utilise des secteurs logiques de {} octets et la destination de {} octets. \
                     Les systèmes de fichiers NTFS et FAT enregistrent la taille de secteur et ne peuvent pas être \
                     transposés tels quels : ce clonage n'est pas pris en charge.", "The source uses {}-byte logical sectors and the destination {}-byte ones. NTFS and FAT file systems record the sector size and cannot be transposed as is: this clone is not supported.",
                    src.sector_size, dst_sector
                ),
            )
            .blocking(),
        );
        return blocked_plan(src, dst_size, opts, opts.mode, findings);
    }
    let dst_size = align_down(dst_size, ss);
    let dst_sectors = dst_size / ss;

    // Without a recognised table, only copying the whole disk makes sense.
    let mut mode = opts.mode;
    if src.layout.kind == TableKind::None && mode != CloneMode::RawDisk {
        mode = CloneMode::RawDisk;
        findings.push(Finding::warning(
            "MODE_FORCED_RAW_DISK",
            t!("Copie du disque entier imposée", "Whole-disk copy required"),
            t!(
                "Aucune table de partitions exploitable n'a été trouvée sur la source : le disque sera copié en entier, \
             secteur par secteur.",
                "No usable partition table was found on the source: the disk will be copied in full, sector by sector."
            ),
        ));
    }

    if mode == CloneMode::RawDisk {
        return plan_raw_disk(src, dst_size, opts, findings);
    }

    // Table inconsistencies forbid working per partition.
    let layout_blockers: Vec<Finding> = src.layout.findings.iter().filter(|f| f.blocking).cloned().collect();
    if !layout_blockers.is_empty() {
        findings.extend(layout_blockers);
        findings.push(Finding::info(
            "RAW_DISK_SUGGESTED",
            t!("Copie du disque entier possible", "Whole-disk copy available"),
            t!("Le mode « Disque entier (secteur par secteur) » reste disponible : il reproduit le disque tel quel, \
             défauts compris, sur une destination au moins aussi grande.", "The \"Whole disk (sector by sector)\" mode remains available: it reproduces the disk as is, defects included, onto a destination at least as large."),
        ));
        return blocked_plan(src, dst_size, opts, mode, findings);
    }
    if src.parts.is_empty() {
        findings.push(
            Finding::error(
                "NO_PARTITIONS",
                t!("Aucune partition à cloner", "No partition to clone"),
                t!("La table de partitions de la source est vide.", "The source partition table is empty."),
            )
            .blocking(),
        );
        return blocked_plan(src, dst_size, opts, mode, findings);
    }

    match src.layout.kind {
        TableKind::Gpt => plan_gpt(src, dst_size, dst_sectors, opts, mode, findings),
        TableKind::Mbr => plan_mbr(src, dst_size, dst_sectors, opts, mode, findings),
        TableKind::None => unreachable!("handled above"),
    }
}

/// Smallest destination (multiple of 1 MiB) that receives the whole source with these
/// options: the default size of a new image, so that it holds the partitions rather
/// than the size of the whole source disk. Partitions keep their size (nothing is
/// shrunk); only the empty space after them is left out. Falls back to the source
/// size when the plan is blocked for another reason.
pub fn minimal_destination_size(src: &DiskAnalysis, dst_sector: u32, opts: &PlanOptions) -> u64 {
    // Partitions keep their size here: no automatic shrink.
    let opts = &PlanOptions { grow_main: false, ..opts.clone() };
    let too_small = |size: u64| build_plan(src, size, dst_sector, opts).findings.iter().any(|f| f.code == "DEST_TOO_SMALL");
    let full = align_up(src.size, MIB);
    if too_small(full) {
        return full;
    }
    // Binary search over whole mebibytes: planning reads nothing, it is instantaneous.
    let (mut lo, mut hi) = (1u64, full / MIB);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if too_small(mid * MIB) { lo = mid + 1 } else { hi = mid }
    }
    let size = hi * MIB;
    if build_plan(src, size, dst_sector, opts).findings.iter().any(|f| f.blocking) { full } else { size }
}

fn plan_raw_disk(src: &DiskAnalysis, dst_size: u64, opts: &PlanOptions, mut findings: Vec<Finding>) -> ClonePlan {
    let ss = src.sector_size as u64;
    if dst_size < src.size {
        findings.push(
            Finding::error(
                "DEST_TOO_SMALL",
                t!("Destination trop petite", "Destination too small"),
                tf!(
                    "La copie du disque entier exige une destination au moins aussi grande que la source \
                     ({} requis, {} disponibles).",
                    "A whole-disk copy needs a destination at least as large as the source ({} required, {} available).",
                    human_size(src.size),
                    human_size(dst_size)
                ),
            )
            .blocking(),
        );
        return blocked_plan(src, dst_size, opts, CloneMode::RawDisk, findings);
    }
    let head = MIB.min(src.size);

    // If the GPT table must be adjusted to the destination's size, its sectors
    // (start and end of the disk) are not copied from the source: they are written
    // once, at the end, when the table is rebuilt.
    let mut table_plan = TablePlan::None;
    let mut copy_start = 0u64;
    let mut copy_end = src.size;
    if let (TableKind::Gpt, Some(meta)) = (src.layout.kind, &src.layout.gpt)
        && (dst_size > src.size || !meta.backup_at_end || !meta.backup_valid || !meta.primary_valid)
    {
        let entries: Vec<GptEntrySpec> = src.layout.partitions.iter().filter_map(gpt_entry_same).collect();
        let spec = GptSpec {
            sector_size: src.sector_size,
            disk_sectors: dst_size / ss,
            disk_guid: meta.disk_guid,
            num_entries: meta.num_entries,
            entry_size: meta.entry_size,
            entries,
        };
        match gpt::build_gpt(&spec) {
                Ok(img) => {
                    copy_start = img.first_usable_lba * ss;
                    copy_end = src.size.min((img.last_usable_lba + 1) * ss);
                    table_plan = TablePlan::Gpt { spec, src_sector0: src.layout.mbr.as_ref().map(|m| m.sector0.clone()) };
                    findings.push(Finding::info(
                        "GPT_BACKUP_RELOCATED",
                        t!("Table GPT de secours replacée", "Backup GPT relocated"),
                        t!("Après la copie brute, la table GPT est réécrite pour que son exemplaire de secours se trouve bien                          en fin de disque de destination. Les partitions ne sont pas modifiées.", "After the raw copy, the GPT is rewritten so that its backup copy sits at the end of the destination disk. The partitions are not changed."),
                    ));
                }
                Err(_) => findings.push(Finding::warning(
                    "GPT_NOT_REBUILT",
                    t!("Table GPT non réajustée", "GPT not readjusted"),
                    t!("La table GPT de la source ne peut pas être reconstruite proprement pour la destination ;                      elle est copiée telle quelle. Windows corrige en général l'emplacement de la table de secours.", "The source GPT cannot be rebuilt cleanly for the destination; it is copied as is. Windows usually fixes the location of the backup table."),
                )),
            }
    }
    let mut tasks = Vec::new();
    if copy_end > head {
        tasks.push(CopyTask { src: head, dst: head, len: copy_end - head });
    }
    // The start of the disk (partition table) is copied last: an interrupted copy
    // does not leave a disk that looks valid.
    let mut late_tasks = Vec::new();
    if head > copy_start {
        late_tasks.push(CopyTask { src: copy_start, dst: copy_start, len: head.min(copy_end) - copy_start });
    }
    let total: u64 = tasks.iter().chain(late_tasks.iter()).map(|t| t.len).sum();
    if dst_size > src.size {
        findings.push(Finding::info(
            "RAW_DISK_UNUSED_SPACE",
            t!("Espace supplémentaire non utilisé", "Extra space not used"),
            tf!(
                "{} resteront non alloués en fin de disque de destination.",
                "{} will stay unallocated at the end of the destination disk.",
                human_size(dst_size - src.size)
            ),
        ));
    }
    ClonePlan {
        requested_mode: opts.mode,
        mode: CloneMode::RawDisk,
        table: src.layout.kind,
        strategy: LayoutStrategy::WholeDisk,
        src_size: src.size,
        dst_size,
        partitions: describe_identical(src),
        total_copy_bytes: total,
        free_after_main: 0,
        main_partition: main_partition(src).map(|p| p.part.number),
        grow: None,
        shrink: None,
        findings,
        tasks,
        late_tasks,
        table_plan,
    }
}

fn gpt_entry_same(p: &Partition) -> Option<GptEntrySpec> {
    match &p.info {
        PartInfo::Gpt { type_guid, unique_guid, attributes, name, slot } => Some(GptEntrySpec {
            slot: *slot,
            type_guid: *type_guid,
            unique_guid: *unique_guid,
            first_lba: p.start_lba,
            last_lba: p.end_lba() - 1,
            attributes: *attributes,
            name: name.clone(),
        }),
        _ => None,
    }
}

/// "Destination too small" finding with a concrete suggestion.
fn too_small_finding(src: &DiskAnalysis, needed: u64, available: u64, movable: bool, shrink_issue: Option<&str>) -> Finding {
    let deficit = needed.saturating_sub(available);
    let mut detail = tf!(
        "Les partitions de la source ont besoin de {} sur la destination, qui n'offre que {} (il manque {}).",
        "The source partitions need {} on the destination, which only offers {} ({} short).",
        human_size(needed),
        human_size(available),
        human_size(deficit)
    );
    if let Some(r) = shrink_issue {
        detail.push_str(&tf!(
            " Réduction automatique de la partition Windows impossible : {r}.",
            " The Windows partition cannot be shrunk automatically: {r}."
        ));
    }
    if let Some(main) = main_partition(src) {
        let ss = src.sector_size as u64;
        let size = main.part.sectors * ss;
        match main.fs.used_bytes {
            Some(used) if size.saturating_sub(used) > deficit + (512 * MIB).min(size / 20) => {
                detail.push_str(&tf!(
                    " Solution : dans Windows, ouvrez « Gestion des disques », réduisez la partition {} ({}, {} utilisés) \
                     d'au moins {}, puis relancez le clonage.",
                    " Fix: in Windows, open \"Disk Management\", shrink partition {} ({}, {} used) by at least {}, then start cloning again.",
                    main.part.number,
                    human_size(size),
                    human_size(used),
                    human_size(align_up(deficit, MIB) + MIB)
                ));
                if !movable {
                    detail.push_str(t!(
                        " Sur un disque MBR, les partitions ne sont pas déplacées : l'espace libéré doit se trouver en fin de disque.",
                        " On an MBR disk, partitions are not moved: the freed space must be at the end of the disk."
                    ));
                }
            }
            Some(used) => {
                detail.push_str(&tf!(
                    " La partition {} contient {} de données : il faut libérer de l'espace ou choisir une destination plus grande.",
                    " Partition {} holds {} of data: free up space or choose a larger destination.",
                    main.part.number,
                    human_size(used)
                ));
            }
            None => {
                detail.push_str(t!(" Choisissez une destination plus grande.", " Choose a larger destination."));
            }
        }
    }
    Finding::error("DEST_TOO_SMALL", t!("Destination trop petite", "Destination too small"), detail).blocking()
}

fn partition_tasks(
    sp: &SourcePartition,
    src_start: u64,
    dst_start: u64,
    len: u64,
    ss: u64,
    mode: CloneMode,
) -> (CopyMethod, Vec<CopyTask>, Option<String>) {
    if sp.part.role() == PartRole::Msr {
        return (
            CopyMethod::Skip,
            vec![],
            Some(
                t!(
                    "partition réservée Microsoft : aucun contenu à conserver, remise à zéro sur la destination",
                    "Microsoft reserved partition: no content to keep, filled with zeros on the destination"
                )
                .into(),
            ),
        );
    }
    if mode == CloneMode::Smart {
        if let Some(ext) = &sp.fs.used_extents {
            let merged = merge_gaps(ext, ss, len);
            let tasks = merged.iter().map(|e| CopyTask { src: src_start + e.offset, dst: dst_start + e.offset, len: e.len }).collect();
            return (CopyMethod::Smart, tasks, None);
        }
        return (CopyMethod::Raw, vec![CopyTask { src: src_start, dst: dst_start, len }], sp.fs.raw_reason.clone());
    }
    (
        CopyMethod::Raw,
        vec![CopyTask { src: src_start, dst: dst_start, len }],
        Some(t!("mode secteur par secteur demandé", "sector-by-sector mode requested").into()),
    )
}

fn plan_gpt(src: &DiskAnalysis, dst_size: u64, dst_sectors: u64, opts: &PlanOptions, mode: CloneMode, mut findings: Vec<Finding>) -> ClonePlan {
    let ss = src.sector_size as u64;
    let meta = src.layout.gpt.as_ref().expect("GPT without metadata");
    let align = (MIB / ss).max(1);
    let Some((first_usable, last_usable, _)) = gpt::geometry(src.sector_size, dst_sectors, meta.num_entries, meta.entry_size) else {
        findings.push(
            Finding::error(
                "DEST_TOO_SMALL",
                t!("Destination trop petite", "Destination too small"),
                t!("La destination est trop petite pour recevoir une table GPT.", "The destination is too small to hold a GPT."),
            )
            .blocking(),
        );
        return blocked_plan(src, dst_size, opts, mode, findings);
    };

    let parts = &src.parts;
    let src_end = src.layout.last_partition_end_lba();
    let fits_as_is = src_end <= last_usable + 1 && parts.iter().all(|p| p.part.start_lba >= first_usable);
    let main = main_partition(src);
    let mut strategy = LayoutStrategy::Identical;
    let mut starts: Vec<u64> = parts.iter().map(|p| p.part.start_lba).collect();
    let mut free_after_main = 0u64;

    let mut grow_end: Option<u64> = None;
    let mut shrink: Option<ShrinkPlan> = None;
    if fits_as_is && let Some(m) = main {
        let mi = parts.iter().position(|p| p.part.number == m.part.number).expect("partition principale");
        let trailing = &parts[mi + 1..];
        let mut next = trailing.first().map(|t| t.part.start_lba).unwrap_or(last_usable + 1);
        if opts.optimize_layout {
            // Place the trailing partitions against the end of the disk.
            let mut cursor = last_usable + 1;
            let mut new_starts = Vec::new();
            let mut ok = true;
            for t in trailing.iter().rev() {
                if cursor < t.part.sectors {
                    ok = false;
                    break;
                }
                let s = align_down(cursor - t.part.sectors, align);
                if s < m.part.end_lba() {
                    ok = false;
                    break;
                }
                new_starts.push(s);
                cursor = s;
            }
            new_starts.reverse();
            let gain = cursor.saturating_sub(m.part.end_lba()) * ss;
            let old_gap =
                trailing.first().map(|t| t.part.start_lba - m.part.end_lba()).unwrap_or(src.layout.disk_sectors.saturating_sub(m.part.end_lba()))
                    * ss;
            // Only move if it really frees contiguous space (≥ 16 MiB).
            if ok && !trailing.is_empty() && gain >= old_gap + 16 * MIB {
                for (i, s) in new_starts.iter().enumerate() {
                    starts[mi + 1 + i] = *s;
                }
                strategy = LayoutStrategy::TrailingMoved;
                next = cursor;
            }
        }
        free_after_main = next.saturating_sub(m.part.end_lba()) * ss;
        if free_after_main >= 16 * MIB {
            grow_end = Some(next);
        }
    } else if !fits_as_is {
        // Pack the partitions, in order, aligned on 1 MiB.
        strategy = LayoutStrategy::Compacted;
        let pack = |sizes: &[u64], starts: &mut [u64]| {
            let mut cursor = align_up(first_usable, align);
            for (i, n) in sizes.iter().enumerate() {
                let s = align_up(cursor, align);
                starts[i] = s;
                cursor = s + n;
            }
            cursor
        };
        let mut sizes: Vec<u64> = parts.iter().map(|p| p.part.sectors).collect();
        let cursor = pack(&sizes, &mut starts);
        if cursor > last_usable + 1 {
            // Shrink the main partition by just what is missing (whole MiB, so the
            // following partitions move by the same amount).
            let deficit = align_up(cursor - (last_usable + 1), align);
            let checked = match main {
                Some(m) => {
                    let mi = parts.iter().position(|p| p.part.number == m.part.number).expect("partition principale");
                    let new_sectors = m.part.sectors.saturating_sub(deficit);
                    shrink_check(m, new_sectors, ss, opts, mode).map(|r| (mi, new_sectors, r))
                }
                None => Err(t!("aucune partition Windows (NTFS) sur la source", "no Windows (NTFS) partition on the source").into()),
            };
            match checked {
                Ok((mi, new_sectors, relocated)) => {
                    sizes[mi] = new_sectors;
                    let end = pack(&sizes, &mut starts);
                    debug_assert!(end <= last_usable + 1);
                    shrink = Some(ShrinkPlan {
                        number: parts[mi].part.number,
                        src_start: parts[mi].part.start_lba * ss,
                        dst_start: starts[mi] * ss,
                        old_len: parts[mi].part.sectors * ss,
                        new_len: new_sectors * ss,
                        relocated_bytes: relocated,
                    });
                }
                Err(r) => {
                    let overhead = dst_sectors - (last_usable + 1);
                    findings.push(too_small_finding(src, (cursor + overhead) * ss, dst_size, true, Some(&r)));
                    return blocked_plan(src, dst_size, opts, mode, findings);
                }
            }
        }
    }

    // Grow the main partition up to the next partition.
    let mut grow: Option<GrowPlan> = None;
    let mut grow_reason: Option<String> = None;
    if let (Some(end), Some(m)) = (grow_end, main) {
        match grow_blocker(m, opts) {
            None => {
                grow = Some(GrowPlan {
                    number: m.part.number,
                    dst_start: m.part.start_lba * ss,
                    old_len: m.part.sectors * ss,
                    new_len: (end - m.part.start_lba) * ss,
                })
            }
            Some(r) => grow_reason = Some(r),
        }
    }

    let mut planned = Vec::new();
    let mut tasks = Vec::new();
    let mut entries = Vec::new();
    let mut total = 0u64;
    for (i, sp) in parts.iter().enumerate() {
        let p = &sp.part;
        let (src_start, len, dst_start) = (p.start_lba * ss, p.sectors * ss, starts[i] * ss);
        let shrunk = shrink.filter(|s| s.number == p.number);
        // Shrunk partition: the plan lists the used blocks kept in place (estimate);
        // the exact list, with the moved blocks, is computed when the clone starts.
        let copy_len = shrunk.map(|s| shrunk_volume_end(sp, s.new_len, ss)).unwrap_or(len);
        let (method, t, reason) = partition_tasks(sp, src_start, dst_start, copy_len, ss, mode);
        let copy_bytes: u64 = t.iter().map(|x| x.len).sum::<u64>() + shrunk.map(|s| s.relocated_bytes).unwrap_or(0);
        total += copy_bytes;
        tasks.extend(t);
        let dst_len = match (grow, shrunk) {
            (Some(g), _) if g.number == p.number => g.new_len,
            (_, Some(s)) => s.new_len,
            _ => len,
        };
        if let PartInfo::Gpt { type_guid, unique_guid, attributes, name, slot } = &p.info {
            entries.push(GptEntrySpec {
                slot: *slot,
                type_guid: *type_guid,
                unique_guid: *unique_guid,
                first_lba: starts[i],
                last_lba: starts[i] + dst_len / ss - 1,
                attributes: *attributes,
                name: name.clone(),
            });
        }
        planned.push(PlannedPartition {
            number: p.number,
            role: p.role(),
            type_label: p.type_label(),
            name: p.name().map(str::to_string),
            fs: sp.fs.kind,
            fs_label: sp.fs.label.clone(),
            used_bytes: sp.fs.used_bytes,
            src_start,
            src_len: len,
            dst_start,
            dst_len,
            method,
            copy_bytes,
            moved: starts[i] != p.start_lba,
            method_reason: reason,
        });
    }

    let spec = GptSpec {
        sector_size: src.sector_size,
        disk_sectors: dst_sectors,
        disk_guid: meta.disk_guid,
        num_entries: meta.num_entries,
        entry_size: meta.entry_size,
        entries,
    };
    if let Err(e) = gpt::build_gpt(&spec) {
        findings.push(
            Finding::error(
                "GPT_PLAN_INVALID",
                t!("Table de partitions de destination impossible à construire", "Destination partition table cannot be built"),
                tf!("Erreur interne de planification : {e}.", "Internal planning error: {e}."),
            )
            .blocking(),
        );
        return blocked_plan(src, dst_size, opts, mode, findings);
    }

    match strategy {
        LayoutStrategy::Compacted if shrink.is_some() => findings.push(shrink_finding(shrink.as_ref().expect("shrink"))),
        LayoutStrategy::Compacted => findings.push(Finding::info(
            "LAYOUT_COMPACTED",
            t!("Partitions resserrées", "Partitions packed together"),
            t!("La destination est plus petite que la source : les partitions conservent leur taille mais sont rapprochées \
             (l'espace non alloué de la source n'est pas reproduit). Sur un disque GPT, Windows repère ses partitions par \
             identifiant, et non par position.", "The destination is smaller than the source: the partitions keep their size but are moved closer together (the source's unallocated space is not reproduced). On a GPT disk, Windows finds its partitions by identifier, not by position."),
        )),
        LayoutStrategy::TrailingMoved => findings.push(Finding::info(
            "LAYOUT_TRAILING_MOVED",
            t!("Partitions de fin placées en fin de disque", "Trailing partitions placed at the end of the disk"),
            tf!("Les partitions situées après la partition principale sont placées en fin de disque, pour que l'espace \
                 supplémentaire soit contigu à la partition {}.", "The partitions located after the main partition are placed at the end of the disk, so that the extra space is contiguous with partition {}.",
                main.map(|m| m.part.number).unwrap_or(0)
            ),
        )),
        _ => {
            if grow.is_none() && grow_reason.is_none() && dst_size > src.size + 16 * MIB {
                findings.push(Finding::info(
                    "DEST_LARGER_UNUSED",
                    t!("Espace supplémentaire non alloué", "Extra unallocated space"),
                    tf!("{} resteront non alloués sur la destination.", "{} will stay unallocated on the destination.", human_size(dst_size - src.size)),
                ));
            }
        }
    }
    match (&grow, &grow_reason, main) {
        (Some(g), _, _) => findings.push(grow_finding(g)),
        (None, Some(r), Some(m)) => findings.push(no_grow_finding(m.part.number, free_after_main, r)),
        _ => {}
    }
    if grow.is_some() {
        free_after_main = 0;
    }
    if planned.iter().any(|p| p.moved && p.role == PartRole::Recovery) {
        findings.push(Finding::info(
            "RECOVERY_MOVED",
            t!("Partition de récupération déplacée", "Recovery partition moved"),
            t!("La partition de récupération Windows change de position. Si l'environnement de récupération apparaît désactivé \
             sur le clone, la commande « reagentc /enable » (invite administrateur) le réactive.", "The Windows recovery partition changes position. If the recovery environment shows as disabled on the clone, the command \"reagentc /enable\" (administrator prompt) re-enables it."),
        ));
    }

    ClonePlan {
        requested_mode: opts.mode,
        mode,
        table: TableKind::Gpt,
        strategy,
        src_size: src.size,
        dst_size,
        partitions: planned,
        total_copy_bytes: total,
        free_after_main,
        main_partition: main.map(|m| m.part.number),
        grow,
        shrink,
        findings,
        tasks,
        late_tasks: vec![],
        table_plan: TablePlan::Gpt { spec, src_sector0: src.layout.mbr.as_ref().map(|m| m.sector0.clone()) },
    }
}

fn plan_mbr(src: &DiskAnalysis, dst_size: u64, dst_sectors: u64, opts: &PlanOptions, mode: CloneMode, mut findings: Vec<Finding>) -> ClonePlan {
    let ss = src.sector_size as u64;
    let meta = src.layout.mbr.as_ref().expect("MBR without metadata");
    // On MBR, Windows boot relies on the position of the partitions
    // (disk signature + offset): positions are kept identical.
    let mut needed_end = src.layout.last_partition_end_lba();
    if let Some((start, sectors)) = meta.extended {
        needed_end = needed_end.max(start + sectors);
    }
    let align = (MIB / ss).max(1);
    let mut sector0 = meta.sector0.clone();
    let mut shrink: Option<ShrinkPlan> = None;
    if needed_end > dst_sectors {
        // Only the last partition can be shrunk (the others keep their position).
        let checked = match main_partition(src) {
            Some(m) if m.part.end_lba() == needed_end && !m.part.is_logical() => {
                let new_end = align_down(dst_sectors, align);
                let new_sectors = new_end.saturating_sub(m.part.start_lba);
                shrink_check(m, new_sectors, ss, opts, mode).map(|r| (m, new_sectors, r))
            }
            Some(_) => Err(t!("sur un disque MBR, seule la dernière partition peut être réduite", "on an MBR disk, only the last partition can be shrunk").into()),
            None => Err(t!("aucune partition Windows (NTFS) sur la source", "no Windows (NTFS) partition on the source").into()),
        };
        match checked {
            Ok((m, new_sectors, relocated)) => {
                let PartInfo::Mbr { slot, .. } = m.part.info else { unreachable!("MBR partition") };
                let e = 0x1BE + 16 * slot as usize;
                if sector0.len() < 512 || e + 16 > 510 {
                    findings.push(too_small_finding(src, needed_end * ss, dst_size, false, Some(t!("table MBR illisible", "unreadable MBR table"))));
                    return blocked_plan(src, dst_size, opts, mode, findings);
                }
                sector0[e + 12..e + 16].copy_from_slice(&(new_sectors as u32).to_le_bytes());
                shrink = Some(ShrinkPlan {
                    number: m.part.number,
                    src_start: m.part.start_lba * ss,
                    dst_start: m.part.start_lba * ss,
                    old_len: m.part.sectors * ss,
                    new_len: new_sectors * ss,
                    relocated_bytes: relocated,
                });
                needed_end = m.part.start_lba + new_sectors;
            }
            Err(r) => {
                findings.push(too_small_finding(src, needed_end * ss, dst_size, false, Some(&r)));
                return blocked_plan(src, dst_size, opts, mode, findings);
            }
        }
    }

    // Growth: only if the main partition is the last one on the disk
    // (primary, with no extended partition after it).
    let mut grow: Option<GrowPlan> = None;
    let mut grow_reason: Option<String> = None;
    let mut mbr_free = 0u64;
    if let Some(m) = main_partition(src)
        && m.part.end_lba() == needed_end
        && !m.part.is_logical()
        && let PartInfo::Mbr { slot, .. } = m.part.info
    {
        let limit = dst_sectors.min(1u64 << 32);
        let end = align_down(limit, align).max(m.part.end_lba());
        mbr_free = (end - m.part.end_lba()) * ss;
        if mbr_free >= 16 * MIB {
            match grow_blocker(m, opts) {
                None => {
                    let sectors = end - m.part.start_lba;
                    let e = 0x1BE + 16 * slot as usize;
                    if sectors <= u32::MAX as u64 && sector0.len() >= 512 && e + 16 <= 510 {
                        sector0[e + 12..e + 16].copy_from_slice(&(sectors as u32).to_le_bytes());
                        if end > 1024 * 255 * 63 {
                            sector0[e + 5..e + 8].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
                        }
                        grow = Some(GrowPlan {
                            number: m.part.number,
                            dst_start: m.part.start_lba * ss,
                            old_len: m.part.sectors * ss,
                            new_len: sectors * ss,
                        });
                    }
                }
                Some(r) => grow_reason = Some(r),
            }
        }
    }

    let mut planned = Vec::new();
    let mut tasks = Vec::new();
    let mut total = 0u64;

    // Boot track: sectors between the MBR and the first partition (boot loaders).
    let first_start = src.parts.iter().map(|p| p.part.start_lba).min().unwrap_or(1);
    let track_end = (first_start * ss).min(MIB).min(src.size);
    if track_end > ss {
        tasks.push(CopyTask { src: ss, dst: ss, len: track_end - ss });
        total += track_end - ss;
    }
    // EBRs of the logical partitions (same positions).
    for lba in &meta.ebr_lbas {
        tasks.push(CopyTask { src: lba * ss, dst: lba * ss, len: ss });
        total += ss;
    }
    for sp in &src.parts {
        let p = &sp.part;
        let (start, len) = (p.start_lba * ss, p.sectors * ss);
        let shrunk = shrink.filter(|s| s.number == p.number);
        let dst_len = match (grow, shrunk) {
            (Some(g), _) if g.number == p.number => g.new_len,
            (_, Some(s)) => s.new_len,
            _ => len,
        };
        let copy_len = shrunk.map(|s| shrunk_volume_end(sp, s.new_len, ss)).unwrap_or(len);
        let (method, t, reason) = partition_tasks(sp, start, start, copy_len, ss, mode);
        let copy_bytes: u64 = t.iter().map(|x| x.len).sum::<u64>() + shrunk.map(|s| s.relocated_bytes).unwrap_or(0);
        total += copy_bytes;
        tasks.extend(t);
        planned.push(PlannedPartition {
            number: p.number,
            role: p.role(),
            type_label: p.type_label(),
            name: None,
            fs: sp.fs.kind,
            fs_label: sp.fs.label.clone(),
            used_bytes: sp.fs.used_bytes,
            src_start: start,
            src_len: len,
            dst_start: start,
            dst_len,
            method,
            copy_bytes,
            moved: false,
            method_reason: reason,
        });
    }
    tasks.sort_by_key(|t| t.dst);

    let free_end = dst_size.saturating_sub(needed_end * ss);
    if let Some(s) = &shrink {
        findings.push(shrink_finding(s));
    }
    match (&grow, &grow_reason, main_partition(src)) {
        (Some(g), _, _) => findings.push(grow_finding(g)),
        (None, Some(r), Some(m)) => findings.push(no_grow_finding(m.part.number, mbr_free, r)),
        _ => {}
    }
    if grow.is_none() && grow_reason.is_none() && free_end > 16 * MIB {
        findings.push(Finding::info(
            "DEST_LARGER_UNUSED",
            t!("Espace supplémentaire non alloué", "Extra unallocated space"),
            tf!("{} resteront non alloués en fin de disque de destination. Une fois le clone en service, \
                 « Gestion des disques » permet d'étendre la dernière partition si elle est adjacente à cet espace.", "{} will stay unallocated at the end of the destination disk. Once the clone is in use, \"Disk Management\" can extend the last partition if it is adjacent to this space.",
                human_size(free_end)
            ),
        ));
    }
    if dst_size > (u32::MAX as u64) * ss {
        findings.push(Finding::warning(
            "MBR_2TB_LIMIT",
            t!("Limite de 2 To du partitionnement MBR", "MBR partitioning 2 TB limit"),
            t!("La destination dépasse 2 To : un disque MBR ne peut pas adresser l'espace au-delà. \
             L'excédent restera inutilisable tant que le disque n'est pas converti en GPT.", "The destination exceeds 2 TB: an MBR disk cannot address the space beyond. The excess will stay unusable until the disk is converted to GPT."),
        ));
    }

    ClonePlan {
        requested_mode: opts.mode,
        mode,
        table: TableKind::Mbr,
        strategy: LayoutStrategy::Identical,
        src_size: src.size,
        dst_size,
        partitions: planned,
        total_copy_bytes: total,
        free_after_main: 0,
        main_partition: main_partition(src).map(|p| p.part.number),
        grow,
        shrink,
        findings,
        tasks,
        late_tasks: vec![],
        table_plan: TablePlan::Mbr { sector0 },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_gaps_aligns_and_merges() {
        let e = vec![
            Extent { offset: 0, len: 4096 },
            Extent { offset: 8192, len: 4096 },     // 4 KiB gap → merged
            Extent { offset: 10 * MIB, len: 1000 }, // far → separate, rounded to the sector
            Extent { offset: 10 * MIB + 2000, len: 100 },
        ];
        let m = merge_gaps(&e, 512, 11 * MIB);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0], Extent { offset: 0, len: 12288 });
        assert_eq!(m[1].offset, 10 * MIB);
        assert_eq!(m[1].len, 2560);
    }

    #[test]
    fn merge_gaps_clamps_to_partition() {
        let e = vec![Extent { offset: 1024, len: 3000 }];
        let m = merge_gaps(&e, 512, 3584);
        assert_eq!(m, vec![Extent { offset: 1024, len: 2560 }]);
    }
}
