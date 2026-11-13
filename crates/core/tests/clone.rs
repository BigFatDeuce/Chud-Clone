//! Tests d'intégration du moteur de clonage sur des images disque réelles (en mémoire).
//!
//! Chaque scénario exécute le clonage complet (plan → copie → table → vérifications)
//! et contrôle à la fois le bilan annoncé et le contenu réel de la destination.

mod common;

use chud_clone_core::clone::{CloneOutcome, CloneRequest, FinalStatus, run_clone};
use chud_clone_core::device::{BlockDevice, IoErrorClass, MemDevice, fault};
use chud_clone_core::engine::{
    BadOutcome, Control, Decision, EngineOptions, LogLevel, MismatchKind, Observer, Progress, StopReason, UnreadableInfo, UnreadablePolicy,
    VerifyMode,
};
use chud_clone_core::fs;
use chud_clone_core::partition::gpt::{GptEntrySpec, GptSpec, build_gpt};
use chud_clone_core::partition::{MIB, PartInfo, PartRole, TableKind, mbr, read_layout};
use chud_clone_core::plan::{CloneMode, ClonePlan, CopyMethod, DiskAnalysis, LayoutStrategy, PlanOptions, analyze_disk, build_plan};
use chud_clone_core::testing::{FaultPlan, FaultyDevice};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

const SS: u64 = 512;

/// Les scénarios manipulent des images de plusieurs centaines de Mio : un seul à la fois.
static HEAVY: Mutex<()> = Mutex::new(());
fn heavy() -> MutexGuard<'static, ()> {
    HEAVY.lock().unwrap_or_else(|e| e.into_inner())
}

fn gpt_image() -> &'static [u8] {
    static IMG: OnceLock<Vec<u8>> = OnceLock::new();
    IMG.get_or_init(|| common::load_fixture("gpt-win.img"))
}
fn mbr_image() -> &'static [u8] {
    static IMG: OnceLock<Vec<u8>> = OnceLock::new();
    IMG.get_or_init(|| common::load_fixture("mbr-win.img"))
}

