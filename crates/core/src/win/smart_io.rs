//! Reading health data from the hardware (ATA, NVMe, ATA behind a USB bridge).
//!
//! These commands are read-only: they query the disk without modifying
//! anything on it. They require administrator rights.

use super::{Access, Handle, ioctl, open_device, u32le};
use crate::model::DiskInfo;
use crate::smart::{SmartReport, parse_ata, parse_nvme};
use std::io;
use windows_sys::Win32::Storage::IscsiDisc::{IOCTL_ATA_PASS_THROUGH, IOCTL_SCSI_PASS_THROUGH};
use windows_sys::Win32::System::Ioctl::IOCTL_STORAGE_QUERY_PROPERTY;

const ATA_FLAGS_DRDY_REQUIRED: u16 = 0x01;
const ATA_FLAGS_DATA_IN: u16 = 0x02;
/// Size of ATA_PASS_THROUGH_EX on 64-bit.
const APT_SIZE: usize = 48;
/// Size of SCSI_PASS_THROUGH on 64-bit.
const SPT_SIZE: usize = 56;
const SENSE_LEN: usize = 32;

const SMART_CMD: u8 = 0xB0;
const SMART_READ_DATA: u8 = 0xD0;
const SMART_READ_THRESHOLDS: u8 = 0xD1;
const SMART_RETURN_STATUS: u8 = 0xDA;

/// ATA SMART command through IOCTL_ATA_PASS_THROUGH. Returns (output registers, data).
fn ata_smart(h: &Handle, feature: u8, data_in: bool) -> io::Result<([u8; 8], Vec<u8>)> {
    let data_len = if data_in { 512 } else { 0 };
    let mut buf = vec![0u8; APT_SIZE + data_len];
    buf[0..2].copy_from_slice(&(APT_SIZE as u16).to_le_bytes());
    let flags = ATA_FLAGS_DRDY_REQUIRED | if data_in { ATA_FLAGS_DATA_IN } else { 0 };
    buf[2..4].copy_from_slice(&flags.to_le_bytes());
    buf[8..12].copy_from_slice(&(data_len as u32).to_le_bytes());
    buf[12..16].copy_from_slice(&10u32.to_le_bytes()); // timeout in seconds
    buf[24..32].copy_from_slice(&(APT_SIZE as u64).to_le_bytes()); // DataBufferOffset
    // CurrentTaskFile: Features, SectorCount, LBA low, LBA mid, LBA high, Device, Command, reserved.
    let tf = [feature, if data_in { 1 } else { 0 }, 0, 0x4F, 0xC2, 0xA0, SMART_CMD, 0];
    buf[40..48].copy_from_slice(&tf);
    let input = buf.clone();
    let n = ioctl(h, IOCTL_ATA_PASS_THROUGH, &input, &mut buf)?;
    if n < APT_SIZE {
        return Err(io::Error::other(t!("réponse ATA tronquée", "truncated ATA response")));
    }
    let mut out_tf = [0u8; 8];
    out_tf.copy_from_slice(&buf[40..48]);
    // Status register (byte 6): bit 0 = error.
    if out_tf[6] & 0x01 != 0 {
        return Err(io::Error::other(t!("le disque a rejeté la commande SMART", "the disk rejected the SMART command")));
    }
    Ok((out_tf, buf[APT_SIZE..].to_vec()))
}

/// ATA SMART command wrapped in SCSI (SAT, ATA PASS-THROUGH 12) for USB bridges.
fn sat_smart(h: &Handle, feature: u8) -> io::Result<Vec<u8>> {
    let total = SPT_SIZE + SENSE_LEN + 512;
    let mut buf = vec![0u8; total];
    buf[0..2].copy_from_slice(&(SPT_SIZE as u16).to_le_bytes());
    buf[6] = 12; // CdbLength
    buf[7] = SENSE_LEN as u8;
    buf[8] = 1; // SCSI_IOCTL_DATA_IN
    buf[12..16].copy_from_slice(&512u32.to_le_bytes());
    buf[16..20].copy_from_slice(&10u32.to_le_bytes());
    buf[24..32].copy_from_slice(&((SPT_SIZE + SENSE_LEN) as u64).to_le_bytes());
    buf[32..36].copy_from_slice(&(SPT_SIZE as u32).to_le_bytes());
    // ATA PASS-THROUGH (12): PIO data-in protocol, length in the sector count.
    let cdb = [0xA1u8, 4 << 1, 0x0E, feature, 1, 0, 0x4F, 0xC2, 0xA0, SMART_CMD, 0, 0];
    buf[36..48].copy_from_slice(&cdb);
    let input = buf.clone();
    let n = ioctl(h, IOCTL_SCSI_PASS_THROUGH, &input, &mut buf)?;
    if n < SPT_SIZE + SENSE_LEN {
        return Err(io::Error::other(t!("réponse SCSI tronquée", "truncated SCSI response")));
    }
    if buf[2] != 0 {
        return Err(io::Error::other(t!(
            "le boîtier USB ne relaie pas les commandes SMART",
            "the USB enclosure does not pass SMART commands through"
        )));
    }
    let data = buf[SPT_SIZE + SENSE_LEN..].to_vec();
    if data.iter().all(|b| *b == 0) {
        return Err(io::Error::other(t!("le boîtier USB a renvoyé des données SMART vides", "the USB enclosure returned empty SMART data")));
    }
    Ok(data)
}

