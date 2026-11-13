//! Déroulement complet d'une opération, partagé par l'interface graphique et la ligne
//! de commande : description des disques, préparation (lecture seule), puis exécution
//! avec revalidation de tout ce qui a été montré à l'utilisateur.

use crate::boot::{BootAssessment, assess_boot};
use crate::clone::{CloneRequest, run_clone};
use crate::device::{BlockDevice, FileDevice, ReadOnly};
use crate::diag::{Finding, has_blocking};
use crate::engine::{Control, EngineOptions, LogLevel, Observer};
use crate::image::image_info;
use crate::model::{DiskInfo, DiskKind, MediaKind, fingerprint};
use crate::partition::TableKind;
use crate::plan::{ClonePlan, DiskAnalysis, PlanOptions, analyze_disk, build_plan};
use crate::precheck::precheck;
use crate::report::{Journal, Report, new_report_id, now_string, save_report};
use crate::smart::SmartReport;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Désignation d'un disque : numéro Windows ou fichier image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Target {
    Disk(u32),
    Image(PathBuf),
}

impl Target {
    /// `disk:3` ou chemin de fichier.
    pub fn parse(s: &str) -> Result<Self, String> {
        let t = s.trim();
        if let Some(n) = t.strip_prefix("disk:").or_else(|| t.strip_prefix("disque:")) {
            return n.parse::<u32>().map(Target::Disk).map_err(|_| tf!("numéro de disque invalide : {n}", "invalid disk number: {n}"));
        }
        if let Some(p) = t.strip_prefix("image:") {
            return Ok(Target::Image(PathBuf::from(p)));
        }
        if t.is_empty() {
            return Err(t!("cible vide", "empty target").into());
        }
        Ok(Target::Image(PathBuf::from(t)))
    }

    pub fn id(&self) -> String {
        match self {
            Target::Disk(n) => format!("disk:{n}"),
            Target::Image(p) => format!("image:{}", p.to_string_lossy()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobConfig {
    pub source: Target,
    pub destination: Target,
    pub plan: PlanOptions,
    pub engine: EngineOptions,
    /// Taille de l'image de destination à créer si le fichier n'existe pas
    /// (`None` : même taille que la source).
    pub new_image_size: Option<u64>,
    /// Accord explicite pour écrire sur un disque physique réel alors que cette voie
    /// n'est pas encore validée (voir `win::PHYSICAL_WRITE_VALIDATED`).
    pub allow_unvalidated_physical_write: bool,
    /// Empreintes relevées au moment de la sélection ; comparées avant d'écrire.
    pub expected_source_fingerprint: Option<String>,
    pub expected_destination_fingerprint: Option<String>,
    /// Dossiers du programme à protéger (jamais sur la destination).
    pub app_paths: Vec<String>,
}

/// Tout ce qui est présenté à l'utilisateur avant confirmation. Lecture seule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preparation {
    pub source: DiskInfo,
    pub destination: DiskInfo,
    pub analysis: DiskAnalysis,
    pub plan: ClonePlan,
    pub findings: Vec<Finding>,
    pub blocked: bool,
    pub boot: BootAssessment,
    pub smart_source: Option<SmartReport>,
    pub smart_destination: Option<SmartReport>,
}

#[derive(Debug)]
pub enum JobError {
    /// Les pré-vérifications interdisent l'opération.
    Blocked(Vec<Finding>),
    Failed(String),
}

impl std::fmt::Display for JobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JobError::Blocked(list) => {
                write!(f, "{}", tf!("opération refusée par les pré-vérifications", "operation refused by the pre-checks"))?;
                for x in list.iter().filter(|x| x.blocking) {
                    write!(f, "\n  - {} : {}", x.title, x.detail)?;
                }
                Ok(())
            }
            JobError::Failed(m) => f.write_str(m),
        }
    }
}

fn fail(e: impl std::fmt::Display) -> JobError {
    JobError::Failed(e.to_string())
}

