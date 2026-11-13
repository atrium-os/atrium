//! The station state machine: scan, select, authenticate, associate, hand
//! the handshake to aura-keys, and police management frames once PMF is on.
//! No I/O: the daemon feeds events in and carries out the [`Out`]s.

use crate::ie::{self, BssInfo, Pmf, Security};
use crate::mgmt::{self, Mac};
use aura_ipc::{fmt_mac, Line};
use aura_proto::{bss_flags, rx_flags, Bss, Width};
use aura_rsn::crypto::Akm;
use aura_rsn::rsne::{self, Rsne};
use std::collections::HashSet;

/// Kernel requests (owned; the daemon encodes them as aura-proto messages).
#[derive(Clone, Debug, PartialEq)]
pub enum Req {
    Scan { mac: Mac },
    SetMac(Mac),
    SetRegulatory { alpha2: [u8; 2], rules: Vec<aura_regdb::RegRule> },
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
    /// A password network: WPA3 when the AP offers it, else WPA2.
    Password,
    /// WPA3 only (the user said so, or the network disabled transition).
    Wpa3,
}

/// Which address to use on a network (wifi.md §3.4, decision 8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MacPolicy {
    /// Random, stable per network (the default).
    Network,
    /// A new random address for every join.
    Random,
    /// The radio's burned-in address (for networks that filter by MAC).
    Device,
}

#[derive(Clone, Debug)]
struct JoinReq {
    ssid: Vec<u8>,
    want: Want,
    mac: MacPolicy,
}

#[derive(Clone, Debug)]
struct Target {
    bss: BssInfo,
    own_rsne: Option<Vec<u8>>,
    own_rsnxe: Option<Vec<u8>>,
    akm: Option<Akm>,
    pmf: bool,
    h2e: bool,
}

#[derive(Debug)]
enum State {
    Idle,
    Scanning { join: Option<JoinReq> },
    Auth { t: Target, tries: u8, deadline: u64 },
    /// SAE authentication; aura-keys does the crypto. `last` is the frame
    /// to retransmit (none while waiting for aura-keys).
    Sae { t: Target, tries: u8, deadline: u64, last: Option<Vec<u8>> },
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
/// SAE's crypto in aura-keys under TCG takes a while.
const SAE_KEYS_TIMEOUT_MS: u64 = 5000;
/// Roaming (no 802.11r yet: a roam is a fresh authentication, association
/// and handshake with the new BSS). Scan in the background when the
/// current BSS's beacons fall below `ROAM_RSSI`, at most every
/// `ROAM_SCAN_EVERY_MS`; move when another BSS of the network is
/// `ROAM_GAIN` dB better.
const ROAM_RSSI: i32 = -70;
const ROAM_GAIN: i32 = 8;
const ROAM_SCAN_EVERY_MS: u64 = 10_000;
/// The RSNXE we send when using H2E: SAE hash-to-element.
const OWN_RSNXE_H2E: [u8; 3] = [ie::RSNX, 1, ie::RSNX_SAE_H2E];

pub const REASON_LEAVING: u16 = 3;
pub const REASON_4WAY_TIMEOUT: u16 = 15;

pub struct Station {
    /// The radio's own address, and the one in use now.
    device: Mac,
    own: Mac,
    /// From aura-keys: the key for per-network addresses.
    mac_key: Option<[u8; 32]>,
    /// Networks that disabled WPA2 transition (Transition Disable KDE).
    /// Kept for the daemon's life; atrium-net is told and persists it.
    wpa3_only: HashSet<Vec<u8>>,
    /// The join in force (for roaming), the current BSS's beacon RSSI
    /// (moving average, dBm × 16), and the background scan state.
    joined: Option<JoinReq>,
    rssi16: Option<i32>,
    roam_scan: bool,
    last_roam_scan: Option<u64>,
    /// The country atrium-net set ("00" until then), and whether an AP's
    /// Country element has narrowed its rules for this association.
    country: [u8; 2],
    narrowed: bool,
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
        Station { device: own, own, mac_key: None, wpa3_only: HashSet::new(), joined: None, rssi16: None, roam_scan: false, last_roam_scan: None, country: *b"00", narrowed: false, st: State::Idle, results: Vec::new(), seq: 0, sa_query: None, rand }
    }

