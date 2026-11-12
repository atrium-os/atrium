//! The two channels of `hospes-cdm` (docs/spec/hospes.md §3, §8).
//!
//! - **Control**, supervisor ↔ CDM: sessions, licence messages, key status. The supervisor
//!   relays licence messages to and from the engine and never looks inside them.
//! - **Data**, decode worker ↔ CDM: encrypted samples in, clear samples or decoded frames
//!   out. One request, one answer.
//!
//! Every message is `kind: u32, len: u32, payload[len]`, little-endian, on a stream socket.
//! [`Msg::encode`] and [`Msg::decode`] are the only codec; both ends use them. Whatever the
//! CDM side sends is hostile to its peers (it runs a closed library), so decoding checks every
//! length against [`MAX_LEN`] and [`MAX_ITEMS`], and [`Frame::planes`] checks a frame's
//! geometry against its buffer before anyone reads pixels.
//!
//! Version 1 carries sample and frame bytes inline. Shared frame slots (§8.2 `Allocate`)
//! replace that later without changing the message set.
#![no_std]

extern crate alloc;
#[cfg(feature = "std")]
extern crate std;

use alloc::vec::Vec;

pub const VERSION: u32 = 1;
/// Largest message either side accepts.
pub const MAX_LEN: u32 = 64 << 20;
/// Largest count of any list in a message (subsamples, keys).
pub const MAX_ITEMS: u32 = 4096;
/// Longest id, key id, IV or similar short field.
pub const MAX_SHORT: u32 = 1024;

/// `cdm::EncryptionScheme`.
pub mod scheme {
    pub const CLEAR: u32 = 0;
    pub const CENC: u32 = 1;
    pub const CBCS: u32 = 2;
}

/// `cdm::Status` values the data channel reports.
pub mod status {
    pub const SUCCESS: u32 = 0;
    pub const NEED_MORE_DATA: u32 = 1;
    pub const NO_KEY: u32 = 2;
    pub const INITIALIZATION_ERROR: u32 = 3;
    pub const DECRYPT_ERROR: u32 = 4;
    pub const DECODE_ERROR: u32 = 5;
    pub const DEFERRED_INITIALIZATION: u32 = 6;
}

/// One encrypted (or clear) sample: `cdm::InputBuffer_2` without the pointers.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Sample {
    pub scheme: u32,
    pub key_id: Vec<u8>,
    pub iv: Vec<u8>,
    /// `(clear_bytes, cipher_bytes)` pairs; empty means the whole sample is encrypted.
    pub subsamples: Vec<(u32, u32)>,
    pub crypt_byte_block: u32,
    pub skip_byte_block: u32,
    /// Microseconds.
    pub timestamp: i64,
    pub data: Vec<u8>,
}

/// A decoded frame from the CDM's decrypt-and-decode path.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Frame {
    pub status: u32,
    /// `cdm::VideoFormat` (2 = I420).
    pub format: u32,
    pub width: u32,
    pub height: u32,
    pub offsets: [u32; 3],
    pub strides: [u32; 3],
    pub timestamp: i64,
    pub data: Vec<u8>,
}

impl Frame {
    /// Y, U and V planes of an 8-bit 4:2:0 frame (I420, or YV12 with U and V swapped back),
    /// as `(offset, stride, rows)`, after checking that each plane lies inside `data` and
    /// that the size is within `max_w × max_h`. `None` for anything else.
    pub fn planes(&self, max_w: u32, max_h: u32) -> Option<[(usize, usize, usize); 3]> {
        let (w, h) = (self.width as usize, self.height as usize);
        if self.status != status::SUCCESS || w == 0 || h == 0 || self.width > max_w || self.height > max_h {
            return None;
        }
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let order: [usize; 3] = match self.format {
            2 => [0, 1, 2], // I420
            1 => [0, 2, 1], // YV12: V before U in memory, same plane indices
            _ => return None,
        };
        let mut out = [(0usize, 0usize, 0usize); 3];
        for (i, &p) in order.iter().enumerate() {
            let (pw, ph) = if i == 0 { (w, h) } else { (cw, ch) };
            let (off, stride) = (self.offsets[p] as usize, self.strides[p] as usize);
            if stride < pw {
                return None;
            }
            let end = off.checked_add(stride.checked_mul(ph - 1)?)?.checked_add(pw)?;
            if end > self.data.len() {
                return None;
            }
            out[i] = (off, stride, ph);
        }
        Some(out)
    }
}

