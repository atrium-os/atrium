//! ★ `HTMLMediaElement` AND MEDIA SOURCE EXTENSIONS AS A FRONT END TO THE
//! `media-player` ROLE (limen.md §2.2; legacy-web §8.1). An engine island's
//! `<video>` and `MediaSource` never decode: they map the page's calls onto
//! the role's messages, and the player's answers onto the attributes and
//! events a page reads. Engine-independent: an engine binds its JS objects
//! to [`Media`], sends what [`Media::take_out`] gives to the embed, feeds the
//! embed's messages to [`Media::from_player`], and dispatches
//! [`Media::take_events`] in order. No engine in the tree runs at view time
//! yet (the converter's boa runs offline), so this is built and tested
//! against [`crate::fake`] (legacy-web §8.1 has the mapping).
//!
//! The mapping, call by call:
//!
//! | page | message | answered by |
//! |---|---|---|
//! | `video.src = url` (a file) | the engine fetches it (Nuntius `Fd`) and sends `Load { Fd }` with the descriptor | `Ready` → `loadedmetadata`, `loadeddata`, `canplay`, `canplaythrough` |
//! | `video.src = URL.createObjectURL(ms)` | `OpenSource` | `sourceopen` at once |
//! | `MediaSource.isTypeSupported(t)` | (sync) from answers to `CanPlay`; an unknown type answers `false` and is asked | `CanPlay` fills the cache |
//! | `ms.addSourceBuffer(t)` | `AddTrack` (track = its index, at most 8) | |
//! | `sb.appendBuffer(b)` | `Append`s of at most 60 KiB, ONE in flight per track; `timestampOffset` on the first | each `Appended` releases the next; the last ends the call: `update`, `updateend` |
//! | `sb.remove(a, b)` | `Remove` | the next `Buffered` for the track: `update`, `updateend` |
//! | `sb.abort()` | `Abort` | `abort`, `updateend` at once; an `Append` still in flight holds the next |
//! | `ms.endOfStream(e)` | `EndOfStream` | `sourceended` |
//! | `ms.duration = d` | `SetDuration` | `durationchange` |
//! | `sb.buffered`, `video.buffered` | | `Buffered` (the element's: every track's intersected) |
//! | `play()`, `pause()` | `Play`, `Pause` | `Playing` → `playing`; `Paused` → `pause` |
//! | `currentTime = t` | `Seek` | `seeking` at once; the next `Time` → `seeked`, `timeupdate` |
//! | `volume`, `muted` | `SetVolume` (0 while muted) | `volumechange` at once |
//! | `playbackRate` | `SetRate` (0.25–4; else `NotSupportedError`) | `ratechange` at once |
//! | | | `Time` → `timeupdate`; `Waiting` → `waiting`; `Seeking` (native controls) → `seeking`; `Ended` → `pause`, `ended` |
//! | | `Error { TOO_BIG }` | the append in flight ends with `error`, `updateend`; the NEXT `appendBuffer` throws `QuotaExceededError` until a `remove` completes |
//! | | `Error { DECODE \| UNSUPPORTED \| NETWORK }` | `video.error` (`MEDIA_ERR_DECODE` 3, `SRC_NOT_SUPPORTED` 4, `NETWORK` 2), `error` |
//!
//! Two assumptions about the player, to be confirmed by P1: it answers in
//! the order it is sent, and an `Append`'s `Buffered` comes before its
//! `Appended`. And one gap: `Error` names no track, so `TOO_BIG` ends every
//! append in flight.

use limen::media_player::{code, ToChild, ToParent, APPEND_MAX, TRACKS_MAX};
use std::collections::{HashMap, VecDeque};

/// What a call throws, as the DOM names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DomError { InvalidState, NotSupported, QuotaExceeded, Type, IndexSize }

/// A DOM event to dispatch: on the element, the `MediaSource`, or a
/// `SourceBuffer` (by track).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fire { Element(&'static str), Source(&'static str), Buffer(u8, &'static str) }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadyState { Closed, Open, Ended }

#[derive(Debug, Default)]
struct Sb {
    mime: String,
    updating: bool,
    removing: bool,
    chunks: VecDeque<Vec<u8>>,
    offset_pending: Option<f64>,
    in_flight: bool,
    buffered: Vec<(f64, f64)>,
    timestamp_offset: f64,
    quota: bool,
}

struct Ms { ready: ReadyState, duration: f64, bufs: Vec<Sb> }

