//! GAP advertising data (AD structures), parsed by the generated
//! `pdl/gap.pdl` grammar.

#[allow(warnings, clippy::all)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/gap.rs"));
}

use generated::AdvertisingData;
use pdl_runtime::{DecodeError, Packet};

/// AD type assigned numbers used by Vinculum.
pub mod ad {
    pub const FLAGS: u8 = 0x01;
    pub const INCOMPLETE_16BIT_UUIDS: u8 = 0x02;
    pub const COMPLETE_16BIT_UUIDS: u8 = 0x03;
    pub const INCOMPLETE_128BIT_UUIDS: u8 = 0x06;
    pub const COMPLETE_128BIT_UUIDS: u8 = 0x07;
    pub const SHORTENED_LOCAL_NAME: u8 = 0x08;
    pub const COMPLETE_LOCAL_NAME: u8 = 0x09;
    pub const TX_POWER_LEVEL: u8 = 0x0A;
    pub const CLASS_OF_DEVICE: u8 = 0x0D;
    pub const SERVICE_DATA_16BIT: u8 = 0x16;
    pub const APPEARANCE: u8 = 0x19;
    pub const MANUFACTURER_SPECIFIC: u8 = 0xFF;
}

/// One AD structure: its type and data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdStructure {
    pub ad_type: u8,
    pub data: Vec<u8>,
}

/// Parses an advertising (or EIR, scan response) payload into its AD
/// structures. A zero length byte at a structure boundary starts padding,
/// which the spec allows, and ends the list.
pub fn parse(payload: &[u8]) -> Result<Vec<AdStructure>, DecodeError> {
    let end = padding_start(payload);
    let parsed = AdvertisingData::decode_full(&payload[..end])?;
    Ok(parsed
        .structures
        .into_iter()
        .map(|s| {
            // padding_start guarantees every length byte before `end` is >= 1.
            let mut body = s.body;
            let ad_type = body.remove(0);
            AdStructure { ad_type, data: body }
        })
        .collect())
}

/// The first zero length byte at a structure boundary starts padding.
fn padding_start(payload: &[u8]) -> usize {
    let mut i = 0;
    while i < payload.len() {
        let len = payload[i] as usize;
        if len == 0 {
            return i;
        }
        i += 1 + len;
    }
    payload.len()
}

/// The device's name: complete if present, else shortened. Invalid UTF-8 is
/// replaced, never trusted.
pub fn local_name(structures: &[AdStructure]) -> Option<String> {
    let pick = |t| structures.iter().find(|s| s.ad_type == t);
    pick(ad::COMPLETE_LOCAL_NAME)
        .or_else(|| pick(ad::SHORTENED_LOCAL_NAME))
        .map(|s| String::from_utf8_lossy(&s.data).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_and_flags() {
        // flags 0x06, complete name "Kb"
        let p = [2, ad::FLAGS, 0x06, 3, ad::COMPLETE_LOCAL_NAME, b'K', b'b'];
        let s = parse(&p).unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].data, vec![0x06]);
        assert_eq!(local_name(&s).as_deref(), Some("Kb"));
    }

    #[test]
    fn padding_and_garbage() {
        let p = [2, ad::FLAGS, 0x06, 0, 0, 0];
        assert_eq!(parse(&p).unwrap().len(), 1);
        // length runs past the end
        assert!(parse(&[5, ad::COMPLETE_LOCAL_NAME, b'x']).is_err());
        assert!(parse(&[]).unwrap().is_empty());
    }
}
