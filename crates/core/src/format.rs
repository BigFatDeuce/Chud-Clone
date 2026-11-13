//! Formatting a whole disk: new partition table (GPT or MBR), a single partition
//! covering the disk, one file system (NTFS, exFAT or FAT32) and a volume name.
//!
//! Windows' own `diskpart` creates the table and the partition, and formats NTFS,
//! exFAT and FAT32 up to 32 GiB (the limit of Windows' FAT32 formatter). Larger
//! FAT32 volumes are written by this module itself ([`format_fat32`], through the
//! `fatfs` crate), so a USB key or disk of any size up to 2 TiB can be FAT32.
//! `diskpart` and `format` both exist in WinPE: the rescue key formats the same way.
//!
//! DESTRUCTIVE OPERATION: everything on the disk is erased. The caller must have
//! obtained the user's typed confirmation; the disk's identity is checked again
//! right before anything is written.

use crate::device::BlockDevice;
use crate::model::{DiskInfo, DiskKind};
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Seek, SeekFrom, Write};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FsChoice {
    Ntfs,
    Exfat,
    Fat32,
}

impl FsChoice {
    pub fn label(self) -> &'static str {
        match self {
            FsChoice::Ntfs => "NTFS",
            FsChoice::Exfat => "exFAT",
            FsChoice::Fat32 => "FAT32",
        }
    }
    fn diskpart_name(self) -> &'static str {
        match self {
            FsChoice::Ntfs => "ntfs",
            FsChoice::Exfat => "exfat",
            FsChoice::Fat32 => "fat32",
        }
    }
    /// Longest volume name accepted by the file system.
    pub fn max_label(self) -> usize {
        match self {
            FsChoice::Ntfs => 32,
            FsChoice::Exfat => 15,
            FsChoice::Fat32 => 11,
        }
    }
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ntfs" => Ok(FsChoice::Ntfs),
            "exfat" => Ok(FsChoice::Exfat),
            "fat32" | "fat" => Ok(FsChoice::Fat32),
            x => Err(tf!("système de fichiers inconnu : {x} (ntfs, exfat, fat32)", "unknown file system: {x} (ntfs, exfat, fat32)")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TableChoice {
    Gpt,
    Mbr,
}

impl TableChoice {
    pub fn label(self) -> &'static str {
        match self {
            TableChoice::Gpt => "GPT",
            TableChoice::Mbr => "MBR",
        }
    }
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "gpt" => Ok(TableChoice::Gpt),
            "mbr" => Ok(TableChoice::Mbr),
            x => Err(tf!("table de partitions inconnue : {x} (gpt, mbr)", "unknown partition table: {x} (gpt, mbr)")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormatRequest {
    pub fs: FsChoice,
    pub table: TableChoice,
    /// Volume name (may be empty).
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormatOutcome {
    pub fs: FsChoice,
    pub table: TableChoice,
    pub label: String,
    /// Size of the new partition.
    pub partition_size: u64,
    /// Drive letter or mount point given by Windows, if any.
    pub mount: Option<String>,
    /// Steps done, in order (for the log and the interface).
    pub steps: Vec<String>,
}

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;
/// Largest FAT32 volume Windows' formatter accepts; beyond, this module formats it.
pub const WINDOWS_FAT32_LIMIT: u64 = 32 * GIB;

/// Checks a volume name and returns it as it will be written (FAT32: upper case).
pub fn validate_label(fs: FsChoice, label: &str) -> Result<String, String> {
    let label = label.trim();
    let n = label.chars().count();
    if n > fs.max_label() {
        return Err(tf!(
            "nom trop long pour {} : {n} caractères, {} au plus",
            "name too long for {}: {n} characters, {} at most",
            fs.label(),
            fs.max_label()
        ));
    }
    // Characters refused by Windows in volume names, plus those of the diskpart script.
    let forbidden = ['"', '\\', '/', ':', '*', '?', '<', '>', '|', '\r', '\n', '\t'];
    if let Some(c) = label.chars().find(|c| forbidden.contains(c) || c.is_control()) {
        return Err(tf!("caractère interdit dans le nom : « {c} »", "character not allowed in the name: \"{c}\""));
    }
    if fs == FsChoice::Fat32 {
        if let Some(c) = label.chars().find(|c| !c.is_ascii() || ".,;=+[]".contains(*c)) {
            return Err(tf!(
                "caractère interdit dans un nom FAT32 : « {c} » (lettres sans accent, chiffres, espace, - et _)",
                "character not allowed in a FAT32 name: \"{c}\" (unaccented letters, digits, space, - and _)"
            ));
        }
        return Ok(label.to_ascii_uppercase());
    }
    Ok(label.to_string())
}

/// Reasons why this disk cannot be formatted this way (empty: allowed).
pub fn refusals(disk: &DiskInfo, req: &FormatRequest) -> Vec<String> {
    let mut out = Vec::new();
    if disk.kind != DiskKind::Physical || disk.number.is_none() {
        out.push(t!("seuls les disques peuvent être formatés (pas les fichiers image)", "only disks can be formatted (not image files)").into());
    }
    if disk.is_system {
        out.push(t!("ce disque porte le Windows en cours d'exécution", "this disk holds the running Windows").into());
    }
    if disk.hosts_app {
        out.push(t!("ce disque contient Chud Clone ou ses rapports", "this disk holds Chud Clone or its reports").into());
    }
    if disk.read_only {
        out.push(t!("ce disque est protégé en écriture", "this disk is write-protected").into());
    }
    if let Err(e) = validate_label(req.fs, &req.label) {
        out.push(e);
    }
    if req.table == TableChoice::Mbr && disk.size > 2048 * GIB && disk.sector_size == 512 {
        out.push(t!(
            "une table MBR ne peut pas dépasser 2 Tio avec des secteurs de 512 octets : choisissez GPT",
            "an MBR table cannot go beyond 2 TiB with 512-byte sectors: choose GPT"
        )
        .into());
    }
    if req.fs == FsChoice::Fat32 {
        if disk.size < 64 * MIB {
            out.push(t!("disque trop petit pour FAT32 (64 Mio au moins)", "disk too small for FAT32 (64 MiB at least)").into());
        }
        if disk.size / disk.sector_size.max(512) as u64 > u32::MAX as u64 {
            out.push(t!("disque trop grand pour FAT32 : choisissez exFAT ou NTFS", "disk too large for FAT32: choose exFAT or NTFS").into());
        }
    }
    out
}

/// `diskpart` script for one disk. `format` false: the partition is created but left
/// unformatted (large FAT32, written by [`format_fat32`] afterwards).
pub fn diskpart_script(number: u32, req: &FormatRequest, format: bool) -> String {
    let mut s = String::new();
    s.push_str(&format!("select disk {number}\r\n"));
    // A disk taken offline by Windows (same signature as another one, typical after a
    // clone) or marked read-only by an attribute is brought back first.
    s.push_str("online disk noerr\r\n");
    s.push_str("attributes disk clear readonly noerr\r\n");
    s.push_str("clean\r\n");
    match req.table {
        TableChoice::Gpt => s.push_str("convert gpt\r\n"),
        TableChoice::Mbr => s.push_str("convert mbr noerr\r\n"),
    }
    s.push_str("create partition primary\r\n");
    if format {
        let label = req.label.trim();
        if label.is_empty() {
            s.push_str(&format!("format fs={} quick\r\n", req.fs.diskpart_name()));
        } else {
            s.push_str(&format!("format fs={} label=\"{label}\" quick\r\n", req.fs.diskpart_name()));
        }
    }
    s
}

/// Uses Windows' formatter (`diskpart format`) for this request, or this module.
pub fn uses_own_fat32(req: &FormatRequest, partition_size: u64) -> bool {
    req.fs == FsChoice::Fat32 && partition_size > WINDOWS_FAT32_LIMIT
}

/// Writes an empty FAT32 file system in `[offset, offset + len)` of a device.
/// `hidden_sectors` is the partition's first sector (recorded in the boot sector).
pub fn format_fat32(dev: &mut dyn BlockDevice, offset: u64, len: u64, label: &str, hidden_sectors: u32) -> io::Result<()> {
    let sector = dev.sector_size();
    if !offset.is_multiple_of(sector as u64) || !len.is_multiple_of(sector as u64) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, t!("partition non alignée", "unaligned partition")));
    }
    let mut name = [b' '; 11];
    for (i, b) in label.trim().to_ascii_uppercase().bytes().take(11).enumerate() {
        name[i] = b;
    }
    if label.trim().is_empty() {
        name = *b"NO NAME    ";
    }
    let volume_id = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as u32 ^ 0x5A3C_96E1).unwrap_or(0x5A3C_96E1);
    {
        let mut w = DeviceWriter::new(dev, offset, len);
        let opts = fatfs::FormatVolumeOptions::new()
            .fat_type(fatfs::FatType::Fat32)
            .bytes_per_sector(sector as u16)
            .volume_label(name)
            .volume_id(volume_id);
        fatfs::format_volume(&mut w, opts)?;
        w.flush()?;
    }
    // Hidden sectors (start of the partition) in the boot sector and its backup copy.
    let mut buf = vec![0u8; sector as usize];
    for copy in [0u64, 6] {
        let at = offset + copy * sector as u64;
        dev.read_at(at, &mut buf)?;
        buf[0x1C..0x20].copy_from_slice(&hidden_sectors.to_le_bytes());
        dev.write_at(at, &buf)?;
    }
    dev.flush()
}

