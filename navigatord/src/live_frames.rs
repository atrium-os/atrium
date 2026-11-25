//! A live page's cross-origin frames (navigator-islands.md D11, I6.5).
//!
//! Each frame runs in its own engine, an island over the page's, with a
//! throwaway jar. The page's engine never loads it: navigatord's script in
//! the page reports where the frame is ([`FrameAsk`]), and the two engines
//! talk only through [`Mailbox`], which stamps each message with the
//! sender's real origin and delivers it only to the origin it names.

use std::collections::{HashMap, VecDeque};

/// The most frames a page runs at once; more are not started.
pub const FRAMES_MAX: usize = 4;
/// A frame smaller than this on either side (a tracking pixel) is not started.
pub const FRAME_MIN: u32 = 16;
/// The longest message, as JSON text.
pub const MESSAGE_MAX: usize = 64 * 1024;
/// The most messages waiting for one engine; more are dropped.
pub const QUEUE_MAX: usize = 32;
/// The most frame engines one run of a page starts, over its life: a page
/// can't churn engines by adding and removing frames.
pub const FRAME_STARTS_MAX: usize = 16;
/// The most frame reports waiting for the watcher.
pub const ASKS_MAX: usize = 64;

/// What the page's script said about one of its frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameAsk {
    /// Frame `id` shows `url` at `rect` (x, y, w, h in device pixels of
    /// the page's viewport).
    At { id: u32, url: String, rect: [i32; 4] },
    Gone { id: u32 },
}

/// Where a frame may run: a plain HTTPS address of another origin than the
/// page's (a same-origin frame loads in the page's engine). `None`: it is
/// not started.
pub fn frame_address(page: &str, url: &str) -> Option<String> {
    let to = crate::live_page::navigation(Some(page), url)?;
    (crate::origin(&to).is_some() && crate::origin(&to) != crate::origin(page)).then_some(to)
}

/// The frame's island in the window: `frame` (in the page's viewport)
/// moved by the page's place and cut to it. `None` when nothing of it shows.
pub fn frame_rect(page: [u32; 4], frame: [i32; 4]) -> Option<[u32; 4]> {
    let [px, py, pw, ph] = page.map(i64::from);
    let [fx, fy, fw, fh] = frame.map(i64::from);
    if fw <= 0 || fh <= 0 { return None; }
    let (left, top) = ((px + fx).max(px), (py + fy).max(py));
    let (right, bottom) = ((px + fx + fw).min(px + pw), (py + fy + fh).min(py + ph));
    if right <= left || bottom <= top { return None; }
    let r = [left, top, right - left, bottom - top].map(|v| u32::try_from(v).ok());
    Some([r[0]?, r[1]?, r[2]?, r[3]?])
}

/// Whether a message for `target` may go to an engine at `receiver`:
/// `*`, `/` (the sender's own origin) or the receiver's origin, as
/// `postMessage` reads its target.
fn target_ok(target: &str, sender: &str, receiver: &str) -> bool {
    match target {
        "*" => true,
        "/" => sender == receiver,
        t => crate::origin(t).is_some_and(|o| o == receiver),
    }
}

#[derive(Default)]
struct Frame {
    /// Its engine's origin, once it delivered its document.
    origin: Option<String>,
    /// (sender's origin, the origin it was for, data).
    inbox: VecDeque<(String, String, String)>,
}

/// JSON for the reporter. Strings and numbers always encode; a failure is
/// said, and nothing is delivered.
fn encoded(all: &[serde_json::Value]) -> Vec<u8> {
    serde_json::to_vec(all).unwrap_or_else(|e| {
        eprintln!("navigatord: a live page's messages did not encode ({e}); {} dropped", all.len());
        b"[]".to_vec()
    })
}

/// The messages between a page and its frames, for one run.
#[derive(Default)]
pub struct Mailbox {
    page: Option<String>,
    frames: HashMap<u32, Frame>,
    for_page: VecDeque<(u32, String, String)>,
}

/// A message is JSON text the sender's script made, at most [`MESSAGE_MAX`].
fn message_ok(data: &str) -> bool { !data.is_empty() && data.len() <= MESSAGE_MAX }

impl Mailbox {
    pub fn page_started(&mut self, origin: String) { self.page = Some(origin); }

    pub fn frame_added(&mut self, id: u32) { self.frames.entry(id).or_default(); }

    pub fn frame_started(&mut self, id: u32, origin: String) {
        if let Some(f) = self.frames.get_mut(&id) { f.origin = Some(origin); }
    }

    /// The frame went: its messages, both ways, go with it.
    pub fn frame_gone(&mut self, id: u32) {
        self.frames.remove(&id);
        self.for_page.retain(|(from, _, _)| *from != id);
    }