/// A key and its status, as `cdm::KeyInformation` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyInfo {
    pub key_id: Vec<u8>,
    pub status: u32,
    pub system_code: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Msg {
    // ---- control, supervisor → CDM ----
    /// `Initialize`. `link_mask` is what the host reports for output protection.
    Init { allow_persistent: bool, link_mask: u32 },
    GetStatusForPolicy { promise: u32, min_hdcp: u32 },
    SetServerCertificate { promise: u32, cert: Vec<u8> },
    CreateSession { promise: u32, session_type: u32, init_type: u32, init: Vec<u8> },
    LoadSession { promise: u32, session_type: u32, id: Vec<u8> },
    UpdateSession { promise: u32, id: Vec<u8>, response: Vec<u8> },
    CloseSession { promise: u32, id: Vec<u8> },
    RemoveSession { promise: u32, id: Vec<u8> },

    // ---- control, CDM → supervisor ----
    /// The library is loaded and the CDM instance exists.
    Ready { cdm_version: Vec<u8> },
    Initialized { ok: bool },
    KeyStatus { promise: u32, status: u32 },
    NewSession { promise: u32, id: Vec<u8> },
    Resolved { promise: u32 },
    Rejected { promise: u32, exception: u32, system_code: u32, message: Vec<u8> },
    SessionMessage { id: Vec<u8>, message_type: u32, message: Vec<u8> },
    KeysChange { id: Vec<u8>, has_new_usable: bool, keys: Vec<KeyInfo> },
    Expiration { id: Vec<u8>, time: f64 },
    Closed { id: Vec<u8> },
    /// Fatal: the host is about to exit.
    Fail { code: u32, what: Vec<u8> },

    // ---- data, worker → CDM ----
    Decrypt { sample: Sample },
    InitAudio { codec: u32, channels: u32, bits: u32, rate: u32, extra: Vec<u8>, scheme: u32 },
    InitVideo { codec: u32, profile: u32, format: u32, width: u32, height: u32, extra: Vec<u8>, scheme: u32 },
    /// An empty `data` is end of stream.
    DecryptAndDecode { sample: Sample },
    ResetDecoder { stream: u32 },
    DeinitDecoder { stream: u32 },

    // ---- data, CDM → worker ----
    Decrypted { status: u32, timestamp: i64, data: Vec<u8> },
    InitResult { status: u32 },
    Decoded { frame: Frame },
    /// Answer to `ResetDecoder` and `DeinitDecoder`.
    Done,
}

mod kind {
    pub const INIT: u32 = 0x01;
    pub const POLICY: u32 = 0x02;
    pub const CERT: u32 = 0x03;
    pub const CREATE: u32 = 0x04;
    pub const LOAD: u32 = 0x05;
    pub const UPDATE: u32 = 0x06;
    pub const CLOSE: u32 = 0x07;
    pub const REMOVE: u32 = 0x08;
    pub const READY: u32 = 0x81;
    pub const INITIALIZED: u32 = 0x82;
    pub const KEY_STATUS: u32 = 0x83;
    pub const NEW_SESSION: u32 = 0x84;
    pub const RESOLVED: u32 = 0x85;
    pub const REJECTED: u32 = 0x86;
    pub const MESSAGE: u32 = 0x87;
    pub const KEYS: u32 = 0x88;
    pub const EXPIRATION: u32 = 0x89;
    pub const CLOSED: u32 = 0x8a;
    pub const FAIL: u32 = 0x8f;
    pub const DECRYPT: u32 = 0x21;
    pub const INIT_AUDIO: u32 = 0x22;
    pub const INIT_VIDEO: u32 = 0x23;
    pub const DECODE: u32 = 0x24;
    pub const RESET: u32 = 0x25;
    pub const DEINIT: u32 = 0x26;
    pub const DECRYPTED: u32 = 0xa1;
    pub const INIT_RESULT: u32 = 0xa2;
    pub const DECODED: u32 = 0xa3;
    pub const DONE: u32 = 0xa4;
}

