//! Corrupted sources: the analysis of a damaged or hostile disk (partition tables, boot
//! sectors, MFT, FAT) must end in findings or errors, never in a panic or an endless
//! loop. Deterministic mutations of the test images, in memory only.
//!
//! The default run is short; `CHUD_FUZZ_ROUNDS=20000 cargo test --release --test
//! corrupt_sources -- --nocapture` explores much further.

mod common;

use chud_clone_core::boot::assess_boot;
use chud_clone_core::device::{BlockDevice, MemDevice};
use chud_clone_core::fs::ntfs_shrink;
use chud_clone_core::plan::{PlanOptions, analyze_disk, build_plan, minimal_destination_size};
use std::time::{Duration, Instant};

/// xorshift64*: reproducible without a dependency.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }
}

/// Regions holding metadata: start and end of the disk (tables), start of every
/// partition (boot sector, FAT), and the first records of each MFT.
fn hot_regions(data: &[u8]) -> Vec<(u64, u64)> {
    let mut dev = MemDevice::from_vec(data.to_vec(), 512);
    let a = analyze_disk(&mut dev).expect("clean image");
    let size = data.len() as u64;
    let mut r = vec![(0, 64 * 1024), (size - 64 * 1024, 64 * 1024)];
    for p in &a.parts {
        let base = p.part.start_lba * 512;
        r.push((base, 64 * 1024));
        // NTFS: $MFT position from the boot sector.
        let bs = &data[base as usize..base as usize + 512];
        if &bs[3..11] == b"NTFS    " {
            let bps = u16::from_le_bytes([bs[11], bs[12]]) as u64;
            let spc = bs[13] as u64;
            let mft = u64::from_le_bytes(bs[0x30..0x38].try_into().unwrap());
            let off = base + mft * bps * spc;
            if off + 256 * 1024 <= size {
                r.push((off, 256 * 1024));
            }
        }
    }
    r
}

fn exercise(data: Vec<u8>) {
    let len = data.len() as u64;
    let mut dev = MemDevice::from_vec(data, 512);
    let Ok(a) = analyze_disk(&mut dev) else { return };
    let _ = assess_boot(&mut dev, &a);
    let opts = PlanOptions::default();
    let min = minimal_destination_size(&a, 512, &opts);
    for dst in [len, len * 2, len / 2, min, 0, 4096] {
        let plan = build_plan(&a, dst, 512, &opts);
        if let Some(s) = &plan.shrink {
            let _ = ntfs_shrink::prepare(&mut dev, s.src_start, s.old_len, s.new_len);
        }
    }
    let _ = dev.size();
}

#[test]
fn damaged_metadata_never_panics() {
    let rounds: u64 = std::env::var("CHUD_FUZZ_ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(60);
    let mut failures = Vec::new();
    for name in ["gpt-win.img", "mbr-win.img", "ntfs-dirty.img"] {
        let mut data = common::load_fixture(name);
        let hot = hot_regions(&data);
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ name.len() as u64);
        for round in 0..rounds {
            let seed = rng.0;
            // 1 to 8 mutations, kept to restore the image afterwards.
            let mut saved = Vec::new();
            for _ in 0..1 + rng.below(8) {
                let (start, span) = hot[rng.below(hot.len() as u64) as usize];
                let off = (start + rng.below(span)) as usize;
                let width = [1usize, 2, 4, 8][rng.below(4) as usize].min(data.len() - off);
                saved.push((off, data[off..off + width].to_vec()));
                let v = match rng.below(5) {
                    0 => 0u64,
                    1 => u64::MAX,
                    2 => 1u64 << rng.below(64),
                    3 => rng.next(),
                    _ => u64::from_le_bytes(data[off..off + 8.min(data.len() - off)].iter().chain([0u8; 8].iter()).take(8).copied().collect::<Vec<_>>().try_into().unwrap()) ^ (1 << rng.below(64)),
                };
                data[off..off + width].copy_from_slice(&v.to_le_bytes()[..width]);
            }
            let started = Instant::now();
            let copy = data.clone();
            let outcome = std::panic::catch_unwind(move || exercise(copy));
            let took = started.elapsed();
            let offsets: Vec<usize> = saved.iter().map(|(o, _)| *o).collect();
            if let Err(e) = outcome {
                let msg = e.downcast_ref::<String>().cloned().or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default();
                failures.push(format!("{name} round {round} seed {seed:#x} offsets {offsets:x?}: panic: {msg}"));
            } else if took > Duration::from_secs(20) {
                failures.push(format!("{name} round {round} seed {seed:#x} offsets {offsets:x?}: took {took:?}"));
            }
            for (off, bytes) in saved.into_iter().rev() {
                data[off..off + bytes.len()].copy_from_slice(&bytes);
            }
        }
    }
    assert!(failures.is_empty(), "{} failure(s):\n{}", failures.len(), failures.join("\n"));
}
