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
    /// From now on (`true`), or no longer (`false`), reads come from the medium itself
    /// rather than from a copy kept in memory by the system. The verification after a
    /// copy uses it: it must show what the medium holds, not what is still in memory.
    /// Returns whether reads now bypass the system's cache (a physical disk is always
    /// read that way; an image file on Windows is reopened without buffering).
    fn set_uncached_reads(&mut self, on: bool) -> io::Result<bool> {
        let _ = on;
        Ok(false)
    }
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

/// Second handle on an image file, opened without buffering
/// (`FILE_FLAG_NO_BUFFERING`): its reads come from the disk that holds the file, not
/// from the system's cache. Such reads must cover whole sectors of that disk, at
/// aligned positions, into aligned memory: each request is widened to the sectors it
/// touches and goes through an aligned buffer.
#[cfg_attr(not(windows), allow(dead_code))]
struct DirectReader {
    file: File,
    /// Alignment of positions, lengths and memory (≥ the disk's sector).
    align: u64,
    buf: Option<AlignedBuf>,
}

/// Largest single unbuffered read.
#[cfg(windows)]
const DIRECT_IO_MAX: usize = 8 << 20;

#[cfg(windows)]
impl DirectReader {
    /// Opens a second handle on the file of `file` itself (`ReOpenFile`), never on its
    /// path: the file read back is the one written, whatever happened to the name since.
    fn open(file: &File) -> io::Result<Self> {
        use std::os::windows::io::{AsRawHandle, FromRawHandle};
        use windows_sys::Win32::Foundation::{GENERIC_READ, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_NO_BUFFERING, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STORAGE_INFO, FileStorageInfo,
            GetFileInformationByHandleEx, ReOpenFile,
        };
        // SAFETY: zeroed plain C structure, filled by the call for its stated size.
        let mut info: FILE_STORAGE_INFO = unsafe { std::mem::zeroed() };
        // SAFETY: valid handle owned by `file`; buffer of the announced size.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileStorageInfo,
                (&mut info as *mut FILE_STORAGE_INFO).cast(),
                std::mem::size_of::<FILE_STORAGE_INFO>() as u32,
            )
        };
        // Sector sizes of the disk holding the file; 4096 covers every common disk when
        // the file system does not tell (network share).
        let sector = if ok != 0 { info.LogicalBytesPerSector.max(info.PhysicalBytesPerSectorForAtomicity) } else { 0 };
        let align = (sector as usize).max(4096);
        if !align.is_power_of_two() || align > BUF_ALIGN {
            return Err(io::Error::new(io::ErrorKind::Unsupported, tf!("secteurs de {align} octets", "{align}-byte sectors")));
        }
        // SAFETY: valid handle; the new handle is owned by the `File` built below.
        let h =
            unsafe { ReOpenFile(file.as_raw_handle(), GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, FILE_FLAG_NO_BUFFERING) };
        if h == INVALID_HANDLE_VALUE || h.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `h` is a new, valid handle that nothing else owns.
        let file = unsafe { File::from_raw_handle(h) };
        Ok(Self { file, align: align as u64, buf: None })
    }

    fn read(&mut self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        use std::os::windows::fs::FileExt;
        let a = self.align;
        let start = offset / a * a;
        let span = ((offset + out.len() as u64).div_ceil(a) * a - start) as usize;
        // Bytes needed from `start`; the file may end inside the last aligned block.
        let need = (offset - start) as usize + out.len();
        let mut buf = match self.buf.take() {
            Some(b) if b.len() >= span => b,
            _ => AlignedBuf::new(span),
        };
        let mut got = 0usize;
        let mut result = Ok(());
        while got < need {
            let n = (span - got).min(DIRECT_IO_MAX);
            match self.file.seek_read(&mut buf[got..got + n], start + got as u64) {
                Ok(0) => {
                    result = Err(io::Error::new(io::ErrorKind::UnexpectedEof, t!("fin de l'image atteinte", "end of the image reached")));
                    break;
                }
                Ok(k) => got += k,
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
        }
        if result.is_ok() {
            let skip = (offset - start) as usize;
            out.copy_from_slice(&buf[skip..skip + out.len()]);
        }
        self.buf = Some(buf);
        result
    }
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

/// Identity of a file on its volume (volume serial number and file ID), independent of
/// the name used to reach it: two hard links, or a local path and an administrative
/// share (`\\host\c$\...`) to the same file, give the same identity. Path comparisons
/// cannot see through those aliases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileIdentity {
    volume: u64,
    file: u128,
}

