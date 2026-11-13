//! Operation log and final report (JSON, TXT, HTML).
//!
//! The report is saved locally. Nothing is ever sent over the network.

use crate::clone::{CloneOutcome, FinalStatus};
use crate::diag::{Finding, Severity, human_size};
use crate::engine::{BadOutcome, Decision, EngineOptions, LogLevel, Observer, Progress, UnreadableInfo};
use crate::model::DiskInfo;
use crate::plan::{ClonePlan, CopyMethod, PlanOptions};
use crate::smart::SmartReport;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    /// Milliseconds since the start of the operation.
    pub t_ms: u64,
    /// Heure locale (HH:MM:SS).
    pub time: String,
    pub level: LogLevel,
    pub message: String,
}

/// Observer that records the log (in memory and, if requested, in a file written
/// as it goes) then relays to another observer.
pub struct Journal<'a> {
    inner: &'a dyn Observer,
    start: Instant,
    entries: Mutex<Vec<LogEntry>>,
    file: Option<Mutex<File>>,
}

impl<'a> Journal<'a> {
    pub fn new(inner: &'a dyn Observer, log_file: Option<&Path>) -> Self {
        let file = log_file.and_then(|p| File::create(p).ok()).map(Mutex::new);
        Self { inner, start: Instant::now(), entries: Mutex::new(Vec::new()), file }
    }

    pub fn entries(&self) -> Vec<LogEntry> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Adds a line to the log without going through the engine.
    pub fn note(&self, level: LogLevel, msg: &str) {
        self.log(level, msg);
    }
}

fn level_tag(l: LogLevel) -> &'static str {
    match l {
        LogLevel::Debug => "DEBUG",
        LogLevel::Info => "INFO ",
        LogLevel::Warning => t!("AVERT", "WARN "),
        LogLevel::Error => t!("ERREUR", "ERROR"),
    }
}

impl Observer for Journal<'_> {
    fn progress(&self, p: &Progress) {
        self.inner.progress(p);
    }
    fn log(&self, level: LogLevel, msg: &str) {
        let entry = LogEntry {
            t_ms: self.start.elapsed().as_millis() as u64,
            time: chrono::Local::now().format("%H:%M:%S").to_string(),
            level,
            message: msg.to_string(),
        };
        if let Some(f) = &self.file
            && let Ok(mut f) = f.lock()
        {
            let _ = writeln!(f, "{} [{}] {}", entry.time, level_tag(level), entry.message);
            let _ = f.flush();
        }
        self.entries.lock().unwrap_or_else(|e| e.into_inner()).push(entry);
        self.inner.log(level, msg);
    }
    fn decide_unreadable(&self, info: &UnreadableInfo) -> Decision {
        let d = self.inner.decide_unreadable(info);
        self.log(
            LogLevel::Warning,
            &tf!(
                "Décision de l'utilisateur pour {} secteur(s) illisible(s) : {}",
                "User decision for {} unreadable sector(s): {}",
                info.total_sectors,
                match d {
                    Decision::Abort => t!("arrêter le clonage", "stop cloning"),
                    Decision::ZeroFill => t!("poursuivre en remplaçant par des zéros", "continue, replacing with zeros"),
                }
            ),
        );
        d
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub id: String,
    pub app: String,
    pub version: String,
    pub started_at: String,
    pub finished_at: String,
    pub source: DiskInfo,
    pub destination: DiskInfo,
    pub plan_options: PlanOptions,
    pub engine_options: EngineOptions,
    pub plan: ClonePlan,
    pub precheck: Vec<Finding>,
    pub smart_source: Option<SmartReport>,
    pub smart_destination: Option<SmartReport>,
    pub outcome: CloneOutcome,
    pub log: Vec<LogEntry>,
}

/// Summary of a report, for lists.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportSummary {
    pub id: String,
    pub started_at: String,
    pub status: FinalStatus,
    pub source: String,
    pub destination: String,
    pub bytes_copied: u64,
    pub duration_ms: u64,
}

impl Report {
    pub fn summary(&self) -> ReportSummary {
        ReportSummary {
            id: self.id.clone(),
            started_at: self.started_at.clone(),
            status: self.outcome.status,
            source: self.source.title(),
            destination: self.destination.title(),
            bytes_copied: self.outcome.copy.bytes_copied,
            duration_ms: self.outcome.duration_ms,
        }
    }
}

/// Report identifier based on the local date (`2026-11-13_142233`).
pub fn new_report_id() -> String {
    chrono::Local::now().format("%Y-%m-%d_%H%M%S").to_string()
}

pub fn now_string() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

pub fn save_report(dir: &Path, report: &Report) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}.json", report.id));
    let json = serde_json::to_vec_pretty(report).map_err(std::io::Error::other)?;
    std::fs::write(&path, json)?;
    Ok(path)
}

pub fn load_report(dir: &Path, id: &str) -> std::io::Result<Report> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, t!("identifiant de rapport invalide", "invalid report identifier")));
    }
    let data = std::fs::read(dir.join(format!("{id}.json")))?;
    serde_json::from_slice(&data).map_err(std::io::Error::other)
}

