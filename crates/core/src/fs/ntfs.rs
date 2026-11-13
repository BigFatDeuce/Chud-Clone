//! NTFS : carte d'allocation ($Bitmap), état du journal et recherche de fichiers.
//!
//! L'analyse des structures NTFS est déléguée au crate `ntfs` (lecture seule). Ce
//! module ne fait qu'en tirer la carte des clusters utilisés et décider si elle est
//! digne de confiance.

use super::{BitmapScanner, Extent, FsAnalysis, normalize};
use crate::device::{BlockDevice, DeviceReader};
use crate::diag::Finding;
use ::ntfs::indexes::NtfsFileNameIndex;
use ::ntfs::structured_values::NtfsVolumeFlags;
use ::ntfs::{KnownNtfsFileRecordNumber, Ntfs, NtfsFile, NtfsReadSeek};
use std::io::{Read, Seek};

/// Au-delà de cette taille, la zone située après le dernier cluster n'est plus copiée
/// en entier (cas d'un système de fichiers plus petit que sa partition).
const MAX_FULL_TAIL: u64 = 64 * 1024 * 1024;

fn err_text(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// Lit jusqu'à `max` octets du flux de données sans nom d'un fichier.
fn read_file_head<T: Read + Seek>(fs: &mut T, file: &NtfsFile, max: usize) -> Result<Vec<u8>, String> {
    let item = file.data(fs, "").ok_or(t!("attribut $DATA absent", "missing $DATA attribute"))?.map_err(err_text)?;
    let attr = item.to_attribute().map_err(err_text)?;
    let mut value = attr.value(fs).map_err(err_text)?;
    let n = (value.len() as usize).min(max);
    let mut buf = vec![0u8; n];
    value.read_exact(fs, &mut buf).map_err(err_text)?;
    Ok(buf)
}

/// État du journal $LogFile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogState {
    /// Journal vide (rempli de 0xFF) ou marqué propre : aucune opération en attente.
    Clean,
    /// Des opérations journalisées peuvent rester à rejouer.
    Unclean,
    /// Structure non reconnue.
    Unknown,
}

/// Interprète les deux pages de redémarrage du journal.
///
/// Même règle que ntfs-3g : le journal est propre s'il est vide, si aucun client
/// n'est actif, ou si l'indicateur « volume propre » est positionné.
pub fn logfile_state(head: &[u8]) -> LogState {
    if head.len() < 4096 {
        return LogState::Unknown;
    }
    if head.iter().take(8192.min(head.len())).all(|b| *b == 0xFF) {
        return LogState::Clean;
    }
    let page_size = {
        let p = u32::from_le_bytes([head[16], head[17], head[18], head[19]]) as usize;
        if p.is_power_of_two() && (512..=65536).contains(&p) { p } else { 4096 }
    };
    let mut best: Option<(u64, bool)> = None;
    for page in 0..2 {
        let o = page * page_size;
        if o + 64 > head.len() || &head[o..o + 4] != b"RSTR" {
            continue;
        }
        let ra = o + u16::from_le_bytes([head[o + 24], head[o + 25]]) as usize;
        if ra + 16 > head.len() {
            continue;
        }
        let mut lsn = [0u8; 8];
        lsn.copy_from_slice(&head[ra..ra + 8]);
        let lsn = u64::from_le_bytes(lsn);
        let client_in_use = u16::from_le_bytes([head[ra + 12], head[ra + 13]]);
        let flags = u16::from_le_bytes([head[ra + 14], head[ra + 15]]);
        let clean = client_in_use == 0xFFFF || flags & 0x0002 != 0;
        if best.is_none_or(|(l, _)| lsn >= l) {
            best = Some((lsn, clean));
        }
    }
    match best {
        Some((_, true)) => LogState::Clean,
        Some((_, false)) => LogState::Unclean,
        None => LogState::Unknown,
    }
}

