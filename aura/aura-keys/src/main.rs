//! aura-keys: Aura's key daemon (docs/spec/wifi.md §8.3, decision 14).
//!
//! Owns `/dev/aura/keyN`: EAPOL frames in and out, keys, the controlled
//! port. Nothing else in Aura sees a password, a PMK or a key. Listens on
//! one socket for two peers, told apart by uid:
//!
//! - aura-mlme (`--mlme-uid`): `assoc ...` when associated (BSSID, both
//!   RSNEs, AKM), `disassoc`; gets back `authorized` or `failed why=..`.
//! - atrium-net (`--ctl-uid`): `remember ssid=.. secret=..`, `forget
//!   ssid=..`, `prepare ssid=..` (release the credential for the next join;
//!   Arca hands out one per join, never the list); gets `ok` or `error`.
//!
//! usage: aura-keys [-v] [--dev /dev/aura/key0] [--sock PATH]
//!                  [--mlme-uid UID] [--ctl-uid UID]
//!                  [--arca PATH | --memory-store]

mod creds;

use aura_ipc::sock::{self, Conn};
use aura_ipc::{Got, Line};
use aura_proto::{decode, encode, Cipher, Direction, Key, KeyKind, Msg, HEADER_LEN, MAX_BODY};
use aura_rsn::crypto::{self, Akm};
use aura_rsn::supp::{Action, Config, Supplicant};
use creds::{Secret, Store};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

struct Opts {
    verbose: bool,
    dev: String,
    sock: PathBuf,
    mlme_uid: u32,
    ctl_uid: u32,
    arca: Option<PathBuf>,
}

fn opts() -> Opts {
    let mut o = Opts {
        verbose: false,
        dev: "/dev/aura/key0".into(),
        sock: "/var/run/aura/keys.sock".into(),
        mlme_uid: 65031,
        ctl_uid: 0,
        arca: Some("/atrium/sockets/arca/arcad.sock".into()),
    };
    let mut a = std::env::args().skip(1);
    while let Some(x) = a.next() {
        match x.as_str() {
            "-v" => o.verbose = true,
            "--dev" => o.dev = a.next().unwrap_or_default(),
            "--sock" => o.sock = a.next().unwrap_or_default().into(),
            "--mlme-uid" => o.mlme_uid = a.next().and_then(|v| v.parse().ok()).unwrap_or(u32::MAX),
            "--ctl-uid" => o.ctl_uid = a.next().and_then(|v| v.parse().ok()).unwrap_or(u32::MAX),
            "--arca" => o.arca = a.next().map(Into::into),
            "--memory-store" => o.arca = None,
            _ => {
                eprintln!("usage: aura-keys [-v] [--dev DEV] [--sock PATH] [--mlme-uid UID] [--ctl-uid UID] [--arca PATH | --memory-store]");
                std::process::exit(2);
            }
        }
    }
    o
}

fn die(s: &str) -> ! {
    eprintln!("aura-keys: {s}");
    std::process::exit(1)
}

const BROADCAST: [u8; 6] = [0xff; 6];
const MAX_PENDING: usize = 4;

struct Join {
    bssid: [u8; 6],
    supp: Option<Supplicant>,
    /// The mlme connection to answer.
    mlme: usize,
}

struct Keys {
    f: File,
    seq: u32,
    verbose: bool,
    store: Box<dyn Store>,
    prepared: Option<(Vec<u8>, Secret)>,
    join: Option<Join>,
    /// EAPOL that arrived before aura-mlme's `assoc` (they race).
    pending: Vec<([u8; 6], Vec<u8>)>,
}

impl Keys {
    fn send(&mut self, m: &Msg<'_>) {
        let mut buf = zeroize::Zeroizing::new(vec![0u8; HEADER_LEN + MAX_BODY]);
        self.seq = self.seq.wrapping_add(1).max(1);
        match encode(m, self.seq, &mut buf) {
            Ok(n) => {
                if let Err(e) = self.f.write(&buf[..n]) {
                    eprintln!("aura-keys: write: {e}");
                }
            }
            Err(e) => eprintln!("aura-keys: encode: {e:?}"),
        }
    }

    fn set_key(&mut self, kind: KeyKind, idx: u8, cipher: Cipher, addr: [u8; 6], rsc: [u8; 6], material: &[u8]) {
        let mut k = Key { kind, idx, cipher, addr, rsc, key: [0; 32] };
        k.key[..material.len()].copy_from_slice(material);
        self.send(&Msg::SetKey(k));
    }

