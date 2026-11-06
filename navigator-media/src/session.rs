//! ★ LIVE MEDIA EMBEDS (limen.md §2.1–2.2; fresco-video P3). A slot shows
//! its placeholder (or poster) until it is started; a started slot is a
//! Limen `media-player` embed for the UI's own window, bound above the
//! doc-viewer. The player's jail has no network (8ba526f): the Navigator
//! fetches the video's file itself, through its fetcher (Nuntius, `Fd`
//! delivery), and sends `Load { Fd }` with the read-only descriptor. Native
//! controls are the player's, so after that the Navigator relays nothing
//! but what its own UI asks (and, for a script's video, what [`crate::mse`]
//! maps).
//!
//! `Load` goes as soon as the fetcher hands over the file's descriptor, with
//! `growing: true` while the fetch is still filling it; `LoadDone` follows
//! when the fetch is whole (limen.md §2.2, e3fa293). A file fetched whole
//! before the embed attached goes as an ordinary `Load`.
//!
//! The policy (limen.md §2.1): at most [`LIVE_MAX`] live media embeds per
//! page, and one starts only on the reader's press on its slot, or by the
//! autoplay policy — `autoplay` on a `muted` video, as browsers allow. A
//! slot that leaves (its band evicted) releases its embed.

use crate::slots::{Changes, Slots};
use limen::media_player::{Source, ToChild, ToParent};
use navigator_render::{embed_flags, EmbedSource};
use std::collections::BTreeMap;

/// Most live media embeds per page.
pub const LIVE_MAX: usize = 8;

/// The UI's side of Limen and of its fetcher, as the Navigator uses them:
/// in Pergola, `pergola::embed::Embeds::request("media-player",
/// window_token, key)`, `Embeds::send` / `send_fd`, and dropping the slot
/// node (or `Embeds::detach`); and a Nuntius `Open` with `Deliver::Fd`
/// whose `Fd` and `Done` come back as [`Sessions::fetched`].
pub trait MediaHost {
    fn request(&mut self, key: u64) -> Result<(), String>;
    fn send(&mut self, key: u64, msg: &ToChild) -> Result<(), String>;
    /// A message with one descriptor (limend's `SEND_FD`).
    fn send_fd(&mut self, key: u64, msg: &ToChild, fd: std::os::fd::OwnedFd) -> Result<(), String>;
    fn release(&mut self, key: u64);
    /// Fetch `url` whole into a descriptor (Nuntius `Fd`), for `key`.
    fn fetch(&mut self, key: u64, url: &str) -> Result<(), String>;
    fn cancel_fetch(&mut self, key: u64);
}

/// Why a slot was not started, for the UI (the lane indicator, a tooltip).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotStarted {
    /// Its source is a page script's (MSE or a script-set `src`): it plays
    /// only in an engine island (legacy-web §8.1).
    NeedsEngine,
    /// No source the player plays.
    Unplayable,
    /// The URL does not resolve to `https` or `http`.
    BadUrl,
    /// [`LIVE_MAX`] are live.
    TooMany,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State { Requested, Loaded, Playing, Paused, Ended, Failed }

struct Live { state: State, want_play: bool, looping: bool, attached: bool, load: ToChild, muted: bool,
              file: Option<std::os::fd::OwnedFd>, fetched: bool,
              /// `Load` went with `growing: true`, so `LoadDone` is owed when the file is whole.
              growing_sent: bool }

pub struct Sessions {
    base_url: String,
    live: BTreeMap<u64, Live>,
}

impl Sessions {
    /// `base_url`: the document's URL, which a video's `src` resolves against.
    pub fn new(base_url: &str) -> Self { Sessions { base_url: base_url.to_string(), live: BTreeMap::new() } }

    pub fn live(&self) -> usize { self.live.len() }
    pub fn is_live(&self, key: u64) -> bool { self.live.contains_key(&key) }

    /// Apply what the slots did: removed slots release their embeds; new
    /// `autoplay muted` ones start (within the cap). Returns the slots not
    /// started and why, for those that wanted to.
    pub fn sync(&mut self, slots: &Slots, changes: &Changes, host: &mut dyn MediaHost) -> Vec<(u64, NotStarted)> {
        for k in &changes.removed {
            if self.live.remove(k).is_some() { host.cancel_fetch(*k); host.release(*k) }
        }
        let mut out = vec![];
        for k in &changes.added {
            let Some(e) = slots.get(*k) else { continue };
            let auto = e.flags & embed_flags::AUTOPLAY != 0 && e.flags & embed_flags::MUTED != 0;
            if auto { if let Err(why) = self.start(slots, *k, host) { out.push((*k, why)) } }
        }
        out
    }

    /// ★ The reader pressed a slot's placeholder: start it, or say why not.
    pub fn press(&mut self, slots: &Slots, key: u64, host: &mut dyn MediaHost) -> Result<(), NotStarted> {
        if let Some(l) = self.live.get_mut(&key) {
            // Already live: the player's own controls have the press.
            l.want_play = true;
            return Ok(());
        }
        self.start(slots, key, host)
    }

