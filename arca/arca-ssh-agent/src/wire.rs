//! The ssh-agent protocol (draft-miller-ssh-agent), the two requests a
//! workspace may make: list identities and sign. Everything else fails.

pub const FAILURE: u8 = 5;
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

/// Answer one request (`type | payload`, unframed). `keys` lists the
/// workspace's granted keys (`None`: Arca refused or is locked); `sign`
/// asks Arca to sign with one of them.
pub fn answer(
    req: &[u8],
    keys: &mut dyn FnMut() -> Option<Vec<Key>>,
    sign: &mut dyn FnMut(&Key, &[u8]) -> Option<[u8; 64]>,
) -> Vec<u8> {
    let fail = vec![FAILURE];
    let Some((&t, rest)) = req.split_first() else { return fail };
    match t {
        REQUEST_IDENTITIES if rest.is_empty() => {
            // Locked or refused: no identities, so ssh moves on to its
            // other methods rather than failing.
            let ks = keys().unwrap_or_default();
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
            let Some(ks) = keys() else { return fail };
            let Some(k) = ks.iter().find(|k| key_blob(&k.pk) == blob) else { return fail };
            let Some(sig) = sign(k, data) else { return fail };
            let mut o = vec![SIGN_RESPONSE];
            put_string(&mut o, &sig_blob(&sig));
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

    #[test]
    fn lists_and_signs_only_granted_keys() {
        let sk = SigningKey::from_bytes(&[9; 32]);
        let k = Key { id: [1; 16], pk: sk.verifying_key().to_bytes(), comment: "alice@web".into() };
        let mut keys = || Some(vec![k.clone()]);
        let mut sign = |key: &Key, d: &[u8]| (key.id == [1; 16]).then(|| sk.sign(d).to_bytes());
        let a = answer(&[REQUEST_IDENTITIES], &mut keys, &mut sign);
        assert_eq!(a[0], IDENTITIES_ANSWER);
        assert_eq!(&a[1..5], &1u32.to_be_bytes());
        let mut r = R(&a[5..]);
        assert_eq!(r.string().unwrap(), key_blob(&k.pk).as_slice());
        assert_eq!(r.string().unwrap(), b"alice@web");

        let s = answer(&req_sign(&key_blob(&k.pk), b"data"), &mut keys, &mut sign);
        assert_eq!(s[0], SIGN_RESPONSE);
        let mut r = R(&s[1..]);
        let mut sr = R(r.string().unwrap());
        assert_eq!(sr.string().unwrap(), b"ssh-ed25519");
        let sig: [u8; 64] = sr.string().unwrap().try_into().unwrap();
        sk.verifying_key().verify(b"data", &ed25519_dalek::Signature::from_bytes(&sig)).unwrap();

        // A key it doesn't have, trailing bytes, an add-identity request,
        // a locked store: failure.
        assert_eq!(answer(&req_sign(&key_blob(&[3; 32]), b"d"), &mut keys, &mut sign), vec![FAILURE]);
        let mut bad = req_sign(&key_blob(&k.pk), b"d");
        bad.push(0);
        assert_eq!(answer(&bad, &mut keys, &mut sign), vec![FAILURE]);
        assert_eq!(answer(&[17, 0, 0, 0, 0], &mut keys, &mut sign), vec![FAILURE]);
        assert_eq!(answer(&[], &mut keys, &mut sign), vec![FAILURE]);
        let mut none = || None;
        assert_eq!(answer(&[REQUEST_IDENTITIES], &mut none, &mut sign), vec![IDENTITIES_ANSWER, 0, 0, 0, 0]);
    }
}