    /// The page posted `data` to its frame `to` for `target`.
    pub fn from_page(&mut self, to: u32, target: &str, data: String) -> Result<(), &'static str> {
        let page = self.page.clone().ok_or("before the page")?;
        let frame = self.frames.get_mut(&to).ok_or("no such frame")?;
        let receiver = frame.origin.as_deref().ok_or("the frame is not running yet")?;
        if !message_ok(&data) { return Err("a message out of bounds"); }
        if !target_ok(target, &page, receiver) { return Err("the frame is not at that origin"); }
        if frame.inbox.len() >= QUEUE_MAX { return Err("the frame's messages are full"); }
        frame.inbox.push_back((page, receiver.to_string(), data));
        Ok(())
    }

    /// Frame `id` posted `data` to its page for `target`.
    pub fn from_frame(&mut self, id: u32, target: &str, data: String) -> Result<(), &'static str> {
        let page = self.page.as_deref().ok_or("before the page")?;
        let sender = self.frames.get(&id).and_then(|f| f.origin.clone()).ok_or("no such frame")?;
        if !message_ok(&data) { return Err("a message out of bounds"); }
        if !target_ok(target, &sender, page) { return Err("the page is not at that origin"); }
        if self.for_page.len() >= QUEUE_MAX { return Err("the page's messages are full"); }
        self.for_page.push_back((id, sender, data));
        Ok(())
    }

    /// The page's waiting messages, as the JSON its script reads.
    pub fn take_for_page(&mut self) -> Vec<u8> {
        let all: Vec<serde_json::Value> = self.for_page.drain(..)
            .map(|(from, origin, data)| serde_json::json!({ "from": from, "origin": origin, "data": data })).collect();
        encoded(&all)
    }

    /// Frame `id`'s waiting messages. Each target is checked again here,
    /// as `postMessage` checks at delivery: a frame now at another origin
    /// than the one a message named doesn't get it.
    pub fn take_for_frame(&mut self, id: u32) -> Vec<u8> {
        let Some(f) = self.frames.get_mut(&id) else { return b"[]".to_vec() };
        let now = f.origin.clone();
        let all: Vec<serde_json::Value> = f.inbox.drain(..)
            .filter(|(_, target, _)| now.as_deref().is_some_and(|o| target == o))
            .map(|(origin, _, data)| serde_json::json!({ "from": null, "origin": origin, "data": data })).collect();
        encoded(&all)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_runs_only_for_another_origins_plain_https_address() {
        let page = "https://app.example/app.html";
        assert_eq!(frame_address(page, "https://pay.example/frame.html").as_deref(), Some("https://pay.example/frame.html"));
        assert_eq!(frame_address(page, "https://api.app.example/w.html").as_deref(), Some("https://api.app.example/w.html"), "a sibling host is another origin");
        assert_eq!(frame_address(page, "https://app.example/inner.html"), None, "the page's own origin loads in its engine");
        for bad in ["http://pay.example/", "https://u:p@pay.example/", "javascript:alert(1)", "https://PAY.example/", "data:text/html,x"] {
            assert_eq!(frame_address(page, bad), None, "{bad}");
        }
    }

    #[test]
    fn a_frame_is_placed_in_the_page_and_cut_to_it() {
        let page = [100, 50, 800, 600];
        assert_eq!(frame_rect(page, [10, 20, 320, 80]), Some([110, 70, 320, 80]));
        assert_eq!(frame_rect(page, [-30, -10, 320, 80]), Some([100, 50, 290, 70]), "scrolled partly out");
        assert_eq!(frame_rect(page, [700, 550, 320, 80]), Some([800, 600, 100, 50]));
        assert_eq!(frame_rect(page, [0, 700, 320, 80]), None, "below the page");
        assert_eq!(frame_rect(page, [0, 0, 0, 80]), None);
        assert_eq!(frame_rect(page, [i32::MIN, i32::MIN, i32::MAX, i32::MAX]), None);
        assert_eq!(frame_rect(page, [i32::MAX, i32::MAX, i32::MAX, i32::MAX]), None);
    }

    #[test]
    fn messages_carry_the_senders_real_origin_to_the_origin_named() {
        let mut m = Mailbox::default();
        m.frame_added(1);
        assert_eq!(m.from_frame(1, "*", "\"x\"".into()), Err("before the page"));
        m.page_started("https://app.example".into());
        assert_eq!(m.from_page(1, "*", "\"x\"".into()), Err("the frame is not running yet"));
        m.frame_started(1, "https://pay.example".into());
        // The frame to its page: only to the page's origin, or any.
        assert_eq!(m.from_frame(1, "https://evil.example", "\"a\"".into()), Err("the page is not at that origin"));
        assert_eq!(m.from_frame(1, "https://app.example/any/path", "\"b\"".into()), Ok(()));
        assert_eq!(m.from_frame(1, "/", "\"c\"".into()), Err("the page is not at that origin"), "'/' is the sender's own");
        assert_eq!(m.from_frame(2, "*", "\"d\"".into()), Err("no such frame"));
        let got: serde_json::Value = serde_json::from_slice(&m.take_for_page()).unwrap();
        assert_eq!(got, serde_json::json!([{ "from": 1, "origin": "https://pay.example", "data": "\"b\"" }]));
        assert_eq!(m.take_for_page(), b"[]", "taken once");
        // The page to its frame.
        assert_eq!(m.from_page(1, "https://app.example", "1".into()), Err("the frame is not at that origin"));
        assert_eq!(m.from_page(1, "https://pay.example", "1".into()), Ok(()));
        assert_eq!(m.from_page(1, "*", "x".repeat(MESSAGE_MAX + 1)), Err("a message out of bounds"));
        let got: serde_json::Value = serde_json::from_slice(&m.take_for_frame(1)).unwrap();
        assert_eq!(got, serde_json::json!([{ "from": null, "origin": "https://app.example", "data": "1" }]));
        // A frame that moved to another origin since doesn't get what was
        // for the old one.
        assert_eq!(m.from_page(1, "*", "2".into()), Ok(()));
        m.frame_started(1, "https://other.pay.example".into());
        assert_eq!(m.take_for_frame(1), b"[]");
        // Bounded, and gone with the frame.
        for _ in 0..QUEUE_MAX { m.from_frame(1, "*", "0".into()).unwrap(); }
        assert_eq!(m.from_frame(1, "*", "0".into()), Err("the page's messages are full"));
        m.frame_gone(1);
        assert_eq!(m.take_for_page(), b"[]");
        assert_eq!(m.take_for_frame(1), b"[]");
    }
}
