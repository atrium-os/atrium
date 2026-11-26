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
    /// `Pair` for a Classic (BR/EDR) device, paged by address: Classic
    /// devices are not found by an LE scan.
    PairClassic { address: Addr },
    /// Bluetooth on or off. Off: every link is dropped, nothing scans,
    /// pages or is let in, and requests that would use the radio are
    /// refused, until it is turned on again; kept across restarts when
    /// vinculumd has a state file.
    Power { on: bool },
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
    /// The battery level the device last reported (Battery Service), in
    /// percent; None while not connected or when it has none.
    pub battery: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    Ok,
    Error { why: String },
    /// `powered`: Bluetooth is on (`Power`).
    Status { ready: bool, powered: bool, address: Option<Addr>, scanning: bool, pairable: bool, agents: u32, connections: u32 },
    /// A device the scan saw. `class_of_device` is set for a Classic
    /// (BR/EDR) device found by inquiry, to pair with `PairClassic`; it is
    /// `None` for an LE advertiser (`Pair`).
    Found { address: Addr, name: Option<String>, rssi: i8, class_of_device: Option<u32> },
    Devices { devices: Vec<DeviceInfo> },
    Prompt { address: Addr, prompt: Prompt },
    Paired { address: Addr, authenticated: bool, keyboard_allowed: bool },
    PairingFailed { address: Addr, why: String },
    Connected { address: Addr, encrypted: bool },
    Disconnected { address: Addr, reason: String },
    /// A connected device reported a new battery level, in percent.
    Battery { address: Addr, percent: u8 },
}

/// A name a device gave (advertising, inquiry, Remote Name Request), made
/// safe to show beside other devices' names: control characters (C0, C1)
/// and the bidirectional embeddings, overrides and isolates (U+202A–202E,
/// U+2066–2069), direction marks (U+200E, U+200F, U+061C), line and
/// paragraph separators (U+2028, U+2029) and zero-width characters (U+200B
/// to U+200D, U+2060, U+FEFF) are removed, so a device cannot break or
/// redraw the line it is shown on, hide a character or pose as another;
/// whitespace at the ends is trimmed.
pub fn clean_name(name: &str) -> String {
    let keep = |c: &char| !c.is_control() && !matches!(*c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200B}'..='\u{200F}' | '\u{2028}' | '\u{2029}' | '\u{2060}' | '\u{FEFF}' | '\u{061C}');
    name.chars().filter(keep).collect::<String>().trim().to_string()
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

    #[test]
    fn names_lose_controls_and_direction_overrides() {
        assert_eq!(clean_name(" Keys\u{202E}draobyek\u{202C}\n"), "Keysdraobyek");
        assert_eq!(clean_name("a\u{2066}b\u{0085}c\u{1b}[2J"), "abc[2J");
        assert_eq!(clean_name("Écouteurs ✓"), "Écouteurs ✓");
        assert_eq!(clean_name("Mo\u{200B}use\u{2028}\u{FEFF}X\u{2029}"), "MouseX");
    }
}
