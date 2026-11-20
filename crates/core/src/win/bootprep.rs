//! Windows side of the boot preparation (see `crate::bootprep`).
//!
//! 1. Direct phase: the disk's volumes are locked and dismounted, then the saved
//!    session and the boot drivers are fixed in place on the Windows volume
//!    ([`crate::bootprep::prepare_volume`]). Works on a disk Windows keeps offline.
//! 2. Boot configuration: when the disk is online, its Windows partition and system
//!    partition get a temporary drive letter; the BCD is made to designate the disk's
//!    own partitions (`bcdedit /store`, or `bcdboot` without touching this PC's firmware
//!    entries when the store is missing), and the fallback UEFI loader is added if
//!    missing. The temporary letters are removed at the end.
//!
//! A disk kept offline because it carries the same identifiers as another connected
//! disk is not brought online: Windows would change its identifiers, which is exactly
//! what breaks the boot of a clone.

use super::enumerate::list_volumes;
use super::wide;
use crate::boot::{BootStyle, assess_boot};
use crate::bootprep::{BootPrepOutcome, prepare_volume};
use crate::partition::PartRole;
use crate::plan::analyze_disk;
use std::path::PathBuf;
use std::process::Command;
use windows_sys::Win32::Storage::FileSystem::{DeleteVolumeMountPointW, GetLogicalDrives, SetVolumeMountPointW};

/// Partitions to work on, by byte offset on the disk.
struct Targets {
    style: BootStyle,
    windows_offset: u64,
    windows_len: u64,
    system_offset: u64,
}

fn find_targets(number: u32) -> Result<Targets, String> {
    let mut dev = super::rawdisk::RawDisk::open_read_only(&format!(r"\\.\PhysicalDrive{number}"))
        .map_err(|e| tf!("lecture du disque impossible : {e}", "cannot read the disk: {e}"))?;
    let a = analyze_disk(&mut dev).map_err(|e| tf!("analyse du disque impossible : {e}", "disk analysis failed: {e}"))?;
    let boot = assess_boot(&mut dev, &a);
    let ss = a.sector_size as u64;
    let part = |n: u32| a.parts.iter().find(|p| p.part.number == n).map(|p| (p.part.start_lba * ss, p.part.sectors * ss));
    let (windows, windows_len) = boot.windows_partition.and_then(part).ok_or(t!(
        "aucune installation de Windows trouvée sur ce disque (dossier \\Windows\\System32 avec son chargeur de démarrage)",
        "no Windows installation found on this disk (\\Windows\\System32 folder with its boot loader)"
    ))?;
    let system = match boot.style {
        BootStyle::WindowsUefi => boot.esp_partition.and_then(part).map(|p| p.0).ok_or(t!(
            "pas de partition système EFI sur ce disque",
            "no EFI system partition on this disk"
        ))?,
        BootStyle::WindowsBios => a
            .parts
            .iter()
            .find(|p| p.part.is_active())
            .or_else(|| a.parts.iter().find(|p| p.part.role() == PartRole::BasicData && p.part.start_lba * ss == windows))
            .map(|p| p.part.start_lba * ss)
            .unwrap_or(windows),
        _ => return Err(t!("ce disque n'est pas un disque de démarrage Windows", "this disk is not a Windows boot disk").into()),
    };
    Ok(Targets { style: boot.style, windows_offset: windows, windows_len, system_offset: system })
}

/// Volume path and existing drive letter of the partition starting at `offset`.
fn volume_at(number: u32, offset: u64) -> Option<(String, Option<char>)> {
    list_volumes().into_iter().find(|v| v.extents.iter().any(|e| e.0 == number && e.1 == offset)).map(|v| {
        let letter = v.mounts.iter().find_map(|m| {
            let b = m.as_bytes();
            (b.len() >= 2 && b.len() <= 3 && b[1] == b':' && b[0].is_ascii_alphabetic()).then(|| (b[0] as char).to_ascii_uppercase())
        });
        (v.guid_path, letter)
    })
}

fn free_letter(taken: &[char]) -> Option<char> {
    // SAFETY: no argument; returns the bitmask of the drive letters in use.
    let used = unsafe { GetLogicalDrives() };
    ('D'..='Z').rev().find(|c| used & (1 << (*c as u32 - 'A' as u32)) == 0 && !taken.contains(c))
}

/// Drive letter of a partition; a temporary one is removed when dropped.
struct Mounted {
    letter: char,
    added: bool,
}

impl Mounted {
    fn root(&self) -> PathBuf {
        PathBuf::from(format!("{}:\\", self.letter))
    }
    fn drive(&self) -> String {
        format!("{}:", self.letter)
    }
}

