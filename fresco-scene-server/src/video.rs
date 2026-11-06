//! Video layers (docs/spec/fresco-video.md): the frame buffers a client
//! fills, and their conversion to BGRA pixels.
//!
//! The frontend that serves the client owns the buffers and converts each
//! queued frame (in split mode that is the window's renderer, in its jail;
//! fresco-video.md §10, decision 6), then hands the pixels to whoever
//! composes ([`VideoSink`]): frescod's own video module in direct mode, or
//! the split broker in a renderer.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::OnceLock;

use fresco_protocol::video::format;

/// Where converted frames go.
pub trait VideoSink: Send + Sync {
    /// Layer `layer` of window `window` (the frontend's own window id)
    /// now shows at `rect` (`[x, y, w, h]` in the window's pixels, inside
    /// the window), or is hidden. Its next frame is that size.
    fn place(&self, window: u32, layer: u32, rect: Option<[i32; 4]>);
    /// A frame for the layer, `w` x `h` (its rect's size): `fill` writes
    /// it, BGRA words, into the slice it is given (`w * h` long).
    fn frame(&self, window: u32, layer: u32, w: u32, h: u32, seq: u32, present_ns: u64, fill: &mut dyn FnMut(&mut [u32]));
    /// The layer is gone.
    fn destroy(&self, window: u32, layer: u32);
}

/// Set once per process: frescod's video module, or the renderer's link
/// to the broker. Without one, video layers are refused.
pub static SINK: OnceLock<Box<dyn VideoSink>> = OnceLock::new();

/// One layer, as the frontend serving its client keeps it.
pub struct Layer {
    pub client: u8,
    pub window: u32,
    pub format: u8,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
    /// The buffers, mapped read-only.
    pub maps: Vec<memmap2::Mmap>,
    /// Where the client put it, in the window's pixels.
    pub rect: Option<[i32; 4]>,
    /// The part of it inside the window, as last told to the sink.
    pub shown: Option<[i32; 4]>,
}

/// `n` anonymous shared-memory buffers of `len` bytes, sealed against
/// resizing (so a client can't shrink one under our mapping), each with a
/// read-only mapping for us.
pub fn buffers(n: u32, len: u64) -> std::io::Result<(Vec<OwnedFd>, Vec<memmap2::Mmap>)> {
    let mut fds = Vec::new();
    let mut maps = Vec::new();
    for _ in 0..n {
        let name = c"fresco-video";
        // SAFETY: memfd_create with a NUL-terminated name; a negative
        // result is an error, else a new fd we own.
        let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: fd is fresh and ours.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: plain syscalls on our own fd.
        unsafe {
            if libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) != 0
                || libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
        }
        let f = std::fs::File::from(fd.try_clone()?);
        // SAFETY: the file can't shrink (sealed), so the mapping stays
        // valid; its bytes may change under us, which we only read as
        // samples, never as lengths or indices.
        let map = unsafe { memmap2::MmapOptions::new().len(len as usize).map(&f)? };
        fds.push(fd);
        maps.push(map);
    }
    Ok((fds, maps))
}

/// BT.709 limited range, 8-bit Y'CbCr to R'G'B' (the default until
/// layers carry their colour, V3).
#[inline]
fn yuv_to_bgra(y: f32, cb: f32, cr: f32) -> u32 {
    let y = (y - 16.0) * (255.0 / 219.0);
    let cb = (cb - 128.0) * (255.0 / 224.0);
    let cr = (cr - 128.0) * (255.0 / 224.0);
    let r = y + 1.5748 * cr;
    let g = y - 0.1873 * cb - 0.4681 * cr;
    let b = y + 1.8556 * cb;
    let c = |v: f32| v.round().clamp(0.0, 255.0) as u32;
    0xff00_0000 | c(r) << 16 | c(g) << 8 | c(b)
}

