//! The high-level audio-app API — one front door.
//!
//! An app calls [`play`]: it connects to **choragusd** (the session layer),
//! registers under a role, and receives the producer end of its data-plane ring
//! — which choragusd brokered from lyrad. The app never touches the RT engine
//! directly; policy (routing, ducking, the §9 capability check) is applied on the
//! way. The returned [`PlayStream`] owns the session connection: dropping it
//! closes the stream.

use crate::app::{write_hello, AppMsg, DENIED};
use crate::fdpass::recv_fd;
use crate::ring::Ring;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

/// A registered, playing audio stream: the producer end of its ring plus the
/// live session connection to choragusd.
pub struct PlayStream {
    pub stream_id: u32,
    pub ring: Ring,
    conn: UnixStream, // hold the session open; dropping it closes the stream
}

impl PlayStream {
    /// Write interleaved frames into the ring; returns frames accepted (bounded
    /// by free space — the consumer's drain paces a well-behaved source).
    pub fn write(&self, frames: &[f32]) -> u64 {
        self.ring.write(frames)
    }

    /// The session fd (for poll/close); the stream lives as long as it is open.
    pub fn session(&self) -> &UnixStream {
        &self.conn
    }
}

/// Connect to choragusd, register `app_id` as `role`/`caps`, and receive the
/// data-plane ring (brokered from lyrad). The single front door: control + data
/// set up over one connection, policy applied between.
pub fn play(choragus_sock: &str, app_id: &str, role: u8, caps: u8) -> io::Result<PlayStream> {
    let mut conn = UnixStream::connect(choragus_sock)?;
    write_hello(&mut conn, app_id)?;
    conn.write_all(&AppMsg::Register { role, caps }.encode())?;

    let mut idb = [0u8; 4];
    conn.read_exact(&mut idb)?;
    let id = u32::from_le_bytes(idb);
    if id == DENIED {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "registration denied"));
    }

    // choragusd follows the id with the ring's fd (it brokered it from lyrad).
    let fd = recv_fd(conn.as_raw_fd())?;
    let ring = Ring::from_fd(fd, true)?; // this end is the producer
    Ok(PlayStream { stream_id: id, ring, conn })
}
