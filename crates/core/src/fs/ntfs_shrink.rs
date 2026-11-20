//! Offline shrinking of an NTFS file system, performed while cloning onto a
//! smaller destination.
//!
//! The source is only read. Everything is computed from it before the first write:
//!
//! - every in-use MFT record is read; each non-resident attribute run lying beyond
//!   the new end of the volume is given new clusters taken from the free space below
//!   it (best fit, so files stay as contiguous as possible) and its run list is
//!   rewritten in the record (a record that would overflow refuses the shrink);
//! - the copy list: used clusters below the new end are copied in place, relocated
//!   runs are copied from their old position to their new one;
//! - `$Bitmap` is truncated to the new size and rebuilt (source map below the new end
//!   plus the relocated clusters), `$BadClus:$Bad` is cut to the new size (a volume
//!   with bad clusters is refused), `$LogFile` is emptied (filled with 0xFF, as
//!   ntfs-3g does: Windows recreates it), `$Volume` is flagged "needs checking" so
//!   that Windows runs chkdsk once on the first boot;
//! - boot sector: new size and MFT / MFT mirror positions, with the backup copy in
//!   the last sector of the new partition.
//!
//! The clone engine copies the data, verifies it, then calls [`apply`], which writes
//! the updated records and metadata on the destination, and [`verify`], which reads
//! them back.

use super::Extent;
use super::ntfs_grow::{
    ATTR_ATTRIBUTE_LIST, ATTR_DATA, ATTR_VOLUME_INFORMATION, CHUNK, Geometry, Record, Run, VOLUME_IS_DIRTY, Volume, align_up, attrs, data_runs,
    find_data, le16, le32, le64, nonres, parse_boot, push_run, put16, put64, rewrite_nonres, total_len, unfixup,
};
use crate::device::BlockDevice;
use std::collections::BTreeMap;

const MIB: u64 = 1024 * 1024;
const RECORD_IN_USE: u16 = 0x0001;

/// Free space kept on the shrunk volume, at least: Windows needs room to boot and
/// to update itself.
pub fn reserve(new_size: u64) -> u64 {
    (new_size / 50).max((256 * MIB).min(new_size / 4))
}

/// Outcome of a shrink.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ShrinkSummary {
    pub old_size: u64,
    pub new_size: u64,
    pub cluster_size: u64,
    pub old_clusters: u64,
    pub new_clusters: u64,
    /// Clusters moved from beyond the new end to the free space below it.
    pub relocated_clusters: u64,
    /// MFT records rewritten.
    pub records_updated: u64,
    /// Clusters marked used in the new allocation map.
    pub used_clusters: u64,
}

/// Plan-time estimate, from the source allocation map only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShrinkEstimate {
    pub new_clusters: u64,
    /// Bytes that will be moved below the new end.
    pub relocated_bytes: u64,
    /// Bytes used on the source volume.
    pub used_bytes: u64,
}

/// Clusters of a volume held by a partition of `len` bytes (the last sector holds
/// the backup boot sector).
fn clusters_for(len: u64, sector: u64, cluster: u64) -> u64 {
    (len / sector).saturating_sub(1) * sector / cluster
}

/// Checks that a volume whose used clusters are `extents` (bytes, relative to the
/// start of the volume) fits in a partition of `new_len` bytes.
pub fn estimate(extents: &[Extent], cluster: u64, fs_size: u64, sector: u64, new_len: u64) -> Result<ShrinkEstimate, String> {
    if cluster == 0 || sector == 0 || !cluster.is_multiple_of(sector) {
        return Err(t!("géométrie NTFS invalide", "invalid NTFS geometry").into());
    }
    let old_end = fs_size / cluster * cluster;
    let n = clusters_for(new_len, sector, cluster);
    let new_end = n * cluster;
    if new_end >= old_end {
        return Err(t!("la nouvelle taille ne réduit pas le volume", "the new size does not shrink the volume").into());
    }
    let (mut used, mut beyond) = (0u64, 0u64);
    for e in extents {
        let (s, t) = (e.offset.min(old_end), e.end().min(old_end));
        used += t - s;
        beyond += t.saturating_sub(s.max(new_end));
    }
    let need = used + reserve(new_end);
    if need > new_end {
        return Err(tf!(
            "le volume contient {} de données ; la place disponible ({}) doit en garder au moins {} de libre",
            "the volume holds {} of data; the available room ({}) must keep at least {} free",
            crate::diag::human_size(used),
            crate::diag::human_size(new_end),
            crate::diag::human_size(reserve(new_end))
        ));
    }
    Ok(ShrinkEstimate { new_clusters: n, relocated_bytes: beyond, used_bytes: used })
}