/// Recherche un chemin (composants insensibles à la casse) depuis la racine.
fn lookup<'n, T: Read + Seek>(ntfs: &'n Ntfs, fs: &mut T, path: &[&str]) -> Result<Option<NtfsFile<'n>>, String> {
    let mut current = ntfs.root_directory(fs).map_err(err_text)?;
    for comp in path {
        if !current.is_directory() {
            return Ok(None);
        }
        let index = current.directory_index(fs).map_err(err_text)?;
        let mut finder = index.finder();
        let next = match NtfsFileNameIndex::find(&mut finder, ntfs, fs, comp) {
            None => return Ok(None),
            Some(entry) => entry.map_err(err_text)?.to_file(ntfs, fs).map_err(err_text)?,
        };
        current = next;
    }
    Ok(Some(current))
}

/// Indique si un fichier ou dossier existe dans le volume NTFS `[base, base+len)`.
pub fn path_exists(dev: &mut dyn BlockDevice, base: u64, len: u64, path: &[&str]) -> Result<bool, String> {
    let mut fs = DeviceReader::new(dev, base, len);
    let mut ntfs = Ntfs::new(&mut fs).map_err(err_text)?;
    ntfs.read_upcase_table(&mut fs).map_err(err_text)?;
    Ok(lookup(&ntfs, &mut fs, path)?.is_some())
}

/// Lit le contenu complet d'un fichier (tests et vérifications ; fichiers modestes).
pub fn read_file(dev: &mut dyn BlockDevice, base: u64, len: u64, path: &[&str], max: usize) -> Result<Option<Vec<u8>>, String> {
    let mut fs = DeviceReader::new(dev, base, len);
    let mut ntfs = Ntfs::new(&mut fs).map_err(err_text)?;
    ntfs.read_upcase_table(&mut fs).map_err(err_text)?;
    match lookup(&ntfs, &mut fs, path)? {
        None => Ok(None),
        Some(f) => read_file_head(&mut fs, &f, max).map(Some),
    }
}

/// Liste récursive (chemin, taille) de tous les fichiers ordinaires. Réservé aux tests
/// et aux comparaisons de petits volumes.
pub fn list_files(dev: &mut dyn BlockDevice, base: u64, len: u64) -> Result<Vec<(String, u64)>, String> {
    let mut fs = DeviceReader::new(dev, base, len);
    let mut ntfs = Ntfs::new(&mut fs).map_err(err_text)?;
    ntfs.read_upcase_table(&mut fs).map_err(err_text)?;
    let mut out = Vec::new();
    let root = ntfs.root_directory(&mut fs).map_err(err_text)?;
    let mut stack: Vec<(String, u64)> = vec![(String::new(), root.file_record_number())];
    while let Some((prefix, record)) = stack.pop() {
        let dir = ntfs.file(&mut fs, record).map_err(err_text)?;
        let index = dir.directory_index(&mut fs).map_err(err_text)?;
        let mut children: Vec<(String, u64, bool, u64)> = Vec::new();
        {
            let mut iter = index.entries();
            while let Some(entry) = iter.next(&mut fs) {
                let entry = entry.map_err(err_text)?;
                let key = entry.key().ok_or(t!("entrée d'index sans clé", "index entry without a key"))?.map_err(err_text)?;
                // Les noms courts DOS doublonnent les noms longs.
                if key.namespace() == ::ntfs::structured_values::NtfsFileNamespace::Dos {
                    continue;
                }
                let name = key.name().to_string_lossy();
                if name.starts_with('$') && prefix.is_empty() || name == "." {
                    continue;
                }
                children.push((name, entry.file_reference().file_record_number(), key.is_directory(), key.data_size()));
            }
        }
        for (name, rec, is_dir, _) in children {
            let path = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
            if is_dir {
                stack.push((path, rec));
            } else {
                // La taille de l'index peut être périmée : on lit celle de l'attribut $DATA.
                let f = ntfs.file(&mut fs, rec).map_err(err_text)?;
                let size = match f.data(&mut fs, "") {
                    Some(item) => item.map_err(err_text)?.to_attribute().map_err(err_text)?.value_length(),
                    None => 0,
                };
                out.push((path, size));
            }
        }
    }
    out.sort();
    Ok(out)
}