struct W(Vec<u8>);
impl W {
    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn i64(&mut self, v: i64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.u32(b.len() as u32);
        self.0.extend_from_slice(b);
        self
    }
    fn sample(&mut self, s: &Sample) -> &mut Self {
        self.u32(s.scheme).bytes(&s.key_id).bytes(&s.iv).u32(s.subsamples.len() as u32);
        for &(c, e) in &s.subsamples {
            self.u32(c).u32(e);
        }
        self.u32(s.crypt_byte_block).u32(s.skip_byte_block).i64(s.timestamp).bytes(&s.data)
    }
}

/// A malformed or over-limit message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Malformed(pub &'static str);

struct Rd<'a>(&'a [u8]);
impl<'a> Rd<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Malformed> {
        if self.0.len() < n {
            return Err(Malformed("truncated"));
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u32(&mut self) -> Result<u32, Malformed> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn i64(&mut self) -> Result<i64, Malformed> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn bool(&mut self) -> Result<bool, Malformed> {
        match self.u32()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Malformed("bad bool")),
        }
    }
    fn bytes_max(&mut self, max: u32) -> Result<Vec<u8>, Malformed> {
        let n = self.u32()?;
        if n > max {
            return Err(Malformed("field too long"));
        }
        Ok(self.take(n as usize)?.to_vec())
    }
    fn short(&mut self) -> Result<Vec<u8>, Malformed> {
        self.bytes_max(MAX_SHORT)
    }
    fn long(&mut self) -> Result<Vec<u8>, Malformed> {
        self.bytes_max(MAX_LEN)
    }
    fn count(&mut self) -> Result<u32, Malformed> {
        let n = self.u32()?;
        if n > MAX_ITEMS {
            return Err(Malformed("too many items"));
        }
        Ok(n)
    }
    fn sample(&mut self) -> Result<Sample, Malformed> {
        let scheme = self.u32()?;
        let key_id = self.short()?;
        let iv = self.short()?;
        let n = self.count()?;
        let mut subsamples = Vec::with_capacity(n as usize);
        for _ in 0..n {
            subsamples.push((self.u32()?, self.u32()?));
        }
        Ok(Sample {
            scheme,
            key_id,
            iv,
            subsamples,
            crypt_byte_block: self.u32()?,
            skip_byte_block: self.u32()?,
            timestamp: self.i64()?,
            data: self.long()?,
        })
    }
    fn end(&self) -> Result<(), Malformed> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(Malformed("trailing bytes"))
        }
    }
}