/// Everything computed from the source; applied to the destination after the copy.
#[derive(Debug, Clone)]
pub struct ShrinkPrep {
    pub summary: ShrinkSummary,
    /// Volume-relative copies `(source offset, destination offset, length)`, sorted
    /// by source offset, with disjoint destinations.
    pub copies: Vec<(u64, u64, u64)>,
    records: Vec<(u64, Vec<u8>)>,
    geo: Geometry,
    boot: Vec<u8>,
    mft_runs: Vec<Run>,
    mirror_runs: Vec<Run>,
    mirror_records: u64,
    bitmap_runs: Vec<Run>,
    bitmap: Vec<u8>,
    log_runs: Vec<Run>,
    log_len: u64,
}

/// Free clusters available for relocation, indexed by (length, start) for best fit.
struct Pool {
    by_len: BTreeMap<(u64, u64), ()>,
}

impl Pool {
    fn new(bits: &[u8], limit: u64) -> Self {
        let mut by_len = BTreeMap::new();
        let mut c = 0u64;
        while c < limit {
            if used(bits, c) {
                c += 1;
                continue;
            }
            let s = c;
            while c < limit && !used(bits, c) {
                c += 1;
            }
            by_len.insert((c - s, s), ());
        }
        Pool { by_len }
    }

    /// Takes `n` clusters: one extent when one is large enough (the smallest that
    /// fits), otherwise the largest extents in turn.
    fn take(&mut self, mut n: u64) -> Option<Vec<(u64, u64)>> {
        let mut out = Vec::new();
        while n > 0 {
            let key = match self.by_len.range((n, 0)..).next() {
                Some((k, _)) => *k,
                None => *self.by_len.keys().next_back()?,
            };
            self.by_len.remove(&key);
            let (len, start) = key;
            let k = len.min(n);
            out.push((start, k));
            if len > k {
                self.by_len.insert((len - k, start + k), ());
            }
            n -= k;
        }
        Some(out)
    }
}

fn used(bits: &[u8], c: u64) -> bool {
    bits.get((c / 8) as usize).is_some_and(|b| b & (1 << (c % 8)) != 0)
}

fn set(bits: &mut [u8], c: u64, on: bool) {
    let b = &mut bits[(c / 8) as usize];
    if on {
        *b |= 1 << (c % 8);
    } else {
        *b &= !(1 << (c % 8));
    }
}

/// Reads and decodes an MFT record (fixups applied); `None` if it is not in use.
fn decode_record(raw: &[u8], n: u64) -> Result<Option<Vec<u8>>, String> {
    let in_use = raw.len() > 0x18 && le16(raw, 0x16) & RECORD_IN_USE != 0;
    match &raw[0..4] {
        b"FILE" => {}
        b"BAAD" if in_use => {
            return Err(tf!("enregistrement MFT {n} endommagé (BAAD)", "MFT record {n} damaged (BAAD)"));
        }
        _ => return Ok(None),
    }
    if !in_use {
        return Ok(None);
    }
    let mut rec = raw.to_vec();
    unfixup(&mut rec).map_err(|e| tf!("enregistrement MFT {n} : {e}", "MFT record {n}: {e}"))?;
    if le32(&rec, 0x1C) as usize != rec.len() {
        return Err(tf!("enregistrement MFT {n} : taille allouée inattendue", "MFT record {n}: unexpected allocated size"));
    }
    Ok(Some(rec))
}

/// Base record of an extension record (`None` for a base record). The whole
/// reference is tested: the extensions of `$MFT` point to record 0.
fn base_ref(rec: &[u8]) -> Option<u64> {
    let r = le64(rec, 0x20);
    (r != 0).then_some(r & 0xFFFF_FFFF_FFFF)
}

/// Value of a resident attribute.
fn resident_value(rec: &[u8], off: usize, len: usize) -> Result<Vec<u8>, String> {
    let vlen = le32(rec, off + 0x10) as usize;
    let voff = le16(rec, off + 0x14) as usize;
    if voff + vlen > len {
        return Err(t!("attribut résident invalide", "invalid resident attribute").into());
    }
    Ok(rec[off + voff..off + voff + vlen].to_vec())
}

/// Joins the pieces `(start_vcn, runs)` of an attribute split over several records.
fn join(mut pieces: Vec<(u64, Vec<Run>)>) -> Result<Vec<Run>, String> {
    pieces.sort_by_key(|p| p.0);
    let mut out: Vec<Run> = Vec::new();
    for (start, runs) in pieces {
        if start != total_len(&out) {
            return Err(t!("liste d'extensions de la MFT discontinue", "discontinuous MFT run list").into());
        }
        for r in runs {
            push_run(&mut out, r);
        }
    }
    Ok(out)
}

