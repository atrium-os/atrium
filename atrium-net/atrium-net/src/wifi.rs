//! Wi-Fi policy (docs/spec/atrium-net.md §4): remembered-network profiles,
//! auto-join, and turning aura-mlme's events into link up/down for the
//! interface's lease. Pure: `main` carries the actions out.

use aura_ipc::{Line, MAX_LINE};

/// Looking for a known network when none was in range.
pub const SCAN_BACKOFF_MS: [u64; 4] = [30_000, 60_000, 120_000, 300_000];
/// Right after losing a network (a tunnel, a dropout, a hotspot asleep) it is
/// likely to come back: look again soon, for `RECENT_LOSS_MS`.
pub const FAST_BACKOFF_MS: [u64; 6] = [2_000, 5_000, 10_000, 20_000, 30_000, 60_000];
pub const RECENT_LOSS_MS: u64 = 10 * 60_000;
pub const REJOIN_DELAY_MS: u64 = 2_000;
/// A network that failed this many joins in a row (a changed password, a
/// broken AP) is skipped by auto-join for `FAILING_HOLD_MS`.
pub const MAX_FAILURES: u32 = 3;
pub const FAILING_HOLD_MS: u64 = 10 * 60_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    pub ssid: Vec<u8>,
    /// An open network (no credential). Never auto-joined unless `auto`
    /// was set for it explicitly (decision 17); `remember --open` leaves it off.
    pub open: bool,
    /// Set by Transition Disable; never cleared automatically.
    pub wpa3_only: bool,
    pub mac: String,
    pub auto: bool,
    pub prio: i32,
    /// The user says this network is metered (a phone's hotspot, a data
    /// plan). Also inferred per lease (DHCP's ANDROID_METERED).
    pub metered: bool,
}

impl Profile {
    pub fn to_line(&self) -> Line {
        Line::new("net")
            .with_hex("ssid", &self.ssid)
            .with("open", self.open as u8)
            .with("wpa3_only", self.wpa3_only as u8)
            .with("mac", &self.mac)
            .with("auto", self.auto as u8)
            .with("prio", self.prio)
            .with("metered", self.metered as u8)
    }

    pub fn from_line(l: &Line) -> Option<Profile> {
        let ssid = l.hex("ssid").filter(|s| !s.is_empty() && s.len() <= 32)?;
        let mac = l.get("mac").unwrap_or("network");
        matches!(mac, "network" | "random" | "device").then_some(())?;
        Some(Profile {
            ssid,
            open: l.get("open") == Some("1"),
            wpa3_only: l.get("wpa3_only") == Some("1"),
            mac: mac.to_string(),
            auto: l.get("auto") != Some("0"),
            prio: l.num("prio").unwrap_or(0).clamp(-1000, 1000),
            metered: l.get("metered") == Some("1"),
        })
    }

    /// What aura-mlme is asked for: the profile's floor wins over the request.
    pub fn security(&self, wanted: Option<&str>) -> &'static str {
        if self.open {
            "open"
        } else if self.wpa3_only || wanted == Some("sae") {
            "sae"
        } else {
            "psk"
        }
    }
}

/// The saved state: profiles and the regulatory country.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Saved {
    pub profiles: Vec<Profile>,
    pub country: Option<String>,
}

pub const MAX_PROFILES: usize = 256;

impl Saved {
    pub fn parse(text: &str) -> Saved {
        let mut s = Saved::default();
        for line in text.lines().take(MAX_PROFILES + 1) {
            let Some(l) = Line::parse(line) else { continue };
            match l.verb.as_str() {
                "net" => {
                    if let Some(p) = Profile::from_line(&l) {
                        s.profiles.retain(|q| q.ssid != p.ssid);
                        s.profiles.push(p);
                    }
                }
                "country" => s.country = l.get("cc").filter(|c| c.len() == 2).map(|c| c.to_ascii_uppercase()),
                _ => {}
            }
        }
        s
    }

