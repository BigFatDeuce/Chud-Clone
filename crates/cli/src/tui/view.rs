//! Drawing of the full-screen interface.
//!
//! Only the 16 basic console colours and characters of the classic console fonts
//! (box drawing, blocks) are used: the WinPE console has nothing more.

use super::app::{App, FormatStage, Modal, OPTION_ROWS, Screen, confirm_word};
use chud_clone_core::boot::{BootAssessment, CheckState};
use chud_clone_core::clone::FinalStatus;
use chud_clone_core::diag::{Finding, Severity, human_size};
use chud_clone_core::engine::{LogLevel, UnreadablePolicy, VerifyMode};
use chud_clone_core::model::DiskInfo;
use chud_clone_core::plan::{CopyMethod, DiskAnalysis};
use chud_clone_core::report::duration_text;
use chud_clone_core::scan::{CELL_UNREAD, CLASS_ERROR, ScanEnd, class_labels};
use chud_clone_core::smart::{HealthLevel, SmartReport};
use chud_clone_core::{t, tf};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, Gauge, List, ListItem, ListState, Paragraph, Wrap};

const ACCENT: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;
const SPINNER: [char; 4] = ['|', '/', '-', '\\'];

fn title(s: &str) -> Line<'static> {
    Line::from(Span::styled(s.to_string(), Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)))
}

fn key_hint(keys: &[(&str, &str)]) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, (k, label)) in keys.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(format!("[{k}]"), Style::new().fg(Color::Black).bg(ACCENT)));
        spans.push(Span::raw(format!(" {label}")));
    }
    Line::from(spans)
}

fn severity_color(s: Severity) -> Color {
    match s {
        Severity::Info => ACCENT,
        Severity::Warning => Color::Yellow,
        Severity::Error => Color::LightRed,
        Severity::Critical => Color::LightMagenta,
    }
}

pub fn status_color(s: FinalStatus) -> Color {
    match s {
        FinalStatus::SuccessVerified => Color::LightGreen,
        FinalStatus::CompletedWithWarnings => Color::Yellow,
        FinalStatus::Incomplete => Color::LightRed,
        FinalStatus::Failed => Color::LightRed,
    }
}

fn finding_lines(out: &mut Vec<Line<'static>>, list: &[Finding]) {
    for f in list {
        let mut head = vec![Span::styled(
            format!("[{}{}] ", f.severity.label(), if f.blocking { t!(", BLOQUANT", ", BLOCKING") } else { "" }),
            Style::new().fg(severity_color(f.severity)).add_modifier(Modifier::BOLD),
        )];
        head.push(Span::styled(f.title.clone(), Style::new().add_modifier(Modifier::BOLD)));
        if let Some(s) = &f.scope {
            head.push(Span::styled(format!(" ({s})"), Style::new().fg(DIM)));
        }
        out.push(Line::from(head));
        out.push(Line::from(format!("    {}", f.detail)));
    }
}

/// Short tags of a disk: running system, read-only...
fn disk_tags(d: &DiskInfo) -> Vec<&'static str> {
    let mut tags = Vec::new();
    if d.is_system {
        tags.push(t!("SYSTÈME", "SYSTEM"));
    }
    if d.hosts_app {
        tags.push(t!("Chud Clone", "Chud Clone"));
    }
    if d.read_only {
        tags.push(t!("lecture seule", "read-only"));
    }
    if d.offline {
        tags.push(t!("hors ligne", "offline"));
    }
    if d.removable {
        tags.push(t!("amovible", "removable"));
    }
    tags
}

/// One line per disk in the lists.
fn disk_item(d: &DiskInfo, disabled: Option<&str>) -> ListItem<'static> {
    let style = if disabled.is_some() { Style::new().fg(DIM) } else { Style::new() };
    let mut spans = vec![
        Span::styled(d.title(), style.add_modifier(Modifier::BOLD)),
        Span::styled(format!("  {}  {}", human_size(d.size), d.bus), style),
    ];
    let tags = disk_tags(d);
    if !tags.is_empty() {
        spans.push(Span::styled(format!("  [{}]", tags.join(", ")), Style::new().fg(if d.is_system { Color::Yellow } else { DIM })));
    }
    let mut lines = vec![Line::from(spans)];
    if let Some(why) = disabled {
        lines.push(Line::from(Span::styled(format!("   {why}"), Style::new().fg(DIM))));
    }
    ListItem::new(lines)
}

/// Description of a disk: identity, then partitions.
pub fn disk_lines(d: &DiskInfo) -> Vec<Line<'static>> {
    let mut out = vec![title(&d.title())];
    out.push(Line::from(format!("{} · {} · {} · {}", human_size(d.size), d.bus, d.media.label(), d.table.label())));
    if !d.serial.is_empty() {
        out.push(Line::from(tf!("N° de série : {}  ·  Micrologiciel : {}", "Serial number: {}  ·  Firmware: {}", d.serial, d.firmware)));
    }
    out.push(Line::from(tf!("Secteurs : {} / {} octets", "Sectors: {} / {} bytes", d.sector_size, d.physical_sector_size)));
    if let Some(u) = d.used_bytes {
        out.push(Line::from(tf!("Espace utilisé : {}", "Space used: {}", human_size(u))));
    }
    let tags = disk_tags(d);
    if !tags.is_empty() {
        out.push(Line::from(Span::styled(tags.join(" · "), Style::new().fg(Color::Yellow))));
    }
    if d.is_system {
        out.push(Line::from(Span::styled(
            t!(
                "Disque du Windows en cours d'exécution : il ne peut être ni source ni destination.",
                "Disk of the running Windows: it can be neither source nor destination."
            ),
            Style::new().fg(Color::Yellow),
        )));
    }
    out.push(Line::from(""));
    if d.partitions.is_empty() {
        out.push(Line::from(Span::styled(t!("Aucune partition.", "No partition."), Style::new().fg(DIM))));
    }
    for p in &d.partitions {
        let mut spans = vec![
            Span::styled(format!("{:>2}. ", p.number), Style::new().fg(DIM)),
            Span::styled(p.type_label.clone(), Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(format!("  {}", human_size(p.size))),
        ];
        if let Some(fs) = &p.fs {
            spans.push(Span::styled(format!("  {fs}"), Style::new().fg(ACCENT)));
        }
        if !p.mounts.is_empty() {
            spans.push(Span::raw(format!("  {}", p.mounts.join(" "))));
        }
        if let Some(l) = &p.label {
            spans.push(Span::raw(tf!("  « {l} »", "  \"{l}\"")));
        }
        if let Some(u) = p.used_bytes {
            spans.push(Span::styled(tf!("  ({} utilisés)", "  ({} used)", human_size(u)), Style::new().fg(DIM)));
        }
        out.push(Line::from(spans));
    }
    for n in &d.notes {
        out.push(Line::from(Span::styled(format!("! {n}"), Style::new().fg(Color::Yellow))));
    }
    out
}

