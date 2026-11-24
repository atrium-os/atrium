//! The ssh-agent protocol (draft-miller-ssh-agent), the three requests a
//! workspace may make: list identities, sign, and Atrium's own extension
//! [`GIT_CREDENTIAL`] (an HTTPS token granted to it, for git's credential
//! helper). Everything else fails.

pub const FAILURE: u8 = 5;
pub const SUCCESS: u8 = 6;
pub const EXTENSION: u8 = 27;
/// `string "git-credential@atrium" | string host` → `SUCCESS | string
/// username | string token`; FAILURE if no token for that host is granted.
pub const GIT_CREDENTIAL: &[u8] = b"git-credential@atrium";
pub const REQUEST_IDENTITIES: u8 = 11;
pub const IDENTITIES_ANSWER: u8 = 12;
pub const SIGN_REQUEST: u8 = 13;
pub const SIGN_RESPONSE: u8 = 14;

/// Longest request the agent reads. A sign request is a key blob and a
/// session's user-auth data or an SSHSIG blob (a hash): small.
pub const MAX_MSG: usize = 16 * 1024;

const ED25519: &[u8] = b"ssh-ed25519";
const ECDSA_P256: &[u8] = b"ecdsa-sha2-nistp256";
const NISTP256: &[u8] = b"nistp256";

fn put_string(o: &mut Vec<u8>, b: &[u8]) {
    o.extend_from_slice(&(b.len() as u32).to_be_bytes());
    o.extend_from_slice(b);
}

/// The public key blob (RFC 8709, RFC 5656): `string "ssh-ed25519" |
/// string pk`, or `string "ecdsa-sha2-nistp256" | string "nistp256" |
/// string Q` with Q the uncompressed point.
pub fn key_blob(k: &Key) -> Vec<u8> {
    let mut o = Vec::with_capacity(104);
    if k.p256 {
        put_string(&mut o, ECDSA_P256);
        put_string(&mut o, NISTP256);
    } else {
        put_string(&mut o, ED25519);
    }
    put_string(&mut o, &k.pk);
    o
}

/// An SSH mpint of a 32-byte big-endian unsigned integer: leading zeros
/// dropped, a zero byte added when the top bit is set.
fn put_mpint(o: &mut Vec<u8>, n: &[u8]) {
    let n = &n[n.iter().position(|b| *b != 0).unwrap_or(n.len())..];
    if n.first().is_some_and(|b| b & 0x80 != 0) {
        o.extend_from_slice(&(n.len() as u32 + 1).to_be_bytes());
        o.push(0);
    } else {
        o.extend_from_slice(&(n.len() as u32).to_be_bytes());
    }
    o.extend_from_slice(n);
}

/// The signature blob: `string "ssh-ed25519" | string sig`, or
/// `string "ecdsa-sha2-nistp256" | string (mpint r | mpint s)` from
/// Arca's `r[32] | s[32]`.
pub fn sig_blob(k: &Key, sig: &[u8; 64]) -> Vec<u8> {
    let mut o = Vec::with_capacity(100);
    if k.p256 {
        put_string(&mut o, ECDSA_P256);
        let mut rs = Vec::with_capacity(74);
        put_mpint(&mut rs, &sig[..32]);
        put_mpint(&mut rs, &sig[32..]);
        put_string(&mut o, &rs);
    } else {
        put_string(&mut o, ED25519);
        put_string(&mut o, sig);
    }
    o
}

/// The `r[32] | s[32]` of an `ecdsa-sha2-nistp256` signature blob (what a
/// client such as opifex turns back into DER). `None` for anything else.
pub fn ecdsa_rs(sig_blob: &[u8]) -> Option<[u8; 64]> {
    let mut r = R(sig_blob);
    (r.string()? == ECDSA_P256).then_some(())?;
    let mut rs = R(r.string()?);
    r.0.is_empty().then_some(())?;
    let mut out = [0u8; 64];
    for half in out.chunks_mut(32) {
        let n = rs.string()?;
        let n = &n[n.iter().position(|b| *b != 0).unwrap_or(n.len())..];
        if n.len() > 32 {
            return None;
        }
        half[32 - n.len()..].copy_from_slice(n);
    }
    rs.0.is_empty().then_some(out)
}

