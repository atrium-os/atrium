//! BER to DER, as much as CMS from mail clients needs: indefinite lengths
//! (NSS, Outlook) become definite, and constructed OCTET STRINGs and BIT
//! STRINGs become one primitive string. Bounded in depth and size.

const MAX_DEPTH: usize = 48;

/// `input` as DER (as far as lengths and constructed strings go).
pub fn to_der(input: &[u8]) -> Result<Vec<u8>, String> {
    if input.len() > crate::MAX_CMS {
        return Err("too large".into());
    }
    let mut out = Vec::with_capacity(input.len());
    let used = item(input, 0, &mut out)?;
    if input[used..].iter().any(|b| *b != 0) {
        return Err("bytes after the end".into());
    }
    Ok(out)
}

struct Head {
    tag: Vec<u8>,
    constructed: bool,
    /// None: indefinite.
    len: Option<usize>,
    at: usize,
}

fn head(b: &[u8]) -> Result<Head, String> {
    let first = *b.first().ok_or("short")?;
    let mut i = 1;
    if first & 0x1f == 0x1f {
        while *b.get(i).ok_or("short tag")? & 0x80 != 0 {
            i += 1;
            if i > 5 {
                return Err("tag too long".into());
            }
        }
        i += 1;
    }
    let tag = b[..i].to_vec();
    let l = *b.get(i).ok_or("short length")?;
    i += 1;
    let len = if l == 0x80 {
        None
    } else if l & 0x80 == 0 {
        Some(l as usize)
    } else {
        let n = (l & 0x7f) as usize;
        if n > 4 {
            return Err("length too long".into());
        }
        let mut v = 0usize;
        for _ in 0..n {
            v = (v << 8) | *b.get(i).ok_or("short length")? as usize;
            i += 1;
        }
        Some(v)
    };
    Ok(Head { tag, constructed: first & 0x20 != 0, len, at: i })
}

fn put_len(out: &mut Vec<u8>, n: usize) {
    if n < 0x80 {
        out.push(n as u8);
    } else {
        let bytes = n.to_be_bytes();
        let skip = bytes.iter().position(|b| *b != 0).unwrap_or(bytes.len() - 1);
        out.push(0x80 | (bytes.len() - skip) as u8);
        out.extend_from_slice(&bytes[skip..]);
    }
}

/// The children of a constructed value: (their bytes, how much was used).
fn children(b: &[u8], h: &Head, depth: usize) -> Result<(Vec<Vec<u8>>, usize), String> {
    let mut kids = Vec::new();
    let mut i = h.at;
    match h.len {
        Some(n) => {
            let end = h.at.checked_add(n).filter(|e| *e <= b.len()).ok_or("length past the end")?;
            while i < end {
                let mut k = Vec::new();
                i += item(&b[i..end], depth + 1, &mut k)?;
                kids.push(k);
            }
            Ok((kids, end))
        }
        None => loop {
            if b.get(i..i + 2) == Some(&[0, 0]) {
                return Ok((kids, i + 2));
            }
            if i >= b.len() {
                return Err("no end of contents".into());
            }
            let mut k = Vec::new();
            i += item(&b[i..], depth + 1, &mut k)?;
            kids.push(k);
        },
    }
}

/// One value; the bytes of input used.
fn item(b: &[u8], depth: usize, out: &mut Vec<u8>) -> Result<usize, String> {
    if depth > MAX_DEPTH {
        return Err("too deep".into());
    }
    let h = head(b)?;
    if !h.constructed {
        let n = h.len.ok_or("indefinite primitive")?;
        let end = h.at.checked_add(n).filter(|e| *e <= b.len()).ok_or("length past the end")?;
        out.extend_from_slice(&h.tag);
        put_len(out, n);
        out.extend_from_slice(&b[h.at..end]);
        return Ok(end);
    }
    let (kids, used) = children(b, &h, depth)?;
    // A constructed OCTET STRING or BIT STRING: one primitive string.
    if h.tag == [0x24] || h.tag == [0x23] {
        let mut v = Vec::new();
        for (n, k) in kids.iter().enumerate() {
            let kh = head(k)?;
            let body = &k[kh.at..];
            if h.tag == [0x23] && n > 0 {
                v.extend_from_slice(body.get(1..).unwrap_or(&[]));
            } else {
                v.extend_from_slice(body);
            }
        }
        out.push(h.tag[0] & !0x20);
        put_len(out, v.len());
        out.extend_from_slice(&v);
        return Ok(used);
    }
    let mut kids = kids;
    // EncryptedContentInfo ::= SEQUENCE { OID, AlgorithmIdentifier,
    // [0] IMPLICIT OCTET STRING }: streamed, the [0] is constructed from
    // OCTET STRING pieces and becomes one primitive [0]. Only in this shape,
    // since elsewhere a [0] holding an OCTET STRING is EXPLICIT.
    if h.tag == [0x30] && kids.len() == 3 && kids[0].first() == Some(&0x06) && kids[1].first() == Some(&0x30) && kids[2].first() == Some(&0xa0) {
        if let Some(v) = implicit_octets(&kids[2]) {
            let mut k = vec![0x80];
            put_len(&mut k, v.len());
            k.extend_from_slice(&v);
            kids[2] = k;
        }
    }
    let body: Vec<u8> = kids.concat();
    out.extend_from_slice(&h.tag);
    put_len(out, body.len());
    out.extend_from_slice(&body);
    Ok(used)
}

/// The joined contents of a constructed value (DER already) whose
/// children are all primitive OCTET STRINGs; None otherwise.
fn implicit_octets(k: &[u8]) -> Option<Vec<u8>> {
    let h = head(k).ok()?;
    let mut i = h.at;
    let mut v = Vec::new();
    while i < k.len() {
        let c = head(&k[i..]).ok()?;
        if c.tag != [0x04] {
            return None;
        }
        let end = i + c.at + c.len?;
        v.extend_from_slice(k.get(i + c.at..end)?);
        i = end;
    }
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indefinite_and_constructed_strings() {
        // SEQUENCE (indefinite) { OCTET STRING constructed (indefinite) { "ab", "c" } }
        let ber = [0x30, 0x80, 0x24, 0x80, 0x04, 0x02, b'a', b'b', 0x04, 0x01, b'c', 0, 0, 0, 0];
        assert_eq!(to_der(&ber).unwrap(), [0x30, 0x05, 0x04, 0x03, b'a', b'b', b'c']);
        let der = [0x30, 0x03, 0x02, 0x01, 0x05];
        assert_eq!(to_der(&der).unwrap(), der);
        assert!(to_der(&[0x30, 0x80, 0x02, 0x01]).is_err());
    }
}
