//! The `picker` role (limen.md §2.3, scrinium.md §4): the system's file
//! picker, drawn by Scrinium in a slot of the app's window. The app says
//! what it wants; the user picks in Scrinium's window, which the app can't
//! draw over or read; only what was picked reaches the app.
//!
//! - A **file** comes as an open descriptor with [`ToParent::File`]
//!   (`EMIT_FD`, checked and narrowed by limend): the app never learns
//!   where the file is, and gets no other file of the folder.
//! - A **folder** is mounted into the app's jail by dispensatord (a grant,
//!   device-assignment.md §4.8); [`ToParent::Folder`] says where. It lasts
//!   until the app's jail goes, the stick is ejected or unplugged, or the
//!   session locks.
//!
//! The embed ends after [`ToParent::Done`] or [`ToParent::Cancelled`]: one
//! pick per embed.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    /// One file, or several with `multiple`.
    File,
    /// One folder.
    Folder,
}

/// What the app asks for. A hint about what to show, never a scope: the
/// app can't name a place or a path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Filter {
    pub kind: Kind,
    /// File name extensions to show, lower case, no dot (`["txt", "md"]`);
    /// empty shows every file. At most [`MAX_EXTENSIONS`], each at most
    /// [`MAX_EXTENSION`] bytes.
    pub extensions: Vec<String>,
    pub multiple: bool,
    /// The app wants to change what it gets: files open read-write, a
    /// folder mounted read-write. The picker says so on its button.
    pub write: bool,
    /// The picker's heading ("Open a document"); empty: the picker's own.
    /// At most [`MAX_TITLE`] bytes, no control characters.
    pub title: String,
}

pub const MAX_EXTENSIONS: usize = 16;
pub const MAX_EXTENSION: usize = 16;
pub const MAX_TITLE: usize = 80;
/// The most files one pick hands over.
pub const MAX_FILES: usize = 64;
pub const MAX_NAME: usize = 255;

impl Filter {
    pub fn file() -> Self {
        Self { kind: Kind::File, extensions: Vec::new(), multiple: false, write: false, title: String::new() }
    }

    pub fn folder() -> Self {
        Self { kind: Kind::Folder, ..Self::file() }
    }

    fn sane(&self) -> bool {
        self.extensions.len() <= MAX_EXTENSIONS
            && self.extensions.iter().all(|e| !e.is_empty() && e.len() <= MAX_EXTENSION && e.chars().all(|c| c.is_ascii_alphanumeric()))
            && self.title.len() <= MAX_TITLE
            && !self.title.chars().any(char::is_control)
            && !(self.kind == Kind::Folder && self.multiple)
    }

    /// Whether a file named `name` is shown.
    pub fn shows(&self, name: &str) -> bool {
        if self.extensions.is_empty() {
            return true;
        }
        match name.rsplit_once('.') {
            Some((stem, ext)) if !stem.is_empty() => self.extensions.iter().any(|e| e.eq_ignore_ascii_case(ext)),
            _ => false,
        }
    }
}

/// Parent to child.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToChild {
    /// Show the picker. Sent once, first; a second is ignored.
    Open(Filter),
}

impl ToChild {
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("encodes")
    }

    /// Decode the parent's message, refusing anything out of shape.
    pub fn decode(b: &[u8]) -> Option<Self> {
        let m: Self = postcard::from_bytes(b).ok()?;
        let ToChild::Open(f) = &m;
        f.sane().then_some(m)
    }
}

/// Child to parent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToParent {
    /// One picked file; its descriptor comes with this message. `name` is
    /// the file's own name (no folder), for the app's title bar.
    File { name: String, size: u64, write: bool },
    /// The picked folder, mounted in the app's jail at `at` (`/media/da0s1`).
    Folder { at: String, name: String, read_only: bool },
    /// The pick is complete: `files` descriptors came before it.
    Done { files: u32 },
    Cancelled,
    /// The pick could not be handed over (the stick went, a grant failed).
    Failed { why: String },
}

pub const MAX_WHY: usize = 200;

fn clean(s: &str, max: usize) -> bool {
    s.len() <= max && !s.chars().any(char::is_control)
}

impl ToParent {
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("encodes")
    }

    /// Decode the picker's message, refusing anything out of shape.
    pub fn decode(b: &[u8]) -> Option<Self> {
        let m: Self = postcard::from_bytes(b).ok()?;
        let ok = match &m {
            ToParent::File { name, .. } => !name.is_empty() && clean(name, MAX_NAME) && !name.contains('/'),
            ToParent::Folder { at, name, .. } => at.starts_with("/media/") && clean(at, 64) && !at.split('/').any(|c| c == "..") && clean(name, MAX_NAME),
            ToParent::Done { files } => (*files as usize) <= MAX_FILES,
            ToParent::Cancelled => true,
            ToParent::Failed { why } => clean(why, MAX_WHY),
        };
        ok.then_some(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_round_trip_and_odd_ones_are_refused() {
        let f = Filter { extensions: vec!["txt".into(), "md".into()], multiple: true, title: "Open notes".into(), ..Filter::file() };
        let m = ToChild::Open(f.clone());
        assert_eq!(ToChild::decode(&m.encode()), Some(m));
        for bad in [
            Filter { extensions: vec!["t.xt".into()], ..Filter::file() },
            Filter { extensions: vec!["".into()], ..Filter::file() },
            Filter { extensions: vec!["x".into(); MAX_EXTENSIONS + 1], ..Filter::file() },
            Filter { title: "a\u{1b}[2J".into(), ..Filter::file() },
            Filter { multiple: true, ..Filter::folder() },
        ] {
            assert_eq!(ToChild::decode(&ToChild::Open(bad.clone()).encode()), None, "{bad:?}");
        }
    }

    #[test]
    fn what_a_filter_shows() {
        let f = Filter { extensions: vec!["txt".into()], ..Filter::file() };
        assert!(f.shows("a.txt") && f.shows("B.TXT"));
        assert!(!f.shows("a.md") && !f.shows(".txt") && !f.shows("txt"));
        assert!(Filter::file().shows("anything"));
    }

    #[test]
    fn picks_round_trip_and_odd_ones_are_refused() {
        for m in [
            ToParent::File { name: "notes.txt".into(), size: 12, write: false },
            ToParent::Folder { at: "/media/da0s1".into(), name: "Photos".into(), read_only: true },
            ToParent::Done { files: 2 },
            ToParent::Cancelled,
            ToParent::Failed { why: "the stick went".into() },
        ] {
            assert_eq!(ToParent::decode(&m.encode()), Some(m));
        }
        for bad in [
            ToParent::File { name: "a/b".into(), size: 1, write: false },
            ToParent::File { name: "".into(), size: 1, write: false },
            ToParent::Folder { at: "/etc".into(), name: "x".into(), read_only: true },
            ToParent::Folder { at: "/media/../etc".into(), name: "x".into(), read_only: true },
            ToParent::Done { files: MAX_FILES as u32 + 1 },
            ToParent::Failed { why: "\u{7}".into() },
        ] {
            assert_eq!(ToParent::decode(&bad.encode()), None, "{bad:?}");
        }
    }
}
