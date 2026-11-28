//! B5 gate source. Python carries opaque SDUs between this profile and
//! Bumble; the fixture below plays the other end of the audio-device socket.
use lyra_protocol::{
    device, fdpass,
    ring::Ring,
    wire::{self, FromDevice, OutputKind, ToDevice},
};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;
use std::time::Duration;
use vinculum_atrium_audio::Output;
use vinculum_audio::{Channels, Stream, MAX_SDU};

const PACKETS: usize = 48;

struct Bridge;
impl Bridge {
    fn write(&self, kind: u8, data: &[u8]) -> io::Result<()> {
        let mut out = io::stdout().lock();
        out.write_all(&[kind])?;
        out.write_all(
            &u16::try_from(data.len())
                .map_err(io::Error::other)?
                .to_be_bytes(),
        )?;
        out.write_all(data)?;
        out.flush()
    }

    fn word(&self) -> io::Result<u16> {
        let mut buf = [0; 2];
        io::stdin().read_exact(&mut buf)?;
        Ok(u16::from_be_bytes(buf))
    }
}
impl Channels for Bridge {
    fn exchange(&mut self, cmd: &[u8], reply: &mut [u8; MAX_SDU]) -> io::Result<usize> {
        self.write(1, cmd)?;
        let len = usize::from(self.word()?);
        let buf = reply
            .get_mut(..len)
            .ok_or_else(|| io::Error::other("oversized reply"))?;
        io::stdin().read_exact(buf)?;
        Ok(len)
    }
    fn open_media(&mut self) -> io::Result<u16> {
        self.write(2, &[])?;
        self.word()
    }
    fn close_media(&mut self) -> io::Result<()> {
        self.write(4, &[])?;
        if self.word()? != 0 {
            return Err(io::Error::other("media close failed"));
        }
        Ok(())
    }
    fn send_media(&mut self, packet: &[u8]) -> io::Result<()> {
        self.write(3, packet)
    }
}

fn fixture(listener: UnixListener, rate: u32, frames: usize) -> io::Result<()> {
    let (mut sock, _) = listener.accept()?;
    sock.set_read_timeout(Some(Duration::from_secs(5)))?;
    assert_eq!(
        wire::recv::<_, ToDevice>(&mut sock)?,
        ToDevice::Hello {
            version: wire::VERSION
        }
    );
    wire::send(&mut sock, &FromDevice::Welcome)?;
    let announce = wire::recv::<_, ToDevice>(&mut sock)?;
    assert_eq!(
        announce,
        ToDevice::Announce {
            persistent_id: 7,
            name: "B5 sink".into(),
            kind: OutputKind::Headphones,
            rate,
            channels: 2,
            hw_volume: false,
            initial_delay_frames: frames as u32
        }
    );
    let capacity = 32768;
    let (ring, ring_fd) = fixture_ring(capacity)?;
    #[cfg(not(target_os = "macos"))]
    let (page, page_fd) = device::create()?;
    #[cfg(target_os = "macos")]
    let (page, page_fd) = {
        let file = tempfile::tempfile()?;
        file.set_len(device::PAGE_LEN as u64)?;
        let fd = file.into();
        (device::DeviceReader::from_fd(&fd)?, fd)
    };

    let count = frames * PACKETS - frames / 2; // Exercise the short final read.
    let mut pcm = Vec::with_capacity(count * 2);
    for i in 0..count {
        for hz in [440.0, 880.0] {
            pcm.push(0.5 * (std::f32::consts::TAU * hz * i as f32 / rate as f32).sin());
        }
    }
    assert_eq!(ring.write(&pcm), count as u64);
    wire::send(
        &mut sock,
        &FromDevice::Ready {
            device: 1,
            ring_frames: capacity,
        },
    )?;
    fdpass::send_fd(sock.as_raw_fd(), ring_fd.as_raw_fd())?;
    fdpass::send_fd(sock.as_raw_fd(), page_fd.as_raw_fd())?;
    assert_eq!(wire::recv::<_, ToDevice>(&mut sock)?, ToDevice::Gone);
    let state = page
        .read()
        .ok_or_else(|| io::Error::other("unstable device page"))?;
    assert_eq!(state.rate, rate);
    assert_eq!(state.state, device::STATE_STREAMING);
    assert_eq!(state.consumed, count as u64);
    assert_eq!(state.delay_frames, (frames / 2) as u32);
    assert_eq!(state.underruns, 1);
    assert!(state.at_ns > 0);
    eprintln!(
        "Lyra device announced and removed; {} frames consumed, short read reported",
        state.consumed
    );
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn fixture_ring(capacity: u64) -> io::Result<(Ring, std::os::fd::OwnedFd)> {
    Ring::create_anon(capacity, 2, true)
}

#[cfg(target_os = "macos")]
fn fixture_ring(capacity: u64) -> io::Result<(Ring, std::os::fd::OwnedFd)> {
    // macOS cannot pass POSIX shm descriptors over SCM_RIGHTS. Back the
    // normal ring layout with an unlinked temporary file and pass that fd.
    let mut file = tempfile::tempfile()?;
    for word in [capacity, 2, 0, 0] {
        file.write_all(&word.to_ne_bytes())?;
    }
    file.set_len(32 + capacity * 2 * 4)?;
    Ok((Ring::from_fd(file.try_clone()?.into(), true)?, file.into()))
}

fn main() -> io::Result<()> {
    let stream = Stream::start(Bridge)?;
    let (rate, frames) = (stream.rate(), stream.pcm_frames());
    let tmp = tempfile::tempdir_in("/tmp")?;
    let path = tmp.path().join("audio.sock");
    let listener = UnixListener::bind(&path)?;
    let mixer = std::thread::spawn(move || fixture(listener, rate, frames));
    let mut output = Output::connect(
        path.to_str()
            .ok_or_else(|| io::Error::other("socket path"))?,
        7,
        "B5 sink".into(),
        OutputKind::Headphones,
        stream,
        0,
    )?;
    for _ in 0..PACKETS {
        output.pump()?;
    }
    output.close()?;
    mixer
        .join()
        .map_err(|_| io::Error::other("device fixture failed"))?
}