impl Msg {
    /// The whole message, header included.
    pub fn encode(&self) -> Vec<u8> {
        use kind::*;
        let mut w = W(Vec::with_capacity(64));
        let k = match self {
            Msg::Init { allow_persistent, link_mask } => {
                w.u32(*allow_persistent as u32).u32(*link_mask);
                INIT
            }
            Msg::GetStatusForPolicy { promise, min_hdcp } => {
                w.u32(*promise).u32(*min_hdcp);
                POLICY
            }
            Msg::SetServerCertificate { promise, cert } => {
                w.u32(*promise).bytes(cert);
                CERT
            }
            Msg::CreateSession { promise, session_type, init_type, init } => {
                w.u32(*promise).u32(*session_type).u32(*init_type).bytes(init);
                CREATE
            }
            Msg::LoadSession { promise, session_type, id } => {
                w.u32(*promise).u32(*session_type).bytes(id);
                LOAD
            }
            Msg::UpdateSession { promise, id, response } => {
                w.u32(*promise).bytes(id).bytes(response);
                UPDATE
            }
            Msg::CloseSession { promise, id } => {
                w.u32(*promise).bytes(id);
                CLOSE
            }
            Msg::RemoveSession { promise, id } => {
                w.u32(*promise).bytes(id);
                REMOVE
            }
            Msg::Ready { cdm_version } => {
                w.u32(VERSION).bytes(cdm_version);
                READY
            }
            Msg::Initialized { ok } => {
                w.u32(*ok as u32);
                INITIALIZED
            }
            Msg::KeyStatus { promise, status } => {
                w.u32(*promise).u32(*status);
                KEY_STATUS
            }
            Msg::NewSession { promise, id } => {
                w.u32(*promise).bytes(id);
                NEW_SESSION
            }
            Msg::Resolved { promise } => {
                w.u32(*promise);
                RESOLVED
            }
            Msg::Rejected { promise, exception, system_code, message } => {
                w.u32(*promise).u32(*exception).u32(*system_code).bytes(message);
                REJECTED
            }
            Msg::SessionMessage { id, message_type, message } => {
                w.bytes(id).u32(*message_type).bytes(message);
                MESSAGE
            }
            Msg::KeysChange { id, has_new_usable, keys } => {
                w.bytes(id).u32(*has_new_usable as u32).u32(keys.len() as u32);
                for k in keys {
                    w.bytes(&k.key_id).u32(k.status).u32(k.system_code);
                }
                KEYS
            }
            Msg::Expiration { id, time } => {
                w.bytes(id);
                w.0.extend_from_slice(&time.to_bits().to_le_bytes());
                EXPIRATION
            }
            Msg::Closed { id } => {
                w.bytes(id);
                CLOSED
            }
            Msg::Fail { code, what } => {
                w.u32(*code).bytes(what);
                FAIL
            }
            Msg::Decrypt { sample } => {
                w.sample(sample);
                DECRYPT
            }
            Msg::InitAudio { codec, channels, bits, rate, extra, scheme } => {
                w.u32(*codec).u32(*channels).u32(*bits).u32(*rate).bytes(extra).u32(*scheme);
                INIT_AUDIO
            }
            Msg::InitVideo { codec, profile, format, width, height, extra, scheme } => {
                w.u32(*codec).u32(*profile).u32(*format).u32(*width).u32(*height).bytes(extra).u32(*scheme);
                INIT_VIDEO
            }
            Msg::DecryptAndDecode { sample } => {
                w.sample(sample);
                DECODE
            }
            Msg::ResetDecoder { stream } => {
                w.u32(*stream);
                RESET
            }
            Msg::DeinitDecoder { stream } => {
                w.u32(*stream);
                DEINIT
            }
            Msg::Decrypted { status, timestamp, data } => {
                w.u32(*status).i64(*timestamp).bytes(data);
                DECRYPTED
            }
            Msg::InitResult { status } => {
                w.u32(*status);
                INIT_RESULT
            }
            Msg::Decoded { frame: f } => {
                w.u32(f.status).u32(f.format).u32(f.width).u32(f.height);
                for v in f.offsets.iter().chain(f.strides.iter()) {
                    w.u32(*v);
                }
                w.i64(f.timestamp).bytes(&f.data);
                DECODED
            }
            Msg::Done => DONE,
        };
        let mut out = Vec::with_capacity(8 + w.0.len());
        out.extend_from_slice(&k.to_le_bytes());
        out.extend_from_slice(&(w.0.len() as u32).to_le_bytes());
        out.extend_from_slice(&w.0);
        out
    }