/// Opens the volume: boot sector and complete run list of `$MFT`, including when
/// it is split over extension records (attribute list).
fn open_volume<'a>(dev: &'a mut dyn BlockDevice, base: u64, len: u64) -> Result<(Volume<'a>, u64), String> {
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
    let g = v.geo;
    if g.sector != ss {
        return Err(tf!("le volume utilise des secteurs de {} octets, le disque de {ss}", "the volume uses {}-byte sectors, the disk {ss}", g.sector));
    }
    if (g.total_sectors + 1) * ss > len {
        return Err(
            t!("le système de fichiers déclare une taille supérieure à sa partition", "the file system claims a size larger than its partition").into()
        );
    }
    let rs = g.record_size;
    let raw0 = v.read_bytes(g.mft_lcn * g.cluster, rs as usize)?;
    let r0 = decode_record(&raw0, 0)?.ok_or(t!("enregistrement MFT 0 inutilisé", "MFT record 0 unused"))?;
    let list = attrs(&r0)?;
    let mut pieces = Vec::new();
    let mut size = None;
    for a in list.iter().filter(|a| a.kind == ATTR_DATA && a.name.is_empty()) {
        let nr = nonres(&r0, a)?;
        if nr.start_vcn == 0 {
            size = Some(nr.data);
        }
        pieces.push((nr.start_vcn, nr.runs));
    }
    if let Some(al) = list.iter().find(|a| a.kind == ATTR_ATTRIBUTE_LIST) {
        let content = if al.nonres {
            let nr = nonres(&r0, al)?;
            let runs = nr.runs.clone();
            v.read_stream(&runs, 0, nr.data)?
        } else {
            resident_value(&r0, al.off, al.len)?
        };
        let mut entries = Vec::new();
        let mut o = 0usize;
        while o + 0x1A <= content.len() {
            let elen = le16(&content, o + 4) as usize;
            if elen < 0x1A || o + elen > content.len() {
                return Err(t!("liste d'attributs de $MFT invalide", "invalid $MFT attribute list").into());
            }
            let (kind, nlen, rec) = (le32(&content, o), content[o + 6], le64(&content, o + 0x10) & 0xFFFF_FFFF_FFFF);
            if kind == ATTR_DATA && nlen == 0 && rec != 0 {
                entries.push((le64(&content, o + 8), rec));
            }
            o += elen;
        }
        entries.sort();
        for (vcn, rec) in entries {
            let runs = join(pieces.clone())?;
            let raw = v.read_stream(&runs, rec * rs, rs)?;
            let r = decode_record(&raw, rec)?.ok_or_else(|| tf!("enregistrement MFT {rec} inutilisé", "MFT record {rec} unused"))?;
            if base_ref(&r) != Some(0) {
                return Err(tf!("enregistrement MFT {rec} : extension étrangère à $MFT", "MFT record {rec}: extension not owned by $MFT"));
            }
            let l = attrs(&r)?;
            let a = l
                .iter()
                .find(|a| a.kind == ATTR_DATA && a.name.is_empty() && a.nonres && le64(&r, a.off + 0x10) == vcn)
                .ok_or_else(|| tf!("enregistrement MFT {rec} : extension de $MFT introuvable", "MFT record {rec}: $MFT extension not found"))?;
            pieces.push((vcn, nonres(&r, a)?.runs));
        }
    }
    v.mft_runs = join(pieces)?;
    let size = size.ok_or(t!("attribut $DATA de $MFT absent", "$MFT $DATA attribute missing"))?;
    if size > total_len(&v.mft_runs) * g.cluster || !size.is_multiple_of(rs) {
        return Err(t!("taille de $MFT incohérente", "inconsistent $MFT size").into());
    }
    let r1 = v.read_record(1)?;
    let mirror = data_runs(&r1.data, "")?;
    v.mirror_records = mirror.data / rs;
    v.mirror_runs = mirror.runs;
    Ok((v, size / rs))
}

/// State of the relocation while records are processed.
struct Ctx {
    n: u64,
    cluster: u64,
    pool: Pool,
    bits: Vec<u8>,
    copies: Vec<(u64, u64, u64)>,
    relocated: u64,
    bitmap_alloc_clusters: u64,
    bitmap_data: u64,
    freed: Vec<(u64, u64)>,
}

