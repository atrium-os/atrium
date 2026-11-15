//! Transfer encodings (base64, quoted-printable) and charsets.

use encoding_rs::{Encoding, WINDOWS_1252};

/// Decodes base64, skipping whitespace and anything outside the alphabet
/// (as mail readers do), stopping at padding. Never fails.
pub fn base64(input: &[u8], cap: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity((input.len() / 4 * 3).min(cap));
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for &c in input {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ => continue,
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            if out.len() >= cap {
                break;
            }
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'A'..=b'F' => Some(c - b'A' + 10),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    }
}

/// Quoted-printable (RFC 2045 §6.7). `header` is the RFC 2047 "Q" variant,
/// where `_` is a space. A malformed `=` sequence is kept as it is.
pub fn quoted_printable(input: &[u8], header: bool, cap: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len().min(cap));
    let mut i = 0;
    while i < input.len() && out.len() < cap {
        let c = input[i];
        if c == b'=' {
            // soft line break: "=" then optional whitespace then CRLF/LF
            let mut j = i + 1;
            while j < input.len() && (input[j] == b' ' || input[j] == b'\t') {
                j += 1;
            }
            if j < input.len() && (input[j] == b'\n' || input[j] == b'\r') {
                if input[j] == b'\r' && j + 1 < input.len() && input[j + 1] == b'\n' {
                    j += 1;
                }
                i = j + 1;
                continue;
            }
            if j >= input.len() {
                // "=" (and blanks) at the very end: a soft break with no newline
                break;
            }
            if i + 2 < input.len() {
                if let (Some(h), Some(l)) = (hex(input[i + 1]), hex(input[i + 2])) {
                    out.push(h << 4 | l);
                    i += 3;
                    continue;
                }
            }
            out.push(b'=');
            i += 1;
        } else if header && c == b'_' {
            out.push(b' ');
            i += 1;
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// Text from bytes in a named charset (WHATWG labels). Returns the text and
/// whether the label was known; an unknown or missing label is read as
/// windows-1252 (what WHATWG calls Latin-1), and `us-ascii` likewise, so
/// stray 8-bit bytes still show as something.
pub fn to_utf8(bytes: &[u8], label: Option<&str>) -> (String, bool) {
    let (enc, known) = match label.and_then(|l| Encoding::for_label(l.trim().as_bytes())) {
        Some(e) => (e, true),
        None => (WINDOWS_1252, label.is_none()),
    };
    // UTF-7 isn't in WHATWG's set; it's rare and a known XSS trick, so it
    // reads as windows-1252 and is reported as unknown.
    let (text, _, _) = enc.decode(bytes);
    let mut s = text.into_owned();
    s.retain(|c| c != '\0');
    (s, known)
}
