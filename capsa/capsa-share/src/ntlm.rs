//! NTLMv2 inside SPNEGO, the client side, for SMB's password logins
//! (network-shares.md §5.3). NTLMv2 only: no LM, no NTLMv1, no anonymous,
//! no key exchange (RC4). It proves the client to the server and nothing
//! about the server; SMB 3.1.1's signed final reply proves the server knew
//! the password-derived key, which is the most a password protocol gives.
//!
//! Every length in the server's challenge is checked before it is used.

use hmac::{Hmac, Mac};
use md4::{Digest as _, Md4};
use md5::Md5;
use zeroize::Zeroizing;

type HmacMd5 = Hmac<Md5>;

const SIG: &[u8; 8] = b"NTLMSSP\0";

pub const UNICODE: u32 = 0x0000_0001;
pub const REQUEST_TARGET: u32 = 0x0000_0004;
pub const SIGN: u32 = 0x0000_0010;
pub const NTLM: u32 = 0x0000_0200;
pub const ALWAYS_SIGN: u32 = 0x0000_8000;
pub const EXTENDED_SESSIONSECURITY: u32 = 0x0008_0000;
pub const TARGET_INFO: u32 = 0x0080_0000;
pub const N128: u32 = 0x2000_0000;

/// What this client asks for.
pub const FLAGS: u32 = UNICODE | REQUEST_TARGET | SIGN | NTLM | ALWAYS_SIGN | EXTENDED_SESSIONSECURITY | TARGET_INFO | N128;

const AV_EOL: u16 = 0;
const AV_TIMESTAMP: u16 = 7;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The server's message isn't a well-formed NTLM challenge.
    Malformed(&'static str),
    /// The server wants something this client never does.
    Refused(&'static str),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Malformed(w) => write!(f, "malformed NTLM challenge: {w}"),
            Error::Refused(w) => write!(f, "refused: {w}"),
        }
    }
}

fn utf16(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
}

/// The first message.
pub fn negotiate() -> Vec<u8> {
    let mut m = Vec::with_capacity(32);
    m.extend_from_slice(SIG);
    m.extend_from_slice(&1u32.to_le_bytes());
    m.extend_from_slice(&FLAGS.to_le_bytes());
    m.extend_from_slice(&[0u8; 16]); // no domain, no workstation
    m
}

#[derive(Debug, Clone)]
pub struct Challenge {
    pub flags: u32,
    pub server_challenge: [u8; 8],
    /// The AV pairs, as sent, ending in MsvAvEOL.
    pub target_info: Vec<u8>,
    /// MsvAvTimestamp, if the server sent one.
    pub timestamp: Option<u64>,
}

fn field(m: &[u8], at: usize) -> Result<&[u8], Error> {
    let h = m.get(at..at + 8).ok_or(Error::Malformed("short"))?;
    let len = u16::from_le_bytes([h[0], h[1]]) as usize;
    let off = u32::from_le_bytes([h[4], h[5], h[6], h[7]]) as usize;
    if len == 0 {
        return Ok(&[]);
    }
    m.get(off..off.checked_add(len).ok_or(Error::Malformed("field"))?).ok_or(Error::Malformed("field outside the message"))
}

/// The server's challenge.
pub fn parse_challenge(m: &[u8]) -> Result<Challenge, Error> {
    if m.len() < 48 || &m[..8] != SIG {
        return Err(Error::Malformed("not NTLMSSP"));
    }
    if u32::from_le_bytes(m[8..12].try_into().unwrap()) != 2 {
        return Err(Error::Malformed("not a challenge"));
    }
    let flags = u32::from_le_bytes(m[20..24].try_into().unwrap());
    if flags & UNICODE == 0 {
        return Err(Error::Refused("the server won't use Unicode"));
    }
    if flags & TARGET_INFO == 0 {
        return Err(Error::Refused("no target information: NTLMv2 needs it"));
    }
    let server_challenge: [u8; 8] = m[24..32].try_into().unwrap();
    let ti = field(m, 40)?;
    if ti.len() > 4096 {
        return Err(Error::Malformed("target information too long"));
    }
    // Walk the AV pairs: well formed and ending in EOL.
    let mut timestamp = None;
    let mut at = 0;
    let mut end = None;
    while at + 4 <= ti.len() {
        let id = u16::from_le_bytes([ti[at], ti[at + 1]]);
        let len = u16::from_le_bytes([ti[at + 2], ti[at + 3]]) as usize;
        let v = ti.get(at + 4..at + 4 + len).ok_or(Error::Malformed("AV pair"))?;
        if id == AV_EOL {
            end = Some(at + 4);
            break;
        }
        if id == AV_TIMESTAMP && len == 8 {
            timestamp = Some(u64::from_le_bytes(v.try_into().unwrap()));
        }
        at += 4 + len;
    }
    let end = end.ok_or(Error::Malformed("AV pairs don't end"))?;
    Ok(Challenge { flags, server_challenge, target_info: ti[..end].to_vec(), timestamp })
}

