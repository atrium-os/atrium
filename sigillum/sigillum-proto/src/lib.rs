//! Sigillum's wires (sigillum.md §14).
//!
//! Both are `u32 LE length | postcard`, capped: [`client::MAX_MESSAGE`] for
//! clients, [`driver::MAX_MESSAGE`] for drivers. The driver records are the
//! only device-derived data `sigillumd` reads, so every field has a cap that
//! [`driver::FromDriver::check_caps`] enforces after decoding.
#![forbid(unsafe_code)]

use std::io::{self, Read, Write};

use serde::{de::DeserializeOwned, Serialize};

pub mod client;
pub mod driver;
pub mod ui;

/// Write one message.
pub fn write_msg<T: Serialize>(w: &mut impl Write, msg: &T, max: usize) -> io::Result<()> {
    let body = postcard::to_stdvec(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
    if body.len() > max {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "message too large"));
    }
    w.write_all(&(body.len() as u32).to_le_bytes())?;
    w.write_all(&body)?;
    w.flush()
}

/// Read one message; `Ok(None)` at a clean end of stream.
pub fn read_msg<T: DeserializeOwned>(r: &mut impl Read, max: usize) -> io::Result<Option<T>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let n = u32::from_le_bytes(len) as usize;
    if n > max {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "message too large"));
    }
    let mut buf = zeroize::Zeroizing::new(vec![0u8; n]);
    r.read_exact(&mut buf)?;
    postcard::from_bytes(&buf).map(Some).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}
