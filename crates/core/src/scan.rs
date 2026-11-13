//! Scan de surface en LECTURE SEULE.
//!
//! Lit tout le disque bloc par bloc et mesure le temps de chaque lecture. Un disque dur qui
//! peine à relire une zone met beaucoup plus longtemps à répondre (relectures internes) bien
//! avant que la zone ne devienne illisible : les blocs lents sont donc un signe précoce. Les
//! blocs en erreur sont ensuite cernés par sous-blocs de 64 Kio puis secteur par secteur, pour
//! donner la position exacte (LBA) de chaque secteur illisible.
//!
//! Rien n'est jamais écrit : « réparer » un secteur reviendrait à le réécrire (ce qui force le
//! disque à le réallouer) et détruirait son contenu. Ce module ne le fait pas.
//!
//! Sur un disque qui meurt, chaque lecture ratée peut prendre plusieurs secondes et l'user
//! davantage : le scan s'arrête de lui-même après un nombre limité de blocs en erreur.

use crate::device::{AlignedBuf, BlockDevice, IoErrorClass, classify};
use crate::diag::Finding;
use crate::engine::Control;
use serde::{Deserialize, Serialize};
use std::time::Instant;

/// Libellés des classes de temps de réponse (index = classe).
pub fn class_labels() -> [&'static str; 7] {
    if crate::i18n::is_fr() {
        ["< 5 ms", "5 à 20 ms", "20 à 50 ms", "50 à 150 ms", "150 à 500 ms", "≥ 500 ms", "Illisible"]
    } else {
        ["< 5 ms", "5 to 20 ms", "20 to 50 ms", "50 to 150 ms", "150 to 500 ms", "≥ 500 ms", "Unreadable"]
    }
}
/// Classe des blocs en erreur.
pub const CLASS_ERROR: u8 = 6;
/// Valeur d'une cellule de la carte pas encore lue.
pub const CELL_UNREAD: u8 = 255;
const LIMITS_MS: [f64; 5] = [5.0, 20.0, 50.0, 150.0, 500.0];
const PROBE: usize = 64 * 1024;

