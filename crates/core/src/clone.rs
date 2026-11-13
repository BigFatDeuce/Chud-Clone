//! Orchestration of a clone: destination preparation, copy, writing of the
//! partition table, verifications and final status.
//!
//! Order of the writes to the destination:
//! 1. erasing the old tables (start and end of the disk);
//! 2. partition data;
//! 3. partition table, at the very end.
//!
//! An interrupted operation thus leaves a destination without a partition table,
//! clearly unusable, rather than a disk that looks valid but is incomplete.

use crate::boot::{self, BootAssessment};
use crate::device::{AlignedBuf, BlockDevice};
use crate::diag::{Finding, Severity};
use crate::engine::{
    BadOutcome, BadRange, Control, Copier, DataVerification, EngineOptions, LogLevel, MismatchKind, Observer, Phase, Progress, RecoveredRange,
    StopReason, VerifyMode, verify_data,
};
use crate::fs::ntfs_grow::{self, GrowSummary};
use crate::partition::gpt;
use crate::partition::mbr;
use crate::partition::{MIB, PartInfo, TableKind};
use crate::plan::{CloneMode, ClonePlan, CopyMethod, DiskAnalysis, GrowPlan, TablePlan, analyze_disk};
use serde::{Deserialize, Serialize};
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinalStatus {
    /// Complete copy, without errors, and verifications passed.
    SuccessVerified,
    /// Complete copy, but with reservations (verification not done, anomalies…).
    CompletedWithWarnings,
    /// Data is missing on the destination.
    Incomplete,
    /// The operation failed; the destination is not a usable clone.
    Failed,
}

