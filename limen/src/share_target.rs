//! The `share-target` role (limen.md §2.4, share-sheet.md §4): an app the
//! person chose in the share sheet receives one item, in a sheet slot over
//! the sharing app's window, and says whether it took it.
//!
//! limend starts the target only from the person's choice in a share sheet
//! the parent itself opened ([`crate::share`]); there is no default
//! provider, so an app can't push content into another app the person
//! didn't pick.
//!
//! The item is the same shape as a Tabula clipboard entry (tabula.md §0):
//! up to [`MAX_FORMATS`] `(mime, bytes)` formats, best first. Small ones
//! travel in the message ([`Body::Inline`]); larger ones (a picture, a
//! PDF) as a read-only descriptor each ([`Body::Fd`]), one
//! [`ToChild::Part`] per format, sent with `SEND_FD` right after
//! [`ToChild::Share`]. limend checks each is a read-only regular file or
//! pipe and narrows it, as for any role that takes one.
//!
//! The target treats the item as hostile input, as any paste.

use serde::{Deserialize, Serialize};

/// Formats in one item (as a Tabula entry).
pub const MAX_FORMATS: usize = 16;
/// Bytes of a title.
pub const MAX_TITLE: usize = 200;
/// Inline bytes in one [`ToChild::Share`], all formats together: what fits
/// in one Limen message with room for the rest.
pub const MAX_INLINE: usize = 48 * 1024;
/// The size a [`Body::Fd`] format may declare.
pub const MAX_PART: u64 = 2 << 30;

/// One format's bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Body {
    /// In the message.
    Inline(Vec<u8>),
    /// A descriptor follows as [`ToChild::Part`] with this format's index;
    /// `size` is what the sender says it holds (the target reads at most
    /// that).
    Fd { size: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Format {
    /// A media type, [`valid_mime`].
    pub mime: String,
    pub body: Body,
}

/// What is shared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    /// As the sheet showed it: plain text, at most [`MAX_TITLE`] bytes.
    pub title: String,
    pub formats: Vec<Format>,
}

impl Item {
    /// Text, inline.
    pub fn text(title: &str, text: &str) -> Self {
        Item { title: title.into(), formats: vec![Format { mime: "text/plain".into(), body: Body::Inline(text.as_bytes().to_vec()) }] }
    }

    /// The media types, in order.
    pub fn mimes(&self) -> Vec<String> {
        self.formats.iter().map(|f| f.mime.clone()).collect()
    }

    /// Indexes of the formats whose bytes come by descriptor, in the order
    /// their [`ToChild::Part`]s follow.
    pub fn fd_parts(&self) -> Vec<u8> {
        self.formats.iter().enumerate().filter(|(_, f)| matches!(f.body, Body::Fd { .. })).map(|(i, _)| i as u8).collect()
    }

    fn sane(&self) -> bool {
        let inline: usize = self.formats.iter().map(|f| match &f.body { Body::Inline(b) => b.len(), Body::Fd { .. } => 0 }).sum();
        clean(&self.title, MAX_TITLE)
            && !self.formats.is_empty()
            && self.formats.len() <= MAX_FORMATS
            && inline <= MAX_INLINE
            && self.formats.iter().all(|f| valid_mime(&f.mime) && match f.body { Body::Fd { size } => size <= MAX_PART, Body::Inline(_) => true })
            // One format per type.
            && self.formats.iter().enumerate().all(|(i, f)| !self.formats[..i].iter().any(|g| g.mime == f.mime))
    }
}

/// Parent (the sharing app) to the target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToChild {
    /// The item. Sent once, first; a second is ignored.
    Share(Item),
    /// Format `index`'s bytes: a read-only descriptor in the same frame
    /// (`SEND_FD`), for a format whose body is [`Body::Fd`].
    Part { index: u8 },
}

impl ToChild {
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("encodes")
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        let m: Self = postcard::from_bytes(b).ok()?;
        let ok = match &m {
            ToChild::Share(item) => item.sane(),
            ToChild::Part { index } => (*index as usize) < MAX_FORMATS,
        };
        ok.then_some(m)
    }
}

/// Target to parent. The parent detaches after either.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToParent {
    /// The target took the item (sent, saved, posted: its business).
    Accepted,
    /// The person cancelled in the target, or it couldn't use the item.
    Declined,
}

