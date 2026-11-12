//! Video through the CDM's decrypt-and-decode path (hospes.md §8.3, H0b): Widevine refuses
//! decrypt-only for part of every video stream, so encrypted video goes to the CDM whole and
//! comes back as I420 pictures. Audio stays decrypt-only.
//!
//! The CDM runs in hospes-cdm, a separate capability-mode process; nothing it sends is
//! trusted. Every frame is size- and bounds-checked ([`hospes_proto::Frame::planes`])
//! before a plane is borrowed from it.
//!
//! H.264 goes in as Chromium sends it: Annex-B, with SPS and PPS in front of each key frame
//! and no extra data. 4-byte AVCC length fields become start codes in place, so the
//! subsample map stays the same; the parameter sets add to the first clear run.
use std::os::unix::net::UnixStream;

use atrium_media_decode::{Error, Layout, Picture, Result, VideoDecoder};
use atrium_media_demux::{self as demux, Codec, TrackInfo};
use hospes_proto::{io, scheme, status, Msg};

/// `cdm::VideoCodec`, `cdm::VideoCodecProfile` and `cdm::VideoFormat` values.
mod cdm {
    pub const CODEC_H264: u32 = 2;
    pub const CODEC_VP9: u32 = 3;
    pub const H264_BASELINE: u32 = 2;
    pub const H264_MAIN: u32 = 3;
    pub const H264_EXTENDED: u32 = 4;
    pub const H264_HIGH: u32 = 5;
    pub const H264_HIGH10: u32 = 6;
    pub const H264_HIGH422: u32 = 7;
    pub const H264_HIGH444: u32 = 8;
    pub const VP9_PROFILE0: u32 = 9;
    pub const FORMAT_I420: u32 = 2;
    /// `cdm::StreamType::kStreamTypeVideo`.
    pub const STREAM_VIDEO: u32 = 1;
}

const START: [u8; 4] = [0, 0, 0, 1];

/// A sample as hospes-cdm takes it. Chromium hands CDMs 16-byte IVs: an 8-byte one is the
/// counter's high half.
pub fn proto_sample(s: &demux::Sample, data: Vec<u8>) -> hospes_proto::Sample {
    let mut out = hospes_proto::Sample { timestamp: s.pts_ns / 1000, data, ..Default::default() };
    if let Some(c) = s.crypto.as_ref() {
        let mut iv = c.iv.clone();
        iv.resize(16, 0);
        out.scheme = match c.scheme {
            demux::Scheme::Cenc => scheme::CENC,
            demux::Scheme::Cbcs => scheme::CBCS,
        };
        out.key_id = c.key_id.to_vec();
        out.iv = iv;
        out.subsamples = c.subsamples.clone();
        out.crypt_byte_block = c.crypt_byte_block as u32;
        out.skip_byte_block = c.skip_byte_block as u32;
    }
    out
}

/// `avcC` → (profile, SPS+PPS as Annex-B), for 4-byte length fields only.
fn h264_config(c: &[u8]) -> Option<(u32, Vec<u8>)> {
    if c.len() < 7 || c[0] != 1 || c[4] & 3 != 3 {
        return None;
    }
    let profile = match c[1] {
        66 => cdm::H264_BASELINE,
        77 => cdm::H264_MAIN,
        88 => cdm::H264_EXTENDED,
        100 => cdm::H264_HIGH,
        110 => cdm::H264_HIGH10,
        122 => cdm::H264_HIGH422,
        244 => cdm::H264_HIGH444,
        _ => return None,
    };
    let mut params = Vec::new();
    let mut i = 5;
    for group in 0..2 {
        let n = if group == 0 { (c.get(i)? & 0x1f) as usize } else { *c.get(i)? as usize };
        i += 1;
        for _ in 0..n {
            let l = u16::from_be_bytes([*c.get(i)?, *c.get(i + 1)?]) as usize;
            params.extend_from_slice(&START);
            params.extend_from_slice(c.get(i + 2..i + 2 + l)?);
            i += 2 + l;
        }
    }
    Some((profile, params))
}

