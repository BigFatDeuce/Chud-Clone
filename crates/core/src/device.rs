//! Block device abstraction.
//!
//! The whole engine (partition tables, file systems, copy, verification) works
//! only through [`BlockDevice`]. This makes it possible to test on image files exactly
//! the same code that runs on a physical disk.

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};

/// Class of an I/O error, independent of the platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IoErrorClass {
    /// Media error: unreadable sector, CRC, read/write fault.
    Media,
    /// The device disappeared (disconnection, cable, power).
    DeviceGone,
    /// Access denied (privileges, locked by another process).
    AccessDenied,
    /// Write-protected media.
    WriteProtected,
    /// Timeout: often a struggling disk or an unstable USB bridge.
    Timeout,
    /// Autre erreur.
    Other,
}

/// Marker attached to an `io::Error` to force its class (used by the
/// simulated devices and by the Windows layer).
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

/// Builds an `io::Error` carrying an explicit class.
pub fn fault(class: IoErrorClass) -> io::Error {
    io::Error::other(DevFault(class))
}

/// Determines the class of an I/O error.
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

/// Win32 error codes relevant to disks.
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

/// Block device: physical disk, image file or simulated device.
///
/// Contract: `offset` and the buffer length are multiples of
/// [`BlockDevice::sector_size`]. A read or a write is either complete or an
/// error (never partial).
pub trait BlockDevice: Send {
    /// Total size in bytes.
    fn size(&self) -> u64;
    /// Logical sector size (LBA addressing unit).
    fn sector_size(&self) -> u32;
    /// Physical sector size (≥ logical).
    fn physical_sector_size(&self) -> u32 {
        self.sector_size()
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()>;
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()>;
    fn flush(&mut self) -> io::Result<()>;
    fn is_read_only(&self) -> bool;
    /// Readable description, for the logs.
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

/// Checks alignment and bounds; to be called by every implementation.
pub fn validate_io(dev: &dyn BlockDevice, offset: u64, len: usize) -> io::Result<()> {
    check_io_args(dev.size(), dev.sector_size(), offset, len)
}

/// Marks a file as sparse (NTFS). Fails on file systems that do not support it.
#[cfg(windows)]
fn set_sparse(file: &File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::FSCTL_SET_SPARSE;
    let mut returned = 0u32;
    // SAFETY: valid file handle owned by `file`; no input or output buffer
    // (no input buffer means "set the sparse flag").
    let ok = unsafe {
        DeviceIoControl(file.as_raw_handle(), FSCTL_SET_SPARSE, std::ptr::null(), 0, std::ptr::null_mut(), 0, &mut returned, std::ptr::null_mut())
    };
    if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

#[cfg(not(windows))]
fn set_sparse(_: &File) -> io::Result<()> {
    // On Unix file systems, a file extended with set_len is already sparse.
    Ok(())
}

#[cfg(windows)]
fn is_sparse(file: &File) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_SPARSE_FILE: u32 = 0x200;
    file.metadata().map(|m| m.file_attributes() & FILE_ATTRIBUTE_SPARSE_FILE != 0).unwrap_or(false)
}

#[cfg(not(windows))]
fn is_sparse(_: &File) -> bool {
    true
}

/// Turns a range of a sparse file into a hole: it reads back as zeros and takes no
/// space on disk, whatever it held before (NTFS `FSCTL_SET_ZERO_DATA`).
#[cfg(windows)]
fn punch_hole(file: &File, offset: u64, len: u64) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::FSCTL_SET_ZERO_DATA;
    // FILE_ZERO_DATA_INFORMATION { FileOffset, BeyondFinalZero }
    let mut info = [0u8; 16];
    info[..8].copy_from_slice(&(offset as i64).to_le_bytes());
    info[8..].copy_from_slice(&((offset + len) as i64).to_le_bytes());
    let mut returned = 0u32;
    // SAFETY: valid file handle owned by `file`; 16-byte input buffer, no output.
    let ok = unsafe {
        DeviceIoControl(file.as_raw_handle(), FSCTL_SET_ZERO_DATA, info.as_ptr().cast(), 16, std::ptr::null_mut(), 0, &mut returned, std::ptr::null_mut())
    };
    if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

#[cfg(not(windows))]
fn punch_hole(_: &File, _: u64, _: u64) -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

/// Space a file actually takes on disk (less than its size for a sparse image).
#[cfg(windows)]
pub fn allocated_bytes(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut high = 0u32;
    // SAFETY: NUL-terminated path, valid out pointer.
    let low = unsafe { windows_sys::Win32::Storage::FileSystem::GetCompressedFileSizeW(wide.as_ptr(), &mut high) };
    if low == u32::MAX && io::Error::last_os_error().raw_os_error().unwrap_or(0) != 0 {
        return None;
    }
    Some(((high as u64) << 32) | low as u64)
}

#[cfg(not(windows))]
pub fn allocated_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| m.blocks() * 512)
}

