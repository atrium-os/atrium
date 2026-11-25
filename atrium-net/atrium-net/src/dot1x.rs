//! Wired 802.1X policy (wifi.md §7.5): which Ethernet port signs in, when
//! it starts again after a failure, and when its lease starts over. The
//! EAP itself runs in aura-keys; atrium-net only relays the port's EAPOL
//! frames, so this is all it decides. Free of system calls (host tests).

/// A switch's quiet period after a failure (802.1X `quietPeriod`; hostapd
/// uses 60 s): starting again sooner only meets a port that ignores us.
pub const HOLD_MS: u64 = 60_000;
/// Failures in a row before the port waits for a new profile or a cable
/// plugged in again (a wrong password must not lock the account).
pub const MAX_FAILS: u8 = 3;

/// EAPOL's ethertype and the PAE group address switches listen on.
pub const ETHERTYPE: u16 = 0x888e;
pub const PAE_GROUP: [u8; 6] = [0x01, 0x80, 0xc2, 0x00, 0x00, 0x03];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// No 802.1X on this port (or not yet).
    Idle,
    /// The switch asks for a sign-in and none is set up.
    Needed,
    Running,
    Authorized,
    Failed(String),
}

impl State {
    pub fn word(&self) -> &str {
        match self {
            State::Idle => "idle",
            State::Needed => "needed",
            State::Running => "running",
            State::Authorized => "authorized",
            State::Failed(_) => "failed",
        }
    }
}

/// What atrium-net is to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Do {
    /// Open (or reuse) the connection to aura-keys for port k and send
    /// `wired-start`: aura-keys sends EAPOL-Start when it has a profile.
    Start(usize),
    /// EAPOL-Logoff to port k's switch (when its link is up),
    /// `wired-stop`, and close the connection.
    Stop(usize),
    /// DHCP again on port k: the switch moves a port to another VLAN when
    /// it authorizes it, and back (or shuts it) when that ends, so the
    /// old lease is wrong either way.
    RestartLease(usize),
    /// Report the port's state (`dot1x` event).
    Report(usize),
}

#[derive(Debug)]
pub struct Dot1x {
    /// The port signing in (one at a time).
    pub port: Option<usize>,
    pub state: State,
    pub fails: u8,
    /// Frames from the switch are dropped until then (after a failure).
    pub hold_until: Option<u64>,
    /// A wired profile is set up (kept in atrium-net's settings).
    pub configured: bool,
    /// The switch opened the port and the lease since is on its VLAN:
    /// set by `authorized`, ended by a failure, a forget or the link.
    pub open: bool,
}

impl Dot1x {
    pub fn new(configured: bool) -> Dot1x {
        Dot1x { port: None, state: State::Idle, fails: 0, hold_until: None, configured, open: false }
    }

    fn held(&self, now: u64) -> bool {
        self.hold_until.is_some_and(|t| now < t)
    }

    /// Port k's link went up or down.
    pub fn link(&mut self, k: usize, up: bool, now: u64) -> Vec<Do> {
        if !up {
            if self.port != Some(k) {
                return Vec::new();
            }
            *self = Dot1x::new(self.configured);
            return vec![Do::Stop(k), Do::Report(k)];
        }
        // A cable plugged in again is a fresh start.
        if self.port == Some(k) {
            self.fails = 0;
            self.hold_until = None;
        }
        if !self.configured || self.port.is_some_and(|p| p != k) || self.held(now) {
            return Vec::new();
        }
        self.port = Some(k);
        self.state = State::Running;
        vec![Do::Start(k), Do::Report(k)]
    }

    /// An EAPOL frame from the switch on port k: whether to relay it,
    /// and what to do first.
    pub fn frame(&mut self, k: usize, now: u64) -> (bool, Vec<Do>) {
        match self.port {
            Some(p) if p != k => (false, Vec::new()),
            _ if self.held(now) => (false, Vec::new()),
            Some(_) => {
                let mut out = Vec::new();
                if matches!(self.state, State::Failed(_)) {
                    // The hold is over and the switch asks again.
                    self.state = State::Running;
                    out.push(Do::Report(k));
                }
                (true, out)
            }
            None => {
                self.port = Some(k);
                self.state = State::Running;
                (true, vec![Do::Start(k), Do::Report(k)])
            }
        }
    }

