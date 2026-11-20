//! Preparing a Windows disk to boot on another computer ("repair boot").
//!
//! Three things make a cloned Windows stop at a blue screen on its new PC:
//!
//! - Windows was hibernated or shut down with "fast startup" (the default): it tries to
//!   resume a session saved on other hardware;
//! - the storage controller of the new PC (NVMe, AHCI instead of RAID) needs a driver
//!   that Windows does not load at boot (INACCESSIBLE_BOOT_DEVICE, then the automatic
//!   repair screen);
//! - its boot configuration (BCD) designates partitions by their identifiers; when
//!   Windows changed the clone's identifiers (clone brought "online" while the source
//!   was connected), the BCD points to partitions that no longer exist (0xc000000e).
//!
//! The first two are fixed here by writing a few bytes directly on the volume, in
//! place: the header of `hiberfil.sys` is cleared, and the `Start` values of the
//! standard NVMe and AHCI drivers are set to "boot" in the offline `SYSTEM` hive. This
//! needs neither Windows tools nor a mounted volume, so it also works at the end of a
//! clone, while the clone is still held, and on a disk Windows keeps offline. The BCD
//! is handled on Windows by `win::bootprep` (`bcdedit` / `bcdboot`).

use crate::device::{AlignedBuf, BlockDevice};
use serde::{Deserialize, Serialize};

/// Storage drivers set to start at boot, so that Windows finds its disk whatever the
/// controller of the new PC (standard NVMe, SATA in AHCI mode). Both ship with every
/// Windows 10 and 11; loading them on a PC without that hardware is harmless.
pub const BOOT_DRIVERS: [&str; 2] = ["stornvme", "storahci"];

/// Bytes read at the start of `hiberfil.sys`.
pub const HIBER_HEAD: usize = 8;

/// Path of the offline SYSTEM hive inside the Windows volume.
const SYSTEM_HIVE: [&str; 4] = ["Windows", "System32", "config", "SYSTEM"];

/// Tells whether the start of `hiberfil.sys` describes a saved session (hibernation
/// or "fast startup"). After a normal start, Windows clears this header.
pub fn hiber_active(head: &[u8]) -> bool {
    head.len() >= 4 && matches!(&head[..4], b"HIBR" | b"WAKE" | b"RSTR")
}

/// What one boot preparation did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootPrepOutcome {
    /// Boot configuration rewritten to designate the disk's own partitions.
    pub bcd_fixed: bool,
    /// How: "bcdedit" (existing BCD updated) or "bcdboot" (BCD rebuilt).
    pub bcd_method: String,
    /// Drivers set to start at boot (`stornvme`, `storahci`).
    pub drivers: Vec<String>,
    /// A saved session (hibernation, fast startup) was cancelled.
    pub hibernation_cleared: bool,
    /// `\EFI\Boot\bootx64.efi` was missing and has been added: a PC without a
    /// firmware entry for this disk starts it anyway.
    #[serde(default)]
    pub fallback_added: bool,
    /// Steps that could not be done (each one explained).
    pub warnings: Vec<String>,
    /// Detailed journal of the operation.
    pub steps: Vec<String>,
}

impl BootPrepOutcome {
    /// One-line summary for the journal and the interfaces.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if self.bcd_fixed {
            parts.push(t!("configuration de démarrage (BCD) mise à jour", "boot configuration (BCD) updated").to_string());
        }
        if !self.drivers.is_empty() {
            parts.push(tf!("pilotes de stockage activés au démarrage ({})", "storage drivers enabled at boot ({})", self.drivers.join(", ")));
        }
        if self.hibernation_cleared {
            parts.push(t!("session en veille prolongée annulée", "hibernated session cancelled").to_string());
        }
        if self.fallback_added {
            parts.push(t!("chargeur UEFI de secours ajouté", "fallback UEFI loader added").to_string());
        }
        if parts.is_empty() {
            t!("aucune modification nécessaire", "no change needed").to_string()
        } else {
            parts.join(" ; ")
        }
    }
}

// ---------------------------------------------------------------- Offline hive