pub fn smart_lines(d: &DiskInfo, s: &SmartReport) -> Vec<Line<'static>> {
    let mut out = vec![title(&d.title()), Line::from("")];
    if !s.available {
        out.push(Line::from(tf!(
            "Données de santé indisponibles : {}",
            "Health data unavailable: {}",
            s.unavailable_reason.clone().unwrap_or_default()
        )));
        return out;
    }
    let color = match s.level {
        HealthLevel::Good => Color::LightGreen,
        HealthLevel::Watch => Color::Yellow,
        HealthLevel::Bad => Color::LightRed,
        HealthLevel::Unknown => DIM,
    };
    out.push(Line::from(vec![
        Span::raw(format!("{} : ", s.protocol)),
        Span::styled(s.level.label().to_string(), Style::new().fg(color).add_modifier(Modifier::BOLD)),
        Span::styled(t!("  (indicateur, pas une certitude)", "  (indicator, not a certainty)").to_string(), Style::new().fg(DIM)),
    ]));
    if let Some(t) = s.temperature_c {
        out.push(Line::from(tf!("Température : {t} °C", "Temperature: {t} °C")));
    }
    if let Some(h) = s.power_on_hours {
        out.push(Line::from(tf!("Fonctionnement : {h} h", "Power-on time: {h} h")));
    }
    if let Some(c) = s.power_cycles {
        out.push(Line::from(tf!("Mises sous tension : {c}", "Power cycles: {c}")));
    }
    if let Some(w) = s.wear_percent_used {
        out.push(Line::from(tf!("Usure estimée : {w} %", "Estimated wear: {w} %")));
    }
    if !s.findings.is_empty() {
        out.push(Line::from(""));
        finding_lines(&mut out, &s.findings);
    }
    if !s.attributes.is_empty() {
        out.push(Line::from(""));
        out.push(Line::from(Span::styled(
            tf!(
                " ID  {:<34} {:>6} {:>5} {:>5}  {}",
                " ID  {:<34} {:>6} {:>5} {:>5}  {}",
                "Attribut",
                "Valeur",
                "Pire",
                "Seuil",
                "Brut"
            )
            .replace("Attribut", t!("Attribut", "Attribute"))
            .replace("Valeur", t!("Valeur", " Value"))
            .replace("Pire", t!("Pire", "Worst"))
            .replace("Seuil", t!("Seuil", "Thr."))
            .replace("Brut", t!("Brut", "Raw")),
            Style::new().fg(DIM),
        )));
        for a in &s.attributes {
            let style = if a.below_threshold { Style::new().fg(Color::LightRed) } else { Style::new() };
            let mut name = a.name.clone();
            if name.chars().count() > 34 {
                name = name.chars().take(33).collect::<String>() + "...";
            }
            out.push(Line::from(Span::styled(
                format!(
                    "{:>3}  {:<34} {:>6} {:>5} {:>5}  {}",
                    a.id,
                    name,
                    a.current,
                    a.worst,
                    a.threshold.map(|t| t.to_string()).unwrap_or_else(|| "-".into()),
                    a.raw
                ),
                style,
            )));
        }
    }
    out
}

