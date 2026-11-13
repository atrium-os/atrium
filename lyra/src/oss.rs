//! OSS `/dev/dsp` output sink — the path-A device interface
//! (`docs/spec/atrium-lyra-architecture.md` §4.1).
//!
//! On FreeBSD the audio stack *is* OSS: `sound(4)` exposes `/dev/dsp*` through
//! the OSS v4 ioctls in `<sys/soundcard.h>`. Lyra opens it in **bit-perfect**
//! mode — vchans and feeder conversion disabled at bring-up (`dev.pcm.N.bitperfect=1`
//! / `hw.snd.maxautovchans=0`), and the stream opened at the exact hardware
//! rate/format so no in-kernel resampling fires — so the kernel just DMAs Lyra's
//! already-mixed buffer to the codec. Lyra still owns the mix and resamples once.
//!
//! The key method is [`OssSink::played_frames`]: `SNDCTL_DSP_GETOPTR` returns the
//! total bytes the DMA engine has consumed, i.e. the device's **cumulative
//! consumed-frame count** — the hardware clock the sink deadline (§3) and the
//! measured-drift resampler (§4) anchor to. (OSS gives an approximation of the
//! DMA position; the native HDA driver, path B, gives the exact frame-interrupt
//! timestamp later.)
//!
//! Builds on any unix; off FreeBSD `/dev/dsp` is absent and `open` fails cleanly,
//! exactly like the lane shim.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

/* FreeBSD ioctl encodings (sys/soundcard.h):
 *   _IOWR('P', n, int)  control ioctls (write-back the granted value)
 *   _IOR ('P', n, T)    query ioctls. 'P' = 0x50, IOCPARM mask = 13 bits. */
const IOC_OUT: u64 = 0x4000_0000;
const IOC_IN: u64 = 0x8000_0000;
const fn iowr(num: u8, len: usize) -> u64 {
    (IOC_IN | IOC_OUT) | (((len & 0x1fff) as u64) << 16) | ((b'P' as u64) << 8) | num as u64
}
const fn ior(num: u8, len: usize) -> u64 {
    IOC_OUT | (((len & 0x1fff) as u64) << 16) | ((b'P' as u64) << 8) | num as u64
}
const SNDCTL_DSP_SPEED: u64 = iowr(2, 4);
const SNDCTL_DSP_SETFMT: u64 = iowr(5, 4);
const SNDCTL_DSP_CHANNELS: u64 = iowr(6, 4);
const SNDCTL_DSP_SETFRAGMENT: u64 = iowr(10, 4);
const SNDCTL_DSP_GETOPTR: u64 = ior(18, std::mem::size_of::<CountInfo>());
const SNDCTL_DSP_GETODELAY: u64 = ior(23, 4);
const SNDCTL_DSP_GETERROR: u64 = ior(25, std::mem::size_of::<AudioErrInfo>());
const SNDCTL_DSP_SYNC: u64 = 0x2000_0000 | ((b'P' as u64) << 8) | 1; // _IO('P', 1)
/// `SNDCTL_AUDIOINFO`: `_IOWR('X', 7, oss_audioinfo)`, 1180 bytes on FreeBSD
/// (checked against sys/soundcard.h with the cross sysroot, 2026-11-13).
const OSS_AUDIOINFO_LEN: usize = 1180;
const SNDCTL_AUDIOINFO: u64 = (IOC_IN | IOC_OUT) | ((OSS_AUDIOINFO_LEN as u64) << 16) | ((b'X' as u64) << 8) | 7;
/// Mixer ioctls ('M'): the device mask, and the master level (0).
const SOUND_MIXER_READ_DEVMASK: u64 = IOC_OUT | (4 << 16) | ((b'M' as u64) << 8) | 0xfe;
const SOUND_MIXER_VOLUME: u64 = 0;
const SOUND_MIXER_WRITE_VOLUME: u64 = (IOC_IN | IOC_OUT) | (4 << 16) | ((b'M' as u64) << 8) | SOUND_MIXER_VOLUME;
const SOUND_MASK_VOLUME: i32 = 1 << SOUND_MIXER_VOLUME;

