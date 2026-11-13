//! aura-mlme: Aura's MLME daemon (docs/spec/wifi.md §3.4, §8.3).
//!
//! Owns `/dev/aura/ctlN`. Serves atrium-net on a Unix socket (only the uid
//! given by `--ctl-uid` may connect), and talks to aura-keys over its
//! socket. Holds no secrets: it never sees EAPOL or keys (the kernel keeps
//! those on `keyN`).
//!
//! usage: aura-mlme [-v] [--dev /dev/aura/ctl0] [--sock PATH] [--keys PATH]
//!                  [--ctl-uid UID] [--capsicum]

use aura_ipc::sock::{self, Conn};
use aura_ipc::{Got, Line};
use aura_mlme::sta::{Out, Req, Station};
use aura_proto::{decode, encode, Direction, Msg, HEADER_LEN, MAX_BODY};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

struct Opts {
    verbose: bool,
    dev: String,
    sock: PathBuf,
    keys: PathBuf,
    ctl_uid: u32,
    capsicum: bool,
}

fn opts() -> Opts {
    let mut o = Opts {
        verbose: false,
        dev: "/dev/aura/ctl0".into(),
        sock: "/atrium/sockets/aura/mlme.sock".into(),
        keys: "/atrium/sockets/aura-keys/keys.sock".into(),
        ctl_uid: 0,
        capsicum: false,
    };
    let mut a = std::env::args().skip(1);
    while let Some(x) = a.next() {
        match x.as_str() {
            "-v" => o.verbose = true,
            "--capsicum" => o.capsicum = true,
            "--dev" => o.dev = a.next().unwrap_or_default(),
            "--sock" => o.sock = a.next().unwrap_or_default().into(),
            "--keys" => o.keys = a.next().unwrap_or_default().into(),
            "--ctl-uid" => o.ctl_uid = a.next().and_then(|v| v.parse().ok()).unwrap_or(0),
            _ => {
                eprintln!("usage: aura-mlme [-v] [--dev DEV] [--sock PATH] [--keys PATH] [--ctl-uid UID] [--capsicum]");
                std::process::exit(2);
            }
        }
    }
    o
}

fn die(s: &str) -> ! {
    eprintln!("aura-mlme: {s}");
    std::process::exit(1)
}

struct Kernel {
    f: File,
    seq: u32,
    pending: HashMap<u32, &'static str>,
}

impl Kernel {
    fn send(&mut self, m: &Msg<'_>, what: &'static str) {
        let mut buf = vec![0u8; HEADER_LEN + MAX_BODY];
        self.seq = self.seq.wrapping_add(1).max(1);
        let n = match encode(m, self.seq, &mut buf) {
            Ok(n) => n,
            Err(e) => {
                eprintln!("aura-mlme: encode {what}: {e:?}");
                return;
            }
        };
        if let Err(e) = self.f.write(&buf[..n]) {
            eprintln!("aura-mlme: write {what}: {e}");
            return;
        }
        self.pending.insert(self.seq, what);
    }

    fn req(&mut self, r: &Req) {
        match r {
            Req::Scan { mac } => {
                let ssids = aura_proto::List::from_raw(&[]).unwrap_or_else(|| unreachable!());
                let freqs = aura_proto::List::from_raw(&[]).unwrap_or_else(|| unreachable!());
                let s = aura_proto::ScanReq { flags: aura_proto::scan_flags::RANDOM_MAC, mac: *mac, dwell_ms: 0, ssids, freqs };
                self.send(&Msg::Scan(s), "scan");
            }
            Req::TxMgmt { freq, frame } => self.send(&Msg::TxMgmt { freq: *freq, frame }, "tx-mgmt"),
            Req::SetBss(b) => self.send(&Msg::SetBss(*b), "set-bss"),
            Req::ClearBss => self.send(&Msg::ClearBss, "clear-bss"),
            Req::Disconnect { reason } => self.send(&Msg::Disconnect { reason: *reason }, "disconnect"),
        }
    }
}

#[allow(unsafe_code)]
fn random(b: &mut [u8]) {
    if getrandom::getrandom(b).is_err() {
        die("no randomness");
    }
}

