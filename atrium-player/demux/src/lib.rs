//! WebM/Matroska and MP4 demuxers for atrium-player (docs/spec/atrium-player.md §3).
//!
//! Every byte given to these parsers is hostile. So:
//! - no `unsafe`, no I/O: a parser takes byte slices and returns samples;
//! - every size is checked against its parent and against [`Limits`] before
//!   anything is allocated, and going over a limit is an error, never a
//!   bigger allocation;
//! - unknown elements and boxes are skipped by size.
//!
//! Both parsers are *push* parsers, so they take MSE appends as they come.
//! A whole file (`Load`) is pushed the same way; when the parser needs bytes
//! from elsewhere in the file (an MP4 whose `moov` follows its `mdat`, or a
//! seek), it says so with [`Demuxer::wants_offset`] and the driver repositions
//! its input and calls [`Demuxer::input_at`].
#![forbid(unsafe_code)]

mod rd;
pub mod mp4;
pub mod webm;

use std::fmt;

/// How far before a seek target sound is decoded from, and the result
/// dropped, so the decoder has converged by the target (RFC 7845 §4.6).
pub const AUDIO_PREROLL_NS: i64 = 80_000_000;

/// Hard limits. Defaults suit everything up to 8K video.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Most tracks in one file or init segment.
    pub max_tracks: usize,
    /// Largest single coded frame.
    pub max_frame: usize,
    /// Largest codec configuration blob (`CodecPrivate`, `av1C`, ...).
    pub max_config: usize,
    /// Largest element or box the parser buffers whole (Tracks, Cues, moov, moof).
    pub max_buffered: usize,
    /// Most samples in an MP4 sample table, and cue points in WebM.
    pub max_index: usize,
    /// Largest frame dimensions (fresco-video §3).
    pub max_width: u32,
    pub max_height: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_tracks: 8,
            max_frame: 16 << 20,
            max_config: 256 << 10,
            max_buffered: 64 << 20,
            max_index: 4_000_000,
            max_width: 7680,
            max_height: 4320,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The bytes aren't this format.
    NotThisFormat,
    /// Malformed: a size that overruns its parent, a bad varint, ...
    Malformed(&'static str),
    /// Over one of [`Limits`].
    TooBig(&'static str),
    /// Well-formed, but not something the player handles (encryption,
    /// compressed tracks, a plain MP4 pushed without random access).
    Unsupported(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotThisFormat => write!(f, "not this format"),
            Error::Malformed(w) => write!(f, "malformed: {w}"),
            Error::TooBig(w) => write!(f, "over a limit: {w}"),
            Error::Unsupported(w) => write!(f, "unsupported: {w}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Av1,
    Vp9,
    Vp8,
    H264,
    Opus,
    Vorbis,
    Aac,
    /// WebVTT cues: WebM's `D_WEBVTT/*` (a block is a cue's text), or MP4's
    /// `wvtt` (a sample is `vttc`/`vtte` boxes).
    WebVtt,
    /// MP4 timed text (`tx3g`, 3GPP TS 26.245): a u16 length, then UTF-8.
    Tx3g,
    Unknown,
}

impl Codec {
    pub fn is_video(self) -> bool {
        matches!(self, Codec::Av1 | Codec::Vp9 | Codec::Vp8 | Codec::H264)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Video { width: u32, height: u32 },
    Audio { rate: u32, channels: u16 },
    /// Subtitles or captions.
    Text,
    Other,
}

/// How a video track's samples are to be read as colour, in ITU-T H.273
/// code points (2: unspecified), as the container says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
    /// 0: unspecified, 1: limited, 2: full.
    pub range: u8,
    pub hdr: Option<Hdr>,
}

impl Default for Color {
    fn default() -> Self {
        Color { primaries: 2, transfer: 2, matrix: 2, range: 0, hdr: None }
    }
}

/// Static HDR metadata, in CTA-861.3's units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Hdr {
    /// R, G, B chromaticity of the mastering display, units of 0.00002.
    pub primaries_x: [u16; 3],
    pub primaries_y: [u16; 3],
    pub white_x: u16,
    pub white_y: u16,
    /// Peak, cd/m².
    pub max_luminance: u16,
    /// Black, units of 0.0001 cd/m².
    pub min_luminance: u16,
    /// Brightest pixel and brightest frame average, cd/m² (0: unknown).
    pub max_cll: u16,
    pub max_fall: u16,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackInfo {
    /// Index into the track list; what [`Sample::track`] refers to.
    pub index: u8,
    /// The container's own track number or ID.
    pub id: u64,
    pub codec: Codec,
    pub kind: Kind,
    /// Codec configuration, normalised: Opus as an `OpusHead`, Vorbis as
    /// the three Xiph headers each prefixed by a u32 LE length, others as
    /// the container carries them (`av1C`, `vpcC`, `avcC`, `esds`).
    pub config: Vec<u8>,
    pub default_duration_ns: Option<u64>,
    /// Encrypted (`ContentEncryption`, `encv`/`enca`): the player can't
    /// decode it without a CDM.
    pub encrypted: bool,
    /// Video: the container's colour description, if it has one.
    pub color: Option<Color>,
}

#[derive(Clone, PartialEq)]
pub struct Sample {
    pub track: u8,
    pub pts_ns: i64,
    pub dts_ns: i64,
    pub duration_ns: Option<u64>,
    pub key: bool,
    pub data: Vec<u8>,
}

impl fmt::Debug for Sample {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sample")
            .field("track", &self.track)
            .field("pts_ns", &self.pts_ns)
            .field("dts_ns", &self.dts_ns)
            .field("duration_ns", &self.duration_ns)
            .field("key", &self.key)
            .field("len", &self.data.len())
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// The track list, from the header or an init segment. May come again
    /// for a new init segment.
    Tracks(Vec<TrackInfo>),
    /// The duration the container declares, in seconds.
    Duration(f64),
    Sample(Sample),
}

/// Which parser to use, from the first bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Webm,
    Mp4,
}

pub fn probe(head: &[u8]) -> Option<Format> {
    if head.len() >= 4 && head[..4] == [0x1A, 0x45, 0xDF, 0xA3] {
        return Some(Format::Webm);
    }
    if head.len() >= 8 && matches!(&head[4..8], b"ftyp" | b"styp" | b"moov" | b"moof" | b"free" | b"mdat" | b"skip") {
        return Some(Format::Mp4);
    }
    None
}

/// What both parsers offer the player.
pub trait Demuxer {
    /// More bytes, continuing from where the input is (see [`Self::input_at`]).
    fn push(&mut self, bytes: &[u8]);
    /// The next event, or `None` when it needs more bytes (or a seek).
    fn poll(&mut self) -> Result<Option<Event>>;
    /// Bytes are needed from this absolute offset next. The driver
    /// repositions its input and calls [`Self::input_at`]. A segment feed
    /// can't, and reports [`Error::Unsupported`] instead.
    fn wants_offset(&self) -> Option<u64>;
    /// The input continues at this absolute offset; pending bytes are dropped.
    fn input_at(&mut self, offset: u64);
    /// Move to the keyframe at or before `t_ns` (random-access input only),
    /// with other tracks' samples from [`AUDIO_PREROLL_NS`] before that.
    /// Returns whether the parser knows where that is; when it does,
    /// [`Self::wants_offset`] says where to read from.
    fn seek(&mut self, t_ns: i64) -> bool;
    /// MSE `Abort`: drop a half-parsed segment, keep the track list.
    fn abort(&mut self);
    /// The input is a file that can be read out of order, so the parser may
    /// ask (by [`Self::wants_offset`]) for an index stored later in it.
    fn set_seekable(&mut self, _yes: bool) {}
    /// No more bytes come: an error if a segment is half done. A parser that
    /// went ahead for an index may ask for an offset instead.
    fn finish(&mut self) -> Result<()>;
}

/// A parser for the format `probe` found.
pub fn open(format: Format, limits: Limits) -> Box<dyn Demuxer + Send> {
    match format {
        Format::Webm => Box::new(webm::Parser::new(limits)),
        Format::Mp4 => Box::new(mp4::Parser::new(limits)),
    }
}

/// `v * 1e9 / timescale` without overflow, saturating.
pub(crate) fn to_ns(v: i128, timescale: u64) -> i64 {
    if timescale == 0 {
        return 0;
    }
    let ns = v.saturating_mul(1_000_000_000) / timescale as i128;
    ns.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

/// A byte queue with an absolute offset, shared by both parsers.
pub(crate) struct Input {
    buf: Vec<u8>,
    start: usize,
    /// Absolute offset of `buf[start]`.
    pos: u64,
}

impl Input {
    pub fn new() -> Self {
        Input { buf: Vec::new(), start: 0, pos: 0 }
    }
    pub fn push(&mut self, b: &[u8]) {
        if self.start > 0 && self.start >= self.buf.len() / 2 {
            self.buf.drain(..self.start);
            self.start = 0;
        }
        self.buf.extend_from_slice(b);
    }
    pub fn avail(&self) -> &[u8] {
        &self.buf[self.start..]
    }
    pub fn pos(&self) -> u64 {
        self.pos
    }
    pub fn consume(&mut self, n: usize) {
        let n = n.min(self.buf.len() - self.start);
        self.start += n;
        self.pos += n as u64;
    }
    pub fn reset_at(&mut self, pos: u64) {
        self.buf.clear();
        self.start = 0;
        self.pos = pos;
    }
    pub fn is_empty(&self) -> bool {
        self.start >= self.buf.len()
    }
}

/// Answer for `MediaSource.isTypeSupported` from an MSE-style MIME type
/// such as `video/webm; codecs="vp09.00.10.08, opus"`: the containers we
/// demux and the codecs the worker decodes.
pub fn mime_supported(mime: &str) -> bool {
    let mut parts = mime.split(';');
    let base = parts.next().unwrap_or("").trim().to_ascii_lowercase();
    if !matches!(base.as_str(), "video/webm" | "audio/webm" | "video/mp4" | "audio/mp4") {
        return false;
    }
    let codecs = parts.find_map(|p| p.trim().strip_prefix("codecs=")).map(|c| c.trim_matches('"'));
    let Some(codecs) = codecs else { return true };
    codecs.split(',').map(str::trim).all(|c| {
        let c = c.to_ascii_lowercase();
        c == "vp8" || c == "vp9" || c.starts_with("vp09.") || c.starts_with("vp08.") || c.starts_with("av01.") || c == "opus" || c == "vorbis"
    })
}