    /// Decode one payload of kind `k` (the header already read and its length checked).
    pub fn decode(k: u32, p: &[u8]) -> Result<Msg, Malformed> {
        use kind::*;
        let mut r = Rd(p);
        let m = match k {
            INIT => Msg::Init { allow_persistent: r.bool()?, link_mask: r.u32()? },
            POLICY => Msg::GetStatusForPolicy { promise: r.u32()?, min_hdcp: r.u32()? },
            CERT => Msg::SetServerCertificate { promise: r.u32()?, cert: r.long()? },
            CREATE => Msg::CreateSession { promise: r.u32()?, session_type: r.u32()?, init_type: r.u32()?, init: r.long()? },
            LOAD => Msg::LoadSession { promise: r.u32()?, session_type: r.u32()?, id: r.short()? },
            UPDATE => Msg::UpdateSession { promise: r.u32()?, id: r.short()?, response: r.long()? },
            CLOSE => Msg::CloseSession { promise: r.u32()?, id: r.short()? },
            REMOVE => Msg::RemoveSession { promise: r.u32()?, id: r.short()? },
            READY => {
                if r.u32()? != VERSION {
                    return Err(Malformed("protocol version"));
                }
                Msg::Ready { cdm_version: r.short()? }
            }
            INITIALIZED => Msg::Initialized { ok: r.bool()? },
            KEY_STATUS => Msg::KeyStatus { promise: r.u32()?, status: r.u32()? },
            NEW_SESSION => Msg::NewSession { promise: r.u32()?, id: r.short()? },
            RESOLVED => Msg::Resolved { promise: r.u32()? },
            REJECTED => Msg::Rejected { promise: r.u32()?, exception: r.u32()?, system_code: r.u32()?, message: r.short()? },
            MESSAGE => Msg::SessionMessage { id: r.short()?, message_type: r.u32()?, message: r.long()? },
            KEYS => {
                let id = r.short()?;
                let has_new_usable = r.bool()?;
                let n = r.count()?;
                let mut keys = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    keys.push(KeyInfo { key_id: r.short()?, status: r.u32()?, system_code: r.u32()? });
                }
                Msg::KeysChange { id, has_new_usable, keys }
            }
            EXPIRATION => {
                let id = r.short()?;
                let t = f64::from_bits(u64::from_le_bytes(r.take(8)?.try_into().unwrap()));
                Msg::Expiration { id, time: t }
            }
            CLOSED => Msg::Closed { id: r.short()? },
            FAIL => Msg::Fail { code: r.u32()?, what: r.short()? },
            DECRYPT => Msg::Decrypt { sample: r.sample()? },
            INIT_AUDIO => Msg::InitAudio {
                codec: r.u32()?,
                channels: r.u32()?,
                bits: r.u32()?,
                rate: r.u32()?,
                extra: r.long()?,
                scheme: r.u32()?,
            },
            INIT_VIDEO => Msg::InitVideo {
                codec: r.u32()?,
                profile: r.u32()?,
                format: r.u32()?,
                width: r.u32()?,
                height: r.u32()?,
                extra: r.long()?,
                scheme: r.u32()?,
            },
            DECODE => Msg::DecryptAndDecode { sample: r.sample()? },
            RESET => Msg::ResetDecoder { stream: r.u32()? },
            DEINIT => Msg::DeinitDecoder { stream: r.u32()? },
            DECRYPTED => Msg::Decrypted { status: r.u32()?, timestamp: r.i64()?, data: r.long()? },
            INIT_RESULT => Msg::InitResult { status: r.u32()? },
            DECODED => {
                let mut f = Frame { status: r.u32()?, format: r.u32()?, width: r.u32()?, height: r.u32()?, ..Frame::default() };
                for i in 0..3 {
                    f.offsets[i] = r.u32()?;
                }
                for i in 0..3 {
                    f.strides[i] = r.u32()?;
                }
                f.timestamp = r.i64()?;
                f.data = r.long()?;
                Msg::Decoded { frame: f }
            }
            DONE => Msg::Done,
            _ => return Err(Malformed("unknown kind")),
        };
        r.end()?;
        Ok(m)
    }
}

/// Split a header into `(kind, len)`, refusing a length over [`MAX_LEN`].
pub fn parse_header(h: &[u8; 8]) -> Result<(u32, u32), Malformed> {
    let k = u32::from_le_bytes([h[0], h[1], h[2], h[3]]);
    let n = u32::from_le_bytes([h[4], h[5], h[6], h[7]]);
    if n > MAX_LEN {
        return Err(Malformed("message too long"));
    }
    Ok((k, n))
}

#[cfg(feature = "std")]
pub mod io {
    //! Blocking send and receive for the player's side.
    use super::*;
    use std::io::{self, Read, Write};

    pub fn send(w: &mut impl Write, m: &Msg) -> io::Result<()> {
        w.write_all(&m.encode())
    }

