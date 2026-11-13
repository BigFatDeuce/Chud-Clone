//! Disk health history: for each disk, a series of dated SMART readings is kept,
//! and what GETS WORSE from one reading to the next is flagged.
//!
//! A high but stable counter (e.g. 8 sectors reallocated years ago) does not mean the same
//! as a counter that climbs: the trend is what most reliably heralds a failure. The
//! conclusions remain indicators, and are presented as such.

use crate::diag::Finding;
use crate::model::DiskInfo;
use crate::smart::{HealthLevel, SmartReport};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Maximum number of readings kept per disk.
const MAX_SNAPSHOTS: usize = 400;
/// Minimum gap between two identical saved readings (seconds).
const MIN_INTERVAL_SECS: i64 = 6 * 3600;

/// Dated health reading. Counters absent on a type of disk stay `None`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HealthSnapshot {
    /// Horodatage Unix (secondes).
    pub at: i64,
    pub level: HealthLevel,
    pub temperature_c: Option<i32>,
    pub power_on_hours: Option<u64>,
    pub wear_percent_used: Option<u8>,
    /// ATA 5 — reallocated sectors.
    pub reallocated: Option<u64>,
    /// ATA 197 — sectors pending reallocation.
    pub pending: Option<u64>,
    /// ATA 198 — secteurs non corrigibles hors ligne.
    pub offline_uncorrectable: Option<u64>,
    /// ATA 187 — reported uncorrectable errors.
    pub reported_uncorrectable: Option<u64>,
    /// ATA 199 — link CRC errors (cable, connector, USB enclosure).
    pub crc_errors: Option<u64>,
    /// NVMe — media / data integrity errors.
    pub media_errors: Option<u64>,
    /// NVMe — available spare (%).
    pub available_spare: Option<u8>,
}

impl HealthSnapshot {
    pub fn from_report(r: &SmartReport, at: i64) -> Option<HealthSnapshot> {
        if !r.available {
            return None;
        }
        let raw = |id: u8| r.attributes.iter().find(|a| a.id == id).map(|a| a.raw & 0xFFFF_FFFF);
        Some(HealthSnapshot {
            at,
            level: r.level,
            temperature_c: r.temperature_c,
            power_on_hours: r.power_on_hours,
            wear_percent_used: r.wear_percent_used,
            reallocated: raw(5),
            pending: raw(197),
            offline_uncorrectable: raw(198),
            reported_uncorrectable: raw(187),
            crc_errors: raw(199),
            media_errors: r.nvme.as_ref().map(|n| n.media_errors),
            available_spare: r.nvme.as_ref().map(|n| n.available_spare),
        })
    }

