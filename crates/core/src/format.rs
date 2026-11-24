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

/// The `diskpart` scripts of a whole-disk format, run one after the other: Windows
/// needs time between them on a USB key (the disk is re-read and its volumes come and
/// go after `clean` and after `create partition`).
pub struct DiskpartSteps {
    /// Erases the disk.
    pub clean: String,
    /// New table and its data partition.
    pub partition: String,
}

/// `diskpart` scripts that erase one disk and create its table and data partition. The
/// file system is written afterwards, by [`format_script`] or [`format_fat32`].
pub fn diskpart_steps(number: u32, req: &FormatRequest) -> DiskpartSteps {
    let select = format!("select disk {number}\r\n");
    // A disk taken offline by Windows (same signature as another one, typical after a
    // clone) or marked read-only by an attribute is brought back first.
    let clean = format!("{select}online disk noerr\r\nattributes disk clear readonly noerr\r\nclean\r\n");
    // `clean` does not always reset the partition style: some USB keys stay GPT (or
    // MBR) afterwards, and converting to the style a disk already has is an error.
    // `noerr` lets the script go on; format_disk then checks the table really created.
    let convert = match req.table {
        TableChoice::Gpt => "convert gpt noerr\r\n",
        TableChoice::Mbr => "convert mbr noerr\r\n",
    };
    let partition = format!("{select}{convert}create partition primary\r\n");
    DiskpartSteps { clean, partition }
}

/// `diskpart` script giving the data partition (number `partition`, as returned by
/// [`check_new_table`]) Windows' file system.
pub fn format_script(number: u32, partition: u32, req: &FormatRequest) -> String {
    let label = req.label.trim();
    let fs = req.fs.diskpart_name();
    let cmd = if label.is_empty() { format!("format fs={fs} quick") } else { format!("format fs={fs} label=\"{label}\" quick") };
    format!("select disk {number}\r\nselect partition {partition}\r\n{cmd}\r\n")
}