/// `Read + Write + Seek` view of a window of a device, with a write-back cache of one
/// aligned block: `fatfs` writes small pieces, the device takes whole sectors.
struct DeviceWriter<'a> {
    dev: &'a mut dyn BlockDevice,
    base: u64,
    len: u64,
    pos: u64,
    block: Vec<u8>,
    block_start: u64,
    block_len: usize,
    dirty: bool,
}

const WRITER_BLOCK: u64 = 1 << 20;

impl<'a> DeviceWriter<'a> {
    fn new(dev: &'a mut dyn BlockDevice, base: u64, len: u64) -> Self {
        Self { dev, base, len, pos: 0, block: vec![0u8; WRITER_BLOCK as usize], block_start: u64::MAX, block_len: 0, dirty: false }
    }

    fn write_back(&mut self) -> io::Result<()> {
        if self.dirty && self.block_start != u64::MAX {
            self.dev.write_at(self.base + self.block_start, &self.block[..self.block_len])?;
            self.dirty = false;
        }
        Ok(())
    }

    /// Loads the block holding `pos` (within the window).
    fn load(&mut self, pos: u64) -> io::Result<()> {
        let start = pos - pos % WRITER_BLOCK;
        if start == self.block_start {
            return Ok(());
        }
        self.write_back()?;
        let n = (self.len - start).min(WRITER_BLOCK) as usize;
        self.block_start = u64::MAX;
        self.dev.read_at(self.base + start, &mut self.block[..n])?;
        self.block_start = start;
        self.block_len = n;
        Ok(())
    }
}

