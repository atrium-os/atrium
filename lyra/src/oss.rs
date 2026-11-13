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
    wide: std::cell::RefCell<Vec<u8>>, // S24/S32 staging, reused
}

impl OssSink {
    /// Open `/dev/dsp` for playback at the exact `rate_hz`/`channels`, with
    /// `nfrags` fragments of `frag_frames` each. `nfrags` is the **latency knob**:
    /// total buffer = `nfrags · frag_frames` frames. A small buffer is low
    /// latency but only survives load if the feed thread is punctual — which is
    /// what the deadline lane provides; a large buffer hides scheduling jitter at
    /// the cost of latency (why consumer stacks default high).
    pub fn open(rate_hz: u32, channels: u32, frag_frames: u32, nfrags: u32) -> io::Result<Self> {
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
        // Ask for S16. A bit-perfect device answers with the format it has;
        // take S24 or S32 (widened exactly), refuse anything else.
        let got = sink.set(SNDCTL_DSP_SETFMT, AFMT_S16_NE)?;
        sink.format = Format::from_afmt(got).ok_or_else(|| io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("device coerced format: asked {AFMT_S16_NE}, got {got:#x} (not S16, S24 or S32)"),
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
    fn open_degrades_cleanly_without_a_device() {
        // off FreeBSD (or with no /dev/dsp) open fails cleanly — lyrad then runs
        // without a hardware sink, exactly like the lane shim.
        assert!(OssSink::open(48_000, 2, 128, 3).is_err());
    }
}
