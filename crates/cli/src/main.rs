//! Chud Clone — command line.
//!
//! Same engine as the graphical application. Useful for tests, automation and
//! environments without an interface (rescue environment).

use chud_clone_core::clone::FinalStatus;
use chud_clone_core::diag::{Finding, human_size};
use chud_clone_core::engine::{Control, EngineOptions, LogLevel, Observer, Phase, Progress, UnreadablePolicy, VerifyMode};
use chud_clone_core::job::{self, JobConfig, JobError, Target};
use chud_clone_core::model::DiskInfo;
use chud_clone_core::plan::{CloneMode, PlanOptions};
use chud_clone_core::report;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Mutex;

mod tui;

fn usage() -> &'static str {
    chud_clone_core::t!(
        "\
Chud Clone — clonage et diagnostic de disques (ligne de commande)

UTILISATION
  chud-clone-cli list [--deep] [--json]
      Liste les disques détectés. --deep lit aussi les partitions sans lettre
      (lecture seule, droits administrateur requis).

  chud-clone-cli inspect <cible> [--json]
      Analyse une cible : table de partitions, systèmes de fichiers, démarrage.

  chud-clone-cli smart <disk:N> [--json]
      Lit l'état de santé SMART d'un disque (lecture seule, administrateur).

  chud-clone-cli plan <source> <destination> [options]
      Affiche le plan de clonage et les pré-vérifications. N'écrit rien.

  chud-clone-cli clone <source> <destination> --confirm-erase [options]
      Exécute le clonage. TOUT LE CONTENU DE LA DESTINATION EST EFFACÉ.

  chud-clone-cli report <rapport.json> (--txt | --html) [--out <fichier>]
      Convertit un rapport enregistré.

  chud-clone-cli tui [--reports <dossier>] [--image <fichier>]… [--only-virtual]
      Interface plein écran au clavier : mêmes étapes et mêmes vérifications que
      l'application (clé de secours WinPE, ou toute console).

  chud-clone-cli format <disk:N> --fs ntfs|exfat|fat32 [--table gpt|mbr]
                 [--label <nom>] --confirm-erase
      Formate le disque entier : nouvelle table (défaut GPT), une partition,
      un système de fichiers. TOUT LE CONTENU DU DISQUE EST EFFACÉ.

  chud-clone-cli eject <disk:N>
      Éjecte un disque USB (comme « Éjecter » dans l'Explorateur).

  chud-clone-cli fix-boot <disk:N>
      Répare le démarrage du Windows de ce disque pour un autre PC : la
      configuration de démarrage (BCD) désigne ses propres partitions, les
      pilotes NVMe et AHCI démarrent avec Windows, une session en veille
      prolongée est annulée. Corrige « Votre PC doit être réparé »
      (0xc000000e) après un changement d'identifiants. Aucune donnée effacée.

CIBLES
  disk:N          disque Windows numéro N (voir « list »)
  <chemin>        fichier image disque

OPTIONS DE CLONAGE
  --mode smart|partitions|disk   méthode (défaut : smart)
  --no-optimize                  ne pas déplacer les partitions de fin (GPT)
  --no-grow                      ne pas ajuster la partition Windows (NTFS) :
                                 ni agrandissement, ni réduction
  --verify hash|full|none        vérification après copie (défaut : hash)
  --on-unreadable stop|zero      secteurs illisibles : arrêter (défaut) ou
                                 remplacer par des zéros (données perdues)
  --retries N                    tentatives supplémentaires par secteur (défaut 3)
  --image-size <taille>          taille de l'image de destination à créer
                                 (octets, ou suffixe M / G ; défaut : la taille
                                 des partitions de la source)
  --reports <dossier>            dossier des rapports (défaut : ./rapports)
  --only-virtual                 refuser toute destination qui n'est pas une image
                                 ou un disque virtuel (VHD/VHDX)
  --no-boot-prep                 ne pas préparer le démarrage du clone pour un
                                 autre PC (voir fix-boot ; fait par défaut)
  --json                         sortie JSON

OPTION GÉNÉRALE
  --lang fr|en                   langue des messages (défaut : langue de Windows,
                                 ou variable CHUD_LANG)

CODES DE RETOUR
  0 réussi et vérifié · 1 terminé avec avertissements · 2 incomplet
  3 échoué · 4 refusé ou erreur d'utilisation
",
        "\
Chud Clone — disk cloning and diagnostics (command line)

USAGE
  chud-clone-cli list [--deep] [--json]
      Lists the detected disks. --deep also reads partitions without a drive
      letter (read-only, administrator rights required).

  chud-clone-cli inspect <target> [--json]
      Analyses a target: partition table, file systems, boot.

  chud-clone-cli smart <disk:N> [--json]
      Reads a disk's SMART health (read-only, administrator).

  chud-clone-cli plan <source> <destination> [options]
      Shows the clone plan and the pre-checks. Writes nothing.

  chud-clone-cli clone <source> <destination> --confirm-erase [options]
      Runs the clone. ALL CONTENT OF THE DESTINATION IS ERASED.

  chud-clone-cli report <report.json> (--txt | --html) [--out <file>]
      Converts a saved report.

  chud-clone-cli tui [--reports <folder>] [--image <file>]… [--only-virtual]
      Full-screen keyboard interface: same steps and same checks as the
      application (WinPE rescue key, or any console).

  chud-clone-cli format <disk:N> --fs ntfs|exfat|fat32 [--table gpt|mbr]
                 [--label <name>] --confirm-erase
      Formats the whole disk: new table (default GPT), one partition, one
      file system. ALL CONTENT OF THE DISK IS ERASED.

  chud-clone-cli eject <disk:N>
      Ejects a USB disk (like \"Eject\" in File Explorer).

  chud-clone-cli fix-boot <disk:N>
      Repairs the boot of this disk's Windows for another PC: the boot
      configuration (BCD) designates its own partitions, the NVMe and AHCI
      drivers start with Windows, a hibernated session is cancelled. Fixes
      \"Your PC needs to be repaired\" (0xc000000e) after an identifier
      change. No data is erased.

TARGETS
  disk:N          Windows disk number N (see \"list\")
  <path>          disk image file

CLONE OPTIONS
  --mode smart|partitions|disk   method (default: smart)
  --no-optimize                  do not move trailing partitions (GPT)
  --no-grow                      do not resize the Windows partition (NTFS):
                                 neither grow nor shrink it
  --verify hash|full|none        verification after the copy (default: hash)
  --on-unreadable stop|zero      unreadable sectors: stop (default) or
                                 replace with zeros (data lost)
  --retries N                    extra attempts per sector (default 3)
  --image-size <size>            size of the destination image to create
                                 (bytes, or M / G suffix; default: the size
                                 of the source's partitions)
  --reports <folder>             reports folder (default: ./reports)
  --only-virtual                 refuse any destination that is not an image
                                 or a virtual disk (VHD/VHDX)
  --no-boot-prep                 do not prepare the clone's boot for another
                                 PC (see fix-boot; done by default)
  --json                         JSON output

GENERAL OPTION
  --lang fr|en                   message language (default: Windows language,
                                 or the CHUD_LANG variable)

EXIT CODES
  0 succeeded and verified · 1 completed with warnings · 2 incomplete
  3 failed · 4 refused or usage error
"
    )
}

struct Args {
    positional: Vec<String>,
    flags: Vec<(String, Option<String>)>,
}

const VALUE_FLAGS: [&str; 13] = ["--fs", "--table", "--label", "--mode", "--verify", "--on-unreadable", "--retries", "--image-size", "--reports", "--out", "--chunk", "--image", "--lang-file"];

fn parse_args(raw: &[String]) -> Result<Args, String> {
    let mut a = Args { positional: vec![], flags: vec![] };
    let mut i = 0;
    while i < raw.len() {
        let x = &raw[i];
        if x.starts_with("--") {
            if VALUE_FLAGS.contains(&x.as_str()) {
                let v = raw.get(i + 1).ok_or_else(|| chud_clone_core::tf!("valeur manquante après {x}", "missing value after {x}"))?;
                a.flags.push((x.clone(), Some(v.clone())));
                i += 1;
            } else {
                a.flags.push((x.clone(), None));
            }
        } else {
            a.positional.push(x.clone());
        }
        i += 1;
    }
    Ok(a)
}

impl Args {
    fn has(&self, f: &str) -> bool {
        self.flags.iter().any(|(k, _)| k == f)
    }
    fn value(&self, f: &str) -> Option<&str> {
        self.flags.iter().find(|(k, _)| k == f).and_then(|(_, v)| v.as_deref())
    }
    fn values(&self, f: &str) -> Vec<String> {
        self.flags.iter().filter(|(k, _)| k == f).filter_map(|(_, v)| v.clone()).collect()
    }
    fn check_known(&self, known: &[&str]) -> Result<(), String> {
        for (k, _) in &self.flags {
            if !known.contains(&k.as_str()) {
                return Err(chud_clone_core::tf!("option inconnue : {k}", "unknown option: {k}"));
            }
        }
        Ok(())
    }
}

fn parse_size(s: &str) -> Result<u64, String> {
    let t = s.trim().to_ascii_uppercase();
    let (num, mul) = if let Some(n) = t.strip_suffix('G') {
        (n, 1u64 << 30)
    } else if let Some(n) = t.strip_suffix('M') {
        (n, 1u64 << 20)
    } else {
        (t.as_str(), 1)
    };
    num.trim().parse::<u64>().map(|v| v * mul).map_err(|_| chud_clone_core::tf!("taille invalide : {s}", "invalid size: {s}"))
}

fn print_findings(list: &[Finding]) {
    for f in list {
        println!(
            "  [{}{}] {}{}",
            f.severity.label(),
            if f.blocking { chud_clone_core::t!(", BLOQUANT", ", BLOCKING") } else { "" },
            f.title,
            f.scope.as_ref().map(|s| format!(" ({s})")).unwrap_or_default()
        );
        println!("      {}", f.detail);
    }
}

fn print_disk(d: &DiskInfo) {
    let mut tags = Vec::new();
    if d.is_system {
        tags.push(chud_clone_core::t!("SYSTÈME EN COURS", "RUNNING SYSTEM"));
    }
    if d.hosts_app {
        tags.push(chud_clone_core::t!("contient Chud Clone", "holds Chud Clone"));
    }
    if d.offline {
        tags.push(chud_clone_core::t!("hors ligne", "offline"));
    }
    if d.read_only {
        tags.push(chud_clone_core::t!("lecture seule", "read-only"));
    }
    if d.removable {
        tags.push("amovible");
    }
    println!("{}  [{}]", d.title(), d.id);
    println!(
        "{}",
        chud_clone_core::tf!(
            "    {} · {} · {} · {} · secteurs {}/{}{}",
            "    {} · {} · {} · {} · sectors {}/{}{}",
            human_size(d.size),
            d.bus,
            d.media.label(),
            d.table.label(),
            d.sector_size,
            d.physical_sector_size,
            if tags.is_empty() { String::new() } else { format!(" · {}", tags.join(", ")) }
        )
    );
    if !d.serial.is_empty() {
        println!(
            "{}",
            chud_clone_core::tf!("    N° de série : {}   Micrologiciel : {}", "    Serial number: {}   Firmware: {}", d.serial, d.firmware)
        );
    }
    for p in &d.partitions {
        println!(
            "    {:>2}. {:<30} {:>10}  {:<8} {:<14} {} {}",
            p.number,
            p.type_label,
            human_size(p.size),
            p.fs.clone().unwrap_or_else(|| "—".into()),
            p.used_bytes.map(|u| chud_clone_core::tf!("{} utilisés", "{} used", human_size(u))).unwrap_or_default(),
            p.mounts.join(" "),
            p.label.clone().map(|l| chud_clone_core::tf!("« {l} »", "\"{l}\"")).unwrap_or_default()
        );
    }
    for n in &d.notes {
        println!("    ! {n}");
    }
}

struct Console {
    last_phase: Mutex<Option<Phase>>,
    quiet: bool,
}

impl Observer for Console {
    fn progress(&self, p: &Progress) {
        if self.quiet || p.total == 0 {
            return;
        }
        let pct = p.done as f64 * 100.0 / p.total as f64;
        let eta = p.eta_secs.map(|s| chud_clone_core::tf!(" · reste ~{}", " · ~{} left", report::duration_text(s * 1000))).unwrap_or_default();
        let mut last = self.last_phase.lock().unwrap();
        if *last != Some(p.phase) {
            *last = Some(p.phase);
            eprintln!();
        }
        eprint!(
            "{}",
            chud_clone_core::tf!(
                "\r{} : {:5.1} %  {} / {}  {}/s{}  erreurs L/E : {}/{}      ",
                "\r{}: {:5.1} %  {} / {}  {}/s{}  R/W errors: {}/{}      ",
                p.phase.label(),
                pct,
                human_size(p.done),
                human_size(p.total),
                human_size(p.speed_bps),
                eta,
                p.read_errors,
                p.write_errors
            )
        );
        let _ = std::io::stderr().flush();
    }
    fn log(&self, level: LogLevel, msg: &str) {
        if self.quiet && level == LogLevel::Info {
            return;
        }
        let tag = match level {
            LogLevel::Debug => "debug",
            LogLevel::Info => "info ",
            LogLevel::Warning => chud_clone_core::t!("AVERT", "WARN "),
            LogLevel::Error => chud_clone_core::t!("ERREUR", "ERROR"),
        };
        eprintln!("\n[{tag}] {msg}");
    }
}

fn job_config(a: &Args) -> Result<JobConfig, String> {
    if a.positional.len() != 3 {
        return Err(chud_clone_core::t!("source et destination attendues", "source and destination expected").into());
    }
    let mode = match a.value("--mode").unwrap_or("smart") {
        "smart" => CloneMode::Smart,
        "partitions" => CloneMode::RawPartitions,
        "disk" => CloneMode::RawDisk,
        x => return Err(chud_clone_core::tf!("mode inconnu : {x}", "unknown mode: {x}")),
    };
    let verify = match a.value("--verify").unwrap_or("hash") {
        "hash" => VerifyMode::Hash,
        "full" => VerifyMode::Full,
        "none" => VerifyMode::None,
        x => return Err(chud_clone_core::tf!("vérification inconnue : {x}", "unknown verification: {x}")),
    };
    let unreadable = match a.value("--on-unreadable").unwrap_or("stop") {
        "stop" => UnreadablePolicy::Stop,
        "zero" => UnreadablePolicy::ZeroFill,
        x => return Err(chud_clone_core::tf!("politique inconnue : {x}", "unknown policy: {x}")),
    };
    let mut engine = EngineOptions { verify, unreadable, ..Default::default() };
    if let Some(r) = a.value("--retries") {
        engine.read_retries = r.parse().map_err(|_| chud_clone_core::t!("nombre de tentatives invalide", "invalid number of retries").to_string())?;
    }
    if let Some(c) = a.value("--chunk") {
        engine.chunk_size = parse_size(c)? as usize;
    }
    engine.validate()?;
    let reports = PathBuf::from(a.value("--reports").unwrap_or(chud_clone_core::t!("rapports", "reports")));
    Ok(JobConfig {
        source: Target::parse(&a.positional[1])?,
        destination: Target::parse(&a.positional[2])?,
        plan: PlanOptions { mode, optimize_layout: !a.has("--no-optimize"), grow_main: !a.has("--no-grow") },
        engine,
        new_image_size: a.value("--image-size").map(parse_size).transpose()?,
        allow_unvalidated_physical_write: a.has("--allow-unvalidated-physical-write"),
        expected_source_fingerprint: None,
        expected_destination_fingerprint: None,
        app_paths: vec![std::fs::canonicalize(&reports).unwrap_or(reports).to_string_lossy().to_string()],
        prepare_boot: !a.has("--no-boot-prep"),
    })
}

const CLONE_FLAGS: [&str; 14] = [
    "--mode",
    "--no-optimize",
    "--no-grow",
    "--verify",
    "--on-unreadable",
    "--retries",
    "--image-size",
    "--reports",
    "--only-virtual",
    "--allow-unvalidated-physical-write",
    "--json",
    "--confirm-erase",
    "--chunk",
    "--no-boot-prep",
];

fn only_virtual_ok(d: &DiskInfo) -> bool {
    d.is_virtual()
}

fn run(raw: Vec<String>) -> Result<u8, String> {
    let a = parse_args(&raw)?;
    let cmd = a.positional.first().map(String::as_str).unwrap_or("");
    let json = a.has("--json");
    match cmd {
        "" | "help" | "aide" => {
            println!("{}", usage());
            Ok(0)
        }
        "list" => {
            a.check_known(&["--deep", "--json"])?;
            let disks = job::list_disks(a.has("--deep"), &[]);
            if json {
                println!("{}", serde_json::to_string_pretty(&disks).map_err(|e| e.to_string())?);
            } else {
                if disks.is_empty() {
                    println!("{}", chud_clone_core::tf!("Aucun disque détecté.", "No disk detected."));
                }
                for d in &disks {
                    print_disk(d);
                    println!();
                }
                if !job::is_elevated() {
                    println!(
                        "{}",
                        chud_clone_core::tf!(
                            "(Sans droits administrateur : informations limitées à ce que Windows expose ; SMART et lecture directe indisponibles.)",
                            "(Without administrator rights: information limited to what Windows exposes; SMART and direct reads unavailable.)"
                        )
                    );
                }
            }
            Ok(0)
        }
        "inspect" => {
            a.check_known(&["--json"])?;
            let target = Target::parse(a.positional.get(1).ok_or(chud_clone_core::t!("cible attendue", "target expected"))?)?;
            let info = job::describe(&target, true, &[], None)?;
            let (analysis, boot) = job::inspect(&target)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({ "disk": info, "analysis": analysis, "boot": boot }))
                        .map_err(|e| e.to_string())?
                );
            } else {
                print_disk(&info);
                println!("{}", chud_clone_core::tf!("\nDémarrage : {}", "\nBoot: {}", boot.style.label()));
                for c in &boot.checks {
                    println!("  [{:?}] {}", c.state, c.label);
                }
                let mut f = analysis.all_findings();
                f.extend(boot.findings.clone());
                println!("{}", chud_clone_core::tf!("\nConstats :", "\nFindings:"));
                if f.is_empty() {
                    println!("{}", chud_clone_core::tf!("  aucun", "  none"));
                }
                print_findings(&f);
            }
            Ok(0)
        }
        "smart" => {
            a.check_known(&["--json"])?;
            let target = Target::parse(a.positional.get(1).ok_or(chud_clone_core::t!("cible attendue", "target expected"))?)?;
            let info = job::describe(&target, false, &[], None)?;
            let s = job::read_smart(&info);
            if json {
                println!("{}", serde_json::to_string_pretty(&s).map_err(|e| e.to_string())?);
            } else if !s.available {
                println!(
                    "{}",
                    chud_clone_core::tf!(
                        "Données de santé indisponibles : {}",
                        "Health data unavailable: {}",
                        s.unavailable_reason.unwrap_or_default()
                    )
                );
            } else {
                println!(
                    "{}",
                    chud_clone_core::tf!(
                        "{} — {} : {} (indicateur, pas une certitude)",
                        "{} — {}: {} (indicator, not a certainty)",
                        info.title(),
                        s.protocol,
                        s.level.label()
                    )
                );
                if let Some(t) = s.temperature_c {
                    println!("{}", chud_clone_core::tf!("  Température : {t} °C", "  Temperature: {t} °C"));
                }
                if let Some(h) = s.power_on_hours {
                    println!("{}", chud_clone_core::tf!("  Fonctionnement : {h} h", "  Power-on time: {h} h"));
                }
                if let Some(w) = s.wear_percent_used {
                    println!("{}", chud_clone_core::tf!("  Usure estimée : {w} %", "  Estimated wear: {w} %"));
                }
                for at in &s.attributes {
                    println!(
                        "{}",
                        chud_clone_core::tf!(
                            "  {:>3} {:<42} valeur {:>3}  pire {:>3}  seuil {:>3}  brut {}",
                            "  {:>3} {:<42} value {:>3}  worst {:>3}  thresh {:>3}  raw {}",
                            at.id,
                            at.name,
                            at.current,
                            at.worst,
                            at.threshold.map(|t| t.to_string()).unwrap_or_else(|| "—".into()),
                            at.raw
                        )
                    );
                }
                print_findings(&s.findings);
            }
            Ok(0)
        }
        "plan" | "clone" => {
            a.check_known(&CLONE_FLAGS)?;
            let cfg = job_config(&a)?;
            let prep = job::prepare(&cfg, false)?;
            if a.has("--only-virtual") && !only_virtual_ok(&prep.destination) {
                return Err(chud_clone_core::tf!(
                    "--only-virtual : la destination « {} » n'est ni une image ni un disque virtuel. Refusé.",
                    "--only-virtual: destination \"{}\" is neither an image nor a virtual disk. Refused.",
                    prep.destination.title()
                ));
            }
            if cmd == "plan" || !json {
                if json {
                    println!("{}", serde_json::to_string_pretty(&prep).map_err(|e| e.to_string())?);
                } else {
                    println!("SOURCE");
                    print_disk(&prep.source);
                    println!("\nDESTINATION");
                    print_disk(&prep.destination);
                    println!("{}", chud_clone_core::tf!("\nPLAN — {} · {:?}", "\nPLAN — {} · {:?}", prep.plan.mode.label(), prep.plan.strategy));
                    for p in &prep.plan.partitions {
                        println!(
                            "{}",
                            chud_clone_core::tf!(
                                "  {:>2}. {:<30} {:>10} → position {:>10}  {:?}  à copier : {}{}{}",
                                "  {:>2}. {:<30} {:>10} → position {:>10}  {:?}  to copy: {}{}{}",
                                p.number,
                                p.type_label,
                                human_size(p.src_len),
                                human_size(p.dst_start),
                                p.method,
                                human_size(p.copy_bytes),
                                if p.dst_len != p.src_len {
                                    chud_clone_core::tf!("  → agrandie à {}", "  → grown to {}", human_size(p.dst_len))
                                } else {
                                    String::new()
                                },
                                p.method_reason.as_ref().map(|r| format!("  ({r})")).unwrap_or_default()
                            )
                        );
                    }
                    println!(
                        "{}",
                        chud_clone_core::tf!(
                            "  Total à copier : {} sur {}",
                            "  Total to copy: {} of {}",
                            human_size(prep.plan.total_copy_bytes),
                            human_size(prep.plan.src_size)
                        )
                    );
                    println!("{}", chud_clone_core::tf!("\nPRÉ-VÉRIFICATIONS", "\nPRE-CHECKS"));
                    print_findings(&prep.findings);
                }
            }
            if cmd == "plan" {
                return Ok(if prep.blocked { 4 } else { 0 });
            }
            if prep.blocked {
                return Err(chud_clone_core::t!(
                    "le clonage est bloqué par les pré-vérifications ci-dessus",
                    "cloning is blocked by the pre-checks above"
                )
                .into());
            }
            if !a.has("--confirm-erase") {
                return Err(chud_clone_core::tf!(
                    "confirmation manquante. Cette commande EFFACE tout le contenu de « {} ». Ajoutez --confirm-erase pour confirmer.",
                    "confirmation missing. This command ERASES all content of \"{}\". Add --confirm-erase to confirm.",
                    prep.destination.title()
                ));
            }
            let mut cfg = cfg;
            cfg.expected_source_fingerprint = Some(prep.source.fingerprint.clone());
            cfg.expected_destination_fingerprint = Some(prep.destination.fingerprint.clone());
            let reports = PathBuf::from(a.value("--reports").unwrap_or(chud_clone_core::t!("rapports", "reports")));
            let ctl = Control::new();
            let console = Console { last_phase: Mutex::new(None), quiet: json };
            let rep = match job::execute(&cfg, &reports, &ctl, &console) {
                Ok(r) => r,
                Err(JobError::Blocked(f)) => {
                    print_findings(&f);
                    return Err(chud_clone_core::t!("clonage refusé par les pré-vérifications", "clone refused by the pre-checks").into());
                }
                Err(JobError::Failed(m)) => return Err(m),
            };
            eprintln!();
            if json {
                println!("{}", serde_json::to_string_pretty(&rep).map_err(|e| e.to_string())?);
            } else {
                println!("\n=== {} ===", rep.outcome.status.label().to_uppercase());
                for r in &rep.outcome.reasons {
                    println!("  - {r}");
                }
                println!("{}", chud_clone_core::tf!("Rapport : {}", "Report: {}", reports.join(format!("{}.json", rep.id)).display()));
            }
            let _ = report::write_atomic(&reports.join(format!("{}.txt", rep.id)), report::render_txt(&rep).as_bytes());
            let _ = report::write_atomic(&reports.join(format!("{}.html", rep.id)), report::render_html(&rep).as_bytes());
            Ok(match rep.outcome.status {
                FinalStatus::SuccessVerified => 0,
                FinalStatus::CompletedWithWarnings => 1,
                FinalStatus::Incomplete => 2,
                FinalStatus::Failed => 3,
            })
        }
        "tui" => {
            // --winpe, --lang-file and --no-disks are used by the rescue key menu and the tests.
            a.check_known(&["--reports", "--image", "--only-virtual", "--winpe", "--lang-file", "--no-disks"])?;
            if a.positional.len() > 1 {
                return Err(chud_clone_core::t!("tui n'accepte pas d'argument (images : --image <fichier>)", "tui takes no argument (images: --image <file>)").into());
            }
            tui::run(tui::Options {
                reports: PathBuf::from(a.value("--reports").unwrap_or(chud_clone_core::t!("rapports", "reports"))),
                images: a.values("--image").into_iter().map(PathBuf::from).collect(),
                list_disks: !a.has("--no-disks"),
                only_virtual: a.has("--only-virtual"),
                winpe: a.has("--winpe"),
                lang_file: a.value("--lang-file").map(PathBuf::from),
            })
        }
        "format" => {
            use chud_clone_core::format::{FormatRequest, FsChoice, TableChoice, refusals, validate_label};
            a.check_known(&["--fs", "--table", "--label", "--confirm-erase", "--json"])?;
            let target = Target::parse(a.positional.get(1).ok_or(chud_clone_core::t!("disque attendu (disk:N)", "disk expected (disk:N)"))?)?;
            let fs = FsChoice::parse(a.value("--fs").ok_or(chud_clone_core::t!("--fs ntfs|exfat|fat32 attendu", "--fs ntfs|exfat|fat32 expected"))?)?;
            let table = TableChoice::parse(a.value("--table").unwrap_or("gpt"))?;
            let label = validate_label(fs, a.value("--label").unwrap_or(""))?;
            let req = FormatRequest { fs, table, label };
            let info = job::describe(&target, false, &[], None)?;
            let refused = refusals(&info, &req);
            if !refused.is_empty() {
                return Err(refused.join(" ; "));
            }
            println!(
                "{}",
                chud_clone_core::tf!(
                    "{} → {} {}, nom « {} »",
                    "{} → {} {}, name \"{}\"",
                    info.title(),
                    req.table.label(),
                    req.fs.label(),
                    req.label
                )
            );
            if !a.has("--confirm-erase") {
                return Err(chud_clone_core::tf!(
                    "confirmation manquante. Cette commande EFFACE tout le contenu de « {} ». Ajoutez --confirm-erase pour confirmer.",
                    "confirmation missing. This command ERASES all content of \"{}\". Add --confirm-erase to confirm.",
                    info.title()
                ));
            }
            #[cfg(windows)]
            {
                let number = info.number.ok_or(chud_clone_core::t!("disque attendu (disk:N)", "disk expected (disk:N)"))?;
                let out = chud_clone_core::format::format_disk(number, &info.fingerprint, &req, &[], &|s| eprintln!("  {s}"))?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?);
                } else {
                    println!(
                        "{}",
                        chud_clone_core::tf!(
                            "Disque formaté : {} {} « {} » {} {}",
                            "Disk formatted: {} {} \"{}\" {} {}",
                            out.table.label(),
                            out.fs.label(),
                            out.label,
                            human_size(out.partition_size),
                            out.mount.clone().unwrap_or_default()
                        )
                    );
                }
                Ok(0)
            }
            #[cfg(not(windows))]
            Err("Windows only".into())
        }
        "eject" => {
            a.check_known(&[])?;
            let target = Target::parse(a.positional.get(1).ok_or(chud_clone_core::t!("disque attendu (disk:N)", "disk expected (disk:N)"))?)?;
            let info = job::describe(&target, false, &[], None)?;
            let number = info.number.ok_or(chud_clone_core::t!("disque attendu (disk:N)", "disk expected (disk:N)"))?;
            #[cfg(windows)]
            {
                chud_clone_core::win::eject::eject_disk(number)?;
                println!("{}", chud_clone_core::tf!("{} éjecté : il peut être débranché.", "{} ejected: it can be unplugged.", info.title()));
                Ok(0)
            }
            #[cfg(not(windows))]
            {
                let _ = number;
                Err("Windows only".into())
            }
        }
        "fix-boot" => {
            a.check_known(&["--json"])?;
            let target = Target::parse(a.positional.get(1).ok_or(chud_clone_core::t!("disque attendu (disk:N)", "disk expected (disk:N)"))?)?;
            let info = job::describe(&target, false, &[], None)?;
            let number = info.number.ok_or(chud_clone_core::t!("disque attendu (disk:N)", "disk expected (disk:N)"))?;
            if info.is_system {
                return Err(chud_clone_core::t!(
                    "c'est le disque du Windows en cours d'exécution : sa configuration de démarrage ne se répare pas depuis lui-même",
                    "this is the disk of the running Windows: its boot configuration is not repaired from itself"
                )
                .into());
            }
            println!("{}", chud_clone_core::tf!("{} — réparation du démarrage", "{} — repairing boot", info.title()));
            let out = chud_clone_core::bootprep::prepare_disk(number, &|s| eprintln!("  {s}"))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?);
            } else {
                println!("{}", chud_clone_core::tf!("Résultat : {}", "Result: {}", out.summary()));
                for w in &out.warnings {
                    println!("  ! {w}");
                }
            }
            Ok(if out.warnings.is_empty() { 0 } else { 1 })
        }
        "report" => {
            a.check_known(&["--txt", "--html", "--out"])?;
            let path = PathBuf::from(a.positional.get(1).ok_or(chud_clone_core::t!("fichier de rapport attendu", "report file expected"))?);
            let data = std::fs::read(&path).map_err(|e| chud_clone_core::tf!("lecture impossible : {e}", "cannot read: {e}"))?;
            let rep: report::Report =
                serde_json::from_slice(&data).map_err(|e| chud_clone_core::tf!("rapport invalide : {e}", "invalid report: {e}"))?;
            let text = if a.has("--html") { report::render_html(&rep) } else { report::render_txt(&rep) };
            match a.value("--out") {
                Some(out) => std::fs::write(out, text).map_err(|e| e.to_string())?,
                None => println!("{text}"),
            }
            Ok(0)
        }
        other => Err(chud_clone_core::tf!(
            "commande inconnue : {other}

{}",
            "unknown command: {other}

{}",
            usage()
        )),
    }
}

/// Message language: `--lang fr|en` (removed from the arguments), otherwise `CHUD_LANG`,
/// otherwise the Windows language.
fn take_lang(raw: &mut Vec<String>) {
    use chud_clone_core::i18n::{Lang, set_lang, system_lang};
    let mut lang = std::env::var("CHUD_LANG").ok().filter(|v| !v.trim().is_empty()).map(|v| Lang::from_tag(&v)).unwrap_or_else(system_lang);
    if let Some(i) = raw.iter().position(|x| x == "--lang") {
        if let Some(v) = raw.get(i + 1) {
            lang = Lang::from_tag(v);
            raw.drain(i..=i + 1);
        } else {
            raw.remove(i);
        }
    } else if let Some(i) = raw.iter().position(|x| x.starts_with("--lang=")) {
        lang = Lang::from_tag(&raw[i]["--lang=".len()..]);
        raw.remove(i);
    }
    set_lang(lang);
}

fn main() -> ExitCode {
    let mut raw: Vec<String> = std::env::args().skip(1).collect();
    take_lang(&mut raw);
    match run(raw) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("{}", chud_clone_core::tf!("Erreur : {e}", "Error: {e}"));
            ExitCode::from(4)
        }
    }
}