pub fn inspect_lines(d: &DiskInfo, analysis: &DiskAnalysis, boot: &BootAssessment) -> Vec<Line<'static>> {
    let mut out = disk_lines(d);
    out.push(Line::from(""));
    out.push(title(&tf!("Démarrage : {}", "Boot: {}", boot.style.label())));
    for c in &boot.checks {
        let (mark, color) = match c.state {
            CheckState::Present => ("OK ", Color::LightGreen),
            CheckState::Missing => (if c.required { "!! " } else { "-- " }, if c.required { Color::LightRed } else { DIM }),
            CheckState::Unknown => ("?? ", Color::Yellow),
        };
        out.push(Line::from(vec![Span::styled(mark.to_string(), Style::new().fg(color)), Span::raw(c.label.clone())]));
    }
    let mut f = analysis.all_findings();
    f.extend(boot.findings.clone());
    out.push(Line::from(""));
    out.push(title(t!("Constats", "Findings")));
    if f.is_empty() {
        out.push(Line::from(t!("Aucun.", "None.")));
    }
    finding_lines(&mut out, &f);
    out
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    if area.width < 60 || area.height < 18 {
        f.render_widget(
            Paragraph::new(t!("Fenêtre trop petite : agrandissez-la (60 × 18 minimum).", "Window too small: enlarge it (60 × 18 minimum)."))
                .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }
    let [head, body, foot] = Layout::vertical([Constraint::Length(2), Constraint::Min(5), Constraint::Length(3)]).areas(area);
    draw_header(f, app, head);
    match app.screen {
        Screen::Home => draw_home(f, app, body),
        Screen::Text => {
            let lines = app.text.clone();
            let t = app.text_title.clone();
            draw_scrolled(f, app, body, &t, lines);
        }
        Screen::PickDest => draw_pick(f, app, body),
        Screen::Options => draw_options(f, app, body),
        Screen::Review => draw_review(f, app, body),
        Screen::Running => draw_running(f, app, body),
        Screen::Result => draw_result(f, app, body),
        Screen::Scan => draw_scan(f, app, body),
        Screen::Format => draw_format(f, app, body),
    }
    draw_footer(f, app, foot);
    if let Some(msg) = &app.busy {
        let text = format!("{}  {msg}", SPINNER[(app.tick as usize / 2) % 4]);
        let r = centered(area, (text.chars().count() as u16 + 6).min(area.width - 4), 3);
        f.render_widget(Clear, r);
        f.render_widget(Paragraph::new(text).block(Block::bordered().border_style(Style::new().fg(ACCENT))), r);
    }
    if app.modal.is_some() {
        draw_modal(f, app, area);
    }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let screen = match app.screen {
        Screen::Home => t!("Disques", "Disks"),
        Screen::Text => t!("Détails", "Details"),
        Screen::PickDest => t!("Clonage 2/4 : destination", "Clone 2/4: destination"),
        Screen::Options => t!("Clonage 3/4 : options", "Clone 3/4: options"),
        Screen::Review => t!("Clonage 4/4 : vérification et confirmation", "Clone 4/4: review and confirmation"),
        Screen::Running => t!("Clonage en cours", "Cloning"),
        Screen::Result => t!("Résultat du clonage", "Clone result"),
        Screen::Scan => t!("Test de surface", "Surface scan"),
        Screen::Format => t!("Formater un disque", "Format a disk"),
    };
    let mut l1 = vec![
        Span::styled(format!(" CHUD CLONE {} ", chud_clone_core::APP_VERSION), Style::new().fg(Color::Black).bg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled(format!("  {screen}"), Style::new().add_modifier(Modifier::BOLD)),
    ];
    if !app.admin {
        l1.push(Span::styled(t!("   sans droits administrateur", "   no administrator rights").to_string(), Style::new().fg(Color::Yellow)));
    }
    let l2 = Line::from(Span::styled(
        tf!(" Rapports : {}", " Reports: {}", app.opts.reports.display()),
        Style::new().fg(DIM),
    ));
    f.render_widget(Paragraph::new(vec![Line::from(l1), l2]), area);
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let lang = t!("Language: English", "Langue : français");
    let keys: Vec<(&str, &str)> = if app.modal.is_some() {
        vec![]
    } else {
        match app.screen {
            Screen::Home => vec![
                (t!("Entrée", "Enter"), t!("Cloner ce disque", "Clone this disk")),
                ("H", t!("Santé SMART", "SMART health")),
                ("D", t!("Analyse", "Analysis")),
                ("T", t!("Test de surface", "Surface scan")),
                ("M", t!("Formater", "Format")),
                ("F", t!("Fichier image", "Image file")),
                ("R", t!("Actualiser", "Refresh")),
                ("L", lang),
                (t!("Échap", "Esc"), t!("Quitter", "Quit")),
            ],
            Screen::Text => vec![("↑↓ PgUp PgDn", t!("Défiler", "Scroll")), (t!("Échap", "Esc"), t!("Retour", "Back"))],
            Screen::PickDest => vec![("↑↓", t!("Choisir", "Choose")), (t!("Entrée", "Enter"), t!("Valider", "Select")), (t!("Échap", "Esc"), t!("Retour", "Back"))],
            Screen::Options => vec![
                ("↑↓", t!("Option", "Option")),
                ("← →", t!("Changer", "Change")),
                (t!("Entrée", "Enter"), t!("Analyser (rien n'est écrit)", "Analyse (nothing is written)")),
                (t!("Échap", "Esc"), t!("Retour", "Back")),
            ],
            Screen::Review => {
                if app.prep.as_ref().is_some_and(|p| p.blocked) {
                    vec![("↑↓ PgUp PgDn", t!("Défiler", "Scroll")), (t!("Échap", "Esc"), t!("Retour aux options", "Back to options"))]
                } else {
                    vec![
                        ("↑↓ PgUp PgDn", t!("Défiler", "Scroll")),
                        (t!("Entrée", "Enter"), t!("Lancer le clonage", "Start the clone")),
                        (t!("Échap", "Esc"), t!("Retour aux options", "Back to options")),
                    ]
                }
            }
            Screen::Running => vec![(t!("Échap", "Esc"), t!("Annuler le clonage", "Cancel the clone"))],
            Screen::Format => {
                if app.fmt.as_ref().is_some_and(|x| matches!(x.stage, FormatStage::Running)) {
                    vec![]
                } else if app.fmt.as_ref().is_some_and(|x| matches!(x.stage, FormatStage::Done(_))) {
                    vec![(t!("Entrée", "Enter"), t!("Retour aux disques", "Back to the disks"))]
                } else {
                    vec![("↑↓", t!("Ligne", "Row")), ("← →", t!("Changer", "Change")), (t!("Entrée", "Enter"), t!("Valider / formater", "Next / format")), (t!("Échap", "Esc"), t!("Retour", "Back"))]
                }
            }
            Screen::Result if app.shutdown_at.is_some() => vec![(t!("Échap", "Esc"), t!("Annuler l'extinction", "Cancel the shutdown"))],
            Screen::Result => vec![("↑↓", t!("Défiler", "Scroll")), (t!("Entrée", "Enter"), t!("Retour aux disques", "Back to the disks"))],
            Screen::Scan => {
                if app.scan.as_ref().is_some_and(|s| s.result.is_some() || s.error.is_some()) {
                    vec![("↑↓", t!("Défiler", "Scroll")), (t!("Entrée", "Enter"), t!("Retour aux disques", "Back to the disks"))]
                } else {
                    vec![(t!("Échap", "Esc"), t!("Arrêter le test", "Stop the scan"))]
                }
            }
        }
    };
    let mut lines = vec![];
    if let Some(at) = app.shutdown_at {
        let left = at.saturating_duration_since(std::time::Instant::now()).as_secs();
        lines.push(Line::from(Span::styled(
            tf!("Extinction de l'ordinateur dans {left} s.", "The computer shuts down in {left} s."),
            Style::new().fg(Color::LightRed).add_modifier(Modifier::BOLD),
        )));
    }
    if let Some(n) = &app.notice {
        lines.push(Line::from(Span::styled(n.clone(), Style::new().fg(Color::Yellow))));
    }
    lines.push(key_hint(&keys));
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }).block(Block::new().borders(ratatui::widgets::Borders::TOP).border_style(Style::new().fg(DIM))), area);
}

fn draw_home(f: &mut Frame, app: &mut App, area: Rect) {
    let [left, right] = Layout::horizontal([Constraint::Percentage(42), Constraint::Percentage(58)]).areas(area);
    let items: Vec<ListItem> = app.entries.iter().map(|e| disk_item(&e.info, None)).collect();
    let block = Block::bordered().title(t!(" 1/4 : disque SOURCE ", " 1/4: SOURCE disk ")).border_style(Style::new().fg(ACCENT));
    if items.is_empty() && app.busy.is_none() {
        f.render_widget(
            Paragraph::new(t!(
                "Aucun disque détecté. [R] pour actualiser, [F] pour ouvrir un fichier image.",
                "No disk detected. [R] to refresh, [F] to open an image file."
            ))
            .wrap(Wrap { trim: true })
            .block(block),
            left,
        );
    } else {
        let list = List::new(items).block(block).highlight_style(Style::new().bg(Color::Blue).fg(Color::White)).highlight_symbol("> ");
        let mut st = ListState::default().with_selected(Some(app.home_sel));
        f.render_stateful_widget(list, left, &mut st);
    }
    let detail = app.entries.get(app.home_sel).map(|e| disk_lines(&e.info)).unwrap_or_default();
    f.render_widget(Paragraph::new(detail).wrap(Wrap { trim: false }).block(Block::bordered()), right);
}

