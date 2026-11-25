//! The `editor-extension` role (dev-tools.md §3.8): an Artifex plugin, an
//! app in its own jail that limend starts when Artifex names it
//! (`REQUEST_PLUGIN`), several at once.
//!
//! A plugin speaks the Language Server Protocol with Artifex over the
//! embed's messages instead of a process's stdio, plus a few Artifex
//! messages: commands for the palette, a panel (its own window in the
//! slot), a status line. One JSON-RPC message travels as one or more
//! [`ToChild::Lsp`] / [`ToParent::Lsp`] parts of at most [`MAX_PART`]
//! bytes, the last with `last: true`; [`Joiner`] puts them back together
//! and refuses a message over [`MAX_LSP`]. No `Content-Length` header: a
//! part sequence is the framing.
//!
//! Files are named `artifex:///<path in work>`; their text comes in
//! `didOpen`/`didChange`. A plugin granted `workspace-read` sees the
//! workspace read-only at `/work` in its jail ([`Init::read`]).
//!
//! Both ends treat the other as hostile: every string is bounded and
//! checked in [`ToChild::decode`] and [`ToParent::decode`].

use serde::{Deserialize, Serialize};

/// Bytes of one LSP part: a Limen message is at most 64 KiB.
pub const MAX_PART: usize = 60 * 1024;
/// Bytes of one whole LSP message. Limen carries at most 256 messages a
/// second each way, so this is about a quarter-second of parts.
pub const MAX_LSP: usize = 4 << 20;
/// Bytes of a command id, a language name.
pub const MAX_ID: usize = 64;
/// Bytes of a command label or a panel title.
pub const MAX_LABEL: usize = 80;
/// Bytes of a status line.
pub const MAX_STATUS: usize = 120;
/// Bytes of a workspace name.
pub const MAX_WORKSPACE: usize = 128;
/// Languages in one [`Init`].
pub const MAX_LANGUAGES: usize = 16;
/// Commands one plugin may register (the host keeps the first ones).
pub const MAX_COMMANDS: usize = 64;

/// What Artifex tells a plugin first, once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Init {
    /// Its languages that Artifex has files of open (Artifex's names:
    /// `rust`, `c`, `cpp`, `python`, `markdown`, ...).
    pub languages: Vec<String>,
    /// The workspace's name, for the plugin's own display.
    pub workspace: String,
    /// Whether the person granted `workspace-read` here: the workspace is
    /// at `/work`, read-only.
    pub read: bool,
}

/// Artifex to the plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToChild {
    Init(Init),
    /// Part of one LSP message; `last` ends it.
    Lsp { last: bool, part: Vec<u8> },
    /// The person ran one of its registered commands from the palette.
    /// `file` is the active file's `artifex:///` URI, `line` its caret
    /// line from 0, when there is one.
    Command { id: String, file: Option<String>, line: u32 },
    /// Its panel was shown or hidden (a tab switch): a hidden panel need
    /// not redraw.
    PanelShown(bool),
}

/// A panel request from the plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Panel {
    /// Show its panel with this title; its window comes as the embed's
    /// `READY`.
    Open { title: String },
    Close,
}

/// The plugin to Artifex.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToParent {
    /// Part of one LSP message; `last` ends it.
    Lsp { last: bool, part: Vec<u8> },
    /// Add a command to the palette: `id` is the plugin's own name for it,
    /// [`valid_id`]; `label` what the palette shows.
    RegisterCommand { id: String, label: String },
    Panel(Panel),
    /// A short line for Artifex's status bar (empty clears it).
    Status(String),
}

impl ToChild {
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("encodes")
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        let m: Self = postcard::from_bytes(b).ok()?;
        let ok = match &m {
            ToChild::Init(i) => i.languages.len() <= MAX_LANGUAGES
                && i.languages.iter().all(|l| valid_id(l))
                && clean(&i.workspace, MAX_WORKSPACE),
            ToChild::Lsp { part, .. } => part.len() <= MAX_PART,
            ToChild::Command { id, file, .. } => valid_id(id)
                && file.as_deref().is_none_or(|f| f.len() <= 4096 && f.starts_with("artifex:///") && clean(f, 4096)),
            ToChild::PanelShown(_) => true,
        };
        ok.then_some(m)
    }
}

impl ToParent {
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("encodes")
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        let m: Self = postcard::from_bytes(b).ok()?;
        let ok = match &m {
            ToParent::Lsp { part, .. } => part.len() <= MAX_PART,
            ToParent::RegisterCommand { id, label } => valid_id(id) && !label.trim().is_empty() && clean(label, MAX_LABEL),
            ToParent::Panel(Panel::Open { title }) => !title.trim().is_empty() && clean(title, MAX_LABEL),
            ToParent::Panel(Panel::Close) => true,
            ToParent::Status(s) => clean(s, MAX_STATUS),
        };
        ok.then_some(m)
    }
}