impl FinalStatus {
    pub fn label(self) -> &'static str {
        match self {
            FinalStatus::SuccessVerified => t!("Clonage réussi et vérifié", "Clone succeeded and verified"),
            FinalStatus::CompletedWithWarnings => t!("Clonage terminé avec avertissements", "Clone completed with warnings"),
            FinalStatus::Incomplete => t!("Clonage incomplet", "Clone incomplete"),
            FinalStatus::Failed => t!("Clonage échoué", "Clone failed"),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CopySummary {
    pub bytes_planned: u64,
    pub bytes_copied: u64,
    pub bytes_zero_filled: u64,
    pub read_error_events: u64,
    pub write_error_events: u64,
    pub recovered_bytes: u64,
    pub copy_ms: u64,
    pub avg_bps: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableCheck {
    pub performed: bool,
    pub ok: bool,
    pub problems: Vec<String>,
}

/// Result of growing the main partition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrowOutcome {
    pub number: u32,
    pub attempted: bool,
    pub done: bool,
    pub summary: Option<GrowSummary>,
    pub error: Option<String>,
    /// Volume structures had already been modified at the time of the failure.
    pub modified: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloneOutcome {
    pub status: FinalStatus,
    /// Reasons for the status, in plain words.
    pub reasons: Vec<String>,
    pub stop: StopReason,
    pub copy: CopySummary,
    pub bad_ranges: Vec<BadRange>,
    pub recovered: Vec<RecoveredRange>,
    pub table_written: bool,
    pub table_check: TableCheck,
    pub data_check: DataVerification,
    /// Findings from checking the clone's structures.
    pub structure_findings: Vec<Finding>,
    pub boot_source: Option<BootAssessment>,
    pub boot_destination: Option<BootAssessment>,
    pub boot_findings: Vec<Finding>,
    #[serde(default)]
    pub grow: Option<GrowOutcome>,
    pub duration_ms: u64,
}

pub struct CloneRequest<'a> {
    pub src: &'a mut dyn BlockDevice,
    pub dst: &'a mut dyn BlockDevice,
    pub src_analysis: &'a DiskAnalysis,
    pub plan: &'a ClonePlan,
    pub opts: &'a EngineOptions,
    pub ctl: &'a Control,
    pub obs: &'a dyn Observer,
}

fn phase(obs: &dyn Observer, phase: Phase) {
    obs.log(LogLevel::Info, phase.label());
    obs.progress(&Progress {
        phase,
        done: 0,
        total: 0,
        elapsed_ms: 0,
        speed_bps: 0,
        avg_bps: 0,
        eta_secs: None,
        read_errors: 0,
        bad_bytes: 0,
        recovered_bytes: 0,
        write_errors: 0,
        current_lba: 0,
    });
}

/// Consistency checks of the plan against the devices actually opened.
fn sanity(req: &CloneRequest) -> Result<(), String> {
    if req.plan.is_blocked() {
        return Err(t!("le plan comporte des constats bloquants", "the plan contains blocking findings").into());
    }
    if req.dst.is_read_only() {
        return Err(t!("la destination est en lecture seule", "the destination is read-only").into());
    }
    if req.src.size() != req.plan.src_size {
        return Err(tf!(
            "la taille de la source a changé depuis l'analyse ({} ≠ {})",
            "the source size has changed since the analysis ({} ≠ {})",
            req.src.size(),
            req.plan.src_size
        ));
    }
    let ss = req.src.sector_size() as u64;
    if req.dst.sector_size() as u64 != ss {
        return Err(
            t!("tailles de secteur différentes entre la source et la destination", "different sector sizes between source and destination").into()
        );
    }
    if req.dst.size() - req.dst.size() % ss != req.plan.dst_size {
        return Err(tf!(
            "la taille de la destination a changé depuis l'analyse ({} ≠ {})",
            "the destination size has changed since the analysis ({} ≠ {})",
            req.dst.size(),
            req.plan.dst_size
        ));
    }
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    for t in req.plan.tasks.iter().chain(req.plan.late_tasks.iter()) {
        if t.len == 0 {
            continue;
        }
        if t.src.checked_add(t.len).is_none_or(|e| e > req.src.size()) {
            return Err(tf!("tâche hors de la source ({t:?})", "task outside the source ({t:?})"));
        }
        if t.dst.checked_add(t.len).is_none_or(|e| e > req.dst.size()) {
            return Err(tf!("tâche hors de la destination ({t:?})", "task outside the destination ({t:?})"));
        }
        ranges.push((t.dst, t.dst + t.len));
    }
    ranges.sort();
    if ranges.windows(2).any(|w| w[0].1 > w[1].0) {
        return Err(t!(
            "deux tâches de copie écrivent au même endroit de la destination",
            "two copy tasks write to the same place on the destination"
        )
        .into());
    }
    req.opts.validate()
}

fn zero_range(dst: &mut dyn BlockDevice, offset: u64, len: u64) -> std::io::Result<()> {
    let buf = AlignedBuf::new(MIB as usize);
    let mut p = 0;
    while p < len {
        let n = (MIB).min(len - p) as usize;
        dst.write_at(offset + p, &buf[..n])?;
        p += n as u64;
    }
    Ok(())
}

fn write_table(req: &mut CloneRequest) -> Result<bool, String> {
    let ss = req.dst.sector_size() as u64;
    match &req.plan.table_plan {
        TablePlan::None => Ok(false),
        TablePlan::Mbr { sector0 } => {
            let mut s = AlignedBuf::new(ss as usize);
            let n = sector0.len().min(ss as usize);
            s[..n].copy_from_slice(&sector0[..n]);
            req.dst.write_at(0, &s[..ss as usize]).map_err(|e| tf!("écriture du MBR : {e}", "writing the MBR: {e}"))?;
            Ok(true)
        }
        TablePlan::Gpt { spec, src_sector0 } => {
            let img = gpt::build_gpt(spec)?;
            let write = |dst: &mut dyn BlockDevice, lba: u64, data: &[u8], what: &str| -> Result<(), String> {
                let mut b = AlignedBuf::new(data.len());
                b.copy_from_slice(data);
                dst.write_at(lba * ss, &b).map_err(|e| tf!("écriture de {what} (LBA {lba}) : {e}", "writing {what} (LBA {lba}): {e}"))
            };
            // Backup first, primary next, protective MBR last.
            write(req.dst, img.backup_lba, &img.backup, t!("la table GPT de secours", "the backup GPT"))?;
            write(req.dst, img.primary_lba, &img.primary, t!("la table GPT principale", "the primary GPT"))?;
            let pm = mbr::build_protective(src_sector0.as_deref(), spec.sector_size, spec.disk_sectors);
            write(req.dst, 0, &pm, t!("le MBR de protection", "the protective MBR"))?;
            Ok(true)
        }
    }
}

/// Compares the table read back from the destination with what the plan intended.
fn check_table(plan: &ClonePlan, src: &DiskAnalysis, dst: &DiskAnalysis) -> TableCheck {
    let ss = src.sector_size as u64;
    let mut problems = Vec::new();
    if dst.layout.kind != plan.table {
        problems.push(tf!(
            "type de table relu : {} (attendu : {})",
            "table type read back: {} (expected: {})",
            dst.layout.kind.label(),
            plan.table.label()
        ));
    }
    for f in &dst.layout.findings {
        if f.severity >= Severity::Warning {
            problems.push(format!("{} — {}", f.title, f.detail));
        }
    }
    match (plan.table, &src.layout.gpt, &dst.layout.gpt) {
        (TableKind::Gpt, Some(s), Some(d)) => {
            if s.disk_guid != d.disk_guid {
                problems.push(
                    t!("l'identifiant de disque GPT diffère de celui de la source", "the GPT disk identifier differs from the source's").into(),
                );
            }
            if !(d.primary_valid && d.backup_valid && d.backup_at_end) {
                problems.push(
                    t!(
                        "la table GPT principale ou sa copie de secours n'est pas valide sur la destination",
                        "the primary GPT or its backup copy is not valid on the destination"
                    )
                    .into(),
                );
            }
        }
        (TableKind::Gpt, _, None) => {
            problems.push(t!("aucune table GPT valide relue sur la destination", "no valid GPT read back from the destination").into())
        }
        _ => {}
    }
    if let (TableKind::Mbr, Some(s), Some(d)) = (plan.table, &src.layout.mbr, &dst.layout.mbr)
        && s.signature != d.signature
    {
        problems.push(t!("la signature de disque MBR diffère de celle de la source", "the MBR disk signature differs from the source's").into());
    }
    if plan.table != TableKind::None {
        if dst.parts.len() != plan.partitions.len() {
            problems.push(tf!(
                "{} partition(s) relue(s), {} attendue(s)",
                "{} partition(s) read back, {} expected",
                dst.parts.len(),
                plan.partitions.len()
            ));
        }
        for pp in &plan.partitions {
            let Some(d) = dst.parts.iter().find(|d| d.part.number == pp.number) else {
                problems.push(tf!("partition {} absente de la destination", "partition {} missing from the destination", pp.number));
                continue;
            };
            if d.part.start_lba * ss != pp.dst_start || d.part.sectors * ss != pp.dst_len {
                problems.push(tf!(
                    "partition {} : position relue LBA {} (+{}), attendue LBA {} (+{})",
                    "partition {}: read back at LBA {} (+{}), expected LBA {} (+{})",
                    pp.number,
                    d.part.start_lba,
                    d.part.sectors,
                    pp.dst_start / ss,
                    pp.dst_len / ss
                ));
            }
            if let Some(s) = src.parts.iter().find(|s| s.part.number == pp.number) {
                let same = match (&s.part.info, &d.part.info) {
                    (
                        PartInfo::Gpt { type_guid: t1, unique_guid: u1, attributes: a1, name: n1, .. },
                        PartInfo::Gpt { type_guid: t2, unique_guid: u2, attributes: a2, name: n2, .. },
                    ) => t1 == t2 && u1 == u2 && a1 == a2 && n1 == n2,
                    (PartInfo::Mbr { type_id: t1, active: a1, logical: l1, .. }, PartInfo::Mbr { type_id: t2, active: a2, logical: l2, .. }) => {
                        t1 == t2 && a1 == a2 && l1 == l2
                    }
                    _ => false,
                };
                if !same {
                    problems.push(tf!(
                        "partition {} : type, identifiant ou attributs différents de la source",
                        "partition {}: type, identifier or attributes differ from the source",
                        pp.number
                    ));
                }
            }
        }
    }
    TableCheck { performed: true, ok: problems.is_empty(), problems }
}

/// Compares the clone's file systems with the source's (taking a successful
/// growth into account).
fn check_structures(plan: &ClonePlan, src: &DiskAnalysis, dst: &DiskAnalysis, grow: Option<&GrowOutcome>) -> Vec<Finding> {
    let mut out = Vec::new();
    for pp in &plan.partitions {
        if pp.method == CopyMethod::Skip {
            continue;
        }
        let (Some(s), Some(d)) = (src.parts.iter().find(|s| s.part.number == pp.number), dst.parts.iter().find(|d| d.part.number == pp.number))
        else {
            continue;
        };
        let scope = format!("partition {}", pp.number);
        if s.fs.kind != d.fs.kind {
            out.push(
                Finding::error(
                    "CLONE_FS_KIND_DIFFERS",
                    t!("Système de fichiers non reconnu sur le clone", "File system not recognised on the clone"),
                    tf!("Source : {}. Clone : {}.", "Source: {}. Clone: {}.", s.fs.kind.label(), d.fs.kind.label()),
                )
                .scope(scope),
            );
            continue;
        }
        let grown = grow.filter(|g| g.number == pp.number && g.done).and_then(|g| g.summary.as_ref());
        let expected_size = grown.map(|g| Some(g.new_size)).unwrap_or(s.fs.fs_size);
        let expected_used = match grown {
            Some(g) => s.fs.used_bytes.map(|u| u + g.bitmap_extra_clusters * g.cluster_size),
            None => s.fs.used_bytes,
        };
        if expected_size != d.fs.fs_size || s.fs.cluster_size != d.fs.cluster_size || s.fs.label != d.fs.label {
            out.push(
                Finding::error(
                    "CLONE_FS_GEOMETRY_DIFFERS",
                    t!("Caractéristiques du système de fichiers différentes sur le clone", "File system characteristics differ on the clone"),
                    t!(
                        "La taille, la taille de cluster ou le nom de volume relus sur le clone ne correspondent pas à la source.",
                        "The size, cluster size or volume name read back from the clone do not match the source."
                    ),
                )
                .scope(scope),
            );
            continue;
        }
        if expected_used != d.fs.used_bytes {
            out.push(
                Finding::error(
                    "CLONE_FS_USAGE_DIFFERS",
                    t!("Carte d'allocation différente sur le clone", "Different allocation map on the clone"),
                    tf!(
                        "Espace utilisé attendu : {:?} octets ; relu sur le clone : {:?} octets.",
                        "Expected used space: {:?} bytes; read back from the clone: {:?} bytes.",
                        expected_used,
                        d.fs.used_bytes
                    ),
                )
                .scope(scope),
            );
            continue;
        }
        // A structural defect that appeared on the clone while the source was free of it.
        for f in &d.fs.findings {
            if f.severity >= Severity::Error && !s.fs.findings.iter().any(|x| x.code == f.code) {
                out.push(f.clone().scope(scope.clone()));
            }
        }
    }
    out
}

/// Runs a complete clone and returns its outcome. Does not panic on an I/O
/// error: every failure is described in the result.
pub fn run_clone(mut req: CloneRequest) -> CloneOutcome {
    let started = Instant::now();
    let mut outcome = CloneOutcome {
        status: FinalStatus::Failed,
        reasons: vec![],
        stop: StopReason::Completed,
        copy: CopySummary { bytes_planned: req.plan.total_copy_bytes, ..Default::default() },
        bad_ranges: vec![],
        recovered: vec![],
        table_written: false,
        table_check: TableCheck { performed: false, ok: false, problems: vec![] },
        data_check: DataVerification::skipped(),
        structure_findings: vec![],
        boot_source: None,
        boot_destination: None,
        boot_findings: vec![],
        grow: None,
        duration_ms: 0,
    };

    if let Err(e) = sanity(&req) {
        req.obs.log(LogLevel::Error, &tf!("Clonage refusé avant toute écriture : {e}", "Clone refused before any write: {e}"));
        outcome.stop = StopReason::Internal(e.clone());
        outcome.reasons.push(tf!("Clonage refusé avant toute écriture : {e}.", "Clone refused before any write: {e}."));
        outcome.duration_ms = started.elapsed().as_millis() as u64;
        return outcome;
    }
    req.obs.log(
        LogLevel::Info,
        &tf!(
            "Début du clonage — mode : {} ; {} à copier ; vérification : {}",
            "Clone started — mode: {}; {} to copy; verification: {}",
            req.plan.mode.label(),
            crate::diag::human_size(req.plan.total_copy_bytes),
            req.opts.verify.label()
        ),
    );

    // --- 1. Preparation: invalidating the old tables -----------------------------
    phase(req.obs, Phase::Preparing);
    let dst_size = req.plan.dst_size;
    let head = MIB.min(dst_size);
    let prep = zero_range(req.dst, 0, head).and_then(|_| if dst_size > 2 * MIB { zero_range(req.dst, dst_size - MIB, MIB) } else { Ok(()) });
    if let Err(e) = prep {
        let msg =
            tf!("Impossible d'écrire sur la destination pendant la préparation : {e}", "Cannot write to the destination during preparation: {e}");
        req.obs.log(LogLevel::Error, &msg);
        outcome.stop = StopReason::DestinationFailed(msg.clone());
        outcome.reasons.push(msg);
        outcome.duration_ms = started.elapsed().as_millis() as u64;
        return outcome;
    }

    // --- 2. Copie -------------------------------------------------------------
    let (stop, stats) = {
        let mut copier = Copier::new(req.src, req.dst, req.opts, req.ctl, req.obs, req.plan.total_copy_bytes);
        let mut stop = copier.run(&req.plan.tasks);
        if stop == StopReason::Completed && !req.plan.late_tasks.is_empty() {
            stop = copier.run(&req.plan.late_tasks);
        }
        (stop, copier.stats)
    };
    outcome.copy = CopySummary {
        bytes_planned: req.plan.total_copy_bytes,
        bytes_copied: stats.bytes_copied,
        bytes_zero_filled: stats.bytes_zero_filled,
        read_error_events: stats.read_error_events,
        write_error_events: stats.write_error_events,
        recovered_bytes: stats.recovered_bytes,
        copy_ms: stats.copy_ms,
        avg_bps: if stats.copy_ms > 0 { (stats.bytes_copied as u128 * 1000 / stats.copy_ms as u128) as u64 } else { 0 },
    };
    outcome.bad_ranges = stats.bad_ranges.clone();
    outcome.recovered = stats.recovered.clone();
    outcome.stop = stop.clone();

    // --- 3. Table de partitions ------------------------------------------------
    if stop == StopReason::Completed {
        phase(req.obs, Phase::WritingTable);
        match write_table(&mut req) {
            Ok(written) => outcome.table_written = written,
            Err(e) => {
                let msg = tf!("Échec de l'écriture de la table de partitions : {e}", "Failed to write the partition table: {e}");
                req.obs.log(LogLevel::Error, &msg);
                outcome.stop = StopReason::DestinationFailed(msg);
            }
        }
    }
    if let Err(e) = req.dst.flush() {
        let msg = tf!("Échec du vidage des écritures vers la destination : {e}", "Failed to flush writes to the destination: {e}");
        req.obs.log(LogLevel::Error, &msg);
        if outcome.stop == StopReason::Completed {
            outcome.stop = StopReason::DestinationFailed(msg);
        }
    }

    // --- 4. Verifications ---------------------------------------------------------
    if outcome.stop == StopReason::Completed {
        if req.opts.verify != VerifyMode::None {
            phase(req.obs, Phase::Verifying);
            outcome.data_check = verify_data(req.src, req.dst, &stats.hashes, req.opts.verify, req.ctl, req.obs);
        }
        // Growth: only on a destination that has just returned the data faithfully
        // (it modifies areas already verified).
        if let Some(g) = req.plan.grow {
            outcome.grow = Some(grow_main(&mut req, g, &outcome.data_check));
        }
        phase(req.obs, Phase::Checking);
        match analyze_disk(req.dst) {
            Ok(dst_analysis) => {
                outcome.table_check = check_table(req.plan, req.src_analysis, &dst_analysis);
                for p in &outcome.table_check.problems {
                    req.obs.log(LogLevel::Error, &tf!("Table de partitions du clone : {p}", "Clone partition table: {p}"));
                }
                outcome.structure_findings = check_structures(req.plan, req.src_analysis, &dst_analysis, outcome.grow.as_ref());
                let bs = boot::assess_boot(req.src, req.src_analysis);
                let bd = boot::assess_boot(req.dst, &dst_analysis);
                outcome.boot_findings = boot::compare_boot(&bs, &bd);
                outcome.boot_source = Some(bs);
                outcome.boot_destination = Some(bd);
            }
            Err(e) => {
                outcome.table_check = TableCheck {
                    performed: true,
                    ok: false,
                    problems: vec![tf!("relecture de la destination impossible : {e}", "cannot read the destination back: {e}")],
                };
            }
        }
    }

    decide_status(&mut outcome, req.plan, req.src_analysis, req.opts);
    outcome.duration_ms = started.elapsed().as_millis() as u64;
    phase_done(req.obs, &outcome);
    outcome
}

fn grow_main(req: &mut CloneRequest, g: GrowPlan, check: &DataVerification) -> GrowOutcome {
    let mut go = GrowOutcome { number: g.number, attempted: false, done: false, summary: None, error: None, modified: false };
    if check.mismatches.iter().any(|m| matches!(m.kind, MismatchKind::DestinationDiffers | MismatchKind::DestinationUnreadable)) {
        go.error = Some(
            t!(
                "non tenté : la destination n'a pas restitué fidèlement les données copiées",
                "not attempted: the destination did not faithfully return the copied data"
            )
            .into(),
        );
        return go;
    }
    if req.ctl.is_cancelled() {
        go.error = Some(t!("non tenté : opération annulée", "not attempted: operation cancelled").into());
        return go;
    }
    phase(req.obs, Phase::Growing);
    go.attempted = true;
    match ntfs_grow::grow(req.dst, g.dst_start, g.new_len) {
        Ok(s) => {
            req.obs.log(
                LogLevel::Info,
                &tf!(
                    "Partition {} agrandie : {} → {} ({} clusters libres ajoutés)",
                    "Partition {} grown: {} → {} ({} free clusters added)",
                    g.number,
                    crate::diag::human_size(s.old_size),
                    crate::diag::human_size(s.new_size),
                    s.new_clusters - s.old_clusters - s.bitmap_extra_clusters
                ),
            );
            go.done = true;
            go.summary = Some(s);
        }
        Err(e) => {
            req.obs
                .log(LogLevel::Error, &tf!("Agrandissement de la partition {} impossible : {}", "Cannot grow partition {}: {}", g.number, e.message));
            go.error = Some(e.message);
            go.modified = e.modified;
        }
    }
    if let Err(e) = req.dst.flush() {
        go.done = false;
        go.modified = true;
        go.error = Some(tf!("vidage des écritures : {e}", "flushing writes: {e}"));
    }
    go
}

fn phase_done(obs: &dyn Observer, outcome: &CloneOutcome) {
    let level = match outcome.status {
        FinalStatus::SuccessVerified => LogLevel::Info,
        FinalStatus::CompletedWithWarnings => LogLevel::Warning,
        _ => LogLevel::Error,
    };
    obs.log(level, &tf!("Résultat : {}", "Result: {}", outcome.status.label()));
    for r in &outcome.reasons {
        obs.log(level, &format!("  · {r}"));
    }
    phase(obs, Phase::Done);
}

/// Determines the final status. "Succeeded" is only granted if the copy is complete
/// AND the verifications were actually done and are conclusive.
fn decide_status(o: &mut CloneOutcome, plan: &ClonePlan, src: &DiskAnalysis, opts: &EngineOptions) {
    let mut reasons = Vec::new();
    let status;
    match &o.stop {
        StopReason::Cancelled => {
            status = FinalStatus::Incomplete;
            reasons.push(
                t!("Opération annulée. Les données déjà écrites restent sur la destination, mais aucune table de partitions n'y a été \
                 écrite : elle n'est pas utilisable et son ancien contenu est perdu.", "Operation cancelled. The data already written stays on the destination, but no partition table was written to it: it is not usable and its previous content is lost.")
                    .to_string(),
            );
        }
        StopReason::Unreadable => {
            status = FinalStatus::Incomplete;
            let sectors: u64 = o.bad_ranges.iter().map(|b| b.sectors).sum();
            reasons.push(tf!("{sectors} secteur(s) de la source sont restés illisibles après plusieurs tentatives ; le clonage a été arrêté. \
                 La destination n'a pas reçu de table de partitions et n'est pas utilisable.", "{sectors} source sector(s) stayed unreadable after several attempts; cloning was stopped. The destination did not receive a partition table and is not usable."
            ));
        }
        StopReason::SourceLost(m) | StopReason::DestinationFailed(m) | StopReason::Internal(m) => {
            status = FinalStatus::Failed;
            reasons.push(m.clone());
        }
        StopReason::Completed => {
            let mut failed = false;
            let mut incomplete = false;
            let mut warn = false;

            let zero: u64 = o.bad_ranges.iter().filter(|b| b.outcome == BadOutcome::ZeroFilled).map(|b| b.sectors).sum();
            if zero > 0 {
                incomplete = true;
                reasons.push(tf!(
                    "{zero} secteur(s) illisible(s) de la source ont été remplacés par des zéros : les fichiers ou structures \
                     qui les utilisaient sont corrompus sur le clone.",
                    "{zero} unreadable source sector(s) were replaced with zeros: the files or structures that used them are corrupted on the clone."
                ));
            }
            if !o.table_check.performed || !o.table_check.ok {
                failed = true;
                reasons.push(tf!(
                    "La table de partitions relue sur la destination n'est pas conforme : {}.",
                    "The partition table read back from the destination does not conform: {}.",
                    if o.table_check.problems.is_empty() {
                        t!("contrôle non effectué", "check not performed").to_string()
                    } else {
                        o.table_check.problems.join(" ; ")
                    }
                ));
            }
            let dst_bad = o
                .data_check
                .mismatches
                .iter()
                .filter(|m| matches!(m.kind, MismatchKind::DestinationDiffers | MismatchKind::DestinationUnreadable))
                .count();
            let src_bad = o.data_check.mismatches.len() - dst_bad;
            if dst_bad > 0 {
                failed = true;
                reasons.push(tf!("La vérification a trouvé {dst_bad} plage(s) où la destination ne restitue pas les données écrites : \
                     la destination (ou sa connexion) n'est pas fiable.", "Verification found {dst_bad} range(s) where the destination does not return the written data: the destination (or its connection) is not reliable."
                ));
            }
            if src_bad > 0 {
                warn = true;
                reasons.push(tf!("À la relecture, {src_bad} plage(s) de la source ont donné un contenu différent ou illisible : la source est \
                     instable ou a été modifiée pendant le clonage. Le clone correspond à la première lecture.", "On re-read, {src_bad} source range(s) returned different or unreadable content: the source is unstable or was modified during cloning. The clone matches the first read."
                ));
            }
            match opts.verify {
                VerifyMode::None => {
                    warn = true;
                    reasons.push(
                        t!(
                            "La relecture des données copiées a été désactivée : le contenu du clone n'a pas été vérifié.",
                            "Read-back of the copied data was disabled: the clone's content was not verified."
                        )
                        .to_string(),
                    );
                }
                _ if !o.data_check.completed => {
                    warn = true;
                    reasons.push(
                        t!("La vérification des données a été interrompue avant la fin.", "Data verification was interrupted before the end.")
                            .to_string(),
                    );
                }
                _ => {}
            }
            let s_err = o.structure_findings.iter().filter(|f| f.severity >= Severity::Error).count();
            if s_err > 0 {
                failed = true;
                reasons.push(tf!(
                    "{s_err} système(s) de fichiers du clone ne correspondent pas à la source (voir le détail).",
                    "{s_err} clone file system(s) do not match the source (see details)."
                ));
            } else if !o.structure_findings.is_empty() {
                warn = true;
                reasons.push(
                    t!("Le contrôle des systèmes de fichiers du clone a émis des avertissements.", "The clone file system check raised warnings.")
                        .to_string(),
                );
            }
            let b_err = o.boot_findings.iter().filter(|f| f.severity >= Severity::Error).count();
            if b_err > 0 {
                failed = true;
                reasons.push(tf!(
                    "{b_err} élément(s) de démarrage présents sur la source sont introuvables sur le clone.",
                    "{b_err} boot element(s) present on the source cannot be found on the clone."
                ));
            } else if !o.boot_findings.is_empty() {
                warn = true;
                reasons.push(
                    t!("Les éléments de démarrage du clone diffèrent de ceux de la source.", "The clone's boot elements differ from the source's.")
                        .to_string(),
                );
            }
            if o.copy.recovered_bytes > 0 {
                warn = true;
                reasons.push(tf!(
                    "{} octets de la source n'ont pu être lus qu'après plusieurs tentatives : le disque source montre des signes de faiblesse.",
                    "{} bytes of the source could only be read after several attempts: the source disk shows signs of weakness.",
                    o.copy.recovered_bytes
                ));
            }
            // Pre-existing defects of the source: the clone reproduces them faithfully.
            let inherited: Vec<&Finding> = src
                .layout
                .findings
                .iter()
                .chain(src.parts.iter().flat_map(|p| p.fs.findings.iter()))
                .filter(|f| f.severity >= Severity::Warning)
                .collect();
            if !inherited.is_empty() {
                warn = true;
                reasons.push(tf!(
                    "La source présentait déjà {} anomalie(s) ({}), reproduite(s) à l'identique sur le clone.",
                    "The source already had {} anomaly(ies) ({}), reproduced identically on the clone.",
                    inherited.len(),
                    inherited.iter().map(|f| f.title.as_str()).collect::<Vec<_>>().join(" ; ")
                ));
            }
            if let Some(b) = &o.boot_source {
                let missing = b.findings.iter().filter(|f| f.code == "BOOT_ELEMENT_MISSING").count();
                if missing > 0 {
                    warn = true;
                    reasons.push(tf!("{missing} élément(s) de démarrage manquaient déjà sur la source : le clone ne démarrera vraisemblablement pas mieux qu'elle.", "{missing} boot element(s) were already missing on the source: the clone will most likely not boot any better."
                    ));
                }
            }
            if let Some(g) = o.grow.as_ref().filter(|g| g.attempted && !g.done) {
                let e = g.error.clone().unwrap_or_default();
                if g.modified {
                    failed = true;
                    reasons.push(tf!("L'agrandissement de la partition {} a échoué après avoir commencé à modifier le volume ({e}). Le volume est \
                         marqué « à vérifier » : lancez « chkdsk /f » dessus avant de l'utiliser.", "Growing partition {} failed after it had started modifying the volume ({e}). The volume is flagged \"needs checking\": run \"chkdsk /f\" on it before using it.",
                        g.number
                    ));
                } else {
                    warn = true;
                    reasons.push(tf!("La partition {} n'a pas pu être agrandie ({e}). Rien n'a été modifié : le volume garde sa taille d'origine \
                         dans une partition plus grande ; « diskpart » → « extend filesystem » permettra d'utiliser l'espace ajouté.", "Partition {} could not be grown ({e}). Nothing was changed: the volume keeps its original size inside a larger partition; \"diskpart\" → \"extend filesystem\" will let you use the added space.",
                        g.number
                    ));
                }
            }
            if plan.mode == CloneMode::RawDisk && plan.table == TableKind::None {
                warn = true;
                reasons.push(
                    t!(
                        "Source sans table de partitions reconnue : seule l'égalité des octets copiés a pu être contrôlée.",
                        "Source without a recognised partition table: only the equality of the copied bytes could be checked."
                    )
                    .to_string(),
                );
            }

            status = if failed {
                FinalStatus::Failed
            } else if incomplete {
                FinalStatus::Incomplete
            } else if warn {
                FinalStatus::CompletedWithWarnings
            } else {
                reasons.push(tf!("Toutes les données prévues ont été copiées sans erreur, puis vérifiées ({}) sans aucune différence. \
                     La table de partitions et les systèmes de fichiers du clone correspondent à la source{}.", "All planned data was copied without errors, then verified ({}) with no differences at all. The clone's partition table and file systems match the source{}.",
                    opts.verify.label(),
                    if o.grow.as_ref().is_some_and(|g| g.done) { t!(" (hors agrandissement indiqué ci-dessous)", " (apart from the growth noted below)") } else { "" }
                ));
                FinalStatus::SuccessVerified
            };
            if let Some(GrowOutcome { done: true, summary: Some(s), number, .. }) = &o.grow {
                reasons.push(tf!(
                    "La partition {number} a été agrandie de {} à {} ; Windows vérifiera ce volume (chkdsk) une fois, au premier \
                     démarrage du clone.",
                    "Partition {number} was grown from {} to {}; Windows will check this volume (chkdsk) once, on the clone's first boot.",
                    crate::diag::human_size(s.old_size),
                    crate::diag::human_size(s.new_size)
                ));
            }
        }
    }
    o.status = status;
    o.reasons = reasons;
}
