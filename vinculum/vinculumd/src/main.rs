//! vinculumd: Vinculum's host daemon (docs/spec/bluetooth.md §6).
//!
//! B1 is the skeleton: one controller, reached through `vinculum-hci`'s
//! socket (or straight to Rootcanal over TCP for development), driven by the
//! sans-I/O `vinculum_host::Host`. The service sockets (control, HID, audio,
//! apps) arrive with B2–B6. Until then it has development modes, used by
//! `vinculum/harness/e2e.sh`:
//!
//! ```text
//! vinculumd --transport unix:PATH|tcp:HOST:PORT
//!           [--scan SECS] [--connect NAME] [--att-mtu] [--timeout SECS]
//!           [--pair [--reconnect] [--passkey-file PATH]]
//!           [--keys DIR --machine-key PATH | --arca SOCKET]
//! ```
//!
//! `--connect NAME` scans until a device advertising NAME appears, connects
//! over LE, and with `--att-mtu` sends an ATT Exchange MTU Request on the
//! fixed ATT channel and waits for the response, then disconnects. Exit 0
//! when everything asked for happened.
//!
//! `--pair` pairs (LE Secure Connections, KeyboardDisplay) after
//! connecting, answering every prompt "yes" (development only: the real
//! daemon asks the user through the trusted UI, spec §7). A displayed
//! passkey is written to `--passkey-file` for the test peer to type. With
//! `--reconnect` it then disconnects, reconnects and encrypts with the
//! bond's key.
//!
//! Bonds persist in a key store (spec §9): `--keys DIR --machine-key PATH`
//! is the stock-FreeBSD sealed file store, `--arca SOCKET` the Arca machine
//! store (built with the `atrium` feature). Without either they live in
//! memory for the run. A peer with a stored bond is encrypted with it on
//! connect instead of being paired again.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::time::Instant;

mod policy;

use vinculum_hci_proto::h4::Decoder;
use vinculum_host::{Host, HostEvent, PairConfig, Prompt};
use vinculum_keys::{Address, KeyStore, MemoryStore};
use vinculum_smp::pairing::IoCap;

enum Link {
    Unix(UnixStream),
    Tcp(TcpStream),
}

impl Link {
    fn open(spec: &str) -> std::io::Result<Link> {
        if let Some(p) = spec.strip_prefix("unix:") {
            Ok(Link::Unix(UnixStream::connect(p)?))
        } else if let Some(a) = spec.strip_prefix("tcp:") {
            let s = TcpStream::connect(a)?;
            s.set_nodelay(true)?;
            Ok(Link::Tcp(s))
        } else {
            Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "transport must be unix:PATH or tcp:HOST:PORT"))
        }
    }
    fn fd(&self) -> RawFd {
        match self {
            Link::Unix(s) => s.as_raw_fd(),
            Link::Tcp(s) => s.as_raw_fd(),
        }
    }
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Link::Unix(s) => s.read(b),
            Link::Tcp(s) => s.read(b),
        }
    }
    fn write_all(&mut self, b: &[u8]) -> std::io::Result<()> {
        match self {
            Link::Unix(s) => s.write_all(b),
            Link::Tcp(s) => s.write_all(b),
        }
    }
}

/// poll(2) one fd for input, up to `timeout_ms`. True if readable.
fn wait(fd: RawFd, timeout_ms: i32) -> bool {
    let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    // SAFETY: one valid, exclusively borrowed pollfd; count 1.
    let r = unsafe { libc::poll(&mut p, 1, timeout_ms) };
    r > 0 && p.revents != 0
}

struct Opts {
    transport: String,
    scan_secs: Option<u64>,
    connect: Option<String>,
    att_mtu: bool,
    timeout_secs: u64,
    pair: bool,
    reconnect: bool,
    passkey_file: Option<String>,
    keys: Option<String>,
    machine_key: Option<String>,
    arca: Option<String>,
}