pub fn list_reports(dir: &Path) -> Vec<ReportSummary> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        if let Ok(data) = std::fs::read(&p)
            && let Ok(r) = serde_json::from_slice::<Report>(&data)
        {
            out.push(r.summary());
        }
    }
    out.sort_by(|a, b| b.id.cmp(&a.id));
    out
}

pub fn duration_text(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 3600 {
        tf!("{} h {:02} min {:02} s", "{} h {:02} min {:02} s", s / 3600, (s % 3600) / 60, s % 60)
    } else if s >= 60 {
        tf!("{} min {:02} s", "{} min {:02} s", s / 60, s % 60)
    } else {
        format!("{}.{} s", s, (ms % 1000) / 100)
    }
}

fn method_label(m: CopyMethod) -> &'static str {
    match m {
        CopyMethod::Smart => t!("blocs utilisés", "used blocks"),
        CopyMethod::Raw => t!("secteur par secteur", "sector by sector"),
        CopyMethod::Skip => t!("non copiée", "not copied"),
    }
}

fn disk_lines(d: &DiskInfo) -> Vec<(String, String)> {
    vec![
        (t!("Désignation", "Name").into(), d.title()),
        (t!("Modèle", "Model").into(), d.model.clone()),
        (
            t!("Numéro de série", "Serial number").into(),
            if d.serial.is_empty() { t!("non communiqué", "not reported").into() } else { d.serial.clone() },
        ),
        (t!("Interface", "Interface").into(), format!("{} — {}", d.bus, d.media.label())),
        (t!("Capacité", "Capacity").into(), tf!("{} ({} octets)", "{} ({} bytes)", human_size(d.size), d.size)),
        (
            t!("Secteurs", "Sectors").into(),
            tf!("{} octets logiques / {} physiques", "{} bytes logical / {} physical", d.sector_size, d.physical_sector_size),
        ),
        (t!("Partitionnement", "Partitioning").into(), d.table.label().to_string()),
        (t!("Chemin", "Path").into(), d.path.clone()),
    ]
}

// ---------------------------------------------------------------------------
// TXT
// ---------------------------------------------------------------------------

