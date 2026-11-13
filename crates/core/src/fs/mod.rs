//! Reconnaissance des systèmes de fichiers et carte des blocs utilisés.
//!
//! Le « clonage intelligent » ne copie que les blocs marqués utilisés par le système
//! de fichiers. Ce n'est sûr que si la carte d'allocation lue sur le disque est
//! fiable : volume proprement démonté, journal vide, pas d'hibernation. Dans tous les
//! autres cas, [`analyze`] ne fournit pas d'étendues et la partition est copiée
//! intégralement, secteur par secteur.

pub mod fat;
pub mod ntfs;
pub mod ntfs_grow;

use crate::device::{BlockDevice, read_vec};
use crate::diag::Finding;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FsKind {
    Ntfs,
    Fat12,
    Fat16,
    Fat32,
    Exfat,
    /// Volume chiffré BitLocker (contenu opaque).
    Bitlocker,
    Refs,
    Ext,
    Xfs,
    Btrfs,
    LinuxSwap,
    Luks,
    /// Début de partition entièrement à zéro.
    Blank,
    Unknown,
}

impl FsKind {
    pub fn label(self) -> &'static str {
        match self {
            FsKind::Ntfs => "NTFS",
            FsKind::Fat12 => "FAT12",
            FsKind::Fat16 => "FAT16",
            FsKind::Fat32 => "FAT32",
            FsKind::Exfat => "exFAT",
            FsKind::Bitlocker => t!("BitLocker (chiffré)", "BitLocker (encrypted)"),
            FsKind::Refs => "ReFS",
            FsKind::Ext => "ext2/3/4",
            FsKind::Xfs => "XFS",
            FsKind::Btrfs => "Btrfs",
            FsKind::LinuxSwap => t!("Swap Linux", "Linux swap"),
            FsKind::Luks => t!("LUKS (chiffré)", "LUKS (encrypted)"),
            FsKind::Blank => t!("Vide", "Blank"),
            FsKind::Unknown => t!("Inconnu", "Unknown"),
        }
    }

    pub fn is_fat(self) -> bool {
        matches!(self, FsKind::Fat12 | FsKind::Fat16 | FsKind::Fat32)
    }
}

/// Plage d'octets relative au début de la partition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Extent {
    pub offset: u64,
    pub len: u64,
}