    /// Same counters (except temperature and hours, which change all the time).
    fn same_counters(&self, o: &HealthSnapshot) -> bool {
        self.level == o.level
            && self.wear_percent_used == o.wear_percent_used
            && self.reallocated == o.reallocated
            && self.pending == o.pending
            && self.offline_uncorrectable == o.offline_uncorrectable
            && self.reported_uncorrectable == o.reported_uncorrectable
            && self.crc_errors == o.crc_errors
            && self.media_errors == o.media_errors
            && self.available_spare == o.available_spare
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HealthHistory {
    pub model: String,
    pub serial: String,
    pub size: u64,
    pub snapshots: Vec<HealthSnapshot>,
}

/// File key of a disk: its identity (model, serial number, size), not its Windows
/// number, which changes with the plugging order.
pub fn disk_key(d: &DiskInfo) -> Option<String> {
    if d.serial.trim().is_empty() {
        return None; // without a serial number, two identical disks would be confused
    }
    let text = format!("{}|{}|{}", d.model.trim(), d.serial.trim(), d.size);
    Some(blake3::hash(text.as_bytes()).to_hex()[..20].to_string())
}

fn history_path(dir: &Path, key: &str) -> PathBuf {
    dir.join(format!("{key}.json"))
}

pub fn load(dir: &Path, d: &DiskInfo) -> HealthHistory {
    let Some(key) = disk_key(d) else {
        return HealthHistory::default();
    };
    std::fs::read(history_path(dir, &key)).ok().and_then(|b| serde_json::from_slice::<HealthHistory>(&b).ok()).unwrap_or_else(|| HealthHistory {
        model: d.model.clone(),
        serial: d.serial.clone(),
        size: d.size,
        snapshots: vec![],
    })
}

/// Adds a reading to the disk's history (unless a recent duplicate) and saves it.
/// Returns the updated history. A write failure does not prevent the analysis.
pub fn record(dir: &Path, d: &DiskInfo, report: &SmartReport, now: i64) -> HealthHistory {
    let mut h = load(dir, d);
    let Some(snap) = HealthSnapshot::from_report(report, now) else {
        return h;
    };
    let keep = match h.snapshots.last() {
        Some(last) => !last.same_counters(&snap) || now - last.at >= MIN_INTERVAL_SECS,
        None => true,
    };
    if keep {
        h.snapshots.push(snap);
        if h.snapshots.len() > MAX_SNAPSHOTS {
            let excess = h.snapshots.len() - MAX_SNAPSHOTS;
            h.snapshots.drain(0..excess);
        }
        if let Some(key) = disk_key(d)
            && std::fs::create_dir_all(dir).is_ok()
            && let Ok(json) = serde_json::to_vec_pretty(&h)
        {
            let path = history_path(dir, &key);
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }
    h
}

/// Horodatage Unix courant (secondes).
pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn when(at: i64) -> String {
    chrono::DateTime::from_timestamp(at, 0)
        .map(|t| t.with_timezone(&chrono::Local).format(if crate::i18n::is_fr() { "%d/%m/%Y" } else { "%Y-%m-%d" }).to_string())
        .unwrap_or_else(|| "?".into())
}

/// Compares the latest reading with the first and the previous: flags every counter that increases.
pub fn trends(h: &HealthHistory) -> Vec<Finding> {
    let mut out = Vec::new();
    let (Some(first), Some(last)) = (h.snapshots.first(), h.snapshots.last()) else {
        return out;
    };
    if h.snapshots.len() < 2 {
        return out;
    }
    let since = tf!("depuis le {}", "since {}", when(first.at));
    let grew = |f: fn(&HealthSnapshot) -> Option<u64>| -> Option<(u64, u64)> {
        let a = h.snapshots.iter().find_map(f)?;
        let b = f(last)?;
        (b > a).then_some((a, b))
    };
    if let Some((a, b)) = grew(|s| s.pending) {
        out.push(Finding::error(
            "TREND_PENDING",
            tf!("Secteurs en attente de réallocation : {a} → {b} {since}", "Sectors pending reallocation: {a} → {b} {since}"),
            t!("Le disque rencontre de nouveaux secteurs qu'il n'arrive plus à lire. C'est le signe le plus net d'une surface qui se dégrade : copiez vos données ou clonez ce disque sans attendre.", "The disk keeps finding new sectors it can no longer read. This is the clearest sign of a degrading surface: copy your data or clone this disk without delay."),
        ));
    }
    if let Some((a, b)) = grew(|s| s.offline_uncorrectable) {
        out.push(Finding::error(
            "TREND_UNCORRECTABLE",
            tf!("Secteurs non corrigibles : {a} → {b} {since}", "Uncorrectable sectors: {a} → {b} {since}"),
            t!(
                "Des secteurs illisibles apparaissent lors des tests internes du disque. Sauvegardez rapidement.",
                "Unreadable sectors are showing up in the disk's internal tests. Back up soon."
            ),
        ));
    }
    if let Some((a, b)) = grew(|s| s.reported_uncorrectable) {
        out.push(Finding::error(
            "TREND_REPORTED_UNCORRECTABLE",
            tf!("Erreurs de lecture non corrigées : {a} → {b} {since}", "Uncorrected read errors: {a} → {b} {since}"),
            t!(
                "Le disque a renvoyé des données qu'il n'a pas pu corriger. Certaines lectures ont échoué ou échoueront.",
                "The disk returned data it could not correct. Some reads failed or will fail."
            ),
        ));
    }
    if let Some((a, b)) = grew(|s| s.media_errors) {
        out.push(Finding::error(
            "TREND_MEDIA_ERRORS",
            tf!("Erreurs de support NVMe : {a} → {b} {since}", "NVMe media errors: {a} → {b} {since}"),
            t!(
                "Le SSD signale de nouvelles erreurs d'intégrité des données. Sauvegardez rapidement.",
                "The SSD reports new data-integrity errors. Back up soon."
            ),
        ));
    }
    if let Some((a, b)) = grew(|s| s.reallocated) {
        out.push(Finding::warning(
            "TREND_REALLOCATED",
            tf!("Secteurs réalloués : {a} → {b} {since}", "Reallocated sectors: {a} → {b} {since}"),
            t!("Le disque a remplacé de nouveaux secteurs défectueux par des secteurs de réserve. Une hausse régulière annonce souvent une panne : surveillez et sauvegardez.", "The disk replaced new bad sectors with spare ones. A steady increase often precedes a failure: keep an eye on it and back up."),
        ));
    }
    if let Some((a, b)) = grew(|s| s.crc_errors) {
        out.push(Finding::warning(
            "TREND_CRC",
            tf!("Erreurs de transmission : {a} → {b} {since}", "Transfer errors: {a} → {b} {since}"),
            t!(
                "Ces erreurs viennent en général du câble, du connecteur ou du boîtier USB, pas du disque lui-même. Changez de câble ou de port.",
                "These errors usually come from the cable, connector or USB enclosure, not the disk itself. Try another cable or port."
            ),
        ));
    }
    let spare = |s: &HealthSnapshot| s.available_spare.map(u64::from);
    if let (Some(a), Some(b)) = (h.snapshots.iter().find_map(spare), spare(last))
        && b < a
    {
        out.push(Finding::warning(
            "TREND_SPARE",
            tf!("Réserve du SSD : {a} % → {b} % {since}", "SSD spare: {a} % → {b} % {since}"),
            t!(
                "Le SSD a consommé une partie de ses blocs de réserve pour remplacer des blocs usés.",
                "The SSD has used part of its spare blocks to replace worn ones."
            ),
        ));
    }
    let wear = |s: &HealthSnapshot| s.wear_percent_used.map(u64::from);
    if let (Some(a), Some(b)) = (h.snapshots.iter().find_map(wear), wear(last))
        && b >= a + 10
    {
        out.push(Finding::info(
            "TREND_WEAR",
            tf!("Usure estimée : {a} % → {b} % {since}", "Estimated wear: {a} % → {b} % {since}"),
            t!(
                "L'usure progresse normalement avec les écritures ; elle n'est préoccupante qu'à l'approche de 100 %.",
                "Wear grows normally with writes; it only becomes a concern close to 100 %."
            ),
        ));
    }
    if let Some(max) = h.snapshots.iter().filter_map(|s| s.temperature_c).max()
        && max >= 60
    {
        out.push(Finding::warning(
            "TREND_HOT",
            tf!("Température maximale relevée : {max} °C", "Highest temperature recorded: {max} °C"),
            t!(
                "Une chaleur prolongée use les disques plus vite. Vérifiez la ventilation du boîtier ou du disque externe.",
                "Prolonged heat wears disks faster. Check the airflow of the case or external enclosure."
            ),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diag::Severity;

    fn snap(at: i64, realloc: u64, pending: u64, temp: i32) -> HealthSnapshot {
        HealthSnapshot {
            at,
            level: HealthLevel::Good,
            temperature_c: Some(temp),
            power_on_hours: Some(1000),
            wear_percent_used: None,
            reallocated: Some(realloc),
            pending: Some(pending),
            offline_uncorrectable: Some(0),
            reported_uncorrectable: None,
            crc_errors: Some(0),
            media_errors: None,
            available_spare: None,
        }
    }

    #[test]
    fn stable_counters_raise_nothing() {
        let h = HealthHistory { snapshots: vec![snap(0, 8, 0, 35), snap(86400, 8, 0, 41)], ..Default::default() };
        assert!(trends(&h).is_empty());
    }

    #[test]
    fn growing_counters_are_reported_by_severity() {
        let h = HealthHistory { snapshots: vec![snap(0, 8, 0, 35), snap(86400, 12, 0, 38), snap(2 * 86400, 12, 3, 63)], ..Default::default() };
        let f = trends(&h);
        let codes: Vec<_> = f.iter().map(|x| (x.code.as_str(), x.severity)).collect();
        assert!(codes.contains(&("TREND_PENDING", Severity::Error)), "{codes:?}");
        assert!(codes.contains(&("TREND_REALLOCATED", Severity::Warning)));
        assert!(codes.contains(&("TREND_HOT", Severity::Warning)));
        assert!(f.iter().any(|x| x.title.contains("8 → 12")));
    }

    #[test]
    fn record_skips_recent_duplicates_and_persists() {
        let dir = std::env::temp_dir().join(format!("chudclone-health-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let disk = DiskInfo { model: "Disque test".into(), serial: "SN123".into(), size: 1 << 30, ..crate::model::DiskInfo::test_default() };
        let rep = |pending: u64| SmartReport {
            available: true,
            protocol: "ATA".into(),
            level: HealthLevel::Good,
            drive_reports_failure: Some(false),
            temperature_c: Some(30),
            power_on_hours: Some(10),
            power_cycles: Some(1),
            wear_percent_used: None,
            attributes: vec![crate::smart::SmartAttribute {
                id: 197,
                name: String::new(),
                current: 100,
                worst: 100,
                threshold: Some(0),
                raw: pending,
                prefail: false,
                below_threshold: false,
            }],
            nvme: None,
            findings: vec![],
            unavailable_reason: None,
        };
        assert_eq!(record(&dir, &disk, &rep(0), 1000).snapshots.len(), 1);
        assert_eq!(record(&dir, &disk, &rep(0), 2000).snapshots.len(), 1, "doublon récent ignoré");
        assert_eq!(record(&dir, &disk, &rep(2), 3000).snapshots.len(), 2, "changement enregistré");
        let reloaded = load(&dir, &disk);
        assert_eq!(reloaded.snapshots.len(), 2);
        assert_eq!(reloaded.snapshots[1].pending, Some(2));
        // Without a serial number: nothing is saved (uncertain identity).
        let anon = DiskInfo { serial: String::new(), ..disk.clone() };
        assert!(record(&dir, &anon, &rep(5), 4000).snapshots.len() <= 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
