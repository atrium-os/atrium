//! H4 framing: one type byte, then an HCI packet whose header carries its
//! own length (Core 6.0 Vol 4 Part A). Used on USB-less transports (UART),
//! by Rootcanal over TCP, and between `vinculum-hci` and `vinculumd`.
//!
//! The decoder is a byte-stream reassembler. It refuses a packet whose type
//! byte is unknown or whose length is over the bound for its type, and it
//! never allocates more than one packet's worth. H4 has no resync marker, so
//! after an error the caller drops the stream (the transport resets the
//! controller); the decoder stays in its error state until [`Decoder::reset`].

/// The H4 packet type byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Kind {
    Command = 0x01,
    Acl = 0x02,
    Sco = 0x03,
    Event = 0x04,
    Iso = 0x05,
}

impl Kind {
    pub fn from_byte(b: u8) -> Option<Kind> {
        Some(match b {
            0x01 => Kind::Command,
            0x02 => Kind::Acl,
            0x03 => Kind::Sco,
            0x04 => Kind::Event,
            0x05 => Kind::Iso,
            _ => return None,
        })
    }

    /// Header length of the HCI packet that follows the type byte.
    pub fn header_len(self) -> usize {
        match self {
            Kind::Command => 3, // opcode u16, length u8
            Kind::Acl => 4,     // handle+flags u16, length u16
            Kind::Sco => 3,     // handle+flags u16, length u8
            Kind::Event => 2,   // code u8, length u8
            Kind::Iso => 4,     // handle+flags u16, length u14 + 2 rfu bits
        }
    }

    /// Payload length from a complete header.
    fn payload_len(self, h: &[u8]) -> usize {
        match self {
            Kind::Command | Kind::Sco => h[2] as usize,
            Kind::Event => h[1] as usize,
            Kind::Acl => u16::from_le_bytes([h[2], h[3]]) as usize,
            Kind::Iso => (u16::from_le_bytes([h[2], h[3]]) & 0x3fff) as usize,
        }
    }
}

/// Largest payload accepted per type. Commands, events and SCO are bounded
/// by their 8-bit length field (255). ACL and ISO are bounded by what
/// Vinculum ever configures the controller for (LE and BR/EDR buffers top out
/// well under this), not by the 16-bit field.
pub const MAX_ACL_PAYLOAD: usize = 4096;
pub const MAX_ISO_PAYLOAD: usize = 4096;

fn max_payload(k: Kind) -> usize {
    match k {
        Kind::Command | Kind::Sco | Kind::Event => 255,
        Kind::Acl => MAX_ACL_PAYLOAD,
        Kind::Iso => MAX_ISO_PAYLOAD,
    }
}

/// One complete H4 frame: its type and the HCI packet (header + payload),
/// without the type byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub kind: Kind,
    pub packet: Vec<u8>,
}

impl Frame {
    /// The frame as bytes on the wire: type byte, then the packet.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(1 + self.packet.len());
        v.push(self.kind as u8);
        v.extend_from_slice(&self.packet);
        v
    }

    /// Checks a whole frame given at once (for message-based transports such
    /// as a SOCK_SEQPACKET socketpair): its type is known, its header is
    /// complete and its length field matches exactly.
    pub fn parse(bytes: &[u8]) -> Result<Frame, Error> {
        let (&t, rest) = bytes.split_first().ok_or(Error::Truncated)?;
        let kind = Kind::from_byte(t).ok_or(Error::UnknownType(t))?;
        let hl = kind.header_len();
        if rest.len() < hl {
            return Err(Error::Truncated);
        }
        let pl = kind.payload_len(&rest[..hl]);
        if pl > max_payload(kind) {
            return Err(Error::TooLong { kind, len: pl });
        }
        match rest.len().cmp(&(hl + pl)) {
            std::cmp::Ordering::Less => Err(Error::Truncated),
            std::cmp::Ordering::Greater => Err(Error::TrailingBytes),
            std::cmp::Ordering::Equal => Ok(Frame { kind, packet: rest.to_vec() }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    UnknownType(u8),
    TooLong { kind: Kind, len: usize },
    Truncated,
    TrailingBytes,
    /// The stream already failed; call [`Decoder::reset`].
    Failed,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::UnknownType(t) => write!(f, "unknown H4 packet type 0x{t:02x}"),
            Error::TooLong { kind, len } => write!(f, "{kind:?} payload of {len} bytes over the bound"),
            Error::Truncated => write!(f, "truncated H4 frame"),
            Error::TrailingBytes => write!(f, "bytes after the H4 frame"),
            Error::Failed => write!(f, "H4 stream already failed"),
        }
    }
}

impl std::error::Error for Error {}

