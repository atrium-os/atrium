//! cryptad ↔ engine (vpn.md §3): one socketpair, length-prefixed postcard
//! frames both ways (`u32` little-endian length, then the body).
//!
//! The engine gets its tun as a descriptor (fd 3 at start), never a device
//! node, and its static key only as answers to [`FromEngine::Dh`] for the
//! one profile cryptad started it for.

use crate::net::Prefix4;
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};
use std::net::SocketAddrV4;

/// No frame is longer: [`Start`] with [`MAX_ALLOWED`] prefixes fits, and
/// an AnyConnect push or prompt with its gateway's text.
pub const MAX_FRAME: usize = 64 * 1024;
pub const MAX_ALLOWED: usize = 1024;

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Start {
    pub our_public: [u8; 32],
    pub peer_public: [u8; 32],
    pub psk: Option<[u8; 32]>,
    pub endpoint: SocketAddrV4,
    pub keepalive_s: Option<u16>,
    /// Packets from the tunnel are delivered only from these sources, and
    /// packets to the tunnel sent only to these destinations (WireGuard's
    /// cryptokey routing for one peer).
    pub allowed: Vec<Prefix4>,
}

impl std::fmt::Debug for Start {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Start").field("endpoint", &self.endpoint).field("keepalive_s", &self.keepalive_s).field("psk", &self.psk.is_some()).field("allowed", &self.allowed.len()).finish()
    }
}

impl Drop for Start {
    fn drop(&mut self) {
        if let Some(p) = &mut self.psk {
            zeroize::Zeroize::zeroize(p);
        }
    }
}

/// An AnyConnect tunnel (vpn.md §9, V4): what `atrium-crypta-ac` needs.
/// No secret: passwords and tokens reach the engine from Arca through its
/// own secrets helper (fd 4), never through cryptad.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct StartAc {
    /// The gateway's name: TLS server name and `Host`.
    pub host: String,
    /// Where cryptad resolved it; the engine's jail reaches only this.
    pub gateway: SocketAddrV4,
    /// `/` or the group's path (`/staff`).
    pub path: String,
    /// CA certificates (DER) the profile trusts besides the system's.
    pub cas: Vec<Vec<u8>>,
    /// SHA-256 of the gateway's certificate, when the profile pins it
    /// instead (a self-signed gateway): then no CA is consulted.
    pub pin: Option<[u8; 32]>,
    /// What the gateway is told the machine is called (vpn.md §9.5).
    pub hostname: String,
    pub mtu: u16,
    /// The username to answer the first text field with, when the profile
    /// keeps one.
    pub username: Option<String>,
    /// The group to choose when the gateway offers one (`group_list`).
    pub group: Option<String>,
    /// Arca holds a saved password for this profile: the first password
    /// field is answered with it once, without asking.
    pub saved: bool,
    /// The profile's client certificate chain (DER, its own first), whose
    /// key Arca holds: the engine signs through its secrets helper.
    pub cert_chain: Vec<Vec<u8>>,
    /// The session cookie of a sign-in still valid, kept by Arca: try it
    /// before asking again (a roam, an engine restart).
    pub resume: bool,
}

/// One answer to a prompt field.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Value {
    Text(String),
    /// What was typed into Arca's field (`HOLD_FOR_VPN`): only the
    /// engine's secrets helper redeems it. `keep`: save it as the
    /// profile's password once the gateway accepts it.
    Held { handle: [u8; 16], keep: bool },
}

impl std::fmt::Debug for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::Text(t) => write!(f, "Text({} bytes)", t.len()),
            Value::Held { keep, .. } => write!(f, "Held(.., keep {keep})"),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ToEngine {
    Start(Start),
    StartAc(StartAc),
    /// The person's answers to the last `Prompt`, field by field; `None`:
    /// they cancelled.
    Answer(Option<Vec<(String, Value)>>),
    /// An SSO sign-in finished (vpn.md §9.3): Arca holds the token for this
    /// profile's gateway; `false`: it failed or was cancelled.
    SsoDone(bool),
    /// The answer to [`FromEngine::Dh`]; `None`: refused or unavailable.
    Dh(Option<DhShared>),
    /// The network changed: send from the new path (vpn.md §5.6).
    Rebind,
    Stop,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DhShared(pub [u8; 32]);

impl std::fmt::Debug for DhShared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DhShared(..)")
    }
}

impl Drop for DhShared {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.0);
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum FromEngine {
    /// X25519(our static key, point), please.
    Dh { point: [u8; 32] },
    /// A handshake completed: the tunnel carries traffic.
    Up,
    /// Handshakes stopped getting answers (90 s), or keys were erased.
    Down { why: String },
    /// Every 10 s while running.
    Stats { rx: u64, tx: u64, dropped: u64 },
    /// AnyConnect: the gateway asks the person (vpn.md §9.3). The JSON-free
    /// form is `crypta_ac::auth::Form`'s, encoded by the engine.
    Prompt(Prompt),
    /// AnyConnect: sign in at an identity provider in a Navigator sign-in
    /// window; the final page at `final_url` sets `token_cookie`.
    Sso { login: String, final_url: String, token_cookie: String },
    /// AnyConnect: signed in and the tunnel is open; these are the
    /// gateway's settings, for atrium-net (vpn.md §5.1).
    Pushed(Pushed),
    /// The gateway refused, or asked what Atrium does not do: shown to the
    /// person as is. The engine stops after it.
    Refused { why: String },
}

