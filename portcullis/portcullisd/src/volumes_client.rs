//! Synchronous Rust client for `atrium-volumes`' wire protocol.
//!
//! Mirrors the shape of `jaild_client` exactly — length-prefixed
//! JSON, single persistent connection per portcullisd lifetime,
//! one-request-at-a-time. atrium-volumes is single-threaded so
//! the same single-connection convention applies.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use atrium_volumes::protocol::{Request, Response, MAX_FRAME_BYTES};

pub struct Client {
    stream: UnixStream,
}

impl Client {
    pub fn connect(path: impl AsRef<Path>) -> io::Result<Self> {
        Ok(Self { stream: UnixStream::connect(path)? })
    }

    pub fn send(&mut self, req: &Request) -> io::Result<Response> {
        let body = serde_json::to_vec(req)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData,
                format!("serialize: {e}")))?;
        write_frame(&mut self.stream, &body)?;
        self.stream.flush()?;

        let frame = read_frame(&mut self.stream)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof,
                "atrium-volumes closed connection"))?;
        let resp: Response = serde_json::from_slice(&frame)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData,
                format!("parse response: {e}")))?;
        Ok(resp)
    }
}

fn write_frame<W: Write>(mut w: W, body: &[u8]) -> io::Result<()> {
    if body.len() as u64 > MAX_FRAME_BYTES as u64 {
        return Err(io::Error::new(io::ErrorKind::InvalidData,
            format!("outbound frame too large: {}", body.len())));
    }
    let len = (body.len() as u32).to_le_bytes();
    w.write_all(&len)?;
    w.write_all(body)?;
    Ok(())
}

fn read_frame<R: Read>(mut r: R) -> io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match r.read(&mut len_buf)? {
        0 => return Ok(None),
        n if n < 4 => r.read_exact(&mut len_buf[n..])?,
        _ => {}
    }
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidData,
            format!("inbound frame too large: {len}")));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    Ok(Some(buf))
}
