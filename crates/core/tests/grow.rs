//! Agrandissement automatique de la partition NTFS principale sur une destination plus
//! grande : plan, exécution, contrôles et cas d'échec. Images en mémoire uniquement.
//!
//! Pour une validation indépendante avec les outils Linux de référence, définir
//! `CHUD_EXPORT_DIR` : les clones agrandis y sont écrits (voir scripts/validate-grow.sh).

mod common;

use chud_clone_core::clone::{CloneOutcome, CloneRequest, FinalStatus, run_clone};
use chud_clone_core::device::MemDevice;
use chud_clone_core::engine::{Control, EngineOptions, NullObserver};
use chud_clone_core::fs::{self, FsKind, ntfs_grow};
use chud_clone_core::partition::{MIB, read_layout};
use chud_clone_core::plan::{ClonePlan, PlanOptions, analyze_disk, build_plan};
use chud_clone_core::testing::{FaultPlan, FaultyDevice};
use std::sync::{Mutex, MutexGuard, OnceLock};

const SS: u64 = 512;
static HEAVY: Mutex<()> = Mutex::new(());
fn heavy() -> MutexGuard<'static, ()> {
    HEAVY.lock().unwrap_or_else(|e| e.into_inner())
}

fn gpt_image() -> &'static [u8] {
    static IMG: OnceLock<Vec<u8>> = OnceLock::new();
    IMG.get_or_init(|| common::load_fixture("gpt-win.img"))
}

/// Disque MBR « Réservé au système + Windows », Windows en dernière position : l'image
/// de test MBR tronquée après la partition 2, sans la partition étendue.
fn mbr_two_parts() -> &'static [u8] {
    static IMG: OnceLock<Vec<u8>> = OnceLock::new();
    IMG.get_or_init(|| {
        let mut img = common::load_fixture("mbr-win.img");
        let end = (67584 + 98304) * SS as usize;
        img.truncate(end + MIB as usize);
        img[0x1BE + 32..0x1BE + 64].fill(0);
        img
    })
}

struct Out {
    outcome: CloneOutcome,
    plan: ClonePlan,
    dst: Vec<u8>,
}

fn clone(src: &[u8], dst_size: u64, popts: PlanOptions, dst_faults: FaultPlan) -> Out {
    let mut s = MemDevice::from_vec(src.to_vec(), 512);
    let analysis = analyze_disk(&mut s).unwrap();
    let plan = build_plan(&analysis, dst_size, 512, &popts);
    let mut d = FaultyDevice::new(MemDevice::from_vec(vec![0xA5u8; dst_size as usize], 512), dst_faults);
    let ctl = Control::new();
    let eopts = EngineOptions { chunk_size: 256 * 1024, retry_block: 16 * 1024, ..Default::default() };
    let outcome =
        run_clone(CloneRequest { src: &mut s, dst: &mut d, src_analysis: &analysis, plan: &plan, opts: &eopts, ctl: &ctl, obs: &NullObserver });
    Out { outcome, plan, dst: d.inner.data }
}

fn export(name: &str, data: &[u8]) {
    if let Ok(dir) = std::env::var("CHUD_EXPORT_DIR") {
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(std::path::Path::new(&dir).join(name), data).unwrap();
    }
}

/// Tous les fichiers de la partition `n` sont identiques entre source et clone.
fn same_files(src: &[u8], o: &Out, n: u32) -> usize {
    let pp = o.plan.partitions.iter().find(|p| p.number == n).unwrap();
    let mut s = MemDevice::from_vec(src.to_vec(), 512);
    let mut d = MemDevice::from_vec(o.dst.clone(), 512);
    let a = fs::ntfs::list_files(&mut s, pp.src_start, pp.src_len).unwrap();
    let b = fs::ntfs::list_files(&mut d, pp.dst_start, pp.dst_len).unwrap();
    assert_eq!(a, b);
    for (path, _) in &a {
        let comps: Vec<&str> = path.split('/').collect();
        let x = fs::ntfs::read_file(&mut s, pp.src_start, pp.src_len, &comps, usize::MAX).unwrap().unwrap();
        let y = fs::ntfs::read_file(&mut d, pp.dst_start, pp.dst_len, &comps, usize::MAX).unwrap().unwrap();
        assert!(x == y, "contenu différent : {path}");
    }
    a.len()
}

fn dump(o: &CloneOutcome) -> String {
    format!("{:?} / {:#?} / grow={:#?} / struct={:#?}", o.status, o.reasons, o.grow, o.structure_findings)
}

