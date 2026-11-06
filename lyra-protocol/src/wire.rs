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
    /// The connection's media session (atrium-player-pip-keys.md §4.2): what
    /// it plays, for media keys, Forum's now-playing and the lock screen.
    /// One per connection; `SessionState::None` ends it. The text is the
    /// page's: choragusd cuts it to [`MAX_SESSION_TEXT`] bytes and removes
    /// control and bidi characters. `actions` is a set of `action::*` bits.
    /// `position` is taken at most once a second (state changes always).
    Session {
        state: SessionState,
        title: String,
        artist: String,
        actions: u16,
        duration: Option<f64>,
        position: Option<f64>,
    },
}

/// Longest session title or artist kept (bytes).
pub const MAX_SESSION_TEXT: usize = 128;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionState {
    None,
    Paused,
    Playing,
}

/// A media action: what a key, Forum or the lock screen asks of the active
/// session. `PlayPause` is the controllers' (the Play/Pause key); a session
/// only ever receives `Play` or `Pause`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaAction {
    Play,
    Pause,
    PlayPause,
    Stop,
    Next,
    Previous,
    SeekBackward,
    SeekForward,
    SeekTo,
}

/// The `actions` bits of a [`ToChoragus::Session`].
pub mod action {
    pub const PLAY: u16 = 1;
    pub const PAUSE: u16 = 2;
    pub const STOP: u16 = 4;
    pub const NEXT: u16 = 8;
    pub const PREVIOUS: u16 = 16;
    pub const SEEK_BACKWARD: u16 = 32;
    pub const SEEK_FORWARD: u16 = 64;
    pub const SEEK_TO: u16 = 128;
    pub const ALL: u16 = 255;
}

impl MediaAction {
    /// The `action::*` bit a session must offer to receive it.
    pub fn bit(self) -> u16 {
        match self {
            MediaAction::Play => action::PLAY,
            MediaAction::Pause => action::PAUSE,
            MediaAction::PlayPause => action::PLAY | action::PAUSE,
            MediaAction::Stop => action::STOP,
            MediaAction::Next => action::NEXT,
            MediaAction::Previous => action::PREVIOUS,
            MediaAction::SeekBackward => action::SEEK_BACKWARD,
            MediaAction::SeekForward => action::SEEK_FORWARD,
            MediaAction::SeekTo => action::SEEK_TO,
        }
    }
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
    /// The output's layout changed (the user set 5.1, say): streams keep
    /// playing, converted to it; a player may switch to a matching track.
    OutputChanged { device_channels: u8 },
    /// A media key, Forum or the lock screen asks this connection's session
    /// for `action` (never `PlayPause`). `seek_offset` (seconds, for
    /// `SeekBackward`/`SeekForward`) or `seek_time` (seconds, for `SeekTo`)
    /// where it applies.
    SessionAction { action: MediaAction, seek_offset: Option<f64>, seek_time: Option<f64> },
}

// ── now-playing ⟷ choragusd (media-control/, atrium-player-pip-keys.md §4) ──