/// One media element, and its `MediaSource` when it has one.
pub struct Media {
    out: Vec<ToChild>,
    events: Vec<Fire>,
    ms: Option<Ms>,
    can_play: HashMap<String, bool>,
    asked: std::collections::HashSet<String>,
    loaded: bool,
    pub paused: bool,
    pub current_time: f64,
    pub duration: f64,
    pub volume: f32,
    pub muted: bool,
    pub rate: f32,
    pub seeking: bool,
    pub ended: bool,
    /// HAVE_NOTHING 0 … HAVE_ENOUGH_DATA 4.
    pub ready_state: u8,
    /// `MediaError.code`.
    pub error: Option<u16>,
    pub size: (u32, u32),
    file_buffered: Vec<(f64, f64)>,
}

impl Default for Media { fn default() -> Self { Self::new() } }

impl Media {
    pub fn new() -> Self {
        Media { out: vec![], events: vec![], ms: None, can_play: HashMap::new(), asked: Default::default(), loaded: false,
                paused: true, current_time: 0.0, duration: f64::NAN, volume: 1.0, muted: false, rate: 1.0, seeking: false,
                ended: false, ready_state: 0, error: None, size: (0, 0), file_buffered: vec![] }
    }

    /// The messages to send to the embed, in order.
    pub fn take_out(&mut self) -> Vec<ToChild> { std::mem::take(&mut self.out) }
    /// The events to dispatch, in order.
    pub fn take_events(&mut self) -> Vec<Fire> { std::mem::take(&mut self.events) }

    /// Ask the player about types the page is likely to test, before it
    /// does (`isTypeSupported` is synchronous).
    pub fn prime(&mut self, types: &[&str]) { for t in types { self.ask(t) } }

    fn ask(&mut self, t: &str) {
        let t = t.trim();
        if t.is_empty() || t.len() > limen::media_player::MIME_MAX || self.can_play.contains_key(t) || !self.asked.insert(t.to_string()) { return }
        self.out.push(ToChild::CanPlay { mime: t.to_string() });
    }

    // ---- the element ----

    /// `video.src = url` for a whole resource, once the engine has fetched
    /// it into a descriptor (the player has no network): the `Load` this
    /// queues goes with that descriptor (`Parent::send_fd`).
    pub fn load_fd(&mut self, mime: &str) -> Result<(), DomError> {
        if self.loaded { return Err(DomError::InvalidState) } // one source per embed (atrium-player M5)
        self.loaded = true;
        self.out.push(ToChild::Load { source: limen::media_player::Source::Fd, mime: mime.to_string() });
        Ok(())
    }

    /// `video.src = URL.createObjectURL(mediaSource)`.
    pub fn attach_source(&mut self) -> Result<(), DomError> {
        if self.loaded { return Err(DomError::InvalidState) }
        self.loaded = true;
        self.ms = Some(Ms { ready: ReadyState::Open, duration: f64::NAN, bufs: vec![] });
        self.out.push(ToChild::OpenSource);
        self.events.push(Fire::Source("sourceopen"));
        Ok(())
    }

    pub fn play(&mut self) -> Result<(), DomError> {
        if !self.loaded { return Err(DomError::NotSupported) }
        self.error = None;
        if self.ended { self.ended = false; self.out.push(ToChild::Seek { secs: 0.0 }); self.current_time = 0.0 }
        if self.paused { self.paused = false; self.events.push(Fire::Element("play")) }
        self.out.push(ToChild::Play);
        Ok(())
    }

    pub fn pause(&mut self) {
        if !self.loaded { return }
        if !self.paused { self.paused = true; self.events.push(Fire::Element("pause")) }
        self.out.push(ToChild::Pause);
    }

    /// `currentTime = t`.
    pub fn seek(&mut self, t: f64) -> Result<(), DomError> {
        if !t.is_finite() { return Err(DomError::Type) }
        let t = t.max(0.0);
        let t = if self.duration.is_finite() { t.min(self.duration) } else { t };
        self.current_time = t;
        self.error = None;
        self.ended = false;
        self.seeking = true;
        self.events.push(Fire::Element("seeking"));
        if self.loaded { self.out.push(ToChild::Seek { secs: t }) }
        Ok(())
    }

    pub fn set_volume(&mut self, v: f64) -> Result<(), DomError> {
        if !(0.0..=1.0).contains(&v) { return Err(DomError::IndexSize) }
        self.volume = v as f32;
        if !self.muted && self.loaded { self.out.push(ToChild::SetVolume { volume: self.volume }) }
        self.events.push(Fire::Element("volumechange"));
        Ok(())
    }

