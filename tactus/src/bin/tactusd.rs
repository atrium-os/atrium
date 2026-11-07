//! tactusd — Atrium's input service (docs/spec/atrium-input.md §2).
//!
//! Finds input devices, reads their reports, decodes them with the
//! `tactus` crate (key repeat and the pointer stage included) and sends
//! the canonical events to every connected client (frescod) on one
//! socket. Runs jailed as `_tactus` with only the input device nodes
//! and its socket directory.
//!
//! Sources today: hidbus through `/dev/hidraw*` (USB and I2C), and
//! Bluetooth HID devices that vinculumd hands over (bluetooth.md §10):
//! one SEQPACKET fd per device, read like a hidraw node, and the i8042
//! PS/2 keyboard (`/dev/atkbd0` in K_RAW, scancode set 1; tactus-console
//! hands it over from the console) and pointer (`/dev/bpsm0`, sysmouse
//! packets).
//!
//! Usage:
//!   tactusd [--socket PATH]   serve (default /atrium/sockets/tactus/tactus.sock)
//!   tactusd --list            print the devices found and exit
//!   tactusd --watch PATH      connect to a running tactusd, print events
//!
//! Environment: TACTUS_PEERS (uids allowed to connect, comma-separated, or
//! `any`; default 65010 (_frescod) and 0), TACTUS_REPEAT,
//! TACTUS_POINTER (see tactus::repeat / tactus::pointer),
//! TACTUS_BLUETOOTH_SOCKET (vinculumd's HID socket, default
//! /atrium/sockets/bluetooth-hid/hid.sock; `off` to not connect).

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tactus::decode::{Decoder, Input};
use tactus::hid::{Class, Descriptor};
use tactus::pointer::{Curve, Stage};
use tactus::ps2::{self, Source};
use tactus::repeat::{Repeat, Settings};
use tactus::touchpad::{Out as TouchOut, Settings as TouchSettings, Touchpad};
use tactus::wire::{self, Bus, Message};
use vinculum_hid_wire as bt;

const DEFAULT_SOCKET: &str = "/atrium/sockets/tactus/tactus.sock";
const MAX_HIDRAW: u32 = 64;
/// Touchpad fingers landing this soon after a key press are ignored.
const TYPING: Duration = Duration::from_millis(300);
/// _frescod, the one client in production.
const FRESCOD_UID: u32 = 65010;

struct State {
    /// The last key press, from any keyboard: touchpad fingers landing
    /// soon after it are palms (the typing hand), not pointing.
    last_key: Option<Instant>,
    clients: Vec<UnixStream>,
    devices: HashMap<u32, Message>,
    repeat: Repeat,
}

struct Hub {
    state: Mutex<State>,
    wake: Condvar,
    curve: Curve,
}

impl State {
    fn send(&mut self, m: &Message) {
        self.clients.retain_mut(|c| match wire::write(c, m) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("tactusd: dropping a client: {e}");
                false
            }
        });
    }
}

// ---- hidraw ----------------------------------------------------------

const HID_MAX_DESCRIPTOR_SIZE: usize = 4096;

#[repr(C)]
struct HidrawReportDescriptor {
    size: u32,
    value: [u8; HID_MAX_DESCRIPTOR_SIZE],
}

#[repr(C)]
#[derive(Default)]
struct HidrawDevinfo {
    bustype: u32,
    vendor: i16,
    product: i16,
}

// FreeBSD hidraw(4)'s Linux-compatible ioctls (sys/dev/hid/hidraw.h).
#[cfg(target_os = "freebsd")]
mod ioc {
    pub const HIDIOCGRDESCSIZE: libc::c_ulong = 0x4004_551e;
    pub const HIDIOCGRDESC: libc::c_ulong = 0x2000_551f;
    pub const HIDIOCGRAWINFO: libc::c_ulong = 0x4008_5520;
    pub const HIDIOCGRAWNAME_128: libc::c_ulong = 0x4080_5521;
}
// Linux hidraw, for running tactusd on a development host.
#[cfg(not(target_os = "freebsd"))]
mod ioc {
    pub const HIDIOCGRDESCSIZE: libc::c_ulong = 0x8004_4801;
    pub const HIDIOCGRDESC: libc::c_ulong = 0x9004_4802;
    pub const HIDIOCGRAWINFO: libc::c_ulong = 0x8008_4803;
    pub const HIDIOCGRAWNAME_128: libc::c_ulong = 0x8080_4804;
}

