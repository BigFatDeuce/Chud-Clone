//! Agrandissement hors ligne d'un système de fichiers NTFS (sur la destination d'un clone).
//!
//! Reproduit les modifications de l'outil de référence `ntfsresize` (ntfs-3g) lors d'un
//! agrandissement, relevées octet par octet sur des volumes de test :
//!
//! - `$Volume` (enregistrement 3, et sa copie dans `$MFTMirr`) : indicateur « à vérifier »,
//!   pour que Windows contrôle le volume (chkdsk) à son premier montage ;
//! - `$Bitmap` (enregistrement 6) : carte d'allocation étendue aux nouveaux clusters
//!   (libres), bits de remplissage au-delà du dernier cluster positionnés à 1 ;
//! - `$BadClus:$Bad` (enregistrement 8) : flux creux étendu à la nouvelle taille ;
//! - secteur d'amorçage : nombre total de secteurs, plus une copie de secours écrite
//!   dans le dernier secteur de la partition.
//!
//! Aucune donnée de fichier n'est déplacée. Tout est lu, contrôlé et calculé AVANT la
//! première écriture ; le secteur d'amorçage principal est écrit en dernier : tant qu'il
//! ne l'est pas, le volume garde sa taille d'origine.

use crate::device::{AlignedBuf, BlockDevice};

/// Pas des tableaux de correction (« update sequence ») : toujours 512 octets.
const USA_BLOCK: usize = 512;
const ATTR_ATTRIBUTE_LIST: u32 = 0x20;
const ATTR_VOLUME_INFORMATION: u32 = 0x70;
const ATTR_DATA: u32 = 0x80;
const ATTR_END: u32 = 0xFFFF_FFFF;
const VOLUME_IS_DIRTY: u16 = 0x0001;
const CHUNK: u64 = 1024 * 1024;

fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn le64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}
fn put16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
}
fn put32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}
fn put64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}
fn align_up(v: u64, a: u64) -> u64 {
    v.div_ceil(a) * a
}

/// Géométrie lue dans le secteur d'amorçage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub sector: u64,
    pub spc: u64,
    pub cluster: u64,
    /// Secteurs du volume (le secteur d'amorçage de secours n'en fait pas partie).
    pub total_sectors: u64,
    pub mft_lcn: u64,
    pub mftmirr_lcn: u64,
    pub record_size: u64,
}

impl Geometry {
    pub fn clusters(&self) -> u64 {
        self.total_sectors / self.spc
    }
}

pub fn parse_boot(b: &[u8]) -> Result<Geometry, String> {
    if b.len() < 512 {
        return Err(t!("secteur d'amorçage tronqué", "truncated boot sector").into());
    }
    if &b[3..11] != b"NTFS    " {
        return Err(t!("signature NTFS absente du secteur d'amorçage", "NTFS signature missing from the boot sector").into());
    }
    if b[510] != 0x55 || b[511] != 0xAA {
        return Err(t!("signature 55AA absente du secteur d'amorçage", "55AA signature missing from the boot sector").into());
    }
    let sector = le16(b, 0x0B) as u64;
    if !sector.is_power_of_two() || !(256..=4096).contains(&sector) {
        return Err(tf!("taille de secteur NTFS invalide ({sector})", "invalid NTFS sector size ({sector})"));
    }
    let raw = b[0x0D];
    let spc: u64 = match raw {
        1..=0x80 => raw as u64,
        0xF0..=0xFF => 1u64 << (256 - raw as u32),
        _ => {
            return Err(tf!("nombre de secteurs par cluster invalide ({raw:#x})", "invalid sectors-per-cluster value ({raw:#x})"));
        }
    };
    if !spc.is_power_of_two() {
        return Err(tf!("nombre de secteurs par cluster invalide ({spc})", "invalid sectors-per-cluster value ({spc})"));
    }
    let cluster = sector * spc;
    if cluster > 2 * 1024 * 1024 {
        return Err(tf!("taille de cluster non prise en charge ({cluster} octets)", "unsupported cluster size ({cluster} bytes)"));
    }
    let cpr = b[0x40] as i8;
    let record_size = if cpr > 0 {
        cpr as u64 * cluster
    } else {
        let shift = -(cpr as i32);
        if !(9..=16).contains(&shift) {
            return Err(tf!("taille d'enregistrement MFT invalide ({cpr})", "invalid MFT record size ({cpr})"));
        }
        1u64 << shift
    };
    if !record_size.is_multiple_of(USA_BLOCK as u64) || record_size > 65536 {
        return Err(tf!("taille d'enregistrement MFT non prise en charge ({record_size})", "unsupported MFT record size ({record_size})"));
    }
    let g = Geometry { sector, spc, cluster, total_sectors: le64(b, 0x28), mft_lcn: le64(b, 0x30), mftmirr_lcn: le64(b, 0x38), record_size };
    if g.clusters() == 0 || g.mft_lcn >= g.clusters() || g.mftmirr_lcn >= g.clusters() {
        return Err(t!("secteur d'amorçage incohérent (taille ou position de la MFT)", "inconsistent boot sector (MFT size or position)").into());
    }
    Ok(g)
}