    fn start(&mut self, slots: &Slots, key: u64, host: &mut dyn MediaHost) -> Result<(), NotStarted> {
        let e = slots.get(key).ok_or(NotStarted::Unplayable)?;
        let (load, url) = match &e.src {
            EmbedSource::Url { src, mime } => {
                let url = crate::url::resolve(&self.base_url, src).ok_or(NotStarted::BadUrl)?;
                let mime: String = mime.chars().take(limen::media_player::MIME_MAX).collect();
                (ToChild::Load { source: Source::Fd, mime, growing: false }, url)
            }
            EmbedSource::Script => return Err(NotStarted::NeedsEngine),
            EmbedSource::Unplayable => return Err(NotStarted::Unplayable),
        };
        if self.live.len() >= LIVE_MAX { return Err(NotStarted::TooMany) }
        // (The message is checked as the player will check it.)
        if ToChild::decode(&load.encode()).is_none() { return Err(NotStarted::BadUrl) }
        if host.request(key).is_err() { return Err(NotStarted::TooMany) }
        if host.fetch(key, &url).is_err() { host.release(key); return Err(NotStarted::BadUrl) }
        self.live.insert(key, Live { state: State::Requested, want_play: true, looping: e.flags & embed_flags::LOOP != 0,
                                     attached: false, load, muted: e.flags & embed_flags::MUTED != 0, file: None, fetched: false, growing_sent: false });
        Ok(())
    }

    /// The fetcher's answer for `key`: the file's descriptor (Nuntius `Fd`)
    /// and whether it is whole (`Done`), or `None` when the fetch failed
    /// (after the fetcher's retries) — the embed is released, and a press
    /// starts it afresh.
    pub fn fetched(&mut self, key: u64, fd: Option<std::os::fd::OwnedFd>, done: bool, host: &mut dyn MediaHost) {
        let Some(l) = self.live.get_mut(&key) else { return };
        match (fd, done) {
            (Some(f), _) => { l.file = Some(f); if done { l.fetched = true } }
            (None, true) if l.file.is_some() || l.growing_sent => l.fetched = true,
            _ => { self.live.remove(&key); host.release(key); return }
        }
        // A file already loaded while growing is now whole: tell the player.
        if l.fetched && l.growing_sent {
            l.growing_sent = false;
            if host.send(key, &ToChild::LoadDone).is_err() { l.state = State::Failed }
            return;
        }
        self.try_load(key, host);
    }

    fn try_load(&mut self, key: u64, host: &mut dyn MediaHost) {
        let Some(l) = self.live.get_mut(&key) else { return };
        // ★ The player takes a file that is still being written (limen.md
        // §2.2, `growing`): `Load` goes as soon as the descriptor exists, and
        // `LoadDone` follows when the fetch is whole (`fetched`).
        if !(l.attached && l.state == State::Requested) { return }
        let Some(fd) = l.file.take() else { return };
        let growing = !l.fetched;
        let load = match &l.load {
            ToChild::Load { source, mime, .. } => ToChild::Load { source: source.clone(), mime: mime.clone(), growing },
            other => other.clone(),
        };
        if host.send_fd(key, &load, fd).is_ok() { l.state = State::Loaded; l.growing_sent = growing } else { l.state = State::Failed }
    }

    /// The embed for `key` is attached (Limen `Attached`): silence it first
    /// if the element is `muted`; `Load` goes once the file is whole.
    pub fn attached(&mut self, key: u64, host: &mut dyn MediaHost) {
        let Some(l) = self.live.get_mut(&key) else { return };
        if l.attached { return }
        l.attached = true;
        if l.muted { let _ = host.send(key, &ToChild::SetVolume { volume: 0.0 }); }
        self.try_load(key, host);
    }

    /// A message from the player (already checked with `ToParent::decode`).
    pub fn from_player(&mut self, key: u64, m: &ToParent, host: &mut dyn MediaHost) {
        let Some(l) = self.live.get_mut(&key) else { return };
        match m {
            ToParent::Ready { .. } => { if l.want_play { let _ = host.send(key, &ToChild::Play); } }
            ToParent::Playing => l.state = State::Playing,
            ToParent::Paused => { l.state = State::Paused; l.want_play = false }
            // `loop` is the element's, not the role's: start again.
            ToParent::Ended => {
                l.state = State::Ended;
                if l.looping { let _ = host.send(key, &ToChild::Seek { secs: 0.0 }); let _ = host.send(key, &ToChild::Play); }
            }
            // A refusal for one track (`Some`) belongs to a segment feed, not
            // to this file session; only the player's own failure (`None`)
            // ends it.
            ToParent::Error { track: None, .. } => l.state = State::Failed,
            _ => {}
        }
    }

    /// The embed went away on its own (`ChildLost`): the slot shows its
    /// placeholder again, and a press may start it afresh.
    pub fn lost(&mut self, key: u64) { self.live.remove(&key); }
}
