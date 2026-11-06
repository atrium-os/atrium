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
    /// `protected`: kept out of anything that captures the screen.
    fn place(&self, window: u32, layer: u32, rect: Option<[i32; 4]>, protected: bool);
    /// A frame for the layer, `w` x `h` (its rect's size): `fill` writes
    /// it, BGRA words, into the slice it is given (`w * h` long).
    fn frame(&self, window: u32, layer: u32, w: u32, h: u32, seq: u32, present_ns: u64, fill: &mut dyn FnMut(&mut [u32]));
    /// The layer's content runs at `hz_milli` (0: not said).
    fn rate(&self, window: u32, layer: u32, hz_milli: u32);
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
    pub colour: Colour,
    /// Kept out of screen capture (fresco-video.md §6).
    pub protected: bool,
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

/// SDR reference white, cd/m² (BT.2408): HDR is shown with this as the
/// desktop's white.
pub const SDR_WHITE_NITS: f32 = 203.0;

/// Entries of the transfer table, over a signal of 0..=1.
const LUT_IN: usize = 1024;
/// Entries of the sRGB encode table, over linear 0..=1.
const LUT_OUT: usize = 4096;

/// How a layer's samples become desktop sRGB (fresco-video.md §5): the
/// matrix and range turn Y'CbCr into R'G'B'; for SDR BT.709 that is the
/// picture as it is, else each channel goes through its transfer to linear
/// light (PQ and HLG tone-mapped to SDR white with the BT.2390 EETF), from
/// BT.2020 to BT.709 primaries, and back to sRGB.
#[derive(Clone)]
pub struct Colour {
    kr: f32,
    kb: f32,
    full: bool,
    /// R'G'B' is shown as it is (SDR, BT.709).
    direct: bool,
    /// Signal (0..=1) to linear light, 1.0 = SDR white.
    lin: Vec<f32>,
    /// BT.2020 to BT.709, in linear light.
    gamut: Option<[[f32; 3]; 3]>,
    /// Linear light (0..=1) to an sRGB code value.
    srgb: Vec<u8>,
}

impl Default for Colour {
    fn default() -> Self {
        use fresco_protocol::video::{matrix, primaries, range, transfer};
        Colour::new(matrix::BT709, range::LIMITED, primaries::BT709, transfer::BT1886, None)
    }
}

/// SMPTE ST 2084: signal to cd/m², and back.
pub fn pq_eotf(e: f32) -> f32 {
    let (m1, m2, c1, c2, c3) = (0.159_301_76_f32, 78.84375_f32, 0.8359375_f32, 18.851_562_f32, 18.6875_f32);
    let p = e.clamp(0.0, 1.0).powf(1.0 / m2);
    10000.0 * ((p - c1).max(0.0) / (c2 - c3 * p)).powf(1.0 / m1)
}

pub fn pq_oetf(nits: f32) -> f32 {
    let (m1, m2, c1, c2, c3) = (0.159_301_76_f32, 78.84375_f32, 0.8359375_f32, 18.851_562_f32, 18.6875_f32);
    let y = (nits / 10000.0).clamp(0.0, 1.0).powf(m1);
    ((c1 + c2 * y) / (1.0 + c3 * y)).powf(m2)
}

/// HLG (BT.2100) signal to scene light, 0..=1.
fn hlg_inverse_oetf(e: f32) -> f32 {
    let (a, b, c) = (0.178_832_77_f32, 0.284_668_92_f32, 0.559_910_7_f32);
    let e = e.clamp(0.0, 1.0);
    if e <= 0.5 { e * e / 3.0 } else { (((e - c) / a).exp() + b) / 12.0 }
}

/// The BT.2390 EETF: a PQ signal from content peaking at `src_peak`
/// cd/m² onto a display peaking at `dst_peak` (black at 0), as a PQ
/// signal.
pub fn eetf(e: f32, src_peak: f32, dst_peak: f32) -> f32 {
    let lw = pq_oetf(src_peak);
    if src_peak <= dst_peak || lw <= 0.0 {
        return e;
    }
    let e1 = (e / lw).clamp(0.0, 1.0);
    let max_lum = pq_oetf(dst_peak) / lw;
    let ks = 1.5 * max_lum - 0.5;
    let e2 = if e1 < ks {
        e1
    } else {
        let t = (e1 - ks) / (1.0 - ks);
        let (t2, t3) = (t * t, t * t * t);
        (2.0 * t3 - 3.0 * t2 + 1.0) * ks + (t3 - 2.0 * t2 + t) * (1.0 - ks) + (-2.0 * t3 + 3.0 * t2) * max_lum
    };
    e2 * lw
}