impl Read for DeviceWriter<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.len || out.is_empty() {
            return Ok(0);
        }
        self.load(self.pos)?;
        let off = (self.pos - self.block_start) as usize;
        let n = (self.block_len - off).min(out.len());
        out[..n].copy_from_slice(&self.block[off..off + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Write for DeviceWriter<'_> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        if self.pos >= self.len {
            return Err(io::Error::new(io::ErrorKind::WriteZero, t!("écriture au-delà de la partition", "write past the end of the partition")));
        }
        self.load(self.pos)?;
        let off = (self.pos - self.block_start) as usize;
        let n = (self.block_len - off).min(data.len());
        self.block[off..off + n].copy_from_slice(&data[..n]);
        self.dirty = true;
        self.pos += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.write_back()?;
        self.dev.flush()
    }
}

impl Seek for DeviceWriter<'_> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let new = match from {
            SeekFrom::Start(p) => p as i128,
            SeekFrom::Current(d) => self.pos as i128 + d as i128,
            SeekFrom::End(d) => self.len as i128 + d as i128,
        };
        if new < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, t!("position négative", "negative position")));
        }
        self.pos = new as u64;
        Ok(self.pos)
    }
}

impl Drop for DeviceWriter<'_> {
    fn drop(&mut self) {
        let _ = self.write_back();
    }
}