    pub fn set_muted(&mut self, m: bool) {
        if self.muted == m { return }
        self.muted = m;
        if self.loaded { self.out.push(ToChild::SetVolume { volume: if m { 0.0 } else { self.volume } }) }
        self.events.push(Fire::Element("volumechange"));
    }

    /// `playbackRate = r`: the player plays 0.25 to 4 times (pitch kept).
    pub fn set_rate(&mut self, r: f64) -> Result<(), DomError> {
        if !r.is_finite() { return Err(DomError::Type) }
        if !(0.25..=4.0).contains(&r) { return Err(DomError::NotSupported) }
        self.rate = r as f32;
        if self.loaded { self.out.push(ToChild::SetRate { rate: self.rate }) }
        self.events.push(Fire::Element("ratechange"));
        Ok(())
    }

    /// `video.buffered`: a file's, or every source buffer's intersected.
    pub fn buffered(&self) -> Vec<(f64, f64)> {
        match &self.ms {
            None => self.file_buffered.clone(),
            Some(ms) if ms.bufs.is_empty() => vec![],
            Some(ms) => ms.bufs.iter().skip(1).fold(ms.bufs[0].buffered.clone(), |acc, b| intersect(&acc, &b.buffered)),
        }
    }

    // ---- MediaSource ----

    /// `MediaSource.isTypeSupported(t)`: from the player's answers; a type
    /// it has not answered is asked now and answers `false` this time.
    pub fn is_type_supported(&mut self, t: &str) -> bool {
        match self.can_play.get(t.trim()) { Some(v) => *v, None => { self.ask(t); false } }
    }

    pub fn ready_state(&self) -> ReadyState { self.ms.as_ref().map(|m| m.ready).unwrap_or(ReadyState::Closed) }

    fn open_ms(&mut self) -> Result<&mut Ms, DomError> {
        let ms = self.ms.as_mut().ok_or(DomError::InvalidState)?;
        if ms.ready == ReadyState::Closed { return Err(DomError::InvalidState) }
        Ok(ms)
    }

    /// `ms.addSourceBuffer(t)`: a track.
    pub fn add_source_buffer(&mut self, t: &str) -> Result<u8, DomError> {
        if t.trim().is_empty() { return Err(DomError::Type) }
        let supported = self.can_play.get(t.trim()).copied();
        let ms = self.ms.as_mut().ok_or(DomError::InvalidState)?;
        if ms.ready != ReadyState::Open { return Err(DomError::InvalidState) }
        if supported != Some(true) { self.ask(t); return Err(DomError::NotSupported) }
        let ms = self.ms.as_mut().unwrap();
        if ms.bufs.len() >= TRACKS_MAX as usize { return Err(DomError::QuotaExceeded) }
        let track = ms.bufs.len() as u8;
        ms.bufs.push(Sb { mime: t.trim().to_string(), ..Default::default() });
        self.out.push(ToChild::AddTrack { track, mime: t.trim().to_string() });
        Ok(track)
    }

    fn sb(&mut self, track: u8) -> Result<&mut Sb, DomError> {
        self.ms.as_mut().and_then(|m| m.bufs.get_mut(track as usize)).ok_or(DomError::InvalidState)
    }

    fn reopen_if_ended(&mut self) {
        if let Some(ms) = self.ms.as_mut() {
            if ms.ready == ReadyState::Ended { ms.ready = ReadyState::Open; self.events.push(Fire::Source("sourceopen")) }
        }
    }

    /// `sb.timestampOffset = o`.
    pub fn set_timestamp_offset(&mut self, track: u8, o: f64) -> Result<(), DomError> {
        if !o.is_finite() || o.abs() > 1.0e9 { return Err(DomError::Type) }
        let sb = self.sb(track)?;
        if sb.updating { return Err(DomError::InvalidState) }
        sb.timestamp_offset = o;
        sb.offset_pending = Some(o);
        Ok(())
    }

    /// `sb.appendBuffer(bytes)`.
    pub fn append_buffer(&mut self, track: u8, bytes: &[u8]) -> Result<(), DomError> {
        let ms_open = self.open_ms()?.ready;
        let _ = ms_open;
        let sb = self.sb(track)?;
        if sb.updating { return Err(DomError::InvalidState) }
        if sb.quota { return Err(DomError::QuotaExceeded) }
        self.reopen_if_ended();
        let sb = self.sb(track)?;
        sb.updating = true;
        if bytes.is_empty() { sb.chunks.push_back(vec![]) }
        for c in bytes.chunks(APPEND_MAX) { sb.chunks.push_back(c.to_vec()) }
        self.events.push(Fire::Buffer(track, "updatestart"));
        self.pump(track);
        Ok(())
    }

