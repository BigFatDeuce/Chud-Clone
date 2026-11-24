//! Device with simulated defects.
//!
//! Used by the engine tests: permanent or intermittent unreadable sectors,
//! disconnection during the copy, write errors, silent corruption.
//! No real disk is involved.

use crate::device::{BlockDevice, IoErrorClass, fault};
use std::collections::HashMap;
use std::io;

#[derive(Default)]
pub struct FaultPlan {
    /// Ranges (bytes) permanently unreadable.
    pub bad_read: Vec<(u64, u64)>,
    /// Ranges unreadable during the first `n` attempts touching them.
    pub flaky_read: Vec<(u64, u64, u32)>,
    /// The device disappears after this number of bytes read.
    pub vanish_after_read: Option<u64>,
    /// The device disappears after this number of bytes written.
    pub vanish_after_write: Option<u64>,
    /// Ranges where every write fails (media error).
    pub bad_write: Vec<(u64, u64)>,
    /// Ranges where the write "succeeds" but is not recorded faithfully.
    pub corrupt_write: Vec<(u64, u64)>,
    /// Access denied on every read.
    pub deny_read: bool,
    /// Write protection.
    pub write_protected: bool,
    /// Ranges whose content changes on every read (unstable source).
    pub unstable_read: Vec<(u64, u64)>,
    /// Requests cancellation once this number of bytes has been read (simulates a click on "Cancel").
    pub cancel_after_read: Option<(u64, &'static crate::engine::Control)>,
    /// Slow ranges: every read touching them takes this number of milliseconds.
    pub slow_read: Vec<(u64, u64, u64)>,
}

pub struct FaultyDevice<D: BlockDevice> {
    pub inner: D,
    pub plan: FaultPlan,
    pub bytes_read: u64,
    pub bytes_written: u64,
    pub read_calls: u64,
    pub write_calls: u64,
    flaky_hits: HashMap<usize, u32>,
    unstable_counter: u8,
}

fn overlaps(a: u64, alen: u64, b: u64, blen: u64) -> bool {
    a < b + blen && b < a + alen
}

impl<D: BlockDevice> FaultyDevice<D> {
    pub fn new(inner: D, plan: FaultPlan) -> Self {
        Self { inner, plan, bytes_read: 0, bytes_written: 0, read_calls: 0, write_calls: 0, flaky_hits: HashMap::new(), unstable_counter: 0 }
    }
}

impl<D: BlockDevice> BlockDevice for FaultyDevice<D> {
    fn size(&self) -> u64 {
        self.inner.size()
    }
    fn sector_size(&self) -> u32 {
        self.inner.sector_size()
    }
    fn physical_sector_size(&self) -> u32 {
        self.inner.physical_sector_size()
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.read_calls += 1;
        let len = buf.len() as u64;
        if self.plan.deny_read {
            return Err(fault(IoErrorClass::AccessDenied));
        }
        if let Some(limit) = self.plan.vanish_after_read
            && self.bytes_read + len > limit
        {
            return Err(fault(IoErrorClass::DeviceGone));
        }
        if self.plan.bad_read.iter().any(|(o, l)| overlaps(offset, len, *o, *l)) {
            return Err(fault(IoErrorClass::Media));
        }
        for (i, (o, l, n)) in self.plan.flaky_read.iter().enumerate() {
            if overlaps(offset, len, *o, *l) {
                let hits = self.flaky_hits.entry(i).or_insert(0);
                if *hits < *n {
                    *hits += 1;
                    return Err(fault(IoErrorClass::Media));
                }
            }
        }
        if let Some((_, _, ms)) = self.plan.slow_read.iter().find(|(o, l, _)| overlaps(offset, len, *o, *l)) {
            std::thread::sleep(std::time::Duration::from_millis(*ms));
        }
        self.inner.read_at(offset, buf)?;
        for (o, l) in &self.plan.unstable_read {
            if overlaps(offset, len, *o, *l) {
                self.unstable_counter = self.unstable_counter.wrapping_add(1);
                let start = o.max(&offset) - offset;
                buf[start as usize] ^= self.unstable_counter | 1;
            }
        }
        self.bytes_read += len;
        if let Some((limit, ctl)) = self.plan.cancel_after_read
            && self.bytes_read >= limit
        {
            ctl.cancel();
        }
        Ok(())
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()> {
        self.write_calls += 1;
        let len = buf.len() as u64;
        if self.plan.write_protected {
            return Err(fault(IoErrorClass::WriteProtected));
        }
        if let Some(limit) = self.plan.vanish_after_write
            && self.bytes_written + len > limit
        {
            return Err(fault(IoErrorClass::DeviceGone));
        }
        if self.plan.bad_write.iter().any(|(o, l)| overlaps(offset, len, *o, *l)) {
            return Err(fault(IoErrorClass::Media));
        }
        if let Some((o, _)) = self.plan.corrupt_write.iter().find(|(o, l)| overlaps(offset, len, *o, *l)) {
            let mut copy = buf.to_vec();
            let at = (o.max(&offset) - offset) as usize;
            copy[at] ^= 0x5A;
            self.inner.write_at(offset, &copy)?;
        } else {
            self.inner.write_at(offset, buf)?;
        }
        self.bytes_written += len;
        Ok(())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
    fn is_read_only(&self) -> bool {
        self.inner.is_read_only()
    }
    fn label(&self) -> String {
        format!("{} (défauts simulés)", self.inner.label())
    }
    fn set_uncached_reads(&mut self, on: bool) -> io::Result<bool> {
        self.inner.set_uncached_reads(on)
    }
}
