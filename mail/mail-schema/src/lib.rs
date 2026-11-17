//! The fixed data format between Mail's engine and a parse jail
//! (docs/spec/mail.md §4, §7.1).
//!
//! The engine never parses a message; a parse jail does, and sends back a
//! **bounded summary** in the types below. The jail is the process a hostile
//! message may have taken over, so the engine treats every frame it reads as
//! a claim: [`FromJail::check`] holds each string, list and count to the
//! limits in [`Caps`] and refuses the frame otherwise, and the engine retires
//! a jail that sent one ("a jail that lies").
//!
//! Framing on the jail's stdin/stdout: a 4-byte big-endian length, then that
//! many bytes of postcard. A frame longer than [`MAX_FRAME`] is refused
//! before it is read.
//!
//! The conversation:
//!
//! ```text
//! engine → jail   Summarize { batch }      (≤ 64 messages, ≤ 32 MiB, one account)
//! jail → engine   Query { … }               any number of times, each answered by
//! engine → jail   Answer { … }              (a TXT record mail-sync looked up)
//! jail → engine   Summary { … }             one per message, in batch order
//! jail → engine   Done
//!
//! engine → jail   Open { … }                (one message, its own jail, §2.4)
//! jail → engine   Query / Answer …, then Opened { … }, Done
//! ```

use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};

/// Largest frame either side accepts: the 64 MiB message limit plus room for
/// the request around it.
pub const MAX_FRAME: usize = (64 << 20) + (1 << 20);
/// A batch (§4): one account's messages, at most this many and this many bytes.
pub const MAX_BATCH: usize = 64;
pub const MAX_BATCH_BYTES: usize = 32 << 20;
/// TXT lookups one message may ask for: a few DKIM keys and the DMARC record
/// (and its organizational domain's). More is a message trying to make the
/// mailbox send DNS queries for it.
pub const MAX_QUERIES_PER_MESSAGE: u32 = 6;