impl Ctx {
    /// New runs: the parts beyond the new end move to free clusters below it.
    fn relocate(&mut self, runs: &[Run]) -> Result<(Vec<Run>, bool), String> {
        let mut out = Vec::new();
        let mut moved = false;
        for r in runs {
            let Some(l) = r.lcn else {
                push_run(&mut out, *r);
                continue;
            };
            let keep = if l >= self.n { 0 } else { r.len.min(self.n - l) };
            if keep > 0 {
                push_run(&mut out, Run { len: keep, lcn: Some(l) });
            }
            let mut from = l + keep;
            let rest = r.len - keep;
            if rest == 0 {
                continue;
            }
            moved = true;
            let parts = self.pool.take(rest).ok_or(t!(
                "espace libre insuffisant sous la nouvelle limite du volume",
                "not enough free space below the new end of the volume"
            ))?;
            for (t, k) in parts {
                push_run(&mut out, Run { len: k, lcn: Some(t) });
                self.copies.push((from * self.cluster, t * self.cluster, k * self.cluster));
                for c in t..t + k {
                    set(&mut self.bits, c, true);
                }
                from += k;
                self.relocated += k;
            }
        }
        Ok((out, moved))
    }
}

/// Truncates a run list to `clusters` clusters; returns the kept and the removed runs.
fn truncate(runs: &[Run], clusters: u64) -> (Vec<Run>, Vec<Run>) {
    let (mut keep, mut cut) = (Vec::new(), Vec::new());
    let mut left = clusters;
    for r in runs {
        if left >= r.len {
            keep.push(*r);
            left -= r.len;
        } else {
            if left > 0 {
                keep.push(Run { len: left, lcn: r.lcn });
            }
            cut.push(Run { len: r.len - left, lcn: r.lcn.map(|l| l + left) });
            left = 0;
        }
    }
    (keep, cut)
}

/// Special streams whose new run lists are needed to write the destination.
#[derive(Default)]
struct Special {
    mft: Vec<(u64, Vec<Run>)>,
    mirror: Option<Vec<Run>>,
    log: Option<(Vec<Run>, u64)>,
    bitmap: Option<Vec<Run>>,
}

fn process_record(ctx: &mut Ctx, sp: &mut Special, n: u64, rec: &mut Vec<u8>) -> Result<bool, String> {
    let base = base_ref(rec);
    let ext = base.is_some();
    let owner = base.unwrap_or(n);
    let mut changed = false;
    let mut i = 0usize;
    loop {
        let list = attrs(rec).map_err(|e| tf!("enregistrement MFT {n} : {e}", "MFT record {n}: {e}"))?;
        let Some(a) = list.get(i).cloned() else { break };
        i += 1;
        if !a.nonres {
            continue;
        }
        let mut nr = nonres(rec, &a).map_err(|e| tf!("enregistrement MFT {n} : {e}", "MFT record {n}: {e}"))?;
        let unnamed_data = a.kind == ATTR_DATA && a.name.is_empty();
        let room = |e: String| {
            tf!(
                "enregistrement MFT {n} : {e} (fichier trop fragmenté pour être déplacé)",
                "MFT record {n}: {e} (file too fragmented to be moved)"
            )
        };
        if owner == 8 && a.kind == ATTR_DATA && a.name == "$Bad" {
            if ext || nr.start_vcn != 0 {
                return Err(t!("$BadClus fragmenté, non pris en charge", "fragmented $BadClus, not supported").into());
            }
            nr.runs = vec![Run { len: ctx.n, lcn: None }];
            nr.last_vcn = ctx.n - 1;
            nr.alloc = ctx.n * ctx.cluster;
            nr.data = nr.alloc;
            nr.init = nr.init.min(nr.data);
            rewrite_nonres(rec, &a, &nr).map_err(room)?;
            changed = true;
            continue;
        }
        let bitmap = owner == 6 && unnamed_data;
        if bitmap {
            if ext || nr.start_vcn != 0 || a.flags & 0x80FF != 0 {
                return Err(t!("$Bitmap fragmenté ou compressé, non pris en charge", "fragmented or compressed $Bitmap, not supported").into());
            }
            let (keep, cut) = truncate(&nr.runs, ctx.bitmap_alloc_clusters);
            for r in cut {
                if let Some(l) = r.lcn {
                    ctx.freed.push((l, r.len));
                }
            }
            nr.runs = keep;
            nr.last_vcn = ctx.bitmap_alloc_clusters - 1;
            nr.alloc = ctx.bitmap_alloc_clusters * ctx.cluster;
            nr.data = ctx.bitmap_data;
            nr.init = ctx.bitmap_data;
        }
        let (runs, moved) = ctx.relocate(&nr.runs).map_err(|e| tf!("enregistrement MFT {n} : {e}", "MFT record {n}: {e}"))?;
        if moved || bitmap {
            nr.runs = runs;
            rewrite_nonres(rec, &a, &nr).map_err(room)?;
            changed = true;
        }
        if unnamed_data {
            match owner {
                0 => sp.mft.push((nr.start_vcn, nr.runs.clone())),
                1 => sp.mirror = Some(nr.runs.clone()),
                2 if nr.start_vcn == 0 => sp.log = Some((nr.runs.clone(), nr.data)),
                2 => return Err(t!("$LogFile fragmenté, non pris en charge", "fragmented $LogFile, not supported").into()),
                6 => sp.bitmap = Some(nr.runs.clone()),
                _ => {}
            }
        }
    }
    // $Volume: "needs checking" flag.
    if n == 3 {
        let list = attrs(rec)?;
        let vi = list
            .iter()
            .find(|a| a.kind == ATTR_VOLUME_INFORMATION && !a.nonres)
            .ok_or(t!("attribut $VOLUME_INFORMATION absent de $Volume", "$VOLUME_INFORMATION attribute missing from $Volume"))?;
        let voff = vi.off + le16(rec, vi.off + 0x14) as usize;
        if le32(rec, vi.off + 0x10) < 12 || voff + 12 > vi.off + vi.len {
            return Err(t!("attribut $VOLUME_INFORMATION invalide", "invalid $VOLUME_INFORMATION attribute").into());
        }
        if rec[voff + 8] != 3 {
            return Err(tf!("version NTFS {}.x non prise en charge", "NTFS version {}.x not supported", rec[voff + 8]));
        }
        let flags = le16(rec, voff + 10);
        put16(rec, voff + 10, flags | VOLUME_IS_DIRTY);
        changed = true;
    }
    Ok(changed)
}

