//! The `share` role: the share sheet (share-sheet.md §3). A system role,
//! provided by the share sheet service (`org.atrium.share`, running as
//! `_share`) in a sheet slot over the app's window, which the app can't
//! draw over or read.
//!
//! The app says what it would share: a title and the media types it has,
//! never the content, with the press that asked for it. The sheet spends
//! that press with frescod (purpose `SHARE`), so no app can open a sheet
//! by itself, then lists the installed apps that take one of the types
//! (`[roles] implements = ["share-target"]`, `[roles.share-target] mime`)
//! for the app's person. The person presses one; the sheet tells limend
//! (`CHOOSE`), and only then can the app embed a `share-target`, which
//! limend starts from the person's choice and nothing else
//! ([`crate::share_target`]). The app never learns which apps are
//! installed, nor which one was chosen.

use serde::{Deserialize, Serialize};

pub use crate::share_target::{valid_mime, MAX_FORMATS, MAX_TITLE};

/// Parent to sheet. Sent once, first; a second is ignored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToChild {
    Open(Open),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Open {
    /// The press in the app's window that asked to share (frescod's
    /// activation of the key or button press being handled).
    pub activation: [u8; 16],
    /// What is shared, as the sheet shows it ("Q4 report.pdf", the first
    /// line of a note): plain text, at most [`MAX_TITLE`] bytes, no
    /// control characters.
    pub title: String,
    /// The media types the app will send, best first: at most
    /// [`MAX_FORMATS`], each [`valid_mime`].
    pub mimes: Vec<String>,
}

impl ToChild {
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("encodes")
    }

    /// Decode the app's message, refusing anything out of shape.
    pub fn decode(b: &[u8]) -> Option<Self> {
        let m: Self = postcard::from_bytes(b).ok()?;
        let ToChild::Open(o) = &m;
        let ok = crate::share_target::clean(&o.title, MAX_TITLE)
            && !o.mimes.is_empty()
            && o.mimes.len() <= MAX_FORMATS
            && o.mimes.iter().all(|t| valid_mime(t));
        ok.then_some(m)
    }
}

/// Sheet to parent. The app detaches the sheet after any of them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToParent {
    /// The person chose a target: embed `share-target` now (within
    /// [`crate::catalogue::CHOICE_MS`]) and send it the item.
    Chosen,
    /// The person closed the sheet.
    Cancelled,
    /// Nothing was shown: the press wasn't a fresh one in the app's focused
    /// window, or the request was odd. `why` is for the app's log.
    Failed { why: String },
}

pub const MAX_WHY: usize = 200;

impl ToParent {
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("encodes")
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        let m: Self = postcard::from_bytes(b).ok()?;
        let ok = match &m {
            ToParent::Failed { why } => crate::share_target::clean(why, MAX_WHY),
            _ => true,
        };
        ok.then_some(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_round_trip_and_odd_ones_are_refused() {
        let ok = Open { activation: [7; 16], title: "Q4 report.pdf".into(), mimes: vec!["application/pdf".into(), "text/plain".into()] };
        let m = ToChild::Open(ok.clone());
        assert_eq!(ToChild::decode(&m.encode()), Some(m));
        for bad in [
            Open { mimes: vec![], ..ok.clone() },
            Open { mimes: vec!["text".into()], ..ok.clone() },
            Open { mimes: vec!["image/*".into()], ..ok.clone() },
            Open { mimes: vec!["Text/Plain".into()], ..ok.clone() },
            Open { mimes: vec!["text/plain".into(); MAX_FORMATS + 1], ..ok.clone() },
            Open { title: "a\u{1b}[2J".into(), ..ok.clone() },
            Open { title: "x".repeat(MAX_TITLE + 1), ..ok.clone() },
        ] {
            assert_eq!(ToChild::decode(&ToChild::Open(bad.clone()).encode()), None, "{bad:?}");
        }
        for m in [ToParent::Chosen, ToParent::Cancelled, ToParent::Failed { why: "no press".into() }] {
            assert_eq!(ToParent::decode(&m.encode()), Some(m));
        }
        assert_eq!(ToParent::decode(&ToParent::Failed { why: "\u{7}".into() }.encode()), None);
    }
}