/// Smallest write turned into a hole rather than written (smaller writes of zeros
/// are cheaper to write than to deallocate).
const HOLE_MIN: usize = 64 * 1024;

/// True if every byte of the buffer is zero.
pub fn is_all_zero(buf: &[u8]) -> bool {
    // SAFETY: u64 has no invalid bit patterns; align_to only reinterprets the middle.
    let (head, mid, tail) = unsafe { buf.align_to::<u64>() };
    head.iter().all(|&b| b == 0) && mid.iter().all(|&w| w == 0) && tail.iter().all(|&b| b == 0)
}

/// Image file used as a block device.
pub struct FileDevice {
    file: File,
    path: PathBuf,
    size: u64,
    sector: u32,
    read_only: bool,
    /// Sparse file: runs of zeros are stored as holes instead of being written, so
    /// an image only takes the space of the data it holds.
    sparse: bool,
}

impl FileDevice {
    /// Opens an image read-only.
    pub fn open_read_only(path: &Path, sector: u32) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).open(path)?;
        Self::from_file(file, path, sector, true)
    }

    /// Opens an existing image read/write.
    pub fn open_read_write(path: &Path, sector: u32) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        Self::from_file(file, path, sector, false)
    }

    /// Creates (or truncates) an image of the requested size, filled with zeros.
    ///
    /// On NTFS the file is made sparse before it is sized. Without that, the first write
    /// near the end of the file (the backup GPT area is cleared before the copy) forces
    /// Windows to physically write zeros over the whole file first: tens of minutes for
    /// a large image on a hard disk, with no progress to report. A sparse file reads as
    /// zeros where nothing was written and only takes the space actually copied.
    pub fn create(path: &Path, size: u64, sector: u32) -> io::Result<Self> {
        if !size.is_multiple_of(sector as u64) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                t!("la taille de l'image doit être un multiple de la taille de secteur", "the image size must be a multiple of the sector size"),
            ));
        }
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(path)?;
        // File systems without sparse files (FAT32, exFAT) refuse it: the image is then
        // an ordinary file, which stays correct, only slower to start.
        let _ = set_sparse(&file);
        file.set_len(size)?;
        Self::from_file(file, path, sector, false)
    }

    /// True if the image is stored as a sparse file.
    pub fn is_sparse(&self) -> bool {
        is_sparse(&self.file)
    }

    fn from_file(file: File, path: &Path, sector: u32, read_only: bool) -> io::Result<Self> {
        if !sector.is_power_of_two() || sector < 512 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, t!("taille de secteur invalide", "invalid sector size")));
        }
        let len = file.metadata()?.len();
        // An image whose size is not a multiple of the sector is truncated to the
        // last complete sector (the remainder is not LBA-addressable).
        let size = len - (len % sector as u64);
        let sparse = !read_only && is_sparse(&file);
        Ok(Self { file, path: path.to_path_buf(), size, sector, read_only, sparse })
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
        if self.sparse && buf.len() >= HOLE_MIN && is_all_zero(buf) && punch_hole(&self.file, offset, buf.len() as u64).is_ok() {
            return Ok(());
        }
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

/// In-memory device (unit tests).
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

/// Wrapper guaranteeing that no write reaches the underlying device.
/// The source disk of a clone is always handled through this wrapper.
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

/// Buffer aligned on 4096 bytes (required by unbuffered I/O on Windows).
pub struct AlignedBuf {
    ptr: *mut u8,
    len: usize,
}

const BUF_ALIGN: usize = 4096;

impl AlignedBuf {
    pub fn new(len: usize) -> Self {
        assert!(len > 0, "empty buffer");
        let layout = Layout::from_size_align(len, BUF_ALIGN).expect("layout");
        // SAFETY: non-zero layout with a valid alignment.
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
        // SAFETY: allocated by alloc_zeroed with the same layout.
        unsafe { dealloc(self.ptr, layout) };
    }
}

