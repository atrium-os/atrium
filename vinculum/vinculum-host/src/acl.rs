//! ACL data: controller buffer credits and L2CAP reassembly/fragmentation
//! (Core 6.0 Vol 4 Part E §4.1, Vol 3 Part A §7).
//!
//! **Credits.** The controller has N ACL buffers (one pool for LE, one for
//! BR/EDR, or a shared pool when it reports no LE pool). Each packet sent
//! takes one; Number Of Completed Packets returns them per handle. A
//! controller that returns more than were outstanding on a handle is clamped
//! and counted: credits never exceed the pool.
//!
//! **Reassembly.** An L2CAP PDU is a 4-byte basic header (length, channel)
//! plus `length` bytes, split across ACL packets: a "first" fragment then
//! "continuing" ones. The PDU length is bounded by [`MAX_PDU`]; a peer that
//! exceeds it, or sends a continuation with no start, is a protocol error
//! and the host disconnects it.

use std::collections::HashMap;

/// Largest L2CAP PDU accepted (header + information payload). 64 KiB is the
/// field's own limit; Vinculum's channels use far less.
pub const MAX_PDU: usize = 4 + 0xFFFF;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pool {
    Le,
    BrEdr,
}

#[derive(Debug, Default)]
pub struct Credits {
    le_total: u16,
    bredr_total: u16,
    le_free: u16,
    bredr_free: u16,
    /// Whether LE shares the BR/EDR pool (controller reported 0 LE buffers).
    shared: bool,
    outstanding: HashMap<u16, (Pool, u16)>,
    pub violations: u32,
}

impl Credits {
    pub fn new(bredr_total: u16, le_total: u16) -> Credits {
        Credits {
            le_total,
            bredr_total,
            le_free: le_total,
            bredr_free: bredr_total,
            shared: le_total == 0,
            outstanding: HashMap::new(),
            violations: 0,
        }
    }

    fn pool_of(&self, p: Pool) -> Pool {
        if p == Pool::Le && self.shared {
            Pool::BrEdr
        } else {
            p
        }
    }

    fn free_mut(&mut self, p: Pool) -> &mut u16 {
        match self.pool_of(p) {
            Pool::Le => &mut self.le_free,
            Pool::BrEdr => &mut self.bredr_free,
        }
    }

    pub fn available(&self, p: Pool) -> u16 {
        match self.pool_of(p) {
            Pool::Le => self.le_free,
            Pool::BrEdr => self.bredr_free,
        }
    }

    /// Takes one credit for `handle`; false if the pool is empty.
    pub fn take(&mut self, handle: u16, p: Pool) -> bool {
        let f = self.free_mut(p);
        if *f == 0 {
            return false;
        }
        *f -= 1;
        self.outstanding.entry(handle).or_insert((p, 0)).1 += 1;
        true
    }

    /// Packets sent on `handle` that the controller has not completed.
    pub fn outstanding(&self, handle: u16) -> u16 {
        self.outstanding.get(&handle).map_or(0, |(_, n)| *n)
    }

    /// Number Of Completed Packets for one handle.
    pub fn completed(&mut self, handle: u16, n: u16) {
        let Some((p, out)) = self.outstanding.get_mut(&handle) else {
            if n > 0 {
                self.violations += 1;
            }
            return;
        };
        let back = if n > *out {
            self.violations += 1;
            *out
        } else {
            n
        };
        *out -= back;
        let p = *p;
        let total = match self.pool_of(p) {
            Pool::Le => self.le_total,
            Pool::BrEdr => self.bredr_total,
        };
        let f = self.free_mut(p);
        *f = (*f + back).min(total);
    }

    /// A connection ended: the controller flushes its packets, so its
    /// outstanding credits come back.
    pub fn disconnected(&mut self, handle: u16) {
        if let Some((p, out)) = self.outstanding.remove(&handle) {
            let total = match self.pool_of(p) {
                Pool::Le => self.le_total,
                Pool::BrEdr => self.bredr_total,
            };
            let f = self.free_mut(p);
            *f = (*f + out).min(total);
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReassemblyError {
    /// A continuing fragment with no PDU started (dropped).
    Orphan,
    /// The header announces more than [`MAX_PDU`], fragments run past it,
    /// or more bytes arrive than the header announced. Fatal for the link.
    TooLong,
}

/// One connection's reassembly buffer.
#[derive(Debug, Default)]
pub struct Reassembler {
    buf: Vec<u8>,
    active: bool,
}

/// A complete L2CAP PDU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pdu {
    pub cid: u16,
    pub payload: Vec<u8>,
}

/// What one fragment did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Pushed {
    pub pdu: Option<Pdu>,
    /// A first fragment arrived while a PDU was incomplete; the old one was
    /// dropped (as the spec says) and the new one started. A violation, not
    /// fatal.
    pub interrupted: bool,
}

impl Reassembler {
    /// Feeds one ACL payload. `first` is the packet boundary flag's "first"
    /// (either flushability).
    pub fn push(&mut self, first: bool, data: &[u8]) -> Result<Pushed, ReassemblyError> {
        let mut interrupted = false;
        if first {
            interrupted = self.active;
            self.buf.clear();
            self.active = true;
        } else if !self.active {
            return Err(ReassemblyError::Orphan);
        }
        if self.buf.len() + data.len() > MAX_PDU {
            self.reset();
            return Err(ReassemblyError::TooLong);
        }
        self.buf.extend_from_slice(data);
        self.check()?;
        Ok(Pushed { pdu: self.take_complete(), interrupted })
    }

