//! Helpers shared by the integration tests: access to the test images.
//!
//! The images are real virtual disks (GPT/MBR, NTFS, FAT) produced by
//! `scripts/make-fixtures.sh`. No test touches a physical disk.
#![allow(dead_code)]

use chud_clone_core::device::{BlockDevice, FileDevice};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

static LOCK: Mutex<()> = Mutex::new(());

/// Path of the decompressed image (cached in cargo's temporary folder).
pub fn fixture(name: &str) -> PathBuf {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let gz = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join(format!("{name}.gz"));
    assert!(gz.exists(), "image de test absente : {} (voir scripts/make-fixtures.sh)", gz.display());
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("fixtures");
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join(name);
    let fresh = match (std::fs::metadata(&out), std::fs::metadata(&gz)) {
        (Ok(o), Ok(g)) => o.modified().unwrap() >= g.modified().unwrap() && o.len() > 0,
        _ => false,
    };
    if !fresh {
        let tmp = dir.join(format!("{name}.{}.tmp", std::process::id()));
        let mut dec = flate2::read::GzDecoder::new(File::open(&gz).unwrap());
        let mut f = File::create(&tmp).unwrap();
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = dec.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            f.write_all(&buf[..n]).unwrap();
        }
        f.sync_all().unwrap();
        drop(f);
        let _ = std::fs::remove_file(&out);
        std::fs::rename(&tmp, &out).unwrap();
    }
    out
}

/// Opens a test image read-only.
pub fn open_fixture(name: &str) -> FileDevice {
    FileDevice::open_read_only(&fixture(name), 512).unwrap()
}

/// Loads a test image entirely into memory.
pub fn load_fixture(name: &str) -> Vec<u8> {
    std::fs::read(fixture(name)).unwrap()
}

/// Reads a whole device into memory.
pub fn dump(dev: &mut dyn BlockDevice) -> Vec<u8> {
    let mut out = vec![0u8; dev.size() as usize];
    let chunk = 1 << 20;
    let mut off = 0usize;
    while off < out.len() {
        let n = chunk.min(out.len() - off);
        dev.read_at(off as u64, &mut out[off..off + n]).unwrap();
        off += n;
    }
    out
}
