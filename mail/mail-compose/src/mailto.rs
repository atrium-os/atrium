//! `mailto:` links (RFC 6068) as a compose window's fields (mail.md §9.1).
//! A link only fills the window; the person sees all of it and presses
//! Send. Only `to`, `cc`, `bcc`, `subject`, `body` and `in-reply-to` are
//! taken: a link can't set other headers.

use crate::write::{address, clean};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Fill {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body: String,
    pub in_reply_to: Option<String>,
    /// What the window says about the link: addresses dropped, fields
    /// ignored, a body with hidden characters.
    pub notes: Vec<String>,
}

/// The longest link taken (as Monstrator's).
pub const LINK_MAX: usize = 8192;
/// A body this long is said so (it may hide something below the fold).
pub const LONG_BODY: usize = 2000;

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Characters that hide or turn text around.
fn hidden(c: char) -> bool {
    matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}' | '\u{061C}')
}

fn addrs(s: &str, into: &mut Vec<String>, notes: &mut Vec<String>) {
    for a in decode(s).split(',') {
        let a = a.trim();
        if a.is_empty() {
            continue;
        }
        match address(a) {
            Ok(a) if !into.contains(&a) => into.push(a),
            Ok(_) => {}
            Err(_) => notes.push(format!("Left out {:?}: not an address", clean(a).chars().take(80).collect::<String>())),
        }
    }
}

/// The fields a `mailto:` link fills. `Err`: not a mailto link.
pub fn parse(link: &str) -> Result<Fill, String> {
    if link.len() > LINK_MAX {
        return Err("the link is too long".into());
    }
    let lower = link.get(..7).map(str::to_ascii_lowercase);
    if lower.as_deref() != Some("mailto:") {
        return Err("not a mailto: link".into());
    }
    let rest = &link[7..];
    let (to, query) = match rest.split_once('?') {
        Some((t, q)) => (t, q),
        None => (rest, ""),
    };
    let mut f = Fill::default();
    addrs(to, &mut f.to, &mut f.notes);
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        match decode(k).to_ascii_lowercase().as_str() {
            "to" => addrs(v, &mut f.to, &mut f.notes),
            "cc" => addrs(v, &mut f.cc, &mut f.notes),
            "bcc" => addrs(v, &mut f.bcc, &mut f.notes),
            "subject" => f.subject = clean(&decode(v)),
            "body" => f.body = decode(v).replace("\r\n", "\n"),
            "in-reply-to" => {
                let id = clean(&decode(v));
                if id.starts_with('<') && id.ends_with('>') && id.contains('@') {
                    f.in_reply_to = Some(id);
                }
            }
            other => f.notes.push(format!("Ignored the link's {:?} field", clean(other).chars().take(40).collect::<String>())),
        }
    }
    if !f.bcc.is_empty() {
        f.notes.push(format!("The link adds a hidden copy to {}", f.bcc.join(", ")));
    }
    if f.body.chars().any(hidden) || f.subject.chars().any(hidden) {
        f.body.retain(|c| !hidden(c));
        f.subject.retain(|c| !hidden(c));
        f.notes.push("The link's text had hidden characters; they were removed".into());
    }
    if f.body.chars().count() > LONG_BODY {
        f.notes.push("The link fills in a long message: read it all before sending".into());
    }
    Ok(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links() {
        let f = parse("mailto:priya@example.org,arun@acme.example?cc=lena%40acme.example&subject=Q4%20numbers&body=Hi%2C%0D%0Athanks").unwrap();
        assert_eq!(f.to, ["priya@example.org", "arun@acme.example"]);
        assert_eq!(f.cc, ["lena@acme.example"]);
        assert_eq!(f.subject, "Q4 numbers");
        assert_eq!(f.body, "Hi,\nthanks");
        assert!(f.notes.is_empty());
        // No header can be added, and the subject can't carry a line break.
        let f = parse("mailto:a@b.example?subject=Hi%0D%0ABcc:%20evil@x.example&X-Evil=1&bcc=spy@x.example").unwrap();
        assert_eq!(f.subject, "Hi  Bcc: evil@x.example");
        assert!(f.notes.iter().any(|n| n.contains("\"x-evil\"")));
        assert!(f.notes.iter().any(|n| n.contains("hidden copy to spy@x.example")));
        let f = parse("mailto:a@b.example?body=pay%E2%80%AEnow").unwrap();
        assert_eq!(f.body, "paynow");
        assert!(f.notes.iter().any(|n| n.contains("hidden characters")));
        assert!(parse("https://x.example").is_err());
        assert!(parse("MAILTO:a@b.example").is_ok());
        assert_eq!(parse("mailto:not-an-address").unwrap().notes.len(), 1);
    }
}
