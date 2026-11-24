//! MS-CHAPv2 (RFC 2759), only ever inside a TLS tunnel (PEAP, TTLS): the
//! NT-Response we send and the Authenticator Response we require back, so
//! a server that does not know the password cannot finish the login.

use des::cipher::{BlockEncrypt, KeyInit};
use md4::{Digest, Md4};
use sha1::Sha1;
use zeroize::Zeroizing;

/// NtPasswordHash: MD4 of the password in UTF-16LE.
pub fn password_hash(password: &str) -> Zeroizing<[u8; 16]> {
    let mut h = Md4::new();
    for u in password.encode_utf16() {
        h.update(u.to_le_bytes());
    }
    Zeroizing::new(h.finalize().into())
}

/// ChallengeHash: the first 8 bytes of SHA-1(peer || authenticator || user).
/// `user` is the name without any domain (RFC 2759 §8.2).
pub fn challenge_hash(peer: &[u8; 16], auth: &[u8; 16], user: &str) -> [u8; 8] {
    let mut h = Sha1::new();
    h.update(peer);
    h.update(auth);
    h.update(bare(user).as_bytes());
    let d = h.finalize();
    d[..8].try_into().unwrap()
}

/// The user name without a leading `DOMAIN\`.
pub fn bare(user: &str) -> &str {
    user.rsplit_once('\\').map_or(user, |(_, u)| u)
}

/// 7 key bytes spread over 8, the low bit of each left for parity.
fn des_key(k7: &[u8]) -> [u8; 8] {
    let mut k = [0u8; 8];
    k[0] = k7[0];
    k[1] = (k7[0] << 7) | (k7[1] >> 1);
    k[2] = (k7[1] << 6) | (k7[2] >> 2);
    k[3] = (k7[2] << 5) | (k7[3] >> 3);
    k[4] = (k7[3] << 4) | (k7[4] >> 4);
    k[5] = (k7[4] << 3) | (k7[5] >> 5);
    k[6] = (k7[5] << 2) | (k7[6] >> 6);
    k[7] = k7[6] << 1;
    k
}

/// ChallengeResponse: the challenge DES-encrypted under each third of the
/// zero-padded password hash.
fn challenge_response(challenge: &[u8; 8], hash: &[u8; 16]) -> [u8; 24] {
    let mut z = Zeroizing::new([0u8; 21]);
    z[..16].copy_from_slice(hash);
    let mut out = [0u8; 24];
    for i in 0..3 {
        let c = des::Des::new(&des_key(&z[i * 7..i * 7 + 7]).into());
        let mut b = (*challenge).into();
        c.encrypt_block(&mut b);
        out[i * 8..i * 8 + 8].copy_from_slice(&b);
    }
    out
}

/// GenerateNTResponse (RFC 2759 §8.1).
pub fn nt_response(auth: &[u8; 16], peer: &[u8; 16], user: &str, password: &str) -> [u8; 24] {
    challenge_response(&challenge_hash(peer, auth, user), &password_hash(password))
}

/// GenerateAuthenticatorResponse (RFC 2759 §8.7): the `S=` string a server
/// that knows the password sends back, 40 upper-case hex digits.
pub fn authenticator_response(password: &str, nt: &[u8; 24], peer: &[u8; 16], auth: &[u8; 16], user: &str) -> String {
    const MAGIC1: &[u8] = b"Magic server to client signing constant";
    const MAGIC2: &[u8] = b"Pad to make it do more than one iteration";
    let hh = Zeroizing::<[u8; 16]>::new(Md4::digest(*password_hash(password)).into());
    let mut h = Sha1::new();
    h.update(*hh);
    h.update(nt);
    h.update(MAGIC1);
    let d = h.finalize();
    let mut h = Sha1::new();
    h.update(d);
    h.update(challenge_hash(peer, auth, user));
    h.update(MAGIC2);
    let a = h.finalize();
    let mut s = String::from("S=");
    for b in a {
        s.push_str(&format!("{b:02X}"));
    }
    s
}

/// Whether a server's success message carries the authenticator response
/// we expect. Compared in constant time; `msg` is the server's text
/// (`S=<40 hex> M=...`).
pub fn check_success(msg: &[u8], want: &str) -> bool {
    let got = msg.get(..42).unwrap_or(&[]);
    if got.len() != 42 {
        return false;
    }
    let mut d = 0u8;
    for (a, b) in got.iter().zip(want.as_bytes()) {
        d |= a.to_ascii_uppercase() ^ b;
    }
    d == 0 && msg.get(42).is_none_or(|&c| c == b' ')
}

/// The PEAP inner session key (RFC 3079 §3.4, as hostapd and Windows take
/// it): the peer's send key then its receive key, 16 bytes each.
pub fn session_key(password: &str, nt: &[u8; 24]) -> Zeroizing<[u8; 32]> {
    const MAGIC1: &[u8] = b"This is the MPPE Master Key";
    const MAGIC2: &[u8] = b"On the client side, this is the send key; on the server side, it is the receive key.";
    const MAGIC3: &[u8] = b"On the client side, this is the receive key; on the server side, it is the send key.";
    let hh = Zeroizing::<[u8; 16]>::new(Md4::digest(*password_hash(password)).into());
    let mut h = Sha1::new();
    h.update(*hh);
    h.update(nt);
    h.update(MAGIC1);
    let master = Zeroizing::<[u8; 20]>::new(h.finalize().into());
    let mut out = Zeroizing::new([0u8; 32]);
    for (i, magic) in [MAGIC2, MAGIC3].into_iter().enumerate() {
        let mut h = Sha1::new();
        h.update(&master[..16]);
        h.update([0u8; 40]);
        h.update(magic);
        h.update([0xf2u8; 40]);
        out[i * 16..i * 16 + 16].copy_from_slice(&h.finalize()[..16]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    /// RFC 2759 §9.2.
    #[test]
    fn rfc2759_vectors() {
        let auth: [u8; 16] = h("5B5D7C7D7B3F2F3E3C2C602132262628").try_into().unwrap();
        let peer: [u8; 16] = h("21402324255E262A28295F2B3A337C7E").try_into().unwrap();
        assert_eq!(challenge_hash(&peer, &auth, "User").to_vec(), h("D02E4386BCE91226"));
        assert_eq!(password_hash("clientPass").to_vec(), h("44EBBA8D5312B8D611474411F56989AE"));
        let nt = nt_response(&auth, &peer, "User", "clientPass");
        assert_eq!(nt.to_vec(), h("82309ECD8D708B5EA08FAA3981CD83544233114A3D85D6DF"));
        let a = authenticator_response("clientPass", &nt, &peer, &auth, "User");
        assert_eq!(a, "S=407A5589115FD0D6209F510FE9C04566932CDA56");
        assert!(check_success(b"S=407A5589115FD0D6209F510FE9C04566932CDA56 M=welcome", &a));
        assert!(check_success(b"S=407a5589115fd0d6209f510fe9c04566932cda56", &a));
        assert!(!check_success(b"S=407A5589115FD0D6209F510FE9C04566932CDA57", &a));
        assert!(!check_success(b"S=407A", &a));
        assert_eq!(challenge_hash(&peer, &auth, "CORP\\User"), challenge_hash(&peer, &auth, "User"));
        // RFC 3079 §3.5.3's SendStartKey128 is the server's send key, so
        // our receive key (the second half).
        assert_eq!(session_key("clientPass", &nt)[16..].to_vec(), h("8B7CDC149B993A1BA118CB153F56DCCB"));
    }
}
