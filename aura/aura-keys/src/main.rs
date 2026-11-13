//! aura-keys: Aura's key daemon (docs/spec/wifi.md §8.3, decision 14).
//!
//! Owns `/dev/aura/keyN`: EAPOL frames in and out, keys, the controlled
//! port. Nothing else in Aura sees a password, a PMK or a key. Listens on
//! one socket for two peers, told apart by uid:
//!
//! - aura-mlme (`--mlme-uid`): `hello mac_key=..` when it connects (the
//!   key for per-network addresses); SAE authentication as `sae-start`,
//!   `sae-rx`, `sae-abort`, answered with `sae-commit`, `sae-confirm`,
//!   `sae-done` or `sae-failed` (aura-mlme frames them, this side does the
//!   crypto); `assoc ...` when associated (BSSID, RSNEs and RSNXEs, AKM),
//!   `disassoc`; gets back `authorized [td=..]` or `failed why=..`.
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
use aura_rsn::crypto::{self, Akm, Pmk};
use aura_rsn::sae::{self, Sae, SaeError};
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
        sock: "/atrium/sockets/aura-keys/keys.sock".into(),
        mlme_uid: 65033,
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

/// An SAE exchange in progress, and its PMK once done (kept for the
/// association that follows).
struct SaeRun {
    bssid: [u8; 6],
    sae: Sae,
    token: Option<Vec<u8>>,
}

struct Keys {
    sae: Option<SaeRun>,
    sae_pmk: Option<([u8; 6], Pmk)>,
    /// Transition Disable bits from the last message 3.
    td: u8,
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
                Action::TransitionDisable(b) => self.td = *b,
                Action::Authorized => {
                    self.send(&Msg::PortAuthorized(true));
                    let mut l = Line::new("authorized");
                    if self.td != 0 {
                        l = l.with("td", self.td);
                    }
                    reply = Some(l);
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
        let Some(akm) = Akm::from_suite(0x000f_ac00 | akm) else {
            return fail("akm");
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
        let pmk = if akm == Akm::Sae {
            // The PMK from the SAE exchange with this very BSS.
            match self.sae_pmk.take() {
                Some((b, pmk)) if b == bssid => pmk,
                _ => return fail("no-sae-pmk"),
            }
        } else {
            match crypto::psk(secret, &ssid) {
                Some(p) => p,
                None => return fail("bad-credential"),
            }
        };
        let mut snonce = [0u8; 32];
        if getrandom::getrandom(&mut snonce).is_err() {
            die("no randomness");
        }
        self.td = 0;
        let cfg = Config { akm, pmk, aa: bssid, spa: own, own_rsne, ap_rsne, own_rsnxe: l.hex("own_rsnxe"), ap_rsnxe: l.hex("ap_rsnxe"), pmf: pmf == 1 };
        self.join = Some(Join { bssid, supp: Some(Supplicant::new(cfg, snonce)), mlme });
        for (src, body) in std::mem::take(&mut self.pending) {
            if let Some(r) = self.eapol(src, &body) {
                out.push(r);
            }
        }
        out
    }

    /// SAE, the station side (sae.rs does the crypto).
    fn sae_msg(&mut self, l: &Line) -> Line {
        let failed = |e: &str| Line::new("sae-failed").with("why", e);
        match l.verb.as_str() {
            "sae-start" => {
                self.sae = None;
                self.sae_pmk = None;
                let (Some(ssid), Some(bssid), Some(own), Some(h2e)) = (l.hex("ssid"), l.mac("bssid"), l.mac("own"), l.num::<u8>("h2e")) else {
                    return failed("bad-start");
                };
                let Some((pssid, secret)) = &self.prepared else {
                    return failed("no-credential");
                };
                if *pssid != ssid {
                    return failed("not-prepared");
                }
                // SAE takes the passphrase itself; a raw 64-hex-digit PSK
                // cannot be used with it.
                if secret.len() == 64 && secret.iter().all(|b| b.is_ascii_hexdigit()) {
                    return failed("psk-not-password");
                }
                let pwe = if h2e == 1 {
                    sae::pwe_from_pt(&sae::pt_h2e(secret, &ssid), &own, &bssid)
                } else {
                    match sae::pwe_hnp(secret, &own, &bssid) {
                        Some(p) => p,
                        None => return failed("pwe"),
                    }
                };
                let s = Sae::new(pwe, h2e == 1, &mut |b: &mut [u8]| {
                    if getrandom::getrandom(b).is_err() {
                        die("no randomness");
                    }
                });
                eprintln!("aura-keys: SAE start with {} ({})", aura_ipc::fmt_mac(&bssid), if h2e == 1 { "hash-to-element" } else { "hunting-and-pecking" });
                let reply = Line::new("sae-commit").with("status", s.commit_status()).with_hex("body", &s.commit_body(None));
                self.sae = Some(SaeRun { bssid, sae: s, token: None });
                reply
            }
            "sae-rx" => {
                let (Some(tr), Some(status), Some(body)) = (l.num::<u16>("trans"), l.num::<u16>("status"), l.hex("body")) else {
                    return failed("bad-rx");
                };
                let Some(run) = &mut self.sae else {
                    return failed("not-started");
                };
                let r: Result<Line, SaeError> = (|| match (tr, status) {
                    (1, sae::STATUS_ANTI_CLOGGING) => {
                        let t = run.sae.token_request(&body)?;
                        eprintln!("aura-keys: SAE anti-clogging token requested ({} bytes), commit again", t.len());
                        let b = run.sae.commit_body(Some(&t));
                        run.token = Some(t);
                        Ok(Line::new("sae-commit").with("status", run.sae.commit_status()).with_hex("body", &b))
                    }
                    (1, st) if st == run.sae.commit_status() => {
                        match run.sae.rx_commit(&body) {
                            // A repeated commit after ours was answered:
                            // the AP missed our confirm, send it again.
                            Ok(()) | Err(SaeError::UnexpectedMessage) => {}
                            Err(e) => return Err(e),
                        }
                        Ok(Line::new("sae-confirm").with("status", 0).with_hex("body", &run.sae.confirm_body()?))
                    }
                    (2, 0) => {
                        run.sae.rx_confirm(&body)?;
                        Ok(Line::new("sae-done"))
                    }
                    _ => Err(SaeError::UnexpectedMessage),
                })();
                match r {
                    Ok(l) => {
                        if l.verb == "sae-done" {
                            if let Some(run) = self.sae.take() {
                                if let Some((pmk, _)) = run.sae.pmk() {
                                    eprintln!("aura-keys: SAE accepted by {}", aura_ipc::fmt_mac(&run.bssid));
                                    self.sae_pmk = Some((run.bssid, Pmk(pmk)));
                                }
                            }
                        }
                        l
                    }
                    Err(e) => {
                        eprintln!("aura-keys: SAE failed: {e:?}");
                        self.sae = None;
                        failed(&format!("{e:?}").to_ascii_lowercase())
                    }
                }
            }
            _ => {
                // sae-abort
                self.sae = None;
                self.sae_pmk = None;
                Line::new("ok")
            }
        }
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
                match self.store.remember(&creds::account(&ssid), &secret) {
                    Ok(()) => Line::new("ok"),
                    Err(e) => err(&store_err(&e)),
                }
            }
            "forget" => match self.store.forget(&creds::account(&ssid)) {
                Ok(true) => Line::new("ok"),
                Ok(false) => err("unknown"),
                Err(e) => err(&store_err(&e)),
            },
            "prepare" => match self.store.release(&creds::account(&ssid)) {
                Ok(Some(s)) => {
                    self.prepared = Some((ssid, s));
                    Line::new("ok")
                }
                Ok(None) => err("no-credential"),
                Err(e) => err(&store_err(&e)),
            },
            _ => err("unknown-command"),
        }
    }
}

