//! Chud Clone — application graphique.
//!
//! Cette couche ne contient aucune logique de clonage : elle relaie les demandes de
//! l'interface au moteur (`chud-clone-core`) et lui renvoie progression, journal et
//! rapports. Toutes les vérifications de sécurité sont refaites par le moteur.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use chud_clone_core::boot::BootAssessment;
use chud_clone_core::engine::{Control, Decision, EngineOptions, LogLevel, Observer, Progress, UnreadableInfo, UnreadablePolicy, VerifyMode};
use chud_clone_core::job::{self, JobConfig, JobError, Preparation, Target};
use chud_clone_core::model::DiskInfo;
use chud_clone_core::plan::{CloneMode, DiskAnalysis, PlanOptions};
use chud_clone_core::report::{self, Report, ReportSummary};
use chud_clone_core::smart::SmartReport;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, State};

/// Texte que l'utilisateur doit saisir pour confirmer l'effacement de la destination.
const CONFIRM_WORD: &str = "EFFACER";
const CONFIRM_WORD_EN: &str = "ERASE";

#[derive(Clone)]
struct ImageEntry {
    path: PathBuf,
    /// Taille demandée pour une image à créer.
    new_size: Option<u64>,
}

struct RunningJob {
    ctl: Arc<Control>,
    decision: Arc<(Mutex<Option<Decision>>, Condvar)>,
}

struct Inner {
    images: Mutex<Vec<ImageEntry>>,
    job: Mutex<Option<RunningJob>>,
    /// Scan de surface en cours (un seul à la fois, jamais pendant un clonage).
    scan: Mutex<Option<Arc<Control>>>,
    data_dir: PathBuf,
    reports_dir: PathBuf,
    portable: bool,
}

type Shared = Arc<Inner>;

impl Inner {
    fn health_dir(&self) -> PathBuf {
        self.data_dir.join("sante")
    }

    fn app_paths(&self) -> Vec<String> {
        vec![self.data_dir.to_string_lossy().to_string(), self.reports_dir.to_string_lossy().to_string()]
    }

    /// Résout un identifiant d'interface en cible du moteur.
    fn target(&self, id: &str) -> Result<(Target, Option<u64>), String> {
        if let Some(n) = id.strip_prefix("disk:") {
            return n
                .parse::<u32>()
                .map(|n| (Target::Disk(n), None))
                .map_err(|_| chud_clone_core::t!("identifiant de disque invalide", "invalid disk identifier").to_string());
        }
        let images = self.images.lock().unwrap_or_else(|e| e.into_inner());
        for e in images.iter() {
            if job::image_id(&e.path) == id {
                return Ok((Target::Image(e.path.clone()), e.new_size));
            }
        }
        Err(chud_clone_core::t!("cette image n'est plus dans la liste", "this image is no longer in the list").into())
    }
}

#[derive(Serialize)]
struct EnvInfo {
    name: &'static str,
    version: &'static str,
    elevated: bool,
    portable: bool,
    data_dir: String,
    reports_dir: String,
    physical_write_validated: bool,
    confirm_word: &'static str,
    /// Langue de Windows (« fr » ou « en ») : langue par défaut de l'interface.
    system_lang: &'static str,
}

#[tauri::command]
fn env_info(state: State<'_, Shared>) -> EnvInfo {
    EnvInfo {
        name: chud_clone_core::APP_NAME,
        version: chud_clone_core::APP_VERSION,
        elevated: job::is_elevated(),
        portable: state.portable,
        data_dir: state.data_dir.to_string_lossy().to_string(),
        reports_dir: state.reports_dir.to_string_lossy().to_string(),
        physical_write_validated: physical_write_validated(),
        confirm_word: CONFIRM_WORD,
        system_lang: chud_clone_core::i18n::system_lang().code(),
    }
}

/// Langue des messages du moteur (constats, raisons, rapports), choisie dans l'interface.
#[tauri::command]
fn set_language(lang: String) {
    chud_clone_core::i18n::set_lang(chud_clone_core::i18n::Lang::from_tag(&lang));
}

