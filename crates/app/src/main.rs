//! Chud Clone — graphical application.
//!
//! This layer holds no cloning logic: it relays the interface's requests to the
//! engine (`chud-clone-core`) and sends back progress, log and reports. Every safety
//! check is done again by the engine.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod update;

use chud_clone_core::boot::BootAssessment;
use chud_clone_core::engine::{Control, Decision, EngineOptions, LogLevel, Observer, Progress, UnreadableInfo, UnreadablePolicy, VerifyMode};
use chud_clone_core::job::{self, JobConfig, JobError, Preparation, Target};
use chud_clone_core::format::{FormatOutcome, FormatRequest};
use chud_clone_core::model::DiskInfo;
use chud_clone_core::plan::{CloneMode, DiskAnalysis, PlanOptions};
use chud_clone_core::report::{self, Report, ReportSummary};
use chud_clone_core::smart::SmartReport;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, State};

/// Text the user must type to confirm erasing the destination.
const CONFIRM_WORD: &str = "EFFACER";
const CONFIRM_WORD_EN: &str = "ERASE";

#[derive(Clone)]
struct ImageEntry {
    path: PathBuf,
    /// Size of an image to be created (shown in the lists).
    new_size: Option<u64>,
    /// No size chosen: the engine creates the image with the size the source's
    /// partitions need for the chosen options (`new_size` is then an estimate).
    auto_size: bool,
}

struct RunningJob {
    ctl: Arc<Control>,
    decision: Arc<(Mutex<Option<Decision>>, Condvar)>,
}

/// What the application is doing. There is exactly one activity at a time: a clone, a
/// surface scan, a format and an update exclude each other. Every command that starts
/// something goes through [`ActivitySlot::begin`], the only place where this is decided
/// (see docs/STATE-MACHINES.md).
enum Activity {
    Idle,
    Cloning(RunningJob),
    Scanning(Arc<Control>),
    Formatting,
    RepairingBoot,
    Updating,
}

impl Activity {
    /// Name given to the interface and to tools (`app_state`).
    fn name(&self) -> &'static str {
        match self {
            Activity::Idle => "idle",
            Activity::Cloning(_) => "cloning",
            Activity::Scanning(_) => "scanning",
            Activity::Formatting => "formatting",
            Activity::RepairingBoot => "repairing_boot",
            Activity::Updating => "updating",
        }
    }

    /// Why nothing else can start meanwhile (`None` when idle).
    fn busy(&self) -> Option<String> {
        Some(
            match self {
                Activity::Idle => return None,
                Activity::Cloning(_) => chud_clone_core::t!("un clonage est en cours", "a clone is running"),
                Activity::Scanning(_) => {
                    chud_clone_core::t!("un test de surface est en cours : arrêtez-le d'abord", "a surface scan is running: stop it first")
                }
                Activity::Formatting => chud_clone_core::t!("un formatage est en cours", "a format is running"),
                Activity::RepairingBoot => chud_clone_core::t!("une réparation du démarrage est en cours", "a boot repair is running"),
                Activity::Updating => chud_clone_core::t!("une mise à jour est en cours d'installation", "an update is being installed"),
            }
            .to_string(),
        )
    }

    /// Closing the window now would leave a disk half written.
    fn blocks_close(&self) -> bool {
        matches!(self, Activity::Cloning(_) | Activity::Formatting | Activity::RepairingBoot)
    }
}

struct ActivitySlot(Mutex<Activity>);

impl ActivitySlot {
    fn get(&self) -> std::sync::MutexGuard<'_, Activity> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Idle → `next`. Refused, with the reason, while anything else is running.
    fn begin(&self, next: Activity) -> Result<(), String> {
        let mut current = self.get();
        if let Some(reason) = current.busy() {
            return Err(reason);
        }
        *current = next;
        Ok(())
    }

    /// Back to idle (end of the operation, whatever its result).
    fn end(&self) {
        *self.get() = Activity::Idle;
    }
}