/// Source en lecture seule sur une image partagée ; compte les tentatives d'écriture.
struct SliceDev {
    data: &'static [u8],
    write_attempts: u64,
}
impl SliceDev {
    fn new(data: &'static [u8]) -> Self {
        Self { data, write_attempts: 0 }
    }
}
impl BlockDevice for SliceDev {
    fn size(&self) -> u64 {
        self.data.len() as u64
    }
    fn sector_size(&self) -> u32 {
        512
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        chud_clone_core::device::validate_io(self, offset, buf.len())?;
        buf.copy_from_slice(&self.data[offset as usize..offset as usize + buf.len()]);
        Ok(())
    }
    fn write_at(&mut self, _offset: u64, _buf: &[u8]) -> std::io::Result<()> {
        self.write_attempts += 1;
        Err(fault(IoErrorClass::WriteProtected))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn label(&self) -> String {
        "image de test".into()
    }
}

#[derive(Default)]
struct Recorder {
    logs: Mutex<Vec<(LogLevel, String)>>,
    last: Mutex<Option<Progress>>,
    asked: Mutex<Option<UnreadableInfo>>,
    answer: Option<Decision>,
    cancel_after: Option<u64>,
    ctl: Option<&'static Control>,
    progress_calls: AtomicU64,
}
impl Observer for Recorder {
    fn progress(&self, p: &Progress) {
        self.progress_calls.fetch_add(1, Ordering::Relaxed);
        if let (Some(limit), Some(ctl)) = (self.cancel_after, self.ctl)
            && p.done >= limit
        {
            ctl.cancel();
        }
        *self.last.lock().unwrap() = Some(p.clone());
    }
    fn log(&self, level: LogLevel, msg: &str) {
        self.logs.lock().unwrap().push((level, msg.to_string()));
    }
    fn decide_unreadable(&self, info: &UnreadableInfo) -> Decision {
        *self.asked.lock().unwrap() = Some(info.clone());
        self.answer.unwrap_or(Decision::Abort)
    }
}

fn opts() -> EngineOptions {
    EngineOptions { chunk_size: 256 * 1024, retry_block: 16 * 1024, ..Default::default() }
}

struct Run {
    outcome: CloneOutcome,
    plan: ClonePlan,
    analysis: DiskAnalysis,
    dst: Vec<u8>,
    rec: Recorder,
}

/// Exécute un clonage complet de `src` vers une destination en mémoire.
fn run_with(
    src_img: &'static [u8],
    dst_size: u64,
    popts: PlanOptions,
    eopts: EngineOptions,
    src_faults: FaultPlan,
    dst_faults: FaultPlan,
    rec: Recorder,
) -> Run {
    let mut src = FaultyDevice::new(SliceDev::new(src_img), FaultPlan::default());
    let analysis = analyze_disk(&mut src).unwrap();
    let plan = build_plan(&analysis, dst_size, 512, &popts);
    src.plan = src_faults;
    // Destination pré-remplie de déchets : rien ne doit dépendre de son ancien contenu.
    let mut dst = FaultyDevice::new(MemDevice::from_vec(vec![0xA5u8; dst_size as usize], 512), dst_faults);
    let ctl: &'static Control = rec.ctl.unwrap_or_else(|| Box::leak(Box::new(Control::new())));
    let outcome = run_clone(CloneRequest { src: &mut src, dst: &mut dst, src_analysis: &analysis, plan: &plan, opts: &eopts, ctl, obs: &rec });
    assert_eq!(src.inner.write_attempts, 0, "aucune écriture ne doit être tentée sur la source");
    Run { outcome, plan, analysis, dst: dst.inner.data, rec }
}

fn run(src_img: &'static [u8], dst_size: u64, popts: PlanOptions) -> Run {
    run_with(src_img, dst_size, popts, opts(), FaultPlan::default(), FaultPlan::default(), Recorder::default())
}

/// Toutes les plages planifiées doivent être identiques entre la source et la destination.
fn assert_tasks_equal(r: &Run, src: &[u8], except: &[(u64, u64)]) {
    for t in r.plan.tasks.iter().chain(r.plan.late_tasks.iter()) {
        let mut off = 0u64;
        while off < t.len {
            let n = 4096.min(t.len - off);
            let (s, d) = ((t.src + off) as usize, (t.dst + off) as usize);
            let skip = except.iter().any(|(eo, el)| t.dst + off < eo + el && *eo < t.dst + off + n);
            // En copie brute du disque entier, les secteurs de table sont réécrits ensuite.
            if !skip && src[s..s + n as usize] != r.dst[d..d + n as usize] {
                panic!("différence source/destination à l'octet source {s} (destination {d})");
            }
            off += n;
        }
    }
}

/// Tous les fichiers de chaque partition NTFS/FAT du clone sont identiques à la source.
fn assert_same_files(r: &Run, src_img: &'static [u8]) {
    let mut s = SliceDev::new(src_img);
    let mut d = MemDevice::from_vec(r.dst.clone(), 512);
    let mut checked = 0;
    for pp in &r.plan.partitions {
        if pp.fs != fs::FsKind::Ntfs {
            continue;
        }
        let files = fs::ntfs::list_files(&mut s, pp.src_start, pp.src_len).unwrap();
        let files_d = fs::ntfs::list_files(&mut d, pp.dst_start, pp.dst_len).unwrap();
        assert_eq!(files, files_d, "liste des fichiers de la partition {}", pp.number);
        for (path, _) in &files {
            let comps: Vec<&str> = path.split('/').collect();
            let a = fs::ntfs::read_file(&mut s, pp.src_start, pp.src_len, &comps, usize::MAX).unwrap().unwrap();
            let b = fs::ntfs::read_file(&mut d, pp.dst_start, pp.dst_len, &comps, usize::MAX).unwrap().unwrap();
            assert!(a == b, "partition {} : contenu différent pour {path}", pp.number);
            checked += 1;
        }
    }
    assert!(checked > 0);
}

fn no_table(dst: &[u8]) -> bool {
    dst[..1024].iter().all(|b| *b == 0)
}

fn dump_reasons(o: &CloneOutcome) -> String {
    format!(
        "{:?} / {:?} / {:#?} / table={:?} / struct={:#?} / boot={:#?}",
        o.status, o.stop, o.reasons, o.table_check, o.structure_findings, o.boot_findings
    )
}

// ---------------------------------------------------------------------------
// Scénarios nominaux
// ---------------------------------------------------------------------------

#[test]
fn gpt_same_size_smart_clone_is_verified() {
    let _g = heavy();
    let src = gpt_image();
    let r = run(src, src.len() as u64, PlanOptions::default());
    assert_eq!(r.outcome.status, FinalStatus::SuccessVerified, "{}", dump_reasons(&r.outcome));
    assert_eq!(r.plan.strategy, LayoutStrategy::Identical);
    assert!(r.outcome.table_written && r.outcome.table_check.ok);
    assert!(r.outcome.data_check.passed());
    assert_eq!(r.outcome.copy.bytes_copied, r.plan.total_copy_bytes);
    // Le clonage intelligent copie nettement moins que la taille du disque.
    assert!(r.plan.total_copy_bytes < src.len() as u64 * 6 / 10, "copié : {}", r.plan.total_copy_bytes);
    let methods: Vec<CopyMethod> = r.plan.partitions.iter().map(|p| p.method).collect();
    assert_eq!(methods, vec![CopyMethod::Smart, CopyMethod::Skip, CopyMethod::Smart, CopyMethod::Smart]);

    assert_tasks_equal(&r, src, &[]);
    assert_same_files(&r, src);

    // Identité préservée : mêmes GUID de disque et de partitions.
    let mut d = MemDevice::from_vec(r.dst.clone(), 512);
    let l = read_layout(&mut d).unwrap();
    assert!(l.findings.is_empty(), "{:#?}", l.findings);
    assert_eq!(l.gpt.as_ref().unwrap().disk_guid.to_string(), "5A5A5A5A-1111-2222-3333-0123456789AB");
    let mut s = SliceDev::new(src);
    let ls = read_layout(&mut s).unwrap();
    assert_eq!(l.partitions, ls.partitions);
    // Fichiers de démarrage présents sur l'ESP du clone.
    let esp = &l.partitions[0];
    assert!(fs::fat::file_size(&mut d, esp.start_lba * SS, esp.sectors * SS, "EFI/Microsoft/Boot/BCD").unwrap().is_some());
    let boot = r.outcome.boot_destination.as_ref().unwrap();
    assert_eq!(boot.style, chud_clone_core::boot::BootStyle::WindowsUefi);
    assert!(boot.findings.is_empty(), "{:#?}", boot.findings);
    assert_eq!(boot.windows_partition, Some(3));
    // La MSR n'est pas copiée : la destination y garde son ancien contenu.
    let msr = &l.partitions[1];
    let o = (msr.start_lba * SS + 4096) as usize;
    assert!(r.dst[o..o + 4096].iter().all(|b| *b == 0xA5));
}

#[test]
fn gpt_larger_destination_moves_trailing_partitions() {
    let _g = heavy();
    let src = gpt_image();
    let dst_size = 320 * MIB;
    let r = run(src, dst_size, PlanOptions { grow_main: false, ..Default::default() });
    assert_eq!(r.outcome.status, FinalStatus::SuccessVerified, "{}", dump_reasons(&r.outcome));
    assert_eq!(r.plan.strategy, LayoutStrategy::TrailingMoved);
    assert!(r.plan.grow.is_none());
    assert!(r.plan.findings.iter().any(|f| f.code == "GROW_NOT_AUTOMATIC" && f.detail.contains("désactivé")));
    assert_eq!(r.plan.main_partition, Some(3));
    assert!(r.plan.free_after_main > 100 * MIB, "espace libre contigu : {}", r.plan.free_after_main);
    assert!(r.plan.findings.iter().any(|f| f.code == "LAYOUT_TRAILING_MOVED"));
    assert!(r.plan.findings.iter().any(|f| f.code == "RECOVERY_MOVED"));

    let mut d = MemDevice::from_vec(r.dst.clone(), 512);
    let l = read_layout(&mut d).unwrap();
    assert!(l.findings.is_empty(), "{:#?}", l.findings);
    let gpt = l.gpt.as_ref().unwrap();
    assert!(gpt.backup_at_end);
    let rec = l.partitions.iter().find(|p| p.role() == PartRole::Recovery).unwrap();
    // La récupération est collée à la fin, alignée sur 1 Mio.
    assert_eq!(rec.start_lba % 2048, 0);
    assert!(gpt.last_usable_lba + 1 - rec.end_lba() < 2048);
    // Les trois premières partitions n'ont pas bougé.
    for i in 0..3 {
        assert!(!r.plan.partitions[i].moved);
    }
    assert!(r.plan.partitions[3].moved);
    assert_tasks_equal(&r, src, &[]);
    assert_same_files(&r, src);
}

#[test]
fn gpt_larger_destination_without_layout_optimisation_stays_identical() {
    let _g = heavy();
    let src = gpt_image();
    let r = run(src, 256 * MIB, PlanOptions { mode: CloneMode::Smart, optimize_layout: false, grow_main: true });
    assert_eq!(r.outcome.status, FinalStatus::SuccessVerified, "{}", dump_reasons(&r.outcome));
    assert_eq!(r.plan.strategy, LayoutStrategy::Identical);
    assert!(r.plan.partitions.iter().all(|p| !p.moved));
    assert!(r.plan.findings.iter().any(|f| f.code == "DEST_LARGER_UNUSED"));
    assert_same_files(&r, src);
}

/// Source avec un trou de 64 Mio avant la partition de récupération, obtenue en
/// déplaçant celle-ci dans une copie agrandie de l'image de test.
fn gpt_image_with_gap() -> &'static [u8] {
    static IMG: OnceLock<Vec<u8>> = OnceLock::new();
    IMG.get_or_init(|| {
        let base = gpt_image();
        let mut dev = MemDevice::from_vec(base.to_vec(), 512);
        let l = read_layout(&mut dev).unwrap();
        let meta = l.gpt.clone().unwrap();
        let new_size = 256 * MIB;
        let mut img = base.to_vec();
        img.resize(new_size as usize, 0);
        let rec = l.partitions[3].clone();
        let new_start = rec.start_lba + 64 * 2048;
        let (o, n) = ((rec.start_lba * SS) as usize, (rec.sectors * SS) as usize);
        let content = img[o..o + n].to_vec();
        img[o..o + n].fill(0);
        let no = (new_start * SS) as usize;
        img[no..no + n].copy_from_slice(&content);
        let entries = l
            .partitions
            .iter()
            .map(|p| match &p.info {
                PartInfo::Gpt { type_guid, unique_guid, attributes, name, slot } => {
                    let first = if p.number == 4 { new_start } else { p.start_lba };
                    GptEntrySpec {
                        slot: *slot,
                        type_guid: *type_guid,
                        unique_guid: *unique_guid,
                        first_lba: first,
                        last_lba: first + p.sectors - 1,
                        attributes: *attributes,
                        name: name.clone(),
                    }
                }
                _ => unreachable!(),
            })
            .collect();
        let spec = GptSpec {
            sector_size: 512,
            disk_sectors: new_size / SS,
            disk_guid: meta.disk_guid,
            num_entries: meta.num_entries,
            entry_size: meta.entry_size,
            entries,
        };
        let g = build_gpt(&spec).unwrap();
        let pm = mbr::build_protective(None, 512, new_size / SS);
        // Efface l'ancienne table de secours puis installe la nouvelle.
        let old_backup = base.len() - 33 * 512;
        img[old_backup..base.len()].fill(0);
        img[..512].copy_from_slice(&pm);
        img[512..512 + g.primary.len()].copy_from_slice(&g.primary);
        let bo = (g.backup_lba * SS) as usize;
        img[bo..bo + g.backup.len()].copy_from_slice(&g.backup);
        img
    })
}