/// NVMe health log (page 0x02) through a protocol-specific property query.
fn nvme_health(h: &Handle) -> io::Result<Vec<u8>> {
    const HDR: usize = 8; // PropertyId + QueryType
    const PSD: usize = 40; // STORAGE_PROTOCOL_SPECIFIC_DATA
    let mut buf = vec![0u8; HDR + PSD + 512];
    buf[0..4].copy_from_slice(&50u32.to_le_bytes()); // StorageDeviceProtocolSpecificProperty
    buf[4..8].copy_from_slice(&0u32.to_le_bytes()); // PropertyStandardQuery
    let p = HDR;
    buf[p..p + 4].copy_from_slice(&3u32.to_le_bytes()); // ProtocolTypeNvme
    buf[p + 4..p + 8].copy_from_slice(&2u32.to_le_bytes()); // NVMeDataTypeLogPage
    buf[p + 8..p + 12].copy_from_slice(&2u32.to_le_bytes()); // page 0x02 : SMART / Health
    buf[p + 12..p + 16].copy_from_slice(&0u32.to_le_bytes());
    buf[p + 16..p + 20].copy_from_slice(&(PSD as u32).to_le_bytes()); // ProtocolDataOffset
    buf[p + 20..p + 24].copy_from_slice(&512u32.to_le_bytes()); // ProtocolDataLength
    let input = buf.clone();
    let n = ioctl(h, IOCTL_STORAGE_QUERY_PROPERTY, &input, &mut buf)?;
    // Sortie : STORAGE_PROTOCOL_DATA_DESCRIPTOR { Version, Size, STORAGE_PROTOCOL_SPECIFIC_DATA }.
    if n < HDR + PSD {
        return Err(io::Error::other(t!("réponse NVMe tronquée", "truncated NVMe response")));
    }
    let off = HDR + u32le(&buf, HDR + 16) as usize;
    let len = u32le(&buf, HDR + 20) as usize;
    if len < 192 || off + len > buf.len() {
        return Err(io::Error::other(t!("journal de santé NVMe absent de la réponse", "NVMe health log missing from the response")));
    }
    Ok(buf[off..off + len].to_vec())
}

/// Reads a disk's health. Never returns an error: unavailability is
/// described in the report, with its reason.
pub fn read_smart(disk: &DiskInfo) -> SmartReport {
    if disk.kind != crate::model::DiskKind::Physical {
        return SmartReport::unavailable(t!("un fichier image n'a pas de données de santé matérielle", "an image file has no hardware health data"));
    }
    if disk.media == crate::model::MediaKind::Virtual {
        return SmartReport::unavailable(t!("disque virtuel : pas de données de santé matérielle", "virtual disk: no hardware health data"));
    }
    if disk.bus == "NVMe" {
        // The NVMe query sometimes succeeds without privilege: try from least to most demanding.
        let mut last = String::new();
        for access in [Access::Query, Access::Read, Access::ReadWrite] {
            match open_device(&disk.path, access, 0).and_then(|h| nvme_health(&h)) {
                Ok(log) => return parse_nvme(&log),
                Err(e) => last = e.to_string(),
            }
        }
        return SmartReport::unavailable(if super::is_elevated() {
            tf!("le SSD NVMe n'a pas communiqué son journal de santé ({last})", "the NVMe SSD did not report its health log ({last})")
        } else {
            t!("droits administrateur requis pour lire l'état de santé de ce SSD", "administrator rights are required to read this SSD's health")
                .to_string()
        });
    }
    if !super::is_elevated() {
        return SmartReport::unavailable(t!(
            "droits administrateur requis pour lire les données SMART",
            "administrator rights are required to read SMART data"
        ));
    }
    let h = match open_device(&disk.path, Access::ReadWrite, 0) {
        Ok(h) => h,
        Err(e) => return SmartReport::unavailable(tf!("ouverture du disque impossible ({e})", "cannot open the disk ({e})")),
    };
    // 1. Direct ATA pass-through (SATA, most AHCI controllers).
    let ata_err = match ata_smart(&h, SMART_READ_DATA, true) {
        Ok((_, data)) => {
            let thresholds = ata_smart(&h, SMART_READ_THRESHOLDS, true).ok().map(|x| x.1);
            let status = ata_smart(&h, SMART_RETURN_STATUS, false).ok().and_then(|(tf, _)| match (tf[3], tf[4]) {
                (0x4F, 0xC2) => Some(false),
                (0xF4, 0x2C) => Some(true),
                _ => None,
            });
            return parse_ata(&data, thresholds.as_deref(), status, "ATA");
        }
        Err(e) => e.to_string(),
    };
    // 2. Encapsulation SCSI/ATA (ponts USB-SATA compatibles SAT).
    match sat_smart(&h, SMART_READ_DATA) {
        Ok(data) => {
            let thresholds = sat_smart(&h, SMART_READ_THRESHOLDS).ok();
            let mut r = parse_ata(&data, thresholds.as_deref(), None, "ATA via USB (SAT)");
            if r.available {
                r.findings.push(crate::diag::Finding::info(
                    "SMART_NO_SELF_VERDICT",
                    t!("Verdict interne du disque non lu", "Disk's internal verdict not read"),
                    t!("À travers ce boîtier USB, seuls les compteurs sont lus ; le pronostic interne du disque (« défaillance imminente ») n'est pas interrogé.", "Through this USB enclosure only the counters are read; the disk's internal prognosis (\"imminent failure\") is not queried."),
                ));
            }
            r
        }
        Err(e) => SmartReport::unavailable(tf!(
            "ce disque ou son boîtier ne relaie pas les commandes SMART (ATA : {ata_err} ; USB/SCSI : {e})",
            "this disk or its enclosure does not pass SMART commands through (ATA: {ata_err}; USB/SCSI: {e})"
        )),
    }
}