struct Inner {
    images: Mutex<Vec<ImageEntry>>,
    activity: ActivitySlot,
    /// No sleep while the application is open (Settings; on by default).
    awake: chud_clone_core::awake::AwakeSwitch,
    keep_awake: std::sync::atomic::AtomicBool,
    data_dir: PathBuf,
    reports_dir: PathBuf,
    portable: bool,
    /// Result of the last update check (with the package to download).
    update: Mutex<Option<update::UpdateInfo>>,
}

type Shared = Arc<Inner>;

impl Inner {
    fn health_dir(&self) -> PathBuf {
        self.data_dir.join("sante")
    }

    fn app_paths(&self) -> Vec<String> {
        vec![self.data_dir.to_string_lossy().to_string(), self.reports_dir.to_string_lossy().to_string()]
    }

    /// Resolves an interface identifier into an engine target.
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
                return Ok((Target::Image(e.path.clone()), if e.auto_size { None } else { e.new_size }));
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
    /// Windows language ("fr" or "en"): default language of the interface.
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

/// Language of the engine's messages (findings, reasons, reports), chosen in the interface.
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
                    // File gone: remove it from the list.
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
            images.push(ImageEntry { path: p, new_size: None, auto_size: false });
        }
        Ok(info)
    })
    .await?
}

#[tauri::command]
async fn add_new_image(state: State<'_, Shared>, path: String, size: Option<u64>, source_id: Option<String>) -> Result<DiskInfo, String> {
    let s = state.inner().clone();
    blocking(move || {
        // Without an explicit size, the image gets the size the source's partitions
        // need, not the size of the whole source disk (estimate shown until the
        // engine computes it for the chosen options).
        let auto_size = size.is_none();
        let size = match size {
            Some(size) => size,
            None => {
                let (source, _) = s.target(source_id.as_deref().unwrap_or_default())?;
                let (analysis, _) = job::inspect(&source)?;
                chud_clone_core::plan::minimal_destination_size(&analysis, 512, &PlanOptions::default())
            }
        };
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
        images.push(ImageEntry { path: p, new_size: Some(size), auto_size });
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

/// Reads a disk's health (read-only), adds it to its history and flags what got
/// worse since the previous readings.
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
    /// Safely remove the destination disk after a successful clone.
    #[serde(default)]
    eject: bool,
    /// Prepare a Windows clone to boot on another PC.
    #[serde(default = "default_true")]
    boot_prep: bool,
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
        prepare_boot: p.boot_prep,
    })
}

/// Analysis, plan and pre-checks. Writes nothing.
#[tauri::command]
async fn prepare_clone(state: State<'_, Shared>, params: CloneParams) -> Result<Preparation, String> {
    let mut cfg = config(&state, &params)?;
    // Preparation describes the current state: fingerprints are only used for execution.
    cfg.expected_source_fingerprint = None;
    cfg.expected_destination_fingerprint = None;
    let dir = state.health_dir();
    let prep = blocking(move || job::prepare(&cfg, true)).await??;
    // Every health reading enriches the history of the disks involved.
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
            // Cancelling during the question amounts to "stop".
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
    /// Destination ejected after the clone (None: not requested or not attempted).
    ejected: Option<bool>,
    eject_error: Option<String>,
}

impl DoneEvent {
    fn failed(error: String, blocked: Vec<chud_clone_core::diag::Finding>) -> Self {
        DoneEvent { report: None, error: Some(error), blocked, ejected: None, eject_error: None }
    }
}

