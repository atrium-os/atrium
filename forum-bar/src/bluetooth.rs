//! The bar's Bluetooth item and its menu (bluetooth.md §13).
//!
//! It follows vinculumd's control socket (`bluetooth-control`) as a plain
//! client, never the pairing agent: Curia's Bluetooth page draws pairing
//! prompts. The item is on the bar while a paired device is connected; its
//! menu lists each with the battery level the device reports and a
//! Disconnect, and the icon turns amber when a battery is at 20% or less.

use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

use pergola::widgets::popup::MenuItem;
use vinculum_control::{encode, Addr, DeviceInfo, Event, Reader, Request};

/// A battery at or under this, in percent, is low.
pub const LOW_BATTERY: u8 = 20;

fn path() -> PathBuf {
    std::env::var_os("FORUM_BAR_BT_SOCK").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/atrium/sockets/bluetooth-control/control.sock"))
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BtShown {
    /// The paired devices; None while vinculumd is not reachable.
    pub devices: Option<Vec<DeviceInfo>>,
    /// The last disconnect that did not work.
    pub note: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BtAction {
    Disconnect(Addr),
}

pub enum BtMsg {
    Devices(Option<Vec<DeviceInfo>>),
    Battery { address: Addr, percent: u8 },
    Done(Result<(), String>),
}

/// What the menu calls a device.
pub fn label(d: &DeviceInfo) -> String {
    if d.name.trim().is_empty() {
        d.address.to_string()
    } else {
        d.name.clone()
    }
}

impl BtShown {
    fn connected(&self) -> impl Iterator<Item = &DeviceInfo> {
        self.devices.as_deref().unwrap_or(&[]).iter().filter(|d| d.connected)
    }

    /// Whether the item is on the bar at all.
    pub fn shown(&self) -> bool {
        self.connected().next().is_some()
    }

    /// A connected device's battery is low.
    pub fn low(&self) -> bool {
        self.connected().any(|d| d.battery.is_some_and(|p| p <= LOW_BATTERY))
    }

    /// A device reported a new level.
    pub fn battery(&mut self, address: Addr, percent: u8) {
        if let Some(d) = self.devices.as_mut().and_then(|v| v.iter_mut().find(|d| d.address == address || d.identity == Some(address))) {
            d.battery = Some(percent);
        }
    }
}

/// The menu and what each item does.
pub fn menu(n: &BtShown) -> (Vec<MenuItem>, Vec<Option<BtAction>>) {
    let mut items = vec![MenuItem::new("Bluetooth").enabled(false)];
    let mut acts = vec![None];
    if let Some(note) = &n.note {
        items.push(MenuItem::new(note.clone()).enabled(false));
        acts.push(None);
    }
    for (i, d) in n.connected().enumerate() {
        let state = match d.battery {
            Some(p) if p <= LOW_BATTERY => format!("battery low, {p}%"),
            Some(p) => format!("battery {p}%"),
            None => "connected".to_string(),
        };
        let it = MenuItem::new(format!("Disconnect {} ({state})", label(d)));
        items.push(if i == 0 { it.separated() } else { it });
        acts.push(Some(BtAction::Disconnect(d.address)));
    }
    (items, acts)
}

/// One connection's messages, in order, read with a deadline.
struct Conn {
    s: UnixStream,
    r: Reader,
}

impl Conn {
    fn open() -> Result<Conn, String> {
        let s = UnixStream::connect(path()).map_err(|e| e.to_string())?;
        s.set_write_timeout(Some(Duration::from_secs(5))).map_err(|e| e.to_string())?;
        let mut c = Conn { s, r: Reader::default() };
        c.send(&Request::Hello { agent: false })?;
        Ok(c)
    }

    fn send(&mut self, r: &Request) -> Result<(), String> {
        self.s.write_all(&encode(r)).map_err(|_| "Bluetooth went away".to_string())
    }

    /// The next event; Ok(None) when `wait` passed with none.
    fn next(&mut self, wait: Duration) -> Result<Option<Event>, String> {
        let end = Instant::now() + wait;
        let mut buf = [0u8; 4096];
        loop {
            if let Some(e) = self.r.next::<Event>().map_err(|e| format!("Bluetooth sent a bad message ({e:?})"))? {
                return Ok(Some(e));
            }
            let Some(left) = end.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) else { return Ok(None) };
            self.s.set_read_timeout(Some(left)).map_err(|e| e.to_string())?;
            match self.s.read(&mut buf) {
                Ok(0) => return Err("Bluetooth went away".into()),
                Ok(n) => self.r.push(&buf[..n]),
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted) => {}
                Err(e) => return Err(e.to_string()),
            }
        }
    }
}

