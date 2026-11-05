//! atrium-split-fuzz: F3, the broker fuzzed through its real sockets
//! (docs/spec/fresco-f4-split.md). Two halves:
//!
//! - `atrium-split-fuzz renderer <seed>` is a hostile renderer. frescod runs
//!   it in place of `fresco-renderer` (`FRESCOD_RENDERER_LAUNCHER=
//!   "atrium-split-fuzz renderer {client}"`). It speaks the record protocol
//!   badly on purpose: mutated and random records, records with fds, the
//!   broker's own kinds, windows it does not have, frames of the wrong size,
//!   corrupt frame slots, and stretches where it stops reading.
//! - `atrium-split-fuzz drive <fresco.sock> <chrome.sock|-> <seconds> <seed>`
//!   keeps a few clients connected (so frescod keeps starting hostile
//!   renderers) and plays a hostile Navigator on the chrome socket. After the
//!   time is up it checks that frescod still serves a well-behaved client.
//!
//! What must hold: frescod never stops, never blocks its frame loop on a
//! renderer, and is left with nothing of a renderer it retired.

use std::collections::HashMap;
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use fresco_compute::fuzz::{self, Rng};
use fresco_compute::Frame;
use fresco_split::chan::{self, Chan, Raw, Shm};
use fresco_split::chrome::ChromeMsg;
use fresco_split::{slot, Msg, Role, MSG_BYTES};

/// Raised by the driver when the fuzzing is over.
const BENIGN_FLAG: &str = "/tmp/split-fuzz-benign";

fn main() {
    let a: Vec<String> = std::env::args().collect();
    // Installed in a jail as `split-fuzz-hog`, with no arguments (a jail
    // gets neither): the renderer that fills every slot it is given.
    if a.first().is_some_and(|p| p.ends_with("split-fuzz-hog")) {
        hog();
    }
    let seed = |i: usize| a.get(i).and_then(|s| s.parse::<u64>().ok()).unwrap_or(1);
    match a.get(1).map(String::as_str) {
        Some("renderer") => {
            // Once the driver is done it raises the flag, and the real
            // renderer serves the well-behaved client that checks frescod.
            if let (Some(real), true) = (a.get(3), std::path::Path::new(BENIGN_FLAG).exists()) {
                use std::os::unix::process::CommandExt;
                let e = std::process::Command::new(real).exec();
                eprintln!("split-fuzz: exec {real}: {e}");
                std::process::exit(1);
            }
            let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64;
            renderer(seed(2) ^ t)
        }
        Some("hoard") if a.len() >= 4 => hoard(&a[2], seed(3) as usize),
        Some("drive") if a.len() >= 6 => drive(&a[2], &a[3], seed(4), seed(5)),
        _ => {
            eprintln!("usage: atrium-split-fuzz renderer <seed> [real-renderer] | hoard <fresco.sock> <n> | drive <fresco.sock> <chrome.sock|-> <seconds> <seed>");
            std::process::exit(2);
        }
    }
}

// ── The hostile renderer ────────────────────────────────────────────────

struct Win {
    slots: Option<[Shm; 2]>,
    size: (u32, u32),
}

