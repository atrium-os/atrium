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

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CountInfo {
    bytes: i32,  // total bytes processed by the DMA engine (monotonic)
    blocks: i32, // fragment transitions since last call
    ptr: i32,    // current DMA pointer within the buffer
}

/// A bit-perfect OSS playback sink. Signed 16-bit native-endian PCM.
pub struct OssSink {
    fd: OwnedFd,
    rate_hz: u32,
    channels: u32,
    frame_bytes: u32, // channels * 2 (S16)
}

impl OssSink {
    /// Open `/dev/dsp` for playback at the exact `rate_hz`/`channels`, with a
    /// fragment size near `frag_frames` (the latency knob → the lane period).
    /// Bit-perfect mode is a bring-up sysctl, set outside this call.
    pub fn open(rate_hz: u32, channels: u32, frag_frames: u32) -> io::Result<Self> {
        let fd = unsafe { libc::open(c"/dev/dsp".as_ptr(), libc::O_WRONLY) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let sink = OssSink {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
            rate_hz,
            channels,
            frame_bytes: channels * 2,
        };
        // fragment first (latency), then format, channels, rate. Each ioctl
        // writes back the granted value; we require the device accept ours
        // exactly (bit-perfect — no silent feeder conversion).
        let frag_bytes = (frag_frames * channels * 2).max(64);
        // SETFRAGMENT arg: (count << 16) | log2(bytes). 0x7fff = "as many as fit".
        let sz_sel = (31 - (frag_bytes.max(1)).leading_zeros()) as i32;
        sink.set(SNDCTL_DSP_SETFRAGMENT, (0x7fff << 16) | sz_sel)?;
        sink.require(SNDCTL_DSP_SETFMT, AFMT_S16_NE, "format")?;
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

    /// Write interleaved S16 frames; blocks until the kernel buffer accepts them.
    pub fn write_i16(&self, samples: &[i16]) -> io::Result<()> {
        let bytes = unsafe {
            std::slice::from_raw_parts(samples.as_ptr() as *const u8, std::mem::size_of_val(samples))
        };
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
    fn open_degrades_cleanly_without_a_device() {
        // off FreeBSD (or with no /dev/dsp) open fails cleanly — lyrad then runs
        // without a hardware sink, exactly like the lane shim.
        assert!(OssSink::open(48_000, 2, 128).is_err());
    }
}
