//! Copy and verification engine.
//!
//! Principles, inspired by ddrescue:
//! 1. First copy everything that reads without trouble, in large blocks. An area that
//!    fails is set aside instead of blocking the copy: a degrading disk must deliver
//!    as much healthy data as possible before its defects are insisted on.
//! 2. Then come back to the areas set aside, sector by sector, with a bounded number
//!    of attempts.
//! 3. Never hide an unreadable sector: every range is recorded (position, attempts,
//!    error) and what happens next depends on an explicit
//!    decision.
//!
//! A write error on the destination is never worked around: the copy
//! stops.

use crate::device::{AlignedBuf, BlockDevice, IoErrorClass, classify};
use crate::plan::CopyTask;
use serde::{Deserialize, Serialize};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnreadablePolicy {
    /// Stop: the clone is reported incomplete (default behaviour).
    Stop,
    /// Ask the user, once recovery has been attempted.
    Ask,
    /// Replace the unreadable sectors with zeros and continue.
    ZeroFill,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyMode {
    /// Aucune relecture.
    None,
    /// Reads the destination back and compares it with the checksums of the data read from the source.
    Hash,
    /// Reads the source AND the destination back and compares them.
    Full,
}

impl VerifyMode {
    pub fn label(self) -> &'static str {
        match self {
            VerifyMode::None => t!("aucune", "none"),
            VerifyMode::Hash => t!(
                "relecture de la destination comparée aux empreintes de la source",
                "destination read back and compared with the source's checksums"
            ),
            VerifyMode::Full => t!("comparaison complète de la source et de la destination", "full comparison of source and destination"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineOptions {
    /// Size of the copy blocks (bytes, multiple of 4096).
    pub chunk_size: usize,
    /// Size of the sub-blocks used to isolate a failing area.
    pub retry_block: usize,
    /// Retries per sector during the recovery phase.
    pub read_retries: u32,
    /// Write retries before giving up.
    pub write_retries: u32,
    pub unreadable: UnreadablePolicy,
    pub verify: VerifyMode,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            chunk_size: 4 * 1024 * 1024,
            retry_block: 64 * 1024,
            read_retries: 3,
            write_retries: 2,
            unreadable: UnreadablePolicy::Stop,
            verify: VerifyMode::Hash,
        }
    }
}

impl EngineOptions {
    pub fn validate(&self) -> Result<(), String> {
        if self.chunk_size < 4096 || !self.chunk_size.is_multiple_of(4096) || self.chunk_size > 64 * 1024 * 1024 {
            return Err(t!(
                "taille de bloc invalide (multiple de 4096, entre 4 Kio et 64 Mio)",
                "invalid block size (multiple of 4096, between 4 KiB and 64 MiB)"
            )
            .into());
        }
        if self.retry_block < 4096 || !self.retry_block.is_multiple_of(4096) || self.retry_block > self.chunk_size {
            return Err(t!("taille de sous-bloc invalide", "invalid sub-block size").into());
        }
        if self.read_retries > 50 {
            return Err(t!("nombre de tentatives de lecture déraisonnable (maximum 50)", "unreasonable number of read retries (maximum 50)").into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Preparing,
    Copying,
    Recovering,
    WritingTable,
    Verifying,
    /// Growth of the main partition's file system.
    Growing,
    Checking,
    Done,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Phase::Preparing => t!("Préparation de la destination", "Preparing the destination"),
            Phase::Copying => t!("Copie des données", "Copying data"),
            Phase::Recovering => t!("Nouvelle lecture des zones difficiles", "Re-reading difficult areas"),
            Phase::WritingTable => t!("Écriture de la table de partitions", "Writing the partition table"),
            Phase::Verifying => t!("Vérification des données copiées", "Verifying copied data"),
            Phase::Growing => t!("Agrandissement de la partition Windows", "Growing the Windows partition"),
            Phase::Checking => t!("Contrôle des structures du clone", "Checking the clone's structures"),
            Phase::Done => t!("Terminé", "Done"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Progress {
    pub phase: Phase,
    /// Progress in the current phase (bytes).
    pub done: u64,
    pub total: u64,
    pub elapsed_ms: u64,
    /// Smoothed instantaneous speed (bytes/s).
    pub speed_bps: u64,
    /// Average speed since the start of the phase.
    pub avg_bps: u64,
    pub eta_secs: Option<u64>,
    /// Read error events encountered.
    pub read_errors: u64,
    /// Bytes currently considered unreadable.
    pub bad_bytes: u64,
    /// Bytes recovered after a retry.
    pub recovered_bytes: u64,
    pub write_errors: u64,
    /// Current position on the source (LBA).
    pub current_lba: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Debug,
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Abort,
    ZeroFill,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BadOutcome {
    /// Decision pending.
    Pending,
    /// Not copied: the destination does not contain this data.
    NotCopied,
    /// Replaced with zeros on the destination.
    ZeroFilled,
}

/// Range permanently unreadable on the source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BadRange {
    pub src_offset: u64,
    pub dst_offset: u64,
    pub len: u64,
    /// Starting LBA on the source.
    pub lba: u64,
    pub sectors: u64,
    /// Read attempts made per sector.
    pub attempts: u32,
    pub error: String,
    pub class: IoErrorClass,
    pub outcome: BadOutcome,
}

/// Range read successfully after at least one failure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveredRange {
    pub lba: u64,
    pub sectors: u64,
    /// Attempt that succeeded (1 = first re-read in the recovery phase).
    pub attempt: u32,
}

#[derive(Debug, Clone)]
pub struct HashEntry {
    pub src: u64,
    pub dst: u64,
    pub len: u64,
    pub hash: [u8; 32],
    /// Range filled with zeros (no corresponding source data).
    pub zero_fill: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum StopReason {
    Completed,
    Cancelled,
    /// The source disappeared or refuses access.
    SourceLost(String),
    /// Write error on the destination.
    DestinationFailed(String),
    /// Unreadable sectors and a decision to stop.
    Unreadable,
    Internal(String),
}

/// Information given when deciding what to do with unreadable sectors.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnreadableInfo {
    pub ranges: Vec<BadRange>,
    pub total_bytes: u64,
    pub total_sectors: u64,
    pub sector_size: u32,
}

pub trait Observer: Send + Sync {
    fn progress(&self, _p: &Progress) {}
    fn log(&self, _level: LogLevel, _msg: &str) {}
    /// Called once per pass, after the recovery phase, if sectors remain
    /// unreadable and the policy is `Ask`.
    fn decide_unreadable(&self, _info: &UnreadableInfo) -> Decision {
        Decision::Abort
    }
}

/// Observateur muet.
pub struct NullObserver;
impl Observer for NullObserver {}

/// Cancellation control shared between the interface and the engine.
#[derive(Default)]
pub struct Control {
    cancel: AtomicBool,
}

impl Control {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }
}

struct Meter {
    start: Instant,
    last_emit: Instant,
    last_done: u64,
    ema: f64,
}

impl Meter {
    fn new() -> Self {
        let now = Instant::now();
        Self { start: now, last_emit: now, last_done: 0, ema: 0.0 }
    }
}

#[derive(Debug, Clone, Default)]
pub struct CopyStats {
    pub bytes_total: u64,
    /// Bytes read from the source and written to the destination.
    pub bytes_copied: u64,
    pub bytes_zero_filled: u64,
    pub read_error_events: u64,
    pub write_error_events: u64,
    pub recovered_bytes: u64,
    pub bad_ranges: Vec<BadRange>,
    pub recovered: Vec<RecoveredRange>,
    pub hashes: Vec<HashEntry>,
    pub copy_ms: u64,
}

impl CopyStats {
    pub fn bad_bytes(&self) -> u64 {
        self.bad_ranges.iter().map(|b| b.len).sum()
    }
}

/// Runs the copies. One `Copier` can chain several batches of tasks;
/// the statistics accumulate.
pub struct Copier<'a> {
    src: &'a mut dyn BlockDevice,
    dst: &'a mut dyn BlockDevice,
    opts: &'a EngineOptions,
    ctl: &'a Control,
    obs: &'a dyn Observer,
    pub stats: CopyStats,
    done: u64,
    phase: Phase,
    meter: Meter,
    current_lba: u64,
}

fn is_fatal_source(class: IoErrorClass) -> bool {
    matches!(class, IoErrorClass::DeviceGone | IoErrorClass::AccessDenied)
}

impl<'a> Copier<'a> {
    pub fn new(
        src: &'a mut dyn BlockDevice,
        dst: &'a mut dyn BlockDevice,
        opts: &'a EngineOptions,
        ctl: &'a Control,
        obs: &'a dyn Observer,
        bytes_total: u64,
    ) -> Self {
        Self {
            src,
            dst,
            opts,
            ctl,
            obs,
            stats: CopyStats { bytes_total, ..Default::default() },
            done: 0,
            phase: Phase::Copying,
            meter: Meter::new(),
            current_lba: 0,
        }
    }

    fn emit(&mut self, force: bool) {
        let now = Instant::now();
        let dt = now.duration_since(self.meter.last_emit);
        if !force && dt < Duration::from_millis(100) {
            return;
        }
        let secs = dt.as_secs_f64();
        if secs > 0.0 {
            let inst = (self.done - self.meter.last_done) as f64 / secs;
            self.meter.ema = if self.meter.ema == 0.0 { inst } else { 0.7 * self.meter.ema + 0.3 * inst };
        }
        self.meter.last_emit = now;
        self.meter.last_done = self.done;
        let elapsed = now.duration_since(self.meter.start);
        let avg = if elapsed.as_secs_f64() > 0.0 { self.done as f64 / elapsed.as_secs_f64() } else { 0.0 };
        let remaining = self.stats.bytes_total.saturating_sub(self.done);
        // The estimate relies on the average, which is steadier, tempered by the recent speed.
        let basis = if self.meter.ema > 0.0 && avg > 0.0 { 0.5 * self.meter.ema + 0.5 * avg } else { avg.max(self.meter.ema) };
        let eta = if basis > 1.0 && elapsed.as_millis() > 1500 { Some((remaining as f64 / basis).ceil() as u64) } else { None };
        let p = Progress {
            phase: self.phase,
            done: self.done.min(self.stats.bytes_total),
            total: self.stats.bytes_total,
            elapsed_ms: elapsed.as_millis() as u64,
            speed_bps: self.meter.ema as u64,
            avg_bps: avg as u64,
            eta_secs: eta,
            read_errors: self.stats.read_error_events,
            bad_bytes: self.stats.bad_bytes(),
            recovered_bytes: self.stats.recovered_bytes,
            write_errors: self.stats.write_error_events,
            current_lba: self.current_lba,
        };
        self.obs.progress(&p);
    }

    /// Read from the source. A device disappearing is retried briefly:
    /// a USB bridge that resets often comes back within a second.
    fn read_src(&mut self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        let mut tries = 0;
        loop {
            match self.src.read_at(off, buf) {
                Ok(()) => return Ok(()),
                Err(e) if classify(&e) == IoErrorClass::DeviceGone && tries < 2 && !self.ctl.is_cancelled() => {
                    tries += 1;
                    self.obs.log(
                        LogLevel::Warning,
                        &tf!("Source injoignable ({e}) — nouvelle tentative {tries}/2", "Source unreachable ({e}) — retry {tries}/2"),
                    );
                    std::thread::sleep(Duration::from_millis(400));
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn write_dst(&mut self, src_off: u64, dst_off: u64, data: &[u8], zero_fill: bool) -> Result<(), StopReason> {
        let mut attempt = 0;
        loop {
            match self.dst.write_at(dst_off, data) {
                Ok(()) => break,
                Err(e) => {
                    self.stats.write_error_events += 1;
                    let class = classify(&e);
                    let lba = dst_off / self.dst.sector_size() as u64;
                    let retriable = !matches!(class, IoErrorClass::WriteProtected | IoErrorClass::AccessDenied);
                    if attempt < self.opts.write_retries && retriable && !self.ctl.is_cancelled() {
                        attempt += 1;
                        self.obs.log(
                            LogLevel::Warning,
                            &tf!(
                                "Erreur d'écriture sur la destination au LBA {lba} ({e}) — tentative {attempt}/{}",
                                "Write error on the destination at LBA {lba} ({e}) — attempt {attempt}/{}",
                                self.opts.write_retries
                            ),
                        );
                        std::thread::sleep(Duration::from_millis(300));
                        continue;
                    }
                    let msg = tf!(
                        "Erreur d'écriture sur la destination au LBA {lba} ({} secteurs) : {e} [{}]",
                        "Write error on the destination at LBA {lba} ({} sectors): {e} [{}]",
                        data.len() as u64 / self.dst.sector_size() as u64,
                        crate::device::DevFault(class)
                    );
                    self.obs.log(LogLevel::Error, &msg);
                    return Err(StopReason::DestinationFailed(msg));
                }
            }
        }
        if self.opts.verify != VerifyMode::None {
            self.stats.hashes.push(HashEntry { src: src_off, dst: dst_off, len: data.len() as u64, hash: *blake3::hash(data).as_bytes(), zero_fill });
        }
        if zero_fill {
            self.stats.bytes_zero_filled += data.len() as u64;
        } else {
            self.stats.bytes_copied += data.len() as u64;
        }
        Ok(())
    }

    fn source_lost(&self, e: &io::Error, off: u64) -> StopReason {
        let lba = off / self.src.sector_size() as u64;
        let msg = match classify(e) {
            IoErrorClass::AccessDenied => tf!("Accès à la source refusé au LBA {lba} : {e}", "Access to the source denied at LBA {lba}: {e}"),
            _ => tf!(
                "La source ne répond plus (LBA {lba}) : {e}. Vérifiez le câble, l'alimentation ou le boîtier USB.",
                "The source is no longer responding (LBA {lba}): {e}. Check the cable, the power supply or the USB enclosure."
            ),
        };
        self.obs.log(LogLevel::Error, &msg);
        StopReason::SourceLost(msg)
    }

    /// Copies a batch of tasks, in order. Returns `Completed` if all the batch's data
    /// is on the destination (ranges filled with zeros are recorded in
    /// `stats.bad_ranges`).
    pub fn run(&mut self, tasks: &[CopyTask]) -> StopReason {
        let started = Instant::now();
        let r = self.run_inner(tasks);
        self.stats.copy_ms += started.elapsed().as_millis() as u64;
        self.emit(true);
        r
    }

    fn run_inner(&mut self, tasks: &[CopyTask]) -> StopReason {
        let ss = self.src.sector_size() as u64;
        let chunk = self.opts.chunk_size;
        let piece = self.opts.retry_block;
        let mut buf = AlignedBuf::new(chunk);
        let mut deferred: Vec<CopyTask> = Vec::new();
        self.phase = Phase::Copying;

        for t in tasks {
            if t.len % ss != 0 || t.src % ss != 0 || t.dst % ss != 0 {
                return StopReason::Internal(tf!("tâche de copie non alignée ({t:?})", "misaligned copy task ({t:?})"));
            }
            let mut off = 0u64;
            while off < t.len {
                if self.ctl.is_cancelled() {
                    return StopReason::Cancelled;
                }
                let n = (chunk as u64).min(t.len - off) as usize;
                let (s, d) = (t.src + off, t.dst + off);
                self.current_lba = s / ss;
                match self.read_src(s, &mut buf[..n]) {
                    Ok(()) => {
                        if let Err(stop) = self.write_dst(s, d, &buf[..n], false) {
                            return stop;
                        }
                        self.done += n as u64;
                    }
                    Err(e) => {
                        if is_fatal_source(classify(&e)) {
                            return self.source_lost(&e, s);
                        }
                        self.stats.read_error_events += 1;
                        self.obs.log(
                            LogLevel::Warning,
                            &tf!(
                                "Erreur de lecture sur la source vers le LBA {} ({e}) — isolement de la zone",
                                "Read error on the source near LBA {} ({e}) — isolating the area",
                                s / ss
                            ),
                        );
                        // Isolate the faulty area by sub-blocks; the rest is copied right away.
                        let mut p = 0usize;
                        while p < n {
                            if self.ctl.is_cancelled() {
                                return StopReason::Cancelled;
                            }
                            let m = piece.min(n - p);
                            let (ps, pd) = (s + p as u64, d + p as u64);
                            self.current_lba = ps / ss;
                            match self.read_src(ps, &mut buf[p..p + m]) {
                                Ok(()) => {
                                    if let Err(stop) = self.write_dst(ps, pd, &buf[p..p + m], false) {
                                        return stop;
                                    }
                                    self.done += m as u64;
                                }
                                Err(e2) => {
                                    if is_fatal_source(classify(&e2)) {
                                        return self.source_lost(&e2, ps);
                                    }
                                    self.stats.read_error_events += 1;
                                    deferred.push(CopyTask { src: ps, dst: pd, len: m as u64 });
                                }
                            }
                            p += m;
                            self.emit(false);
                        }
                    }
                }
                off += n as u64;
                self.emit(false);
            }
        }

        if deferred.is_empty() {
            return StopReason::Completed;
        }

        // --- Recovery phase: sector by sector -----------------------------------------
        self.phase = Phase::Recovering;
        let deferred_bytes: u64 = deferred.iter().map(|d| d.len).sum();
        self.obs.log(
            LogLevel::Warning,
            &tf!("{} zone(s) difficiles ({} octets) mises de côté ; nouvelle lecture secteur par secteur, {} tentative(s) supplémentaire(s) par secteur", "{} difficult area(s) ({} bytes) set aside; re-reading sector by sector, {} extra attempt(s) per sector",
                deferred.len(),
                deferred_bytes,
                self.opts.read_retries
            ),
        );
        let phys = self.src.physical_sector_size() as u64;
        let unit = if phys > ss && phys.is_multiple_of(ss) && phys <= piece as u64 { phys } else { ss } as usize;
        let first_new_bad = self.stats.bad_ranges.len();
        for d in &deferred {
            let mut p = 0u64;
            while p < d.len {
                if self.ctl.is_cancelled() {
                    return StopReason::Cancelled;
                }
                let m = (unit as u64).min(d.len - p) as usize;
                let (us, ud) = (d.src + p, d.dst + p);
                self.current_lba = us / ss;
                let mut ok_attempt = None;
                let mut last: Option<io::Error> = None;
                for attempt in 1..=self.opts.read_retries + 1 {
                    match self.read_src(us, &mut buf[..m]) {
                        Ok(()) => {
                            ok_attempt = Some(attempt);
                            break;
                        }
                        Err(e) => {
                            if is_fatal_source(classify(&e)) {
                                return self.source_lost(&e, us);
                            }
                            last = Some(e);
                        }
                    }
                    if self.ctl.is_cancelled() {
                        return StopReason::Cancelled;
                    }
                }
                match ok_attempt {
                    Some(attempt) => {
                        if let Err(stop) = self.write_dst(us, ud, &buf[..m], false) {
                            return stop;
                        }
                        self.stats.recovered_bytes += m as u64;
                        match self.stats.recovered.last_mut() {
                            Some(r) if (r.lba + r.sectors) * ss == us && r.attempt == attempt => r.sectors += m as u64 / ss,
                            _ => self.stats.recovered.push(RecoveredRange { lba: us / ss, sectors: m as u64 / ss, attempt }),
                        }
                        self.done += m as u64;
                    }
                    None => {
                        let e = last.expect("failure without an error");
                        let class = classify(&e);
                        let attempts = self.opts.read_retries + 2; // lecture initiale + sous-bloc + reprises
                        let mergeable = self.stats.bad_ranges.len() > first_new_bad;
                        match self.stats.bad_ranges.last_mut() {
                            Some(b) if mergeable && b.src_offset + b.len == us && b.dst_offset + b.len == ud => {
                                b.len += m as u64;
                                b.sectors += m as u64 / ss;
                            }
                            _ => {
                                self.obs.log(
                                    LogLevel::Error,
                                    &tf!(
                                        "Secteur illisible sur la source au LBA {} : {e}",
                                        "Unreadable sector on the source at LBA {}: {e}",
                                        us / ss
                                    ),
                                );
                                self.stats.bad_ranges.push(BadRange {
                                    src_offset: us,
                                    dst_offset: ud,
                                    len: m as u64,
                                    lba: us / ss,
                                    sectors: m as u64 / ss,
                                    attempts,
                                    error: e.to_string(),
                                    class,
                                    outcome: BadOutcome::Pending,
                                });
                            }
                        }
                    }
                }
                p += m as u64;
                self.emit(false);
            }
        }

        let pending: Vec<BadRange> = self.stats.bad_ranges[first_new_bad..].to_vec();
        if pending.is_empty() {
            self.obs
                .log(LogLevel::Info, t!("Toutes les zones difficiles ont finalement pu être lues.", "All difficult areas could finally be read."));
            return StopReason::Completed;
        }
        let total_bytes: u64 = pending.iter().map(|b| b.len).sum();
        let info = UnreadableInfo { ranges: pending, total_bytes, total_sectors: total_bytes / ss, sector_size: ss as u32 };
        let decision = match self.opts.unreadable {
            UnreadablePolicy::Stop => Decision::Abort,
            UnreadablePolicy::ZeroFill => Decision::ZeroFill,
            UnreadablePolicy::Ask => {
                self.emit(true);
                self.obs.decide_unreadable(&info)
            }
        };
        match decision {
            Decision::Abort => {
                for b in &mut self.stats.bad_ranges[first_new_bad..] {
                    b.outcome = BadOutcome::NotCopied;
                }
                self.obs.log(
                    LogLevel::Error,
                    &tf!(
                        "{} secteur(s) illisible(s) : arrêt du clonage, la destination est incomplète.",
                        "{} unreadable sector(s): cloning stopped, the destination is incomplete.",
                        info.total_sectors
                    ),
                );
                StopReason::Unreadable
            }
            Decision::ZeroFill => {
                self.obs.log(
                    LogLevel::Warning,
                    &tf!(
                        "{} secteur(s) illisible(s) remplacé(s) par des zéros sur la destination : les fichiers concernés seront corrompus.",
                        "{} unreadable sector(s) replaced with zeros on the destination: the affected files will be corrupted.",
                        info.total_sectors
                    ),
                );
                buf.fill(0);
                for i in first_new_bad..self.stats.bad_ranges.len() {
                    let (bs, bd, bl) = {
                        let b = &self.stats.bad_ranges[i];
                        (b.src_offset, b.dst_offset, b.len)
                    };
                    let mut p = 0u64;
                    while p < bl {
                        let m = (chunk as u64).min(bl - p) as usize;
                        if let Err(stop) = self.write_dst(bs + p, bd + p, &buf[..m], true) {
                            return stop;
                        }
                        p += m as u64;
                    }
                    self.stats.bad_ranges[i].outcome = BadOutcome::ZeroFilled;
                    self.done += bl;
                }
                StopReason::Completed
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Data verification
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MismatchKind {
    /// The destination does not return what was written to it.
    DestinationDiffers,
    /// The destination could not be read back.
    DestinationUnreadable,
    /// The source returned different content on the second read.
    SourceChanged,
    /// The source could not be read back.
    SourceUnreadable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mismatch {
    pub kind: MismatchKind,
    /// LBA on the destination (or the source for the `Source*` cases).
    pub lba: u64,
    pub sectors: u64,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataVerification {
    pub mode: VerifyMode,
    pub total_bytes: u64,
    pub checked_bytes: u64,
    /// True if all the planned ranges were examined.
    pub completed: bool,
    pub mismatches: Vec<Mismatch>,
    pub duration_ms: u64,
}

impl DataVerification {
    pub fn skipped() -> Self {
        Self { mode: VerifyMode::None, total_bytes: 0, checked_bytes: 0, completed: false, mismatches: vec![], duration_ms: 0 }
    }
    pub fn passed(&self) -> bool {
        self.mode != VerifyMode::None && self.completed && self.mismatches.is_empty()
    }
}

/// Maximum number of detailed differences kept (the total stays exact in the logs).
const MAX_MISMATCHES: usize = 1000;

/// Reads the destination back (and the source in full mode) and compares with the checksums.
pub fn verify_data(
    src: &mut dyn BlockDevice,
    dst: &mut dyn BlockDevice,
    hashes: &[HashEntry],
    mode: VerifyMode,
    ctl: &Control,
    obs: &dyn Observer,
) -> DataVerification {
    let started = Instant::now();
    let total: u64 = hashes.iter().map(|h| h.len).sum();
    let mut out = DataVerification { mode, total_bytes: total, checked_bytes: 0, completed: false, mismatches: vec![], duration_ms: 0 };
    if mode == VerifyMode::None {
        return out;
    }
    let ss = dst.sector_size() as u64;
    let max_len = hashes.iter().map(|h| h.len).max().unwrap_or(4096) as usize;
    let mut dbuf = AlignedBuf::new(max_len.max(4096));
    let mut sbuf = if mode == VerifyMode::Full { Some(AlignedBuf::new(max_len.max(4096))) } else { None };
    let mut meter = Meter::new();
    let mut done = 0u64;
    let mut dropped = 0usize;
    let mut push = |out: &mut DataVerification, m: Mismatch| {
        obs.log(
            LogLevel::Error,
            &tf!("Vérification : {} (LBA {}, {} secteur(s))", "Verification: {} (LBA {}, {} sector(s))", m.detail, m.lba, m.sectors),
        );
        if out.mismatches.len() < MAX_MISMATCHES {
            out.mismatches.push(m);
        } else {
            dropped += 1;
        }
    };

    for h in hashes {
        if ctl.is_cancelled() {
            out.duration_ms = started.elapsed().as_millis() as u64;
            return out;
        }
        let n = h.len as usize;
        let mut dst_ok = true;
        match dst.read_at(h.dst, &mut dbuf[..n]) {
            Ok(()) => {
                if blake3::hash(&dbuf[..n]).as_bytes() != &h.hash {
                    dst_ok = false;
                    push(
                        &mut out,
                        Mismatch {
                            kind: MismatchKind::DestinationDiffers,
                            lba: h.dst / ss,
                            sectors: h.len / ss,
                            detail: t!("la destination ne contient pas les données écrites", "the destination does not contain the written data")
                                .into(),
                        },
                    );
                }
            }
            Err(e) => {
                dst_ok = false;
                push(
                    &mut out,
                    Mismatch {
                        kind: MismatchKind::DestinationUnreadable,
                        lba: h.dst / ss,
                        sectors: h.len / ss,
                        detail: tf!("relecture de la destination impossible ({e})", "cannot read the destination back ({e})"),
                    },
                );
                if matches!(classify(&e), IoErrorClass::DeviceGone | IoErrorClass::AccessDenied) {
                    obs.log(
                        LogLevel::Error,
                        t!(
                            "La destination ne répond plus : vérification interrompue.",
                            "The destination is no longer responding: verification stopped."
                        ),
                    );
                    out.duration_ms = started.elapsed().as_millis() as u64;
                    return out;
                }
            }
        }
        if let (Some(sbuf), false) = (sbuf.as_mut(), h.zero_fill) {
            match src.read_at(h.src, &mut sbuf[..n]) {
                Ok(()) => {
                    if dst_ok && sbuf[..n] != dbuf[..n] {
                        // Locate the first differing sector for a precise report.
                        let first =
                            (0..n).step_by(ss as usize).find(|o| sbuf[*o..*o + ss as usize] != dbuf[*o..*o + ss as usize]).unwrap_or(0) as u64;
                        push(
                            &mut out,
                            Mismatch {
                                kind: MismatchKind::SourceChanged,
                                lba: (h.src + first) / ss,
                                sectors: (h.len - first) / ss,
                                detail: t!(
                                    "la source a renvoyé un contenu différent à la relecture (source instable ou modifiée pendant le clonage)",
                                    "the source returned different content on re-read (source unstable or modified during cloning)"
                                )
                                .into(),
                            },
                        );
                    }
                }
                Err(e) => {
                    push(
                        &mut out,
                        Mismatch {
                            kind: MismatchKind::SourceUnreadable,
                            lba: h.src / ss,
                            sectors: h.len / ss,
                            detail: tf!("relecture de la source impossible ({e})", "cannot read the source back ({e})"),
                        },
                    );
                }
            }
        }
        done += h.len;
        out.checked_bytes = done;
        let now = Instant::now();
        if now.duration_since(meter.last_emit) >= Duration::from_millis(100) {
            let secs = now.duration_since(meter.last_emit).as_secs_f64();
            let inst = (done - meter.last_done) as f64 / secs.max(1e-6);
            meter.ema = if meter.ema == 0.0 { inst } else { 0.7 * meter.ema + 0.3 * inst };
            meter.last_emit = now;
            meter.last_done = done;
            let elapsed = now.duration_since(meter.start);
            let avg = done as f64 / elapsed.as_secs_f64().max(1e-6);
            obs.progress(&Progress {
                phase: Phase::Verifying,
                done,
                total,
                elapsed_ms: elapsed.as_millis() as u64,
                speed_bps: meter.ema as u64,
                avg_bps: avg as u64,
                eta_secs: if avg > 1.0 && elapsed.as_millis() > 1500 { Some(((total - done) as f64 / avg).ceil() as u64) } else { None },
                read_errors: 0,
                bad_bytes: 0,
                recovered_bytes: 0,
                write_errors: 0,
                current_lba: h.dst / ss,
            });
        }
    }
    if dropped > 0 {
        obs.log(
            LogLevel::Error,
            &tf!("{dropped} écart(s) supplémentaire(s) non détaillé(s) dans le rapport.", "{dropped} more mismatch(es) not detailed in the report."),
        );
    }
    out.completed = true;
    out.duration_ms = started.elapsed().as_millis() as u64;
    out
}