fn main() {
    let o = opts();
    let start = Instant::now();
    let now = || start.elapsed().as_millis() as u64;
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&o.dev)
        .unwrap_or_else(|e| die(&format!("{}: {e}", o.dev)));
    let mut k = Kernel { f, seq: 0, pending: HashMap::new() };
    let mut buf = vec![0u8; HEADER_LEN + MAX_BODY];

    // The radio's address.
    k.send(&Msg::GetCaps, "caps");
    let deadline = Instant::now() + Duration::from_secs(5);
    let own = loop {
        match k.f.read(&mut buf) {
            Ok(n) => {
                if let Ok((_, Msg::Caps(c))) = decode(&buf[..n], Direction::KernelToMlme) {
                    break c.mac;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() > deadline {
                    die("no caps from the kernel");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => die(&format!("read caps: {e}")),
        }
    };
    k.pending.clear();

    let listener = sock::listen(&o.sock, 0o600).unwrap_or_else(|e| die(&format!("{}: {e}", o.sock.display())));
    let mut keys: Option<Conn> = None;
    let mut keys_retry = Instant::now();
    let mut clients: Vec<Conn> = Vec::new();
    let mut sta = Station::new(own, Box::new(random));
    if o.capsicum {
        // Everything after this works on descriptors already open; aura-keys
        // must be reachable now (its restart restarts us).
        keys = UnixStream::connect(&o.keys).ok().and_then(|s| Conn::new(s).ok());
        if keys.is_none() {
            die("aura-keys not reachable");
        }
        #[cfg(target_os = "freebsd")]
        // SAFETY: no arguments; process-wide sandbox switch.
        if unsafe { libc::cap_enter() } != 0 {
            die("cap_enter failed");
        }
    }
    eprintln!("aura-mlme: up, radio {}", aura_ipc::fmt_mac(&own));

    loop {
        if keys.is_none() && !o.capsicum && Instant::now() >= keys_retry {
            keys = UnixStream::connect(&o.keys).ok().and_then(|s| Conn::new(s).ok());
            keys_retry = Instant::now() + Duration::from_secs(1);
        }
        let mut fds = vec![k.f.as_raw_fd(), listener.as_raw_fd()];
        if let Some(c) = &keys {
            fds.push(c.fd());
        }
        let base = fds.len();
        fds.extend(clients.iter().map(|c| c.fd()));
        let ready = sock::poll_read(&fds, 100);
        let mut outs: Vec<Out> = Vec::new();

        if ready[0] {
            loop {
                let n = match k.f.read(&mut buf) {
                    Ok(n) => n,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) => die(&format!("read {}: {e}", o.dev)),
                };
                let (h, m) = match decode(&buf[..n], Direction::KernelToMlme) {
                    Ok(x) => x,
                    Err(e) => {
                        eprintln!("aura-mlme: bad message from kernel: {e:?}");
                        continue;
                    }
                };
                match m {
                    Msg::Ack { status } => {
                        let what = k.pending.remove(&h.seq).unwrap_or("?");
                        if status != 0 {
                            eprintln!("aura-mlme: {what} failed: errno {status}");
                            outs.extend(sta.request_failed(what, status));
                        }
                    }
                    Msg::MgmtRx { freq, rssi, flags, frame, .. } => {
                        if o.verbose && frame[0] >> 4 != 8 {
                            eprintln!("aura-mlme: rx subtype {} flags {flags:#x}", frame[0] >> 4);
                        }
                        outs.extend(sta.mgmt_rx(freq, rssi, flags, frame, now()));
                    }
                    Msg::ScanDone { status } => outs.extend(sta.scan_done(status, now())),
                    Msg::Disassociated { reason, .. } => outs.extend(sta.kernel_disassociated(reason)),
                    other => {
                        if o.verbose {
                            eprintln!("aura-mlme: kernel: {other:?}");
                        }
                    }
                }
            }
        }
        if ready[1] {
            while let Ok((s, _)) = listener.accept() {
                match Conn::new(s) {
                    Ok(c) if c.uid == o.ctl_uid => clients.push(c),
                    Ok(c) => eprintln!("aura-mlme: refused client uid {}", c.uid),
                    Err(e) => eprintln!("aura-mlme: accept: {e}"),
                }
            }
        }
        if keys.is_some() && ready[2] {
            let r = keys.as_mut().map(|c| c.lines());
            match r {
                Some(Ok(lines)) => {
                    for g in lines {
                        if let Got::Line(l) = g {
                            if o.verbose {
                                eprintln!("aura-mlme: keys: {l:?}");
                            }
                            outs.extend(sta.keys_msg(&l));
                        }
                    }
                }
                _ => {
                    eprintln!("aura-mlme: aura-keys went away");
                    keys = None;
                    if o.capsicum {
                        die("aura-keys went away");
                    }
                    outs.extend(sta.keys_msg(&Line::new("failed").with("why", "keys-gone")));
                }
            }
        }
        let mut dead = Vec::new();
        // Clients accepted above have no poll slot yet: next round.
        for (i, c) in clients.iter_mut().enumerate().take(ready.len() - base) {
            if !ready[base + i] {
                continue;
            }
            match c.lines() {
                Ok(lines) => {
                    for g in lines {
                        match g {
                            Got::Line(l) => {
                                if o.verbose {
                                    eprintln!("aura-mlme: ctl: {l:?}");
                                }
                                outs.extend(sta.cmd(&l, now()));
                            }
                            Got::Bad => outs.push(Out::Ctl(Line::new("error").with("why", "bad-line"))),
                        }
                    }
                }
                Err(_) => dead.push(i),
            }
        }
        for i in dead.into_iter().rev() {
            clients.remove(i);
        }
        outs.extend(sta.tick(now()));

        for out in outs {
            match out {
                Out::Kernel(r) => {
                    if o.verbose {
                        eprintln!("aura-mlme: -> kernel {}", match &r {
                            Req::Scan { .. } => "scan".to_string(),
                            Req::TxMgmt { frame, .. } => format!("tx-mgmt subtype {}", frame[0] >> 4),
                            Req::SetBss(b) => format!("set-bss aid {}", b.aid),
                            Req::ClearBss => "clear-bss".into(),
                            Req::Disconnect { reason } => format!("disconnect {reason}"),
                        });
                    }
                    k.req(&r);
                }
                Out::Keys(l) => {
                    if o.verbose {
                        eprintln!("aura-mlme: -> keys {l:?}");
                    }
                    if let Some(c) = &mut keys {
                        if c.send(&l).is_err() {
                            keys = None;
                        }
                    }
                }
                Out::Ctl(l) => {
                    if o.verbose {
                        eprintln!("aura-mlme: -> ctl {l:?}");
                    }
                    clients.retain_mut(|c| c.send(&l).is_ok());
                }
            }
        }
    }
}