/// Formats a whole disk (Windows). `expected_fingerprint`: identity of the disk the
/// user chose, checked again here before anything is written. `step` receives a short
/// description of each step.
#[cfg(windows)]
pub fn format_disk(number: u32, expected_fingerprint: &str, req: &FormatRequest, app_paths: &[String], step: &dyn Fn(&str)) -> Result<FormatOutcome, String> {
    use crate::win::enumerate::{disk_by_number, volumes_of_disk};
    use crate::win::rawdisk::{RawDisk, lock_disk_volumes, refresh_disk};

    let _awake = crate::awake::KeepAwake::new();
    let label = validate_label(req.fs, &req.label)?;
    let req = FormatRequest { label: label.clone(), ..req.clone() };
    let disk = disk_by_number(number, false, app_paths).ok_or_else(|| tf!("disque {number} introuvable", "disk {number} not found"))?;
    if disk.fingerprint != expected_fingerprint {
        return Err(t!(
            "le disque n'est plus celui qui avait été choisi (modèle, numéro de série ou taille différents). Rien n'a été écrit.",
            "the disk is no longer the one that was chosen (different model, serial number or size). Nothing was written."
        )
        .into());
    }
    let refused = refusals(&disk, &req);
    if !refused.is_empty() {
        return Err(refused.join(" ; "));
    }
    let mut steps = Vec::new();
    let mut note = |s: String| {
        step(&s);
        steps.push(s);
    };
    // Estimated size of the partition diskpart will create (1 MiB alignment, GPT
    // reserves a little at both ends): decides who formats a FAT32 volume.
    let estimated = disk.size.saturating_sub(2 * MIB);
    let own_fat32 = uses_own_fat32(&req, estimated);
    note(tf!(
        "Effacement du disque {number} et nouvelle table {}",
        "Erasing disk {number} and creating a new {} table",
        req.table.label()
    ));
    run_diskpart(&diskpart_script(number, &req, !own_fat32))?;
    let mut partition_size = estimated;
    if own_fat32 {
        note(tf!("Formatage FAT32 ({}) par Chud Clone", "FAT32 formatting ({}) by Chud Clone", crate::diag::human_size(estimated)));
        let mut dev = RawDisk::open_read_only(&disk.path).map_err(|e| e.to_string())?;
        let layout = crate::partition::read_layout(&mut dev).map_err(|e| tf!("nouvelle table illisible : {e}", "new table unreadable: {e}"))?;
        drop(dev);
        let p = layout
            .partitions
            .first()
            .ok_or_else(|| t!("la partition créée par diskpart est introuvable", "the partition created by diskpart cannot be found").to_string())?;
        let ss = disk.sector_size as u64;
        let (start, len) = (p.start_lba * ss, p.sectors * ss);
        partition_size = len;
        // The new, unformatted volume is locked and dismounted while it is written.
        let fresh = disk_by_number(number, false, app_paths).unwrap_or(disk.clone());
        let locks = lock_disk_volumes(&fresh, &volumes_of_disk(number)).map_err(|e| e.to_string())?;
        let mut dev = RawDisk::open_read_write(&disk.path).map_err(|e| e.to_string())?;
        format_fat32(&mut dev, start, len, &label, p.start_lba.min(u32::MAX as u64) as u32).map_err(|e| tf!("formatage FAT32 : {e}", "FAT32 formatting: {e}"))?;
        let _ = refresh_disk(&dev);
        drop(dev);
        drop(locks);
    } else {
        note(tf!("Formatage {} par Windows", "{} formatting by Windows", req.fs.label()));
    }
    // A drive letter, unless Windows already gave one.
    std::thread::sleep(std::time::Duration::from_millis(800));
    let mut mount = volumes_of_disk(number).into_iter().map(|v| v.1).find(|m| m.ends_with(":\\") || m.ends_with(':'));
    if mount.is_none() {
        let _ = run_diskpart(&format!("select disk {number}\r\nselect partition 1\r\nassign noerr\r\n"));
        std::thread::sleep(std::time::Duration::from_millis(800));
        mount = volumes_of_disk(number).into_iter().map(|v| v.1).find(|m| m.ends_with(":\\") || m.ends_with(':'));
    }
    note(match &mount {
        Some(m) => tf!("Volume prêt : {m}", "Volume ready: {m}"),
        None => t!("Volume prêt (sans lettre de lecteur)", "Volume ready (no drive letter)").to_string(),
    });
    Ok(FormatOutcome { fs: req.fs, table: req.table, label, partition_size, mount, steps })
}

