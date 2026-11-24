//! Keys in Arca, reached through an SSH agent (arca.md §9.1): a workspace's
//! `SSH_AUTH_SOCK` is Arca's agent, which offers the P-256 keys granted to
//! that workspace as `ecdsa-sha2-nistp256`. An agent's ECDSA signature is
//! ECDSA P-256 over SHA-256 of the data, which is what a bundle's
//! `atrium.toml.sig` is, so a publisher key can sign bundles without its
//! private half ever reaching the workspace.
//!
//! The key file names the key by its public line:
//!
//! ```text
//! # A key in Arca, used through the SSH agent (opifex token agent).
//! atrium-agent-key = 1
//! public = "ecdsa-sha2-nistp256 AAAA... publisher"
//! ```
//!
//! Any agent holding that key works (OpenSSH's `ssh-agent` too, off Atrium).

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use base64::Engine;
use p256::ecdsa::{signature::Verifier, Signature, VerifyingKey};
use p256::pkcs8::{EncodePublicKey, LineEnding};
use serde::Deserialize;

const ECDSA: &[u8] = b"ecdsa-sha2-nistp256";
const CURVE: &[u8] = b"nistp256";
const REQUEST_IDENTITIES: u8 = 11;
const IDENTITIES_ANSWER: u8 = 12;
const SIGN_REQUEST: u8 = 13;
const SIGN_RESPONSE: u8 = 14;
/// Longest reply read from an agent.
const MAX_REPLY: usize = 256 * 1024;
/// A signature can wait on a security-key touch in Sigillum's sheet.
const SIGN_WAIT: Duration = Duration::from_secs(11 * 60 + 5);

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AgentRef {
    #[serde(rename = "atrium-agent-key")]
    pub format: u32,
    /// `ecdsa-sha2-nistp256 BASE64 [comment]`.
    pub public: String,
}

/// A P-256 key an agent offers: its blob, point and comment.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentKey {
    pub blob: Vec<u8>,
    pub point: Vec<u8>,
    pub comment: String,
}

impl AgentKey {
    pub fn line(&self) -> String {
        let b = base64::engine::general_purpose::STANDARD.encode(&self.blob);
        if self.comment.is_empty() { format!("ecdsa-sha2-nistp256 {b}") } else { format!("ecdsa-sha2-nistp256 {b} {}", self.comment) }
    }

    /// The SPKI PEM a publisher entry (`/etc/atrium/publishers/*.pem`) holds.
    pub fn pem(&self) -> Result<String, String> {
        let vk = VerifyingKey::from_sec1_bytes(&self.point).map_err(|_| "not a P-256 point".to_string())?;
        vk.to_public_key_pem(LineEnding::LF).map_err(|e| e.to_string())
    }

    pub fn text(&self) -> String {
        format!("# A key in Arca, used through the SSH agent (opifex token agent).\n\
                 atrium-agent-key = 1\npublic = {:?}\n", self.line())
    }
}

impl AgentRef {
    /// `None` when the text isn't an agent key reference at all.
    pub fn parse(text: &str) -> Option<Result<AgentKey, String>> {
        if !text.lines().any(|l| l.trim_start().starts_with("atrium-agent-key")) {
            return None;
        }
        Some((|| {
            let r: AgentRef = toml::from_str(text).map_err(|e| format!("agent key file: {e}"))?;
            if r.format != 1 {
                return Err(format!("agent key file: format {} (this opifex reads 1)", r.format));
            }
            let mut f = r.public.split_ascii_whitespace();
            let (Some(algo), Some(b64)) = (f.next(), f.next()) else {
                return Err("agent key file: public is `ecdsa-sha2-nistp256 BASE64 [comment]`".into());
            };
            let blob = base64::engine::general_purpose::STANDARD.decode(b64)
                .map_err(|_| "agent key file: the public key isn't base64".to_string())?;
            let point = point(&blob).filter(|_| algo.as_bytes() == ECDSA)
                .ok_or("agent key file: not an ecdsa-sha2-nistp256 key (bundles are signed with P-256)")?;
            Ok(AgentKey { blob, point, comment: f.collect::<Vec<_>>().join(" ") })
        })())
    }
}

fn put_string(o: &mut Vec<u8>, b: &[u8]) {
    o.extend_from_slice(&(b.len() as u32).to_be_bytes());
    o.extend_from_slice(b);
}

struct R<'a>(&'a [u8]);

impl<'a> R<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        (self.0.len() >= n).then(|| {
            let (a, b) = self.0.split_at(n);
            self.0 = b;
            a
        })
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }
    fn string(&mut self) -> Option<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }
}

