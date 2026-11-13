//! Lecture des tables de partitions et des systèmes de fichiers sur de vraies images.

mod common;

use chud_clone_core::device::MemDevice;
use chud_clone_core::fs::{self, FsKind};
use chud_clone_core::partition::{PartInfo, PartRole, TableKind, read_layout};

const SS: u64 = 512;

#[test]
fn gpt_windows_layout_is_read_correctly() {
    let mut dev = common::open_fixture("gpt-win.img");
    let l = read_layout(&mut dev).unwrap();
    assert_eq!(l.kind, TableKind::Gpt);
    assert!(l.findings.is_empty(), "{:#?}", l.findings);
    let gpt = l.gpt.as_ref().unwrap();
    assert_eq!(gpt.disk_guid.to_string(), "5A5A5A5A-1111-2222-3333-0123456789AB");
    assert!(gpt.primary_valid && gpt.backup_valid && gpt.backup_at_end);
    let roles: Vec<PartRole> = l.partitions.iter().map(|p| p.role()).collect();
    assert_eq!(roles, vec![PartRole::Esp, PartRole::Msr, PartRole::BasicData, PartRole::Recovery]);
    assert_eq!(l.partitions[0].start_lba, 2048);
    assert_eq!(l.partitions[0].sectors, 40 * 2048);
    assert_eq!(l.partitions[2].sectors, 96 * 2048);
    match &l.partitions[3].info {
        PartInfo::Gpt { attributes, unique_guid, .. } => {
            assert_eq!(*attributes, 0x8000_0000_0000_0001);
            assert_eq!(unique_guid.to_string(), "44444444-AAAA-4AAA-8AAA-000000000004");
        }
        _ => panic!("GPT attendu"),
    }
    assert_eq!(l.partitions[0].name(), Some("EFI system partition"));
}

#[test]
fn mbr_layout_with_logical_partitions() {
    let mut dev = common::open_fixture("mbr-win.img");
    let l = read_layout(&mut dev).unwrap();
    assert_eq!(l.kind, TableKind::Mbr);
    assert!(l.findings.is_empty(), "{:#?}", l.findings);
    let m = l.mbr.as_ref().unwrap();
    assert_eq!(m.signature, 0x1BADF00D);
    assert!(m.boot_code_present);
    assert_eq!(m.extended, Some((165888, 90112)));
    assert_eq!(m.ebr_lbas.len(), 2);
    let v: Vec<(u32, u64, u64, bool)> = l.partitions.iter().map(|p| (p.number, p.start_lba, p.sectors, p.is_logical())).collect();
    assert_eq!(v, vec![(1, 2048, 65536, false), (2, 67584, 98304, false), (5, 167936, 40960, true), (6, 210944, 40960, true)]);
    assert!(l.partitions[0].is_active());
}

#[test]
fn filesystems_are_identified_and_sized() {
    let mut dev = common::open_fixture("gpt-win.img");
    let l = read_layout(&mut dev).unwrap();
    let kinds: Vec<FsKind> = l.partitions.iter().map(|p| fs::analyze(&mut dev, p.start_lba * SS, p.sectors * SS).kind).collect();
    assert_eq!(kinds, vec![FsKind::Fat32, FsKind::Unknown, FsKind::Ntfs, FsKind::Ntfs]);

    let win = &l.partitions[2];
    let a = fs::analyze(&mut dev, win.start_lba * SS, win.sectors * SS);
    assert!(a.findings.is_empty(), "{:#?}", a.findings);
    assert_eq!(a.label.as_deref(), Some("Windows"));
    assert_eq!(a.cluster_size, Some(4096));
    // NTFS réserve le dernier secteur de la partition au secteur d'amorçage de secours.
    assert_eq!(a.fs_size, Some(win.sectors * SS - SS));
    let used = a.used_bytes.unwrap();
    assert!(used > 8 << 20 && used < 40 << 20, "utilisé = {used}");
    let ext = a.used_extents.expect("volume propre");
    assert!(ext.len() > 10, "la fragmentation du jeu de test doit produire plusieurs étendues");
    assert_eq!(ext[0].offset, 0);
    assert_eq!(ext.last().unwrap().end(), win.sectors * SS, "le secteur d'amorçage de secours est inclus");

    let mut dev = common::open_fixture("mbr-win.img");
    let l = read_layout(&mut dev).unwrap();
    let kinds: Vec<FsKind> = l.partitions.iter().map(|p| fs::analyze(&mut dev, p.start_lba * SS, p.sectors * SS).kind).collect();
    assert_eq!(kinds, vec![FsKind::Ntfs, FsKind::Ntfs, FsKind::Fat16, FsKind::Unknown]);
}

