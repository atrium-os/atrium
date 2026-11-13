//! One reader session over a bulk pipe: power the card on, exchange APDUs
//! at the reader's APDU level (short, or extended with CCID chaining), and
//! follow `61xx` / `6Cxx` the way ISO 7816-4 says. Bounded throughout: a
//! response is at most [`MAX_RESPONSE`] bytes, a reader may ask for at most
//! [`MAX_TIME_EXTENSIONS`] time extensions per command.

use crate::apdu::{Command, Response};
use crate::atr::Atr;
use crate::desc::{Ccid, Level};
use crate::msg::{self, chain, Reply};
use crate::Error;
use alloc::vec::Vec;

pub const MAX_RESPONSE: usize = 65_538;
pub const MAX_TIME_EXTENSIONS: u32 = 120;
/// GET RESPONSE rounds after a `61xx`.
const MAX_GET_RESPONSE: usize = 64;

/// The reader's bulk endpoints. `recv` returns one whole CCID message (the
/// driver reads USB packets until a short one), at most `cap` bytes.
pub trait Bulk {
    fn send(&mut self, m: &[u8]) -> Result<(), Error>;
    fn recv(&mut self, cap: usize) -> Result<Vec<u8>, Error>;
}

pub struct Reader<B: Bulk> {
    bulk: B,
    desc: Ccid,
    slot: u8,
    seq: u8,
    pub atr: Option<Atr>,
}

impl<B: Bulk> Reader<B> {
    /// Only APDU-level readers: TPDU and character level would need T=1
    /// block handling here, which no reader we've met requires.
    pub fn new(bulk: B, desc: Ccid) -> Result<Reader<B>, Error> {
        match desc.level() {
            Level::ShortApdu | Level::ExtendedApdu => {}
            _ => return Err(Error::Unsupported("reader exchange level below APDU")),
        }
        Ok(Reader { bulk, desc, slot: 0, seq: 0, atr: None })
    }

    pub fn into_inner(self) -> B {
        self.bulk
    }

    fn next_seq(&mut self) -> u8 {
        let s = self.seq;
        self.seq = self.seq.wrapping_add(1);
        s
    }

    /// Send one message and read its reply, waiting through time extensions.
    fn call(&mut self, m: &[u8], seq: u8, want: u8) -> Result<Reply, Error> {
        if m.len() > self.desc.message_cap() {
            return Err(Error::TooLong);
        }
        self.bulk.send(m)?;
        for _ in 0..=MAX_TIME_EXTENSIONS {
            let r = Reply::parse(&self.bulk.recv(self.desc.message_cap())?, self.desc.message_cap())?;
            if r.seq != seq || r.slot != self.slot {
                return Err(Error::Protocol("CCID reply out of sequence"));
            }
            match r.command() {
                msg::CMD_TIME_EXTENSION => continue,
                msg::CMD_FAILED if r.icc() == msg::ICC_ABSENT => return Err(Error::NoCard),
                msg::CMD_FAILED => return Err(Error::Slot(r.error)),
                msg::CMD_OK if r.kind == want => return Ok(r),
                _ => return Err(Error::Protocol("CCID reply")),
            }
        }
        Err(Error::Io)
    }

    pub fn slot_status(&mut self) -> Result<u8, Error> {
        let seq = self.next_seq();
        let m = msg::encode(msg::PC_TO_RDR_GET_SLOT_STATUS, self.slot, seq, [0; 3], &[]);
        self.bulk.send(&m)?;
        let r = Reply::parse(&self.bulk.recv(self.desc.message_cap())?, self.desc.message_cap())?;
        if r.seq != seq || r.kind != msg::RDR_TO_PC_SLOT_STATUS {
            return Err(Error::Protocol("CCID slot status"));
        }
        Ok(r.icc())
    }

    pub fn power_on(&mut self) -> Result<&Atr, Error> {
        let seq = self.next_seq();
        let m = msg::encode(msg::PC_TO_RDR_ICC_POWER_ON, self.slot, seq, [msg::VOLTAGE_AUTO, 0, 0], &[]);
        let r = self.call(&m, seq, msg::RDR_TO_PC_DATA_BLOCK)?;
        self.atr = Some(Atr::parse(&r.data)?);
        Ok(self.atr.as_ref().expect("just set"))
    }