/// The uncompressed point of an `ecdsa-sha2-nistp256` key blob.
fn point(blob: &[u8]) -> Option<Vec<u8>> {
    let mut r = R(blob);
    (r.string()? == ECDSA && r.string()? == CURVE).then_some(())?;
    let q = r.string()?;
    (r.0.is_empty() && q.len() == 65 && q[0] == 4).then(|| q.to_vec())
}

/// An `ecdsa-sha2-nistp256` signature blob as a P-256 signature.
fn signature(blob: &[u8]) -> Option<Signature> {
    let mut r = R(blob);
    (r.string()? == ECDSA).then_some(())?;
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
    rs.0.is_empty().then_some(())?;
    Signature::from_slice(&out).ok()
}

fn socket() -> Result<String, String> {
    std::env::var("SSH_AUTH_SOCK").ok().filter(|s| !s.is_empty())
        .ok_or_else(|| "no SSH agent (SSH_AUTH_SOCK isn't set; in a workspace it is Arca's)".into())
}

/// One request, one reply.
fn ask(req: &[u8], wait: Duration) -> Result<Vec<u8>, String> {
    let path = socket()?;
    let mut s = UnixStream::connect(&path).map_err(|e| format!("SSH agent {path}: {e}"))?;
    s.set_read_timeout(Some(wait)).and_then(|_| s.set_write_timeout(Some(Duration::from_secs(10))))
        .map_err(|e| e.to_string())?;
    let mut framed = Vec::with_capacity(4 + req.len());
    put_string(&mut framed, req);
    s.write_all(&framed).map_err(|e| format!("SSH agent: {e}"))?;
    let mut n = [0u8; 4];
    s.read_exact(&mut n).map_err(|e| format!("SSH agent: {e}"))?;
    let n = u32::from_be_bytes(n) as usize;
    if n == 0 || n > MAX_REPLY {
        return Err(format!("SSH agent: a {n}-byte reply"));
    }
    let mut b = vec![0u8; n];
    s.read_exact(&mut b).map_err(|e| format!("SSH agent: {e}"))?;
    Ok(b)
}

/// The P-256 keys the agent offers (in a workspace: those granted to it).
pub fn keys() -> Result<Vec<AgentKey>, String> {
    let b = ask(&[REQUEST_IDENTITIES], Duration::from_secs(10))?;
    let bad = || "SSH agent: a malformed identities answer".to_string();
    let (&t, rest) = b.split_first().ok_or_else(bad)?;
    if t != IDENTITIES_ANSWER {
        return Err("SSH agent: it refused to list keys".into());
    }
    let mut r = R(rest);
    let n = r.u32().ok_or_else(bad)?;
    let mut v = Vec::new();
    for _ in 0..n {
        let (blob, comment) = (r.string().ok_or_else(bad)?, r.string().ok_or_else(bad)?);
        if let Some(point) = point(blob) {
            v.push(AgentKey { blob: blob.to_vec(), point, comment: String::from_utf8_lossy(comment).into_owned() });
        }
    }
    Ok(v)
}

/// Sign `msg` with `k` through the agent: a DER ECDSA signature, checked
/// against the key before it is returned.
pub fn sign(k: &AgentKey, msg: &[u8]) -> Result<Vec<u8>, String> {
    let mut req = vec![SIGN_REQUEST];
    put_string(&mut req, &k.blob);
    put_string(&mut req, msg);
    req.extend_from_slice(&0u32.to_be_bytes());
    let what = format!("Arca key {:?}", k.comment);
    let b = ask(&req, SIGN_WAIT)?;
    let (&t, rest) = b.split_first().ok_or("SSH agent: an empty answer")?;
    if t != SIGN_RESPONSE {
        return Err(format!("{what}: the agent refused to sign (not granted to this workspace, or Arca is locked)"));
    }
    let mut r = R(rest);
    let sig = r.string().filter(|_| r.0.is_empty()).and_then(signature)
        .ok_or_else(|| format!("{what}: a malformed signature"))?;
    let vk = VerifyingKey::from_sec1_bytes(&k.point).map_err(|_| format!("{what}: not a P-256 point"))?;
    vk.verify(msg, &sig).map_err(|_| format!("{what}: the agent's signature doesn't verify"))?;
    Ok(sig.to_der().as_bytes().to_vec())
}

pub(crate) fn sign_ref(msg: &[u8], text: &str) -> Option<Result<Vec<u8>, String>> {
    Some(AgentRef::parse(text)?.and_then(|k| {
        eprintln!("opifex: signing with {} through the SSH agent", if k.comment.is_empty() { "the Arca key" } else { &k.comment });
        sign(&k, msg)
    }))
}

