//! Private channel between one I4 video element and its broker-owned player.
//! No operation accepts a URL, window token, embed id or descriptor.
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::io::{self, Read, Write};

pub const MIME: &str = "video/webm; codecs=\"vp9\"";
pub const OPUS_MIME: &str = "audio/webm; codecs=\"opus\"";
// One SourceBuffer append may contain a complete bounded CDN response.
pub const MAX_APPEND: usize = crate::youtube_media::REPLY;

#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    Open { rect: [u16; 4] },
    AddVp9,
    Append(Vec<u8>),
    End,
    Seek(f64),
    AddOpus,
    AppendOpus(Vec<u8>),
    SetPlaying(bool),
    Volume(f64),
    Clock,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Reply {
    Ok,
    Appended { ranges: Vec<(f64, f64)> },
    Ready { duration: f64 },
    Seeked { secs: f64 },
    Failed,
    Position { secs: f64 },
}

pub fn send<T: Serialize>(socket: &mut impl Write, message: &T) -> io::Result<()> {
    let bytes = postcard::to_stdvec(message).map_err(io::Error::other)?;
    if bytes.len() > MAX_APPEND + 8 {
        return Err(io::Error::other("video frame too large"));
    }
    socket.write_all(&(bytes.len() as u32).to_le_bytes())?;
    socket.write_all(&bytes)
}

pub fn receive<T: DeserializeOwned>(socket: &mut impl Read) -> io::Result<T> {
    let mut size = [0; 4];
    socket.read_exact(&mut size)?;
    let size = u32::from_le_bytes(size) as usize;
    if size > MAX_APPEND + 8 {
        return Err(io::Error::other("video frame too large"));
    }
    let mut bytes = vec![0; size];
    socket.read_exact(&mut bytes)?;
    let (message, trailing) = postcard::take_from_bytes(&bytes).map_err(io::Error::other)?;
    if !trailing.is_empty() {
        return Err(io::Error::other("trailing video bytes"));
    }
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn large_appends_round_trip_and_bad_frames_are_refused() {
        let mut bytes = Vec::new();
        send(&mut bytes, &Request::Append(vec![7; MAX_APPEND])).unwrap();
        assert!(
            matches!(receive(&mut bytes.as_slice()), Ok(Request::Append(b)) if b == vec![7; MAX_APPEND])
        );
        assert!(receive::<Request>(&mut u32::MAX.to_le_bytes().as_slice()).is_err());
        let mut audio = Vec::new();
        send(&mut audio, &Request::AppendOpus(vec![3; MAX_APPEND])).unwrap();
        assert!(
            matches!(receive(&mut audio.as_slice()), Ok(Request::AppendOpus(b)) if b == vec![3; MAX_APPEND])
        );
        let mut extra = Vec::new();
        send(&mut extra, &Request::End).unwrap();
        extra[0] += 1;
        extra.push(0);
        assert!(receive::<Request>(&mut extra.as_slice()).is_err());
        assert!(receive::<Request>(&mut &bytes[..bytes.len() - 1]).is_err());
    }
}