/// The key per-network MAC addresses are derived from: kept in the store
/// so addresses survive reboots; made on first use. If the store cannot be
/// reached, a random one for this run (addresses then change per boot,
/// which only costs the network a new lease).
fn mac_key(store: &mut dyn Store) -> zeroize::Zeroizing<[u8; 32]> {
    let mut key = zeroize::Zeroizing::new([0u8; 32]);
    match store.release(creds::MAC_KEY) {
        Ok(Some(v)) if v.len() == 32 => {
            key.copy_from_slice(&v);
            return key;
        }
        Ok(_) => {}
        Err(e) => eprintln!("aura-keys: mac key: {}", store_err(&e)),
    }
    if getrandom::getrandom(&mut key[..]).is_err() {
        die("no randomness");
    }
    if let Err(e) = store.remember(creds::MAC_KEY, &key[..]) {
        eprintln!("aura-keys: mac key not stored: {}", store_err(&e));
    }
    key
}

/// An error for the reply line: no spaces, no secrets (store errors carry
/// none).
fn store_err(e: &std::io::Error) -> String {
    format!("store:{e}").chars().map(|c| if c.is_ascii_graphic() && c != '=' { c } else { '-' }).take(80).collect()
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
    let mut k = Keys { sae: None, sae_pmk: None, td: 0, f, seq: 0, verbose: o.verbose, store, prepared: None, join: None, pending: Vec::new() };
    let listener = sock::listen(&o.sock, 0o666).unwrap_or_else(|e| die(&format!("{}: {e}", o.sock.display())));
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
                    Ok(mut c) if c.uid == o.mlme_uid => {
                        let key = mac_key(&mut *k.store);
                        let _ = c.send(&Line::new("hello").with_hex("mac_key", &key[..]));
                        conns.push(Some(c));
                    }
                    Ok(c) if c.uid == o.ctl_uid => conns.push(Some(c)),
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
                    "sae-start" | "sae-rx" | "sae-abort" if uid == o.mlme_uid => {
                        let r = k.sae_msg(&l);
                        if r.verb != "ok" {
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
