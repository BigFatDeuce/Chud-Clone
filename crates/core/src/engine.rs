//! Moteur de copie et de vérification.
//!
//! Principes, inspirés de ddrescue :
//! 1. Copier d'abord tout ce qui se lit sans difficulté, par gros blocs. Une zone qui
//!    échoue est mise de côté au lieu de bloquer la copie : un disque qui se dégrade
//!    doit livrer un maximum de données saines avant qu'on insiste sur ses défauts.
//! 2. Revenir ensuite sur les zones mises de côté, secteur par secteur, avec un nombre
//!    borné de tentatives.
//! 3. Ne jamais passer sous silence un secteur illisible : chaque plage est
//!    enregistrée (position, tentatives, erreur) et la suite dépend d'une décision
//!    explicite.
//!
//! Une erreur d'écriture sur la destination n'est jamais contournée : la copie
//! s'arrête.

use crate::device::{AlignedBuf, BlockDevice, IoErrorClass, classify};
use crate::plan::CopyTask;
use serde::{Deserialize, Serialize};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnreadablePolicy {
    /// Arrêter : le clone est déclaré incomplet (comportement par défaut).
    Stop,
    /// Demander à l'utilisateur, une fois la récupération tentée.
    Ask,
    /// Remplacer les secteurs illisibles par des zéros et poursuivre.
    ZeroFill,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyMode {
    /// Aucune relecture.
    None,
    /// Relit la destination et la compare aux empreintes des données lues sur la source.
    Hash,
    /// Relit la source ET la destination et les compare.
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
    /// Taille des blocs de copie (octets, multiple de 4096).
    pub chunk_size: usize,
    /// Taille des sous-blocs utilisés pour isoler une zone en erreur.
    pub retry_block: usize,
    /// Nouvelles tentatives par secteur pendant la phase de récupération.
    pub read_retries: u32,
    /// Nouvelles tentatives d'écriture avant abandon.
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
    /// Agrandissement du système de fichiers de la partition principale.
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
    /// Avancement dans la phase courante (octets).
    pub done: u64,
    pub total: u64,
    pub elapsed_ms: u64,
    /// Débit instantané lissé (octets/s).
    pub speed_bps: u64,
    /// Débit moyen depuis le début de la phase.
    pub avg_bps: u64,
    pub eta_secs: Option<u64>,
    /// Événements d'erreur de lecture rencontrés.
    pub read_errors: u64,
    /// Octets actuellement tenus pour illisibles.
    pub bad_bytes: u64,
    /// Octets récupérés après nouvelle tentative.
    pub recovered_bytes: u64,
    pub write_errors: u64,
    /// Position courante sur la source (LBA).
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
    /// Décision en attente.
    Pending,
    /// Non copié : la destination ne contient pas ces données.
    NotCopied,
    /// Remplacé par des zéros sur la destination.
    ZeroFilled,
}

/// Plage définitivement illisible sur la source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BadRange {
    pub src_offset: u64,
    pub dst_offset: u64,
    pub len: u64,
    /// LBA de début sur la source.
    pub lba: u64,
    pub sectors: u64,
    /// Tentatives de lecture effectuées par secteur.
    pub attempts: u32,
    pub error: String,
    pub class: IoErrorClass,
    pub outcome: BadOutcome,
}

/// Plage lue avec succès après au moins un échec.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveredRange {
    pub lba: u64,
    pub sectors: u64,
    /// Tentative qui a réussi (1 = première relecture en phase de récupération).
    pub attempt: u32,
}

#[derive(Debug, Clone)]
pub struct HashEntry {
    pub src: u64,
    pub dst: u64,
    pub len: u64,
    pub hash: [u8; 32],
    /// Plage remplie de zéros (aucune donnée source correspondante).
    pub zero_fill: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum StopReason {
    Completed,
    Cancelled,
    /// La source a disparu ou refuse l'accès.
    SourceLost(String),
    /// Erreur d'écriture sur la destination.
    DestinationFailed(String),
    /// Secteurs illisibles et décision d'arrêt.
    Unreadable,
    Internal(String),
}

/// Informations fournies au moment de décider du sort des secteurs illisibles.
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
    /// Appelé une seule fois par passe, après la phase de récupération, si des
    /// secteurs restent illisibles et que la politique est `Ask`.
    fn decide_unreadable(&self, _info: &UnreadableInfo) -> Decision {
        Decision::Abort
    }
}

/// Observateur muet.
pub struct NullObserver;
impl Observer for NullObserver {}

/// Commande d'annulation partagée entre l'interface et le moteur.
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
    /// Octets lus sur la source et écrits sur la destination.
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

/// Exécute les copies. Un même `Copier` peut enchaîner plusieurs lots de tâches ;
/// les statistiques s'accumulent.
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
        // L'estimation s'appuie sur la moyenne, plus stable, tempérée par le débit récent.
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

    /// Lecture sur la source. Une disparition du périphérique est retentée brièvement :
    /// un pont USB qui se réinitialise revient souvent en une seconde.
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

    /// Copie un lot de tâches, dans l'ordre. Renvoie `Completed` si toutes les données
    /// du lot se trouvent sur la destination (les plages remplies de zéros sont
    /// consignées dans `stats.bad_ranges`).
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
                        // Isole la zone fautive par sous-blocs ; le reste est copié tout de suite.
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

        // --- Phase de récupération : secteur par secteur ------------------------
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
                        let e = last.expect("échec sans erreur");
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
// Vérification des données
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MismatchKind {
    /// La destination ne restitue pas ce qui y a été écrit.
    DestinationDiffers,
    /// La destination n'a pas pu être relue.
    DestinationUnreadable,
    /// La source a renvoyé un contenu différent à la seconde lecture.
    SourceChanged,
    /// La source n'a pas pu être relue.
    SourceUnreadable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mismatch {
    pub kind: MismatchKind,
    /// LBA sur la destination (ou la source pour les cas `Source*`).
    pub lba: u64,
    pub sectors: u64,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataVerification {
    pub mode: VerifyMode,
    pub total_bytes: u64,
    pub checked_bytes: u64,
    /// Vrai si toutes les plages prévues ont été examinées.
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

/// Nombre maximal d'écarts détaillés conservés (le total reste exact dans les journaux).
const MAX_MISMATCHES: usize = 1000;

/// Relit la destination (et la source en mode complet) et compare aux empreintes.
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
                        // Localise le premier secteur différent pour un rapport précis.
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