// --- Tableaux de correction -------------------------------------------------

fn unfixup(rec: &mut [u8]) -> Result<(), String> {
    let ofs = le16(rec, 4) as usize;
    let cnt = le16(rec, 6) as usize;
    if cnt < 2 || (cnt - 1) * USA_BLOCK != rec.len() || ofs + cnt * 2 > rec.len() {
        return Err(t!("tableau de correction de l'enregistrement invalide", "invalid record fixup array").into());
    }
    let usn = [rec[ofs], rec[ofs + 1]];
    for i in 1..cnt {
        let end = i * USA_BLOCK;
        if rec[end - 2..end] != usn {
            return Err(t!(
                "enregistrement MFT incohérent (écriture partielle ou secteur endommagé)",
                "inconsistent MFT record (partial write or damaged sector)"
            )
            .into());
        }
        let saved = [rec[ofs + 2 * i], rec[ofs + 2 * i + 1]];
        rec[end - 2..end].copy_from_slice(&saved);
    }
    Ok(())
}

fn fixup(rec: &mut [u8]) {
    let ofs = le16(rec, 4) as usize;
    let cnt = le16(rec, 6) as usize;
    let mut usn = le16(rec, ofs).wrapping_add(1);
    if usn == 0 || usn == 0xFFFF {
        usn = 1;
    }
    put16(rec, ofs, usn);
    for i in 1..cnt {
        let end = i * USA_BLOCK;
        let saved = [rec[end - 2], rec[end - 1]];
        rec[ofs + 2 * i..ofs + 2 * i + 2].copy_from_slice(&saved);
        put16(rec, end - 2, usn);
    }
}

// --- Listes d'extensions (runlists) -----------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    pub len: u64,
    /// `None` : extension creuse.
    pub lcn: Option<u64>,
}

pub fn decode_runs(b: &[u8]) -> Result<Vec<Run>, String> {
    let bad = || t!("liste d'extensions NTFS invalide", "invalid NTFS run list").to_string();
    let mut i = 0usize;
    let mut lcn: i64 = 0;
    let mut out = Vec::new();
    loop {
        let h = *b.get(i).ok_or_else(bad)?;
        if h == 0 {
            break;
        }
        let ls = (h & 0x0F) as usize;
        let os = (h >> 4) as usize;
        if ls == 0 || ls > 8 || os > 8 || i + 1 + ls + os > b.len() {
            return Err(bad());
        }
        let mut len = 0u64;
        for k in 0..ls {
            len |= (b[i + 1 + k] as u64) << (8 * k);
        }
        if len == 0 {
            return Err(bad());
        }
        let run_lcn = if os == 0 {
            None
        } else {
            let mut d: i64 = 0;
            for k in 0..os {
                d |= (b[i + 1 + ls + k] as i64) << (8 * k);
            }
            if os < 8 && b[i + ls + os] & 0x80 != 0 {
                d |= -1i64 << (8 * os);
            }
            lcn = lcn.checked_add(d).ok_or_else(bad)?;
            if lcn < 0 {
                return Err(bad());
            }
            Some(lcn as u64)
        };
        out.push(Run { len, lcn: run_lcn });
        i += 1 + ls + os;
    }
    Ok(out)
}

fn unsigned_bytes(v: u64) -> Vec<u8> {
    let mut n = 1;
    while n < 8 && v >> (8 * n) != 0 {
        n += 1;
    }
    v.to_le_bytes()[..n].to_vec()
}

fn signed_bytes(v: i64) -> Vec<u8> {
    for n in 1..8 {
        let lim = 1i64 << (8 * n - 1);
        if (-lim..lim).contains(&v) {
            return v.to_le_bytes()[..n].to_vec();
        }
    }
    v.to_le_bytes().to_vec()
}

pub fn encode_runs(runs: &[Run]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut prev: i64 = 0;
    for r in runs {
        let l = unsigned_bytes(r.len);
        match r.lcn {
            None => {
                out.push(l.len() as u8);
                out.extend(l);
            }
            Some(lcn) => {
                let o = signed_bytes(lcn as i64 - prev);
                out.push(((o.len() as u8) << 4) | l.len() as u8);
                out.extend(l);
                out.extend(o);
                prev = lcn as i64;
            }
        }
    }
    out.push(0);
    out
}

fn push_run(runs: &mut Vec<Run>, r: Run) {
    if let Some(last) = runs.last_mut() {
        let contiguous = match (last.lcn, r.lcn) {
            (None, None) => true,
            (Some(a), Some(b)) => a + last.len == b,
            _ => false,
        };
        if contiguous {
            last.len += r.len;
            return;
        }
    }
    runs.push(r);
}