/// NTOWFv2: HMAC-MD5 keyed by the NT hash over UPPER(user) || domain.
fn ntowf_v2(password: &[u8], user: &str, domain: &str) -> Zeroizing<[u8; 16]> {
    let pw = Zeroizing::new(utf16(&String::from_utf8_lossy(password)));
    let nt = Zeroizing::new(<[u8; 16]>::from(Md4::digest(&pw[..])));
    let mut h = <HmacMd5 as Mac>::new_from_slice(&nt[..]).unwrap();
    h.update(&utf16(&user.to_uppercase()));
    h.update(&utf16(domain));
    Zeroizing::new(h.finalize().into_bytes().into())
}

/// The third message and the session base key.
pub struct Authenticate {
    pub message: Vec<u8>,
    pub session_key: Zeroizing<[u8; 16]>,
}

/// `now` is the time as a FILETIME, used when the server sent none;
/// `client_challenge` is random.
pub fn authenticate(c: &Challenge, user: &str, domain: &str, password: &[u8], client_challenge: [u8; 8], now: u64) -> Authenticate {
    let key = ntowf_v2(password, user, domain);
    let mut temp = Vec::with_capacity(32 + c.target_info.len());
    temp.extend_from_slice(&[1, 1, 0, 0, 0, 0, 0, 0]);
    temp.extend_from_slice(&c.timestamp.unwrap_or(now).to_le_bytes());
    temp.extend_from_slice(&client_challenge);
    temp.extend_from_slice(&[0; 4]);
    temp.extend_from_slice(&c.target_info);
    temp.extend_from_slice(&[0; 4]);
    let mut h = <HmacMd5 as Mac>::new_from_slice(&key[..]).unwrap();
    h.update(&c.server_challenge);
    h.update(&temp);
    let proof: [u8; 16] = h.finalize().into_bytes().into();
    let mut h = <HmacMd5 as Mac>::new_from_slice(&key[..]).unwrap();
    h.update(&proof);
    let session_key = Zeroizing::new(h.finalize().into_bytes().into());
    let mut nt = proof.to_vec();
    nt.extend_from_slice(&temp);
    // With a server timestamp, MS-NLMP says send no LMv2: 24 zeros.
    let lm = [0u8; 24];
    let flags = FLAGS & c.flags | UNICODE | NTLM;
    let (d, u) = (utf16(domain), utf16(user));
    let parts: [&[u8]; 6] = [&lm, &nt, &d, &u, &[], &[]];
    let mut m = Vec::with_capacity(64 + lm.len() + nt.len() + d.len() + u.len());
    m.extend_from_slice(SIG);
    m.extend_from_slice(&3u32.to_le_bytes());
    let mut off = 64u32;
    for p in parts {
        m.extend_from_slice(&(p.len() as u16).to_le_bytes());
        m.extend_from_slice(&(p.len() as u16).to_le_bytes());
        m.extend_from_slice(&off.to_le_bytes());
        off += p.len() as u32;
    }
    m.extend_from_slice(&flags.to_le_bytes());
    for p in parts {
        m.extend_from_slice(p);
    }
    Authenticate { message: m, session_key }
}

// ---- SPNEGO (RFC 4178), the little of it NTLM needs ------------------------

