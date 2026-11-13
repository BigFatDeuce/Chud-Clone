//! Abstraction de périphérique bloc.
//!
//! Tout le moteur (tables de partitions, systèmes de fichiers, copie, vérification)
//! travaille uniquement à travers [`BlockDevice`]. Cela permet de tester exactement le
//! même code sur des fichiers image que celui qui s'exécute sur un disque physique.

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};

/// Classe d'une erreur d'E/S, indépendante de la plateforme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IoErrorClass {
    /// Erreur de support : secteur illisible, CRC, défaut de lecture/écriture.
    Media,
    /// Le périphérique a disparu (déconnexion, câble, alimentation).
    DeviceGone,
    /// Accès refusé (privilèges, verrouillage par un autre processus).
    AccessDenied,
    /// Support protégé en écriture.
    WriteProtected,
    /// Délai dépassé : souvent un disque en difficulté ou un pont USB instable.
    Timeout,
    /// Autre erreur.
    Other,
}

/// Marqueur attaché à une `io::Error` pour forcer sa classe (utilisé par les
/// périphériques simulés et par la couche Windows).
#[derive(Debug, Clone, Copy)]
pub struct DevFault(pub IoErrorClass);

impl std::fmt::Display for DevFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self.0 {
            IoErrorClass::Media => t!("erreur de support (secteur illisible)", "media error (unreadable sector)"),
            IoErrorClass::DeviceGone => t!("périphérique déconnecté", "device disconnected"),
            IoErrorClass::AccessDenied => t!("accès refusé", "access denied"),
            IoErrorClass::WriteProtected => t!("support protégé en écriture", "media is write-protected"),
            IoErrorClass::Timeout => t!("délai dépassé", "timed out"),
            IoErrorClass::Other => t!("erreur d'E/S", "I/O error"),
        };
        f.write_str(s)
    }
}
impl std::error::Error for DevFault {}

/// Construit une `io::Error` portant une classe explicite.
pub fn fault(class: IoErrorClass) -> io::Error {
    io::Error::other(DevFault(class))
}

/// Détermine la classe d'une erreur d'E/S.
pub fn classify(err: &io::Error) -> IoErrorClass {
    if let Some(inner) = err.get_ref()
        && let Some(f) = inner.downcast_ref::<DevFault>()
    {
        return f.0;
    }
    if let Some(code) = err.raw_os_error()
        && let Some(c) = classify_os_code(code)
    {
        return c;
    }
    match err.kind() {
        io::ErrorKind::PermissionDenied => IoErrorClass::AccessDenied,
        io::ErrorKind::TimedOut => IoErrorClass::Timeout,
        io::ErrorKind::NotFound => IoErrorClass::DeviceGone,
        _ => IoErrorClass::Other,
    }
}

/// Codes d'erreur Win32 pertinents pour les disques.
#[cfg(windows)]
fn classify_os_code(code: i32) -> Option<IoErrorClass> {
    Some(match code {
        // ERROR_CRC, ERROR_SEEK, ERROR_SECTOR_NOT_FOUND, ERROR_WRITE_FAULT,
        // ERROR_READ_FAULT, ERROR_IO_DEVICE, ERROR_DISK_OPERATION_FAILED,
        // ERROR_DEVICE_HARDWARE_ERROR
        23 | 25 | 27 | 29 | 30 | 1117 | 1127 | 483 => IoErrorClass::Media,
        // ERROR_NOT_READY, ERROR_BAD_UNIT, ERROR_DEV_NOT_EXIST, ERROR_NO_SUCH_DEVICE,
        // ERROR_DEVICE_NOT_CONNECTED, ERROR_DEVICE_REMOVED, ERROR_MEDIA_CHANGED,
        // ERROR_NO_MEDIA_IN_DRIVE, ERROR_GEN_FAILURE, ERROR_BAD_COMMAND
        21 | 20 | 55 | 433 | 1167 | 1617 | 1110 | 1112 | 31 | 22 => IoErrorClass::DeviceGone,
        // ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION
        5 | 32 | 33 => IoErrorClass::AccessDenied,
        // ERROR_WRITE_PROTECT
        19 => IoErrorClass::WriteProtected,
        // ERROR_SEM_TIMEOUT, ERROR_TIMEOUT
        121 | 1460 => IoErrorClass::Timeout,
        _ => return None,
    })
}

