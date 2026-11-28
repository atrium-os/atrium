//! Atrium's external-device adapter. The portable A2DP profile lives in Vinculum.
use lyra_protocol::client::{Announce, Device};
use lyra_protocol::device::{DeviceState, STATE_STREAMING};
use lyra_protocol::wire::OutputKind;
use std::time::{Duration, Instant};
use std::{io, os::fd::AsRawFd};
use vinculum_audio::{Channels, Stream};

pub struct Output<C> {
    device: Device,
    stream: Stream<C>,
    pcm: Vec<f32>,
    epoch: Instant,
    played: u64,
    underruns: u32,
    downstream_delay: u32,
}

impl<C: Channels> Output<C> {
    /// Announce only after the sink accepts Start. downstream_delay is the
    /// estimate beyond the current packet, in negotiated-rate sample frames.
    pub fn connect(
        socket: &str,
        id: u64,
        name: String,
        kind: OutputKind,
        stream: Stream<C>,
        downstream_delay: u32,
    ) -> io::Result<Self> {
        let frames = stream.pcm_frames();
        let device = Device::connect(
            socket,
            &Announce {
                persistent_id: id,
                name,
                kind,
                rate: stream.rate(),
                channels: 2,
                hw_volume: false,
                initial_delay_frames: downstream_delay.saturating_add(frames as u32),
            },
        )?;
        Ok(Self {
            device,
            stream,
            pcm: vec![0.0; frames * 2],
            epoch: Instant::now(),
            played: 0,
            underruns: 0,
            downstream_delay,
        })
    }

    /// Each read is on the system clock. Pad a short read with silence and
    /// report only the frames actually consumed to Lyra.
    pub fn pump(&mut self) -> io::Result<()> {
        let rate = self.stream.rate();
        let frames = self.stream.pcm_frames() as u64;
        let duration = |n: u64| {
            Duration::from_nanos(((u128::from(n) * 1_000_000_000) / u128::from(rate)) as u64)
        };
        let deadline = self.epoch + duration(self.played);
        let now = Instant::now();
        if deadline > now {
            std::thread::sleep(deadline - now);
        } else if now - deadline > duration(frames) {
            self.epoch = now;
            self.played = 0;
        }

        // Choragus owns the route. Drop the radio session if it disappears,
        // even though the ring mapping remains readable after its owner exits.
        let mut fd = libc::pollfd {
            fd: self.device.socket().as_raw_fd(),
            events: 0,
            revents: 0,
        };
        // SAFETY: fd is a single initialized pollfd with a live socket.
        if unsafe { libc::poll(&mut fd, 1, 0) } < 0 {
            return Err(io::Error::last_os_error());
        }
        if fd.revents != 0 {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "Choragus disconnected",
            ));
        }
        self.pcm.fill(0.0);
        let read = self.device.ring.read(&mut self.pcm);
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: ts is initialized and writable for the duration of the call.
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if read < frames {
            self.underruns = self.underruns.saturating_add(1);
        }
        self.stream.send(&self.pcm)?;
        self.device.page.publish(&DeviceState {
            state: STATE_STREAMING,
            rate,
            underruns: self.underruns,
            delay_frames: self.downstream_delay.saturating_add(read as u32),
            at_ns: ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64,
            consumed: self.device.ring.consumed(),
        });
        self.played += frames;
        Ok(())
    }

    pub fn close(mut self) -> io::Result<()> {
        let close = self.stream.close();
        self.device.gone()?;
        close
    }
}
