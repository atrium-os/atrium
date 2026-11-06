//! Sound through Lyra (atrium-player.md §5, M6): one stream through
//! choragusd as role `Media`, and the audio clock as the master the video
//! follows (lyra-v1.md §6, §12.1).
//!
//! The supervisor reads PCM from the worker's ring and writes it into the
//! Lyra stream's ring. Each contiguous run is anchored: stream frame `f0`
//! carries media time `m0`, so media time `m` is heard at
//! `heard_at(f0) + (m − m0)`, corrected by the device's measured speed. The
//! video layer is told to show each frame at that instant.
use std::io;

use lyra_protocol::client::{Client, Stream};
use lyra_protocol::clock::{Clock, STATE_PLAYING};
use lyra_protocol::wire::{Role, StreamParams, ToApp};

use crate::supervisor::{Player, Sink};
use crate::wsola::Wsola;

/// Where choragusd listens inside an app's jail (lyra-v1.md §5).
pub const DEFAULT_SOCKET: &str = "/atrium/sockets/audio/audio.sock";
/// How much the Lyra ring holds.
const BUFFER_MS: u32 = 200;
/// Queued before the stream starts, so it doesn't underrun at once.
const PRIME_MS: u64 = 60;
/// Timestamp differences this small are not gaps.
const JITTER_NS: i64 = 5_000_000;

/// Media time to `CLOCK_MONOTONIC`, from the audio clock.
#[derive(Debug, Clone, Copy)]
pub struct AvMap {
    pub clock: Clock,
    pub frame0: u64,
    pub media0: i64,
    /// Playback rate: media seconds per second heard.
    pub rate: f64,
}

impl AvMap {
    /// When media time `pts_ns` is (or was) heard.
    pub fn at(&self, pts_ns: i64) -> i64 {
        let speed = 1.0 + self.clock.rate_ppm as f64 / 1e6;
        self.clock.heard_at(self.frame0) + ((pts_ns - self.media0) as f64 / self.rate / speed) as i64
    }
}

pub struct AudioOut {
    client: Client,
    stream: Stream,
    /// The stream's sample rate.
    rate_hz: u32,
    channels: usize,
    /// The stream position of the next frame written.
    next_frame: u64,
    /// The current run's anchor: (stream frame, media ns).
    anchor: Option<(u64, i64)>,
    /// Media time just after the last frame written.
    written_until: Option<i64>,
    /// Waiting for `Flushed` after a seek: write nothing.
    flushing: bool,
    /// PCM before this media time is dropped (a seek's pre-roll).
    from_ns: Option<i64>,
    playing: bool,
    started: bool,
    pub lost: bool,
    pub underruns: u64,
    buf: Vec<f32>,
    /// Rate other than 1: the PCM goes through WSOLA first.
    stretch: Option<Wsola>,
    rate: f64,
}

impl AudioOut {
    /// Open a stream for `rate` and `channels`. `ticket`: Limen's attribution
    /// ticket, for an embedded player (it plays as its parent).
    pub fn open(path: &str, ticket: Option<[u8; 16]>, rate: u32, channels: u16) -> io::Result<Self> {
        let mut client = Client::connect(path, ticket)?;
        let stream = client.open(StreamParams { role: Role::Media, rate, channels: channels as u8, buffer_ms: BUFFER_MS, label: "Video".into() })?;
        Ok(AudioOut {
            client,
            stream,
            rate_hz: rate,
            channels: channels as usize,
            next_frame: 0,
            anchor: None,
            written_until: None,
            flushing: false,
            from_ns: None,
            playing: false,
            started: false,
            lost: false,
            underruns: 0,
            buf: vec![0f32; 48_000 * 8],
            stretch: None,
            rate: 1.0,
        })
    }

    pub fn set_playing(&mut self, play: bool) {
        if play == self.playing {
            return;
        }
        self.playing = play;
        if !play && self.started {
            self.started = false;
            self.lost |= self.client.pause(&self.stream).is_err();
        }
        // Play starts the stream once enough is queued (in `pump`).
    }

    /// Drop what is queued; PCM from `to_ns` on comes next.
    pub fn seek(&mut self, to_ns: i64) {
        if self.started {
            self.started = false;
            self.lost |= self.client.pause(&self.stream).is_err();
        }
        self.lost |= self.client.flush(&self.stream).is_err();
        self.flushing = true;
        self.anchor = None;
        self.written_until = None;
        self.from_ns = Some(to_ns);
        self.stretch = None;
    }

    /// Play at `rate` (0.25 to 4) with the pitch kept. Takes effect from
    /// the next run: the caller seeks to where it is, which flushes.
    pub fn set_rate(&mut self, rate: f64) {
        self.rate = rate.clamp(0.25, 4.0);
        self.stretch = None;
    }

