//! The `media-player` role (limen.md §2.2, atrium-player.md): a jailed
//! native player that demuxes, decodes and presents in its own window. The
//! parent feeds it a whole resource (`Load`) or MSE-style segments, and hears
//! back only what MSE and the video element expose to a page anyway.
//!
//! Both sides decode with [`ToChild::decode`] / [`ToParent::decode`], which
//! refuse anything out of shape: the player never trusts the parent's bytes,
//! and the parent never trusts the player's.
use serde::{Deserialize, Serialize};

/// Most tracks in one segment feed.
pub const TRACKS_MAX: u8 = 8;
/// Most bytes in one `Append`: limend carries at most
/// [`crate::catalogue::MAX_MESSAGE`] per message, so a parent splits a
/// segment into appends of this size.
pub const APPEND_MAX: usize = 60 * 1024;
/// Most buffered ranges in one `Buffered`.
pub const RANGES_MAX: usize = 64;
/// Longest MIME type and `CanPlay` query.
pub const MIME_MAX: usize = 256;
/// The player's buffer cap per feed (bytes), unless its jail allows less.
pub const FEED_BYTES_MAX: u64 = 150_000_000;
/// Most `Time` events a second.
pub const TIME_HZ_MAX: u32 = 4;
/// Most frame width and height the player accepts (fresco-video §3).
pub const WIDTH_MAX: u32 = 7680;
pub const HEIGHT_MAX: u32 = 4320;

/// What `Load` plays.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Source {
    /// A file descriptor that limend passes with this message (a file, or a
    /// Tabula-style blob the parent uploaded). There is no URL source: the
    /// player's jail has no network, and the parent fetches (Shankar,
    /// 2026-11-06), passing a descriptor or feeding segments.
    Fd,
}

/// Parent to child.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ToChild {
    /// `growing`: the file is still being written (a progressive
    /// download). The player waits at its end for more instead of ending,
    /// until `LoadDone`.
    Load { source: Source, mime: String, growing: bool },
    /// A growing `Load`'s file is complete.
    LoadDone,
    /// Begins a segment feed.
    OpenSource,
    AddTrack { track: u8, mime: String },
    Append { track: u8, bytes: Vec<u8>, timestamp_offset: Option<f64> },
    Remove { track: u8, start: f64, end: f64 },
    /// Drop a half-parsed segment.
    Abort { track: u8 },
    EndOfStream { error: Option<u8> },
    SetDuration { secs: f64 },
    /// `MediaSource.isTypeSupported`: answered with `CanPlay`.
    CanPlay { mime: String },
    Play,
    Pause,
    Stop,
    Seek { secs: f64 },
    /// 0 to 1.
    SetVolume { volume: f32 },
    /// 0.25 to 4.
    SetRate { rate: f32 },
    /// A subtitle file the page supplies (`<track>`): WebVTT, as a regular
    /// file passed with this message. A second one with the same `id`
    /// replaces the first. `lang` (BCP 47) and `label` are for the
    /// player's own subtitle menu.
    TextTrack { id: u8, lang: String, label: String },
    /// Which subtitles show (`TextTrack.mode = "showing"`).
    ShowText { text: TextChoice },
}

/// What [`ToChild::ShowText`] shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TextChoice {
    None,
    /// The media's own text track, if it has one. The default.
    InBand,
    /// The page's `TextTrack` with this `id`.
    Page(u8),
}

/// Most page text tracks per embed, and the length of their names.
pub const TEXT_TRACKS_MAX: u8 = 16;
pub const TEXT_NAME_MAX: usize = 64;

/// Error codes in `ToParent::Error`.
pub mod code {
    pub const DECODE: u16 = 1;
    pub const UNSUPPORTED: u16 = 2;
    pub const TOO_BIG: u16 = 3;
    /// A message the player can't act on now (an `Append` before
    /// `AddTrack`, a second in-flight `Append`).
    pub const STATE: u16 = 4;
    pub const NETWORK: u16 = 5;
}