/// Runs a `diskpart` script; its output is returned in the error when it fails.
#[cfg(windows)]
fn run_diskpart(script: &str) -> Result<String, String> {
    let dir = std::env::temp_dir();
    let file = dir.join(format!("chudclone-diskpart-{}.txt", std::process::id()));
    std::fs::write(&file, script).map_err(|e| tf!("script diskpart : {e}", "diskpart script: {e}"))?;
    let out = std::process::Command::new("diskpart").arg("/s").arg(&file).output();
    let _ = std::fs::remove_file(&file);
    let out = out.map_err(|e| tf!("diskpart introuvable ou refusé : {e}", "diskpart not found or refused: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout).replace('\r', "");
    if out.status.success() {
        Ok(text)
    } else {
        // Last meaningful lines: diskpart's own explanation.
        let tail: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
        let tail = tail[tail.len().saturating_sub(4)..].join(" / ");
        Err(tf!("diskpart a échoué (code {}) : {tail}", "diskpart failed (code {}): {tail}", out.status.code().unwrap_or(-1)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::MemDevice;

    fn req(fs: FsChoice, label: &str) -> FormatRequest {
        FormatRequest { fs, table: TableChoice::Gpt, label: label.into() }
    }

    #[test]
    fn labels() {
        assert_eq!(validate_label(FsChoice::Fat32, " usb key ").unwrap(), "USB KEY");
        assert!(validate_label(FsChoice::Fat32, "clé").is_err(), "accents are not FAT32");
        assert_eq!(validate_label(FsChoice::Fat32, "backup_2026").unwrap(), "BACKUP_2026");
        assert!(validate_label(FsChoice::Fat32, "TWELVE_CHARS").is_err());
        assert_eq!(validate_label(FsChoice::Ntfs, "Données été").unwrap(), "Données été");
        assert!(validate_label(FsChoice::Ntfs, "a\"b").is_err());
        assert!(validate_label(FsChoice::Exfat, "SIXTEEN_CHARS_XX").is_err());
        assert_eq!(validate_label(FsChoice::Exfat, "").unwrap(), "");
    }

    #[test]
    fn script() {
        let s = diskpart_script(3, &req(FsChoice::Ntfs, "Data"), true);
        assert!(s.starts_with("select disk 3\r\n"));
        assert!(s.contains("clean\r\nconvert gpt\r\ncreate partition primary\r\nformat fs=ntfs label=\"Data\" quick\r\n"));
        let s = diskpart_script(3, &FormatRequest { fs: FsChoice::Fat32, table: TableChoice::Mbr, label: String::new() }, false);
        assert!(s.contains("convert mbr noerr"));
        assert!(!s.contains("format"));
        assert!(uses_own_fat32(&req(FsChoice::Fat32, ""), 64 * GIB));
        assert!(!uses_own_fat32(&req(FsChoice::Fat32, ""), 16 * GIB));
        assert!(!uses_own_fat32(&req(FsChoice::Exfat, ""), 64 * GIB));
    }

    #[test]
    fn refusals_protect_system_and_images() {
        let mut d = DiskInfo::test_default();
        d.kind = DiskKind::Physical;
        d.number = Some(2);
        d.size = 64 * GIB;
        assert!(refusals(&d, &req(FsChoice::Ntfs, "x")).is_empty());
        d.is_system = true;
        assert_eq!(refusals(&d, &req(FsChoice::Ntfs, "x")).len(), 1);
        d.is_system = false;
        d.kind = DiskKind::Image;
        assert!(!refusals(&d, &req(FsChoice::Ntfs, "x")).is_empty());
        d.kind = DiskKind::Physical;
        d.size = 3 * 1024 * GIB;
        assert!(!refusals(&d, &FormatRequest { fs: FsChoice::Ntfs, table: TableChoice::Mbr, label: String::new() }).is_empty());
        d.size = 32 * MIB;
        assert!(!refusals(&d, &req(FsChoice::Fat32, "")).is_empty());
    }

    /// A FAT32 volume written in a partition of a device can be read back by `fatfs`
    /// (and by the engine's own FAT reader), with its name and the hidden sectors.
    #[test]
    fn fat32_in_a_partition() {
        let ss = 512u64;
        let start = 2048 * ss;
        let len = 300 * MIB;
        let mut dev = MemDevice::new(start + len + MIB, 512);
        format_fat32(&mut dev, start, len, "Chud Key", 2048).unwrap();
        let region = &mut dev.data[start as usize..(start + len) as usize];
        assert_eq!(&region[0x1C..0x20], &2048u32.to_le_bytes());
        assert_eq!(&region[6 * 512 + 0x1C..6 * 512 + 0x20], &2048u32.to_le_bytes());
        let mut cur = std::io::Cursor::new(region.to_vec());
        {
            let fs = fatfs::FileSystem::new(&mut cur, fatfs::FsOptions::new()).unwrap();
            assert_eq!(fs.fat_type(), fatfs::FatType::Fat32);
            assert_eq!(fs.volume_label(), "CHUD KEY");
            let mut f = fs.root_dir().create_file("hello.txt").unwrap();
            f.write_all(b"chud").unwrap();
        }
        // Nothing outside the partition was touched.
        assert!(dev.data[..start as usize].iter().all(|b| *b == 0));
        assert!(dev.data[(start + len) as usize..].iter().all(|b| *b == 0));
    }
}