    pub fn power_off(&mut self) -> Result<(), Error> {
        let seq = self.next_seq();
        let m = msg::encode(msg::PC_TO_RDR_ICC_POWER_OFF, self.slot, seq, [0; 3], &[]);
        self.call(&m, seq, msg::RDR_TO_PC_SLOT_STATUS).map(|_| ())
    }

    /// One APDU as bytes, one response as bytes (data and SW), with CCID
    /// chaining both ways when it doesn't fit one message.
    pub fn transmit_raw(&mut self, apdu: &[u8]) -> Result<Vec<u8>, Error> {
        let room = self.desc.message_cap() - msg::HEADER;
        let extended = self.desc.level() == Level::ExtendedApdu;
        if apdu.len() > room && !extended {
            return Err(Error::TooLong);
        }
        // Command: whole, or begins / continues / ends.
        let parts: Vec<&[u8]> = apdu.chunks(room).collect();
        let mut r = None;
        for (i, p) in parts.iter().enumerate() {
            let level = match (parts.len(), i) {
                (1, _) => chain::WHOLE,
                (_, 0) => chain::BEGINS,
                (n, i) if i == n - 1 => chain::ENDS,
                _ => chain::CONTINUES,
            };
            let seq = self.next_seq();
            let reply = self.call(&msg::xfr_block(self.slot, seq, level, p), seq, msg::RDR_TO_PC_DATA_BLOCK)?;
            if i + 1 < parts.len() && !reply.data.is_empty() {
                return Err(Error::Protocol("CCID data mid-chain"));
            }
            r = Some(reply);
        }
        let mut reply = r.ok_or(Error::Protocol("empty APDU"))?;
        // Response: 0 whole, 1 begins, 3 continues, 2 ends.
        let mut out = core::mem::take(&mut reply.data);
        let mut chained = reply.param;
        while matches!(chained, 1 | 3) {
            let seq = self.next_seq();
            let next = self.call(&msg::xfr_block(self.slot, seq, chain::NEXT, &[]), seq, msg::RDR_TO_PC_DATA_BLOCK)?;
            if out.len() + next.data.len() > MAX_RESPONSE {
                return Err(Error::TooLong);
            }
            out.extend_from_slice(&next.data);
            chained = next.param;
        }
        if chained != 0 && chained != 2 {
            return Err(Error::Protocol("CCID chain parameter"));
        }
        Ok(out)
    }

