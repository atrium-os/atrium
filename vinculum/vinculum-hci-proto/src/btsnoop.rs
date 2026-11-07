//! btsnoop trace files (datalink 1002, "HCI UART (H4)"), the format
//! Wireshark and Android's tools read.
//!
//! Used for recorded-trace tests (a trace from Rootcanal + Bumble is replayed
//! through the parsers on any host, no controller needed) and for the dev-only
//! HCI log. The writer zeroes key material before it reaches the file
//! (spec §7: HCI logs never carry keys).

use crate::h4::{Frame, Kind};
use std::io::{self, Read, Write};

const MAGIC: &[u8; 8] = b"btsnoop\0";
const VERSION: u32 = 1;
const DATALINK_H4: u32 = 1002;
/// Microseconds from 0000-01-01 to 1970-01-01, as btsnoop counts time.
const EPOCH_DELTA_US: i64 = 0x00dc_ddb3_0f2f_8000;
/// A record larger than this is refused by the reader (an H4 frame is at most
/// 1 + 4 + 64 KiB even with an unbounded ACL length field).
const MAX_RECORD: u32 = 70_000;

/// Which way a packet went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    HostToController,
    ControllerToHost,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub direction: Direction,
    /// Microseconds since the Unix epoch.
    pub unix_us: i64,
    pub frame: Frame,
}

pub struct Writer<W: Write> {
    out: W,
}

impl<W: Write> Writer<W> {
    pub fn new(mut out: W) -> io::Result<Writer<W>> {
        out.write_all(MAGIC)?;
        out.write_all(&VERSION.to_be_bytes())?;
        out.write_all(&DATALINK_H4.to_be_bytes())?;
        Ok(Writer { out })
    }

    /// Writes one record, with key material zeroed (see [`redact`]).
    pub fn write(&mut self, r: &Record) -> io::Result<()> {
        let mut bytes = r.frame.to_bytes();
        redact(&mut bytes);
        let len = bytes.len() as u32;
        let mut flags = match r.direction {
            Direction::HostToController => 0,
            Direction::ControllerToHost => 1,
        };
        if matches!(r.frame.kind, Kind::Command | Kind::Event) {
            flags |= 2;
        }
        self.out.write_all(&len.to_be_bytes())?; // original length
        self.out.write_all(&len.to_be_bytes())?; // included length
        self.out.write_all(&(flags as u32).to_be_bytes())?;
        self.out.write_all(&0u32.to_be_bytes())?; // cumulative drops
        self.out.write_all(&(r.unix_us + EPOCH_DELTA_US).to_be_bytes())?;
        self.out.write_all(&bytes)
    }

    pub fn into_inner(self) -> W {
        self.out
    }
}

fn read_exact_or_eof<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<bool> {
    let mut got = 0;
    while got < buf.len() {
        match r.read(&mut buf[got..])? {
            0 if got == 0 => return Ok(false),
            0 => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated btsnoop record")),
            n => got += n,
        }
    }
    Ok(true)
}

/// Reads every record of a btsnoop file. Each record's bytes must be one
/// well-formed H4 frame ([`Frame::parse`]).
pub fn read_all<R: Read>(mut r: R) -> io::Result<Vec<Record>> {
    let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_string());
    let mut hdr = [0u8; 16];
    r.read_exact(&mut hdr)?;
    if &hdr[..8] != MAGIC {
        return Err(bad("not a btsnoop file"));
    }
    if u32::from_be_bytes(hdr[12..16].try_into().unwrap()) != DATALINK_H4 {
        return Err(bad("btsnoop datalink is not H4 (1002)"));
    }
    let mut out = Vec::new();
    let mut rec = [0u8; 24];
    while read_exact_or_eof(&mut r, &mut rec)? {
        let incl = u32::from_be_bytes(rec[4..8].try_into().unwrap());
        if incl > MAX_RECORD {
            return Err(bad("btsnoop record too large"));
        }
        let flags = u32::from_be_bytes(rec[8..12].try_into().unwrap());
        let ts = i64::from_be_bytes(rec[16..24].try_into().unwrap());
        let mut data = vec![0u8; incl as usize];
        r.read_exact(&mut data)?;
        let frame = Frame::parse(&data).map_err(|e| bad(&format!("btsnoop record: {e}")))?;
        out.push(Record {
            direction: if flags & 1 == 0 { Direction::HostToController } else { Direction::ControllerToHost },
            unix_us: ts - EPOCH_DELTA_US,
            frame,
        });
    }
    Ok(out)
}