/// Child to parent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ToParent {
    Ready { duration_secs: f64, width: u32, height: u32 },
    CanPlay { mime: String, supported: bool },
    Buffered { track: u8, ranges: Vec<(f64, f64)> },
    /// The append is parsed, so the next may go (`updateend`).
    Appended { track: u8 },
    Time { secs: f64 },
    /// Playback reached the end of what is buffered.
    Waiting { secs: f64 },
    /// The user seeked in the native controls; the parent fetches from there.
    Seeking { secs: f64 },
    Playing,
    Paused,
    Ended,
    /// `track` names the feed track whose `Append` this ends (a `TOO_BIG`
    /// append over the buffer cap: that append is dropped, no `Appended`
    /// follows, and the feed goes on). `None`: playback has failed.
    Error { code: u16, track: Option<u8> },
}

fn finite_secs(s: f64) -> bool {
    s.is_finite() && (0.0..=1.0e9).contains(&s)
}

fn clean_text(s: &str, max: usize) -> bool {
    s.len() <= max && !s.chars().any(char::is_control)
}

impl ToChild {
    pub fn decode(b: &[u8]) -> Option<Self> {
        let m: Self = postcard::from_bytes(b).ok()?;
        let ok = match &m {
            ToChild::Load { source, mime, .. } => {
                clean_text(mime, MIME_MAX) && matches!(source, Source::Fd)
            }
            ToChild::OpenSource | ToChild::LoadDone | ToChild::Play | ToChild::Pause | ToChild::Stop => true,
            ToChild::AddTrack { track, mime } => *track < TRACKS_MAX && clean_text(mime, MIME_MAX),
            ToChild::Append { track, bytes, timestamp_offset } => {
                *track < TRACKS_MAX
                    && bytes.len() <= APPEND_MAX
                    && timestamp_offset.map_or(true, |t| t.is_finite() && t.abs() <= 1.0e9)
            }
            ToChild::Remove { track, start, end } => {
                *track < TRACKS_MAX && finite_secs(*start) && (end.is_infinite() && *end > 0.0 || finite_secs(*end)) && start <= end
            }
            ToChild::Abort { track } => *track < TRACKS_MAX,
            ToChild::EndOfStream { .. } => true,
            ToChild::SetDuration { secs } => finite_secs(*secs) || (secs.is_infinite() && *secs > 0.0),
            ToChild::CanPlay { mime } => clean_text(mime, MIME_MAX),
            ToChild::Seek { secs } => finite_secs(*secs),
            ToChild::SetVolume { volume } => (0.0..=1.0).contains(volume),
            ToChild::SetRate { rate } => (0.25..=4.0).contains(rate),
            ToChild::TextTrack { id, lang, label } => *id < TEXT_TRACKS_MAX && clean_text(lang, TEXT_NAME_MAX) && clean_text(label, TEXT_NAME_MAX),
            ToChild::ShowText { text } => !matches!(text, TextChoice::Page(id) if *id >= TEXT_TRACKS_MAX),
        };
        ok.then_some(m)
    }

    pub fn encode(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("encodes")
    }
}

impl ToParent {
    pub fn decode(b: &[u8]) -> Option<Self> {
        let m: Self = postcard::from_bytes(b).ok()?;
        let ok = match &m {
            ToParent::Ready { duration_secs, width, height } => {
                (finite_secs(*duration_secs) || duration_secs.is_infinite() && *duration_secs > 0.0)
                    && *width <= WIDTH_MAX
                    && *height <= HEIGHT_MAX
            }
            ToParent::CanPlay { mime, .. } => clean_text(mime, MIME_MAX),
            ToParent::Buffered { track, ranges } => {
                *track < TRACKS_MAX
                    && ranges.len() <= RANGES_MAX
                    && ranges.iter().all(|(a, b)| finite_secs(*a) && finite_secs(*b) && a <= b)
            }
            ToParent::Appended { track } => *track < TRACKS_MAX,
            ToParent::Time { secs } | ToParent::Waiting { secs } | ToParent::Seeking { secs } => finite_secs(*secs),
            ToParent::Error { track, .. } => track.is_none_or(|t| t < TRACKS_MAX),
            ToParent::Playing | ToParent::Paused | ToParent::Ended => true,
        };
        ok.then_some(m)
    }