    pub fn render(&self) -> String {
        let mut out = String::from("# atrium-net Wi-Fi profiles (docs/spec/atrium-net.md §4); written by atrium-net\n");
        if let Some(c) = &self.country {
            out.push_str(&format!("country cc={c}\n"));
        }
        for p in &self.profiles {
            let b = p.to_line().encode().expect("a profile line fits");
            debug_assert!(b.len() <= MAX_LINE);
            out.push_str(&String::from_utf8_lossy(&b));
        }
        out
    }

    pub fn get(&self, ssid: &[u8]) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.ssid == ssid)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Act {
    Mlme(Line),
    /// Ask aura-keys to load the credential, then send this join to aura-mlme.
    Join { ssid: Vec<u8>, security: &'static str, mac: String },
    LeaseUp,
    /// Off the network: stop DHCP but keep the address for a grace period,
    /// so a short dropout does not break connections.
    LeaseDown,
    /// Remove the address now (the user left).
    Drop,
    LeaseMac([u8; 6]),
    Save,
    Event(Line),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum St {
    Idle,
    /// Our own scan, for auto-join.
    Scanning,
    Joining { ssid: Vec<u8>, rest: Vec<Vec<u8>> },
    Up { ssid: Vec<u8> },
}

pub struct Wifi {
    pub saved: Saved,
    st: St,
    seen: Vec<(Vec<u8>, i32)>,
    mac: Option<[u8; 6]>,
    /// The user left: no auto-join until they join or remember again.
    held: bool,
    next_scan: Option<u64>,
    backoff: usize,
    connected: bool,
    lost_at: Option<u64>,
    /// Consecutive failed joins per network, and until when it is skipped.
    failures: Vec<(Vec<u8>, u32, u64)>,
}

impl Wifi {
    pub fn new(saved: Saved) -> Wifi {
        Wifi { saved, st: St::Idle, seen: Vec::new(), mac: None, held: false, next_scan: None, backoff: 0, connected: false, lost_at: None, failures: Vec::new() }
    }

    pub fn state(&self) -> &'static str {
        match self.st {
            St::Idle => "idle",
            St::Scanning => "scanning",
            St::Joining { .. } => "joining",
            St::Up { .. } => "up",
        }
    }

    pub fn ssid(&self) -> Option<&[u8]> {
        match &self.st {
            St::Joining { ssid, .. } | St::Up { ssid } => Some(ssid),
            _ => None,
        }
    }

    fn autos(&self) -> bool {
        self.saved.profiles.iter().any(|p| p.auto)
    }

    /// (Re)connected to aura-mlme.
    pub fn mlme_up(&mut self, now: u64) -> Vec<Act> {
        self.connected = true;
        self.st = St::Idle;
        let mut a = Vec::new();
        if let Some(c) = &self.saved.country {
            a.push(Act::Mlme(Line::new("country").with("cc", c)));
        }
        self.next_scan = Some(now);
        a.extend(self.tick(now));
        a
    }

    pub fn mlme_down(&mut self) -> Vec<Act> {
        self.connected = false;
        let was_up = matches!(self.st, St::Up { .. } | St::Joining { .. });
        self.st = St::Idle;
        if was_up { vec![Act::LeaseDown] } else { vec![] }
    }

    pub fn tick(&mut self, now: u64) -> Vec<Act> {
        if !self.connected || self.held || self.st != St::Idle || !self.autos() {
            return vec![];
        }
        match self.next_scan {
            Some(t) if now >= t => {
                self.next_scan = None;
                self.st = St::Scanning;
                self.seen.clear();
                vec![Act::Mlme(Line::new("scan"))]
            }
            _ => vec![],
        }
    }

    pub fn deadline(&self) -> Option<u64> {
        self.next_scan
    }

    fn retry_later(&mut self, now: u64) {
        let recent = self.lost_at.is_some_and(|t| now < t + RECENT_LOSS_MS);
        let table: &[u64] = if recent { &FAST_BACKOFF_MS } else { &SCAN_BACKOFF_MS };
        let d = table[self.backoff.min(table.len() - 1)];
        self.backoff += 1;
        self.next_scan = Some(now + d);
    }

    fn join_next(&mut self, mut rest: Vec<Vec<u8>>, now: u64) -> Vec<Act> {
        while !rest.is_empty() {
            let ssid = rest.remove(0);
            if let Some(p) = self.saved.get(&ssid) {
                let act = Act::Join { ssid: ssid.clone(), security: p.security(None), mac: p.mac.clone() };
                self.st = St::Joining { ssid, rest };
                return vec![act];
            }
        }
        self.st = St::Idle;
        self.retry_later(now);
        vec![]
    }

    /// A join the user asked for (also clears a hold from `leave`).
    pub fn join(&mut self, ssid: Vec<u8>, wanted: Option<&str>, mac: Option<&str>) -> Vec<Act> {
        self.held = false;
        self.backoff = 0;
        let (security, mac) = match self.saved.get(&ssid) {
            Some(p) => (p.security(wanted), mac.unwrap_or(&p.mac).to_string()),
            None => (
                match wanted {
                    Some("open") => "open",
                    Some("sae") => "sae",
                    _ => "psk",
                },
                mac.unwrap_or("network").to_string(),
            ),
        };
        self.st = St::Joining { ssid: ssid.clone(), rest: Vec::new() };
        vec![Act::Join { ssid, security, mac }]
    }

    pub fn leave(&mut self) -> Vec<Act> {
        self.held = true;
        self.next_scan = None;
        self.lost_at = None;
        vec![Act::Mlme(Line::new("leave")), Act::Drop]
    }

    pub fn remember(&mut self, p: Profile, now: u64) -> Vec<Act> {
        let auto = p.auto;
        let p_ssid = p.ssid.clone();
        // Remembering keeps an earlier Transition Disable.
        let mut p = p;
        if let Some(old) = self.saved.get(&p.ssid) {
            p.wpa3_only |= old.wpa3_only;
        }
        self.saved.profiles.retain(|q| q.ssid != p.ssid);
        self.saved.profiles.push(p);
        if self.saved.profiles.len() > MAX_PROFILES {
            self.saved.profiles.remove(0);
        }
        self.failures.retain(|(s, _, _)| *s != p_ssid);
        if auto {
            self.held = false;
            self.backoff = 0;
            if self.st == St::Idle {
                self.next_scan = Some(now);
            }
        }
        vec![Act::Save]
    }

    pub fn forget(&mut self, ssid: &[u8]) -> Vec<Act> {
        self.saved.profiles.retain(|p| p.ssid != ssid);
        let mut a = vec![Act::Save];
        if self.ssid() == Some(ssid) {
            a.extend(self.leave());
            self.held = false;
            self.next_scan = None;
        }
        a
    }

    pub fn set_country(&mut self, cc: &str) -> Vec<Act> {
        self.saved.country = Some(cc.to_ascii_uppercase());
        vec![Act::Save, Act::Mlme(Line::new("country").with("cc", cc.to_ascii_uppercase()))]
    }

    /// A line from aura-mlme.
    pub fn mlme_line(&mut self, l: &Line, now: u64) -> Vec<Act> {
        let mut a = vec![Act::Event(l.clone())];
        match (l.verb.as_str(), l.get("state")) {
            ("bss", _) if self.st == St::Scanning => {
                if let (Some(ssid), Some(rssi)) = (l.hex("ssid"), l.num::<i32>("rssi")) {
                    self.seen.push((ssid, rssi));
                }
            }
            ("scan-done", _) if self.st == St::Scanning && l.get("for") != Some("join") => {
                let mut c: Vec<(&Profile, i32)> = self
                    .saved
                    .profiles
                    .iter()
                    .filter(|p| p.auto)
                    .filter(|p| !self.failures.iter().any(|(s, n, until)| *s == p.ssid && *n >= MAX_FAILURES && now < *until))
                    .filter_map(|p| self.seen.iter().filter(|(s, _)| *s == p.ssid).map(|(_, r)| *r).max().map(|r| (p, r)))
                    .collect();
                c.sort_by(|x, y| y.0.prio.cmp(&x.0.prio).then(y.1.cmp(&x.1)));
                let rest: Vec<Vec<u8>> = c.into_iter().map(|(p, _)| p.ssid.clone()).collect();
                a.extend(self.join_next(rest, now));
            }
            ("state", Some("authenticating")) => {
                if let Some(m) = l.mac("mac") {
                    if self.mac != Some(m) {
                        self.mac = Some(m);
                        a.push(Act::LeaseMac(m));
                    }
                }
            }
            ("state", Some("authorized")) => {
                let ssid = match &self.st {
                    St::Joining { ssid, .. } | St::Up { ssid } => Some(ssid.clone()),
                    _ => None,
                };
                if let Some(ssid) = ssid {
                    if l.get("transition_disabled") == Some("1") {
                        if let Some(p) = self.saved.profiles.iter_mut().find(|p| p.ssid == ssid) {
                            if !p.wpa3_only {
                                p.wpa3_only = true;
                                a.push(Act::Save);
                            }
                        }
                    }
                    self.backoff = 0;
                    self.lost_at = None;
                    self.failures.retain(|(s, _, _)| *s != ssid);
                    self.st = St::Up { ssid };
                    // Also after a roam: atrium-lease confirms the lease.
                    a.push(Act::LeaseUp);
                }
            }
            ("state", Some("failed")) => {
                if let St::Joining { ssid, rest } = std::mem::replace(&mut self.st, St::Idle) {
                    a.push(Act::LeaseDown);
                    let n = match self.failures.iter_mut().find(|(s, _, _)| *s == ssid) {
                        Some(f) => {
                            f.1 += 1;
                            f.2 = now + FAILING_HOLD_MS;
                            f.1
                        }
                        None => {
                            self.failures.push((ssid.clone(), 1, now + FAILING_HOLD_MS));
                            1
                        }
                    };
                    if n == MAX_FAILURES {
                        a.push(Act::Event(Line::new("event").with("what", "network-failing").with_hex("ssid", &ssid).with("why", l.get("why").unwrap_or("?"))));
                    }
                    if !self.held && !rest.is_empty() {
                        a.extend(self.join_next(rest, now));
                    } else {
                        self.retry_later(now);
                    }
                }
            }
            ("state", Some("disconnected")) => {
                if matches!(self.st, St::Up { .. } | St::Joining { .. }) {
                    a.push(Act::LeaseDown);
                    if !self.held {
                        self.lost_at = Some(now);
                        self.backoff = 0;
                    }
                }
                if self.st != St::Scanning {
                    self.st = St::Idle;
                    self.next_scan = Some(now + REJOIN_DELAY_MS);
                }
            }
            ("error", _) if self.st == St::Scanning => {
                self.st = St::Idle;
                self.retry_later(now);
            }
            _ => {}
        }
        a
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prof(s: &str, prio: i32) -> Profile {
        Profile { ssid: s.as_bytes().to_vec(), open: false, wpa3_only: false, mac: "network".into(), auto: true, prio, metered: false }
    }

    fn line(s: &str) -> Line {
        Line::parse(s).unwrap()
    }

    fn bss(ssid: &str, rssi: i32) -> Line {
        Line::new("bss").with_hex("ssid", ssid.as_bytes()).with("rssi", rssi).with("security", "psk")
    }

    #[test]
    fn saved_roundtrip_and_hostile_lines() {
        let mut s = Saved { profiles: vec![prof("home", 5), prof("cafe", 0)], country: Some("DE".into()) };
        s.profiles[1].wpa3_only = true;
        assert_eq!(Saved::parse(&s.render()), s);
        let junk = "net ssid=zz\nnet ssid=6161 mac=evil\nnet ssid=\ncountry cc=DEU\nnet ssid=6262 prio=99999\n";
        let p = Saved::parse(junk);
        assert_eq!(p.profiles.len(), 1);
        assert_eq!(p.profiles[0].prio, 1000);
        assert_eq!(p.country, None);
    }

    #[test]
    fn auto_join_best_known_then_lease() {
        let mut w = Wifi::new(Saved { profiles: vec![prof("home", 0), prof("work", 5)], country: Some("DE".into()) });
        let a = w.mlme_up(0);
        assert_eq!(a[0], Act::Mlme(line("country cc=DE")));
        assert_eq!(a[1], Act::Mlme(line("scan")));
        w.mlme_line(&bss("home", -40), 1);
        w.mlme_line(&bss("work", -80), 1);
        w.mlme_line(&bss("stranger", -20), 1);
        let a = w.mlme_line(&line("scan-done status=0 count=3"), 2);
        // Priority beats signal.
        assert!(a.contains(&Act::Join { ssid: b"work".to_vec(), security: "psk", mac: "network".into() }));
        let a = w.mlme_line(&line("state state=authenticating mac=02:11:22:33:44:55"), 3);
        assert!(a.contains(&Act::LeaseMac([2, 0x11, 0x22, 0x33, 0x44, 0x55])));
        // It failed: the next candidate.
        let a = w.mlme_line(&line("state state=failed why=timeout"), 4);
        assert!(a.contains(&Act::Join { ssid: b"home".to_vec(), security: "psk", mac: "network".into() }));
        let a = w.mlme_line(&line("state state=authorized bssid=02:00:00:00:00:00 transition_disabled=1"), 5);
        assert!(a.contains(&Act::LeaseUp) && a.contains(&Act::Save));
        assert!(w.saved.get(b"home").unwrap().wpa3_only);
        assert_eq!(w.saved.get(b"home").unwrap().security(Some("psk")), "sae");
        // Lost the link: lease down, rejoin after a short delay.
        let a = w.mlme_line(&line("state state=disconnected why=deauth"), 10);
        assert!(a.contains(&Act::LeaseDown));
        assert!(w.tick(11).is_empty());
        assert_eq!(w.tick(10 + REJOIN_DELAY_MS), vec![Act::Mlme(line("scan"))]);
    }

    #[test]
    fn nothing_known_backs_off_and_leave_holds() {
        let mut w = Wifi::new(Saved { profiles: vec![prof("home", 0)], country: None });
        assert_eq!(w.mlme_up(0), vec![Act::Mlme(line("scan"))]);
        w.mlme_line(&line("scan-done status=0 count=0"), 1);
        assert!(w.tick(1 + SCAN_BACKOFF_MS[0] - 1).is_empty());
        assert_eq!(w.tick(1 + SCAN_BACKOFF_MS[0]).len(), 1);
        w.mlme_line(&line("scan-done status=0 count=0"), 40_000);
        assert_eq!(w.deadline(), Some(40_000 + SCAN_BACKOFF_MS[1]));
        // Leave: no auto-join until the user joins again.
        w.leave();
        assert!(w.tick(10_000_000).is_empty());
        let a = w.join(b"home".to_vec(), None, None);
        assert_eq!(a, vec![Act::Join { ssid: b"home".to_vec(), security: "psk", mac: "network".into() }]);
    }

    #[test]
    fn open_networks_only_when_opted_in_and_floor_kept() {
        let mut open = prof("free", 9);
        open.open = true;
        open.auto = false;
        let mut w = Wifi::new(Saved { profiles: vec![open.clone()], country: None });
        // No auto profile at all: no scan.
        assert!(w.mlme_up(0).is_empty());
        // Re-remembering a network keeps its WPA3-only flag.
        let mut home = prof("home", 0);
        home.wpa3_only = true;
        w.remember(home, 1);
        w.remember(prof("home", 3), 2);
        assert!(w.saved.get(b"home").unwrap().wpa3_only);
        assert_eq!(w.tick(2), vec![Act::Mlme(line("scan"))]);
    }
}

#[cfg(test)]
mod dropout_tests {
    use super::*;

    fn prof(s: &str) -> Profile {
        Profile { ssid: s.as_bytes().to_vec(), open: false, wpa3_only: false, mac: "network".into(), auto: true, prio: 0, metered: false }
    }

    fn line(s: &str) -> Line {
        Line::parse(s).unwrap()
    }

    fn up(w: &mut Wifi, t: u64) {
        w.mlme_line(&Line::new("bss").with_hex("ssid", b"home").with("rssi", -50), t);
        w.mlme_line(&line("scan-done status=0 count=1"), t);
        w.mlme_line(&line("state state=authorized bssid=02:00:00:00:00:01"), t);
        assert_eq!(w.state(), "up");
    }

    #[test]
    fn tunnel_fast_reconnect_then_slow() {
        let mut w = Wifi::new(Saved { profiles: vec![prof("home")], country: None });
        w.mlme_up(0);
        up(&mut w, 1);
        // Beacons gone: lease paused (address kept), look again in 2 s.
        let a = w.mlme_line(&line("state state=disconnected why=beacon-loss"), 1000);
        assert!(a.contains(&Act::LeaseDown) && !a.contains(&Act::Drop));
        assert_eq!(w.deadline(), Some(1000 + REJOIN_DELAY_MS));
        // Nothing in range for a while: 2, 5, 10, 20, 30, 60, 60 s ...
        let mut t = w.deadline().unwrap();
        let mut gaps = Vec::new();
        for _ in 0..8 {
            assert_eq!(w.tick(t), vec![Act::Mlme(line("scan"))]);
            w.mlme_line(&line("scan-done status=0 count=0"), t);
            let d = w.deadline().unwrap();
            gaps.push(d - t);
            t = d;
        }
        assert_eq!(gaps, vec![2_000, 5_000, 10_000, 20_000, 30_000, 60_000, 60_000, 60_000]);
        // Out of the tunnel: found, joined, lease confirmed.
        w.tick(t);
        up(&mut w, t + 1);
        // Ten minutes after a loss with nothing found, the slow schedule.
        w.mlme_line(&line("state state=disconnected why=beacon-loss"), 1_000_000);
        let mut t = w.deadline().unwrap();
        while t < 1_000_000 + RECENT_LOSS_MS {
            w.tick(t);
            w.mlme_line(&line("scan-done status=0 count=0"), t);
            t = w.deadline().unwrap();
        }
        w.tick(t);
        w.mlme_line(&line("scan-done status=0 count=0"), t);
        assert!(w.deadline().unwrap() - t >= SCAN_BACKOFF_MS[0]);
    }

    #[test]
    fn failing_network_is_skipped_then_retried() {
        let mut w = Wifi::new(Saved { profiles: vec![prof("home")], country: None });
        w.mlme_up(0);
        let mut events = 0;
        for i in 0..3u64 {
            let t = i * 1_000_000;
            w.tick(t);
            w.mlme_line(&Line::new("bss").with_hex("ssid", b"home").with("rssi", -50), t);
            let a = w.mlme_line(&line("scan-done status=0 count=1"), t);
            assert!(matches!(a.last(), Some(Act::Join { .. })), "try {i}: {a:?}");
            let a = w.mlme_line(&line("state state=failed why=sae-failed"), t + 1);
            events += a.iter().filter(|x| matches!(x, Act::Event(l) if l.get("what") == Some("network-failing"))).count();
        }
        assert_eq!(events, 1);
        // Now skipped: scans find it but do not join.
        let t = w.deadline().unwrap();
        w.tick(t);
        w.mlme_line(&Line::new("bss").with_hex("ssid", b"home").with("rssi", -50), t);
        let a = w.mlme_line(&line("scan-done status=0 count=1"), t);
        assert!(!a.iter().any(|x| matches!(x, Act::Join { .. })));
        // A new password (remember) clears it.
        w.remember(prof("home"), t + 1);
        let t2 = w.deadline().unwrap();
        w.tick(t2);
        w.mlme_line(&Line::new("bss").with_hex("ssid", b"home").with("rssi", -50), t2);
        assert!(w.mlme_line(&line("scan-done status=0 count=1"), t2).iter().any(|x| matches!(x, Act::Join { .. })));
    }

    #[test]
    fn user_leave_drops_address_now() {
        let mut w = Wifi::new(Saved { profiles: vec![prof("home")], country: None });
        w.mlme_up(0);
        up(&mut w, 1);
        let a = w.leave();
        assert!(a.contains(&Act::Drop));
        let a = w.mlme_line(&line("state state=disconnected why=asked"), 5);
        assert!(a.contains(&Act::LeaseDown));
        assert!(w.tick(100_000).is_empty());
    }
}