/// Ejects the destination disk after a clone whose result is usable.
fn eject_after(p: &CloneParams, report: &Report) -> (Option<bool>, Option<String>) {
    use chud_clone_core::clone::FinalStatus;
    if !p.eject || !matches!(report.outcome.status, FinalStatus::SuccessVerified | FinalStatus::CompletedWithWarnings) {
        return (None, None);
    }
    let Some(n) = p.destination_id.strip_prefix("disk:").and_then(|n| n.parse::<u32>().ok()) else {
        return (None, None);
    };
    #[cfg(windows)]
    {
        std::thread::sleep(Duration::from_millis(500));
        match chud_clone_core::win::eject::eject_disk(n) {
            Ok(()) => (Some(true), None),
            Err(e) => (Some(false), Some(e)),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = n;
        (Some(false), None)
    }
}

/// Starts the clone. DESTRUCTIVE OPERATION: requires the confirmation word typed by
/// the user; the engine then redoes every pre-check.
#[tauri::command]
fn start_clone(app: AppHandle, state: State<'_, Shared>, params: CloneParams, confirmation: String) -> Result<(), String> {
    // Confirmation word of the displayed language (EFFACER in French, ERASE in English).
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
    let ctl = Arc::new(Control::new());
    let decision = Arc::new((Mutex::new(None), Condvar::new()));
    state.activity.begin(Activity::Cloning(RunningJob { ctl: ctl.clone(), decision: decision.clone() }))?;

    let shared = state.inner().clone();
    let params = params.clone();
    std::thread::Builder::new()
        .name("clonage".into())
        .spawn(move || {
            let obs = UiObserver { app: app.clone(), ctl: ctl.clone(), decision };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job::execute(&cfg, &shared.reports_dir, &ctl, &obs)));
            let event = match result {
                Ok(Ok(report)) => {
                    let (ejected, eject_error) = eject_after(&params, &report);
                    DoneEvent { report: Some(report), error: None, blocked: vec![], ejected, eject_error }
                }
                Ok(Err(JobError::Blocked(f))) => DoneEvent::failed(
                    chud_clone_core::t!(
                        "Le clonage a été refusé par les pré-vérifications. Rien n'a été écrit.",
                        "The clone was refused by the pre-checks. Nothing was written."
                    )
                    .into(),
                    f,
                ),
                Ok(Err(JobError::Failed(m))) => DoneEvent::failed(m, vec![]),
                Err(_) => DoneEvent::failed(
                    chud_clone_core::t!(
                        "Erreur interne inattendue pendant le clonage. La destination doit être considérée comme incomplète.",
                        "Unexpected internal error during cloning. The destination must be considered incomplete."
                    )
                    .into(),
                    vec![],
                ),
            };
            shared.activity.end();
            let _ = app.emit("clone-done", event);
        })
        .map_err(|e| {
            state.activity.end();
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

/// Starts a READ-ONLY surface scan. The result is also saved as JSON in the
/// data folder (`analyses`).
#[tauri::command]
fn start_scan(app: AppHandle, state: State<'_, Shared>, id: String) -> Result<(), String> {
    let (target, _) = state.target(&id)?;
    let ctl = Arc::new(Control::new());
    state.activity.begin(Activity::Scanning(ctl.clone()))?;
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
            shared.activity.end();
            let _ = app.emit("scan-done", event);
        })
        .map_err(|e| {
            state.activity.end();
            chud_clone_core::tf!("impossible de démarrer le scan : {e}", "cannot start the scan: {e}")
        })?;
    Ok(())
}

/// Shuts the computer down (end of a clone queue, after the interface's countdown).
#[tauri::command]
fn shutdown_pc(state: State<'_, Shared>) -> Result<(), String> {
    if let Some(reason) = state.activity.get().busy() {
        return Err(reason);
    }
    #[cfg(windows)]
    return chud_clone_core::win::eject::shutdown_now();
    #[cfg(not(windows))]
    Err("Windows only".into())
}

#[derive(Serialize)]
struct FormatCheck {
    disk: DiskInfo,
    /// Name as it will be written (FAT32: upper case).
    label: String,
    refusals: Vec<String>,
    /// FAT32 above 32 GiB: written by Chud Clone instead of Windows.
    own_fat32: bool,
}

