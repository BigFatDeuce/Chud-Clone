//! State of the full-screen interface and its reactions to keys and to the engine.
//!
//! Slow work (disk enumeration, analysis, SMART, clone, surface scan) runs on worker
//! threads that report through a channel; the interface thread only draws and reacts.

use chud_clone_core::engine::{Control, Decision, EngineOptions, LogLevel, Observer, Progress, UnreadableInfo, UnreadablePolicy, VerifyMode};
use chud_clone_core::i18n::{Lang, lang, set_lang};
use chud_clone_core::job::{self, JobConfig, JobError, Preparation, Target};
use chud_clone_core::model::DiskInfo;
use chud_clone_core::plan::{CloneMode, PlanOptions};
use chud_clone_core::report::{self, Report};
use chud_clone_core::scan::{CELL_UNREAD, ScanObserver, ScanOptions, ScanProgress, ScanResult};
use chud_clone_core::{t, tf};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::text::Line;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Exit codes understood by the WinPE menu (packaging/winpe/menu.cmd).
pub mod exit {
    pub const QUIT: u8 = 0;
    pub const SHELL: u8 = 20;
    pub const CLASSIC_MENU: u8 = 21;
    pub const REBOOT: u8 = 22;
    pub const SHUTDOWN: u8 = 23;
}

/// Number of cells of the surface scan map (drawn scaled to the window).
pub const SCAN_CELLS: u32 = 2400;

#[derive(Debug, Clone)]
pub struct Options {
    /// Reports folder.
    pub reports: PathBuf,
    /// Image files listed next to the disks.
    pub images: Vec<PathBuf>,
    /// Lists the disks seen by Windows (off in tests: images only).
    pub list_disks: bool,
    /// Refuses any destination that is not an image or a virtual disk.
    pub only_virtual: bool,
    /// Running from the WinPE rescue key: restart / shut down / command prompt
    /// entries, and the keyboard layout follows the language.
    pub winpe: bool,
    /// File where the chosen language is written (read back by the WinPE menu).
    pub lang_file: Option<PathBuf>,
}

