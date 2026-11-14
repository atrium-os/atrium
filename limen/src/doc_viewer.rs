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
    /// for the recording and its address, and show it scrolled to `scroll`
    /// px from the top (a parked page coming back where it was), laid out
    /// as `reading` says.
    Open { ticket: [u8; 16], reading: Reading, scroll: f32 },
    /// The reader's controls changed (navigator-ui.md §7): lay the page out
    /// again with them, keeping the reader's place.
    SetReading { reading: Reading },
    /// Find `query` in the page (case folded; empty clears): `step` 0 goes
    /// to the first match at or after the top of the view, 1 to the next,
    /// -1 to the previous. Answered by `Found`. The child scrolls to the
    /// match and marks it; the parent never gets the page's text.
    Find { query: String, step: i8 },
    /// The parent is about to close this child (the page is being parked,
    /// navigator-ui.md §4.4): answer `Parked` with what it needs to bring
    /// the page back as it was.
    Park,
}

/// How the reader wants the page laid out (navigator-ui.md §7; backend
/// §4.9): text size and page zoom in 1/64 (64 = 100%), the appearance,
/// and whether the measure is held to a comfortable line length.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reading {
    pub text_64: u16,
    pub zoom_64: u16,
    pub dark: bool,
    pub comfortable: bool,
}

impl Reading {
    pub const TEXT_MIN: u16 = 32;
    pub const TEXT_MAX: u16 = 256;
    pub const ZOOM_MIN: u16 = 16;
    pub const ZOOM_MAX: u16 = 512;

    pub fn plain(dark: bool) -> Self { Reading { text_64: 64, zoom_64: 64, dark, comfortable: false } }

    pub fn ok(&self) -> bool {
        (Self::TEXT_MIN..=Self::TEXT_MAX).contains(&self.text_64) && (Self::ZOOM_MIN..=Self::ZOOM_MAX).contains(&self.zoom_64)
    }
}

/// Longest find query, and the most matches a child counts.
pub const QUERY_MAX: usize = 256;
pub const FOUND_MAX: u32 = 1000;

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
    /// The answer to `Park`: how far down the page is scrolled, in px.
    Parked { scroll: f32 },
    /// The answer to `Find`: how many matches (at most `FOUND_MAX`), which
    /// one is current (from 1; 0 with none), and where it is now in the
    /// child's window (x, y, w, h px), for the parent to point at.
    Found { count: u32, current: u32, rect: Option<[f32; 4]> },
    /// A chrome shortcut pressed while the child had focus, which the
    /// child does not use itself (navigator-ui.md §9): Ctrl or Alt with a
    /// character, Tab, Left or Right, or Escape. The parent runs it through
    /// its own keyboard map and acts only on what that map has; it is no
    /// more than a press the person could have made with the chrome focused.
    Key { key: ChromeKey, chars: String, ctrl: bool, shift: bool, alt: bool },
    /// The person zoomed the page in the child (Ctrl-plus, Ctrl-minus,
    /// Ctrl-0, a pinch) and it settled here, in 1/64: for the parent to
    /// remember (navigator-ui.md §7).
    Zoomed { zoom_64: u16 },
}

/// The keys `Key` may carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChromeKey {
    Char,
    Tab,
    Left,
    Right,
    Escape,
    /// F6 (Shift-F6): the next (previous) part of the chrome has the keys
    /// (navigator-ui.md §9).
    F6,
}

/// The longest `chars` in `Key` (one character).
pub const KEY_CHARS_MAX: usize = 4;

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

/// Largest scroll offset either side sends (px).
pub const SCROLL_MAX: f32 = 1.0e7;

fn scroll_ok(y: f32) -> bool {
    y.is_finite() && (0.0..=SCROLL_MAX).contains(&y)
}