pub(super) fn analyze(dev: &mut dyn BlockDevice, base: u64, len: u64, out: &mut FsAnalysis) {
    let ss = dev.sector_size() as u64;
    let lba = base / ss;
    if let Err(e) = analyze_inner(dev, base, len, out) {
        out.used_extents = None;
        out.raw_reason = Some(tf!("structures NTFS illisibles ({e})", "unreadable NTFS structures ({e})"));
        out.findings.push(
            Finding::error(
                "NTFS_UNREADABLE",
                t!("Structures NTFS illisibles", "Unreadable NTFS structures"),
                tf!("Le volume se présente comme NTFS mais ses structures n'ont pas pu être lues : {e}. \
                     Il peut être endommagé. La partition sera copiée intégralement, secteur par secteur, \
                     et le clone portera les mêmes défauts.", "The volume presents itself as NTFS but its structures could not be read: {e}. It may be damaged. The partition will be copied in full, sector by sector, and the clone will carry the same defects."
                ),
            )
            .at_lba(lba),
        );
        return;
    }
    // Agrandissement possible ? Uniquement sur un volume dont la carte d'allocation est
    // fiable ; contrôle en lecture seule des structures que l'agrandissement modifie.
    match (&out.raw_reason, out.used_extents.is_some()) {
        (_, true) => match super::ntfs_grow::preflight(dev, base, len) {
            Ok(()) => out.grow_ready = true,
            Err(e) => out.grow_issue = Some(e),
        },
        (Some(r), false) => out.grow_issue = Some(r.clone()),
        (None, false) => out.grow_issue = Some(t!("carte d'allocation non lue", "allocation map not read").into()),
    }
}