/// Preuve de suffisance de la carte d'allocation : une copie ne contenant QUE les
/// étendues déclarées utilisées (le reste étant rempli de déchets) doit redonner
/// exactement les mêmes fichiers, octet pour octet.
#[test]
fn ntfs_used_extents_are_sufficient_to_read_every_file() {
    for (image, part_index) in [("gpt-win.img", 2usize), ("gpt-win.img", 3), ("mbr-win.img", 0), ("mbr-win.img", 1)] {
        let img = common::load_fixture(image);
        let mut dev = MemDevice::from_vec(img.clone(), 512);
        let l = read_layout(&mut dev).unwrap();
        let p = &l.partitions[part_index];
        let (base, len) = (p.start_lba * SS, p.sectors * SS);
        let a = fs::analyze(&mut dev, base, len);
        let ext = a.used_extents.unwrap_or_else(|| panic!("{image} p{part_index}: {:?}", a.raw_reason));

        let mut sparse = vec![0xEEu8; img.len()];
        for e in &ext {
            let (o, n) = ((base + e.offset) as usize, e.len as usize);
            sparse[o..o + n].copy_from_slice(&img[o..o + n]);
        }
        let mut copy = MemDevice::from_vec(sparse, 512);

        let files = fs::ntfs::list_files(&mut dev, base, len).unwrap();
        let files_copy = fs::ntfs::list_files(&mut copy, base, len).unwrap();
        assert_eq!(files, files_copy, "{image} p{part_index}: mêmes fichiers");
        assert!(files.len() >= 2, "{image} p{part_index}: {} fichiers", files.len());
        for (path, size) in &files {
            let comps: Vec<&str> = path.split('/').collect();
            let a = fs::ntfs::read_file(&mut dev, base, len, &comps, usize::MAX).unwrap().unwrap();
            let b = fs::ntfs::read_file(&mut copy, base, len, &comps, usize::MAX).unwrap().unwrap();
            assert_eq!(a.len() as u64, *size, "{path}");
            assert!(a == b, "{image} p{part_index}: contenu différent pour {path}");
        }
        // Le second passage d'analyse sur la copie doit donner la même carte.
        let a2 = fs::analyze(&mut copy, base, len);
        assert_eq!(a2.used_extents.unwrap(), ext);
        assert_eq!(copy.writes, 0);
    }
}

#[test]
fn known_files_are_found() {
    let mut dev = common::open_fixture("gpt-win.img");
    let l = read_layout(&mut dev).unwrap();
    let esp = &l.partitions[0];
    let win = &l.partitions[2];
    assert!(fs::fat::file_size(&mut dev, esp.start_lba * SS, esp.sectors * SS, "EFI/Microsoft/Boot/bootmgfw.efi").unwrap().is_some());
    assert!(fs::fat::file_size(&mut dev, esp.start_lba * SS, esp.sectors * SS, "EFI/Microsoft/Boot/BCD").unwrap().is_some());
    assert!(fs::ntfs::path_exists(&mut dev, win.start_lba * SS, win.sectors * SS, &["Windows", "System32", "winload.efi"]).unwrap());
    assert!(fs::ntfs::path_exists(&mut dev, win.start_lba * SS, win.sectors * SS, &["WINDOWS", "system32", "WinLoad.EFI"]).unwrap());
    assert!(!fs::ntfs::path_exists(&mut dev, win.start_lba * SS, win.sectors * SS, &["Windows", "System32", "absent.dll"]).unwrap());
    let note = fs::ntfs::read_file(&mut dev, win.start_lba * SS, win.sectors * SS, &["Users", "Test", "note.txt"], 1000).unwrap().unwrap();
    assert_eq!(String::from_utf8(note).unwrap(), "petit fichier résident dans la MFT\n");
}

#[test]
fn dirty_ntfs_is_never_smart_copied() {
    let img = common::load_fixture("ntfs-dirty.img");
    let len = img.len() as u64;
    let mut dev = MemDevice::from_vec(img, 512);
    let a = fs::analyze(&mut dev, 0, len);
    assert_eq!(a.kind, FsKind::Ntfs);
    assert!(a.used_extents.is_none(), "un volume marqué dirty doit être copié en brut");
    assert!(a.findings.iter().any(|f| f.code == "NTFS_DIRTY"), "{:#?}", a.findings);
    assert!(a.used_bytes.is_some(), "l'espace utilisé reste affichable");
}

#[test]
fn unclean_log_and_hibernation_force_raw_copy() {
    let img = common::load_fixture("gpt-win.img");
    let mut dev = MemDevice::from_vec(img.clone(), 512);
    let l = read_layout(&mut dev).unwrap();
    let p = &l.partitions[2];
    let (base, len) = ((p.start_lba * SS) as usize, (p.sectors * SS) as usize);

    // Localise $LogFile via sa propre carte d'exécution n'est pas nécessaire : après un
    // démontage par ntfs-3g le journal est rempli de 0xFF. On y écrit une page de
    // redémarrage « client actif, non propre », comme le laisse un arrêt brutal.
    let part = &img[base..base + len];
    let pos = find_ff_run(part, 1 << 20).expect("journal vide introuvable");
    let mut bad = img.clone();
    for page in 0..2usize {
        let o = base + pos + page * 4096;
        bad[o..o + 4096].fill(0);
        bad[o..o + 4].copy_from_slice(b"RSTR");
        bad[o + 16..o + 20].copy_from_slice(&4096u32.to_le_bytes());
        bad[o + 24..o + 26].copy_from_slice(&0x30u16.to_le_bytes());
        bad[o + 0x30..o + 0x38].copy_from_slice(&(77u64 + page as u64).to_le_bytes());
    }
    let mut dev = MemDevice::from_vec(bad, 512);
    let a = fs::analyze(&mut dev, base as u64, len as u64);
    assert!(a.used_extents.is_none());
    assert!(a.findings.iter().any(|f| f.code == "NTFS_LOG_UNCLEAN"), "{:#?}", a.findings);
}

/// Cherche le début d'une longue plage de 0xFF alignée sur 4 Kio (le journal vidé).
fn find_ff_run(data: &[u8], min_len: usize) -> Option<usize> {
    let mut start = None;
    let mut i = 0;
    while i + 4096 <= data.len() {
        if data[i..i + 4096].iter().all(|b| *b == 0xFF) {
            let s = *start.get_or_insert(i);
            if i + 4096 - s >= min_len {
                return Some(s);
            }
        } else {
            start = None;
        }
        i += 4096;
    }
    None
}