    /// A line from aura-keys on the port's connection.
    pub fn keys_line(&mut self, verb: &str, why: Option<&str>, now: u64) -> Vec<Do> {
        let Some(k) = self.port else { return Vec::new() };
        match (verb, why) {
            ("authorized", _) => {
                self.state = State::Authorized;
                self.open = true;
                self.fails = 0;
                self.hold_until = None;
                vec![Do::RestartLease(k), Do::Report(k)]
            }
            ("failed", Some("no-credential")) => {
                self.state = State::Needed;
                vec![Do::Report(k)]
            }
            ("failed", why) => {
                let why = why.unwrap_or("unknown").to_string();
                self.fails = self.fails.saturating_add(1);
                // Waiting for the user ("Trust this server") or past the
                // limit: nothing until the profile changes or the cable does.
                let forever = why == "eap-trust" || self.fails >= MAX_FAILS;
                self.hold_until = Some(if forever { u64::MAX } else { now + HOLD_MS });
                self.state = State::Failed(why);
                if std::mem::take(&mut self.open) {
                    return vec![Do::RestartLease(k), Do::Report(k)];
                }
                vec![Do::Report(k)]
            }
            _ => Vec::new(),
        }
    }

    /// The hold after a failure ended: start again if the port is still up.
    pub fn tick(&mut self, now: u64, link_up: impl Fn(usize) -> bool) -> Vec<Do> {
        match (self.port, &self.state, self.hold_until) {
            (Some(k), State::Failed(_), Some(t)) if now >= t && self.configured && link_up(k) => {
                self.hold_until = None;
                self.state = State::Running;
                vec![Do::Start(k), Do::Report(k)]
            }
            _ => Vec::new(),
        }
    }

    /// The wired profile was set up, changed or trusted (`on`), or
    /// forgotten. `up` is the first wired port with its link up.
    pub fn configure(&mut self, on: bool, up: Option<usize>) -> Vec<Do> {
        self.configured = on;
        self.fails = 0;
        self.hold_until = None;
        if !on {
            let (k, was) = (self.port, self.open);
            *self = Dot1x::new(false);
            return match k {
                Some(k) if was => vec![Do::Stop(k), Do::RestartLease(k), Do::Report(k)],
                Some(k) => vec![Do::Stop(k), Do::Report(k)],
                None => Vec::new(),
            };
        }
        match self.port.or(up) {
            Some(k) => {
                self.port = Some(k);
                self.state = State::Running;
                vec![Do::Start(k), Do::Report(k)]
            }
            None => Vec::new(),
        }
    }

    /// aura-keys went away: the next frame or link change starts over.
    pub fn keys_lost(&mut self) -> Vec<Do> {
        let (k, open) = (self.port, self.open);
        *self = Dot1x::new(self.configured);
        // The switch still has the port open.
        self.open = open;
        k.map(|k| vec![Do::Report(k)]).unwrap_or_default()
    }
}

/// EAPOL-Logoff (802.1X-2010 §11.3: version 2, type 2, no body).
pub const LOGOFF: [u8; 4] = [2, 2, 0, 0];

/// The Ethernet frame carrying `body` (an EAPOL PDU) to the switch.
pub fn frame_to_switch(own: &[u8; 6], body: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(14 + body.len());
    f.extend_from_slice(&PAE_GROUP);
    f.extend_from_slice(own);
    f.extend_from_slice(&ETHERTYPE.to_be_bytes());
    f.extend_from_slice(body);
    f
}

