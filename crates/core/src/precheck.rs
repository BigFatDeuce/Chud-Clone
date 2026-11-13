//! Pré-vérifications de sécurité, exécutées avant chaque clonage.
//!
//! Tout constat « bloquant » interdit le démarrage. Ces contrôles sont refaits par le
//! moteur au moment de lancer l'opération, jamais sur la seule foi de l'interface.

use crate::diag::{Finding, human_size};
use crate::model::{DiskInfo, DiskKind};
use crate::partition::TableKind;
use crate::plan::{CloneMode, ClonePlan, DiskAnalysis};

/// Contrôles portant sur l'identité et l'état des deux disques.
pub fn precheck(src: &DiskInfo, dst: &DiskInfo, src_analysis: Option<&DiskAnalysis>, plan: Option<&ClonePlan>) -> Vec<Finding> {
    let mut out = Vec::new();

    // --- Même disque -----------------------------------------------------------
    let same_path = src.path.eq_ignore_ascii_case(&dst.path);
    let same_number = src.kind == DiskKind::Physical && dst.kind == DiskKind::Physical && src.number.is_some() && src.number == dst.number;
    if src.id == dst.id || same_path || same_number {
        out.push(
            Finding::critical(
                "SAME_DISK",
                t!("Source et destination identiques", "Source and destination are the same"),
                t!(
                    "Le disque source et le disque de destination sont le même disque. Un disque ne peut pas être cloné sur lui-même.",
                    "The source disk and the destination disk are the same disk. A disk cannot be cloned onto itself."
                ),
            )
            .blocking(),
        );
        return out;
    }
    if !src.serial.trim().is_empty() && src.serial.trim() == dst.serial.trim() && src.model == dst.model && src.size == dst.size {
        out.push(Finding::warning(
            "SAME_IDENTITY",
            t!("Deux disques d'identité identique", "Two disks with the same identity"),
            t!("La source et la destination annoncent le même modèle, le même numéro de série et la même taille. \
             Il peut s'agir de deux chemins vers le même disque (boîtier double, multipath) : vérifiez avant de continuer.", "The source and the destination report the same model, serial number and size. They may be two paths to the same disk (dual enclosure, multipath): check before continuing."),
        ));
    }

    // --- Destination protégée ---------------------------------------------------
    if dst.is_system {
        out.push(
            Finding::critical(
                "DEST_IS_SYSTEM",
                t!("La destination contient le Windows en cours d'exécution", "The destination holds the running Windows"),
                t!(
                    "Ce disque héberge le système actuellement démarré (ou son amorçage, ou son fichier d'échange). \
                 Il ne peut pas être choisi comme destination.",
                    "This disk hosts the currently booted system (or its boot files, or its paging file). It cannot be chosen as the destination."
                ),
            )
            .scope("destination")
            .blocking(),
        );
    }
    if dst.hosts_app {
        out.push(
            Finding::critical(
                "DEST_HOSTS_APP",
                t!("La destination contient Chud Clone lui-même", "The destination holds Chud Clone itself"),
                t!(
                    "Le programme, ou son dossier de rapports, se trouve sur ce disque. L'effacer en cours d'opération ferait échouer le clonage.",
                    "The program, or its reports folder, is on this disk. Erasing it during the operation would make cloning fail."
                ),
            )
            .scope("destination")
            .blocking(),
        );
    }
    if dst.read_only {
        out.push(
            Finding::error(
                "DEST_READ_ONLY",
                t!("Destination protégée en écriture", "Write-protected destination"),
                t!(
                    "Le disque de destination est en lecture seule (commutateur de protection, attribut Windows ou support verrouillé).",
                    "The destination disk is read-only (protection switch, Windows attribute or locked media)."
                ),
            )
            .scope("destination")
            .blocking(),
        );
    }
    if dst.offline && dst.kind == DiskKind::Physical {
        out.push(
            Finding::error(
                "DEST_OFFLINE",
                t!("Destination hors ligne", "Destination offline"),
                t!(
                    "Windows a placé ce disque hors ligne. Mettez-le en ligne dans « Gestion des disques » avant de le choisir comme destination.",
                    "Windows has put this disk offline. Bring it online in \"Disk Management\" before choosing it as the destination."
                ),
            )
            .scope("destination")
            .blocking(),
        );
    }
    if dst.size == 0 {
        out.push(
            Finding::error(
                "DEST_NO_MEDIA",
                t!("Destination sans support", "Destination without media"),
                t!(
                    "Le disque de destination annonce une taille nulle (lecteur vide ?).",
                    "The destination disk reports a size of zero (empty drive?)."
                ),
            )
            .scope("destination")
            .blocking(),
        );
    }

    // --- Source ------------------------------------------------------------------
    if src.size == 0 {
        out.push(
            Finding::error(
                "SRC_NO_MEDIA",
                t!("Source sans support", "Source without media"),
                t!("Le disque source annonce une taille nulle.", "The source disk reports a size of zero."),
            )
            .scope("source")
            .blocking(),
        );
    }
    if src.is_system && src.kind == DiskKind::Physical {
        out.push(
            Finding::error(
                "SOURCE_LIVE_SYSTEM",
                t!("La source est le Windows en cours d'exécution", "The source is the running Windows"),
                t!("Ce disque héberge le système actuellement démarré. Ses fichiers changent en permanence : le copier tel quel \
                 produirait un clone incohérent, qui pourrait ne pas démarrer ou contenir des fichiers corrompus. \
                 Le clonage à chaud (par instantané VSS) n'est pas encore pris en charge par cette version. \
                 Pour cloner ce disque de façon fiable aujourd'hui : branchez-le comme disque secondaire sur un autre PC, \
                 ou lancez Chud Clone depuis un autre Windows (par exemple un environnement de secours).", "This disk hosts the currently booted system. Its files change constantly: copying it as is would produce an inconsistent clone that might not boot or might contain corrupted files. Live cloning (through a VSS snapshot) is not supported yet in this version. To clone this disk reliably today: connect it as a secondary disk to another PC, or run Chud Clone from another Windows (for example the WinPE rescue drive)."),
            )
            .scope("source")
            .blocking(),
        );
    } else if src.kind == DiskKind::Physical && src.has_mounted_volumes() {
        out.push(
            Finding::info(
                "SOURCE_VOLUMES_LOCKED",
                t!("Les volumes de la source seront verrouillés", "The source volumes will be locked"),
                tf!("Pendant le clonage, les volumes {} seront démontés et verrouillés pour garantir une copie cohérente : \
                     ils seront inaccessibles jusqu'à la fin. Fermez les programmes qui les utilisent, sinon le clonage sera refusé.", "During cloning, volumes {} will be dismounted and locked to guarantee a consistent copy: they will be inaccessible until the end. Close the programs using them, otherwise cloning will be refused.",
                    src.mounts().join(", ")
                ),
            )
            .scope("source"),
        );
    }
    if src.offline && src.kind == DiskKind::Physical {
        out.push(Finding::info(
            "SOURCE_OFFLINE",
            t!("Source hors ligne", "Source offline"),
            t!(
                "Windows a placé le disque source hors ligne ; il reste lisible secteur par secteur, ce qui convient au clonage.",
                "Windows has put the source disk offline; it can still be read sector by sector, which is fine for cloning."
            ),
        ));
    }

    // --- Données présentes sur la destination ------------------------------------
    let dst_used: u64 = dst.partitions.iter().filter_map(|p| p.used_bytes).sum();
    if !dst.partitions.is_empty() {
        let mounts = dst.mounts();
        let mut detail = tf!(
            "Le disque de destination contient {} partition(s){}. TOUT son contenu sera définitivement effacé et remplacé par celui de la source.",
            "The destination disk contains {} partition(s){}. ALL its content will be permanently erased and replaced with the source's.",
            dst.partitions.len(),
            if dst_used > 0 { tf!(", dont environ {} de données", ", including about {} of data", human_size(dst_used)) } else { String::new() }
        );
        if !mounts.is_empty() {
            detail.push_str(&tf!(" Volumes concernés : {}.", " Affected volumes: {}.", mounts.join(", ")));
        }
        out.push(
            Finding::warning("DEST_HAS_DATA", t!("La destination contient des données", "The destination contains data"), detail)
                .scope("destination"),
        );
    }
    if dst.partitions.iter().any(|p| p.role == crate::partition::PartRole::Esp) && dst.partitions.len() >= 2 {
        out.push(
            Finding::warning(
                "DEST_LOOKS_BOOTABLE",
                t!("La destination ressemble à un disque système", "The destination looks like a system disk"),
                t!("Le disque de destination contient une partition de démarrage EFI : il porte peut-être une installation \
                 de Windows ou d'un autre système. Vérifiez qu'il s'agit bien du disque à écraser.", "The destination disk contains an EFI boot partition: it may hold an installation of Windows or another system. Make sure it is really the disk to overwrite."),
            )
            .scope("destination"),
        );
    }

    // --- Inversion probable ---------------------------------------------------------
    let src_blank = src.table == TableKind::None && src.partitions.is_empty();
    let src_used: u64 = src.partitions.iter().filter_map(|p| p.used_bytes).sum();
    if src_blank && !dst.partitions.is_empty() {
        out.push(Finding::critical(
            "SWAP_SUSPECTED",
            t!("Source vide, destination pleine : inversion probable", "Empty source, full destination: probably swapped"),
            t!("Le disque choisi comme SOURCE ne contient aucune partition, alors que la DESTINATION en contient. \
             Cloner dans ce sens effacerait vos données en les remplaçant par un disque vide. Vérifiez le sens de l'opération.", "The disk chosen as SOURCE contains no partition, while the DESTINATION does. Cloning in this direction would erase your data and replace it with an empty disk. Check the direction of the operation."),
        ));
    } else if src_used > 0 && dst_used > src_used.saturating_mul(4) && dst_used > 1 << 30 {
        out.push(Finding::warning(
            "SWAP_POSSIBLE",
            t!("La destination contient bien plus de données que la source", "The destination holds much more data than the source"),
            tf!(
                "La source contient environ {} de données, la destination environ {}. \
                 Assurez-vous de ne pas avoir inversé la source et la destination.",
                "The source holds about {} of data, the destination about {}. Make sure you have not swapped source and destination.",
                human_size(src_used),
                human_size(dst_used)
            ),
        ));
    }

    // --- Matériel -------------------------------------------------------------------
    if src.bus == "USB" || dst.bus == "USB" {
        out.push(Finding::info(
            "USB_INVOLVED",
            t!("Connexion USB", "USB connection"),
            t!("Au moins un des disques est relié en USB. Le débit dépendra du boîtier et du port ; évitez les concentrateurs \
             non alimentés et ne débranchez rien pendant l'opération.", "At least one of the disks is connected over USB. Speed will depend on the enclosure and port; avoid unpowered hubs and do not unplug anything during the operation."),
        ));
    }
    if dst.removable && dst.size < 64 * (1 << 30) && src.size > dst.size {
        out.push(
            Finding::info(
                "DEST_SMALL_REMOVABLE",
                t!("Petit support amovible en destination", "Small removable media as destination"),
                t!("La destination est un petit support amovible.", "The destination is a small removable medium."),
            )
            .scope("destination"),
        );
    }

    // --- Constats issus de l'analyse et du plan -------------------------------------
    if let Some(a) = src_analysis {
        for f in a.all_findings() {
            let mut f = f;
            if f.scope.is_none() {
                f.scope = Some("source".into());
            }
            // Les incohérences de table ne bloquent que le travail par partition ;
            // le plan les reporte lui-même si elles s'appliquent au mode choisi.
            f.blocking = false;
            out.push(f);
        }
    }
    if let Some(p) = plan {
        out.extend(p.findings.iter().cloned());
        if !p.is_blocked() && p.table != TableKind::None && src.kind == DiskKind::Physical && dst.kind == DiskKind::Physical {
            out.push(Finding::info(
                "IDENTITY_PRESERVED",
                t!("Le clone portera les mêmes identifiants que la source", "The clone will carry the same identifiers as the source"),
                t!("Pour que Windows démarre sur le clone, les identifiants de disque et de partitions sont conservés. \
                 Tant que les deux disques sont branchés sur le même PC, Windows garde l'un des deux hors ligne \
                 (« conflit de signature »). Débranchez le disque source avant de démarrer sur le clone, et ne remettez pas \
                 le clone « en ligne » depuis Windows tant que la source est présente : Windows modifierait ses identifiants.", "For Windows to boot from the clone, the disk and partition identifiers are kept. While both disks are connected to the same PC, Windows keeps one of them offline (\"signature collision\"). Unplug the source disk before booting from the clone, and do not bring the clone \"online\" from Windows while the source is present: Windows would change its identifiers."),
            ));
        }
        if p.mode == CloneMode::RawDisk && !p.is_blocked() {
            out.push(Finding::info(
                "RAW_DISK_MODE",
                t!("Copie brute du disque entier", "Raw copy of the whole disk"),
                t!(
                    "Tous les secteurs sont copiés, y compris l'espace libre et d'éventuels défauts logiques de la source.",
                    "Every sector is copied, including free space and any logical defects of the source."
                ),
            ));
        }
    }

    // Dédoublonne (même code + même portée + même détail).
    let mut seen: Vec<(String, Option<String>, String)> = Vec::new();
    out.retain(|f| {
        let key = (f.code.clone(), f.scope.clone(), f.detail.clone());
        if seen.contains(&key) {
            false
        } else {
            seen.push(key);
            true
        }
    });
    out.sort_by(|a, b| b.blocking.cmp(&a.blocking).then(b.severity.cmp(&a.severity)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diag::has_blocking;
    use crate::model::{MediaKind, PartitionInfo};
    use crate::partition::PartRole;

    fn disk(n: u32, size: u64) -> DiskInfo {
        DiskInfo {
            id: format!("disk:{n}"),
            kind: DiskKind::Physical,
            path: format!(r"\\.\PhysicalDrive{n}"),
            number: Some(n),
            model: format!("Modèle {n}"),
            vendor: String::new(),
            serial: format!("SN{n}"),
            firmware: String::new(),
            bus: "SATA".into(),
            media: MediaKind::Ssd,
            size,
            sector_size: 512,
            physical_sector_size: 512,
            removable: false,
            read_only: false,
            offline: false,
            is_system: false,
            hosts_app: false,
            table: TableKind::Gpt,
            partitions: vec![],
            used_bytes: None,
            fingerprint: String::new(),
            deep: true,
            notes: vec![],
        }
    }

    fn part(used: u64, mount: Option<&str>, role: PartRole) -> PartitionInfo {
        PartitionInfo {
            number: 1,
            offset: 1 << 20,
            size: 100 << 30,
            role,
            type_label: "Données de base".into(),
            name: None,
            fs: Some("NTFS".into()),
            label: None,
            used_bytes: Some(used),
            mounts: mount.map(|m| vec![m.to_string()]).unwrap_or_default(),
            active: false,
            volume_path: None,
        }
    }

    fn codes(f: &[Finding]) -> Vec<&str> {
        f.iter().map(|x| x.code.as_str()).collect()
    }

    #[test]
    fn same_disk_is_blocked() {
        let a = disk(1, 1 << 40);
        let f = precheck(&a, &a.clone(), None, None);
        assert_eq!(codes(&f), vec!["SAME_DISK"]);
        assert!(has_blocking(&f));
        // Même numéro Windows sous un autre identifiant interne.
        let mut b = disk(1, 1 << 40);
        b.id = "autre".into();
        assert!(has_blocking(&precheck(&a, &b, None, None)));
    }

    #[test]
    fn system_destination_is_blocked() {
        let a = disk(1, 1 << 40);
        let mut b = disk(2, 1 << 40);
        b.is_system = true;
        let f = precheck(&a, &b, None, None);
        assert!(f.iter().any(|x| x.code == "DEST_IS_SYSTEM" && x.blocking));
        let mut c = disk(3, 1 << 40);
        c.hosts_app = true;
        assert!(precheck(&a, &c, None, None).iter().any(|x| x.code == "DEST_HOSTS_APP" && x.blocking));
        let mut d = disk(4, 1 << 40);
        d.read_only = true;
        assert!(precheck(&a, &d, None, None).iter().any(|x| x.code == "DEST_READ_ONLY" && x.blocking));
    }

    #[test]
    fn live_system_source_is_blocked_with_explanation() {
        let mut a = disk(0, 1 << 40);
        a.is_system = true;
        let b = disk(1, 1 << 40);
        let f = precheck(&a, &b, None, None);
        let x = f.iter().find(|x| x.code == "SOURCE_LIVE_SYSTEM").unwrap();
        assert!(x.blocking && x.detail.contains("VSS"));
    }

    #[test]
    fn data_on_destination_and_swap_are_flagged() {
        let mut a = disk(1, 1 << 40);
        a.table = TableKind::None;
        let mut b = disk(2, 1 << 40);
        b.partitions = vec![part(50 << 30, Some("E:\\"), PartRole::BasicData)];
        let f = precheck(&a, &b, None, None);
        assert!(codes(&f).contains(&"DEST_HAS_DATA"));
        assert!(codes(&f).contains(&"SWAP_SUSPECTED"));
        assert!(!has_blocking(&f), "avertissements forts, mais la décision reste à l'utilisateur");
        assert!(f.iter().find(|x| x.code == "DEST_HAS_DATA").unwrap().detail.contains("E:\\"));

        let mut a = disk(1, 1 << 40);
        a.partitions = vec![part(2 << 30, None, PartRole::BasicData)];
        let f = precheck(&a, &b, None, None);
        assert!(codes(&f).contains(&"SWAP_POSSIBLE"));
    }

    #[test]
    fn mounted_source_volumes_are_announced() {
        let mut a = disk(1, 1 << 40);
        a.partitions = vec![part(2 << 30, Some("D:\\"), PartRole::BasicData)];
        let b = disk(2, 1 << 40);
        let f = precheck(&a, &b, None, None);
        assert!(f.iter().any(|x| x.code == "SOURCE_VOLUMES_LOCKED" && x.detail.contains("D:\\")));
        assert!(!has_blocking(&f));
    }
}