#[test]
fn gpt_smaller_destination_compacts_partitions() {
    let _g = heavy();
    let src = gpt_image_with_gap();
    // Vérifie d'abord que la source fabriquée est saine.
    let mut s = SliceDev::new(src);
    let a = analyze_disk(&mut s).unwrap();
    assert!(a.all_findings().is_empty(), "{:#?}", a.all_findings());
    assert_eq!(a.parts[3].part.start_lba, (1 + 40 + 16 + 96 + 64) * 2048);

    // 190 Mio : trop petit pour les positions d'origine (fin à 249 Mio), assez pour 185 Mio resserrés.
    let r = run(src, 190 * MIB, PlanOptions::default());
    assert_eq!(r.plan.strategy, LayoutStrategy::Compacted, "{:#?}", r.plan.findings);
    assert_eq!(r.outcome.status, FinalStatus::SuccessVerified, "{}", dump_reasons(&r.outcome));
    assert!(r.plan.partitions[3].moved);
    assert_eq!(r.plan.partitions[3].dst_start, (1 + 40 + 16 + 96) * MIB);
    assert_tasks_equal(&r, src, &[]);
    assert_same_files(&r, src);
    let mut d = MemDevice::from_vec(r.dst.clone(), 512);
    let l = read_layout(&mut d).unwrap();
    assert!(l.findings.is_empty(), "{:#?}", l.findings);
}

