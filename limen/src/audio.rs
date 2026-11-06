//! The real [`Attribution`](crate::daemon::Attribution): choragusd's
//! `audio-attribution/` socket (lyra-v1.md §4). choragusd serves only
//! limend's uid there, and a ticket dies when limend cancels it or hangs up.

use std::os::unix::net::UnixStream;
use std::time::Duration;

use lyra_protocol::wire::{self, FromAttribution, ToAttribution};

use crate::daemon::Attribution;

pub const DEFAULT_SOCKET: &str = "/atrium/sockets/audio-attribution/audio-attribution.sock";

pub struct Choragus {
    path: String,
    conn: Option<UnixStream>,
}

impl Choragus {
    pub fn new(path: impl Into<String>) -> Self {
        Self { path: path.into(), conn: None }
    }

    /// One request and its reply; reconnects once if choragusd restarted
    /// (its tickets died with the old connection anyway).
    fn ask(&mut self, m: &ToAttribution) -> Option<FromAttribution> {
        for _ in 0..2 {
            if self.conn.is_none() {
                let s = UnixStream::connect(&self.path).ok()?;
                // limend asks under its state lock: never wait long.
                s.set_read_timeout(Some(Duration::from_secs(1))).ok()?;
                s.set_write_timeout(Some(Duration::from_secs(1))).ok()?;
                self.conn = Some(s);
            }
            let c = self.conn.as_mut()?;
            match wire::send(c, m).and_then(|_| wire::recv(c)) {
                Ok(r) => return Some(r),
                Err(_) => self.conn = None,
            }
        }
        None
    }
}

impl Attribution for Choragus {
    fn issue(&mut self, child_uid: u32, parent_uid: u32) -> Option<[u8; 16]> {
        match self.ask(&ToAttribution::Issue { child_uid, parent_uid }) {
            Some(FromAttribution::Issued { ticket }) => Some(ticket),
            r => {
                eprintln!("limend: no audio ticket for uid {child_uid} under uid {parent_uid} ({r:?})");
                None
            }
        }
    }

    fn cancel(&mut self, ticket: [u8; 16]) {
        let _ = self.ask(&ToAttribution::Cancel { ticket });
    }
}