/// Convertit la plage d'octets `[off, off+len)` d'un flux en plages du volume.
fn map_range(runs: &[Run], cluster: u64, off: u64, len: u64) -> Result<Vec<(u64, u64)>, String> {
    let mut out: Vec<(u64, u64)> = Vec::new();
    let (mut pos, end) = (off, off + len);
    let mut vcn_start = 0u64;
    for r in runs {
        let (rs, re) = (vcn_start * cluster, (vcn_start + r.len) * cluster);
        vcn_start += r.len;
        if pos >= end {
            break;
        }
        if pos >= re {
            continue;
        }
        let lcn = r.lcn.ok_or(t!("plage creuse inattendue dans un fichier système", "unexpected sparse run in a system file"))?;
        let n = end.min(re) - pos;
        let vol = lcn * cluster + (pos - rs);
        match out.last_mut() {
            Some(l) if l.0 + l.1 == vol => l.1 += n,
            _ => out.push((vol, n)),
        }
        pos += n;
    }
    if pos < end {
        return Err(t!("plage située au-delà de la fin d'un fichier système", "run located past the end of a system file").into());
    }
    Ok(out)
}

// --- Attributs ----------------------------------------------------------------

#[derive(Debug, Clone)]
struct Attr {
    off: usize,
    len: usize,
    kind: u32,
    nonres: bool,
    flags: u16,
    name: String,
}

fn attrs(rec: &[u8]) -> Result<Vec<Attr>, String> {
    let bad = |w: &str| tf!("enregistrement MFT invalide ({w})", "invalid MFT record ({w})");
    let in_use = le32(rec, 0x18) as usize;
    if in_use > rec.len() {
        return Err(bad(t!("taille utilisée", "used size")));
    }
    let mut o = le16(rec, 0x14) as usize;
    let mut out = Vec::new();
    loop {
        if o + 8 > in_use {
            return Err(bad(t!("fin des attributs absente", "end of attributes missing")));
        }
        let kind = le32(rec, o);
        if kind == ATTR_END {
            break;
        }
        let len = le32(rec, o + 4) as usize;
        if len < 0x18 || !len.is_multiple_of(8) || o + len > in_use {
            return Err(bad(t!("longueur d'attribut", "attribute length")));
        }
        let nlen = rec[o + 9] as usize;
        let noff = le16(rec, o + 0x0A) as usize;
        if noff + 2 * nlen > len {
            return Err(bad(t!("nom d'attribut", "attribute name")));
        }
        let units: Vec<u16> = (0..nlen).map(|k| le16(rec, o + noff + 2 * k)).collect();
        out.push(Attr { off: o, len, kind, nonres: rec[o + 8] != 0, flags: le16(rec, o + 0x0C), name: String::from_utf16_lossy(&units) });
        o += len;
    }
    Ok(out)
}

#[derive(Debug, Clone)]
struct NonRes {
    start_vcn: u64,
    last_vcn: u64,
    alloc: u64,
    data: u64,
    init: u64,
    runs: Vec<Run>,
}

fn nonres(rec: &[u8], a: &Attr) -> Result<NonRes, String> {
    if !a.nonres || a.len < 0x40 {
        return Err(t!("attribut non résident attendu", "non-resident attribute expected").into());
    }
    let b = &rec[a.off..a.off + a.len];
    let ro = le16(b, 0x20) as usize;
    if ro < 0x40 || ro >= a.len {
        return Err(t!("position de la liste d'extensions invalide", "invalid run list offset").into());
    }
    Ok(NonRes {
        start_vcn: le64(b, 0x10),
        last_vcn: le64(b, 0x18),
        alloc: le64(b, 0x28),
        data: le64(b, 0x30),
        init: le64(b, 0x38),
        runs: decode_runs(&b[ro..])?,
    })
}

/// Remplace un attribut non résident par une version aux tailles et extensions mises à
/// jour, en décalant les attributs suivants.
fn rewrite_nonres(rec: &mut Vec<u8>, a: &Attr, nr: &NonRes) -> Result<(), String> {
    let old = &rec[a.off..a.off + a.len];
    let ro = le16(old, 0x20) as usize;
    let mut new = old[..ro].to_vec();
    put64(&mut new, 0x18, nr.last_vcn);
    put64(&mut new, 0x28, nr.alloc);
    put64(&mut new, 0x30, nr.data);
    put64(&mut new, 0x38, nr.init);
    new.extend(encode_runs(&nr.runs));
    while !new.len().is_multiple_of(8) {
        new.push(0);
    }
    let n = new.len() as u32;
    put32(&mut new, 4, n);
    let in_use = le32(rec, 0x18) as usize;
    let allocated = (le32(rec, 0x1C) as usize).min(rec.len());
    let new_in_use = in_use - a.len + new.len();
    if new_in_use > allocated {
        return Err(t!(
            "place insuffisante dans l'enregistrement MFT pour la nouvelle liste d'extensions",
            "not enough room in the MFT record for the new run list"
        )
        .into());
    }
    let mut out = rec[..a.off].to_vec();
    out.extend_from_slice(&new);
    out.extend_from_slice(&rec[a.off + a.len..in_use]);
    out.resize(rec.len(), 0);
    put32(&mut out, 0x18, new_in_use as u32);
    *rec = out;
    Ok(())
}

fn total_len(runs: &[Run]) -> u64 {
    runs.iter().map(|r| r.len).sum()
}

// --- Accès au volume ------------------------------------------------------------