// ---- engine → jail ----

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ToJail {
    Summarize {
        /// The account's provider `authserv-id`, for its Authentication-Results.
        authserv_id: Option<String>,
        /// The persona's own addresses, to find "my" status in an invitation.
        me: Vec<String>,
        /// Seconds since the epoch, for signature expiry.
        now: i64,
        /// The messages are header blocks only (the staged first sync,
        /// §6.4): no body, so no DKIM check yet; the verdict is `Pending`
        /// for a signed message until its whole text is summarized.
        headers_only: bool,
        batch: Vec<Raw>,
    },
    Open {
        authserv_id: Option<String>,
        me: Vec<String>,
        now: i64,
        message: Raw,
        /// Pictures already loaded: resource id → natural size, so the
        /// document keeps their real space.
        loaded: Vec<(String, u32, u32)>,
    },
    /// The answer to a [`FromJail::Query`]: the TXT record's text (strings
    /// joined), or `None` for no record or a failed lookup.
    Answer { query: u32, txt: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Raw {
    /// The engine's id for the message (its index key); echoed back.
    pub id: u64,
    pub bytes: Vec<u8>,
}

// ---- jail → engine ----

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FromJail {
    /// Look up a TXT record: only `<selector>._domainkey.<domain>` or
    /// `_dmarc.<domain>`; anything else is a lie.
    Query { query: u32, name: String },
    Summary { id: u64, result: Result<Summary, Refusal> },
    Opened { id: u64, result: Result<Opened, Refusal> },
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Refusal {
    /// Over a limit: "This message is too large to show; you can save it."
    TooLarge,
    /// Not a message at all.
    NotMail,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Summary {
    pub message_id: Option<String>,
    pub in_reply_to: Vec<String>,
    pub references: Vec<String>,
    pub subject: String,
    pub date: Option<i64>,
    pub from: Vec<Address>,
    pub sender: Option<Address>,
    pub reply_to: Vec<Address>,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub bcc: Vec<Address>,
    pub list_id: Option<String>,
    pub list_unsubscribe: Vec<String>,
    pub one_click_unsubscribe: bool,
    pub wants_receipt: bool,
    pub automated: bool,
    pub parts: Vec<Part>,
    /// The body text for the index and the reading pane's plain path.
    pub text: Option<String>,
    pub preview: String,
    pub sender_check: SenderCheck,
    pub invite: Option<Invite>,
    pub unknown_charset: bool,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Address {
    pub name: String,
    pub addr: String,
    pub name_looks_like_address: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PartKind { Multipart, Text, Html, Calendar, Inline, Message, Attachment }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Part {
    /// "1.2.3", IMAP's numbering, so a part can be fetched alone.
    pub path: String,
    pub kind: PartKind,
    pub content_type: String,
    pub filename: Option<String>,
    pub content_id: Option<String>,
    /// Decoded size, estimated from the encoded size.
    pub size: u64,
    /// Where the encoded body lies in the raw message.
    pub body: (u64, u64),
}

/// §7.5: what Mail shows next to the sender, from what was checked.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SenderCheck {
    Verified { domain: String, by_provider: bool },
    #[default]
    NotVerified,
    /// A key could not be fetched; check again later.
    Pending,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InviteMethod { Request, Cancel, Reply, Publish, Counter, Other }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum When { Utc(i64), Zoned { local: String, tzid: String }, Floating(String), Date(String) }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invite {
    pub method: InviteMethod,
    pub uid: String,
    pub sequence: u32,
    pub title: String,
    pub start: Option<When>,
    pub end: Option<When>,
    pub location: String,
    pub organizer: String,
    pub attendees: u32,
    pub my_status: Option<String>,
    pub cancelled: bool,
    pub repeats: bool,
}

/// A message being opened: its summary plus what the reading pane shows.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Opened {
    pub summary: Summary,
    /// The HTML body as a profile document (§7.3), when there is one.
    pub document: Option<Document>,
    /// Files unpacked from a `winmail.dat` (TNEF), shown as attachments.
    pub unpacked: Vec<Unpacked>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Document {
    pub html: String,
    pub resources: Vec<Resource>,
    /// What was removed or could not be shown, by reason, with a count.
    pub dropped: Vec<(String, u32)>,
    /// The bytes of the pictures the message carries itself (`cid:` parts
    /// and `data:` URLs), by resource id, for the document viewer; never
    /// decoded by the engine or Mail (§7.3).
    pub pictures: Vec<(String, Vec<u8>)>,
}

/// The most pictures a document carries, each one's largest size, and all of them together.
pub const PICTURES_MAX: usize = 64;
pub const PICTURE_BYTES_MAX: usize = 16 << 20;
pub const PICTURES_BYTES_MAX: usize = 32 << 20;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Source { Cid(String), Remote(String), Data(String) }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Use { Image, Background, Other }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resource {
    pub id: String,
    pub source: Source,
    pub used_as: Use,
    pub hidden: bool,
}

impl Resource {
    /// The engine's own rule for what "Load pictures" may fetch (§7.4): it
    /// does not take the jail's word beyond `hidden`, which only ever
    /// removes something from the list.
    pub fn loadable(&self) -> bool {
        matches!(&self.source, Source::Remote(u) if u.starts_with("https://") || u.starts_with("http://"))
            && !self.hidden && self.used_as != Use::Other
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unpacked { pub name: String, pub data: Vec<u8> }

// ---- the engine's checks ----

/// What the engine accepts back. Mirrors the parser's own limits (mail-parse
/// `Limits`), so an honest jail never trips them.
#[derive(Debug, Clone, Copy)]
pub struct Caps {
    pub short: usize,
    pub addresses: usize,
    pub ids: usize,
    pub parts: usize,
    pub text: usize,
    pub preview: usize,
    pub document: usize,
    pub resources: usize,
    pub url: usize,
    pub unpacked: usize,
}

impl Default for Caps {
    fn default() -> Self {
        Caps { short: 1024, addresses: 1000, ids: 1000, parts: 1000, text: 4 << 20, preview: 256,
               document: 16 << 20, resources: 10_000, url: 8192, unpacked: 64 }
    }
}

/// Why a frame was refused. Any of these retires the jail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lie(pub &'static str);

impl std::fmt::Display for Lie {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "parse jail sent an invalid frame: {}", self.0) }
}

fn short(s: &str, c: &Caps, what: &'static str) -> Result<(), Lie> {
    if s.len() > c.short || s.chars().any(|ch| ch.is_control() && ch != '\t') { Err(Lie(what)) } else { Ok(()) }
}

fn addresses(list: &[Address], c: &Caps) -> Result<(), Lie> {
    if list.len() > c.addresses { return Err(Lie("too many addresses")) }
    for a in list { short(&a.name, c, "address name")?; short(&a.addr, c, "address")? }
    Ok(())
}

impl Summary {
    /// `raw_len`: the length of the message the engine sent, so part ranges
    /// can be checked against it.
    pub fn check(&self, c: &Caps, raw_len: u64) -> Result<(), Lie> {
        for s in [&self.subject].into_iter().chain(&self.message_id).chain(&self.list_id) { short(s, c, "header")? }
        if self.in_reply_to.len() + self.references.len() > c.ids { return Err(Lie("too many message ids")) }
        for s in self.in_reply_to.iter().chain(&self.references) { short(s, c, "message id")? }
        if self.list_unsubscribe.len() > 8 { return Err(Lie("too many unsubscribe targets")) }
        for u in &self.list_unsubscribe { if u.len() > c.url { return Err(Lie("unsubscribe target")) } }
        for l in [&self.from, &self.reply_to, &self.to, &self.cc, &self.bcc] { addresses(l, c)? }
        if let Some(s) = &self.sender { addresses(std::slice::from_ref(s), c)? }
        if self.parts.len() > c.parts { return Err(Lie("too many parts")) }
        for p in &self.parts {
            short(&p.path, c, "part path")?;
            short(&p.content_type, c, "content type")?;
            if let Some(f) = &p.filename { short(f, c, "filename")?; if f.contains(['/', '\\']) || f == ".." { return Err(Lie("filename with a path")) } }
            if let Some(i) = &p.content_id { short(i, c, "content id")? }
            if p.body.0 > p.body.1 || p.body.1 > raw_len { return Err(Lie("part outside the message")) }
        }
        if self.text.as_ref().is_some_and(|t| t.len() > c.text) { return Err(Lie("text too long")) }
        if self.preview.len() > c.preview { return Err(Lie("preview too long")) }
        if let SenderCheck::Verified { domain, .. } = &self.sender_check {
            short(domain, c, "verified domain")?;
            // Verified means aligned with From: the domain must be From's or its parent.
            let from = self.from.first().and_then(|a| a.addr.rsplit_once('@')).map(|(_, d)| d.to_ascii_lowercase());
            if !from.is_some_and(|f| f == *domain || f.ends_with(&format!(".{domain}"))) { return Err(Lie("verified domain is not From's")) }
        }
        if let Some(i) = &self.invite {
            for s in [&i.uid, &i.title, &i.location, &i.organizer] { short(s, c, "invitation field")? }
        }
        Ok(())
    }
}

impl Opened {
    pub fn check(&self, c: &Caps, raw_len: u64) -> Result<(), Lie> {
        self.summary.check(c, raw_len)?;
        if let Some(d) = &self.document {
            if d.html.len() > c.document { return Err(Lie("document too large")) }
            if d.resources.len() > c.resources { return Err(Lie("too many resources")) }
            if d.dropped.len() > 256 { return Err(Lie("too many drop reasons")) }
            let mut ids = std::collections::BTreeSet::new();
            for r in &d.resources {
                short(&r.id, c, "resource id")?;
                if !ids.insert(&r.id) { return Err(Lie("resource id used twice")) }
                let u = match &r.source { Source::Cid(s) | Source::Remote(s) => s.len(), Source::Data(s) => s.len().min(c.url) };
                if u > c.url { return Err(Lie("resource url too long")) }
            }
            if d.pictures.len() > PICTURES_MAX { return Err(Lie("too many pictures")) }
            let mut total = 0usize;
            for (id, b) in &d.pictures {
                // Only the message's own pictures: a remote one is fetched by the engine, never handed over.
                let own = d.resources.iter().any(|r| r.id == *id && matches!(r.source, Source::Cid(_) | Source::Data(_)));
                if !own { return Err(Lie("picture for no inline resource")) }
                if b.len() > PICTURE_BYTES_MAX || b.len() as u64 > raw_len { return Err(Lie("picture too large")) }
                total += b.len();
            }
            if total > PICTURES_BYTES_MAX { return Err(Lie("pictures too large")) }
        }
        if self.unpacked.len() > c.unpacked { return Err(Lie("too many unpacked files")) }
        for u in &self.unpacked {
            short(&u.name, c, "unpacked name")?;
            if u.name.contains(['/', '\\']) { return Err(Lie("unpacked name with a path")) }
            if u.data.len() as u64 > raw_len { return Err(Lie("unpacked file larger than its message")) }
        }
        Ok(())
    }
}

/// Is `name` a lookup the jail may ask for? (`<sel>._domainkey.<domain>` or
/// `_dmarc.<domain>`, each label a DNS label.)
pub fn query_allowed(name: &str) -> bool {
    if name.len() > 253 { return false }
    let labels: Vec<&str> = name.split('.').collect();
    let label_ok = |l: &str| !l.is_empty() && l.len() <= 63 && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !labels.iter().all(|l| label_ok(l)) { return false }
    let domain_after = |i: usize| labels.len() >= i + 2; // at least "x.tld"
    if labels[0].eq_ignore_ascii_case("_dmarc") { return domain_after(1) }
    labels.iter().position(|l| l.eq_ignore_ascii_case("_domainkey")).is_some_and(|i| i >= 1 && domain_after(i + 1))
}

impl FromJail {
    /// The engine's check of one frame from a jail. `raw_len` is the length
    /// of the message the frame is about (for `Summary`/`Opened`).
    pub fn check(&self, c: &Caps, raw_len: u64) -> Result<(), Lie> {
        match self {
            FromJail::Query { name, .. } => if query_allowed(name) { Ok(()) } else { Err(Lie("lookup outside _domainkey/_dmarc")) },
            FromJail::Summary { result: Ok(s), .. } => s.check(c, raw_len),
            FromJail::Opened { result: Ok(o), .. } => o.check(c, raw_len),
            _ => Ok(()),
        }
    }
}

// ---- framing ----

pub fn write_frame<T: Serialize>(w: &mut impl Write, msg: &T) -> io::Result<()> {
    let bytes = postcard::to_stdvec(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if bytes.len() > MAX_FRAME { return Err(io::Error::new(io::ErrorKind::InvalidInput, "frame too large")) }
    w.write_all(&(bytes.len() as u32).to_be_bytes())?;
    w.write_all(&bytes)?;
    w.flush()
}

/// `Ok(None)` at a clean end of stream (no partial length).
pub fn read_frame<T: for<'de> Deserialize<'de>>(r: &mut impl Read) -> io::Result<Option<T>> {
    let mut len = [0u8; 4];
    let mut got = 0;
    while got < 4 {
        let n = r.read(&mut len[got..])?;
        if n == 0 {
            return if got == 0 { Ok(None) } else { Err(io::ErrorKind::UnexpectedEof.into()) };
        }
        got += n;
    }
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME { return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large")) }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    postcard::from_bytes(&buf).map(Some).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pictures_are_only_the_message_s_own() {
        let res = |id: &str, source: Source| Resource { id: id.into(), source, used_as: Use::Image, hidden: false };
        let mut o = Opened {
            summary: Summary::default(),
            document: Some(Document {
                html: String::new(),
                resources: vec![res("mail-res:0", Source::Cid("a@b".into())), res("mail-res:1", Source::Remote("https://x.example/p.png".into()))],
                dropped: vec![],
                pictures: vec![("mail-res:0".into(), vec![1; 10])],
            }),
            unpacked: vec![],
        };
        let c = Caps::default();
        assert_eq!(o.check(&c, 100), Ok(()));
        // A remote picture is the engine's to fetch, never the jail's to hand over.
        o.document.as_mut().unwrap().pictures = vec![("mail-res:1".into(), vec![1; 10])];
        assert_eq!(o.check(&c, 100), Err(Lie("picture for no inline resource")));
        o.document.as_mut().unwrap().pictures = vec![("mail-res:9".into(), vec![1; 10])];
        assert!(o.check(&c, 100).is_err());
        o.document.as_mut().unwrap().pictures = vec![("mail-res:0".into(), vec![1; 101])];
        assert_eq!(o.check(&c, 100), Err(Lie("picture too large")));
    }
}
