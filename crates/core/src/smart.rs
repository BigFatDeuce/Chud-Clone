//! Interprétation des données de santé SMART (ATA) et du journal de santé NVMe.
//!
//! Ce module ne fait qu'analyser des tampons bruts ; leur lecture sur le matériel est
//! faite par la couche Windows. Les conclusions sont des INDICATEURS : un disque aux
//! compteurs irréprochables peut tomber en panne, et un compteur dégradé ne prédit pas
//! une date de panne. Les libellés le disent.

use crate::diag::{Finding, Severity, max_severity};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthLevel {
    /// Aucun indicateur défavorable relevé.
    Good,
    /// Indicateurs à surveiller.
    Watch,
    /// Indicateurs de défaillance ou de dégradation sérieuse.
    Bad,
    /// Données de santé indisponibles.
    Unknown,
}

impl HealthLevel {
    pub fn label(self) -> &'static str {
        match self {
            HealthLevel::Good => t!("Aucune anomalie signalée", "No anomaly reported"),
            HealthLevel::Watch => t!("À surveiller", "Keep an eye on it"),
            HealthLevel::Bad => t!("Dégradé", "Degraded"),
            HealthLevel::Unknown => t!("Santé inconnue", "Health unknown"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmartAttribute {
    pub id: u8,
    pub name: String,
    pub current: u8,
    pub worst: u8,
    pub threshold: Option<u8>,
    pub raw: u64,
    /// Attribut de type « pré-défaillance » (son franchissement de seuil annonce une panne).
    pub prefail: bool,
    /// Valeur normalisée au niveau du seuil ou en dessous.
    pub below_threshold: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NvmeHealth {
    pub critical_warning: u8,
    pub available_spare: u8,
    pub spare_threshold: u8,
    pub percentage_used: u8,
    pub data_read_bytes: u128,
    pub data_written_bytes: u128,
    pub unsafe_shutdowns: u64,
    pub media_errors: u64,
    pub error_log_entries: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmartReport {
    pub available: bool,
    /// Protocole utilisé : « ATA », « NVMe », « ATA via USB (SAT) ».
    pub protocol: String,
    pub level: HealthLevel,
    /// Verdict du disque lui-même (commande SMART RETURN STATUS), si obtenu.
    pub drive_reports_failure: Option<bool>,
    pub temperature_c: Option<i32>,
    pub power_on_hours: Option<u64>,
    pub power_cycles: Option<u64>,
    /// Usure estimée par le constructeur (SSD), en pourcentage consommé.
    pub wear_percent_used: Option<u8>,
    pub attributes: Vec<SmartAttribute>,
    pub nvme: Option<NvmeHealth>,
    pub findings: Vec<Finding>,
    pub unavailable_reason: Option<String>,
}

impl SmartReport {
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            available: false,
            protocol: String::new(),
            level: HealthLevel::Unknown,
            drive_reports_failure: None,
            temperature_c: None,
            power_on_hours: None,
            power_cycles: None,
            wear_percent_used: None,
            attributes: vec![],
            nvme: None,
            findings: vec![],
            unavailable_reason: Some(reason.into()),
        }
    }

    fn finish(mut self) -> Self {
        self.level = match max_severity(&self.findings) {
            Some(Severity::Critical) | Some(Severity::Error) => HealthLevel::Bad,
            Some(Severity::Warning) => HealthLevel::Watch,
            _ => HealthLevel::Good,
        };
        self
    }
}

fn attr_name(id: u8) -> &'static str {
    match id {
        1 => t!("Taux d'erreurs de lecture", "Read error rate"),
        3 => t!("Temps de mise en rotation", "Spin-up time"),
        4 => t!("Cycles marche/arrêt du moteur", "Start/stop count"),
        5 => t!("Secteurs réalloués", "Reallocated sectors"),
        7 => t!("Taux d'erreurs de positionnement", "Seek error rate"),
        9 => t!("Heures de fonctionnement", "Power-on hours"),
        10 => t!("Nouvelles tentatives de mise en rotation", "Spin retry count"),
        12 => t!("Cycles d'alimentation", "Power cycle count"),
        170 => t!("Blocs de réserve disponibles", "Available reserved blocks"),
        171 => t!("Échecs de programmation", "Program fail count"),
        172 => t!("Échecs d'effacement", "Erase fail count"),
        173 => t!("Usure (nivellement)", "Wear leveling"),
        174 => t!("Pertes d'alimentation inattendues", "Unexpected power losses"),
        177 => t!("Usure (nivellement)", "Wear leveling"),
        179 => t!("Blocs de réserve utilisés", "Used reserved blocks"),
        181 => t!("Échecs de programmation", "Program fail count"),
        182 => t!("Échecs d'effacement", "Erase fail count"),
        183 => t!("Erreurs d'interface / blocs défectueux", "Interface errors / bad blocks"),
        184 => t!("Erreurs de bout en bout", "End-to-end errors"),
        187 => t!("Erreurs non corrigibles signalées", "Reported uncorrectable errors"),
        188 => t!("Commandes expirées", "Command timeouts"),
        190 => t!("Température (flux d'air)", "Airflow temperature"),
        191 => t!("Chocs détectés", "G-sense errors"),
        192 => t!("Arrêts d'urgence des têtes", "Emergency head retracts"),
        193 => t!("Cycles de chargement des têtes", "Head load cycles"),
        194 => t!("Température", "Temperature"),
        195 => t!("Corrections ECC matérielles", "Hardware ECC recovered"),
        196 => t!("Événements de réallocation", "Reallocation events"),
        197 => t!("Secteurs en attente de réallocation", "Current pending sectors"),
        198 => t!("Secteurs non corrigibles (hors ligne)", "Offline uncorrectable sectors"),
        199 => t!("Erreurs CRC d'interface (câble)", "Interface CRC errors (cable)"),
        200 => t!("Taux d'erreurs d'écriture", "Write error rate"),
        202 => t!("Durée de vie consommée", "Lifetime used"),
        231 => t!("Durée de vie restante (SSD)", "Remaining life (SSD)"),
        232 => t!("Réserve restante", "Remaining spare"),
        233 => t!("Indicateur d'usure du support", "Media wearout indicator"),
        241 => t!("Total écrit", "Total written"),
        242 => t!("Total lu", "Total read"),
        _ => t!("Attribut constructeur", "Vendor attribute"),
    }
}

/// Analyse la table d'attributs SMART ATA (512 octets), la table des seuils
/// (512 octets, facultative) et le verdict du disque (facultatif).
pub fn parse_ata(data: &[u8], thresholds: Option<&[u8]>, drive_reports_failure: Option<bool>, protocol: &str) -> SmartReport {
    let mut r = SmartReport::unavailable("");
    r.unavailable_reason = None;
    r.available = true;
    r.protocol = protocol.to_string();
    r.drive_reports_failure = drive_reports_failure;
    if data.len() < 362 {
        return SmartReport::unavailable(t!("réponse SMART tronquée", "truncated SMART response"));
    }
    let threshold_of = |id: u8| -> Option<u8> {
        let t = thresholds?;
        if t.len() < 362 {
            return None;
        }
        (0..30).map(|i| 2 + i * 12).find(|o| t[*o] == id).map(|o| t[o + 1])
    };
    for i in 0..30 {
        let o = 2 + i * 12;
        let id = data[o];
        if id == 0 {
            continue;
        }
        let flags = u16::from_le_bytes([data[o + 1], data[o + 2]]);
        let current = data[o + 3];
        let worst = data[o + 4];
        let mut raw = [0u8; 8];
        raw[..6].copy_from_slice(&data[o + 5..o + 11]);
        let raw = u64::from_le_bytes(raw);
        let threshold = threshold_of(id);
        let prefail = flags & 1 != 0;
        let below = threshold.is_some_and(|t| t > 0 && current <= t);
        r.attributes.push(SmartAttribute { id, name: attr_name(id).to_string(), current, worst, threshold, raw, prefail, below_threshold: below });
    }
    if r.attributes.is_empty() {
        return SmartReport::unavailable(t!("le disque n'a renvoyé aucun attribut SMART", "the disk returned no SMART attributes"));
    }
    let raw = |id: u8| r.attributes.iter().find(|a| a.id == id).map(|a| a.raw);
    let cur = |id: u8| r.attributes.iter().find(|a| a.id == id).map(|a| a.current);

    r.power_on_hours = raw(9).map(|v| v & 0xFFFF_FFFF);
    r.power_cycles = raw(12).map(|v| v & 0xFFFF_FFFF);
    r.temperature_c = raw(194).or(raw(190)).map(|v| (v & 0xFF) as i32).filter(|t| (1..=120).contains(t));
    // Usure SSD : valeur normalisée « restante » selon l'attribut que le constructeur renseigne.
    r.wear_percent_used = [231u8, 233, 177, 173].iter().find_map(|id| cur(*id)).filter(|v| (1..=100).contains(v)).map(|v| 100 - v);

    let mut f = Vec::new();
    if drive_reports_failure == Some(true) {
        f.push(Finding::critical(
            "SMART_FAILING",
            t!("Le disque annonce une défaillance", "The disk reports a failure"),
            t!("Le disque déclare lui-même qu'un de ses seuils de fiabilité est dépassé (état SMART « défaillant »). \
             C'est son propre pronostic, pas une certitude, mais il doit être pris au sérieux : \
             sauvegardez ou clonez ce disque sans attendre et prévoyez son remplacement.", "The disk itself reports that one of its reliability thresholds is exceeded (SMART status \"failing\"). This is its own prognosis, not a certainty, but it must be taken seriously: back up or clone this disk without delay and plan its replacement."),
        ));
    }
    for a in r.attributes.iter().filter(|a| a.below_threshold) {
        let (sev, what): (Severity, &str) =
            if a.prefail { (Severity::Critical, t!("pré-défaillance", "pre-failure")) } else { (Severity::Warning, "vieillissement") };
        f.push(Finding::new(
            sev,
            "SMART_THRESHOLD",
            tf!("Seuil SMART franchi : {}", "SMART threshold crossed: {}", a.name),
            tf!(
                "L'attribut {} ({}) vaut {} pour un seuil de {} (attribut de {what}).",
                "Attribute {} ({}) is {} for a threshold of {} ({what} attribute).",
                a.id,
                a.name,
                a.current,
                a.threshold.unwrap_or(0)
            ),
        ));
    }
    let low32 = |id: u8| raw(id).map(|v| v & 0xFFFF_FFFF).unwrap_or(0);
    let pending = low32(197);
    if pending > 0 {
        f.push(Finding::error(
            "SMART_PENDING",
            tf!("{pending} secteur(s) en attente de réallocation", "{pending} sector(s) pending reallocation"),
            t!("Le disque a rencontré des secteurs qu'il n'a pas réussi à lire correctement et qu'il tentera de remplacer. \
             Des erreurs de lecture sont probables pendant le clonage à ces endroits.", "The disk ran into sectors it could not read correctly and will try to replace. Read errors are likely at these places during cloning."),
        ));
    }
    let offline = low32(198);
    if offline > 0 {
        f.push(Finding::error(
            "SMART_UNCORRECTABLE",
            tf!("{offline} secteur(s) non corrigible(s)", "{offline} uncorrectable sector(s)"),
            t!(
                "Le disque a recensé des secteurs définitivement illisibles lors de ses tests internes.",
                "The disk recorded permanently unreadable sectors during its internal tests."
            ),
        ));
    }
    let realloc = low32(5);
    if realloc > 0 {
        f.push(Finding::new(
            if realloc >= 50 { Severity::Error } else { Severity::Warning },
            "SMART_REALLOCATED",
            tf!("{realloc} secteur(s) réalloué(s)", "{realloc} reallocated sector(s)"),
            t!("Le disque a déjà remplacé des secteurs défectueux par des secteurs de réserve. Un nombre qui augmente \
             est un signe de dégradation ; un nombre stable et faible peut rester sans conséquence pendant longtemps.", "The disk has already replaced bad sectors with spare ones. A growing number is a sign of degradation; a low, stable number can stay harmless for a long time."),
        ));
    }
    let reported = low32(187);
    if reported > 0 {
        f.push(Finding::warning(
            "SMART_REPORTED_UNCORRECTABLE",
            tf!("{reported} erreur(s) non corrigible(s) signalée(s) à l'ordinateur", "{reported} uncorrectable error(s) reported to the computer"),
            t!(
                "Le disque a déjà renvoyé des erreurs de lecture ou d'écriture qu'il n'a pas pu corriger.",
                "The disk has already returned read or write errors it could not correct."
            ),
        ));
    }
    let crc = low32(199);
    if crc > 0 {
        f.push(Finding::info(
            "SMART_CRC",
            tf!("{crc} erreur(s) de transmission (CRC)", "{crc} transfer error(s) (CRC)"),
            t!("Ce compteur cumule, sur toute la vie du disque, les erreurs de communication avec l'ordinateur. \
             Il met en cause le câble ou le connecteur plutôt que le disque. S'il augmente, changez de câble ou de port.", "This counter accumulates, over the disk's whole life, the communication errors with the computer. It points to the cable or connector rather than the disk. If it grows, change the cable or port."),
        ));
    }
    if let Some(t) = r.temperature_c {
        if t >= 65 {
            f.push(Finding::error(
                "SMART_TEMPERATURE",
                tf!("Température très élevée : {t} °C", "Very high temperature: {t} °C"),
                t!(
                    "Le disque fonctionne au-delà de la plage habituelle. Améliorez son refroidissement avant une opération longue.",
                    "The disk runs beyond its usual range. Improve its cooling before a long operation."
                ),
            ));
        } else if t >= 55 {
            f.push(Finding::warning(
                "SMART_TEMPERATURE",
                tf!("Température élevée : {t} °C", "High temperature: {t} °C"),
                t!(
                    "Le disque est chaud. Une opération longue comme un clonage le sollicitera davantage.",
                    "The disk is hot. A long operation such as cloning will stress it further."
                ),
            ));
        }
    }
    if let Some(w) = r.wear_percent_used
        && w >= 90
    {
        f.push(Finding::warning(
            "SMART_WEAR",
            tf!("Usure estimée : {w} % de la durée de vie annoncée", "Estimated wear: {w} % of rated life"),
            t!(
                "Selon le compteur du constructeur, ce SSD approche de la fin de sa durée de vie nominale. C'est une estimation, pas une échéance.",
                "According to the vendor counter, this SSD is nearing the end of its rated life. This is an estimate, not a deadline."
            ),
        ));
    }
    r.findings = f;
    r.finish()
}

fn le128(b: &[u8], o: usize) -> u128 {
    let mut a = [0u8; 16];
    a.copy_from_slice(&b[o..o + 16]);
    u128::from_le_bytes(a)
}

/// Analyse le journal « SMART / Health Information » NVMe (page 0x02, 512 octets).
pub fn parse_nvme(log: &[u8]) -> SmartReport {
    if log.len() < 192 {
        return SmartReport::unavailable(t!("journal de santé NVMe tronqué", "truncated NVMe health log"));
    }
    let mut r = SmartReport::unavailable("");
    r.unavailable_reason = None;
    r.available = true;
    r.protocol = "NVMe".into();
    let cw = log[0];
    let kelvin = u16::from_le_bytes([log[1], log[2]]) as i32;
    let sat = |v: u128| v.min(u64::MAX as u128) as u64;
    let h = NvmeHealth {
        critical_warning: cw,
        available_spare: log[3],
        spare_threshold: log[4],
        percentage_used: log[5],
        // Unités de 1000 blocs de 512 octets.
        data_read_bytes: le128(log, 32).saturating_mul(512_000),
        data_written_bytes: le128(log, 48).saturating_mul(512_000),
        unsafe_shutdowns: sat(le128(log, 144)),
        media_errors: sat(le128(log, 160)),
        error_log_entries: sat(le128(log, 176)),
    };
    r.temperature_c = if kelvin > 0 { Some(kelvin - 273) } else { None };
    r.power_cycles = Some(sat(le128(log, 112)));
    r.power_on_hours = Some(sat(le128(log, 128)));
    r.wear_percent_used = Some(h.percentage_used);
    r.drive_reports_failure = Some(cw & !0x02 != 0);

    let mut f = Vec::new();
    if cw & 0x01 != 0 {
        f.push(Finding::critical("NVME_SPARE_LOW", t!("Réserve de blocs épuisée", "Spare block reserve exhausted"), t!("Le SSD signale que sa réserve de blocs de remplacement est passée sous son seuil d'alerte. Le remplacement du disque est à prévoir sans tarder.", "The SSD reports that its spare block reserve has dropped below its alert threshold. Plan to replace the disk soon.")));
    }
    if cw & 0x04 != 0 {
        f.push(Finding::critical(
            "NVME_RELIABILITY",
            t!("Fiabilité dégradée signalée par le SSD", "Degraded reliability reported by the SSD"),
            t!(
                "Le SSD signale une dégradation de sa fiabilité (erreurs de support ou internes). Clonez-le ou sauvegardez-le sans attendre.",
                "The SSD reports degraded reliability (media or internal errors). Clone or back it up without delay."
            ),
        ));
    }
    if cw & 0x08 != 0 {
        f.push(Finding::critical(
            "NVME_READ_ONLY",
            t!("SSD passé en lecture seule", "SSD switched to read-only"),
            t!(
                "Le SSD s'est placé en lecture seule, comportement typique d'une fin de vie. Il peut encore servir de SOURCE, pas de destination.",
                "The SSD has put itself in read-only mode, typical of end of life. It can still be used as a SOURCE, not as a destination."
            ),
        ));
    }
    if cw & 0x10 != 0 {
        f.push(Finding::error(
            "NVME_BACKUP_FAILED",
            t!("Défaut de la protection contre les coupures", "Power-loss protection failure"),
            t!(
                "Le SSD signale une défaillance de son dispositif de sauvegarde de la mémoire volatile.",
                "The SSD reports a failure of its volatile memory backup device."
            ),
        ));
    }
    if cw & 0x02 != 0 {
        f.push(Finding::warning(
            "NVME_TEMPERATURE",
            t!("Température hors plage signalée par le SSD", "Out-of-range temperature reported by the SSD"),
            t!(
                "Le SSD signale une température au-delà de son seuil. Améliorez son refroidissement.",
                "The SSD reports a temperature beyond its threshold. Improve its cooling."
            ),
        ));
    } else if let Some(t) = r.temperature_c
        && t >= 75
    {
        f.push(Finding::warning(
            "NVME_TEMPERATURE",
            tf!("Température élevée : {t} °C", "High temperature: {t} °C"),
            t!(
                "Le SSD est chaud ; il peut réduire son débit pendant une opération longue.",
                "The SSD is hot; it may throttle during a long operation."
            ),
        ));
    }
    if h.media_errors > 0 {
        f.push(Finding::error(
            "NVME_MEDIA_ERRORS",
            tf!("{} erreur(s) de support enregistrée(s)", "{} media error(s) recorded", h.media_errors),
            t!(
                "Le SSD a recensé des erreurs d'intégrité de données non récupérées. Des erreurs de lecture sont possibles pendant le clonage.",
                "The SSD recorded unrecovered data-integrity errors. Read errors are possible during cloning."
            ),
        ));
    }
    if h.percentage_used >= 100 {
        f.push(Finding::warning(
            "NVME_WEAR",
            tf!("Usure estimée : {} % de l'endurance annoncée", "Estimated wear: {} % of rated endurance", h.percentage_used),
            t!("Selon l'estimation du constructeur, ce SSD a dépassé son endurance nominale. Ce n'est pas une panne, mais la garantie d'endurance est épuisée.", "According to the vendor's estimate, this SSD has exceeded its rated endurance. This is not a failure, but the endurance warranty is used up."),
        ));
    } else if h.percentage_used >= 90 {
        f.push(Finding::info(
            "NVME_WEAR",
            tf!("Usure estimée : {} % de l'endurance annoncée", "Estimated wear: {} % of rated endurance", h.percentage_used),
            t!("Estimation du constructeur ; ce n'est pas une échéance.", "Vendor estimate; this is not a deadline."),
        ));
    }
    r.nvme = Some(h);
    r.findings = f;
    r.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put_attr(data: &mut [u8], slot: usize, id: u8, flags: u16, cur: u8, worst: u8, raw: u64) {
        let o = 2 + slot * 12;
        data[o] = id;
        data[o + 1..o + 3].copy_from_slice(&flags.to_le_bytes());
        data[o + 3] = cur;
        data[o + 4] = worst;
        data[o + 5..o + 11].copy_from_slice(&raw.to_le_bytes()[..6]);
    }
    fn put_thr(t: &mut [u8], slot: usize, id: u8, thr: u8) {
        let o = 2 + slot * 12;
        t[o] = id;
        t[o + 1] = thr;
    }

    #[test]
    fn healthy_ata_disk() {
        let mut d = vec![0u8; 512];
        let mut t = vec![0u8; 512];
        put_attr(&mut d, 0, 5, 0x33, 100, 100, 0);
        put_thr(&mut t, 0, 5, 10);
        put_attr(&mut d, 1, 9, 0x32, 95, 95, 12345);
        put_attr(&mut d, 2, 194, 0x22, 64, 50, 36 | (50 << 16));
        put_attr(&mut d, 3, 197, 0x32, 100, 100, 0);
        put_attr(&mut d, 4, 12, 0x32, 99, 99, 800);
        let r = parse_ata(&d, Some(&t), Some(false), "ATA");
        assert!(r.available);
        assert_eq!(r.level, HealthLevel::Good);
        assert_eq!(r.power_on_hours, Some(12345));
        assert_eq!(r.power_cycles, Some(800));
        assert_eq!(r.temperature_c, Some(36));
        assert!(r.findings.is_empty());
        assert_eq!(r.attributes.len(), 5);
        assert_eq!(r.attributes[0].threshold, Some(10));
    }

    #[test]
    fn degraded_ata_disk() {
        let mut d = vec![0u8; 512];
        let mut t = vec![0u8; 512];
        put_attr(&mut d, 0, 5, 0x33, 5, 5, 1200);
        put_thr(&mut t, 0, 5, 10);
        put_attr(&mut d, 1, 197, 0x32, 100, 100, 16);
        put_attr(&mut d, 2, 198, 0x30, 100, 100, 3);
        put_attr(&mut d, 3, 199, 0x3E, 200, 200, 41);
        let r = parse_ata(&d, Some(&t), Some(true), "ATA");
        assert_eq!(r.level, HealthLevel::Bad);
        let codes: Vec<&str> = r.findings.iter().map(|f| f.code.as_str()).collect();
        for c in ["SMART_FAILING", "SMART_THRESHOLD", "SMART_PENDING", "SMART_UNCORRECTABLE", "SMART_REALLOCATED", "SMART_CRC"] {
            assert!(codes.contains(&c), "{c} manquant dans {codes:?}");
        }
        // Une erreur CRC seule (câble) ne dégrade pas le niveau de santé du disque.
        let mut d2 = vec![0u8; 512];
        put_attr(&mut d2, 0, 199, 0x3E, 200, 200, 41);
        let r2 = parse_ata(&d2, None, None, "ATA");
        assert_eq!(r2.level, HealthLevel::Good);
        assert_eq!(r2.findings.len(), 1);
    }

    #[test]
    fn watch_level_for_few_reallocated() {
        let mut d = vec![0u8; 512];
        put_attr(&mut d, 0, 5, 0x33, 100, 100, 8);
        let r = parse_ata(&d, None, None, "ATA via USB (SAT)");
        assert_eq!(r.level, HealthLevel::Watch);
        assert_eq!(r.protocol, "ATA via USB (SAT)");
    }

    #[test]
    fn empty_or_truncated_ata() {
        assert!(!parse_ata(&[0u8; 512], None, None, "ATA").available);
        assert!(!parse_ata(&[0u8; 100], None, None, "ATA").available);
    }

    #[test]
    fn nvme_health() {
        let mut l = vec![0u8; 512];
        l[1..3].copy_from_slice(&(273u16 + 41).to_le_bytes());
        l[3] = 100;
        l[4] = 10;
        l[5] = 7;
        l[48..56].copy_from_slice(&2_000_000u64.to_le_bytes());
        l[112..120].copy_from_slice(&321u64.to_le_bytes());
        l[128..136].copy_from_slice(&4567u64.to_le_bytes());
        let r = parse_nvme(&l);
        assert_eq!(r.level, HealthLevel::Good);
        assert_eq!(r.temperature_c, Some(41));
        assert_eq!(r.power_on_hours, Some(4567));
        assert_eq!(r.power_cycles, Some(321));
        assert_eq!(r.wear_percent_used, Some(7));
        assert_eq!(r.nvme.as_ref().unwrap().data_written_bytes, 2_000_000u128 * 512_000);
        assert_eq!(r.drive_reports_failure, Some(false));

        l[0] = 0x05; // réserve basse + fiabilité dégradée
        l[160..168].copy_from_slice(&9u64.to_le_bytes());
        l[5] = 104;
        let r = parse_nvme(&l);
        assert_eq!(r.level, HealthLevel::Bad);
        let codes: Vec<&str> = r.findings.iter().map(|f| f.code.as_str()).collect();
        for c in ["NVME_SPARE_LOW", "NVME_RELIABILITY", "NVME_MEDIA_ERRORS", "NVME_WEAR"] {
            assert!(codes.contains(&c), "{c} manquant");
        }
        assert!(!parse_nvme(&[0u8; 64]).available);
    }
}
