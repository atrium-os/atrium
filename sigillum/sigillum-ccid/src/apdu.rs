//! ISO/IEC 7816-4 APDUs (§5.1): command encoding in short or extended form,
//! and response parsing.

use crate::Error;

/// Longest command data (extended Lc).
pub const MAX_DATA: usize = 65_535;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub cla: u8,
    pub ins: u8,
    pub p1: u8,
    pub p2: u8,
    pub data: Vec<u8>,
    /// Expected response length: `None` (case 1/3), `Some(n)`; 256 (short)
    /// or 65536 (extended) mean "as much as there is".
    pub le: Option<u32>,
}

impl Command {
    pub fn new(cla: u8, ins: u8, p1: u8, p2: u8) -> Command {
        Command { cla, ins, p1, p2, data: Vec::new(), le: None }
    }

    pub fn data(mut self, d: &[u8]) -> Command {
        self.data = d.to_vec();
        self
    }

    pub fn le(mut self, le: u32) -> Command {
        self.le = Some(le);
        self
    }

    /// Whether the command needs extended length fields.
    pub fn extended(&self) -> bool {
        self.data.len() > 255 || self.le.is_some_and(|l| l > 256)
    }

    /// Encode, short form when it fits, else extended (if `allow_extended`).
    pub fn encode(&self, allow_extended: bool) -> Result<Vec<u8>, Error> {
        if self.data.len() > MAX_DATA || self.le.is_some_and(|l| l > 65_536) {
            return Err(Error::TooLong);
        }
        let mut out = vec![self.cla, self.ins, self.p1, self.p2];
        if !self.extended() {
            if !self.data.is_empty() {
                out.push(self.data.len() as u8);
                out.extend_from_slice(&self.data);
            }
            if let Some(le) = self.le {
                out.push(if le == 256 { 0 } else { le as u8 });
            }
            return Ok(out);
        }
        if !allow_extended {
            return Err(Error::TooLong);
        }
        if !self.data.is_empty() {
            out.push(0);
            out.extend_from_slice(&(self.data.len() as u16).to_be_bytes());
            out.extend_from_slice(&self.data);
        }
        if let Some(le) = self.le {
            if self.data.is_empty() {
                out.push(0);
            }
            out.extend_from_slice(&(if le == 65_536 { 0 } else { le as u16 }).to_be_bytes());
        }
        Ok(out)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub data: Vec<u8>,
    pub sw: u16,
}

impl Response {
    pub fn parse(b: &[u8]) -> Result<Response, Error> {
        if b.len() < 2 {
            return Err(Error::Protocol("response APDU"));
        }
        let (d, sw) = b.split_at(b.len() - 2);
        Ok(Response { data: d.to_vec(), sw: u16::from_be_bytes([sw[0], sw[1]]) })
    }

    pub fn ok(&self) -> bool {
        self.sw == 0x9000
    }

    pub fn sw1(&self) -> u8 {
        (self.sw >> 8) as u8
    }

    pub fn sw2(&self) -> u8 {
        self.sw as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cases() {
        // Case 1, 2, 3, 4 short.
        assert_eq!(Command::new(0, 0xA4, 0, 0).encode(false).unwrap(), [0, 0xA4, 0, 0]);
        assert_eq!(Command::new(0x80, 0xCA, 0x9F, 0x7F).le(256).encode(false).unwrap(), [0x80, 0xCA, 0x9F, 0x7F, 0]);
        assert_eq!(Command::new(0, 0xA4, 0, 0).data(&[0x3F, 0]).encode(false).unwrap(), [0, 0xA4, 0, 0, 2, 0x3F, 0]);
        assert_eq!(
            Command::new(0, 0xA4, 0, 0).data(&[0x3F, 0]).le(256).encode(false).unwrap(),
            [0, 0xA4, 0, 0, 2, 0x3F, 0, 0]
        );
        // Extended case 4 and 2.
        let big = vec![0xAB; 300];
        let e = Command::new(0, 0x2A, 0x9E, 0x9A).data(&big).le(65_536).encode(true).unwrap();
        assert_eq!(&e[..7], &[0, 0x2A, 0x9E, 0x9A, 0, 0x01, 0x2C]);
        assert_eq!(&e[e.len() - 2..], &[0, 0]);
        assert_eq!(e.len(), 4 + 3 + 300 + 2);
        assert_eq!(Command::new(0, 0xB0, 0, 0).le(1000).encode(true).unwrap(), [0, 0xB0, 0, 0, 0, 0x03, 0xE8]);
        assert_eq!(Command::new(0, 0xB0, 0, 0).le(1000).encode(false), Err(Error::TooLong));
        assert_eq!(Command::new(0, 0xB0, 0, 0).le(70_000).encode(true), Err(Error::TooLong));
    }

    #[test]
    fn responses() {
        let r = Response::parse(&[0x6F, 0x10, 0x90, 0x00]).unwrap();
        assert!(r.ok());
        assert_eq!(r.data, [0x6F, 0x10]);
        let r = Response::parse(&[0x61, 0x20]).unwrap();
        assert_eq!((r.sw1(), r.sw2()), (0x61, 0x20));
        assert!(Response::parse(&[0x90]).is_err());
    }
}