impl ToParent {
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("encodes")
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        postcard::from_bytes(b).ok()
    }
}

/// The target's side: gathers the [`ToChild::Share`] and the descriptors
/// of its [`Body::Fd`] formats as they arrive, so a target can wait for the
/// whole item. Feed it every message of the embed.
#[derive(Default)]
pub struct Incoming {
    item: Option<Item>,
    fds: std::collections::HashMap<u8, std::os::fd::OwnedFd>,
}

/// One whole item: its formats in order, each with its bytes.
pub struct Received {
    pub item: Item,
    fds: std::collections::HashMap<u8, std::os::fd::OwnedFd>,
}

impl Incoming {
    /// A [`ChildEvent::Message`](crate::proto::ChildEvent::Message)'s bytes.
    /// Returns false for anything out of shape (the target declines).
    pub fn message(&mut self, bytes: &[u8]) -> bool {
        match ToChild::decode(bytes) {
            Some(ToChild::Share(item)) if self.item.is_none() => {
                self.item = Some(item);
                true
            }
            _ => false,
        }
    }

    /// A [`ChildEvent::MessageFd`](crate::proto::ChildEvent::MessageFd)'s
    /// bytes and its descriptor ([`Child::take_fd`](crate::client::Child::take_fd)).
    /// Only for a format the item said comes by descriptor, once each.
    pub fn message_fd(&mut self, bytes: &[u8], fd: std::os::fd::OwnedFd) -> bool {
        let Some(ToChild::Part { index }) = ToChild::decode(bytes) else { return false };
        let wanted = self.item.as_ref().is_some_and(|i| i.fd_parts().contains(&index));
        if !wanted || self.fds.contains_key(&index) {
            return false;
        }
        self.fds.insert(index, fd);
        true
    }

    /// The item, once it and all its descriptors are here.
    pub fn take(&mut self) -> Option<Received> {
        let item = self.item.as_ref()?;
        if item.fd_parts().iter().any(|i| !self.fds.contains_key(i)) {
            return None;
        }
        Some(Received { item: self.item.take()?, fds: std::mem::take(&mut self.fds) })
    }
}

impl Received {
    /// Format `index`'s bytes, at most `max` (and at most what its sender
    /// declared). Read from the descriptor for a [`Body::Fd`] format.
    pub fn bytes(&self, index: usize, max: u64) -> std::io::Result<Vec<u8>> {
        use std::io::Read;
        let f = self.item.formats.get(index).ok_or(std::io::ErrorKind::NotFound)?;
        match &f.body {
            Body::Inline(b) => Ok(b[..b.len().min(max as usize)].to_vec()),
            Body::Fd { size } => {
                let fd = self.fds.get(&(index as u8)).ok_or(std::io::ErrorKind::NotFound)?;
                let mut out = Vec::new();
                let mut f = std::fs::File::from(fd.try_clone()?);
                // From the start each time; a pipe can't seek and reads once.
                let _ = std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(0));
                f.take(max.min(*size)).read_to_end(&mut out)?;
                Ok(out)
            }
        }
    }

    /// The first format whose type `accept` takes (patterns as in a
    /// manifest's `[roles.share-target] mime`), by index.
    pub fn best(&self, accept: &[String]) -> Option<usize> {
        self.item.formats.iter().position(|f| accepts(accept, &f.mime))
    }
}

/// A media type as offers carry them: `type/subtype`, lower case
/// `a-z 0-9 . + -`, at most 127 bytes (tabula.md §3.1). No wildcards.
pub fn valid_mime(t: &str) -> bool {
    let part = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'+' | b'-'));
    t.len() <= 127 && matches!(t.split_once('/'), Some((a, b)) if part(a) && part(b))
}

/// Whether a target declaring `patterns` (`[roles.share-target] mime`:
/// `text/plain`, `image/*`, `*/*`) takes `mime`. No patterns: anything.
pub fn accepts(patterns: &[String], mime: &str) -> bool {
    if patterns.is_empty() {
        return true;
    }
    let ty = mime.split_once('/').map_or(mime, |(t, _)| t);
    patterns.iter().any(|p| match p.split_once('/') {
        Some(("*", "*")) => true,
        Some((t, "*")) => t == ty,
        _ => p == mime,
    })
}

