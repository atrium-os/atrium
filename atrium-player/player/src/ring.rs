//! The PCM ring: decoded audio from the worker (the only writer) to the
//! supervisor (the only reader), in shared memory the supervisor creates.
//!
//! Layout: `write` u64 at 0, `read` u64 at 8 (both in frames since the
//! ring began), `capacity` u32 at 16 and `channels` u32 at 20, interleaved
//! f32 samples from byte 64. The supervisor reads `write` from a process it
//! presumes compromised, so it checks `read <= write <= read + capacity`
//! every time and treats anything else as the worker failing.
use std::io;
use std::os::fd::OwnedFd;
use std::sync::atomic::Ordering;

use crate::sys::{shm_anon, Map};

const DATA: usize = 64;

pub struct PcmRing {
    map: Map,
    cap: u64,
    ch: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Corrupt;

impl PcmRing {
    fn bytes_for(cap: u32, ch: u16) -> usize {
        DATA + cap as usize * ch as usize * 4
    }

    /// Supervisor: a new ring of `capacity` frames. Returns the descriptor
    /// to hand the worker.
    pub fn create(capacity: u32, channels: u16) -> io::Result<(PcmRing, OwnedFd)> {
        assert!(capacity > 0 && (1..=8).contains(&channels));
        let len = Self::bytes_for(capacity, channels);
        let fd = shm_anon(len)?;
        let mut map = Map::new(&fd, len)?;
        let b = map.bytes_mut();
        b[16..20].copy_from_slice(&capacity.to_le_bytes());
        b[20..24].copy_from_slice(&(channels as u32).to_le_bytes());
        Ok((PcmRing { map, cap: capacity as u64, ch: channels as usize }, fd))
    }

    /// Worker: map the ring the supervisor made.
    pub fn open(fd: &OwnedFd, capacity: u32, channels: u16) -> io::Result<PcmRing> {
        let map = Map::new(fd, Self::bytes_for(capacity, channels))?;
        Ok(PcmRing { map, cap: capacity as u64, ch: channels as usize })
    }

    pub fn channels(&self) -> usize {
        self.ch
    }

    fn w(&self) -> u64 {
        self.map.atomic_u64(0).load(Ordering::Acquire)
    }
    fn r(&self) -> u64 {
        self.map.atomic_u64(8).load(Ordering::Acquire)
    }

    /// Worker: frames written so far (the next frame's ring position).
    pub fn write_pos(&self) -> u64 {
        self.w()
    }

    /// Worker: room for this many frames.
    pub fn free(&self) -> u64 {
        self.cap.saturating_sub(self.w().saturating_sub(self.r()))
    }

    /// Worker: append whole frames from `pcm` (interleaved), as many as fit.
    pub fn write(&mut self, pcm: &[f32]) -> usize {
        let frames = (pcm.len() / self.ch).min(self.free() as usize);
        let w = self.w();
        self.copy(w, |slot, i| *slot = pcm[i].to_le_bytes(), frames);
        self.map.atomic_u64(0).store(w + frames as u64, Ordering::Release);
        frames
    }

    fn copy(&mut self, at: u64, mut f: impl FnMut(&mut [u8; 4], usize), frames: usize) {
        let (cap, ch) = (self.cap as usize, self.ch);
        let b = &mut self.map.bytes_mut()[DATA..];
        for i in 0..frames * ch {
            let pos = ((at as usize % cap) * ch + i) % (cap * ch);
            let slot: &mut [u8; 4] = (&mut b[pos * 4..pos * 4 + 4]).try_into().unwrap();
            f(slot, i);
        }
    }

    /// Supervisor: frames ready to read, or `Corrupt` if the worker's
    /// counter is out of range.
    pub fn readable(&self) -> Result<u64, Corrupt> {
        let (w, r) = (self.w(), self.r());
        if w < r || w - r > self.cap {
            return Err(Corrupt);
        }
        Ok(w - r)
    }

    pub fn read_pos(&self) -> u64 {
        self.r()
    }

    /// Supervisor: read up to `out.len() / channels` frames.
    pub fn read(&mut self, out: &mut [f32]) -> Result<usize, Corrupt> {
        let frames = (out.len() / self.ch).min(self.readable()? as usize);
        let r = self.r();
        let (cap, ch) = (self.cap as usize, self.ch);
        let b = &self.map.bytes()[DATA..];
        for (i, o) in out.iter_mut().take(frames * ch).enumerate() {
            let pos = ((r as usize % cap) * ch + i) % (cap * ch);
            *o = f32::from_le_bytes(b[pos * 4..pos * 4 + 4].try_into().unwrap());
            // Hostile samples: a NaN or huge value must not reach the mixer.
            if !o.is_finite() {
                *o = 0.0;
            }
            *o = o.clamp(-1.0, 1.0);
        }
        self.map.atomic_u64(8).store(r + frames as u64, Ordering::Release);
        Ok(frames)
    }

    /// Supervisor: drop everything before ring position `pos` (a seek).
    pub fn skip_to(&mut self, pos: u64) -> Result<(), Corrupt> {
        let (w, r) = (self.w(), self.r());
        if pos < r || pos > w {
            return Err(Corrupt);
        }
        self.map.atomic_u64(8).store(pos, Ordering::Release);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_and_checks() {
        let (mut rd, fd) = PcmRing::create(4, 2).unwrap();
        let mut wr = PcmRing::open(&fd, 4, 2).unwrap();
        assert_eq!(wr.write(&[0.1, 0.2, 0.3, 0.4, 0.5, 0.6]), 3);
        assert_eq!(wr.write(&[0.7, 0.8, 0.9, 1.0]), 1);
        let mut out = [0f32; 4];
        assert_eq!(rd.read(&mut out).unwrap(), 2);
        assert_eq!(out, [0.1, 0.2, 0.3, 0.4]);
        assert_eq!(wr.write(&[-0.1, -0.2, -0.3, -0.4, 9.0, 9.0]), 2);
        let mut out = [0f32; 8];
        assert_eq!(rd.read(&mut out).unwrap(), 4);
        assert_eq!(out, [0.5, 0.6, 0.7, 0.8, -0.1, -0.2, -0.3, -0.4]);
        // A worker that lies about its counter is caught.
        wr.map.atomic_u64(0).store(1000, Ordering::Release);
        assert_eq!(rd.readable(), Err(Corrupt));
    }
}