    /// Carries out the supplicant's actions; returns a line for aura-mlme.
    fn apply(&mut self, acts: Vec<Action>) -> Option<Line> {
        let bssid = self.join.as_ref()?.bssid;
        let mut reply = None;
        for a in &acts {
            if self.verbose {
                eprintln!("aura-keys: {a:?}");
            }
            match a {
                Action::Send(body) => self.send(&Msg::TxEapol { dst: bssid, body }),
                Action::SetPtk(tk) => self.set_key(KeyKind::Pairwise, 0, Cipher::Ccmp128, bssid, [0; 6], tk),
                Action::SetGtk { idx, key, rsc } => self.set_key(KeyKind::Group, *idx, Cipher::Ccmp128, BROADCAST, *rsc, key),
                Action::SetIgtk { idx, key, ipn } => self.set_key(KeyKind::Igtk, *idx, Cipher::BipCmac128, BROADCAST, *ipn, key),
                Action::Authorized => {
                    self.send(&Msg::PortAuthorized(true));
                    reply = Some(Line::new("authorized"));
                }
            }
        }
        reply
    }

    fn eapol(&mut self, src: [u8; 6], body: &[u8]) -> Option<Line> {
        let Some(j) = &mut self.join else {
            if self.pending.len() < MAX_PENDING {
                self.pending.push((src, body.to_vec()));
            }
            return None;
        };
        if src != j.bssid {
            return None;
        }
        let s = j.supp.as_mut()?;
        match s.rx(body) {
            Ok(acts) => self.apply(acts),
            Err(why) => {
                eprintln!("aura-keys: EAPOL ignored: {why:?}");
                None
            }
        }
    }

    /// aura-mlme reports an association.
    fn assoc(&mut self, l: &Line, mlme: usize) -> Vec<Line> {
        let mut out = Vec::new();
        let fail = |why: &str| vec![Line::new("failed").with("why", why)];
        let (Some(ssid), Some(bssid), Some(own), Some(akm), Some(pmf)) =
            (l.hex("ssid"), l.mac("bssid"), l.mac("own"), l.num::<u32>("akm"), l.num::<u8>("pmf"))
        else {
            return fail("bad-assoc");
        };
        self.join = None;
        if akm == 0 {
            // Open network: nothing to negotiate, open the port.
            self.join = Some(Join { bssid, supp: None, mlme });
            self.pending.clear();
            self.send(&Msg::PortAuthorized(true));
            out.push(Line::new("authorized"));
            return out;
        }
        let akm = match akm {
            2 => Akm::Psk,
            6 => Akm::PskSha256,
            _ => return fail("akm"),
        };
        let (Some(own_rsne), Some(ap_rsne)) = (l.hex("own_rsne"), l.hex("ap_rsne")) else {
            return fail("bad-assoc");
        };
        // Only the network atrium-net asked us to prepare for.
        let Some((pssid, secret)) = &self.prepared else {
            return fail("no-credential");
        };
        if *pssid != ssid {
            return fail("not-prepared");
        }
        let Some(pmk) = crypto::psk(secret, &ssid) else {
            return fail("bad-credential");
        };
        let mut snonce = [0u8; 32];
        if getrandom::getrandom(&mut snonce).is_err() {
            die("no randomness");
        }
        let cfg = Config { akm, pmk, aa: bssid, spa: own, own_rsne, ap_rsne, pmf: pmf == 1 };
        self.join = Some(Join { bssid, supp: Some(Supplicant::new(cfg, snonce)), mlme });
        for (src, body) in std::mem::take(&mut self.pending) {
            if let Some(r) = self.eapol(src, &body) {
                out.push(r);
            }
        }
        out
    }

    fn ctl(&mut self, l: &Line) -> Line {
        let err = |why: &str| Line::new("error").with("why", why);
        let Some(ssid) = l.hex("ssid").filter(|s| !s.is_empty() && s.len() <= 32) else {
            return err("bad-ssid");
        };
        match l.verb.as_str() {
            "remember" => {
                let Some(secret) = l.hex("secret").map(zeroize::Zeroizing::new) else {
                    return err("bad-secret");
                };
                if crypto::psk(&secret, &ssid).is_none() {
                    return err("bad-secret");
                }
                match self.store.remember(&ssid, &secret) {
                    Ok(()) => Line::new("ok"),
                    Err(e) => err(&format!("store:{}", e.kind())),
                }
            }
            "forget" => match self.store.forget(&ssid) {
                Ok(true) => Line::new("ok"),
                Ok(false) => err("unknown"),
                Err(e) => err(&format!("store:{}", e.kind())),
            },
            "prepare" => match self.store.release(&ssid) {
                Ok(Some(s)) => {
                    self.prepared = Some((ssid, s));
                    Line::new("ok")
                }
                Ok(None) => err("no-credential"),
                Err(e) => err(&format!("store:{}", e.kind())),
            },
            _ => err("unknown-command"),
        }
    }
}

