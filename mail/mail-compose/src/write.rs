//! An RFC 5322 / MIME message from a draft.

use std::fmt::Write as _;

/// A name and an address.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Mailbox {
    pub name: String,
    pub addr: String,
}

impl Mailbox {
    pub fn new(name: &str, addr: &str) -> Mailbox {
        Mailbox { name: name.into(), addr: addr.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Attachment {
    pub name: String,
    /// `type/subtype`; anything else is sent as `application/octet-stream`.
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// What the compose window holds.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Draft {
    pub from: Mailbox,
    pub to: Vec<Mailbox>,
    pub cc: Vec<Mailbox>,
    pub bcc: Vec<Mailbox>,
    pub subject: String,
    /// The plain text (always sent).
    pub text: String,
    /// An HTML alternative (rich text), when there is one.
    pub html: Option<String>,
    /// Threading: the message-ids, with their angle brackets.
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub attachments: Vec<Attachment>,
}

/// When and with which random bytes (the caller's clock and generator, so
/// this crate stays deterministic and testable).
#[derive(Debug, Clone, Copy)]
pub struct Stamp {
    /// Seconds since the epoch.
    pub date: i64,
    pub random: [u8; 16],
}

/// A written message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    pub bytes: Vec<u8>,
    /// With its angle brackets.
    pub message_id: String,
    /// The envelope: From's address and every recipient once (Bcc too).
    pub from: String,
    pub rcpts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    NoRecipients,
    /// Not an address Mail can send to, said why.
    BadAddress(String),
    /// A message-id that isn't one.
    BadId,
    TooLarge,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NoRecipients => f.write_str("Add someone to send it to"),
            Error::BadAddress(why) => f.write_str(why),
            Error::BadId => f.write_str("a reference that isn't a message-id"),
            Error::TooLarge => f.write_str("This message is too large to send"),
        }
    }
}

/// The largest message written (servers commonly take 25–50 MiB; big files go
/// as links, §8.3).
pub const MESSAGE_MAX: usize = 40 << 20;
/// Most references kept in a reply (the first and the newest).
pub const REFERENCES_MAX: usize = 20;

/// Remove line breaks and control characters, and trim.
pub fn clean(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect::<String>().trim().to_string()
}

/// An address Mail sends to: `local@domain`, with the domain as ASCII
/// (IDNA) and an ASCII local part (no SMTPUTF8 yet). The normalised form.
pub fn address(a: &str) -> Result<String, Error> {
    let a = a.trim();
    let bad = |why: &str| Err(Error::BadAddress(format!("{a:?} {why}")));
    if a.len() > 254 || a.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return bad("isn't an address");
    }
    let Some((local, domain)) = a.rsplit_once('@') else { return bad("has no @") };
    if local.is_empty() || local.len() > 64 || domain.is_empty() {
        return bad("isn't an address");
    }
    if !local.is_ascii() {
        return bad("has letters before the @ this server may not take");
    }
    let atext = |c: char| c.is_ascii_alphanumeric() || "!#$%&'*+-/=?^_`{|}~".contains(c);
    let dot_atom = local.split('.').all(|p| !p.is_empty() && p.chars().all(atext));
    if !dot_atom {
        return bad("isn't an address");
    }
    let Ok(d) = idna::domain_to_ascii(domain) else { return bad("has a domain that isn't one") };
    if !d.contains('.') || d.split('.').any(|l| l.is_empty() || l.len() > 63) {
        return bad("has a domain that isn't one");
    }
    Ok(format!("{local}@{d}"))
}

fn base64(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        s.push(T[(n >> 18) as usize & 63] as char);
        s.push(T[(n >> 12) as usize & 63] as char);
        s.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        s.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    s
}