struct Record {
    number: u64,
    data: Vec<u8>,
}

struct Volume<'a> {
    dev: &'a mut dyn BlockDevice,
    base: u64,
    /// Octets accessibles à partir de `base` (taille de la partition).
    len: u64,
    geo: Geometry,
    boot: Vec<u8>,
    mft_runs: Vec<Run>,
    mirror_runs: Vec<Run>,
    mirror_records: u64,
}

impl<'a> Volume<'a> {
    fn open(dev: &'a mut dyn BlockDevice, base: u64, len: u64) -> Result<Self, String> {
        let ss = dev.sector_size() as u64;
        if !base.is_multiple_of(ss) || base.checked_add(len).is_none_or(|e| e > dev.size()) {
            return Err(t!("partition hors des limites du disque", "partition outside the disk bounds").into());
        }
        let mut v = Volume {
            dev,
            base,
            len,
            geo: Geometry { sector: ss, spc: 1, cluster: ss, total_sectors: 1, mft_lcn: 0, mftmirr_lcn: 0, record_size: 1024 },
            boot: Vec::new(),
            mft_runs: Vec::new(),
            mirror_runs: Vec::new(),
            mirror_records: 0,
        };
        v.boot = v.read_bytes(0, ss.max(512) as usize)?;
        v.geo = parse_boot(&v.boot)?;
        if v.geo.sector != ss {
            return Err(tf!(
                "le volume utilise des secteurs de {} octets, le disque de {ss}",
                "the volume uses {}-byte sectors, the disk {ss}",
                v.geo.sector
            ));
        }
        if (v.geo.total_sectors + 1) * ss > len {
            return Err(t!(
                "le système de fichiers déclare une taille supérieure à sa partition",
                "the file system claims a size larger than its partition"
            )
            .into());
        }
        // Enregistrement 0 ($MFT), lu à sa position fixe, pour connaître les extensions de la MFT.
        let rs = v.geo.record_size;
        let mut r0 = v.read_bytes(v.geo.mft_lcn * v.geo.cluster, rs as usize)?;
        check_record(&mut r0, 0)?;
        v.mft_runs = data_runs(&r0, "")?.runs;
        let r1 = v.read_record(1)?;
        let mirror = data_runs(&r1.data, "")?;
        v.mirror_records = mirror.data / rs;
        v.mirror_runs = mirror.runs;
        Ok(v)
    }

    fn read_bytes(&mut self, off: u64, len: usize) -> Result<Vec<u8>, String> {
        let ss = self.dev.sector_size() as u64;
        if off + len as u64 > self.len {
            return Err(t!("lecture hors de la partition", "read outside the partition").into());
        }
        let start = off - off % ss;
        let end = align_up(off + len as u64, ss);
        let mut buf = AlignedBuf::new((end - start) as usize);
        self.dev
            .read_at(self.base + start, &mut buf)
            .map_err(|e| tf!("lecture à l'octet {} du volume : {e}", "read at byte {} of the volume: {e}", start))?;
        let s = (off - start) as usize;
        Ok(buf[s..s + len].to_vec())
    }

    fn write_bytes(&mut self, off: u64, data: &[u8]) -> Result<(), String> {
        let ss = self.dev.sector_size() as u64;
        if off + data.len() as u64 > self.len {
            return Err(t!("écriture hors de la partition", "write outside the partition").into());
        }
        let start = off - off % ss;
        let end = align_up(off + data.len() as u64, ss);
        let mut buf = AlignedBuf::new((end - start) as usize);
        if start != off || end != off + data.len() as u64 {
            self.dev
                .read_at(self.base + start, &mut buf)
                .map_err(|e| tf!("lecture à l'octet {start} du volume : {e}", "read at byte {start} of the volume: {e}"))?;
        }
        let s = (off - start) as usize;
        buf[s..s + data.len()].copy_from_slice(data);
        self.dev
            .write_at(self.base + start, &buf)
            .map_err(|e| tf!("écriture à l'octet {start} du volume : {e}", "write at byte {start} of the volume: {e}"))
    }

    fn read_stream(&mut self, runs: &[Run], off: u64, len: u64) -> Result<Vec<u8>, String> {
        let mut out = Vec::with_capacity(len as usize);
        for (vo, n) in map_range(runs, self.geo.cluster, off, len)? {
            out.extend(self.read_bytes(vo, n as usize)?);
        }
        Ok(out)
    }

    fn write_stream(&mut self, runs: &[Run], off: u64, data: &[u8]) -> Result<(), String> {
        let mut p = 0usize;
        for (vo, n) in map_range(runs, self.geo.cluster, off, data.len() as u64)? {
            self.write_bytes(vo, &data[p..p + n as usize])?;
            p += n as usize;
        }
        Ok(())
    }

    fn read_record(&mut self, n: u64) -> Result<Record, String> {
        let rs = self.geo.record_size;
        let runs = self.mft_runs.clone();
        let mut data = self.read_stream(&runs, n * rs, rs)?;
        check_record(&mut data, n)?;
        Ok(Record { number: n, data })
    }