/// What a media controller (Forum's bar and key route, the lock screen: the
/// `media-control` capability) may say. It sees and steers only the active
/// session of its own user, and only while that user's session is the active
/// seat's.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum ToMedia {
    /// `lock_screen`: the lock screen gets the app and state only, unless the
    /// user turned titles on (`SetLockTitles`).
    Hello { version: u16, lock_screen: bool },
    /// Answered by `NowPlaying`, which is also sent unasked on each change.
    NowPlaying,
    /// For the active session. Ignored if it doesn't offer the action.
    Control { action: MediaAction, seek_offset: Option<f64>, seek_time: Option<f64> },
    /// forum-wm: this app has the keyboard focus now. Only a session whose
    /// app had focus since the session began can become active, so a
    /// background app can't take the media keys by starting to play.
    Focused { app_id: String },
    /// The user's choice: titles and artists on the lock screen.
    SetLockTitles { on: bool },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct NowPlaying {
    /// The app it counts for (an embedded player's parent).
    pub app_id: String,
    /// Empty on the lock screen unless the user turned titles on.
    pub title: String,
    pub artist: String,
    pub state: SessionState,
    pub actions: u16,
    pub duration: Option<f64>,
    pub position: Option<f64>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum FromMedia {
    Welcome,
    /// The active session, or none.
    NowPlaying(Option<NowPlaying>),
    Denied { code: DenyCode },
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
    /// The output's left/right balance, −1 (left only) to 1 (right only), 0
    /// centred. A system setting of the output (it corrects speakers or ears,
    /// not apps), kept across restarts; only the active session sets it.
    /// Answered by `Outputs`.
    SetBalance { output: u32, balance: f32 },
    /// The output's speaker layout: 2, 6 (5.1) or 8 (7.1) channels. The
    /// codec can't see what is plugged in, so the user says. Kept across
    /// restarts; only the active session sets it. Answered by `Outputs`.
    SetLayout { output: u32, channels: u8 },
    /// Headphone virtual surround on the output (off by default): the mix
    /// is rendered as a 7.1 speaker bed around the listener. Only takes
    /// effect while the output's layout is stereo. Kept across restarts;
    /// only the active session sets it. Answered by `Outputs`.
    SetSpatial { output: u32, on: bool },
    /// Whether an app's streams go through virtual surround (default yes;
    /// `Pro` streams never do). The user's setting, kept across restarts.
    /// Answered by `Apps`.
    SetAppSpatial { app_id: String, on: bool },
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
    /// The app's streams go through virtual surround when it is on (the
    /// user's per-app opt-out is `false`).
    pub spatial: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Output {
    pub id: u32,
    pub name: String,
    pub balance: f32,
    /// Its speaker layout (2, 6 or 8).
    pub channels: u8,
    /// Headphone virtual surround switched on.
    pub spatial: bool,
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
    /// The sink's left/right balance, −1 to 1 (choragusd clamps it). Only
    /// attenuates: the louder side stays at full level. lyrad ramps to it.
    SetBalance { sink: u32, balance: f32 },
    /// The sink's speaker layout: 2, 6 (5.1) or 8 (7.1) channels. lyrad
    /// reopens the device with it between periods and answers `Layout`.
    SetLayout { sink: u32, channels: u8 },
    /// Headphone virtual surround on the sink (only while it is stereo).
    SetSpatial { sink: u32, on: bool },
    /// Whether one stream goes through the sink's virtual surround (default
    /// yes). choragusd turns it off for `Pro` streams and the apps the user
    /// opted out, before the stream's first level.
    SetStreamSpatial { stream: u32, on: bool },
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
    /// The device is open with this many channels (each time lyrad opens it;
    /// after a `SetLayout` the device refused, the old count again).
    Layout { channels: u8 },
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
    // The whole frame must be the message: a frame of another dictionary that
    // happens to start like one of ours (a `ToMedia::Hello` sent to the app
    // socket) is refused, not half-read.
    let (m, rest) = postcard::take_from_bytes(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if !rest.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "trailing bytes in frame"));
    }
    Ok(m)
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
        let np = NowPlaying {
            app_id: "a".repeat(MAX_APP_ID),
            title: "t".repeat(MAX_SESSION_TEXT),
            artist: "r".repeat(MAX_SESSION_TEXT),
            state: SessionState::Playing,
            actions: action::ALL,
            duration: Some(1.0),
            position: Some(0.5),
        };
        let m = FromMedia::NowPlaying(Some(np));
        assert_eq!(recv::<_, FromMedia>(&mut std::io::Cursor::new(encode(&m).unwrap())).unwrap(), m);
        let m = ToChoragus::Session { state: SessionState::Paused, title: "x".into(), artist: String::new(), actions: action::PLAY, duration: None, position: Some(3.0) };
        assert_eq!(recv::<_, ToChoragus>(&mut std::io::Cursor::new(encode(&m).unwrap())).unwrap(), m);
        let m = ToApp::SessionAction { action: MediaAction::SeekTo, seek_offset: None, seek_time: Some(12.5) };
        assert_eq!(recv::<_, ToApp>(&mut std::io::Cursor::new(encode(&m).unwrap())).unwrap(), m);
        for m in [
            ToEngine::Open { stream: 1, rate: 44_100, channels: 6, ring_frames: 4096 },
            ToEngine::SetGain { stream: 1, db: -18.0 },
            ToEngine::Reroute { stream: 1, sink: 0 },
            ToEngine::SetSpatial { sink: 0, on: true },
            ToEngine::SetStreamSpatial { stream: 1, on: false },
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
        let e = AppEntry { app_id: "a".repeat(MAX_APP_ID), label: "l".repeat(MAX_LABEL), role: Role::Media, volume: 1.0, muted: false, streams: 8, spatial: true };
        let m = FromControl::Apps { master: 1.0, master_muted: false, apps: vec![e; MAX_CONTROL_APPS] };
        assert!(encode(&m).is_ok());
    }
}