fn descriptor(f: &File) -> Option<Vec<u8>> {
    let fd = f.as_raw_fd();
    let mut size: i32 = 0;
    if unsafe { libc::ioctl(fd, ioc::HIDIOCGRDESCSIZE as _, &mut size) } < 0 {
        return None;
    }
    if size < 2 || size as usize > HID_MAX_DESCRIPTOR_SIZE {
        return None;
    }
    let mut d = Box::new(HidrawReportDescriptor { size: size as u32, value: [0; HID_MAX_DESCRIPTOR_SIZE] });
    if unsafe { libc::ioctl(fd, ioc::HIDIOCGRDESC as _, &mut *d as *mut HidrawReportDescriptor) } < 0 {
        return None;
    }
    Some(d.value[..size as usize].to_vec())
}

fn info(f: &File, path: &str) -> (Bus, u16, u16, String) {
    let fd = f.as_raw_fd();
    let mut i = HidrawDevinfo::default();
    let ok = unsafe { libc::ioctl(fd, ioc::HIDIOCGRAWINFO as _, &mut i as *mut HidrawDevinfo) } >= 0;
    let bus = match (ok, i.bustype) {
        (false, _) => Bus::Other,
        (_, 0x03) => Bus::Usb,
        (_, 0x18) => Bus::I2c,
        (_, 0x05) => Bus::Bluetooth,
        (_, 0x06) => Bus::Virtual,
        (_, 0x11) => Bus::Ps2,
        _ => Bus::Other,
    };
    let mut name = [0u8; 128];
    let n = unsafe { libc::ioctl(fd, ioc::HIDIOCGRAWNAME_128 as _, name.as_mut_ptr()) };
    let name = if n > 0 {
        let end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
        String::from_utf8_lossy(&name[..end]).trim().to_string()
    } else {
        String::new()
    };
    let name = if name.is_empty() { path.trim_start_matches("/dev/").to_string() } else { name };
    (bus, i.vendor as u16, i.product as u16, name)
}

/// Open `path` and parse it; `None` if it carries nothing Tactus decodes.
fn probe(path: &str) -> Option<(File, Decoder, Bus, u16, u16, String)> {
    let f = File::open(path).ok()?;
    let d = Decoder::new(Descriptor::parse(&descriptor(&f)?)?);
    if !d.decodes_anything() {
        return None;
    }
    let (bus, vendor, product, name) = info(&f, path);
    Some((f, d, bus, vendor, product, name))
}

// ---- devices ---------------------------------------------------------

struct Devices {
    next_id: u32,
    /// hidraw nodes being read, or found to carry nothing for us.
    claimed: HashSet<String>,
}

fn scan(hub: &Arc<Hub>, devs: &Arc<Mutex<Devices>>) {
    for n in 0..MAX_HIDRAW {
        let path = format!("/dev/hidraw{n}");
        if !std::path::Path::new(&path).exists() || devs.lock().unwrap().claimed.contains(&path) {
            continue;
        }
        // A node another reader holds (hidraw opens are exclusive on some
        // kernels) is tried again at the next scan.
        if File::open(&path).is_err() {
            continue;
        }
        let Some((file, dec, bus, vendor, product, name)) = probe(&path) else {
            devs.lock().unwrap().claimed.insert(path);
            continue;
        };
        let id = {
            let mut d = devs.lock().unwrap();
            d.claimed.insert(path.clone());
            d.next_id += 1;
            d.next_id
        };
        let classes = dec.descriptor().classes();
        eprintln!("tactusd: device {id} {path}: {name:?} {bus:?} {vendor:04x}:{product:04x} {classes:?}");
        let rec = Message::Device { id, bus, classes, vendor, product, name };
        {
            let mut s = hub.state.lock().unwrap();
            s.send(&rec);
            s.devices.insert(id, rec);
        }
        let (hub, devs) = (hub.clone(), devs.clone());
        let _ = std::thread::Builder::new().name(format!("tactus:{path}")).spawn(move || {
            read_device(&hub, id, file, dec);
            eprintln!("tactusd: device {id} {path} gone");
            devs.lock().unwrap().claimed.remove(&path);
        });
    }
}