fn draw_pick(f: &mut Frame, app: &mut App, area: Rect) {
    let src = app.source.and_then(|i| app.entries.get(i)).map(|e| e.info.title()).unwrap_or_default();
    let [top, list_area] = Layout::vertical([Constraint::Length(2), Constraint::Min(3)]).areas(area);
    f.render_widget(
        Paragraph::new(vec![
            Line::from(vec![Span::raw(t!("Source : ", "Source: ")), Span::styled(src, Style::new().fg(Color::LightGreen).add_modifier(Modifier::BOLD))]),
            Line::from(Span::styled(
                t!("Choisissez la DESTINATION. Tout son contenu sera effacé.", "Choose the DESTINATION. All of its content will be erased."),
                Style::new().fg(Color::Yellow),
            )),
        ]),
        top,
    );
    let mut items: Vec<ListItem> = app.dest_candidates().iter().map(|&i| disk_item(&app.entries[i].info, app.dest_refusal(&app.entries[i]))).collect();
    items.push(ListItem::new(Line::from(Span::styled(
        t!("+ Fichier image (nouveau ou existant)...", "+ Image file (new or existing)..."),
        Style::new().fg(ACCENT),
    ))));
    let list = List::new(items)
        .block(Block::bordered().title(t!(" Destination ", " Destination ")).border_style(Style::new().fg(Color::Yellow)))
        .highlight_style(Style::new().bg(Color::Blue).fg(Color::White))
        .highlight_symbol("> ");
    let mut st = ListState::default().with_selected(Some(app.dest_sel));
    f.render_stateful_widget(list, list_area, &mut st);
}

fn option_rows(app: &App) -> Vec<(&'static str, String, &'static str)> {
    let s = app.settings;
    let yes_no = |b: bool| if b { t!("oui", "yes") } else { t!("non", "no") }.to_string();
    vec![
        (
            t!("Méthode de copie", "Copy method"),
            s.mode.label().to_string(),
            t!(
                "Intelligent : seuls les blocs utilisés des systèmes de fichiers fiables sont copiés (rapide). Les autres méthodes copient tout, secteur par secteur.",
                "Smart: only the used blocks of trusted file systems are copied (fast). The other methods copy everything, sector by sector."
            ),
        ),
        (
            t!("Déplacer les partitions de fin", "Move trailing partitions"),
            yes_no(s.optimize),
            t!(
                "Sur un disque plus grand, place la partition de récupération à la fin pour libérer de l'espace après la partition Windows.",
                "On a larger disk, puts the recovery partition at the end to free space after the Windows partition."
            ),
        ),
        (
            t!("Agrandir la partition Windows", "Grow the Windows partition"),
            yes_no(s.grow),
            t!(
                "Étend la partition NTFS principale sur l'espace ajouté, après la copie et sa vérification.",
                "Extends the main NTFS partition over the added space, after the copy and its verification."
            ),
        ),
        (
            t!("Vérification", "Verification"),
            match s.verify {
                VerifyMode::Hash => t!("relecture (empreintes)", "read-back (checksums)"),
                VerifyMode::Full => t!("comparaison complète", "full comparison"),
                VerifyMode::None => t!("aucune", "none"),
            }
            .to_string(),
            s.verify.label(),
        ),
        (
            t!("Secteurs illisibles", "Unreadable sectors"),
            match s.unreadable {
                UnreadablePolicy::Ask => t!("me demander (recommandé)", "ask me (recommended)"),
                UnreadablePolicy::Stop => t!("arrêter", "stop"),
                UnreadablePolicy::ZeroFill => t!("remplacer par des zéros", "replace with zeros"),
            }
            .to_string(),
            t!(
                "Rien n'est ignoré en silence : chaque secteur remplacé est listé dans le rapport et le clone est déclaré incomplet.",
                "Nothing is skipped silently: every replaced sector is listed in the report and the clone is declared incomplete."
            ),
        ),
        (
            t!("Nouvelles lectures par secteur", "Re-reads per sector"),
            s.retries.to_string(),
            t!("Tentatives supplémentaires sur un secteur difficile à lire (0 à 10).", "Extra attempts on a sector that is hard to read (0 to 10)."),
        ),
        (
            t!("Éjecter la destination à la fin", "Eject the destination when done"),
            yes_no(s.eject),
            t!(
                "Après un clonage réussi, le disque de destination est retiré proprement (disques USB, boîtiers, docks).",
                "After a successful clone, the destination disk is safely removed (USB disks, enclosures, docks)."
            ),
        ),
        (
            t!("Éteindre l'ordinateur à la fin", "Shut the computer down when done"),
            yes_no(s.shutdown),
            t!(
                "L'ordinateur s'éteint 60 secondes après la fin du clonage ; Échap annule. Le rapport est déjà enregistré.",
                "The computer shuts down 60 seconds after the end of the clone; Esc cancels. The report is already saved."
            ),
        ),
    ]
}

fn draw_options(f: &mut Frame, app: &mut App, area: Rect) {
    let src = app.source.and_then(|i| app.entries.get(i)).map(|e| e.info.title()).unwrap_or_default();
    let dst = app.dest.and_then(|i| app.entries.get(i)).map(|e| e.info.title()).unwrap_or_default();
    let rows = option_rows(app);
    let mut lines = vec![
        Line::from(vec![Span::raw(t!("Source :      ", "Source:      ")), Span::styled(src, Style::new().fg(Color::LightGreen).add_modifier(Modifier::BOLD))]),
        Line::from(vec![Span::raw(t!("Destination : ", "Destination: ")), Span::styled(dst, Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD))]),
        Line::from(""),
    ];
    for (i, (label, value, _)) in rows.iter().enumerate() {
        let sel = i == app.opt_sel;
        let style = if sel { Style::new().bg(Color::Blue).fg(Color::White) } else { Style::new() };
        lines.push(Line::from(vec![
            Span::styled(format!("{} {:<34}", if sel { ">" } else { " " }, label), style),
            Span::styled(format!(" < {value} > "), if sel { style.add_modifier(Modifier::BOLD) } else { Style::new().fg(ACCENT) }),
        ]));
    }
    lines.push(Line::from(""));
    if let Some((_, _, help)) = rows.get(app.opt_sel.min(OPTION_ROWS - 1)) {
        lines.push(Line::from(Span::styled(help.to_string(), Style::new().fg(DIM))));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(Block::bordered().border_style(Style::new().fg(ACCENT))), area);
}