/// Zeroes key material in an H4 frame (type byte included), in place:
/// PIN codes and link keys (BR/EDR), LTKs and IRKs (LE), and LE Encrypt's
/// key. Unknown or short packets are left alone.
pub fn redact(h4: &mut [u8]) {
    fn zero(b: &mut [u8], from: usize, len: usize) {
        if let Some(s) = b.get_mut(from..from + len) {
            s.fill(0);
        }
    }
    match h4.first() {
        Some(0x01) if h4.len() >= 4 => {
            let op = u16::from_le_bytes([h4[1], h4[2]]);
            let p = 4; // parameters start after type, opcode, length
            match op {
                0x040B => zero(h4, p + 6, 16),     // Link_Key_Request_Reply: bdaddr, key
                0x040D => zero(h4, p + 7, 16),     // PIN_Code_Request_Reply: bdaddr, len, pin
                0x2017 => zero(h4, p, 16),         // LE_Encrypt: key, plaintext
                0x2019 => zero(h4, p + 12, 16),    // LE_Enable_Encryption: handle, rand, ediv, ltk
                0x201A => zero(h4, p + 2, 16),     // LE_Long_Term_Key_Request_Reply: handle, ltk
                0x2027 => zero(h4, p + 7, 32),     // LE_Add_Device_To_Resolving_List: type, addr, peer irk, local irk
                _ => {}
            }
        }
        Some(0x04) if h4.len() >= 3 => {
            if h4[1] == 0x18 {
                zero(h4, 3 + 6, 16); // Link_Key_Notification: bdaddr, key, type
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let recs = vec![
            Record {
                direction: Direction::HostToController,
                unix_us: 1_762_538_000_000_000,
                frame: Frame::parse(&[0x01, 0x03, 0x0c, 0x00]).unwrap(),
            },
            Record {
                direction: Direction::ControllerToHost,
                unix_us: 1_762_538_000_000_123,
                frame: Frame::parse(&[0x04, 0x0e, 0x04, 0x01, 0x03, 0x0c, 0x00]).unwrap(),
            },
        ];
        let mut w = Writer::new(Vec::new()).unwrap();
        for r in &recs {
            w.write(r).unwrap();
        }
        assert_eq!(read_all(&w.into_inner()[..]).unwrap(), recs);
    }

    #[test]
    fn keys_never_reach_the_file() {
        // Link_Key_Notification with a key of 0xAA bytes.
        let mut ev = vec![0x04, 0x18, 23];
        ev.extend_from_slice(&[1, 2, 3, 4, 5, 6]);
        ev.extend_from_slice(&[0xAA; 16]);
        ev.push(0x07);
        // LE_Long_Term_Key_Request_Reply with an LTK of 0xBB bytes.
        let mut cmd = vec![0x01, 0x1A, 0x20, 18, 0x40, 0x00];
        cmd.extend_from_slice(&[0xBB; 16]);
        let mut w = Writer::new(Vec::new()).unwrap();
        for (d, b) in [(Direction::ControllerToHost, &ev), (Direction::HostToController, &cmd)] {
            w.write(&Record { direction: d, unix_us: 0, frame: Frame::parse(b).unwrap() }).unwrap();
        }
        let file = w.into_inner();
        assert!(!file.windows(4).any(|x| x == [0xAA; 4] || x == [0xBB; 4]));
        let back = read_all(&file[..]).unwrap();
        assert_eq!(back[0].frame.packet[2..8], [1, 2, 3, 4, 5, 6]); // address kept
        assert_eq!(back[0].frame.packet[24], 0x07); // key type kept
    }

    #[test]
    fn refuses_garbage() {
        assert!(read_all(&b"notsnoop00000000"[..]).is_err());
        let mut w = Writer::new(Vec::new()).unwrap().into_inner();
        w.extend_from_slice(&[0, 0, 0, 0, 0xff, 0xff, 0xff, 0xff]); // huge included length
        w.extend_from_slice(&[0; 16]);
        assert!(read_all(&w[..]).is_err());
    }
}