    fn write_record(&mut self, rec: &Record) -> Result<(), String> {
        let rs = self.geo.record_size;
        let mut d = rec.data.clone();
        fixup(&mut d);
        let runs = self.mft_runs.clone();
        self.write_stream(&runs, rec.number * rs, &d)?;
        if rec.number < self.mirror_records {
            let runs = self.mirror_runs.clone();
            self.write_stream(&runs, rec.number * rs, &d)?;
        }
        Ok(())
    }
}

/// Contrôle et décode (corrections appliquées) un enregistrement MFT de base.
fn check_record(rec: &mut [u8], n: u64) -> Result<(), String> {
    if &rec[0..4] != b"FILE" {
        return Err(tf!("enregistrement MFT {n} invalide (signature)", "MFT record {n} invalid (signature)"));
    }
    unfixup(rec).map_err(|e| tf!("enregistrement MFT {n} : {e}", "MFT record {n}: {e}"))?;
    if le16(rec, 0x16) & 1 == 0 {
        return Err(tf!("enregistrement MFT {n} marqué inutilisé", "MFT record {n} marked unused"));
    }
    if le64(rec, 0x20) != 0 {
        return Err(tf!("enregistrement MFT {n} : enregistrement d'extension inattendu", "MFT record {n}: unexpected extension record"));
    }
    if le32(rec, 0x1C) as usize != rec.len() {
        return Err(tf!("enregistrement MFT {n} : taille allouée inattendue", "MFT record {n}: unexpected allocated size"));
    }
    Ok(())
}

fn find_data<'r>(list: &'r [Attr], name: &str) -> Result<&'r Attr, String> {
    if list.iter().any(|a| a.kind == ATTR_ATTRIBUTE_LIST) {
        return Err(t!(
            "liste d'attributs présente (fichier système fragmenté), non prise en charge",
            "attribute list present (fragmented system file), not supported"
        )
        .into());
    }
    let mut it = list.iter().filter(|a| a.kind == ATTR_DATA && a.name == name);
    let a = it.next().ok_or_else(|| tf!("attribut $DATA « {name} » absent", "$DATA attribute \"{name}\" missing"))?;
    if it.next().is_some() {
        return Err(tf!("attribut $DATA « {name} » en double", "duplicate $DATA attribute \"{name}\""));
    }
    Ok(a)
}

fn data_runs(rec: &[u8], name: &str) -> Result<NonRes, String> {
    let list = attrs(rec)?;
    let a = find_data(&list, name)?;
    let nr = nonres(rec, a)?;
    if nr.start_vcn != 0 || total_len(&nr.runs) != nr.last_vcn + 1 {
        return Err(t!("liste d'extensions incohérente avec la taille de l'attribut", "run list inconsistent with the attribute size").into());
    }
    Ok(nr)
}

// --- Agrandissement --------------------------------------------------------------

/// Bilan d'un agrandissement.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GrowSummary {
    pub old_size: u64,
    pub new_size: u64,
    pub cluster_size: u64,
    pub old_clusters: u64,
    pub new_clusters: u64,
    /// Clusters ajoutés à `$Bitmap` lui-même (comptés comme utilisés).
    pub bitmap_extra_clusters: u64,
}

struct Prepared {
    summary: GrowSummary,
    rec3: Record,
    rec6: Record,
    rec8: Record,
    bitmap_runs: Vec<Run>,
    /// Octet de `$Bitmap` à partir duquel le contenu est réécrit, et sa valeur d'origine.
    first_byte: u64,
    first_byte_old: u8,
    bitmap_data: u64,
    bitmap_alloc: u64,
    ext: (u64, u64),
    boot: Vec<u8>,
    new_total_sectors: u64,
}