/// A command id or language name: 1–64 bytes of `a-z 0-9 . - _`.
pub fn valid_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_ID && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'_'))
}

fn clean(s: &str, max: usize) -> bool {
    crate::share_target::clean(s, max)
}

/// One LSP message (its JSON, no header) as parts of at most
/// [`MAX_PART`] bytes; `None` over [`MAX_LSP`]. `child` picks the
/// direction's enum: the parts are [`ToChild::Lsp`] or [`ToParent::Lsp`]
/// encoded.
pub fn split(json: &[u8], child: bool) -> Option<Vec<Vec<u8>>> {
    if json.len() > MAX_LSP {
        return None;
    }
    let chunks: Vec<&[u8]> = if json.is_empty() { vec![&[][..]] } else { json.chunks(MAX_PART).collect() };
    let n = chunks.len();
    Some(chunks.into_iter().enumerate().map(|(i, c)| {
        let last = i + 1 == n;
        if child {
            ToChild::Lsp { last, part: c.to_vec() }.encode()
        } else {
            ToParent::Lsp { last, part: c.to_vec() }.encode()
        }
    }).collect())
}

/// Puts LSP parts back together. A message over [`MAX_LSP`] is an error:
/// the receiver stops the peer.
#[derive(Default)]
pub struct Joiner {
    buf: Vec<u8>,
}

/// The sender broke the framing.
#[derive(Debug, PartialEq, Eq)]
pub struct TooBig;

impl Joiner {
    /// Adds one part; a whole message once `last`.
    pub fn push(&mut self, last: bool, part: &[u8]) -> Result<Option<Vec<u8>>, TooBig> {
        if part.len() > MAX_PART || self.buf.len() + part.len() > MAX_LSP {
            self.buf = Vec::new();
            return Err(TooBig);
        }
        self.buf.extend_from_slice(part);
        Ok(last.then(|| std::mem::take(&mut self.buf)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_big_message_goes_in_parts_and_comes_back_whole() {
        let json: Vec<u8> = (0..200_000u32).map(|i| b'a' + (i % 26) as u8).collect();
        let parts = split(&json, true).unwrap();
        assert_eq!(parts.len(), 4);
        assert!(parts.iter().all(|p| p.len() <= crate::catalogue::MAX_MESSAGE));
        let mut j = Joiner::default();
        let mut out = None;
        for p in &parts {
            let Some(ToChild::Lsp { last, part }) = ToChild::decode(p) else { panic!("not a part") };
            assert!(out.is_none());
            out = j.push(last, &part).unwrap();
        }
        assert_eq!(out.unwrap(), json);
        // And an empty one is one part.
        assert_eq!(split(b"", false).unwrap().len(), 1);
    }

    #[test]
    fn too_much_is_refused_both_ways() {
        assert!(split(&vec![b'x'; MAX_LSP + 1], true).is_none());
        let mut j = Joiner::default();
        let part = vec![b'x'; MAX_PART];
        for _ in 0..MAX_LSP / MAX_PART {
            assert_eq!(j.push(false, &part), Ok(None));
        }
        assert_eq!(j.push(false, &part), Err(TooBig));
        // An oversized part is refused on its own.
        let big = ToParent::Lsp { last: true, part: vec![0; MAX_PART + 1] }.encode();
        assert!(ToParent::decode(&big).is_none());
    }

    #[test]
    fn names_and_labels_are_checked() {
        let ok = ToParent::RegisterCommand { id: "todo-lens.list".into(), label: "TODOs: list".into() };
        assert_eq!(ToParent::decode(&ok.encode()), Some(ok));
        for (id, label) in [("Todo", "x"), ("", "x"), ("a/b", "x"), ("a", ""), ("a", "two\nlines"), ("a", "\u{202e}gnp.exe")] {
            assert!(ToParent::decode(&ToParent::RegisterCommand { id: id.into(), label: label.into() }.encode()).is_none(), "{id:?} {label:?}");
        }
        assert!(ToParent::decode(&ToParent::Status("x".repeat(MAX_STATUS + 1)).encode()).is_none());
        assert!(ToParent::decode(&ToParent::Panel(Panel::Open { title: " ".into() }).encode()).is_none());
        let init = ToChild::Init(Init { languages: vec!["rust".into(), "c".into()], workspace: "hello".into(), read: false });
        assert_eq!(ToChild::decode(&init.encode()), Some(init));
        let bad = ToChild::Init(Init { languages: vec!["Rust".into()], workspace: "hello".into(), read: false });
        assert!(ToChild::decode(&bad.encode()).is_none());
        let cmd = ToChild::Command { id: "a.b".into(), file: Some("file:///etc/passwd".into()), line: 0 };
        assert!(ToChild::decode(&cmd.encode()).is_none());
        assert!(ToChild::decode(b"\xff\xff").is_none());
    }
}
