//! Length-prefixed postcard frames: how prelumd and a worker talk over the
//! worker's one socket (its stdin and stdout). A frame is a `u32`
//! little-endian length, then that many bytes of postcard. The length is
//! checked against [`MAX_FRAME`] before anything is allocated.

use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::Serialize;

/// The largest frame either side accepts: a data frame carries at most
/// [`DATA_CHUNK`] bytes, everything else is far smaller.
pub const MAX_FRAME: usize = 1 << 20;
/// How much document data one frame carries.
pub const DATA_CHUNK: usize = 256 * 1024;

pub fn write<T: Serialize>(w: &mut impl Write, v: &T) -> io::Result<()> {
    let body = postcard::to_stdvec(v).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
    if body.len() > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "frame too large"));
    }
    let mut buf = Vec::with_capacity(4 + body.len());
    buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
    buf.extend_from_slice(&body);
    w.write_all(&buf)?;
    w.flush()
}

/// The next frame, or `None` at a clean end of stream (between frames).
pub fn read<T: DeserializeOwned>(r: &mut impl Read) -> io::Result<Option<T>> {
    let mut len = [0u8; 4];
    let mut got = 0;
    while got < 4 {
        match r.read(&mut len[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    let n = u32::from_le_bytes(len) as usize;
    if n > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut body = vec![0u8; n];
    r.read_exact(&mut body)?;
    postcard::from_bytes(&body).map(Some).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipp::Req;

    #[test]
    fn frames() {
        let mut b = Vec::new();
        write(&mut b, &Req::Data(vec![7; 1000])).unwrap();
        write(&mut b, &Req::End).unwrap();
        let mut r = &b[..];
        assert_eq!(read::<Req>(&mut r).unwrap(), Some(Req::Data(vec![7; 1000])));
        assert_eq!(read::<Req>(&mut r).unwrap(), Some(Req::End));
        assert_eq!(read::<Req>(&mut r).unwrap(), None);
        // Cut anywhere inside a frame: an error, never a short value.
        let first = 4 + u32::from_le_bytes(b[..4].try_into().unwrap()) as usize;
        for cut in 1..first {
            let mut r = &b[..cut];
            assert!(read::<Req>(&mut r).is_err(), "cut {cut}");
        }
        // A length past the limit is refused before allocating.
        let mut r: &[u8] = &(MAX_FRAME as u32 + 1).to_le_bytes();
        assert!(read::<Req>(&mut r).is_err());
        assert!(write(&mut Vec::new(), &Req::Data(vec![0; MAX_FRAME + 1])).is_err());
    }
}
