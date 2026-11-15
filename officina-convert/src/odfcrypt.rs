//! Password-protected OpenDocument packages (ODF 1.3 §3.4 and 1.4's
//! AES-GCM/Argon2id): [`decrypt`] turns an encrypted .odt/.ods/.odp into
//! the plain package the readers take, [`encrypt`] does the reverse.
//!
//! Two layouts are read:
//! - per entry (every ODF writer, LibreOffice's default): each part is
//!   deflated, then encrypted on its own (AES-256-CBC, or AES-GCM), with
//!   its key from PBKDF2-HMAC-SHA1 or Argon2id over a SHA-1/SHA-256 hash
//!   of the password; a checksum of the first 1024 bytes tells a wrong
//!   password (CBC), the GCM tag does for GCM;
//! - whole package (LibreOffice 24.2's newest mode): one
//!   `encrypted-package` entry holding the deflated plain package.
//!
//! [`encrypt`] writes per-entry AES-256-CBC with PBKDF2 (100,000
//! iterations, SHA-256 start key), which every ODF reader of the last
//! fifteen years opens.
//!
//! The legacy Blowfish-CFB of OpenOffice.org 1–2 is refused (Unsupported).

use std::collections::HashMap;

use aes_gcm::aead::{Aead, KeyInit};
use sha1::Sha1;
use sha2::{Digest, Sha256};

use crate::crypt::{b64, b64e, damaged, random, Aes, CryptError, MAX_PACKAGE};
use crate::xml::{self, El};
use crate::zip::{Archive, ZipWriter};

const CBC: &str = "http://www.w3.org/2001/04/xmlenc#aes256-cbc";
const GCM: &str = "http://www.w3.org/2009/xmlenc11#aes256-gcm";
const BLOWFISH: &str = "Blowfish CFB";
const ARGON2ID: &str = "urn:org:documentfoundation:names:experimental:office:manifest:argon2id";
const SHA256_1K: &str = "urn:oasis:names:tc:opendocument:xmlns:manifest:1.0#sha256-1k";
const SHA1_1K: &str = "SHA1/1K";
/// The most PBKDF2 iterations or Argon2 passes a file may ask for.
const MAX_ITER: u32 = 10_000_000;
/// The most Argon2 passes a file may ask for (LibreOffice uses 3).
const MAX_ARGON_PASSES: u32 = 64;
/// The most Argon2 memory (KiB) a file may ask for: 1 GiB.
const MAX_ARGON_MEM: u32 = 1 << 20;
pub const PBKDF2_ITER: u32 = 100_000;

/// One encrypted part, as the manifest describes it.
#[derive(Debug, Clone, Default)]
struct Enc {
    size: Option<u64>,
    algorithm: String,
    iv: Vec<u8>,
    start: String,
    derivation: String,
    iterations: u32,
    salt: Vec<u8>,
    key_size: usize,
    argon: (u32, u32, u32),
    checksum_type: String,
    checksum: Vec<u8>,
}

fn manifest(z: &Archive) -> Result<HashMap<String, Enc>, CryptError> {
    let Ok(Some(data)) = z.read_name("META-INF/manifest.xml") else { return Ok(HashMap::new()) };
    let root = xml::parse(&data).map_err(|e| CryptError::Damaged(format!("manifest: {e}")))?;
    let mut out = HashMap::new();
    for fe in root.children("manifest:file-entry") {
        let Some(ed) = fe.child("manifest:encryption-data") else { continue };
        let path = fe.attr("manifest:full-path").unwrap_or_default().to_string();
        out.insert(path, enc(fe, ed)?);
    }
    Ok(out)
}

fn num<T: std::str::FromStr>(e: &El, name: &str) -> Option<T> {
    e.attr(name).and_then(|v| v.trim().parse().ok())
}

