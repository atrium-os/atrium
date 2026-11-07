//! tactusd to its clients (docs/spec/atrium-input.md §4.0): one stream
//! socket, each message a little-endian u32 length then postcard.
//! tactusd sends `Hello`, a `Device` for every device present, then
//! events as they happen. Clients never write.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

pub use crate::decode::Axis;
pub use crate::hid::Class;
pub use crate::touchpad::{GestureKind, Phase};

pub const VERSION: u32 = 1;
/// No message is near this; a longer length is a broken stream.
pub const MAX_MESSAGE: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Bus {
    Usb,
    I2c,
    Ps2,
    Bluetooth,
    Virtual,
    Other,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Message {
    Hello { version: u32 },
    Device { id: u32, bus: Bus, classes: Vec<Class>, vendor: u16, product: u16, name: String },
    Gone { id: u32 },
    /// `usage` is page << 16 | id. `repeat`: synthesised by Tactus.
    Key { id: u32, ts_ns: u64, usage: u32, down: bool, repeat: bool },
    Button { id: u32, ts_ns: u64, usage: u32, down: bool },
    /// Accelerated deltas, and the device's own counts.
    Motion { id: u32, ts_ns: u64, dx: f32, dy: f32, raw_dx: i32, raw_dy: i32 },
    /// 0..=1 across the device's range.
    Absolute { id: u32, ts_ns: u64, x: f32, y: f32 },
    /// 120 per wheel detent; HID sign (positive is away from the user, or right).
    Scroll { id: u32, ts_ns: u64, axis: Axis, value_120: i32 },
    /// Two fingers on a touchpad moved this many pixels (finger
    /// direction: positive is right / towards the user). `End` has no
    /// motion: the fingers lifted, so a scroll view may fling.
    ScrollPixels { id: u32, ts_ns: u64, dx: f32, dy: f32, phase: Phase },
    /// A touchpad swipe (three or more fingers, pixels) or pinch (two,
    /// `scale` against the start).
    Gesture { id: u32, ts_ns: u64, kind: GestureKind, fingers: u8, phase: Phase, dx: f32, dy: f32, scale: f32 },
}

pub fn write(w: &mut impl Write, m: &Message) -> io::Result<()> {
    let body = postcard::to_stdvec(m).map_err(io::Error::other)?;
    let mut buf = Vec::with_capacity(4 + body.len());
    buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
    buf.extend_from_slice(&body);
    w.write_all(&buf)
}

pub fn read(r: &mut impl Read) -> io::Result<Message> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let n = u32::from_le_bytes(len) as usize;
    if n > MAX_MESSAGE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "tactus: message too long"));
    }
    let mut body = vec![0u8; n];
    r.read_exact(&mut body)?;
    let (m, rest) = postcard::take_from_bytes(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if !rest.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "tactus: trailing bytes"));
    }
    Ok(m)
}

/// CLOCK_MONOTONIC in ns, the timestamp every event carries.
pub fn now_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_refuses_junk() {
        let ms = [
            Message::Hello { version: VERSION },
            Message::Device { id: 3, bus: Bus::Usb, classes: vec![Class::Keyboard, Class::MediaKeys], vendor: 0x46d, product: 0xc52b, name: "kbd".into() },
            Message::Key { id: 3, ts_ns: 9, usage: 0x0007_0004, down: true, repeat: false },
            Message::Motion { id: 3, ts_ns: 10, dx: 1.5, dy: -2.0, raw_dx: 1, raw_dy: -1 },
            Message::Scroll { id: 3, ts_ns: 11, axis: Axis::Vertical, value_120: -120 },
        ];
        let mut buf = Vec::new();
        for m in &ms {
            write(&mut buf, m).unwrap();
        }
        let mut r = &buf[..];
        for m in &ms {
            assert_eq!(&read(&mut r).unwrap(), m);
        }
        assert!(read(&mut r).is_err(), "end of stream");
        let mut big = &[0xff, 0xff, 0xff, 0x7f][..];
        assert!(read(&mut big).is_err());
        let mut trailing = &[3, 0, 0, 0, 0, 1, 9][..];
        assert!(read(&mut trailing).is_err());
    }
}
