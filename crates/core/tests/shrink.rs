//! Automatic shrink of the main NTFS partition on a smaller destination: plan,
//! execution (data moved below the new end), checks and failure cases.
//! In-memory images only.
//!
//! Set `CHUD_EXPORT_DIR` to write the shrunk clones there, for a check with the
//! reference tools (chkdsk on Windows, ntfsfix / ntfsinfo on Linux).

mod common;

use chud_clone_core::clone::{CloneOutcome, CloneRequest, FinalStatus, run_clone};
use chud_clone_core::device::MemDevice;
use chud_clone_core::engine::{Control, EngineOptions, NullObserver};
use chud_clone_core::fs::{self, ntfs_shrink};
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

/// MBR disk "System Reserved + Windows", Windows in last position.
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

/// Every file of partition `n` is identical between source and clone.
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
        assert!(x == y, "content differs: {path}");
    }
    a.len()
}

fn dump(o: &CloneOutcome) -> String {
    format!("{:?} / {:#?} / shrink={:#?} / struct={:#?} / table={:#?}", o.status, o.reasons, o.shrink, o.structure_findings, o.table_check)
}

/// Destination size for which partition `n` of `src` must lose `cut` bytes.
fn smaller_by(src: &[u8], cut: u64) -> u64 {
    let mut s = MemDevice::from_vec(src.to_vec(), 512);
    let a = analyze_disk(&mut s).unwrap();
    let parts: u64 = a.parts.iter().map(|p| p.part.sectors * SS).sum();
    // Partitions packed from 1 MiB, plus the GPT backup (and alignment slack).
    parts + 2 * MIB - cut
}

#[test]
fn gpt_smaller_destination_shrinks_windows_partition_and_moves_data() {
    let _g = heavy();
    let src = gpt_image();
    let mut s = MemDevice::from_vec(src.to_vec(), 512);
    let a = analyze_disk(&mut s).unwrap();
    let p3 = a.parts.iter().find(|p| p.part.number == 3).unwrap();
    let used = p3.fs.used_bytes.unwrap();
    // Leave just enough room for the data plus the reserve.
    let new_len = (used + ntfs_shrink::reserve(64 * MIB) + 4 * MIB).next_multiple_of(MIB);
    let cut = p3.part.sectors * SS - new_len;
    let dst_size = smaller_by(src, cut);
    let o = clone(src, dst_size, PlanOptions::default(), FaultPlan::default());
    let sp = o.plan.shrink.unwrap_or_else(|| panic!("shrink planned: {:#?}", o.plan.findings));
    assert_eq!(sp.number, 3);
    assert!(sp.new_len < sp.old_len);
    assert!(o.plan.findings.iter().any(|f| f.code == "NTFS_SHRINK_PLANNED"));
    assert!(!o.plan.is_blocked(), "{:#?}", o.plan.findings);
    assert_eq!(o.outcome.status, FinalStatus::SuccessVerified, "{}", dump(&o.outcome));
    let so = o.outcome.shrink.as_ref().unwrap();
    let sm = so.summary.as_ref().unwrap();
    assert!(so.done && so.attempted);
    assert!(sm.relocated_clusters > 0, "data beyond the new end expected in the test image: {sm:#?}");
    assert!(o.outcome.reasons.iter().any(|r| r.contains("réduite") || r.contains("shrunk")), "{:#?}", o.outcome.reasons);

    // Table: partition 3 has its new size, the recovery partition follows it.
    let mut d = MemDevice::from_vec(o.dst.clone(), 512);
    let l = read_layout(&mut d).unwrap();
    assert!(l.findings.is_empty(), "{:#?}", l.findings);
    let d3 = l.partitions.iter().find(|p| p.number == 3).unwrap();
    assert_eq!(d3.sectors * SS, sp.new_len);

    // File system re-read by the `ntfs` crate (a parser independent of ours).
    let da = analyze_disk(&mut d).unwrap();
    let ap = da.parts.iter().find(|p| p.part.number == 3).unwrap();
    assert_eq!(ap.fs.fs_size, Some(sm.new_size));
    assert_eq!(ap.fs.used_bytes, Some(sm.used_clusters * sm.cluster_size));
    assert!(ap.fs.findings.iter().any(|f| f.code == "NTFS_DIRTY"), "volume marked dirty");
    assert!(!ap.fs.findings.iter().any(|f| f.code == "NTFS_UNREADABLE"), "{:#?}", ap.fs.findings);
    assert_eq!(ap.fs.label.as_deref(), Some("Windows"));

    // Every file identical, in the shrunk partition and in the moved one.
    assert!(same_files(src, &o, 3) > 50);
    assert!(same_files(src, &o, 4) > 0);
    let boot = o.outcome.boot_destination.as_ref().unwrap();
    assert!(boot.findings.is_empty(), "{:#?}", boot.findings);
    export("gpt-shrunk.img", &o.dst);
}

