//! The CCID class functional descriptor (CCID 1.1 §5.1, type 0x21), as
//! found among the interface's descriptors.

use crate::Error;

pub const DESCRIPTOR_TYPE: u8 = 0x21;
pub const LEN: usize = 0x36;

/// `dwFeatures` exchange levels (bits 16..19).
pub const LEVEL_MASK: u32 = 0x0007_0000;
pub const LEVEL_TPDU: u32 = 0x0001_0000;
pub const LEVEL_SHORT_APDU: u32 = 0x0002_0000;
pub const LEVEL_EXTENDED_APDU: u32 = 0x0004_0000;
/// Automatic parameter negotiation by the reader.
pub const AUTO_PARAMETERS: u32 = 0x0000_0002;
/// Automatic PPS made by the reader.
pub const AUTO_PPS: u32 = 0x0000_0080;
/// Automatic IFSD exchange as the first T=1 block.
pub const AUTO_IFSD: u32 = 0x0000_0400;

/// Exchange level the reader offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Character level: the host would speak T=0/T=1 itself.
    Character,
    /// TPDU: the host builds T=1 blocks.
    Tpdu,
    /// Short APDUs: the reader does T=0/T=1 itself.
    ShortApdu,
    /// Short and extended APDUs.
    ExtendedApdu,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ccid {
    pub bcd_ccid: u16,
    pub max_slot_index: u8,
    pub voltage_support: u8,
    pub protocols: u32,
    pub max_ifsd: u32,
    pub features: u32,
    pub max_message_length: u32,
    pub pin_support: u8,
    pub max_busy_slots: u8,
}

impl Ccid {
    /// Parse the 0x36-byte descriptor (`bLength`, `bDescriptorType`, …).
    pub fn parse(d: &[u8]) -> Result<Ccid, Error> {
        if d.len() < LEN || d[0] as usize != LEN || d[1] != DESCRIPTOR_TYPE {
            return Err(Error::Protocol("CCID descriptor"));
        }
        let u16le = |i: usize| u16::from_le_bytes([d[i], d[i + 1]]);
        let u32le = |i: usize| u32::from_le_bytes([d[i], d[i + 1], d[i + 2], d[i + 3]]);
        Ok(Ccid {
            bcd_ccid: u16le(2),
            max_slot_index: d[4],
            voltage_support: d[5],
            protocols: u32le(6),
            max_ifsd: u32le(28),
            features: u32le(40),
            max_message_length: u32le(44),
            pin_support: d[52],
            max_busy_slots: d[53],
        })
    }

    pub fn level(&self) -> Level {
        match self.features & LEVEL_MASK {
            LEVEL_EXTENDED_APDU => Level::ExtendedApdu,
            LEVEL_SHORT_APDU => Level::ShortApdu,
            LEVEL_TPDU => Level::Tpdu,
            _ => Level::Character,
        }
    }

    /// T=1 is offered (bit 1 of dwProtocols).
    pub fn t1(&self) -> bool {
        self.protocols & 2 != 0
    }

    /// The largest bulk message to send or accept, capped: a reader can't
    /// make this driver allocate more than 64 KiB per message.
    pub fn message_cap(&self) -> usize {
        (self.max_message_length as usize).clamp(msg::HEADER + 1, 65_536)
    }
}

use crate::msg;

#[cfg(test)]
mod tests {
    use super::*;

    /// The InnaIT DSC token (338c:0032), read on a Mac 2026-11-13.
    pub const INNAIT: [u8; 54] = [
        0x36, 0x21, 0x10, 0x01, 0x00, 0x07, 0x03, 0x00, 0x00, 0x00, 0x65, 0x0e, 0x00, 0x00, 0xf0, 0x37, 0x00, 0x00, 0x00,
        0xb5, 0x26, 0x00, 0x00, 0x80, 0xf0, 0x0c, 0x00, 0x00, 0xfe, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0xbe, 0x04, 0x04, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
    ];

    #[test]
    fn innait() {
        let c = Ccid::parse(&INNAIT).unwrap();
        assert_eq!(c.bcd_ccid, 0x0110);
        assert_eq!(c.max_slot_index, 0);
        assert_eq!(c.protocols, 3);
        assert!(c.t1());
        assert_eq!(c.max_ifsd, 254);
        assert_eq!(c.features, 0x0004_04be);
        assert_eq!(c.level(), Level::ExtendedApdu);
        assert_eq!(c.max_message_length, 4096);
        assert_eq!(c.message_cap(), 4096);
        assert_eq!(c.pin_support, 0);
        assert_eq!(c.max_busy_slots, 1);
    }

    #[test]
    fn short_or_wrong() {
        assert!(Ccid::parse(&INNAIT[..53]).is_err());
        let mut d = INNAIT;
        d[1] = 0x24;
        assert!(Ccid::parse(&d).is_err());
        let mut d = INNAIT;
        d[44..48].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(Ccid::parse(&d).unwrap().message_cap(), 65_536);
    }
}
