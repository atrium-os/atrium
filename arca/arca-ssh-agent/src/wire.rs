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

fn put_string(o: &mut Vec<u8>, b: &[u8]) {
    o.extend_from_slice(&(b.len() as u32).to_be_bytes());
    o.extend_from_slice(b);
}

/// The public key blob: `string "ssh-ed25519" | string pk`.
pub fn key_blob(pk: &[u8; 32]) -> Vec<u8> {
    let mut o = Vec::with_capacity(51);
    put_string(&mut o, ED25519);
    put_string(&mut o, pk);
    o
}

/// The signature blob: `string "ssh-ed25519" | string sig`.
pub fn sig_blob(sig: &[u8; 64]) -> Vec<u8> {
    let mut o = Vec::with_capacity(83);
    put_string(&mut o, ED25519);
    put_string(&mut o, sig);
    o
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
    pub pk: [u8; 32],
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
                put_string(&mut o, &key_blob(&k.pk));
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
            let Some(k) = ks.iter().find(|k| key_blob(&k.pk) == blob) else { return fail };
            let Some(sig) = ask.sign(k, data) else { return fail };
            let mut o = vec![SIGN_RESPONSE];
            put_string(&mut o, &sig_blob(&sig));
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
        let mut o = vec![SIGN_REQUEST];
        put_string(&mut o, blob);
        put_string(&mut o, data);
        o.extend_from_slice(&0u32.to_be_bytes());
        o
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
        let k = Key { id: [1; 16], pk: sk.verifying_key().to_bytes(), comment: "alice@web".into() };
        let mut t = T { sk: sk.clone(), k: Some(k.clone()) };
        let a = answer(&[REQUEST_IDENTITIES], &mut t);
        assert_eq!(a[0], IDENTITIES_ANSWER);
        assert_eq!(&a[1..5], &1u32.to_be_bytes());
        let mut r = R(&a[5..]);
        assert_eq!(r.string().unwrap(), key_blob(&k.pk).as_slice());
        assert_eq!(r.string().unwrap(), b"alice@web");

        let s = answer(&req_sign(&key_blob(&k.pk), b"data"), &mut t);
        assert_eq!(s[0], SIGN_RESPONSE);
        let mut r = R(&s[1..]);
        let mut sr = R(r.string().unwrap());
        assert_eq!(sr.string().unwrap(), b"ssh-ed25519");
        let sig: [u8; 64] = sr.string().unwrap().try_into().unwrap();
        sk.verifying_key().verify(b"data", &ed25519_dalek::Signature::from_bytes(&sig)).unwrap();

        // A key it doesn't have, trailing bytes, an add-identity request,
        // a locked store: failure.
        assert_eq!(answer(&req_sign(&key_blob(&[3; 32]), b"d"), &mut t), vec![FAILURE]);
        let mut bad = req_sign(&key_blob(&k.pk), b"d");
        bad.push(0);
        assert_eq!(answer(&bad, &mut t), vec![FAILURE]);
        assert_eq!(answer(&[17, 0, 0, 0, 0], &mut t), vec![FAILURE]);
        assert_eq!(answer(&[], &mut t), vec![FAILURE]);
        t.k = None;
        assert_eq!(answer(&[REQUEST_IDENTITIES], &mut t), vec![IDENTITIES_ANSWER, 0, 0, 0, 0]);
    }
}