pub(crate) fn public_ref(text: &str) -> Option<Result<String, String>> {
    Some(AgentRef::parse(text)?.and_then(|k| k.pem()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::{signature::Signer, SigningKey};
    use std::os::unix::net::UnixListener;

    fn mpint(o: &mut Vec<u8>, n: &[u8]) {
        let n = &n[n.iter().position(|b| *b != 0).unwrap_or(n.len())..];
        let pad = n.first().is_some_and(|b| b & 0x80 != 0);
        o.extend_from_slice(&((n.len() + pad as usize) as u32).to_be_bytes());
        if pad {
            o.push(0);
        }
        o.extend_from_slice(n);
    }

    /// A one-key agent on a socket: answers list and sign, `refuse` signs nothing.
    fn agent(dir: &std::path::Path, sk: SigningKey, blob: Vec<u8>, refuse: bool) -> std::thread::JoinHandle<()> {
        let path = dir.join("agent.sock");
        let l = UnixListener::bind(&path).unwrap();
        std::env::set_var("SSH_AUTH_SOCK", &path);
        std::thread::spawn(move || {
            for c in l.incoming().take(3) {
                let mut c = c.unwrap();
                let mut n = [0u8; 4];
                c.read_exact(&mut n).unwrap();
                let mut req = vec![0u8; u32::from_be_bytes(n) as usize];
                c.read_exact(&mut req).unwrap();
                let mut o = Vec::new();
                match req[0] {
                    REQUEST_IDENTITIES => {
                        o.push(IDENTITIES_ANSWER);
                        o.extend_from_slice(&2u32.to_be_bytes());
                        put_string(&mut o, b"\0\0\0\x0bssh-ed25519\0\0\0\x20aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
                        put_string(&mut o, b"an ssh key");
                        put_string(&mut o, &blob);
                        put_string(&mut o, b"publisher");
                    }
                    SIGN_REQUEST if !refuse && R(&req[1..]).string() == Some(blob.as_slice()) => {
                        let mut r = R(&req[1..]);
                        r.string();
                        let sig: Signature = sk.sign(r.string().unwrap());
                        let b = sig.to_bytes();
                        let mut rs = Vec::new();
                        mpint(&mut rs, &b[..32]);
                        mpint(&mut rs, &b[32..]);
                        let mut sb = Vec::new();
                        put_string(&mut sb, ECDSA);
                        put_string(&mut sb, &rs);
                        o.push(SIGN_RESPONSE);
                        put_string(&mut o, &sb);
                    }
                    _ => o.push(5),
                }
                let mut f = Vec::new();
                put_string(&mut f, &o);
                c.write_all(&f).unwrap();
            }
        })
    }

    fn blob_of(sk: &SigningKey) -> Vec<u8> {
        let mut b = Vec::new();
        put_string(&mut b, ECDSA);
        put_string(&mut b, CURVE);
        put_string(&mut b, sk.verifying_key().to_encoded_point(false).as_bytes());
        b
    }

    // One test, since SSH_AUTH_SOCK is process-wide.
    #[test]
    fn signs_a_manifest_through_the_agent_and_the_platform_verifies_it() {
        let d = std::env::temp_dir().join(format!("agent-key-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let sk = SigningKey::from_slice(&[5; 32]).unwrap();
        let t = agent(&d, sk.clone(), blob_of(&sk), false);
        let ks = keys().unwrap();
        assert_eq!(ks.len(), 1, "only the P-256 key: {ks:?}");
        let k = &ks[0];
        assert_eq!(k.comment, "publisher");
        let file = k.text();
        let parsed = AgentRef::parse(&file).unwrap().unwrap();
        assert_eq!(&parsed, k);
        let manifest = b"[app]\nid = \"org.example.app\"\n";
        let der = sign_ref(manifest, &file).unwrap().unwrap();
        let pem = public_ref(&file).unwrap().unwrap();
        portcullis_sig::verify_one(manifest, &der, &pem).unwrap();
        assert!(portcullis_sig::verify_one(b"other", &der, &pem).is_err());
        // Another key's file: the agent's signature doesn't match it.
        let other = SigningKey::from_slice(&[6; 32]).unwrap();
        let wrong = AgentKey { blob: blob_of(&other), point: other.verifying_key().to_encoded_point(false).as_bytes().to_vec(), comment: String::new() };
        let e = sign(&wrong, manifest).unwrap_err();
        assert!(e.contains("refused"), "a key the agent doesn't hold: {e}");
        t.join().unwrap();

        // A refusing agent: a clear error, no signature.
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let t = agent(&d, sk.clone(), blob_of(&sk), true);
        let e = sign(k, manifest).unwrap_err();
        assert!(e.contains("refused"), "{e}");
        let _ = keys();
        let _ = keys();
        t.join().unwrap();
        let _ = std::fs::remove_dir_all(&d);

        // Not an agent file; a broken one.
        assert!(AgentRef::parse("-----BEGIN PRIVATE KEY-----").is_none());
        assert!(AgentRef::parse("atrium-agent-key = 1\npublic = \"ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAAA x\"\n").unwrap().is_err());
    }
}