/// One 32-bit value to change in a hive file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HivePatch {
    /// Offset of the 4 bytes in the hive file.
    pub offset: usize,
    pub old: u32,
    pub new: u32,
    /// Driver concerned.
    pub driver: String,
}

fn utf16(raw: &[u8]) -> String {
    String::from_utf16_lossy(&raw.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect::<Vec<_>>())
}
fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]]))
}
fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// Read-only view of a registry hive file ("regf").
struct Hive<'a> {
    b: &'a [u8],
}

const HBIN_START: usize = 4096;
/// Key name stored in ASCII (KEY_COMP_NAME), value name stored in ASCII (VALUE_COMP_NAME).
const KEY_COMP_NAME: u16 = 0x20;
const VALUE_COMP_NAME: u16 = 0x01;
const REG_DWORD: u32 = 4;

impl<'a> Hive<'a> {
    fn open(b: &'a [u8]) -> Result<Self, String> {
        if b.len() < HBIN_START + 32 || &b[..4] != b"regf" {
            return Err(t!("fichier de registre non reconnu", "registry file not recognised").into());
        }
        // Different sequence numbers: Windows was writing the hive; its logs hold the rest.
        if u32_at(b, 4) != u32_at(b, 8) {
            return Err(t!(
                "registre en cours d'écriture au moment de l'arrêt (journaux en attente)",
                "registry being written when Windows stopped (pending logs)"
            )
            .into());
        }
        Ok(Self { b })
    }

    /// Data of the cell at `off` (offset from the first hbin), without its size field.
    fn cell(&self, off: u32) -> Option<(usize, &'a [u8])> {
        let start = HBIN_START.checked_add(off as usize)?;
        let size = u32_at(self.b, start)? as i32;
        // Allocated cells have a negative size.
        let len = size.checked_neg().filter(|l| *l >= 8)? as usize;
        let data = self.b.get(start + 4..start + len)?;
        Some((start + 4, data))
    }

    fn root(&self) -> Option<u32> {
        u32_at(self.b, 36)
    }

    fn key_name(nk: &[u8]) -> Option<String> {
        let flags = u16_at(nk, 2)?;
        let len = u16_at(nk, 72)? as usize;
        let raw = nk.get(76..76 + len)?;
        Some(if flags & KEY_COMP_NAME != 0 {
            raw.iter().map(|c| *c as char).collect()
        } else {
            utf16(raw)
        })
    }

    /// Subkeys of a key: (name, cell offset).
    fn subkeys(&self, key: u32) -> Vec<(String, u32)> {
        let mut out = Vec::new();
        let Some((_, nk)) = self.cell(key) else { return out };
        if nk.get(..2) != Some(b"nk") {
            return out;
        }
        let (Some(count), Some(list)) = (u32_at(nk, 20), u32_at(nk, 28)) else { return out };
        if count == 0 || list == u32::MAX {
            return out;
        }
        let mut offsets = Vec::new();
        self.list_offsets(list, &mut offsets, 0);
        for o in offsets {
            if let Some((_, child)) = self.cell(o)
                && child.get(..2) == Some(b"nk")
                && let Some(name) = Self::key_name(child)
            {
                out.push((name, o));
            }
        }
        out
    }

    /// Key offsets of a subkey list (lf, lh, li, or ri made of such lists).
    fn list_offsets(&self, list: u32, out: &mut Vec<u32>, depth: u32) {
        let Some((_, l)) = self.cell(list) else { return };
        let Some(n) = u16_at(l, 2).map(|n| n as usize) else { return };
        match l.get(..2) {
            Some(b"lf") | Some(b"lh") => out.extend((0..n).filter_map(|i| u32_at(l, 4 + i * 8))),
            Some(b"li") => out.extend((0..n).filter_map(|i| u32_at(l, 4 + i * 4))),
            Some(b"ri") if depth == 0 => {
                for i in 0..n {
                    if let Some(sub) = u32_at(l, 4 + i * 4) {
                        self.list_offsets(sub, out, 1);
                    }
                }
            }
            _ => {}
        }
    }

