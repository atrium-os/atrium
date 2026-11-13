//! The station state machine: scan, select, authenticate, associate, hand
//! the handshake to aura-keys, and police management frames once PMF is on.
//! No I/O: the daemon feeds events in and carries out the [`Out`]s.

use crate::ie::{self, BssInfo, Pmf, Security};
use crate::mgmt::{self, Mac};
use aura_ipc::{fmt_mac, Line};
use aura_proto::{bss_flags, rx_flags, Bss, Width};
use aura_rsn::crypto::Akm;
use aura_rsn::rsne::{self, Rsne};

/// Kernel requests (owned; the daemon encodes them as aura-proto messages).
#[derive(Clone, Debug, PartialEq)]
pub enum Req {
    Scan { mac: Mac },
    TxMgmt { freq: u16, frame: Vec<u8> },
    SetBss(Bss),
    ClearBss,
    Disconnect { reason: u16 },
}

#[derive(Debug)]
pub enum Out {
    Kernel(Req),
    /// To aura-keys.
    Keys(Line),
    /// To every atrium-net client.
    Ctl(Line),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Want {
    Open,
    Psk,
}

#[derive(Clone, Debug)]
struct JoinReq {
    ssid: Vec<u8>,
    want: Want,
}

#[derive(Clone, Debug)]
struct Target {
    bss: BssInfo,
    own_rsne: Option<Vec<u8>>,
    akm: Option<Akm>,
    pmf: bool,
}

#[derive(Debug)]
enum State {
    Idle,
    Scanning { join: Option<JoinReq> },
    Auth { t: Target, tries: u8, deadline: u64 },
    Assoc { t: Target, tries: u8, deadline: u64 },
    /// Associated; aura-keys runs the handshake.
    Keys { t: Target, deadline: u64 },
    /// Port authorized.
    Up { t: Target },
}

pub const MAX_RESULTS: usize = 128;
/// Frame exchange timeouts are generous: the sim gate runs under TCG.
const MGMT_TIMEOUT_MS: u64 = 2000;
const MGMT_TRIES: u8 = 3;
const KEYS_TIMEOUT_MS: u64 = 20_000;
const SA_QUERY_TIMEOUT_MS: u64 = 1000;

pub const REASON_LEAVING: u16 = 3;
pub const REASON_4WAY_TIMEOUT: u16 = 15;

pub struct Station {
    own: Mac,
    st: State,
    results: Vec<BssInfo>,
    seq: u16,
    sa_query: Option<([u8; 2], u64)>,
    rand: Box<dyn FnMut(&mut [u8])>,
}

fn state_line(name: &str) -> Line {
    Line::new("state").with("state", name)
}

impl Station {
    pub fn new(own: Mac, rand: Box<dyn FnMut(&mut [u8])>) -> Self {
        Station { own, st: State::Idle, results: Vec::new(), seq: 0, sa_query: None, rand }
    }

    pub fn state_name(&self) -> &'static str {
        match self.st {
            State::Idle => "idle",
            State::Scanning { .. } => "scanning",
            State::Auth { .. } => "authenticating",
            State::Assoc { .. } => "associating",
            State::Keys { .. } => "associated",
            State::Up { .. } => "authorized",
        }
    }

    fn seq(&mut self) -> u16 {
        self.seq = self.seq.wrapping_add(1) & 0x0fff;
        self.seq
    }

    fn target(&self) -> Option<&Target> {
        match &self.st {
            State::Auth { t, .. } | State::Assoc { t, .. } | State::Keys { t, .. } | State::Up { t } => Some(t),
            _ => None,
        }
    }

    fn random_mac(&mut self) -> Mac {
        let mut m = [0u8; 6];
        (self.rand)(&mut m);
        m[0] = (m[0] & 0xfc) | 0x02; // unicast, locally administered
        m
    }

    fn start_scan(&mut self, join: Option<JoinReq>, out: &mut Vec<Out>) {
        self.results.clear();
        let mac = self.random_mac();
        self.st = State::Scanning { join };
        out.push(Out::Kernel(Req::Scan { mac }));
    }

