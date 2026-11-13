//! The `doc-viewer` role (limen.md §2.1): a jailed document worker that
//! lays out and draws a document the parent doesn't trust, in its own
//! window, and scrolls it with Pergola's `ScrollView`.
//!
//! The parent hands the document over as a read-only descriptor with
//! `Load` (a Navigator recording, or plain HTML), never as text it parsed,
//! or, for the Navigator, as a ticket with `Open`: the UI never holds the
//! page's bytes (navigator-ui.md §1), so the child redeems the ticket on
//! navigatord's page socket (`navigator_control::redeem`).
//! The child sends back only what the table in §2.1 allows: a title and
//! anchors, link presses and hovers, and errors. It never sends the
//! parent document text.
//!
//! Both sides decode with [`ToChild::decode`] / [`ToParent::decode`],
//! which refuse anything out of shape.

use serde::{Deserialize, Serialize};

/// Longest title (§2.1: at most 256 bytes).
pub const TITLE_MAX: usize = 256;
/// Most anchors in `Loaded`, and the longest one.
pub const ANCHORS_MAX: usize = 256;
pub const ANCHOR_MAX: usize = 256;
/// Longest URL either way (a link's target, a document's base).
pub const URL_MAX: usize = 16 * 1024;
/// The largest document a child reads from a `Load` descriptor.
pub const DOC_MAX: u64 = 128 << 20;

/// What the descriptor sent with `Load` holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DocKind {
    /// An `atrium-navigator-recording` (the converter's output).
    Recording,
    /// An HTML document (a mail body, a help page).
    Html,
}

/// Parent to child.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ToChild {
    /// The document comes as the descriptor limend passes with this
    /// message. `base` is the address relative links resolve against; it
    /// only chooses a base (the parent decides what a link does).
    Load { kind: DocKind, base: String, dark: bool },
    SetTheme { dark: bool },
    ScrollTo { anchor: String },
    /// The Navigator's page: redeem `ticket` on navigatord's page socket
    /// for the recording and its address.
    Open { ticket: [u8; 16], dark: bool },
}

/// Child to parent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ToParent {
    Loaded { title: String, anchors: Vec<String> },
    /// Only after a press in the child: the link's href as the document
    /// spells it, unresolved, so the parent (navigatord, for the
    /// Navigator) can check the document carries it and decide what it
    /// does.
    LinkActivated { url: String },
    /// The link under the pointer, resolved against the document's base,
    /// or none (advisory, for the status bar; navigator-ui.md §5.7).
    LinkHover { url: Option<String> },
    Error { code: u16 },
}

/// `Error` codes.
pub mod error {
    /// No descriptor came with `Load`, or it could not be read.
    pub const NO_DOCUMENT: u16 = 1;
    /// The document is over [`super::DOC_MAX`].
    pub const TOO_BIG: u16 = 2;
    /// The document could not be parsed or laid out.
    pub const UNREADABLE: u16 = 3;
}

fn clean_text(s: &str, max: usize) -> bool {
    s.len() <= max && !s.chars().any(char::is_control)
}

impl ToChild {
    pub fn decode(b: &[u8]) -> Option<Self> {
        let m: Self = postcard::from_bytes(b).ok()?;
        let ok = match &m {
            ToChild::Load { base, .. } => clean_text(base, URL_MAX),
            ToChild::SetTheme { .. } => true,
            ToChild::ScrollTo { anchor } => clean_text(anchor, ANCHOR_MAX),
            ToChild::Open { .. } => true,
        };
        ok.then_some(m)
    }

    pub fn encode(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("encodes")
    }
}

impl ToParent {
    pub fn decode(b: &[u8]) -> Option<Self> {
        let m: Self = postcard::from_bytes(b).ok()?;
        let ok = match &m {
            ToParent::Loaded { title, anchors } => {
                clean_text(title, TITLE_MAX) && anchors.len() <= ANCHORS_MAX && anchors.iter().all(|a| clean_text(a, ANCHOR_MAX))
            }
            ToParent::LinkActivated { url } => !url.is_empty() && clean_text(url, URL_MAX),
            ToParent::LinkHover { url } => url.as_ref().is_none_or(|u| clean_text(u, URL_MAX)),
            ToParent::Error { .. } => true,
        };
        ok.then_some(m)
    }

    pub fn encode(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("encodes")
    }
}

/// Cut `s` to at most `max` bytes on a character boundary (a child
/// trimming a long title before it sends it).
pub fn cut(s: &str, max: usize) -> String {
    let s: String = s.chars().filter(|c| !c.is_control()).collect();
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_refusals() {
        for m in [
            ToChild::Load { kind: DocKind::Recording, base: "https://atlas.example/wiki/A".into(), dark: false },
            ToChild::SetTheme { dark: true },
            ToChild::ScrollTo { anchor: "plan".into() },
            ToChild::Open { ticket: [7; 16], dark: true },
        ] {
            assert_eq!(ToChild::decode(&m.encode()), Some(m));
        }
        for m in [
            ToParent::Loaded { title: "Atrium".into(), anchors: vec!["plan".into()] },
            ToParent::LinkActivated { url: "https://atlas.example/wiki/B".into() },
            ToParent::LinkHover { url: None },
            ToParent::Error { code: error::UNREADABLE },
        ] {
            assert_eq!(ToParent::decode(&m.encode()), Some(m));
        }
        assert_eq!(ToParent::decode(&ToParent::Loaded { title: "x".repeat(TITLE_MAX + 1), anchors: vec![] }.encode()), None);
        assert_eq!(ToParent::decode(&ToParent::LinkActivated { url: String::new() }.encode()), None);
        assert_eq!(ToParent::decode(&ToParent::LinkActivated { url: "a\nb".into() }.encode()), None);
        assert_eq!(ToParent::decode(&[0xff, 0x00]), None);
        assert_eq!(cut("héllo", 2), "h");
    }
}