fn opts() -> Opts {
    let usage = || -> ! {
        eprintln!("usage: vinculumd --transport unix:PATH|tcp:HOST:PORT [--scan SECS] [--connect NAME] [--att-mtu] [--timeout SECS] [--pair [--reconnect] [--passkey-file PATH]] [--keys DIR --machine-key PATH | --arca SOCKET]");
        std::process::exit(2)
    };
    let mut o = Opts { transport: String::new(), scan_secs: None, connect: None, att_mtu: false, timeout_secs: 20, pair: false,
        reconnect: false,
        passkey_file: None,
        keys: None,
        machine_key: None,
        arca: None,
    };
    let mut a = std::env::args().skip(1);
    while let Some(k) = a.next() {
        match k.as_str() {
            "--transport" => o.transport = a.next().unwrap_or_else(|| usage()),
            "--scan" => o.scan_secs = Some(a.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage())),
            "--connect" => o.connect = Some(a.next().unwrap_or_else(|| usage())),
            "--att-mtu" => o.att_mtu = true,
            "--pair" => o.pair = true,
            "--reconnect" => o.reconnect = true,
            "--passkey-file" => o.passkey_file = Some(a.next().unwrap_or_else(|| usage())),
            "--keys" => o.keys = Some(a.next().unwrap_or_else(|| usage())),
            "--machine-key" => o.machine_key = Some(a.next().unwrap_or_else(|| usage())),
            "--arca" => o.arca = Some(a.next().unwrap_or_else(|| usage())),
            "--timeout" => o.timeout_secs = a.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage()),
            _ => usage(),
        }
    }
    if o.transport.is_empty() || o.keys.is_some() != o.machine_key.is_some() || (o.keys.is_some() && o.arca.is_some()) {
        usage()
    }
    o
}

#[derive(PartialEq)]
enum Phase {
    BringUp,
    Scanning,
    Connecting,
    WaitAtt(u16),
    Pairing(u16),
    Encrypting(u16),
    Reconnecting,
    Disconnecting,
    Done,
}

fn open_store(o: &Opts) -> Box<dyn KeyStore> {
    let fail = |e: vinculum_keys::Error| -> ! {
        // Fail closed: no store, no bonds and no pairing (spec §9).
        eprintln!("vinculumd: key store: {e}");
        std::process::exit(1)
    };
    if let (Some(dir), Some(kp)) = (&o.keys, &o.machine_key) {
        let mut k = vinculum_keys::load_or_create_machine_key(std::path::Path::new(kp)).unwrap_or_else(|e| fail(e));
        let s = vinculum_keys::FileStore::open(std::path::Path::new(dir), &k);
        zeroize::Zeroize::zeroize(&mut k);
        return Box::new(s.unwrap_or_else(|e| fail(e)));
    }
    if let Some(_sock) = &o.arca {
        #[cfg(feature = "atrium")]
        return Box::new(vinculum_keys::arca::ArcaStore::connect(std::path::Path::new(_sock)).unwrap_or_else(|e| fail(e)));
        #[cfg(not(feature = "atrium"))]
        {
            eprintln!("vinculumd: --arca needs the atrium build");
            std::process::exit(2)
        }
    }
    Box::new(MemoryStore::default())
}

