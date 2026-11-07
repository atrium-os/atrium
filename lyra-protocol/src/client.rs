//! The app API (lyra-v1.md §5). An app connects to choragusd, opens streams,
//! writes interleaved `f32` (or `i16`) samples into each stream's ring, and
//! reads events. It never talks to lyrad.
//!
//! ```no_run
//! use lyra_protocol::client::Client;
//! use lyra_protocol::wire::{Role, StreamParams};
//! let mut c = Client::connect("/atrium/sockets/audio/audio.sock", None)?;
//! let s = c.open(StreamParams { role: Role::Media, rate: 48_000, channels: 2, buffer_ms: 100, label: "Video".into() })?;
//! s.write(&[0.0; 960]);   // prime the ring
//! c.start(&s)?;
//! # Ok::<(), std::io::Error>(())
//! ```

use crate::clock::{Clock, ClockReader};
use crate::fdpass::recv_fd;
use crate::ring::Ring;
use crate::device::DeviceWriter;
use crate::wire::{self, DenyCode, FromControl, FromDevice, FromMedia, MediaAction, OutputKind, SessionState, StreamParams, ToApp, ToChoragus, ToControl, ToDevice, ToMedia};
use std::collections::VecDeque;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

/// An open stream: the producer end of its ring.
pub struct Stream {
    pub id: u32,
    pub channels: u8,
    pub rate: u32,
    pub ring: Ring,
    /// When each frame is heard (lyra-v1.md §6).
    pub clock: ClockReader,
    /// lyrad's output latency when the stream opened (device queue + delays).
    pub latency_ns: u64,
}

impl Stream {
    /// Write interleaved frames; returns frames accepted (bounded by free space).
    pub fn write(&self, frames: &[f32]) -> u64 {
        self.ring.write(frames)
    }

    /// Write interleaved `i16` frames, converted to `f32`.
    pub fn write_i16(&self, frames: &[i16]) -> u64 {
        let f: Vec<f32> = frames.iter().map(|&s| s as f32 / 32768.0).collect();
        self.ring.write(&f)
    }

    /// The clock: `clock().heard_at(n)` is when stream frame `n` is heard, on
    /// `CLOCK_MONOTONIC`. Frame numbers count every frame written since the
    /// stream opened (a `Flushed` event says where they continue).
    pub fn clock(&self) -> Clock {
        self.clock.read()
    }

    /// Frames of free space in the ring.
    pub fn writable(&self) -> u64 {
        self.ring.writable()
    }
}

/// A connection to choragusd.
pub struct Client {
    sock: UnixStream,
    pub device_rate: u32,
    pub device_channels: u8,
    pub period_frames: u32,
    next_tag: u32,
    pending: VecDeque<ToApp>,
}

fn denied(code: DenyCode) -> io::Error {
    let kind = match code {
        DenyCode::NotGranted => io::ErrorKind::PermissionDenied,
        DenyCode::EngineUnavailable => io::ErrorKind::NotConnected,
        _ => io::ErrorKind::InvalidInput,
    };
    io::Error::new(kind, format!("denied: {code:?}"))
}

impl Client {
    /// Connect and say hello. `ticket`: a Limen attribution ticket, if this
    /// process is an embedded player.
    pub fn connect(path: &str, ticket: Option<[u8; 16]>) -> io::Result<Client> {
        let mut sock = UnixStream::connect(path)?;
        wire::send(&mut sock, &ToChoragus::Hello { version: wire::VERSION, ticket })?;
        match wire::recv::<_, ToApp>(&mut sock)? {
            ToApp::Welcome { device_rate, device_channels, period_frames } => Ok(Client {
                sock,
                device_rate,
                device_channels,
                period_frames,
                next_tag: 1,
                pending: VecDeque::new(),
            }),
            ToApp::Denied { code, .. } => Err(denied(code)),
            other => Err(io::Error::new(io::ErrorKind::InvalidData, format!("expected Welcome, got {other:?}"))),
        }
    }

    /// Open a stream (paused). Events that arrive meanwhile are kept for
    /// [`Client::next_event`].
    pub fn open(&mut self, params: StreamParams) -> io::Result<Stream> {
        let tag = self.next_tag;
        self.next_tag = self.next_tag.wrapping_add(1).max(1);
        let (rate, channels) = (params.rate, params.channels);
        wire::send(&mut self.sock, &ToChoragus::Open { tag, params })?;
        loop {
            match wire::recv::<_, ToApp>(&mut self.sock)? {
                ToApp::Opened { tag: t, stream, latency_ns, .. } if t == tag => {
                    let ring = Ring::from_fd(recv_fd(self.sock.as_raw_fd())?, true)?;
                    let clock = ClockReader::from_fd(recv_fd(self.sock.as_raw_fd())?)?;
                    return Ok(Stream { id: stream, channels, rate, ring, clock, latency_ns });
                }
                ToApp::Denied { tag: t, code } if t == tag => return Err(denied(code)),
                other => self.pending.push_back(other),
            }
        }
    }