/// The EAPOL PDU of an Ethernet frame, or None for anything else.
pub fn eapol_of(f: &[u8]) -> Option<&[u8]> {
    (f.len() > 14 && f.len() <= 1514 && f[12..14] == ETHERTYPE.to_be_bytes()).then(|| &f[14..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_port_signs_in_and_starts_its_lease_over() {
        let mut d = Dot1x::new(true);
        assert_eq!(d.link(1, true, 0), vec![Do::Start(1), Do::Report(1)]);
        assert_eq!(d.link(2, true, 0), vec![], "one port at a time");
        assert_eq!(d.frame(2, 0), (false, vec![]), "the other port's switch is not relayed");
        assert_eq!(d.frame(1, 0), (true, vec![]));
        assert_eq!(d.keys_line("authorized", None, 5), vec![Do::RestartLease(1), Do::Report(1)]);
        assert_eq!(d.state, State::Authorized);
        assert_eq!(d.link(1, false, 9), vec![Do::Stop(1), Do::Report(1)], "the lease follows the link itself");
        assert_eq!((d.port, &d.state), (None, &State::Idle));
    }

    #[test]
    fn a_switch_that_asks_with_nothing_set_up() {
        let mut d = Dot1x::new(false);
        assert_eq!(d.link(0, true, 0), vec![], "no EAPOL-Start without a profile");
        assert_eq!(d.frame(0, 0), (true, vec![Do::Start(0), Do::Report(0)]));
        assert_eq!(d.keys_line("failed", Some("no-credential"), 1), vec![Do::Report(0)]);
        assert_eq!(d.state, State::Needed);
        assert_eq!(d.configure(true, Some(0)), vec![Do::Start(0), Do::Report(0)]);
        assert_eq!(d.state, State::Running);
    }

    #[test]
    fn failures_wait_out_the_quiet_period_then_stop() {
        let mut d = Dot1x::new(true);
        d.link(0, true, 0);
        d.keys_line("failed", Some("eap-refused"), 1_000);
        assert_eq!(d.frame(0, 2_000), (false, vec![]), "held: the switch's requests are dropped");
        assert_eq!(d.tick(30_000, |_| true), vec![]);
        assert_eq!(d.tick(61_000, |_| false), vec![], "not while the link is down");
        assert_eq!(d.tick(61_000, |_| true), vec![Do::Start(0), Do::Report(0)]);
        d.keys_line("failed", Some("eap-refused"), 62_000);
        d.keys_line("failed", Some("eap-refused"), 130_000);
        assert_eq!(d.fails, MAX_FAILS);
        assert_eq!(d.tick(u64::MAX - 1, |_| true), vec![], "after three, only a new profile or a new cable");
        assert_eq!(d.link(0, true, 200_000), vec![Do::Start(0), Do::Report(0)], "plugged in again");
        d.keys_line("failed", Some("eap-trust"), 300_000);
        assert_eq!(d.tick(u64::MAX - 1, |_| true), vec![], "waits for Trust this server");
        assert_eq!(d.configure(true, None), vec![Do::Start(0), Do::Report(0)]);
        assert_eq!(d.configure(false, None), vec![Do::Stop(0), Do::Report(0)]);
        assert!(!d.configured && d.port.is_none());
    }

    #[test]
    fn the_lease_starts_over_when_authorization_ends() {
        let mut d = Dot1x::new(true);
        d.link(0, true, 0);
        d.keys_line("authorized", None, 1);
        // The switch asks again and the sign-in fails: the port is shut.
        assert_eq!(d.keys_line("failed", Some("eap-rejected"), 2), vec![Do::RestartLease(0), Do::Report(0)]);
        assert_eq!(d.keys_line("failed", Some("eap-rejected"), 3), vec![Do::Report(0)], "only once");
        d.tick(70_000, |_| true);
        d.keys_line("authorized", None, 70_001);
        assert_eq!(d.configure(false, Some(0)), vec![Do::Stop(0), Do::RestartLease(0), Do::Report(0)], "forgotten");
        // A new profile on an open port that then fails: the switch shuts it.
        let mut d = Dot1x::new(true);
        d.link(0, true, 0);
        d.keys_line("authorized", None, 1);
        assert_eq!(d.configure(true, Some(0)), vec![Do::Start(0), Do::Report(0)]);
        assert_eq!(d.keys_line("failed", Some("eap-rejected"), 2), vec![Do::RestartLease(0), Do::Report(0)]);
        let mut d = Dot1x::new(true);
        d.link(0, true, 0);
        assert_eq!(d.configure(false, Some(0)), vec![Do::Stop(0), Do::Report(0)], "never authorized: the lease stands");
    }

    #[test]
    fn frames() {
        let f = frame_to_switch(&[2, 0, 0, 0, 0, 9], &[2, 1, 0, 0]);
        assert_eq!(&f[..6], &PAE_GROUP);
        assert_eq!(eapol_of(&f), Some(&[2, 1, 0, 0][..]));
        let mut ip = f.clone();
        ip[12..14].copy_from_slice(&[8, 0]);
        assert_eq!(eapol_of(&ip), None);
        assert_eq!(eapol_of(&f[..14]), None, "nothing after the header");
        let mut big = frame_to_switch(&[2, 0, 0, 0, 0, 9], &[0; 1501]);
        assert_eq!(eapol_of(&big), None, "longer than an Ethernet frame");
        big.pop();
        assert!(eapol_of(&big).is_some());
    }
}