impl FileIdentity {
    /// Identity of an open file; `None` when the file system does not report one.
    #[cfg(windows)]
    pub fn of(file: &File) -> io::Result<Option<Self>> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_ID_INFO, FileIdInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
        };
        // 128-bit file ID first (ReFS needs it), then the classic 64-bit index.
        // SAFETY: plain C structure, valid when zeroed.
        let mut id: FILE_ID_INFO = unsafe { std::mem::zeroed() };
        // SAFETY: valid handle owned by `file`; output buffer of the announced size.
        let ok = unsafe {
            GetFileInformationByHandleEx(file.as_raw_handle(), FileIdInfo, (&mut id as *mut FILE_ID_INFO).cast(), size_of::<FILE_ID_INFO>() as u32)
        };
        if ok != 0 {
            let file_id = u128::from_le_bytes(id.FileId.Identifier);
            return Ok((file_id != 0).then_some(Self { volume: id.VolumeSerialNumber, file: file_id }));
        }
        // SAFETY: plain C structure, valid when zeroed.
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: valid handle; output structure of the expected type.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let file_id = ((info.nFileIndexHigh as u128) << 32) | info.nFileIndexLow as u128;
        Ok((file_id != 0).then_some(Self { volume: info.dwVolumeSerialNumber as u64, file: file_id }))
    }

    #[cfg(unix)]
    pub fn of(file: &File) -> io::Result<Option<Self>> {
        use std::os::unix::fs::MetadataExt;
        let m = file.metadata()?;
        Ok(Some(Self { volume: m.dev(), file: m.ino() as u128 }))
    }

    #[cfg(not(any(windows, unix)))]
    pub fn of(_: &File) -> io::Result<Option<Self>> {
        Ok(None)
    }

    /// Identity of the file at `path`, opened only to query it (no read access needed,
    /// every sharing mode allowed: it does not disturb a program using the file).
    pub fn of_path(path: &Path) -> io::Result<Option<Self>> {
        #[cfg(windows)]
        let file = {
            use std::os::windows::fs::OpenOptionsExt;
            // No access right, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE.
            OpenOptions::new().access_mode(0).share_mode(7).open(path)?
        };
        #[cfg(not(windows))]
        let file = File::open(path)?;
        Self::of(&file)
    }
}

/// True if both paths name the same existing file, whatever the names used (hard links,
/// administrative share, `\\?\` prefix, 8.3 short name...). False if either is missing
/// or if the file system cannot tell.
pub fn same_file(a: &Path, b: &Path) -> bool {
    matches!((FileIdentity::of_path(a), FileIdentity::of_path(b)), (Ok(Some(x)), Ok(Some(y))) if x == y)
}

/// Windows error: the file is open in a way that forbids this access.
const ERROR_SHARING_VIOLATION: i32 = 32;

fn not_a_file() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        t!(
            "ce chemin ne désigne pas un fichier ordinaire (périphérique ou dossier) : un disque se choisit dans la liste des disques",
            "this path is not an ordinary file (device or folder): a disk is chosen from the list of disks"
        ),
    )
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
    /// Unbuffered reading, while [`BlockDevice::set_uncached_reads`] is on.
    direct: Option<DirectReader>,
}