/// The uncompressed point of an `ecdsa-sha2-nistp256` key blob.
pub fn ecdsa_point(key_blob: &[u8]) -> Option<Vec<u8>> {
    let mut r = R(key_blob);
    (r.string()? == ECDSA_P256 && r.string()? == NISTP256).then_some(())?;
    let q = r.string()?;
    (r.0.is_empty() && q.len() == 65 && q[0] == 4).then(|| q.to_vec())
}

/// A sign request for `blob` over `data`, flags 0 (what a client sends).
pub fn sign_request(blob: &[u8], data: &[u8]) -> Vec<u8> {
    let mut o = vec![SIGN_REQUEST];
    put_string(&mut o, blob);
    put_string(&mut o, data);
    o.extend_from_slice(&0u32.to_be_bytes());
    o
}

/// The signature blob in a reply to [`sign_request`].
pub fn sign_answer(reply: &[u8]) -> Option<Vec<u8>> {
    let (&t, rest) = reply.split_first()?;
    (t == SIGN_RESPONSE).then_some(())?;
    let mut r = R(rest);
    let b = r.string()?;
    r.0.is_empty().then(|| b.to_vec())
}

/// The key blobs and comments in a reply to `REQUEST_IDENTITIES`.
pub fn identities_answer(reply: &[u8]) -> Option<Vec<(Vec<u8>, String)>> {
    let (&t, rest) = reply.split_first()?;
    (t == IDENTITIES_ANSWER).then_some(())?;
    let mut r = R(rest);
    let n = r.u32()? as usize;
    let mut v = Vec::with_capacity(n.min(64));
    for _ in 0..n {
        let b = r.string()?.to_vec();
        let c = String::from_utf8_lossy(r.string()?).into_owned();
        v.push((b, c));
    }
    r.0.is_empty().then_some(v)
}

struct R<'a>(&'a [u8]);

impl<'a> R<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }
    fn string(&mut self) -> Option<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }
}

/// A key the asking workspace may use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    pub id: [u8; 16],
    /// P-256 (`ecdsa-sha2-nistp256`, pk the 65-byte point) or Ed25519 (32).
    pub p256: bool,
    pub pk: Vec<u8>,
    pub comment: String,
}

/// Frame a reply: `u32 len | body`.
pub fn frame(body: &[u8]) -> Vec<u8> {
    let mut o = Vec::with_capacity(4 + body.len());
    o.extend_from_slice(&(body.len() as u32).to_be_bytes());
    o.extend_from_slice(body);
    o
}

/// The request git-credential-arca sends.
pub fn credential_request(host: &str) -> Vec<u8> {
    let mut o = vec![EXTENSION];
    put_string(&mut o, GIT_CREDENTIAL);
    put_string(&mut o, host.as_bytes());
    o
}

/// Its answer: the username and token, or `None` (none granted, locked).
pub fn credential_answer(reply: &[u8]) -> Option<(String, Vec<u8>)> {
    let (&t, rest) = reply.split_first()?;
    if t != SUCCESS {
        return None;
    }
    let mut r = R(rest);
    let (u, tok) = (r.string()?, r.string()?);
    r.0.is_empty().then_some(())?;
    Some((String::from_utf8(u.to_vec()).ok()?, tok.to_vec()))
}

/// The callbacks [`answer`] uses, each asking Arca for this workspace.
pub trait Ask {
    /// Its granted keys (`None`: Arca refused or is locked).
    fn keys(&mut self) -> Option<Vec<Key>>;
    /// Sign with one of them.
    fn sign(&mut self, k: &Key, data: &[u8]) -> Option<[u8; 64]>;
    /// The token for `host` granted to it.
    fn token(&mut self, host: &str) -> Option<(String, Vec<u8>)>;
}