#[derive(Debug, Default)]
enum State {
    #[default]
    Type,
    Header(Kind),
    Payload(Kind, usize),
    Failed,
}

/// Reassembles H4 frames from a byte stream (UART, TCP).
#[derive(Debug, Default)]
pub struct Decoder {
    state: State,
    buf: Vec<u8>,
}

impl Decoder {
    pub fn new() -> Decoder {
        Decoder::default()
    }

    pub fn reset(&mut self) {
        *self = Decoder::default();
    }

    /// Feeds bytes; calls `out` for each complete frame. Stops at the first
    /// error and stays failed.
    pub fn push(&mut self, mut data: &[u8], mut out: impl FnMut(Frame)) -> Result<(), Error> {
        while !data.is_empty() {
            match self.state {
                State::Failed => return Err(Error::Failed),
                State::Type => {
                    let t = data[0];
                    data = &data[1..];
                    match Kind::from_byte(t) {
                        Some(k) => {
                            self.buf.clear();
                            self.state = State::Header(k);
                        }
                        None => {
                            self.state = State::Failed;
                            return Err(Error::UnknownType(t));
                        }
                    }
                }
                State::Header(k) => {
                    let need = k.header_len() - self.buf.len();
                    let n = need.min(data.len());
                    self.buf.extend_from_slice(&data[..n]);
                    data = &data[n..];
                    if self.buf.len() == k.header_len() {
                        let pl = k.payload_len(&self.buf);
                        if pl > max_payload(k) {
                            self.state = State::Failed;
                            return Err(Error::TooLong { kind: k, len: pl });
                        }
                        self.state = State::Payload(k, k.header_len() + pl);
                    }
                }
                State::Payload(_, total) => {
                    let n = (total - self.buf.len()).min(data.len());
                    self.buf.extend_from_slice(&data[..n]);
                    data = &data[n..];
                }
            }
            // A zero-length payload completes as soon as the header does.
            if let State::Payload(k, total) = self.state {
                if self.buf.len() == total {
                    out(Frame { kind: k, packet: std::mem::take(&mut self.buf) });
                    self.state = State::Type;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(d: &mut Decoder, bytes: &[u8]) -> (Vec<Frame>, Result<(), Error>) {
        let mut v = Vec::new();
        let r = d.push(bytes, |f| v.push(f));
        (v, r)
    }

    // HCI_Reset command and its Command Complete.
    const RESET: [u8; 4] = [0x01, 0x03, 0x0c, 0x00];
    const RESET_DONE: [u8; 7] = [0x04, 0x0e, 0x04, 0x01, 0x03, 0x0c, 0x00];

    #[test]
    fn whole_and_byte_by_byte_agree() {
        let mut stream = RESET.to_vec();
        stream.extend_from_slice(&RESET_DONE);
        let (all, r) = collect(&mut Decoder::new(), &stream);
        r.unwrap();
        assert_eq!(all.len(), 2);
        let mut d = Decoder::new();
        let mut one = Vec::new();
        for b in &stream {
            d.push(std::slice::from_ref(b), |f| one.push(f)).unwrap();
        }
        assert_eq!(all, one);
        assert_eq!(all[1].kind, Kind::Event);
        assert_eq!(all[1].to_bytes(), RESET_DONE);
    }

    #[test]
    fn unknown_type_fails_and_stays_failed() {
        let mut d = Decoder::new();
        assert_eq!(collect(&mut d, &[0x09]).1, Err(Error::UnknownType(0x09)));
        assert_eq!(collect(&mut d, &RESET).1, Err(Error::Failed));
        d.reset();
        assert_eq!(collect(&mut d, &RESET).0.len(), 1);
    }

    #[test]
    fn oversized_acl_refused_before_buffering() {
        // ACL header claiming 0xffff bytes.
        let mut d = Decoder::new();
        let r = collect(&mut d, &[0x02, 0x01, 0x00, 0xff, 0xff]).1;
        assert_eq!(r, Err(Error::TooLong { kind: Kind::Acl, len: 0xffff }));
        assert!(d.buf.capacity() < 64);
    }

    #[test]
    fn iso_length_ignores_rfu_bits() {
        let f = Frame::parse(&[0x05, 0x01, 0x00, 0x02, 0xc0, 0xaa, 0xbb]).unwrap();
        assert_eq!(f.packet.len(), 6);
    }

    #[test]
    fn whole_frame_checks() {
        assert_eq!(Frame::parse(&RESET).unwrap().kind, Kind::Command);
        assert_eq!(Frame::parse(&RESET[..3]), Err(Error::Truncated));
        let mut extra = RESET.to_vec();
        extra.push(0);
        assert_eq!(Frame::parse(&extra), Err(Error::TrailingBytes));
        assert_eq!(Frame::parse(&[]), Err(Error::Truncated));
    }
}