/// A disk or an image that can be chosen.
#[derive(Debug, Clone)]
pub struct Entry {
    pub target: Target,
    pub info: DiskInfo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Home,
    Text,
    PickDest,
    Options,
    Review,
    Running,
    Result,
    Scan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputPurpose {
    /// Image file used as a source (must exist).
    SourceImage,
    /// Image file used as a destination (created if missing).
    DestImage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmAction {
    CancelClone,
    CancelScan,
    StartScan(usize),
}

pub enum Modal {
    Message { title: String, text: String },
    Input { title: String, prompt: String, value: String, purpose: InputPurpose },
    Confirm { title: String, text: String, yes: String, no: String, yes_selected: bool, action: ConfirmAction },
    Unreadable { info: UnreadableInfo, zero_selected: bool },
    ExitMenu { sel: usize },
}

/// Clone settings chosen on the options screen.
#[derive(Debug, Clone, Copy)]
pub struct CloneSettings {
    pub mode: CloneMode,
    pub optimize: bool,
    pub grow: bool,
    pub verify: VerifyMode,
    pub unreadable: UnreadablePolicy,
    pub retries: u32,
}

impl Default for CloneSettings {
    fn default() -> Self {
        Self {
            mode: CloneMode::Smart,
            optimize: true,
            grow: true,
            verify: VerifyMode::Hash,
            unreadable: UnreadablePolicy::Ask,
            retries: 3,
        }
    }
}

pub const OPTION_ROWS: usize = 6;

pub struct RunState {
    pub started: Instant,
    pub progress: Option<Progress>,
    pub log: Vec<(LogLevel, String)>,
    pub ctl: Arc<Control>,
    pub decisions: Option<Sender<Decision>>,
    pub cancel_requested: bool,
}

pub struct ScanState {
    pub title: String,
    pub progress: Option<ScanProgress>,
    pub cells: Vec<u8>,
    pub result: Option<ScanResult>,
    pub error: Option<String>,
    pub ctl: Arc<Control>,
    pub cancel_requested: bool,
}

pub enum Msg {
    Disks(Vec<Entry>, Vec<String>),
    Text(Result<Vec<Line<'static>>, String>),
    NewImage(InputPurpose, Result<Entry, String>),
    Prepared(Box<Result<Preparation, String>>),
    Progress(Progress),
    Log(LogLevel, String),
    Unreadable(UnreadableInfo),
    Finished(Box<Result<Report, String>>),
    ScanProgress(ScanProgress),
    ScanDone(Box<Result<ScanResult, String>>),
}

pub struct App {
    pub opts: Options,
    pub tick: u64,
    pub exit: Option<u8>,
    pub admin: bool,
    pub screen: Screen,
    pub modal: Option<Modal>,
    /// Work in progress (message shown with a spinner); keys are ignored meanwhile.
    pub busy: Option<String>,
    pub entries: Vec<Entry>,
    pub home_sel: usize,
    pub source: Option<usize>,
    pub dest_sel: usize,
    pub dest: Option<usize>,
    pub settings: CloneSettings,
    pub opt_sel: usize,
    pub prep: Option<Box<Preparation>>,
    pub confirm: String,
    pub scroll: u16,
    pub text_title: String,
    pub text: Vec<Line<'static>>,
    pub run: Option<RunState>,
    pub report: Option<Box<Report>>,
    pub run_error: Option<String>,
    pub scan: Option<ScanState>,
    /// Status line message (warning or information).
    pub notice: Option<String>,
    pub app_paths: Vec<String>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
}

/// Word to type before erasing the destination (same as the application).
pub fn confirm_word() -> &'static str {
    t!("EFFACER", "ERASE")
}

impl App {
    pub fn new(opts: Options) -> App {
        let (tx, rx) = channel();
        let mut app_paths = Vec::new();
        let _ = std::fs::create_dir_all(&opts.reports);
        app_paths.push(std::fs::canonicalize(&opts.reports).unwrap_or(opts.reports.clone()).to_string_lossy().to_string());
        if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())) {
            app_paths.push(dir.to_string_lossy().to_string());
        }
        let mut app = App {
            opts,
            tick: 0,
            exit: None,
            admin: job::is_elevated(),
            screen: Screen::Home,
            modal: None,
            busy: None,
            entries: Vec::new(),
            home_sel: 0,
            source: None,
            dest_sel: 0,
            dest: None,
            settings: CloneSettings::default(),
            opt_sel: 0,
            prep: None,
            confirm: String::new(),
            scroll: 0,
            text_title: String::new(),
            text: Vec::new(),
            run: None,
            report: None,
            run_error: None,
            scan: None,
            notice: None,
            app_paths,
            tx,
            rx,
        };
        app.refresh();
        app
    }