fn read_device(hub: &Hub, id: u32, mut file: File, dec: Decoder) {
    run_device(hub, id, dec, true, |buf| match file.read(buf) {
        Ok(0) | Err(_) => Got::Gone,
        Ok(n) => Got::Report(n),
    });
}

/// What a source's read gave.
enum Got {
    /// A report of this many bytes, report id first if the descriptor
    /// uses ids (as hidraw gives it).
    Report(usize),
    /// The source lost reports: release what is held.
    Dropped,
    /// Nothing this time (a packet that is not an input report).
    Skip,
    Gone,
}

/// Reads one device until it goes away, decoding and sending its events.
/// `keyboard`: false drops keyboard-page keys (a Bluetooth bond without
/// keyboard rights, bluetooth.md §7).
fn run_device(hub: &Hub, id: u32, mut dec: impl Source, keyboard: bool, mut read: impl FnMut(&mut [u8]) -> Got) {
    let mut stage = Stage::new(hub.curve);
    let mut pad = Touchpad::new(TouchSettings::default());
    let mut buf = vec![0u8; 65536];
    let mut ev = Vec::new();
    loop {
        let n = match read(&mut buf) {
            Got::Gone => break,
            Got::Skip => continue,
            Got::Dropped => {
                release(hub, id, &mut dec, &mut ev, keyboard);
                continue;
            }
            Got::Report(n) => n,
        };
        let ts_ns = wire::now_ns();
        let now = Instant::now();
        ev.clear();
        dec.report(&buf[..n], &mut ev);
        if !keyboard {
            ev.retain(|e| !matches!(e, Input::Key { usage, .. } if usage >> 16 == 0x07));
        }
        if ev.is_empty() {
            continue;
        }
        let mut s = hub.state.lock().unwrap();
        for e in ev.drain(..) {
            let m = match e {
                Input::Key { usage, down } => {
                    s.repeat.key(id, usage, down, now);
                    if down {
                        s.last_key = Some(now);
                    }
                    Message::Key { id, ts_ns, usage, down, repeat: false }
                }
                Input::Touch { contacts, button } => {
                    let typing = s.last_key.is_some_and(|t| now.saturating_duration_since(t) < TYPING);
                    let mut out = Vec::new();
                    pad.frame(now, &contacts, button, typing, &mut out);
                    for o in out {
                        s.send(&match o {
                            TouchOut::Motion { dx, dy } => {
                                Message::Motion { id, ts_ns, dx, dy, raw_dx: dx.round() as i32, raw_dy: dy.round() as i32 }
                            }
                            TouchOut::Button { button, down } => Message::Button { id, ts_ns, usage: 0x0009_0000 | u32::from(button), down },
                            TouchOut::Scroll { dx, dy, phase } => Message::ScrollPixels { id, ts_ns, dx, dy, phase },
                            TouchOut::Gesture { kind, fingers, phase, dx, dy, scale } => {
                                Message::Gesture { id, ts_ns, kind, fingers, phase, dx, dy, scale }
                            }
                        });
                    }
                    continue;
                }
                Input::Button { usage, down } => Message::Button { id, ts_ns, usage, down },
                Input::Motion { dx, dy } => {
                    let (ax, ay) = stage.motion(dx, dy, now);
                    Message::Motion { id, ts_ns, dx: ax, dy: ay, raw_dx: dx, raw_dy: dy }
                }
                Input::Absolute { x, y } => Message::Absolute { id, ts_ns, x, y },
                Input::Scroll { axis, detents } => Message::Scroll { id, ts_ns, axis, value_120: detents.saturating_mul(120) },
            };
            s.send(&m);
        }
        hub.wake.notify_all();
    }
    // Gone: release what it held, then say so.
    release(hub, id, &mut dec, &mut ev, keyboard);
    let mut s = hub.state.lock().unwrap();
    s.devices.remove(&id);
    s.send(&Message::Gone { id });
    hub.wake.notify_all();
}