#[test]
fn destination_too_small_is_refused_before_any_write() {
    let _g = heavy();
    for (src, size) in [(gpt_image(), 150 * MIB), (mbr_image(), 100 * MIB)] {
        let r = run(src, size, PlanOptions::default());
        assert!(r.plan.is_blocked());
        let f = r.plan.findings.iter().find(|f| f.code == "DEST_TOO_SMALL").expect("constat attendu");
        assert!(f.blocking);
        assert!(f.detail.contains("Gestion des disques") || f.detail.contains("plus grande"), "{}", f.detail);
        assert_eq!(r.outcome.status, FinalStatus::Failed);
        assert!(matches!(r.outcome.stop, StopReason::Internal(_)));
        assert!(r.dst.iter().all(|b| *b == 0xA5), "la destination ne doit pas avoir été touchée");
    }
    // La copie brute exige une destination au moins aussi grande.
    let r = run(gpt_image(), 191 * MIB, PlanOptions { mode: CloneMode::RawDisk, optimize_layout: true, grow_main: true });
    assert!(r.plan.is_blocked());
    assert!(r.dst.iter().all(|b| *b == 0xA5));
}

#[test]
fn mbr_smart_clone_preserves_positions_boot_track_and_logicals() {
    let _g = heavy();
    let src = mbr_image();
    for dst_size in [src.len() as u64, 200 * MIB] {
        let r = run(src, dst_size, PlanOptions::default());
        assert_eq!(r.outcome.status, FinalStatus::SuccessVerified, "{}", dump_reasons(&r.outcome));
        assert_eq!(r.plan.table, TableKind::Mbr);
        assert!(r.plan.partitions.iter().all(|p| !p.moved && p.dst_start == p.src_start));
        let methods: Vec<CopyMethod> = r.plan.partitions.iter().map(|p| p.method).collect();
        assert_eq!(methods, vec![CopyMethod::Smart, CopyMethod::Smart, CopyMethod::Smart, CopyMethod::Raw]);
        // Secteur 0 (code d'amorçage, signature, table) et piste d'amorçage identiques.
        assert_eq!(&r.dst[..63 * 512], &src[..63 * 512]);
        let mut d = MemDevice::from_vec(r.dst.clone(), 512);
        let l = read_layout(&mut d).unwrap();
        assert!(l.findings.is_empty(), "{:#?}", l.findings);
        assert_eq!(l.mbr.as_ref().unwrap().signature, 0x1BADF00D);
        assert_eq!(l.partitions.len(), 4);
        // Partition brute (type Linux, contenu inconnu) : identique octet pour octet.
        let p6 = &l.partitions[3];
        let (o, n) = ((p6.start_lba * SS) as usize, (p6.sectors * SS) as usize);
        assert!(src[o..o + n] == r.dst[o..o + n]);
        // FAT16 logique : fichiers lisibles sur le clone.
        let p5 = &l.partitions[2];
        let a = fs::fat::read_file(&mut d, p5.start_lba * SS, p5.sectors * SS, "big.bin").unwrap().unwrap();
        let mut s = SliceDev::new(src);
        let b = fs::fat::read_file(&mut s, p5.start_lba * SS, p5.sectors * SS, "big.bin").unwrap().unwrap();
        assert!(a == b && a.len() == 1900 * 1024);
        assert_same_files(&r, src);
        let boot = r.outcome.boot_destination.as_ref().unwrap();
        assert_eq!(boot.style, chud_clone_core::boot::BootStyle::WindowsBios);
        assert!(boot.findings.is_empty(), "{:#?}", boot.findings);
        if dst_size > src.len() as u64 {
            assert!(r.plan.findings.iter().any(|f| f.code == "DEST_LARGER_UNUSED"));
        }
    }
}