    fn subkey(&self, key: u32, name: &str) -> Option<u32> {
        self.subkeys(key).into_iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, o)| o)
    }

    /// REG_DWORD values of a key stored inline: (name, file offset of the data, value).
    fn dword_values(&self, key: u32) -> Vec<(String, usize, u32)> {
        let mut out = Vec::new();
        let Some((_, nk)) = self.cell(key) else { return out };
        let (Some(count), Some(list)) = (u32_at(nk, 36), u32_at(nk, 40)) else { return out };
        if count == 0 || list == u32::MAX {
            return out;
        }
        let Some((_, vl)) = self.cell(list) else { return out };
        for i in 0..count as usize {
            let Some(vo) = u32_at(vl, i * 4) else { break };
            let Some((vstart, vk)) = self.cell(vo) else { continue };
            if vk.get(..2) != Some(b"vk") {
                continue;
            }
            let (Some(name_len), Some(size), Some(data), Some(kind), Some(flags)) = (u16_at(vk, 2), u32_at(vk, 4), u32_at(vk, 8), u32_at(vk, 12), u16_at(vk, 16))
            else {
                continue;
            };
            // Inline data: high bit of the size set, 4 bytes in the data offset field.
            if kind != REG_DWORD || size != 0x8000_0004 {
                continue;
            }
            let raw = vk.get(20..20 + name_len as usize).unwrap_or_default();
            let name = if flags & VALUE_COMP_NAME != 0 {
                raw.iter().map(|c| *c as char).collect()
            } else {
                utf16(raw)
            };
            out.push((name, vstart + 8, data));
        }
        out
    }
}

/// Changes that make the boot drivers start at boot in every control set of a SYSTEM
/// hive: `Start` set to 0, and the `StartOverride` values that disable them set to 0.
/// Values already at 0 are left alone.
pub fn boot_driver_patches(hive: &[u8]) -> Result<Vec<HivePatch>, String> {
    let h = Hive::open(hive)?;
    let root = h.root().ok_or(t!("registre sans clé racine", "registry without a root key"))?;
    let sets: Vec<u32> = h
        .subkeys(root)
        .into_iter()
        .filter(|(n, _)| n.len() == "ControlSet001".len() && n.to_ascii_lowercase().starts_with("controlset"))
        .map(|(_, o)| o)
        .collect();
    if sets.is_empty() {
        return Err(t!("aucun jeu de contrôle dans le registre SYSTEM", "no control set in the SYSTEM hive").into());
    }
    let mut out = Vec::new();
    for set in sets {
        let Some(services) = h.subkey(set, "Services") else { continue };
        for driver in BOOT_DRIVERS {
            let Some(key) = h.subkey(services, driver) else { continue };
            let mut found = Vec::new();
            found.extend(h.dword_values(key).into_iter().filter(|(n, _, _)| n.eq_ignore_ascii_case("Start")));
            if let Some(ov) = h.subkey(key, "StartOverride") {
                found.extend(h.dword_values(ov));
            }
            for (_, offset, old) in found {
                if old != 0 {
                    out.push(HivePatch { offset, old, new: 0, driver: driver.to_string() });
                }
            }
        }
    }
    Ok(out)
}

/// Drivers of a SYSTEM hive that are present, for the report.
pub fn present_boot_drivers(hive: &[u8]) -> Vec<String> {
    let Ok(h) = Hive::open(hive) else { return vec![] };
    let Some(root) = h.root() else { return vec![] };
    let mut out = Vec::new();
    for (name, set) in h.subkeys(root) {
        if !name.to_ascii_lowercase().starts_with("controlset") {
            continue;
        }
        if let Some(services) = h.subkey(set, "Services") {
            for d in BOOT_DRIVERS {
                if h.subkey(services, d).is_some() && !out.iter().any(|x: &String| x == d) {
                    out.push(d.to_string());
                }
            }
        }
    }
    out
}

/// Reads a value of 4 bytes of a hive (tests and checks).
pub fn hive_u32(hive: &[u8], offset: usize) -> Option<u32> {
    u32_at(hive, offset)
}

// --------------------------------------------------------- Writing on the volume

/// Block used for read-modify-write: covers 512-byte and 4 KiB sectors.
const RMW: u64 = 4096;