fn enc(fe: &El, ed: &El) -> Result<Enc, CryptError> {
    let mut e = Enc { size: num(fe, "manifest:size"), ..Enc::default() };
    e.checksum_type = ed.attr("manifest:checksum-type").unwrap_or_default().to_string();
    e.checksum = b64(ed.attr("manifest:checksum").unwrap_or_default())?;
    let a = ed.child("manifest:algorithm").ok_or(CryptError::Damaged("manifest algorithm".into()))?;
    e.algorithm = a.attr("manifest:algorithm-name").unwrap_or_default().to_string();
    e.iv = b64(a.attr("manifest:initialisation-vector").unwrap_or_default())?;
    // No start-key-generation: SHA-1 (ODF 1.0–1.1).
    let s = ed.child("manifest:start-key-generation");
    e.start = s.and_then(|s| s.attr("manifest:start-key-generation-name")).unwrap_or("SHA1").to_string();
    let k = ed.child("manifest:key-derivation").ok_or(CryptError::Damaged("manifest key derivation".into()))?;
    e.derivation = k.attr("manifest:key-derivation-name").unwrap_or_default().to_string();
    e.iterations = num(k, "manifest:iteration-count").unwrap_or(0);
    e.salt = b64(k.attr("manifest:salt").unwrap_or_default())?;
    e.key_size = num(k, "manifest:key-size").unwrap_or(16);
    e.argon = (
        num(k, "loext:argon2-iterations").or_else(|| num(k, "manifest:argon2-iterations")).unwrap_or(0),
        num(k, "loext:argon2-memory").or_else(|| num(k, "manifest:argon2-memory")).unwrap_or(0),
        num(k, "loext:argon2-lanes").or_else(|| num(k, "manifest:argon2-lanes")).unwrap_or(0),
    );
    Ok(e)
}

/// Whether `file` is an ODF package with encrypted parts.
pub fn is_encrypted_odf(file: &[u8]) -> bool {
    Archive::open(file).ok().and_then(|z| manifest(&z).ok()).is_some_and(|m| !m.is_empty())
}

/// Keys already derived, by (password hash, derivation, salt): a file whose
/// parts share a salt pays once.
type Keys = HashMap<(Vec<u8>, String, Vec<u8>), Vec<u8>>;

fn key(e: &Enc, password: &str, keys: &mut Keys) -> Result<Vec<u8>, CryptError> {
    let start = match e.start.as_str() {
        "SHA1" | "http://www.w3.org/2000/09/xmldsig#sha1" => Sha1::digest(password.as_bytes()).to_vec(),
        "http://www.w3.org/2000/09/xmldsig#sha256" | "http://www.w3.org/2001/04/xmlenc#sha256" | "SHA256" => Sha256::digest(password.as_bytes()).to_vec(),
        s => return Err(CryptError::Unsupported(format!("start key {s}"))),
    };
    if !matches!(e.key_size, 16 | 24 | 32) {
        return Err(CryptError::Unsupported(format!("a {}-byte key", e.key_size)));
    }
    let id = (start.clone(), format!("{}/{}/{:?}/{}", e.derivation, e.iterations, e.argon, e.key_size), e.salt.clone());
    if let Some(k) = keys.get(&id) {
        return Ok(k.clone());
    }
    let mut k = vec![0u8; e.key_size];
    match e.derivation.as_str() {
        "PBKDF2" => {
            if e.iterations == 0 || e.iterations > MAX_ITER {
                return damaged("PBKDF2 iteration count");
            }
            pbkdf2::pbkdf2_hmac::<Sha1>(&start, &e.salt, e.iterations, &mut k);
        }
        ARGON2ID => {
            let (t, m, p) = e.argon;
            if t == 0 || t > MAX_ARGON_PASSES || m > MAX_ARGON_MEM || p == 0 || p > 64 {
                return damaged("Argon2 parameters");
            }
            let params = argon2::Params::new(m, t, p, Some(e.key_size)).map_err(|_| CryptError::Damaged("Argon2 parameters".into()))?;
            argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
                .hash_password_into(&start, &e.salt, &mut k)
                .map_err(|_| CryptError::Damaged("Argon2 parameters".into()))?;
        }
        d => return Err(CryptError::Unsupported(format!("key derivation {d}"))),
    }
    keys.insert(id, k.clone());
    Ok(k)
}