    /// A command, with `6Cxx` (resend with the right Le, once) and `61xx`
    /// (GET RESPONSE until done) followed.
    pub fn transmit(&mut self, c: &Command) -> Result<Response, Error> {
        let extended = self.desc.level() == Level::ExtendedApdu;
        let mut r = Response::parse(&self.transmit_raw(&c.encode(extended)?)?)?;
        if r.sw1() == 0x6C {
            let again = Command { le: Some(if r.sw2() == 0 { 256 } else { r.sw2() as u32 }), ..c.clone() };
            r = Response::parse(&self.transmit_raw(&again.encode(extended)?)?)?;
        }
        let mut data = core::mem::take(&mut r.data);
        let mut rounds = 0;
        while r.sw1() == 0x61 {
            rounds += 1;
            if rounds > MAX_GET_RESPONSE {
                return Err(Error::Protocol("endless GET RESPONSE"));
            }
            let le = if r.sw2() == 0 { 256 } else { r.sw2() as u32 };
            let get = Command::new(c.cla & 0x03, 0xC0, 0, 0).le(le);
            r = Response::parse(&self.transmit_raw(&get.encode(false)?)?)?;
            if data.len() + r.data.len() > MAX_RESPONSE {
                return Err(Error::TooLong);
            }
            data.append(&mut r.data);
        }
        Ok(Response { data, sw: r.sw })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::fake::{desc, innait_desc, Fake};
    use alloc::vec;

    #[test]
    fn power_on_and_select() {
        let mut r = Reader::new(
            Fake::new(|a: &[u8]| if a == [0, 0xA4, 0, 0, 2, 0x3F, 0, 0] { vec![0x6F, 0x10, 0x90, 0x00] } else { vec![0x6A, 0x82] }),
            innait_desc(),
        )
        .unwrap();
        assert_eq!(r.power_on().unwrap().historical, b"PK\x03XXX_DSC_KEY");
        let ok = r.transmit(&Command::new(0, 0xA4, 0, 0).data(&[0x3F, 0]).le(256)).unwrap();
        assert_eq!((ok.data, ok.sw), (vec![0x6F, 0x10], 0x9000));
        assert_eq!(r.transmit(&Command::new(0, 0xA4, 4, 0).data(&[0xA0, 0, 0, 3, 8])).unwrap().sw, 0x6A82);
        r.power_off().unwrap();
    }

    #[test]
    fn get_response_and_wrong_le() {
        let mut left: Vec<u8> = (0..600u32).map(|i| i as u8).collect();
        let mut r = Reader::new(
            Fake::new(move |a: &[u8]| match a[1] {
                0xCB if a.len() == 5 && a[4] != 0 => vec![0x6C, 0x00],
                0xCB => vec![0x61, 0x00],
                0xC0 => {
                    let n = left.len().min(256);
                    let mut v: Vec<u8> = left.drain(..n).collect();
                    v.extend_from_slice(if left.is_empty() { &[0x90, 0x00] } else { &[0x61, 0x00] });
                    v
                }
                _ => vec![0x6D, 0x00],
            }),
            innait_desc(),
        )
        .unwrap();
        r.power_on().unwrap();
        let resp = r.transmit(&Command::new(0, 0xCB, 0x3F, 0xFF).le(5)).unwrap();
        assert_eq!(resp.sw, 0x9000);
        assert_eq!(resp.data.len(), 600);
        assert_eq!(resp.data[599], (599u32) as u8);
    }

    #[test]
    fn ccid_chaining_both_ways_and_time_extensions() {
        let mut f = Fake::new(|a: &[u8]| {
            let mut v = vec![0x5A; a.len() * 2];
            v.extend_from_slice(&[0x90, 0x00]);
            v
        });
        f.chunk = 400;
        f.extensions = 3;
        let mut r = Reader::new(f, desc(0x0004_04be, 512)).unwrap();
        r.power_on().unwrap();
        let big = vec![0x11; 1500];
        let resp = r.transmit(&Command::new(0, 0x2A, 0x9E, 0x9A).data(&big).le(65_536)).unwrap();
        assert_eq!(resp.sw, 0x9000);
        assert_eq!(resp.data.len(), (1500 + 7 + 2) * 2);
        // 1509-byte APDU in 502-byte parts: four XfrBlocks, then a NEXT per part of the response.
        let f = r.into_inner();
        let xfr: Vec<_> = f.sent.iter().filter(|m| m[0] == msg::PC_TO_RDR_XFR_BLOCK).collect();
        assert_eq!(xfr.iter().map(|m| u16::from_le_bytes([m[8], m[9]])).collect::<Vec<_>>()[..4], [1, 3, 3, 2]);
    }

    #[test]
    fn refusals() {
        // TPDU-level readers aren't driven here.
        assert!(matches!(Reader::new(Fake::new(|_: &[u8]| vec![]), desc(0x0001_00be, 512)), Err(Error::Unsupported(_))));
        // Short APDU level can't send an extended APDU.
        let mut r = Reader::new(Fake::new(|_: &[u8]| vec![0x90, 0]), desc(0x0002_00be, 512)).unwrap();
        r.power_on().unwrap();
        assert_eq!(r.transmit(&Command::new(0, 0xB0, 0, 0).le(1000)), Err(Error::TooLong));
        // No card.
        let mut f = Fake::new(|_: &[u8]| vec![]);
        f.present = false;
        let mut r = Reader::new(f, innait_desc()).unwrap();
        assert!(matches!(r.power_on(), Err(Error::NoCard)));
        // Too many time extensions.
        let mut f = Fake::new(|_: &[u8]| vec![0x90, 0]);
        f.extensions = MAX_TIME_EXTENSIONS + 5;
        let mut r = Reader::new(f, innait_desc()).unwrap();
        assert_eq!(r.power_on().err(), Some(Error::Io));
    }

    #[test]
    fn out_of_sequence_replies_end_the_session() {
        struct Liar;
        impl Bulk for Liar {
            fn send(&mut self, _: &[u8]) -> Result<(), Error> {
                Ok(())
            }
            fn recv(&mut self, _: usize) -> Result<Vec<u8>, Error> {
                Ok(vec![0x80, 2, 0, 0, 0, 0, 99, 0, 0, 0, 0x90, 0])
            }
        }
        let mut r = Reader::new(Liar, innait_desc()).unwrap();
        assert_eq!(r.power_on().err(), Some(Error::Protocol("CCID reply out of sequence")));
    }
}