/// Writes `data` at the absolute device position `pos`, after checking that the bytes
/// currently there are `expect` (the mapping from file to disk is thus confirmed
/// before anything is written).
fn patch_bytes(dev: &mut dyn BlockDevice, pos: u64, expect: &[u8], data: &[u8]) -> Result<(), String> {
    let start = pos / RMW * RMW;
    let end = (pos + data.len() as u64).div_ceil(RMW) * RMW;
    let mut buf = AlignedBuf::new((end - start) as usize);
    dev.read_at(start, &mut buf).map_err(|e| e.to_string())?;
    let at = (pos - start) as usize;
    if &buf[at..at + expect.len()] != expect {
        return Err(t!(
            "le contenu lu sur le disque ne correspond pas au fichier : rien n'a été écrit",
            "the content read on the disk does not match the file: nothing was written"
        )
        .into());
    }
    buf[at..at + data.len()].copy_from_slice(data);
    dev.write_at(start, &buf).map_err(|e| e.to_string())
}

/// Cancels a saved session: the first 4 KiB of `hiberfil.sys` are cleared.
fn clear_hibernation(dev: &mut dyn BlockDevice, base: u64, len: u64) -> Result<bool, String> {
    let head = match crate::fs::ntfs::read_file(dev, base, len, &["hiberfil.sys"], HIBER_HEAD)? {
        Some(h) if hiber_active(&h) => h,
        _ => return Ok(false),
    };
    let offsets: Vec<u64> = (0..4096).step_by(512).collect();
    let Some((size, pos)) = crate::fs::ntfs::locate_file(dev, base, len, &["hiberfil.sys"], &offsets)? else { return Ok(false) };
    if size < 4096 || pos.iter().any(Option::is_none) {
        return Err(t!("en-tête de hiberfil.sys introuvable sur le disque", "hiberfil.sys header not found on the disk").into());
    }
    // The signature first: once it is gone, Windows no longer resumes anything.
    for (i, p) in pos.iter().enumerate() {
        let p = base + p.unwrap_or_default();
        let current = if i == 0 { &head[..4] } else { &[][..] };
        patch_bytes(dev, p, current, &[0u8; 512])?;
    }
    Ok(true)
}

/// Sets the boot drivers to start at boot in the offline SYSTEM hive.
fn enable_drivers(dev: &mut dyn BlockDevice, base: u64, len: u64, out: &mut BootPrepOutcome) -> Result<(), String> {
    let Some(hive) = crate::fs::ntfs::read_file(dev, base, len, &SYSTEM_HIVE, 512 << 20)? else {
        return Err(t!("registre SYSTEM introuvable", "SYSTEM registry hive not found").into());
    };
    let patches = boot_driver_patches(&hive)?;
    if patches.is_empty() {
        return Ok(());
    }
    let offsets: Vec<u64> = patches.iter().map(|p| p.offset as u64).collect();
    let Some((_, pos)) = crate::fs::ntfs::locate_file(dev, base, len, &SYSTEM_HIVE, &offsets)? else {
        return Err(t!("registre SYSTEM introuvable", "SYSTEM registry hive not found").into());
    };
    for (p, at) in patches.iter().zip(pos) {
        let at = at.ok_or(t!("valeur du registre introuvable sur le disque", "registry value not found on the disk"))?;
        patch_bytes(dev, base + at, &p.old.to_le_bytes(), &p.new.to_le_bytes())?;
        if !out.drivers.contains(&p.driver) {
            out.drivers.push(p.driver.clone());
        }
    }
    Ok(())
}