/// Reads the source volume at `base` (`len`: its partition) and computes the shrink
/// to `new_len` bytes. Reads only.
pub fn prepare(src: &mut dyn BlockDevice, base: u64, len: u64, new_len: u64) -> Result<ShrinkPrep, String> {
    let (mut v, records) = open_volume(src, base, len)?;
    let g = v.geo;
    let (ss, cl) = (g.sector, g.cluster);
    let oc = g.clusters();
    let new_total = (new_len / ss).saturating_sub(1);
    let n = new_total / g.spc;
    if n == 0 || n >= oc || new_len >= len {
        return Err(t!("la nouvelle taille ne réduit pas le volume", "the new size does not shrink the volume").into());
    }

    // Source allocation map.
    let r6 = v.read_record(6)?;
    let l6 = attrs(&r6.data)?;
    let bm = nonres(&r6.data, find_data(&l6, "")?)?;
    if bm.start_vcn != 0 || bm.runs.iter().any(|r| r.lcn.is_none()) || bm.data < oc.div_ceil(8) || bm.init != bm.data {
        return Err(t!("attribut $DATA de $Bitmap incohérent", "inconsistent $Bitmap $DATA attribute").into());
    }
    let src_bits = v.read_stream(&bm.runs, 0, oc.div_ceil(8))?;
    if !used(&src_bits, 0) {
        return Err(t!("$Bitmap incohérent : le cluster 0 est marqué libre", "inconsistent $Bitmap: cluster 0 is marked free").into());
    }
    let used_total = (0..oc).filter(|&c| used(&src_bits, c)).count() as u64;
    if (used_total * cl).saturating_add(reserve(n * cl)) > n * cl {
        return Err(tf!(
            "le volume contient {} de données, trop pour {}",
            "the volume holds {} of data, too much for {}",
            crate::diag::human_size(used_total * cl),
            crate::diag::human_size(n * cl)
        ));
    }
    // Bad clusters cannot be moved.
    let r8 = v.read_record(8)?;
    let bad = data_runs(&r8.data, "$Bad")?;
    if bad.runs.iter().any(|r| r.lcn.is_some()) {
        return Err(t!(
            "le volume contient des clusters défectueux recensés ($BadClus) : réduction impossible",
            "the volume has recorded bad clusters ($BadClus): cannot shrink"
        )
        .into());
    }

    // New allocation map: source map below the new end, padding bits set.
    let bitmap_data = align_up(n.div_ceil(8), 8);
    let mut bits = src_bits.clone();
    bits.resize(bitmap_data as usize, 0);
    bits.truncate(bitmap_data as usize);
    for c in n..bitmap_data * 8 {
        set(&mut bits, c, true);
    }
    let mut ctx = Ctx {
        n,
        cluster: cl,
        pool: Pool::new(&src_bits, n),
        bits,
        copies: Vec::new(),
        relocated: 0,
        bitmap_alloc_clusters: align_up(bitmap_data, cl) / cl,
        bitmap_data,
        freed: Vec::new(),
    };
    let mut sp = Special::default();
    let mut changed: Vec<(u64, Vec<u8>)> = Vec::new();
    let rs = g.record_size;
    let per = (CHUNK / rs).max(1);
    let runs = v.mft_runs.clone();
    let mut r = 0u64;
    while r < records {
        let k = per.min(records - r);
        let buf = v.read_stream(&runs, r * rs, k * rs)?;
        for i in 0..k {
            let num = r + i;
            let raw = &buf[(i * rs) as usize..((i + 1) * rs) as usize];
            if let Some(mut rec) = decode_record(raw, num)?
                && process_record(&mut ctx, &mut sp, num, &mut rec)?
            {
                changed.push((num, rec));
            }
        }
        r += k;
    }
    for n0 in [3u64, 6, 8] {
        if !changed.iter().any(|c| c.0 == n0) {
            return Err(tf!("enregistrement MFT {n0} non traité", "MFT record {n0} not processed"));
        }
    }
    let mft_runs = join(sp.mft)?;
    let mirror_runs = sp.mirror.ok_or(t!("$MFTMirr introuvable", "$MFTMirr not found"))?;
    let (log_runs, log_len) = sp.log.ok_or(t!("$LogFile introuvable", "$LogFile not found"))?;
    let bitmap_runs = sp.bitmap.ok_or(t!("$Bitmap introuvable", "$Bitmap not found"))?;
    for (l, k) in std::mem::take(&mut ctx.freed) {
        for c in l..(l + k).min(n) {
            set(&mut ctx.bits, c, false);
        }
    }

    // Copies: used clusters kept in place, then the relocated runs.
    let mut copies = std::mem::take(&mut ctx.copies);
    let mut c = 0u64;
    while c < n {
        if !used(&src_bits, c) {
            c += 1;
            continue;
        }
        let s = c;
        while c < n && used(&src_bits, c) {
            c += 1;
        }
        copies.push((s * cl, s * cl, (c - s) * cl));
    }
    copies.sort();
    let mut dsts: Vec<(u64, u64)> = copies.iter().map(|&(_, d, l)| (d, d + l)).collect();
    dsts.sort();
    if dsts.windows(2).any(|w| w[0].1 > w[1].0) || dsts.last().is_some_and(|d| d.1 > n * cl) {
        return Err(t!("plan de déplacement incohérent (chevauchement)", "inconsistent relocation plan (overlap)").into());
    }

    let first = |runs: &[Run]| runs.first().and_then(|r| r.lcn).ok_or(t!("position de début introuvable", "start position not found"));
    let mut boot = v.boot.clone();
    let mft_lcn = first(&mft_runs)?;
    let mirror_lcn = first(&mirror_runs)?;
    put64(&mut boot, 0x28, new_total);
    put64(&mut boot, 0x30, mft_lcn);
    put64(&mut boot, 0x38, mirror_lcn);
    let used_clusters = (0..n).filter(|&c| used(&ctx.bits, c)).count() as u64;
    Ok(ShrinkPrep {
        summary: ShrinkSummary {
            old_size: g.total_sectors * ss,
            new_size: new_total * ss,
            cluster_size: cl,
            old_clusters: oc,
            new_clusters: n,
            relocated_clusters: ctx.relocated,
            records_updated: changed.len() as u64,
            used_clusters,
        },
        copies,
        records: changed,
        geo: Geometry { total_sectors: new_total, mft_lcn, mftmirr_lcn: mirror_lcn, ..g },
        boot,
        mft_runs,
        mirror_runs,
        mirror_records: v.mirror_records,
        bitmap_runs,
        bitmap: ctx.bits,
        log_runs,
        log_len,
    })
}