    pub fn encode(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("encodes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let to_child = [
            ToChild::Load { source: Source::Fd, mime: "video/webm".into(), growing: true },
            ToChild::LoadDone,
            ToChild::TextTrack { id: 3, lang: "en".into(), label: "English".into() },
            ToChild::ShowText { text: TextChoice::Page(3) },
            ToChild::ShowText { text: TextChoice::None },
            ToChild::OpenSource,
            ToChild::AddTrack { track: 0, mime: "video/webm; codecs=\"vp9\"".into() },
            ToChild::Append { track: 0, bytes: vec![1, 2, 3], timestamp_offset: Some(-2.5) },
            ToChild::Remove { track: 1, start: 0.0, end: f64::INFINITY },
            ToChild::Abort { track: 7 },
            ToChild::EndOfStream { error: None },
            ToChild::SetDuration { secs: 61.0 },
            ToChild::CanPlay { mime: "audio/webm; codecs=\"opus\"".into() },
            ToChild::Seek { secs: 12.0 },
            ToChild::SetVolume { volume: 0.5 },
            ToChild::SetRate { rate: 1.5 },
        ];
        for m in to_child {
            assert_eq!(ToChild::decode(&m.encode()), Some(m));
        }
        let to_parent = [
            ToParent::Ready { duration_secs: 10.0, width: 1920, height: 1080 },
            ToParent::Buffered { track: 0, ranges: vec![(0.0, 4.0), (8.0, 9.5)] },
            ToParent::Appended { track: 0 },
            ToParent::Time { secs: 3.25 },
            ToParent::Ended,
            ToParent::Error { code: code::TOO_BIG, track: Some(1) },
        ];
        for m in to_parent {
            assert_eq!(ToParent::decode(&m.encode()), Some(m));
        }
    }

    #[test]
    fn refusals() {
        let bad_child = [
            ToChild::AddTrack { track: TRACKS_MAX, mime: "video/webm".into() },
            ToChild::Append { track: 0, bytes: vec![0; APPEND_MAX + 1], timestamp_offset: None },
            ToChild::Append { track: 0, bytes: vec![], timestamp_offset: Some(f64::NAN) },
            ToChild::Remove { track: 0, start: 5.0, end: 1.0 },
            ToChild::Seek { secs: -1.0 },
            ToChild::Seek { secs: f64::NAN },
            ToChild::SetVolume { volume: 1.5 },
            ToChild::SetRate { rate: 0.1 },
            ToChild::CanPlay { mime: "a".repeat(MIME_MAX + 1) },
            ToChild::Load { source: Source::Fd, mime: "video/\nmp4".into(), growing: false },
            ToChild::TextTrack { id: TEXT_TRACKS_MAX, lang: "en".into(), label: String::new() },
            ToChild::TextTrack { id: 0, lang: "en".into(), label: "x".repeat(TEXT_NAME_MAX + 1) },
            ToChild::TextTrack { id: 0, lang: "e\nn".into(), label: String::new() },
            ToChild::ShowText { text: TextChoice::Page(TEXT_TRACKS_MAX) },
        ];
        for m in bad_child {
            assert_eq!(ToChild::decode(&m.encode()), None, "{m:?}");
        }
        let bad_parent = [
            ToParent::Ready { duration_secs: 1.0, width: WIDTH_MAX + 1, height: 10 },
            ToParent::Buffered { track: 0, ranges: vec![(0.0, 1.0); RANGES_MAX + 1] },
            ToParent::Buffered { track: 0, ranges: vec![(2.0, 1.0)] },
            ToParent::Time { secs: f64::INFINITY },
            ToParent::CanPlay { mime: "x\u{7}".into(), supported: true },
            ToParent::Error { code: code::TOO_BIG, track: Some(TRACKS_MAX) },
        ];
        for m in bad_parent {
            assert_eq!(ToParent::decode(&m.encode()), None, "{m:?}");
        }
        assert_eq!(ToChild::decode(&[0xff, 0xff, 0xff]), None);
        assert_eq!(ToParent::decode(&[]), None);
    }
}
