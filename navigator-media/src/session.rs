//! ★ LIVE MEDIA EMBEDS (limen.md §2.1–2.2; fresco-video P3). A slot shows
//! its placeholder (or poster) until it is started; a started slot is a
//! Limen `media-player` embed for the UI's own window, bound above the
//! doc-viewer, fed `Load` with the video's URL once attached. Native controls
//! are the player's, so after that the Navigator relays nothing but what
//! its own UI asks (and, for a script's video, what [`crate::mse`] maps).
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

/// The UI's side of Limen, as the Navigator uses it: in Pergola,
/// `pergola::embed::Embeds::request("media-player", window_token, key)`,
/// `Embeds::send`, and dropping the slot node (or `Embeds::detach`).
pub trait MediaHost {
    fn request(&mut self, key: u64) -> Result<(), String>;
    fn send(&mut self, key: u64, msg: &ToChild) -> Result<(), String>;
    fn release(&mut self, key: u64);
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

struct Live { state: State, want_play: bool, looping: bool, attached: bool, load: ToChild, muted: bool }

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
            if self.live.remove(k).is_some() { host.release(*k) }
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
        let load = match &e.src {
            EmbedSource::Url { src, mime } => {
                let url = crate::url::resolve(&self.base_url, src).ok_or(NotStarted::BadUrl)?;
                let mime: String = mime.chars().take(limen::media_player::MIME_MAX).collect();
                ToChild::Load { source: Source::Url(url), mime }
            }
            EmbedSource::Script => return Err(NotStarted::NeedsEngine),
            EmbedSource::Unplayable => return Err(NotStarted::Unplayable),
        };
        if self.live.len() >= LIVE_MAX { return Err(NotStarted::TooMany) }
        // (The message is checked as the player will check it.)
        if ToChild::decode(&load.encode()).is_none() { return Err(NotStarted::BadUrl) }
        if host.request(key).is_err() { return Err(NotStarted::TooMany) }
        self.live.insert(key, Live { state: State::Requested, want_play: true, looping: e.flags & embed_flags::LOOP != 0,
                                     attached: false, load, muted: e.flags & embed_flags::MUTED != 0 });
        Ok(())
    }

    /// The embed for `key` is attached (Limen `Attached`): send `Load`, and
    /// silence it first if the element is `muted`.
    pub fn attached(&mut self, key: u64, host: &mut dyn MediaHost) {
        let Some(l) = self.live.get_mut(&key) else { return };
        if l.attached { return }
        l.attached = true;
        if l.muted { let _ = host.send(key, &ToChild::SetVolume { volume: 0.0 }); }
        if host.send(key, &l.load).is_ok() { l.state = State::Loaded } else { l.state = State::Failed }
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
            ToParent::Error { .. } => l.state = State::Failed,
            _ => {}
        }
    }

    /// The embed went away on its own (`ChildLost`): the slot shows its
    /// placeholder again, and a press may start it afresh.
    pub fn lost(&mut self, key: u64) { self.live.remove(&key); }
}
