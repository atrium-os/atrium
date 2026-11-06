//! atrium-player's decoders (docs/spec/atrium-player.md §4): dav1d (AV1),
//! libvpx (VP8, VP9), libopus and libvorbis, each behind a small C shim
//! ([`ffi`] is the only `unsafe` code), and the conversions from their
//! planar output to the formats Fresco's video layers take.
//!
//! This crate links into the decode worker only, which runs in Capsicum
//! capability mode: the C code is treated as exploitable.
#![deny(unsafe_code)]

pub mod convert;
mod ffi;

use atrium_media_demux::{Codec, Kind, TrackInfo};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The bitstream is bad, or the decoder failed.
    Decode,
    /// A frame over the size limit.
    TooBig,
    NoMemory,
    /// A codec, profile or configuration the build doesn't decode.
    Unsupported,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Error::Decode => "decode error",
            Error::TooBig => "frame too big",
            Error::NoMemory => "out of memory",
            Error::Unsupported => "unsupported",
        })
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    I400,
    I420,
    I422,
    I444,
}

impl Layout {
    /// Chroma plane size for a `w`x`h` picture.
    pub fn chroma_size(self, w: u32, h: u32) -> (u32, u32) {
        match self {
            Layout::I400 => (0, 0),
            Layout::I420 => (w.div_ceil(2), h.div_ceil(2)),
            Layout::I422 => (w.div_ceil(2), h),
            Layout::I444 => (w, h),
        }
    }
}

/// A decoded picture, borrowed from its decoder until the next call on it.
/// Samples over 8 bits are u16 little-endian, value in the low bits.
#[derive(Debug)]
pub struct Picture<'a> {
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
    pub layout: Layout,
    pub planes: [&'a [u8]; 3],
    pub strides: [usize; 3],
    pub pts_ns: i64,
}

pub trait VideoDecoder: Send {
    /// Give the decoder one coded frame. `Ok(false)`: it's busy, so
    /// [`Self::receive`] pictures first and send the same frame again.
    fn send(&mut self, data: &[u8], pts_ns: i64) -> Result<bool>;
    /// The next decoded picture, if one is ready. Call until `None` after
    /// each send; after the last send, until `None` to drain.
    fn receive(&mut self) -> Result<Option<Picture<'_>>>;
    /// Drop everything in flight (a seek).
    fn flush(&mut self);
}

pub trait AudioDecoder: Send {
    fn rate(&self) -> u32;
    fn channels(&self) -> u16;
    /// Decode one packet, appending interleaved f32 frames to `out`.
    /// Returns the frames appended.
    fn decode(&mut self, packet: &[u8], out: &mut Vec<f32>) -> Result<usize>;
    /// Drop decoder state (a seek).
    fn reset(&mut self);
}

#[derive(Debug, Clone, Copy)]
pub struct Config {
    pub threads: u32,
    pub max_width: u32,
    pub max_height: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config { threads: 4, max_width: 7680, max_height: 4320 }
    }
}

/// Codecs this build decodes (atrium-player.md §9.3–9.4: no H.264, no AAC).
pub fn supported(codec: Codec) -> bool {
    matches!(codec, Codec::Av1 | Codec::Vp9 | Codec::Vp8 | Codec::Opus | Codec::Vorbis)
}

/// Answer for `MediaSource.isTypeSupported` (lives with the demuxers so the
/// supervisor can answer without linking a decoder).
pub use atrium_media_demux::mime_supported;

pub fn video(track: &TrackInfo, cfg: &Config) -> Result<Box<dyn VideoDecoder>> {
    if track.encrypted {
        return Err(Error::Unsupported);
    }
    if let Kind::Video { width, height } = track.kind {
        if width > cfg.max_width || height > cfg.max_height {
            return Err(Error::TooBig);
        }
    }
    Ok(match track.codec {
        Codec::Av1 => Box::new(ffi::Dav1d::new(cfg.threads, cfg.max_width, cfg.max_height)?),
        Codec::Vp9 => Box::new(ffi::Vpx::new(true, cfg.threads, cfg.max_width, cfg.max_height)?),
        Codec::Vp8 => Box::new(ffi::Vpx::new(false, cfg.threads, cfg.max_width, cfg.max_height)?),
        _ => return Err(Error::Unsupported),
    })
}

pub fn audio(track: &TrackInfo) -> Result<Box<dyn AudioDecoder>> {
    if track.encrypted {
        return Err(Error::Unsupported);
    }
    Ok(match track.codec {
        Codec::Opus => Box::new(OpusDecoder::new(&track.config)?),
        Codec::Vorbis => Box::new(VorbisDecoder(ffi::Vorbis::new(&track.config)?)),
        _ => return Err(Error::Unsupported),
    })
}

/// Opus with its pre-skip applied: the first `pre_skip` frames after the
/// start are encoder delay, not sound.
struct OpusDecoder {
    d: ffi::Opus,
    to_skip: usize,
}

impl OpusDecoder {
    fn new(head: &[u8]) -> Result<Self> {
        let d = ffi::Opus::new(head)?;
        let pre_skip = u16::from_le_bytes([head[10], head[11]]) as usize;
        Ok(OpusDecoder { d, to_skip: pre_skip })
    }
}

impl AudioDecoder for OpusDecoder {
    fn rate(&self) -> u32 {
        48_000
    }
    fn channels(&self) -> u16 {
        self.d.channels()
    }
    fn decode(&mut self, packet: &[u8], out: &mut Vec<f32>) -> Result<usize> {
        let start = out.len();
        let n = self.d.decode(packet, out)?;
        let skip = self.to_skip.min(n);
        if skip > 0 {
            let ch = self.channels() as usize;
            out.drain(start..start + skip * ch);
            self.to_skip -= skip;
        }
        Ok(n - skip)
    }
    fn reset(&mut self) {
        self.d.reset();
        // After a seek the player decodes from 80 ms before the target and
        // drops that itself (RFC 7845 §4.6); the stream's own pre-skip only
        // applies at the very start.
        self.to_skip = 0;
    }
}

struct VorbisDecoder(ffi::Vorbis);

impl AudioDecoder for VorbisDecoder {
    fn rate(&self) -> u32 {
        self.0.rate()
    }
    fn channels(&self) -> u16 {
        self.0.channels()
    }
    fn decode(&mut self, packet: &[u8], out: &mut Vec<f32>) -> Result<usize> {
        self.0.decode(packet, out)
    }
    fn reset(&mut self) {
        self.0.reset()
    }
}