pub fn render_txt(r: &Report) -> String {
    let mut s = String::new();
    let o = &r.outcome;
    let line = "=".repeat(78);
    let _ = writeln!(s, "{}", tf!("{line}\n{} {} — RAPPORT DE CLONAGE\n{line}", "{line}\n{} {} — CLONE REPORT\n{line}", r.app, r.version));
    let _ = writeln!(s, "{}", tf!("Rapport        : {}", "Report         : {}", r.id));
    let _ = writeln!(s, "{}", tf!("Début          : {}", "Started        : {}", r.started_at));
    let _ = writeln!(s, "{}", tf!("Fin            : {}", "Finished       : {}", r.finished_at));
    let _ = writeln!(s, "{}", tf!("Durée          : {}", "Duration       : {}", duration_text(o.duration_ms)));
    let _ = writeln!(s, "{}", tf!("\nRÉSULTAT       : {}", "\nRESULT         : {}", o.status.label().to_uppercase()));
    for reason in &o.reasons {
        let _ = writeln!(s, "  - {reason}");
    }

    for (title, d) in [(t!("DISQUE SOURCE", "SOURCE DISK"), &r.source), (t!("DISQUE DE DESTINATION", "DESTINATION DISK"), &r.destination)] {
        let _ = writeln!(s, "\n--- {title} {}", "-".repeat(72usize.saturating_sub(title.len())));
        for (k, v) in disk_lines(d) {
            let _ = writeln!(s, "{k:<18}: {v}");
        }
    }

    let _ = writeln!(s, "{}", tf!("\n--- MÉTHODE {}", "\n--- METHOD {}", "-".repeat(66)));
    let _ = writeln!(s, "{}", tf!("Mode de clonage   : {}", "Clone mode        : {}", r.plan.mode.label()));
    let _ = writeln!(s, "{}", tf!("Vérification      : {}", "Verification      : {}", r.engine_options.verify.label()));
    let _ = writeln!(
        s,
        "{}",
        tf!("Données planifiées: {} ({} octets)", "Planned data      : {} ({} bytes)", human_size(r.plan.total_copy_bytes), r.plan.total_copy_bytes)
    );
    let _ = writeln!(
        s,
        "{}",
        tf!("Données copiées   : {} ({} octets)", "Copied data       : {} ({} bytes)", human_size(o.copy.bytes_copied), o.copy.bytes_copied)
    );
    if o.copy.copy_ms > 0 {
        let _ = writeln!(
            s,
            "{}",
            tf!(
                "Débit moyen       : {}/s (copie : {})",
                "Average speed     : {}/s (copy: {})",
                human_size(o.copy.avg_bps),
                duration_text(o.copy.copy_ms)
            )
        );
    }

    let _ = writeln!(s, "\n--- PARTITIONS {}", "-".repeat(63));
    let _ = writeln!(
        s,
        "{}",
        tf!(
            "{:<3} {:<28} {:<10} {:>10} {:>10}  {:<20} Position",
            "{:<3} {:<28} {:<10} {:>10} {:>10}  {:<20} Position",
            "N°",
            t!("Type", "No."),
            t!("Système", "File system"),
            t!("Taille", "Size"),
            t!("Utilisé", "Used"),
            t!("Copie", "Copy")
        )
    );
    for p in &r.plan.partitions {
        let _ = writeln!(
            s,
            "{:<3} {:<28} {:<10} {:>10} {:>10}  {:<20} {}",
            p.number,
            truncate(&p.type_label, 28),
            truncate(p.fs.label(), 10),
            human_size(p.src_len),
            p.used_bytes.map(human_size).unwrap_or_else(|| "—".into()),
            method_label(p.method),
            if p.moved { tf!("déplacée → {}", "moved → {}", human_size(p.dst_start)) } else { t!("inchangée", "unchanged").into() }
        );
        if let Some(reason) = &p.method_reason
            && p.method == CopyMethod::Raw
        {
            let _ = writeln!(s, "      ({reason})");
        }
        if p.dst_len != p.src_len {
            let _ = writeln!(
                s,
                "{}",
                tf!("      (partition agrandie à {} sur la destination)", "      (partition grown to {} on the destination)", human_size(p.dst_len))
            );
        }
    }

    let _ = writeln!(s, "{}", tf!("\n--- ERREURS DE LECTURE ET D'ÉCRITURE {}", "\n--- READ AND WRITE ERRORS {}", "-".repeat(41)));
    let _ = writeln!(s, "{}", tf!("Événements d'erreur de lecture : {}", "Read error events              : {}", o.copy.read_error_events));
    let _ = writeln!(s, "{}", tf!("Erreurs d'écriture             : {}", "Write errors                   : {}", o.copy.write_error_events));
    let _ = writeln!(s, "{}", tf!("Octets récupérés après reprise : {}", "Bytes recovered after retry    : {}", o.copy.recovered_bytes));
    let _ = writeln!(s, "{}", tf!("Octets remplacés par des zéros : {}", "Bytes replaced with zeros      : {}", o.copy.bytes_zero_filled));
    if o.bad_ranges.is_empty() {
        let _ = writeln!(s, "{}", tf!("Secteurs illisibles            : aucun", "Unreadable sectors             : none"));
    } else {
        let _ = writeln!(s, "{}", tf!("Secteurs illisibles (LBA sur la source) :", "Unreadable sectors (LBA on the source):"));
        for b in &o.bad_ranges {
            let _ = writeln!(
                s,
                "{}",
                tf!(
                    "  LBA {:>12} – {:>12}  ({} secteur(s), {} tentatives) : {} — {}",
                    "  LBA {:>12} – {:>12}  ({} sector(s), {} attempts): {} — {}",
                    b.lba,
                    b.lba + b.sectors - 1,
                    b.sectors,
                    b.attempts,
                    b.error,
                    match b.outcome {
                        BadOutcome::ZeroFilled => t!("REMPLACÉ PAR DES ZÉROS", "REPLACED WITH ZEROS"),
                        BadOutcome::NotCopied => t!("NON COPIÉ", "NOT COPIED"),
                        BadOutcome::Pending => t!("non traité", "not handled"),
                    }
                )
            );
        }
    }
    if !o.recovered.is_empty() {
        let _ = writeln!(s, "{}", tf!("Secteurs lus après nouvelle tentative :", "Sectors read after a retry:"));
        for x in &o.recovered {
            let _ = writeln!(
                s,
                "{}",
                tf!(
                    "  LBA {:>12} – {:>12}  ({} secteur(s), réussite à la tentative {})",
                    "  LBA {:>12} – {:>12}  ({} sector(s), succeeded on attempt {})",
                    x.lba,
                    x.lba + x.sectors - 1,
                    x.sectors,
                    x.attempt
                )
            );
        }
    }

    let _ = writeln!(s, "{}", tf!("\n--- VÉRIFICATIONS {}", "\n--- CHECKS {}", "-".repeat(60)));
    let _ = writeln!(
        s,
        "{}",
        tf!(
            "Table de partitions : {}",
            "Partition table     : {}",
            if !o.table_check.performed {
                t!("non contrôlée", "not checked").to_string()
            } else if o.table_check.ok {
                "conforme".to_string()
            } else {
                tf!("NON CONFORME — {}", "NOT CONFORMING — {}", o.table_check.problems.join(" ; "))
            }
        )
    );
    let dc = &o.data_check;
    let _ = writeln!(
        s,
        "{}",
        tf!(
            "Données             : {}",
            "Data                : {}",
            match (dc.mode, dc.completed, dc.mismatches.len()) {
                (crate::engine::VerifyMode::None, _, _) =>
                    t!("non vérifiées (relecture désactivée ou clonage interrompu)", "not verified (read-back disabled or cloning interrupted)")
                        .to_string(),
                (_, false, _) =>
                    tf!("vérification interrompue ({} sur {} octets)", "verification interrupted ({} of {} bytes)", dc.checked_bytes, dc.total_bytes),
                (_, true, 0) => tf!(
                    "{} relus, aucune différence ({})",
                    "{} read back, no differences ({})",
                    human_size(dc.checked_bytes),
                    duration_text(dc.duration_ms)
                ),
                (_, true, n) => tf!("{n} DIFFÉRENCE(S) sur {} relus", "{n} DIFFERENCE(S) in {} read back", human_size(dc.checked_bytes)),
            }
        )
    );
    for m in &dc.mismatches {
        let _ = writeln!(s, "{}", tf!("  LBA {} (+{}) : {}", "  LBA {} (+{}): {}", m.lba, m.sectors, m.detail));
    }
    if let Some(g) = &o.grow {
        let _ = writeln!(s, "{}", tf!("Agrandissement      : {}", "Growth              : {}", grow_text(g)));
    }
    for (title, list) in [
        (t!("Systèmes de fichiers du clone", "Clone file systems"), &o.structure_findings),
        (t!("Démarrage (comparaison)", "Boot (comparison)"), &o.boot_findings),
    ] {
        if list.is_empty() {
            let _ = writeln!(s, "{}", tf!("{title:<20}: aucun écart", "{title:<20}: no difference"));
        } else {
            let _ = writeln!(s, "{}", tf!("{title} :", "{title}:"));
            write_findings_txt(&mut s, list);
        }
    }
    if let Some(b) = &o.boot_destination {
        let _ = writeln!(s, "{}", tf!("Éléments de démarrage du clone ({}) :", "Clone boot elements ({}):", b.style.label()));
        for c in &b.checks {
            let _ = writeln!(
                s,
                "  [{}] {}",
                match c.state {
                    crate::boot::CheckState::Present => t!("présent", "present"),
                    crate::boot::CheckState::Missing => t!("ABSENT ", "MISSING"),
                    crate::boot::CheckState::Unknown => t!("inconnu", "unknown"),
                },
                c.label
            );
        }
        let _ = writeln!(
            s,
            "{}",
            tf!(
                "  (La présence de ces éléments ne garantit pas le démarrage, qui dépend aussi du micrologiciel de la machine.)",
                "  (The presence of these elements does not guarantee booting, which also depends on the machine's firmware.)"
            )
        );
    }

    for (title, smart) in [
        (t!("SANTÉ DE LA SOURCE (SMART)", "SOURCE HEALTH (SMART)"), &r.smart_source),
        (t!("SANTÉ DE LA DESTINATION (SMART)", "DESTINATION HEALTH (SMART)"), &r.smart_destination),
    ] {
        let _ = writeln!(s, "\n--- {title} {}", "-".repeat(72usize.saturating_sub(title.len())));
        match smart {
            None => {
                let _ = writeln!(s, "{}", tf!("Non relevée.", "Not read."));
            }
            Some(sm) if !sm.available => {
                let _ = writeln!(s, "{}", tf!("Indisponible : {}", "Unavailable: {}", sm.unavailable_reason.clone().unwrap_or_default()));
            }
            Some(sm) => {
                let _ = writeln!(
                    s,
                    "{}",
                    tf!(
                        "Indicateur global : {} (estimation fondée sur les compteurs du disque)",
                        "Overall indicator : {} (estimate based on the disk's counters)",
                        sm.level.label()
                    )
                );
                if let Some(t) = sm.temperature_c {
                    let _ = writeln!(s, "{}", tf!("Température       : {t} °C", "Temperature       : {t} °C"));
                }
                if let Some(h) = sm.power_on_hours {
                    let _ = writeln!(s, "{}", tf!("Fonctionnement    : {h} heures", "Power-on time     : {h} hours"));
                }
                if let Some(w) = sm.wear_percent_used {
                    let _ = writeln!(s, "{}", tf!("Usure estimée     : {w} %", "Estimated wear    : {w} %"));
                }
                write_findings_txt(&mut s, &sm.findings);
            }
        }
    }

    let _ = writeln!(s, "{}", tf!("\n--- PRÉ-VÉRIFICATIONS {}", "\n--- PRE-CHECKS {}", "-".repeat(56)));
    if r.precheck.is_empty() {
        let _ = writeln!(s, "{}", tf!("Aucun constat.", "No findings."));
    }
    write_findings_txt(&mut s, &r.precheck);

    let _ = writeln!(s, "{}", tf!("\n--- JOURNAL {}", "\n--- LOG {}", "-".repeat(66)));
    for e in &r.log {
        let _ = writeln!(s, "{} [{}] {}", e.time, level_tag(e.level), e.message);
    }
    let _ = writeln!(
        s,
        "{}",
        tf!(
            "\n{line}\nRapport généré localement par {} {}. Aucune donnée n'est transmise.",
            "\n{line}\nReport generated locally by {} {}. No data is sent anywhere.",
            r.app,
            r.version
        )
    );
    s
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { s.chars().take(n - 1).chain(std::iter::once('…')).collect() }
}

