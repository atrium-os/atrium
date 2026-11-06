//! The one place this crate calls C. Each wrapper owns its decoder handle,
//! checks every pointer and size the shim hands back before making a slice
//! of it, and frees the handle on drop.
#![allow(unsafe_code)]

use std::ffi::c_void;
use std::os::raw::c_int;

use crate::{Error, Layout, Picture, Result};

#[repr(C)]
struct RawPicture {
    width: u32,
    height: u32,
    bit_depth: u32,
    layout: u32,
    plane: [*const u8; 3],
    stride: [isize; 3],
    pts: i64,
}

impl Default for RawPicture {
    fn default() -> Self {
        RawPicture { width: 0, height: 0, bit_depth: 0, layout: 0, plane: [std::ptr::null(); 3], stride: [0; 3], pts: 0 }
    }
}

const OK: c_int = 0;
const AGAIN: c_int = 1;
const GOT: c_int = 1;
const TOO_BIG: c_int = -2;
const NOMEM: c_int = -3;

extern "C" {
    fn atr_dav1d_new(threads: c_int, max_pixels: u32) -> *mut c_void;
    fn atr_dav1d_send(d: *mut c_void, buf: *const u8, len: usize, pts: i64) -> c_int;
    fn atr_dav1d_get(d: *mut c_void, out: *mut RawPicture) -> c_int;
    fn atr_dav1d_flush(d: *mut c_void);
    fn atr_dav1d_free(d: *mut c_void);

    fn atr_vpx_new(vp9: c_int, threads: c_int, max_w: u32, max_h: u32) -> *mut c_void;
    fn atr_vpx_send(d: *mut c_void, buf: *const u8, len: usize, pts: i64) -> c_int;
    fn atr_vpx_get(d: *mut c_void, out: *mut RawPicture) -> c_int;
    fn atr_vpx_free(d: *mut c_void);

    fn atr_opus_new(head: *const u8, len: usize, channels: *mut c_int) -> *mut c_void;
    fn atr_opus_decode(d: *mut c_void, pkt: *const u8, len: usize, out: *mut f32, cap: c_int) -> c_int;
    fn atr_opus_reset(d: *mut c_void);
    fn atr_opus_free(d: *mut c_void);

    fn atr_vorbis_new(hdrs: *const u8, len: usize, rate: *mut c_int, channels: *mut c_int) -> *mut c_void;
    fn atr_vorbis_decode(d: *mut c_void, pkt: *const u8, len: usize, out: *mut f32, cap: c_int) -> c_int;
    fn atr_vorbis_reset(d: *mut c_void);
    fn atr_vorbis_free(d: *mut c_void);
}

fn err(r: c_int) -> Error {
    match r {
        TOO_BIG => Error::TooBig,
        NOMEM => Error::NoMemory,
        _ => Error::Decode,
    }
}

