//! An in-memory CCID reader with one card, for tests here and in crates
//! that drive a [`Reader`](crate::reader::Reader) (feature `fake`).
use crate::desc::Ccid;
use crate::msg::{self, chain};
use crate::reader::Bulk;
use crate::Error;
use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;

/// A reader with one card: answers XfrBlocks with `card(apdu)`, sends
/// responses in parts of `chunk` bytes (CCID chaining), and optionally
/// asks for time extensions first.
pub struct Fake<F: FnMut(&[u8]) -> Vec<u8>> {
    pub card: F,
    pub chunk: usize,
    pub extensions: u32,
    pub present: bool,
    pending: VecDeque<Vec<u8>>,
    cmd: Vec<u8>,
    out: Vec<u8>,
    pub sent: Vec<Vec<u8>>,
}

impl<F: FnMut(&[u8]) -> Vec<u8>> Fake<F> {
    pub fn new(card: F) -> Self {
        Fake { card, chunk: 4086, extensions: 0, present: true, pending: VecDeque::new(), cmd: vec![], out: vec![], sent: vec![] }
    }
}

pub fn reply(kind: u8, seq: u8, status: u8, param: u8, data: &[u8]) -> Vec<u8> {
    let mut m = vec![kind];
    m.extend_from_slice(&(data.len() as u32).to_le_bytes());
    m.extend_from_slice(&[0, seq, status, 0, param]);
    m.extend_from_slice(data);
    m
}

impl<F: FnMut(&[u8]) -> Vec<u8>> Bulk for Fake<F> {
    fn send(&mut self, m: &[u8]) -> Result<(), Error> {
        self.sent.push(m.to_vec());
        let (kind, seq, level) = (m[0], m[6], u16::from_le_bytes([m[8], m[9]]));
        for _ in 0..self.extensions {
            self.pending.push_back(reply(msg::RDR_TO_PC_DATA_BLOCK, seq, msg::CMD_TIME_EXTENSION << 6, 1, &[]));
        }
        if !self.present {
            self.pending.push_back(reply(msg::RDR_TO_PC_SLOT_STATUS, seq, (msg::CMD_FAILED << 6) | msg::ICC_ABSENT, 0, &[]));
            return Ok(());
        }
        let atr = [0x3B, 0x8E, 0x80, 0x01, 0x50, 0x4B, 0x03, 0x58, 0x58, 0x58, 0x5F, 0x44, 0x53, 0x43, 0x5F, 0x4B, 0x45, 0x59, 0x4C];
        let r = match kind {
            msg::PC_TO_RDR_ICC_POWER_ON => reply(msg::RDR_TO_PC_DATA_BLOCK, seq, 0, 0, &atr),
            msg::PC_TO_RDR_ICC_POWER_OFF | msg::PC_TO_RDR_GET_SLOT_STATUS => reply(msg::RDR_TO_PC_SLOT_STATUS, seq, 0, 0, &[]),
            msg::PC_TO_RDR_XFR_BLOCK => {
                match level {
                    chain::WHOLE | chain::ENDS => {
                        self.cmd.extend_from_slice(&m[10..]);
                        let apdu = core::mem::take(&mut self.cmd);
                        self.out = (self.card)(&apdu);
                    }
                    chain::BEGINS | chain::CONTINUES => {
                        self.cmd.extend_from_slice(&m[10..]);
                        self.pending.push_back(reply(msg::RDR_TO_PC_DATA_BLOCK, seq, 0, 0x10, &[]));
                        return Ok(());
                    }
                    chain::NEXT => {}
                    _ => panic!("level {level}"),
                }
                let first = level != chain::NEXT;
                let n = self.out.len().min(self.chunk);
                let part: Vec<u8> = self.out.drain(..n).collect();
                let param = match (first, self.out.is_empty()) {
                    (true, true) => 0,
                    (true, false) => 1,
                    (false, true) => 2,
                    (false, false) => 3,
                };
                reply(msg::RDR_TO_PC_DATA_BLOCK, seq, 0, param, &part)
            }
            _ => panic!("message {kind:#x}"),
        };
        self.pending.push_back(r);
        Ok(())
    }

    fn recv(&mut self, _cap: usize) -> Result<Vec<u8>, Error> {
        self.pending.pop_front().ok_or(Error::Io)
    }
}


/// A descriptor with these `dwFeatures` and `dwMaxCCIDMessageLength`.
pub fn desc(features: u32, max: u32) -> Ccid {
    let mut d = [0u8; 54];
    d[0] = 0x36;
    d[1] = 0x21;
    d[6] = 3;
    d[40..44].copy_from_slice(&features.to_le_bytes());
    d[44..48].copy_from_slice(&max.to_le_bytes());
    Ccid::parse(&d).unwrap()
}

/// The InnaIT DSC token's exchange level and message size.
pub fn innait_desc() -> Ccid {
    desc(0x0004_04be, 4096)
}