/// `s` as encoded words when it needs them (RFC 2047, UTF-8, B), each
/// at most 75 characters; else as it is.
fn words(s: &str) -> Vec<String> {
    let plain = s.chars().all(|c| c.is_ascii() && !c.is_ascii_control()) && !s.contains("=?");
    if plain {
        return vec![s.to_string()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in s.chars() {
        // 39 bytes: a 64-character word, which fits after "Subject: ".
        if cur.len() + c.len_utf8() > 39 {
            out.push(format!("=?UTF-8?B?{}?=", base64(cur.as_bytes())));
            cur.clear();
        }
        cur.push(c);
    }
    if !cur.is_empty() {
        out.push(format!("=?UTF-8?B?{}?=", base64(cur.as_bytes())));
    }
    out
}

/// A display name as a phrase: atoms as they are, specials quoted, other
/// scripts as encoded words.
fn phrase(name: &str) -> Vec<String> {
    let atom = |c: char| c.is_ascii_alphanumeric() || " !#$%&'*+-/=?^_`{|}~".contains(c);
    if name.chars().all(atom) && !name.contains("=?") {
        return vec![name.to_string()];
    }
    if name.is_ascii() && !name.contains("=?") {
        let q = name.replace('\\', "\\\\").replace('"', "\\\"");
        return vec![format!("\"{q}\"")];
    }
    words(name)
}

/// Header lines folded at whitespace to stay under 78 characters.
struct Head {
    out: String,
}

impl Head {
    /// `name: ` and its pieces, each a run that must not be broken.
    fn field(&mut self, name: &str, pieces: &[String]) {
        let mut line = format!("{name}:");
        for p in pieces {
            let sep = " ";
            if line.len() + sep.len() + p.len() > 76 && line.len() > name.len() + 1 {
                self.out.push_str(&line);
                self.out.push_str("\r\n");
                line = String::new();
            }
            line.push_str(sep);
            line.push_str(p);
        }
        self.out.push_str(&line);
        self.out.push_str("\r\n");
    }
}

fn mailbox_pieces(m: &Mailbox, last: bool) -> Result<Vec<String>, Error> {
    let addr = address(&m.addr)?;
    let name = clean(&m.name);
    let mut v = if name.is_empty() || name.eq_ignore_ascii_case(&addr) { Vec::new() } else { phrase(&name) };
    v.push(format!("<{addr}>{}", if last { "" } else { "," }));
    Ok(v)
}

fn list_pieces(l: &[Mailbox]) -> Result<Vec<String>, Error> {
    let mut v = Vec::new();
    for (i, m) in l.iter().enumerate() {
        v.extend(mailbox_pieces(m, i + 1 == l.len())?);
    }
    Ok(v)
}

/// A message-id, with or without its angle brackets (the parser keeps
/// them without), written with them.
fn msg_id(s: &str) -> Result<String, Error> {
    let s = s.trim();
    let inner = s.strip_prefix('<').and_then(|x| x.strip_suffix('>')).unwrap_or(s);
    let ok = !inner.is_empty() && inner.len() <= 990 && inner.contains('@') && inner.chars().all(|c| c.is_ascii_graphic() && c != '<' && c != '>');
    if ok { Ok(format!("<{inner}>")) } else { Err(Error::BadId) }
}

/// RFC 5322 date in UTC: "Tue, 17 Nov 2026 17:25:53 +0000".
pub fn date(t: i64) -> String {
    let days = t.div_euclid(86_400);
    let secs = t.rem_euclid(86_400);
    // Civil from days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    let wd = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"][days.rem_euclid(7) as usize];
    let mon = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"][(m - 1) as usize];
    format!("{wd}, {d} {mon} {y} {:02}:{:02}:{:02} +0000", secs / 3600, secs % 3600 / 60, secs % 60)
}

/// Text with CRLF line ends.
fn crlf(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n").replace('\n', "\r\n")
}

/// Quoted-printable (RFC 2045 §6.7), lines at most 76.
fn qp(s: &str) -> String {
    let mut out = String::new();
    for (i, line) in s.split("\r\n").enumerate() {
        if i > 0 {
            out.push_str("\r\n");
        }
        let b = line.as_bytes();
        let mut cur = String::new();
        for (k, &c) in b.iter().enumerate() {
            let last = k + 1 == b.len();
            let enc = match c {
                b'=' => "=3D".to_string(),
                b' ' | b'\t' if last => format!("={c:02X}"),
                33..=126 | b' ' | b'\t' => (c as char).to_string(),
                _ => format!("={c:02X}"),
            };
            if cur.len() + enc.len() > 75 {
                out.push_str(&cur);
                out.push_str("=\r\n");
                cur.clear();
            }
            cur.push_str(&enc);
        }
        out.push_str(&cur);
    }
    out
}

/// A text part's body and its transfer encoding.
fn text_body(s: &str) -> (String, &'static str) {
    let t = crlf(s);
    if t.is_ascii() && t.split("\r\n").all(|l| l.len() <= 998) {
        (t, "7bit")
    } else {
        (qp(&t), "quoted-printable")
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// A parameter value: as a quoted string when ASCII, else RFC 2231.
fn param(name: &str, v: &str) -> String {
    let v = clean(v);
    if v.is_ascii() {
        format!("{name}=\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        let enc: String = v.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect();
        format!("{name}*=utf-8''{enc}")
    }
}

fn mime_type(m: &str) -> String {
    let m = m.trim().to_ascii_lowercase();
    let token = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "!#$&-^_.+".contains(c));
    match m.split_once('/') {
        Some((a, b)) if token(a) && token(b) => m,
        _ => "application/octet-stream".into(),
    }
}

/// Write `d` as a message.
pub fn write(d: &Draft, stamp: &Stamp) -> Result<Written, Error> {
    write_as(d, stamp, false)
}

/// Write `d` as the copy kept in the server's Drafts folder: it may have no
/// recipients yet, addresses still being typed are left out, and Bcc is
/// kept (as a header, for the person's other devices). Never sent.
pub fn write_draft(d: &Draft, stamp: &Stamp) -> Result<Written, Error> {
    let ok = |l: &[Mailbox]| l.iter().filter(|m| address(&m.addr).is_ok()).cloned().collect::<Vec<_>>();
    let d = Draft { to: ok(&d.to), cc: ok(&d.cc), bcc: ok(&d.bcc), ..d.clone() };
    write_as(&d, stamp, true)
}

fn write_as(d: &Draft, stamp: &Stamp, draft: bool) -> Result<Written, Error> {
    let from = address(&d.from.addr)?;
    let mut rcpts: Vec<String> = Vec::new();
    for m in d.to.iter().chain(&d.cc).chain(&d.bcc) {
        let a = address(&m.addr)?;
        if !rcpts.iter().any(|r| r.eq_ignore_ascii_case(&a)) {
            rcpts.push(a);
        }
    }
    if rcpts.is_empty() && !draft {
        return Err(Error::NoRecipients);
    }
    let domain = from.rsplit_once('@').map(|x| x.1).unwrap_or("localhost");
    let message_id = format!("<{}@{domain}>", hex(&stamp.random));
    let boundary = |k: u8| format!("=_atrium_{}_{k}", hex(&stamp.random[..8]));

    let mut h = Head { out: String::new() };
    h.field("Date", &[date(stamp.date)]);
    h.field("From", &mailbox_pieces(&d.from, true)?);
    if !d.to.is_empty() {
        h.field("To", &list_pieces(&d.to)?);
    }
    if !d.cc.is_empty() {
        h.field("Cc", &list_pieces(&d.cc)?);
    }
    if draft && !d.bcc.is_empty() {
        h.field("Bcc", &list_pieces(&d.bcc)?);
    }
    let subject = clean(&d.subject);
    if !subject.is_empty() {
        // Words of the subject, so folding keeps them whole.
        let pieces: Vec<String> = if words(&subject).len() == 1 && subject.is_ascii() {
            subject.split(' ').filter(|w| !w.is_empty()).map(str::to_string).collect()
        } else {
            words(&subject)
        };
        h.field("Subject", &pieces);
    }
    h.field("Message-ID", &[message_id.clone()]);
    if let Some(r) = &d.in_reply_to {
        h.field("In-Reply-To", &[msg_id(r)?]);
    }
    if !d.references.is_empty() {
        let mut refs: Vec<String> = d.references.iter().map(|r| msg_id(r)).collect::<Result<_, _>>()?;
        if refs.len() > REFERENCES_MAX {
            let tail = refs.split_off(refs.len() - (REFERENCES_MAX - 1));
            refs.truncate(1);
            refs.extend(tail);
        }
        h.field("References", &refs);
    }
    h.field("MIME-Version", &["1.0".into()]);

    // The body: text, or text and HTML as alternatives; with attachments, mixed.
    let text_part = |out: &mut String, s: &str, sub: &str| {
        let (b, enc) = text_body(s);
        let _ = write!(out, "Content-Type: text/{sub}; charset=utf-8\r\nContent-Transfer-Encoding: {enc}\r\n\r\n{b}\r\n");
    };
    let mut body = String::new();
    let mut alt = String::new();
    let alt_head = match &d.html {
        Some(html) => {
            let b = boundary(1);
            let _ = write!(alt, "--{b}\r\n");
            text_part(&mut alt, &d.text, "plain");
            let _ = write!(alt, "--{b}\r\n");
            text_part(&mut alt, html, "html");
            let _ = write!(alt, "--{b}--\r\n");
            format!("Content-Type: multipart/alternative; boundary=\"{b}\"\r\n\r\n")
        }
        None => {
            let (b, enc) = text_body(&d.text);
            alt.push_str(&b);
            alt.push_str("\r\n");
            format!("Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: {enc}\r\n\r\n")
        }
    };
    if d.attachments.is_empty() {
        h.out.push_str(&alt_head);
        body.push_str(&alt);
    } else {
        let b = boundary(2);
        h.out.push_str(&format!("Content-Type: multipart/mixed; boundary=\"{b}\"\r\n\r\n"));
        let _ = write!(body, "--{b}\r\n{alt_head}{alt}");
        for a in &d.attachments {
            let name = if clean(&a.name).is_empty() { "attachment".to_string() } else { clean(&a.name) };
            let _ = write!(
                body,
                "--{b}\r\nContent-Type: {}; {}\r\nContent-Disposition: attachment; {}\r\nContent-Transfer-Encoding: base64\r\n\r\n",
                mime_type(&a.mime),
                param("name", &name),
                param("filename", &name)
            );
            let enc = base64(&a.bytes);
            for chunk in enc.as_bytes().chunks(76) {
                body.push_str(std::str::from_utf8(chunk).unwrap_or_default());
                body.push_str("\r\n");
            }
        }
        let _ = write!(body, "--{b}--\r\n");
    }
    let mut bytes = h.out.into_bytes();
    bytes.extend_from_slice(body.as_bytes());
    if bytes.len() > MESSAGE_MAX {
        return Err(Error::TooLarge);
    }
    Ok(Written { bytes, message_id, from, rcpts })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(date(0), "Thu, 1 Jan 1970 00:00:00 +0000");
        assert_eq!(date(1_794_936_353), "Tue, 17 Nov 2026 17:25:53 +0000");
        assert_eq!(date(951_782_400), "Tue, 29 Feb 2000 00:00:00 +0000");
    }

    #[test]
    fn addresses() {
        assert_eq!(address(" priya@Example.COM ").unwrap(), "priya@example.com");
        assert_eq!(address("p@bücher.example").unwrap(), "p@xn--bcher-kva.example");
        for bad in ["no-at", "a b@x.example", "a@b", "a@@b.example", "x@y.example\r\nBcc: z@q.example", "ü@x.example", ".a@x.example"] {
            assert!(address(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn qp_lines() {
        let long = "é".repeat(60);
        let q = qp(&long);
        assert!(q.split("\r\n").all(|l| l.len() <= 76), "{q}");
        assert!(qp("a = b ").ends_with("=20"));
    }
}
