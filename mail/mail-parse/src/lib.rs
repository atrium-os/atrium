//! Mail's message parser (docs/spec/mail.md §7.1).
//!
//! Takes the raw bytes of one RFC 5322 message and returns a bounded
//! [`Summary`]: decoded headers, addresses, the MIME part tree, and the text
//! of the message in UTF-8. It does no I/O and holds no state, so it can run
//! in a parse jail with nothing but a pipe. Every length and count is capped
//! by [`Limits`]; anything over the hard limits is refused, anything over the
//! soft ones is cut.
//!
//! Also: [`dkim`] (sender verification), [`ical`] (invites), [`tnef`]
//! (winmail.dat). Not here yet: the HTML →
//! document conversion (navigator-normalize) and the postcard wire schema.

pub mod address;
pub mod decode;
pub mod dkim;
pub mod header;
pub mod ical;
pub mod tnef;
#[cfg(feature = "html")]
pub mod html;
#[cfg(feature = "jail")]
pub mod jail;
pub(crate) mod mime;

pub use address::Address;
pub use mime::{parse_date, part_bytes, sanitize_filename, Part, PartKind};

/// Hard limits refuse the message; soft ones cut what is kept.
#[derive(Debug, Clone)]
pub struct Limits {
    /// Whole message. Hard.
    pub max_message: usize,
    /// One header block (the message's or a part's). Hard.
    pub max_header_block: usize,
    /// Header fields in one block. Hard.
    pub max_header_fields: usize,
    /// MIME parts in the whole tree. Hard.
    pub max_parts: usize,
    /// Multipart nesting. Hard.
    pub max_depth: usize,
    /// Each text body kept (plain, HTML), in bytes of UTF-8. Soft.
    pub max_text: usize,
    /// Addresses kept per header. Soft.
    pub max_addresses: usize,
    /// Subject and other short strings. Soft.
    pub max_short: usize,
    /// Preview line. Soft.
    pub max_preview: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_message: 64 << 20,
            max_header_block: 1 << 20,
            max_header_fields: 1000,
            max_parts: 1000,
            max_depth: 32,
            max_text: 4 << 20,
            max_addresses: 1000,
            max_short: 1024,
            max_preview: 256,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    TooLarge,
    HeaderTooLarge,
    TooManyHeaders,
    TooManyParts,
    TooDeep,
    /// No header block at all (not a message).
    Empty,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Refused::TooLarge => "message too large",
            Refused::HeaderTooLarge => "header block too large",
            Refused::TooManyHeaders => "too many header fields",
            Refused::TooManyParts => "too many MIME parts",
            Refused::TooDeep => "MIME nesting too deep",
            Refused::Empty => "no header block",
        })
    }
}

impl std::error::Error for Refused {}

/// What Mail's engine stores and indexes about a message.
#[derive(Debug, Clone, Default)]
pub struct Summary {
    pub message_id: Option<String>,
    pub in_reply_to: Vec<String>,
    pub references: Vec<String>,
    pub subject: String,
    /// The `Date:` header as written (cleaned) and as seconds since 1970 UTC
    /// when it parses.
    pub date_raw: String,
    pub date: Option<i64>,
    pub from: Vec<Address>,
    pub sender: Option<Address>,
    pub reply_to: Vec<Address>,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub bcc: Vec<Address>,
    pub list_id: Option<String>,
    /// `List-Unsubscribe` targets (RFC 2369), `https:` and `mailto:` only.
    pub list_unsubscribe: Vec<String>,
    /// `List-Unsubscribe-Post: List-Unsubscribe=One-Click` (RFC 8058).
    pub one_click_unsubscribe: bool,
    /// The sender asked for a read receipt (never sent automatically).
    pub wants_receipt: bool,
    /// `Auto-Submitted` other than `no`, or `Precedence: bulk|list|junk`.
    pub automated: bool,
    /// Raw `DKIM-Signature` and `Authentication-Results` values, for the
    /// checks of §7.5 (later step).
    pub dkim_signatures: usize,
    pub parts: Vec<Part>,
    /// The body text: the first `text/plain` shown inline.
    pub text: Option<String>,
    /// The first `text/html` shown inline, decoded to UTF-8 but not
    /// otherwise touched; it goes to navigator-normalize in the same jail.
    pub html: Option<String>,
    /// A one-line preview from `text` (or the HTML's text when there is none).
    pub preview: String,
    /// A charset label in the message wasn't known and was read as
    /// windows-1252.
    pub unknown_charset: bool,
    /// Something was cut by a soft limit.
    pub truncated: bool,
}