    pub fn state_name(&self) -> &'static str {
        match self.st {
            State::Idle => "idle",
            State::Scanning { .. } => "scanning",
            State::Auth { .. } | State::Sae { .. } => "authenticating",
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
            State::Auth { t, .. } | State::Sae { t, .. } | State::Assoc { t, .. } | State::Keys { t, .. } | State::Up { t } => Some(t),
            _ => None,
        }
    }

    fn random_mac(&mut self) -> Mac {
        let mut m = [0u8; 6];
        (self.rand)(&mut m);
        m[0] = (m[0] & 0xfc) | 0x02; // unicast, locally administered
        m
    }

    /// The address for joining `ssid` under `policy`.
    fn join_mac(&mut self, ssid: &[u8], policy: MacPolicy) -> Mac {
        match (policy, self.mac_key) {
            (MacPolicy::Device, _) => self.device,
            (MacPolicy::Network, Some(k)) => {
                use hmac::Mac as _;
                let mut h = <hmac::Hmac<sha2::Sha256> as hmac::Mac>::new_from_slice(&k).expect("any key length");
                h.update(b"aura per-network address");
                h.update(ssid);
                let d = h.finalize().into_bytes();
                let mut m = [0u8; 6];
                m.copy_from_slice(&d[..6]);
                m[0] = (m[0] & 0xfc) | 0x02;
                m
            }
            // No key yet (aura-keys not up), or asked for: random.
            _ => self.random_mac(),
        }
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
        self.rssi16 = None;
        self.roam_scan = false;
        self.st = State::Idle;
        self.unnarrow(out);
        out.push(Out::Ctl(state_line("disconnected").with("reason", reason).with("why", why)));
    }

    fn own_rules(&self) -> Vec<aura_regdb::RegRule> {
        aura_regdb::country(self.country).unwrap_or_else(aura_regdb::world).rules.to_vec()
    }

    /// Back to our own country's rules after an AP narrowed them.
    fn unnarrow(&mut self, out: &mut Vec<Out>) {
        if self.narrowed {
            self.narrowed = false;
            out.push(Out::Kernel(Req::SetRegulatory { alpha2: self.country, rules: self.own_rules() }));
        }
    }

    /// Associated with an AP that names a country: its rules may only
    /// narrow ours (wifi.md §3.6).
    fn narrow_for(&mut self, ap: Option<[u8; 2]>, out: &mut Vec<Out>) {
        let Some(cc) = ap.filter(|c| *c != self.country) else { return };
        let Some(c) = aura_regdb::country(cc) else { return };
        let rules = aura_regdb::intersect(&self.own_rules(), c.rules);
        self.narrowed = true;
        out.push(Out::Ctl(Line::new("event").with("what", "regulatory-narrowed").with("ap_country", String::from_utf8_lossy(&cc)).with("rules", rules.len())));
        out.push(Out::Kernel(Req::SetRegulatory { alpha2: self.country, rules }));
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
                    Some("psk") => Some(Want::Password),
                    Some("sae") => Some(Want::Wpa3),
                    _ => None,
                };
                let mac = match l.get("mac") {
                    None | Some("network") => Some(MacPolicy::Network),
                    Some("random") => Some(MacPolicy::Random),
                    Some("device") => Some(MacPolicy::Device),
                    _ => None,
                };
                let (Some(ssid), Some(want), Some(mac)) = (ssid, want, mac) else {
                    out.push(Out::Ctl(Line::new("error").with("why", "bad-join")));
                    return out;
                };
                if !matches!(self.st, State::Idle) {
                    self.leave(REASON_LEAVING, true, "new-join", &mut out);
                }
                out.push(Out::Ctl(state_line("scanning")));
                self.joined = Some(JoinReq { ssid: ssid.clone(), want, mac });
                self.last_roam_scan = None;
                self.start_scan(Some(JoinReq { ssid, want, mac }), &mut out);
            }
            "country" => {
                let cc = l.get("cc").map(|c| c.as_bytes().to_vec()).unwrap_or_default();
                let cc: Option<[u8; 2]> = cc.try_into().ok();
                match cc.and_then(aura_regdb::country) {
                    Some(c) => {
                        self.country = c.alpha2;
                        self.narrowed = false;
                        out.push(Out::Kernel(Req::SetRegulatory { alpha2: c.alpha2, rules: c.rules.to_vec() }));
                        out.push(Out::Ctl(Line::new("ok").with("country", String::from_utf8_lossy(&c.alpha2)).with("rules", c.rules.len())));
                        if let State::Up { t } = &self.st {
                            let ap = t.bss.country;
                            self.narrow_for(ap, &mut out);
                        }
                    }
                    None => out.push(Out::Ctl(Line::new("error").with("why", "unknown-country"))),
                }
            }
            "leave" => {
                self.joined = None;
                self.leave(REASON_LEAVING, true, "asked", &mut out)
            }
            "status" => {
                let mut s = Line::new("status").with("state", self.state_name());
                if let Some(t) = self.target() {
                    let used = match t.akm {
                        Some(Akm::Sae) => if t.h2e { "sae-h2e" } else { "sae" },
                        Some(_) => "psk",
                        None => "open",
                    };
                    s = s.with("country", String::from_utf8_lossy(&self.country)).with("narrowed", self.narrowed as u8);
                    s = s.with_hex("ssid", &t.bss.ssid).with("bssid", fmt_mac(&t.bss.bssid)).with("freq", t.bss.freq).with("security", t.bss.security.name()).with("using", used).with("pmf", t.pmf as u8).with("mac", fmt_mac(&self.own));
                }
                out.push(Out::Ctl(s));
            }
            _ => out.push(Out::Ctl(Line::new("error").with("why", "unknown-command"))),
        }
        out
    }

    /// The best BSS for `j` among the scan results, other than `skip`.
    fn select(&self, j: &JoinReq) -> Result<Target, &'static str> {
        self.select_from(j, None)
    }

    fn select_from(&self, j: &JoinReq, skip: Option<&Mac>) -> Result<Target, &'static str> {
        let mut best: Option<&BssInfo> = None;
        let mut wrong_security = false;
        for b in self.results.iter().filter(|b| b.ssid == j.ssid && Some(&b.bssid) != skip) {
            let wpa3 = j.want == Want::Wpa3 || self.wpa3_only.contains(&j.ssid);
            let ok = match (j.want, b.security) {
                (Want::Open, Security::Open) => true,
                (Want::Password, Security::Psk { .. }) => !wpa3,
                (Want::Password | Want::Wpa3, Security::Sae { .. }) => true,
                _ => false,
            };
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
        let build = |akm: Akm, caps: u16| Some(Rsne::build(rsne::CIPHER_CCMP128, rsne::CIPHER_CCMP128, akm.suite(), caps, None));
        let t = match b.security {
            Security::Psk { akm, pmf } => {
                // PMF capable by default; on whenever the AP offers it.
                let on = pmf != Pmf::No;
                let caps = if on { rsne::CAP_MFPC } else { 0 };
                Target { bss: b.clone(), own_rsne: build(akm, caps), own_rsnxe: None, akm: Some(akm), pmf: on, h2e: false }
            }
            Security::Sae { .. } => {
                // SAE whenever offered, even in transition mode; PMF on;
                // H2E when the AP does it (Dragonblood: no hunting loop).
                let h2e = b.h2e || b.h2e_only;
                let rsnxe = h2e.then(|| OWN_RSNXE_H2E.to_vec());
                Target { bss: b.clone(), own_rsne: build(Akm::Sae, rsne::CAP_MFPC), own_rsnxe: rsnxe, akm: Some(Akm::Sae), pmf: true, h2e }
            }
            _ => Target { bss: b.clone(), own_rsne: None, own_rsnxe: None, akm: None, pmf: false, h2e: false },
        };
        Ok(t)
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
            rsnxe: t.own_rsnxe.as_deref(),
            wmm: t.bss.wmm,
        };
        let s = self.seq();
        out.push(Out::Kernel(Req::TxMgmt { freq: t.bss.freq, frame: mgmt::assoc_req(&self.own, &t.bss.bssid, s, &r) }));
    }

    /// Starts authentication with `t` (after a join scan, or to roam).
    fn begin(&mut self, t: Target, now: u64, out: &mut Vec<Out>) {
        out.push(Out::Ctl(state_line("authenticating").with("bssid", fmt_mac(&t.bss.bssid)).with("freq", t.bss.freq).with("mac", fmt_mac(&self.own))));
        if t.akm == Some(Akm::Sae) {
            out.push(Out::Keys(
                Line::new("sae-start")
                    .with_hex("ssid", &t.bss.ssid)
                    .with("bssid", fmt_mac(&t.bss.bssid))
                    .with("own", fmt_mac(&self.own))
                    .with("h2e", t.h2e as u8),
            ));
            self.st = State::Sae { t, tries: 0, deadline: now + SAE_KEYS_TIMEOUT_MS, last: None };
        } else {
            self.send_auth(&t, out);
            self.st = State::Auth { t, tries: 1, deadline: now + MGMT_TIMEOUT_MS };
        }
    }

    /// A background scan while connected finished: roam if a BSS of the
    /// same network is clearly better.
    fn roam_done(&mut self, now: u64) -> Vec<Out> {
        let mut out = Vec::new();
        self.roam_scan = false;
        let (Some(j), State::Up { t }) = (self.joined.clone(), &self.st) else {
            return out;
        };
        let cur = self.rssi16.map(|r| r / 16).unwrap_or(t.bss.rssi as i32);
        let from = t.bss.bssid;
        let Ok(c) = self.select_from(&j, Some(&from)) else {
            return out;
        };
        if (c.bss.rssi as i32) < cur + ROAM_GAIN {
            return out;
        }
        out.push(Out::Ctl(Line::new("event").with("what", "roam").with("from", fmt_mac(&from)).with("to", fmt_mac(&c.bss.bssid)).with("rssi", cur).with("new_rssi", c.bss.rssi)));
        self.leave(REASON_LEAVING, true, "roam", &mut out);
        self.begin(c, now, &mut out);
        out
    }

    pub fn scan_done(&mut self, status: i32, now: u64) -> Vec<Out> {
        if self.roam_scan {
            return self.roam_done(now);
        }
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
                    let mac = self.join_mac(&j.ssid, j.mac);
                    if mac != self.own {
                        self.own = mac;
                        out.push(Out::Kernel(Req::SetMac(mac)));
                    }
                    self.begin(t, now, &mut out);
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
            // Our BSS's beacons: its signal, for roaming.
            if let State::Up { t } = &self.st {
                if sub == mgmt::BEACON && mgmt::addr2(f) == t.bss.bssid && freq == t.bss.freq {
                    let r = rssi as i32 * 16;
                    self.rssi16 = Some(self.rssi16.map_or(r, |a| a + (r - a) / 4));
                }
            }
            let collecting = matches!(self.st, State::Scanning { .. }) || self.roam_scan;
            if let (true, Some(b)) = (collecting, ie::parse_bss(f, freq, rssi)) {
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
            (mgmt::AUTH, State::Sae { .. }) => {
                let Some((alg, tr, status)) = mgmt::parse_auth(f) else { return out };
                if alg != aura_rsn::sae::AUTH_ALG_SAE || !(1..=2).contains(&tr) {
                    return out;
                }
                use aura_rsn::sae::{STATUS_ANTI_CLOGGING, STATUS_H2E, STATUS_SUCCESS};
                let usable = match tr {
                    1 => matches!(status, STATUS_SUCCESS | STATUS_H2E | STATUS_ANTI_CLOGGING),
                    _ => status == STATUS_SUCCESS,
                };
                if !usable {
                    out.push(Out::Keys(Line::new("sae-abort")));
                    self.st = State::Idle;
                    out.push(Out::Ctl(state_line("failed").with("why", "auth-refused").with("status", status)));
                    return out;
                }
                // The frame goes to aura-keys as is; it answers with the
                // next frame's body, "sae-done", or "sae-failed".
                out.push(Out::Keys(Line::new("sae-rx").with("trans", tr).with("status", status).with_hex("body", mgmt::auth_body(f))));
                if let State::Sae { deadline, last, .. } = &mut self.st {
                    // Waiting on aura-keys now, not on the AP.
                    *deadline = now + SAE_KEYS_TIMEOUT_MS;
                    *last = None;
                }
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
                let akm = t.akm.map(|a| a.suite() & 0xff);
                let mut k = Line::new("assoc")
                    .with_hex("ssid", &t.bss.ssid)
                    .with("bssid", fmt_mac(&t.bss.bssid))
                    .with("own", fmt_mac(&self.own))
                    .with("akm", akm.unwrap_or(0))
                    .with("pmf", t.pmf as u8);
                if let (Some(o), Some(a)) = (&t.own_rsne, &t.bss.rsne) {
                    k = k.with_hex("own_rsne", o).with_hex("ap_rsne", a);
                }
                if let Some(x) = &t.own_rsnxe {
                    k = k.with_hex("own_rsnxe", x);
                }
                if let Some(x) = &t.bss.rsnxe {
                    k = k.with_hex("ap_rsnxe", x);
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
    pub fn keys_msg(&mut self, l: &Line, now: u64) -> Vec<Out> {
        let mut out = Vec::new();
        match (l.verb.as_str(), &self.st) {
            ("hello", _) => {
                if let Some(k) = l.hex("mac_key").and_then(|k| <[u8; 32]>::try_from(k).ok()) {
                    self.mac_key = Some(k);
                }
            }
            ("sae-commit" | "sae-confirm", State::Sae { t, .. }) => {
                let t = t.clone();
                let (Some(body), Some(status)) = (l.hex("body"), l.num::<u16>("status")) else {
                    return out;
                };
                let tr = if l.verb == "sae-commit" { 1 } else { 2 };
                let s = self.seq();
                let f = mgmt::auth_sae(&self.own, &t.bss.bssid, s, tr, status, &body);
                out.push(Out::Kernel(Req::TxMgmt { freq: t.bss.freq, frame: f.clone() }));
                self.st = State::Sae { t, tries: 1, deadline: now + MGMT_TIMEOUT_MS, last: Some(f) };
            }
            ("sae-done", State::Sae { t, .. }) => {
                let t = t.clone();
                out.push(Out::Ctl(state_line("associating")));
                self.send_assoc(&t, &mut out);
                self.st = State::Assoc { t, tries: 1, deadline: now + MGMT_TIMEOUT_MS };
            }
            ("sae-failed", State::Sae { .. }) => {
                self.st = State::Idle;
                let why = format!("sae-{}", l.get("why").unwrap_or("failed"));
                out.push(Out::Ctl(state_line("failed").with("why", &why)));
            }
            ("authorized", State::Keys { t, .. }) => {
                let t = t.clone();
                let mut st = state_line("authorized").with("bssid", fmt_mac(&t.bss.bssid));
                if l.num::<u8>("td").is_some_and(|b| b & 1 != 0) {
                    // The network turned WPA2 off for us: never again.
                    self.wpa3_only.insert(t.bss.ssid.clone());
                    st = st.with("transition_disabled", 1);
                }
                out.push(Out::Ctl(st));
                let ap = t.bss.country;
                self.st = State::Up { t };
                self.narrow_for(ap, &mut out);
            }
            ("failed", State::Sae { .. }) => {
                self.st = State::Idle;
                out.push(Out::Ctl(state_line("failed").with("why", l.get("why").unwrap_or("keys"))));
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
        if what == "scan" && self.roam_scan {
            self.roam_scan = false;
        } else if what == "scan" {
            if let State::Scanning { .. } = self.st {
                self.st = State::Idle;
                out.push(Out::Ctl(state_line("failed").with("why", "scan").with("status", status)));
            }
        } else if what == "set-bss" || what == "tx-mgmt" || what == "set-mac" {
            self.leave(REASON_LEAVING, false, what, &mut out);
        }
        out
    }

    pub fn tick(&mut self, now: u64) -> Vec<Out> {
        let mut out = Vec::new();
        if let (State::Up { .. }, Some(r), false) = (&self.st, self.rssi16, self.roam_scan) {
            let due = self.last_roam_scan.is_none_or(|l| now >= l + ROAM_SCAN_EVERY_MS);
            if r / 16 < ROAM_RSSI && due && self.joined.is_some() {
                self.last_roam_scan = Some(now);
                self.roam_scan = true;
                self.results.clear();
                // Same address as the association: scans while connected
                // must not reveal a second identity next to it.
                out.push(Out::Kernel(Req::Scan { mac: self.own }));
                out.push(Out::Ctl(Line::new("event").with("what", "roam-scan").with("rssi", r / 16)));
            }
        }
        if let Some((_, d)) = self.sa_query {
            if now >= d {
                // No protected answer: the association is really gone.
                self.leave(REASON_LEAVING, false, "sa-query-timeout", &mut out);
                return out;
            }
        }
        let freq = self.target().map(|t| t.bss.freq).unwrap_or(0);
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
            State::Sae { tries, deadline, last, .. } if now >= *deadline => {
                match last {
                    Some(f) if *tries < MGMT_TRIES => {
                        *tries += 1;
                        *deadline = now + MGMT_TIMEOUT_MS;
                        out.push(Out::Kernel(Req::TxMgmt { freq, frame: f.clone() }));
                    }
                    _ => {
                        self.st = State::Idle;
                        out.push(Out::Keys(Line::new("sae-abort")));
                        out.push(Out::Ctl(state_line("failed").with("why", "auth-timeout")));
                    }
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
