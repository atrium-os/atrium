//! tactusd — Atrium's input service (docs/spec/atrium-input.md §2).
//!
//! Finds input devices, reads their reports, decodes them with the
//! `tactus` crate (key repeat and the pointer stage included) and sends
//! the canonical events to every connected client (frescod) on one
//! socket. Runs jailed as `_tactus` with only the input device nodes
//! and its socket directory.
//!
//! Sources today: hidbus through `/dev/hidraw*` (USB and I2C). PS/2
//! and Bluetooth HID are later steps (spec §8, T4/T6).
//!
//! Usage:
//!   tactusd [--socket PATH]   serve (default /atrium/sockets/tactus/tactus.sock)
//!   tactusd --list            print the devices found and exit
//!   tactusd --watch PATH      connect to a running tactusd, print events
//!
//! Environment: TACTUS_PEERS (uids allowed to connect, comma-separated, or
//! `any`; default 65010 (_frescod) and 0), TACTUS_REPEAT,
//! TACTUS_POINTER (see tactus::repeat / tactus::pointer).

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tactus::decode::{Decoder, Input};
use tactus::hid::Descriptor;
use tactus::pointer::{Curve, Stage};
use tactus::repeat::{Repeat, Settings};
use tactus::touchpad::{Out as TouchOut, Settings as TouchSettings, Touchpad};
use tactus::wire::{self, Bus, Message};

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

fn read_device(hub: &Hub, id: u32, mut file: File, mut dec: Decoder) {
    let mut stage = Stage::new(hub.curve);
    let mut pad = Touchpad::new(TouchSettings::default());
    let mut buf = vec![0u8; 65536];
    let mut ev = Vec::new();
    loop {
        let n = match file.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let ts_ns = wire::now_ns();
        let now = Instant::now();
        ev.clear();
        dec.report(&buf[..n], &mut ev);
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
    let ts_ns = wire::now_ns();
    dec.release_all(&mut ev);
    let mut s = hub.state.lock().unwrap();
    s.repeat.gone(id);
    for e in ev.drain(..) {
        match e {
            Input::Key { usage, .. } => s.send(&Message::Key { id, ts_ns, usage, down: false, repeat: false }),
            Input::Button { usage, .. } => s.send(&Message::Button { id, ts_ns, usage, down: false }),
            _ => {}
        }
    }
    s.devices.remove(&id);
    s.send(&Message::Gone { id });
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