#[test]
fn raw_partitions_mode_copies_every_sector() {
    let _g = heavy();
    let src = gpt_image();
    let r = run(src, src.len() as u64, PlanOptions { mode: CloneMode::RawPartitions, optimize_layout: true, grow_main: true });
    assert_eq!(r.outcome.status, FinalStatus::SuccessVerified, "{}", dump_reasons(&r.outcome));
    for pp in &r.plan.partitions {
        if pp.role == PartRole::Msr {
            continue;
        }
        assert_eq!(pp.method, CopyMethod::Raw);
        let (o, n) = (pp.src_start as usize, pp.src_len as usize);
        assert!(src[o..o + n] == r.dst[o..o + n], "partition {} identique", pp.number);
    }
}

#[test]
fn raw_disk_mode_reproduces_the_disk_and_relocates_backup_gpt() {
    let _g = heavy();
    let src = gpt_image();
    // Même taille : copie strictement identique.
    let r = run(src, src.len() as u64, PlanOptions { mode: CloneMode::RawDisk, optimize_layout: true, grow_main: true });
    assert_eq!(r.outcome.status, FinalStatus::SuccessVerified, "{}", dump_reasons(&r.outcome));
    assert!(r.dst == src, "copie brute de même taille : identique à l'octet près");

    // Plus grand : tout est identique sauf la table GPT, réajustée à la nouvelle taille.
    let r = run(src, 256 * MIB, PlanOptions { mode: CloneMode::RawDisk, optimize_layout: true, grow_main: true });
    assert_eq!(r.outcome.status, FinalStatus::SuccessVerified, "{}", dump_reasons(&r.outcome));
    assert!(r.plan.findings.iter().any(|f| f.code == "GPT_BACKUP_RELOCATED"));
    let body = 34 * 512..src.len() - 33 * 512;
    assert!(r.dst[body.clone()] == src[body]);
    let mut d = MemDevice::from_vec(r.dst.clone(), 512);
    let l = read_layout(&mut d).unwrap();
    assert!(l.findings.is_empty(), "{:#?}", l.findings);
    assert!(l.gpt.as_ref().unwrap().backup_at_end);
    let mut s = SliceDev::new(src);
    assert_eq!(l.partitions, read_layout(&mut s).unwrap().partitions);
}

#[test]
fn disk_without_partition_table_is_cloned_raw_with_reservation() {
    let _g = heavy();
    static IMG: OnceLock<Vec<u8>> = OnceLock::new();
    let src: &'static [u8] = IMG.get_or_init(|| {
        let mut v = vec![0u8; 8 * MIB as usize];
        for (i, b) in v.iter_mut().enumerate().skip(4096) {
            *b = (i % 253) as u8;
        }
        v
    });
    let r = run(src, 10 * MIB, PlanOptions::default());
    assert_eq!(r.plan.mode, CloneMode::RawDisk);
    assert!(r.plan.findings.iter().any(|f| f.code == "MODE_FORCED_RAW_DISK"));
    // Octets identiques, mais jamais « réussi et vérifié » : aucune structure n'a pu être contrôlée.
    assert_eq!(r.outcome.status, FinalStatus::CompletedWithWarnings, "{}", dump_reasons(&r.outcome));
    assert!(r.dst[..src.len()] == *src);
}

#[test]
fn dirty_ntfs_partition_falls_back_to_raw_copy_and_is_flagged() {
    let _g = heavy();
    static IMG: OnceLock<Vec<u8>> = OnceLock::new();
    let src: &'static [u8] = IMG.get_or_init(|| {
        let mut img = gpt_image().to_vec();
        let dirty = common::load_fixture("ntfs-dirty.img");
        let o = ((1 + 40 + 16) * MIB) as usize;
        img[o..o + dirty.len()].copy_from_slice(&dirty);
        img
    });
    let r = run(src, src.len() as u64, PlanOptions::default());
    let p3 = &r.plan.partitions[2];
    assert_eq!(p3.method, CopyMethod::Raw);
    assert!(p3.method_reason.as_deref().unwrap().contains("dirty"), "{:?}", p3.method_reason);
    assert_eq!(p3.copy_bytes, p3.src_len);
    // Copie fidèle, mais la source avait un défaut : jamais « réussi » sans réserve.
    assert_eq!(r.outcome.status, FinalStatus::CompletedWithWarnings, "{}", dump_reasons(&r.outcome));
    assert!(r.outcome.reasons.iter().any(|x| x.contains("à vérifier")), "{:#?}", r.outcome.reasons);
    let (o, n) = (p3.src_start as usize, p3.src_len as usize);
    assert!(src[o..o + n] == r.dst[o..o + n]);
}

// ---------------------------------------------------------------------------
// Secteurs illisibles
// ---------------------------------------------------------------------------

/// Choisit une plage de 3 secteurs au milieu d'une grande étendue utilisée de la
/// partition Windows.
fn pick_bad_range(src: &'static [u8]) -> (u64, u64) {
    let mut s = SliceDev::new(src);
    let a = analyze_disk(&mut s).unwrap();
    let plan = build_plan(&a, src.len() as u64, 512, &PlanOptions::default());
    let p3 = &plan.partitions[2];
    let t = plan.tasks.iter().find(|t| t.src >= p3.src_start && t.len >= 2 * MIB).expect("grande étendue");
    (t.src + MIB + 7 * 512, 3 * 512)
}

