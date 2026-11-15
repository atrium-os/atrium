//! RFC 5322 header blocks, RFC 2047 encoded words, MIME parameters with
//! RFC 2231 continuations and charsets.

use crate::decode::{base64, quoted_printable, to_utf8};
use crate::Refused;

/// One header field: the name as written and the unfolded raw value.
#[derive(Debug, Clone)]
pub struct Field {
    pub name: String,
    pub value: Vec<u8>,
}

/// Splits `raw` into its header fields and the offset where the body starts.
/// Lines may end in CRLF or bare LF. A line that is neither a field nor a
/// continuation (no colon) is skipped, as mail readers do.
pub fn split(raw: &[u8], max_block: usize, max_fields: usize) -> Result<(Vec<Field>, usize), Refused> {
    let mut fields: Vec<Field> = Vec::new();
    let mut i = 0;
    loop {
        if i >= raw.len() {
            return Ok((fields, raw.len()));
        }
        if i > max_block {
            return Err(Refused::HeaderTooLarge);
        }
        let end = memchr(b'\n', &raw[i..]).map(|n| i + n).unwrap_or(raw.len());
        let mut line = &raw[i..end];
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        let next = (end + 1).min(raw.len());
        if line.is_empty() {
            return Ok((fields, next));
        }
        if line[0] == b' ' || line[0] == b'\t' {
            if let Some(f) = fields.last_mut() {
                // unfold: the CRLF goes, the whitespace stays (RFC 5322 §2.2.3)
                f.value.extend_from_slice(line);
            }
        } else if let Some(colon) = memchr(b':', line) {
            let name = &line[..colon];
            if !name.is_empty() && name.iter().all(|&c| (33..=126).contains(&c)) {
                if fields.len() >= max_fields {
                    return Err(Refused::TooManyHeaders);
                }
                let mut v = &line[colon + 1..];
                while let [b' ' | b'\t', rest @ ..] = v {
                    v = rest;
                }
                fields.push(Field { name: String::from_utf8_lossy(name).into_owned(), value: v.to_vec() });
            }
        }
        i = next;
    }
}

pub fn memchr(needle: u8, hay: &[u8]) -> Option<usize> {
    hay.iter().position(|&c| c == needle)
}

/// First field with this name (case-insensitive).
pub fn get<'a>(fields: &'a [Field], name: &str) -> Option<&'a [u8]> {
    fields.iter().find(|f| f.name.eq_ignore_ascii_case(name)).map(|f| f.value.as_slice())
}

pub fn get_all<'a>(fields: &'a [Field], name: &'a str) -> impl Iterator<Item = &'a [u8]> + 'a {
    fields.iter().filter(move |f| f.name.eq_ignore_ascii_case(name)).map(|f| f.value.as_slice())
}

/// Decodes an unstructured value (Subject, a display name) with RFC 2047
/// encoded words. Whitespace between two encoded words is dropped (§6.2).
/// Raw 8-bit bytes are read as UTF-8 if valid (RFC 6532), else windows-1252.
pub fn decode_words(value: &[u8], cap: usize) -> String {
    let mut out = String::new();
    let mut i = 0;
    let mut pending_ws = String::new();
    let mut last_was_word = false;
    while i < value.len() && out.len() < cap {
        if value[i..].starts_with(b"=?") {
            if let Some((text, used)) = encoded_word(&value[i..], cap) {
                if !last_was_word {
                    out.push_str(&pending_ws);
                }
                pending_ws.clear();
                out.push_str(&text);
                i += used;
                last_was_word = true;
                continue;
            }
        }
        // a run of whitespace or of other text
        let start = i;
        let ws = value[i] == b' ' || value[i] == b'\t';
        let is_ws = |c: u8| c == b' ' || c == b'\t';
        if ws {
            while i < value.len() && is_ws(value[i]) {
                i += 1;
            }
        } else {
            i += 1;
            while i < value.len() && !is_ws(value[i]) && !value[i..].starts_with(b"=?") {
                i += 1;
            }
        }
        let run = raw_text(&value[start..i]);
        if ws {
            pending_ws = run;
        } else {
            out.push_str(&pending_ws);
            pending_ws.clear();
            out.push_str(&run);
            last_was_word = false;
        }
    }
    if !pending_ws.is_empty() && !last_was_word {
        out.push_str(&pending_ws);
    }
    clean(&out, cap)
}

fn raw_text(b: &[u8]) -> String {
    match std::str::from_utf8(b) {
        Ok(s) => s.to_owned(),
        Err(_) => to_utf8(b, Some("windows-1252")).0,
    }
}

