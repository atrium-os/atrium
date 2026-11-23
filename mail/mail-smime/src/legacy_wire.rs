//! Private one-shot wire between the trusted legacy-read fixture and its
//! fixed worker. The control channel is inherited from Portcullis.

use std::io::{self, Read, Write};

pub const MAX_CERT: usize = 4000;
pub const MAX_CMS: usize = 128 << 10;
pub const MAX_TEXT: usize = 64 << 10;
pub const WORKER_READY: &[u8] = b"legacy-jail-capmode-1";
pub const FAILURE: &str = "Couldn’t open this legacy S/MIME message.";

pub fn write(out: &mut impl Write, bytes: &[u8], limit: usize) -> io::Result<()> {
    if bytes.is_empty() || bytes.len() > limit {
        return Err(io::Error::other("invalid legacy S/MIME frame size"));
    }
    out.write_all(&(bytes.len() as u32).to_le_bytes())?;
    out.write_all(bytes)
}

pub fn read(input: &mut impl Read, limit: usize) -> io::Result<Vec<u8>> {
    let mut size = [0; 4];
    input.read_exact(&mut size)?;
    let size = u32::from_le_bytes(size) as usize;
    if size == 0 || size > limit {
        return Err(io::Error::other("invalid legacy S/MIME frame size"));
    }
    let mut data = vec![0; size];
    input.read_exact(&mut data)?;
    Ok(data)
}

pub fn plain_text(decrypted: &[u8]) -> Option<&str> {
    let body = decrypted.strip_prefix(b"Content-Type: text/plain\r\n\r\n")?;
    if body.is_empty() || body.len() > MAX_TEXT {
        return None;
    }
    let text = std::str::from_utf8(body).ok()?;
    text.chars()
        .all(|ch| !ch.is_control() || matches!(ch, '\n' | '\r' | '\t'))
        .then_some(text)
}