    /// Send the next chunk if none is in flight on the track.
    fn pump(&mut self, track: u8) {
        let Ok(sb) = self.sb(track) else { return };
        if sb.in_flight { return }
        let Some(bytes) = sb.chunks.pop_front() else { return };
        sb.in_flight = true;
        let timestamp_offset = sb.offset_pending.take();
        self.out.push(ToChild::Append { track, bytes, timestamp_offset });
    }

    /// `sb.remove(start, end)`.
    pub fn remove(&mut self, track: u8, start: f64, end: f64) -> Result<(), DomError> {
        self.open_ms()?;
        let duration = self.ms.as_ref().map(|m| m.duration).unwrap_or(f64::NAN);
        if !start.is_finite() || start < 0.0 || (duration.is_finite() && start > duration) || end.is_nan() || end <= start { return Err(DomError::Type) }
        let sb = self.sb(track)?;
        if sb.updating { return Err(DomError::InvalidState) }
        self.reopen_if_ended();
        let sb = self.sb(track)?;
        sb.updating = true;
        sb.removing = true;
        self.events.push(Fire::Buffer(track, "updatestart"));
        self.out.push(ToChild::Remove { track, start, end });
        Ok(())
    }

    /// `sb.abort()`.
    pub fn abort(&mut self, track: u8) -> Result<(), DomError> {
        let ms = self.ms.as_ref().ok_or(DomError::InvalidState)?;
        if ms.ready != ReadyState::Open { return Err(DomError::InvalidState) }
        let sb = self.sb(track)?;
        if sb.removing { return Err(DomError::InvalidState) }
        let was = sb.updating;
        sb.chunks.clear();
        sb.updating = false;
        self.out.push(ToChild::Abort { track });
        if was { self.events.push(Fire::Buffer(track, "abort")); self.events.push(Fire::Buffer(track, "updateend")) }
        Ok(())
    }

    /// `ms.endOfStream(error)`: `None`, `"network"` or `"decode"`.
    pub fn end_of_stream(&mut self, error: Option<&str>) -> Result<(), DomError> {
        let e = match error { None => None, Some("network") => Some(1u8), Some("decode") => Some(2u8), Some(_) => return Err(DomError::Type) };
        let ms = self.ms.as_mut().ok_or(DomError::InvalidState)?;
        if ms.ready != ReadyState::Open || ms.bufs.iter().any(|b| b.updating) { return Err(DomError::InvalidState) }
        ms.ready = ReadyState::Ended;
        self.out.push(ToChild::EndOfStream { error: e });
        self.events.push(Fire::Source("sourceended"));
        if let Some(e) = e { self.fail(if e == 1 { 2 } else { 3 }) }
        Ok(())
    }

    /// `ms.duration = d`.
    pub fn set_duration(&mut self, d: f64) -> Result<(), DomError> {
        if d.is_nan() || d < 0.0 { return Err(DomError::Type) }
        let ms = self.ms.as_mut().ok_or(DomError::InvalidState)?;
        if ms.ready != ReadyState::Open || ms.bufs.iter().any(|b| b.updating) { return Err(DomError::InvalidState) }
        ms.duration = d;
        self.duration = d;
        self.out.push(ToChild::SetDuration { secs: d });
        self.events.push(Fire::Element("durationchange"));
        Ok(())
    }

    pub fn source_buffered(&self, track: u8) -> Vec<(f64, f64)> {
        self.ms.as_ref().and_then(|m| m.bufs.get(track as usize)).map(|b| b.buffered.clone()).unwrap_or_default()
    }
    pub fn updating(&self, track: u8) -> bool { self.ms.as_ref().and_then(|m| m.bufs.get(track as usize)).is_some_and(|b| b.updating) }
    /// `sb`'s type, as added.
    pub fn source_type(&self, track: u8) -> Option<&str> { self.ms.as_ref().and_then(|m| m.bufs.get(track as usize)).map(|b| b.mime.as_str()) }
    pub fn source_duration(&self) -> f64 { self.ms.as_ref().map(|m| m.duration).unwrap_or(f64::NAN) }

    /// The Navigator's own fetch for this element failed after its retries
    /// and alternates (`MEDIA_ERR_NETWORK`). A later play or seek starts
    /// fresh, so the element is never wedged.
    pub fn network_error(&mut self) { self.fail(2) }

    fn fail(&mut self, media_err: u16) {
        self.error = Some(media_err);
        self.events.push(Fire::Element("error"));
    }

    // ---- the player's answers ----