fn write_findings_txt(s: &mut String, list: &[Finding]) {
    for f in list {
        let _ = writeln!(
            s,
            "  [{}{}] {}{}",
            f.severity.label(),
            if f.blocking { t!(", bloquant", ", blocking") } else { "" },
            f.title,
            f.scope.as_ref().map(|x| format!(" ({x})")).unwrap_or_default()
        );
        let _ = writeln!(s, "      {}", f.detail);
    }
}

// ---------------------------------------------------------------------------
// HTML
// ---------------------------------------------------------------------------

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn sev_class(s: Severity) -> &'static str {
    match s {
        Severity::Info => "info",
        Severity::Warning => "warn",
        Severity::Error => "err",
        Severity::Critical => "crit",
    }
}

fn findings_html(list: &[Finding]) -> String {
    if list.is_empty() {
        return t!("<p class=\"muted\">Aucun constat.</p>", "<p class=\"muted\">No findings.</p>").into();
    }
    let mut h = String::from("<ul class=\"findings\">");
    for f in list {
        let _ = write!(
            h,
            "<li class=\"{}\"><span class=\"tag\">{}{}</span><strong>{}</strong>{}<div>{}</div></li>",
            sev_class(f.severity),
            f.severity.label(),
            if f.blocking { t!(" · bloquant", " · blocking") } else { "" },
            esc(&f.title),
            f.scope.as_ref().map(|x| format!(" <em>({})</em>", esc(x))).unwrap_or_default(),
            esc(&f.detail)
        );
    }
    h.push_str("</ul>");
    h
}