/// One part decrypted and inflated.
fn open_part(e: &Enc, body: &[u8], password: &str, keys: &mut Keys) -> Result<Vec<u8>, CryptError> {
    let k = key(e, password, keys)?;
    let packed = match e.algorithm.as_str() {
        CBC => {
            if e.iv.len() != 16 || body.len() % 16 != 0 || body.is_empty() {
                return damaged("an AES-CBC part");
            }
            let mut d = body.to_vec();
            Aes::new(&k)?.cbc_dec(&e.iv, &mut d);
            let sum = |b: &[u8]| -> Result<Vec<u8>, CryptError> {
                let b = &b[..b.len().min(1024)];
                Ok(match e.checksum_type.as_str() {
                    SHA256_1K | "http://www.w3.org/2000/09/xmldsig#sha256-1k" => Sha256::digest(b).to_vec(),
                    SHA1_1K | "" => Sha1::digest(b).to_vec(),
                    t => return Err(CryptError::Unsupported(format!("checksum {t}"))),
                })
            };
            let padded = sum(&d)?;
            let pad = *d.last().unwrap() as usize;
            let ok_pad = (1..=16).contains(&pad);
            if !ok_pad {
                return Err(CryptError::WrongPassword);
            }
            d.truncate(d.len() - pad);
            // LibreOffice sums the bytes with the padding off; accept
            // either.
            if padded != e.checksum && sum(&d)? != e.checksum {
                return Err(CryptError::WrongPassword);
            }
            d
        }
        GCM => {
            // LibreOffice writes the nonce in front of the ciphertext
            // (the manifest's IV is not the one used).
            if e.key_size != 32 || body.len() < 12 + 16 {
                return damaged("an AES-GCM part");
            }
            let c = aes_gcm::Aes256Gcm::new_from_slice(&k).map_err(|_| CryptError::Damaged("AES-GCM key".into()))?;
            c.decrypt(aes_gcm::Nonce::from_slice(&body[..12]), &body[12..]).map_err(|_| CryptError::WrongPassword)?
        }
        BLOWFISH => return Err(CryptError::Unsupported("Blowfish encryption (OpenOffice.org 1 and 2)".into())),
        a => return Err(CryptError::Unsupported(format!("algorithm {a}"))),
    };
    let limit = e.size.unwrap_or(MAX_PACKAGE).min(MAX_PACKAGE) as usize;
    match miniz_oxide::inflate::decompress_to_vec_with_limit(&packed, limit.max(1)) {
        Ok(v) => Ok(v),
        // Stored, not deflated (allowed, if rare).
        Err(_) if e.size.is_none_or(|s| s == packed.len() as u64) => Ok(packed),
        Err(_) => damaged("an encrypted part does not inflate"),
    }
}

/// The plain package inside `file`, an encrypted ODF package. A file with
/// nothing encrypted comes back as it is.
pub fn decrypt(file: &[u8], password: &str) -> Result<Vec<u8>, CryptError> {
    let z = Archive::open(file).map_err(|e| CryptError::Damaged(e.to_string()))?;
    let m = manifest(&z)?;
    if m.is_empty() {
        return Ok(file.to_vec());
    }
    let mut keys = Keys::new();
    if let Some(e) = m.get("encrypted-package") {
        let body = z.read_name("encrypted-package").map_err(|e| CryptError::Damaged(e.to_string()))?.ok_or(CryptError::Damaged("no encrypted-package".into()))?;
        let inner = open_part(e, &body, password, &mut keys)?;
        if Archive::open(&inner).is_err() {
            return damaged("the encrypted package is not a package");
        }
        return Ok(inner);
    }
    // Try the smallest part first: a wrong password costs one key and a
    // tiny decrypt.
    let mut order: Vec<&str> = m.keys().map(|s| s.as_str()).collect();
    order.sort_by_key(|n| (z.find(n).map(|e| e.compressed).unwrap_or(u64::MAX), n.to_string()));
    let mut plain: HashMap<&str, Vec<u8>> = HashMap::new();
    for n in order {
        let Some(entry) = z.find(n) else { continue };
        let body = z.read(entry).map_err(|e| CryptError::Damaged(e.to_string()))?;
        plain.insert(n, open_part(&m[n], &body, password, &mut keys)?);
    }
    let mut w = ZipWriter::new(Vec::new());
    let io = |e: std::io::Error| CryptError::Damaged(e.to_string());
    for entry in &z.entries {
        let n = entry.name.as_str();
        if n == "META-INF/manifest.xml" {
            let data = z.read(entry).map_err(|e| CryptError::Damaged(e.to_string()))?;
            w.add(n, &strip_encryption(&data)).map_err(io)?;
        } else if n == "mimetype" {
            let data = z.read(entry).map_err(|e| CryptError::Damaged(e.to_string()))?;
            w.add_stored(n, &data).map_err(io)?;
        } else if let Some(p) = plain.get(n) {
            w.add(n, p).map_err(io)?;
        } else {
            let data = z.read(entry).map_err(|e| CryptError::Damaged(e.to_string()))?;
            w.add(n, &data).map_err(io)?;
        }
    }
    w.finish().map_err(io)
}

