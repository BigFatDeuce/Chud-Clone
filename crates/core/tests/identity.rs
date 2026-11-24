//! Identity of the source and destination image files.
//!
//! The same file can be reached through two different names: a hard link, an
//! administrative share (`\\host\c$\...`), a second mount point of its volume. Comparing
//! paths cannot see through those; the clone must compare the files themselves and
//! refuse before writing anything, otherwise opening the destination (truncated to be
//! recreated) erases the source.
//!
//! Every test works on copies of the test images in cargo's temporary folder.

mod common;

use chud_clone_core::engine::{Control, EngineOptions, NullObserver};
use chud_clone_core::job::{JobConfig, JobError, Target, execute, prepare};
use chud_clone_core::plan::PlanOptions;
use std::path::{Path, PathBuf};

fn work_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("identity").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn config(src: &Path, dst: &Path) -> JobConfig {
    JobConfig {
        source: Target::Image(src.to_path_buf()),
        destination: Target::Image(dst.to_path_buf()),
        plan: PlanOptions::default(),
        engine: EngineOptions::default(),
        new_image_size: None,
        allow_unvalidated_physical_write: false,
        expected_source_fingerprint: None,
        expected_destination_fingerprint: None,
        app_paths: vec![],
        prepare_boot: false,
    }
}

fn hash(path: &Path) -> String {
    blake3::hash(&std::fs::read(path).unwrap()).to_hex().to_string()
}

/// Source image copied into `dir`, with its hash.
fn source_copy(dir: &Path) -> (PathBuf, String) {
    let src = dir.join("source.img");
    std::fs::copy(common::fixture("gpt-win.img"), &src).unwrap();
    let h = hash(&src);
    (src, h)
}

#[test]
fn hard_link_to_the_source_is_refused_before_any_write() {
    let dir = work_dir("hard-link");
    let (src, before) = source_copy(&dir);
    let alias = dir.join("alias.img");
    std::fs::hard_link(&src, &alias).unwrap();

    let cfg = config(&src, &alias);
    let prep = prepare(&cfg, false).unwrap();
    assert!(prep.blocked, "a hard link to the source must be refused: {:#?}", prep.findings);
    assert!(prep.findings.iter().any(|f| f.code == "SAME_DISK" && f.blocking), "{:#?}", prep.findings);

    match execute(&cfg, &dir.join("reports"), &Control::new(), &NullObserver) {
        Err(JobError::Blocked(f)) => assert!(f.iter().any(|x| x.code == "SAME_DISK")),
        Err(JobError::Failed(m)) => panic!("refused, but not by the pre-checks: {m}"),
        Ok(r) => panic!("the clone ran onto its own source: {:?}", r.outcome.status),
    }
    assert_eq!(hash(&src), before, "the source image was modified");
}

#[test]
fn a_copy_of_the_source_is_a_different_disk() {
    let dir = work_dir("copy");
    let (src, _) = source_copy(&dir);
    let copy = dir.join("copy.img");
    std::fs::copy(&src, &copy).unwrap();
    let prep = prepare(&config(&src, &copy), false).unwrap();
    assert!(!prep.findings.iter().any(|f| f.code == "SAME_DISK"), "{:#?}", prep.findings);
}

/// The destination is checked on the handle actually used for writing: a hard link made
/// after the pre-checks (a file swapped in at the last moment) is refused as well, and
/// the source keeps every byte.
#[test]
fn alias_made_after_the_checks_is_refused_on_the_open_handle() {
    use chud_clone_core::device::FileDevice;
    let dir = work_dir("late-alias");
    let (src, before) = source_copy(&dir);
    let size = std::fs::metadata(&src).unwrap().len();

    // Source opened as the clone opens it: no writer allowed while it is open.
    let held = FileDevice::open_source(&src, 512).unwrap();
    let id = held.identity().unwrap();
    assert!(id.is_some(), "NTFS reports file identities");
    let alias = dir.join("late.img");
    std::fs::hard_link(&src, &alias).unwrap();
    assert!(FileDevice::create_excluding(&alias, size, 512, id).is_err());
    drop(held);
    assert_eq!(hash(&src), before);

    // Even when the source was opened without that protection, the identity check on the
    // destination handle refuses it before emptying it.
    let plain = FileDevice::open_read_only(&src, 512).unwrap();
    let err = FileDevice::create_excluding(&alias, size, 512, plain.identity().unwrap()).err().expect("alias refused");
    assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists, "{err}");
    drop(plain);
    assert_eq!(hash(&src), before, "the source image was modified");

    // A genuinely different file is created normally.
    let other = dir.join("other.img");
    let dev = FileDevice::create_excluding(&other, 1 << 20, 512, id).unwrap();
    assert_eq!(chud_clone_core::device::BlockDevice::size(&dev), 1 << 20);
}