/// Identifiant stable d'un fichier image : chemin canonique, sans préfixe `\\?\`, en
/// minuscules. Deux désignations du même fichier (relative, absolue, avant et après sa
/// création) donnent le même identifiant : c'est ce qui permet de refuser une source
/// et une destination identiques.
pub fn image_id(path: &Path) -> String {
    let canon = std::fs::canonicalize(path).ok().or_else(|| {
        // Fichier pas encore créé : on canonise le dossier parent.
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        std::fs::canonicalize(parent).ok().and_then(|p| path.file_name().map(|n| p.join(n)))
    });
    let text = canon.unwrap_or_else(|| path.to_path_buf()).to_string_lossy().to_string();
    let text = text.strip_prefix(r"\\?\UNC\").map(|s| format!(r"\\{s}")).unwrap_or_else(|| text.strip_prefix(r"\\?\").unwrap_or(&text).to_string());
    format!("image:{}", text.to_lowercase())
}

/// Décrit une cible. Pour une image de destination inexistante, renvoie une
/// description « à créer » de la taille prévue.
pub fn describe(target: &Target, deep: bool, app_paths: &[String], missing_image_size: Option<u64>) -> Result<DiskInfo, String> {
    match target {
        Target::Image(path) => {
            let _ = app_paths;
            if path.exists() {
                let (mut info, _) =
                    image_info(path, 512).map_err(|e| tf!("image illisible ({}) : {e}", "unreadable image ({}): {e}", path.display()))?;
                info.id = image_id(path);
                Ok(info)
            } else if let Some(size) = missing_image_size {
                let full = path.to_string_lossy().to_string();
                Ok(DiskInfo {
                    id: image_id(path),
                    kind: DiskKind::Image,
                    path: full.clone(),
                    number: None,
                    model: tf!("{} (à créer)", "{} (to be created)", path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()),
                    vendor: String::new(),
                    serial: String::new(),
                    firmware: String::new(),
                    bus: t!("Fichier", "File").into(),
                    media: MediaKind::Image,
                    size: size - size % 512,
                    sector_size: 512,
                    physical_sector_size: 512,
                    removable: false,
                    read_only: false,
                    offline: false,
                    is_system: false,
                    hosts_app: false,
                    table: TableKind::None,
                    partitions: vec![],
                    used_bytes: None,
                    fingerprint: fingerprint(&full, "", size - size % 512, "Fichier", 512),
                    deep: true,
                    notes: vec![t!("Le fichier sera créé.", "The file will be created.").into()],
                })
            } else {
                Err(tf!("fichier image introuvable : {}", "image file not found: {}", path.display()))
            }
        }
        Target::Disk(n) => describe_disk(*n, deep, app_paths),
    }
}

#[cfg(windows)]
fn describe_disk(n: u32, deep: bool, app_paths: &[String]) -> Result<DiskInfo, String> {
    crate::win::enumerate::disk_by_number(n, deep, app_paths)
        .ok_or_else(|| tf!("le disque {n} n'existe pas (ou plus)", "disk {n} does not exist (any more)"))
}
#[cfg(not(windows))]
fn describe_disk(_n: u32, _deep: bool, _app_paths: &[String]) -> Result<DiskInfo, String> {
    Err(t!("les disques physiques ne sont pris en charge que sous Windows", "physical disks are only supported on Windows").into())
}

/// Liste des disques visibles. Lecture seule.
pub fn list_disks(deep: bool, app_paths: &[String]) -> Vec<DiskInfo> {
    #[cfg(windows)]
    {
        crate::win::enumerate::list_disks(deep, app_paths)
    }
    #[cfg(not(windows))]
    {
        let _ = (deep, app_paths);
        Vec::new()
    }
}

/// Lit l'état de santé d'un disque (lecture seule).
pub fn read_smart(info: &DiskInfo) -> SmartReport {
    #[cfg(windows)]
    {
        crate::win::smart_io::read_smart(info)
    }
    #[cfg(not(windows))]
    {
        let _ = info;
        SmartReport::unavailable(t!("non pris en charge sur cette plateforme", "not supported on this platform"))
    }
}

pub fn is_elevated() -> bool {
    #[cfg(windows)]
    {
        crate::win::is_elevated()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Ouvre une cible en lecture seule.
pub fn open_read_only(target: &Target) -> Result<Box<dyn BlockDevice>, String> {
    match target {
        Target::Image(p) => FileDevice::open_read_only(p, 512)
            .map(|d| Box::new(d) as Box<dyn BlockDevice>)
            .map_err(|e| tf!("ouverture de l'image impossible : {e}", "cannot open the image: {e}")),
        Target::Disk(n) => {
            #[cfg(windows)]
            {
                if !crate::win::is_elevated() {
                    return Err(t!(
                        "les droits administrateur sont nécessaires pour lire directement un disque",
                        "administrator rights are required to read a disk directly"
                    )
                    .into());
                }
                crate::win::rawdisk::RawDisk::open_read_only(&format!(r"\\.\PhysicalDrive{n}"))
                    .map(|d| Box::new(d) as Box<dyn BlockDevice>)
                    .map_err(|e| tf!("ouverture du disque {n} impossible : {e}", "cannot open disk {n}: {e}"))
            }
            #[cfg(not(windows))]
            {
                let _ = n;
                Err(t!("non pris en charge sur cette plateforme", "not supported on this platform").into())
            }
        }
    }
}

/// Analyse une cible (table de partitions, systèmes de fichiers, démarrage). Lecture seule.
pub fn inspect(target: &Target) -> Result<(DiskAnalysis, BootAssessment), String> {
    let mut dev = ReadOnly(open_read_only(target)?);
    let analysis = analyze_disk(&mut dev).map_err(|e| tf!("analyse impossible : {e}", "analysis failed: {e}"))?;
    let boot = assess_boot(&mut dev, &analysis);
    Ok((analysis, boot))
}

/// Scan de surface d'une cible, en lecture seule (le périphérique est enveloppé dans
/// `ReadOnly`, qui refuse toute écriture).
pub fn surface_scan(
    target: &Target,
    app_paths: &[String],
    opts: &crate::scan::ScanOptions,
    ctl: &Control,
    obs: &dyn crate::scan::ScanObserver,
) -> Result<(DiskInfo, crate::scan::ScanResult), String> {
    let info = describe(target, false, app_paths, None)?;
    let mut dev = ReadOnly(open_read_only(target)?);
    let result = crate::scan::scan(&mut dev, opts, ctl, obs);
    Ok((info, result))
}

/// Prépare un clonage sans rien écrire : analyse de la source, plan, pré-vérifications.
pub fn prepare(cfg: &JobConfig, with_smart: bool) -> Result<Preparation, String> {
    let source = describe(&cfg.source, true, &cfg.app_paths, None)?;
    // Disque externe dont les volumes sont montés (un Windows branché en USB, par
    // exemple) : un volume NTFS monté a un journal « en cours d'utilisation », ce qui
    // fausserait l'aperçu (copie intégrale, pas d'agrandissement). Comme au lancement,
    // on le verrouille et le démonte le temps de l'analyse — une seule tentative, sans
    // rien écrire. Si un programme l'utilise, l'aperçu se fait volume monté.
    #[cfg(windows)]
    let mut in_use = false;
    #[cfg(windows)]
    let locks = match cfg.source {
        Target::Disk(_) if !source.is_system && source.has_mounted_volumes() => {
            match crate::win::rawdisk::try_lock_disk_volumes(&volumes_for_lock(&source)) {
                Ok(l) => l,
                Err(_) => {
                    in_use = true;
                    Vec::new()
                }
            }
        }
        _ => Vec::new(),
    };
    let (analysis, boot) = inspect(&cfg.source)?;
    #[cfg(windows)]
    drop(locks);
    let wanted_size = cfg.new_image_size.unwrap_or(analysis.size);
    let destination = describe(&cfg.destination, true, &cfg.app_paths, Some(wanted_size))?;
    let plan = build_plan(&analysis, destination.size, destination.sector_size, &cfg.plan);
    let mut findings = precheck(&source, &destination, Some(&analysis), Some(&plan));
    for f in &boot.findings {
        if !findings.iter().any(|x| x.code == f.code && x.title == f.title) {
            findings.push(f.clone().scope("source"));
        }
    }
    #[cfg(windows)]
    if in_use {
        findings.push(
            Finding::info(
                "SOURCE_IN_USE",
                t!("Volumes de la source en cours d'utilisation", "Source volumes in use"),
                t!("Un programme ou une fenêtre de l'Explorateur utilise un volume de la source : l'aperçu a été fait volume monté \
                 et peut prévoir une copie intégrale là où une copie intelligente sera possible. Au lancement, les volumes \
                 seront verrouillés et démontés, puis la source réanalysée. Fermez ce qui les utilise avant de lancer.", "A program or File Explorer window is using a source volume: the preview was made with the volume mounted and may plan a full copy where a smart copy will be possible. At launch, the volumes will be locked and dismounted, then the source analysed again. Close whatever is using them before starting."),
            )
            .scope("source"),
        );
    }
    let (smart_source, smart_destination) = if with_smart { (Some(read_smart(&source)), Some(read_smart(&destination))) } else { (None, None) };
    for (who, s) in [("source", &smart_source), ("destination", &smart_destination)] {
        if let Some(s) = s {
            for f in &s.findings {
                findings.push(f.clone().scope(who));
            }
        }
    }
    if let Some(msg) = physical_write_gate(&destination, cfg.allow_unvalidated_physical_write) {
        findings.push(
            Finding::error(
                "PHYSICAL_WRITE_UNVALIDATED",
                t!("Écriture sur disque physique non encore validée", "Writing to a physical disk not validated yet"),
                msg,
            )
            .scope("destination")
            .blocking(),
        );
    }
    findings.sort_by(|a, b| b.blocking.cmp(&a.blocking).then(b.severity.cmp(&a.severity)));
    let blocked = has_blocking(&findings);
    Ok(Preparation { source, destination, analysis, plan, findings, blocked, boot, smart_source, smart_destination })
}

fn physical_write_gate(dst: &DiskInfo, allowed: bool) -> Option<String> {
    #[cfg(windows)]
    {
        crate::win::rawdisk::check_write_allowed(dst, allowed).err().map(|e| {
            tf!("{e} Dans cette version, le moteur a été validé sur des images disque et des fichiers ; l'écriture sur un vrai \
                 disque n'a pas encore été éprouvée sur du matériel. Utilisez d'abord le script de validation sur disques \
                 virtuels (scripts\\validate-vhd.ps1).", "{e} In this version the engine has been validated on disk images and files; writing to a real disk has not yet been proven on hardware. First use the virtual disk validation script (scripts\\validate-vhd.ps1)."
            )
        })
    }
    #[cfg(not(windows))]
    {
        let _ = (dst, allowed);
        None
    }
}

struct Opened {
    src: Box<dyn BlockDevice>,
    dst: Box<dyn BlockDevice>,
    #[cfg(windows)]
    _locks: Vec<crate::win::rawdisk::VolumeLock>,
    #[cfg(windows)]
    dst_number: Option<u32>,
}

fn volumes_for_lock(info: &DiskInfo) -> Vec<(String, String)> {
    info.partitions
        .iter()
        .filter_map(|p| p.volume_path.clone().map(|v| (v, p.mounts.first().cloned().unwrap_or_else(|| format!("partition {}", p.number)))))
        .collect()
}

/// Exécute le clonage. Tout est revérifié ici : identité des disques, analyse, plan
/// et pré-vérifications, quelle que soit la préparation affichée auparavant.
///
/// OPÉRATION DESTRUCTRICE pour la destination : l'appelant doit avoir obtenu la
/// confirmation explicite de l'utilisateur.
pub fn execute(cfg: &JobConfig, reports_dir: &Path, ctl: &Control, obs: &dyn Observer) -> Result<Report, JobError> {
    let started_at = now_string();
    let id = new_report_id();
    std::fs::create_dir_all(reports_dir)
        .map_err(|e| fail(tf!("dossier des rapports inaccessible ({}) : {e}", "reports folder inaccessible ({}): {e}", reports_dir.display())))?;
    let journal = Journal::new(obs, Some(&reports_dir.join(format!("{id}.log"))));
    journal.note(LogLevel::Info, &tf!("{} {} — rapport {id}", "{} {} — report {id}", crate::APP_NAME, crate::APP_VERSION));

    // 1. Identité des disques, relue maintenant.
    let source = describe(&cfg.source, true, &cfg.app_paths, None).map_err(fail)?;
    if let Some(fp) = &cfg.expected_source_fingerprint
        && *fp != source.fingerprint
    {
        return Err(fail(t!(
            "le disque source n'est plus celui qui avait été sélectionné (modèle, numéro de série ou taille différents). Opération annulée, rien n'a été écrit.",
            "the source disk is no longer the one that was selected (different model, serial number or size). Operation cancelled, nothing was written."
        )));
    }
    journal.note(
        LogLevel::Info,
        &tf!("Source : {} — {} — {}", "Source: {} — {} — {}", source.title(), crate::diag::human_size(source.size), serial_text(&source)),
    );

    // 2. Ouverture de la source (lecture seule), volumes verrouillés pour une image cohérente.
    #[cfg(windows)]
    let mut locks = Vec::new();
    #[cfg(windows)]
    if let Target::Disk(_) = cfg.source {
        // Un disque système est refusé plus bas par les pré-vérifications : inutile de le verrouiller.
        if !source.is_system {
            locks = crate::win::rawdisk::lock_disk_volumes(&source, &volumes_for_lock(&source)).map_err(fail)?;
            if !locks.is_empty() {
                journal.note(
                    LogLevel::Info,
                    &tf!(
                        "{} volume(s) de la source verrouillé(s) pendant l'opération",
                        "{} source volume(s) locked during the operation",
                        locks.len()
                    ),
                );
            }
        }
    }
    let mut src = ReadOnly(open_read_only(&cfg.source).map_err(fail)?);
    let analysis = analyze_disk(&mut src).map_err(|e| fail(tf!("analyse de la source impossible : {e}", "source analysis failed: {e}")))?;
    let boot_src = assess_boot(&mut src, &analysis);

    // 3. Destination : description, plan et pré-vérifications.
    let wanted_size = cfg.new_image_size.unwrap_or(analysis.size);
    let destination = describe(&cfg.destination, true, &cfg.app_paths, Some(wanted_size)).map_err(fail)?;
    if let Some(fp) = &cfg.expected_destination_fingerprint
        && *fp != destination.fingerprint
    {
        return Err(fail(t!(
            "le disque de destination n'est plus celui qui avait été sélectionné (modèle, numéro de série ou taille différents). Opération annulée, rien n'a été écrit.",
            "the destination disk is no longer the one that was selected (different model, serial number or size). Operation cancelled, nothing was written."
        )));
    }
    journal.note(
        LogLevel::Info,
        &tf!(
            "Destination : {} — {} — {}",
            "Destination: {} — {} — {}",
            destination.title(),
            crate::diag::human_size(destination.size),
            serial_text(&destination)
        ),
    );
    let plan = build_plan(&analysis, destination.size, destination.sector_size, &cfg.plan);
    let mut findings = precheck(&source, &destination, Some(&analysis), Some(&plan));
    for f in &boot_src.findings {
        findings.push(f.clone().scope("source"));
    }
    if let Some(msg) = physical_write_gate(&destination, cfg.allow_unvalidated_physical_write) {
        findings.push(
            Finding::error(
                "PHYSICAL_WRITE_UNVALIDATED",
                t!("Écriture sur disque physique non encore validée", "Writing to a physical disk not validated yet"),
                msg,
            )
            .scope("destination")
            .blocking(),
        );
    }
    cfg.engine.validate().map_err(fail)?;
    if has_blocking(&findings) {
        for f in findings.iter().filter(|f| f.blocking) {
            journal.note(LogLevel::Error, &tf!("Pré-vérification bloquante : {} — {}", "Blocking pre-check: {} — {}", f.title, f.detail));
        }
        return Err(JobError::Blocked(findings));
    }
    for f in &findings {
        let level = match f.severity {
            crate::diag::Severity::Info => LogLevel::Info,
            crate::diag::Severity::Warning => LogLevel::Warning,
            _ => LogLevel::Error,
        };
        journal.note(level, &tf!("Pré-vérification [{}] {} — {}", "Pre-check [{}] {} — {}", f.severity.label(), f.title, f.detail));
    }

    // 4. Santé des disques (lecture seule), avant l'opération.
    let smart_source = Some(read_smart(&source));
    let smart_destination = Some(read_smart(&destination));
    for (who, s) in [("source", &smart_source), ("destination", &smart_destination)] {
        if let Some(s) = s
            && s.available
        {
            journal.note(LogLevel::Info, &tf!("Santé de la {who} ({}) : {}", "{who} health ({}): {}", s.protocol, s.level.label()));
            for f in &s.findings {
                journal.note(LogLevel::Warning, &tf!("Santé de la {who} : {} — {}", "{who} health: {} — {}", f.title, f.detail));
            }
        }
    }

    // 5. Ouverture de la destination. À partir d'ici, son contenu est modifié.
    let mut opened = Opened {
        src: Box::new(src),
        dst: open_destination(&cfg.destination, &destination, cfg.allow_unvalidated_physical_write)?,
        #[cfg(windows)]
        _locks: locks,
        #[cfg(windows)]
        dst_number: match cfg.destination {
            Target::Disk(n) => Some(n),
            _ => None,
        },
    };

    let outcome = run_clone(CloneRequest {
        src: opened.src.as_mut(),
        dst: opened.dst.as_mut(),
        src_analysis: &analysis,
        plan: &plan,
        opts: &cfg.engine,
        ctl,
        obs: &journal,
    });

    // 6. Windows relit la table de partitions de la destination.
    #[cfg(windows)]
    if let Some(n) = opened.dst_number {
        match crate::win::rawdisk::RawDisk::open_read_write(&format!(r"\\.\PhysicalDrive{n}")).and_then(|d| crate::win::rawdisk::refresh_disk(&d)) {
            Ok(()) => journal.note(
                LogLevel::Info,
                t!(
                    "Windows a été invité à relire la table de partitions de la destination.",
                    "Windows was asked to re-read the destination's partition table."
                ),
            ),
            Err(e) => journal.note(
                LogLevel::Warning,
                &tf!(
                    "Windows n'a pas pu relire la table de partitions de la destination ({e}). Débranchez puis rebranchez le disque.",
                    "Windows could not re-read the destination's partition table ({e}). Unplug the disk and plug it back in."
                ),
            ),
        }
    }
    drop(opened);

    // Le fichier image existe désormais : le rapport le nomme simplement.
    let mut destination = destination;
    if let Some(name) = destination.model.strip_suffix(t!(" (à créer)", " (to be created)")) {
        destination.model = name.to_string();
    }

    let report = Report {
        id,
        app: crate::APP_NAME.to_string(),
        version: crate::APP_VERSION.to_string(),
        started_at,
        finished_at: now_string(),
        source,
        destination,
        plan_options: cfg.plan.clone(),
        engine_options: cfg.engine.clone(),
        plan,
        precheck: findings,
        smart_source,
        smart_destination,
        outcome,
        log: journal.entries(),
    };
    if let Err(e) = save_report(reports_dir, &report) {
        obs.log(LogLevel::Error, &tf!("Le rapport n'a pas pu être enregistré : {e}", "The report could not be saved: {e}"));
    }
    Ok(report)
}

fn serial_text(d: &DiskInfo) -> String {
    if d.serial.trim().is_empty() {
        if d.kind == crate::model::DiskKind::Image {
            t!("fichier image", "image file").to_string()
        } else {
            t!("n° de série non communiqué", "serial number not reported").to_string()
        }
    } else {
        tf!("n° de série « {} »", "serial number \"{}\"", d.serial)
    }
}

fn open_destination(target: &Target, info: &DiskInfo, allow_unvalidated: bool) -> Result<Box<dyn BlockDevice>, JobError> {
    match target {
        Target::Image(p) => {
            let dev = if p.exists() {
                FileDevice::open_read_write(p, 512)
            } else {
                if let Some(parent) = p.parent()
                    && !parent.as_os_str().is_empty()
                {
                    std::fs::create_dir_all(parent).map_err(fail)?;
                }
                FileDevice::create(p, info.size, 512)
            };
            dev.map(|d| Box::new(d) as Box<dyn BlockDevice>)
                .map_err(|e| fail(tf!("ouverture de l'image de destination impossible : {e}", "cannot open the destination image: {e}")))
        }
        Target::Disk(_n) => {
            #[cfg(windows)]
            {
                crate::win::rawdisk::prepare_destination(info, &volumes_for_lock(info), allow_unvalidated)
                    .map(|d| Box::new(d) as Box<dyn BlockDevice>)
                    .map_err(|e| fail(tf!("préparation du disque de destination impossible : {e}", "cannot prepare the destination disk: {e}")))
            }
            #[cfg(not(windows))]
            {
                let _ = (info, allow_unvalidated);
                Err(fail(t!("non pris en charge sur cette plateforme", "not supported on this platform")))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_parsing() {
        assert_eq!(Target::parse("disk:3").unwrap(), Target::Disk(3));
        assert_eq!(Target::parse("image:C:\\x\\a.img").unwrap(), Target::Image(PathBuf::from("C:\\x\\a.img")));
        assert_eq!(Target::parse("D:\\b.img").unwrap(), Target::Image(PathBuf::from("D:\\b.img")));
        assert!(Target::parse("disk:abc").is_err());
        assert!(Target::parse("  ").is_err());
        assert_eq!(Target::Disk(7).id(), "disk:7");
    }

    #[test]
    fn image_id_is_stable_before_and_after_creation() {
        let dir = std::env::temp_dir().join(format!("chudclone-id-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("Clone.IMG");
        let _ = std::fs::remove_file(&file);
        let before = image_id(&file);
        std::fs::write(&file, b"x").unwrap();
        let after = image_id(&file);
        let via_dotdot = image_id(&dir.join("..").join(dir.file_name().unwrap()).join("clone.img"));
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(before, after);
        assert_eq!(after, via_dotdot);
        assert!(!after.contains(r"\\?\"), "{after}");
    }
}