#[test]
fn unreadable_sectors_stop_the_clone_by_default() {
    let _g = heavy();
    let src = gpt_image();
    let (bad_off, bad_len) = pick_bad_range(src);
    let r = run_with(
        src,
        src.len() as u64,
        PlanOptions::default(),
        opts(),
        FaultPlan { bad_read: vec![(bad_off, bad_len)], ..Default::default() },
        FaultPlan::default(),
        Recorder::default(),
    );
    assert_eq!(r.outcome.status, FinalStatus::Incomplete, "{}", dump_reasons(&r.outcome));
    assert_eq!(r.outcome.stop, StopReason::Unreadable);
    assert_eq!(r.outcome.bad_ranges.len(), 1, "{:#?}", r.outcome.bad_ranges);
    let b = &r.outcome.bad_ranges[0];
    assert_eq!((b.lba, b.sectors), (bad_off / SS, 3), "position exacte au secteur près");
    assert_eq!(b.outcome, BadOutcome::NotCopied);
    assert_eq!(b.class, IoErrorClass::Media);
    assert_eq!(b.attempts, opts().read_retries + 2);
    assert!(!r.outcome.table_written);
    assert!(no_table(&r.dst), "aucune table de partitions ne doit exister sur une destination incomplète");
    assert!(r.rec.asked.lock().unwrap().is_none(), "politique Stop : aucune question posée");
    // Le reste des données saines a tout de même été copié avant l'arrêt.
    assert!(r.outcome.copy.bytes_copied >= r.plan.total_copy_bytes - 16 * 1024);
    assert!(r.rec.logs.lock().unwrap().iter().any(|(l, m)| *l == LogLevel::Error && m.contains(&format!("LBA {}", bad_off / SS))));
}

#[test]
fn zero_fill_policy_completes_but_reports_incomplete_clone() {
    let _g = heavy();
    let src = gpt_image();
    let (bad_off, bad_len) = pick_bad_range(src);
    let mut e = opts();
    e.unreadable = UnreadablePolicy::ZeroFill;
    let r = run_with(
        src,
        src.len() as u64,
        PlanOptions::default(),
        e,
        FaultPlan { bad_read: vec![(bad_off, bad_len)], ..Default::default() },
        FaultPlan::default(),
        Recorder::default(),
    );
    assert_eq!(r.outcome.stop, StopReason::Completed);
    // Jamais « réussi » avec des données manquantes, même si tout le reste est vérifié.
    assert_eq!(r.outcome.status, FinalStatus::Incomplete, "{}", dump_reasons(&r.outcome));
    assert!(r.outcome.table_written && r.outcome.table_check.ok);
    assert_eq!(r.outcome.bad_ranges.len(), 1);
    assert_eq!(r.outcome.bad_ranges[0].outcome, BadOutcome::ZeroFilled);
    assert_eq!(r.outcome.copy.bytes_zero_filled, bad_len);
    let o = bad_off as usize;
    assert!(r.dst[o..o + bad_len as usize].iter().all(|b| *b == 0), "secteurs illisibles remplacés par des zéros");
    assert_tasks_equal(&r, src, &[(bad_off, bad_len)]);
    assert!(r.outcome.reasons.iter().any(|x| x.contains("remplacés par des zéros")));
}

#[test]
fn ask_policy_consults_the_user_with_exact_positions() {
    let _g = heavy();
    let src = gpt_image();
    let (bad_off, bad_len) = pick_bad_range(src);
    for answer in [Decision::Abort, Decision::ZeroFill] {
        let mut e = opts();
        e.unreadable = UnreadablePolicy::Ask;
        let rec = Recorder { answer: Some(answer), ..Default::default() };
        let r = run_with(
            src,
            src.len() as u64,
            PlanOptions::default(),
            e,
            FaultPlan { bad_read: vec![(bad_off, bad_len), (bad_off + 40 * 512, 512)], ..Default::default() },
            FaultPlan::default(),
            rec,
        );
        let asked = r.rec.asked.lock().unwrap().clone().expect("la décision doit être demandée");
        assert_eq!(asked.total_sectors, 4);
        assert_eq!(asked.ranges.len(), 2);
        assert_eq!(asked.ranges[0].lba, bad_off / SS);
        assert_eq!(asked.ranges[1].lba, bad_off / SS + 40);
        match answer {
            Decision::Abort => {
                assert_eq!(r.outcome.stop, StopReason::Unreadable);
                assert!(no_table(&r.dst));
            }
            Decision::ZeroFill => {
                assert_eq!(r.outcome.stop, StopReason::Completed);
                assert_eq!(r.outcome.status, FinalStatus::Incomplete);
            }
        }
    }
}

#[test]
fn intermittent_read_errors_are_recovered_and_reported_as_warning() {
    let _g = heavy();
    let src = gpt_image();
    let (bad_off, bad_len) = pick_bad_range(src);
    // Échoue 3 fois (bloc, sous-bloc, 1re relecture) puis réussit.
    let r = run_with(
        src,
        src.len() as u64,
        PlanOptions::default(),
        opts(),
        FaultPlan { flaky_read: vec![(bad_off, bad_len, 3)], ..Default::default() },
        FaultPlan::default(),
        Recorder::default(),
    );
    assert_eq!(r.outcome.stop, StopReason::Completed);
    assert!(r.outcome.bad_ranges.is_empty(), "{:#?}", r.outcome.bad_ranges);
    assert!(r.outcome.copy.recovered_bytes > 0);
    assert!(!r.outcome.recovered.is_empty());
    assert!(r.outcome.copy.read_error_events >= 2);
    // Données complètes et vérifiées, mais la source a montré une faiblesse.
    assert_eq!(r.outcome.status, FinalStatus::CompletedWithWarnings, "{}", dump_reasons(&r.outcome));
    assert!(r.outcome.data_check.passed());
    assert_tasks_equal(&r, src, &[]);
    assert_same_files(&r, src);
}