    /// Leaves whatever BSS we are in or joining. `tell_ap` sends a
    /// deauthentication first (protected once PMF is up).
    fn leave(&mut self, reason: u16, tell_ap: bool, why: &str, out: &mut Vec<Out>) {
        let Some(t) = self.target().cloned() else {
            self.st = State::Idle;
            return;
        };
        let associated = matches!(self.st, State::Keys { .. } | State::Up { .. });
        if tell_ap {
            let protect = matches!(self.st, State::Up { .. }) && t.pmf;
            let s = self.seq();
            let f = mgmt::deauth(mgmt::DEAUTH, &self.own, &t.bss.bssid, s, reason, protect);
            out.push(Out::Kernel(Req::TxMgmt { freq: t.bss.freq, frame: f }));
        }
        if associated {
            out.push(Out::Kernel(Req::Disconnect { reason }));
            out.push(Out::Keys(Line::new("disassoc")));
        }
        self.sa_query = None;
        self.st = State::Idle;
        out.push(Out::Ctl(state_line("disconnected").with("reason", reason).with("why", why)));
    }

    /// A command from atrium-net.
    pub fn cmd(&mut self, l: &Line, now: u64) -> Vec<Out> {
        let _ = now;
        let mut out = Vec::new();
        match l.verb.as_str() {
            "scan" => {
                // W1 scans only while idle (no background scans when
                // connected yet: that needs off-channel time from the radio).
                if matches!(self.st, State::Idle) {
                    self.start_scan(None, &mut out);
                } else {
                    out.push(Out::Ctl(Line::new("error").with("why", "busy")));
                }
            }
            "join" => {
                let ssid = l.hex("ssid").filter(|s| !s.is_empty() && s.len() <= 32);
                let want = match l.get("security") {
                    Some("open") => Some(Want::Open),
                    Some("psk") => Some(Want::Psk),
                    _ => None,
                };
                let (Some(ssid), Some(want)) = (ssid, want) else {
                    out.push(Out::Ctl(Line::new("error").with("why", "bad-join")));
                    return out;
                };
                if !matches!(self.st, State::Idle) {
                    self.leave(REASON_LEAVING, true, "new-join", &mut out);
                }
                out.push(Out::Ctl(state_line("scanning")));
                self.start_scan(Some(JoinReq { ssid, want }), &mut out);
            }
            "leave" => self.leave(REASON_LEAVING, true, "asked", &mut out),
            "status" => {
                let mut s = Line::new("status").with("state", self.state_name());
                if let Some(t) = self.target() {
                    s = s.with_hex("ssid", &t.bss.ssid).with("bssid", fmt_mac(&t.bss.bssid)).with("freq", t.bss.freq).with("security", t.bss.security.name()).with("pmf", t.pmf as u8);
                }
                out.push(Out::Ctl(s));
            }
            _ => out.push(Out::Ctl(Line::new("error").with("why", "unknown-command"))),
        }
        out
    }