/// Mirror of `audio_errinfo` (sys/soundcard.h): 8 ints, 2 longs, filler[16].
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct AudioErrInfo {
    play_underruns: i32,
    rec_overruns: i32,
    play_ptradjust: u32,
    rec_ptradjust: u32,
    play_errorcount: i32,
    rec_errorcount: i32,
    play_lasterror: i32,
    rec_lasterror: i32,
    play_errorparm: i64,
    rec_errorparm: i64,
    filler: [i32; 16],
}

const AFMT_S16_NE: i32 = 0x0000_0010; // AFMT_S16_LE on little-endian (aarch64)
const AFMT_S32_NE: i32 = 0x0000_1000; // AFMT_S32_LE on little-endian
const AFMT_S24_NE: i32 = 0x0001_0000; // AFMT_S24_LE: packed, 3 bytes a sample

/// The sample format the device took. Lyra mixes to S16; a device whose only
/// bit-perfect format is wider gets each sample widened exactly, the same
/// 16-bit word in the top bits, so still bit-perfect:
/// - S32_LE (`s << 16`): USB Audio 2 DACs such as the ESS ES9281 in an
///   EarMen Sparrow offer it alone;
/// - S24_LE, packed 3 bytes (`s << 8`): an AudioQuest DragonFly Red offers it
///   alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    S16,
    S24,
    S32,
}

impl Format {
    fn bytes(self) -> u32 {
        match self { Format::S16 => 2, Format::S24 => 3, Format::S32 => 4 }
    }
    fn afmt(self) -> i32 {
        match self { Format::S16 => AFMT_S16_NE, Format::S24 => AFMT_S24_NE, Format::S32 => AFMT_S32_NE }
    }
    /// The OSS format the device granted, if Lyra can feed it.
    fn from_afmt(afmt: i32) -> Option<Self> {
        match afmt {
            AFMT_S16_NE => Some(Format::S16),
            AFMT_S24_NE => Some(Format::S24),
            AFMT_S32_NE => Some(Format::S32),
            _ => None,
        }
    }
}

impl Format {
    /// Significant bits of a sample.
    pub fn bits(self) -> u32 {
        match self { Format::S16 => 16, Format::S24 => 24, Format::S32 => 32 }
    }
}

/// Quantize one mix sample (full scale ±1.0) to a `bits`-bit integer: the
/// exact inverse of an integer source's `s / 2^(bits-1)`, so a sample that
/// came in as a `bits`-bit (or narrower) integer goes out as the same
/// integer, bit for bit. Rounded, then clamped to the integer range.
#[inline]
pub fn quantize(x: f32, bits: u32) -> i32 {
    let scale = (1u64 << (bits - 1)) as f64;
    let v = (x as f64 * scale).round();
    v.clamp(-scale, scale - 1.0) as i32
}

/// Mix samples as `fmt`'s little-endian bytes, into `out` (cleared first).
/// f32 holds 24 significant bits, so S24 and S32 carry a 24-bit source
/// exactly; S32's low 8 bits are then zero.
pub fn encode_f32(samples: &[f32], fmt: Format, out: &mut Vec<u8>) {
    out.clear();
    out.reserve(samples.len() * fmt.bytes() as usize);
    for &x in samples {
        let x = if x.is_finite() { x } else { 0.0 };
        let v = quantize(x, fmt.bits());
        match fmt {
            Format::S16 => out.extend_from_slice(&(v as i16).to_le_bytes()),
            Format::S24 => out.extend_from_slice(&v.to_le_bytes()[..3]),
            Format::S32 => out.extend_from_slice(&v.to_le_bytes()),
        }
    }
}