/// Writes the updated structures on the destination volume at `base`, once its data
/// has been copied according to [`ShrinkPrep::copies`]. The primary boot sector is
/// written last.
pub fn apply(dst: &mut dyn BlockDevice, base: u64, new_len: u64, p: &ShrinkPrep) -> Result<(), String> {
    let mut v = Volume {
        dev: dst,
        base,
        len: new_len,
        geo: p.geo,
        boot: p.boot.clone(),
        mft_runs: p.mft_runs.clone(),
        mirror_runs: p.mirror_runs.clone(),
        mirror_records: p.mirror_records,
    };
    let flush = |v: &mut Volume| v.dev.flush().map_err(|e| tf!("vidage des écritures : {e}", "flushing writes: {e}"));
    // $Volume first ("needs checking"), then every other record.
    for (n, data) in p.records.iter().filter(|r| r.0 == 3).chain(p.records.iter().filter(|r| r.0 != 3)) {
        v.write_record(&Record { number: *n, data: data.clone() })?;
    }
    let runs = p.bitmap_runs.clone();
    let mut pos = 0u64;
    while pos < p.bitmap.len() as u64 {
        let k = CHUNK.min(p.bitmap.len() as u64 - pos);
        v.write_stream(&runs, pos, &p.bitmap[pos as usize..(pos + k) as usize])?;
        pos += k;
    }
    let runs = p.log_runs.clone();
    let ff = vec![0xFFu8; CHUNK as usize];
    let mut pos = 0u64;
    while pos < p.log_len {
        let k = CHUNK.min(p.log_len - pos);
        v.write_stream(&runs, pos, &ff[..k as usize])?;
        pos += k;
    }
    flush(&mut v)?;
    let ss = p.geo.sector;
    v.write_bytes(p.geo.total_sectors * ss, &p.boot)?;
    flush(&mut v)?;
    v.write_bytes(0, &p.boot)?;
    flush(&mut v)
}