fn method_text(m: CopyMethod) -> &'static str {
    match m {
        CopyMethod::Smart => t!("intelligente", "smart"),
        CopyMethod::Raw => t!("intégrale", "full"),
        CopyMethod::Skip => t!("ignorée", "skipped"),
    }
}

fn draw_review(f: &mut Frame, app: &mut App, area: Rect) {
    let Some(prep) = app.prep.as_ref() else { return };
    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("SOURCE       ", Style::new().fg(Color::LightGreen).add_modifier(Modifier::BOLD)),
        Span::raw(format!("{}  ({})", prep.source.title(), human_size(prep.source.size))),
    ]));
    lines.push(Line::from(vec![
        Span::styled("DESTINATION  ", Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        Span::raw(format!("{}  ({})", prep.destination.title(), human_size(prep.destination.size))),
    ]));
    if !prep.destination.partitions.is_empty() {
        lines.push(Line::from(Span::styled(
            tf!(
                "             contient actuellement {} partition(s){} : ELLES SERONT EFFACÉES",
                "             currently holds {} partition(s){}: THEY WILL BE ERASED",
                prep.destination.partitions.len(),
                if prep.destination.mounts().is_empty() { String::new() } else { format!(" ({})", prep.destination.mounts().join(" ")) }
            ),
            Style::new().fg(Color::LightRed),
        )));
    }
    lines.push(Line::from(""));
    lines.push(title(&tf!("Plan : {}", "Plan: {}", prep.plan.mode.label())));
    for p in &prep.plan.partitions {
        let mut spans = vec![
            Span::styled(format!("{:>2}. ", p.number), Style::new().fg(DIM)),
            Span::styled(p.type_label.clone(), Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(format!("  {}  {}", human_size(p.src_len), p.fs.label())),
            Span::styled(tf!("  copie {}", "  {} copy", method_text(p.method)), Style::new().fg(ACCENT)),
            Span::raw(tf!(" : {}", ": {}", human_size(p.copy_bytes))),
        ];
        if p.dst_len != p.src_len {
            spans.push(Span::styled(tf!("  -> agrandie à {}", "  -> grown to {}", human_size(p.dst_len)), Style::new().fg(Color::LightGreen)));
        }
        if p.moved {
            spans.push(Span::styled(t!("  (déplacée)", "  (moved)").to_string(), Style::new().fg(DIM)));
        }
        lines.push(Line::from(spans));
        if let Some(r) = &p.method_reason {
            lines.push(Line::from(Span::styled(format!("      {r}"), Style::new().fg(DIM))));
        }
    }
    lines.push(Line::from(tf!(
        "Total à copier : {} sur {}",
        "Total to copy: {} of {}",
        human_size(prep.plan.total_copy_bytes),
        human_size(prep.plan.src_size)
    )));
    lines.push(Line::from(tf!("Démarrage de la source : {}", "Source boot: {}", prep.boot.style.label())));
    lines.push(Line::from(""));
    lines.push(title(t!("Pré-vérifications", "Pre-checks")));
    if prep.findings.is_empty() {
        lines.push(Line::from(t!("Aucun problème trouvé.", "No problem found.")));
    }
    finding_lines(&mut lines, &prep.findings);

    let blocked = prep.blocked;
    let dest_title = prep.destination.title();
    let [top, bottom] = Layout::vertical([Constraint::Min(3), Constraint::Length(4)]).areas(area);
    draw_scrolled(f, app, top, "", lines);
    let bottom_text = if blocked {
        vec![
            Line::from(Span::styled(t!("CLONAGE BLOQUÉ", "CLONE BLOCKED"), Style::new().fg(Color::LightRed).add_modifier(Modifier::BOLD))),
            Line::from(t!(
                "Les problèmes marqués BLOQUANT ci-dessus doivent être réglés. Rien n'a été écrit.",
                "The problems marked BLOCKING above must be solved. Nothing was written."
            )),
        ]
    } else {
        let cursor = if (app.tick / 4).is_multiple_of(2) { "_" } else { " " };
        vec![
            Line::from(Span::styled(
                tf!(
                    "Tout le contenu de « {dest_title} » va être effacé. Tapez {} puis Entrée :",
                    "All content of \"{dest_title}\" will be erased. Type {} then Enter:",
                    confirm_word()
                ),
                Style::new().fg(Color::Yellow),
            )),
            Line::from(vec![Span::raw("> "), Span::styled(app.confirm.clone(), Style::new().add_modifier(Modifier::BOLD)), Span::raw(cursor)]),
        ]
    };
    f.render_widget(
        Paragraph::new(bottom_text)
            .wrap(Wrap { trim: true })
            .block(Block::bordered().border_style(Style::new().fg(if blocked { Color::LightRed } else { Color::Yellow }))),
        bottom,
    );
}

fn draw_scrolled(f: &mut Frame, app: &mut App, area: Rect, title_text: &str, lines: Vec<Line<'static>>) {
    let max = (lines.len() as u16).saturating_sub(area.height.saturating_sub(2));
    app.scroll = app.scroll.min(max);
    let mut block = Block::bordered();
    if !title_text.is_empty() {
        block = block.title(format!(" {title_text} "));
    }
    f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).scroll((app.scroll, 0)).block(block), area);
}

