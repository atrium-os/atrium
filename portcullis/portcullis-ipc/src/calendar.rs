//! One selected Mail scheduling part and its private return channel.
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{
    io::{self, Read, Write},
    os::unix::net::UnixStream,
};

pub const MIME: &str = "application/vnd.atrium.mail-invitation-session";
pub const FILE_MAX: usize = 4 << 20;
pub const REVIEW_SECONDS: u64 = 300;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub account: u32,
    pub from: String,
    pub identities: Vec<String>,
    pub verified: bool,
    pub reply_destination_matches: bool,
}
pub fn address(s: &str) -> bool {
    let Some((local, domain)) = s.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && s.len() <= 254
        && local
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b".+_-'".contains(&c))
        && domain
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b".-".contains(&c))
}
impl Source {
    pub fn valid(&self) -> bool {
        self.from.len() <= 254
            && !self.from.chars().any(char::is_control)
            && self.identities.len() <= 32
            && self.identities.iter().all(|s| address(s))
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Answer {
    Accept,
    Maybe,
    Decline,
}
impl Answer {
    pub fn partstat(self) -> &'static str {
        match self {
            Self::Accept => "ACCEPTED",
            Self::Maybe => "TENTATIVE",
            Self::Decline => "DECLINED",
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub answer: Answer,
    pub attendee: String,
    pub uid: String,
    pub organizer: String,
    pub sequence: u32,
}
pub fn write<T: Serialize>(s: &mut UnixStream, value: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > 16384 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    s.write_all(&(bytes.len() as u32).to_be_bytes())?;
    s.write_all(&bytes)
}
pub fn read<T: DeserializeOwned>(s: &mut UnixStream) -> io::Result<T> {
    let mut len = [0; 4];
    s.read_exact(&mut len)?;
    let n = u32::from_be_bytes(len) as usize;
    if n > 16384 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut bytes = vec![0; n];
    s.read_exact(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}