fn prepare(v: &mut Volume, new_len: u64) -> Result<Prepared, String> {
    let g = v.geo;
    let ss = g.sector;
    let new_total = new_len / ss - 1;
    let (oc, nc) = (g.clusters(), new_total / g.spc);
    if nc <= oc {
        return Err(t!("la nouvelle taille n'ajoute aucun cluster", "the new size adds no cluster").into());
    }

    // $Volume : indicateur « à vérifier ».
    let mut rec3 = v.read_record(3)?;
    let list = attrs(&rec3.data)?;
    let vi = list
        .iter()
        .find(|a| a.kind == ATTR_VOLUME_INFORMATION && !a.nonres)
        .ok_or(t!("attribut $VOLUME_INFORMATION absent de $Volume", "$VOLUME_INFORMATION attribute missing from $Volume"))?;
    let voff = vi.off + le16(&rec3.data, vi.off + 0x14) as usize;
    if le32(&rec3.data, vi.off + 0x10) < 12 || voff + 12 > vi.off + vi.len {
        return Err(t!("attribut $VOLUME_INFORMATION invalide", "invalid $VOLUME_INFORMATION attribute").into());
    }
    let major = rec3.data[voff + 8];
    if major != 3 {
        return Err(tf!("version NTFS {major}.x non prise en charge", "NTFS version {major}.x not supported"));
    }
    let flags = le16(&rec3.data, voff + 10);
    put16(&mut rec3.data, voff + 10, flags | VOLUME_IS_DIRTY);

    // $Bitmap.
    let mut rec6 = v.read_record(6)?;
    let list = attrs(&rec6.data)?;
    let a6 = find_data(&list, "")?.clone();
    if a6.flags & 0x80FF != 0 {
        return Err(t!("$Bitmap compressé ou creux, non pris en charge", "compressed or sparse $Bitmap, not supported").into());
    }
    let mut bm = nonres(&rec6.data, &a6)?;
    if bm.start_vcn != 0
        || bm.runs.iter().any(|r| r.lcn.is_none())
        || total_len(&bm.runs) != bm.last_vcn + 1
        || bm.alloc != (bm.last_vcn + 1) * g.cluster
        || bm.init != bm.data
        || bm.data < oc.div_ceil(8)
        || bm.data > bm.alloc
    {
        return Err(t!("attribut $DATA de $Bitmap incohérent", "inconsistent $Bitmap $DATA attribute").into());
    }
    let old_runs = bm.runs.clone();
    let data_new = align_up(nc.div_ceil(8), 8).max(bm.data);
    let alloc_new = align_up(data_new, g.cluster).max(bm.alloc);
    let extra = (alloc_new - bm.alloc) / g.cluster;
    // Les clusters supplémentaires de $Bitmap sont pris au début de l'espace ajouté,
    // libre par construction.
    let ext = (oc, extra);
    if oc + extra > nc {
        return Err(t!("agrandissement trop faible pour loger la carte d'allocation", "growth too small to hold the allocation map").into());
    }
    if extra > 0 {
        push_run(&mut bm.runs, Run { len: extra, lcn: Some(oc) });
    }
    bm.last_vcn = alloc_new / g.cluster - 1;
    bm.alloc = alloc_new;
    bm.data = data_new;
    bm.init = data_new;
    rewrite_nonres(&mut rec6.data, &a6, &bm)?;
    let first_byte = oc / 8;
    // Octet partagé entre anciens et nouveaux clusters (seulement si `oc` n'est pas un
    // multiple de 8 ; sinon il peut se trouver juste après la fin de l'ancien $Bitmap).
    let first_byte_old = if oc % 8 == 0 { 0 } else { v.read_stream(&old_runs, first_byte, 1)?[0] };

    // $BadClus:$Bad : flux creux couvrant tout le volume.
    let mut rec8 = v.read_record(8)?;
    let list = attrs(&rec8.data)?;
    let a8 = find_data(&list, "$Bad")?.clone();
    let mut bad = nonres(&rec8.data, &a8)?;
    if bad.start_vcn != 0 || total_len(&bad.runs) != bad.last_vcn + 1 || bad.last_vcn + 1 != oc || bad.alloc != oc * g.cluster {
        return Err(
            t!("attribut $BadClus:$Bad incohérent avec la taille du volume", "$BadClus:$Bad attribute inconsistent with the volume size").into()
        );
    }
    push_run(&mut bad.runs, Run { len: nc - oc, lcn: None });
    bad.last_vcn = nc - 1;
    bad.alloc = nc * g.cluster;
    bad.data = nc * g.cluster;
    rewrite_nonres(&mut rec8.data, &a8, &bad)?;

    let mut boot = v.boot.clone();
    put64(&mut boot, 0x28, new_total);

    Ok(Prepared {
        summary: GrowSummary {
            old_size: g.total_sectors * ss,
            new_size: new_total * ss,
            cluster_size: g.cluster,
            old_clusters: oc,
            new_clusters: nc,
            bitmap_extra_clusters: extra,
        },
        rec3,
        rec6,
        rec8,
        bitmap_runs: bm.runs,
        first_byte,
        first_byte_old,
        bitmap_data: data_new,
        bitmap_alloc: alloc_new,
        ext,
        boot,
        new_total_sectors: new_total,
    })
}

/// Contenu de `$Bitmap` pour les octets `[from, from+len)` (≥ `first_byte`).
fn bitmap_chunk(p: &Prepared, from: u64, len: u64) -> Vec<u8> {
    let oc = p.summary.old_clusters;
    let nc = p.summary.new_clusters;
    let (ext_start, ext_len) = p.ext;
    let mut out = vec![0u8; len as usize];
    for (i, byte) in out.iter_mut().enumerate() {
        let b = from + i as u64;
        if b >= p.bitmap_data {
            continue;
        }
        // Cas courants traités par octet entier (volumes de plusieurs To).
        let (lo, hi) = (b * 8, b * 8 + 8);
        if lo >= oc && hi <= nc && (hi <= ext_start || lo >= ext_start + ext_len) {
            continue; // nouveaux clusters libres
        }
        if lo >= nc {
            *byte = 0xFF; // remplissage au-delà du dernier cluster
            continue;
        }
        let mut v = 0u8;
        for bit in 0..8 {
            let c = b * 8 + bit;
            let set = if c < oc {
                p.first_byte_old & (1 << bit) != 0
            } else if c >= ext_start && c < ext_start + ext_len {
                true
            } else {
                c >= nc
            };
            if set {
                v |= 1 << bit;
            }
        }
        *byte = v;
    }
    out
}

