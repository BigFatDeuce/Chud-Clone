//! Read-ahead on the source during the copy.
//!
//! Without it, the copy alternates: read a block from the source, then write it to the
//! destination, then read the next one. Each disk waits while the other works. With
//! read-ahead, a worker thread reads block N+1 while block N is being written, so the
//! time per block becomes the slower of the two operations instead of their sum.
//!
//! This matters most when both disks sit in the same USB dock: USB 3 carries data in
//! both directions at once (reads towards the computer, writes towards the dock), so an
//! alternating copy leaves half of the link idle.
//!
//! Only reads that follow the copy's planned order are prefetched (the list of blocks is
//! known in advance). Any other read (isolation of a failing area, re-reads) is done
//! synchronously, exactly as without read-ahead, and nothing is read speculatively
//! outside the plan. The source stays read-only: [`BlockDevice::write_at`] is refused.

use crate::device::{AlignedBuf, BlockDevice};

use std::io;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::Scope;

struct Request {
    offset: u64,
    len: usize,
    buf: AlignedBuf,
}

struct Reply {
    offset: u64,
    len: usize,
    buf: AlignedBuf,
    result: io::Result<()>,
}

/// Source device read through a worker thread that stays one block ahead.
pub struct ReadAhead {
    size: u64,
    sector: u32,
    physical: u32,
    label: String,
    requests: Option<Sender<Request>>,
    replies: Receiver<Reply>,
    /// Block being read ahead, if any.
    pending: Option<(u64, usize)>,
    /// Buffers not in use (at most two exist).
    spare: Vec<AlignedBuf>,
    buf_len: usize,
    /// Planned reads, in copy order, and the position reached in that list.
    schedule: Vec<(u64, usize)>,
    cursor: usize,
    /// Reads served from the read-ahead (for tests and logs).
    pub prefetched_reads: u64,
}

impl ReadAhead {
    /// Starts the worker thread in `scope`. `schedule` lists the reads the copy will
    /// make, in order; `max_len` is the largest of them.
    pub fn new<'scope, 'env>(scope: &'scope Scope<'scope, 'env>, src: &'env mut dyn BlockDevice, schedule: Vec<(u64, usize)>) -> Self {
        let buf_len = schedule.iter().map(|s| s.1).max().unwrap_or(0).max(src.sector_size() as usize);
        let (size, sector, physical, label) = (src.size(), src.sector_size(), src.physical_sector_size(), src.label());
        let (req_tx, req_rx) = channel::<Request>();
        let (rep_tx, rep_rx) = channel::<Reply>();
        scope.spawn(move || {
            // Ends when the `ReadAhead` is dropped (the request channel closes).
            for mut r in req_rx {
                let result = src.read_at(r.offset, &mut r.buf[..r.len]);
                if rep_tx.send(Reply { offset: r.offset, len: r.len, buf: r.buf, result }).is_err() {
                    break;
                }
            }
        });
        Self {
            size,
            sector,
            physical,
            label,
            requests: Some(req_tx),
            replies: rep_rx,
            pending: None,
            spare: Vec::new(),
            buf_len,
            schedule,
            cursor: 0,
            prefetched_reads: 0,
        }
    }

    fn take_buf(&mut self, len: usize) -> AlignedBuf {
        match self.spare.pop() {
            Some(b) if b.len() >= len => b,
            _ => AlignedBuf::new(len.max(self.buf_len)),
        }
    }

    fn send(&mut self, offset: u64, len: usize) -> io::Result<()> {
        let buf = self.take_buf(len);
        self.requests
            .as_ref()
            .and_then(|tx| tx.send(Request { offset, len, buf }).ok())
            .ok_or_else(|| io::Error::other(t!("lecture anticipée interrompue", "read-ahead stopped")))
    }

    fn receive(&mut self) -> io::Result<Reply> {
        self.replies.recv().map_err(|_| io::Error::other(t!("lecture anticipée interrompue", "read-ahead stopped")))
    }

    /// Starts reading the next planned block after a read at (`offset`, `len`), if that
    /// read was itself the planned one.
    fn advance(&mut self, offset: u64, len: usize) {
        // The planned read may be a few entries further on when blocks were skipped
        // (a failing block isolated by smaller reads is never read whole again).
        const WINDOW: usize = 4096;
        let end = (self.cursor + WINDOW).min(self.schedule.len());
        let Some(i) = self.schedule[self.cursor..end].iter().position(|s| *s == (offset, len)) else {
            return;
        };
        self.cursor += i + 1;
        if let Some(&(o, l)) = self.schedule.get(self.cursor)
            && self.send(o, l).is_ok()
        {
            self.pending = Some((o, l));
        }
    }
}