fn main() {
    let o = opts();
    let mut store = open_store(&o);
    let mut bonds: Vec<(vinculum_keys::Id, vinculum_keys::Bond)> = Vec::new();
    for (id, b) in store.load_all().unwrap_or_else(|e| {
        eprintln!("vinculumd: key store: {e}");
        std::process::exit(1)
    }) {
        match b {
            Ok(b) => bonds.push((id, b)),
            Err(e) => eprintln!("vinculumd: bond {id:02x?} unusable ({e}); that device must pair again"),
        }
    }
    println!("bonds: {}", bonds.len());
    let mut names: std::collections::HashMap<vinculum_host::Peer, String> = Default::default();
    let mut link = Link::open(&o.transport).unwrap_or_else(|e| {
        eprintln!("vinculumd: transport {}: {e}", o.transport);
        std::process::exit(1)
    });
    let t0 = Instant::now();
    let now = || t0.elapsed().as_millis() as u64;
    let give_up = o.timeout_secs * 1000;
    let mut host = Host::new();
    let mut dec = Decoder::new();
    let mut buf = vec![0u8; 8192];
    let mut phase = Phase::BringUp;
    let mut scan_until: Option<u64> = None;
    let mut ok = true;
    let mut pending_events: Vec<HostEvent> = Vec::new();

    let mut pairable = policy::Pairable::default();
    if o.pair {
        // Development: running with --pair is the user asking to pair.
        pairable.asked(now());
    }
    host.start(now());
    loop {
        // Events can queue frames (a scan start after Ready), and frames
        // can't queue events, so: events first, then flush.
        while let Some(ev) = host.poll_event() {
            pending_events.push(ev);
        }
        while let Some(f) = host.poll_frame() {
            if std::env::var_os("VINCULUM_TRACE").is_some() {
                eprintln!("> {:02x?}", f.to_bytes());
            }
            if let Err(e) = link.write_all(&f.to_bytes()) {
                eprintln!("vinculumd: transport write: {e}");
                std::process::exit(1);
            }
        }
        for ev in std::mem::take(&mut pending_events) {
            match ev {
                HostEvent::Ready(info) => {
                    let a = info.address.unwrap_or_default();
                    println!(
                        "ready: address {:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}, HCI version {}, ACL {}x{}, LE {}x{}, ISO {}x{}",
                        a[0], a[1], a[2], a[3], a[4], a[5], info.hci_version, info.acl_buffers, info.acl_mtu,
                        info.le_buffers, info.le_mtu, info.iso_buffers, info.iso_mtu
                    );
                    if o.scan_secs.is_some() || o.connect.is_some() {
                        host.le_scan(true, now());
                        phase = Phase::Scanning;
                        scan_until = o.scan_secs.map(|s| now() + s * 1000);
                    } else {
                        phase = Phase::Done;
                    }
                }
                HostEvent::Fatal(why) => {
                    eprintln!("vinculumd: controller unusable: {why} ({:?})", host.stats());
                    std::process::exit(1);
                }
                HostEvent::Advertisement { peer, rssi, name } => {
                    if phase == Phase::Scanning {
                        if let (Some(want), Some(n)) = (&o.connect, &name) {
                            names.insert(peer, n.clone());
                            if want == n {
                                println!("found {n} at {peer} (rssi {rssi}); connecting");
                                host.le_scan(false, now());
                                host.le_connect(peer, now());
                                phase = Phase::Connecting;
                            }
                        }
                        if o.connect.is_none() {
                            println!("advertisement: {peer} rssi {rssi} name {}", name.as_deref().unwrap_or("-"));
                        }
                    }
                }
                HostEvent::Connected { handle, peer, transport, central } => {
                    println!("connected: handle {handle} {peer} {transport:?} {}", if central { "central" } else { "peripheral" });
                    let at = Address::from_msb(peer.address, peer.random);
                    if let Some(b) = vinculum_keys::find(bonds.iter().map(|(_, b)| b), &at) {
                        println!("bonded: {} ({}authenticated)", b.name, if b.authenticated { "" } else { "un" });
                        if let Err(e) = host.encrypt(handle, b.ltk, now()) {
                            eprintln!("vinculumd: encrypt: {e}");
                            ok = false;
                        }
                        phase = Phase::Encrypting(handle);
                    } else if o.pair {
                        ok &= start_pair(&mut host, &pairable, handle, now());
                        phase = Phase::Pairing(handle);
                    } else if o.att_mtu {
                        // ATT Exchange MTU Request (opcode 0x02), client MTU 512.
                        if let Err(e) = host.send_l2cap(handle, 0x0004, &[0x02, 0x00, 0x02], now()) {
                            eprintln!("vinculumd: send: {e}");
                            ok = false;
                        }
                        phase = Phase::WaitAtt(handle);
                    } else {
                        host.disconnect(handle, now());
                        phase = Phase::Disconnecting;
                    }
                }
                HostEvent::ConnectFailed { peer, status } => {
                    eprintln!("vinculumd: connect to {peer:?} failed: {status}");
                    ok = false;
                    phase = Phase::Done;
                }
                HostEvent::L2cap { handle, cid, payload } => {
                    println!("l2cap: handle {handle} cid 0x{cid:04x} {:02x?}", payload);
                    if phase == Phase::WaitAtt(handle) && cid == 0x0004 && payload.first() == Some(&0x03) && payload.len() == 3 {
                        println!("ATT MTU exchange: server MTU {}", u16::from_le_bytes([payload[1], payload[2]]));
                        host.disconnect(handle, now());
                        phase = Phase::Disconnecting;
                    }
                }
                HostEvent::Disconnected { handle, reason } => {
                    println!("disconnected: handle {handle} ({reason})");
                    if phase == Phase::Disconnecting {
                        phase = Phase::Done;
                    } else if phase == Phase::Reconnecting {
                        println!("reconnecting");
                        host.le_scan(true, now());
                        phase = Phase::Scanning;
                    } else if matches!(phase, Phase::Pairing(h) | Phase::Encrypting(h) | Phase::WaitAtt(h) if h == handle) {
                        eprintln!("vinculumd: link lost");
                        ok = false;
                        phase = Phase::Done;
                    }
                }
                HostEvent::PairingPrompt { handle, prompt } => {
                    match prompt {
                        Prompt::Confirm { number: Some(n) } => println!("pairing: confirm {n:06} (auto-yes)"),
                        Prompt::Confirm { number: None } => println!("pairing: just works (auto-yes)"),
                        Prompt::Display { passkey } => {
                            println!("pairing: passkey {passkey:06}");
                            if let Some(p) = &o.passkey_file {
                                let _ = std::fs::write(p, format!("{passkey}\n"));
                            }
                        }
                        Prompt::EnterPasskey => {
                            eprintln!("vinculumd: cannot enter a passkey in development mode");
                            host.pairing_passkey(handle, None, now());
                        }
                    }
                    if matches!(prompt, Prompt::Confirm { .. }) {
                        host.pairing_confirm(handle, true, now());
                    }
                }
                HostEvent::Encrypted { handle, key_size } => {
                    println!("encrypted: handle {handle} key size {key_size}");
                    if phase == Phase::Encrypting(handle) {
                        println!("reconnect: encrypted with the stored key");
                        host.disconnect(handle, now());
                        phase = Phase::Disconnecting;
                    }
                }
                HostEvent::EncryptionFailed { handle, status } => {
                    eprintln!("vinculumd: encryption on handle {handle} failed: {status}");
                    if o.pair && pairable.is_open(now()) {
                        // The device lost its bond; the user asked to pair,
                        // so pair again (the new bond must not be weaker).
                        println!("pairing again");
                        ok &= start_pair(&mut host, &pairable, handle, now());
                        phase = Phase::Pairing(handle);
                    } else {
                        ok = false;
                        host.disconnect(handle, now());
                        phase = Phase::Disconnecting;
                    }
                }
                HostEvent::Paired { handle, bond: b } => {
                    println!("paired: {b:?}");
                    let Some(peer) = host.peer(handle) else { continue };
                    let name = names.get(&peer).cloned().unwrap_or_default();
                    // Development: paired by the invoking user.
                    let uid = getuid();
                    let at = Address::from_msb(peer.address, peer.random);
                    let rec = vinculum_keys::Bond::from_pairing(&b, at, uid, &name);
                    let old = bonds.iter().position(|(_, x)| vinculum_keys::find([x], &at).is_some() || x.identity.is_some() && x.identity == rec.identity);
                    let verdict = policy::new_bond(old.map(|i| &bonds[i].1), &rec);
                    if let policy::NewBond::Refuse(why) = verdict {
                        eprintln!("vinculumd: pairing refused: {why}; keeping the stored bond");
                        ok = false;
                        host.disconnect(handle, now());
                        phase = Phase::Disconnecting;
                        continue;
                    }
                    println!("keyboard reports: {}", if policy::keyboard_allowed(&rec) { "allowed" } else { "not allowed (pair with passkey or numeric comparison)" });
                    match store.store(&rec) {
                        Ok(id) => {
                            println!("stored bond for {name}");
                            if let Some(i) = old {
                                let (old_id, _) = bonds.remove(i);
                                if let Err(e) = store.delete(&old_id) {
                                    eprintln!("vinculumd: removing the replaced bond: {e}");
                                }
                                println!("replaced the stored bond");
                            }
                            bonds.push((id, rec));
                        }
                        Err(e) => {
                            eprintln!("vinculumd: storing the bond: {e}");
                            ok = false;
                        }
                    }
                    host.disconnect(handle, now());
                    phase = if o.reconnect { Phase::Reconnecting } else { Phase::Disconnecting };
                }
                HostEvent::PairingFailed { handle, reason, why } => {
                    eprintln!("vinculumd: pairing on handle {handle} failed: 0x{reason:02x} {why}");
                    ok = false;
                    host.disconnect(handle, now());
                    phase = Phase::Disconnecting;
                }
                HostEvent::SecurityRequest { handle, auth_req } => {
                    println!("security request: handle {handle} auth 0x{auth_req:02x}");
                    let bond = host
                        .peer(handle)
                        .and_then(|p| vinculum_keys::find(bonds.iter().map(|(_, b)| b), &Address::from_msb(p.address, p.random)))
                        .map(|b| b.ltk);
                    match policy::on_security_request(bond.is_some(), pairable.is_open(now())) {
                        policy::OnSecurityRequest::Encrypt => {
                            let _ = host.encrypt(handle, bond.expect("bonded"), now());
                        }
                        policy::OnSecurityRequest::Pair => {
                            start_pair(&mut host, &pairable, handle, now());
                        }
                        policy::OnSecurityRequest::Ignore => println!("ignored: not bonded and not pairable"),
                    }
                }
                HostEvent::CommandFailed { op, status } => {
                    eprintln!("vinculumd: {op} failed: {status}");
                    ok = false;
                }
            }
        }
        while let Some(f) = host.poll_frame() {
            if std::env::var_os("VINCULUM_TRACE").is_some() {
                eprintln!("> {:02x?}", f.to_bytes());
            }
            if let Err(e) = link.write_all(&f.to_bytes()) {
                eprintln!("vinculumd: transport write: {e}");
                std::process::exit(1);
            }
        }
        if host.poll_event_pending() {
            continue;
        }
        if phase == Phase::Done {
            break;
        }
        let t = now();
        if t >= give_up {
            eprintln!("vinculumd: timed out");
            ok = false;
            break;
        }
        if let Some(u) = scan_until {
            if t >= u && phase == Phase::Scanning {
                host.le_scan(false, t);
                scan_until = None;
                phase = Phase::Done;
                continue;
            }
        }
        let mut next = give_up;
        if let Some(d) = host.deadline() {
            next = next.min(d);
        }
        if let Some(u) = scan_until {
            next = next.min(u);
        }
        let wait_ms = next.saturating_sub(t).min(i32::MAX as u64) as i32;
        if wait(link.fd(), wait_ms) {
            let n = link.read(&mut buf).unwrap_or(0);
            if n == 0 {
                eprintln!("vinculumd: transport closed");
                std::process::exit(1);
            }
            let mut frames = Vec::new();
            if let Err(e) = dec.push(&buf[..n], |f| frames.push(f)) {
                eprintln!("vinculumd: transport stream: {e}");
                std::process::exit(1);
            }
            for f in frames {
                if std::env::var_os("VINCULUM_TRACE").is_some() {
                    eprintln!("< {:02x?}", f.to_bytes());
                }
                host.on_frame(f, now());
            }
        }
        host.tick(now());
    }
    // Flush what the last events queued (scan disable, disconnect).
    while let Some(f) = host.poll_frame() {
        let _ = link.write_all(&f.to_bytes());
    }
    let s = host.stats();
    println!("stats: {s:?}");
    if s != vinculum_host::Stats::default() {
        eprintln!("vinculumd: protocol violations counted");
        ok = false;
    }
    std::process::exit(if ok { 0 } else { 1 })
}

/// Starts pairing if the pairable window is open (spec §7).
fn start_pair(host: &mut Host, pairable: &policy::Pairable, handle: u16, now_ms: u64) -> bool {
    if !pairable.is_open(now_ms) {
        eprintln!("vinculumd: not pairable now; pairing refused");
        return false;
    }
    // Development IRK; the real daemon keeps one per controller in the key
    // store.
    let cfg = PairConfig { io_cap: IoCap::KeyboardDisplay, irk: [0x5C; 16] };
    if let Err(e) = host.pair(handle, cfg, now_ms) {
        eprintln!("vinculumd: pair: {e}");
        return false;
    }
    true
}

fn getuid() -> u32 {
    // SAFETY: getuid(2) has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}