/// The manifest without its `<manifest:encryption-data>` elements.
fn strip_encryption(m: &[u8]) -> Vec<u8> {
    let s = String::from_utf8_lossy(m);
    let mut out = String::with_capacity(s.len());
    let mut rest: &str = &s;
    while let Some(a) = rest.find("<manifest:encryption-data") {
        out.push_str(&rest[..a]);
        let tail = &rest[a..];
        let end = match (tail.find("</manifest:encryption-data>"), tail.find("/>")) {
            (Some(c), _) => c + "</manifest:encryption-data>".len(),
            (None, Some(e)) => e + 2,
            (None, None) => tail.len(),
        };
        rest = &tail[end..];
    }
    out.push_str(rest);
    out.into_bytes()
}

/// `package`, a plain ODF package, with every part but `mimetype` and the
/// manifest encrypted under `password`.
pub fn encrypt(package: &[u8], password: &str) -> Result<Vec<u8>, CryptError> {
    encrypt_with(package, password, PBKDF2_ITER)
}

/// [`encrypt`] with `iterations` of PBKDF2 (tests use fewer).
pub fn encrypt_with(package: &[u8], password: &str, iterations: u32) -> Result<Vec<u8>, CryptError> {
    let z = Archive::open(package).map_err(|e| CryptError::Damaged(e.to_string()))?;
    let io = |e: std::io::Error| CryptError::Damaged(e.to_string());
    let rd = |e: &crate::zip::Entry| z.read(e).map_err(|e| CryptError::Damaged(e.to_string()));
    let manifest = match z.find("META-INF/manifest.xml") {
        Some(e) => String::from_utf8_lossy(&strip_encryption(&rd(e)?)).into_owned(),
        None => return damaged("no manifest"),
    };
    let start = Sha256::digest(password.as_bytes());
    // One salt, so one key, for the whole file (as LibreOffice does per
    // save); every part has its own IV.
    let salt = random(16);
    let mut k = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha1>(&start, &salt, iterations, &mut k);
    let aes = Aes::new(&k)?;
    let mut w = ZipWriter::new(Vec::new());
    let mut data_for: HashMap<String, String> = HashMap::new();
    if let Some(e) = z.find("mimetype") {
        w.add_stored("mimetype", &rd(e)?).map_err(io)?;
    }
    for entry in &z.entries {
        let n = entry.name.as_str();
        if n == "mimetype" || n == "META-INF/manifest.xml" || n.ends_with('/') {
            if n.ends_with('/') {
                w.add_stored(n, &[]).map_err(io)?;
            }
            continue;
        }
        let plain = rd(entry)?;
        let mut d = miniz_oxide::deflate::compress_to_vec(&plain, 6);
        let check = Sha256::digest(&d[..d.len().min(1024)]);
        let pad = 16 - d.len() % 16;
        let mut tail = random(pad);
        tail[pad - 1] = pad as u8;
        d.extend_from_slice(&tail);
        let iv = random(16);
        aes.cbc_enc(&iv, &mut d);
        w.add_stored(n, &d).map_err(io)?;
        data_for.insert(
            n.to_string(),
            format!(
                r#"<manifest:encryption-data manifest:checksum-type="{SHA256_1K}" manifest:checksum="{}"><manifest:algorithm manifest:algorithm-name="{CBC}" manifest:initialisation-vector="{}"/><manifest:start-key-generation manifest:start-key-generation-name="http://www.w3.org/2000/09/xmldsig#sha256" manifest:key-size="32"/><manifest:key-derivation manifest:key-derivation-name="PBKDF2" manifest:iteration-count="{iterations}" manifest:salt="{}" manifest:key-size="32"/></manifest:encryption-data>"#,
                b64e(&check),
                b64e(&iv),
                b64e(&salt)
            ),
        );
        data_for.insert(format!("{n}\0size"), plain.len().to_string());
    }
    w.add("META-INF/manifest.xml", rewrite_manifest(&manifest, &data_for)?.as_bytes()).map_err(io)?;
    w.finish().map_err(io)
}

