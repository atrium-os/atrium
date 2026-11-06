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
use crate::wire::{self, DenyCode, FromControl, StreamParams, ToApp, ToChoragus, ToControl};
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
}