    fn check(&mut self) -> Result<(), ReassemblyError> {
        if self.buf.len() > MAX_PDU {
            self.reset();
            return Err(ReassemblyError::TooLong);
        }
        if let Some(want) = self.wanted() {
            if self.buf.len() > want {
                // More bytes than the header announced: malformed.
                self.reset();
                return Err(ReassemblyError::TooLong);
            }
        }
        Ok(())
    }

    fn wanted(&self) -> Option<usize> {
        (self.buf.len() >= 4).then(|| 4 + u16::from_le_bytes([self.buf[0], self.buf[1]]) as usize)
    }

    fn take_complete(&mut self) -> Option<Pdu> {
        if self.wanted() == Some(self.buf.len()) {
            let cid = u16::from_le_bytes([self.buf[2], self.buf[3]]);
            let payload = self.buf.split_off(4);
            self.reset();
            Some(Pdu { cid, payload })
        } else {
            None
        }
    }

    pub fn reset(&mut self) {
        self.buf = Vec::new();
        self.active = false;
    }
}

/// Splits an L2CAP PDU (`cid`, `payload`) into ACL payloads of at most
/// `mtu` bytes. The first carries the basic header.
pub fn fragment(cid: u16, payload: &[u8], mtu: usize) -> Vec<Vec<u8>> {
    assert!(mtu > 0);
    let mut pdu = Vec::with_capacity(4 + payload.len());
    pdu.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    pdu.extend_from_slice(&cid.to_le_bytes());
    pdu.extend_from_slice(payload);
    pdu.chunks(mtu).map(|c| c.to_vec()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credits_never_exceed_the_pool() {
        let mut c = Credits::new(2, 3);
        assert!(c.take(1, Pool::Le));
        assert!(c.take(1, Pool::Le));
        assert_eq!(c.available(Pool::Le), 1);
        c.completed(1, 5); // controller lies
        assert_eq!(c.available(Pool::Le), 3);
        assert_eq!(c.violations, 1);
        c.completed(9, 1); // unknown handle
        assert_eq!(c.violations, 2);
        assert_eq!(c.available(Pool::BrEdr), 2);
    }

    #[test]
    fn shared_pool_when_no_le_buffers() {
        let mut c = Credits::new(1, 0);
        assert!(c.take(1, Pool::Le));
        assert!(!c.take(2, Pool::BrEdr));
        c.disconnected(1);
        assert_eq!(c.available(Pool::BrEdr), 1);
    }

    #[test]
    fn reassembles_split_pdu_including_split_header() {
        let frags = fragment(0x0004, &[1, 2, 3, 4, 5, 6, 7], 3);
        assert_eq!(frags.len(), 4);
        let mut r = Reassembler::default();
        let mut out = None;
        for (i, f) in frags.iter().enumerate() {
            out = r.push(i == 0, f).unwrap().pdu;
        }
        assert_eq!(out, Some(Pdu { cid: 4, payload: vec![1, 2, 3, 4, 5, 6, 7] }));
    }

    #[test]
    fn protocol_errors() {
        let mut r = Reassembler::default();
        assert_eq!(r.push(false, &[1, 2]), Err(ReassemblyError::Orphan));
        // header says 2 bytes, 3 arrive
        assert_eq!(r.push(true, &[2, 0, 4, 0, 1, 2, 3]), Err(ReassemblyError::TooLong));
        // a new start interrupts an incomplete PDU
        assert_eq!(r.push(true, &[4, 0, 4, 0, 1]), Ok(Pushed::default()));
        assert_eq!(r.push(true, &[1, 0, 4, 0]), Ok(Pushed { pdu: None, interrupted: true }));
        assert_eq!(r.push(false, &[9]).unwrap().pdu, Some(Pdu { cid: 4, payload: vec![9] }));
        // a complete PDU in the interrupting fragment is not lost
        r.push(true, &[4, 0, 4, 0]).unwrap();
        assert_eq!(r.push(true, &[1, 0, 5, 0, 7]), Ok(Pushed { pdu: Some(Pdu { cid: 5, payload: vec![7] }), interrupted: true }));
    }
}
