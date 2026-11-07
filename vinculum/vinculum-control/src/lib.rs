//! The `bluetooth-control` wire (docs/spec/bluetooth.md §13).
//!
//! One Unix stream socket, `/atrium/sockets/bluetooth-control/control.sock`
//! on Atrium (behind the restricted `bluetooth-control` capability: Curia
//! and the Forum bar), or a path of the admin's choosing on stock FreeBSD
//! (with `vinculum-ctl`). Each message is `u32 LE length | postcard body`,
//! at most [`MAX_MESSAGE`] bytes, as on Lyra's wire.
//!
//! A client sends [`Request`]s and receives [`Event`]s: one reply per
//! request (`Ok`, `Error` or the data asked for), plus unsolicited events
//! (devices found while scanning, connections, pairing progress). A client
//! that says `Hello { agent: true }` is a pairing agent: it draws the
//! prompts. Pairing prompts go only to agents, and with no agent connected
//! every pairing is refused (§7: pairing only while the UI is open).
//!
//! Addresses are shown to this trusted UI (it is where the user picks a
//! device), never to apps (§12).
#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

pub const MAX_MESSAGE: usize = 4096;

/// A device address, most significant byte first (as printed), and
/// whether it is random.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Addr {
    pub bytes: [u8; 6],
    pub random: bool,
}

impl std::fmt::Display for Addr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let a = self.bytes;
        write!(f, "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}", a[0], a[1], a[2], a[3], a[4], a[5])?;
        if self.random {
            write!(f, "/r")?;
        }
        Ok(())
    }
}

impl std::str::FromStr for Addr {
    type Err = &'static str;
    /// `AA:BB:CC:DD:EE:FF`, with `/r` for a random address.
    fn from_str(s: &str) -> Result<Addr, &'static str> {
        let (hex, random) = match s.strip_suffix("/r") {
            Some(h) => (h, true),
            None => (s, false),
        };
        let parts: Vec<&str> = hex.split(':').collect();
        if parts.len() != 6 {
            return Err("address is six hex octets AA:BB:CC:DD:EE:FF[/r]");
        }
        let mut bytes = [0u8; 6];
        for (b, p) in bytes.iter_mut().zip(parts) {
            if p.len() != 2 {
                return Err("address octet is two hex digits");
            }
            *b = u8::from_str_radix(p, 16).map_err(|_| "address octet is two hex digits")?;
        }
        Ok(Addr { bytes, random })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    /// First message. `agent`: this client draws pairing prompts.
    Hello { agent: bool },
    Status,
    /// Scan for devices to pair; results arrive as `Found`.
    Scan { on: bool },
    /// Pair a device found by a scan: opens the pairable window (§7),
    /// connects and pairs. Progress: `Prompt`, then `Paired` or
    /// `PairingFailed`.
    Pair { address: Addr },
    /// Connect a bonded device (encrypted with its bond).
    Connect { address: Addr },
    Disconnect { address: Addr },
    /// The bonded devices.
    Devices,
    /// Forget a bond: its key is deleted from the key store.
    Remove { address: Addr },
    /// An agent's answer to `Prompt::Confirm`.
    Confirm { address: Addr, yes: bool },
    /// An agent's answer to `Prompt::EnterPasskey`; None cancels.
    Passkey { address: Addr, passkey: Option<u32> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Prompt {
    /// Confirm pairing; `number` is the six digits to compare with the
    /// device's display, None for Just Works.
    Confirm { number: Option<u32> },
    /// Show this passkey; the user types it on the device.
    Display { passkey: u32 },
    /// Ask the user for the passkey the device shows.
    EnterPasskey,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub address: Addr,
    pub identity: Option<Addr>,
    pub name: String,
    pub authenticated: bool,
    pub secure_connections: bool,
    /// Whether the bond may carry keyboard reports (§7).
    pub keyboard_allowed: bool,
    pub connected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    Ok,
    Error { why: String },
    Status { ready: bool, address: Option<Addr>, scanning: bool, pairable: bool, agents: u32, connections: u32 },
    Found { address: Addr, name: Option<String>, rssi: i8 },
    Devices { devices: Vec<DeviceInfo> },
    Prompt { address: Addr, prompt: Prompt },
    Paired { address: Addr, authenticated: bool, keyboard_allowed: bool },
    PairingFailed { address: Addr, why: String },
    Connected { address: Addr, encrypted: bool },
    Disconnected { address: Addr, reason: String },
}

#[derive(Debug, PartialEq, Eq)]
pub enum WireError {
    TooLong,
    Malformed,
}

/// One message, framed.
pub fn encode<T: Serialize>(m: &T) -> Vec<u8> {
    let body = postcard::to_stdvec(m).expect("serialize");
    assert!(body.len() <= MAX_MESSAGE, "message over MAX_MESSAGE");
    let mut out = (body.len() as u32).to_le_bytes().to_vec();
    out.extend_from_slice(&body);
    out
}

/// Accumulates a byte stream and yields whole messages.
#[derive(Debug, Default)]
pub struct Reader {
    buf: Vec<u8>,
}

impl Reader {
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// The next whole message, if one has arrived. An error poisons the
    /// stream: the caller drops the connection.
    pub fn next<T: for<'de> Deserialize<'de>>(&mut self) -> Result<Option<T>, WireError> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let n = u32::from_le_bytes(self.buf[..4].try_into().expect("4")) as usize;
        if n > MAX_MESSAGE {
            return Err(WireError::TooLong);
        }
        if self.buf.len() < 4 + n {
            return Ok(None);
        }
        let m = postcard::from_bytes(&self.buf[4..4 + n]).map_err(|_| WireError::Malformed)?;
        self.buf.drain(..4 + n);
        Ok(Some(m))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_split_and_bounds() {
        let a: Addr = "F0:F1:F2:F3:F4:F5/r".parse().unwrap();
        assert_eq!(a.to_string(), "F0:F1:F2:F3:F4:F5/r");
        assert!("F0:F1".parse::<Addr>().is_err());
        assert!("G0:F1:F2:F3:F4:F5".parse::<Addr>().is_err());
        let msgs = [Request::Hello { agent: true }, Request::Pair { address: a }, Request::Passkey { address: a, passkey: Some(42) }];
        let bytes: Vec<u8> = msgs.iter().flat_map(encode).collect();
        let mut r = Reader::default();
        let mut got = Vec::new();
        for b in bytes.chunks(3) {
            r.push(b);
            while let Some(m) = r.next::<Request>().unwrap() {
                got.push(m);
            }
        }
        assert_eq!(got, msgs);
        let mut r = Reader::default();
        r.push(&(MAX_MESSAGE as u32 + 1).to_le_bytes());
        assert_eq!(r.next::<Request>(), Err(WireError::TooLong));
        let mut r = Reader::default();
        r.push(&[1, 0, 0, 0, 0xFF]);
        assert_eq!(r.next::<Request>(), Err(WireError::Malformed));
    }
}