fn renderer(seed: u64) {
    let mut r = Rng(seed);
    // SAFETY: stdin is the broker channel.
    let broker = Chan::from_fd(unsafe { OwnedFd::from_raw_fd(0) });
    let client = match broker.recv() {
        Ok(chan::Recv::Msg(Msg::Client { .. }, mut fds)) if fds.len() == 1 => fds.pop().unwrap(),
        _ => std::process::exit(1),
    };
    // SPLIT_FUZZ_PERSONA=staller|flooder runs only that one (an A/B test).
    let pick = match std::env::var("SPLIT_FUZZ_PERSONA").as_deref() {
        Ok("staller") => 0,
        Ok("flooder") => 15,
        _ => r.below(100),
    };
    // This renderer never serves its client, but like the real one it goes
    // when the client does; a staller (below) ignores that and stays.
    if pick <= 14 {
        std::mem::forget(client);
    } else {
        std::thread::spawn(move || {
            let mut s = std::os::unix::net::UnixStream::from(client);
            let _ = std::io::copy(&mut s, &mut std::io::sink());
            std::process::exit(0);
        });
    }

    // The broker's records, read on their own thread so a stall here is a
    // real stall: nothing reads while `stalled` is set.
    let reader = broker.try_clone().unwrap();
    let (tx, rx) = mpsc::channel::<(Msg, Vec<OwnedFd>)>();
    let stalled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let stalled = stalled.clone();
        std::thread::spawn(move || loop {
            while stalled.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(20));
            }
            match reader.recv() {
                Ok(chan::Recv::Msg(m, fds)) => {
                    if tx.send((m, fds)).is_err() {
                        return;
                    }
                }
                _ => std::process::exit(0), // the broker is done with us
            }
        });
    }

    match pick {
        0..=14 => staller(&broker, &stalled),
        15..=29 => flooder(&mut r, &broker, &rx),
        _ => {}
    }

    let mut wins: HashMap<u32, Win> = HashMap::new();
    let steps = 200 + r.below(3000);
    for _ in 0..steps {
        for (m, fds) in rx.try_iter() {
            match m {
                Msg::Slots { window, bytes } if fds.len() == 2 => {
                    let mut it = fds.into_iter();
                    let (a, b) = (it.next().unwrap(), it.next().unwrap());
                    if let (Ok(a), Ok(b)) = (Shm::open(a, bytes as usize), Shm::open(b, bytes as usize)) {
                        wins.entry(window).or_insert(Win { slots: None, size: (1, 1) }).slots = Some([a, b]);
                    }
                }
                Msg::Geometry { window, width, height } => {
                    wins.entry(window).or_insert(Win { slots: None, size: (1, 1) }).size = (width, height);
                }
                _ => {}
            }
        }
        if r.chance(2) {
            // Stop reading for a while; keep talking.
            stalled.store(true, std::sync::atomic::Ordering::Relaxed);
        } else if r.chance(3) {
            stalled.store(false, std::sync::atomic::Ordering::Relaxed);
        }
        if r.below(4000) == 0 {
            std::process::exit(0); // vanish mid-conversation
        }
        let known: Vec<u32> = wins.keys().copied().collect();
        let some_window = |r: &mut Rng| {
            if !known.is_empty() && r.chance(80) { known[r.below(known.len() as u32) as usize] } else { r.below(80) }
        };
        let rec: [u8; MSG_BYTES] = match r.below(100) {
            0..=24 => {
                let window = if r.chance(70) { 1 + r.below(70) } else { r.word(70) };
                let dim = |r: &mut Rng| if r.chance(80) { 1 + r.below(900) } else { r.word(20000) };
                let role = [Role::Document, Role::Dialog, Role::Panel, Role::Background, Role::Chrome][r.below(5) as usize];
                let pos = if r.chance(30) { Some((r.next() as i32, r.next() as i32)) } else { None };
                let title: String = (0..r.below(70)).map(|_| char::from_u32(0x20 + r.below(0x2000)).unwrap_or('?')).collect();
                Msg::WindowCreate { window, width: dim(&mut r), height: dim(&mut r), role, pos, title }.encode()
            }
            25..=54 => {
                let window = some_window(&mut r);
                let s = r.below(2);
                if let Some(w) = wins.get_mut(&window) {
                    if let Some(slots) = w.slots.as_mut() {
                        write_slot(&mut r, slots[s as usize].as_mut_slice(), w.size);
                    }
                }
                Msg::FrameReady { window, slot: s, seq: r.next() as u32 }.encode()
            }
            55..=62 => {
                let window = some_window(&mut r);
                if r.chance(80) {
                    wins.remove(&window);
                }
                Msg::WindowDestroy { window }.encode()
            }
            63..=69 => Msg::WindowTitle { window: some_window(&mut r), title: "x".repeat(r.below(64) as usize) }.encode(),
            70..=74 => {
                // A record only the broker sends.
                let window = some_window(&mut r);
                [
                    Msg::Geometry { window, width: 10, height: 10 },
                    Msg::FrameDone { window, slot: 0 },
                    Msg::Focus { window, gained: true },
                    Msg::Client { width: 1, height: 1, refresh_mhz: 1 },
                ][r.below(4) as usize]
                    .encode()
            }
            75..=89 => {
                // A valid record, mutated.
                let mut b = Msg::FrameReady { window: some_window(&mut r), slot: 0, seq: 1 }.encode();
                if r.chance(50) {
                    b = Msg::WindowCreate { window: 3, width: 100, height: 100, role: Role::Document, pos: None, title: "t".into() }.encode();
                }
                for _ in 0..1 + r.below(3) {
                    b[r.below(MSG_BYTES as u32) as usize] = r.next() as u8;
                }
                b
            }
            _ => {
                let mut b = [0u8; MSG_BYTES];
                b.iter_mut().for_each(|x| *x = r.next() as u8);
                b
            }
        };
        let res = if r.chance(2) {
            // A record with an fd attached (the broker accepts none).
            broker.send_raw(&rec, &[1]) // stdout: /dev/null in every renderer
        } else {
            broker.send_raw(&rec, &[])
        };
        if res.is_err() {
            std::process::exit(0); // retired
        }
        if r.chance(10) {
            std::thread::sleep(Duration::from_millis(r.below(30) as u64));
        }
    }
    // Then idle with the windows it has, until the broker lets it go.
    std::thread::sleep(Duration::from_secs(r.below(5) as u64));
}