fn physical_write_validated() -> bool {
    #[cfg(windows)]
    {
        chud_clone_core::win::PHYSICAL_WRITE_VALIDATED
    }
    #[cfg(not(windows))]
    {
        false
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(|e| chud_clone_core::tf!("tâche interrompue : {e}", "task interrupted: {e}"))
}

#[tauri::command]
async fn list_disks(state: State<'_, Shared>) -> Result<Vec<DiskInfo>, String> {
    let s = state.inner().clone();
    blocking(move || {
        let mut disks = job::list_disks(true, &s.app_paths());
        let images = s.images.lock().unwrap_or_else(|e| e.into_inner()).clone();
        for e in images {
            match job::describe(&Target::Image(e.path.clone()), true, &[], e.new_size) {
                Ok(d) => disks.push(d),
                Err(_) => {
                    // Fichier disparu : on le retire de la liste.
                    s.images.lock().unwrap_or_else(|x| x.into_inner()).retain(|x| x.path != e.path);
                }
            }
        }
        disks
    })
    .await
}

#[tauri::command]
async fn add_image(state: State<'_, Shared>, path: String) -> Result<DiskInfo, String> {
    let s = state.inner().clone();
    blocking(move || {
        let p = PathBuf::from(&path);
        let info = job::describe(&Target::Image(p.clone()), true, &[], None)?;
        if info.size < 1024 * 1024 {
            return Err(
                chud_clone_core::t!("ce fichier est trop petit pour être une image disque", "this file is too small to be a disk image").into()
            );
        }
        let mut images = s.images.lock().unwrap_or_else(|e| e.into_inner());
        if !images.iter().any(|e| e.path == p) {
            images.push(ImageEntry { path: p, new_size: None });
        }
        Ok(info)
    })
    .await?
}

#[tauri::command]
async fn add_new_image(state: State<'_, Shared>, path: String, size: u64) -> Result<DiskInfo, String> {
    let s = state.inner().clone();
    blocking(move || {
        let p = PathBuf::from(&path);
        if p.exists() {
            return Err(chud_clone_core::t!(
                "ce fichier existe déjà : choisissez un nouveau nom, ou ajoutez-le comme image existante",
                "this file already exists: choose a new name, or add it as an existing image"
            )
            .into());
        }
        if size < 1024 * 1024 {
            return Err(chud_clone_core::t!("taille d'image trop petite", "image size too small").into());
        }
        let info = job::describe(&Target::Image(p.clone()), true, &[], Some(size))?;
        let mut images = s.images.lock().unwrap_or_else(|e| e.into_inner());
        images.retain(|e| e.path != p);
        images.push(ImageEntry { path: p, new_size: Some(size) });
        Ok(info)
    })
    .await?
}

#[tauri::command]
fn remove_image(state: State<'_, Shared>, id: String) -> Result<(), String> {
    let (target, _) = state.target(&id)?;
    if let Target::Image(p) = target {
        state.images.lock().unwrap_or_else(|e| e.into_inner()).retain(|e| e.path != p);
    }
    Ok(())
}

#[derive(Serialize)]
struct Inspection {
    analysis: DiskAnalysis,
    boot: BootAssessment,
    findings: Vec<chud_clone_core::diag::Finding>,
}

#[tauri::command]
async fn inspect_disk(state: State<'_, Shared>, id: String) -> Result<Inspection, String> {
    let (target, _) = state.target(&id)?;
    blocking(move || {
        let (analysis, boot) = job::inspect(&target)?;
        let mut findings = analysis.all_findings();
        findings.extend(boot.findings.clone());
        Ok(Inspection { analysis, boot, findings })
    })
    .await?
}

#[tauri::command]
async fn read_smart(state: State<'_, Shared>, id: String) -> Result<SmartReport, String> {
    let (target, size) = state.target(&id)?;
    let paths = state.app_paths();
    blocking(move || {
        let info = job::describe(&target, false, &paths, size)?;
        Ok(job::read_smart(&info))
    })
    .await?
}

#[derive(Serialize)]
struct DiskHealth {
    smart: SmartReport,
    history: chud_clone_core::health::HealthHistory,
    trends: Vec<chud_clone_core::diag::Finding>,
}

/// Lit la santé d'un disque (lecture seule), l'ajoute à son historique et signale ce qui
/// s'aggrave depuis les relevés précédents.
#[tauri::command]
async fn disk_health(state: State<'_, Shared>, id: String) -> Result<DiskHealth, String> {
    let (target, size) = state.target(&id)?;
    let paths = state.app_paths();
    let dir = state.health_dir();
    blocking(move || {
        let info = job::describe(&target, false, &paths, size)?;
        let smart = job::read_smart(&info);
        let history = chud_clone_core::health::record(&dir, &info, &smart, chud_clone_core::health::now());
        let trends = chud_clone_core::health::trends(&history);
        Ok(DiskHealth { smart, history, trends })
    })
    .await?
}

fn default_true() -> bool {
    true
}

#[derive(Deserialize, Clone)]
struct CloneParams {
    source_id: String,
    destination_id: String,
    mode: CloneMode,
    optimize_layout: bool,
    #[serde(default = "default_true")]
    grow_main: bool,
    verify: VerifyMode,
    unreadable: UnreadablePolicy,
    read_retries: u32,
    allow_unvalidated_physical_write: bool,
    source_fingerprint: Option<String>,
    destination_fingerprint: Option<String>,
}

fn config(state: &Inner, p: &CloneParams) -> Result<JobConfig, String> {
    let (source, _) = state.target(&p.source_id)?;
    let (destination, new_size) = state.target(&p.destination_id)?;
    let engine = EngineOptions { verify: p.verify, unreadable: p.unreadable, read_retries: p.read_retries, ..Default::default() };
    engine.validate()?;
    Ok(JobConfig {
        source,
        destination,
        plan: PlanOptions { mode: p.mode, optimize_layout: p.optimize_layout, grow_main: p.grow_main },
        engine,
        new_image_size: new_size,
        allow_unvalidated_physical_write: p.allow_unvalidated_physical_write,
        expected_source_fingerprint: p.source_fingerprint.clone(),
        expected_destination_fingerprint: p.destination_fingerprint.clone(),
        app_paths: state.app_paths(),
    })
}

/// Analyse, plan et pré-vérifications. N'écrit rien.
#[tauri::command]
async fn prepare_clone(state: State<'_, Shared>, params: CloneParams) -> Result<Preparation, String> {
    let mut cfg = config(&state, &params)?;
    // La préparation décrit l'état courant : les empreintes ne servent qu'à l'exécution.
    cfg.expected_source_fingerprint = None;
    cfg.expected_destination_fingerprint = None;
    let dir = state.health_dir();
    let prep = blocking(move || job::prepare(&cfg, true)).await??;
    // Chaque lecture de santé enrichit l'historique des disques concernés.
    let now = chud_clone_core::health::now();
    for (disk, smart) in [(&prep.source, &prep.smart_source), (&prep.destination, &prep.smart_destination)] {
        if let Some(s) = smart {
            chud_clone_core::health::record(&dir, disk, s, now);
        }
    }
    Ok(prep)
}

struct UiObserver {
    app: AppHandle,
    ctl: Arc<Control>,
    decision: Arc<(Mutex<Option<Decision>>, Condvar)>,
}

#[derive(Serialize, Clone)]
struct LogEvent<'a> {
    level: LogLevel,
    message: &'a str,
    time: String,
}