/// What formatting would do, and why it is refused. Writes nothing.
#[tauri::command]
async fn format_check(state: State<'_, Shared>, id: String, request: FormatRequest) -> Result<FormatCheck, String> {
    let (target, _) = state.target(&id)?;
    let paths = state.app_paths();
    blocking(move || {
        let disk = job::describe(&target, false, &paths, None)?;
        let label = chud_clone_core::format::validate_label(request.fs, &request.label).unwrap_or_else(|_| request.label.clone());
        let refusals = chud_clone_core::format::refusals(&disk, &request);
        let own_fat32 = chud_clone_core::format::uses_own_fat32(&request, disk.size.saturating_sub(2 << 20));
        Ok(FormatCheck { disk, label, refusals, own_fat32 })
    })
    .await?
}

/// Formats a whole disk. DESTRUCTIVE OPERATION: requires the confirmation word and
/// the disk's identity as shown to the user; the engine checks both again.
#[tauri::command]
async fn format_disk(
    app: AppHandle,
    state: State<'_, Shared>,
    id: String,
    fingerprint: String,
    request: FormatRequest,
    confirmation: String,
) -> Result<FormatOutcome, String> {
    if confirmation.trim() != CONFIRM_WORD && confirmation.trim() != CONFIRM_WORD_EN {
        return Err(chud_clone_core::tf!(
            "confirmation incorrecte : saisissez {CONFIRM_WORD} pour autoriser l'effacement du disque",
            "wrong confirmation: type {CONFIRM_WORD_EN} to allow erasing the disk"
        ));
    }
    let number = id
        .strip_prefix("disk:")
        .and_then(|n| n.parse::<u32>().ok())
        .ok_or_else(|| chud_clone_core::t!("seuls les disques peuvent être formatés", "only disks can be formatted").to_string())?;
    state.activity.begin(Activity::Formatting)?;
    let shared = state.inner().clone();
    let paths = state.app_paths();
    let result = blocking(move || {
        #[cfg(windows)]
        {
            let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                chud_clone_core::format::format_disk(number, &fingerprint, &request, &paths, &|s| {
                    let _ = app.emit("format-step", s);
                })
            }));
            out.unwrap_or_else(|_| Err(chud_clone_core::t!("erreur interne pendant le formatage", "internal error while formatting").into()))
        }
        #[cfg(not(windows))]
        {
            let _ = (app, number, fingerprint, request, paths);
            Err::<FormatOutcome, String>("Windows only".into())
        }
    })
    .await;
    shared.activity.end();
    result?
}

/// Repairs the boot of a Windows disk for another PC (BCD on its own partitions,
/// storage drivers at boot, saved session cancelled). Erases nothing; refused on the
/// disk of the running Windows.
#[tauri::command]
async fn fix_boot(app: AppHandle, state: State<'_, Shared>, id: String) -> Result<chud_clone_core::bootprep::BootPrepOutcome, String> {
    let (target, _) = state.target(&id)?;
    let paths = state.app_paths();
    let info = blocking({
        let target = target.clone();
        move || job::describe(&target, false, &paths, None)
    })
    .await??;
    let number = info.number.ok_or_else(|| chud_clone_core::t!("seuls les disques peuvent être réparés", "only disks can be repaired").to_string())?;
    if info.is_system {
        return Err(chud_clone_core::t!(
            "c'est le disque du Windows en cours d'exécution : sa configuration de démarrage ne se répare pas depuis lui-même",
            "this is the disk of the running Windows: its boot configuration is not repaired from itself"
        )
        .into());
    }
    state.activity.begin(Activity::RepairingBoot)?;
    let shared = state.inner().clone();
    let result = blocking(move || {
        let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            chud_clone_core::bootprep::prepare_disk(number, &|s| {
                let _ = app.emit("fixboot-step", s);
            })
        }));
        out.unwrap_or_else(|_| Err(chud_clone_core::t!("erreur interne pendant la réparation", "internal error during the repair").into()))
    })
    .await;
    shared.activity.end();
    result?
}

#[tauri::command]
fn cancel_scan(state: State<'_, Shared>) {
    if let Activity::Scanning(ctl) = &*state.activity.get() {
        ctl.cancel();
    }
}