    /// The next message; `Ok(None)` when the peer closed cleanly between messages.
    pub fn recv(r: &mut impl Read) -> io::Result<Option<Msg>> {
        let mut h = [0u8; 8];
        let mut got = 0;
        while got < 8 {
            match r.read(&mut h[got..]) {
                Ok(0) if got == 0 => return Ok(None),
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => got += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        let (k, n) = parse_header(&h).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.0))?;
        let mut p = std::vec![0u8; n as usize];
        r.read_exact(&mut p)?;
        Msg::decode(k, &p).map(Some).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn round(m: Msg) {
        let b = m.encode();
        let (k, n) = parse_header(b[..8].try_into().unwrap()).unwrap();
        assert_eq!(n as usize, b.len() - 8);
        assert_eq!(Msg::decode(k, &b[8..]).unwrap(), m);
        // Every truncation is refused, never a panic.
        for cut in 0..b.len() - 8 {
            assert!(Msg::decode(k, &b[8..8 + cut]).is_err());
        }
    }

    #[test]
    fn round_trips() {
        let s = Sample {
            scheme: scheme::CBCS,
            key_id: vec![1; 16],
            iv: vec![2; 16],
            subsamples: vec![(3, 160), (0, 17)],
            crypt_byte_block: 1,
            skip_byte_block: 9,
            timestamp: -5,
            data: vec![9; 180],
        };
        round(Msg::Init { allow_persistent: true, link_mask: 2 });
        round(Msg::CreateSession { promise: 4, session_type: 1, init_type: 0, init: vec![7; 40] });
        round(Msg::UpdateSession { promise: 5, id: b"1".to_vec(), response: b"{}".to_vec() });
        round(Msg::Ready { cdm_version: b"x 1".to_vec() });
        round(Msg::Rejected { promise: 3, exception: 1, system_code: 0, message: b"no".to_vec() });
        round(Msg::KeysChange {
            id: b"1".to_vec(),
            has_new_usable: true,
            keys: vec![KeyInfo { key_id: vec![1; 16], status: 0, system_code: 0 }],
        });
        round(Msg::Expiration { id: b"2".to_vec(), time: 1.5 });
        round(Msg::Decrypt { sample: s.clone() });
        round(Msg::DecryptAndDecode { sample: s });
        round(Msg::InitVideo { codec: 3, profile: 9, format: 2, width: 64, height: 48, extra: vec![], scheme: 1 });
        round(Msg::Decrypted { status: 0, timestamp: 7, data: vec![1, 2, 3] });
        round(Msg::Decoded {
            frame: Frame { status: 0, format: 2, width: 2, height: 2, offsets: [0, 4, 5], strides: [2, 1, 1], timestamp: 1, data: vec![0; 6] },
        });
        round(Msg::Done);
    }

    #[test]
    fn limits() {
        let mut b = Msg::Closed { id: vec![0; 4] }.encode();
        b[8..12].copy_from_slice(&(MAX_SHORT + 1).to_le_bytes());
        assert!(Msg::decode(0x8a, &b[8..]).is_err());
        assert!(parse_header(&[0, 0, 0, 0, 0xff, 0xff, 0xff, 0xff]).is_err());
        assert!(Msg::decode(0x7777, &[]).is_err());
        assert!(Msg::decode(0x82, &[2, 0, 0, 0]).is_err());
    }

    #[test]
    fn frame_geometry() {
        let mut f = Frame { status: 0, format: 2, width: 4, height: 2, offsets: [0, 8, 10], strides: [4, 2, 2], timestamp: 0, data: vec![0; 12] };
        assert_eq!(f.planes(64, 64), Some([(0, 4, 2), (8, 2, 1), (10, 2, 1)]));
        f.offsets[2] = 11; // V runs past the end
        assert_eq!(f.planes(64, 64), None);
        f.offsets[2] = 10;
        f.strides[0] = 3; // stride below width
        assert_eq!(f.planes(64, 64), None);
        f.strides[0] = 4;
        assert_eq!(f.planes(3, 64), None); // over the limit
        f.offsets[1] = u32::MAX; // overflow
        assert_eq!(f.planes(64, 64), None);
    }
}
