//! Where a link may take a reader (threat model §2.10).
//!
//! ★★ DECIDED IN THE BROKER, ON THE URL A BROWSER WOULD ACTUALLY OPEN.
//!
//! A link's `href` reaches the UI from a rendered scene, so it comes from the
//! page, through a worker that parsed hostile bytes. Neither is in a position
//! to say where a click may go: a converter that dropped `javascript:` links
//! proves nothing about the scene a compromised worker emits. So the decision
//! is made here, in `navigatord`, on every activation, whatever produced the
//! href.
//!
//! And it is made on the RESOLVED URL, through the WHATWG parser (the `url`
//! crate), never on the href's spelling. `java\tscript:alert(1)`,
//! `  JAVASCRIPT:…` and `\x01javascript:` are all `javascript:` to a browser:
//! the parser strips tabs, newlines and leading C0 controls and lowercases the
//! scheme. A check on the raw string would have to reimplement exactly that,
//! and every difference between the two would be a bypass. A relative href
//! takes the document's scheme, so a page served over `https` cannot reach a
//! refused scheme with one.
//!
//! The URL parser is the one parse the broker does. It is the same crate the
//! converter uses for every URL it resolves, it is bounded by the href's
//! length, and its input is one string — not a document (`navigatord`'s
//! "never parses" is about the document's bytes, which still never reach it).

use url::Url;

/// What activating a link does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkAction {
    /// Scroll to a fragment of the document already open. `fragment` is
    /// empty for a bare `#`, the top of the document.
    InDocument { fragment: String },
    /// Open another page. `insecure` is the `http` mark a UI must show: the
    /// page came, or will come, over a connection anyone on the path could
    /// have rewritten.
    Navigate { url: String, insecure: bool },
    /// Hand to the scheme's handler outside the browser — only for a scheme
    /// the user allowlisted.
    Handoff { url: String, scheme: String },
}

/// Why an activation was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkRefusal {
    /// The href does not parse as a URL, alone or against the document.
    Unparseable,
    /// A scheme that is neither `https`, `http` nor allowlisted.
    Scheme(String),
    /// Longer than any link a reader needs (8 KiB), so the parse is bounded.
    TooLong(usize),
}

impl std::fmt::Display for LinkRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LinkRefusal::Unparseable => write!(f, "the link is not a URL"),
            LinkRefusal::Scheme(s) => write!(f, "links to {s}: are refused (not https or http, and not allowlisted)"),
            LinkRefusal::TooLong(n) => write!(f, "the link is {n} bytes, over the {MAX_HREF} allowed"),
        }
    }
}

/// An href longer than this is refused before it is parsed. Browsers accept
/// far more, but a link a reader follows by hand does not need it, and a data
/// URL — the usual reason for a long one — is refused anyway.
pub const MAX_HREF: usize = 8 * 1024;

/// Schemes the user has chosen to hand to another program (`mailto`, `tel`,
/// an app's own). Empty by default: every scheme other than `https` and
/// `http` is refused until the user names it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SchemeAllowlist(Vec<String>);

impl SchemeAllowlist {
    /// Allow a scheme, given without its colon.
    ///
    /// ★ `javascript` (and `vbscript`) cannot be allowlisted. The document lane
    /// has no script to run it in, so an allowlist entry would not mean
    /// "run it"; it would mean "hand page-authored code to whatever program
    /// claims the scheme". `https` and `http` need no entry, and a name that
    /// is not a scheme at all is refused rather than stored unmatched.
    pub fn allow(&mut self, scheme: &str) -> Result<(), String> {
        let s = scheme.trim_end_matches(':').to_ascii_lowercase();
        let valid = s.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
            && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
        if !valid { return Err(format!("{scheme:?} is not a URL scheme")) }
        if matches!(s.as_str(), "javascript" | "vbscript") {
            return Err(format!("{s}: cannot be allowlisted: the document lane runs no script"));
        }
        if matches!(s.as_str(), "https" | "http") { return Ok(()) }
        if !self.0.contains(&s) { self.0.push(s) }
        Ok(())
    }

    pub fn allows(&self, scheme: &str) -> bool { self.0.iter().any(|s| s == scheme) }
}

/// Decide what activating `href` in the document at `document_url` does.
pub fn decide(document_url: &str, href: &str, allow: &SchemeAllowlist)
    -> Result<LinkAction, LinkRefusal>
{
    if href.len() > MAX_HREF { return Err(LinkRefusal::TooLong(href.len())) }
    // ★ The document's own URL is parsed too, and an unusable one leaves only
    // absolute links working: a relative href resolved against nothing is not
    // a URL, and guessing a base is how a link ends up somewhere unintended.
    let base = Url::parse(document_url).ok();
    let target = match &base {
        Some(b) => b.join(href),
        None => Url::parse(href),
    }.map_err(|_| LinkRefusal::Unparseable)?;

    // In-document: the same document (ignoring the fragment) with a fragment.
    // Compared on the parsed forms, so `#x`, `?q#x` against `?q` and the
    // absolute spelling of this page's URL all agree.
    if let (Some(b), Some(frag)) = (&base, target.fragment()) {
        let mut a = b.clone();
        a.set_fragment(None);
        let mut t = target.clone();
        t.set_fragment(None);
        if a == t && matches!(b.scheme(), "https" | "http") {
            return Ok(LinkAction::InDocument { fragment: frag.to_string() });
        }
    }
    match target.scheme() {
        "https" => Ok(LinkAction::Navigate { url: target.into(), insecure: false }),
        "http" => Ok(LinkAction::Navigate { url: target.into(), insecure: true }),
        s if allow.allows(s) => {
            let scheme = s.to_string();
            Ok(LinkAction::Handoff { url: target.into(), scheme })
        }
        s => Err(LinkRefusal::Scheme(s.to_string())),
    }
}

/// ★★ HOW AN HREF CAN BE SPELLED IN A RECORDING, for the broker's bound
/// (threat model §2.10): a byte search in the recording it holds, which it
/// does not parse.
///
/// A link reaches a recording in two forms, both written by the converter's
/// emitter: as an attribute of serialized HTML inside a JSON string (the
/// document, or an `insert` effect's markup) — `href=\"…\"`, HTML-escaped
/// by the serializer and then JSON-escaped; and as an `attribute` effect's
/// value — a JSON string token on its own. A recording spelled any other
/// way (another emitter's escaping) fails this check and its links are
/// refused: the bound fails CLOSED.
pub fn recording_spellings(href: &str) -> [Vec<u8>; 2] {
    let attr = format!("href=\\\"{}\\\"", json_body(&navigator_dom::dom::escape(href)));
    let token = format!("\"{}\"", json_body(href));
    [attr.into_bytes(), token.into_bytes()]
}

/// A JSON string's body as the converter's emitter writes it
/// (navigator-prerender `artifact::json_str`, without the quotes).
fn json_body(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// ★ The PRECISE question, for whoever holds the current document: does an
/// `<a>` or `<area>` in it carry exactly this href? Connected elements only —
/// a link a transition removed is no longer on the page.
pub fn document_carries(dom: &navigator_dom::Dom, href: &str) -> bool {
    ["a", "area"].iter().any(|t| dom.by_tag(t).into_iter().any(|h| dom.attr(h, "href") == Some(href)))
}