fn draw_running(f: &mut Frame, app: &mut App, area: Rect) {
    let Some(run) = app.run.as_ref() else { return };
    let prep = app.prep.as_ref();
    let names = prep.map(|p| format!("{}  ->  {}", p.source.title(), p.destination.title())).unwrap_or_default();
    let [top, gauge_area, stats, log_area] =
        Layout::vertical([Constraint::Length(2), Constraint::Length(3), Constraint::Length(4), Constraint::Min(3)]).areas(area);
    let phase = run.progress.as_ref().map(|p| p.phase.label()).unwrap_or(t!("Démarrage...", "Starting..."));
    let mut head = vec![Line::from(Span::styled(names, Style::new().add_modifier(Modifier::BOLD)))];
    head.push(Line::from(vec![
        Span::styled(phase.to_string(), Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled(
            if run.cancel_requested { t!("   annulation demandée...", "   cancellation requested...") } else { "" }.to_string(),
            Style::new().fg(Color::Yellow),
        ),
    ]));
    f.render_widget(Paragraph::new(head), top);
    let (ratio, label) = match &run.progress {
        Some(p) if p.total > 0 => {
            let r = (p.done as f64 / p.total as f64).clamp(0.0, 1.0);
            (r, format!("{:.1} %", r * 100.0))
        }
        _ => (0.0, String::new()),
    };
    f.render_widget(
        Gauge::default().block(Block::bordered()).gauge_style(Style::new().fg(Color::LightGreen).bg(Color::Black)).ratio(ratio).label(label),
        gauge_area,
    );
    let mut st = Vec::new();
    if let Some(p) = &run.progress {
        st.push(Line::from(tf!(
            "{} / {}   ·   {}/s   ·   reste {}",
            "{} / {}   ·   {}/s   ·   {} left",
            human_size(p.done),
            human_size(p.total),
            human_size(p.speed_bps),
            p.eta_secs.map(|s| duration_text(s * 1000)).unwrap_or_else(|| "?".into())
        )));
        let err_style = if p.read_errors + p.write_errors > 0 { Style::new().fg(Color::LightRed) } else { Style::new() };
        st.push(Line::from(Span::styled(
            tf!(
                "Erreurs de lecture : {}   ·   illisible : {}   ·   récupéré : {}   ·   erreurs d'écriture : {}",
                "Read errors: {}   ·   unreadable: {}   ·   recovered: {}   ·   write errors: {}",
                p.read_errors,
                human_size(p.bad_bytes),
                human_size(p.recovered_bytes),
                p.write_errors
            ),
            err_style,
        )));
    }
    st.push(Line::from(Span::styled(
        tf!("Durée : {}", "Elapsed: {}", duration_text(run.started.elapsed().as_millis() as u64)),
        Style::new().fg(DIM),
    )));
    f.render_widget(Paragraph::new(st).wrap(Wrap { trim: true }), stats);
    let visible = log_area.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = run
        .log
        .iter()
        .rev()
        .take(visible)
        .rev()
        .map(|(lvl, m)| {
            let color = match lvl {
                LogLevel::Warning => Color::Yellow,
                LogLevel::Error => Color::LightRed,
                _ => Color::Gray,
            };
            Line::from(Span::styled(m.clone(), Style::new().fg(color)))
        })
        .collect();
    f.render_widget(Paragraph::new(lines).block(Block::bordered().title(t!(" Journal ", " Log ")).border_style(Style::new().fg(DIM))), log_area);
}

fn draw_result(f: &mut Frame, app: &mut App, area: Rect) {
    let mut lines = Vec::new();
    if let Some(rep) = app.report.as_ref() {
        let o = &rep.outcome;
        lines.push(Line::from(Span::styled(
            format!("   {}   ", o.status.label().to_uppercase()),
            Style::new().fg(Color::Black).bg(status_color(o.status)).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(format!("{}  ->  {}", rep.source.title(), rep.destination.title())));
        for r in &o.reasons {
            lines.push(Line::from(format!("- {r}")));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(tf!(
            "Copié : {}   ·   durée : {}   ·   vitesse moyenne : {}/s",
            "Copied: {}   ·   time: {}   ·   average speed: {}/s",
            human_size(o.copy.bytes_copied),
            duration_text(o.duration_ms),
            human_size(o.copy.avg_bps)
        )));
        if !o.bad_ranges.is_empty() {
            let sectors: u64 = o.bad_ranges.iter().map(|b| b.sectors).sum();
            lines.push(Line::from(Span::styled(
                tf!("Secteurs illisibles : {sectors} (liste complète dans le rapport)", "Unreadable sectors: {sectors} (full list in the report)"),
                Style::new().fg(Color::LightRed),
            )));
        }
        if let Some(g) = &o.grow
            && g.done
        {
            lines.push(Line::from(Span::styled(
                tf!("Partition {} agrandie.", "Partition {} grown.", g.number),
                Style::new().fg(Color::LightGreen),
            )));
        }
        let mut findings = o.structure_findings.clone();
        findings.extend(o.boot_findings.clone());
        if !findings.is_empty() {
            lines.push(Line::from(""));
            finding_lines(&mut lines, &findings);
        }
        lines.push(Line::from(""));
        let base = app.opts.reports.join(&rep.id);
        lines.push(Line::from(tf!("Rapport : {}.txt (.html, .json)", "Report: {}.txt (.html, .json)", base.display())));
        if o.status == FinalStatus::SuccessVerified || o.status == FinalStatus::CompletedWithWarnings {
            lines.push(Line::from(Span::styled(
                t!(
                    "Gardez la source intacte tant que le clone n'a pas été vérifié en usage réel.",
                    "Keep the source intact until the clone has been checked in real use."
                ),
                Style::new().fg(DIM),
            )));
        }
    } else {
        lines.push(Line::from(Span::styled(
            format!("   {}   ", FinalStatus::Failed.label().to_uppercase()),
            Style::new().fg(Color::Black).bg(Color::LightRed).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
        for l in app.run_error.clone().unwrap_or_default().lines() {
            lines.push(Line::from(l.to_string()));
        }
    }
    draw_scrolled(f, app, area, "", lines);
}

fn class_color(c: u8) -> Color {
    match c {
        0 => Color::Green,
        1 => Color::LightGreen,
        2 => Color::LightCyan,
        3 => Color::Yellow,
        4 => Color::LightRed,
        5 => Color::LightMagenta,
        CLASS_ERROR => Color::Red,
        _ => DIM,
    }
}

fn draw_scan(f: &mut Frame, app: &mut App, area: Rect) {
    let Some(scan) = app.scan.as_ref() else { return };
    let [top, gauge_area, mid, map_area] = Layout::vertical([Constraint::Length(2), Constraint::Length(3), Constraint::Length(4), Constraint::Min(4)]).areas(area);
    let state = if let Some(r) = &scan.result {
        match &r.end {
            ScanEnd::Completed => t!("Terminé", "Completed").to_string(),
            ScanEnd::Cancelled => t!("Arrêté", "Stopped").to_string(),
            ScanEnd::DeviceLost(e) => tf!("Le disque ne répond plus : {e}", "The disk stopped responding: {e}"),
            ScanEnd::TooManyErrors => t!("Arrêté : trop de zones illisibles", "Stopped: too many unreadable areas").to_string(),
            ScanEnd::Denied(e) => tf!("Lecture refusée : {e}", "Read refused: {e}"),
        }
    } else if let Some(e) = &scan.error {
        e.clone()
    } else if scan.cancel_requested {
        t!("Arrêt demandé...", "Stopping...").to_string()
    } else {
        t!("Lecture en cours (lecture seule)...", "Reading (read-only)...").to_string()
    };
    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(scan.title.clone(), Style::new().add_modifier(Modifier::BOLD))),
            Line::from(Span::styled(state, Style::new().fg(ACCENT))),
        ]),
        top,
    );
    let (ratio, label, speed) = match (&scan.result, &scan.progress) {
        (Some(r), _) => (
            if r.size > 0 { (r.scanned_bytes as f64 / r.size as f64).clamp(0.0, 1.0) } else { 1.0 },
            human_size(r.scanned_bytes),
            tf!(
                "Durée : {}   ·   vitesse moyenne : {}/s",
                "Time: {}   ·   average speed: {}/s",
                duration_text(r.duration_ms),
                human_size(r.avg_bps)
            ),
        ),
        (None, Some(p)) if p.total > 0 => {
            let r = (p.done as f64 / p.total as f64).clamp(0.0, 1.0);
            (
                r,
                format!("{:.1} %", r * 100.0),
                tf!(
                    "{} / {}   ·   {}/s   ·   reste {}",
                    "{} / {}   ·   {}/s   ·   {} left",
                    human_size(p.done),
                    human_size(p.total),
                    human_size(p.speed_bps),
                    p.eta_secs.map(|s| duration_text(s * 1000)).unwrap_or_else(|| "?".into())
                ),
            )
        }
        _ => (0.0, String::new(), String::new()),
    };
    f.render_widget(Gauge::default().block(Block::bordered()).gauge_style(Style::new().fg(Color::LightGreen).bg(Color::Black)).ratio(ratio).label(label), gauge_area);
    let counts = scan.result.as_ref().map(|r| r.counts).or(scan.progress.as_ref().map(|p| p.counts)).unwrap_or_default();
    let bad = scan.result.as_ref().map(|r| r.bad_sectors).or(scan.progress.as_ref().map(|p| p.bad_sectors)).unwrap_or(0);
    let labels = class_labels();
    let mut legend = Vec::new();
    for (i, l) in labels.iter().enumerate() {
        legend.push(Span::styled("■ ", Style::new().fg(class_color(i as u8))));
        legend.push(Span::raw(format!("{l}: {}   ", counts[i])));
    }
    let mut mid_lines = vec![Line::from(speed), Line::from(legend)];
    if bad > 0 {
        mid_lines.push(Line::from(Span::styled(tf!("Secteurs illisibles : {bad}", "Unreadable sectors: {bad}"), Style::new().fg(Color::LightRed))));
    }
    f.render_widget(Paragraph::new(mid_lines).wrap(Wrap { trim: true }), mid);

    if let Some(r) = &scan.result {
        // Finished: the findings, then the map below them.
        let mut lines = Vec::new();
        finding_lines(&mut lines, &r.findings);
        for b in r.bad.iter().take(50) {
            lines.push(Line::from(Span::styled(
                tf!("LBA {} : {} secteur(s) — {}", "LBA {}: {} sector(s) — {}", b.lba, b.sectors, b.error),
                Style::new().fg(Color::LightRed),
            )));
        }
        if !lines.is_empty() {
            let [fa, ma] = Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(map_area);
            draw_map(f, &scan.cells, ma);
            draw_scrolled(f, app, fa, "", lines);
            return;
        }
    }
    draw_map(f, &scan.cells, map_area);
}

/// Map of the disk surface: each character is a group of cells, coloured by its worst class.
fn draw_format(f: &mut Frame, app: &mut App, area: Rect) {
    let Some(st) = app.fmt.as_ref() else { return };
    let Some(entry) = app.entries.get(st.idx) else { return };
    let d = &entry.info;
    let refusals = app.format_refusals();
    let mut lines = vec![
        Line::from(Span::styled(d.title(), Style::default().add_modifier(Modifier::BOLD))),
        Line::from(Span::styled(
            format!("{} . {} . {}", d.number.map(|n| format!("disk {n}")).unwrap_or_else(|| d.path.clone()), human_size(d.size), d.mounts().join(" ")),
            Style::default().fg(Color::Gray),
        )),
        Line::from(""),
    ];
    let label_shown = if st.label.is_empty() { t!("(sans nom)", "(no name)").to_string() } else { st.label.clone() };
    let rows: [(String, String); 4] = [
        (t!("Système de fichiers", "File system").into(), format!("< {} >", st.fs.label())),
        (t!("Table", "Table").into(), format!("< {} >", st.table.label())),
        (tf!("Nom ({} max)", "Name ({} max)", st.fs.max_label()), label_shown),
        (tf!("Tapez {}", "Type {}", confirm_word()), st.confirm.clone()),
    ];
    for (i, (k, v)) in rows.iter().enumerate() {
        let editing = matches!(st.stage, FormatStage::Edit);
        let on = i == st.row && editing;
        let style = if on { Style::default().fg(Color::Black).bg(Color::Cyan) } else { Style::default() };
        let cursor = if on && i >= 2 && (app.tick / 5).is_multiple_of(2) { "_" } else { " " };
        lines.push(Line::from(vec![Span::raw(format!("  {k:<24} ")), Span::styled(format!(" {v}{cursor} "), style)]));
    }
    lines.push(Line::from(""));
    if st.fs == chud_clone_core::format::FsChoice::Fat32 {
        lines.push(Line::from(Span::styled(t!("FAT32 : fichiers de 4 Go maximum.", "FAT32: 4 GB maximum file size."), Style::default().fg(Color::Gray))));
    }
    for r in &refusals {
        lines.push(Line::from(Span::styled(format!("x {r}"), Style::default().fg(Color::LightRed))));
    }
    if refusals.is_empty() && matches!(st.stage, FormatStage::Edit) {
        lines.push(Line::from(Span::styled(
            tf!("TOUT le contenu de ce disque sera effacé ({}).", "ALL content of this disk will be erased ({}).", human_size(d.size)),
            Style::default().fg(Color::LightRed).add_modifier(Modifier::BOLD),
        )));
    }
    let running = matches!(st.stage, FormatStage::Running);
    if running || !st.steps.is_empty() {
        lines.push(Line::from(""));
        for s in &st.steps {
            lines.push(Line::from(format!("  - {s}")));
        }
        if running {
            lines.push(Line::from(Span::styled(t!("  Formatage en cours...", "  Formatting..."), Style::default().fg(Color::Yellow))));
        }
    }
    match &st.stage {
        FormatStage::Done(Ok(o)) => {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                tf!(
                    "Disque formaté : {} {} \"{}\" {} {}",
                    "Disk formatted: {} {} \"{}\" {} {}",
                    o.table.label(),
                    o.fs.label(),
                    o.label,
                    human_size(o.partition_size),
                    o.mount.clone().unwrap_or_default()
                ),
                Style::default().fg(Color::LightGreen).add_modifier(Modifier::BOLD),
            )));
        }
        FormatStage::Done(Err(e)) => {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(tf!("Échec : {e}", "Failed: {e}"), Style::default().fg(Color::LightRed).add_modifier(Modifier::BOLD))));
        }
        FormatStage::Edit | FormatStage::Running => {}
    }
    let block = Block::default().borders(ratatui::widgets::Borders::ALL).title(title(t!(" Formater (disque entier) ", " Format (whole disk) ")));
    f.render_widget(Paragraph::new(lines).block(block).wrap(Wrap { trim: false }), area);
}