/// Parses one message.
pub fn parse(raw: &[u8], limits: &Limits) -> Result<Summary, Refused> {
    if raw.len() > limits.max_message {
        return Err(Refused::TooLarge);
    }
    let (fields, body_at) = header::split(raw, limits.max_header_block, limits.max_header_fields)?;
    if fields.is_empty() {
        return Err(Refused::Empty);
    }
    let mut s = Summary::default();
    let short = limits.max_short;
    let h = |n: &str| header::get(&fields, n);
    s.subject = h("subject").map(|v| header::decode_words(v, short)).unwrap_or_default();
    s.message_id = h("message-id").and_then(|v| msg_ids(v, 1).into_iter().next());
    s.in_reply_to = h("in-reply-to").map(|v| msg_ids(v, 32)).unwrap_or_default();
    s.references = h("references").map(|v| msg_ids(v, 256)).unwrap_or_default();
    if let Some(d) = h("date") {
        s.date_raw = header::clean(&String::from_utf8_lossy(d), 128);
        s.date = mime::parse_date(&s.date_raw);
    }
    let addrs = |n: &str| h(n).map(|v| address::parse_list(v, limits.max_addresses)).unwrap_or_default();
    s.from = addrs("from");
    s.sender = addrs("sender").into_iter().next();
    s.reply_to = addrs("reply-to");
    s.to = addrs("to");
    s.cc = addrs("cc");
    s.bcc = addrs("bcc");
    s.list_id = h("list-id").map(|v| header::decode_words(v, 256)).filter(|v| !v.is_empty());
    if let Some(v) = h("list-unsubscribe") {
        s.list_unsubscribe = angle_urls(v, 8);
    }
    s.one_click_unsubscribe = h("list-unsubscribe-post")
        .map(|v| String::from_utf8_lossy(v).trim().eq_ignore_ascii_case("List-Unsubscribe=One-Click"))
        .unwrap_or(false);
    s.wants_receipt = h("disposition-notification-to").is_some();
    s.automated = h("auto-submitted").map(|v| !String::from_utf8_lossy(v).trim().to_ascii_lowercase().starts_with("no")).unwrap_or(false)
        || h("precedence")
            .map(|v| matches!(String::from_utf8_lossy(v).trim().to_ascii_lowercase().as_str(), "bulk" | "list" | "junk"))
            .unwrap_or(false);
    s.dkim_signatures = header::get_all(&fields, "dkim-signature").count();

    mime::walk(raw, &fields, body_at, limits, &mut s)?;
    s.preview = mime::preview(s.text.as_deref(), s.html.as_deref(), limits.max_preview);
    Ok(s)
}

/// `<id>` tokens from Message-ID / In-Reply-To / References.
fn msg_ids(v: &[u8], max: usize) -> Vec<String> {
    let s = String::from_utf8_lossy(v);
    let mut out = Vec::new();
    let mut rest = &*s;
    while let Some(a) = rest.find('<') {
        let Some(b) = rest[a..].find('>') else { break };
        let id = header::clean(&rest[a + 1..a + b], 998);
        if !id.is_empty() && !id.contains(char::is_whitespace) && out.len() < max {
            out.push(id);
        }
        rest = &rest[a + b + 1..];
    }
    if out.is_empty() {
        // some senders omit the brackets
        let t = header::clean(s.trim(), 998);
        if !t.is_empty() && !t.contains(char::is_whitespace) && t.contains('@') {
            out.push(t);
        }
    }
    out
}

fn angle_urls(v: &[u8], max: usize) -> Vec<String> {
    let s = String::from_utf8_lossy(v);
    let mut out = Vec::new();
    let mut rest = &*s;
    while let Some(a) = rest.find('<') {
        let Some(b) = rest[a..].find('>') else { break };
        let u: String = rest[a + 1..a + b].chars().filter(|c| !c.is_whitespace()).collect();
        let lower = u.to_ascii_lowercase();
        if (lower.starts_with("https://") || lower.starts_with("mailto:")) && u.len() <= 2048 && out.len() < max {
            out.push(header::clean(&u, 2048));
        }
        rest = &rest[a + b + 1..];
    }
    out
}