pub fn class_of(ms: f64) -> u8 {
    LIMITS_MS.iter().position(|l| ms < *l).unwrap_or(5) as u8
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanOptions {
    /// Taille des lectures (octets) ; arrondie à un multiple du secteur.
    pub block_size: u32,
    /// Nombre de cellules de la carte affichée.
    pub cells: u32,
    /// Nouvelles tentatives par secteur en erreur.
    pub sector_retries: u32,
    /// Arrêt après ce nombre de blocs en erreur (protection d'un disque mourant).
    pub max_bad_blocks: u32,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self { block_size: 1 << 20, cells: 2400, sector_retries: 1, max_bad_blocks: 256 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScanBad {
    pub lba: u64,
    pub sectors: u64,
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlowBlock {
    pub lba: u64,
    pub ms: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum ScanEnd {
    Completed,
    Cancelled,
    /// Le disque a cessé de répondre.
    DeviceLost(String),
    /// Trop de blocs en erreur : arrêt pour ne pas user davantage le disque.
    TooManyErrors,
    /// Lecture refusée (droits, verrou…).
    Denied(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanProgress {
    pub done: u64,
    pub total: u64,
    pub speed_bps: u64,
    pub eta_secs: Option<u64>,
    pub counts: [u64; 7],
    pub bad_sectors: u64,
    pub current_lba: u64,
    /// Cellules modifiées depuis le dernier envoi : (index, classe).
    pub cells: Vec<(u32, u8)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanResult {
    pub size: u64,
    pub sector_size: u32,
    pub block_size: u32,
    pub scanned_bytes: u64,
    /// Pire classe relevée dans chaque cellule (`CELL_UNREAD` si non lue).
    pub cells: Vec<u8>,
    /// Nombre de blocs par classe.
    pub counts: [u64; 7],
    pub bad: Vec<ScanBad>,
    pub bad_sectors: u64,
    /// Blocs dont la lecture a échoué une fois mais qui se sont relus par morceaux.
    pub recovered_blocks: u64,
    /// Blocs les plus lents (≥ 150 ms), du plus lent au moins lent, 100 au plus.
    pub slowest: Vec<SlowBlock>,
    pub duration_ms: u64,
    pub avg_bps: u64,
    pub end: ScanEnd,
    pub findings: Vec<Finding>,
}

pub trait ScanObserver: Sync {
    fn progress(&self, p: &ScanProgress);
}

pub struct NullScanObserver;
impl ScanObserver for NullScanObserver {
    fn progress(&self, _: &ScanProgress) {}
}

struct BadList {
    list: Vec<ScanBad>,
    total: u64,
}

impl BadList {
    fn add(&mut self, lba: u64, error: String) {
        self.total += 1;
        if let Some(last) = self.list.last_mut()
            && last.lba + last.sectors == lba
            && last.error == error
        {
            last.sectors += 1;
            return;
        }
        self.list.push(ScanBad { lba, sectors: 1, error });
    }
}

enum Probe {
    Done,
    Lost(String),
}

/// Cerne les secteurs illisibles d'un bloc en erreur : sous-blocs de 64 Kio, puis secteurs.
#[allow(clippy::too_many_arguments)]
fn probe_block(
    dev: &mut dyn BlockDevice,
    off: u64,
    len: usize,
    sector: usize,
    retries: u32,
    buf: &mut AlignedBuf,
    bad: &mut BadList,
    ctl: &Control,
) -> Probe {
    let mut sub = 0;
    while sub < len {
        if ctl.is_cancelled() {
            return Probe::Done;
        }
        let n = PROBE.min(len - sub);
        let at = off + sub as u64;
        if let Err(e) = dev.read_at(at, &mut buf[..n]) {
            if classify(&e) == IoErrorClass::DeviceGone {
                return Probe::Lost(e.to_string());
            }
            let mut s = 0;
            while s < n {
                let pos = at + s as u64;
                let mut last = None;
                for _ in 0..=retries {
                    match dev.read_at(pos, &mut buf[..sector]) {
                        Ok(()) => {
                            last = None;
                            break;
                        }
                        Err(e) => {
                            if classify(&e) == IoErrorClass::DeviceGone {
                                return Probe::Lost(e.to_string());
                            }
                            last = Some(e.to_string());
                        }
                    }
                }
                if let Some(err) = last {
                    bad.add(pos / sector as u64, err);
                }
                s += sector;
            }
        }
        sub += n;
    }
    Probe::Done
}

/// Lit toute la surface du périphérique (lecture seule) et mesure les temps de réponse.
pub fn scan(dev: &mut dyn BlockDevice, opts: &ScanOptions, ctl: &Control, obs: &dyn ScanObserver) -> ScanResult {
    let size = dev.size();
    let sector = dev.sector_size().max(512) as usize;
    let block = ((opts.block_size as usize).max(sector) / sector) * sector;
    let n_blocks = size.div_ceil(block as u64).max(1);
    let n_cells = (opts.cells as u64).clamp(1, n_blocks) as usize;
    let mut cells = vec![CELL_UNREAD; n_cells];
    let mut dirty: Vec<u32> = Vec::new();
    let mut counts = [0u64; 7];
    let mut bad = BadList { list: vec![], total: 0 };
    let mut slow: Vec<SlowBlock> = vec![];
    let mut bad_blocks = 0u32;
    let mut recovered_blocks = 0u64;
    let mut buf = AlignedBuf::new(block);
    let start = Instant::now();
    let mut last_emit = Instant::now();
    let mut done = 0u64;
    let mut end = ScanEnd::Completed;

    let mark = |cells: &mut Vec<u8>, dirty: &mut Vec<u32>, idx: u64, class: u8| {
        let c = ((idx * n_cells as u64) / n_blocks) as usize;
        if cells[c] == CELL_UNREAD || cells[c] < class {
            cells[c] = class;
            dirty.push(c as u32);
        }
    };

    for idx in 0..n_blocks {
        if ctl.is_cancelled() {
            end = ScanEnd::Cancelled;
            break;
        }
        let off = idx * block as u64;
        let len = (block as u64).min(size - off) as usize;
        let t = Instant::now();
        let r = dev.read_at(off, &mut buf[..len]);
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        match r {
            Ok(()) => {
                let c = class_of(ms);
                counts[c as usize] += 1;
                mark(&mut cells, &mut dirty, idx, c);
                if c >= 4 {
                    slow.push(SlowBlock { lba: off / sector as u64, ms: ms.min(u32::MAX as f64) as u32 });
                }
            }
            Err(e) => match classify(&e) {
                IoErrorClass::DeviceGone => {
                    end = ScanEnd::DeviceLost(e.to_string());
                    break;
                }
                IoErrorClass::AccessDenied => {
                    end = ScanEnd::Denied(e.to_string());
                    break;
                }
                _ => {
                    let before = bad.total;
                    if let Probe::Lost(msg) = probe_block(dev, off, len, sector, opts.sector_retries, &mut buf, &mut bad, ctl) {
                        end = ScanEnd::DeviceLost(msg);
                        break;
                    }
                    if bad.total == before {
                        // Le bloc a échoué d'un coup mais chaque morceau se relit : zone difficile,
                        // pas illisible. Classée parmi les plus lentes.
                        recovered_blocks += 1;
                        counts[5] += 1;
                        mark(&mut cells, &mut dirty, idx, 5);
                        slow.push(SlowBlock { lba: off / sector as u64, ms: u32::MAX });
                    } else {
                        counts[CLASS_ERROR as usize] += 1;
                        mark(&mut cells, &mut dirty, idx, CLASS_ERROR);
                        bad_blocks += 1;
                        if bad_blocks >= opts.max_bad_blocks {
                            end = ScanEnd::TooManyErrors;
                            done += len as u64;
                            break;
                        }
                    }
                }
            },
        }
        done += len as u64;
        if last_emit.elapsed().as_millis() >= 200 || idx + 1 == n_blocks {
            last_emit = Instant::now();
            let secs = start.elapsed().as_secs_f64().max(1e-3);
            let speed = (done as f64 / secs) as u64;
            obs.progress(&ScanProgress {
                done,
                total: size,
                speed_bps: speed,
                eta_secs: (speed > 0).then(|| (size - done) / speed),
                counts,
                bad_sectors: bad.total,
                current_lba: (off + len as u64) / sector as u64,
                cells: dirty.drain(..).map(|c| (c, cells[c as usize])).collect(),
            });
        }
    }
    slow.sort_by_key(|s| std::cmp::Reverse(s.ms));
    slow.truncate(100);
    let duration_ms = start.elapsed().as_millis() as u64;
    let avg_bps = (done * 1000).checked_div(duration_ms).unwrap_or(0);
    let mut r = ScanResult {
        size,
        sector_size: sector as u32,
        block_size: block as u32,
        scanned_bytes: done,
        cells,
        counts,
        bad: bad.list,
        bad_sectors: bad.total,
        recovered_blocks,
        slowest: slow,
        duration_ms,
        avg_bps,
        end,
        findings: vec![],
    };
    r.findings = conclude(&r);
    r
}

fn conclude(r: &ScanResult) -> Vec<Finding> {
    let mut f = Vec::new();
    let pct = if r.size > 0 { r.scanned_bytes as f64 * 100.0 / r.size as f64 } else { 0.0 };
    match &r.end {
        ScanEnd::Completed => {}
        ScanEnd::Cancelled => f.push(Finding::info("SCAN_CANCELLED", tf!("Scan interrompu à {pct:.1} %", "Scan stopped at {pct:.1} %"), t!("Les résultats ne portent que sur la partie lue.", "The results only cover the part that was read."))),
        ScanEnd::DeviceLost(e) => f.push(Finding::critical("SCAN_DEVICE_LOST", t!("Le disque a cessé de répondre pendant le scan", "The disk stopped responding during the scan"), tf!("{e}. Un disque qui se déconnecte sous une simple lecture est en très mauvais état, ou sa connexion (câble, boîtier USB) est défaillante.", "{e}. A disk that disconnects under a simple read is in very poor condition, or its connection (cable, USB enclosure) is failing."))),
        ScanEnd::TooManyErrors => f.push(Finding::critical(
            "SCAN_TOO_MANY_ERRORS",
            t!("Scan arrêté : trop de zones illisibles", "Scan stopped: too many unreadable areas"),
            t!("Le disque accumule les zones illisibles. Le scan s'est arrêté pour ne pas l'user davantage : si ses données comptent, clonez-le maintenant (Chud Clone copie d'abord tout ce qui se lit facilement).", "The disk keeps piling up unreadable areas. The scan stopped so as not to wear it further: if its data matters, clone it now (Chud Clone copies everything that reads easily first)."),
        )),
        ScanEnd::Denied(e) => f.push(Finding::error("SCAN_DENIED", t!("Lecture du disque refusée", "Disk read denied"), e.clone())),
    }
    if r.bad_sectors > 0 {
        f.push(Finding::error(
            "SCAN_BAD_SECTORS",
            tf!("{} secteur(s) illisible(s)", "{} unreadable sector(s)", r.bad_sectors),
            t!("Ces secteurs ne se relisent pas, même après nouvelles tentatives. Les fichiers qui les utilisent sont endommagés. Le disque les remplacera par des secteurs de réserve s'ils sont un jour réécrits, mais leur contenu actuel est perdu. Sauvegardez ou clonez ce disque.", "These sectors cannot be read back, even after retries. The files using them are damaged. The disk will swap them for spare sectors if they are ever rewritten, but their current content is lost. Back up or clone this disk."),
        ));
    }
    if r.recovered_blocks > 0 {
        f.push(Finding::warning(
            "SCAN_READ_RETRIED",
            tf!("{} zone(s) relue(s) seulement après une erreur", "{} area(s) read only after an error", r.recovered_blocks),
            t!("La première lecture a échoué, la suivante a réussi. Le disque peine à relire ces zones : c'est un signe de fragilité, ou d'une connexion instable (câble, boîtier USB).", "The first read failed, the next one succeeded. The disk struggles to read these areas back: a sign of weakness, or of an unstable connection (cable, USB enclosure)."),
        ));
    }
    let very_slow = r.counts[4] + r.counts[5] - r.recovered_blocks;
    if very_slow > 0 {
        f.push(Finding::warning(
            "SCAN_SLOW",
            tf!("{very_slow} zone(s) très lente(s) (≥ 150 ms)", "{very_slow} very slow area(s) (≥ 150 ms)"),
            t!("Le disque a dû insister pour relire ces zones. Sur un disque dur, c'est souvent le signe précurseur de secteurs qui deviendront illisibles. Si cela se répète au même endroit, sauvegardez.", "The disk had to insist to read these areas back. On a hard disk this often heralds sectors that will become unreadable. If it keeps happening at the same place, back up."),
        ));
    }
    if r.end == ScanEnd::Completed && r.bad_sectors == 0 && very_slow == 0 {
        f.push(Finding::info(
            "SCAN_CLEAN",
            t!("Toute la surface a été relue sans erreur ni lenteur notable", "The whole surface was read back with no errors or notable slowness"),
            t!(
                "Ce résultat décrit l'état actuel ; il ne garantit pas l'absence de panne future.",
                "This result describes the current state; it does not rule out a future failure."
            ),
        ));
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::MemDevice;
    use crate::testing::{FaultPlan, FaultyDevice};
    use std::sync::Mutex;

    struct Count(Mutex<(u32, u64)>);
    impl ScanObserver for Count {
        fn progress(&self, p: &ScanProgress) {
            let mut g = self.0.lock().unwrap();
            g.0 += 1;
            g.1 = p.done;
        }
    }

    const MIB: u64 = 1 << 20;

    #[test]
    fn classes() {
        assert_eq!(class_of(0.2), 0);
        assert_eq!(class_of(7.0), 1);
        assert_eq!(class_of(149.9), 3);
        assert_eq!(class_of(800.0), 5);
    }

    #[test]
    fn clean_device_is_fully_read_and_never_written() {
        let mut dev = FaultyDevice::new(MemDevice::new(16 * MIB, 512), FaultPlan::default());
        let obs = Count(Mutex::new((0, 0)));
        let r = scan(&mut dev, &ScanOptions { cells: 8, ..Default::default() }, &Control::new(), &obs);
        assert_eq!(r.end, ScanEnd::Completed);
        assert_eq!(r.scanned_bytes, 16 * MIB);
        assert_eq!(r.counts.iter().sum::<u64>(), 16);
        assert!(r.cells.iter().all(|c| *c != CELL_UNREAD && *c < CLASS_ERROR));
        assert_eq!(dev.write_calls, 0, "un scan n'écrit jamais");
        assert!(r.findings.iter().any(|f| f.code == "SCAN_CLEAN"));
        assert_eq!(obs.0.lock().unwrap().1, 16 * MIB);
    }

    #[test]
    fn bad_sectors_are_located_exactly() {
        // Secteurs 10 000 à 10 002 et 20 480 illisibles.
        let plan = FaultPlan { bad_read: vec![(10_000 * 512, 3 * 512), (20_480 * 512, 512)], ..Default::default() };
        let mut dev = FaultyDevice::new(MemDevice::new(16 * MIB, 512), plan);
        let r = scan(&mut dev, &ScanOptions::default(), &Control::new(), &NullScanObserver);
        assert_eq!(r.end, ScanEnd::Completed);
        assert_eq!(r.bad_sectors, 4);
        assert_eq!(r.bad.iter().map(|b| (b.lba, b.sectors)).collect::<Vec<_>>(), vec![(10_000, 3), (20_480, 1)]);
        assert_eq!(r.counts[CLASS_ERROR as usize], 2);
        assert!(r.findings.iter().any(|f| f.code == "SCAN_BAD_SECTORS"));
        assert_eq!(dev.write_calls, 0);
    }

    #[test]
    fn transient_failure_is_not_counted_as_bad_sector() {
        let plan = FaultPlan { flaky_read: vec![(3 * MIB, 512, 1)], ..Default::default() };
        let mut dev = FaultyDevice::new(MemDevice::new(8 * MIB, 512), plan);
        let r = scan(&mut dev, &ScanOptions::default(), &Control::new(), &NullScanObserver);
        assert_eq!(r.bad_sectors, 0);
        assert!(r.bad.is_empty());
        assert_eq!(r.recovered_blocks, 1);
        assert_eq!(r.counts[CLASS_ERROR as usize], 0);
        assert!(r.findings.iter().any(|f| f.code == "SCAN_READ_RETRIED"));
        assert!(!r.findings.iter().any(|f| f.code == "SCAN_SLOW"));
    }

    #[test]
    fn slow_zone_is_reported() {
        let plan = FaultPlan { slow_read: vec![(5 * MIB, 1, 170)], ..Default::default() };
        let mut dev = FaultyDevice::new(MemDevice::new(8 * MIB, 512), plan);
        let r = scan(&mut dev, &ScanOptions::default(), &Control::new(), &NullScanObserver);
        assert_eq!(r.counts[4], 1, "{:?}", r.counts);
        assert_eq!(r.slowest[0].lba, 5 * MIB / 512);
        assert!(r.findings.iter().any(|f| f.code == "SCAN_SLOW"));
    }

    #[test]
    fn stops_when_device_vanishes_or_too_many_errors() {
        let plan = FaultPlan { vanish_after_read: Some(3 * MIB), ..Default::default() };
        let mut dev = FaultyDevice::new(MemDevice::new(8 * MIB, 512), plan);
        let r = scan(&mut dev, &ScanOptions::default(), &Control::new(), &NullScanObserver);
        assert!(matches!(r.end, ScanEnd::DeviceLost(_)));
        assert_eq!(r.scanned_bytes, 3 * MIB);

        let plan = FaultPlan { bad_read: vec![(0, 8 * MIB)], ..Default::default() };
        let mut dev = FaultyDevice::new(MemDevice::new(8 * MIB, 512), plan);
        let r = scan(&mut dev, &ScanOptions { max_bad_blocks: 2, sector_retries: 0, ..Default::default() }, &Control::new(), &NullScanObserver);
        assert_eq!(r.end, ScanEnd::TooManyErrors);
        assert_eq!(r.bad_sectors, 2 * 2048);
        assert_eq!(r.bad.len(), 1, "plage fusionnée");
    }

    #[test]
    fn cancellation_stops_cleanly() {
        static CTL: std::sync::OnceLock<Control> = std::sync::OnceLock::new();
        let ctl = CTL.get_or_init(Control::new);
        let plan = FaultPlan { cancel_after_read: Some((2 * MIB, ctl)), ..Default::default() };
        let mut dev = FaultyDevice::new(MemDevice::new(8 * MIB, 512), plan);
        let r = scan(&mut dev, &ScanOptions::default(), ctl, &NullScanObserver);
        assert_eq!(r.end, ScanEnd::Cancelled);
        assert_eq!(r.scanned_bytes, 2 * MIB);
        assert!(r.cells.contains(&CELL_UNREAD));
    }
}