    pub fn start(&mut self, s: &Stream) -> io::Result<()> {
        wire::send(&mut self.sock, &ToChoragus::Start { stream: s.id })
    }
    pub fn pause(&mut self, s: &Stream) -> io::Result<()> {
        wire::send(&mut self.sock, &ToChoragus::Pause { stream: s.id })
    }
    /// Drop what is queued; a `Flushed` event says where the next frame starts.
    pub fn flush(&mut self, s: &Stream) -> io::Result<()> {
        wire::send(&mut self.sock, &ToChoragus::Flush { stream: s.id })
    }
    /// A `Drained` event follows when the last written frame has been heard.
    pub fn drain(&mut self, s: &Stream) -> io::Result<()> {
        wire::send(&mut self.sock, &ToChoragus::Drain { stream: s.id })
    }
    pub fn set_volume(&mut self, s: &Stream, gain: f32) -> io::Result<()> {
        wire::send(&mut self.sock, &ToChoragus::SetVolume { stream: s.id, gain })
    }
    /// Publish this connection's media session (atrium-player-pip-keys.md
    /// §4.2). Actions the user asks for arrive as `ToApp::SessionAction`.
    pub fn session(&mut self, state: SessionState, title: &str, artist: &str, actions: u16, duration: Option<f64>, position: Option<f64>) -> io::Result<()> {
        let m = ToChoragus::Session { state, title: title.into(), artist: artist.into(), actions, duration, position };
        wire::send(&mut self.sock, &m)
    }

    pub fn close(&mut self, s: Stream) -> io::Result<()> {
        wire::send(&mut self.sock, &ToChoragus::Close { stream: s.id })
    }

    /// The next event (blocking).
    pub fn next_event(&mut self) -> io::Result<ToApp> {
        if let Some(e) = self.pending.pop_front() {
            return Ok(e);
        }
        wire::recv(&mut self.sock)
    }