impl Observer for UiObserver {
    fn progress(&self, p: &Progress) {
        let _ = self.app.emit("clone-progress", p);
    }
    fn log(&self, level: LogLevel, msg: &str) {
        let _ = self.app.emit("clone-log", LogEvent { level, message: msg, time: report::now_string()[11..].to_string() });
    }
    fn decide_unreadable(&self, info: &UnreadableInfo) -> Decision {
        let (lock, cv) = &*self.decision;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let _ = self.app.emit("clone-ask", info);
        let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(d) = guard.take() {
                return d;
            }
            // Une annulation pendant la question équivaut à « arrêter ».
            if self.ctl.is_cancelled() {
                return Decision::Abort;
            }
            guard = cv.wait_timeout(guard, Duration::from_millis(200)).unwrap_or_else(|e| e.into_inner()).0;
        }
    }
}

#[derive(Serialize, Clone)]
struct DoneEvent {
    report: Option<Report>,
    error: Option<String>,
    blocked: Vec<chud_clone_core::diag::Finding>,
}

/// Lance le clonage. OPÉRATION DESTRUCTRICE : exige le mot de confirmation saisi par
/// l'utilisateur ; le moteur refait ensuite toutes les pré-vérifications.
#[tauri::command]
fn start_clone(app: AppHandle, state: State<'_, Shared>, params: CloneParams, confirmation: String) -> Result<(), String> {
    // Mot de confirmation de la langue affichée (EFFACER en français, ERASE en anglais).
    if confirmation.trim() != CONFIRM_WORD && confirmation.trim() != CONFIRM_WORD_EN {
        return Err(chud_clone_core::tf!(
            "confirmation incorrecte : saisissez {CONFIRM_WORD} pour autoriser l'effacement de la destination",
            "wrong confirmation: type {CONFIRM_WORD_EN} to allow erasing the destination"
        ));
    }
    if params.source_fingerprint.is_none() || params.destination_fingerprint.is_none() {
        return Err(chud_clone_core::t!(
            "identité des disques non fournie : relancez la vérification",
            "disk identity not provided: run the check again"
        )
        .into());
    }
    let cfg = config(&state, &params)?;
    if state.scan.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
        return Err(chud_clone_core::t!(
            "un scan de surface est en cours : arrêtez-le avant de lancer un clonage",
            "a surface scan is running: stop it before starting a clone"
        )
        .into());
    }
    let mut slot = state.job.lock().unwrap_or_else(|e| e.into_inner());
    if slot.is_some() {
        return Err(chud_clone_core::t!("un clonage est déjà en cours", "a clone is already running").into());
    }
    let ctl = Arc::new(Control::new());
    let decision = Arc::new((Mutex::new(None), Condvar::new()));
    *slot = Some(RunningJob { ctl: ctl.clone(), decision: decision.clone() });
    drop(slot);

    let shared = state.inner().clone();
    std::thread::Builder::new()
        .name("clonage".into())
        .spawn(move || {
            let obs = UiObserver { app: app.clone(), ctl: ctl.clone(), decision };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job::execute(&cfg, &shared.reports_dir, &ctl, &obs)));
            let event = match result {
                Ok(Ok(report)) => DoneEvent { report: Some(report), error: None, blocked: vec![] },
                Ok(Err(JobError::Blocked(f))) => DoneEvent {
                    report: None,
                    error: Some(
                        chud_clone_core::t!(
                            "Le clonage a été refusé par les pré-vérifications. Rien n'a été écrit.",
                            "The clone was refused by the pre-checks. Nothing was written."
                        )
                        .into(),
                    ),
                    blocked: f,
                },
                Ok(Err(JobError::Failed(m))) => DoneEvent { report: None, error: Some(m), blocked: vec![] },
                Err(_) => DoneEvent {
                    report: None,
                    error: Some(
                        chud_clone_core::t!(
                            "Erreur interne inattendue pendant le clonage. La destination doit être considérée comme incomplète.",
                            "Unexpected internal error during cloning. The destination must be considered incomplete."
                        )
                        .into(),
                    ),
                    blocked: vec![],
                },
            };
            *shared.job.lock().unwrap_or_else(|e| e.into_inner()) = None;
            let _ = app.emit("clone-done", event);
        })
        .map_err(|e| {
            *state.job.lock().unwrap_or_else(|x| x.into_inner()) = None;
            chud_clone_core::tf!("impossible de démarrer le clonage : {e}", "cannot start the clone: {e}")
        })?;
    Ok(())
}