/// Follow the paired devices on a thread of its own; reconnect when
/// vinculumd restarts. A quiet connection is asked for its status after
/// 30 s and dropped as hung 10 s later.
pub fn follow() -> (Receiver<BtMsg>, Sender<BtMsg>) {
    let (tx, rx) = channel();
    let t = tx.clone();
    let mut backoff = atrium_listen::Backoff::new(Duration::from_secs(3));
    std::thread::spawn(move || loop {
        let began = Instant::now();
        if let Ok(mut c) = Conn::open() {
            let mut probing = false;
            let mut ok = c.send(&Request::Devices).is_ok();
            while ok {
                let m = match c.next(Duration::from_secs(if probing { 10 } else { 30 })) {
                    Ok(Some(e)) => e,
                    Ok(None) if !probing => {
                        probing = true;
                        ok = c.send(&Request::Status).is_ok();
                        continue;
                    }
                    Ok(None) | Err(_) => break,
                };
                probing = false;
                let sent = match m {
                    Event::Devices { devices } => t.send(BtMsg::Devices(Some(devices))),
                    Event::Battery { address, percent } => t.send(BtMsg::Battery { address, percent }),
                    Event::Connected { .. } | Event::Disconnected { .. } | Event::Paired { .. } => {
                        ok = c.send(&Request::Devices).is_ok();
                        Ok(())
                    }
                    _ => Ok(()),
                };
                if sent.is_err() {
                    return;
                }
            }
        }
        if t.send(BtMsg::Devices(None)).is_err() {
            return;
        }
        backoff.after(path(), began.elapsed());
    });
    (rx, tx)
}

pub fn act(tx: Sender<BtMsg>, a: BtAction) {
    std::thread::spawn(move || {
        let r = Conn::open().and_then(|mut c| {
            let BtAction::Disconnect(address) = a.clone();
            c.send(&Request::Disconnect { address })?;
            // The answer comes after any events already on their way.
            let end = Instant::now() + Duration::from_secs(5);
            loop {
                let left = end.saturating_duration_since(Instant::now());
                match c.next(left)? {
                    Some(Event::Ok) => return Ok(()),
                    Some(Event::Error { why }) => return Err(why),
                    Some(_) => {}
                    None => return Err("Bluetooth didn't answer".into()),
                }
            }
        });
        match &r {
            Ok(()) => eprintln!("forum-bar: bluetooth: {a:?}"),
            Err(e) => eprintln!("forum-bar: bluetooth: {a:?} failed: {e}"),
        }
        let _ = tx.send(BtMsg::Done(r));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(n: u8) -> Addr {
        Addr { bytes: [n; 6], random: false }
    }

    fn dev(n: u8, name: &str, connected: bool, battery: Option<u8>) -> DeviceInfo {
        DeviceInfo { address: a(n), identity: None, name: name.into(), authenticated: true, secure_connections: true, keyboard_allowed: true, connected, battery }
    }

    #[test]
    fn shown_while_connected_with_battery_levels() {
        assert!(!BtShown::default().shown());
        let mut n = BtShown { devices: Some(vec![dev(1, "Mouse", false, None)]), note: None };
        assert!(!n.shown(), "paired but not connected");
        n.devices = Some(vec![dev(1, "Mouse", true, Some(60)), dev(2, "", true, None), dev(3, "Keys", false, Some(5))]);
        assert!(n.shown());
        assert!(!n.low(), "a disconnected device's level does not count");
        let (items, acts) = menu(&n);
        assert_eq!(items.len(), 3);
        assert_eq!(acts[1], Some(BtAction::Disconnect(a(1))));
        assert_eq!(acts[2], Some(BtAction::Disconnect(a(2))));
        let labels: Vec<String> = n.connected().map(label).collect();
        assert_eq!(labels, ["Mouse", "02:02:02:02:02:02"], "an unnamed device goes by its address");
        n.battery(a(1), 15);
        assert!(n.low());
        assert_eq!(n.devices.as_ref().unwrap()[0].battery, Some(15));
        n.battery(a(9), 1);
        assert!(n.devices.as_ref().unwrap().iter().all(|d| d.address != a(9)), "an unknown device is ignored");
    }

    #[test]
    fn follows_devices_and_battery_over_the_socket() {
        let dir = std::env::temp_dir().join(format!("forum-bar-bt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("control.sock");
        let l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        std::env::set_var("FORUM_BAR_BT_SOCK", &sock);
        let (rx, _tx) = follow();
        let (mut s, _) = l.accept().unwrap();
        let mut r = Reader::default();
        let mut buf = [0u8; 256];
        let mut got = Vec::new();
        while got.len() < 2 {
            let n = s.read(&mut buf).unwrap();
            r.push(&buf[..n]);
            while let Some(q) = r.next::<Request>().unwrap() {
                got.push(q);
            }
        }
        assert_eq!(got, [Request::Hello { agent: false }, Request::Devices]);
        s.write_all(&encode(&Event::Devices { devices: vec![dev(1, "Mouse", true, None)] })).unwrap();
        s.write_all(&encode(&Event::Battery { address: a(1), percent: 42 })).unwrap();
        let wait = Duration::from_secs(5);
        assert!(matches!(rx.recv_timeout(wait).unwrap(), BtMsg::Devices(Some(v)) if v.len() == 1));
        assert!(matches!(rx.recv_timeout(wait).unwrap(), BtMsg::Battery { address, percent: 42 } if address == a(1)));
        drop(s);
        assert!(matches!(rx.recv_timeout(wait).unwrap(), BtMsg::Devices(None)), "a lost connection says so");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
