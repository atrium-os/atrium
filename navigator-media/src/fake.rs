//! ★ A FAKE `media-player` CHILD, for tests only. It takes what a parent
//! sends as bytes, checks each message as the real player does
//! (`ToChild::decode`), and checks the ORDER and LIMITS limen.md §2.2 sets —
//! `AddTrack` only in a feed, `Append` only on a track that exists, ONE
//! `Append` in flight per track, at most 8 tracks, the feed's byte cap —
//! recording every breach in [`FakePlayer::violations`]. It answers as the
//! player would, in order (an `Append`'s `Buffered` before its `Appended`),
//! but only when [`FakePlayer::deliver`] is called, so a test can hold
//! answers back and see whether the parent waits for them.
//!
//! It plays nothing. Everything checked against it is checked against the
//! role's contract, not against the native player (P1).

use limen::media_player::{code, Source, ToChild, ToParent, FEED_BYTES_MAX, TRACKS_MAX};
use std::collections::VecDeque;

pub struct FakePlayer {
    /// Every message received, in order (as decoded).
    pub received: Vec<ToChild>,
    pub violations: Vec<String>,
    /// The feed's byte cap (default the role's 150 MB).
    pub cap: u64,
    /// Types it answers `CanPlay` true for (prefixes).
    pub supports: Vec<&'static str>,
    feed: bool,
    loaded: bool,
    tracks: Vec<Option<(String, u64, bool)>>, // (mime, bytes, in_flight)
    pending: VecDeque<ToParent>,
    ready_sent: bool,
    time: f64,
    bytes_per_sec: f64,
}

impl Default for FakePlayer { fn default() -> Self { Self::new() } }

impl FakePlayer {
    pub fn new() -> Self {
        FakePlayer { received: vec![], violations: vec![], cap: FEED_BYTES_MAX, supports: vec!["video/webm", "audio/webm", "video/mp4", "audio/mp4"],
                     feed: false, loaded: false, tracks: vec![None; TRACKS_MAX as usize], pending: VecDeque::new(), ready_sent: false,
                     time: 0.0, bytes_per_sec: 100_000.0 }
    }

    fn violation(&mut self, v: String) { self.violations.push(v) }

