//! The four WireGuard messages, parsed without trusting anything in them.
//! Every field is fixed-size except a transport message's payload; a
//! datagram of any other size or type is dropped (vpn.md §4.2).

pub const INITIATION: u8 = 1;
pub const RESPONSE: u8 = 2;
pub const COOKIE_REPLY: u8 = 3;
pub const TRANSPORT: u8 = 4;

pub const INITIATION_LEN: usize = 148;
pub const RESPONSE_LEN: usize = 92;
pub const COOKIE_REPLY_LEN: usize = 64;
pub const TRANSPORT_HEADER_LEN: usize = 16;
/// The smallest transport message: a keepalive (header and tag).
pub const TRANSPORT_MIN_LEN: usize = TRANSPORT_HEADER_LEN + 16;

/// Where mac1 and mac2 start in a handshake message.
pub const INITIATION_MAC1: usize = 116;
pub const RESPONSE_MAC1: usize = 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Initiation {
    pub sender: u32,
    pub ephemeral: [u8; 32],
    pub static_sealed: [u8; 48],
    pub timestamp_sealed: [u8; 28],
    pub mac1: [u8; 16],
    pub mac2: [u8; 16],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub sender: u32,
    pub receiver: u32,
    pub ephemeral: [u8; 32],
    pub empty_sealed: [u8; 16],
    pub mac1: [u8; 16],
    pub mac2: [u8; 16],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CookieReply {
    pub receiver: u32,
    pub nonce: [u8; 24],
    pub cookie_sealed: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transport<'a> {
    pub receiver: u32,
    pub counter: u64,
    /// The sealed packet and its tag.
    pub sealed: &'a [u8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message<'a> {
    Initiation(Initiation),
    Response(Response),
    CookieReply(CookieReply),
    Transport(Transport<'a>),
}

fn arr<const N: usize>(b: &[u8], at: usize) -> [u8; N] {
    b[at..at + N].try_into().expect("checked length")
}

fn u32le(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(arr(b, at))
}

/// Parse a datagram. `None` for anything that is not exactly one of the
/// four messages (wrong type, nonzero reserved bytes, wrong size).
pub fn parse(b: &[u8]) -> Option<Message<'_>> {
    if b.len() < 4 || b[1..4] != [0, 0, 0] {
        return None;
    }
    Some(match (b[0], b.len()) {
        (INITIATION, INITIATION_LEN) => Message::Initiation(Initiation {
            sender: u32le(b, 4),
            ephemeral: arr(b, 8),
            static_sealed: arr(b, 40),
            timestamp_sealed: arr(b, 88),
            mac1: arr(b, 116),
            mac2: arr(b, 132),
        }),
        (RESPONSE, RESPONSE_LEN) => Message::Response(Response {
            sender: u32le(b, 4),
            receiver: u32le(b, 8),
            ephemeral: arr(b, 12),
            empty_sealed: arr(b, 44),
            mac1: arr(b, 60),
            mac2: arr(b, 76),
        }),
        (COOKIE_REPLY, COOKIE_REPLY_LEN) => Message::CookieReply(CookieReply {
            receiver: u32le(b, 4),
            nonce: arr(b, 8),
            cookie_sealed: arr(b, 32),
        }),
        (TRANSPORT, n) if n >= TRANSPORT_MIN_LEN => Message::Transport(Transport {
            receiver: u32le(b, 4),
            counter: u64::from_le_bytes(arr(b, 8)),
            sealed: &b[16..],
        }),
        _ => return None,
    })
}

impl Initiation {
    pub fn encode(&self) -> [u8; INITIATION_LEN] {
        let mut b = [0u8; INITIATION_LEN];
        b[0] = INITIATION;
        b[4..8].copy_from_slice(&self.sender.to_le_bytes());
        b[8..40].copy_from_slice(&self.ephemeral);
        b[40..88].copy_from_slice(&self.static_sealed);
        b[88..116].copy_from_slice(&self.timestamp_sealed);
        b[116..132].copy_from_slice(&self.mac1);
        b[132..148].copy_from_slice(&self.mac2);
        b
    }
}

impl Response {
    pub fn encode(&self) -> [u8; RESPONSE_LEN] {
        let mut b = [0u8; RESPONSE_LEN];
        b[0] = RESPONSE;
        b[4..8].copy_from_slice(&self.sender.to_le_bytes());
        b[8..12].copy_from_slice(&self.receiver.to_le_bytes());
        b[12..44].copy_from_slice(&self.ephemeral);
        b[44..60].copy_from_slice(&self.empty_sealed);
        b[60..76].copy_from_slice(&self.mac1);
        b[76..92].copy_from_slice(&self.mac2);
        b
    }
}

impl CookieReply {
    pub fn encode(&self) -> [u8; COOKIE_REPLY_LEN] {
        let mut b = [0u8; COOKIE_REPLY_LEN];
        b[0] = COOKIE_REPLY;
        b[4..8].copy_from_slice(&self.receiver.to_le_bytes());
        b[8..32].copy_from_slice(&self.nonce);
        b[32..64].copy_from_slice(&self.cookie_sealed);
        b
    }
}

/// A transport message's header, ahead of `sealed_len` bytes of payload.
pub fn transport_header(receiver: u32, counter: u64) -> [u8; TRANSPORT_HEADER_LEN] {
    let mut b = [0u8; TRANSPORT_HEADER_LEN];
    b[0] = TRANSPORT;
    b[4..8].copy_from_slice(&receiver.to_le_bytes());
    b[8..16].copy_from_slice(&counter.to_le_bytes());
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_message_round_trips() {
        let i = Initiation { sender: 7, ephemeral: [1; 32], static_sealed: [2; 48], timestamp_sealed: [3; 28], mac1: [4; 16], mac2: [5; 16] };
        assert_eq!(parse(&i.encode()), Some(Message::Initiation(i.clone())));
        let r = Response { sender: 8, receiver: 9, ephemeral: [1; 32], empty_sealed: [2; 16], mac1: [3; 16], mac2: [4; 16] };
        assert_eq!(parse(&r.encode()), Some(Message::Response(r.clone())));
        let c = CookieReply { receiver: 10, nonce: [6; 24], cookie_sealed: [7; 32] };
        assert_eq!(parse(&c.encode()), Some(Message::CookieReply(c.clone())));
        let mut t = transport_header(11, 12).to_vec();
        t.extend_from_slice(&[0; 16]);
        assert_eq!(parse(&t), Some(Message::Transport(Transport { receiver: 11, counter: 12, sealed: &[0; 16] })));
    }

    #[test]
    fn anything_else_is_dropped() {
        let i = Initiation { sender: 7, ephemeral: [1; 32], static_sealed: [2; 48], timestamp_sealed: [3; 28], mac1: [4; 16], mac2: [5; 16] }.encode();
        assert!(parse(&i[..147]).is_none());
        let mut long = i.to_vec();
        long.push(0);
        assert!(parse(&long).is_none());
        let mut reserved = i;
        reserved[2] = 1;
        assert!(parse(&reserved).is_none());
        let mut ty = i;
        ty[0] = 5;
        assert!(parse(&ty).is_none());
        assert!(parse(&transport_header(1, 1)).is_none(), "a transport message needs at least a tag");
        assert!(parse(&[]).is_none() && parse(&[4, 0, 0]).is_none());
    }
}