/// 1.3.6.1.4.1.311.2.2.10
const NTLM_OID: &[u8] = &[0x2b, 0x06, 0x01, 0x04, 0x01, 0x82, 0x37, 0x02, 0x02, 0x0a];
/// 1.3.6.1.5.5.2
const SPNEGO_OID: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x02];

fn der(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut v = vec![tag];
    let n = body.len();
    if n < 0x80 {
        v.push(n as u8);
    } else if n < 0x100 {
        v.extend_from_slice(&[0x81, n as u8]);
    } else {
        v.extend_from_slice(&[0x82, (n >> 8) as u8, n as u8]);
    }
    v.extend_from_slice(body);
    v
}

fn tlv(b: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let tag = *b.first()?;
    let l0 = *b.get(1)?;
    let (len, hdr) = if l0 < 0x80 {
        (l0 as usize, 2)
    } else {
        let n = (l0 & 0x7f) as usize;
        if n == 0 || n > 3 {
            return None;
        }
        let mut len = 0usize;
        for i in 0..n {
            len = (len << 8) | *b.get(2 + i)? as usize;
        }
        (len, 2 + n)
    };
    Some((tag, b.get(hdr..hdr + len)?, b.get(hdr + len..)?))
}

/// The first session setup token: NegTokenInit offering NTLM, with the
/// NTLM negotiate message.
pub fn spnego_init(ntlm: &[u8]) -> Vec<u8> {
    let mech_types = der(0xa0, &der(0x30, &der(0x06, NTLM_OID)));
    let token = der(0xa2, &der(0x04, ntlm));
    let init = der(0xa0, &der(0x30, &[mech_types, token].concat()));
    der(0x60, &[der(0x06, SPNEGO_OID), init].concat())
}

/// Later tokens: NegTokenResp with the NTLM message.
pub fn spnego_resp(ntlm: &[u8]) -> Vec<u8> {
    der(0xa1, &der(0x30, &der(0xa2, &der(0x04, ntlm))))
}