/// Releases every key and button device `id` holds and stops its repeat
/// (keyboard keys only if they were sent: `keyboard`).
fn release(hub: &Hub, id: u32, dec: &mut impl Source, ev: &mut Vec<Input>, keyboard: bool) {
    let ts_ns = wire::now_ns();
    ev.clear();
    dec.release_all(ev);
    if !keyboard {
        ev.retain(|e| !matches!(e, Input::Key { usage, .. } if usage >> 16 == 0x07));
    }
    let mut s = hub.state.lock().unwrap();
    s.repeat.gone(id);
    for e in ev.drain(..) {
        match e {
            Input::Key { usage, .. } => s.send(&Message::Key { id, ts_ns, usage, down: false, repeat: false }),
            Input::Button { usage, .. } => s.send(&Message::Button { id, ts_ns, usage, down: false }),
            _ => {}
        }
    }
    hub.wake.notify_all();
}

fn repeat_loop(hub: &Hub) {
    let mut s = hub.state.lock().unwrap();
    loop {
        let now = Instant::now();
        if let Some((id, usage)) = s.repeat.due(now) {
            s.send(&Message::Key { id, ts_ns: wire::now_ns(), usage, down: true, repeat: true });
            continue;
        }
        s = match s.repeat.deadline() {
            Some(t) => hub.wake.wait_timeout(s, t.saturating_duration_since(now)).unwrap().0,
            None => hub.wake.wait(s).unwrap(),
        };
    }
}

// ---- PS/2 (atrium-input.md §3.2) -------------------------------------

/// The i8042 keyboard and pointer nodes, tried every 2 s while not open.
/// `/dev/atkbd0` is busy until tactus-console releases it from the
/// console's kbdmux; `/dev/bpsm0` (psm's blocking node: `/dev/psm0` reads
/// never wait) until nothing else has it open.
const PS2_KEYBOARD: &str = "/dev/atkbd0";
const PS2_POINTER: &str = "/dev/bpsm0";

// sys/kbio.h KDSKBMODE (_IOWINT('K', 7)), sys/mouse.h MOUSE_SETLEVEL
// (_IOW('M', 5, int)).
const KDSKBMODE: libc::c_ulong = 0x2004_4b07;
const K_RAW: libc::c_int = 0;
const MOUSE_SETLEVEL: libc::c_ulong = 0x8004_4d05;