    pub fn set_volume(&mut self, v: f32) {
        self.lost |= self.client.set_volume(&self.stream, v.clamp(0.0, 1.0)).is_err();
    }

    fn rate_hz(&self) -> u32 {
        self.rate_hz
    }

    /// The playback rate.
    pub fn rate(&self) -> f64 {
        self.rate
    }

    fn ns_of(&self, frames: u64) -> i64 {
        (frames as i128 * 1_000_000_000 / self.rate_hz as i128) as i64
    }

    /// Move decoded audio into the stream, as much as it has room for, and
    /// start it once primed. Returns false once Lyra is gone.
    pub fn pump<S: Sink>(&mut self, p: &mut Player<S>, ended: bool) -> bool {
        loop {
            match self.client.try_event() {
                Ok(Some(ToApp::Flushed { frame, .. })) => {
                    self.next_frame = frame;
                    self.flushing = false;
                }
                Ok(Some(ToApp::Underrun { frames, .. })) => self.underruns += frames,
                Ok(Some(ToApp::StreamLost { .. })) | Err(_) => {
                    self.lost = true;
                    break;
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
            }
        }
        if self.lost || self.flushing {
            return !self.lost;
        }
        let ch = self.channels;
        loop {
            // Stretched output waiting for room goes first.
            if let Some(w) = self.stretch.as_mut() {
                while w.ready() > 0 {
                    let room = self.stream.writable() as usize;
                    if room == 0 {
                        break;
                    }
                    let mut out = vec![0f32; room.min(w.ready()) * ch];
                    let n = w.pull(&mut out);
                    self.next_frame += self.stream.write(&out[..n * ch]);
                }
                if w.ready() > 0 {
                    break;
                }
            }
            let room = self.stream.writable() as usize;
            if room == 0 {
                break;
            }
            // Input for that much output.
            let room_in = ((room as f64 * self.rate) as usize).max(1);
            let want = (room_in * ch).min(self.buf.len() / ch * ch);
            let mut buf = std::mem::take(&mut self.buf);
            let (n, pts) = p.read_pcm(&mut buf[..want]);
            if n == 0 {
                self.buf = buf;
                break;
            }
            let Some(mut pts) = pts else {
                self.buf = buf;
                break;
            };
            let mut start = 0;
            if let Some(from) = self.from_ns {
                if pts < from {
                    let skip = (((from - pts) as i128 * self.rate_hz() as i128 / 1_000_000_000) as usize).min(n);
                    start = skip;
                    pts += self.ns_of(skip as u64);
                }
                if start < n {
                    self.from_ns = None;
                }
            }
            if start < n {
                // A gap or a jump in media time starts a new run. Container
                // times jitter by a millisecond or so (WebM counts in ms);
                // within that, the sample count is the truth.
                let contiguous = self.written_until.is_some_and(|w| (pts - w).abs() <= JITTER_NS.max(self.ns_of(1)));
                if !contiguous {
                    // A new run: what WSOLA still holds belongs to the old one.
                    self.stretch = None;
                    self.anchor = Some((self.next_frame, pts));
                }
                let frames = &buf[start * ch..n * ch];
                if self.rate == 1.0 {
                    let w = self.stream.write(frames) as usize;
                    self.next_frame += w as u64;
                    self.written_until = Some(pts + self.ns_of(w as u64));
                } else {
                    let (rate_hz, rate) = (self.rate_hz(), self.rate);
                    let w = self.stretch.get_or_insert_with(|| Wsola::new(rate_hz, ch, rate));
                    w.push(frames);
                    self.written_until = Some(pts + self.ns_of((n - start) as u64));
                }
            }
            self.buf = buf;
        }
        if self.playing && !self.started {
            let queued = self.anchor.map_or(0, |(f0, _)| self.next_frame.saturating_sub(f0));
            if queued * 1000 >= PRIME_MS * self.rate_hz as u64 || (ended && queued > 0) {
                self.started = true;
                self.lost |= self.client.start(&self.stream).is_err();
            }
        }
        !self.lost
    }

    /// The current run's (stream frame, media ns).
    pub fn anchor(&self) -> Option<(u64, i64)> {
        self.anchor
    }

    /// The map the video follows, once the stream plays.
    pub fn map(&self) -> Option<AvMap> {
        let clock = self.stream.clock();
        let (frame0, media0) = self.anchor?;
        (self.started && clock.state == STATE_PLAYING).then_some(AvMap { clock, frame0, media0, rate: self.rate })
    }

    /// Media time being heard now, if playing.
    pub fn position_ns(&self, now_ns: i64) -> Option<i64> {
        let m = self.map()?;
        let speed = 1.0 + m.clock.rate_ppm as f64 / 1e6;
        Some(m.media0 + ((now_ns - m.clock.heard_at(m.frame0)) as f64 * speed * m.rate) as i64)
    }
}
