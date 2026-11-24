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
//! - atrium-net (`--ctl-uid`): `remember ssid=.. secret=..|handle=.. [trial=1]` (a handle is redeemed at arcad, TAKE_WIFI;
//!   `trial=1` keeps a newly typed passphrase in memory for the next join
//!   only, and stores it once that join is authorized), `forget
//!   ssid=..`, `prepare ssid=..` (release the credential for the next join;
//!   Arca hands out one per join, never the list); gets `ok` or `error`.
//!
//!
//! 802.1X networks (wifi.md §7.4): `remember-eap ssid=.. method=..
//! identity=.. [anonymous=..] [domain=..] [ca=<DER hex>] secret=..|handle=..`
//! stores the network's profile; an `assoc` with AKM 1 or 5 runs EAP in
//! `atrium-aura-eap` (eap.rs) and the 4-way handshake on its MSK. A first
//! join with no CA stops at the server's certificate: `eap-seen ssid=..`
//! shows it, `eap-trust ssid=..` pins it for that network. EAP-TLS:
//! `eap-key ssid=.. cn=..` mints the network's key in Arca and answers
//! `key id=.. csr=..` (a PKCS #10 request for IT to sign); `remember-eap
//! method=tls key=.. cert=<DER hex>` takes the certificate issued for it.
//! The helper never holds the key: it asks here, and Arca signs.
//!
//! usage: aura-keys [-v] [--dev /dev/aura/key0] [--sock PATH]
//!                  [--mlme-uid UID] [--ctl-uid UID]
//!                  [--arca PATH | --memory-store] [--eap-helper PATH]

mod creds;
mod eap;

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
    eap_helper: String,
}