pub fn render_html(r: &Report) -> String {
    let o = &r.outcome;
    let status_class = match o.status {
        FinalStatus::SuccessVerified => "ok",
        FinalStatus::CompletedWithWarnings => "warn",
        FinalStatus::Incomplete => "err",
        FinalStatus::Failed => "crit",
    };
    let mut h = String::new();
    let _ = write!(
        h,
        r#"<!doctype html><html lang="{lang}"><head><meta charset="utf-8"><title>{report_word} {id} — {app}</title>
<style>
:root{{--bg:#05060d;--panel:#0d1029;--line:rgba(146,170,255,.2);--text:#e8ecff;--text2:#a7b0d4;--cyan:#4fe3ff;--ok:#54f0b6;--warn:#ffc56b;--err:#ff8a5c;--crit:#ff4f82}}
*{{box-sizing:border-box}}body{{margin:0;background:radial-gradient(900px 600px at 85% 0%,#1b1140 0%,transparent 65%),var(--bg);color:var(--text);font:14px/1.55 "Segoe UI",system-ui,sans-serif;padding:32px}}
main{{max-width:980px;margin:0 auto}}h1{{font-size:22px;margin:0 0 4px}}h2{{font-size:15px;margin:28px 0 10px;color:var(--cyan);letter-spacing:.04em;text-transform:uppercase}}
.muted{{color:var(--text2)}}section,.card{{background:var(--panel);border:1px solid var(--line);border-radius:14px;padding:16px 18px}}
.status{{border-radius:14px;padding:18px 20px;margin:18px 0;border:1px solid}}.status h1{{font-size:20px}}
.status.ok{{border-color:var(--ok);background:rgba(84,240,182,.08)}}.status.warn{{border-color:var(--warn);background:rgba(255,197,107,.08)}}
.status.err{{border-color:var(--err);background:rgba(255,138,92,.08)}}.status.crit{{border-color:var(--crit);background:rgba(255,79,130,.1)}}
.grid{{display:grid;grid-template-columns:1fr 1fr;gap:14px}}dl{{display:grid;grid-template-columns:150px 1fr;gap:4px 12px;margin:0}}dt{{color:var(--text2)}}dd{{margin:0;word-break:break-word}}
table{{width:100%;border-collapse:collapse;font-size:13px}}th,td{{text-align:left;padding:6px 8px;border-bottom:1px solid var(--line)}}th{{color:var(--text2);font-weight:600}}
td.num{{text-align:right;font-variant-numeric:tabular-nums}}code,.mono{{font-family:Consolas,"Cascadia Mono",monospace;font-size:12.5px}}
ul.findings{{list-style:none;padding:0;margin:0;display:grid;gap:8px}}ul.findings li{{border-left:3px solid;padding:6px 12px;background:rgba(255,255,255,.03);border-radius:0 8px 8px 0}}
ul.findings li div{{color:var(--text2);margin-top:2px}}.tag{{font-size:11px;text-transform:uppercase;letter-spacing:.06em;margin-right:8px}}
li.info{{border-color:var(--cyan)}}li.warn{{border-color:var(--warn)}}li.err{{border-color:var(--err)}}li.crit{{border-color:var(--crit)}}
li.info .tag{{color:var(--cyan)}}li.warn .tag{{color:var(--warn)}}li.err .tag{{color:var(--err)}}li.crit .tag{{color:var(--crit)}}
pre{{background:#03040b;border:1px solid var(--line);border-radius:10px;padding:12px;overflow:auto;font-size:12px;max-height:420px}}
.lw{{color:var(--warn)}}.le{{color:var(--crit)}}
@media print{{body{{background:#fff;color:#111;padding:0}}section,.card{{background:#fff;border-color:#bbb}}h2{{color:#124}}.muted,dt,th,ul.findings li div{{color:#444}}pre{{background:#f4f4f4;max-height:none}}}}
</style></head><body><main>
<p class="muted">{app} {version} — {report_lc} {id}</p>
<div class="status {status_class}"><h1>{status}</h1><ul>"#,
        id = esc(&r.id),
        app = esc(&r.app),
        version = esc(&r.version),
        status = o.status.label(),
        lang = crate::i18n::lang().code(),
        report_word = t!("Rapport", "Report"),
        report_lc = t!("rapport", "report"),
    );
    for reason in &o.reasons {
        let _ = write!(h, "<li>{}</li>", esc(reason));
    }
    let _ = write!(
        h,
        "{}",
        tf!(
            "</ul><p class=\"muted\">Début : {} · Fin : {} · Durée : {}</p></div>",
            "</ul><p class=\"muted\">Started: {} · Finished: {} · Duration: {}</p></div>",
            esc(&r.started_at),
            esc(&r.finished_at),
            duration_text(o.duration_ms)
        )
    );

    h.push_str("<div class=\"grid\">");
    for (title, d) in [(t!("Disque source", "Source disk"), &r.source), (t!("Disque de destination", "Destination disk"), &r.destination)] {
        let _ = write!(h, "<section><h2 style=\"margin-top:0\">{title}</h2><dl>");
        for (k, v) in disk_lines(d) {
            let _ = write!(h, "<dt>{}</dt><dd>{}</dd>", esc(&k), esc(&v));
        }
        h.push_str("</dl></section>");
    }
    h.push_str("</div>");

    let _ = write!(
        h,
        "{}",
        tf!(
            "<h2>Méthode</h2><section><dl><dt>Mode de clonage</dt><dd>{}</dd><dt>Vérification</dt><dd>{}</dd><dt>Données planifiées</dt><dd>{}</dd><dt>Données copiées</dt><dd>{} ({} octets)</dd><dt>Débit moyen</dt><dd>{}/s</dd></dl></section>",
            "<h2>Method</h2><section><dl><dt>Clone mode</dt><dd>{}</dd><dt>Verification</dt><dd>{}</dd><dt>Planned data</dt><dd>{}</dd><dt>Copied data</dt><dd>{} ({} bytes)</dd><dt>Average speed</dt><dd>{}/s</dd></dl></section>",
            r.plan.mode.label(),
            r.engine_options.verify.label(),
            human_size(r.plan.total_copy_bytes),
            human_size(o.copy.bytes_copied),
            o.copy.bytes_copied,
            human_size(o.copy.avg_bps)
        )
    );

    h.push_str(t!("<h2>Partitions</h2><section><table><tr><th>N°</th><th>Type</th><th>Système</th><th>Taille</th><th>Utilisé</th><th>Copie</th><th>Position</th></tr>", "<h2>Partitions</h2><section><table><tr><th>No.</th><th>Type</th><th>File system</th><th>Size</th><th>Used</th><th>Copy</th><th>Position</th></tr>"));
    for p in &r.plan.partitions {
        let _ = write!(
            h,
            "<tr><td>{}</td><td>{}{}</td><td>{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td>{}{}</td><td>{}{}</td></tr>",
            p.number,
            esc(&p.type_label),
            p.fs_label
                .as_ref()
                .map(|l| tf!(" <span class=\"muted\">« {} »</span>", " <span class=\"muted\">“{}”</span>", esc(l)))
                .unwrap_or_default(),
            p.fs.label(),
            human_size(p.src_len),
            p.used_bytes.map(human_size).unwrap_or_else(|| "—".into()),
            method_label(p.method),
            match (&p.method_reason, p.method) {
                (Some(reason), CopyMethod::Raw) => format!("<div class=\"muted\">{}</div>", esc(reason)),
                _ => String::new(),
            },
            if p.moved { tf!("déplacée → {}", "moved → {}", human_size(p.dst_start)) } else { t!("inchangée", "unchanged").to_string() },
            if p.dst_len != p.src_len {
                tf!("<div class=\"muted\">agrandie à {}</div>", "<div class=\"muted\">grown to {}</div>", human_size(p.dst_len))
            } else {
                String::new()
            }
        );
    }
    h.push_str("</table></section>");

    let _ = write!(
        h,
        "{}",
        tf!(
            "<h2>Erreurs de lecture et d'écriture</h2><section><dl><dt>Erreurs de lecture</dt><dd>{} événement(s)</dd><dt>Erreurs d'écriture</dt><dd>{}</dd><dt>Récupéré après reprise</dt><dd>{} octets</dd><dt>Remplacé par des zéros</dt><dd>{} octets</dd></dl>",
            "<h2>Read and write errors</h2><section><dl><dt>Read errors</dt><dd>{} event(s)</dd><dt>Write errors</dt><dd>{}</dd><dt>Recovered after retry</dt><dd>{} bytes</dd><dt>Replaced with zeros</dt><dd>{} bytes</dd></dl>",
            o.copy.read_error_events,
            o.copy.write_error_events,
            o.copy.recovered_bytes,
            o.copy.bytes_zero_filled
        )
    );
    if o.bad_ranges.is_empty() {
        h.push_str(t!("<p class=\"muted\">Aucun secteur illisible.</p>", "<p class=\"muted\">No unreadable sectors.</p>"));
    } else {
        h.push_str(t!(
            "<table><tr><th>LBA (source)</th><th>Secteurs</th><th>Tentatives</th><th>Erreur</th><th>Traitement</th></tr>",
            "<table><tr><th>LBA (source)</th><th>Sectors</th><th>Attempts</th><th>Error</th><th>Handling</th></tr>"
        ));
        for b in &o.bad_ranges {
            let _ = write!(
                h,
                "<tr><td class=\"mono\">{} – {}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td>{}</td><td>{}</td></tr>",
                b.lba,
                b.lba + b.sectors - 1,
                b.sectors,
                b.attempts,
                esc(&b.error),
                match b.outcome {
                    BadOutcome::ZeroFilled => t!("Remplacé par des zéros", "Replaced with zeros"),
                    BadOutcome::NotCopied => t!("Non copié", "Not copied"),
                    BadOutcome::Pending => t!("Non traité", "Not handled"),
                }
            );
        }
        h.push_str("</table>");
    }
    if !o.recovered.is_empty() {
        h.push_str(t!(
            "<p>Secteurs lus après nouvelle tentative :</p><table><tr><th>LBA</th><th>Secteurs</th><th>Tentative réussie</th></tr>",
            "<p>Sectors read after a retry:</p><table><tr><th>LBA</th><th>Sectors</th><th>Successful attempt</th></tr>"
        ));
        for x in &o.recovered {
            let _ = write!(
                h,
                "<tr><td class=\"mono\">{} – {}</td><td class=\"num\">{}</td><td class=\"num\">{}</td></tr>",
                x.lba,
                x.lba + x.sectors - 1,
                x.sectors,
                x.attempt
            );
        }
        h.push_str("</table>");
    }
    h.push_str("</section>");

    let dc = &o.data_check;
    let data_text = match (dc.mode, dc.completed, dc.mismatches.len()) {
        (crate::engine::VerifyMode::None, _, _) => t!("Non vérifiées", "Not verified").to_string(),
        (_, false, _) => tf!(
            "Vérification interrompue ({} sur {})",
            "Verification interrupted ({} of {})",
            human_size(dc.checked_bytes),
            human_size(dc.total_bytes)
        ),
        (_, true, 0) => tf!("{} relus, aucune différence", "{} read back, no differences", human_size(dc.checked_bytes)),
        (_, true, n) => tf!("{n} différence(s) sur {} relus", "{n} difference(s) in {} read back", human_size(dc.checked_bytes)),
    };
    let _ = write!(
        h,
        "{}",
        tf!(
            "<h2>Vérifications</h2><section><dl><dt>Table de partitions</dt><dd>{}</dd><dt>Données</dt><dd>{}</dd>{}</dl>",
            "<h2>Checks</h2><section><dl><dt>Partition table</dt><dd>{}</dd><dt>Data</dt><dd>{}</dd>{}</dl>",
            if !o.table_check.performed {
                t!("Non contrôlée", "Not checked").to_string()
            } else if o.table_check.ok {
                t!("Conforme", "Conforming").to_string()
            } else {
                tf!("Non conforme — {}", "Not conforming — {}", esc(&o.table_check.problems.join(" ; ")))
            },
            esc(&data_text),
            o.grow.as_ref().map(|g| tf!("<dt>Agrandissement</dt><dd>{}</dd>", "<dt>Growth</dt><dd>{}</dd>", esc(&grow_text(g)))).unwrap_or_default()
        )
    );
    if !dc.mismatches.is_empty() {
        h.push_str(t!(
            "<table><tr><th>LBA</th><th>Secteurs</th><th>Écart</th></tr>",
            "<table><tr><th>LBA</th><th>Sectors</th><th>Difference</th></tr>"
        ));
        for m in &dc.mismatches {
            let _ = write!(h, "<tr><td class=\"mono\">{}</td><td class=\"num\">{}</td><td>{}</td></tr>", m.lba, m.sectors, esc(&m.detail));
        }
        h.push_str("</table>");
    }
    if !o.structure_findings.is_empty() {
        let _ =
            write!(h, "{}", tf!("<p>Systèmes de fichiers du clone :</p>{}", "<p>Clone file systems:</p>{}", findings_html(&o.structure_findings)));
    }
    if !o.boot_findings.is_empty() {
        let _ = write!(h, "{}", tf!("<p>Démarrage :</p>{}", "<p>Boot:</p>{}", findings_html(&o.boot_findings)));
    }
    if let Some(b) = &o.boot_destination {
        let _ = write!(
            h,
            "{}",
            tf!(
                "<p>Éléments de démarrage relevés sur le clone ({}) :</p><table>",
                "<p>Boot elements found on the clone ({}):</p><table>",
                b.style.label()
            )
        );
        for c in &b.checks {
            let _ = write!(
                h,
                "<tr><td>{}</td><td>{}</td></tr>",
                esc(&c.label),
                match c.state {
                    crate::boot::CheckState::Present => t!("présent", "present"),
                    crate::boot::CheckState::Missing => t!("<strong>absent</strong>", "<strong>missing</strong>"),
                    crate::boot::CheckState::Unknown => t!("non vérifiable", "not verifiable"),
                }
            );
        }
        h.push_str(t!("</table><p class=\"muted\">La présence de ces éléments ne garantit pas le démarrage, qui dépend aussi du micrologiciel et des réglages de la machine.</p>", "</table><p class=\"muted\">The presence of these elements does not guarantee booting, which also depends on the machine's firmware and settings.</p>"));
    }
    h.push_str("</section>");

    for (title, smart) in [
        (t!("Santé de la source (SMART)", "Source health (SMART)"), &r.smart_source),
        (t!("Santé de la destination (SMART)", "Destination health (SMART)"), &r.smart_destination),
    ] {
        let _ = write!(h, "<h2>{title}</h2><section>");
        match smart {
            None => h.push_str(t!("<p class=\"muted\">Non relevée.</p>", "<p class=\"muted\">Not read.</p>")),
            Some(sm) if !sm.available => {
                let _ = write!(
                    h,
                    "{}",
                    tf!(
                        "<p class=\"muted\">Indisponible : {}</p>",
                        "<p class=\"muted\">Unavailable: {}</p>",
                        esc(&sm.unavailable_reason.clone().unwrap_or_default())
                    )
                );
            }
            Some(sm) => {
                let _ = write!(
                    h,
                    "{}",
                    tf!(
                        "<dl><dt>Indicateur global</dt><dd>{} <span class=\"muted\">(estimation fondée sur les compteurs du disque)</span></dd>",
                        "<dl><dt>Overall indicator</dt><dd>{} <span class=\"muted\">(estimate based on the disk's counters)</span></dd>",
                        sm.level.label()
                    )
                );
                if let Some(t) = sm.temperature_c {
                    let _ = write!(h, "{}", tf!("<dt>Température</dt><dd>{t} °C</dd>", "<dt>Temperature</dt><dd>{t} °C</dd>"));
                }
                if let Some(x) = sm.power_on_hours {
                    let _ = write!(h, "{}", tf!("<dt>Fonctionnement</dt><dd>{x} heures</dd>", "<dt>Power-on time</dt><dd>{x} hours</dd>"));
                }
                if let Some(w) = sm.wear_percent_used {
                    let _ = write!(h, "{}", tf!("<dt>Usure estimée</dt><dd>{w} %</dd>", "<dt>Estimated wear</dt><dd>{w} %</dd>"));
                }
                h.push_str("</dl>");
                if !sm.findings.is_empty() {
                    h.push_str(&findings_html(&sm.findings));
                }
            }
        }
        h.push_str("</section>");
    }

    let _ = write!(
        h,
        "{}",
        tf!("<h2>Pré-vérifications</h2><section>{}</section>", "<h2>Pre-checks</h2><section>{}</section>", findings_html(&r.precheck))
    );

    h.push_str(t!("<h2>Journal</h2><pre>", "<h2>Log</h2><pre>"));
    for e in &r.log {
        let cls = match e.level {
            LogLevel::Warning => " class=\"lw\"",
            LogLevel::Error => " class=\"le\"",
            _ => "",
        };
        let _ = writeln!(h, "<span{cls}>{} [{}] {}</span>", e.time, level_tag(e.level), esc(&e.message));
    }
    let _ = write!(
        h,
        "{}",
        tf!(
            "</pre><p class=\"muted\">Rapport généré localement par {} {}. Aucune donnée n'est transmise.</p></main></body></html>",
            "</pre><p class=\"muted\">Report generated locally by {} {}. No data is sent anywhere.</p></main></body></html>",
            esc(&r.app),
            esc(&r.version)
        )
    );
    h
}

/// Summary of a growth for the reports.
fn grow_text(g: &crate::clone::GrowOutcome) -> String {
    match (&g.summary, g.done, &g.error) {
        (Some(x), true, _) => tf!(
            "partition {} : {} → {}, structures relues et contrôlées ; volume marqué « à vérifier » (chkdsk au premier démarrage)",
            "partition {}: {} → {}, structures read back and checked; volume flagged \"needs checking\" (chkdsk on first boot)",
            g.number,
            human_size(x.old_size),
            human_size(x.new_size)
        ),
        (_, _, Some(e)) if g.modified => {
            tf!("partition {} : ÉCHEC après modification du volume — {e}", "partition {}: FAILED after modifying the volume — {e}", g.number)
        }
        (_, _, Some(e)) => tf!("partition {} : non effectué — {e}", "partition {}: not done — {e}", g.number),
        _ => tf!("partition {} : non effectué", "partition {}: not done", g.number),
    }
}