#[tauri::command]
fn cancel_clone(state: State<'_, Shared>) {
    if let Activity::Cloning(job) = &*state.activity.get() {
        job.ctl.cancel();
        job.decision.1.notify_all();
    }
}

#[tauri::command]
fn answer_unreadable(state: State<'_, Shared>, zero_fill: bool) {
    if let Activity::Cloning(job) = &*state.activity.get() {
        let (lock, cv) = &*job.decision;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = Some(if zero_fill { Decision::ZeroFill } else { Decision::Abort });
        cv.notify_all();
    }
}

#[tauri::command]
fn clone_running(state: State<'_, Shared>) -> bool {
    matches!(&*state.activity.get(), Activity::Cloning(_))
}

#[derive(Serialize)]
struct AppState {
    /// "idle", "cloning", "scanning", "formatting" or "updating".
    activity: &'static str,
    keep_awake: bool,
}

/// Current state of the application, for the interface, diagnostics and tools.
#[tauri::command]
fn app_state(state: State<'_, Shared>) -> AppState {
    AppState { activity: state.activity.get().name(), keep_awake: state.keep_awake.load(std::sync::atomic::Ordering::SeqCst) }
}

/// No sleep while the application is open (Settings). While a clone, a scan or a
/// format runs, the engine keeps the computer awake by itself in any case.
#[tauri::command]
fn set_keep_awake(state: State<'_, Shared>, enabled: bool) {
    state.keep_awake.store(enabled, std::sync::atomic::Ordering::SeqCst);
    state.awake.set(enabled);
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
    if let Some(reason) = state.activity.get().busy() {
        return Err(reason);
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

// ------------------------------------------------------------------ Updates

/// Checks the latest published release. Read-only: nothing is downloaded here.
#[tauri::command]
async fn update_check(state: State<'_, Shared>) -> Result<update::UpdateInfo, String> {
    let portable = state.portable;
    let info = blocking(move || update::check(portable)).await??;
    *state.update.lock().unwrap_or_else(|e| e.into_inner()) = Some(info.clone());
    Ok(info)
}

#[derive(Serialize, Clone)]
struct UpdateProgress {
    phase: &'static str,
    done: u64,
    total: u64,
}

/// Downloads, verifies and installs the update found by the last check. The
/// application closes (and restarts) once the new version is in place.
#[tauri::command]
fn update_install(app: AppHandle, state: State<'_, Shared>) -> Result<(), String> {
    let info = state.update.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let Some(info) = info.filter(|i| i.available && i.method != update::Method::Manual) else {
        return Err(chud_clone_core::t!("aucune mise à jour installable", "no installable update").into());
    };
    state.activity.begin(Activity::Updating)?;
    let shared = state.inner().clone();
    std::thread::Builder::new()
        .name("update".into())
        .spawn(move || {
            let progress = |phase, done, total| {
                let _ = app.emit("update-progress", UpdateProgress { phase, done, total });
            };
            let result = update::download(&info, &progress).and_then(|path| {
                let _ = app.emit("update-progress", UpdateProgress { phase: "install", done: 0, total: 0 });
                apply_update(&info, &path)
            });
            match result {
                Ok(()) => app.exit(0),
                Err(e) => {
                    shared.activity.end();
                    let _ = app.emit("update-failed", e);
                }
            }
        })
        .map_err(|e| {
            state.activity.end();
            e.to_string()
        })?;
    Ok(())
}

#[cfg(windows)]
fn apply_update(info: &update::UpdateInfo, package: &std::path::Path) -> Result<(), String> {
    match info.method {
        update::Method::Installer => update::run_installer(package),
        update::Method::Portable => {
            let exe = update::apply_portable(package)?;
            std::process::Command::new(exe).spawn().map(|_| ()).map_err(|e| {
                chud_clone_core::tf!(
                    "nouvelle version installée, mais son lancement a échoué : {e}",
                    "new version installed, but it could not be started: {e}"
                )
            })
        }
        update::Method::Manual => Err(chud_clone_core::t!("aucune mise à jour installable", "no installable update").into()),
    }
}

#[cfg(not(windows))]
fn apply_update(_: &update::UpdateInfo, _: &std::path::Path) -> Result<(), String> {
    Err(chud_clone_core::t!("non pris en charge sur cette plateforme", "not supported on this platform").into())
}

/// Opens the release page of the last check (or the releases list) in the browser.
#[tauri::command]
fn open_release_page(state: State<'_, Shared>) -> Result<(), String> {
    let url = state
        .update
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|i| i.safe_page_url())
        .unwrap_or_else(|| format!("https://github.com/{}/releases", update::REPOSITORY));
    #[cfg(windows)]
    {
        update::shell_open(url, None)
    }
    #[cfg(not(windows))]
    {
        let _ = url;
        Err(chud_clone_core::t!("non pris en charge sur cette plateforme", "not supported on this platform").into())
    }
}

/// Data folder: next to the executable in portable mode (`portable.flag` file
/// present), otherwise in `%APPDATA%\ChudClone`.
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
    // "rapports" for data created by the first versions, otherwise "reports".
    let reports_dir = if data_dir.join("rapports").is_dir() { data_dir.join("rapports") } else { data_dir.join("reports") };
    chud_clone_core::i18n::set_lang(chud_clone_core::i18n::system_lang());
    let _ = std::fs::create_dir_all(&reports_dir);
    // The web engine's profile follows the data folder (important in portable mode).
    // SAFETY: called before any other thread is created.
    unsafe { std::env::set_var("WEBVIEW2_USER_DATA_FOLDER", data_dir.join("webview")) };

    // Leftovers of a previous update (downloaded package, replaced executables).
    update::cleanup(portable);

    let shared: Shared = Arc::new(Inner {
        images: Mutex::new(Vec::new()),
        activity: ActivitySlot(Mutex::new(Activity::Idle)),
        awake: chud_clone_core::awake::AwakeSwitch::new(),
        keep_awake: std::sync::atomic::AtomicBool::new(true),
        data_dir,
        reports_dir,
        portable,
        update: Mutex::new(None),
    });
    // On until the interface applies the saved setting.
    shared.awake.set(true);

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
            fix_boot,
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
            relaunch_elevated,
            update_check,
            update_install,
            open_release_page,
            shutdown_pc,
            format_check,
            format_disk,
            app_state,
            set_keep_awake
        ])
        .on_window_event(|window, event| {
            // Closing the window during a clone or a format would leave a disk half
            // written: the interface says so instead.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let state = window.state::<Shared>();
                if state.activity.get().blocks_close() {
                    api.prevent_close();
                    let _ = window.emit("close-blocked", ());
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("failed to start Chud Clone");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One activity at a time, whatever the order; `end` frees the slot.
    #[test]
    fn activities_exclude_each_other() {
        let slot = ActivitySlot(Mutex::new(Activity::Idle));
        assert_eq!(slot.get().name(), "idle");
        assert!(slot.get().busy().is_none());
        slot.begin(Activity::Scanning(Arc::new(Control::new()))).unwrap();
        assert_eq!(slot.get().name(), "scanning");
        assert!(!slot.get().blocks_close());
        for next in [Activity::Formatting, Activity::Updating, Activity::Scanning(Arc::new(Control::new()))] {
            assert!(slot.begin(next).is_err(), "nothing starts during a scan");
        }
        assert_eq!(slot.get().name(), "scanning", "a refused start changes nothing");
        slot.end();
        slot.begin(Activity::Formatting).unwrap();
        assert!(slot.get().blocks_close());
        assert!(slot.begin(Activity::Updating).is_err());
        slot.end();
        slot.begin(Activity::Cloning(RunningJob { ctl: Arc::new(Control::new()), decision: Arc::new((Mutex::new(None), Condvar::new())) })).unwrap();
        assert!(slot.get().blocks_close());
        assert_eq!(slot.get().name(), "cloning");
        slot.end();
        assert_eq!(slot.get().name(), "idle");
    }
}