struct ScanUi {
    app: AppHandle,
}

impl chud_clone_core::scan::ScanObserver for ScanUi {
    fn progress(&self, p: &chud_clone_core::scan::ScanProgress) {
        let _ = self.app.emit("scan-progress", p);
    }
}

#[derive(Serialize, Clone)]
struct ScanDone {
    id: String,
    disk: Option<DiskInfo>,
    result: Option<chud_clone_core::scan::ScanResult>,
    error: Option<String>,
    saved: Option<String>,
}

/// Lance un scan de surface en LECTURE SEULE. Le résultat est aussi enregistré en JSON dans
/// le dossier de données (`analyses`).
#[tauri::command]
fn start_scan(app: AppHandle, state: State<'_, Shared>, id: String) -> Result<(), String> {
    let (target, _) = state.target(&id)?;
    if state.job.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
        return Err(chud_clone_core::t!(
            "un clonage est en cours : le scan attendra qu'il soit terminé",
            "a clone is running: the scan will wait until it has finished"
        )
        .into());
    }
    let mut slot = state.scan.lock().unwrap_or_else(|e| e.into_inner());
    if slot.is_some() {
        return Err(chud_clone_core::t!("un scan est déjà en cours", "a scan is already running").into());
    }
    let ctl = Arc::new(Control::new());
    *slot = Some(ctl.clone());
    drop(slot);
    let shared = state.inner().clone();
    let paths = state.app_paths();
    std::thread::Builder::new()
        .name("scan".into())
        .spawn(move || {
            let obs = ScanUi { app: app.clone() };
            let opts = chud_clone_core::scan::ScanOptions::default();
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job::surface_scan(&target, &paths, &opts, &ctl, &obs)));
            let event = match res {
                Ok(Ok((disk, result))) => {
                    let dir = shared.data_dir.join("analyses");
                    let file = dir.join(format!("scan-{}.json", report::new_report_id()));
                    let saved = std::fs::create_dir_all(&dir)
                        .ok()
                        .and_then(|_| serde_json::to_vec_pretty(&serde_json::json!({ "disk": disk, "result": result })).ok())
                        .and_then(|b| std::fs::write(&file, b).ok())
                        .map(|_| file.to_string_lossy().to_string());
                    ScanDone { id, disk: Some(disk), result: Some(result), error: None, saved }
                }
                Ok(Err(e)) => ScanDone { id, disk: None, result: None, error: Some(e), saved: None },
                Err(_) => ScanDone {
                    id,
                    disk: None,
                    result: None,
                    error: Some(chud_clone_core::t!("erreur interne pendant le scan", "internal error during the scan").into()),
                    saved: None,
                },
            };
            *shared.scan.lock().unwrap_or_else(|e| e.into_inner()) = None;
            let _ = app.emit("scan-done", event);
        })
        .map_err(|e| {
            *state.scan.lock().unwrap_or_else(|x| x.into_inner()) = None;
            chud_clone_core::tf!("impossible de démarrer le scan : {e}", "cannot start the scan: {e}")
        })?;
    Ok(())
}