    fn spawn(&mut self, message: String, work: impl FnOnce() -> Msg + Send + 'static) {
        self.busy = Some(message);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(work());
        });
    }

    /// Enumerates the disks again (and the image files known so far).
    pub fn refresh(&mut self) {
        let list = self.opts.list_disks;
        let images = self.opts.images.clone();
        let paths = self.app_paths.clone();
        self.spawn(t!("Détection des disques...", "Detecting disks...").into(), move || {
            let mut out = Vec::new();
            let mut warnings = Vec::new();
            if list {
                for d in job::list_disks(true, &paths) {
                    if let Some(n) = d.number {
                        out.push(Entry { target: Target::Disk(n), info: d });
                    }
                }
            }
            for p in images {
                let target = Target::Image(p.clone());
                match job::describe(&target, true, &paths, None) {
                    Ok(info) => out.push(Entry { target, info }),
                    Err(e) => warnings.push(tf!("{} : {e}", "{}: {e}", p.display())),
                }
            }
            Msg::Disks(out, warnings)
        });
    }

    /// Handles the messages of the worker threads.
    pub fn pump(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            self.handle(msg);
        }
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Disks(list, warnings) => {
                self.busy = None;
                self.entries = list;
                self.home_sel = self.home_sel.min(self.entries.len().saturating_sub(1));
                self.notice = if warnings.is_empty() { None } else { Some(warnings.join(" · ")) };
            }
            Msg::Text(res) => {
                self.busy = None;
                match res {
                    Ok(lines) => {
                        self.text = lines;
                        self.scroll = 0;
                        self.screen = Screen::Text;
                    }
                    Err(e) => self.message(t!("Erreur", "Error"), e),
                }
            }
            Msg::NewImage(purpose, res) => {
                self.busy = None;
                match res {
                    Ok(entry) => {
                        let path = match &entry.target {
                            Target::Image(p) => p.clone(),
                            Target::Disk(_) => PathBuf::new(),
                        };
                        // Keep the image across refreshes, unless it does not exist yet.
                        if path.exists() && !self.opts.images.contains(&path) {
                            self.opts.images.push(path);
                        }
                        let idx = match self.entries.iter().position(|e| e.target == entry.target) {
                            Some(i) => {
                                self.entries[i] = entry;
                                i
                            }
                            None => {
                                self.entries.push(entry);
                                self.entries.len() - 1
                            }
                        };
                        match purpose {
                            InputPurpose::SourceImage => self.home_sel = idx,
                            InputPurpose::DestImage => {
                                self.dest = Some(idx);
                                self.opt_sel = 0;
                                self.screen = Screen::Options;
                            }
                        }
                    }
                    Err(e) => self.message(t!("Fichier image", "Image file"), e),
                }
            }
            Msg::Prepared(res) => {
                self.busy = None;
                match *res {
                    Ok(p) => {
                        self.prep = Some(Box::new(p));
                        self.confirm.clear();
                        self.scroll = 0;
                        self.screen = Screen::Review;
                    }
                    Err(e) => self.message(t!("Analyse impossible", "Analysis failed"), e),
                }
            }
            Msg::Progress(p) => {
                if let Some(r) = &mut self.run {
                    r.progress = Some(p);
                }
            }
            Msg::Log(level, text) => {
                if let Some(r) = &mut self.run {
                    r.log.push((level, text));
                    if r.log.len() > 500 {
                        r.log.drain(..100);
                    }
                }
            }
            Msg::Unreadable(info) => {
                self.modal = Some(Modal::Unreadable { info, zero_selected: false });
            }
            Msg::Finished(res) => {
                if let Some(r) = &mut self.run {
                    r.decisions = None;
                }
                match *res {
                    Ok(rep) => {
                        self.report = Some(Box::new(rep));
                        self.run_error = None;
                    }
                    Err(e) => {
                        self.report = None;
                        self.run_error = Some(e);
                    }
                }
                if matches!(self.modal, Some(Modal::Confirm { action: ConfirmAction::CancelClone, .. }) | Some(Modal::Unreadable { .. })) {
                    self.modal = None;
                }
                self.scroll = 0;
                self.screen = Screen::Result;
            }
            Msg::ScanProgress(p) => {
                if let Some(s) = &mut self.scan {
                    for &(i, c) in &p.cells {
                        if let Some(cell) = s.cells.get_mut(i as usize) {
                            *cell = c;
                        }
                    }
                    s.progress = Some(p);
                }
            }
            Msg::ScanDone(res) => {
                if let Some(s) = &mut self.scan {
                    match *res {
                        Ok(r) => {
                            s.cells = r.cells.clone();
                            s.result = Some(r);
                        }
                        Err(e) => s.error = Some(e),
                    }
                }
                if matches!(self.modal, Some(Modal::Confirm { action: ConfirmAction::CancelScan, .. })) {
                    self.modal = None;
                }
            }
        }
    }

    pub fn message(&mut self, title: &str, text: String) {
        self.modal = Some(Modal::Message { title: title.to_string(), text });
    }

    /// Indexes of the possible destinations (every entry except the source).
    pub fn dest_candidates(&self) -> Vec<usize> {
        (0..self.entries.len()).filter(|&i| Some(i) != self.source).collect()
    }

    /// Why an entry cannot be a destination (shown greyed out), if it cannot.
    pub fn dest_refusal(&self, e: &Entry) -> Option<&'static str> {
        let d = &e.info;
        if d.is_system {
            Some(t!("Windows en cours d'exécution", "running Windows"))
        } else if d.hosts_app {
            Some(t!("contient Chud Clone ou ses rapports", "holds Chud Clone or its reports"))
        } else if d.read_only {
            Some(t!("protégé en écriture", "write-protected"))
        } else if self.opts.only_virtual && !d.is_virtual() {
            Some(t!("disque physique refusé (--only-virtual)", "physical disk refused (--only-virtual)"))
        } else {
            None
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        let ctrl_c = key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'));
        if self.modal.is_some() {
            self.modal_key(key, ctrl_c);
            return;
        }
        if self.busy.is_some() {
            return;
        }
        match self.screen {
            Screen::Home => self.home_key(key, ctrl_c),
            Screen::Text => match key.code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Backspace => self.screen = Screen::Home,
                _ => self.scroll_key(key.code),
            },
            Screen::PickDest => self.pick_key(key),
            Screen::Options => self.options_key(key),
            Screen::Review => self.review_key(key),
            Screen::Running => {
                if key.code == KeyCode::Esc || ctrl_c {
                    self.ask_cancel(ConfirmAction::CancelClone);
                }
            }
            Screen::Result => match key.code {
                KeyCode::Esc | KeyCode::Enter => self.back_home(true),
                _ => self.scroll_key(key.code),
            },
            Screen::Scan => {
                let done = self.scan.as_ref().is_some_and(|s| s.result.is_some() || s.error.is_some());
                if done {
                    match key.code {
                        KeyCode::Esc | KeyCode::Enter => self.back_home(false),
                        _ => self.scroll_key(key.code),
                    }
                } else if key.code == KeyCode::Esc || ctrl_c {
                    self.ask_cancel(ConfirmAction::CancelScan);
                }
            }
        }
    }

    fn back_home(&mut self, refresh: bool) {
        self.screen = Screen::Home;
        self.prep = None;
        self.run = None;
        self.scan = None;
        self.confirm.clear();
        self.scroll = 0;
        if refresh {
            self.refresh();
        }
    }

    fn scroll_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Up => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::Down => self.scroll = self.scroll.saturating_add(1),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_add(10),
            KeyCode::Home => self.scroll = 0,
            _ => {}
        }
    }

    fn ask_cancel(&mut self, action: ConfirmAction) {
        let (title, text) = if action == ConfirmAction::CancelClone {
            (
                t!("Annuler le clonage ?", "Cancel the clone?"),
                t!(
                    "La destination restera inutilisable (sans table de partitions) tant qu'un clonage complet n'aura pas été refait. La source n'est pas touchée.",
                    "The destination will stay unusable (no partition table) until a complete clone is made again. The source is not touched."
                ),
            )
        } else {
            (t!("Arrêter le test de surface ?", "Stop the surface scan?"), t!("Rien n'a été écrit sur le disque.", "Nothing was written to the disk."))
        };
        self.modal = Some(Modal::Confirm {
            title: title.into(),
            text: text.into(),
            yes: t!("Arrêter", "Stop").into(),
            no: t!("Continuer", "Keep going").into(),
            yes_selected: false,
            action,
        });
    }

    fn home_key(&mut self, key: KeyEvent, ctrl_c: bool) {
        let n = self.entries.len();
        match key.code {
            KeyCode::Up => self.home_sel = self.home_sel.saturating_sub(1),
            KeyCode::Down => {
                if self.home_sel + 1 < n {
                    self.home_sel += 1
                }
            }
            KeyCode::Esc | KeyCode::F(10) => self.modal = Some(Modal::ExitMenu { sel: 0 }),
            _ if ctrl_c => self.modal = Some(Modal::ExitMenu { sel: 0 }),
            KeyCode::Enter if n > 0 => {
                self.source = Some(self.home_sel);
                self.dest = None;
                self.dest_sel = 0;
                self.screen = Screen::PickDest;
            }
            KeyCode::Char(c) => match c.to_ascii_lowercase() {
                'r' => self.refresh(),
                'l' => self.toggle_lang(),
                'f' => self.ask_image(InputPurpose::SourceImage),
                'h' if n > 0 => self.show_smart(self.home_sel),
                'd' if n > 0 => self.show_inspect(self.home_sel),
                't' if n > 0 => {
                    let title = self.entries[self.home_sel].info.title();
                    self.modal = Some(Modal::Confirm {
                        title: t!("Test de surface", "Surface scan").into(),
                        text: tf!(
                            "Relit toute la surface de « {title} » et mesure le temps de réponse de chaque zone. Lecture seule : rien n'est écrit. Cela peut prendre plusieurs heures ; sur un disque mourant, clonez d'abord.",
                            "Reads the whole surface of \"{title}\" and measures the response time of each area. Read-only: nothing is written. It can take hours; on a dying disk, clone first."
                        ),
                        yes: t!("Lancer", "Start").into(),
                        no: t!("Retour", "Back").into(),
                        yes_selected: true,
                        action: ConfirmAction::StartScan(self.home_sel),
                    });
                }
                _ => {}
            },
            _ => {}
        }
    }

    fn ask_image(&mut self, purpose: InputPurpose) {
        let (title, prompt) = match purpose {
            InputPurpose::SourceImage => (t!("Ouvrir un fichier image", "Open an image file"), t!("Chemin du fichier image (.img, .bin...) :", "Image file path (.img, .bin...):")),
            InputPurpose::DestImage => (
                t!("Cloner vers un fichier image", "Clone to an image file"),
                t!(
                    "Chemin du fichier image. S'il n'existe pas, il est créé à la taille de la source :",
                    "Image file path. If it does not exist, it is created with the size of the source:"
                ),
            ),
        };
        self.modal = Some(Modal::Input { title: title.into(), prompt: prompt.into(), value: String::new(), purpose });
    }

    fn submit_image(&mut self, purpose: InputPurpose, value: String) {
        let path = PathBuf::from(value.trim().trim_matches('"'));
        if path.as_os_str().is_empty() {
            return;
        }
        let paths = self.app_paths.clone();
        let size = match purpose {
            InputPurpose::DestImage => self.source.and_then(|i| self.entries.get(i)).map(|e| e.info.size),
            InputPurpose::SourceImage => None,
        };
        self.spawn(t!("Lecture du fichier image...", "Reading the image file...").into(), move || {
            if purpose == InputPurpose::SourceImage && !path.is_file() {
                return Msg::NewImage(purpose, Err(tf!("fichier introuvable : {}", "file not found: {}", path.display())));
            }
            let target = Target::Image(path);
            Msg::NewImage(purpose, job::describe(&target, true, &paths, size).map(|info| Entry { target, info }))
        });
    }

    fn show_smart(&mut self, idx: usize) {
        let info = self.entries[idx].info.clone();
        self.text_title = tf!("Santé — {}", "Health — {}", info.title());
        self.spawn(t!("Lecture de l'état de santé (SMART)...", "Reading health data (SMART)...").into(), move || {
            let s = job::read_smart(&info);
            Msg::Text(Ok(super::view::smart_lines(&info, &s)))
        });
    }

    fn show_inspect(&mut self, idx: usize) {
        let entry = self.entries[idx].clone();
        self.text_title = tf!("Analyse — {}", "Analysis — {}", entry.info.title());
        self.spawn(t!("Analyse du disque (lecture seule)...", "Analysing the disk (read-only)...").into(), move || {
            Msg::Text(job::inspect(&entry.target).map(|(analysis, boot)| super::view::inspect_lines(&entry.info, &analysis, &boot)))
        });
    }

    fn toggle_lang(&mut self) {
        let new = if lang() == Lang::Fr { Lang::En } else { Lang::Fr };
        set_lang(new);
        if self.opts.winpe {
            // The keyboard layout follows the language: French AZERTY or US QWERTY.
            let layout = if new == Lang::Fr { "040c:0000040c" } else { "0409:00000409" };
            let _ = std::process::Command::new("wpeutil")
                .args(["SetKeyboardLayout", layout])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
        if let Some(f) = &self.opts.lang_file {
            let _ = std::fs::write(f, new.code());
        }
    }

    fn pick_key(&mut self, key: KeyEvent) {
        let cands = self.dest_candidates();
        // Last row: an image file.
        let rows = cands.len() + 1;
        match key.code {
            KeyCode::Esc | KeyCode::Backspace => self.screen = Screen::Home,
            KeyCode::Up => self.dest_sel = self.dest_sel.saturating_sub(1),
            KeyCode::Down => {
                if self.dest_sel + 1 < rows {
                    self.dest_sel += 1
                }
            }
            KeyCode::Enter => {
                if self.dest_sel >= cands.len() {
                    self.ask_image(InputPurpose::DestImage);
                    return;
                }
                let idx = cands[self.dest_sel];
                if let Some(why) = self.dest_refusal(&self.entries[idx]) {
                    self.message(t!("Destination impossible", "Destination not allowed"), tf!("Ce disque ne peut pas être effacé : {why}.", "This disk cannot be erased: {why}."));
                    return;
                }
                self.dest = Some(idx);
                self.opt_sel = 0;
                self.screen = Screen::Options;
            }
            _ => {}
        }
    }

    fn options_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Backspace => self.screen = Screen::PickDest,
            KeyCode::Up => self.opt_sel = self.opt_sel.saturating_sub(1),
            KeyCode::Down => self.opt_sel = (self.opt_sel + 1).min(OPTION_ROWS - 1),
            KeyCode::Left => self.change_option(false),
            KeyCode::Right | KeyCode::Char(' ') => self.change_option(true),
            KeyCode::Enter => self.prepare(),
            _ => {}
        }
    }

    fn change_option(&mut self, forward: bool) {
        fn cycle<T: Copy + PartialEq>(list: &[T], cur: T, forward: bool) -> T {
            let i = list.iter().position(|x| *x == cur).unwrap_or(0);
            let n = list.len();
            list[if forward { (i + 1) % n } else { (i + n - 1) % n }]
        }
        let s = &mut self.settings;
        match self.opt_sel {
            0 => s.mode = cycle(&[CloneMode::Smart, CloneMode::RawPartitions, CloneMode::RawDisk], s.mode, forward),
            1 => s.optimize = !s.optimize,
            2 => s.grow = !s.grow,
            3 => s.verify = cycle(&[VerifyMode::Hash, VerifyMode::Full, VerifyMode::None], s.verify, forward),
            4 => s.unreadable = cycle(&[UnreadablePolicy::Ask, UnreadablePolicy::Stop, UnreadablePolicy::ZeroFill], s.unreadable, forward),
            _ => {
                s.retries = if forward { (s.retries + 1).min(10) } else { s.retries.saturating_sub(1) };
            }
        }
    }

    fn job_config(&self) -> Option<JobConfig> {
        let src = self.entries.get(self.source?)?;
        let dst = self.entries.get(self.dest?)?;
        let s = self.settings;
        Some(JobConfig {
            source: src.target.clone(),
            destination: dst.target.clone(),
            plan: PlanOptions { mode: s.mode, optimize_layout: s.optimize, grow_main: s.grow },
            engine: EngineOptions { verify: s.verify, unreadable: s.unreadable, read_retries: s.retries, ..Default::default() },
            new_image_size: None,
            allow_unvalidated_physical_write: false,
            expected_source_fingerprint: Some(src.info.fingerprint.clone()),
            expected_destination_fingerprint: None,
            app_paths: self.app_paths.clone(),
        })
    }

    fn prepare(&mut self) {
        let Some(cfg) = self.job_config() else { return };
        self.spawn(t!("Analyse de la source et pré-vérifications (lecture seule)...", "Analysing the source and pre-checks (read-only)...").into(), move || {
            Msg::Prepared(Box::new(job::prepare(&cfg, true)))
        });
    }

    fn review_key(&mut self, key: KeyEvent) {
        let blocked = self.prep.as_ref().is_none_or(|p| p.blocked);
        match key.code {
            KeyCode::Esc => self.screen = Screen::Options,
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown => self.scroll_key(key.code),
            KeyCode::Backspace if !blocked => {
                self.confirm.pop();
            }
            KeyCode::Char(c) if !blocked && !key.modifiers.contains(KeyModifiers::CONTROL) && self.confirm.chars().count() < 20 => self.confirm.push(c),
            KeyCode::Enter if !blocked => {
                if self.confirm.trim().eq_ignore_ascii_case(confirm_word()) {
                    self.start_clone();
                } else {
                    self.message(
                        t!("Confirmation", "Confirmation"),
                        tf!("Tapez {} puis Entrée pour lancer le clonage.", "Type {} then Enter to start the clone.", confirm_word()),
                    );
                }
            }
            _ => {}
        }
    }

    fn start_clone(&mut self) {
        let (Some(mut cfg), Some(prep)) = (self.job_config(), self.prep.as_ref()) else { return };
        // The identities shown on the review screen are checked again before writing.
        cfg.expected_source_fingerprint = Some(prep.source.fingerprint.clone());
        cfg.expected_destination_fingerprint = Some(prep.destination.fingerprint.clone());
        let ctl = Arc::new(Control::new());
        let (dtx, drx) = channel();
        self.run = Some(RunState { started: Instant::now(), progress: None, log: Vec::new(), ctl: ctl.clone(), decisions: Some(dtx), cancel_requested: false });
        self.report = None;
        self.run_error = None;
        self.screen = Screen::Running;
        let tx = self.tx.clone();
        let reports = self.opts.reports.clone();
        std::thread::spawn(move || {
            let obs = CloneObserver { tx: tx.clone(), decisions: Mutex::new(drx) };
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job::execute(&cfg, &reports, &ctl, &obs)));
            let res = match res {
                Ok(Ok(rep)) => {
                    let _ = std::fs::write(reports.join(format!("{}.txt", rep.id)), report::render_txt(&rep));
                    let _ = std::fs::write(reports.join(format!("{}.html", rep.id)), report::render_html(&rep));
                    Ok(rep)
                }
                Ok(Err(JobError::Blocked(f))) => Err(tf!(
                    "Clonage refusé par les pré-vérifications :\n{}",
                    "Clone refused by the pre-checks:\n{}",
                    f.iter().filter(|x| x.blocking).map(|x| format!("- {} : {}", x.title, x.detail)).collect::<Vec<_>>().join("\n")
                )),
                Ok(Err(JobError::Failed(m))) => Err(m),
                Err(_) => Err(t!("erreur interne du moteur (voir le journal)", "internal engine error (see the log)").into()),
            };
            let _ = tx.send(Msg::Finished(Box::new(res)));
        });
    }

    fn start_scan(&mut self, idx: usize) {
        let Some(entry) = self.entries.get(idx).cloned() else { return };
        let ctl = Arc::new(Control::new());
        self.scan = Some(ScanState {
            title: entry.info.title(),
            progress: None,
            cells: vec![CELL_UNREAD; SCAN_CELLS as usize],
            result: None,
            error: None,
            ctl: ctl.clone(),
            cancel_requested: false,
        });
        self.scroll = 0;
        self.screen = Screen::Scan;
        let tx = self.tx.clone();
        let paths = self.app_paths.clone();
        std::thread::spawn(move || {
            let opts = ScanOptions { cells: SCAN_CELLS, ..Default::default() };
            let obs = ScanSender { tx: tx.clone() };
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job::surface_scan(&entry.target, &paths, &opts, &ctl, &obs)));
            let res = match res {
                Ok(r) => r.map(|(_, r)| r),
                Err(_) => Err(t!("erreur interne pendant le test", "internal error during the scan").into()),
            };
            let _ = tx.send(Msg::ScanDone(Box::new(res)));
        });
    }

    pub fn exit_items(&self) -> Vec<(&'static str, u8)> {
        if self.opts.winpe {
            vec![
                (t!("Redémarrer l'ordinateur", "Restart the computer"), exit::REBOOT),
                (t!("Éteindre l'ordinateur", "Shut down the computer"), exit::SHUTDOWN),
                (t!("Invite de commandes", "Command prompt"), exit::SHELL),
                (t!("Menu texte classique", "Classic text menu"), exit::CLASSIC_MENU),
            ]
        } else {
            vec![(t!("Quitter Chud Clone", "Quit Chud Clone"), exit::QUIT)]
        }
    }

    fn modal_key(&mut self, key: KeyEvent, ctrl_c: bool) {
        let Some(modal) = self.modal.take() else { return };
        match modal {
            Modal::Message { .. } => {
                if !matches!(key.code, KeyCode::Enter | KeyCode::Esc | KeyCode::Char(' ')) {
                    self.modal = Some(modal);
                }
            }
            Modal::Input { title, prompt, mut value, purpose } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => self.submit_image(purpose, value),
                KeyCode::Backspace => {
                    value.pop();
                    self.modal = Some(Modal::Input { title, prompt, value, purpose });
                }
                KeyCode::Char(c) if !ctrl_c => {
                    if value.chars().count() < 260 {
                        value.push(c);
                    }
                    self.modal = Some(Modal::Input { title, prompt, value, purpose });
                }
                _ => self.modal = Some(Modal::Input { title, prompt, value, purpose }),
            },
            Modal::Confirm { title, text, yes, no, mut yes_selected, action } => match key.code {
                KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                    yes_selected = !yes_selected;
                    self.modal = Some(Modal::Confirm { title, text, yes, no, yes_selected, action });
                }
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    if yes_selected {
                        self.confirmed(action);
                    }
                }
                _ => self.modal = Some(Modal::Confirm { title, text, yes, no, yes_selected, action }),
            },
            Modal::Unreadable { info, mut zero_selected } => match key.code {
                KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                    zero_selected = !zero_selected;
                    self.modal = Some(Modal::Unreadable { info, zero_selected });
                }
                KeyCode::Enter => {
                    let d = if zero_selected { Decision::ZeroFill } else { Decision::Abort };
                    if let Some(tx) = self.run.as_ref().and_then(|r| r.decisions.as_ref()) {
                        let _ = tx.send(d);
                    }
                }
                _ => self.modal = Some(Modal::Unreadable { info, zero_selected }),
            },
            Modal::ExitMenu { mut sel } => {
                let items = self.exit_items();
                match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Up => {
                        sel = sel.saturating_sub(1);
                        self.modal = Some(Modal::ExitMenu { sel });
                    }
                    KeyCode::Down => {
                        sel = (sel + 1).min(items.len());
                        self.modal = Some(Modal::ExitMenu { sel });
                    }
                    KeyCode::Enter => {
                        // The last row is "Back".
                        if let Some((_, code)) = items.get(sel) {
                            self.exit = Some(*code);
                        }
                    }
                    _ => self.modal = Some(Modal::ExitMenu { sel }),
                }
            }
        }
    }

    fn confirmed(&mut self, action: ConfirmAction) {
        match action {
            ConfirmAction::CancelClone => {
                if let Some(r) = &mut self.run {
                    r.ctl.cancel();
                    r.cancel_requested = true;
                    // A pending decision about unreadable sectors becomes "stop".
                    if let Some(tx) = &r.decisions {
                        let _ = tx.send(Decision::Abort);
                    }
                }
            }
            ConfirmAction::CancelScan => {
                if let Some(s) = &mut self.scan {
                    s.ctl.cancel();
                    s.cancel_requested = true;
                }
            }
            ConfirmAction::StartScan(idx) => self.start_scan(idx),
        }
    }

    /// Final status of the last clone, if one finished.
    #[cfg(test)]
    pub fn final_status(&self) -> Option<chud_clone_core::clone::FinalStatus> {
        self.report.as_ref().map(|r| r.outcome.status)
    }
}

struct CloneObserver {
    tx: Sender<Msg>,
    decisions: Mutex<Receiver<Decision>>,
}

impl Observer for CloneObserver {
    fn progress(&self, p: &Progress) {
        let _ = self.tx.send(Msg::Progress(p.clone()));
    }
    fn log(&self, level: LogLevel, msg: &str) {
        if level != LogLevel::Debug {
            let _ = self.tx.send(Msg::Log(level, msg.to_string()));
        }
    }
    fn decide_unreadable(&self, info: &UnreadableInfo) -> Decision {
        let _ = self.tx.send(Msg::Unreadable(info.clone()));
        self.decisions.lock().map(|rx| rx.recv().unwrap_or(Decision::Abort)).unwrap_or(Decision::Abort)
    }
}

struct ScanSender {
    tx: Sender<Msg>,
}

impl ScanObserver for ScanSender {
    fn progress(&self, p: &ScanProgress) {
        let _ = self.tx.send(Msg::ScanProgress(p.clone()));
    }
}