/// 4-byte AVCC length fields → start codes, in place. `None` if a length runs past the end.
fn annexb_in_place(d: &mut [u8]) -> Option<()> {
    let mut i = 0;
    while i < d.len() {
        let l = u32::from_be_bytes(d.get(i..i + 4)?.try_into().ok()?) as usize;
        d[i..i + 4].copy_from_slice(&START);
        i = i.checked_add(4 + l)?;
    }
    (i == d.len()).then_some(())
}

struct Owned {
    frame: hospes_proto::Frame,
    planes: [(usize, usize, usize); 3],
}

pub struct CdmVideo {
    sock: UnixStream,
    max: (u32, u32),
    /// H.264: SPS and PPS, put in front of each key frame. `None` for VP9.
    params: Option<Vec<u8>>,
    pending: Option<Owned>,
    cur: Option<Owned>,
    finishing: bool,
    finished: bool,
}

impl CdmVideo {
    /// Ask the CDM to decode `t` itself. `Err(Unsupported)` when the track is one we don't
    /// send it, or the CDM says no (the Clear Key test CDM, for one): the caller then falls
    /// back to decrypt-only and our own decoders.
    pub fn new(t: &TrackInfo, max: (u32, u32), mut sock: UnixStream) -> Result<CdmVideo> {
        let p = t.protection.as_ref().ok_or(Error::Unsupported)?;
        let demux::Kind::Video { width, height } = t.kind else { return Err(Error::Unsupported) };
        let (codec, profile, params) = match t.codec {
            Codec::H264 => {
                let (profile, params) = h264_config(&t.config).ok_or(Error::Unsupported)?;
                (cdm::CODEC_H264, profile, Some(params))
            }
            // vpcC: version, flags, then profile.
            Codec::Vp9 if t.config.len() < 5 || t.config[4] == 0 => (cdm::CODEC_VP9, cdm::VP9_PROFILE0, None),
            _ => return Err(Error::Unsupported),
        };
        let init = Msg::InitVideo {
            codec,
            profile,
            format: cdm::FORMAT_I420,
            width,
            height,
            extra: Vec::new(),
            scheme: match p.scheme {
                demux::Scheme::Cenc => scheme::CENC,
                demux::Scheme::Cbcs => scheme::CBCS,
            },
        };
        io::send(&mut sock, &init).map_err(|_| Error::Unsupported)?;
        match io::recv(&mut sock) {
            Ok(Some(Msg::InitResult { status })) if status == status::SUCCESS => {}
            _ => return Err(Error::Unsupported),
        }
        Ok(CdmVideo { sock, max, params, pending: None, cur: None, finishing: false, finished: false })
    }

    /// One `DecryptAndDecode`; a picture, if the CDM had one, goes to `pending`.
    fn call(&mut self, sample: hospes_proto::Sample) -> Result<()> {
        io::send(&mut self.sock, &Msg::DecryptAndDecode { sample }).map_err(|_| Error::Decode)?;
        let frame = match io::recv(&mut self.sock) {
            Ok(Some(Msg::Decoded { frame })) => frame,
            _ => return Err(Error::Decode),
        };
        match frame.status {
            status::SUCCESS => {
                if frame.width > self.max.0 || frame.height > self.max.1 {
                    return Err(Error::TooBig);
                }
                let planes = frame.planes(self.max.0, self.max.1).ok_or(Error::Decode)?;
                self.pending = Some(Owned { frame, planes });
                Ok(())
            }
            status::NEED_MORE_DATA => {
                if self.finishing {
                    self.finished = true;
                }
                Ok(())
            }
            status::NO_KEY => Err(Error::NoKey),
            _ => Err(Error::Decode),
        }
    }
}

impl VideoDecoder for CdmVideo {
    fn send(&mut self, data: &[u8], pts_ns: i64) -> Result<bool> {
        let s = demux::Sample { track: 0, pts_ns, dts_ns: pts_ns, duration_ns: None, key: false, data: data.to_vec(), crypto: None };
        self.send_sample(&s)
    }

