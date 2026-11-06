//! Lyra v1 wire (lyra-v1.md §5): length-prefixed postcard messages, the same
//! framing Limen uses. Samples never cross these sockets; they go through the
//! shared-memory ring, whose fd follows the message that announces it.
//!
//! The dictionaries:
//! - **app ⟷ choragusd** ([`ToChoragus`], [`ToApp`]): what an app may say.
//! - **volume UI ⟷ choragusd** ([`ToControl`], [`FromControl`]): levels only.
//! - **limend ⟷ choragusd** ([`ToAttribution`], [`FromAttribution`]): tickets
//!   that let an embedded player play as its parent.
//! - **choragusd ⟷ lyrad** ([`ToEngine`], [`FromEngine`]): mechanism only.
//!
//! A frame is a `u32` little-endian length followed by that many bytes of
//! postcard, at most [`MAX_FRAME`]. A message that carries fds (`Opened`: the
//! ring, then the clock page) is followed on the socket by one `SCM_RIGHTS`
//! message per fd ([`crate::fdpass`]), in that order.

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::io::{self, Read, Write};

/// Wire version an app sends in `Hello`.
pub const VERSION: u16 = 1;
/// Largest frame body, in bytes.
pub const MAX_FRAME: usize = 4096;

// ── limits (lyra-v1.md §5) ───────────────────────────────────────────────────
pub const MIN_RATE: u32 = 8_000;
pub const MAX_RATE: u32 = 192_000;
/// Channel counts a stream may have: mono, stereo, 5.1, 7.1.
pub const CHANNELS: [u8; 4] = [1, 2, 6, 8];
pub const MIN_BUFFER_MS: u32 = 10;
pub const MAX_BUFFER_MS: u32 = 2_000;
pub const MAX_LABEL: usize = 64;
/// Streams one app connection may hold.
pub const MAX_STREAMS_PER_APP: usize = 8;
/// Ring memory one app connection may hold, in bytes.
pub const MAX_RING_BYTES_PER_APP: usize = 4 << 20;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Media,
    Communication,
    Notification,
    Game,
    Pro,
}

/// What a stream is, as an app asks for it. Samples in the ring are always
/// interleaved `f32`; the client library converts `i16` for apps that have it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct StreamParams {
    pub role: Role,
    pub rate: u32,
    pub channels: u8,
    pub buffer_ms: u32,
    /// Shown in the volume UI (e.g. "Video"); at most [`MAX_LABEL`] bytes.
    pub label: String,
}

impl StreamParams {
    /// Ring depth in frames for these params: the buffer rounded up to a power
    /// of two.
    pub fn ring_frames(&self) -> u64 {
        ((self.rate as u64 * self.buffer_ms as u64).div_ceil(1000)).max(1).next_power_of_two()
    }

    /// Ring size in bytes (header excluded).
    pub fn ring_bytes(&self) -> usize {
        self.ring_frames() as usize * self.channels as usize * 4
    }