impl ToChild {
    pub fn decode(b: &[u8]) -> Option<Self> {
        let m: Self = postcard::from_bytes(b).ok()?;
        let ok = match &m {
            ToChild::Load { base, .. } => clean_text(base, URL_MAX),
            ToChild::SetTheme { .. } => true,
            ToChild::ScrollTo { anchor } => clean_text(anchor, ANCHOR_MAX),
            ToChild::Open { scroll, reading, .. } => scroll_ok(*scroll) && reading.ok(),
            ToChild::SetReading { reading } => reading.ok(),
            ToChild::Find { query, step } => clean_text(query, QUERY_MAX) && (-1..=1).contains(step),
            ToChild::Park => true,
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
            ToParent::Parked { scroll } => scroll_ok(*scroll),
            ToParent::Found { count, current, rect } => {
                *count <= FOUND_MAX && *current <= *count && (*current == 0) == rect.is_none()
                    && rect.is_none_or(|r| r.iter().all(|v| v.is_finite() && v.abs() <= SCROLL_MAX))
            }
            ToParent::Zoomed { zoom_64 } => (Reading::ZOOM_MIN..=Reading::ZOOM_MAX).contains(zoom_64),
            ToParent::Key { key, chars, ctrl, alt, .. } => {
                clean_text(chars, KEY_CHARS_MAX) && chars.chars().count() <= 1 && (*key != ChromeKey::Char || !chars.is_empty())
                    && (matches!(key, ChromeKey::Escape | ChromeKey::F6) || *ctrl || *alt)
            }
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
            ToChild::Open { ticket: [7; 16], reading: Reading::plain(true), scroll: 240.0 },
            ToChild::SetReading { reading: Reading { text_64: 96, zoom_64: 128, dark: false, comfortable: true } },
            ToChild::Find { query: "impluvium".into(), step: 1 },
            ToChild::Park,
        ] {
            assert_eq!(ToChild::decode(&m.encode()), Some(m));
        }
        for m in [
            ToParent::Loaded { title: "Atrium".into(), anchors: vec!["plan".into()] },
            ToParent::LinkActivated { url: "https://atlas.example/wiki/B".into() },
            ToParent::LinkHover { url: None },
            ToParent::Error { code: error::UNREADABLE },
            ToParent::Parked { scroll: 0.0 },
            ToParent::Found { count: 3, current: 2, rect: Some([10.0, 200.0, 80.0, 18.0]) },
            ToParent::Found { count: 0, current: 0, rect: None },
            ToParent::Key { key: ChromeKey::Char, chars: "f".into(), ctrl: true, shift: false, alt: false },
            ToParent::Zoomed { zoom_64: 96 },
            ToParent::Key { key: ChromeKey::Escape, chars: String::new(), ctrl: false, shift: false, alt: false },
        ] {
            assert_eq!(ToParent::decode(&m.encode()), Some(m));
        }
        assert_eq!(ToParent::decode(&ToParent::Loaded { title: "x".repeat(TITLE_MAX + 1), anchors: vec![] }.encode()), None);
        assert_eq!(ToParent::decode(&ToParent::LinkActivated { url: String::new() }.encode()), None);
        assert_eq!(ToParent::decode(&ToParent::LinkActivated { url: "a\nb".into() }.encode()), None);
        assert_eq!(ToParent::decode(&[0xff, 0x00]), None);
        for y in [f32::NAN, f32::INFINITY, -1.0, SCROLL_MAX * 2.0] {
            assert_eq!(ToParent::decode(&ToParent::Parked { scroll: y }.encode()), None);
            assert_eq!(ToChild::decode(&ToChild::Open { ticket: [0; 16], reading: Reading::plain(false), scroll: y }.encode()), None);
        }
        let r = |text_64, zoom_64| ToChild::SetReading { reading: Reading { text_64, zoom_64, dark: false, comfortable: false } };
        for bad in [r(0, 64), r(64, 0), r(Reading::TEXT_MAX + 1, 64), r(64, Reading::ZOOM_MAX + 1)] {
            assert_eq!(ToChild::decode(&bad.encode()), None);
        }
        assert_eq!(ToChild::decode(&ToChild::Find { query: "x".repeat(QUERY_MAX + 1), step: 0 }.encode()), None);
        assert_eq!(ToChild::decode(&ToChild::Find { query: "a\nb".into(), step: 0 }.encode()), None);
        assert_eq!(ToChild::decode(&ToChild::Find { query: "a".into(), step: 2 }.encode()), None);
        assert_eq!(ToParent::decode(&ToParent::Found { count: 2, current: 3, rect: Some([0.0; 4]) }.encode()), None);
        assert_eq!(ToParent::decode(&ToParent::Found { count: FOUND_MAX + 1, current: 0, rect: None }.encode()), None);
        assert_eq!(ToParent::decode(&ToParent::Found { count: 1, current: 1, rect: None }.encode()), None);
        assert_eq!(ToParent::decode(&ToParent::Key { key: ChromeKey::Char, chars: "f".into(), ctrl: false, shift: false, alt: false }.encode()), None, "a plain letter is typing");
        assert_eq!(ToParent::decode(&ToParent::Key { key: ChromeKey::Char, chars: "fw".into(), ctrl: true, shift: false, alt: false }.encode()), None);
        assert_eq!(ToParent::decode(&ToParent::Key { key: ChromeKey::Char, chars: String::new(), ctrl: true, shift: false, alt: false }.encode()), None);
        assert_eq!(cut("héllo", 2), "h");
    }
}
