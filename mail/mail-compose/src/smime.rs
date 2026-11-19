//! S/MIME message shapes (RFC 8551 §3.3, §3.5) around a written message:
//! the cryptography is done elsewhere (mail-smime, with a key someone else
//! holds); this only cuts the message and wraps it.

use crate::write::base64;

/// A written message cut into its outer header lines (Date to
/// MIME-Version, each ending CRLF) and its body entity: its `Content-*`
/// header lines, the blank line and the body, exactly as signed or
/// encrypted.
pub fn split(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    let end = bytes.windows(4).position(|w| w == b"\r\n\r\n")?;
    let mut i = 0;
    while i < end {
        if bytes[i..].len() >= 13 && bytes[i..i + 13].eq_ignore_ascii_case(b"content-type:") {
            return Some((&bytes[..i], &bytes[i..]));
        }
        i += bytes[i..].windows(2).position(|w| w == b"\r\n")? + 2;
    }
    None
}

fn b64_lines(der: &[u8], out: &mut Vec<u8>) {
    let enc = base64(der);
    for chunk in enc.as_bytes().chunks(76) {
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\r\n");
    }
}

/// `outer` with `entity` signed by `sig` (the DER of a detached CMS
/// signature over `entity`, SHA-256).
pub fn signed(outer: &[u8], entity: &[u8], sig: &[u8], boundary: &str) -> Vec<u8> {
    let mut out = outer.to_vec();
    out.extend_from_slice(format!("Content-Type: multipart/signed; protocol=\"application/pkcs7-signature\"; micalg=sha-256;\r\n boundary=\"{boundary}\"\r\n\r\nThis is an S/MIME signed message.\r\n\r\n--{boundary}\r\n").as_bytes());
    out.extend_from_slice(entity);
    if !entity.ends_with(b"\r\n") {
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("\r\n--{boundary}\r\nContent-Type: application/pkcs7-signature; name=\"smime.p7s\"\r\nContent-Transfer-Encoding: base64\r\nContent-Disposition: attachment; filename=\"smime.p7s\"\r\n\r\n").as_bytes());
    b64_lines(sig, &mut out);
    out.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    out
}

/// The entity of a signed message (what [`signed`] made, from Content-Type
/// on), to encrypt as a whole.
pub fn entity_of(bytes: &[u8]) -> Option<&[u8]> {
    split(bytes).map(|(_, e)| e)
}

/// `outer` with an encrypted body: `env`, the DER of CMS enveloped data.
pub fn enveloped(outer: &[u8], env: &[u8]) -> Vec<u8> {
    let mut out = outer.to_vec();
    out.extend_from_slice(b"Content-Type: application/pkcs7-mime; smime-type=enveloped-data; name=\"smime.p7m\"\r\nContent-Transfer-Encoding: base64\r\nContent-Disposition: attachment; filename=\"smime.p7m\"\r\n\r\n");
    b64_lines(env, &mut out);
    out
}

/// Add visible recipients' Autocrypt gossip to the entity that will be
/// encrypted. Fold the key data so even large certificates fit on the wire.
pub fn autocrypt_gossip<'a>(entity: &[u8], keys: impl IntoIterator<Item = (&'a str, &'a [u8])>) -> Vec<u8> {
    const HEADER_BUDGET: usize = 256 << 10;
    let mut out = Vec::new();
    for (addr, key) in keys {
        let Ok(addr) = crate::write::address(addr) else { continue };
        if addr.len() > 254 || key.is_empty() {
            continue;
        }
        let data = base64(key);
        let header = format!("Autocrypt-Gossip: addr={addr};\r\n keydata=");
        let folded_len = header.len() + data.len() + 3 * data.len().div_ceil(68);
        if out.len() + folded_len > HEADER_BUDGET {
            break;
        }
        out.extend_from_slice(header.as_bytes());
        for (i, chunk) in data.as_bytes().chunks(68).enumerate() {
            if i != 0 {
                out.extend_from_slice(b"\r\n ");
            }
            out.extend_from_slice(chunk);
        }
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(entity);
    out
}

/// `outer` with a PGP/MIME encrypted body (RFC 3156 §4): `armored`, the
/// OpenPGP message, ASCII-armored.
pub fn pgp_encrypted(outer: &[u8], armored: &[u8], boundary: &str) -> Vec<u8> {
    let mut out = outer.to_vec();
    out.extend_from_slice(format!("Content-Type: multipart/encrypted; protocol=\"application/pgp-encrypted\"; boundary=\"{boundary}\"\r\n\r\n").as_bytes());
    out.extend_from_slice(format!("This is an OpenPGP/MIME encrypted message (RFC 3156).\r\n--{boundary}\r\nContent-Type: application/pgp-encrypted\r\nContent-Description: PGP/MIME version identification\r\n\r\nVersion: 1\r\n\r\n--{boundary}\r\nContent-Type: application/octet-stream; name=\"encrypted.asc\"\r\nContent-Description: OpenPGP encrypted message\r\nContent-Disposition: inline; filename=\"encrypted.asc\"\r\n\r\n").as_bytes());
    // Armor, with CRLF line ends whatever it came with.
    for line in armored.split(|b| *b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        out.extend_from_slice(line);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cut_and_wrap() {
        let m = b"Date: x\r\nFrom: a@b.example\r\nMIME-Version: 1.0\r\nContent-Type: text/plain\r\nContent-Transfer-Encoding: 7bit\r\n\r\nHi\r\n";
        let (outer, entity) = split(m).unwrap();
        assert_eq!(outer, b"Date: x\r\nFrom: a@b.example\r\nMIME-Version: 1.0\r\n");
        assert_eq!(entity, b"Content-Type: text/plain\r\nContent-Transfer-Encoding: 7bit\r\n\r\nHi\r\n");
        let s = signed(outer, entity, &[1, 2, 3], "B");
        let text = String::from_utf8(s.clone()).unwrap();
        assert!(text.contains("\r\n--B\r\nContent-Type: text/plain\r\nContent-Transfer-Encoding: 7bit\r\n\r\nHi\r\n\r\n--B\r\nContent-Type: application/pkcs7-signature"), "{text}");
        assert!(text.ends_with("AQID\r\n\r\n--B--\r\n"));
        assert!(entity_of(&s).unwrap().starts_with(b"Content-Type: multipart/signed"));
        // A header block with no Content-Type isn't cut.
        assert!(split(b"Subject: x\r\n\r\nbody").is_none());
        // "Content-Type" only in the body doesn't count.
        assert!(split(b"Subject: x\r\n\r\nContent-Type: y\r\n").is_none());
        let p = String::from_utf8(pgp_encrypted(outer, b"-----BEGIN PGP MESSAGE-----\n\nwV4D\n-----END PGP MESSAGE-----\n", "E")).unwrap();
        assert!(p.contains("Content-Type: multipart/encrypted; protocol=\"application/pgp-encrypted\"; boundary=\"E\"\r\n\r\n"), "{p}");
        assert!(p.contains("\r\n--E\r\nContent-Type: application/pgp-encrypted\r\n") && p.contains("Version: 1\r\n"));
        assert!(p.contains("\r\nwV4D\r\n-----END PGP MESSAGE-----\r\n\r\n--E--\r\n"), "{p}");
    }
}