    /// Check the limits; the error is the [`DenyCode`] to answer with.
    pub fn check(&self) -> Result<(), DenyCode> {
        if !(MIN_RATE..=MAX_RATE).contains(&self.rate)
            || !CHANNELS.contains(&self.channels)
            || !(MIN_BUFFER_MS..=MAX_BUFFER_MS).contains(&self.buffer_ms)
            || self.label.len() > MAX_LABEL
        {
            return Err(DenyCode::BadFormat);
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum DenyCode {
    /// The app's grant (or the socket it reached) doesn't allow it.
    NotGranted,
    TooManyStreams,
    TooMuchMemory,
    BadFormat,
    /// lyrad refused or is not there.
    EngineUnavailable,
    /// The message named a stream this connection doesn't own.
    NoSuchStream,
}

// ── app ⟷ choragusd ─────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum ToChoragus {
    /// First message. `ticket`: the attribution ticket limend gave an
    /// embedded player (Limen `AudioTicket`), so its streams count as the
    /// parent app's; none for an ordinary app.
    Hello { version: u16, ticket: Option<[u8; 16]> },
    /// `tag` is the app's own number, echoed in `Opened` or `Denied`.
    Open { tag: u32, params: StreamParams },
    /// A new stream is paused until `Start`, so the app can fill the ring first.
    Start { stream: u32 },
    Pause { stream: u32 },
    /// Drop everything queued (a seek).
    Flush { stream: u32 },
    /// Tell me when the last frame written has reached the speaker.
    Drain { stream: u32 },
    /// The app's own level, 0 to 1, under the user's.
    SetVolume { stream: u32, gain: f32 },
    Close { stream: u32 },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum ToApp {
    Welcome { device_rate: u32, device_channels: u8, period_frames: u32 },
    /// Followed by two fds: the ring (producer end), then the clock page
    /// (read-only, [`crate::clock`]).
    Opened { tag: u32, stream: u32, ring_frames: u64, latency_ns: u64 },
    /// `tag`: the `Open` this answers, or 0 for another message.
    Denied { tag: u32, code: DenyCode },
    /// The next frame written will have stream position `frame`.
    Flushed { stream: u32, frame: u64 },
    Drained { stream: u32 },
    /// Frames of silence played because the ring was empty (at most once a second).
    Underrun { stream: u32, frames: u64 },
    /// Policy is holding the stream silent (seat switch) or released it.
    Held { stream: u32, held: bool },
    /// lyrad went away; open a new stream when the service is back.
    StreamLost { stream: u32 },
}

// ── volume UI ⟷ choragusd (audio-control/, lyra-v1.md §7) ──────────────────

/// Apps an `Apps` message lists (so it fits one frame).
pub const MAX_CONTROL_APPS: usize = 16;
/// Longest app id the control wire carries (longer ones are cut).
pub const MAX_APP_ID: usize = 128;

/// What the volume UI (Forum, Curia: the `audio-control` capability) may say.
/// It names apps, never streams, and only its own user's.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum ToControl {
    Hello { version: u16 },
    /// Answered by `Apps`.
    List,
    /// The user's level for an app, 0 to 1. Kept across restarts.
    SetAppVolume { app_id: String, volume: f32 },
    SetAppMute { app_id: String, muted: bool },
    SetMaster { volume: f32 },
    SetMasterMute { muted: bool },
    /// Answered by `Outputs`.
    ListOutputs,
    SetOutput { output: u32 },
}

/// One app with a stream open, as the volume UI shows it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AppEntry {
    pub app_id: String,
    /// The label of its first stream (e.g. "Video").
    pub label: String,
    pub role: Role,
    /// The user's level for the app.
    pub volume: f32,
    pub muted: bool,
    pub streams: u8,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Output {
    pub id: u32,
    pub name: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum FromControl {
    Welcome,
    /// The user's apps with a stream open. The answer to `List`, and sent
    /// unasked whenever it changes (an app opens or closes a stream, a level
    /// changes). Empty while the user's session is not the active one.
    Apps { master: f32, master_muted: bool, apps: Vec<AppEntry> },
    Outputs { current: u32, outputs: Vec<Output> },
    Denied { code: DenyCode },
}

// ── limend ⟷ choragusd (audio-attribution/, lyra-v1.md §4) ──────────────────

/// An attribution ticket: 128 random bits.
pub type Ticket = [u8; 16];

/// limend asks for, and cancels, attribution tickets. Only limend's uid is
/// served. One reply per request.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum ToAttribution {
    /// "The embedded instance running as `child_uid` plays for the app whose
    /// connection to limend has `parent_uid`." choragusd names the parent
    /// through the launch registry and refuses unless it holds `audio`.
    Issue { child_uid: u32, parent_uid: u32 },
    /// The embed ended: the ticket dies, and a connection that used it is
    /// hung up.
    Cancel { ticket: Ticket },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum FromAttribution {
    Issued { ticket: Ticket },
    Refused,
    Cancelled,
}

// ── choragusd ⟷ lyrad ───────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum ToEngine {
    /// Make a stream (paused) and its ring. lyrad answers `Opened` + ring fd,
    /// or `OpenFailed`.
    Open { stream: u32, rate: u32, channels: u8, ring_frames: u64 },
    Close { stream: u32 },
    Start { stream: u32 },
    Pause { stream: u32 },
    Flush { stream: u32 },
    Drain { stream: u32 },
    /// The stream's whole level: user × app × policy × seat, composed by
    /// choragusd. lyrad ramps to it.
    SetGain { stream: u32, db: f32 },
    /// Move to another sink (one sink in v1).
    Reroute { stream: u32, sink: u32 },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum FromEngine {
    /// Followed by two fds, both for the app: the ring (producer end), then the
    /// clock page (read-only).
    Opened { stream: u32, ring_frames: u64, latency_ns: u64 },
    OpenFailed { stream: u32 },
    Flushed { stream: u32, frame: u64 },
    Drained { stream: u32 },
    Underrun { stream: u32, frames: u64 },
}

// ── framing ─────────────────────────────────────────────────────────────────

/// Encode one message as a frame.
pub fn encode<T: Serialize>(msg: &T) -> io::Result<Vec<u8>> {
    let body = postcard::to_allocvec(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if body.len() > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut f = Vec::with_capacity(4 + body.len());
    f.extend_from_slice(&(body.len() as u32).to_le_bytes());
    f.extend_from_slice(&body);
    Ok(f)
}

/// Write one message.
pub fn send<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    w.write_all(&encode(msg)?)
}

/// Read one message. An over-long frame or one that doesn't decode is an error
/// (the caller hangs up); end of file is `UnexpectedEof`.
pub fn recv<R: Read, T: DeserializeOwned>(r: &mut R) -> io::Result<T> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    postcard::from_bytes(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> StreamParams {
        StreamParams { role: Role::Media, rate: 48_000, channels: 2, buffer_ms: 100, label: "Video".into() }
    }

    #[test]
    fn every_message_round_trips() {
        let app: Vec<ToChoragus> = vec![
            ToChoragus::Hello { version: VERSION, ticket: Some([7; 16]) },
            ToChoragus::Open { tag: 3, params: params() },
            ToChoragus::Start { stream: 1 },
            ToChoragus::Pause { stream: 1 },
            ToChoragus::Flush { stream: 1 },
            ToChoragus::Drain { stream: 1 },
            ToChoragus::SetVolume { stream: 1, gain: 0.5 },
            ToChoragus::Close { stream: 1 },
        ];
        for m in app {
            let mut c = std::io::Cursor::new(encode(&m).unwrap());
            assert_eq!(recv::<_, ToChoragus>(&mut c).unwrap(), m);
        }
        let ev: Vec<ToApp> = vec![
            ToApp::Welcome { device_rate: 48_000, device_channels: 2, period_frames: 128 },
            ToApp::Opened { tag: 3, stream: 1, ring_frames: 8192, latency_ns: 5_000_000 },
            ToApp::Denied { tag: 3, code: DenyCode::TooManyStreams },
            ToApp::Flushed { stream: 1, frame: 99 },
            ToApp::Drained { stream: 1 },
            ToApp::Underrun { stream: 1, frames: 128 },
            ToApp::Held { stream: 1, held: true },
            ToApp::StreamLost { stream: 1 },
        ];
        for m in ev {
            let mut c = std::io::Cursor::new(encode(&m).unwrap());
            assert_eq!(recv::<_, ToApp>(&mut c).unwrap(), m);
        }
        for m in [
            ToEngine::Open { stream: 1, rate: 44_100, channels: 6, ring_frames: 4096 },
            ToEngine::SetGain { stream: 1, db: -18.0 },
            ToEngine::Reroute { stream: 1, sink: 0 },
        ] {
            let mut c = std::io::Cursor::new(encode(&m).unwrap());
            assert_eq!(recv::<_, ToEngine>(&mut c).unwrap(), m);
        }
        for m in [FromEngine::Opened { stream: 1, ring_frames: 4096, latency_ns: 1 }, FromEngine::Underrun { stream: 1, frames: 5 }] {
            let mut c = std::io::Cursor::new(encode(&m).unwrap());
            assert_eq!(recv::<_, FromEngine>(&mut c).unwrap(), m);
        }
    }

    #[test]
    fn oversize_and_garbage_frames_are_refused() {
        let mut big = (MAX_FRAME as u32 + 1).to_le_bytes().to_vec();
        big.extend(vec![0u8; MAX_FRAME + 1]);
        assert!(recv::<_, ToChoragus>(&mut std::io::Cursor::new(big)).is_err());
        let garbage = [3u8, 0, 0, 0, 0xff, 0xff, 0xff];
        assert!(recv::<_, ToChoragus>(&mut std::io::Cursor::new(garbage)).is_err());
        let label = "x".repeat(MAX_FRAME);
        assert!(encode(&ToChoragus::Open { tag: 0, params: StreamParams { label, ..params() } }).is_err());
    }

    #[test]
    fn params_limits() {
        assert_eq!(params().check(), Ok(()));
        for p in [
            StreamParams { rate: 7_999, ..params() },
            StreamParams { rate: 192_001, ..params() },
            StreamParams { channels: 3, ..params() },
            StreamParams { channels: 0, ..params() },
            StreamParams { buffer_ms: 9, ..params() },
            StreamParams { buffer_ms: 2_001, ..params() },
            StreamParams { label: "x".repeat(65), ..params() },
        ] {
            assert_eq!(p.check(), Err(DenyCode::BadFormat), "{p:?}");
        }
        // 48 kHz × 100 ms = 4800 frames -> 8192.
        assert_eq!(params().ring_frames(), 8192);
        assert_eq!(params().ring_bytes(), 8192 * 2 * 4);
    }

    #[test]
    fn a_full_apps_list_fits_one_frame() {
        let e = AppEntry { app_id: "a".repeat(MAX_APP_ID), label: "l".repeat(MAX_LABEL), role: Role::Media, volume: 1.0, muted: false, streams: 8 };
        let m = FromControl::Apps { master: 1.0, master_muted: false, apps: vec![e; MAX_CONTROL_APPS] };
        assert!(encode(&m).is_ok());
    }
}