/// Prepares the Windows volume `[base, base+len)` of `dev` to boot on another PC:
/// cancels a saved session and enables the boot drivers. Writes in place; the device
/// must be held exclusively (clone destination, or volumes locked).
pub fn prepare_volume(dev: &mut dyn BlockDevice, base: u64, len: u64, out: &mut BootPrepOutcome, log: &dyn Fn(&str)) {
    let note = |out: &mut BootPrepOutcome, s: String| {
        log(&s);
        out.steps.push(s);
    };
    match clear_hibernation(dev, base, len) {
        Ok(true) => {
            out.hibernation_cleared = true;
            note(out, t!("Session en veille prolongée / démarrage rapide annulée", "Hibernated / fast startup session cancelled").into());
        }
        Ok(false) => {}
        Err(e) => out.warnings.push(tf!("veille prolongée non annulée : {e}", "hibernation not cancelled: {e}")),
    }
    match enable_drivers(dev, base, len, out) {
        Ok(()) if !out.drivers.is_empty() => {
            let s = tf!("Pilotes activés au démarrage : {}", "Drivers enabled at boot: {}", out.drivers.join(", "));
            note(out, s);
        }
        Ok(()) => note(out, t!("Pilotes de stockage déjà actifs au démarrage", "Storage drivers already enabled at boot").into()),
        Err(e) => out.warnings.push(tf!("pilotes de stockage non modifiés : {e}", "storage drivers not changed: {e}")),
    }
    if let Err(e) = dev.flush() {
        out.warnings.push(tf!("écriture sur le disque non confirmée : {e}", "write to the disk not confirmed: {e}"));
    }
}

#[cfg(windows)]
pub use crate::win::bootprep::prepare_disk;

/// Not available outside Windows.
#[cfg(not(windows))]
pub fn prepare_disk(_number: u32, _log: &dyn Fn(&str)) -> Result<BootPrepOutcome, String> {
    Err(t!("disponible uniquement sous Windows", "only available on Windows").into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_hive() -> Vec<u8> {
        std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/system-hive.bin")).unwrap()
    }

    #[test]
    fn hibernation_header() {
        assert!(hiber_active(b"HIBR\0\0\0\0"));
        assert!(hiber_active(b"WAKE...."));
        assert!(hiber_active(b"RSTR...."));
        assert!(!hiber_active(b"hibr...."));
        assert!(!hiber_active(b"wake...."));
        assert!(!hiber_active(&[0u8; 8]));
        assert!(!hiber_active(b"HI"));
    }

    #[test]
    fn summary_lists_what_was_done() {
        let mut o = BootPrepOutcome::default();
        assert_eq!(o.summary(), t!("aucune modification nécessaire", "no change needed"));
        o.bcd_fixed = true;
        o.drivers = vec!["stornvme".into()];
        o.hibernation_cleared = true;
        let s = o.summary();
        assert!(s.contains("BCD") && s.contains("stornvme") && s.matches(" ; ").count() == 2, "{s}");
    }

    /// The fixture is a hive saved by `reg save`: two control sets, 30 dummy services,
    /// stornvme and storahci on demand (3), storahci disabled at boot by StartOverride.
    #[test]
    fn real_hive_patches_both_drivers_in_every_control_set() {
        let hive = fixture_hive();
        assert_eq!(present_boot_drivers(&hive), vec!["stornvme", "storahci"]);
        let patches = boot_driver_patches(&hive).unwrap();
        // Per control set: stornvme Start, storahci Start, storahci StartOverride\0.
        assert_eq!(patches.len(), 6, "{patches:#?}");
        assert!(patches.iter().all(|p| p.old == 3 && p.new == 0));
        assert_eq!(patches.iter().filter(|p| p.driver == "stornvme").count(), 2);
        for p in &patches {
            assert_eq!(hive_u32(&hive, p.offset), Some(3));
        }
        // Once applied, nothing is left to change: the patch is idempotent.
        let mut patched = hive.clone();
        for p in &patches {
            patched[p.offset..p.offset + 4].copy_from_slice(&p.new.to_le_bytes());
        }
        assert!(boot_driver_patches(&patched).unwrap().is_empty());
        // Only those 4-byte values changed.
        let changed = hive.iter().zip(&patched).filter(|(a, b)| a != b).count();
        assert!(changed <= 6 && changed > 0, "{changed}");
    }

    #[test]
    fn hive_being_written_or_foreign_is_refused() {
        let mut hive = fixture_hive();
        hive[4] ^= 1;
        assert!(boot_driver_patches(&hive).is_err());
        assert!(boot_driver_patches(b"not a hive at all").is_err());
        let mut junk = vec![0u8; 8192];
        junk[..4].copy_from_slice(b"regf");
        // A "regf" header pointing nowhere gives no control set, not a crash.
        assert!(boot_driver_patches(&junk).is_err());
    }
}