/// Reads the shrunk volume back and checks the structures written by [`apply`].
pub fn verify(dst: &mut dyn BlockDevice, base: u64, new_len: u64, p: &ShrinkPrep) -> Result<(), String> {
    let ctx = |e: String| tf!("relecture après réduction : {e}", "read-back after shrinking: {e}");
    let (mut v, _) = open_volume(dst, base, new_len).map_err(ctx)?;
    let g = v.geo;
    if g.total_sectors != p.geo.total_sectors || g.mft_lcn != p.geo.mft_lcn || g.mftmirr_lcn != p.geo.mftmirr_lcn {
        return Err(ctx(t!("secteur d'amorçage inattendu", "unexpected boot sector").into()));
    }
    if v.mft_runs != p.mft_runs {
        return Err(ctx(t!("liste d'extensions de $MFT inattendue", "unexpected $MFT run list").into()));
    }
    let r6 = v.read_record(6).map_err(ctx)?;
    let bm = data_runs(&r6.data, "").map_err(ctx)?;
    if bm.runs != p.bitmap_runs || bm.data != p.bitmap.len() as u64 {
        return Err(ctx(t!("attribut $DATA de $Bitmap inattendu", "unexpected $Bitmap $DATA attribute").into()));
    }
    let content = v.read_stream(&bm.runs, 0, bm.data).map_err(ctx)?;
    if content != p.bitmap {
        return Err(ctx(t!("carte d'allocation différente de celle calculée", "allocation map differs from the computed one").into()));
    }
    let r8 = v.read_record(8).map_err(ctx)?;
    let bad = data_runs(&r8.data, "$Bad").map_err(ctx)?;
    if bad.last_vcn + 1 != p.summary.new_clusters {
        return Err(ctx(t!("taille de $BadClus inattendue", "unexpected $BadClus size").into()));
    }
    let backup = v.read_bytes(g.total_sectors * g.sector, 512).map_err(ctx)?;
    if backup[..512] != v.boot[..512] {
        return Err(ctx(t!("secteur d'amorçage de secours différent du principal", "backup boot sector differs from the primary").into()));
    }
    drop(v);
    // Whole allocation, as chkdsk checks it: every run of every record decodes as
    // Windows reads it, lies in the volume, is marked used and has a single owner.
    let problems = check_allocation(dst, base, new_len).map_err(ctx)?;
    if !problems.is_empty() {
        return Err(ctx(tf!("allocation incohérente : {}", "inconsistent allocation: {}", problems.join(" ; "))));
    }
    Ok(())
}