#[test]
fn source_analysis_marks_ntfs_volumes_growable() {
    let _g = heavy();
    let mut s = MemDevice::from_vec(gpt_image().to_vec(), 512);
    let a = analyze_disk(&mut s).unwrap();
    for p in &a.parts {
        if p.fs.kind == FsKind::Ntfs {
            assert!(p.fs.grow_ready, "partition {} : {:?}", p.part.number, p.fs.grow_issue);
        } else {
            assert!(!p.fs.grow_ready);
        }
    }
}

#[test]
fn gpt_larger_destination_grows_windows_partition() {
    let _g = heavy();
    let src = gpt_image();
    let dst_size = 320 * MIB;
    let o = clone(src, dst_size, PlanOptions::default(), FaultPlan::default());
    assert_eq!(o.outcome.status, FinalStatus::SuccessVerified, "{}", dump(&o.outcome));
    let g = o.plan.grow.expect("agrandissement prévu");
    assert_eq!(g.number, 3);
    assert_eq!(g.old_len, 96 * MIB);
    assert!(g.new_len > 200 * MIB, "{}", g.new_len);
    assert_eq!(o.plan.free_after_main, 0);
    assert!(o.plan.findings.iter().any(|f| f.code == "NTFS_GROW_PLANNED"));
    let go = o.outcome.grow.as_ref().unwrap();
    assert!(go.attempted && go.done && !go.modified, "{go:#?}");
    assert!(o.outcome.reasons.iter().any(|r| r.contains("agrandie")));

    // Table : la partition 3 s'étend jusqu'à la partition de récupération déplacée.
    let mut d = MemDevice::from_vec(o.dst.clone(), 512);
    let l = read_layout(&mut d).unwrap();
    assert!(l.findings.is_empty(), "{:#?}", l.findings);
    let p3 = l.partitions.iter().find(|p| p.number == 3).unwrap();
    let p4 = l.partitions.iter().find(|p| p.number == 4).unwrap();
    assert_eq!(p3.sectors * SS, g.new_len);
    assert_eq!(p3.end_lba(), p4.start_lba);

    // Système de fichiers relu par le crate `ntfs` (analyseur indépendant du nôtre).
    let a = analyze_disk(&mut d).unwrap();
    let ap = a.parts.iter().find(|p| p.part.number == 3).unwrap();
    let s = go.summary.as_ref().unwrap();
    assert_eq!(ap.fs.fs_size, Some(g.new_len - SS));
    assert_eq!(s.new_size, g.new_len - SS);
    assert!(ap.fs.findings.iter().any(|f| f.code == "NTFS_DIRTY"), "volume marqué à vérifier");
    let mut sd = MemDevice::from_vec(src.to_vec(), 512);
    let sa = analyze_disk(&mut sd).unwrap();
    let sp = sa.parts.iter().find(|p| p.part.number == 3).unwrap();
    assert_eq!(ap.fs.used_bytes, Some(sp.fs.used_bytes.unwrap() + s.bitmap_extra_clusters * s.cluster_size));
    assert_eq!(ap.fs.label.as_deref(), Some("Windows"));

    // Contrôle de relecture du module, et fichiers identiques dans les deux partitions NTFS.
    ntfs_grow::verify(&mut d, g.dst_start, g.new_len, s).unwrap();
    assert!(same_files(src, &o, 3) > 50);
    assert!(same_files(src, &o, 4) > 0);
    // Démarrage toujours reconnu.
    let boot = o.outcome.boot_destination.as_ref().unwrap();
    assert!(boot.findings.is_empty(), "{:#?}", boot.findings);
    export("gpt-grown.img", &o.dst);
}

#[test]
fn grown_volume_can_be_grown_again() {
    let _g = heavy();
    // Second agrandissement d'un volume déjà agrandi (bitmap répartie sur deux extensions).
    let src = gpt_image();
    let o = clone(src, 320 * MIB, PlanOptions::default(), FaultPlan::default());
    let g = o.plan.grow.unwrap();
    let mut img = o.dst.clone();
    img.resize(1024 * MIB as usize, 0);
    let mut d = MemDevice::from_vec(img, 512);
    // La partition récupérée n'est pas déplacée ici : seul le système de fichiers est
    // étendu dans une partition supposée plus grande (test du module seul).
    let base = g.dst_start;
    let new_len = 1024 * MIB - base - MIB;
    let s = ntfs_grow::grow(&mut d, base, new_len).unwrap();
    assert_eq!(s.new_size, new_len - SS);
    assert!(s.bitmap_extra_clusters >= 1);
    ntfs_grow::verify(&mut d, base, new_len, &s).unwrap();
    let a = fs::analyze(&mut d, base, new_len);
    assert_eq!(a.fs_size, Some(new_len - SS));
    let files = fs::ntfs::list_files(&mut d, base, new_len).unwrap();
    assert!(files.len() > 50);
    let x = fs::ntfs::read_file(&mut d, base, new_len, &["Program Files", "App", "app.exe"], usize::MAX).unwrap().unwrap();
    assert_eq!(x.len(), 3100 * 1024);
}