fn analyze_inner(dev: &mut dyn BlockDevice, base: u64, len: u64, out: &mut FsAnalysis) -> Result<(), String> {
    let dev_sector = dev.sector_size() as u64;
    let mut fs = DeviceReader::new(dev, base, len);
    let mut ntfs = Ntfs::new(&mut fs).map_err(err_text)?;
    let cluster = ntfs.cluster_size() as u64;
    let fs_size = ntfs.size();
    let fs_sector = ntfs.sector_size() as u64;
    out.cluster_size = Some(cluster as u32);
    out.fs_size = Some(fs_size);
    if cluster == 0 || fs_size == 0 {
        return Err(t!("secteur d'amorçage incohérent", "inconsistent boot sector").into());
    }
    if fs_sector != dev_sector {
        out.findings.push(Finding::warning(
            "NTFS_SECTOR_MISMATCH",
            t!("Taille de secteur NTFS différente de celle du disque", "NTFS sector size differs from the disk's"),
            tf!(
                "Le volume NTFS a été formaté pour des secteurs de {fs_sector} octets alors que le disque en annonce {dev_sector}. \
                 Windows peut refuser de le monter.",
                "The NTFS volume was formatted for {fs_sector}-byte sectors while the disk reports {dev_sector}. Windows may refuse to mount it."
            ),
        ));
    }
    if fs_size > len {
        out.raw_reason = Some(
            t!("le système de fichiers déclare une taille supérieure à sa partition", "the file system claims a size larger than its partition")
                .into(),
        );
        out.findings.push(Finding::error(
            "NTFS_LARGER_THAN_PARTITION",
            t!("Volume NTFS plus grand que sa partition", "NTFS volume larger than its partition"),
            tf!("Le volume NTFS déclare {fs_size} octets mais la partition n'en contient que {len}. \
                 La table de partitions ou le système de fichiers est incohérent ; la partition sera copiée telle quelle.", "The NTFS volume claims {fs_size} bytes but the partition only holds {len}. The partition table or the file system is inconsistent; the partition will be copied as is."
            ),
        ));
        return Ok(());
    }
    if let Some(Ok(name)) = ntfs.volume_name(&mut fs) {
        let s = name.name().to_string_lossy();
        if !s.is_empty() {
            out.label = Some(s);
        }
    }

    // --- Fiabilité de la carte d'allocation ---------------------------------
    let mut unreliable: Option<String> = None;
    let flags = ntfs.volume_info(&mut fs).map_err(err_text)?.flags();
    if flags.contains(NtfsVolumeFlags::IS_DIRTY) {
        unreliable = Some(t!("le volume est marqué « à vérifier » (dirty)", "the volume is flagged \"needs checking\" (dirty)").into());
        out.findings.push(Finding::warning(
            "NTFS_DIRTY",
            t!("Volume NTFS marqué « à vérifier »", "NTFS volume flagged \"needs checking\""),
            t!("Windows a signalé ce volume comme potentiellement incohérent (arrêt brutal ou erreurs détectées). \
             Sa carte d'allocation n'est pas fiable : la partition sera copiée intégralement, secteur par secteur. \
             Lancer « chkdsk /f » sur la source avant de cloner est recommandé.", "Windows flagged this volume as possibly inconsistent (hard shutdown or errors detected). Its allocation map cannot be trusted: the partition will be copied in full, sector by sector. Running \"chkdsk /f\" on the source before cloning is recommended."),
        ));
    }
    let logfile = ntfs.file(&mut fs, KnownNtfsFileRecordNumber::LogFile as u64).map_err(err_text)?;
    match read_file_head(&mut fs, &logfile, 2 * 65536).map(|h| logfile_state(&h)) {
        Ok(LogState::Clean) => {}
        Ok(state) => {
            if unreliable.is_none() {
                unreliable =
                    Some(t!("le journal NTFS contient des opérations non terminées", "the NTFS journal contains unfinished operations").into());
            }
            out.findings.push(Finding::warning(
                "NTFS_LOG_UNCLEAN",
                t!("Journal NTFS non vidé", "NTFS journal not flushed"),
                tf!("Le journal du volume {} : le volume n'a pas été démonté proprement, ou il est en cours d'utilisation. \
                     Sa carte d'allocation n'est pas fiable ; la partition sera copiée intégralement, secteur par secteur.", "The volume journal {}: the volume was not unmounted cleanly, or it is in use. Its allocation map cannot be trusted; the partition will be copied in full, sector by sector.",
                    if state == LogState::Unclean { t!("contient des opérations en attente", "contains pending operations") } else { t!("n'a pas pu être interprété", "could not be interpreted") }
                ),
            ));
        }
        Err(e) => return Err(tf!("$LogFile illisible : {e}", "unreadable $LogFile: {e}")),
    }
    ntfs.read_upcase_table(&mut fs).map_err(err_text)?;
    if let Some(hiber) = lookup(&ntfs, &mut fs, &["hiberfil.sys"])?
        && let Ok(h) = read_file_head(&mut fs, &hiber, 4096)
        && h.len() >= 4
        && h[..4].eq_ignore_ascii_case(b"hibr")
    {
        if unreliable.is_none() {
            unreliable = Some(t!("Windows est en veille prolongée sur ce volume", "Windows is hibernated on this volume").into());
        }
        out.findings.push(Finding::warning(
            "NTFS_HIBERNATED",
            t!("Windows en veille prolongée ou démarrage rapide", "Windows hibernated or Fast Startup"),
            t!("Ce volume contient une session Windows mise en veille prolongée (ou le « démarrage rapide » est actif). \
                     L'état sur disque dépend de la mémoire enregistrée : la partition sera copiée intégralement, secteur par secteur. \
                     Pour un clone plus sûr, arrêtez complètement Windows (« shutdown /s /t 0 ») avant de cloner.", "This volume holds a hibernated Windows session (or Fast Startup is enabled). The on-disk state depends on the saved memory: the partition will be copied in full, sector by sector. For a safer clone, shut Windows down completely (\"shutdown /s /t 0\") before cloning."),
        ));
    }

    // --- Lecture de $Bitmap ---------------------------------------------------
    let total_clusters = fs_size / cluster;
    let bitmap = ntfs.file(&mut fs, KnownNtfsFileRecordNumber::Bitmap as u64).map_err(err_text)?;
    let item = bitmap.data(&mut fs, "").ok_or(t!("$Bitmap sans attribut $DATA", "$Bitmap without a $DATA attribute"))?.map_err(err_text)?;
    let attr = item.to_attribute().map_err(err_text)?;
    let mut value = attr.value(&mut fs).map_err(err_text)?;
    let needed = total_clusters.div_ceil(8);
    if value.len() < needed {
        return Err(tf!(
            "$Bitmap trop court ({} octets pour {} clusters)",
            "$Bitmap too short ({} bytes for {} clusters)",
            value.len(),
            total_clusters
        ));
    }
    let mut scanner = BitmapScanner::new(cluster, 0, total_clusters);
    let mut buf = vec![0u8; 256 * 1024];
    let mut remaining = needed;
    while remaining > 0 {
        let n = remaining.min(buf.len() as u64) as usize;
        value.read_exact(&mut fs, &mut buf[..n]).map_err(|e| tf!("$Bitmap illisible : {e}", "unreadable $Bitmap: {e}"))?;
        scanner.feed(&buf[..n]);
        remaining -= n as u64;
    }
    let (mut extents, used_clusters, seen) = scanner.finish();
    if seen != total_clusters {
        return Err(t!("lecture de $Bitmap incomplète", "incomplete $Bitmap read").into());
    }
    if extents.first().is_none_or(|e| e.offset != 0) {
        return Err(t!(
            "$Bitmap incohérent : le cluster 0 (secteur d'amorçage) est marqué libre",
            "inconsistent $Bitmap: cluster 0 (boot sector) is marked free"
        )
        .into());
    }
    out.used_bytes = Some(used_clusters * cluster);
    out.last_used_end = extents.last().map(|e| e.end());

    // Zone située après le dernier cluster entier : elle contient le secteur
    // d'amorçage de secours (dernier secteur du volume).
    let clusters_end = total_clusters * cluster;
    if len > clusters_end {
        let tail = len - clusters_end;
        if tail <= MAX_FULL_TAIL {
            extents.push(Extent { offset: clusters_end, len: tail });
        } else {
            let backup_end = (fs_size + fs_sector).min(len);
            extents.push(Extent { offset: clusters_end, len: backup_end - clusters_end });
            out.findings.push(Finding::info(
                "NTFS_SMALLER_THAN_PARTITION",
                t!("Volume NTFS plus petit que sa partition", "NTFS volume smaller than its partition"),
                tf!(
                    "Le volume NTFS occupe {} alors que la partition fait {}. L'espace excédentaire n'est pas copié.",
                    "The NTFS volume spans {} while the partition is {}. The extra space is not copied.",
                    crate::diag::human_size(fs_size),
                    crate::diag::human_size(len)
                ),
            ));
        }
    }

    match unreliable {
        None => out.used_extents = Some(normalize(extents)),
        Some(reason) => out.raw_reason = Some(reason),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logfile_states() {
        assert_eq!(logfile_state(&vec![0xFFu8; 8192]), LogState::Clean);
        assert_eq!(logfile_state(&vec![0u8; 8192]), LogState::Unknown);

        let mut h = vec![0u8; 8192];
        for page in 0..2usize {
            let o = page * 4096;
            h[o..o + 4].copy_from_slice(b"RSTR");
            h[o + 16..o + 20].copy_from_slice(&4096u32.to_le_bytes());
            h[o + 24..o + 26].copy_from_slice(&0x30u16.to_le_bytes());
            let ra = o + 0x30;
            h[ra..ra + 8].copy_from_slice(&(100 + page as u64).to_le_bytes());
            h[ra + 12..ra + 14].copy_from_slice(&0u16.to_le_bytes()); // client actif
        }
        assert_eq!(logfile_state(&h), LogState::Unclean);
        // La page la plus récente (LSN le plus élevé) porte l'indicateur « propre ».
        let ra = 4096 + 0x30;
        h[ra + 14..ra + 16].copy_from_slice(&0x0002u16.to_le_bytes());
        assert_eq!(logfile_state(&h), LogState::Clean);
        // Aucun client actif = propre également.
        h[ra + 14..ra + 16].copy_from_slice(&0u16.to_le_bytes());
        h[ra + 12..ra + 14].copy_from_slice(&0xFFFFu16.to_le_bytes());
        assert_eq!(logfile_state(&h), LogState::Clean);
    }
}