#[tauri::command]
fn cancel_scan(state: State<'_, Shared>) {
    if let Some(ctl) = state.scan.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        ctl.cancel();
    }
}

#[tauri::command]
fn cancel_clone(state: State<'_, Shared>) {
    if let Some(job) = state.job.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        job.ctl.cancel();
        job.decision.1.notify_all();
    }
}

#[tauri::command]
fn answer_unreadable(state: State<'_, Shared>, zero_fill: bool) {
    if let Some(job) = state.job.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        let (lock, cv) = &*job.decision;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = Some(if zero_fill { Decision::ZeroFill } else { Decision::Abort });
        cv.notify_all();
    }
}

#[tauri::command]
fn clone_running(state: State<'_, Shared>) -> bool {
    state.job.lock().unwrap_or_else(|e| e.into_inner()).is_some()
}

#[tauri::command]
async fn list_reports(state: State<'_, Shared>) -> Result<Vec<ReportSummary>, String> {
    let dir = state.reports_dir.clone();
    blocking(move || report::list_reports(&dir)).await
}

#[tauri::command]
async fn get_report(state: State<'_, Shared>, id: String) -> Result<Report, String> {
    let dir = state.reports_dir.clone();
    blocking(move || report::load_report(&dir, &id).map_err(|e| chud_clone_core::tf!("rapport illisible : {e}", "unreadable report: {e}"))).await?
}