fn apply(v: &mut Volume, p: &Prepared) -> Result<(), String> {
    let flush = |v: &mut Volume| v.dev.flush().map_err(|e| tf!("vidage des écritures : {e}", "flushing writes: {e}"));
    // 1. Volume marqué « à vérifier » : toute interruption ultérieure sera rattrapée
    //    par chkdsk au prochain montage.
    v.write_record(&p.rec3)?;
    flush(v)?;
    // 2. Carte d'allocation (octets touchés uniquement).
    let runs = p.bitmap_runs.clone();
    let mut pos = p.first_byte;
    while pos < p.bitmap_alloc {
        let n = CHUNK.min(p.bitmap_alloc - pos);
        let data = bitmap_chunk(p, pos, n);
        v.write_stream(&runs, pos, &data)?;
        pos += n;
    }
    // 3. Enregistrements $BadClus puis $Bitmap.
    v.write_record(&p.rec8)?;
    v.write_record(&p.rec6)?;
    flush(v)?;
    // 4. Secteur d'amorçage : secours (dernier secteur de la partition), puis principal.
    let ss = v.geo.sector;
    let boot = p.boot.clone();
    v.write_bytes(p.new_total_sectors * ss, &boot)?;
    flush(v)?;
    v.write_bytes(0, &boot)?;
    flush(v)
}

/// Vérifie, sans rien écrire, qu'un volume NTFS peut être agrandi par ce module.
pub fn preflight(dev: &mut dyn BlockDevice, base: u64, len: u64) -> Result<(), String> {
    let mut v = Volume::open(dev, base, len)?;
    // Taille hypothétique : quatre fois plus grand (au moins +64 Gio), pour éprouver
    // tous les calculs ; seules des lectures ont lieu.
    let fake = len.saturating_mul(4).max(len.saturating_add(1 << 36));
    prepare(&mut v, fake).map(|_| ())
}

/// Agrandit le volume NTFS situé à `base` pour occuper `new_len` octets (la partition
/// doit déjà avoir cette taille). Toutes les vérifications sont faites avant la
/// première écriture.
pub fn grow(dev: &mut dyn BlockDevice, base: u64, new_len: u64) -> Result<GrowSummary, GrowError> {
    let before = |message: String| GrowError { message, modified: false };
    let after = |message: String| GrowError { message, modified: true };
    let mut v = Volume::open(dev, base, new_len).map_err(before)?;
    let p = prepare(&mut v, new_len).map_err(before)?;
    apply(&mut v, &p).map_err(after)?;
    drop(v);
    verify(dev, base, new_len, &p.summary).map_err(after)?;
    Ok(p.summary)
}

/// Échec d'un agrandissement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrowError {
    pub message: String,
    /// Des écritures avaient déjà eu lieu (le volume est alors marqué « à vérifier »).
    pub modified: bool,
}