impl BlockDevice for ReadAhead {
    fn size(&self) -> u64 {
        self.size
    }
    fn sector_size(&self) -> u32 {
        self.sector
    }
    fn physical_sector_size(&self) -> u32 {
        self.physical
    }

    fn read_at(&mut self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        let len = out.len();
        // The very first planned read starts the chain.
        if self.pending.is_none() && self.cursor == 0 && self.schedule.first() == Some(&(offset, len)) {
            self.send(offset, len)?;
            self.pending = Some((offset, len));
        }
        if let Some(p) = self.pending.take() {
            let reply = self.receive()?;
            debug_assert_eq!((reply.offset, reply.len), p);
            if (reply.offset, reply.len) == (offset, len) {
                if reply.result.is_ok() {
                    out.copy_from_slice(&reply.buf[..len]);
                }
                self.spare.push(reply.buf);
                self.prefetched_reads += 1;
                if reply.result.is_ok() {
                    self.advance(offset, len);
                }
                return reply.result;
            }
            // A different read than planned (failing area being isolated): the block
            // read ahead is dropped and will be read again if the copy asks for it.
            self.spare.push(reply.buf);
        }
        self.send(offset, len)?;
        let reply = self.receive()?;
        if reply.result.is_ok() {
            out.copy_from_slice(&reply.buf[..len]);
        }
        self.spare.push(reply.buf);
        if reply.result.is_ok() {
            self.advance(offset, len);
        }
        reply.result
    }

    fn write_at(&mut self, _offset: u64, _buf: &[u8]) -> io::Result<()> {
        Err(crate::device::fault(crate::device::IoErrorClass::WriteProtected))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn label(&self) -> String {
        self.label.clone()
    }
}

impl Drop for ReadAhead {
    fn drop(&mut self) {
        // Closing the request channel stops the worker; a read in progress finishes first.
        self.requests = None;
    }
}

/// Reads the copy will make for `tasks`, in order, with blocks of `chunk` bytes.
pub fn schedule(tasks: &[crate::plan::CopyTask], chunk: usize) -> Vec<(u64, usize)> {
    let mut v = Vec::new();
    for t in tasks {
        let mut off = 0u64;
        while off < t.len {
            let n = (chunk as u64).min(t.len - off) as usize;
            v.push((t.src + off, n));
            off += n as u64;
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::MemDevice;
    use crate::plan::CopyTask;

    fn pattern(size: usize) -> Vec<u8> {
        (0..size).map(|i| (i / 512 % 251) as u8).collect()
    }

    #[test]
    fn planned_reads_are_served_ahead_and_identical() {
        let data = pattern(64 * 4096);
        let mut dev = MemDevice::from_vec(data.clone(), 512);
        let tasks = [CopyTask { src: 0, dst: 0, len: 40 * 4096 }, CopyTask { src: 48 * 4096, dst: 0, len: 8 * 4096 }];
        let sched = schedule(&tasks, 16 * 4096);
        assert_eq!(sched.len(), 4);
        std::thread::scope(|sc| {
            let mut ra = ReadAhead::new(sc, &mut dev, sched.clone());
            for &(o, l) in &sched {
                let mut b = vec![0u8; l];
                ra.read_at(o, &mut b).unwrap();
                assert_eq!(b, data[o as usize..o as usize + l]);
            }
            assert_eq!(ra.prefetched_reads, 4);
            assert!(ra.write_at(0, &[0; 512]).is_err());
        });
    }

    #[test]
    fn off_plan_reads_stay_correct() {
        let data = pattern(32 * 4096);
        let mut dev = MemDevice::from_vec(data.clone(), 512);
        let sched = schedule(&[CopyTask { src: 0, dst: 0, len: 32 * 4096 }], 8 * 4096);
        std::thread::scope(|sc| {
            let mut ra = ReadAhead::new(sc, &mut dev, sched.clone());
            let mut b = vec![0u8; 8 * 4096];
            ra.read_at(0, &mut b).unwrap();
            // Isolation of an area: smaller reads inside the next planned block.
            let mut s = vec![0u8; 4096];
            for k in 0..8u64 {
                let o = 8 * 4096 + k * 4096;
                ra.read_at(o, &mut s).unwrap();
                assert_eq!(s, data[o as usize..o as usize + 4096]);
            }
            // The plan resumes with the following block.
            for &(o, l) in &sched[2..] {
                let mut b = vec![0u8; l];
                ra.read_at(o, &mut b).unwrap();
                assert_eq!(b, data[o as usize..o as usize + l]);
            }
        });
    }
}