impl Drop for Mounted {
    fn drop(&mut self) {
        if self.added {
            let mp = wide(&format!("{}:\\", self.letter));
            // SAFETY: NUL-terminated mount point that this program created.
            unsafe { DeleteVolumeMountPointW(mp.as_ptr()) };
        }
    }
}

fn mount(number: u32, offset: u64, taken: &[char], what: &str) -> Result<Mounted, String> {
    let (guid_path, letter) = volume_at(number, offset).ok_or(tf!(
        "la partition {what} n'est pas reconnue par Windows (volume introuvable)",
        "the {what} partition is not recognised by Windows (volume not found)"
    ))?;
    if let Some(letter) = letter {
        return Ok(Mounted { letter, added: false });
    }
    let letter = free_letter(taken).ok_or(t!("aucune lettre de lecteur libre", "no free drive letter"))?;
    let mp = wide(&format!("{letter}:\\"));
    let vol = wide(&guid_path);
    // SAFETY: both strings are NUL-terminated and outlive the call.
    if unsafe { SetVolumeMountPointW(mp.as_ptr(), vol.as_ptr()) } == 0 {
        return Err(tf!(
            "impossible d'attribuer une lettre à la partition {what} : {}",
            "cannot give a drive letter to the {what} partition: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(Mounted { letter, added: true })
}

/// Runs a system tool; returns (success, output).
fn run(program: &str, args: &[&str]) -> (bool, String) {
    match Command::new(program).args(args).output() {
        Ok(o) => {
            let mut text = String::from_utf8_lossy(&o.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&o.stderr));
            (o.status.success(), text.replace('\r', "").trim().to_string())
        }
        Err(e) => (false, e.to_string()),
    }
}

fn last_line(text: &str) -> String {
    text.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("").to_string()
}

/// Makes the BCD designate the disk's own partitions.
fn fix_bcd(t: &Targets, win: &Mounted, sys: &Mounted, out: &mut BootPrepOutcome) -> Result<(), String> {
    let store = match t.style {
        BootStyle::WindowsUefi => sys.root().join(r"EFI\Microsoft\Boot\BCD"),
        _ => sys.root().join(r"Boot\BCD"),
    };
    let store_s = store.to_string_lossy().to_string();
    let win_dev = format!("partition={}", win.drive());
    let sys_dev = format!("partition={}", sys.drive());
    if store.exists() {
        let steps: [(&str, &str, &str); 3] = [("{bootmgr}", "device", &sys_dev), ("{default}", "device", &win_dev), ("{default}", "osdevice", &win_dev)];
        let mut ok = true;
        for (obj, elem, value) in steps {
            let (good, text) = run("bcdedit", &["/store", &store_s, "/set", obj, elem, value]);
            if !good {
                out.steps.push(tf!("bcdedit {obj} {elem} : {}", "bcdedit {obj} {elem}: {}", last_line(&text)));
                ok = false;
                break;
            }
        }
        if ok {
            // Optional entry: its failure does not matter.
            let _ = run("bcdedit", &["/store", &store_s, "/set", "{memdiag}", "device", &sys_dev]);
            let (_, check) = run("bcdedit", &["/store", &store_s, "/enum", "{default}"]);
            if check.to_ascii_lowercase().contains(&win_dev.to_ascii_lowercase()) {
                out.bcd_fixed = true;
                out.bcd_method = "bcdedit".into();
                return Ok(());
            }
            out.steps.push(t!("le BCD relu ne désigne pas la partition Windows", "the BCD read back does not designate the Windows partition").into());
        }
    }
    // No usable store: rebuild it from the clone's own boot files.
    let source = win.root().join("Windows").to_string_lossy().to_string();
    let firmware = if t.style == BootStyle::WindowsUefi { "UEFI" } else { "BIOS" };
    let sys_drive = sys.drive();
    let (ok, text) = run("bcdboot", &[&source, "/s", &sys_drive, "/f", firmware, "/nofirmwaresync"]);
    let (ok, text) = if ok {
        (ok, text)
    } else {
        // Older bcdboot without /nofirmwaresync: the entry is then added last, so this
        // PC's boot order does not change.
        out.steps.push(tf!("bcdboot /nofirmwaresync : {}", "bcdboot /nofirmwaresync: {}", last_line(&text)));
        run("bcdboot", &[&source, "/s", &sys_drive, "/f", firmware, "/addlast"])
    };
    if !ok {
        return Err(tf!("bcdboot a échoué : {}", "bcdboot failed: {}", last_line(&text)));
    }
    out.bcd_fixed = true;
    out.bcd_method = "bcdboot".into();
    Ok(())
}

/// Direct phase: volumes locked and dismounted, fixes written in place.
fn direct_phase(number: u32, t: &Targets, out: &mut BootPrepOutcome, log: &dyn Fn(&str)) -> Result<(), String> {
    let mut locks = Vec::new();
    for (guid_path, display) in super::enumerate::volumes_of_disk(number) {
        let lock = super::rawdisk::lock_volume_tries(&guid_path, &display, 6).map_err(|e| {
            tf!(
                "le volume {display} est utilisé (fermez les fenêtres et programmes qui l'ouvrent) : {e}",
                "volume {display} is in use (close the windows and programs that open it): {e}"
            )
        })?;
        locks.push(lock);
    }
    let mut dev = super::rawdisk::RawDisk::open_read_write(&format!(r"\\.\PhysicalDrive{number}"))
        .map_err(|e| tf!("ouverture du disque en écriture impossible : {e}", "cannot open the disk for writing: {e}"))?;
    prepare_volume(&mut dev, t.windows_offset, t.windows_len, out, log);
    drop(dev);
    drop(locks);
    Ok(())
}

/// Prepares the Windows of disk `number` to boot on another PC. `log` receives each
/// step as it happens.
pub fn prepare_disk(number: u32, log: &dyn Fn(&str)) -> Result<BootPrepOutcome, String> {
    let mut out = BootPrepOutcome::default();
    let note = |out: &mut BootPrepOutcome, s: String| {
        log(&s);
        out.steps.push(s);
    };
    let t = find_targets(number)?;
    note(&mut out, tf!("Démarrage : {}", "Boot: {}", t.style.label()));

    direct_phase(number, &t, &mut out, log)?;

    // Boot configuration: only on a disk that Windows has online.
    let online = (0..20).any(|i| {
        if i > 0 {
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        volume_at(number, t.windows_offset).is_some() && volume_at(number, t.system_offset).is_some()
    });
    if !online {
        out.warnings.push(t!(
            "configuration de démarrage non vérifiée : Windows garde ce disque hors ligne, sans doute parce qu'un autre disque branché porte \
             les mêmes identifiants (la source du clone). Ses identifiants sont donc intacts et son BCD reste valable ; pour le vérifier quand \
             même, débranchez l'autre disque puis relancez la réparation. Ne remettez pas ce disque en ligne tant que l'autre est branché.",
            "boot configuration not checked: Windows keeps this disk offline, most likely because another connected disk carries the same \
             identifiers (the clone's source). Its identifiers are therefore intact and its BCD stays valid; to check it anyway, unplug the \
             other disk and run the repair again. Do not bring this disk online while the other one is connected."
        )
        .into());
        return Ok(out);
    }

    let win = mount(number, t.windows_offset, &[], "Windows")?;
    let sys = if t.system_offset == t.windows_offset {
        Mounted { letter: win.letter, added: false }
    } else {
        mount(number, t.system_offset, &[win.letter], t!("système", "system"))?
    };
    note(&mut out, tf!("Partition Windows : {} — partition système : {}", "Windows partition: {} — system partition: {}", win.drive(), sys.drive()));
    match fix_bcd(&t, &win, &sys, &mut out) {
        Ok(()) => {
            let s = tf!("Configuration de démarrage mise à jour ({})", "Boot configuration updated ({})", out.bcd_method);
            note(&mut out, s);
        }
        Err(e) => out.warnings.push(tf!("configuration de démarrage non mise à jour : {e}", "boot configuration not updated: {e}")),
    }
    // A new PC has no firmware entry for this disk: it starts the fallback loader.
    if t.style == BootStyle::WindowsUefi {
        let fallback = sys.root().join(r"EFI\Boot\bootx64.efi");
        let bootmgr = sys.root().join(r"EFI\Microsoft\Boot\bootmgfw.efi");
        if !fallback.exists() && bootmgr.exists() {
            match std::fs::create_dir_all(sys.root().join(r"EFI\Boot")).and_then(|_| std::fs::copy(&bootmgr, &fallback)) {
                Ok(_) => {
                    out.fallback_added = true;
                    note(&mut out, t!("Chargeur UEFI de secours ajouté (\\EFI\\Boot\\bootx64.efi)", "Fallback UEFI loader added (\\EFI\\Boot\\bootx64.efi)").into());
                }
                Err(e) => out.warnings.push(tf!("chargeur UEFI de secours non ajouté : {e}", "fallback UEFI loader not added: {e}")),
            }
        }
    }
    drop(sys);
    drop(win);
    Ok(out)
}