pub(crate) fn clean(s: &str, max: usize) -> bool {
    s.len() <= max && !s.chars().any(|c| c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn photo() -> Item {
        Item {
            title: "Lake at dawn".into(),
            formats: vec![
                Format { mime: "image/png".into(), body: Body::Fd { size: 2_000_000 } },
                Format { mime: "text/plain".into(), body: Body::Inline(b"Lake at dawn".to_vec()) },
            ],
        }
    }

    #[test]
    fn items_round_trip_and_odd_ones_are_refused() {
        for m in [ToChild::Share(photo()), ToChild::Share(Item::text("Note", "hello")), ToChild::Part { index: 0 }] {
            assert_eq!(ToChild::decode(&m.encode()), Some(m));
        }
        assert_eq!(photo().fd_parts(), [0]);
        let mut twice = photo();
        twice.formats[1].mime = "image/png".into();
        let mut big = Item::text("Big", "");
        big.formats[0].body = Body::Inline(vec![b'a'; MAX_INLINE + 1]);
        let mut huge = photo();
        huge.formats[0].body = Body::Fd { size: MAX_PART + 1 };
        for bad in [
            Item { formats: vec![], ..photo() },
            Item { title: "invoice\u{202e}fdp.exe".into(), ..photo() },
            Item { title: "a\nb".into(), ..photo() },
            Item { formats: vec![Format { mime: "image/*".into(), body: Body::Inline(vec![]) }], ..photo() },
            Item { formats: (0..=MAX_FORMATS).map(|i| Format { mime: format!("text/x{i}"), body: Body::Inline(vec![]) }).collect(), ..photo() },
            twice,
            big,
            huge,
        ] {
            assert_eq!(ToChild::decode(&ToChild::Share(bad.clone()).encode()), None, "{bad:?}");
        }
        assert_eq!(ToChild::decode(&ToChild::Part { index: MAX_FORMATS as u8 }.encode()), None);
        for m in [ToParent::Accepted, ToParent::Declined] {
            assert_eq!(ToParent::decode(&m.encode()), Some(m));
        }
    }

    #[test]
    fn a_target_waits_for_the_whole_item() {
        let path = std::env::temp_dir().join(format!("limen-incoming-{}", std::process::id()));
        std::fs::write(&path, b"0123456789").unwrap();
        let fd = || std::os::fd::OwnedFd::from(std::fs::File::open(&path).unwrap());
        let mut item = photo();
        item.formats[0].body = Body::Fd { size: 4 };
        let mut i = Incoming::default();
        assert!(!i.message_fd(&ToChild::Part { index: 0 }.encode(), fd()), "a part before its item");
        assert!(i.message(&ToChild::Share(item.clone()).encode()));
        assert!(!i.message(&ToChild::Share(item.clone()).encode()), "a second item");
        assert!(i.take().is_none(), "the picture isn't here yet");
        assert!(!i.message_fd(&ToChild::Part { index: 1 }.encode(), fd()), "an inline format's part");
        assert!(i.message_fd(&ToChild::Part { index: 0 }.encode(), fd()));
        assert!(!i.message_fd(&ToChild::Part { index: 0 }.encode(), fd()), "twice");
        let r = i.take().unwrap();
        assert_eq!(r.bytes(0, 100).unwrap(), b"0123", "no more than declared");
        assert_eq!(r.bytes(0, 2).unwrap(), b"01");
        assert_eq!(r.bytes(1, 100).unwrap(), b"Lake at dawn");
        assert_eq!(r.best(&["text/*".to_string()]), Some(1));
        assert_eq!(r.best(&[]), Some(0));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn what_a_target_takes() {
        let p = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(accepts(&p(&[]), "image/png"));
        assert!(accepts(&p(&["*/*"]), "application/pdf"));
        assert!(accepts(&p(&["image/*"]), "image/png"));
        assert!(!accepts(&p(&["image/*"]), "imagex/png"));
        assert!(accepts(&p(&["text/plain", "text/uri-list"]), "text/uri-list"));
        assert!(!accepts(&p(&["text/plain"]), "text/html"));
        assert!(valid_mime("application/vnd.oasis.opendocument.text") && !valid_mime("text") && !valid_mime("text/*") && !valid_mime("Text/plain"));
    }
}