impl Extent {
    pub fn end(&self) -> u64 {
        self.offset + self.len
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsAnalysis {
    pub kind: FsKind,
    pub label: Option<String>,
    /// Taille du système de fichiers (peut être inférieure à celle de la partition).
    pub fs_size: Option<u64>,
    pub cluster_size: Option<u32>,
    /// Octets utilisés selon le système de fichiers, si connus.
    pub used_bytes: Option<u64>,
    /// Étendues à copier pour un clonage intelligent. `None` : copie brute obligatoire.
    #[serde(skip)]
    pub used_extents: Option<Vec<Extent>>,
    /// Pourquoi le clonage intelligent n'est pas possible, le cas échéant.
    pub raw_reason: Option<String>,
    /// Dernier octet utilisé + 1 (taille minimale sans déplacer de données).
    pub last_used_end: Option<u64>,
    /// NTFS : le volume peut être agrandi hors ligne (structures contrôlées).
    #[serde(default)]
    pub grow_ready: bool,
    /// NTFS : pourquoi il ne peut pas l'être, le cas échéant.
    #[serde(default)]
    pub grow_issue: Option<String>,
    pub findings: Vec<Finding>,
}

impl FsAnalysis {
    pub fn new(kind: FsKind) -> Self {
        Self {
            kind,
            label: None,
            fs_size: None,
            cluster_size: None,
            used_bytes: None,
            used_extents: None,
            raw_reason: None,
            last_used_end: None,
            grow_ready: false,
            grow_issue: None,
            findings: Vec::new(),
        }
    }

    pub fn smart_capable(&self) -> bool {
        self.used_extents.is_some()
    }
}

/// Identifie un système de fichiers à partir des premiers octets de la partition.
/// `head` doit contenir au moins 4096 octets (davantage améliore la détection).
pub fn probe_kind(head: &[u8]) -> FsKind {
    if head.len() < 512 {
        return FsKind::Unknown;
    }
    let oem = &head[3..11];
    if oem == b"NTFS    " {
        return FsKind::Ntfs;
    }
    if oem == b"EXFAT   " {
        return FsKind::Exfat;
    }
    if oem == b"-FVE-FS-" || oem == b"MSWIN4.1" && head.len() > 0x1A8 && &head[0x1A0..0x1A8] == b"-FVE-FS-" {
        return FsKind::Bitlocker;
    }
    if &head[3..7] == b"ReFS" {
        return FsKind::Refs;
    }
    if &head[..6] == b"LUKS\xBA\xBE" {
        return FsKind::Luks;
    }
    if &head[..4] == b"XFSB" {
        return FsKind::Xfs;
    }
    if head.len() >= 1082 && head[1080] == 0x53 && head[1081] == 0xEF {
        return FsKind::Ext;
    }
    if head.len() >= 4096 && (&head[4086..4096] == b"SWAPSPACE2" || &head[4086..4096] == b"SWAP-SPACE") {
        return FsKind::LinuxSwap;
    }
    if head.len() >= 0x10048 && &head[0x10040..0x10048] == b"_BHRfS_M" {
        return FsKind::Btrfs;
    }
    if let Some(kind) = fat::probe(head) {
        return kind;
    }
    if head.iter().all(|b| *b == 0) {
        return FsKind::Blank;
    }
    FsKind::Unknown
}

/// Analyse la partition `[base, base+len)` : type, taille, espace utilisé et, si la
/// carte d'allocation est fiable, les étendues à copier.
///
/// N'échoue jamais : une erreur de lecture devient un constat et impose la copie brute.
pub fn analyze(dev: &mut dyn BlockDevice, base: u64, len: u64) -> FsAnalysis {
    let ss = dev.sector_size() as u64;
    // 128 Kio suffisent à toutes les signatures reconnues.
    let want = (128 * 1024u64).min(len);
    let want = want - want % ss;
    if want < 512 {
        return FsAnalysis::new(FsKind::Unknown);
    }
    let head = match read_vec(dev, base, want as usize) {
        Ok(h) => h,
        Err(e) => {
            let mut a = FsAnalysis::new(FsKind::Unknown);
            a.raw_reason = Some(t!("début de partition illisible", "start of partition unreadable").into());
            a.findings.push(
                Finding::error(
                    "FS_HEAD_UNREADABLE",
                    t!("Début de partition illisible", "Start of partition unreadable"),
                    tf!(
                        "Impossible de lire le début de la partition ({e}). Le système de fichiers ne peut pas être identifié.",
                        "Cannot read the start of the partition ({e}). The file system cannot be identified."
                    ),
                )
                .at_lba(base / ss),
            );
            return a;
        }
    };
    let kind = probe_kind(&head[..want as usize]);
    let mut a = FsAnalysis::new(kind);
    match kind {
        FsKind::Ntfs => ntfs::analyze(dev, base, len, &mut a),
        FsKind::Fat12 | FsKind::Fat16 | FsKind::Fat32 => fat::analyze(dev, base, len, &mut a),
        FsKind::Bitlocker => {
            a.raw_reason = Some(
                t!(
                    "volume chiffré BitLocker : contenu opaque, copie intégrale secteur par secteur",
                    "BitLocker-encrypted volume: opaque content, full sector-by-sector copy"
                )
                .into(),
            );
        }
        FsKind::Blank => {
            a.raw_reason = Some(t!("aucun système de fichiers reconnu", "no recognised file system").into());
        }
        other => {
            a.raw_reason = Some(tf!(
                "système de fichiers {} non analysé par cette version : copie intégrale secteur par secteur",
                "file system {} not analysed by this version: full sector-by-sector copy",
                other.label()
            ));
        }
    }
    a
}

/// Fusionne des étendues triées et adjacentes ou chevauchantes.
pub fn normalize(mut extents: Vec<Extent>) -> Vec<Extent> {
    extents.retain(|e| e.len > 0);
    extents.sort_by_key(|e| e.offset);
    let mut out: Vec<Extent> = Vec::with_capacity(extents.len());
    for e in extents {
        match out.last_mut() {
            Some(last) if e.offset <= last.end() => {
                let end = last.end().max(e.end());
                last.len = end - last.offset;
            }
            _ => out.push(e),
        }
    }
    out
}

/// Construit des étendues à partir d'une carte de bits (1 bit = 1 unité, LSB d'abord).
/// Seuls les `units` premiers bits sont pris en compte. Renvoie (étendues, unités utilisées).
pub(crate) struct BitmapScanner {
    unit_size: u64,
    base_offset: u64,
    units: u64,
    pos: u64,
    run_start: Option<u64>,
    pub extents: Vec<Extent>,
    pub used_units: u64,
}

impl BitmapScanner {
    pub fn new(unit_size: u64, base_offset: u64, units: u64) -> Self {
        Self { unit_size, base_offset, units, pos: 0, run_start: None, extents: Vec::new(), used_units: 0 }
    }

    fn close_run(&mut self, end: u64) {
        if let Some(start) = self.run_start.take() {
            self.extents.push(Extent { offset: self.base_offset + start * self.unit_size, len: (end - start) * self.unit_size });
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if self.pos >= self.units {
                break;
            }
            let remaining = self.units - self.pos;
            if remaining >= 8 && b == 0xFF {
                if self.run_start.is_none() {
                    self.run_start = Some(self.pos);
                }
                self.used_units += 8;
                self.pos += 8;
                continue;
            }
            if remaining >= 8 && b == 0x00 {
                self.close_run(self.pos);
                self.pos += 8;
                continue;
            }
            for bit in 0..8u64.min(remaining) {
                let used = b & (1 << bit) != 0;
                if used {
                    if self.run_start.is_none() {
                        self.run_start = Some(self.pos);
                    }
                    self.used_units += 1;
                } else {
                    self.close_run(self.pos);
                }
                self.pos += 1;
            }
        }
    }

    pub fn finish(mut self) -> (Vec<Extent>, u64, u64) {
        self.close_run(self.pos);
        (self.extents, self.used_units, self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitmap_scanner_runs() {
        // bits : 1111 1111 | 0000 0000 | 1010 0001 (LSB d'abord : 1,0,0,0,0,1,0,1) | 1111 1111
        let mut s = BitmapScanner::new(4096, 0, 28);
        s.feed(&[0xFF, 0x00, 0b1010_0001, 0xFF]);
        let (ext, used, seen) = s.finish();
        assert_eq!(seen, 28);
        assert_eq!(used, 8 + 3 + 4);
        assert_eq!(
            ext,
            vec![
                Extent { offset: 0, len: 8 * 4096 },
                Extent { offset: 16 * 4096, len: 4096 },
                Extent { offset: 21 * 4096, len: 4096 },
                Extent { offset: 23 * 4096, len: 5 * 4096 },
            ]
        );
    }

    #[test]
    fn normalize_merges() {
        let v = normalize(vec![
            Extent { offset: 100, len: 50 },
            Extent { offset: 0, len: 100 },
            Extent { offset: 400, len: 10 },
            Extent { offset: 120, len: 100 },
            Extent { offset: 500, len: 0 },
        ]);
        assert_eq!(v, vec![Extent { offset: 0, len: 220 }, Extent { offset: 400, len: 10 }]);
    }

    #[test]
    fn probe_signatures() {
        let mut h = vec![0u8; 8192];
        assert_eq!(probe_kind(&h), FsKind::Blank);
        h[3..11].copy_from_slice(b"NTFS    ");
        assert_eq!(probe_kind(&h), FsKind::Ntfs);
        h[3..11].copy_from_slice(b"-FVE-FS-");
        assert_eq!(probe_kind(&h), FsKind::Bitlocker);
        let mut h = vec![0u8; 8192];
        h[1080] = 0x53;
        h[1081] = 0xEF;
        assert_eq!(probe_kind(&h), FsKind::Ext);
        let mut h = vec![0u8; 8192];
        h[100] = 7;
        assert_eq!(probe_kind(&h), FsKind::Unknown);
    }
}
