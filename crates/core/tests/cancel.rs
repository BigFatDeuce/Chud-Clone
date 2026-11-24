//! Cancelling before the first write: the destination keeps every byte.
//!
//! Cancelling is possible as soon as the operation is started, while the disks are
//! read again, locked and checked (health readings can take seconds over USB). Until the
//! destination is opened for writing, a cancellation must leave it untouched.

mod common;

use chud_clone_core::engine::{Control, EngineOptions, NullObserver};
use chud_clone_core::job::{JobConfig, JobError, Target, execute};
use chud_clone_core::plan::PlanOptions;
use std::path::Path;

fn hash(path: &Path) -> String {
    blake3::hash(&std::fs::read(path).unwrap()).to_hex().to_string()
}

#[test]
fn cancelled_before_the_first_write_leaves_the_destination_intact() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("cancel-early");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // An existing destination with content of its own (another disk image).
    let dst = dir.join("destination.img");
    std::fs::copy(common::fixture("mbr-win.img"), &dst).unwrap();
    let before = hash(&dst);
    let len = std::fs::metadata(&dst).unwrap().len();

    let cfg = JobConfig {
        source: Target::Image(common::fixture("gpt-win.img")),
        destination: Target::Image(dst.clone()),
        plan: PlanOptions::default(),
        engine: EngineOptions::default(),
        new_image_size: None,
        allow_unvalidated_physical_write: false,
        expected_source_fingerprint: None,
        expected_destination_fingerprint: None,
        app_paths: vec![],
        prepare_boot: false,
    };
    let ctl = Control::new();
    ctl.cancel();
    match execute(&cfg, &dir.join("reports"), &ctl, &NullObserver) {
        Err(JobError::Failed(m)) => assert!(m.contains("annul") || m.contains("cancel"), "{m}"),
        Err(JobError::Blocked(f)) => panic!("blocked instead of cancelled: {f:#?}"),
        Ok(r) => panic!("the clone started although it was cancelled: {:?}", r.outcome.status),
    }
    assert_eq!(std::fs::metadata(&dst).unwrap().len(), len);
    assert_eq!(hash(&dst), before, "the destination was modified after the cancellation");
}