#[cfg(not(windows))]
fn classify_os_code(code: i32) -> Option<IoErrorClass> {
    Some(match code {
        5 => IoErrorClass::Media,           // EIO
        6 | 19 => IoErrorClass::DeviceGone, // ENXIO, ENODEV
        13 | 1 => IoErrorClass::AccessDenied,
        30 => IoErrorClass::WriteProtected, // EROFS
        110 => IoErrorClass::Timeout,
        _ => return None,
    })
}

/// Périphérique bloc : disque physique, fichier image ou périphérique simulé.
///
/// Contrat : `offset` et la longueur du tampon sont des multiples de
/// [`BlockDevice::sector_size`]. Une lecture ou une écriture est soit complète, soit en
/// erreur (jamais partielle).
pub trait BlockDevice: Send {
    /// Taille totale en octets.
    fn size(&self) -> u64;
    /// Taille de secteur logique (unité d'adressage LBA).
    fn sector_size(&self) -> u32;
    /// Taille de secteur physique (≥ logique).
    fn physical_sector_size(&self) -> u32 {
        self.sector_size()
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()>;
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()>;
    fn flush(&mut self) -> io::Result<()>;
    fn is_read_only(&self) -> bool;
    /// Description lisible, pour les journaux.
    fn label(&self) -> String;
}

fn check_io_args(dev_size: u64, sector: u32, offset: u64, len: usize) -> io::Result<()> {
    let s = sector as u64;
    if !offset.is_multiple_of(s) || !(len as u64).is_multiple_of(s) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            tf!(
                "E/S non alignée sur le secteur (offset={offset}, longueur={len}, secteur={sector})",
                "I/O not aligned to the sector (offset={offset}, length={len}, sector={sector})"
            ),
        ));
    }
    match offset.checked_add(len as u64) {
        Some(end) if end <= dev_size => Ok(()),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            tf!(
                "E/S hors des limites du périphérique (offset={offset}, longueur={len}, taille={dev_size})",
                "I/O beyond the end of the device (offset={offset}, length={len}, size={dev_size})"
            ),
        )),
    }
}

/// Vérifie alignement et bornes ; à appeler par chaque implémentation.
pub fn validate_io(dev: &dyn BlockDevice, offset: u64, len: usize) -> io::Result<()> {
    check_io_args(dev.size(), dev.sector_size(), offset, len)
}

/// Fichier image utilisé comme périphérique bloc.
pub struct FileDevice {
    file: File,
    path: PathBuf,
    size: u64,
    sector: u32,
    read_only: bool,
}