/// Turn the shim's picture into borrowed slices, refusing anything whose
/// sizes don't add up.
///
/// # Safety
/// `raw` must come from a successful get on a live decoder, and the
/// returned picture must not outlive the next call on that decoder (the
/// lifetime ties it to the `&mut` borrow of the wrapper).
unsafe fn picture<'a>(raw: &RawPicture, max_w: u32, max_h: u32) -> Result<Picture<'a>> {
    let layout = match raw.layout {
        0 => Layout::I400,
        1 => Layout::I420,
        2 => Layout::I422,
        3 => Layout::I444,
        _ => return Err(Error::Decode),
    };
    let (w, h) = (raw.width, raw.height);
    if w == 0 || h == 0 || w > max_w || h > max_h || !matches!(raw.bit_depth, 8 | 10 | 12) {
        return Err(Error::TooBig);
    }
    let bps = if raw.bit_depth > 8 { 2 } else { 1 };
    let (cw, ch) = layout.chroma_size(w, h);
    let planes = if layout == Layout::I400 { 1 } else { 3 };
    let mut out: [&'a [u8]; 3] = [&[], &[], &[]];
    let mut strides = [0usize; 3];
    for i in 0..planes {
        let (pw, ph) = if i == 0 { (w, h) } else { (cw, ch) };
        let row = pw as usize * bps;
        let stride = raw.stride[i];
        if raw.plane[i].is_null() || stride < row as isize {
            return Err(Error::Decode);
        }
        let len = (stride as usize).checked_mul(ph as usize - 1).and_then(|x| x.checked_add(row)).ok_or(Error::Decode)?;
        // SAFETY: the decoder owns a plane of at least `stride * rows` bytes
        // (both libraries allocate whole strided rows), valid until its next
        // call; `len` never exceeds that.
        out[i] = unsafe { std::slice::from_raw_parts(raw.plane[i], len) };
        strides[i] = stride as usize;
    }
    Ok(Picture { width: w, height: h, bit_depth: raw.bit_depth as u8, layout, planes: out, strides, pts_ns: raw.pts })
}

pub struct Dav1d {
    h: *mut c_void,
    max: (u32, u32),
}

// SAFETY: a dav1d context is used from one thread at a time (the wrapper
// needs `&mut self`); dav1d's own worker threads are internal to it.
unsafe impl Send for Dav1d {}

impl Dav1d {
    pub fn new(threads: u32, max_w: u32, max_h: u32) -> Result<Self> {
        // SAFETY: plain values in, a handle or null out.
        let h = unsafe { atr_dav1d_new(threads.min(64) as c_int, max_w.saturating_mul(max_h)) };
        if h.is_null() {
            return Err(Error::NoMemory);
        }
        Ok(Dav1d { h, max: (max_w, max_h) })
    }
}

impl Drop for Dav1d {
    fn drop(&mut self) {
        // SAFETY: the handle is live and freed once.
        unsafe { atr_dav1d_free(self.h) }
    }
}

impl crate::VideoDecoder for Dav1d {
    fn send(&mut self, data: &[u8], pts_ns: i64) -> Result<bool> {
        if data.is_empty() {
            return Ok(true);
        }
        // SAFETY: data is a live slice; the shim copies it.
        match unsafe { atr_dav1d_send(self.h, data.as_ptr(), data.len(), pts_ns) } {
            OK => Ok(true),
            AGAIN => Ok(false),
            r => Err(err(r)),
        }
    }
    fn receive(&mut self) -> Result<Option<Picture<'_>>> {
        let mut raw = RawPicture::default();
        // SAFETY: raw is a valid out-pointer.
        match unsafe { atr_dav1d_get(self.h, &mut raw) } {
            GOT => unsafe { picture(&raw, self.max.0, self.max.1) }.map(Some),
            OK => Ok(None),
            r => Err(err(r)),
        }
    }
    fn flush(&mut self) {
        // SAFETY: live handle.
        unsafe { atr_dav1d_flush(self.h) }
    }
}

pub struct Vpx {
    h: *mut c_void,
    vp9: bool,
    threads: u32,
    max: (u32, u32),
}

// SAFETY: as for Dav1d.
unsafe impl Send for Vpx {}

impl Vpx {
    pub fn new(vp9: bool, threads: u32, max_w: u32, max_h: u32) -> Result<Self> {
        // SAFETY: plain values in.
        let h = unsafe { atr_vpx_new(vp9 as c_int, threads.clamp(1, 64) as c_int, max_w, max_h) };
        if h.is_null() {
            return Err(Error::NoMemory);
        }
        Ok(Vpx { h, vp9, threads, max: (max_w, max_h) })
    }
}

impl Drop for Vpx {
    fn drop(&mut self) {
        // SAFETY: freed once.
        unsafe { atr_vpx_free(self.h) }
    }
}

impl crate::VideoDecoder for Vpx {
    fn send(&mut self, data: &[u8], pts_ns: i64) -> Result<bool> {
        if data.is_empty() {
            return Ok(true);
        }
        // SAFETY: live slice; libvpx reads it during the call only.
        match unsafe { atr_vpx_send(self.h, data.as_ptr(), data.len(), pts_ns) } {
            OK => Ok(true),
            AGAIN => Ok(false),
            r => Err(err(r)),
        }
    }
    fn receive(&mut self) -> Result<Option<Picture<'_>>> {
        let mut raw = RawPicture::default();
        // SAFETY: valid out-pointer.
        match unsafe { atr_vpx_get(self.h, &mut raw) } {
            GOT => unsafe { picture(&raw, self.max.0, self.max.1) }.map(Some),
            OK => Ok(None),
            r => Err(err(r)),
        }
    }
    fn flush(&mut self) {
        // libvpx has no flush: start a fresh decoder.
        if let Ok(fresh) = Vpx::new(self.vp9, self.threads, self.max.0, self.max.1) {
            *self = fresh;
        }
    }
}