/// The NTLM message in the server's NegTokenResp, and whether the server
/// chose NTLM (it must, if it says).
pub fn spnego_token(b: &[u8]) -> Result<Vec<u8>, Error> {
    // A raw NTLMSSP reply is taken as is.
    if b.starts_with(SIG) {
        return Ok(b.to_vec());
    }
    let (0xa1, resp, _) = tlv(b).ok_or(Error::Malformed("SPNEGO"))? else {
        return Err(Error::Malformed("not a NegTokenResp"));
    };
    let (0x30, mut seq, _) = tlv(resp).ok_or(Error::Malformed("SPNEGO"))? else {
        return Err(Error::Malformed("SPNEGO"));
    };
    let mut token = None;
    while let Some((tag, v, rest)) = tlv(seq) {
        seq = rest;
        match tag {
            0xa0 => {
                // negState: 2 is reject.
                if let Some((0x0a, s, _)) = tlv(v) {
                    if s == [2] {
                        return Err(Error::Refused("the server rejected the login"));
                    }
                }
            }
            0xa1 => {
                if let Some((0x06, oid, _)) = tlv(v) {
                    if oid != NTLM_OID {
                        return Err(Error::Refused("the server chose a mechanism other than NTLM"));
                    }
                }
            }
            0xa2 => {
                if let Some((0x04, t, _)) = tlv(v) {
                    token = Some(t.to_vec());
                }
            }
            _ => {}
        }
    }
    token.ok_or(Error::Malformed("no NTLM token"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn challenge(ti: &[u8], flags: u32) -> Vec<u8> {
        let mut m = Vec::new();
        m.extend_from_slice(SIG);
        m.extend_from_slice(&2u32.to_le_bytes());
        m.extend_from_slice(&[0, 0, 0, 0, 48, 0, 0, 0]); // no target name
        m.extend_from_slice(&flags.to_le_bytes());
        m.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        m.extend_from_slice(&[0; 8]);
        m.extend_from_slice(&(ti.len() as u16).to_le_bytes());
        m.extend_from_slice(&(ti.len() as u16).to_le_bytes());
        m.extend_from_slice(&48u32.to_le_bytes());
        m.extend_from_slice(ti);
        m
    }

    /// MS-NLMP 4.2.4's worked example: NTOWFv2 and the NTProofStr.
    #[test]
    fn ntlmv2_as_ms_nlmp_computes_it() {
        let key = ntowf_v2(b"Password", "User", "Domain");
        assert_eq!(&key[..], &[0x0c, 0x86, 0x8a, 0x40, 0x3b, 0xfd, 0x7a, 0x93, 0xa3, 0x00, 0x1e, 0xf2, 0x2e, 0xf0, 0x2e, 0x3f]);
        // The example's AV pairs: NbDomainName "Domain", NbComputerName "Server".
        let mut ti = Vec::new();
        for (id, v) in [(2u16, "Domain"), (1u16, "Server")] {
            let u = utf16(v);
            ti.extend_from_slice(&id.to_le_bytes());
            ti.extend_from_slice(&(u.len() as u16).to_le_bytes());
            ti.extend_from_slice(&u);
        }
        ti.extend_from_slice(&[0; 4]);
        let mut c = parse_challenge(&challenge(&ti, FLAGS)).unwrap();
        c.server_challenge = [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef];
        let a = authenticate(&c, "User", "Domain", b"Password", [0xaa; 8], 0);
        // NTProofStr (MS-NLMP 4.2.4.2.2).
        let nt_off = u32::from_le_bytes(a.message[24..28].try_into().unwrap()) as usize;
        assert_eq!(&a.message[nt_off..nt_off + 16], &[0x68, 0xcd, 0x0a, 0xb8, 0x51, 0xe5, 0x1c, 0x96, 0xaa, 0xbc, 0x92, 0x7b, 0xeb, 0xef, 0x6a, 0x1c]);
        // SessionBaseKey (4.2.4.1.2).
        assert_eq!(&a.session_key[..], &[0x8d, 0xe4, 0x0c, 0xca, 0xdb, 0xc1, 0x4a, 0x82, 0xf1, 0x5c, 0xb0, 0xad, 0x0d, 0xe9, 0x5c, 0xa3]);
    }

    #[test]
    fn hostile_challenges_are_refused() {
        let ti = [0u8; 4];
        assert!(parse_challenge(&challenge(&ti, FLAGS)).is_ok());
        assert!(matches!(parse_challenge(&challenge(&ti, FLAGS & !UNICODE)), Err(Error::Refused(_))));
        assert!(matches!(parse_challenge(&challenge(&ti, FLAGS & !TARGET_INFO)), Err(Error::Refused(_))));
        // AV pairs that run past the field, or never end.
        assert!(parse_challenge(&challenge(&[1, 0, 200, 0, 0, 0], FLAGS)).is_err());
        assert!(parse_challenge(&challenge(&[1, 0, 0, 0], FLAGS)).is_err());
        // A target info offset past the end.
        let mut m = challenge(&ti, FLAGS);
        m[44] = 0xff;
        assert!(parse_challenge(&m).is_err());
        assert!(parse_challenge(b"NTLMSSP\0").is_err());
        assert!(parse_challenge(&[]).is_err());
    }

    #[test]
    fn spnego_round_trip() {
        let n = negotiate();
        let init = spnego_init(&n);
        assert_eq!(init[0], 0x60);
        let resp = spnego_resp(b"NTLMSSP\0xyz");
        assert_eq!(spnego_token(&resp).unwrap(), b"NTLMSSP\0xyz");
        // A server choosing another mechanism, or rejecting.
        let other = der(0xa1, &der(0x30, &[der(0xa1, &der(0x06, SPNEGO_OID)), der(0xa2, &der(0x04, b"x"))].concat()));
        assert!(matches!(spnego_token(&other), Err(Error::Refused(_))));
        let reject = der(0xa1, &der(0x30, &der(0xa0, &der(0x0a, &[2]))));
        assert!(matches!(spnego_token(&reject), Err(Error::Refused(_))));
        assert!(spnego_token(&[0xa1, 0x05, 0x30]).is_err());
    }
}
