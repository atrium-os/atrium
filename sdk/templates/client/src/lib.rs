//! {{title}}: what the core and its parse worker share (template: client).
//!
//! The core asks Nuntius for a feed (atrium.toml's `net-fetch`): the bytes
//! arrive as a descriptor, which goes to the `parse` worker unread. The
//! worker, in a jail with no capabilities, parses the JSON Feed
//! (jsonfeed.org) with serde_json and answers with a [`Feed`] whose bounds
//! were checked as it arrived.
//!
//! `cargo atrium fuzz worker:parse` fuzzes [`workers::parse`] with no
//! setup: each input is a response body.

use atrium_message::Message;

/// The largest response the worker reads.
pub const MAX_BODY: u64 = 8 << 20;

/// What the core asks for.
#[derive(Message, Debug, Clone, PartialEq)]
#[message(version = 1)]
pub struct Ask {
    /// At most this many items back.
    #[message(min = 1, max = 200)]
    pub max_items: u32,
}

/// What a fuzz run asks for (the input is the response).
impl Default for Ask {
    fn default() -> Self { Ask { max_items: 50 } }
}

/// One entry of the feed.
#[derive(Message, Debug, Clone, PartialEq)]
#[message(version = 1)]
pub struct Item {
    #[message(max_len = 512, no_control)]
    pub title: String,
    #[message(max_len = 2048, no_control)]
    pub url: String,
}

/// What the worker answers.
#[derive(Message, Debug, Clone, PartialEq)]
#[message(version = 1)]
pub struct Feed {
    #[message(max_len = 256, no_control)]
    pub title: String,
    #[message(max_len = 200)]
    pub items: Vec<Item>,
}

/// At most `max` bytes of `s`, cut at a character, with control
/// characters (new lines too) turned into spaces.
pub fn clean(s: &str, max: usize) -> String {
    let mut out = String::new();
    for c in s.chars().map(|c| if c.is_control() { ' ' } else { c }) {
        if out.len() + c.len_utf8() > max {
            break;
        }
        out.push(c);
    }
    out.trim().to_string()
}

/// The workers' handlers: `cargo atrium fuzz` finds `workers::<name>` for
/// each `[[workers]]` entry.
pub mod workers {
    use super::*;
    use std::fs::File;
    use std::io::Read;

    /// The parse worker: a response body in, a feed out.
    pub fn parse(ask: Ask, file: Option<File>) -> Result<Feed, String> {
        let file = file.ok_or("no response")?;
        let mut bytes = Vec::new();
        file.take(MAX_BODY + 1).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
        if bytes.len() as u64 > MAX_BODY {
            return Err("the response is larger than 8 MiB".into());
        }
        let v: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| format!("not JSON: {e}"))?;
        let text = |v: &serde_json::Value, k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or_default().to_string();
        let mut items = Vec::new();
        for it in v.get("items").and_then(|i| i.as_array()).into_iter().flatten() {
            if items.len() >= ask.max_items.clamp(1, 200) as usize {
                break;
            }
            let title = clean(&text(it, "title"), 512);
            let url = clean(&text(it, "url"), 2048);
            if !title.is_empty() || !url.is_empty() {
                items.push(Item { title, url });
            }
        }
        Ok(Feed { title: clean(&text(&v, "title"), 256), items })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_seed_parses() {
        let seed = std::fs::File::open(concat!(env!("CARGO_MANIFEST_DIR"), "/fuzz/corpus/worker-parse/seed.json")).unwrap();
        let f = workers::parse(Ask::default(), Some(seed)).unwrap();
        assert_eq!(f.title, "An example feed");
        assert_eq!(f.items.len(), 2);
        assert_eq!(clean("a\nb\u{7}c", 64), "a b c");
        assert_eq!(clean(&"\u{e9}".repeat(10), 5), "\u{e9}\u{e9}");
        // What it answers fits its own bounds.
        let long = serde_json::json!({ "title": "\u{e9}".repeat(400), "items": [{ "title": "x", "url": "y".repeat(5000) }] }).to_string();
        let mut t = tempfile();
        std::io::Write::write_all(&mut t, long.as_bytes()).unwrap();
        std::io::Seek::rewind(&mut t).unwrap();
        let f = workers::parse(Ask::default(), Some(t)).unwrap();
        assert!(Feed::from_frame(&f.to_frame(), 1 << 20).is_ok());
    }

    fn tempfile() -> std::fs::File {
        let p = std::env::temp_dir().join(format!("{{name}}-test-{}", std::process::id()));
        let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&p).unwrap();
        let _ = std::fs::remove_file(&p);
        f
    }
}
