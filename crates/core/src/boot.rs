//! Contrôle des éléments nécessaires au démarrage de Windows.
//!
//! Ces contrôles constatent la présence (ou l'absence) des structures attendues. Ils
//! ne peuvent pas prouver qu'un disque démarrera : cela dépend aussi du micrologiciel
//! de la machine, de ses réglages et du contenu de la configuration de démarrage.
//! Les libellés sont rédigés en conséquence.

use crate::device::BlockDevice;
use crate::diag::Finding;
use crate::fs::{self, FsKind};
use crate::partition::{PartRole, TableKind};
use crate::plan::{DiskAnalysis, SourcePartition};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BootStyle {
    /// Windows démarrant en UEFI (disque GPT, partition système EFI).
    WindowsUefi,
    /// Windows démarrant en BIOS hérité (disque MBR, partition active).
    WindowsBios,
    /// Partition EFI présente mais pas de Windows reconnu (autre système).
    OtherUefi,
    /// Aucun élément de démarrage : disque de données.
    DataOnly,
    Unknown,
}

impl BootStyle {
    pub fn label(self) -> &'static str {
        match self {
            BootStyle::WindowsUefi => t!("Windows (UEFI / GPT)", "Windows (UEFI / GPT)"),
            BootStyle::WindowsBios => t!("Windows (BIOS hérité / MBR)", "Windows (legacy BIOS / MBR)"),
            BootStyle::OtherUefi => t!("Autre système (UEFI)", "Other system (UEFI)"),
            BootStyle::DataOnly => t!("Disque de données (non amorçable)", "Data disk (not bootable)"),
            BootStyle::Unknown => t!("Indéterminé", "Undetermined"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckState {
    Present,
    Missing,
    /// Non vérifiable (volume chiffré, structure illisible).
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootCheck {
    pub key: String,
    pub label: String,
    pub state: CheckState,
    /// Vrai si l'absence de cet élément empêche vraisemblablement le démarrage.
    pub required: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootAssessment {
    pub style: BootStyle,
    pub windows_partition: Option<u32>,
    pub esp_partition: Option<u32>,
    pub checks: Vec<BootCheck>,
    pub findings: Vec<Finding>,
}

fn bounds(sp: &SourcePartition, ss: u64) -> (u64, u64) {
    (sp.part.start_lba * ss, sp.part.sectors * ss)
}

fn state(r: Result<bool, String>) -> CheckState {
    match r {
        Ok(true) => CheckState::Present,
        Ok(false) => CheckState::Missing,
        Err(_) => CheckState::Unknown,
    }
}

/// Présence d'un fichier sur un volume NTFS ou FAT.
fn file_present(dev: &mut dyn BlockDevice, sp: &SourcePartition, ss: u64, path: &[&str]) -> CheckState {
    let (base, len) = bounds(sp, ss);
    match sp.fs.kind {
        FsKind::Ntfs => state(fs::ntfs::path_exists(dev, base, len, path)),
        k if k.is_fat() => state(fs::fat::file_size(dev, base, len, &path.join("/")).map(|s| s.is_some())),
        _ => CheckState::Unknown,
    }
}

fn find_windows(dev: &mut dyn BlockDevice, a: &DiskAnalysis, loader: &str) -> Option<u32> {
    let ss = a.sector_size as u64;
    for sp in &a.parts {
        if sp.fs.kind == FsKind::Ntfs && matches!(sp.part.role(), PartRole::BasicData) {
            let (base, len) = bounds(sp, ss);
            if fs::ntfs::path_exists(dev, base, len, &["Windows", "System32", loader]).unwrap_or(false) {
                return Some(sp.part.number);
            }
        }
    }
    None
}

/// Examine les éléments de démarrage d'un disque déjà analysé. Lecture seule.
pub fn assess_boot(dev: &mut dyn BlockDevice, a: &DiskAnalysis) -> BootAssessment {
    let ss = a.sector_size as u64;
    let mut out = BootAssessment { style: BootStyle::Unknown, windows_partition: None, esp_partition: None, checks: vec![], findings: vec![] };
    let check = |out: &mut BootAssessment, key: &str, label: &str, state: CheckState, required: bool| {
        out.checks.push(BootCheck { key: key.into(), label: label.into(), state, required });
    };
    let bitlocker: Vec<u32> = a.parts.iter().filter(|p| p.fs.kind == FsKind::Bitlocker).map(|p| p.part.number).collect();

    match a.layout.kind {
        TableKind::Gpt => {
            let esp = a.parts.iter().find(|p| p.part.role() == PartRole::Esp);
            out.esp_partition = esp.map(|p| p.part.number);
            out.windows_partition = find_windows(dev, a, "winload.efi");
            let windows_likely = out.windows_partition.is_some() || !bitlocker.is_empty();

            if esp.is_none() && !windows_likely {
                out.style = BootStyle::DataOnly;
                return out;
            }
            check(
                &mut out,
                "esp",
                t!("Partition système EFI (ESP)", "EFI system partition (ESP)"),
                if esp.is_some() { CheckState::Present } else { CheckState::Missing },
                true,
            );
            if let Some(esp) = esp {
                let fat = esp.fs.kind.is_fat();
                check(
                    &mut out,
                    "esp_fat",
                    t!("ESP au format FAT", "ESP in FAT format"),
                    if fat { CheckState::Present } else { CheckState::Missing },
                    true,
                );
                if fat {
                    let bootmgr = file_present(dev, esp, ss, &["EFI", "Microsoft", "Boot", "bootmgfw.efi"]);
                    let bcd = file_present(dev, esp, ss, &["EFI", "Microsoft", "Boot", "BCD"]);
                    let fallback = file_present(dev, esp, ss, &["EFI", "Boot", "bootx64.efi"]);
                    let has_ms = bootmgr == CheckState::Present || bcd == CheckState::Present;
                    if has_ms || windows_likely {
                        check(
                            &mut out,
                            "bootmgfw",
                            t!("Gestionnaire de démarrage Windows (bootmgfw.efi)", "Windows Boot Manager (bootmgfw.efi)"),
                            bootmgr,
                            true,
                        );
                        check(&mut out, "bcd", t!("Configuration de démarrage (BCD)", "Boot configuration (BCD)"), bcd, true);
                    }
                    check(
                        &mut out,
                        "fallback",
                        t!("Chargeur de secours (\\EFI\\Boot\\bootx64.efi)", "Fallback loader (\\EFI\\Boot\\bootx64.efi)"),
                        fallback,
                        false,
                    );
                    out.style = if has_ms || windows_likely { BootStyle::WindowsUefi } else { BootStyle::OtherUefi };
                } else {
                    out.style = if windows_likely { BootStyle::WindowsUefi } else { BootStyle::OtherUefi };
                }
            } else {
                out.style = BootStyle::WindowsUefi;
            }
            if out.style == BootStyle::WindowsUefi {
                let win_state = match (out.windows_partition, bitlocker.is_empty()) {
                    (Some(_), _) => CheckState::Present,
                    (None, false) => CheckState::Unknown,
                    (None, true) => CheckState::Missing,
                };
                check(&mut out, "windows", t!("Partition Windows (winload.efi)", "Windows partition (winload.efi)"), win_state, true);
                let msr = a.parts.iter().any(|p| p.part.role() == PartRole::Msr);
                check(
                    &mut out,
                    "msr",
                    t!("Partition réservée Microsoft (MSR)", "Microsoft reserved partition (MSR)"),
                    if msr { CheckState::Present } else { CheckState::Missing },
                    false,
                );
            }
        }
        TableKind::Mbr => {
            let active = a.parts.iter().find(|p| p.part.is_active());
            out.windows_partition = find_windows(dev, a, "winload.exe");
            let boot_code = a.layout.mbr.as_ref().is_some_and(|m| m.boot_code_present);
            let windows_likely = out.windows_partition.is_some() || !bitlocker.is_empty();
            if !windows_likely && active.is_none() {
                out.style = BootStyle::DataOnly;
                return out;
            }
            out.style = if windows_likely { BootStyle::WindowsBios } else { BootStyle::Unknown };
            check(
                &mut out,
                "mbr_code",
                t!("Code d'amorçage dans le MBR", "Boot code in the MBR"),
                if boot_code { CheckState::Present } else { CheckState::Missing },
                true,
            );
            check(
                &mut out,
                "active",
                t!("Partition marquée active", "Partition marked active"),
                if active.is_some() { CheckState::Present } else { CheckState::Missing },
                true,
            );
            if let (Some(active), true) = (active, windows_likely) {
                check(
                    &mut out,
                    "bootmgr",
                    t!("Gestionnaire de démarrage (bootmgr)", "Boot manager (bootmgr)"),
                    file_present(dev, active, ss, &["bootmgr"]),
                    true,
                );
                check(
                    &mut out,
                    "bcd",
                    t!("Configuration de démarrage (\\Boot\\BCD)", "Boot configuration (\\Boot\\BCD)"),
                    file_present(dev, active, ss, &["Boot", "BCD"]),
                    true,
                );
            }
            if windows_likely {
                let win_state = match (out.windows_partition, bitlocker.is_empty()) {
                    (Some(_), _) => CheckState::Present,
                    (None, false) => CheckState::Unknown,
                    (None, true) => CheckState::Missing,
                };
                check(&mut out, "windows", t!("Partition Windows (winload.exe)", "Windows partition (winload.exe)"), win_state, true);
            }
        }
        TableKind::None => {
            out.style = BootStyle::Unknown;
            return out;
        }
    }

    for c in &out.checks {
        if c.required && c.state == CheckState::Missing {
            out.findings.push(Finding::error(
                "BOOT_ELEMENT_MISSING",
                tf!("Élément de démarrage absent : {}", "Boot element missing: {}", c.label),
                t!("Cet élément est normalement indispensable au démarrage de Windows. Sans lui, le disque ne démarrera \
                 vraisemblablement pas tel quel (une réparation du démarrage resterait possible).", "This element is normally essential for Windows to boot. Without it, the disk will most likely not boot as is (a startup repair would still be possible)."),
            ));
        }
    }
    if out.checks.iter().any(|c| c.key == "fallback" && c.state == CheckState::Missing) && out.style == BootStyle::WindowsUefi {
        out.findings.push(Finding::info(
            "BOOT_NO_FALLBACK",
            t!("Pas de chargeur UEFI de secours", "No fallback UEFI loader"),
            t!("Le fichier \\EFI\\Boot\\bootx64.efi est absent : le démarrage repose uniquement sur l'entrée « Windows Boot Manager » \
             enregistrée dans le micrologiciel de la machine. Sur une autre machine, il faudra peut-être choisir le disque \
             manuellement dans le menu de démarrage.", "The file \\EFI\\Boot\\bootx64.efi is missing: booting relies only on the \"Windows Boot Manager\" entry stored in this machine's firmware. On another machine you may have to pick the disk manually in the boot menu."),
        ));
    }
    if !bitlocker.is_empty() {
        out.findings.push(Finding::warning(
            "BITLOCKER_PRESENT",
            t!("Volume chiffré BitLocker", "BitLocker-encrypted volume"),
            tf!("La ou les partitions {:?} sont chiffrées avec BitLocker. Elles sont copiées telles quelles, sans pouvoir être \
                 vérifiées de l'intérieur. Après un changement de disque, Windows peut exiger la clé de récupération BitLocker : \
                 assurez-vous de l'avoir avant de retirer l'ancien disque.", "Partition(s) {:?} are encrypted with BitLocker. They are copied as is, without being verifiable from the inside. After a disk change, Windows may ask for the BitLocker recovery key: make sure you have it before removing the old disk.",
                bitlocker
            ),
        ));
    }
    out
}

/// Compare les éléments de démarrage de la source et du clone.
pub fn compare_boot(src: &BootAssessment, dst: &BootAssessment) -> Vec<Finding> {
    let mut out = Vec::new();
    for c in &src.checks {
        if c.state != CheckState::Present {
            continue;
        }
        let d = dst.checks.iter().find(|d| d.key == c.key).map(|d| d.state).unwrap_or(CheckState::Missing);
        if d != CheckState::Present {
            let f = Finding::new(
                if c.required { crate::diag::Severity::Error } else { crate::diag::Severity::Warning },
                "BOOT_ELEMENT_LOST",
                tf!("Élément de démarrage non retrouvé sur le clone : {}", "Boot element not found on the clone: {}", c.label),
                t!(
                    "Cet élément est présent sur la source mais n'a pas été retrouvé sur la destination après le clonage.",
                    "This element is present on the source but was not found on the destination after cloning."
                ),
            );
            out.push(f);
        }
    }
    if src.style != dst.style {
        out.push(Finding::warning(
            "BOOT_STYLE_DIFFERS",
            t!("Mode de démarrage différent sur le clone", "Different boot mode on the clone"),
            tf!("Source : {}. Clone : {}.", "Source: {}. Clone: {}.", src.style.label(), dst.style.label()),
        ));
    }
    out
}