    fn send_sample(&mut self, s: &demux::Sample) -> Result<bool> {
        if self.pending.is_some() {
            return Ok(false);
        }
        let mut data = Vec::with_capacity(s.data.len() + self.params.as_ref().map_or(0, |p| p.len()));
        let mut subsamples = s.crypto.as_ref().map(|c| c.subsamples.clone()).unwrap_or_default();
        if let Some(params) = self.params.as_ref() {
            // In CENC the length fields lie in clear runs, so this touches no cipher bytes;
            // a sample that is encrypted whole has none to touch.
            if s.key && (s.crypto.is_none() || !subsamples.is_empty()) {
                data.extend_from_slice(params);
                if let Some(first) = subsamples.first_mut() {
                    first.0 = first.0.checked_add(params.len() as u32).ok_or(Error::Decode)?;
                }
            }
            let at = data.len();
            data.extend_from_slice(&s.data);
            if s.crypto.is_none() || !subsamples.is_empty() {
                annexb_in_place(&mut data[at..]).ok_or(Error::Decode)?;
            }
        } else {
            data.extend_from_slice(&s.data);
        }
        let mut sample = proto_sample(s, data);
        sample.subsamples = subsamples;
        self.call(sample)?;
        Ok(true)
    }

    fn receive(&mut self) -> Result<Option<Picture<'_>>> {
        if self.pending.is_none() && self.finishing && !self.finished {
            self.call(hospes_proto::Sample::default())?;
        }
        self.cur = self.pending.take();
        let Some(o) = self.cur.as_ref() else { return Ok(None) };
        let f = &o.frame;
        let (w, h) = (f.width as usize, f.height as usize);
        let rows = [h, h.div_ceil(2), h.div_ceil(2)];
        let cols = [w, w.div_ceil(2), w.div_ceil(2)];
        let mut planes: [&[u8]; 3] = [&[]; 3];
        let mut strides = [0usize; 3];
        for i in 0..3 {
            let (off, stride, _) = o.planes[i];
            planes[i] = &f.data[off..off + stride * (rows[i] - 1) + cols[i]];
            strides[i] = stride;
        }
        Ok(Some(Picture {
            width: f.width,
            height: f.height,
            bit_depth: 8,
            layout: Layout::I420,
            planes,
            strides,
            pts_ns: f.timestamp.saturating_mul(1000),
            color: demux::Color::default(),
        }))
    }

    fn flush(&mut self) {
        self.pending = None;
        self.cur = None;
        self.finishing = false;
        self.finished = false;
        if io::send(&mut self.sock, &Msg::ResetDecoder { stream: cdm::STREAM_VIDEO }).is_ok() {
            let _ = io::recv(&mut self.sock);
        }
    }

    fn finish(&mut self) {
        self.finishing = true;
    }
}