/// The same file named with the `\\?\` prefix and different letter case.
#[cfg(windows)]
#[test]
fn same_file_under_another_spelling_is_refused() {
    let dir = work_dir("spelling");
    let (src, before) = source_copy(&dir);
    let canon = std::fs::canonicalize(&src).unwrap(); // \\?\D:\...
    let other = PathBuf::from(canon.to_string_lossy().to_uppercase());
    let prep = prepare(&config(&src, &other), false).unwrap();
    assert!(prep.findings.iter().any(|f| f.code == "SAME_DISK" && f.blocking), "{:#?}", prep.findings);
    assert_eq!(hash(&src), before);
}

/// A device or a folder is never taken for an image file: physical disks only go
/// through `disk:N`, with all their protections.
#[cfg(windows)]
#[test]
fn devices_and_folders_are_not_images() {
    use chud_clone_core::device::FileDevice;
    use chud_clone_core::job::describe;
    let dir = work_dir("devices");
    for p in ["NUL", r"\\.\NUL", "CON", r"\\.\PhysicalDrive999"] {
        let p = Path::new(p);
        assert!(FileDevice::open_read_only(p, 512).is_err(), "{} opened as an image", p.display());
        assert!(FileDevice::create(p, 1 << 20, 512).is_err(), "{} created as an image", p.display());
        if p.exists() {
            assert!(describe(&Target::Image(p.to_path_buf()), false, &[], None).is_err(), "{} described as an image", p.display());
        }
    }
    assert!(FileDevice::open_read_only(&dir, 512).is_err());
    assert!(FileDevice::create(&dir, 1 << 20, 512).is_err());
    assert!(dir.is_dir(), "the folder is untouched");
}

/// A source image another program is writing to would change during the copy: it is
/// refused at opening instead of being copied inconsistently.
#[cfg(windows)]
#[test]
fn source_being_written_elsewhere_is_refused() {
    use chud_clone_core::device::FileDevice;
    let dir = work_dir("busy");
    let (src, _) = source_copy(&dir);
    let writer = std::fs::OpenOptions::new().write(true).open(&src).unwrap();
    let err = FileDevice::open_source(&src, 512).err().expect("refused while a writer holds it");
    assert_eq!(err.kind(), std::io::ErrorKind::ResourceBusy, "{err}");
    drop(writer);
    assert!(FileDevice::open_source(&src, 512).is_ok());
}

/// The same file through the administrative share of its own drive
/// (`\\localhost\D$\...`): canonical paths differ (UNC against drive letter), the file
/// is the same. Skipped when the share is not reachable on this computer.
#[cfg(windows)]
#[test]
fn same_file_through_the_administrative_share_is_refused() {
    let dir = work_dir("admin-share");
    let (src, before) = source_copy(&dir);
    let canon = std::fs::canonicalize(&src).unwrap();
    let local = canon.to_string_lossy().trim_start_matches(r"\\?\").to_string();
    let Some((drive, rest)) = local.split_once(":\\") else { return };
    let unc = PathBuf::from(format!(r"\\localhost\{drive}$\{rest}"));
    if !unc.exists() {
        eprintln!("administrative share not reachable ({}): test skipped", unc.display());
        return;
    }
    let cfg = config(&src, &unc);
    let prep = prepare(&cfg, false).unwrap();
    assert!(prep.findings.iter().any(|f| f.code == "SAME_DISK" && f.blocking), "{:#?}", prep.findings);
    assert!(execute(&cfg, &dir.join("reports"), &Control::new(), &NullObserver).is_err());
    assert_eq!(hash(&src), before, "the source image was modified");
}