// ---------------------------------------------------------------------------
// Pannes et interruptions
// ---------------------------------------------------------------------------

#[test]
fn source_disconnection_fails_cleanly() {
    let _g = heavy();
    let src = gpt_image();
    let r = run_with(
        src,
        src.len() as u64,
        PlanOptions::default(),
        opts(),
        // L'analyse est faite avant l'injection ; la source « disparaît » après 20 Mio copiés.
        FaultPlan { vanish_after_read: Some(20 * MIB), ..Default::default() },
        FaultPlan::default(),
        Recorder::default(),
    );
    assert_eq!(r.outcome.status, FinalStatus::Failed);
    assert!(matches!(r.outcome.stop, StopReason::SourceLost(_)), "{:?}", r.outcome.stop);
    assert!(!r.outcome.table_written && no_table(&r.dst));
    assert!(r.outcome.copy.bytes_copied > 0 && r.outcome.copy.bytes_copied < r.plan.total_copy_bytes);
    assert!(r.outcome.reasons[0].contains("ne répond plus"), "{:#?}", r.outcome.reasons);
}

#[test]
fn source_access_denied_fails_cleanly() {
    let _g = heavy();
    let src = gpt_image();
    let r = run_with(
        src,
        src.len() as u64,
        PlanOptions::default(),
        opts(),
        FaultPlan { deny_read: true, ..Default::default() },
        FaultPlan::default(),
        Recorder::default(),
    );
    assert_eq!(r.outcome.status, FinalStatus::Failed);
    assert!(matches!(&r.outcome.stop, StopReason::SourceLost(m) if m.contains("refusé")), "{:?}", r.outcome.stop);
    assert!(no_table(&r.dst));
}