    /// One message, as bytes: decoded and checked; its answers are queued.
    pub fn receive(&mut self, bytes: &[u8]) {
        let Some(m) = ToChild::decode(bytes) else { self.violation("a malformed or out-of-bounds message".into()); return };
        self.received.push(m.clone());
        match m {
            ToChild::Load { source, mime, .. } => {
                if self.loaded { self.violation("a second source for one embed".into()) }
                self.loaded = true;
                let Source::Fd = source;
                let ok = self.supports.iter().any(|s| mime.starts_with(s));
                self.pending.push_back(if ok { ToParent::Ready { duration_secs: 10.0, width: 640, height: 360 } } else { ToParent::Error { code: code::UNSUPPORTED, track: None } });
            }
            ToChild::OpenSource => {
                if self.loaded { self.violation("OpenSource after a source".into()) }
                self.loaded = true;
                self.feed = true;
            }
            ToChild::AddTrack { track, mime } => {
                if !self.feed { self.violation("AddTrack outside a feed".into()); self.pending.push_back(ToParent::Error { code: code::STATE, track: None }); return }
                if self.tracks[track as usize].is_some() { self.violation(format!("track {track} added twice")) }
                self.tracks[track as usize] = Some((mime, 0, false));
            }
            ToChild::Append { track, bytes, .. } => {
                let used: u64 = self.tracks.iter().flatten().map(|t| t.1).sum();
                let cap = self.cap;
                let Some(t) = self.tracks[track as usize].as_mut() else {
                    self.violation(format!("Append on track {track}, which was never added"));
                    self.pending.push_back(ToParent::Error { code: code::STATE, track: None });
                    return;
                };
                if t.2 { self.violation(format!("a second Append in flight on track {track}")); self.pending.push_back(ToParent::Error { code: code::STATE, track: None }); return }
                if used + bytes.len() as u64 > cap { self.pending.push_back(ToParent::Error { code: code::TOO_BIG, track: Some(track) }); return }
                t.1 += bytes.len() as u64;
                t.2 = true;
                let end = t.1 as f64 / self.bytes_per_sec;
                self.pending.push_back(ToParent::Buffered { track, ranges: vec![(0.0, end)] });
                self.pending.push_back(ToParent::Appended { track });
                if !self.ready_sent { self.ready_sent = true; self.pending.push_back(ToParent::Ready { duration_secs: f64::INFINITY, width: 1280, height: 720 }) }
            }
            ToChild::Remove { track, start, end } => {
                let bps = self.bytes_per_sec;
                match self.tracks[track as usize].as_mut() {
                    None => self.violation(format!("Remove on track {track}, which was never added")),
                    Some(t) => {
                        // Removing from the start frees what was played.
                        let have = t.1 as f64 / bps;
                        if start <= 0.0 { t.1 = ((have - end.min(have)).max(0.0) * bps) as u64 }
                        let shown = t.1 as f64 / bps;
                        let ranges = if start <= 0.0 { if shown > 0.0 { vec![(end.min(have), have)] } else { vec![] } } else { vec![(0.0, start.min(have))] };
                        self.pending.push_back(ToParent::Buffered { track, ranges });
                    }
                }
            }
            ToChild::Abort { track } => {
                if self.tracks[track as usize].is_none() { self.violation(format!("Abort on track {track}, which was never added")) }
            }
            ToChild::EndOfStream { .. } => { if !self.feed { self.violation("EndOfStream outside a feed".into()) } }
            ToChild::SetDuration { .. } => { if !self.feed { self.violation("SetDuration outside a feed".into()) } }
            ToChild::CanPlay { mime } => {
                let supported = self.supports.iter().any(|s| mime.starts_with(s)) && !mime.contains("hev1") && !mime.contains("hvc1");
                self.pending.push_back(ToParent::CanPlay { mime, supported });
            }
            ToChild::Play => { if !self.loaded { self.violation("Play before a source".into()) } self.pending.push_back(ToParent::Playing); self.pending.push_back(ToParent::Time { secs: self.time }) }
            ToChild::Pause => self.pending.push_back(ToParent::Paused),
            ToChild::Stop => {}
            ToChild::LoadDone => { if !self.loaded || self.feed { self.violation("LoadDone without a Load".into()) } }
            ToChild::Seek { secs } => { self.time = secs; self.pending.push_back(ToParent::Time { secs }) }
            ToChild::SetVolume { .. } | ToChild::SetRate { .. } | ToChild::TextTrack { .. } | ToChild::ShowText { .. }
            | ToChild::MediaMetadata { .. } | ToChild::MediaActions { .. } | ToChild::RequestPip | ToChild::ExitPip => {}
        }
    }

    /// The player's answers so far, in order, as the parent receives them
    /// (encoded and checked with `ToParent::decode`). Clears the in-flight
    /// marks of the appends answered.
    pub fn deliver(&mut self) -> Vec<ToParent> {
        let out: Vec<ToParent> = self.pending.drain(..).collect();
        for m in &out {
            if let ToParent::Appended { track } = m { if let Some(t) = self.tracks[*track as usize].as_mut() { t.2 = false } }
        }
        out.into_iter().map(|m| ToParent::decode(&m.encode()).expect("the fake sends well-formed messages")).collect()
    }

    /// Answers only the first `n` queued messages (to hold the rest back).
    pub fn deliver_some(&mut self, n: usize) -> Vec<ToParent> {
        let rest = self.pending.split_off(n.min(self.pending.len()));
        let out = self.deliver();
        self.pending = rest;
        out
    }

    /// Playback reaches `secs` (a `Time`), or the end.
    pub fn advance(&mut self, secs: f64, ended: bool) {
        self.time = secs;
        self.pending.push_back(ToParent::Time { secs });
        if ended { self.pending.push_back(ToParent::Ended) }
    }

    /// The bytes buffered on a track.
    pub fn bytes(&self, track: u8) -> u64 { self.tracks[track as usize].as_ref().map(|t| t.1).unwrap_or(0) }
}