/// Sample plane `p` (`pw` x `ph` samples, `stride` samples a row,
/// `step` samples apart, `at` the first) bilinearly at (`x`, `y`).
#[inline]
fn bilinear(read: &dyn Fn(usize) -> f32, pw: usize, ph: usize, stride: usize, step: usize, at: usize, x: f32, y: f32) -> f32 {
    let x = x.clamp(0.0, (pw - 1) as f32);
    let y = y.clamp(0.0, (ph - 1) as f32);
    let (x0, y0) = (x as usize, y as usize);
    let (x1, y1) = ((x0 + 1).min(pw - 1), (y0 + 1).min(ph - 1));
    let (fx, fy) = (x - x0 as f32, y - y0 as f32);
    let s = |xx: usize, yy: usize| read(at + yy * stride + xx * step);
    let top = s(x0, y0) * (1.0 - fx) + s(x1, y0) * fx;
    let bot = s(x0, y1) * (1.0 - fx) + s(x1, y1) * fx;
    top * (1.0 - fy) + bot * fy
}

/// A buffer the client may be writing while we read it: read only with
/// volatile loads, so no Rust reference to its bytes is ever formed
/// (fresco-video.md §10), and out of range reads as 0.
#[derive(Clone, Copy)]
pub struct Shared {
    ptr: *const u8,
    len: usize,
}

impl Shared {
    /// A whole mapping.
    pub fn of(m: &memmap2::Mmap) -> Shared {
        Shared { ptr: m.as_ptr(), len: m.len() }
    }

    /// Bytes we own (tests, and copies already made).
    pub fn bytes(b: &[u8]) -> Shared {
        Shared { ptr: b.as_ptr(), len: b.len() }
    }

    #[inline]
    fn get(&self, i: usize) -> u8 {
        if i < self.len {
            // SAFETY: in bounds of a live mapping (or slice) the caller
            // holds for the call; a volatile load of a u8 is atomic enough
            // that a concurrent write only changes the value read.
            unsafe { std::ptr::read_volatile(self.ptr.add(i)) }
        } else {
            0
        }
    }
}

/// Convert a `format` buffer of `sw` x `sh` in `src`, scaled bilinearly
/// to `dw` x `dh`, and write the part `crop` (`[x, y, w, h]` within that
/// scaled picture) as BGRA words to `out` (`w * h` long). Nothing in `src`
/// is trusted: it is only ever read as samples.
pub fn convert(format_: u8, src: Shared, sw: u32, sh: u32, dw: u32, dh: u32, crop: [u32; 4], out: &mut [u32]) {
    let (sw, sh, dw, dh) = (sw as usize, sh as usize, dw as usize, dh as usize);
    let [cx0, cy0, cw, chh] = crop.map(|v| v as usize);
    if dw == 0 || dh == 0 || sw == 0 || sh == 0 || cw == 0 || chh == 0 || out.len() < cw * chh || cx0 + cw > dw || cy0 + chh > dh {
        return;
    }
    // Source position of destination pixel centres.
    let sx = sw as f32 / dw as f32;
    let sy = sh as f32 / dh as f32;
    match format_ {
        format::NV12 | format::P010 => {
            let wide = format_ == format::P010;
            let read8 = |i: usize| src.get(i) as f32;
            let read16 = |i: usize| {
                let lo = src.get(2 * i) as u16;
                let hi = src.get(2 * i + 1) as u16;
                // 10 bits in the top of 16, scaled to 8-bit code values.
                ((lo | hi << 8) >> 6) as f32 / 4.0
            };
            let read: &dyn Fn(usize) -> f32 = if wide { &read16 } else { &read8 };
            let (cw2, ch2) = ((sw / 2).max(1), (sh / 2).max(1));
            let uv = sw * sh;
            for oy in 0..chh {
                let py = (cy0 + oy) as f32 + 0.5;
                let y = py * sy - 0.5;
                let cy = py * sy / 2.0 - 0.5;
                for ox in 0..cw {
                    let px = (cx0 + ox) as f32 + 0.5;
                    let x = px * sx - 0.5;
                    let cx = px * sx / 2.0 - 0.5;
                    let yv = bilinear(read, sw, sh, sw, 1, 0, x, y);
                    let cb = bilinear(read, cw2, ch2, sw, 2, uv, cx, cy);
                    let cr = bilinear(read, cw2, ch2, sw, 2, uv + 1, cx, cy);
                    out[oy * cw + ox] = yuv_to_bgra(yv, cb, cr);
                }
            }
        }
        format::RGBA8 => {
            let read = |i: usize| src.get(i) as f32;
            for oy in 0..chh {
                let y = ((cy0 + oy) as f32 + 0.5) * sy - 0.5;
                for ox in 0..cw {
                    let x = ((cx0 + ox) as f32 + 0.5) * sx - 0.5;
                    let ch = |c: usize| bilinear(&read, sw, sh, sw * 4, 4, c, x, y).round().clamp(0.0, 255.0) as u32;
                    out[oy * cw + ox] = 0xff00_0000 | ch(0) << 16 | ch(1) << 8 | ch(2);
                }
            }
        }
        _ => out[..cw * chh].fill(0xff00_0000),
    }
}