#[test]
fn destination_write_error_is_never_skipped() {
    let _g = heavy();
    let src = gpt_image();
    let bad = 80 * MIB;
    let r = run_with(
        src,
        src.len() as u64,
        PlanOptions { mode: CloneMode::RawPartitions, optimize_layout: true, grow_main: true },
        opts(),
        FaultPlan::default(),
        FaultPlan { bad_write: vec![(bad, 512)], ..Default::default() },
        Recorder::default(),
    );
    assert_eq!(r.outcome.status, FinalStatus::Failed);
    match &r.outcome.stop {
        StopReason::DestinationFailed(m) => assert!(m.contains("écriture"), "{m}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(r.outcome.copy.write_error_events, opts().write_retries as u64 + 1, "tentatives bornées");
    assert!(!r.outcome.table_written && no_table(&r.dst));
}

#[test]
fn destination_disconnection_and_write_protection_fail_cleanly() {
    let _g = heavy();
    let src = gpt_image();
    let r = run_with(
        src,
        src.len() as u64,
        PlanOptions::default(),
        opts(),
        FaultPlan::default(),
        FaultPlan { vanish_after_write: Some(6 * MIB), ..Default::default() },
        Recorder::default(),
    );
    assert_eq!(r.outcome.status, FinalStatus::Failed);
    assert!(matches!(r.outcome.stop, StopReason::DestinationFailed(_)));
    assert!(!r.outcome.table_written);

    let r = run_with(
        src,
        src.len() as u64,
        PlanOptions::default(),
        opts(),
        FaultPlan::default(),
        FaultPlan { write_protected: true, ..Default::default() },
        Recorder::default(),
    );
    assert_eq!(r.outcome.status, FinalStatus::Failed);
    assert!(matches!(r.outcome.stop, StopReason::DestinationFailed(_)));
    assert!(r.dst.iter().all(|b| *b == 0xA5), "destination protégée : intacte");
    assert_eq!(r.outcome.copy.bytes_copied, 0);
}

#[test]
fn silent_destination_corruption_is_caught_by_verification() {
    let _g = heavy();
    let src = gpt_image();
    let (off, _) = pick_bad_range(src);
    let r = run_with(
        src,
        src.len() as u64,
        PlanOptions::default(),
        opts(),
        FaultPlan::default(),
        FaultPlan { corrupt_write: vec![(off, 512)], ..Default::default() },
        Recorder::default(),
    );
    // La copie s'est « bien passée » du point de vue des E/S : seule la relecture révèle le défaut.
    assert_eq!(r.outcome.stop, StopReason::Completed);
    assert_eq!(r.outcome.copy.write_error_events, 0);
    assert_eq!(r.outcome.status, FinalStatus::Failed, "{}", dump_reasons(&r.outcome));
    assert!(!r.outcome.data_check.passed());
    let m = &r.outcome.data_check.mismatches[0];
    assert_eq!(m.kind, MismatchKind::DestinationDiffers);
    assert!(m.lba <= off / SS && off / SS < m.lba + m.sectors);

    // Sans vérification, la corruption passe inaperçue : le statut ne doit alors JAMAIS être « réussi ».
    let mut e = opts();
    e.verify = VerifyMode::None;
    let r = run_with(
        src,
        src.len() as u64,
        PlanOptions { mode: CloneMode::RawPartitions, optimize_layout: true, grow_main: true },
        e,
        FaultPlan::default(),
        FaultPlan::default(),
        Recorder::default(),
    );
    assert_eq!(r.outcome.status, FinalStatus::CompletedWithWarnings);
    assert!(r.outcome.reasons.iter().any(|x| x.contains("n'a pas été vérifié")));
}

#[test]
fn full_verification_detects_unstable_source() {
    let _g = heavy();
    let src = gpt_image();
    let (off, _) = pick_bad_range(src);
    let mut e = opts();
    e.verify = VerifyMode::Full;
    let r = run_with(
        src,
        src.len() as u64,
        PlanOptions::default(),
        e.clone(),
        FaultPlan { unstable_read: vec![(off, 512)], ..Default::default() },
        FaultPlan::default(),
        Recorder::default(),
    );
    assert_eq!(r.outcome.stop, StopReason::Completed);
    assert!(
        r.outcome.data_check.mismatches.iter().any(|m| m.kind == MismatchKind::SourceChanged && m.lba == off / SS),
        "{:#?}",
        r.outcome.data_check.mismatches
    );
    assert_ne!(r.outcome.status, FinalStatus::SuccessVerified);

    // Source stable : la comparaison complète réussit.
    let r = run_with(src, src.len() as u64, PlanOptions::default(), e, FaultPlan::default(), FaultPlan::default(), Recorder::default());
    assert_eq!(r.outcome.status, FinalStatus::SuccessVerified, "{}", dump_reasons(&r.outcome));
    assert_eq!(r.outcome.data_check.mode, VerifyMode::Full);
}

#[test]
fn cancellation_stops_promptly_and_leaves_no_partition_table() {
    let _g = heavy();
    let src = gpt_image();
    let ctl: &'static Control = Box::leak(Box::new(Control::new()));
    let rec = Recorder { ctl: Some(ctl), ..Default::default() };
    let mut e = opts();
    e.chunk_size = 64 * 1024;
    e.retry_block = 16 * 1024;
    let r = run_with(
        src,
        src.len() as u64,
        PlanOptions { mode: CloneMode::RawPartitions, optimize_layout: true, grow_main: true },
        e,
        FaultPlan { cancel_after_read: Some((10 * MIB, ctl)), ..Default::default() },
        FaultPlan::default(),
        rec,
    );
    assert_eq!(r.outcome.stop, StopReason::Cancelled);
    assert_eq!(r.outcome.status, FinalStatus::Incomplete);
    assert!(!r.outcome.table_written && no_table(&r.dst));
    // Arrêt immédiat : le bloc en cours se termine, pas un de plus. (Le compteur de
    // lecture inclut les quelques Mio lus pendant l'analyse préalable.)
    assert!(r.outcome.copy.bytes_copied >= 4 * MIB && r.outcome.copy.bytes_copied <= 10 * MIB + 64 * 1024, "{}", r.outcome.copy.bytes_copied);
    assert!(r.outcome.reasons[0].contains("annulée"));
}

#[test]
fn plan_and_device_mismatch_is_refused() {
    let _g = heavy();
    let src_img = gpt_image();
    let mut src = SliceDev::new(src_img);
    let analysis = analyze_disk(&mut src).unwrap();
    let plan = build_plan(&analysis, 256 * MIB, 512, &PlanOptions::default());
    // La destination réellement ouverte n'a pas la taille prévue par le plan (disque remplacé entre-temps).
    let mut dst = MemDevice::from_vec(vec![0xA5; 200 * MIB as usize], 512);
    let ctl = Control::new();
    let rec = Recorder::default();
    let o = run_clone(CloneRequest { src: &mut src, dst: &mut dst, src_analysis: &analysis, plan: &plan, opts: &opts(), ctl: &ctl, obs: &rec });
    assert_eq!(o.status, FinalStatus::Failed);
    assert!(o.reasons[0].contains("taille de la destination a changé"), "{:#?}", o.reasons);
    assert_eq!(dst.writes, 0);
}

#[test]
fn sector_size_mismatch_is_refused() {
    let _g = heavy();
    let mut src = SliceDev::new(gpt_image());
    let analysis = analyze_disk(&mut src).unwrap();
    let plan = build_plan(&analysis, 512 * MIB, 4096, &PlanOptions::default());
    assert!(plan.is_blocked());
    assert!(plan.findings.iter().any(|f| f.code == "SECTOR_SIZE_MISMATCH" && f.blocking));
}

#[test]
fn progress_is_reported_and_reaches_total() {
    let _g = heavy();
    let src = gpt_image();
    let r = run(src, src.len() as u64, PlanOptions::default());
    assert!(r.rec.progress_calls.load(Ordering::Relaxed) > 5);
    let logs = r.rec.logs.lock().unwrap();
    assert!(logs.iter().any(|(_, m)| m.contains("Résultat : Clonage réussi et vérifié")));
    assert_eq!(r.outcome.copy.bytes_copied, r.plan.total_copy_bytes);
    assert_eq!(r.outcome.data_check.checked_bytes, r.plan.total_copy_bytes);
    let _ = &r.analysis;
}