impl Deref for AlignedBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        // SAFETY: ptr valid for len bytes, zero-initialised.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}
impl DerefMut for AlignedBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: same, exclusive access through &mut self.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}
// SAFETY: the buffer owns its memory, without aliasing.
unsafe impl Send for AlignedBuf {}

/// Aligned read of `len` bytes at `offset` (both multiples of the sector).
pub fn read_vec(dev: &mut dyn BlockDevice, offset: u64, len: usize) -> io::Result<AlignedBuf> {
    let mut buf = AlignedBuf::new(len.max(1));
    dev.read_at(offset, &mut buf[..len])?;
    Ok(buf)
}

/// `Read + Seek` view (read-only) of a window of a device, with a cache of
/// aligned blocks. Used by the file system parsers.
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
    /// `base` and `len` in bytes, multiples of the sector.
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
        // Clamp to the window, rounded up to the sector (the window is aligned).
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

/// Parsers that require `Write` (fatfs) get an implementation that refuses
/// any write: no modification can reach the disk.
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

    /// Regression: a large new image must not be zero-filled by the file system when
    /// the clone clears its last mebibyte first ("Preparing the destination" stalled
    /// for tens of minutes on a hard disk). The file is sparse, so the test uses no space.
    #[test]
    fn new_image_is_sparse_and_writable_at_the_end_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.img");
        let size = 64u64 << 30;
        let mut d = FileDevice::create(&path, size, 512).unwrap();
        assert_eq!(d.size(), size);
        // Checked before writing: on a non-sparse file the write below would take minutes.
        assert!(d.is_sparse(), "a new image must be a sparse file");
        let t = std::time::Instant::now();
        d.write_at(size - 1024 * 1024, &vec![0xAB; 1024 * 1024]).unwrap();
        d.flush().unwrap();
        assert!(t.elapsed() < std::time::Duration::from_secs(10), "write at the end took {:?}", t.elapsed());
        let mut back = vec![0xFFu8; 4096];
        d.read_at(size / 2, &mut back).unwrap();
        assert!(back.iter().all(|b| *b == 0), "unwritten areas read as zeros");
        d.read_at(size - 4096, &mut back).unwrap();
        assert!(back.iter().all(|b| *b == 0xAB));
        // Recreating an existing image keeps it sparse and empty.
        drop(d);
        let mut d = FileDevice::create(&path, size, 512).unwrap();
        assert!(d.is_sparse());
        d.read_at(size - 4096, &mut back).unwrap();
        assert!(back.iter().all(|b| *b == 0));
    }

    /// Zeros written to a sparse image become holes: they read back as zeros, also
    /// over data written before, and take no space.
    #[test]
    fn zeros_become_holes_in_a_sparse_image() {
        assert!(is_all_zero(&[0u8; 37]) && !is_all_zero(&[0, 0, 0, 1]) && is_all_zero(&[]));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("holes.img");
        let mib = 1024 * 1024;
        let mut d = FileDevice::create(&path, 8 * mib as u64, 512).unwrap();
        d.write_at(0, &vec![0x5A; 4 * mib]).unwrap();
        // Overwritten with zeros: the data must be gone, not kept under a skipped write.
        d.write_at(mib as u64, &vec![0; 2 * mib]).unwrap();
        d.write_at(4 * mib as u64, &vec![0; 4 * mib]).unwrap();
        d.flush().unwrap();
        let mut back = vec![0xFFu8; 4 * mib];
        d.read_at(0, &mut back).unwrap();
        assert!(back[..mib].iter().all(|b| *b == 0x5A));
        assert!(back[mib..3 * mib].iter().all(|b| *b == 0), "zeros over data read back as zeros");
        assert!(back[3 * mib..].iter().all(|b| *b == 0x5A));
        d.read_at(4 * mib as u64, &mut back).unwrap();
        assert!(back.iter().all(|b| *b == 0));
        #[cfg(windows)]
        {
            let used = allocated_bytes(&path).unwrap();
            assert!(used <= 3 * mib as u64, "only the data takes space: {used} bytes");
        }
    }

    #[test]
    fn mem_device_roundtrip_and_alignment() {
        let mut d = MemDevice::new(4096, 512);
        let data = vec![0xABu8; 1024];
        d.write_at(512, &data).unwrap();
        let mut back = vec![0u8; 1024];
        d.read_at(512, &mut back).unwrap();
        assert_eq!(back, data);
        assert!(d.read_at(100, &mut back).is_err(), "unaligned offset refused");
        assert!(d.read_at(3584, &mut back).is_err(), "out-of-range read refused");
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