fn srgb_encode(l: f32) -> f32 {
    if l <= 0.003_130_8 { 12.92 * l } else { 1.055 * l.powf(1.0 / 2.4) - 0.055 }
}

impl Colour {
    /// The colour a layer gives (`fresco_protocol::video` constants);
    /// values it doesn't know read as the default.
    pub fn new(matrix_: u8, range_: u8, primaries_: u8, transfer_: u8, hdr: Option<fresco_protocol::video::Mastering>) -> Colour {
        use fresco_protocol::video::{matrix, primaries, range, transfer};
        let (kr, kb) = match matrix_ {
            matrix::BT601 => (0.299, 0.114),
            matrix::BT2020 => (0.2627, 0.0593),
            _ => (0.2126, 0.0722),
        };
        let wide = primaries_ == primaries::BT2020;
        let transfer_ = match transfer_ {
            transfer::SRGB | transfer::PQ | transfer::HLG => transfer_,
            _ => transfer::BT1886,
        };
        let sdr = matches!(transfer_, transfer::SRGB | transfer::BT1886);
        // The content's peak: what its metadata says, else 1000 cd/m².
        let peak = hdr
            .map(|m| match (m.max_cll, m.max_luminance) {
                (c, l) if c > 0 && (l == 0 || c < l) => f32::from(c),
                (_, l) if l > 0 => f32::from(l),
                _ => 1000.0,
            })
            .unwrap_or(1000.0)
            .clamp(SDR_WHITE_NITS, 10000.0);
        let lin = (0..=LUT_IN)
            .map(|i| {
                let e = i as f32 / LUT_IN as f32;
                match transfer_ {
                    // SDR video is shown on the sRGB desktop as it is
                    // (the `direct` path), so BT.1886 decodes as sRGB too:
                    // a grey stays the same grey whatever the primaries.
                    transfer::SRGB | transfer::BT1886 => if e <= 0.04045 { e / 12.92 } else { ((e + 0.055) / 1.055).powf(2.4) },
                    transfer::PQ => pq_eotf(eetf(e, peak, SDR_WHITE_NITS)) / SDR_WHITE_NITS,
                    transfer::HLG => {
                        // The OOTF for a 1000 cd/m² display (system gamma
                        // 1.2), per channel, then onto SDR white.
                        let nits = 1000.0 * hlg_inverse_oetf(e).powf(1.2);
                        pq_eotf(eetf(pq_oetf(nits), 1000.0, SDR_WHITE_NITS)) / SDR_WHITE_NITS
                    }
                    _ => unreachable!(),
                }
            })
            .collect();
        // BT.2020 to BT.709 (BT.2087), linear light.
        let gamut = wide.then_some([
            [1.6605, -0.5876, -0.0728],
            [-0.1246, 1.1329, -0.0083],
            [-0.0182, -0.1006, 1.1187],
        ]);
        let srgb = (0..=LUT_OUT).map(|i| (srgb_encode(i as f32 / LUT_OUT as f32) * 255.0).round() as u8).collect();
        Colour {
            kr,
            kb,
            full: range_ == range::FULL,
            direct: sdr && !wide,
            lin,
            gamut,
            srgb,
        }
    }

    /// One pixel from Y'CbCr samples in 8-bit code values (P010 scaled to
    /// them), as a BGRA word.
    #[inline]
    fn yuv(&self, y: f32, cb: f32, cr: f32) -> u32 {
        let (y, cb, cr) = if self.full {
            (y / 255.0, (cb - 128.0) / 255.0, (cr - 128.0) / 255.0)
        } else {
            ((y - 16.0) / 219.0, (cb - 128.0) / 224.0, (cr - 128.0) / 224.0)
        };
        let kg = 1.0 - self.kr - self.kb;
        let r = y + 2.0 * (1.0 - self.kr) * cr;
        let b = y + 2.0 * (1.0 - self.kb) * cb;
        let g = (y - self.kr * r - self.kb * b) / kg;
        self.rgb(r, g, b)
    }