    fn select(&self, j: &JoinReq) -> Result<Target, &'static str> {
        let mut best: Option<&BssInfo> = None;
        let mut wrong_security = false;
        for b in self.results.iter().filter(|b| b.ssid == j.ssid) {
            let ok = matches!((j.want, b.security), (Want::Open, Security::Open) | (Want::Psk, Security::Psk { .. }));
            if !ok {
                // Never join a remembered secured network as open, or with
                // weaker security than remembered (wifi.md §8.5).
                wrong_security = true;
                continue;
            }
            if best.is_none_or(|x| b.rssi > x.rssi) {
                best = Some(b);
            }
        }
        let Some(b) = best else {
            return Err(if wrong_security { "security-mismatch" } else { "not-found" });
        };
        let (own_rsne, akm, pmf) = match b.security {
            Security::Psk { akm, pmf } => {
                // PMF capable by default; on whenever the AP offers it.
                let on = pmf != Pmf::No;
                let caps = if on { rsne::CAP_MFPC } else { 0 };
                (Some(Rsne::build(rsne::CIPHER_CCMP128, rsne::CIPHER_CCMP128, akm.suite(), caps, None)), Some(akm), on)
            }
            _ => (None, None, false),
        };
        Ok(Target { bss: b.clone(), own_rsne, akm, pmf })
    }

    fn send_auth(&mut self, t: &Target, out: &mut Vec<Out>) {
        let s = self.seq();
        out.push(Out::Kernel(Req::TxMgmt { freq: t.bss.freq, frame: mgmt::auth_open(&self.own, &t.bss.bssid, s) }));
    }

    fn send_assoc(&mut self, t: &Target, out: &mut Vec<Out>) {
        let mut cap = ie::CAP_ESS;
        if t.own_rsne.is_some() {
            cap |= ie::CAP_PRIVACY;
        }
        cap |= t.bss.cap & (ie::CAP_SHORT_PREAMBLE | ie::CAP_SHORT_SLOT);
        let r = mgmt::AssocReq {
            cap,
            listen_interval: 10,
            ssid: &t.bss.ssid,
            rates: &t.bss.rates,
            rsne: t.own_rsne.as_deref(),
            wmm: t.bss.wmm,
        };
        let s = self.seq();
        out.push(Out::Kernel(Req::TxMgmt { freq: t.bss.freq, frame: mgmt::assoc_req(&self.own, &t.bss.bssid, s, &r) }));
    }

    pub fn scan_done(&mut self, status: i32, now: u64) -> Vec<Out> {
        let mut out = Vec::new();
        let State::Scanning { join } = &self.st else {
            return out;
        };
        let join = join.clone();
        let mut done = Line::new("scan-done").with("status", status).with("count", self.results.len());
        if join.is_some() {
            done = done.with("for", "join");
        }
        if join.is_none() {
            for b in &self.results {
                out.push(Out::Ctl(
                    Line::new("bss")
                        .with_hex("ssid", &b.ssid)
                        .with("bssid", fmt_mac(&b.bssid))
                        .with("freq", b.freq)
                        .with("rssi", b.rssi)
                        .with("security", b.security.name()),
                ));
            }
        }
        out.push(Out::Ctl(done));
        match join {
            None => self.st = State::Idle,
            Some(j) => match self.select(&j) {
                Err(why) => {
                    self.st = State::Idle;
                    out.push(Out::Ctl(state_line("failed").with("why", why)));
                }
                Ok(t) => {
                    out.push(Out::Ctl(state_line("authenticating").with("bssid", fmt_mac(&t.bss.bssid)).with("freq", t.bss.freq)));
                    self.send_auth(&t, &mut out);
                    self.st = State::Auth { t, tries: 1, deadline: now + MGMT_TIMEOUT_MS };
                }
            },
        }
        out
    }

    /// A management frame from the kernel (MgmtRx).
    pub fn mgmt_rx(&mut self, freq: u16, rssi: i8, flags: u8, f: &[u8], now: u64) -> Vec<Out> {
        let mut out = Vec::new();
        if f.len() < 24 {
            return out;
        }
        let sub = mgmt::subtype(f);
        if flags & rx_flags::SCAN != 0 || matches!(sub, mgmt::BEACON | mgmt::PROBE_RESP) {
            if let (State::Scanning { .. }, Some(b)) = (&self.st, ie::parse_bss(f, freq, rssi)) {
                if let Some(x) = self.results.iter_mut().find(|x| x.bssid == b.bssid) {
                    *x = b;
                } else if self.results.len() < MAX_RESULTS {
                    self.results.push(b);
                }
            }
            return out;
        }
        let Some(t) = self.target().cloned() else {
            return out;
        };
        let (a1, a2, a3) = (mgmt::addr1(f), mgmt::addr2(f), mgmt::addr3(f));
        let group = a1[0] & 1 != 0;
        if a2 != t.bss.bssid || a3 != t.bss.bssid || (!group && a1 != self.own) {
            return out;
        }
        let up = matches!(self.st, State::Up { .. });
        // Once PMF is on and the PTK installed, robust frames count only if
        // the radio decrypted them (unicast) or checked their BIP MIC
        // (group); anything else is treated as forged (wifi.md §8.5).
        let robust = matches!(sub, mgmt::DEAUTH | mgmt::DISASSOC | mgmt::ACTION);
        let valid = if group { flags & rx_flags::MIC_OK != 0 } else { flags & rx_flags::DECRYPTED != 0 };
        if up && t.pmf && robust && !valid {
            if matches!(sub, mgmt::DEAUTH | mgmt::DISASSOC) && !group && self.sa_query.is_none() {
                // Ask the AP whether our association is still alive (§11.13).
                let mut tr = [0u8; 2];
                (self.rand)(&mut tr);
                let s = self.seq();
                out.push(Out::Kernel(Req::TxMgmt { freq: t.bss.freq, frame: mgmt::sa_query(&self.own, &t.bss.bssid, s, mgmt::SA_QUERY_REQ, tr) }));
                self.sa_query = Some((tr, now + SA_QUERY_TIMEOUT_MS));
                out.push(Out::Ctl(Line::new("event").with("what", "unprotected-deauth-ignored")));
            }
            return out;
        }
        if !t.pmf && sub == mgmt::ACTION && mgmt::protected(f) {
            return out;
        }
        match (sub, &self.st) {
            (mgmt::AUTH, State::Auth { t, .. }) => {
                let t = t.clone();
                let Some((alg, tr, status)) = mgmt::parse_auth(f) else { return out };
                if alg != 0 || tr != 2 {
                    return out;
                }
                if status != 0 {
                    self.st = State::Idle;
                    out.push(Out::Ctl(state_line("failed").with("why", "auth-refused").with("status", status)));
                    return out;
                }
                out.push(Out::Ctl(state_line("associating")));
                self.send_assoc(&t, &mut out);
                self.st = State::Assoc { t, tries: 1, deadline: now + MGMT_TIMEOUT_MS };
            }
            (mgmt::ASSOC_RESP, State::Assoc { t, .. }) => {
                let t = t.clone();
                let Some((status, aid, ies)) = mgmt::parse_assoc_resp(f) else { return out };
                if status != 0 || !(1..=aura_proto::AID_MAX).contains(&aid) {
                    self.st = State::Idle;
                    out.push(Out::Ctl(state_line("failed").with("why", "assoc-refused").with("status", status)));
                    return out;
                }
                let qos = t.bss.wmm && ie::has_wmm(ies);
                let mut flags = 0;
                if qos {
                    flags |= bss_flags::QOS;
                }
                if t.pmf {
                    flags |= bss_flags::PMF;
                }
                if t.bss.cap & ie::CAP_SHORT_PREAMBLE != 0 {
                    flags |= bss_flags::SHORT_PREAMBLE;
                }
                if t.bss.cap & ie::CAP_SHORT_SLOT != 0 {
                    flags |= bss_flags::SHORT_SLOT;
                }
                let bss = Bss {
                    bssid: t.bss.bssid,
                    freq: t.bss.freq,
                    width: Width::W20,
                    center1: 0,
                    center2: 0,
                    aid,
                    beacon_int: t.bss.beacon_int,
                    dtim_period: t.bss.dtim.max(1),
                    flags,
                    basic_rates: ie::rate_bits(&t.bss.rates, true),
                    rates: ie::rate_bits(&t.bss.rates, false),
                    ht_mcs: [0; 10],
                    vht_rx_mcs: 0,
                    vht_tx_mcs: 0,
                    he_mcs: [0; 6],
                    ampdu_factor: 0,
                    ampdu_density: 0,
                };
                out.push(Out::Kernel(Req::SetBss(bss)));
                let akm = t.akm.map(|a| match a {
                    Akm::Psk => 2,
                    Akm::PskSha256 => 6,
                });
                let mut k = Line::new("assoc")
                    .with_hex("ssid", &t.bss.ssid)
                    .with("bssid", fmt_mac(&t.bss.bssid))
                    .with("own", fmt_mac(&self.own))
                    .with("akm", akm.unwrap_or(0))
                    .with("pmf", t.pmf as u8);
                if let (Some(o), Some(a)) = (&t.own_rsne, &t.bss.rsne) {
                    k = k.with_hex("own_rsne", o).with_hex("ap_rsne", a);
                }
                out.push(Out::Keys(k));
                out.push(Out::Ctl(state_line("associated").with("bssid", fmt_mac(&t.bss.bssid)).with("aid", aid).with("qos", qos as u8).with("pmf", t.pmf as u8)));
                self.st = State::Keys { t, deadline: now + KEYS_TIMEOUT_MS };
            }
            (mgmt::DEAUTH | mgmt::DISASSOC, _) => {
                let reason = mgmt::reason(f).unwrap_or(0);
                self.leave(reason, false, if sub == mgmt::DEAUTH { "deauth" } else { "disassoc" }, &mut out);
            }
            (mgmt::ACTION, State::Up { .. }) => match mgmt::parse_sa_query(f) {
                Some((mgmt::SA_QUERY_REQ, tr)) if !group => {
                    let s = self.seq();
                    out.push(Out::Kernel(Req::TxMgmt { freq: t.bss.freq, frame: mgmt::sa_query(&self.own, &t.bss.bssid, s, mgmt::SA_QUERY_RESP, tr) }));
                }
                Some((mgmt::SA_QUERY_RESP, tr)) if self.sa_query.is_some_and(|(x, _)| x == tr) => {
                    self.sa_query = None;
                    out.push(Out::Ctl(Line::new("event").with("what", "sa-query-ok")));
                }
                _ => {}
            },
            _ => {}
        }
        out
    }

    /// A message from aura-keys.
    pub fn keys_msg(&mut self, l: &Line) -> Vec<Out> {
        let mut out = Vec::new();
        match (l.verb.as_str(), &self.st) {
            ("authorized", State::Keys { t, .. }) => {
                let t = t.clone();
                out.push(Out::Ctl(state_line("authorized").with("bssid", fmt_mac(&t.bss.bssid))));
                self.st = State::Up { t };
            }
            ("failed", State::Keys { .. } | State::Up { .. }) => {
                let why = l.get("why").unwrap_or("keys").to_string();
                self.leave(REASON_4WAY_TIMEOUT, true, &why, &mut out);
            }
            _ => {}
        }
        out
    }

    /// The kernel left the BSS by itself (Disassociated event).
    pub fn kernel_disassociated(&mut self, reason: u16) -> Vec<Out> {
        let mut out = Vec::new();
        if matches!(self.st, State::Keys { .. } | State::Up { .. }) {
            self.sa_query = None;
            self.st = State::Idle;
            out.push(Out::Keys(Line::new("disassoc")));
            out.push(Out::Ctl(state_line("disconnected").with("reason", reason).with("why", "kernel")));
        }
        out
    }

    /// A kernel request failed.
    pub fn request_failed(&mut self, what: &str, status: i32) -> Vec<Out> {
        let mut out = Vec::new();
        if what == "scan" {
            if let State::Scanning { .. } = self.st {
                self.st = State::Idle;
                out.push(Out::Ctl(state_line("failed").with("why", "scan").with("status", status)));
            }
        } else if what == "set-bss" || what == "tx-mgmt" {
            self.leave(REASON_LEAVING, false, what, &mut out);
        }
        out
    }

    pub fn tick(&mut self, now: u64) -> Vec<Out> {
        let mut out = Vec::new();
        if let Some((_, d)) = self.sa_query {
            if now >= d {
                // No protected answer: the association is really gone.
                self.leave(REASON_LEAVING, false, "sa-query-timeout", &mut out);
                return out;
            }
        }
        match &mut self.st {
            State::Auth { t, tries, deadline } | State::Assoc { t, tries, deadline } if now >= *deadline => {
                if *tries >= MGMT_TRIES {
                    let auth = matches!(self.st, State::Auth { .. });
                    self.st = State::Idle;
                    out.push(Out::Ctl(state_line("failed").with("why", if auth { "auth-timeout" } else { "assoc-timeout" })));
                    return out;
                }
                *tries += 1;
                *deadline = now + MGMT_TIMEOUT_MS;
                let t = t.clone();
                if matches!(self.st, State::Auth { .. }) {
                    self.send_auth(&t, &mut out);
                } else {
                    self.send_assoc(&t, &mut out);
                }
            }
            State::Keys { deadline, .. } if now >= *deadline => {
                self.leave(REASON_4WAY_TIMEOUT, true, "handshake-timeout", &mut out);
            }
            _ => {}
        }
        out
    }
}
