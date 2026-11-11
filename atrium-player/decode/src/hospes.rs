//! H.264 through Hospes (docs/spec/hospes.md §9, atrium-player.md §9.3): Cisco's OpenH264
//! binary runs in `hospes-codec`, a sibling process in capability mode that the supervisor
//! starts; this decoder is the worker's end of its socket. Nothing here trusts the codec:
//! every picture is size-checked by the wire crate before it is used.
use std::collections::VecDeque;
use std::os::unix::net::UnixStream;

use atrium_media_demux::{Color, TrackInfo};
use hospes_codec_wire::client::{Client, Error as WireError};

use crate::{Error, Layout, Picture, Result};

struct Owned {
    pts: i64,
    w: u32,
    h: u32,
    data: Vec<u8>,
}

pub struct Openh264 {
    c: Client,
    /// AVCC length-field size (1, 2 or 4); 0 if the samples are already Annex-B.
    nal_len: usize,
    /// SPS and PPS from `avcC`, as Annex-B, sent before the first unit and after a reset.
    params: Vec<u8>,
    need_params: bool,
    q: VecDeque<Owned>,
    cur: Option<Owned>,
    finished: bool,
    au: Vec<u8>,
}

const START: [u8; 4] = [0, 0, 0, 1];

/// `avcC` → (length size, SPS+PPS in Annex-B).
fn parse_avcc(c: &[u8]) -> Option<(usize, Vec<u8>)> {
    if c.len() < 7 || c[0] != 1 {
        return None;
    }
    let nal_len = (c[4] & 3) as usize + 1;
    let mut out = Vec::new();
    let mut i = 5;
    for group in 0..2 {
        let n = if group == 0 { (c.get(i)? & 0x1f) as usize } else { *c.get(i)? as usize };
        i += 1;
        for _ in 0..n {
            let l = u16::from_be_bytes([*c.get(i)?, *c.get(i + 1)?]) as usize;
            let nal = c.get(i + 2..i + 2 + l)?;
            out.extend_from_slice(&START);
            out.extend_from_slice(nal);
            i += 2 + l;
        }
    }
    Some((nal_len, out))
}

fn wire_err(e: WireError) -> Error {
    match e {
        WireError::Protocol(_) => Error::TooBig,
        _ => Error::Decode,
    }
}

impl Openh264 {
    /// Wrap the socket the supervisor connected to `hospes-codec`; waits for its `READY`.
    pub fn new(track: &TrackInfo, max_w: u32, max_h: u32, sock: UnixStream) -> Result<Openh264> {
        let (nal_len, params) = if track.config.is_empty() {
            (0, Vec::new())
        } else {
            parse_avcc(&track.config).ok_or(Error::Unsupported)?
        };
        let c = Client::new(sock, max_w, max_h).map_err(|_| Error::Unsupported)?;
        Ok(Openh264 { c, nal_len, params, need_params: true, q: VecDeque::new(), cur: None, finished: false, au: Vec::new() })
    }

    /// One sample → one Annex-B access unit in `self.au`.
    fn annexb(&mut self, data: &[u8]) -> Result<()> {
        self.au.clear();
        if self.need_params {
            self.au.extend_from_slice(&self.params);
        }
        if self.nal_len == 0 {
            self.au.extend_from_slice(data);
            return Ok(());
        }
        let mut i = 0;
        while i < data.len() {
            let l = data.get(i..i + self.nal_len).ok_or(Error::Decode)?.iter().fold(0usize, |a, &b| a << 8 | b as usize);
            let nal = data.get(i + self.nal_len..i + self.nal_len + l).ok_or(Error::Decode)?;
            self.au.extend_from_slice(&START);
            self.au.extend_from_slice(nal);
            i += self.nal_len + l;
        }
        Ok(())
    }

    fn push(q: &mut VecDeque<Owned>) -> impl FnMut(hospes_codec_wire::Picture<'_>) + '_ {
        move |p| {
            let mut data = Vec::with_capacity(p.y.len() + p.u.len() + p.v.len());
            data.extend_from_slice(p.y);
            data.extend_from_slice(p.u);
            data.extend_from_slice(p.v);
            q.push_back(Owned { pts: p.pts, w: p.width, h: p.height, data });
        }
    }
}

impl crate::VideoDecoder for Openh264 {
    fn send(&mut self, data: &[u8], pts_ns: i64) -> Result<bool> {
        if data.is_empty() {
            return Ok(true);
        }
        self.annexb(data)?;
        let au = std::mem::take(&mut self.au);
        // A non-zero status is OpenH264 reporting concealment or a missing reference; the
        // picture (if any) still comes, as under the reference host.
        let r = self.c.decode(&au, pts_ns, &mut Self::push(&mut self.q));
        self.au = au;
        r.map_err(wire_err)?;
        self.need_params = false;
        self.finished = false;
        Ok(true)
    }

    fn receive(&mut self) -> Result<Option<Picture<'_>>> {
        self.cur = self.q.pop_front();
        let Some(o) = &self.cur else { return Ok(None) };
        let (w, h) = (o.w as usize, o.h as usize);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let (y, rest) = o.data.split_at(w * h);
        let (u, v) = rest.split_at(cw * ch);
        Ok(Some(Picture {
            width: o.w,
            height: o.h,
            bit_depth: 8,
            layout: Layout::I420,
            planes: [y, u, v],
            strides: [w, cw, cw],
            pts_ns: o.pts,
            color: Color::default(),
        }))
    }

    fn flush(&mut self) {
        self.q.clear();
        self.cur = None;
        let _ = self.c.reset();
        self.need_params = true;
        self.finished = false;
    }

    fn finish(&mut self) {
        if !self.finished {
            self.finished = true;
            let _ = self.c.drain(&mut Self::push(&mut self.q));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn avcc_to_annexb() {
        // version 1, profile 66, compat 0xc0, level 30, 0xff (4-byte lengths), 1 SPS, 1 PPS
        let c = [1, 66, 0xc0, 30, 0xff, 0xe1, 0, 3, 0x67, 1, 2, 1, 0, 2, 0x68, 9];
        let (n, p) = parse_avcc(&c).unwrap();
        assert_eq!(n, 4);
        assert_eq!(p, [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 9]);
        assert!(parse_avcc(&c[..9]).is_none());
    }
}