    /// One pixel from R'G'B' (0..=1), as a BGRA word.
    #[inline]
    fn rgb(&self, r: f32, g: f32, b: f32) -> u32 {
        if self.direct {
            let c = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u32;
            return 0xff00_0000 | c(r) << 16 | c(g) << 8 | c(b);
        }
        let lin = |v: f32| {
            let x = v.clamp(0.0, 1.0) * LUT_IN as f32;
            let i = (x as usize).min(LUT_IN - 1);
            let f = x - i as f32;
            self.lin[i] * (1.0 - f) + self.lin[i + 1] * f
        };
        let mut l = [lin(r), lin(g), lin(b)];
        if let Some(m) = &self.gamut {
            let v = l;
            for (o, row) in l.iter_mut().zip(m) {
                *o = row[0] * v[0] + row[1] * v[1] + row[2] * v[2];
            }
        }
        let c = |v: f32| u32::from(self.srgb[(v.clamp(0.0, 1.0) * LUT_OUT as f32).round() as usize]);
        0xff00_0000 | c(l[0]) << 16 | c(l[1]) << 8 | c(l[2])
    }
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
pub fn convert(format_: u8, colour: &Colour, src: Shared, sw: u32, sh: u32, dw: u32, dh: u32, crop: [u32; 4], out: &mut [u32]) {
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
                    out[oy * cw + ox] = colour.yuv(yv, cb, cr);
                }
            }
        }
        format::RGBA8 => {
            let read = |i: usize| src.get(i) as f32;
            for oy in 0..chh {
                let y = ((cy0 + oy) as f32 + 0.5) * sy - 0.5;
                for ox in 0..cw {
                    let x = ((cx0 + ox) as f32 + 0.5) * sx - 0.5;
                    let ch = |c: usize| bilinear(&read, sw, sh, sw * 4, 4, c, x, y) / 255.0;
                    out[oy * cw + ox] = colour.rgb(ch(0), ch(1), ch(2));
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
        let c = Colour::default();
        assert_eq!(c.yuv(16.0, 128.0, 128.0), 0xff00_0000);
        assert_eq!(c.yuv(235.0, 128.0, 128.0), 0xffff_ffff);
        let g = c.yuv(126.0, 128.0, 128.0) & 0xff;
        assert!((127..=129).contains(&g));
    }

    fn rgb(p: u32) -> (u32, u32, u32) {
        ((p >> 16) & 0xff, (p >> 8) & 0xff, p & 0xff)
    }

    #[test]
    fn full_range_and_bt601() {
        use fresco_protocol::video::{matrix, primaries, range, transfer};
        let c = Colour::new(matrix::BT601, range::FULL, primaries::BT709, transfer::BT1886, None);
        assert_eq!(c.yuv(0.0, 128.0, 128.0), 0xff00_0000);
        assert_eq!(c.yuv(255.0, 128.0, 128.0), 0xffff_ffff);
        // BT.601 full-range red: Y 76, Cb 85, Cr 255.
        let (r, g, b) = rgb(c.yuv(76.0, 85.0, 255.0));
        assert!(r > 250 && g < 6 && b < 6, "{r} {g} {b}");
    }

    #[test]
    fn pq_goes_through_the_eetf() {
        // The curve and its inverse agree; reference white is ~0.58.
        for n in [0.1f32, 1.0, 100.0, 203.0, 1000.0, 4000.0] {
            assert!((pq_eotf(pq_oetf(n)) - n).abs() / n < 1e-3, "{n}");
        }
        assert!((pq_oetf(203.0) - 0.5807).abs() < 1e-3);
        // Below the knee unchanged, the content's peak onto the display's,
        // and never brighter.
        let (src, dst) = (1000.0, SDR_WHITE_NITS);
        assert_eq!(eetf(0.3, src, dst), 0.3);
        assert!((pq_eotf(eetf(pq_oetf(src), src, dst)) - dst).abs() < 1.0);
        let mut last = 0.0;
        for i in 0..=100 {
            let e = i as f32 / 100.0 * pq_oetf(src);
            let o = eetf(e, src, dst);
            assert!(o <= e + 1e-6 && o >= last - 1e-6);
            last = o;
        }
    }

    #[test]
    fn hdr_white_and_peaks_land_in_sdr() {
        use fresco_protocol::video::{matrix, primaries, range, transfer, Mastering};
        let m = Mastering { max_luminance: 1000, max_cll: 1000, ..Default::default() };
        let c = Colour::new(matrix::BT2020, range::FULL, primaries::BT2020, transfer::PQ, Some(m));
        // Full-range PQ white at the content's peak: SDR white.
        let (r, g, b) = rgb(c.yuv(255.0, 128.0, 128.0));
        assert!(r >= 254 && g >= 254 && b >= 254, "{r} {g} {b}");
        // Black stays black.
        assert_eq!(c.yuv(0.0, 128.0, 128.0), 0xff00_0000);
        // 100 cd/m² grey, just past the knee for a 1000 cd/m² master:
        // about half of SDR white in linear light, a little less (sRGB 186
        // uncompressed).
        let e = pq_oetf(100.0) * 255.0;
        let (r, _, _) = rgb(c.yuv(e, 128.0, 128.0));
        assert!((165..=186).contains(&r), "{r}");
        // HLG peak white also lands on SDR white.
        let h = Colour::new(matrix::BT2020, range::FULL, primaries::BT2020, transfer::HLG, None);
        let (r, _, _) = rgb(h.yuv(255.0, 128.0, 128.0));
        assert!(r >= 254, "{r}");
    }

    #[test]
    fn bt2020_green_is_clipped_into_bt709() {
        use fresco_protocol::video::{matrix, primaries, range, transfer};
        let c = Colour::new(matrix::BT2020, range::FULL, primaries::BT2020, transfer::BT1886, None);
        // Pure BT.2020 green: R'G'B' (0, 1, 0).
        let (r, g, b) = rgb(c.rgb(0.0, 1.0, 0.0));
        assert!(r == 0 && g == 255 && b == 0, "{r} {g} {b}");
        // A BT.709 grey is still grey.
        let (r, g, b) = rgb(c.rgb(0.5, 0.5, 0.5));
        assert!(r.abs_diff(g) <= 1 && g.abs_diff(b) <= 1 && (127..=129).contains(&r), "{r} {g} {b}");
    }

    #[test]
    fn nv12_red_converts_and_scales() {
        // BT.709 limited red: Y 63, Cb 102, Cr 240.
        let (w, h) = (4u32, 2u32);
        let mut src = vec![63u8; (w * h) as usize];
        src.extend([102u8, 240].repeat((w * h / 4) as usize));
        let mut out = vec![0u32; 8 * 4];
        convert(format::NV12, &Colour::default(), Shared::bytes(&src), w, h, 8, 4, [0, 0, 8, 4], &mut out);
        for p in out {
            let (r, g, b) = ((p >> 16) & 0xff, (p >> 8) & 0xff, p & 0xff);
            assert!(r > 245 && g < 10 && b < 10, "{p:#x}");
        }
    }

    #[test]
    fn a_short_buffer_reads_as_black_not_a_crash() {
        let mut out = vec![1u32; 16];
        convert(format::P010, &Colour::default(), Shared::bytes(&[1, 2, 3]), 4, 4, 4, 4, [0, 0, 4, 4], &mut out);
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
        convert(format::RGBA8, &Colour::default(), Shared::bytes(&src), 4, 2, 8, 4, [0, 0, 8, 4], &mut whole);
        let mut part = vec![0u32; 6];
        convert(format::RGBA8, &Colour::default(), Shared::bytes(&src), 4, 2, 8, 4, [5, 1, 3, 2], &mut part);
        for y in 0..2 {
            for x in 0..3 {
                assert_eq!(part[y * 3 + x], whole[(y + 1) * 8 + x + 5]);
            }
        }
        assert_eq!(visible([-2, 3, 10, 10], 6, 8), Some([0, 3, 6, 5]));
        assert_eq!(visible([7, 0, 4, 4], 6, 8), None);
    }
}