    /// An event if one has arrived, without waiting. An error means the
    /// connection is gone (choragusd hung up: treat as `StreamLost`).
    pub fn try_event(&mut self) -> io::Result<Option<ToApp>> {
        if let Some(e) = self.pending.pop_front() {
            return Ok(Some(e));
        }
        let mut b = [0u8; 1];
        // SAFETY: a one-byte peek into a local buffer.
        let n = unsafe {
            libc::recv(self.sock.as_raw_fd(), b.as_mut_ptr() as *mut _, 1, libc::MSG_PEEK | libc::MSG_DONTWAIT)
        };
        let peeked = if n < 0 { Err(io::Error::last_os_error()) } else { Ok(n as usize) };
        match peeked {
            Ok(0) => Err(io::Error::new(io::ErrorKind::UnexpectedEof, "choragusd hung up")),
            // A frame has started: read all of it, blocking.
            Ok(_) => wire::recv(&mut self.sock).map(Some),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The socket, to poll for events.
    pub fn socket(&self) -> &UnixStream {
        &self.sock
    }
}

/// The volume UI's connection to choragusd (`audio-control/`, lyra-v1.md §7).
/// Requests are fire-and-forget; answers and change events arrive through
/// [`Control::recv`].
pub struct Control {
    sock: UnixStream,
}

impl Control {
    pub fn socket(&self) -> &UnixStream {
        &self.sock
    }

    pub fn connect(path: &str) -> io::Result<Control> {
        let mut sock = UnixStream::connect(path)?;
        wire::send(&mut sock, &ToControl::Hello { version: wire::VERSION })?;
        match wire::recv(&mut sock)? {
            FromControl::Welcome => Ok(Control { sock }),
            FromControl::Denied { code } => Err(denied(code)),
            m => Err(io::Error::new(io::ErrorKind::InvalidData, format!("unexpected {m:?}"))),
        }
    }

    pub fn send(&mut self, m: &ToControl) -> io::Result<()> {
        wire::send(&mut self.sock, m)
    }

    /// The next answer or change event (blocks).
    pub fn recv(&mut self) -> io::Result<FromControl> {
        wire::recv(&mut self.sock)
    }

    /// A second handle on the same connection: one thread reads the change
    /// events while another sends.
    pub fn try_clone(&self) -> io::Result<Control> {
        Ok(Control { sock: self.sock.try_clone()? })
    }
}

/// A media controller's connection to choragusd (`media-control/`,
/// atrium-player-pip-keys.md §4): Forum's bar and key route, the lock screen.
/// `NowPlaying` arrives through [`Media::recv`] on each change.
pub struct Media {
    sock: UnixStream,
}

impl Media {
    pub fn connect(path: &str, lock_screen: bool) -> io::Result<Media> {
        let mut sock = UnixStream::connect(path)?;
        wire::send(&mut sock, &ToMedia::Hello { version: wire::VERSION, lock_screen })?;
        match wire::recv(&mut sock)? {
            FromMedia::Welcome => Ok(Media { sock }),
            FromMedia::Denied { code } => Err(denied(code)),
            m => Err(io::Error::new(io::ErrorKind::InvalidData, format!("unexpected {m:?}"))),
        }
    }

    pub fn send(&mut self, m: &ToMedia) -> io::Result<()> {
        wire::send(&mut self.sock, m)
    }

    /// Ask the active session for `action`.
    pub fn control(&mut self, action: MediaAction) -> io::Result<()> {
        self.send(&ToMedia::Control { action, seek_offset: None, seek_time: None })
    }

    /// The next `NowPlaying` (blocks).
    pub fn recv(&mut self) -> io::Result<FromMedia> {
        wire::recv(&mut self.sock)
    }

    pub fn socket(&self) -> &UnixStream {
        &self.sock
    }
}

/// An output device driver's connection to choragusd (`audio-device/`,
/// lyra-external-device.md): announce, then read lyrad's mix from `ring` and
/// keep `page` current.
pub struct Device {
    sock: UnixStream,
    /// The output's id, as the volume UI shows it.
    pub device: u32,
    pub ring: Ring,
    pub page: DeviceWriter,
}

/// What a device announces.
#[derive(Clone, Debug)]
pub struct Announce {
    pub persistent_id: u64,
    pub name: String,
    pub kind: OutputKind,
    pub rate: u32,
    pub channels: u8,
    pub hw_volume: bool,
    pub initial_delay_frames: u32,
}

impl Device {
    pub fn connect(path: &str, a: &Announce) -> io::Result<Device> {
        let mut sock = UnixStream::connect(path)?;
        wire::send(&mut sock, &ToDevice::Hello { version: wire::VERSION })?;
        match wire::recv(&mut sock)? {
            FromDevice::Welcome => {}
            FromDevice::Denied { code } => return Err(denied(code)),
            m => return Err(io::Error::new(io::ErrorKind::InvalidData, format!("unexpected {m:?}"))),
        }
        Self::announce(sock, a)
    }

    fn announce(mut sock: UnixStream, a: &Announce) -> io::Result<Device> {
        wire::send(&mut sock, &ToDevice::Announce {
            persistent_id: a.persistent_id,
            name: a.name.clone(),
            kind: a.kind,
            rate: a.rate,
            channels: a.channels,
            hw_volume: a.hw_volume,
            initial_delay_frames: a.initial_delay_frames,
        })?;
        loop {
            match wire::recv(&mut sock)? {
                FromDevice::Ready { device, .. } => {
                    let ring = Ring::from_fd(recv_fd(sock.as_raw_fd())?, false)?;
                    let page = DeviceWriter::from_fd(recv_fd(sock.as_raw_fd())?)?;
                    return Ok(Device { sock, device, ring, page });
                }
                FromDevice::Denied { code } => return Err(denied(code)),
                // A volume before Ready: the next recv() would have it anyway.
                FromDevice::SetVolume { .. } | FromDevice::Welcome => {}
            }
        }
    }

    /// The link changed its format (a codec reconfiguration): `Gone`, then a
    /// new announcement on the same socket, so the output stays the same.
    pub fn reconfigure(mut self, a: &Announce) -> io::Result<Device> {
        wire::send(&mut self.sock, &ToDevice::Gone)?;
        let sock = self.sock.try_clone()?;
        drop(self);
        Self::announce(sock, a)
    }

    pub fn volume(&mut self, volume: f32) -> io::Result<()> {
        wire::send(&mut self.sock, &ToDevice::Volume { volume })
    }

    pub fn gone(&mut self) -> io::Result<()> {
        wire::send(&mut self.sock, &ToDevice::Gone)
    }

    /// The next message from choragusd (blocks): `SetVolume`.
    pub fn recv(&mut self) -> io::Result<FromDevice> {
        wire::recv(&mut self.sock)
    }

    pub fn socket(&self) -> &UnixStream {
        &self.sock
    }
}
