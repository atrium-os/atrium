//! The wire between the supervisor and the decode worker, over one
//! `socketpair(AF_UNIX, SOCK_STREAM)`.
//!
//! - **Supervisor → worker:** frames of `[len: u32 LE][nfds: u32 LE][postcard ToWorker]`,
//!   with `nfds` descriptors attached (SCM_RIGHTS) to the frame's header bytes.
//!   The worker trusts these: they come from the process it serves.
//! - **Worker → supervisor:** fixed 32-byte [`Record`]s, decoded by hand
//!   with every field range-checked. The worker is presumed compromised, so
//!   this is the supervisor's whole parsing surface for it.
use serde::{Deserialize, Serialize};

/// Largest supervisor → worker frame: one 1 MiB append plus framing.
pub const FRAME_MAX: usize = (1 << 20) + 4096;
/// Most descriptors on one frame.
pub const FDS_MAX: usize = 16;
/// Most layer buffers (fresco-video §3: 3 to 8).
pub const BUFFERS_MAX: u32 = 8;
/// Longest cue text the worker may send (bytes).
pub const CUE_TEXT_MAX: u32 = 512;

/// The pixel format of the layer buffers (fresco-video §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Format {
    Nv12,
    P010,
}

impl Format {
    pub fn bytes_per_sample(self) -> usize {
        match self {
            Format::Nv12 => 1,
            Format::P010 => 2,
        }
    }
    /// Layer size for a picture: Fresco wants even dimensions for 4:2:0.
    pub fn layer_dims(width: u32, height: u32) -> (u32, u32) {
        (width.div_ceil(2) * 2, height.div_ceil(2) * 2)
    }
    /// Bytes one buffer holds (fresco_protocol `buffer_bytes`): luma rows,
    /// then interleaved chroma rows, packed, at even dimensions.
    pub fn buffer_size(self, width: u32, height: u32) -> usize {
        let (w, h) = Self::layer_dims(width, height);
        w as usize * h as usize * 3 / 2 * self.bytes_per_sample()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Mode {
    /// Play the resource on descriptor 0 of this frame (read-only, seekable).
    Load,
    /// A segment feed: tracks come with `AddTrack`.
    Feed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Limits {
    /// The feed's buffer cap, bytes (limen.md §2.2: 150 MB).
    pub feed_bytes: u64,
    pub max_width: u32,
    pub max_height: u32,
    pub threads: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { feed_bytes: 150_000_000, max_width: 7680, max_height: 4320, threads: 4 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ToWorker {
    /// `growing` (Load only): the file is still being written; wait at
    /// its end for more until `InputDone`.
    Init { mode: Mode, limits: Limits, growing: bool },
    /// A growing Load's file is complete.
    InputDone,
    AddTrack { track: u8, mime: String },
    Append { track: u8, bytes: Vec<u8>, timestamp_offset_ns: Option<i64> },
    Remove { track: u8, start_ns: i64, end_ns: i64 },
    Abort { track: u8 },
    EndOfStream,
    /// The layer buffers, as descriptors on this frame, in order.
    Buffers { format: Format, width: u32, height: u32, count: u32 },
    /// The PCM ring, as descriptor 0 on this frame (see [`crate::ring`]).
    PcmRing,
    /// Buffer `index` is free again.
    Release { index: u32 },
    /// Go to the keyframe at or before `t_ns`; frames from before carry
    /// the old epoch.
    Seek { t_ns: i64, epoch: u32 },
    /// Test builds only: try to escape and report what the kernel said.
    ProbeSandbox,
    Shutdown,
}

/// Record tags.
pub mod tag {
    pub const READY: u32 = 1;
    pub const VIDEO_TRACK: u32 = 2;
    pub const AUDIO_TRACK: u32 = 3;
    pub const DURATION: u32 = 4;
    pub const FRAME: u32 = 5;
    pub const PCM: u32 = 6;
    pub const APPENDED: u32 = 7;
    pub const BUFFERED: u32 = 8;
    pub const SEEKED: u32 = 9;
    pub const ENDED: u32 = 10;
    pub const ERROR: u32 = 11;
    pub const PROBE: u32 = 12;
    pub const COLOR: u32 = 13;
    pub const HDR: u32 = 14;
    pub const CUE: u32 = 15;
    pub const CUE_TEXT: u32 = 16;
    pub const STARVED: u32 = 17;
}

/// Error codes (map to limen `media_player::code`).
pub mod err {
    pub const DECODE: u32 = 1;
    pub const UNSUPPORTED: u32 = 2;
    pub const TOO_BIG: u32 = 3;
    pub const STATE: u32 = 4;
}

pub const RECORD_LEN: usize = 32;

/// A worker → supervisor record, as the supervisor decodes it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Record {
    /// The worker has entered its sandbox and read its Init.
    Ready { sandboxed: bool },
    VideoTrack { track: u8, codec: u32, width: u32, height: u32, bit_depth: u8 },
    AudioTrack { track: u8, codec: u32, rate: u32, channels: u16 },
    Duration { secs: f64 },
    /// Buffer `index` holds a frame to show at `pts_ns`.
    Frame { index: u32, epoch: u32, pts_ns: i64 },
    /// `frames` PCM frames starting at media time `pts_ns` were written at
    /// ring position `pos` (frames since the ring began).
    Pcm { epoch: u32, pts_ns: i64, frames: u32, pos: u64 },
    Appended { track: u8 },
    /// Range `i` of `n` for `track`; `n == 0` means none.
    Buffered { track: u8, i: u8, n: u8, start_ns: i64, end_ns: i64 },
    /// The seek for `epoch` is done; PCM before ring position `pos` is stale.
    Seeked { epoch: u32, pos: u64 },
    /// Everything has been decoded.
    Ended,
    /// `track`: a feed track's append was refused (over the cap), and no
    /// `Appended` follows for it; `None`: the worker has failed.
    Error { code: u32, track: Option<u8> },
    /// `ProbeSandbox` results: bit 0 open refused, bit 1 connect refused,
    /// bit 2 fork refused.
    Probe { refused: u32 },
    /// The video's colour, in H.273 code points (2: unspecified); range 0
    /// unspecified, 1 limited, 2 full. Follows `VideoTrack`.
    Color { primaries: u8, transfer: u8, matrix: u8, range: u8 },
    /// A subtitle cue shown over `[start_ns, end_ns)`; its `len` bytes of
    /// UTF-8 follow in `CueText` records.
    Cue { start_ns: i64, end_ns: i64, len: u32 },
    /// Piece `i` of a cue's text, 24 bytes (the last one zero-padded).
    CueText { i: u32, bytes: [u8; 24] },
    /// Static HDR metadata (CTA-861.3 units), after `Color`.
    Hdr { primaries_x: [u16; 3], primaries_y: [u16; 3], white: [u16; 2], luminance: [u16; 2], light: [u16; 2] },
    /// A growing file has no more bytes yet; sent about once a second
    /// while the worker waits, so the watchdog knows it is alive.
    Starved,
}

fn put(b: &mut [u8; RECORD_LEN], tag: u32, a: u32, x: u64, y: u64, z: u64) {
    b[0..4].copy_from_slice(&tag.to_le_bytes());
    b[4..8].copy_from_slice(&a.to_le_bytes());
    b[8..16].copy_from_slice(&x.to_le_bytes());
    b[16..24].copy_from_slice(&y.to_le_bytes());
    b[24..32].copy_from_slice(&z.to_le_bytes());
}

impl Record {
    pub fn encode(&self) -> [u8; RECORD_LEN] {
        let mut b = [0u8; RECORD_LEN];
        match *self {
            Record::Ready { sandboxed } => put(&mut b, tag::READY, sandboxed as u32, 0, 0, 0),
            Record::VideoTrack { track, codec, width, height, bit_depth } => {
                put(&mut b, tag::VIDEO_TRACK, track as u32 | codec << 8, width as u64, height as u64, bit_depth as u64)
            }
            Record::AudioTrack { track, codec, rate, channels } => {
                put(&mut b, tag::AUDIO_TRACK, track as u32 | codec << 8, rate as u64, channels as u64, 0)
            }
            Record::Duration { secs } => put(&mut b, tag::DURATION, 0, secs.to_bits(), 0, 0),
            Record::Frame { index, epoch, pts_ns } => put(&mut b, tag::FRAME, index, pts_ns as u64, epoch as u64, 0),
            Record::Pcm { epoch, pts_ns, frames, pos } => put(&mut b, tag::PCM, frames, pts_ns as u64, epoch as u64, pos),
            Record::Appended { track } => put(&mut b, tag::APPENDED, track as u32, 0, 0, 0),
            Record::Buffered { track, i, n, start_ns, end_ns } => {
                put(&mut b, tag::BUFFERED, track as u32 | (i as u32) << 8 | (n as u32) << 16, start_ns as u64, end_ns as u64, 0)
            }
            Record::Seeked { epoch, pos } => put(&mut b, tag::SEEKED, epoch, pos, 0, 0),
            Record::Ended => put(&mut b, tag::ENDED, 0, 0, 0, 0),
            Record::Starved => put(&mut b, tag::STARVED, 0, 0, 0, 0),
            Record::Error { code, track } => put(&mut b, tag::ERROR, code, track.map_or(0xFF, |t| t as u64), 0, 0),
            Record::Probe { refused } => put(&mut b, tag::PROBE, refused, 0, 0, 0),
            Record::Color { primaries, transfer, matrix, range } => {
                put(&mut b, tag::COLOR, u32::from_le_bytes([primaries, transfer, matrix, range]), 0, 0, 0)
            }
            Record::Cue { start_ns, end_ns, len } => put(&mut b, tag::CUE, len, start_ns as u64, end_ns as u64, 0),
            Record::CueText { i, bytes } => {
                let w = |k: usize| u64::from_le_bytes(bytes[k * 8..k * 8 + 8].try_into().unwrap());
                put(&mut b, tag::CUE_TEXT, i, w(0), w(1), w(2))
            }
            Record::Hdr { primaries_x: px, primaries_y: py, white, luminance, light } => {
                let w = |a: [u16; 4]| a.iter().enumerate().fold(0u64, |acc, (i, &v)| acc | (v as u64) << (16 * i));
                put(&mut b, tag::HDR, 0, w([px[0], px[1], px[2], py[0]]), w([py[1], py[2], white[0], white[1]]), w([luminance[0], luminance[1], light[0], light[1]]))
            }
        }
        b
    }

    /// Decode a record from a hostile worker. `None` for anything out of range.
    pub fn decode(b: &[u8; RECORD_LEN]) -> Option<Record> {
        let u32_at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        let u64_at = |o: usize| {
            let mut a = [0u8; 8];
            a.copy_from_slice(&b[o..o + 8]);
            u64::from_le_bytes(a)
        };
        let (t, a, x, y, z) = (u32_at(0), u32_at(4), u64_at(8), u64_at(16), u64_at(24));
        let track = (a & 0xFF) as u8;
        let r = match t {
            tag::READY if a <= 1 => Record::Ready { sandboxed: a == 1 },
            tag::VIDEO_TRACK => {
                if track >= 8 || x == 0 || y == 0 || x > 7680 || y > 4320 || !matches!(z, 8 | 10 | 12) {
                    return None;
                }
                Record::VideoTrack { track, codec: a >> 8, width: x as u32, height: y as u32, bit_depth: z as u8 }
            }
            tag::AUDIO_TRACK => {
                if track >= 8 || !(8_000..=192_000).contains(&x) || !matches!(y, 1 | 2 | 6 | 8) {
                    return None;
                }
                Record::AudioTrack { track, codec: a >> 8, rate: x as u32, channels: y as u16 }
            }
            tag::DURATION => {
                let secs = f64::from_bits(x);
                if !(secs.is_finite() && secs >= 0.0 || secs == f64::INFINITY) {
                    return None;
                }
                Record::Duration { secs }
            }
            tag::FRAME if a < BUFFERS_MAX && y <= u32::MAX as u64 => Record::Frame { index: a, epoch: y as u32, pts_ns: x as i64 },
            tag::PCM if a <= 1 << 20 && y <= u32::MAX as u64 => Record::Pcm { epoch: y as u32, pts_ns: x as i64, frames: a, pos: z },
            tag::APPENDED if a < 8 => Record::Appended { track },
            tag::BUFFERED => {
                let (i, n) = ((a >> 8 & 0xFF) as u8, (a >> 16 & 0xFF) as u8);
                if track >= 8 || n > 64 || (n > 0 && i >= n) || (x as i64) > (y as i64) {
                    return None;
                }
                Record::Buffered { track, i, n, start_ns: x as i64, end_ns: y as i64 }
            }
            tag::SEEKED => Record::Seeked { epoch: a, pos: x },
            tag::ENDED => Record::Ended,
            tag::STARVED => Record::Starved,
            tag::ERROR if (1..=4).contains(&a) && (x < 8 || x == 0xFF) => Record::Error { code: a, track: (x != 0xFF).then_some(x as u8) },
            tag::PROBE if a < 8 => Record::Probe { refused: a },
            tag::COLOR => {
                let [primaries, transfer, matrix, range] = a.to_le_bytes();
                if range > 2 {
                    return None;
                }
                Record::Color { primaries, transfer, matrix, range }
            }
            tag::CUE if a <= CUE_TEXT_MAX && (x as i64) <= (y as i64) => Record::Cue { start_ns: x as i64, end_ns: y as i64, len: a },
            tag::CUE_TEXT if a < CUE_TEXT_MAX.div_ceil(24) => {
                let mut bytes = [0u8; 24];
                bytes[..8].copy_from_slice(&x.to_le_bytes());
                bytes[8..16].copy_from_slice(&y.to_le_bytes());
                bytes[16..].copy_from_slice(&z.to_le_bytes());
                Record::CueText { i: a, bytes }
            }
            tag::HDR => {
                let h = |v: u64, i: usize| (v >> (16 * i)) as u16;
                Record::Hdr {
                    primaries_x: [h(x, 0), h(x, 1), h(x, 2)],
                    primaries_y: [h(x, 3), h(y, 0), h(y, 1)],
                    white: [h(y, 2), h(y, 3)],
                    luminance: [h(z, 0), h(z, 1)],
                    light: [h(z, 2), h(z, 3)],
                }
            }
            _ => return None,
        };
        Some(r)
    }
}

pub fn encode_frame(m: &ToWorker) -> Vec<u8> {
    postcard::to_stdvec(m).expect("encodes")
}

pub fn decode_frame(b: &[u8]) -> Option<ToWorker> {
    postcard::from_bytes(b).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip() {
        let rs = [
            Record::Ready { sandboxed: true },
            Record::VideoTrack { track: 0, codec: 2, width: 1920, height: 1080, bit_depth: 10 },
            Record::Color { primaries: 9, transfer: 16, matrix: 9, range: 1 },
            Record::Cue { start_ns: 200_000_000, end_ns: 900_000_000, len: 30 },
            Record::CueText { i: 1, bytes: *b"abcdefghijklmnopqrstuvwx" },
            Record::Hdr { primaries_x: [34000, 13250, 7500], primaries_y: [16000, 34500, 3000], white: [15635, 16450], luminance: [1000, 50], light: [1000, 400] },
            Record::AudioTrack { track: 1, codec: 5, rate: 48_000, channels: 2 },
            Record::Duration { secs: 12.5 },
            Record::Frame { index: 3, epoch: 2, pts_ns: -5 },
            Record::Pcm { epoch: 1, pts_ns: 1_000, frames: 960, pos: 1 << 40 },
            Record::Appended { track: 7 },
            Record::Buffered { track: 0, i: 1, n: 2, start_ns: 0, end_ns: 9 },
            Record::Buffered { track: 0, i: 0, n: 0, start_ns: 0, end_ns: 0 },
            Record::Seeked { epoch: 4, pos: 77 },
            Record::Ended,
            Record::Starved,
            Record::Error { code: err::DECODE, track: None },
            Record::Error { code: err::TOO_BIG, track: Some(2) },
            Record::Probe { refused: 7 },
        ];
        for r in rs {
            assert_eq!(Record::decode(&r.encode()), Some(r));
        }
    }

    #[test]
    fn hostile_records_refused() {
        let bad = [
            Record::VideoTrack { track: 8, codec: 0, width: 10, height: 10, bit_depth: 8 }.encode(),
            Record::VideoTrack { track: 0, codec: 0, width: 9000, height: 10, bit_depth: 8 }.encode(),
            Record::VideoTrack { track: 0, codec: 0, width: 10, height: 10, bit_depth: 9 }.encode(),
            Record::AudioTrack { track: 0, codec: 0, rate: 1, channels: 2 }.encode(),
            Record::Frame { index: BUFFERS_MAX, epoch: 0, pts_ns: 0 }.encode(),
            Record::Buffered { track: 0, i: 3, n: 2, start_ns: 0, end_ns: 1 }.encode(),
            Record::Buffered { track: 0, i: 0, n: 1, start_ns: 5, end_ns: 1 }.encode(),
            Record::Duration { secs: f64::NAN }.encode(),
            Record::Error { code: 99, track: None }.encode(),
            [0xFF; RECORD_LEN],
        ];
        for b in bad {
            assert_eq!(Record::decode(&b), None, "{b:?}");
        }
    }
}