/// Relit le volume agrandi et contrôle les structures modifiées.
pub fn verify(dev: &mut dyn BlockDevice, base: u64, new_len: u64, s: &GrowSummary) -> Result<(), String> {
    let mut v = Volume::open(dev, base, new_len).map_err(|e| tf!("relecture après agrandissement : {e}", "read-back after growing: {e}"))?;
    let g = v.geo;
    if g.total_sectors * g.sector != s.new_size || g.clusters() != s.new_clusters {
        return Err(t!("relecture après agrandissement : taille du volume inattendue", "read-back after growing: unexpected volume size").into());
    }
    let r6 = v.read_record(6)?;
    let bm = data_runs(&r6.data, "")?;
    if bm.data < s.new_clusters.div_ceil(8) || bm.init != bm.data {
        return Err(t!("relecture après agrandissement : taille de $Bitmap inattendue", "read-back after growing: unexpected $Bitmap size").into());
    }
    let r8 = v.read_record(8)?;
    let bad = data_runs(&r8.data, "$Bad")?;
    if bad.last_vcn + 1 != s.new_clusters || bad.alloc != s.new_clusters * g.cluster {
        return Err(t!("relecture après agrandissement : taille de $BadClus inattendue", "read-back after growing: unexpected $BadClus size").into());
    }
    // Les bits des nouveaux clusters : seuls ceux de $Bitmap lui-même sont utilisés.
    let from = s.old_clusters / 8;
    let to = bm.data;
    let bytes = v.read_stream(&bm.runs, from, to - from)?;
    let mut used_new = 0u64;
    for (i, b) in bytes.iter().enumerate() {
        for bit in 0..8 {
            let c = (from + i as u64) * 8 + bit;
            if c >= s.old_clusters && c < s.new_clusters && b & (1 << bit) != 0 {
                used_new += 1;
            }
        }
    }
    if used_new != s.bitmap_extra_clusters {
        return Err(tf!(
            "relecture après agrandissement : {used_new} nouveau(x) cluster(s) marqué(s) utilisé(s), {} attendu(s)",
            "read-back after growing: {used_new} new cluster(s) marked used, {} expected",
            s.bitmap_extra_clusters
        ));
    }
    let backup = v.read_bytes(g.total_sectors * g.sector, 512)?;
    if backup[..512] != v.boot[..512] {
        return Err(t!(
            "relecture après agrandissement : secteur d'amorçage de secours différent du principal",
            "read-back after growing: backup boot sector differs from the primary"
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runlist_roundtrip() {
        let runs = vec![
            Run { len: 1, lcn: Some(4) },
            Run { len: 300, lcn: Some(70000) },
            Run { len: 5, lcn: None },
            Run { len: 2, lcn: Some(12) }, // déplacement négatif
            Run { len: 1 << 40, lcn: Some(1 << 35) },
        ];
        let enc = encode_runs(&runs);
        assert_eq!(decode_runs(&enc).unwrap(), runs);
        // Exemple de référence : 0x11 longueur 0x30, décalage 0x60.
        assert_eq!(decode_runs(&[0x11, 0x30, 0x60, 0x00]).unwrap(), vec![Run { len: 0x30, lcn: Some(0x60) }]);
        // Décalage négatif sur un octet (0xF0 = -16).
        let r = decode_runs(&[0x11, 0x01, 0x40, 0x11, 0x01, 0xF0, 0x00]).unwrap();
        assert_eq!(r[1].lcn, Some(0x30));
        assert!(decode_runs(&[0x11, 0x01]).is_err());
    }

    #[test]
    fn push_run_merges() {
        let mut r = vec![Run { len: 2, lcn: Some(10) }];
        push_run(&mut r, Run { len: 3, lcn: Some(12) });
        push_run(&mut r, Run { len: 1, lcn: Some(40) });
        assert_eq!(r, vec![Run { len: 5, lcn: Some(10) }, Run { len: 1, lcn: Some(40) }]);
        let mut h = vec![Run { len: 7, lcn: None }];
        push_run(&mut h, Run { len: 3, lcn: None });
        assert_eq!(h, vec![Run { len: 10, lcn: None }]);
    }

    #[test]
    fn fixup_roundtrip() {
        let mut rec = vec![0u8; 1024];
        rec[0..4].copy_from_slice(b"FILE");
        put16(&mut rec, 4, 0x30);
        put16(&mut rec, 6, 3);
        put16(&mut rec, 0x30, 7);
        rec[510] = 0xAB;
        rec[1023] = 0xCD;
        let orig = rec.clone();
        fixup(&mut rec);
        assert_eq!(le16(&rec, 510), 8);
        assert_eq!(le16(&rec, 1022), 8);
        unfixup(&mut rec).unwrap();
        assert_eq!(rec[510], 0xAB);
        assert_eq!(rec[1023], 0xCD);
        assert_eq!(le16(&rec, 0x30), 8);
        assert_eq!(rec[..0x30], orig[..0x30]);
        rec[511] ^= 1;
        fixup(&mut rec);
        rec[1022] ^= 0xFF; // écriture partielle simulée
        assert!(unfixup(&mut rec).is_err());
    }

    #[test]
    fn bitmap_chunk_matches_bit_by_bit_reference() {
        for (oc, nc, extra, old) in [(24575u64, 58879u64, 1u64, 0x80u8), (32768, 70000, 2, 0), (100, 101, 0, 0x0F), (13, 4000, 3, 0x1F)] {
            let data = align_up(nc.div_ceil(8), 8);
            let p = Prepared {
                summary: GrowSummary {
                    old_size: 0,
                    new_size: 0,
                    cluster_size: 4096,
                    old_clusters: oc,
                    new_clusters: nc,
                    bitmap_extra_clusters: extra,
                },
                rec3: Record { number: 3, data: vec![] },
                rec6: Record { number: 6, data: vec![] },
                rec8: Record { number: 8, data: vec![] },
                bitmap_runs: vec![],
                first_byte: oc / 8,
                first_byte_old: old,
                bitmap_data: data,
                bitmap_alloc: align_up(data, 4096),
                ext: (oc, extra),
                boot: vec![],
                new_total_sectors: 0,
            };
            let from = oc / 8;
            let got = bitmap_chunk(&p, from, p.bitmap_alloc - from);
            for (i, byte) in got.iter().enumerate() {
                let b = from + i as u64;
                for bit in 0..8 {
                    let c = b * 8 + bit;
                    let want = if b >= data {
                        false
                    } else if c < oc {
                        old & (1 << bit) != 0
                    } else if c < oc + extra {
                        true
                    } else {
                        c >= nc
                    };
                    assert_eq!(byte & (1 << bit) != 0, want, "oc={oc} nc={nc} cluster {c}");
                }
            }
        }
    }

    #[test]
    fn map_range_spans_runs() {
        let runs = vec![Run { len: 2, lcn: Some(10) }, Run { len: 2, lcn: Some(20) }];
        let m = map_range(&runs, 4096, 4096, 8192).unwrap();
        assert_eq!(m, vec![(11 * 4096, 4096), (20 * 4096, 4096)]);
        assert!(map_range(&runs, 4096, 0, 5 * 4096).is_err());
    }
}