impl FileDevice {
    /// Opens an image read-only.
    pub fn open_read_only(path: &Path, sector: u32) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).open(path)?;
        Self::from_file(file, path, sector, true)
    }

    /// Opens the source image of a clone read-only, forbidding any writer while it stays
    /// open (Windows sharing mode). Opening the same file for writing afterwards — under
    /// this name or any other (hard link, share) — then fails in Windows itself: the clone
    /// cannot overwrite its own source, and no other program can change the source
    /// during the copy.
    pub fn open_source(path: &Path, sector: u32) -> io::Result<Self> {
        let mut opts = OpenOptions::new();
        opts.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            opts.share_mode(1); // FILE_SHARE_READ only
        }
        let file = opts.open(path).map_err(|e| {
            if e.raw_os_error() == Some(ERROR_SHARING_VIOLATION) {
                io::Error::new(
                    io::ErrorKind::ResourceBusy,
                    t!(
                        "l'image source est ouverte en écriture par un autre programme : fermez-le, l'image changerait pendant la copie",
                        "the source image is open for writing in another program: close it, the image would change during the copy"
                    ),
                )
            } else {
                e
            }
        })?;
        Self::from_file(file, path, sector, true)
    }

    /// Identity of the open file (see [`FileIdentity`]).
    pub fn identity(&self) -> io::Result<Option<FileIdentity>> {
        FileIdentity::of(&self.file)
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
        Self::create_excluding(path, size, sector, None)
    }

    /// Like [`FileDevice::create`], but refuses — before changing a single byte — a file
    /// that is not an ordinary file (device, `NUL`...) or that is the file `source`
    /// under another name. The check is made on the handle actually used for writing,
    /// so a file swapped in at the last moment is caught as well. The file is only
    /// emptied afterwards, through that handle.
    ///
    /// On Windows, a source opened with [`FileDevice::open_source`] forbids writers: the
    /// same file cannot even be opened here, whatever its name.
    pub fn create_excluding(path: &Path, size: u64, sector: u32, source: Option<FileIdentity>) -> io::Result<Self> {
        if !size.is_multiple_of(sector as u64) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                t!("la taille de l'image doit être un multiple de la taille de secteur", "the image size must be a multiple of the sector size"),
            ));
        }
        // No truncation at opening: nothing is changed before the checks below.
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path).map_err(|e| {
            if e.raw_os_error() == Some(ERROR_SHARING_VIOLATION) {
                io::Error::new(
                    io::ErrorKind::ResourceBusy,
                    t!(
                        "le fichier de destination est ouvert par un autre programme, ou c'est le fichier source sous un autre nom",
                        "the destination file is open in another program, or it is the source file under another name"
                    ),
                )
            } else {
                e
            }
        })?;
        if !file.metadata().map_err(|_| not_a_file())?.is_file() {
            return Err(not_a_file());
        }
        if let Some(src) = source
            && FileIdentity::of(&file)? == Some(src)
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                t!(
                    "le fichier de destination est le fichier source lui-même (sous un autre nom) : il serait effacé",
                    "the destination file is the source file itself (under another name): it would be erased"
                ),
            ));
        }
        file.set_len(0)?;
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
        let meta = file.metadata().map_err(|_| not_a_file())?;
        // A device (`\\.\PhysicalDrive1`, `\\.\E:`, `NUL`...) is never an image: physical
        // disks go through `Target::Disk` and all its protections.
        if !meta.is_file() {
            return Err(not_a_file());
        }
        let len = meta.len();
        // An image whose size is not a multiple of the sector is truncated to the
        // last complete sector (the remainder is not LBA-addressable).
        let size = len - (len % sector as u64);
        let sparse = !read_only && is_sparse(&file);
        Ok(Self { file, path: path.to_path_buf(), size, sector, read_only, sparse, direct: None })
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
        #[cfg(windows)]
        if let Some(d) = self.direct.as_mut() {
            return d.read(offset, buf);
        }
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
    fn set_uncached_reads(&mut self, on: bool) -> io::Result<bool> {
        if !on {
            self.direct = None;
            return Ok(false);
        }
        #[cfg(windows)]
        {
            if self.direct.is_none() {
                self.direct = Some(DirectReader::open(&self.file)?);
            }
            Ok(true)
        }
        #[cfg(not(windows))]
        Ok(false)
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
    fn set_uncached_reads(&mut self, on: bool) -> io::Result<bool> {
        self.0.set_uncached_reads(on)
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
    fn set_uncached_reads(&mut self, on: bool) -> io::Result<bool> {
        (**self).set_uncached_reads(on)
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

    /// The verification reads an image back from the disk holding it, bypassing the
    /// system cache. Any sector reads back what was written, at positions and lengths
    /// that are not multiples of the disk's 4 KiB, up to the last sector of a file whose
    /// size is not a multiple of 4 KiB.
    #[cfg(windows)]
    #[test]
    fn image_reads_back_from_its_disk_without_the_cache() {
        use std::os::windows::fs::FileExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("direct.img");
        let size = (1u64 << 20) + 3 * 512;
        let mut d = FileDevice::create(&path, size, 512).unwrap();
        let pattern: Vec<u8> = (0..size as usize).map(|i| (i * 7 + i / 512) as u8).collect();
        d.write_at(0, &pattern).unwrap();
        d.flush().unwrap();
        assert!(d.set_uncached_reads(true).unwrap());
        // The second handle really is unbuffered: Windows refuses a read at a position
        // that is not sector-aligned (a buffered handle accepts it).
        let mut probe = AlignedBuf::new(4096);
        let err = d.direct.as_ref().unwrap().file.seek_read(&mut probe, 1).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(87), "{err}");
        assert!(d.file.seek_read(&mut probe, 1).is_ok());
        let size = size as usize;
        for (off, len) in [(0, 512), (512, 1024), (4096 - 512, 1024), (size - 512, 512), (size - 3 * 512, 3 * 512), (512_000, 32_768), (0, size)] {
            let mut b = vec![0u8; len];
            d.read_at(off as u64, &mut b).unwrap();
            assert!(b == pattern[off..off + len], "offset {off}, length {len}");
        }
        // Data written later, not flushed yet, is read back too: the file system writes
        // its cache back before an unbuffered read.
        d.write_at(512, &[0xEE; 512]).unwrap();
        let mut b = vec![0u8; 512];
        d.read_at(512, &mut b).unwrap();
        assert_eq!(b, [0xEE; 512]);
        assert!(!d.set_uncached_reads(false).unwrap());
        d.read_at(0, &mut b).unwrap();
        assert!(b == pattern[..512]);
    }

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
