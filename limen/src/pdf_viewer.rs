//! The `pdf-viewer` role (folium.md §8, D4): a jailed child that shows a
//! PDF the parent doesn't trust, in its own window. Its own role rather
//! than a kind of `doc-viewer`, because PDF is a separate parser and a
//! separate threat surface, so it gets a separate binary and jail.
//!
//! The parent hands the file over as a read-only descriptor with `Load`.
//! The child sends back only the page count and title, link presses (web
//! and mail addresses only), find results and errors; it never sends the
//! document's text. A password is typed into the child's own sheet, never
//! the parent's: the parent only learns one is needed.
//!
//! Both sides decode with [`ToChild::decode`] / [`ToParent::decode`],
//! which refuse anything out of shape.

use serde::{Deserialize, Serialize};

pub use crate::doc_viewer::{cut, QUERY_MAX, TITLE_MAX, URL_MAX};

/// The most pages a `Loaded` reports (Folium's own ceiling).
pub const PAGES_MAX: u32 = 1 << 20;
/// Zoom in 1/64 (64 = 100%), as the Navigator's reader sends it.
pub const ZOOM_MIN: u16 = 16;
pub const ZOOM_MAX: u16 = 4096;
/// The most matches a child counts.
pub const FOUND_MAX: u32 = 10_000;

/// How the pages are fitted when the zoom isn't a percent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Fit {
    Width,
    Page,
    /// `zoom_64` / 64 of the page's size at 96 px per inch.
    Percent { zoom_64: u16 },
}

/// Parent to child.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ToChild {
    /// The PDF comes as the descriptor limend passes with this message.
    Load { dark: bool },
    SetTheme { dark: bool },
    /// Show page `page` (from 0) at the top.
    GoTo { page: u32 },
    /// Find `query` (case folded; empty clears): `step` 0 the first match
    /// at or after the top of the view, 1 the next, -1 the previous.
    Find { query: String, step: i8 },
    SetZoom { fit: Fit },
    /// Export the supplied descriptor under the strict visual-copy profile.
    ExportImages,
}

/// Child to parent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ToParent {
    Loaded { pages: u32, title: String },
    /// Only after a press in the child, and only `http`, `https` or
    /// `mailto`: the parent decides what it does.
    LinkActivated { url: String },
    /// The answer to `Find`.
    Found { count: u32, current: u32, page: Option<u32> },
    /// The page at the top of the view changed (for a page number).
    AtPage { page: u32 },
    Error { code: u16 },
    /// The file is encrypted; the child shows its own password sheet.
    PasswordNeeded,
    /// Ctrl+P was pressed in the child: the parent may print the PDF it
    /// handed over (printing.md §5.1). It asks for nothing more; a child
    /// that sends it unasked only opens the print sheet, whose Print still
    /// needs the person's own press, at Limen's rate.
    PrintAsked,
    ImageCopy,
}

/// `Error` codes.
pub mod error {
    /// No descriptor came with `Load`, or it could not be read.
    pub const NO_DOCUMENT: u16 = 1;
    /// The file is not a PDF Folium can open.
    pub const UNREADABLE: u16 = 3;
}

fn clean_text(s: &str, max: usize) -> bool {
    s.len() <= max && !s.chars().any(char::is_control)
}

/// Addresses a child may hand the parent (folium.md §8).
pub fn link_ok(url: &str) -> bool {
    let l = url.to_ascii_lowercase();
    !url.is_empty() && clean_text(url, URL_MAX) && (l.starts_with("http://") || l.starts_with("https://") || l.starts_with("mailto:"))
}

impl ToChild {
    pub fn decode(b: &[u8]) -> Option<Self> {
        let m: Self = postcard::from_bytes(b).ok()?;
        let ok = match &m {
            ToChild::ExportImages | ToChild::Load { .. } | ToChild::SetTheme { .. } => true,
            ToChild::GoTo { page } => *page < PAGES_MAX,
            ToChild::Find { query, step } => clean_text(query, QUERY_MAX) && (-1..=1).contains(step),
            ToChild::SetZoom { fit } => match fit {
                Fit::Percent { zoom_64 } => (ZOOM_MIN..=ZOOM_MAX).contains(zoom_64),
                _ => true,
            },
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
            ToParent::Loaded { pages, title } => *pages <= PAGES_MAX && clean_text(title, TITLE_MAX),
            ToParent::LinkActivated { url } => link_ok(url),
            ToParent::Found { count, current, page } => *count <= FOUND_MAX && *current <= *count && (*current == 0) == page.is_none() && page.is_none_or(|p| p < PAGES_MAX),
            ToParent::AtPage { page } => *page < PAGES_MAX,
            ToParent::ImageCopy | ToParent::Error { .. } | ToParent::PasswordNeeded | ToParent::PrintAsked => true,
        };
        ok.then_some(m)
    }

    pub fn encode(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("encodes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_refusals() {
        for m in [
            ToChild::Load { dark: false },
            ToChild::SetTheme { dark: true },
            ToChild::GoTo { page: 12 },
            ToChild::Find { query: "impluvium".into(), step: 1 },
            ToChild::SetZoom { fit: Fit::Width },
            ToChild::SetZoom { fit: Fit::Percent { zoom_64: 128 } },
        ] {
            assert_eq!(ToChild::decode(&m.encode()), Some(m));
        }
        for m in [
            ToParent::Loaded { pages: 40, title: "Board guide".into() },
            ToParent::LinkActivated { url: "https://atlas.example/".into() },
            ToParent::LinkActivated { url: "mailto:a@b.example".into() },
            ToParent::Found { count: 3, current: 1, page: Some(7) },
            ToParent::Found { count: 0, current: 0, page: None },
            ToParent::AtPage { page: 3 },
            ToParent::Error { code: error::UNREADABLE },
            ToParent::PasswordNeeded,
            ToParent::PrintAsked,
        ] {
            assert_eq!(ToParent::decode(&m.encode()), Some(m));
        }
        for url in ["javascript:alert(1)", "file:///etc/passwd", "", "https://a\n"] {
            assert_eq!(ToParent::decode(&ToParent::LinkActivated { url: url.into() }.encode()), None, "{url}");
        }
        assert_eq!(ToParent::decode(&ToParent::Loaded { pages: PAGES_MAX + 1, title: String::new() }.encode()), None);
        assert_eq!(ToParent::decode(&ToParent::Loaded { pages: 1, title: "x".repeat(TITLE_MAX + 1) }.encode()), None);
        assert_eq!(ToParent::decode(&ToParent::Found { count: 1, current: 1, page: None }.encode()), None);
        assert_eq!(ToChild::decode(&ToChild::SetZoom { fit: Fit::Percent { zoom_64: 0 } }.encode()), None);
        assert_eq!(ToChild::decode(&ToChild::Find { query: "a".into(), step: 2 }.encode()), None);
        assert_eq!(ToChild::decode(&ToChild::GoTo { page: PAGES_MAX }.encode()), None);
        assert_eq!(ToChild::decode(&[0xff, 0xff]), None);
    }
}
