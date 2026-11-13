//! CTAPHID (CTAP 2.1 §11.2): messages over 64-byte HID reports.
//!
//! An initialisation packet is `cid[4] | cmd | 0x80 | bcnt[2 BE] | data[57]`,
//! a continuation packet `cid[4] | seq (0..=127) | data[59]`, so a message
//! is at most 57 + 128 × 59 = 7609 bytes. Channel `0xffffffff` is the
//! broadcast channel, used only for `INIT`.

use crate::Error;

pub const REPORT: usize = 64;
pub const INIT_DATA: usize = REPORT - 7;
pub const CONT_DATA: usize = REPORT - 5;
pub const MAX_MESSAGE: usize = INIT_DATA + 128 * CONT_DATA;
pub const BROADCAST: u32 = 0xffff_ffff;

pub mod cmd {
    pub const PING: u8 = 0x01;
    pub const MSG: u8 = 0x03;
    pub const LOCK: u8 = 0x04;
    pub const INIT: u8 = 0x06;
    pub const WINK: u8 = 0x08;
    pub const CBOR: u8 = 0x10;
    pub const CANCEL: u8 = 0x11;
    pub const KEEPALIVE: u8 = 0x3b;
    pub const ERROR: u8 = 0x3f;
}

/// `ERROR` codes (CTAP 2.1 §11.2.9.1.6).
pub mod err {
    pub const INVALID_CMD: u8 = 0x01;
    pub const INVALID_PAR: u8 = 0x02;
    pub const INVALID_LEN: u8 = 0x03;
    pub const INVALID_SEQ: u8 = 0x04;
    pub const MSG_TIMEOUT: u8 = 0x05;
    pub const CHANNEL_BUSY: u8 = 0x06;
    pub const INVALID_CHANNEL: u8 = 0x0b;
    pub const OTHER: u8 = 0x7f;
}

/// `KEEPALIVE` status bytes.
pub mod keepalive {
    pub const PROCESSING: u8 = 1;
    pub const UP_NEEDED: u8 = 2;
}

/// `INIT` capability flags.
pub mod caps {
    pub const WINK: u8 = 0x01;
    pub const CBOR: u8 = 0x04;
    pub const NMSG: u8 = 0x08;
}

/// A device that sends and receives 64-byte reports. The driver's hidraw
/// fd implements it; tests use the soft authenticator.
pub trait HidDevice {
    fn write_report(&mut self, report: &[u8; REPORT]) -> Result<(), Error>;
    /// The next report, or `Err(Timeout)` after `timeout_ms`.
    fn read_report(&mut self, timeout_ms: u32) -> Result<[u8; REPORT], Error>;
}

/// Split a message into reports.
pub fn frame(cid: u32, command: u8, payload: &[u8]) -> Result<Vec<[u8; REPORT]>, Error> {
    if payload.len() > MAX_MESSAGE {
        return Err(Error::Request("message too large for CTAPHID"));
    }
    let mut out = Vec::new();
    let mut p = [0u8; REPORT];
    p[..4].copy_from_slice(&cid.to_be_bytes());
    p[4] = 0x80 | command;
    p[5..7].copy_from_slice(&(payload.len() as u16).to_be_bytes());
    let first = payload.len().min(INIT_DATA);
    p[7..7 + first].copy_from_slice(&payload[..first]);
    out.push(p);
    for (seq, chunk) in payload[first..].chunks(CONT_DATA).enumerate() {
        let mut p = [0u8; REPORT];
        p[..4].copy_from_slice(&cid.to_be_bytes());
        p[4] = seq as u8;
        p[5..5 + chunk.len()].copy_from_slice(chunk);
        out.push(p);
    }
    Ok(out)
}

/// What one report did to a message being reassembled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Not ours (another channel) or a stray continuation: ignore it.
    Ignored,
    /// More packets to come.
    Partial,
    /// The device is working: [`keepalive`] status.
    Keepalive(u8),
    /// A complete message: (command, payload).
    Message(u8, Vec<u8>),
}

/// Reassembles one response on one channel.
#[derive(Debug)]
pub struct Reassembler {
    cid: u32,
    current: Option<(u8, usize, u8, Vec<u8>)>, // cmd, total, next seq, data
}

impl Reassembler {
    pub fn new(cid: u32) -> Reassembler {
        Reassembler { cid, current: None }
    }