#[test]
fn mbr_last_partition_is_grown_to_end_of_disk() {
    let _g = heavy();
    let src = mbr_two_parts();
    let o = clone(src, 256 * MIB, PlanOptions::default(), FaultPlan::default());
    assert_eq!(o.outcome.status, FinalStatus::SuccessVerified, "{}", dump(&o.outcome));
    let g = o.plan.grow.expect("agrandissement prévu");
    assert_eq!(g.number, 2);
    assert_eq!(g.dst_start + g.new_len, 256 * MIB);
    let mut d = MemDevice::from_vec(o.dst.clone(), 512);
    let l = read_layout(&mut d).unwrap();
    assert!(l.findings.is_empty(), "{:#?}", l.findings);
    assert_eq!(l.partitions.len(), 2);
    assert_eq!(l.partitions[1].sectors * SS, g.new_len);
    // Code d'amorçage et signature inchangés ; seule la taille de l'entrée 2 diffère.
    assert_eq!(o.dst[..0x1BE], src[..0x1BE]);
    assert_eq!(o.dst[0x1BE..0x1BE + 16 + 12], src[0x1BE..0x1BE + 16 + 12]);
    assert_ne!(o.dst[0x1CE + 12..0x1CE + 16], src[0x1CE + 12..0x1CE + 16]);
    assert_eq!(o.dst[510..512], [0x55, 0xAA]);
    assert!(same_files(src, &o, 2) > 10);
    export("mbr-grown.img", &o.dst);
    export("mbr-source.img", src);
}

#[test]
fn grow_disabled_or_destination_same_size_changes_nothing() {
    let _g = heavy();
    let src = gpt_image();
    let o = clone(src, 320 * MIB, PlanOptions { grow_main: false, ..Default::default() }, FaultPlan::default());
    assert_eq!(o.outcome.status, FinalStatus::SuccessVerified, "{}", dump(&o.outcome));
    assert!(o.plan.grow.is_none() && o.outcome.grow.is_none());
    let p3 = o.plan.partitions.iter().find(|p| p.number == 3).unwrap();
    assert_eq!(p3.dst_len, p3.src_len);
    let o = clone(src, src.len() as u64, PlanOptions::default(), FaultPlan::default());
    assert!(o.plan.grow.is_none());
    assert!(!o.plan.findings.iter().any(|f| f.code == "GROW_NOT_AUTOMATIC" || f.code == "NTFS_GROW_PLANNED"));
}

#[test]
fn write_failure_during_grow_is_reported_and_fails_the_clone() {
    let _g = heavy();
    let src = gpt_image();
    // Repérage : le secteur d'amorçage de secours est écrit dans le dernier secteur de la
    // partition agrandie, zone que la copie ne touche pas.
    let probe = clone(src, 320 * MIB, PlanOptions::default(), FaultPlan::default());
    let g = probe.plan.grow.unwrap();
    let s = probe.outcome.grow.unwrap().summary.unwrap();
    let off = g.dst_start + s.new_size;
    let faults = FaultPlan { bad_write: vec![(off, 512)], ..Default::default() };
    let o = clone(src, 320 * MIB, PlanOptions::default(), faults);
    let go = o.outcome.grow.as_ref().unwrap();
    assert!(go.attempted && !go.done && go.modified, "{go:#?}");
    assert_eq!(o.outcome.status, FinalStatus::Failed, "{}", dump(&o.outcome));
    assert!(o.outcome.reasons.iter().any(|r| r.contains("chkdsk /f")), "{:#?}", o.outcome.reasons);
    // Le secteur d'amorçage principal (écrit en dernier) n'a pas changé : le volume
    // garde sa taille d'origine.
    let mut d = MemDevice::from_vec(o.dst.clone(), 512);
    let a = fs::analyze(&mut d, g.dst_start, g.new_len);
    assert_eq!(a.fs_size, Some(g.old_len - SS));
}