fn draw_map(f: &mut Frame, cells: &[u8], area: Rect) {
    let block = Block::bordered().title(t!(" Carte de la surface ", " Surface map ")).border_style(Style::new().fg(DIM));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let slots = (inner.width as usize) * (inner.height as usize);
    if slots == 0 || cells.is_empty() {
        return;
    }
    let mut lines = Vec::new();
    for row in 0..inner.height as usize {
        let mut spans = Vec::new();
        for col in 0..inner.width as usize {
            let k = row * inner.width as usize + col;
            let a = k * cells.len() / slots;
            let b = ((k + 1) * cells.len() / slots).max(a + 1).min(cells.len());
            let read: Vec<u8> = cells[a..b].iter().copied().filter(|&c| c != CELL_UNREAD).collect();
            if read.is_empty() {
                spans.push(Span::styled("·", Style::new().fg(DIM)));
            } else {
                let worst = *read.iter().max().unwrap_or(&0);
                spans.push(Span::styled("█", Style::new().fg(class_color(worst))));
            }
        }
        lines.push(Line::from(spans));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

fn buttons(a: &str, b: &str, second: bool) -> Line<'static> {
    let on = Style::new().bg(ACCENT).fg(Color::Black).add_modifier(Modifier::BOLD);
    let off = Style::new().fg(Color::Gray);
    Line::from(vec![
        Span::styled(format!("  {a}  "), if second { off } else { on }),
        Span::raw("    "),
        Span::styled(format!("  {b}  "), if second { on } else { off }),
    ])
    .alignment(Alignment::Center)
}

fn draw_modal(f: &mut Frame, app: &App, area: Rect) {
    let width = (area.width - 4).min(76);
    let (title_text, lines, color): (String, Vec<Line>, Color) = match app.modal.as_ref().unwrap() {
        Modal::Message { title, text } => {
            let mut l: Vec<Line> = text.lines().map(|x| Line::from(x.to_string())).collect();
            l.push(Line::from(""));
            l.push(Line::from(Span::styled("  OK  ", Style::new().bg(ACCENT).fg(Color::Black).add_modifier(Modifier::BOLD))).alignment(Alignment::Center));
            (title.clone(), l, ACCENT)
        }
        Modal::Input { title, prompt, value, .. } => {
            let cursor = if (app.tick / 4).is_multiple_of(2) { "_" } else { " " };
            (
                title.clone(),
                vec![
                    Line::from(prompt.clone()),
                    Line::from(""),
                    Line::from(vec![Span::raw("> "), Span::styled(value.clone(), Style::new().add_modifier(Modifier::BOLD)), Span::raw(cursor)]),
                    Line::from(""),
                    Line::from(Span::styled(t!("Entrée : valider   ·   Échap : annuler", "Enter: confirm   ·   Esc: cancel"), Style::new().fg(DIM))),
                ],
                ACCENT,
            )
        }
        Modal::Confirm { title, text, yes, no, yes_selected, action } => {
            let danger = !matches!(action, super::app::ConfirmAction::StartScan(_));
            // Buttons: [no] [yes], "no" first so that Enter on the default is harmless.
            let mut l: Vec<Line> = text.lines().map(|x| Line::from(x.to_string())).collect();
            l.push(Line::from(""));
            l.push(buttons(no, yes, *yes_selected));
            (title.clone(), l, if danger { Color::Yellow } else { ACCENT })
        }
        Modal::Unreadable { info, zero_selected } => {
            let mut l = vec![
                Line::from(Span::styled(
                    tf!(
                        "{} secteur(s) restent illisibles sur la source ({}), après de nouvelles tentatives.",
                        "{} sector(s) remain unreadable on the source ({}), after retries.",
                        info.total_sectors,
                        human_size(info.total_bytes)
                    ),
                    Style::new().fg(Color::LightRed),
                )),
                Line::from(""),
            ];
            for r in info.ranges.iter().take(5) {
                l.push(Line::from(tf!("  LBA {} : {} secteur(s)", "  LBA {}: {} sector(s)", r.lba, r.sectors)));
            }
            if info.ranges.len() > 5 {
                l.push(Line::from(tf!("  ... et {} autre(s) zone(s)", "  ... and {} more area(s)", info.ranges.len() - 5)));
            }
            l.push(Line::from(""));
            l.push(Line::from(t!(
                "Poursuivre remplace ces secteurs par des zéros : les données correspondantes sont perdues, chaque secteur est listé dans le rapport et le clone sera déclaré incomplet.",
                "Continuing replaces these sectors with zeros: the matching data is lost, every sector is listed in the report and the clone will be declared incomplete."
            )));
            l.push(Line::from(""));
            l.push(buttons(t!("Arrêter le clonage", "Stop cloning"), t!("Poursuivre (zéros)", "Continue (zeros)"), *zero_selected));
            (t!("Secteurs illisibles", "Unreadable sectors").to_string(), l, Color::LightRed)
        }
        Modal::ExitMenu { sel } => {
            let mut l = Vec::new();
            let mut items: Vec<&str> = app.exit_items().iter().map(|(s, _)| *s).collect();
            items.push(t!("Retour", "Back"));
            for (i, s) in items.iter().enumerate() {
                let style = if i == *sel { Style::new().bg(Color::Blue).fg(Color::White) } else { Style::new() };
                l.push(Line::from(Span::styled(format!(" {} {s} ", if i == *sel { ">" } else { " " }), style)));
            }
            (t!("Quitter", "Quit").to_string(), l, ACCENT)
        }
    };
    let inner_w = width.saturating_sub(4).max(1) as usize;
    let height: u16 = lines.iter().map(|l| (l.width().max(1).div_ceil(inner_w)) as u16).sum::<u16>() + 2;
    let r = centered(area, width, height.min(area.height - 2));
    f.render_widget(Clear, r);
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::bordered().title(format!(" {title_text} ")).border_style(Style::new().fg(color)).padding(ratatui::widgets::Padding::horizontal(1))),
        r,
    );
}