/// Allocation check of a volume, like the first phase of chkdsk: every run of every
/// in-use record must decode as Windows reads it, lie inside the volume, be marked
/// used in `$Bitmap` and be claimed only once. Returns the problems found (empty:
/// consistent). Clusters marked used without an owner are tolerated (chkdsk frees
/// them without loss).
pub fn check_allocation(dev: &mut dyn BlockDevice, base: u64, len: u64) -> Result<Vec<String>, String> {
    let (mut v, records) = open_volume(dev, base, len)?;
    let g = v.geo;
    let oc = g.clusters();
    let r6 = v.read_record(6)?;
    let bm = data_runs(&r6.data, "")?;
    let bits = v.read_stream(&bm.runs, 0, oc.div_ceil(8))?;
    let mut seen = vec![0u8; oc.div_ceil(8) as usize];
    let mut problems = Vec::new();
    let rs = g.record_size;
    let per = (CHUNK / rs).max(1);
    let runs = v.mft_runs.clone();
    let mut r = 0u64;
    while r < records && problems.len() < 20 {
        let k = per.min(records - r);
        let buf = v.read_stream(&runs, r * rs, k * rs)?;
        for i in 0..k {
            let num = r + i;
            let Some(rec) = decode_record(&buf[(i * rs) as usize..((i + 1) * rs) as usize], num)? else { continue };
            let owner = base_ref(&rec).unwrap_or(num);
            let list = attrs(&rec).map_err(|e| tf!("enregistrement MFT {num} : {e}", "MFT record {num}: {e}"))?;
            for a in list.iter().filter(|a| a.nonres) {
                let nr = match nonres(&rec, a) {
                    Ok(nr) => nr,
                    Err(e) => {
                        problems.push(format!("record {num}, attribute {:#x}: {e}", a.kind));
                        continue;
                    }
                };
                if owner == 8 && a.name == "$Bad" {
                    continue;
                }
                for run in &nr.runs {
                    let Some(l) = run.lcn else { continue };
                    if l + run.len > oc {
                        problems.push(format!("record {num}, attribute {:#x}: run {l}+{} beyond the volume ({oc} clusters)", a.kind, run.len));
                        continue;
                    }
                    for c in l..l + run.len {
                        if used(&seen, c) {
                            problems.push(format!("cluster {c} claimed twice (record {num})"));
                            break;
                        }
                        set(&mut seen, c, true);
                        if !used(&bits, c) {
                            problems.push(format!("cluster {c} (record {num}) marked free in $Bitmap"));
                            break;
                        }
                    }
                }
            }
        }
        r += k;
    }
    Ok(problems)
}

/// Clears the "needs checking" flag of `$Volume`, as chkdsk does once it has
/// checked the volume. For tests only.
#[doc(hidden)]
pub fn testing_mark_clean(dev: &mut dyn BlockDevice, base: u64, len: u64) -> Result<(), String> {
    let (mut v, _) = open_volume(dev, base, len)?;
    let mut r3 = v.read_record(3)?;
    let list = attrs(&r3.data)?;
    let vi = list.iter().find(|a| a.kind == ATTR_VOLUME_INFORMATION && !a.nonres).ok_or("no $VOLUME_INFORMATION")?;
    let voff = vi.off + le16(&r3.data, vi.off + 0x14) as usize;
    let flags = le16(&r3.data, voff + 10);
    put16(&mut r3.data, voff + 10, flags & !VOLUME_IS_DIRTY);
    v.write_record(&r3)?;
    v.dev.flush().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_prefers_one_extent_then_largest() {
        // Used: 0-1, 5, 9; free: 2-4 (3), 6-8 (3), 10-15 (6).
        let mut bits = vec![0u8; 2];
        for c in [0u64, 1, 5, 9] {
            set(&mut bits, c, true);
        }
        let mut p = Pool::new(&bits, 16);
        assert_eq!(p.take(3), Some(vec![(2, 3)]), "smallest extent that fits");
        assert_eq!(p.take(2), Some(vec![(6, 2)]));
        assert_eq!(p.take(7), Some(vec![(10, 6), (8, 1)]), "largest first when nothing fits");
        assert_eq!(p.take(1), None);
    }

    #[test]
    fn truncate_splits_runs() {
        let runs = vec![Run { len: 4, lcn: Some(10) }, Run { len: 4, lcn: Some(100) }];
        let (k, c) = truncate(&runs, 6);
        assert_eq!(k, vec![Run { len: 4, lcn: Some(10) }, Run { len: 2, lcn: Some(100) }]);
        assert_eq!(c, vec![Run { len: 2, lcn: Some(102) }]);
    }

    #[test]
    fn estimate_counts_data_beyond_the_new_end() {
        let ext = vec![Extent { offset: 0, len: 64 * MIB }, Extent { offset: 900 * MIB, len: 36 * MIB }];
        let e = estimate(&ext, 4096, 1024 * MIB, 512, 600 * MIB).unwrap();
        assert_eq!(e.used_bytes, 100 * MIB);
        assert_eq!(e.relocated_bytes, 36 * MIB);
        assert!(estimate(&ext, 4096, 1024 * MIB, 512, 300 * MIB).is_ok());
        assert!(estimate(&ext, 4096, 1024 * MIB, 512, 120 * MIB).is_err(), "free space reserve");
        assert!(estimate(&ext, 4096, 1024 * MIB, 512, 2048 * MIB).is_err(), "not a shrink");
    }
}