fn ps2_loop(hub: &Arc<Hub>, devs: &Arc<Mutex<Devices>>) {
    let mut said: HashSet<&str> = HashSet::new();
    loop {
        for path in [PS2_KEYBOARD, PS2_POINTER] {
            if devs.lock().unwrap().claimed.contains(path) || !std::path::Path::new(path).exists() {
                continue;
            }
            match ps2_open(path) {
                Ok(f) => {
                    said.remove(path);
                    ps2_start(hub, devs, path, f);
                }
                Err(e) if said.insert(path) => eprintln!("tactusd: {path}: {e}; trying again every 2 s"),
                Err(_) => {}
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

fn ps2_open(path: &str) -> std::io::Result<File> {
    let f = File::open(path)?;
    let fd = f.as_raw_fd();
    // SAFETY: plain ioctls on our fd; KDSKBMODE takes its int by value,
    // MOUSE_SETLEVEL by pointer.
    let r = if path == PS2_KEYBOARD {
        unsafe { libc::ioctl(fd, KDSKBMODE as _, K_RAW) }
    } else {
        let mut level: libc::c_int = 1;
        unsafe { libc::ioctl(fd, MOUSE_SETLEVEL as _, &mut level as *mut libc::c_int) }
    };
    if r < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(f)
}

fn ps2_start(hub: &Arc<Hub>, devs: &Arc<Mutex<Devices>>, path: &'static str, mut file: File) {
    let keyboard = path == PS2_KEYBOARD;
    let id = {
        let mut d = devs.lock().unwrap();
        d.claimed.insert(path.to_string());
        d.next_id += 1;
        d.next_id
    };
    let node = path.trim_start_matches("/dev/");
    let (name, classes) = if keyboard {
        (format!("AT keyboard ({node})"), vec![Class::Keyboard, Class::MediaKeys, Class::SystemKeys])
    } else {
        (format!("PS/2 pointer ({node})"), vec![Class::Pointer])
    };
    eprintln!("tactusd: device {id} {path}: {name:?} Ps2 {classes:?}");
    let rec = Message::Device { id, bus: Bus::Ps2, classes, vendor: 0, product: 0, name };
    {
        let mut s = hub.state.lock().unwrap();
        s.send(&rec);
        s.devices.insert(id, rec);
    }
    let (hub, devs) = (hub.clone(), devs.clone());
    let _ = std::thread::Builder::new().name(format!("tactus:{node}")).spawn(move || {
        let read = |buf: &mut [u8]| match file.read(buf) {
            Ok(0) | Err(_) => Got::Gone,
            Ok(n) => Got::Report(n),
        };
        if keyboard {
            run_device(&hub, id, ps2::Set1::new(), true, read);
        } else {
            run_device(&hub, id, ps2::SysMouse::new(), true, read);
        }
        eprintln!("tactusd: device {id} {path} gone");
        devs.lock().unwrap().claimed.remove(path);
    });
}

// ---- Bluetooth HID (bluetooth.md §10) --------------------------------

const BT_SOCKET: &str = "/atrium/sockets/bluetooth-hid/hid.sock";

/// Connects to vinculumd's HID socket, retrying every 2 s while the
/// Bluetooth service is down or absent.
fn bluetooth(hub: &Arc<Hub>, devs: &Arc<Mutex<Devices>>, path: &str) {
    let mut said = false;
    loop {
        match UnixStream::connect(path) {
            Ok(c) => {
                eprintln!("tactusd: Bluetooth HID from {path}");
                said = false;
                let e = bt_session(hub, devs, c);
                eprintln!("tactusd: Bluetooth HID connection ended: {e}");
            }
            Err(e) if !said => {
                eprintln!("tactusd: no Bluetooth HID at {path} ({e}); retrying");
                said = true;
            }
            Err(_) => {}
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// A device whose descriptor is still arriving.
struct BtPending {
    fd: OwnedFd,
    record: bt::Record,
    keyboard_allowed: bool,
    len: usize,
    desc: Vec<u8>,
}

fn bt_session(hub: &Arc<Hub>, devs: &Arc<Mutex<Devices>>, mut c: UnixStream) -> std::io::Error {
    if let Err(e) = c.write_all(&bt::encode(&bt::ToVinculum::Hello { version: bt::VERSION })) {
        return e;
    }
    let mut r = bt::Reader::default();
    // Fds arrive with the first byte of their Attach, so in Attach order.
    let mut fds: VecDeque<OwnedFd> = VecDeque::new();
    let mut pending: HashMap<u64, BtPending> = HashMap::new();
    let mut buf = vec![0u8; 2 * bt::MAX_MESSAGE];
    loop {
        let (n, got) = match bt::recv_with_fd(&c, &mut buf) {
            Ok((0, _)) => return std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "vinculumd closed"),
            Ok(x) => x,
            Err(e) => return e,
        };
        fds.extend(got);
        r.push(&buf[..n]);
        loop {
            let m = match r.next::<bt::ToTactus>() {
                Ok(Some(m)) => m,
                Ok(None) => break,
                Err(e) => return std::io::Error::other(format!("bad message from vinculumd: {e:?}")),
            };
            match m {
                bt::ToTactus::Attach { device, record, transport, keyboard_allowed, descriptor_len, descriptor } => {
                    let Some(fd) = fds.pop_front() else {
                        return std::io::Error::other("Attach without an fd");
                    };
                    let len = descriptor_len as usize;
                    if len > bt::MAX_DESCRIPTOR || descriptor.len() > len {
                        eprintln!("tactusd: Bluetooth device {device:x}: bad descriptor length {len}");
                        continue;
                    }
                    eprintln!("tactusd: Bluetooth device {device:x} {:?} ({transport:?})", record.name);
                    pending.insert(device, BtPending { fd, record, keyboard_allowed, len, desc: descriptor });
                }
                bt::ToTactus::Descriptor { device, offset, bytes } => {
                    let ok = pending
                        .get_mut(&device)
                        .is_some_and(|p| offset as usize == p.desc.len() && p.desc.len() + bytes.len() <= p.len);
                    if !ok {
                        eprintln!("tactusd: Bluetooth device {device:x}: descriptor chunk out of order; dropped");
                        pending.remove(&device);
                        continue;
                    }
                    pending.get_mut(&device).unwrap().desc.extend(bytes);
                }
                // Its fd reaches EOF too, which ends its reader.
                bt::ToTactus::Detach { device } => {
                    pending.remove(&device);
                }
                bt::ToTactus::Battery { .. } => {}
            }
        }
        let done: Vec<u64> = pending.iter().filter(|(_, p)| p.desc.len() == p.len).map(|(&d, _)| d).collect();
        for d in done {
            let p = pending.remove(&d).unwrap();
            bt_start(hub, devs, d, p);
        }
    }
}

fn bt_start(hub: &Arc<Hub>, devs: &Arc<Mutex<Devices>>, device: u64, p: BtPending) {
    let Some(desc) = Descriptor::parse(&p.desc) else {
        eprintln!("tactusd: Bluetooth device {device:x}: descriptor did not parse");
        return;
    };
    let dec = Decoder::new(desc);
    if !dec.decodes_anything() {
        eprintln!("tactusd: Bluetooth device {device:x}: nothing to decode");
        return;
    }
    let id = {
        let mut d = devs.lock().unwrap();
        d.next_id += 1;
        d.next_id
    };
    let classes = dec.descriptor().classes();
    let bt::Record { name, vendor, product, .. } = p.record;
    eprintln!(
        "tactusd: device {id} bluetooth:{device:x}: {name:?} {vendor:04x}:{product:04x} {classes:?}{}",
        if p.keyboard_allowed { "" } else { " (no keyboard rights)" }
    );
    let rec = Message::Device { id, bus: Bus::Bluetooth, classes, vendor, product, name };
    {
        let mut s = hub.state.lock().unwrap();
        s.send(&rec);
        s.devices.insert(id, rec);
    }
    let hub = hub.clone();
    let keyboard = p.keyboard_allowed;
    let mut file = File::from(p.fd);
    let _ = std::thread::Builder::new().name(format!("tactus:bt:{device:x}")).spawn(move || {
        let mut pkt = vec![0u8; bt::MAX_PACKET];
        run_device(&hub, id, dec, keyboard, |buf| {
            let n = match file.read(&mut pkt) {
                Ok(0) | Err(_) => return Got::Gone,
                Ok(n) => n,
            };
            match bt::parse_packet(&pkt[..n]) {
                Some((h, report)) if h.kind == bt::Kind::Input => {
                    // The common decoder takes reports as hidraw gives them:
                    // report id first when the descriptor uses ids.
                    let off = usize::from(h.report_id != 0);
                    if off + report.len() > buf.len() {
                        return Got::Skip;
                    }
                    buf[0] = h.report_id;
                    buf[off..off + report.len()].copy_from_slice(report);
                    Got::Report(off + report.len())
                }
                Some((h, _)) if h.kind == bt::Kind::Dropped => Got::Dropped,
                _ => Got::Skip,
            }
        });
        eprintln!("tactusd: device {id} bluetooth:{device:x} gone");
    });
}

// ---- clients ---------------------------------------------------------

#[cfg(target_os = "freebsd")]
fn peer_uid(s: &UnixStream) -> Option<u32> {
    let (mut uid, mut gid) = (0, 0);
    (unsafe { libc::getpeereid(s.as_raw_fd(), &mut uid, &mut gid) } == 0).then_some(uid)
}

#[cfg(target_os = "linux")]
fn peer_uid(s: &UnixStream) -> Option<u32> {
    let mut c = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let r = unsafe {
        libc::getsockopt(s.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, &mut c as *mut _ as *mut libc::c_void, &mut len)
    };
    (r == 0).then_some(c.uid)
}

fn serve_clients(hub: &Hub, listener: UnixListener) {
    let peers: Option<Vec<u32>> = match std::env::var("TACTUS_PEERS") {
        Ok(v) if v.trim() == "any" => None,
        Ok(v) => Some(v.split(',').filter_map(|u| u.trim().parse().ok()).collect()),
        Err(_) => Some(vec![FRESCOD_UID, 0]),
    };
    for c in listener.incoming() {
        let Ok(mut c) = c else { continue };
        let uid = peer_uid(&c);
        if let Some(p) = &peers {
            if !uid.is_some_and(|u| p.contains(&u)) {
                eprintln!("tactusd: refused a client (uid {uid:?})");
                continue;
            }
        }
        let _ = c.set_write_timeout(Some(Duration::from_millis(250)));
        let mut s = hub.state.lock().unwrap();
        let mut hello = vec![Message::Hello { version: wire::VERSION }];
        let mut ids: Vec<&u32> = s.devices.keys().collect();
        ids.sort();
        hello.extend(ids.into_iter().map(|i| s.devices[i].clone()));
        if hello.iter().all(|m| wire::write(&mut c, m).is_ok()) {
            eprintln!("tactusd: client connected (uid {uid:?})");
            s.clients.push(c);
        }
    }
}

// ---- main ------------------------------------------------------------

fn watch(path: &str) -> std::io::Result<()> {
    let mut c = UnixStream::connect(path)?;
    loop {
        println!("{:?}", wire::read(&mut c)?);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned();
    if let Some(p) = arg("--watch") {
        if let Err(e) = watch(&p) {
            eprintln!("tactusd --watch: {e}");
            std::process::exit(1);
        }
        return;
    }
    if args.iter().any(|a| a == "--list") {
        for n in 0..MAX_HIDRAW {
            let path = format!("/dev/hidraw{n}");
            if let Some((_, d, bus, v, p, name)) = probe(&path) {
                println!("{path}: {name:?} {bus:?} {v:04x}:{p:04x} {:?}", d.descriptor().classes());
            }
        }
        return;
    }
    let sock = arg("--socket").unwrap_or_else(|| DEFAULT_SOCKET.to_string());
    let _ = std::fs::remove_file(&sock);
    let listener = match UnixListener::bind(&sock) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("tactusd: bind {sock}: {e}");
            std::process::exit(1);
        }
    };
    let hub = Arc::new(Hub {
        state: Mutex::new(State { last_key: None, clients: Vec::new(), devices: HashMap::new(), repeat: Repeat::new(Settings::from_env()) }),
        wake: Condvar::new(),
        curve: Curve::from_env(),
    });
    eprintln!("tactusd: serving {sock}");
    let devs = Arc::new(Mutex::new(Devices { next_id: 0, claimed: HashSet::new() }));
    {
        let hub = hub.clone();
        std::thread::spawn(move || serve_clients(&hub, listener));
    }
    {
        let hub = hub.clone();
        std::thread::spawn(move || repeat_loop(&hub));
    }
    {
        let (hub, devs) = (hub.clone(), devs.clone());
        std::thread::spawn(move || ps2_loop(&hub, &devs));
    }
    {
        let (hub, devs) = (hub.clone(), devs.clone());
        let path = std::env::var("TACTUS_BLUETOOTH_SOCKET").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| BT_SOCKET.into());
        if path != "off" {
            std::thread::spawn(move || bluetooth(&hub, &devs, &path));
        }
    }
    scan(&hub, &devs);
    hotplug(&hub, &devs);
}

#[cfg(target_os = "freebsd")]
fn hotplug(hub: &Arc<Hub>, devs: &Arc<Mutex<Devices>>) {
    use atrium_devevents::{DeviceWatcher, Event};
    let w = match DeviceWatcher::open() {
        Ok(w) => w,
        Err(e) => {
            eprintln!("tactusd: devd unavailable ({e}); rescanning every 2 s");
            return poll(hub, devs);
        }
    };
    loop {
        match w.recv() {
            Ok(Event::Added { devnode }) | Ok(Event::Removed { devnode }) if devnode.starts_with("/dev/hidraw") => {
                // A node that went away may come back as something else.
                devs.lock().unwrap().claimed.retain(|p| std::path::Path::new(p).exists());
                scan(hub, devs);
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("tactusd: devd: {e}; rescanning every 2 s");
                return poll(hub, devs);
            }
        }
    }
}

#[cfg(not(target_os = "freebsd"))]
fn hotplug(hub: &Arc<Hub>, devs: &Arc<Mutex<Devices>>) {
    poll(hub, devs)
}

fn poll(hub: &Arc<Hub>, devs: &Arc<Mutex<Devices>>) {
    loop {
        std::thread::sleep(Duration::from_secs(2));
        devs.lock().unwrap().claimed.retain(|p| std::path::Path::new(p).exists());
        scan(hub, devs);
    }
}
