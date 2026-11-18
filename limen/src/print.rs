//! The `print` role (printing.md §5.1): the system's print sheet, drawn by
//! prelum-sheet over the app's window. The app hands over a PDF of what it
//! wants printed; the person picks the printer and the options in a window
//! the app can't draw over or read; the app hears only how it ended.
//!
//! - [`ToChild::Print`] comes with the PDF's descriptor (`SEND_FD`, checked
//!   and narrowed to read by limend). The sheet never parses it: previews
//!   are drawn by a render jail through prelumd.
//! - With `can_relayout`, the sheet may ask the app to lay the document out
//!   again for another paper size or orientation ([`ToParent::Relayout`]);
//!   the app answers with [`ToChild::Document`] and a new descriptor. It
//!   learns the paper's size, nothing about the printer.
//! - **Save as PDF**: the sheet hands back the PDF as it would print
//!   (pages, pages per sheet, scale) with [`ToParent::SavePdf`] (`EMIT_FD`,
//!   read-only). `pergola::print` puts it through the picker's Save, so the
//!   app never learns where it went.
//!
//! The embed ends after [`ToParent::Queued`], [`ToParent::Saved`],
//! [`ToParent::Cancelled`] or [`ToParent::Failed`]: one job per sheet. No
//! printer name or job id ever reaches the app.

use serde::{Deserialize, Serialize};

pub const MAX_TITLE: usize = 200;
/// The most a paper side may be, in points (5 m).
pub const MAX_PT: u32 = 14_400;

/// A paper size in points (1/72 inch), portrait: `w <= h`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paper {
    pub w_pt: u32,
    pub h_pt: u32,
}

impl Paper {
    pub const A4: Paper = Paper { w_pt: 595, h_pt: 842 };
    pub const LETTER: Paper = Paper { w_pt: 612, h_pt: 792 };

    fn sane(&self) -> bool {
        (72..=MAX_PT).contains(&self.w_pt) && (72..=MAX_PT).contains(&self.h_pt)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Orientation {
    Portrait,
    Landscape,
}

/// Parent to child.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToChild {
    /// Show the sheet for the PDF that comes with this message. Sent once,
    /// first; a second is ignored.
    Print {
        /// What the document is called (history, the printer's job name).
        title: String,
        /// The paper the app laid it out for, if it knows.
        page_size_hint: Option<Paper>,
        /// The app can lay the document out again ([`ToParent::Relayout`]).
        can_relayout: bool,
    },
    /// The document laid out again, after a [`ToParent::Relayout`]; its
    /// descriptor comes with this message.
    Document,
}

impl ToChild {
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("encodes")
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        let m: Self = postcard::from_bytes(b).ok()?;
        let ok = match &m {
            ToChild::Print { title, page_size_hint, .. } => clean(title, MAX_TITLE) && page_size_hint.is_none_or(|p| p.sane()),
            ToChild::Document => true,
        };
        ok.then_some(m)
    }
}

/// Why the sheet couldn't take the document (printing.md §5.1): a fixed set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Why {
    NotPdf,
    Empty,
    TooLarge,
}

/// Child to parent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToParent {
    /// Lay the document out again for this paper (only if the app said it
    /// can); answer with [`ToChild::Document`].
    Relayout { paper: Paper, orientation: Orientation, margins_pt: u32 },
    /// The PDF as it would print, for the picker's Save; its descriptor
    /// (read-only) comes with this message. `name` is a suggestion
    /// ("Report.pdf"). The app answers nothing; the embed stays until it
    /// sends [`ToParent::Saved`] or [`ToParent::Cancelled`].
    SavePdf { name: String },
    /// The job is with the printer's queue. Nothing about where.
    Queued,
    /// Save as PDF went to the app: the sheet is done.
    Saved,
    Cancelled,
    Failed(Why),
}

pub const MAX_NAME: usize = 255;

fn clean(s: &str, max: usize) -> bool {
    s.len() <= max && !s.chars().any(char::is_control)
}

impl ToParent {
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("encodes")
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        let m: Self = postcard::from_bytes(b).ok()?;
        let ok = match &m {
            ToParent::Relayout { paper, margins_pt, .. } => paper.sane() && *margins_pt <= paper.w_pt.min(paper.h_pt) / 4,
            ToParent::SavePdf { name } => !name.is_empty() && clean(name, MAX_NAME) && !name.contains('/') && name != "." && name != "..",
            _ => true,
        };
        ok.then_some(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip_and_odd_ones_are_refused() {
        for m in [
            ToChild::Print { title: "Q4 report".into(), page_size_hint: Some(Paper::A4), can_relayout: true },
            ToChild::Print { title: String::new(), page_size_hint: None, can_relayout: false },
            ToChild::Document,
        ] {
            assert_eq!(ToChild::decode(&m.encode()), Some(m));
        }
        for bad in [
            ToChild::Print { title: "x".repeat(MAX_TITLE + 1), page_size_hint: None, can_relayout: false },
            ToChild::Print { title: "a\u{1b}[2J".into(), page_size_hint: None, can_relayout: false },
            ToChild::Print { title: "t".into(), page_size_hint: Some(Paper { w_pt: 0, h_pt: 842 }), can_relayout: false },
        ] {
            assert_eq!(ToChild::decode(&bad.encode()), None, "{bad:?}");
        }
        for m in [
            ToParent::Relayout { paper: Paper::LETTER, orientation: Orientation::Landscape, margins_pt: 36 },
            ToParent::SavePdf { name: "Q4 report.pdf".into() },
            ToParent::Queued,
            ToParent::Saved,
            ToParent::Cancelled,
            ToParent::Failed(Why::NotPdf),
        ] {
            assert_eq!(ToParent::decode(&m.encode()), Some(m));
        }
        for bad in [
            ToParent::Relayout { paper: Paper { w_pt: 595, h_pt: MAX_PT + 1 }, orientation: Orientation::Portrait, margins_pt: 0 },
            ToParent::Relayout { paper: Paper::A4, orientation: Orientation::Portrait, margins_pt: 400 },
            ToParent::SavePdf { name: "../x.pdf".into() },
            ToParent::SavePdf { name: String::new() },
        ] {
            assert_eq!(ToParent::decode(&bad.encode()), None, "{bad:?}");
        }
    }
}