/// Checks the table read back after `diskpart`: the requested style, with a single data
/// partition. On a GPT table, diskpart puts a Microsoft reserved partition (MSR) in front
/// of it when the disk is a fixed one (internal disk, external hard disk, VHDX), not on a
/// removable USB key. Returns the number of the data partition, as diskpart counts them.
pub fn check_new_table(layout: &crate::partition::DiskLayout, wanted: TableChoice) -> Result<u32, String> {
    use crate::partition::{PartRole, TableKind};
    let ok = matches!((wanted, layout.kind), (TableChoice::Gpt, TableKind::Gpt) | (TableChoice::Mbr, TableKind::Mbr));
    if !ok {
        return Err(tf!(
            "Windows n'a pas créé la table demandée ({} au lieu de {}). Le disque a été effacé mais n'est pas formaté.",
            "Windows did not create the requested table ({} instead of {}). The disk was erased but is not formatted.",
            layout.kind.label(),
            wanted.label()
        ));
    }
    let parts = &layout.partitions;
    let is_msr = |p: &crate::partition::Partition| layout.kind == TableKind::Gpt && p.role() == PartRole::Msr;
    let msr = parts.iter().filter(|p| is_msr(p)).count();
    let data: Vec<usize> = (0..parts.len()).filter(|&i| !is_msr(&parts[i])).collect();
    if data.len() != 1 || msr > 1 {
        return Err(tf!(
            "La nouvelle table contient {} partition(s) au lieu d'une seule (sans compter la partition réservée Microsoft). Le disque a été effacé mais n'est pas formaté.",
            "The new table holds {} partition(s) instead of one (not counting the Microsoft reserved partition). The disk was erased but is not formatted.",
            data.len()
        ));
    }
    // diskpart numbers the partitions in table order, without gaps. A table with an
    // unused entry before the data partition would make that number ambiguous.
    let position = data[0] as u32 + 1;
    if parts[data[0]].number != position {
        return Err(tf!(
            "Partition {} inattendue dans la nouvelle table (entrée vide avant elle). Le disque a été effacé mais n'est pas formaté.",
            "Unexpected partition {} in the new table (empty entry before it). The disk was erased but is not formatted.",
            parts[data[0]].number
        ));
    }
    Ok(position)
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
    if !crate::job::same_fingerprint(expected_fingerprint, &disk.fingerprint) {
        return Err(t!(
            "le disque n'est plus celui qui avait été choisi (modèle, numéro de série, taille ou partitions différents). Rien n'a été écrit.",
            "the disk is no longer the one that was chosen (different model, serial number, size or partitions). Nothing was written."
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
    let steps_dp = diskpart_steps(number, &req);
    // Before every diskpart run: disk number `number` must still be this disk (same
    // hardware identity; its layout changes as it is erased).
    let identity = hardware_identity(&disk);
    let same_disk = || -> Result<(), String> {
        match disk_by_number(number, false, app_paths) {
            Some(d) if hardware_identity(&d) == identity && !d.is_system && !d.hosts_app => Ok(()),
            _ => Err(tf!(
                "le disque {number} n'est plus celui qui avait été choisi (débranché ou remplacé pendant le formatage) : arrêt",
                "disk {number} is no longer the one that was chosen (unplugged or replaced during formatting): stopped"
            )),
        }
    };
    // Erase, then table and partition. A table of the wrong style (conversion refused
    // while Windows was still re-reading the disk) is retried once from the start.
    let mut layout = None;
    for attempt in 0..2 {
        run_diskpart_retry(&steps_dp.clean, &same_disk)?;
        std::thread::sleep(std::time::Duration::from_millis(1500));
        run_diskpart_retry(&steps_dp.partition, &same_disk)?;
        std::thread::sleep(std::time::Duration::from_millis(500));
        // The table actually written, read back from the disk: a conversion skipped
        // by `noerr` must not go unnoticed.
        let read = {
            let mut dev = RawDisk::open_read_only(&disk.path).map_err(|e| e.to_string())?;
            crate::partition::read_layout(&mut dev).map_err(|e| tf!("nouvelle table illisible : {e}", "new table unreadable: {e}"))?
        };
        match check_new_table(&read, req.table) {
            Ok(data) => {
                layout = Some((read, data));
                break;
            }
            Err(e) if attempt == 1 => return Err(e),
            Err(_) => std::thread::sleep(std::time::Duration::from_secs(3)),
        }
    }
    let (layout, data_number) = layout.ok_or_else(|| t!("nouvelle table introuvable", "new table not found").to_string())?;
    // The data partition (on a fixed disk, the MSR comes before it).
    let data = layout
        .partitions
        .get(data_number as usize - 1)
        .ok_or_else(|| t!("la partition créée par diskpart est introuvable", "the partition created by diskpart cannot be found").to_string())?;
    // Windows announces the new volume a moment after the partition is created.
    wait_for_volume(number);
    let mut partition_size = data.sectors * disk.sector_size as u64;
    if own_fat32 {
        note(tf!("Formatage FAT32 ({}) par Chud Clone", "FAT32 formatting ({}) by Chud Clone", crate::diag::human_size(partition_size)));
        let p = data;
        let ss = disk.sector_size as u64;
        let (start, len) = (p.start_lba * ss, p.sectors * ss);
        partition_size = len;
        // The new, unformatted volume is locked and dismounted while it is written.
        same_disk()?;
        let fresh = disk_by_number(number, false, app_paths).unwrap_or(disk.clone());
        let locks = lock_disk_volumes(&fresh, &volumes_of_disk(number)).map_err(|e| e.to_string())?;
        let mut dev = RawDisk::open_read_write(&disk.path).map_err(|e| e.to_string())?;
        format_fat32(&mut dev, start, len, &label, p.start_lba.min(u32::MAX as u64) as u32).map_err(|e| tf!("formatage FAT32 : {e}", "FAT32 formatting: {e}"))?;
        let _ = refresh_disk(&dev);
        drop(dev);
        drop(locks);
    } else {
        note(tf!("Formatage {} par Windows", "{} formatting by Windows", req.fs.label()));
        run_diskpart_retry(&format_script(number, data_number, &req), &same_disk)?;
    }
    // A drive letter, unless Windows already gave one.
    std::thread::sleep(std::time::Duration::from_millis(800));
    let mut mount = volumes_of_disk(number).into_iter().map(|v| v.1).find(|m| m.ends_with(":\\") || m.ends_with(':'));
    if mount.is_none() {
        if same_disk().is_ok() {
            let _ = run_diskpart(&format!("select disk {number}\r\nselect partition {data_number}\r\nassign noerr\r\n"));
        }
        std::thread::sleep(std::time::Duration::from_millis(800));
        mount = volumes_of_disk(number).into_iter().map(|v| v.1).find(|m| m.ends_with(":\\") || m.ends_with(':'));
    }
    note(match &mount {
        Some(m) => tf!("Volume prêt : {m}", "Volume ready: {m}"),
        None => t!("Volume prêt (sans lettre de lecteur)", "Volume ready (no drive letter)").to_string(),
    });
    Ok(FormatOutcome { fs: req.fs, table: req.table, label, partition_size, mount, steps })
}

/// Runs a `diskpart` script, up to three times: on a USB key that Windows is still
/// re-reading, diskpart answers "device not ready" or "no volume selected" for a moment.
///
/// diskpart designates the disk by its number, and a USB key that is being re-read can
/// briefly disappear and come back under another number while another disk takes its
/// own. `same_disk` is therefore checked before every attempt, the first one included.
#[cfg(windows)]
fn run_diskpart_retry(script: &str, same_disk: &dyn Fn() -> Result<(), String>) -> Result<String, String> {
    let mut last = String::new();
    for attempt in 0..3u64 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(2000 * attempt));
        }
        same_disk()?;
        match run_diskpart(script) {
            Ok(out) => return Ok(out),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// Identity of a disk without its partition layout (model, serial number, size, bus,
/// sector size): what stays the same while it is being erased and partitioned.
pub fn hardware_identity(d: &crate::model::DiskInfo) -> String {
    crate::model::fingerprint(&d.model, &d.serial, d.size, &d.bus, d.sector_size)
}

/// Writes a diskpart script so that no other program can change it before diskpart
/// reads it: a new file (never an existing one or a link put there in advance), then
/// held open without write or delete sharing, and read back through that handle. The
/// script runs as administrator, and the temporary folder is writable by every program
/// of the session.
#[cfg(windows)]
fn script_file(script: &str) -> std::io::Result<(std::path::PathBuf, std::fs::File)> {
    use std::io::{Read, Write};
    use std::os::windows::fs::OpenOptionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let name = format!("chudclone-diskpart-{}-{}-{:x}.txt", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed), nonce);
    let file = std::env::temp_dir().join(name);
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&file)?;
    f.write_all(script.as_bytes())?;
    drop(f);
    // FILE_SHARE_READ only: diskpart can read it, nobody can write, rename or delete it.
    let mut guard = std::fs::OpenOptions::new().read(true).share_mode(1).open(&file)?;
    let mut back = String::new();
    guard.read_to_string(&mut back)?;
    if back != script {
        return Err(std::io::Error::other(t!("le script a été modifié par un autre programme", "the script was changed by another program")));
    }
    Ok((file, guard))
}

/// Waits (20 s at most) until Windows lists a volume on the disk.
#[cfg(windows)]
fn wait_for_volume(number: u32) {
    for _ in 0..40 {
        if !crate::win::enumerate::volumes_of_disk(number).is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(500));
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

/// Runs a `diskpart` script; its output is returned in the error when it fails.
#[cfg(windows)]
pub(crate) fn run_diskpart(script: &str) -> Result<String, String> {
    let (file, guard) = script_file(script).map_err(|e| tf!("script diskpart : {e}", "diskpart script: {e}"))?;
    let out = std::process::Command::new(crate::win::system_tool("diskpart")).arg("/s").arg(&file).output();
    drop(guard);
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

    /// The diskpart script cannot be changed by another program between its writing and
    /// its reading by diskpart.
    #[cfg(windows)]
    #[test]
    fn diskpart_script_is_locked_while_it_runs() {
        let script = "select disk 99\r\nclean\r\n";
        let (path, guard) = script_file(script).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), script, "readable by diskpart");
        assert!(std::fs::OpenOptions::new().write(true).open(&path).is_err(), "another program could rewrite it");
        assert!(std::fs::remove_file(&path).is_err(), "another program could replace it");
        drop(guard);
        std::fs::remove_file(&path).unwrap();
        // Each script gets its own new file.
        let (a, ga) = script_file("x").unwrap();
        let (b, gb) = script_file("x").unwrap();
        assert_ne!(a, b);
        drop((ga, gb));
        let _ = (std::fs::remove_file(a), std::fs::remove_file(b));
    }

    /// The identity re-checked between diskpart runs ignores the partitions (they change
    /// while the disk is erased) but not the hardware.
    #[test]
    fn hardware_identity_ignores_the_layout() {
        let a = DiskInfo { model: "USB DISK".into(), serial: String::new(), size: 16 << 30, bus: "USB".into(), ..DiskInfo::test_default() };
        let mut b = a.clone();
        b.table = crate::partition::TableKind::Mbr;
        b.partitions.clear();
        b.fingerprint = "other".into();
        assert_eq!(hardware_identity(&a), hardware_identity(&b));
        assert_ne!(hardware_identity(&a), hardware_identity(&DiskInfo { size: 32 << 30, ..a.clone() }));
        assert_ne!(hardware_identity(&a), hardware_identity(&DiskInfo { model: "Other".into(), ..a.clone() }));
    }

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
    fn new_table_is_checked() {
        use crate::partition::{DiskLayout, PartInfo, Partition, TableKind};
        let part = Partition {
            number: 1,
            start_lba: 2048,
            sectors: 4096,
            info: PartInfo::Mbr { type_id: 0x0c, active: false, logical: false, slot: 0, ebr_lba: None },
        };
        let layout = |kind, partitions| DiskLayout { kind, sector_size: 512, disk_sectors: 8192, partitions, gpt: None, mbr: None, findings: Vec::new() };
        assert!(check_new_table(&layout(TableKind::Mbr, vec![part.clone()]), TableChoice::Mbr).is_ok());
        // Conversion skipped by diskpart's noerr: wrong style is refused.
        assert!(check_new_table(&layout(TableKind::Mbr, vec![part.clone()]), TableChoice::Gpt).is_err());
        assert!(check_new_table(&layout(TableKind::None, vec![]), TableChoice::Gpt).is_err());
        assert!(check_new_table(&layout(TableKind::Mbr, vec![]), TableChoice::Mbr).is_err());
        assert!(check_new_table(&layout(TableKind::Mbr, vec![part.clone(), Partition { number: 2, ..part }]), TableChoice::Mbr).is_err());
    }

    /// On a fixed disk (internal disk, external USB hard disk, VHDX), diskpart adds a
    /// Microsoft reserved partition (MSR) in front of the one it creates on a GPT table.
    #[test]
    fn gpt_table_of_a_fixed_disk_has_an_msr_first() {
        use crate::guid::Guid;
        use crate::partition::{DiskLayout, GPT_BASIC_DATA, GPT_MSR, PartInfo, Partition, TableKind};
        let gpt = |number: u32, start_lba: u64, sectors: u64, type_guid: &str| Partition {
            number,
            start_lba,
            sectors,
            info: PartInfo::Gpt {
                type_guid: Guid::must(type_guid),
                unique_guid: Guid::must(GPT_MSR),
                attributes: 0,
                name: String::new(),
                slot: number - 1,
            },
        };
        let layout = |partitions| DiskLayout {
            kind: TableKind::Gpt,
            sector_size: 512,
            disk_sectors: 1 << 21,
            partitions,
            gpt: None,
            mbr: None,
            findings: Vec::new(),
        };
        let msr = gpt(1, 34, 32768, GPT_MSR);
        let data = gpt(2, 34816, 2_000_000, GPT_BASIC_DATA);
        // Removable disk: the data partition alone.
        assert!(check_new_table(&layout(vec![gpt(1, 2048, 2_000_000, GPT_BASIC_DATA)]), TableChoice::Gpt).is_ok());
        // Fixed disk: MSR, then the data partition.
        assert!(check_new_table(&layout(vec![msr.clone(), data.clone()]), TableChoice::Gpt).is_ok());
        assert_eq!(check_new_table(&layout(vec![gpt(1, 2048, 2_000_000, GPT_BASIC_DATA)]), TableChoice::Gpt), Ok(1));
        assert_eq!(check_new_table(&layout(vec![msr.clone(), data.clone()]), TableChoice::Gpt), Ok(2));
        assert_eq!(format_script(4, 2, &req(FsChoice::Exfat, "")), "select disk 4\r\nselect partition 2\r\nformat fs=exfat quick\r\n");
        // Anything else is still refused: two data partitions, two MSRs, an MSR alone, a gap.
        let data3 = gpt(3, 3_000_000, 1000, GPT_BASIC_DATA);
        assert!(check_new_table(&layout(vec![msr.clone(), data.clone(), data3.clone()]), TableChoice::Gpt).is_err());
        assert!(check_new_table(&layout(vec![msr.clone(), gpt(2, 40000, 32768, GPT_MSR), data3.clone()]), TableChoice::Gpt).is_err());
        assert!(check_new_table(&layout(vec![msr.clone()]), TableChoice::Gpt).is_err());
        assert!(check_new_table(&layout(vec![msr.clone(), data3]), TableChoice::Gpt).is_err());
        // The MSR type means nothing on an MBR table (no such type there): counted as data.
        let mbr = |number: u32| Partition {
            number,
            start_lba: 2048 * number as u64,
            sectors: 1000,
            info: PartInfo::Mbr { type_id: 0x07, active: false, logical: false, slot: number - 1, ebr_lba: None },
        };
        assert!(check_new_table(&DiskLayout { kind: TableKind::Mbr, ..layout(vec![mbr(1), mbr(2)]) }, TableChoice::Mbr).is_err());
    }

    #[test]
    fn script() {
        let s = diskpart_steps(3, &req(FsChoice::Ntfs, "Data"));
        assert!(s.clean.starts_with("select disk 3\r\n") && s.clean.ends_with("clean\r\n"));
        assert_eq!(s.partition, "select disk 3\r\nconvert gpt noerr\r\ncreate partition primary\r\n");
        assert_eq!(
            format_script(3, 1, &req(FsChoice::Ntfs, "Data")),
            "select disk 3\r\nselect partition 1\r\nformat fs=ntfs label=\"Data\" quick\r\n"
        );
        let s = diskpart_steps(3, &FormatRequest { fs: FsChoice::Fat32, table: TableChoice::Mbr, label: String::new() });
        assert!(s.partition.contains("convert mbr noerr"));
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