fn main() {
    let o = opts();
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&o.dev)
        .unwrap_or_else(|e| die(&format!("{}: {e}", o.dev)));
    let store: Box<dyn Store> = match &o.arca {
        Some(p) => Box::new(creds::Arca::new(p)),
        None => {
            eprintln!("aura-keys: WARNING credentials kept in memory only (--memory-store, test use)");
            Box::new(creds::Memory::default())
        }
    };
    let mut k = Keys { f, seq: 0, verbose: o.verbose, store, prepared: None, join: None, pending: Vec::new() };
    let listener = sock::listen(&o.sock).unwrap_or_else(|e| die(&format!("{}: {e}", o.sock.display())));
    let mut conns: Vec<Option<Conn>> = Vec::new();
    let mut buf = vec![0u8; HEADER_LEN + MAX_BODY];
    eprintln!("aura-keys: up");
    let mut last_gc = Instant::now();

    loop {
        let mut fds = vec![k.f.as_raw_fd(), listener.as_raw_fd()];
        let live: Vec<usize> = (0..conns.len()).filter(|&i| conns[i].is_some()).collect();
        fds.extend(live.iter().map(|&i| conns[i].as_ref().map(|c| c.fd()).unwrap_or(-1)));
        let ready = sock::poll_read(&fds, 500);
        let mut to_mlme: Vec<(usize, Line)> = Vec::new();

        if ready[0] {
            loop {
                let n = match k.f.read(&mut buf) {
                    Ok(n) => n,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) => die(&format!("read {}: {e}", o.dev)),
                };
                let m = match decode(&buf[..n], Direction::KernelToKeys) {
                    Ok((_, m)) => m,
                    Err(e) => {
                        eprintln!("aura-keys: bad message from kernel: {e:?}");
                        continue;
                    }
                };
                match m {
                    Msg::Ack { status } if status != 0 => {
                        eprintln!("aura-keys: kernel refused a request: errno {status}");
                        if let Some(j) = &k.join {
                            to_mlme.push((j.mlme, Line::new("failed").with("why", "kernel-refused")));
                        }
                        k.join = None;
                    }
                    Msg::EapolRx { src, body } => {
                        let body = body.to_vec();
                        if let Some(r) = k.eapol(src, &body) {
                            if let Some(j) = &k.join {
                                to_mlme.push((j.mlme, r));
                            }
                        }
                    }
                    Msg::Disassociated { .. } => {
                        k.join = None;
                        k.pending.clear();
                    }
                    _ => {}
                }
            }
            buf.fill(0);
        }
        if ready[1] {
            while let Ok((s, _)) = listener.accept() {
                match Conn::new(s) {
                    Ok(c) if c.uid == o.mlme_uid || c.uid == o.ctl_uid => conns.push(Some(c)),
                    Ok(c) => eprintln!("aura-keys: refused peer uid {}", c.uid),
                    Err(e) => eprintln!("aura-keys: accept: {e}"),
                }
            }
        }
        for (pos, &i) in live.iter().enumerate() {
            if !ready[2 + pos] {
                continue;
            }
            let Some(c) = conns[i].as_mut() else { continue };
            let uid = c.uid;
            let lines = match c.lines() {
                Ok(l) => l,
                Err(_) => {
                    conns[i] = None;
                    continue;
                }
            };
            for g in lines {
                let Got::Line(l) = g else {
                    to_mlme.push((i, Line::new("error").with("why", "bad-line")));
                    continue;
                };
                if o.verbose {
                    eprintln!("aura-keys: <- {l:?}");
                }
                match l.verb.as_str() {
                    "assoc" if uid == o.mlme_uid => {
                        for r in k.assoc(&l, i) {
                            to_mlme.push((i, r));
                        }
                    }
                    "disassoc" if uid == o.mlme_uid => {
                        k.join = None;
                        k.pending.clear();
                    }
                    "remember" | "forget" | "prepare" if uid == o.ctl_uid => {
                        let r = k.ctl(&l);
                        to_mlme.push((i, r));
                    }
                    _ => to_mlme.push((i, Line::new("error").with("why", "not-permitted"))),
                }
            }
        }
        for (i, l) in to_mlme {
            if o.verbose {
                eprintln!("aura-keys: -> {l:?}");
            }
            if let Some(c) = conns.get_mut(i).and_then(|c| c.as_mut()) {
                if c.send(&l).is_err() {
                    conns[i] = None;
                }
            }
        }
        if last_gc.elapsed() > Duration::from_secs(5) {
            last_gc = Instant::now();
            while conns.last().is_some_and(|c| c.is_none()) && k.join.as_ref().is_none_or(|j| j.mlme + 1 < conns.len()) {
                conns.pop();
            }
        }
    }
}