    /// A message from the embed, already checked with `ToParent::decode`.
    pub fn from_player(&mut self, m: ToParent) {
        match m {
            ToParent::CanPlay { mime, supported } => { self.asked.remove(&mime); self.can_play.insert(mime, supported); }
            ToParent::Ready { duration_secs, width, height } => {
                self.size = (width, height);
                let ms_dur = self.ms.as_ref().map(|m| m.duration);
                match ms_dur {
                    // A feed's duration is the page's (`ms.duration`).
                    Some(d) => { if !d.is_finite() && !self.duration.is_finite() { self.duration = duration_secs; self.events.push(Fire::Element("durationchange")) } }
                    None => { self.duration = duration_secs; self.events.push(Fire::Element("durationchange")) }
                }
                if self.ready_state < 1 { self.events.push(Fire::Element("loadedmetadata")) }
                if self.ready_state < 2 { self.events.push(Fire::Element("loadeddata")) }
                if self.ready_state < 3 { self.events.push(Fire::Element("canplay")) }
                if self.ready_state < 4 { self.events.push(Fire::Element("canplaythrough")) }
                self.ready_state = 4;
            }
            ToParent::Appended { track } => {
                let Ok(sb) = self.sb(track) else { return };
                sb.in_flight = false;
                if !sb.chunks.is_empty() { self.pump(track); return }
                if sb.updating && !sb.removing {
                    sb.updating = false;
                    self.events.push(Fire::Buffer(track, "update"));
                    self.events.push(Fire::Buffer(track, "updateend"));
                }
            }
            ToParent::Buffered { track, ranges } => {
                match self.sb(track) {
                    Ok(sb) => {
                        sb.buffered = ranges;
                        // A remove ends with the Buffered after it (appends
                        // before it have all been answered: it waits on them).
                        if sb.removing && !sb.in_flight {
                            sb.removing = false;
                            sb.updating = false;
                            sb.quota = false;
                            self.events.push(Fire::Buffer(track, "update"));
                            self.events.push(Fire::Buffer(track, "updateend"));
                        }
                    }
                    Err(_) => if self.ms.is_none() { self.file_buffered = ranges; self.events.push(Fire::Element("progress")) },
                }
            }
            ToParent::Time { secs } => {
                self.current_time = secs;
                if self.seeking { self.seeking = false; self.events.push(Fire::Element("seeked")) }
                self.events.push(Fire::Element("timeupdate"));
            }
            ToParent::Waiting { secs } => { self.current_time = secs; self.ready_state = self.ready_state.min(2); self.events.push(Fire::Element("waiting")) }
            ToParent::Seeking { secs } => { self.current_time = secs; self.seeking = true; self.events.push(Fire::Element("seeking")) }
            ToParent::Playing => { self.paused = false; if self.ready_state < 3 { self.ready_state = 3 } self.events.push(Fire::Element("playing")) }
            ToParent::Paused => { if !self.paused { self.paused = true; self.events.push(Fire::Element("pause")) } }
            ToParent::Ended => {
                self.ended = true;
                if !self.paused { self.paused = true; self.events.push(Fire::Element("pause")) }
                self.events.push(Fire::Element("ended"));
            }
            ToParent::Error { code: c, track } if c == code::TOO_BIG => {
                // The player is full: the named track's append ends (every
                // append in flight if none is named), and the next
                // appendBuffer throws QuotaExceededError until a remove.
                let tracks: Vec<u8> = self.ms.as_ref().map(|m| m.bufs.iter().enumerate().filter(|(i, b)| b.updating && !b.removing && track.is_none_or(|t| t as usize == *i)).map(|(i, _)| i as u8).collect()).unwrap_or_default();
                for t in tracks {
                    let sb = self.sb(t).unwrap();
                    sb.chunks.clear();
                    sb.in_flight = false;
                    sb.updating = false;
                    sb.quota = true;
                    self.events.push(Fire::Buffer(t, "error"));
                    self.events.push(Fire::Buffer(t, "updateend"));
                }
                if let Some(ms) = self.ms.as_mut() { for b in &mut ms.bufs { b.quota = true } }
            }
            ToParent::Error { code: c, .. } => {
                self.fail(match c { code::DECODE => 3, code::UNSUPPORTED => 4, code::NETWORK => 2, _ => 3 });
            }
        }
    }
}

/// Two sorted range lists' intersection.
fn intersect(a: &[(f64, f64)], b: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let mut out = vec![];
    for &(a0, a1) in a {
        for &(b0, b1) in b {
            let (s, e) = (a0.max(b0), a1.min(b1));
            if e > s { out.push((s, e)) }
        }
    }
    out
}