#[tauri::command]
async fn export_report(state: State<'_, Shared>, id: String, format: String, path: String) -> Result<(), String> {
    let dir = state.reports_dir.clone();
    blocking(move || {
        let r = report::load_report(&dir, &id).map_err(|e| chud_clone_core::tf!("rapport illisible : {e}", "unreadable report: {e}"))?;
        let text = match format.as_str() {
            "txt" => report::render_txt(&r),
            "html" => report::render_html(&r),
            "json" => serde_json::to_string_pretty(&r).map_err(|e| e.to_string())?,
            other => return Err(chud_clone_core::tf!("format inconnu : {other}", "unknown format: {other}")),
        };
        std::fs::write(&path, text).map_err(|e| chud_clone_core::tf!("écriture impossible : {e}", "cannot write: {e}"))
    })
    .await?
}

#[tauri::command]
fn relaunch_elevated(app: AppHandle, state: State<'_, Shared>) -> Result<(), String> {
    if state.job.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
        return Err(chud_clone_core::t!("un clonage est en cours", "a clone is running").into());
    }
    #[cfg(windows)]
    {
        chud_clone_core::win::relaunch_elevated("").map_err(|e| e.to_string())?;
        app.exit(0);
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = app;
        Err(chud_clone_core::t!("non pris en charge sur cette plateforme", "not supported on this platform").into())
    }
}

/// Dossier de données : à côté de l'exécutable en mode portable (fichier
/// `portable.flag` présent), sinon dans `%APPDATA%\ChudClone`.
fn data_location() -> (PathBuf, bool) {
    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf()));
    if let Some(dir) = &exe_dir
        && dir.join("portable.flag").exists()
    {
        let d = dir.join("ChudClone-data");
        if std::fs::create_dir_all(&d).is_ok() {
            return (d, true);
        }
    }
    let base = std::env::var_os("APPDATA").map(PathBuf::from).or_else(|| exe_dir.clone()).unwrap_or_else(|| PathBuf::from("."));
    (base.join("ChudClone"), false)
}

fn main() {
    let (data_dir, portable) = data_location();
    // « rapports » pour les données créées par les premières versions, sinon « reports ».
    let reports_dir = if data_dir.join("rapports").is_dir() { data_dir.join("rapports") } else { data_dir.join("reports") };
    chud_clone_core::i18n::set_lang(chud_clone_core::i18n::system_lang());
    let _ = std::fs::create_dir_all(&reports_dir);
    // Le profil du moteur web suit le dossier de données (important en mode portable).
    // SAFETY: appelé avant la création de tout autre thread.
    unsafe { std::env::set_var("WEBVIEW2_USER_DATA_FOLDER", data_dir.join("webview")) };

    let shared: Shared =
        Arc::new(Inner { images: Mutex::new(Vec::new()), job: Mutex::new(None), scan: Mutex::new(None), data_dir, reports_dir, portable });

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(shared)
        .invoke_handler(tauri::generate_handler![
            env_info,
            set_language,
            list_disks,
            add_image,
            add_new_image,
            remove_image,
            inspect_disk,
            read_smart,
            disk_health,
            prepare_clone,
            start_clone,
            cancel_clone,
            start_scan,
            cancel_scan,
            answer_unreadable,
            clone_running,
            list_reports,
            get_report,
            export_report,
            relaunch_elevated
        ])
        .on_window_event(|window, event| {
            // Fermer la fenêtre pendant un clonage laisserait la destination incomplète :
            // l'interface demande confirmation avant.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let state = window.state::<Shared>();
                if state.job.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
                    api.prevent_close();
                    let _ = window.emit("close-blocked", ());
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("erreur au démarrage de Chud Clone");
}