    pub fn feed(&mut self, p: &[u8; REPORT]) -> Result<Event, Error> {
        let cid = u32::from_be_bytes(p[..4].try_into().unwrap());
        if cid != self.cid {
            return Ok(Event::Ignored);
        }
        if p[4] & 0x80 != 0 {
            let command = p[4] & 0x7f;
            let total = u16::from_be_bytes([p[5], p[6]]) as usize;
            if total > MAX_MESSAGE {
                return Err(Error::Protocol("CTAPHID length over the maximum"));
            }
            if command == cmd::KEEPALIVE {
                if total < 1 {
                    return Err(Error::Protocol("empty keepalive"));
                }
                return Ok(Event::Keepalive(p[7]));
            }
            if command == cmd::ERROR {
                return Err(Error::Hid(if total >= 1 { p[7] } else { err::OTHER }));
            }
            if self.current.is_some() {
                return Err(Error::Protocol("new message before the last one ended"));
            }
            let n = total.min(INIT_DATA);
            let mut data = Vec::with_capacity(total);
            data.extend_from_slice(&p[7..7 + n]);
            if data.len() == total {
                return Ok(Event::Message(command, data));
            }
            self.current = Some((command, total, 0, data));
            Ok(Event::Partial)
        } else {
            let Some((command, total, seq, mut data)) = self.current.take() else {
                return Ok(Event::Ignored);
            };
            if p[4] != seq {
                return Err(Error::Protocol("CTAPHID sequence out of order"));
            }
            let n = (total - data.len()).min(CONT_DATA);
            data.extend_from_slice(&p[5..5 + n]);
            if data.len() == total {
                return Ok(Event::Message(command, data));
            }
            if seq == 127 {
                return Err(Error::Protocol("CTAPHID sequence overflow"));
            }
            self.current = Some((command, total, seq + 1, data));
            Ok(Event::Partial)
        }
    }
}

/// The `INIT` response (CTAP 2.1 §11.2.9.1.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitResponse {
    pub nonce: [u8; 8],
    pub cid: u32,
    pub protocol: u8,
    pub version: (u8, u8, u8),
    pub caps: u8,
}

impl InitResponse {
    pub fn parse(b: &[u8]) -> Result<InitResponse, Error> {
        if b.len() < 17 {
            return Err(Error::Protocol("short INIT response"));
        }
        let cid = u32::from_be_bytes(b[8..12].try_into().unwrap());
        if cid == BROADCAST || cid == 0 {
            return Err(Error::Protocol("INIT assigned a reserved channel"));
        }
        Ok(InitResponse {
            nonce: b[..8].try_into().unwrap(),
            cid,
            protocol: b[12],
            version: (b[13], b[14], b[15]),
            caps: b[16],
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut v = self.nonce.to_vec();
        v.extend_from_slice(&self.cid.to_be_bytes());
        v.extend_from_slice(&[self.protocol, self.version.0, self.version.1, self.version.2, self.caps]);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_sizes() {
        for n in [0, 1, 57, 58, 116, 117, 1000, MAX_MESSAGE] {
            let payload: Vec<u8> = (0..n).map(|i| i as u8).collect();
            let packets = frame(7, cmd::CBOR, &payload).unwrap();
            let mut r = Reassembler::new(7);
            let mut got = None;
            for p in &packets {
                match r.feed(p).unwrap() {
                    Event::Message(c, d) => got = Some((c, d)),
                    Event::Partial => {}
                    e => panic!("{e:?}"),
                }
            }
            assert_eq!(got, Some((cmd::CBOR, payload)), "{n}");
        }
        assert!(frame(7, cmd::CBOR, &vec![0; MAX_MESSAGE + 1]).is_err());
    }

    #[test]
    fn other_channels_and_order() {
        let packets = frame(7, cmd::CBOR, &[1u8; 200]).unwrap();
        let mut r = Reassembler::new(7);
        let foreign = frame(8, cmd::CBOR, &[0u8; 3]).unwrap();
        assert_eq!(r.feed(&packets[0]).unwrap(), Event::Partial);
        assert_eq!(r.feed(&foreign[0]).unwrap(), Event::Ignored);
        assert!(r.feed(&packets[2]).is_err()); // seq 1 before seq 0
    }

    #[test]
    fn keepalive_and_error() {
        let ka = frame(7, cmd::KEEPALIVE, &[keepalive::UP_NEEDED]).unwrap();
        let mut r = Reassembler::new(7);
        assert_eq!(r.feed(&ka[0]).unwrap(), Event::Keepalive(keepalive::UP_NEEDED));
        let e = frame(7, cmd::ERROR, &[err::CHANNEL_BUSY]).unwrap();
        assert_eq!(r.feed(&e[0]), Err(Error::Hid(err::CHANNEL_BUSY)));
        let mut lie = frame(7, cmd::CBOR, &[]).unwrap()[0];
        lie[5..7].copy_from_slice(&0xffffu16.to_be_bytes());
        assert!(r.feed(&lie).is_err());
    }
}