fn opts() -> Opts {
    let mut o = Opts {
        verbose: false,
        dev: "/dev/aura/key0".into(),
        sock: "/atrium/sockets/aura-keys/keys.sock".into(),
        mlme_uid: 65033,
        ctl_uid: 0,
        arca: Some("/atrium/sockets/arca/arcad.sock".into()),
        eap_helper: "/usr/local/bin/atrium-aura-eap".into(),
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
            "--eap-helper" => o.eap_helper = a.next().unwrap_or_default(),
            _ => {
                eprintln!("usage: aura-keys [-v] [--dev DEV] [--sock PATH] [--mlme-uid UID] [--ctl-uid UID] [--arca PATH | --memory-store] [--eap-helper PATH]");
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
    /// aura-mlme was told we answered message 1 (a deauth after it means
    /// a wrong passphrase); the pairwise key is set.
    told_m2: bool,
    ptk: bool,
    /// 802.1X: the EAP helper, until it hands back the MSK.
    eap: Option<eap::Run>,
    /// What the supplicant needs besides the PMK, kept until EAP ends.
    params: Option<Params>,
    /// EAPOL-Key frames that came before EAP finished.
    early_keys: Vec<Vec<u8>>,
    /// EAP-TLS: the network's key in Arca, for the helper's one signature.
    tls_key: Option<[u8; 16]>,
}

struct Params {
    akm: Akm,
    spa: [u8; 6],
    own_rsne: Vec<u8>,
    ap_rsne: Vec<u8>,
    own_rsnxe: Option<Vec<u8>>,
    ap_rsnxe: Option<Vec<u8>>,
    pmf: bool,
}

/// The certificate an 802.1X server showed on a first join (no CA set).
struct Seen {
    ssid: Vec<u8>,
    names: Vec<String>,
    chain: Vec<Vec<u8>>,
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
    /// A passphrase on trial (`remember trial=1`): used by the next
    /// `prepare` of its network instead of the stored one, and stored only
    /// when that join is authorized. A wrong one never replaces the
    /// stored passphrase.
    trial: Option<(Vec<u8>, Secret)>,
    /// The prepared credential is the trial one: store it on success.
    prepared_trial: bool,
    join: Option<Join>,
    /// EAPOL that arrived before aura-mlme's `assoc` (they race).
    pending: Vec<([u8; 6], Vec<u8>)>,
    eap_helper: String,
    seen: Option<Seen>,
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
                Action::Send(body) => {
                    self.send(&Msg::TxEapol { dst: bssid, body });
                    // Before the pairwise key, a send is message 2.
                    if let Some(j) = self.join.as_mut().filter(|j| !j.ptk && !j.told_m2) {
                        j.told_m2 = true;
                        reply.get_or_insert(Line::new("handshake").with("what", "m2"));
                    }
                }
                Action::SetPtk(tk) => {
                    if let Some(j) = self.join.as_mut() {
                        j.ptk = true;
                    }
                    self.set_key(KeyKind::Pairwise, 0, Cipher::Ccmp128, bssid, [0; 6], tk);
                }
                Action::SetGtk { idx, key, rsc } => self.set_key(KeyKind::Group, *idx, Cipher::Ccmp128, BROADCAST, *rsc, key),
                Action::SetIgtk { idx, key, ipn } => self.set_key(KeyKind::Igtk, *idx, Cipher::BipCmac128, BROADCAST, *ipn, key),
                Action::TransitionDisable(b) => self.td = *b,
                Action::Authorized => {
                    self.send(&Msg::PortAuthorized(true));
                    if std::mem::take(&mut self.prepared_trial) {
                        if let Some((ssid, secret)) = &self.prepared {
                            match self.store.remember(&creds::account(ssid), secret) {
                                Ok(()) => eprintln!("aura-keys: the new passphrase worked; stored"),
                                Err(e) => eprintln!("aura-keys: storing the new passphrase: {}", store_err(&e)),
                            }
                        }
                    }
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
        if let Some(run) = &mut j.eap {
            match eap::parse_eapol(body) {
                Some((eap::EAPOL_EAP, p)) => {
                    if let Err(e) = run.packet(p) {
                        eprintln!("aura-keys: EAP helper: {e}");
                        j.eap = None;
                        return Some(Line::new("failed").with("why", "eap-helper"));
                    }
                }
                Some((eap::EAPOL_KEY, _)) if j.early_keys.len() < MAX_PENDING => j.early_keys.push(body.to_vec()),
                _ => eprintln!("aura-keys: EAPOL ignored during EAP"),
            }
            return None;
        }
        if eap::parse_eapol(body).is_some_and(|(t, _)| t == eap::EAPOL_EAP) {
            // A late EAP retransmission after the MSK: nothing to do.
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

    /// EAPOL-Logoff before we leave a join EAP did not finish: under PMF
    /// the AP ignores our unprotected deauth and would keep the half-done
    /// conversation (and its retry count) for our next association.
    fn logoff(&mut self, bssid: [u8; 6]) {
        self.send(&Msg::TxEapol { dst: bssid, body: &eap::eapol(eap::EAPOL_LOGOFF, &[]) });
    }

    /// What the EAP helper said; lines for aura-mlme.
    fn eap_events(&mut self) -> Vec<Line> {
        let mut out = Vec::new();
        let Some(j) = &mut self.join else { return out };
        let Some(run) = &mut j.eap else { return out };
        let events = match run.read() {
            Ok(e) => e,
            Err(e) => vec![eap::Event::Failed(format!("helper-{}", e.kind()).replace(' ', "-"))],
        };
        let bssid = j.bssid;
        for ev in events {
            match ev {
                eap::Event::Send(f) => self.send(&Msg::TxEapol { dst: bssid, body: &f }),
                eap::Event::Log(w) => eprintln!("aura-keys: EAP helper dropped a packet: {w}"),
                eap::Event::Sign(digest) => {
                    // One CertificateVerify per join, with this network's key.
                    let Some(j) = &mut self.join else { break };
                    let sig = match j.tls_key.take() {
                        Some(id) => self.store.tls_sign(&id, &digest).unwrap_or_else(|e| {
                            eprintln!("aura-keys: Arca did not sign: {e}");
                            Vec::new()
                        }),
                        None => Vec::new(),
                    };
                    let Some(run) = self.join.as_mut().and_then(|j| j.eap.as_mut()) else { break };
                    if run.signed(&sig).is_err() {
                        eprintln!("aura-keys: the EAP helper went away while signing");
                    }
                }
                eap::Event::Done(msk) => {
                    let Some(j) = &mut self.join else { break };
                    j.eap = None;
                    let Some(p) = j.params.take() else { break };
                    let mut pmk = [0u8; 32];
                    pmk.copy_from_slice(&msk[..32]);
                    let mut snonce = [0u8; 32];
                    if getrandom::getrandom(&mut snonce).is_err() {
                        die("no randomness");
                    }
                    eprintln!("aura-keys: EAP succeeded with {}; 4-way handshake next", aura_ipc::fmt_mac(&bssid));
                    let cfg = Config { akm: p.akm, pmk: Pmk(pmk), aa: bssid, spa: p.spa, own_rsne: p.own_rsne, ap_rsne: p.ap_rsne, own_rsnxe: p.own_rsnxe, ap_rsnxe: p.ap_rsnxe, pmf: p.pmf };
                    j.supp = Some(Supplicant::new(cfg, snonce));
                    for f in std::mem::take(&mut j.early_keys) {
                        if let Some(r) = self.eapol(bssid, &f) {
                            out.push(r);
                        }
                    }
                    break;
                }
                eap::Event::Failed(why) => {
                    eprintln!("aura-keys: EAP failed: {why}");
                    self.logoff(bssid);
                    self.join = None;
                    out.push(Line::new("failed").with("why", format!("eap-{why}")));
                    break;
                }
                eap::Event::Trust(names, chain) => {
                    eprintln!("aura-keys: EAP stopped at the server's certificate ({}); waiting for the user", names.join(","));
                    self.logoff(bssid);
                    let ssid = self.prepared.as_ref().map(|(s, _)| s.clone()).unwrap_or_default();
                    self.seen = Some(Seen { ssid, names, chain });
                    self.join = None;
                    out.push(Line::new("failed").with("why", "eap-trust"));
                    break;
                }
            }
        }
        out
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
            self.join = Some(Join { bssid, supp: None, mlme, told_m2: false, ptk: false, eap: None, params: None, early_keys: Vec::new(), tls_key: None });
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
        if akm.is_eap() {
            let Some(profile) = aura_eap::profile::Profile::decode(secret) else {
                return fail("not-enterprise");
            };
            let run = match eap::Run::start(&self.eap_helper, &profile.encode()) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("aura-keys: starting {}: {e}", self.eap_helper);
                    return fail("eap-helper");
                }
            };
            eprintln!("aura-keys: EAP ({}) with {}", profile.method.name(), aura_ipc::fmt_mac(&bssid));
            let params = Params { akm, spa: own, own_rsne, ap_rsne, own_rsnxe: l.hex("own_rsnxe"), ap_rsnxe: l.hex("ap_rsnxe"), pmf: pmf == 1 };
            self.td = 0;
            self.join = Some(Join { bssid, supp: None, mlme, told_m2: false, ptk: false, eap: Some(run), params: Some(params), early_keys: Vec::new(), tls_key: profile.key });
            for (src, body) in std::mem::take(&mut self.pending) {
                if let Some(r) = self.eapol(src, &body) {
                    out.push(r);
                }
            }
            return out;
        }
        if aura_eap::profile::Profile::is_profile(secret) {
            return fail("enterprise-credential");
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
        self.join = Some(Join { bssid, supp: Some(Supplicant::new(cfg, snonce)), mlme, told_m2: false, ptk: false, eap: None, params: None, early_keys: Vec::new(), tls_key: None });
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
                // The passphrase itself (atrium-netctl, as root), or the
                // handle to what the user typed into Arca's field, which
                // only we can redeem (curia.md §4).
                let secret = match (l.hex("secret"), l.hex("handle")) {
                    (Some(s), None) => zeroize::Zeroizing::new(s),
                    (None, Some(h)) => {
                        let Ok(h) = <[u8; 16]>::try_from(h.as_slice()) else { return err("bad-handle") };
                        match self.store.take_typed(&h) {
                            Ok(s) => s,
                            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return err("handle-refused"),
                            Err(e) => return err(&store_err(&e)),
                        }
                    }
                    _ => return err("bad-secret"),
                };
                if crypto::psk(&secret, &ssid).is_none() {
                    return err("bad-secret");
                }
                if l.get("trial") == Some("1") {
                    self.trial = Some((ssid, secret));
                    return Line::new("ok");
                }
                match self.store.remember(&creds::account(&ssid), &secret) {
                    Ok(()) => Line::new("ok"),
                    Err(e) => err(&store_err(&e)),
                }
            }
            "remember-eap" => self.remember_eap(l, ssid),
            "eap-seen" => match &self.seen {
                Some(seen) if seen.ssid == ssid => {
                    use sha2::Digest;
                    let top = seen.chain.last().expect("a chain");
                    Line::new("seen")
                        .with("names", if seen.names.is_empty() { "-".to_string() } else { seen.names.join(",") })
                        .with("certs", seen.chain.len())
                        .with_hex("top_sha256", &sha2::Sha256::digest(top))
                }
                _ => err("nothing-seen"),
            },
            "eap-trust" => self.eap_trust(l, ssid),
            "eap-key" => self.eap_key(l, ssid),
            "forget" => match {
                self.trial = self.trial.take().filter(|(s, _)| *s != ssid);
                let key = self.store.forget(&creds::tls_account(&ssid));
                self.store.forget(&creds::account(&ssid)).and_then(|f| key.map(|k| f || k))
            } {
                Ok(true) => Line::new("ok"),
                Ok(false) => err("unknown"),
                Err(e) => err(&store_err(&e)),
            },
            "prepare" if self.trial.as_ref().is_some_and(|(s, _)| *s == ssid) => {
                // Used once: a failed join leaves the stored one in place.
                let (s, secret) = self.trial.take().expect("trial");
                self.prepared = Some((s, secret));
                self.prepared_trial = true;
                Line::new("ok")
            }
            "prepare" => match self.store.release(&creds::account(&ssid)) {
                Ok(Some(s)) => {
                    self.prepared = Some((ssid, s));
                    self.prepared_trial = false;
                    Line::new("ok")
                }
                Ok(None) => err("no-credential"),
                Err(e) => err(&store_err(&e)),
            },
            _ => err("unknown-command"),
        }
    }
}

impl Keys {
    /// `remember-eap`: an 802.1X network's profile, kept in Arca like a
    /// passphrase (the profile carries the password).
    fn remember_eap(&mut self, l: &Line, ssid: Vec<u8>) -> Line {
        use aura_eap::profile::{Method, Profile};
        let err = |why: &str| Line::new("error").with("why", why);
        let text = |k: &str| l.hex(k).and_then(|v| String::from_utf8(v).ok());
        let Some(method) = l.get("method").and_then(Method::from_name) else { return err("bad-method") };
        let Some(identity) = text("identity").filter(|i| !i.is_empty() && i.len() <= 253) else { return err("bad-identity") };
        let password = match (l.hex("secret"), l.hex("handle")) {
            (Some(s), None) => zeroize::Zeroizing::new(s),
            (None, Some(h)) => {
                let Ok(h) = <[u8; 16]>::try_from(h.as_slice()) else { return err("bad-handle") };
                match self.store.take_typed(&h) {
                    Ok(s) => s,
                    Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return err("handle-refused"),
                    Err(e) => return err(&store_err(&e)),
                }
            }
            (None, None) if method == Method::Tls => zeroize::Zeroizing::new(Vec::new()),
            _ => return err("bad-secret"),
        };
        let Ok(password) = String::from_utf8(password.to_vec()).map(zeroize::Zeroizing::new) else { return err("bad-secret") };
        let domain = text("domain").unwrap_or_default();
        let ca: Vec<Vec<u8>> = l.hex("ca").into_iter().collect();
        if !ca.is_empty() && domain.is_empty() {
            // A CA without a name trusts every server that CA ever signed.
            return err("ca-needs-domain");
        }
        let (mut cert, mut key) = (Vec::new(), None);
        if method == Method::Tls {
            // The certificate must be for the key this network's request
            // was made with: a certificate for another key fails at the
            // server only after the password-free handshake has begun.
            let (Some(id), Some(c)) = (l.hex("key").and_then(|k| <[u8; 16]>::try_from(k.as_slice()).ok()), l.hex("cert")) else {
                return err("tls-needs-key-and-cert");
            };
            let public = match self.store.tls_public(&id) {
                Ok(p) => p,
                Err(e) => return err(&store_err(&e)),
            };
            if !aura_eap::csr::certifies(&c, &public) {
                return err("cert-not-for-key");
            }
            (cert, key) = (vec![c], Some(id));
        }
        let p = Profile { method, identity, anonymous: text("anonymous"), domain, ca, pin: None, password, cert, key };
        match self.store.remember(&creds::account(&ssid), &p.encode()) {
            Ok(()) => Line::new("ok"),
            Err(e) => err(&store_err(&e)),
        }
    }

    /// `eap-key`: a new EAP-TLS key for this network, made and kept in
    /// Arca, and a certificate request for it with `cn` as its name.
    fn eap_key(&mut self, l: &Line, ssid: Vec<u8>) -> Line {
        let err = |why: &str| Line::new("error").with("why", why);
        let Some(cn) = l.hex("cn").and_then(|v| String::from_utf8(v).ok()).filter(|c| !c.is_empty() && c.len() <= 64) else {
            return err("bad-cn");
        };
        let (id, public) = match self.store.mint_tls(&creds::tls_account(&ssid)) {
            Ok(k) => k,
            Err(e) => return err(&store_err(&e)),
        };
        let info = aura_eap::csr::info(&cn, &public);
        let sig = match self.store.tls_sign(&id, &aura_eap::csr::digest(&info)) {
            Ok(s) => s,
            Err(e) => return err(&store_err(&e)),
        };
        Line::new("key").with_hex("id", &id).with_hex("csr", &aura_eap::csr::der(&info, &sig))
    }

    /// `eap-trust`: pin the certificate the last first join stopped at
    /// ("Trust this server"), with its name, for that network only.
    fn eap_trust(&mut self, _l: &Line, ssid: Vec<u8>) -> Line {
        let err = |why: &str| Line::new("error").with("why", why);
        let Some(seen) = self.seen.take_if(|s| s.ssid == ssid) else { return err("nothing-seen") };
        let acc = creds::account(&ssid);
        let mut p = match self.store.release(&acc) {
            Ok(Some(v)) => match aura_eap::profile::Profile::decode(&v) {
                Some(p) => p,
                None => return err("not-enterprise"),
            },
            Ok(None) => return err("no-credential"),
            Err(e) => return err(&store_err(&e)),
        };
        if p.pin.is_some() || !p.ca.is_empty() {
            // A changed server is never trusted by a press: forget the
            // network and set it up again.
            return err("already-trusted");
        }
        if p.domain.is_empty() {
            let Some(d) = seen.names.first() else { return err("no-name") };
            p.domain = d.clone();
        } else if !seen.names.iter().any(|n| n.eq_ignore_ascii_case(&p.domain) || n.to_ascii_lowercase().ends_with(&format!(".{}", p.domain.to_ascii_lowercase()))) {
            return err("name-mismatch");
        }
        p.pin = seen.chain.last().cloned();
        match self.store.remember(&acc, &p.encode()) {
            Ok(()) => Line::new("ok").with("domain", &p.domain),
            Err(e) => err(&store_err(&e)),
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
    let mut k = Keys { sae: None, sae_pmk: None, td: 0, f, seq: 0, verbose: o.verbose, store, prepared: None, trial: None, prepared_trial: false, join: None, pending: Vec::new(), eap_helper: o.eap_helper.clone(), seen: None };
    let listener = sock::listen(&o.sock, 0o666).unwrap_or_else(|e| die(&format!("{}: {e}", o.sock.display())));
    let mut conns: Vec<Option<Conn>> = Vec::new();
    let mut buf = vec![0u8; HEADER_LEN + MAX_BODY];
    eprintln!("aura-keys: up");
    let mut last_gc = Instant::now();

    loop {
        let mut fds = vec![k.f.as_raw_fd(), listener.as_raw_fd()];
        let live: Vec<usize> = (0..conns.len()).filter(|&i| conns[i].is_some()).collect();
        fds.extend(live.iter().map(|&i| conns[i].as_ref().map(|c| c.fd()).unwrap_or(-1)));
        let helper_fd = k.join.as_ref().and_then(|j| j.eap.as_ref()).map(|r| r.fd());
        fds.push(helper_fd.unwrap_or(-1));
        let ready = sock::poll_read(&fds, 500);
        let mut to_mlme: Vec<(usize, Line)> = Vec::new();
        if helper_fd.is_some() && *ready.last().unwrap() {
            let mlme = k.join.as_ref().map(|j| j.mlme);
            for r in k.eap_events() {
                if let Some(m) = mlme {
                    to_mlme.push((m, r));
                }
            }
        }

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
                    "remember" | "forget" | "prepare" | "remember-eap" | "eap-seen" | "eap-trust" | "eap-key" if uid == o.ctl_uid => {
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
