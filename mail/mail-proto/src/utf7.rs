//! IMAP's modified UTF-7 for mailbox names (RFC 3501 §5.1.3), used when the
//! server hasn't enabled `UTF8=ACCEPT`.

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+,";

pub fn encode(s: &str) -> String {
    let mut out = String::new();
    let mut pending: Vec<u16> = Vec::new();
    let flush = |pending: &mut Vec<u16>, out: &mut String| {
        if pending.is_empty() {
            return;
        }
        out.push('&');
        let bytes: Vec<u8> = pending.iter().flat_map(|u| u.to_be_bytes()).collect();
        let mut acc = 0u32;
        let mut bits = 0;
        for b in bytes {
            acc = (acc << 8) | b as u32;
            bits += 8;
            while bits >= 6 {
                bits -= 6;
                out.push(B64[((acc >> bits) & 63) as usize] as char);
            }
        }
        if bits > 0 {
            out.push(B64[((acc << (6 - bits)) & 63) as usize] as char);
        }
        out.push('-');
        pending.clear();
    };
    for c in s.chars() {
        if (' '..='~').contains(&c) {
            flush(&mut pending, &mut out);
            if c == '&' {
                out.push_str("&-");
            } else {
                out.push(c);
            }
        } else {
            let mut buf = [0u16; 2];
            pending.extend_from_slice(c.encode_utf16(&mut buf));
        }
    }
    flush(&mut pending, &mut out);
    out
}

/// Decodes; a malformed name is returned as it was written.
pub fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u16> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'&' {
            let Some(end) = b[i + 1..].iter().position(|&c| c == b'-').map(|p| i + 1 + p) else { return s.to_string() };
            if end == i + 1 {
                out.push('&' as u16);
            } else {
                let mut acc = 0u32;
                let mut bits = 0;
                let mut bytes = Vec::new();
                for &c in &b[i + 1..end] {
                    let Some(v) = B64.iter().position(|&x| x == c) else { return s.to_string() };
                    acc = (acc << 6) | v as u32;
                    bits += 6;
                    if bits >= 8 {
                        bits -= 8;
                        bytes.push((acc >> bits) as u8);
                    }
                }
                for pair in bytes.chunks(2) {
                    if pair.len() == 2 {
                        out.push(u16::from_be_bytes([pair[0], pair[1]]));
                    }
                }
            }
            i = end + 1;
        } else {
            out.push(b[i] as u16);
            i += 1;
        }
    }
    String::from_utf16(&out).unwrap_or_else(|_| s.to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn round_trip() {
        for s in ["INBOX", "Entwürfe", "~peter/mail/台北/日本語", "A & B", "😀 emoji"] {
            assert_eq!(super::decode(&super::encode(s)), s);
        }
        assert_eq!(super::encode("~peter/mail/台北/日本語"), "~peter/mail/&U,BTFw-/&ZeVnLIqe-");
        assert_eq!(super::encode("A & B"), "A &- B");
    }
}