/// Never reads again, and keeps the broker talking: every window it opens
/// makes the broker send SLOTS and GEOMETRY. Once its channel is full, a
/// broker that sends blocking would wait on it forever.
fn staller(broker: &Chan, stalled: &std::sync::atomic::AtomicBool) -> ! {
    stalled.store(true, std::sync::atomic::Ordering::Relaxed);
    let create = Msg::WindowCreate { window: 1, width: 64, height: 64, role: Role::Document, pos: None, title: "stall".into() };
    let mut n = 0u64;
    loop {
        if broker.send(&create, &[]).is_err() || broker.send(&Msg::WindowDestroy { window: 1 }, &[]).is_err() {
            eprintln!("split-fuzz: staller retired after {n} windows");
            std::process::exit(0);
        }
        n += 1;
    }
}

/// Follows the protocol but sends a valid full-size frame as fast as the
/// broker answers, for a few seconds: the broker's cost per frame is the
/// copy and the check, and it must stay bounded.
fn flooder(r: &mut Rng, broker: &Chan, rx: &mpsc::Receiver<(Msg, Vec<OwnedFd>)>) -> ! {
    let _ = broker.send(&Msg::WindowCreate { window: 1, width: 900, height: 700, role: Role::Document, pos: None, title: "flood".into() }, &[]);
    let (mut slots, mut size) = (None, (900, 700));
    let end = Instant::now() + Duration::from_secs(3 + r.below(5) as u64);
    let mut sent = 0u32;
    while Instant::now() < end {
        for (m, fds) in rx.try_iter() {
            match m {
                Msg::Slots { bytes, .. } if fds.len() == 2 => {
                    let mut it = fds.into_iter();
                    slots = Some([Shm::open(it.next().unwrap(), bytes as usize).unwrap(), Shm::open(it.next().unwrap(), bytes as usize).unwrap()]);
                }
                Msg::Geometry { width, height, .. } => size = (width, height),
                _ => {}
            }
        }
        let Some(s) = slots.as_mut() else {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        };
        let f = Frame::pack(&fuzz::scene(r, size.0, size.1), size.0, size.1, size.0);
        let k = (sent % 2) as usize;
        let _ = slot::write(s[k].as_mut_slice(), &f, sent);
        if broker.send(&Msg::FrameReady { window: 1, slot: k as u32, seq: sent }, &[]).is_err() {
            break;
        }
        sent += 1;
    }
    eprintln!("split-fuzz: flooder sent {sent} frames");
    std::process::exit(0);
}