impl FileDevice {
    /// Ouvre une image en lecture seule.
    pub fn open_read_only(path: &Path, sector: u32) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).open(path)?;
        Self::from_file(file, path, sector, true)
    }

    /// Ouvre une image existante en lecture/écriture.
    pub fn open_read_write(path: &Path, sector: u32) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        Self::from_file(file, path, sector, false)
    }

    /// Crée (ou tronque) une image de la taille demandée, remplie de zéros.
    pub fn create(path: &Path, size: u64, sector: u32) -> io::Result<Self> {
        if !size.is_multiple_of(sector as u64) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                t!("la taille de l'image doit être un multiple de la taille de secteur", "the image size must be a multiple of the sector size"),
            ));
        }
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(path)?;
        file.set_len(size)?;
        Self::from_file(file, path, sector, false)
    }

    fn from_file(file: File, path: &Path, sector: u32, read_only: bool) -> io::Result<Self> {
        if !sector.is_power_of_two() || sector < 512 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, t!("taille de secteur invalide", "invalid sector size")));
        }
        let len = file.metadata()?.len();
        // Une image dont la taille n'est pas un multiple du secteur est tronquée au
        // dernier secteur complet (le reliquat n'est pas adressable en LBA).
        let size = len - (len % sector as u64);
        Ok(Self { file, path: path.to_path_buf(), size, sector, read_only })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl BlockDevice for FileDevice {
    fn size(&self) -> u64 {
        self.size
    }
    fn sector_size(&self) -> u32 {
        self.sector
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        check_io_args(self.size, self.sector, offset, buf.len())?;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.read_exact(buf)
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()> {
        if self.read_only {
            return Err(fault(IoErrorClass::WriteProtected));
        }
        check_io_args(self.size, self.sector, offset, buf.len())?;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        if self.read_only {
            return Ok(());
        }
        self.file.sync_all()
    }
    fn is_read_only(&self) -> bool {
        self.read_only
    }
    fn label(&self) -> String {
        format!("image {}", self.path.display())
    }
}

/// Périphérique en mémoire (tests unitaires).
pub struct MemDevice {
    pub data: Vec<u8>,
    sector: u32,
    pub read_only: bool,
    pub writes: u64,
}

impl MemDevice {
    pub fn new(size: u64, sector: u32) -> Self {
        assert!(size.is_multiple_of(sector as u64));
        Self { data: vec![0u8; size as usize], sector, read_only: false, writes: 0 }
    }
    pub fn from_vec(data: Vec<u8>, sector: u32) -> Self {
        assert!((data.len() as u64).is_multiple_of(sector as u64));
        Self { data, sector, read_only: false, writes: 0 }
    }
}

impl BlockDevice for MemDevice {
    fn size(&self) -> u64 {
        self.data.len() as u64
    }
    fn sector_size(&self) -> u32 {
        self.sector
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        check_io_args(self.size(), self.sector, offset, buf.len())?;
        let o = offset as usize;
        buf.copy_from_slice(&self.data[o..o + buf.len()]);
        Ok(())
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()> {
        if self.read_only {
            return Err(fault(IoErrorClass::WriteProtected));
        }
        check_io_args(self.size(), self.sector, offset, buf.len())?;
        let o = offset as usize;
        self.data[o..o + buf.len()].copy_from_slice(buf);
        self.writes += 1;
        Ok(())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
    fn is_read_only(&self) -> bool {
        self.read_only
    }
    fn label(&self) -> String {
        tf!("mémoire ({} octets)", "memory ({} bytes)", self.data.len())
    }
}

/// Enveloppe garantissant qu'aucune écriture n'atteint le périphérique sous-jacent.
/// Le disque source d'un clonage est toujours manipulé à travers cette enveloppe.
pub struct ReadOnly<D: BlockDevice>(pub D);

impl<D: BlockDevice> BlockDevice for ReadOnly<D> {
    fn size(&self) -> u64 {
        self.0.size()
    }
    fn sector_size(&self) -> u32 {
        self.0.sector_size()
    }
    fn physical_sector_size(&self) -> u32 {
        self.0.physical_sector_size()
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.0.read_at(offset, buf)
    }
    fn write_at(&mut self, _offset: u64, _buf: &[u8]) -> io::Result<()> {
        Err(fault(IoErrorClass::WriteProtected))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn label(&self) -> String {
        self.0.label()
    }
}

impl BlockDevice for Box<dyn BlockDevice> {
    fn size(&self) -> u64 {
        (**self).size()
    }
    fn sector_size(&self) -> u32 {
        (**self).sector_size()
    }
    fn physical_sector_size(&self) -> u32 {
        (**self).physical_sector_size()
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        (**self).read_at(offset, buf)
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()> {
        (**self).write_at(offset, buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        (**self).flush()
    }
    fn is_read_only(&self) -> bool {
        (**self).is_read_only()
    }
    fn label(&self) -> String {
        (**self).label()
    }
}

/// Tampon aligné sur 4096 octets (exigé par les E/S non tamponnées sous Windows).
pub struct AlignedBuf {
    ptr: *mut u8,
    len: usize,
}

const BUF_ALIGN: usize = 4096;

impl AlignedBuf {
    pub fn new(len: usize) -> Self {
        assert!(len > 0, "tampon vide");
        let layout = Layout::from_size_align(len, BUF_ALIGN).expect("layout");
        // SAFETY: layout non nul, alignement valide.
        let ptr = unsafe { alloc_zeroed(layout) };
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        Self { ptr, len }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        let layout = Layout::from_size_align(self.len, BUF_ALIGN).expect("layout");
        // SAFETY: alloué par alloc_zeroed avec le même layout.
        unsafe { dealloc(self.ptr, layout) };
    }
}

impl Deref for AlignedBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        // SAFETY: ptr valide pour len octets, initialisés à zéro.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}
impl DerefMut for AlignedBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: idem, accès exclusif via &mut self.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}
// SAFETY: le tampon possède sa mémoire, sans alias.
unsafe impl Send for AlignedBuf {}

/// Lecture alignée de `len` octets à `offset` (tous deux multiples du secteur).
pub fn read_vec(dev: &mut dyn BlockDevice, offset: u64, len: usize) -> io::Result<AlignedBuf> {
    let mut buf = AlignedBuf::new(len.max(1));
    dev.read_at(offset, &mut buf[..len])?;
    Ok(buf)
}

/// Vue `Read + Seek` (lecture seule) sur une fenêtre d'un périphérique, avec cache de
/// blocs alignés. Sert aux analyseurs de systèmes de fichiers.
pub struct DeviceReader<'a> {
    dev: &'a mut dyn BlockDevice,
    base: u64,
    len: u64,
    pos: u64,
    cache: AlignedBuf,
    cache_start: u64,
    cache_len: usize,
}

const READER_CACHE: usize = 128 * 1024;

impl<'a> DeviceReader<'a> {
    /// `base` et `len` en octets, multiples du secteur.
    pub fn new(dev: &'a mut dyn BlockDevice, base: u64, len: u64) -> Self {
        Self { dev, base, len, pos: 0, cache: AlignedBuf::new(READER_CACHE), cache_start: u64::MAX, cache_len: 0 }
    }

    pub fn window_len(&self) -> u64 {
        self.len
    }

    fn fill(&mut self, pos: u64) -> io::Result<()> {
        let sector = self.dev.sector_size() as u64;
        let block = READER_CACHE as u64;
        let start = pos - (pos % block);
        // Borne à la fenêtre, arrondie au secteur supérieur (la fenêtre est alignée).
        let mut end = (start + block).min(self.len);
        end = end.div_ceil(sector) * sector;
        let abs = self.base + start;
        if abs >= self.dev.size() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                t!(
                    "lecture au-delà de la fin du périphérique (partition hors limites ?)",
                    "read past the end of the device (partition out of bounds?)"
                ),
            ));
        }
        let end_abs = (self.base + end).min(self.dev.size());
        let n = (end_abs - abs) as usize;
        self.cache_len = 0;
        self.cache_start = u64::MAX;
        self.dev.read_at(abs, &mut self.cache[..n])?;
        self.cache_start = start;
        self.cache_len = n;
        Ok(())
    }
}

impl Read for DeviceReader<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.len || out.is_empty() {
            return Ok(0);
        }
        let in_cache = self.cache_start != u64::MAX && self.pos >= self.cache_start && self.pos < self.cache_start + self.cache_len as u64;
        if !in_cache {
            self.fill(self.pos)?;
        }
        let off = (self.pos - self.cache_start) as usize;
        let avail = (self.cache_len - off).min((self.len - self.pos) as usize);
        let n = avail.min(out.len());
        out[..n].copy_from_slice(&self.cache[off..off + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for DeviceReader<'_> {
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

/// Les analyseurs qui exigent `Write` (fatfs) reçoivent une implémentation qui refuse
/// toute écriture : aucune modification ne peut atteindre le disque.
impl Write for DeviceReader<'_> {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, t!("vue en lecture seule", "read-only view")))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mem_device_roundtrip_and_alignment() {
        let mut d = MemDevice::new(4096, 512);
        let data = vec![0xABu8; 1024];
        d.write_at(512, &data).unwrap();
        let mut back = vec![0u8; 1024];
        d.read_at(512, &mut back).unwrap();
        assert_eq!(back, data);
        assert!(d.read_at(100, &mut back).is_err(), "offset non aligné refusé");
        assert!(d.read_at(3584, &mut back).is_err(), "dépassement refusé");
    }

    #[test]
    fn read_only_wrapper_blocks_writes() {
        let mut d = ReadOnly(MemDevice::new(4096, 512));
        let err = d.write_at(0, &[0u8; 512]).unwrap_err();
        assert_eq!(classify(&err), IoErrorClass::WriteProtected);
        assert_eq!(d.0.writes, 0);
    }

    #[test]
    fn device_reader_reads_across_cache_blocks() {
        let size = 3 * READER_CACHE as u64 + 4096;
        let mut d = MemDevice::new(size + 8192, 512);
        for (i, b) in d.data.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let expected = d.data.clone();
        let mut r = DeviceReader::new(&mut d, 4096, size);
        let mut all = Vec::new();
        r.read_to_end(&mut all).unwrap();
        assert_eq!(all.len() as u64, size);
        assert_eq!(&all[..], &expected[4096..4096 + size as usize]);
        r.seek(SeekFrom::Start(READER_CACHE as u64 - 3)).unwrap();
        let mut six = [0u8; 6];
        r.read_exact(&mut six).unwrap();
        let o = 4096 + READER_CACHE - 3;
        assert_eq!(&six, &expected[o..o + 6]);
        assert!(r.write(&[1]).is_err());
    }

    #[test]
    fn classify_marked_faults() {
        assert_eq!(classify(&fault(IoErrorClass::Media)), IoErrorClass::Media);
        assert_eq!(classify(&fault(IoErrorClass::DeviceGone)), IoErrorClass::DeviceGone);
    }
}