/// Most frames one packet can decode to: 120 ms at 48 kHz for Opus,
/// a long block for Vorbis.
const MAX_PACKET_FRAMES: usize = 8192;

pub struct Opus {
    h: *mut c_void,
    channels: u16,
}

// SAFETY: as for Dav1d.
unsafe impl Send for Opus {}

impl Opus {
    pub fn new(head: &[u8]) -> Result<Self> {
        let mut ch: c_int = 0;
        // SAFETY: live slice; the shim validates it.
        let h = unsafe { atr_opus_new(head.as_ptr(), head.len(), &mut ch) };
        if h.is_null() || !(1..=8).contains(&ch) {
            return Err(Error::Unsupported);
        }
        Ok(Opus { h, channels: ch as u16 })
    }
    pub fn channels(&self) -> u16 {
        self.channels
    }
    pub fn decode(&mut self, pkt: &[u8], out: &mut Vec<f32>) -> Result<usize> {
        decode_into(out, self.channels, |p, cap| unsafe { atr_opus_decode(self.h, pkt.as_ptr(), pkt.len(), p, cap) })
    }
    pub fn reset(&mut self) {
        // SAFETY: live handle.
        unsafe { atr_opus_reset(self.h) }
    }
}

impl Drop for Opus {
    fn drop(&mut self) {
        // SAFETY: freed once.
        unsafe { atr_opus_free(self.h) }
    }
}

pub struct Vorbis {
    h: *mut c_void,
    rate: u32,
    channels: u16,
}

// SAFETY: as for Dav1d.
unsafe impl Send for Vorbis {}

impl Vorbis {
    pub fn new(headers: &[u8]) -> Result<Self> {
        let (mut rate, mut ch): (c_int, c_int) = (0, 0);
        // SAFETY: live slice; the shim checks the lengths in it.
        let h = unsafe { atr_vorbis_new(headers.as_ptr(), headers.len(), &mut rate, &mut ch) };
        if h.is_null() {
            return Err(Error::Unsupported);
        }
        Ok(Vorbis { h, rate: rate as u32, channels: ch as u16 })
    }
    pub fn rate(&self) -> u32 {
        self.rate
    }
    pub fn channels(&self) -> u16 {
        self.channels
    }
    pub fn decode(&mut self, pkt: &[u8], out: &mut Vec<f32>) -> Result<usize> {
        if pkt.is_empty() {
            return Ok(0);
        }
        decode_into(out, self.channels, |p, cap| unsafe { atr_vorbis_decode(self.h, pkt.as_ptr(), pkt.len(), p, cap) })
    }
    pub fn reset(&mut self) {
        // SAFETY: live handle.
        unsafe { atr_vorbis_reset(self.h) }
    }
}

impl Drop for Vorbis {
    fn drop(&mut self) {
        // SAFETY: freed once.
        unsafe { atr_vorbis_free(self.h) }
    }
}

/// Grow `out` by room for one packet, let the decoder write into it, and
/// keep only what it wrote.
fn decode_into(out: &mut Vec<f32>, channels: u16, f: impl FnOnce(*mut f32, c_int) -> c_int) -> Result<usize> {
    let start = out.len();
    let room = MAX_PACKET_FRAMES * channels as usize;
    out.resize(start + room, 0.0);
    let n = f(out[start..].as_mut_ptr(), MAX_PACKET_FRAMES as c_int);
    if n < 0 {
        out.truncate(start);
        return Err(Error::Decode);
    }
    let n = (n as usize).min(MAX_PACKET_FRAMES);
    out.truncate(start + n * channels as usize);
    Ok(n)
}