/// What a hostile renderer may leave in a slot: a good frame at the size
/// the broker gave, a corrupt one, one of another size, or bytes.
fn write_slot(r: &mut Rng, s: &mut [u8], (w, h): (u32, u32)) {
    match r.below(10) {
        0..=3 if (w as u64) * (h as u64) <= 1 << 20 => {
            let f = Frame::pack(&fuzz::scene(r, w, h), w, h, w);
            let _ = slot::write(s, &f, r.next() as u32);
        }
        4..=6 if (w as u64) * (h as u64) <= 1 << 20 => {
            let mut f = Frame::pack(&fuzz::scene(r, w, h), w, h, w);
            fuzz::corrupt(r, &mut f);
            let _ = slot::write(s, &f, 1);
        }
        7 => {
            let _ = slot::write(s, &fuzz::frame(r), 1);
        }
        _ => {
            let n = (r.below(4096) as usize).min(s.len());
            let at = if r.chance(50) { 0 } else { r.below(s.len() as u32) as usize };
            for x in s[at..].iter_mut().take(n) {
                *x = r.next() as u8;
            }
        }
    }
}

// ── The driver ──────────────────────────────────────────────────────────

fn drive(fresco: &str, chrome: &str, secs: u64, seed: u64) {
    let end = Instant::now() + Duration::from_secs(secs);
    let chrome_thread = (chrome != "-").then(|| {
        let path = chrome.to_string();
        std::thread::spawn(move || hostile_navigator(&path, seed ^ 0xc4, end))
    });

    let mut r = Rng(seed);
    let mut clients: Vec<(std::os::unix::net::UnixStream, Instant)> = Vec::new();
    let mut started = 0u32;
    while Instant::now() < end {
        clients.retain(|(_, until)| Instant::now() < *until);
        if clients.len() < 6 {
            match std::os::unix::net::UnixStream::connect(fresco) {
                Ok(s) => {
                    started += 1;
                    clients.push((s, Instant::now() + Duration::from_millis(500 + r.below(6000) as u64)));
                }
                Err(e) => {
                    eprintln!("split-fuzz: connect failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(clients);
    let navigator = chrome_thread.map(|t| t.join().unwrap()).unwrap_or_default();
    eprintln!("split-fuzz: {started} hostile renderers started; navigator: {navigator}");

    // frescod must still serve a client that follows the protocol.
    std::fs::write(BENIGN_FLAG, b"").expect("raise the flag");
    let ok = (|| -> std::io::Result<bool> {
        let mut c = fresco_client::Connection::connect(fresco)?;
        let win = c.window_create(320, 200, "after-fuzz", fresco_protocol::WindowHints::default())?;
        let until = Instant::now() + Duration::from_secs(20);
        while Instant::now() < until {
            if let Some(ev) = c.wait_event(Some(Duration::from_secs(1)))? {
                eprintln!("split-fuzz: after the fuzz, window {win}: {ev:?}");
                return Ok(true);
            }
        }
        Ok(false)
    })();
    let _ = std::fs::remove_file(BENIGN_FLAG);
    match ok {
        Ok(true) => eprintln!("split-fuzz: PASS, frescod still serves"),
        other => {
            eprintln!("split-fuzz: FAIL, a well-behaved client got no answer: {other:?}");
            std::process::exit(1);
        }
    }
}

/// A Navigator gone wrong: opens connections, names handles it does not
/// own, sends bad records, and drops connections it opened.
fn hostile_navigator(path: &str, seed: u64, end: Instant) -> String {
    let mut r = Rng(seed);
    let (mut conns, mut opened, mut refused) = (0u32, 0u32, 0u32);
    while Instant::now() < end {
        let c = match chan::connect(std::path::Path::new(path)) {
            Ok(c) => c,
            Err(e) => return format!("connect failed: {e}"),
        };
        conns += 1;
        let mut handles: Vec<u32> = Vec::new();
        let mut held: Vec<OwnedFd> = Vec::new();
        for _ in 0..r.below(200) {
            let rec = match r.below(10) {
                0 => ChromeMsg::OpenClient.encode(),
                1..=6 => {
                    let handle = if !handles.is_empty() && r.chance(70) { handles[r.below(handles.len() as u32) as usize] } else { r.below(400) };
                    let origin: String = (0..r.below(70)).map(|_| b"abcxyz019.:/-[]_"[r.below(16) as usize] as char).collect();
                    ChromeMsg::SetOrigin { handle, window: 1 + r.below(70), origin }.encode()
                }
                7 => ChromeMsg::Opened { handle: 1 }.encode(),
                8 => {
                    let mut b = ChromeMsg::SetOrigin { handle: 1, window: 1, origin: "https://a.example".into() }.encode();
                    b[r.below(MSG_BYTES as u32) as usize] = r.next() as u8;
                    b
                }
                _ => {
                    let mut b = [0u8; MSG_BYTES];
                    b.iter_mut().for_each(|x| *x = r.next() as u8);
                    b
                }
            };
            if c.send_raw(&rec, &[]).is_err() {
                refused += 1;
                break;
            }
            if rec[..4] == ChromeMsg::OpenClient.encode()[..4] {
                match c.recv_raw() {
                    Ok(Raw::Rec(b, fds)) => {
                        if let Ok(ChromeMsg::Opened { handle }) = ChromeMsg::decode(&b) {
                            opened += 1;
                            handles.push(handle);
                            held.extend(fds);
                            if held.len() > 8 {
                                held.remove(r.below(held.len() as u32) as usize);
                            }
                        }
                    }
                    _ => {
                        refused += 1;
                        break;
                    }
                }
            }
        }
    }
    format!("{conns} connections, {opened} documents opened, {refused} ended by frescod")
}

/// Hold `n` connections from one uid and count how many frescod keeps:
/// at most `MAX_CLIENTS_PER_UID` (32) may stay open.
fn hoard(fresco: &str, n: usize) {
    use std::io::Read;
    let mut held = Vec::new();
    for _ in 0..n {
        match std::os::unix::net::UnixStream::connect(fresco) {
            Ok(s) => held.push(s),
            Err(e) => eprintln!("split-fuzz: connect: {e}"),
        }
    }
    std::thread::sleep(Duration::from_secs(3));
    let mut open = 0;
    for s in &mut held {
        s.set_nonblocking(true).unwrap();
        let mut b = [0u8; 1];
        match s.read(&mut b) {
            Ok(0) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => open += 1,
            _ => open += 1,
        }
    }
    println!("split-fuzz: {} connections made, {open} kept open by frescod", held.len());
    // SPLIT_FUZZ_HOLD_S: keep them for that long after counting, so several
    // hoarders (one per uid) can be measured together.
    if let Some(t) = std::env::var("SPLIT_FUZZ_HOLD_S").ok().and_then(|v| v.parse().ok()) {
        std::thread::sleep(Duration::from_secs(t));
    }
}

/// Opens windows and writes every byte of their slots: 48 MiB a window of
/// memory frescod created. Its jail's memory cap must stop it, and frescod
/// must clean up after it.
fn hog() -> ! {
    // SAFETY: stdin is the broker channel.
    let broker = Chan::from_fd(unsafe { OwnedFd::from_raw_fd(0) });
    let _client = match broker.recv() {
        Ok(chan::Recv::Msg(Msg::Client { .. }, mut fds)) if fds.len() == 1 => fds.pop().unwrap(),
        _ => std::process::exit(1),
    };
    let mut slots = Vec::new();
    for w in 1..=40u32 {
        let create = Msg::WindowCreate { window: w, width: 64, height: 64, role: Role::Document, pos: None, title: "hog".into() };
        if broker.send(&create, &[]).is_err() {
            std::process::exit(0);
        }
        loop {
            match broker.recv() {
                Ok(chan::Recv::Msg(Msg::Slots { bytes, .. }, fds)) if fds.len() == 2 => {
                    for fd in fds {
                        let mut s = Shm::open(fd, bytes as usize).unwrap();
                        s.as_mut_slice().iter_mut().step_by(4096).for_each(|b| *b = 1);
                        slots.push(s);
                    }
                    eprintln!("split-fuzz-hog: {} MiB of slots touched", slots.len() * bytes as usize >> 20);
                    break;
                }
                Ok(chan::Recv::Msg(..)) => continue,
                _ => std::process::exit(0),
            }
        }
    }
    eprintln!("split-fuzz-hog: not stopped");
    std::thread::sleep(Duration::from_secs(30));
    std::process::exit(0);
}