/// The manifest with each encrypted part's entry given its size and
/// encryption data (parts the manifest doesn't list are added).
fn rewrite_manifest(m: &str, data_for: &HashMap<String, String>) -> Result<String, CryptError> {
    let root = xml::parse(m.as_bytes()).map_err(|e| CryptError::Damaged(format!("manifest: {e}")))?;
    let mut out = String::from(r#"<?xml version="1.0" encoding="UTF-8"?><manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0""#);
    if let Some(v) = root.attr("manifest:version") {
        out += &format!(r#" manifest:version="{}""#, xml::esc(v));
    }
    out.push('>');
    let mut seen = std::collections::HashSet::new();
    let entry = |out: &mut String, path: &str, attrs: &[(String, String)]| {
        out.push_str("<manifest:file-entry");
        for (k, v) in attrs {
            if k != "manifest:size" && k.starts_with("manifest:") {
                out.push_str(&format!(r#" {k}="{}""#, xml::esc(v)));
            }
        }
        if let Some(d) = data_for.get(path) {
            out.push_str(&format!(r#" manifest:size="{}">{d}</manifest:file-entry>"#, data_for[&format!("{path}\0size")]));
        } else {
            out.push_str("/>");
        }
    };
    for fe in root.children("manifest:file-entry") {
        let path = fe.attr("manifest:full-path").unwrap_or_default().to_string();
        entry(&mut out, &path, &fe.attrs);
        seen.insert(path);
    }
    let mut extra: Vec<&String> = data_for.keys().filter(|k| !k.contains('\0') && !seen.contains(*k)).collect();
    extra.sort();
    for p in extra {
        let attrs = vec![("manifest:full-path".to_string(), p.clone()), ("manifest:media-type".to_string(), String::new())];
        entry(&mut out, p, &attrs);
    }
    out += "</manifest:manifest>";
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package() -> Vec<u8> {
        let mut w = ZipWriter::new(Vec::new());
        w.add_stored("mimetype", b"application/vnd.oasis.opendocument.text").unwrap();
        let big: String = (0..3000).map(|i| format!("<text:p>line {i}</text:p>")).collect();
        w.add("content.xml", big.as_bytes()).unwrap();
        w.add("meta.xml", b"<m/>").unwrap();
        w.add("Pictures/a.png", &[7u8; 5000]).unwrap();
        w.add("META-INF/manifest.xml", br#"<?xml version="1.0" encoding="UTF-8"?><manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0" manifest:version="1.3"><manifest:file-entry manifest:full-path="/" manifest:media-type="application/vnd.oasis.opendocument.text"/><manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/><manifest:file-entry manifest:full-path="meta.xml" manifest:media-type="text/xml"/></manifest:manifest>"#).unwrap();
        w.finish().unwrap()
    }

    fn parts(p: &[u8]) -> Vec<(String, Vec<u8>)> {
        let z = Archive::open(p).unwrap();
        let mut v: Vec<_> = z.entries.iter().filter(|e| e.name != "META-INF/manifest.xml").map(|e| (e.name.clone(), z.read(e).unwrap())).collect();
        v.sort();
        v
    }

    #[test]
    fn round_trip_and_a_wrong_password() {
        let p = package();
        let e = encrypt_with(&p, "pässwörd", 1000).unwrap();
        assert!(is_encrypted_odf(&e));
        assert!(!is_encrypted_odf(&p));
        let z = Archive::open(&e).unwrap();
        assert!(z.read_name("content.xml").unwrap().unwrap().windows(7).all(|w| w != b"text:p>"));
        assert!(matches!(decrypt(&e, "password"), Err(CryptError::WrongPassword)));
        let d = decrypt(&e, "pässwörd").unwrap();
        assert_eq!(parts(&d), parts(&p));
        assert!(!is_encrypted_odf(&d));
        let man = String::from_utf8(Archive::open(&d).unwrap().read_name("META-INF/manifest.xml").unwrap().unwrap()).unwrap();
        assert!(man.contains("Pictures/a.png") && !man.contains("encryption"), "{man}");
    }

    #[test]
    fn hostile_files_never_panic() {
        let e = encrypt_with(&package(), "x", 10).unwrap();
        let mut seed = 1u64;
        for _ in 0..300 {
            let mut f = e.clone();
            for _ in 0..4 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let at = (seed >> 33) as usize % f.len();
                f[at] ^= (seed >> 20) as u8 | 1;
            }
            let _ = decrypt(&f, "x");
        }
        // A manifest asking for absurd work is refused, not run.
        let s = String::from_utf8_lossy(&Archive::open(&e).unwrap().read_name("META-INF/manifest.xml").unwrap().unwrap()).replace(r#"iteration-count="10""#, r#"iteration-count="4000000000""#);
        let z = Archive::open(&e).unwrap();
        let mut w = ZipWriter::new(Vec::new());
        for en in &z.entries {
            if en.name == "META-INF/manifest.xml" {
                w.add(&en.name, s.as_bytes()).unwrap();
            } else {
                w.add_stored(&en.name, &z.read(en).unwrap()).unwrap();
            }
        }
        assert!(matches!(decrypt(&w.finish().unwrap(), "x"), Err(CryptError::Damaged(_))));
    }
}