/// A prompt as cryptad passes it on (`crypta_ac::auth::Form` flattened).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Prompt {
    pub title: String,
    pub message: String,
    pub error: String,
    pub banner: String,
    pub fields: Vec<PromptField>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptField {
    pub name: String,
    pub label: String,
    /// 0 text, 1 password, 2 one-time code, 3 choice.
    pub kind: u8,
    /// `(value, label)` for a choice.
    pub choices: Vec<(String, String)>,
}

/// A gateway's push (vpn.md §9.4), checked again by atrium-net.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Pushed {
    /// Our address in the tunnel and its network's prefix length.
    pub addr: std::net::Ipv4Addr,
    pub prefix: u8,
    pub mtu: u16,
    pub dns: Vec<std::net::Ipv4Addr>,
    pub domains: Vec<String>,
    pub includes: Vec<Prefix4>,
    pub excludes: Vec<Prefix4>,
    pub full: bool,
    pub banner: String,
    /// What was dropped (header names), for the log and Settings.
    pub ignored: Vec<String>,
}

pub fn write<T: Serialize>(w: &mut impl Write, m: &T) -> io::Result<()> {
    let b = postcard::to_stdvec(m).map_err(|_| io::Error::other("encode"))?;
    if b.len() > MAX_FRAME {
        return Err(io::Error::other("frame too large"));
    }
    let mut f = Vec::with_capacity(4 + b.len());
    f.extend_from_slice(&(b.len() as u32).to_le_bytes());
    f.extend_from_slice(&b);
    w.write_all(&f)
}

/// One frame; `Ok(None)` at a clean end before any byte.
pub fn read<T: for<'a> Deserialize<'a>>(r: &mut impl Read) -> io::Result<Option<T>> {
    let mut len = [0u8; 4];
    match r.read(&mut len[..1])? {
        0 => return Ok(None),
        _ => r.read_exact(&mut len[1..])?,
    }
    let n = u32::from_le_bytes(len) as usize;
    if n > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut b = zeroize::Zeroizing::new(vec![0u8; n]);
    r.read_exact(&mut b)?;
    decode(&b).map(Some)
}

pub fn decode<T: for<'a> Deserialize<'a>>(b: &[u8]) -> io::Result<T> {
    match postcard::take_from_bytes::<T>(b) {
        Ok((m, rest)) if rest.is_empty() => Ok(m),
        _ => Err(io::Error::new(io::ErrorKind::InvalidData, "bad frame")),
    }
}

/// Frames out of a byte stream read in pieces (a non-blocking socket).
#[derive(Default)]
pub struct Reader {
    buf: Vec<u8>,
}

impl Reader {
    pub fn push(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    /// The next whole frame, if one is here. An error ends the stream.
    pub fn next<T: for<'a> Deserialize<'a>>(&mut self) -> io::Result<Option<T>> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let n = u32::from_le_bytes(self.buf[..4].try_into().unwrap()) as usize;
        if n > MAX_FRAME {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
        }
        if self.buf.len() < 4 + n {
            return Ok(None);
        }
        let m = decode(&self.buf[4..4 + n]);
        zeroize::Zeroize::zeroize(&mut self.buf[..4 + n]);
        self.buf.drain(..4 + n);
        m.map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_bad_ones_end_the_stream() {
        let s = ToEngine::Start(Start {
            our_public: [1; 32],
            peer_public: [2; 32],
            psk: Some([3; 32]),
            endpoint: "198.51.100.51:51820".parse().unwrap(),
            keepalive_s: Some(25),
            allowed: (0..MAX_ALLOWED).map(|i| Prefix4::new(std::net::Ipv4Addr::from(0x0a00_0000 | (i as u32) << 8), 24).unwrap()).collect(),
        });
        let mut b = Vec::new();
        write(&mut b, &s).unwrap();
        write(&mut b, &FromEngine::Dh { point: [9; 32] }).unwrap();
        let mut r = &b[..];
        assert_eq!(read::<ToEngine>(&mut r).unwrap(), Some(s.clone()));
        let mut rd = Reader::default();
        for c in b.chunks(7) {
            rd.push(c);
        }
        assert_eq!(rd.next::<ToEngine>().unwrap(), Some(s));
        assert_eq!(rd.next::<FromEngine>().unwrap(), Some(FromEngine::Dh { point: [9; 32] }));
        assert_eq!(rd.next::<FromEngine>().unwrap(), None);
        let mut big = Reader::default();
        big.push(&((MAX_FRAME + 1) as u32).to_le_bytes());
        assert!(big.next::<FromEngine>().is_err());
        let mut junk = Reader::default();
        junk.push(&[2, 0, 0, 0, 0xff, 0xff]);
        assert!(junk.next::<FromEngine>().is_err());
    }
}