impl Drop for CdmVideo {
    fn drop(&mut self) {
        if io::send(&mut self.sock, &Msg::DeinitDecoder { stream: cdm::STREAM_VIDEO }).is_ok() {
            let _ = io::recv(&mut self.sock);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avcc_lengths_become_start_codes() {
        let mut d = vec![0, 0, 0, 2, 0x65, 1, 0, 0, 0, 1, 0x41];
        annexb_in_place(&mut d).unwrap();
        assert_eq!(d, [0, 0, 0, 1, 0x65, 1, 0, 0, 0, 1, 0x41]);
        let mut bad = vec![0, 0, 0, 9, 1];
        assert!(annexb_in_place(&mut bad).is_none());
    }

    #[test]
    fn avcc_config() {
        // High profile, 4-byte lengths, one SPS, one PPS.
        let c = [1, 100, 0, 31, 0xff, 0xe1, 0, 2, 0x67, 1, 1, 0, 1, 0x68];
        let (p, params) = h264_config(&c).unwrap();
        assert_eq!(p, cdm::H264_HIGH);
        assert_eq!(params, [0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68]);
        // 2-byte lengths: left to decrypt-only.
        let mut c2 = c;
        c2[4] = 0xfd;
        assert!(h264_config(&c2).is_none());
    }

    /// The worker's end against a stand-in hospes-cdm: what goes over the socket, and what a
    /// frame turns into.
    #[test]
    fn decrypt_and_decode_against_a_stand_in_cdm() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let cdm = std::thread::spawn(move || {
            let mut seen = Vec::new();
            loop {
                let m = match io::recv(&mut b) {
                    Ok(Some(m)) => m,
                    _ => return seen,
                };
                let reply = match &m {
                    Msg::InitVideo { .. } => Msg::InitResult { status: status::SUCCESS },
                    Msg::DecryptAndDecode { sample } if sample.data.is_empty() => {
                        Msg::Decoded { frame: hospes_proto::Frame { status: status::NEED_MORE_DATA, ..Default::default() } }
                    }
                    Msg::DecryptAndDecode { sample } if sample.timestamp == 1000 => Msg::Decoded {
                        frame: hospes_proto::Frame {
                            status: status::SUCCESS,
                            format: 2,
                            width: 4,
                            height: 2,
                            offsets: [0, 8, 10],
                            strides: [4, 2, 2],
                            timestamp: 1000,
                            data: (0..12).collect(),
                        },
                    },
                    Msg::DecryptAndDecode { .. } => Msg::Decoded { frame: hospes_proto::Frame { status: status::NO_KEY, ..Default::default() } },
                    _ => Msg::Done,
                };
                seen.push(m);
                io::send(&mut b, &reply).unwrap();
            }
        });
        let t = TrackInfo {
            index: 0,
            id: 1,
            codec: Codec::H264,
            kind: demux::Kind::Video { width: 4, height: 2 },
            config: vec![1, 100, 0, 31, 0xff, 0xe1, 0, 2, 0x67, 1, 1, 0, 1, 0x68],
            default_duration_ns: None,
            encrypted: false,
            protection: Some(demux::Protection { scheme: demux::Scheme::Cenc, key_id: [7; 16], iv_size: 8, constant_iv: Vec::new(), crypt_byte_block: 0, skip_byte_block: 0 }),
            color: None,
        };
        let mut d = CdmVideo::new(&t, (64, 64), a).unwrap();
        let crypto = demux::Crypto { scheme: demux::Scheme::Cenc, key_id: [7; 16], iv: vec![9; 8], subsamples: vec![(5, 3)], crypt_byte_block: 0, skip_byte_block: 0 };
        let key = demux::Sample { track: 0, pts_ns: 1_000_000, dts_ns: 1_000_000, duration_ns: None, key: true, data: vec![0, 0, 0, 4, 0x65, 0xa, 0xb, 0xc], crypto: Some(Box::new(crypto.clone())) };
        assert_eq!(d.send_sample(&key), Ok(true));
        // A picture is waiting: busy until it is taken.
        assert_eq!(d.send_sample(&key), Ok(false));
        let p = d.receive().unwrap().unwrap();
        assert_eq!((p.width, p.height, p.pts_ns, p.layout), (4, 2, 1_000_000, Layout::I420));
        assert_eq!((p.planes[0], p.planes[1], p.planes[2]), (&(0..8).collect::<Vec<u8>>()[..], &[8u8, 9][..], &[10u8, 11][..]));
        assert!(d.receive().unwrap().is_none());
        let next = demux::Sample { pts_ns: 2_000_000, key: false, ..key.clone() };
        assert_eq!(d.send_sample(&next), Err(Error::NoKey));
        d.finish();
        assert!(d.receive().unwrap().is_none());
        d.flush();
        drop(d);
        let seen = cdm.join().unwrap();
        assert!(matches!(&seen[0], Msg::InitVideo { codec: 2, profile: 5, format: 2, width: 4, height: 2, scheme: 1, extra } if extra.is_empty()));
        let Msg::DecryptAndDecode { sample } = &seen[1] else { panic!("{:?}", seen[1]) };
        // SPS and PPS in front, the length field a start code, the clear run grown to match.
        assert_eq!(sample.data, [0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 0, 0, 0, 1, 0x65, 0xa, 0xb, 0xc]);
        assert_eq!(sample.subsamples, [(16, 3)]);
        assert_eq!((sample.scheme, sample.iv.len(), sample.timestamp), (scheme::CENC, 16, 1000));
        let Msg::DecryptAndDecode { sample } = &seen[2] else { panic!() };
        assert_eq!((sample.data.len(), &sample.subsamples[..]), (8, &[(5, 3)][..]));
        assert!(matches!(&seen[3], Msg::DecryptAndDecode { sample } if sample.data.is_empty()));
        assert!(matches!(seen[4], Msg::ResetDecoder { stream: 1 }));
        assert!(matches!(seen[5], Msg::DeinitDecoder { stream: 1 }));
        assert_eq!(seen.len(), 6);
    }
}