#[test]
fn shrunk_clone_can_be_grown_back_and_shrunk_again() {
    let _g = heavy();
    let src = gpt_image();
    let dst_size = smaller_by(src, 24 * MIB);
    let first = clone(src, dst_size, PlanOptions::default(), FaultPlan::default());
    assert!(!first.plan.is_blocked(), "{:#?}", first.plan.findings);
    assert_eq!(first.outcome.status, FinalStatus::SuccessVerified, "{}", dump(&first.outcome));
    // The shrunk clone is "dirty" (chkdsk pending): its allocation map is not
    // trusted, so it is copied in full; growing it is refused for the same reason.
    // Clear the flag as chkdsk would, then grow it onto a larger disk.
    let mut img = first.dst.clone();
    let sp = first.plan.shrink.unwrap();
    clear_dirty(&mut img, sp.dst_start, sp.new_len);
    let second = clone(&img, 400 * MIB, PlanOptions::default(), FaultPlan::default());
    assert_eq!(second.outcome.status, FinalStatus::SuccessVerified, "{}", dump(&second.outcome));
    assert!(second.outcome.grow.as_ref().is_some_and(|g| g.done));
    assert!(same_files(&img, &second, 3) > 50);
    // And shrunk once more, below the first size.
    let mut img2 = second.dst.clone();
    let g = second.plan.grow.unwrap();
    clear_dirty(&mut img2, g.dst_start, g.new_len);
    let third = clone(&img2, smaller_by(src, 30 * MIB), PlanOptions::default(), FaultPlan::default());
    assert_eq!(third.outcome.status, FinalStatus::SuccessVerified, "{}", dump(&third.outcome));
    assert!(same_files(&img2, &third, 3) > 50);
}

/// Clears the "dirty" flag and gives the volume a clean (empty) journal, as
/// Windows does after chkdsk; test helper.
fn clear_dirty(img: &mut [u8], base: u64, len: u64) {
    let mut d = MemDevice::from_vec(img.to_vec(), 512);
    ntfs_shrink::testing_mark_clean(&mut d, base, len).unwrap();
    img.copy_from_slice(&d.data);
}

#[test]
fn mbr_last_partition_is_shrunk() {
    let _g = heavy();
    let src = mbr_two_parts();
    let dst_size = src.len() as u64 - 20 * MIB;
    let o = clone(src, dst_size, PlanOptions::default(), FaultPlan::default());
    assert!(!o.plan.is_blocked(), "{:#?}", o.plan.findings);
    assert_eq!(o.outcome.status, FinalStatus::SuccessVerified, "{}", dump(&o.outcome));
    let sp = o.plan.shrink.expect("shrink planned");
    assert_eq!(sp.number, 2);
    assert!(sp.dst_start + sp.new_len <= dst_size);
    let mut d = MemDevice::from_vec(o.dst.clone(), 512);
    let l = read_layout(&mut d).unwrap();
    assert!(l.findings.is_empty(), "{:#?}", l.findings);
    assert_eq!(l.partitions[1].sectors * SS, sp.new_len);
    // Boot code and signature unchanged; only the size of entry 2 differs.
    assert_eq!(o.dst[..0x1BE], src[..0x1BE]);
    assert_eq!(o.dst[0x1BE..0x1BE + 16 + 12], src[0x1BE..0x1BE + 16 + 12]);
    assert!(same_files(src, &o, 2) > 10);
    export("mbr-shrunk.img", &o.dst);
}

#[test]
fn too_small_even_after_shrink_is_refused_with_the_reason() {
    let _g = heavy();
    let src = gpt_image();
    let o = clone(src, 40 * MIB, PlanOptions::default(), FaultPlan::default());
    assert!(o.plan.is_blocked());
    let f = o.plan.findings.iter().find(|f| f.code == "DEST_TOO_SMALL").unwrap();
    assert!(f.detail.contains("Réduction automatique") || f.detail.contains("shrunk automatically"), "{}", f.detail);
    assert!(o.outcome.shrink.is_none());
    // Resizing disabled: no shrink either.
    let o = clone(src, smaller_by(src, 24 * MIB), PlanOptions { grow_main: false, ..Default::default() }, FaultPlan::default());
    assert!(o.plan.is_blocked() && o.plan.shrink.is_none());
}

#[test]
fn write_failure_during_shrink_fails_without_writing_the_table() {
    let _g = heavy();
    let src = gpt_image();
    let dst_size = smaller_by(src, 24 * MIB);
    let probe = clone(src, dst_size, PlanOptions::default(), FaultPlan::default());
    let sp = probe.plan.shrink.unwrap();
    let sm = probe.outcome.shrink.unwrap().summary.unwrap();
    // The backup boot sector, written by the shrink only.
    let off = sp.dst_start + sm.new_size;
    let o = clone(src, dst_size, PlanOptions::default(), FaultPlan { bad_write: vec![(off, 512)], ..Default::default() });
    let so = o.outcome.shrink.as_ref().unwrap();
    assert!(so.attempted && !so.done, "{so:#?}");
    assert_eq!(o.outcome.status, FinalStatus::Failed, "{}", dump(&o.outcome));
    assert!(!o.outcome.table_written);
}