/// S16 samples as `fmt`'s little-endian bytes, widened exactly, into `out`
/// (cleared first).
pub fn encode_s16(samples: &[i16], fmt: Format, out: &mut Vec<u8>) {
    out.clear();
    out.reserve(samples.len() * fmt.bytes() as usize);
    for &s in samples {
        match fmt {
            Format::S16 => out.extend_from_slice(&s.to_le_bytes()),
            Format::S24 => out.extend_from_slice(&((s as i32) << 8).to_le_bytes()[..3]),
            Format::S32 => out.extend_from_slice(&((s as i32) << 16).to_le_bytes()),
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CountInfo {
    bytes: i32,  // total bytes processed by the DMA engine (monotonic)
    blocks: i32, // fragment transitions since last call
    ptr: i32,    // current DMA pointer within the buffer
}

/// A bit-perfect OSS playback sink. Fed signed 16-bit native-endian PCM;
/// writes S16, or S32 when that is the device's format (see [`Format`]).
pub struct OssSink {
    fd: OwnedFd,
    rate_hz: u32,
    channels: u32,
    format: Format,
    frame_bytes: u32, // channels * format bytes
    wide: std::cell::RefCell<Vec<u8>>, // encode staging, reused
}

impl OssSink {
    /// Open `/dev/dsp` for playback at the exact `rate_hz`/`channels`, with
    /// `nfrags` fragments of `frag_frames` each. `nfrags` is the **latency knob**:
    /// total buffer = `nfrags · frag_frames` frames. A small buffer is low
    /// latency but only survives load if the feed thread is punctual — which is
    /// what the deadline lane provides; a large buffer hides scheduling jitter at
    /// the cost of latency (why consumer stacks default high).
    pub fn open(rate_hz: u32, channels: u32, frag_frames: u32, nfrags: u32) -> io::Result<Self> {
        Self::open_as(rate_hz, channels, frag_frames, nfrags, Format::S16)
    }

    /// As [`OssSink::open`], asking for `want`: the mix engine asks for S32,
    /// so a bit-perfect device answers with its own depth and a converting
    /// one keeps 24 bits of the mix. Feed it with [`OssSink::write_f32`].
    pub fn open_as(rate_hz: u32, channels: u32, frag_frames: u32, nfrags: u32, want: Format) -> io::Result<Self> {
        let fd = unsafe { libc::open(c"/dev/dsp".as_ptr(), libc::O_WRONLY) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut sink = OssSink {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
            rate_hz,
            channels,
            format: Format::S16,
            frame_bytes: channels * 2,
            wide: std::cell::RefCell::new(Vec::new()),
        };
        // fragment first (latency), then format, channels, rate. Each ioctl
        // writes back the granted value; we require the device accept ours
        // exactly (bit-perfect — no silent feeder conversion).
        let frag_bytes = (frag_frames * channels * 2).max(64);
        // SETFRAGMENT arg: (max_count << 16) | log2(bytes). max_count bounds the
        // total buffer = the latency; the device may grant fewer.
        let sz_sel = (31 - (frag_bytes.max(1)).leading_zeros()) as i32;
        sink.set(SNDCTL_DSP_SETFRAGMENT, ((nfrags as i32 & 0x7fff) << 16) | sz_sel)?;
        // Ask for `want`. A bit-perfect device answers with the format it has;
        // take S16, S24 or S32, refuse anything else. Without bit-perfect the
        // kernel converts whatever it is given.
        let got = sink.set(SNDCTL_DSP_SETFMT, want.afmt())?;
        sink.format = Format::from_afmt(got).ok_or_else(|| io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("device coerced format: asked {:#x}, got {got:#x} (not S16, S24 or S32)", want.afmt()),
        ))?;
        if sink.format != Format::S16 {
            sink.require(SNDCTL_DSP_SETFMT, sink.format.afmt(), "format")?;
            // The fragment was sized in S16 bytes: ask again at twice the
            // bytes (a power of two; for S24 a little more than needed), so
            // the buffer holds at least the same frames (the same latency).
            sink.set(SNDCTL_DSP_SETFRAGMENT, ((nfrags as i32 & 0x7fff) << 16) | (sz_sel + 1))?;
        }
        sink.frame_bytes = channels * sink.format.bytes();
        sink.require(SNDCTL_DSP_CHANNELS, channels as i32, "channels")?;
        sink.require(SNDCTL_DSP_SPEED, rate_hz as i32, "rate")?;
        Ok(sink)
    }

    fn set(&self, req: u64, mut val: i32) -> io::Result<i32> {
        let rc = unsafe { libc::ioctl(self.fd.as_raw_fd(), req, &mut val) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(val)
    }

    /// Set a parameter and require the device grant it exactly (bit-perfect).
    fn require(&self, req: u64, want: i32, what: &str) -> io::Result<()> {
        let got = self.set(req, want)?;
        if got != want {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("device coerced {what}: asked {want}, got {got} (not bit-perfect)"),
            ));
        }
        Ok(())
    }

    pub fn rate_hz(&self) -> u32 {
        self.rate_hz
    }
    pub fn channels(&self) -> u32 {
        self.channels
    }
    pub fn format(&self) -> Format {
        self.format
    }

    /// The card's name as sound(4) gives it (e.g. "Auris EarMen Sparrow"),
    /// for keeping per-output settings: the default unit can be any card.
    pub fn card_name(&self) -> Option<String> {
        let mut info = [0u8; OSS_AUDIOINFO_LEN];
        info[..4].copy_from_slice(&(-1i32).to_ne_bytes()); // dev -1: this device
        let rc = unsafe { libc::ioctl(self.fd.as_raw_fd(), SNDCTL_AUDIOINFO, info.as_mut_ptr()) };
        if rc != 0 {
            return None;
        }
        let name = &info[4..68];
        let end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
        let s = String::from_utf8_lossy(&name[..end]).trim().to_string();
        (!s.is_empty()).then_some(s)
    }

    /// Write interleaved S16 frames (widened for an S24 or S32 device);
    /// blocks until the kernel buffer accepts them.
    pub fn write_i16(&self, samples: &[i16]) -> io::Result<()> {
        if self.format != Format::S16 {
            let mut wide = self.wide.borrow_mut();
            encode_s16(samples, self.format, &mut wide);
            return self.write_bytes(&wide);
        }
        let bytes = unsafe {
            std::slice::from_raw_parts(samples.as_ptr() as *const u8, std::mem::size_of_val(samples))
        };
        self.write_bytes(bytes)
    }

    /// Write interleaved mix frames (full scale ±1.0), quantized exactly to
    /// the device's format ([`encode_f32`]); blocks like [`OssSink::write_i16`].
    pub fn write_f32(&self, samples: &[f32]) -> io::Result<()> {
        let mut buf = self.wide.borrow_mut();
        encode_f32(samples, self.format, &mut buf);
        self.write_bytes(&buf)
    }

    fn write_bytes(&self, bytes: &[u8]) -> io::Result<()> {
        let mut off = 0;
        while off < bytes.len() {
            let n = unsafe {
                libc::write(
                    self.fd.as_raw_fd(),
                    bytes[off..].as_ptr() as *const libc::c_void,
                    bytes.len() - off,
                )
            };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            off += n as usize;
        }
        Ok(())
    }

    /// The device's **cumulative consumed-frame count** — the hardware clock
    /// (`SNDCTL_DSP_GETOPTR.bytes / frame_bytes`). Anchors the sink deadline (§3)
    /// and the measured-drift resampler (§4): `actual_rate = Δframes / Δmonotonic`.
    pub fn played_frames(&self) -> io::Result<u64> {
        let mut ci = CountInfo::default();
        let rc =
            unsafe { libc::ioctl(self.fd.as_raw_fd(), SNDCTL_DSP_GETOPTR, &mut ci) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(ci.bytes as u64 / self.frame_bytes as u64)
    }

    /// Total play underruns since the last call (`SNDCTL_DSP_GETERROR`) — the
    /// device's own count of times it ran dry. The objective glitch metric: a
    /// lane-sponsored feed thread holds this at 0 under load; a plain timeshare
    /// one does not. (GETERROR is consume-on-read in OSS v4.)
    pub fn play_underruns(&self) -> io::Result<u32> {
        let mut ei = AudioErrInfo::default();
        let rc =
            unsafe { libc::ioctl(self.fd.as_raw_fd(), SNDCTL_DSP_GETERROR, &mut ei) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(ei.play_underruns as u32)
    }

    /// Block until every queued frame has played, then reset (`SNDCTL_DSP_SYNC`).
    /// Called at shutdown so closing the fd does not abort the buffer mid-play
    /// (the source of the stop-time crackle).
    pub fn drain(&self) -> io::Result<()> {
        let rc = unsafe { libc::ioctl(self.fd.as_raw_fd(), SNDCTL_DSP_SYNC) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Frames still queued in the DMA pipeline ahead of the codec
    /// (`SNDCTL_DSP_GETODELAY`) — the headroom before underrun.
    pub fn delay_frames(&self) -> io::Result<u64> {
        let mut bytes: i32 = 0;
        let rc =
            unsafe { libc::ioctl(self.fd.as_raw_fd(), SNDCTL_DSP_GETODELAY, &mut bytes) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(bytes as u64 / self.frame_bytes as u64)
    }
}

/// The device's own master level (`/dev/mixer`, the default unit's mixer,
/// like `/dev/dsp`): a hardware control, such as a USB DAC's feature unit or
/// a codec's output amplifier. sound(4) emulates only `pcm` in software
/// (SOFTPCMVOL), never `vol`, so a `vol` control is the hardware's.
pub struct HwVolume {
    fd: OwnedFd,
}

impl HwVolume {
    /// The mixer, if it has a master level.
    pub fn open() -> Option<HwVolume> {
        let fd = unsafe { libc::open(c"/dev/mixer".as_ptr(), libc::O_RDWR) };
        if fd < 0 {
            return None;
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut mask: i32 = 0;
        let rc = unsafe { libc::ioctl(fd.as_raw_fd(), SOUND_MIXER_READ_DEVMASK, &mut mask) };
        (rc == 0 && mask & SOUND_MASK_VOLUME != 0).then_some(HwVolume { fd })
    }

    /// Set the level, 0 to 1 (both channels).
    pub fn set(&self, volume: f32) -> io::Result<()> {
        let mut v = level(volume);
        let rc = unsafe { libc::ioctl(self.fd.as_raw_fd(), SOUND_MIXER_WRITE_VOLUME, &mut v) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// An OSS mixer level for `volume` 0 to 1: percent, left in the low byte and
/// right in the next. Not a number is silence.
pub fn level(volume: f32) -> i32 {
    let v = if volume.is_finite() { volume.clamp(0.0, 1.0) } else { 0.0 };
    let pct = (v * 100.0).round() as i32;
    pct | (pct << 8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_encodings_match_soundcard_h() {
        // verified against sys/sys/soundcard.h: 'P'=0x50, IOC_INOUT=0xC0000000.
        assert_eq!(SNDCTL_DSP_SPEED, 0xC004_5002);
        assert_eq!(SNDCTL_DSP_SETFMT, 0xC004_5005);
        assert_eq!(SNDCTL_DSP_CHANNELS, 0xC004_5006);
        assert_eq!(SNDCTL_DSP_SETFRAGMENT, 0xC004_500A);
        assert_eq!(SNDCTL_DSP_GETOPTR, 0x400C_5012); // _IOR('P',18,count_info=12)
        assert_eq!(SNDCTL_DSP_GETODELAY, 0x4004_5017); // _IOR('P',23,int=4)
    }

    #[test]
    fn formats_and_widening() {
        assert_eq!(Format::from_afmt(AFMT_S16_NE), Some(Format::S16));
        assert_eq!(Format::from_afmt(AFMT_S24_NE), Some(Format::S24));
        assert_eq!(Format::from_afmt(AFMT_S32_NE), Some(Format::S32));
        assert_eq!(Format::from_afmt(0x0000_0008), None); // AFMT_U8
        let src = [0i16, 1, -1, i16::MAX, i16::MIN];
        let mut out = Vec::new();
        encode_s16(&src, Format::S32, &mut out);
        let w: Vec<i32> = out.chunks(4).map(|c| i32::from_le_bytes(c.try_into().unwrap())).collect();
        assert_eq!(w, vec![0, 0x1_0000, -0x1_0000, 0x7fff_0000, i32::MIN]);
        encode_s16(&src, Format::S24, &mut out);
        assert_eq!(out.len(), 15);
        // Sign-extend each packed 24-bit word; its top 16 bits are the sample.
        let w: Vec<i32> = out.chunks(3).map(|c| i32::from_le_bytes([0, c[0], c[1], c[2]]) >> 8).collect();
        assert_eq!(w, vec![0, 0x100, -0x100, 0x7f_ff00, -0x80_0000]);
        assert!(w.iter().zip(src).all(|(w, s)| (w >> 8) as i16 == s));
        encode_s16(&src, Format::S16, &mut out);
        assert_eq!(out, src.iter().flat_map(|s| s.to_le_bytes()).collect::<Vec<u8>>());
    }

    #[test]
    fn mixer_encodings_and_levels() {
        // Checked against sys/soundcard.h with the cross sysroot.
        assert_eq!(SNDCTL_AUDIOINFO, 3_298_580_487);
        assert_eq!(SOUND_MIXER_READ_DEVMASK, 1_074_023_934);
        assert_eq!(SOUND_MIXER_WRITE_VOLUME, 3_221_507_328);
        assert_eq!(level(1.0), 100 | 100 << 8);
        assert_eq!(level(0.5), 50 | 50 << 8);
        assert_eq!(level(0.0), 0);
        assert_eq!(level(f32::NAN), 0);
        assert_eq!(level(7.0), 100 | 100 << 8);
    }

    #[test]
    fn integer_sources_come_out_bit_for_bit() {
        // A 16- or 24-bit source, as the app's f32 (s / 2^(bits-1)), leaves
        // as the same integer at its own depth and at every wider one.
        let mut out = Vec::new();
        let s16: Vec<i16> = vec![0, 1, -1, 12_345, -12_345, i16::MAX, i16::MIN];
        let f: Vec<f32> = s16.iter().map(|&s| s as f32 / 32_768.0).collect();
        encode_f32(&f, Format::S16, &mut out);
        assert_eq!(out, s16.iter().flat_map(|s| s.to_le_bytes()).collect::<Vec<u8>>());
        encode_f32(&f, Format::S32, &mut out);
        let w: Vec<i32> = out.chunks(4).map(|c| i32::from_le_bytes(c.try_into().unwrap())).collect();
        assert!(w.iter().zip(&s16).all(|(w, &s)| *w == (s as i32) << 16));
        let s24: Vec<i32> = vec![0, 1, -1, 4_660_901, -4_660_901, (1 << 23) - 1, -(1 << 23)];
        let f: Vec<f32> = s24.iter().map(|&s| s as f32 / 8_388_608.0).collect();
        encode_f32(&f, Format::S24, &mut out);
        let w: Vec<i32> = out.chunks(3).map(|c| i32::from_le_bytes([0, c[0], c[1], c[2]]) >> 8).collect();
        assert_eq!(w, s24);
        encode_f32(&f, Format::S32, &mut out);
        let w: Vec<i32> = out.chunks(4).map(|c| i32::from_le_bytes(c.try_into().unwrap())).collect();
        assert!(w.iter().zip(&s24).all(|(w, &s)| *w == s << 8));
        // Every 24-bit value, not just a few: f32 holds them all exactly.
        let all: Vec<f32> = (-(1i32 << 23)..(1 << 23)).step_by(257).map(|s| s as f32 / 8_388_608.0).collect();
        encode_f32(&all, Format::S24, &mut out);
        let back: Vec<i32> = out.chunks(3).map(|c| i32::from_le_bytes([0, c[0], c[1], c[2]]) >> 8).collect();
        assert!(back.iter().zip((-(1i32 << 23)..(1 << 23)).step_by(257)).all(|(a, b)| *a == b));
    }

    #[test]
    fn out_of_range_and_non_finite_samples_clamp() {
        let mut out = Vec::new();
        encode_f32(&[1.0, -1.0, 4.0, -4.0, f32::NAN, f32::INFINITY], Format::S16, &mut out);
        let w: Vec<i16> = out.chunks(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
        assert_eq!(w, vec![i16::MAX, i16::MIN, i16::MAX, i16::MIN, 0, 0]);
        encode_f32(&[1.0, -1.0], Format::S32, &mut out);
        let w: Vec<i32> = out.chunks(4).map(|c| i32::from_le_bytes(c.try_into().unwrap())).collect();
        assert_eq!(w, vec![i32::MAX, i32::MIN]);
    }

    #[test]
    fn open_degrades_cleanly_without_a_device() {
        // off FreeBSD (or with no /dev/dsp) open fails cleanly — lyrad then runs
        // without a hardware sink, exactly like the lane shim.
        assert!(OssSink::open(48_000, 2, 128, 3).is_err());
    }
}