/// Answer one request (`type | payload`, unframed).
pub fn answer(req: &[u8], ask: &mut dyn Ask) -> Vec<u8> {
    let fail = vec![FAILURE];
    let Some((&t, rest)) = req.split_first() else { return fail };
    match t {
        REQUEST_IDENTITIES if rest.is_empty() => {
            // Locked or refused: no identities, so ssh moves on to its
            // other methods rather than failing.
            let ks = ask.keys().unwrap_or_default();
            let mut o = vec![IDENTITIES_ANSWER];
            o.extend_from_slice(&(ks.len() as u32).to_be_bytes());
            for k in &ks {
                put_string(&mut o, &key_blob(k));
                put_string(&mut o, k.comment.as_bytes());
            }
            o
        }
        SIGN_REQUEST => {
            let mut r = R(rest);
            let (Some(blob), Some(data), Some(_flags)) = (r.string(), r.string(), r.u32()) else { return fail };
            if !r.0.is_empty() {
                return fail;
            }
            let Some(ks) = ask.keys() else { return fail };
            let Some(k) = ks.iter().find(|k| key_blob(k) == blob) else { return fail };
            let Some(sig) = ask.sign(k, data) else { return fail };
            let mut o = vec![SIGN_RESPONSE];
            put_string(&mut o, &sig_blob(k, &sig));
            o
        }
        EXTENSION => {
            let mut r = R(rest);
            let (Some(name), Some(host)) = (r.string(), r.string()) else { return fail };
            if name != GIT_CREDENTIAL || !r.0.is_empty() {
                return fail;
            }
            let Some(host) = std::str::from_utf8(host).ok().filter(|h| arcad::ssh::valid_host(h)) else { return fail };
            let Some((user, tok)) = ask.token(host) else { return fail };
            let mut o = vec![SUCCESS];
            put_string(&mut o, user.as_bytes());
            put_string(&mut o, &tok);
            o
        }
        _ => fail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey, Verifier};

    fn req_sign(blob: &[u8], data: &[u8]) -> Vec<u8> {
        sign_request(blob, data)
    }

    struct T {
        sk: SigningKey,
        k: Option<Key>,
    }

    impl Ask for T {
        fn keys(&mut self) -> Option<Vec<Key>> {
            self.k.clone().map(|k| vec![k])
        }
        fn sign(&mut self, key: &Key, d: &[u8]) -> Option<[u8; 64]> {
            (key.id == [1; 16]).then(|| self.sk.sign(d).to_bytes())
        }
        fn token(&mut self, host: &str) -> Option<(String, Vec<u8>)> {
            (host == "github.com").then(|| ("alice".into(), b"ghp_x".to_vec()))
        }
    }

    #[test]
    fn hands_over_only_a_granted_token() {
        let mut t = T { sk: SigningKey::from_bytes(&[9; 32]), k: None };
        let a = answer(&credential_request("github.com"), &mut t);
        assert_eq!(credential_answer(&a), Some(("alice".into(), b"ghp_x".to_vec())));
        assert_eq!(answer(&credential_request("gitlab.com"), &mut t), vec![FAILURE]);
        assert_eq!(answer(&credential_request("GitHub.com:443"), &mut t), vec![FAILURE]);
        let mut other = vec![EXTENSION];
        put_string(&mut other, b"session-bind@openssh.com");
        put_string(&mut other, b"github.com");
        assert_eq!(answer(&other, &mut t), vec![FAILURE]);
        assert_eq!(credential_answer(&[FAILURE]), None);
    }

    #[test]
    fn lists_and_signs_only_granted_keys() {
        let sk = SigningKey::from_bytes(&[9; 32]);
        let k = Key { id: [1; 16], p256: false, pk: sk.verifying_key().to_bytes().to_vec(), comment: "alice@web".into() };
        let mut t = T { sk: sk.clone(), k: Some(k.clone()) };
        let a = answer(&[REQUEST_IDENTITIES], &mut t);
        assert_eq!(a[0], IDENTITIES_ANSWER);
        assert_eq!(&a[1..5], &1u32.to_be_bytes());
        let mut r = R(&a[5..]);
        assert_eq!(r.string().unwrap(), key_blob(&k).as_slice());
        assert_eq!(r.string().unwrap(), b"alice@web");

        let s = answer(&req_sign(&key_blob(&k), b"data"), &mut t);
        assert_eq!(s[0], SIGN_RESPONSE);
        let mut r = R(&s[1..]);
        let mut sr = R(r.string().unwrap());
        assert_eq!(sr.string().unwrap(), b"ssh-ed25519");
        let sig: [u8; 64] = sr.string().unwrap().try_into().unwrap();
        sk.verifying_key().verify(b"data", &ed25519_dalek::Signature::from_bytes(&sig)).unwrap();

        // A key it doesn't have, trailing bytes, an add-identity request,
        // a locked store: failure.
        let other = Key { pk: vec![3; 32], ..k.clone() };
        assert_eq!(answer(&req_sign(&key_blob(&other), b"d"), &mut t), vec![FAILURE]);
        let mut bad = req_sign(&key_blob(&k), b"d");
        bad.push(0);
        assert_eq!(answer(&bad, &mut t), vec![FAILURE]);
        assert_eq!(answer(&[17, 0, 0, 0, 0], &mut t), vec![FAILURE]);
        assert_eq!(answer(&[], &mut t), vec![FAILURE]);
        t.k = None;
        assert_eq!(answer(&[REQUEST_IDENTITIES], &mut t), vec![IDENTITIES_ANSWER, 0, 0, 0, 0]);
    }

    #[test]
    fn p256_keys_list_and_sign_as_ecdsa_sha2_nistp256() {
        use p256::ecdsa::{signature::{Signer as _, Verifier as _}, Signature, SigningKey as EcKey};
        struct E(EcKey, Key);
        impl Ask for E {
            fn keys(&mut self) -> Option<Vec<Key>> { Some(vec![self.1.clone()]) }
            fn sign(&mut self, _: &Key, d: &[u8]) -> Option<[u8; 64]> {
                let s: Signature = self.0.sign(d);
                Some(s.to_bytes().into())
            }
            fn token(&mut self, _: &str) -> Option<(String, Vec<u8>)> { None }
        }
        let sk = EcKey::from_slice(&[7; 32]).unwrap();
        let pk = sk.verifying_key().to_encoded_point(false).as_bytes().to_vec();
        let k = Key { id: [2; 16], p256: true, pk: pk.clone(), comment: "publisher".into() };
        let mut e = E(sk.clone(), k.clone());
        let ids = identities_answer(&answer(&[REQUEST_IDENTITIES], &mut e)).unwrap();
        assert_eq!(ids, vec![(key_blob(&k), "publisher".to_string())]);
        assert_eq!(ecdsa_point(&ids[0].0), Some(pk));
        assert_eq!(&ids[0].0[4..23], b"ecdsa-sha2-nistp256");
        // Many messages, so some r or s has its top bit set (an extra zero
        // in the mpint) and some may start with a zero byte.
        for i in 0..32u8 {
            let msg = [i; 40];
            let blob = sign_answer(&answer(&sign_request(&key_blob(&k), &msg), &mut e)).unwrap();
            let rs = ecdsa_rs(&blob).unwrap();
            sk.verifying_key().verify(&msg, &Signature::from_slice(&rs).unwrap()).unwrap();
        }
        assert_eq!(ecdsa_rs(&sig_blob(&Key { p256: false, ..k.clone() }, &[1; 64])), None, "not ECDSA");
        assert_eq!(put_mpint_of(&[0x80; 32]).len(), 4 + 33);
        assert_eq!(put_mpint_of(&{ let mut z = [0u8; 32]; z[31] = 1; z }), vec![0, 0, 0, 1, 1]);
    }

    fn put_mpint_of(n: &[u8]) -> Vec<u8> {
        let mut o = Vec::new();
        put_mpint(&mut o, n);
        o
    }
}
