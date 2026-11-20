//! Boot preparation written in place on a real NTFS volume (test image).

mod common;

use chud_clone_core::boot::assess_boot;
use chud_clone_core::bootprep::{BootPrepOutcome, boot_driver_patches, hive_u32, prepare_volume};
use chud_clone_core::device::{BlockDevice, FileDevice};
use chud_clone_core::fs::ntfs::{locate_file, read_file};
use chud_clone_core::plan::analyze_disk;

const SYSTEM: [&str; 4] = ["Windows", "System32", "config", "SYSTEM"];

/// Copy of the GPT image whose `SYSTEM` file starts with a real registry hive.
fn image_with_hive(name: &str) -> (std::path::PathBuf, u64, u64) {
    let src = common::fixture("gpt-win.img");
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("bootprep");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::copy(&src, &path).unwrap();
    let hive = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/system-hive.bin")).unwrap();
    let mut dev = FileDevice::open_read_write(&path, 512).unwrap();
    let a = analyze_disk(&mut dev).unwrap();
    let boot = assess_boot(&mut dev, &a);
    let win = a.parts.iter().find(|p| Some(p.part.number) == boot.windows_partition).expect("Windows partition");
    let (base, len) = (win.part.start_lba * 512, win.part.sectors * 512);
    // The test image's SYSTEM file holds random bytes: its start becomes the hive.
    let offsets: Vec<u64> = (0..hive.len() as u64).step_by(512).collect();
    let (size, pos) = locate_file(&mut dev, base, len, &SYSTEM, &offsets).unwrap().expect("SYSTEM file");
    assert!(size as usize > hive.len());
    for (chunk, p) in hive.chunks(512).zip(pos) {
        dev.write_at(base + p.expect("allocated"), chunk).unwrap();
    }
    dev.flush().unwrap();
    let back = read_file(&mut dev, base, len, &SYSTEM, hive.len()).unwrap().unwrap();
    assert_eq!(back, hive, "the hive must read back through NTFS exactly as written");
    (path, base, len)
}

#[test]
fn drivers_are_enabled_in_place_and_nothing_else_changes() {
    let (path, base, len) = image_with_hive("drivers.img");
    let before = std::fs::read(&path).unwrap();
    let mut dev = FileDevice::open_read_write(&path, 512).unwrap();
    let hive_before = read_file(&mut dev, base, len, &SYSTEM, 1 << 20).unwrap().unwrap();
    let patches = boot_driver_patches(&hive_before).unwrap();
    assert_eq!(patches.len(), 6);

    let mut out = BootPrepOutcome::default();
    prepare_volume(&mut dev, base, len, &mut out, &|_| {});
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert_eq!(out.drivers, vec!["stornvme", "storahci"]);
    // No hiberfil.sys on this image: nothing to cancel.
    assert!(!out.hibernation_cleared);

    let hive_after = read_file(&mut dev, base, len, &SYSTEM, 1 << 20).unwrap().unwrap();
    assert!(boot_driver_patches(&hive_after).unwrap().is_empty());
    for p in &patches {
        assert_eq!(hive_u32(&hive_after, p.offset), Some(0));
    }
    drop(dev);
    // Exactly the 6 values of 4 bytes changed in the whole disk image.
    let after = std::fs::read(&path).unwrap();
    let changed = before.iter().zip(&after).filter(|(a, b)| a != b).count();
    assert!(changed > 0 && changed <= 6 * 4, "{changed} bytes changed");

    // A second run finds nothing to do.
    let mut dev = FileDevice::open_read_write(&path, 512).unwrap();
    let mut again = BootPrepOutcome::default();
    prepare_volume(&mut dev, base, len, &mut again, &|_| {});
    assert!(again.drivers.is_empty() && again.warnings.is_empty(), "{again:?}");
    drop(dev);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_volume_without_windows_registry_reports_it() {
    let src = common::fixture("gpt-win.img");
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("bootprep");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("random-system.img");
    std::fs::copy(&src, &path).unwrap();
    let mut dev = FileDevice::open_read_write(&path, 512).unwrap();
    let a = analyze_disk(&mut dev).unwrap();
    let boot = assess_boot(&mut dev, &a);
    let win = a.parts.iter().find(|p| Some(p.part.number) == boot.windows_partition).unwrap();
    let before = std::fs::read(&path).unwrap();
    let mut out = BootPrepOutcome::default();
    // The image's SYSTEM file is random bytes, not a hive: refused, nothing written.
    prepare_volume(&mut dev, win.part.start_lba * 512, win.part.sectors * 512, &mut out, &|_| {});
    assert_eq!(out.warnings.len(), 1, "{:?}", out.warnings);
    assert!(out.drivers.is_empty());
    drop(dev);
    assert!(std::fs::read(&path).unwrap() == before, "nothing may be written");
    let _ = std::fs::remove_file(&path);
}
