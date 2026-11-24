//! What Settings shows of a server's certificate before "Trust this server"
//! (wifi.md §7.4): who it names, who issued it, until when. A bounded DER
//! walk over the few fields shown; nothing here decides trust.

/// One DER element: (tag, contents, rest). None on anything malformed.
fn tlv(b: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, b) = b.split_first()?;
    let (&first, mut b) = b.split_first()?;
    let len = if first < 0x80 {
        first as usize
    } else {
        let n = (first & 0x7f) as usize;
        if n == 0 || n > 3 || b.len() < n {
            return None;
        }
        let (l, r) = b.split_at(n);
        b = r;
        l.iter().fold(0usize, |a, &x| (a << 8) | x as usize)
    };
    (b.len() >= len).then(|| (tag, &b[..len], &b[len..]))
}

const OID_CN: &[u8] = &[0x55, 0x04, 0x03];
const OID_O: &[u8] = &[0x55, 0x04, 0x0a];

/// A Name's common name, else its organization; printable characters only.
fn name(mut b: &[u8]) -> Option<String> {
    let (mut cn, mut o) = (None, None);
    while !b.is_empty() {
        let (t, set, rest) = tlv(b)?;
        b = rest;
        if t != 0x31 {
            return None;
        }
        let (t, atv, _) = tlv(set)?;
        if t != 0x30 {
            return None;
        }
        let (t, oid, rest) = tlv(atv)?;
        let (_, value, _) = tlv(rest)?;
        if t != 0x06 {
            return None;
        }
        let v = String::from_utf8_lossy(value).chars().filter(|c| !c.is_control()).take(128).collect::<String>();
        match oid {
            OID_CN => cn = Some(v),
            OID_O => o = Some(v),
            _ => {}
        }
    }
    cn.or(o)
}

/// "YYYY-MM-DD" from a UTCTime or GeneralizedTime.
fn date(tag: u8, v: &[u8]) -> Option<String> {
    fn digits(s: &[u8]) -> Option<&str> {
        s.iter().all(u8::is_ascii_digit).then(|| std::str::from_utf8(s).ok()).flatten()
    }
    let (y, rest) = match tag {
        0x17 if v.len() >= 6 => {
            let yy: u32 = digits(&v[..2])?.parse().ok()?;
            ((if yy >= 50 { 1900 } else { 2000 } + yy).to_string(), &v[2..])
        }
        0x18 if v.len() >= 8 => (digits(&v[..4])?.to_string(), &v[4..]),
        _ => return None,
    };
    Some(format!("{y}-{}-{}", digits(&rest[..2])?, digits(&rest[2..4])?))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    pub subject: String,
    pub issuer: String,
    pub not_after: String,
}

pub fn summary(der: &[u8]) -> Option<Summary> {
    let (t, cert, _) = tlv(der)?;
    if t != 0x30 {
        return None;
    }
    let (t, tbs, _) = tlv(cert)?;
    if t != 0x30 {
        return None;
    }
    // [0] version (optional), serial, signature algorithm.
    let (t, _, after_version) = tlv(tbs)?;
    let b = if t == 0xa0 { after_version } else { tbs };
    let (_, _, rest) = tlv(b)?;
    let (_, _, rest) = tlv(rest)?;
    let (t, issuer, rest) = tlv(rest)?;
    if t != 0x30 {
        return None;
    }
    let (t, validity, rest) = tlv(rest)?;
    if t != 0x30 {
        return None;
    }
    let (_, _, after) = tlv(validity)?;
    let (ta, na, _) = tlv(after)?;
    let (t, subject, _) = tlv(rest)?;
    if t != 0x30 {
        return None;
    }
    Some(Summary { subject: name(subject).unwrap_or_default(), issuer: name(issuer).unwrap_or_default(), not_after: date(ta, na)? })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostile_input_is_refused_not_panicked_on() {
        for n in 0..64 {
            let mut b = vec![0x30u8; n];
            b.extend_from_slice(&[0x84, 0xff, 0xff, 0xff, 0xff]);
            assert_eq!(summary(&b), None);
        }
        assert_eq!(summary(&[]), None);
        assert_eq!(summary(&[0x30, 0x82, 0xff]), None);
    }

    #[test]
    fn dates() {
        assert_eq!(date(0x17, b"261124120000Z").as_deref(), Some("2026-11-24"));
        assert_eq!(date(0x17, b"991231235959Z").as_deref(), Some("1999-12-31"));
        assert_eq!(date(0x18, b"20501231235959Z").as_deref(), Some("2050-12-31"));
        assert_eq!(date(0x17, b"26x1"), None);
    }
}
