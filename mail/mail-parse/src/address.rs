//! RFC 5322 address lists, read tolerantly (§3.4, with the obsolete forms
//! real mail still uses), into display name + address pairs.

use crate::header::{clean, decode_words};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Address {
    /// Display name, RFC 2047-decoded and cleaned; empty when none.
    pub name: String,
    /// `local@domain`, the domain lower-cased. Empty when the mailbox had none.
    pub addr: String,
    /// The display name itself contains something shaped like an address or
    /// a domain (`"paypal.com <x@evil.example>"`): Mail then shows the real
    /// address only (mail.md §7.5).
    pub name_looks_like_address: bool,
}

impl Address {
    pub fn domain(&self) -> &str {
        self.addr.rsplit_once('@').map(|(_, d)| d).unwrap_or("")
    }
}

const NAME_CAP: usize = 256;
const ADDR_CAP: usize = 320;

/// Parses an address-list header value. At most `max` addresses.
pub fn parse_list(raw: &[u8], max: usize) -> Vec<Address> {
    let mut out = Vec::new();
    let mut name: Vec<u8> = Vec::new(); // raw display-name bytes (quoted parts unquoted)
    let mut angle: Option<Vec<u8>> = None;
    let mut comment: Vec<u8> = Vec::new();
    let mut i = 0;
    let n = raw.len();
    let finish = |name: &mut Vec<u8>, angle: &mut Option<Vec<u8>>, comment: &mut Vec<u8>, out: &mut Vec<Address>| {
        let (disp, addr) = match angle.take() {
            Some(a) => (std::mem::take(name), a),
            None => {
                // bare address; an old-style trailing "(Name)" comment is the name
                let a: Vec<u8> = name.iter().copied().filter(|c| !c.is_ascii_whitespace()).collect();
                name.clear();
                (std::mem::take(comment), a)
            }
        };
        comment.clear();
        let addr = normalise_addr(&addr);
        let disp = decode_words(trim(&disp), NAME_CAP);
        let disp = disp.trim().to_string();
        if addr.is_empty() && disp.is_empty() {
            return;
        }
        if out.len() < max {
            let looks = looks_like_address(&disp);
            out.push(Address { name: disp, addr, name_looks_like_address: looks });
        }
    };
    while i < n {
        let c = raw[i];
        match c {
            b'"' => {
                i += 1;
                while i < n && raw[i] != b'"' {
                    if raw[i] == b'\\' && i + 1 < n {
                        i += 1;
                    }
                    name.push(raw[i]);
                    i += 1;
                }
                i += 1;
            }
            b'(' => {
                let mut depth = 1;
                i += 1;
                let start = comment.len();
                while i < n && depth > 0 {
                    match raw[i] {
                        b'\\' if i + 1 < n => {
                            i += 1;
                            comment.push(raw[i]);
                        }
                        b'(' => depth += 1,
                        b')' => depth -= 1,
                        x => comment.push(x),
                    }
                    i += 1;
                }
                if comment.len() > 1024 {
                    comment.truncate(start);
                }
            }
            b'<' => {
                let end = raw[i + 1..].iter().position(|&c| c == b'>').map(|p| i + 1 + p).unwrap_or(n);
                angle = Some(raw[i + 1..end].to_vec());
                i = end + 1;
            }
            b',' | b';' => {
                finish(&mut name, &mut angle, &mut comment, &mut out);
                i += 1;
            }
            b':' if angle.is_none() && !name.contains(&b'@') => {
                // group "Name:" — the group's own name is dropped
                name.clear();
                comment.clear();
                i += 1;
            }
            _ => {
                if name.len() < 4096 {
                    name.push(c);
                }
                i += 1;
            }
        }
        if out.len() >= max {
            break;
        }
    }
    finish(&mut name, &mut angle, &mut comment, &mut out);
    out
}

fn trim(b: &[u8]) -> &[u8] {
    let s = b.iter().position(|c| !c.is_ascii_whitespace()).unwrap_or(b.len());
    let e = b.iter().rposition(|c| !c.is_ascii_whitespace()).map(|p| p + 1).unwrap_or(s);
    &b[s..e]
}

/// Strips a source route (`@a,@b:x@y`), whitespace, and lower-cases the
/// domain. Returns "" if what is left isn't `something@something`.
fn normalise_addr(a: &[u8]) -> String {
    let mut a: Vec<u8> = a.iter().copied().filter(|c| !c.is_ascii_whitespace()).collect();
    if a.first() == Some(&b'@') {
        if let Some(p) = a.iter().position(|&c| c == b':') {
            a.drain(..=p);
        }
    }
    let s = String::from_utf8_lossy(&a);
    let s = clean(&s, ADDR_CAP);
    let Some((local, domain)) = s.rsplit_once('@') else { return String::new() };
    let domain = domain.trim_end_matches('.');
    if local.is_empty() || domain.is_empty() || domain.contains(['@', '<', '>', '"', '\\', ',', ';', ':', ' ']) {
        return String::new();
    }
    format!("{}@{}", local, domain.to_lowercase())
}

fn looks_like_address(name: &str) -> bool {
    if name.contains('@') {
        return true;
    }
    // a word with a dot followed by 2+ letters, like "paypal.com" or "secure.bank.example"
    name.split(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == '"' || c == '\'' || c == '<' || c == '>')
        .any(|w| {
            let w = w.trim_matches(|c: char| !c.is_alphanumeric());
            match w.rsplit_once('.') {
                Some((head, tld)) => !head.is_empty() && tld.len() >= 2 && tld.chars().all(|c| c.is_alphabetic()) && !head.ends_with('.'),
                None => false,
            }
        })
}