/// The part of `rect` (`[x, y, w, h]`) inside a `w` x `h` window, the same
/// shape; `None` when nothing of it is.
pub fn visible(rect: [i32; 4], w: u32, h: u32) -> Option<[i32; 4]> {
    let x0 = rect[0].max(0);
    let y0 = rect[1].max(0);
    let x1 = rect[0].saturating_add(rect[2]).min(w.min(i32::MAX as u32) as i32);
    let y1 = rect[1].saturating_add(rect[3]).min(h.min(i32::MAX as u32) as i32);
    (x0 < x1 && y0 < y1).then(|| [x0, y0, x1 - x0, y1 - y0])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limited_range_black_white_and_grey() {
        assert_eq!(yuv_to_bgra(16.0, 128.0, 128.0), 0xff00_0000);
        assert_eq!(yuv_to_bgra(235.0, 128.0, 128.0), 0xffff_ffff);
        let g = yuv_to_bgra(126.0, 128.0, 128.0) & 0xff;
        assert!((127..=129).contains(&g));
    }

    #[test]
    fn nv12_red_converts_and_scales() {
        // BT.709 limited red: Y 63, Cb 102, Cr 240.
        let (w, h) = (4u32, 2u32);
        let mut src = vec![63u8; (w * h) as usize];
        src.extend([102u8, 240].repeat((w * h / 4) as usize));
        let mut out = vec![0u32; 8 * 4];
        convert(format::NV12, Shared::bytes(&src), w, h, 8, 4, [0, 0, 8, 4], &mut out);
        for p in out {
            let (r, g, b) = ((p >> 16) & 0xff, (p >> 8) & 0xff, p & 0xff);
            assert!(r > 245 && g < 10 && b < 10, "{p:#x}");
        }
    }

    #[test]
    fn a_short_buffer_reads_as_black_not_a_crash() {
        let mut out = vec![1u32; 16];
        convert(format::P010, Shared::bytes(&[1, 2, 3]), 4, 4, 4, 4, [0, 0, 4, 4], &mut out);
        assert!(out.iter().all(|&p| p >> 24 == 0xff));
    }

    #[test]
    fn a_crop_is_that_part_of_the_scaled_picture() {
        // Left half red, right half blue, RGBA 4x2 scaled to 8x4.
        let mut src = Vec::new();
        for _ in 0..2 {
            for x in 0..4 {
                src.extend(if x < 2 { [255, 0, 0, 255] } else { [0, 0, 255, 255] });
            }
        }
        let mut whole = vec![0u32; 32];
        convert(format::RGBA8, Shared::bytes(&src), 4, 2, 8, 4, [0, 0, 8, 4], &mut whole);
        let mut part = vec![0u32; 6];
        convert(format::RGBA8, Shared::bytes(&src), 4, 2, 8, 4, [5, 1, 3, 2], &mut part);
        for y in 0..2 {
            for x in 0..3 {
                assert_eq!(part[y * 3 + x], whole[(y + 1) * 8 + x + 5]);
            }
        }
        assert_eq!(visible([-2, 3, 10, 10], 6, 8), Some([0, 3, 6, 5]));
        assert_eq!(visible([7, 0, 4, 4], 6, 8), None);
    }
}