/// `=?charset?B|Q?text?=` at the start of `s`: (decoded text, bytes used).
fn encoded_word(s: &[u8], cap: usize) -> Option<(String, usize)> {
    // at most 75 bytes per RFC, but real mail runs longer; allow 1 KiB
    let lim = s.len().min(1024);
    let q1 = 2 + memchr(b'?', &s[2..lim])?;
    let q2 = q1 + 1 + memchr(b'?', &s[q1 + 1..lim])?;
    let end = q2 + 1 + find(&s[q2 + 1..lim], b"?=")?;
    let charset = std::str::from_utf8(&s[2..q1]).ok()?;
    // RFC 2231 language suffix: charset*lang
    let charset = charset.split('*').next().unwrap_or(charset);
    let text = &s[q2 + 1..end];
    if text.iter().any(|&c| c == b' ' || c == b'\t') {
        return None;
    }
    let bytes = match &s[q1 + 1..q2] {
        b"B" | b"b" => base64(text, cap),
        b"Q" | b"q" => quoted_printable(text, true, cap),
        _ => return None,
    };
    Some((to_utf8(&bytes, Some(charset)).0, end + 2))
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Drops control characters (keeping tab as a space), bidi overrides and
/// zero-width characters, and cuts to `cap` bytes on a character boundary.
/// Header text is shown in Mail's own chrome, so it must not be able to
/// reorder or hide what is drawn (same rule as Praeco, tabula.md §5.1).
pub fn clean(s: &str, cap: usize) -> String {
    let mut out = String::with_capacity(s.len().min(cap));
    for c in s.chars() {
        let c = if c == '\t' || c == '\r' || c == '\n' { ' ' } else { c };
        if c.is_control() || is_invisible(c) {
            continue;
        }
        if out.len() + c.len_utf8() > cap {
            break;
        }
        out.push(c);
    }
    out
}

pub fn is_invisible(c: char) -> bool {
    matches!(c,
        '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}'
        | '\u{2066}'..='\u{2069}' | '\u{FEFF}' | '\u{061C}' | '\u{180E}')
}

/// A parsed `Content-Type` or `Content-Disposition`: the lower-cased value
/// before the first `;`, and the parameters with lower-cased names, RFC 2231
/// continuations joined and charsets applied.
#[derive(Debug, Clone, Default)]
pub struct Params {
    pub value: String,
    pub params: Vec<(String, String)>,
}

impl Params {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.params.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

pub fn params(raw: &[u8], cap: usize) -> Params {
    let text = raw_text(raw);
    let mut parts = split_params(&text);
    let value = parts.first().map(|s| s.trim().to_ascii_lowercase()).unwrap_or_default();
    if !parts.is_empty() {
        parts.remove(0);
    }
    // name -> (sections: Vec<(index, encoded, text)>)
    let mut plain: Vec<(String, String)> = Vec::new();
    let mut sections: Vec<(String, u32, bool, String)> = Vec::new();
    for p in parts {
        let Some(eq) = p.find('=') else { continue };
        let name = p[..eq].trim().to_ascii_lowercase();
        let mut v = p[eq + 1..].trim().to_string();
        if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
            v = unquote(&v[1..v.len() - 1]);
        }
        if name.is_empty() || name.len() > 64 {
            continue;
        }
        if let Some(star) = name.find('*') {
            let base = name[..star].to_string();
            let rest = &name[star + 1..];
            let encoded = rest.ends_with('*') || rest.is_empty();
            let idx: u32 = rest.trim_end_matches('*').parse().unwrap_or(0);
            if idx < 256 && sections.len() < 1024 {
                sections.push((base, idx, encoded, v));
            }
        } else if plain.len() < 64 {
            plain.push((name, v));
        }
    }
    sections.sort_by(|a, b| (a.0.as_str(), a.1).cmp(&(b.0.as_str(), b.1)));
    let mut joined: Vec<(String, String)> = Vec::new();
    let mut i = 0;
    while i < sections.len() {
        let base = sections[i].0.clone();
        let mut charset: Option<String> = None;
        let mut bytes: Vec<u8> = Vec::new();
        let mut first = true;
        while i < sections.len() && sections[i].0 == base {
            let (_, _, enc, v) = &sections[i];
            if *enc {
                let mut v = v.as_str();
                if first {
                    // charset'language'value
                    let mut it = v.splitn(3, '\'');
                    if let (Some(cs), Some(_), Some(rest)) = (it.next(), it.next(), it.next()) {
                        charset = Some(cs.to_string()).filter(|c| !c.is_empty());
                        v = rest;
                    }
                }
                bytes.extend(percent(v));
            } else {
                bytes.extend_from_slice(v.as_bytes());
            }
            first = false;
            i += 1;
        }
        let text = to_utf8(&bytes, Some(charset.as_deref().unwrap_or("utf-8"))).0;
        if joined.len() < 64 {
            joined.push((base, text));
        }
    }
    // an RFC 2231 value wins over a plain one of the same name
    for (n, v) in plain {
        if !joined.iter().any(|(m, _)| *m == n) {
            joined.push((n, v));
        }
    }
    let params = joined
        .into_iter()
        .map(|(n, v)| {
            // some senders put RFC 2047 words inside quoted parameters
            let v = if v.contains("=?") { decode_words(v.as_bytes(), cap) } else { clean(&v, cap) };
            (n, v)
        })
        .collect();
    Params { value: clean(&value, 128), params }
}

fn split_params(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    let mut esc = false;
    for (i, c) in s.char_indices() {
        if esc {
            esc = false;
            continue;
        }
        match c {
            '\\' if quoted => esc = true,
            '"' => quoted = !quoted,
            ';' if !quoted => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        if out.len() >= 256 {
            return out;
        }
    }
    out.push(&s[start..]);
    out
}

fn unquote(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut esc = false;
    for c in s.chars() {
        if esc {
            out.push(c);
            esc = false;
        } else if c == '\\' {
            esc = true;
        } else {
            out.push(c);
        }
    }
    out
}

fn percent(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let h = (b[i + 1] as char).to_digit(16);
            let l = (b[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (h, l) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}
