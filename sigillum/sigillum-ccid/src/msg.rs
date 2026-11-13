//! CCID bulk messages (CCID 1.1 §6.1, §6.2): a 10-byte header and data.
//! Only what an APDU-level reader needs: power on/off, slot status and
//! XfrBlock, and their replies.

use crate::Error;

pub const HEADER: usize = 10;

pub const PC_TO_RDR_ICC_POWER_ON: u8 = 0x62;
pub const PC_TO_RDR_ICC_POWER_OFF: u8 = 0x63;
pub const PC_TO_RDR_GET_SLOT_STATUS: u8 = 0x65;
pub const PC_TO_RDR_XFR_BLOCK: u8 = 0x6F;

pub const RDR_TO_PC_DATA_BLOCK: u8 = 0x80;
pub const RDR_TO_PC_SLOT_STATUS: u8 = 0x81;

/// bmICCStatus (bits 0..1 of bStatus).
pub const ICC_ACTIVE: u8 = 0;
pub const ICC_INACTIVE: u8 = 1;
pub const ICC_ABSENT: u8 = 2;
/// bmCommandStatus (bits 6..7 of bStatus).
pub const CMD_OK: u8 = 0;
pub const CMD_FAILED: u8 = 1;
pub const CMD_TIME_EXTENSION: u8 = 2;

/// Power-on voltage: automatic.
pub const VOLTAGE_AUTO: u8 = 0;

/// `wLevelParameter` for XfrBlock at extended APDU level (§6.1.4).
pub mod chain {
    /// The whole APDU in this one message.
    pub const WHOLE: u16 = 0x00;
    pub const BEGINS: u16 = 0x01;
    pub const ENDS: u16 = 0x02;
    pub const CONTINUES: u16 = 0x03;
    /// An empty XfrBlock asking for the next part of a response.
    pub const NEXT: u16 = 0x10;
}

/// A host-to-reader message.
pub fn encode(kind: u8, slot: u8, seq: u8, params: [u8; 3], data: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(HEADER + data.len());
    m.push(kind);
    m.extend_from_slice(&(data.len() as u32).to_le_bytes());
    m.push(slot);
    m.push(seq);
    m.extend_from_slice(&params);
    m.extend_from_slice(data);
    m
}

pub fn xfr_block(slot: u8, seq: u8, level: u16, data: &[u8]) -> Vec<u8> {
    let l = level.to_le_bytes();
    // bBWI 0: the reader's default block waiting time.
    encode(PC_TO_RDR_XFR_BLOCK, slot, seq, [0, l[0], l[1]], data)
}

/// A reader-to-host message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub kind: u8,
    pub slot: u8,
    pub seq: u8,
    pub status: u8,
    pub error: u8,
    /// bChainParameter for a DataBlock; bClockStatus for SlotStatus.
    pub param: u8,
    pub data: Vec<u8>,
}

impl Reply {
    /// Parse one complete message of at most `cap` bytes.
    pub fn parse(b: &[u8], cap: usize) -> Result<Reply, Error> {
        if b.len() < HEADER || b.len() > cap {
            return Err(Error::Protocol("CCID message length"));
        }
        let n = u32::from_le_bytes([b[1], b[2], b[3], b[4]]) as usize;
        if n != b.len() - HEADER {
            return Err(Error::Protocol("CCID dwLength"));
        }
        if !matches!(b[0], RDR_TO_PC_DATA_BLOCK | RDR_TO_PC_SLOT_STATUS) {
            return Err(Error::Protocol("CCID message type"));
        }
        Ok(Reply { kind: b[0], slot: b[5], seq: b[6], status: b[7], error: b[8], param: b[9], data: b[HEADER..].to_vec() })
    }

    pub fn icc(&self) -> u8 {
        self.status & 0x03
    }

    pub fn command(&self) -> u8 {
        self.status >> 6
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let m = xfr_block(0, 7, chain::WHOLE, &[0, 0xA4, 0, 0]);
        assert_eq!(m, [0x6F, 4, 0, 0, 0, 0, 7, 0, 0, 0, 0, 0xA4, 0, 0]);
        let r = Reply::parse(&[0x80, 2, 0, 0, 0, 0, 7, 0, 0, 0, 0x90, 0x00], 64).unwrap();
        assert_eq!((r.kind, r.seq, r.icc(), r.command()), (RDR_TO_PC_DATA_BLOCK, 7, ICC_ACTIVE, CMD_OK));
        assert_eq!(r.data, [0x90, 0x00]);
    }

    #[test]
    fn bad_lengths_and_types() {
        assert!(Reply::parse(&[0x80, 0, 0, 0, 0, 0, 0, 0, 0], 64).is_err());
        assert!(Reply::parse(&[0x80, 3, 0, 0, 0, 0, 0, 0, 0, 0, 1, 2], 64).is_err());
        assert!(Reply::parse(&[0x83, 0, 0, 0, 0, 0, 0, 0, 0, 0], 64).is_err());
        assert!(Reply::parse(&[0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0], 9).is_err());
        let r = Reply::parse(&[0x81, 0, 0, 0, 0, 0, 1, 0x42, 0xFE, 0], 64).unwrap();
        assert_eq!((r.icc(), r.command(), r.error), (ICC_ABSENT, CMD_FAILED, 0xFE));
    }
}
