//! The Network page's model (curia.md §3): what atrium-netconf reported,
//! what the user is doing (a passphrase row open, a forget to confirm), and
//! the requests the page wants made. `main.rs` makes them on a thread and
//! feeds the answers back with [`NetPage::apply`].

use atrium_netconf::{Iface, Link, Nearby, Online, Profile, Status};

/// A request to atrium-netconf.
#[derive(Clone, PartialEq, Eq)]
pub enum Want {
    Scan,
    Profiles,
    Details,
    /// Join a remembered or open network.
    Join { ssid: Vec<u8>, open: bool },
    /// Remember with this passphrase, then join (interim, curia.md §4).
    JoinWith { ssid: Vec<u8>, passphrase: String },
    Leave,
    Forget(Vec<u8>),
    Save(Profile),
}

impl std::fmt::Debug for Want {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Want::JoinWith { ssid, .. } => write!(f, "JoinWith({:?}, ..)", String::from_utf8_lossy(ssid)),
            Want::Join { ssid, open } => write!(f, "Join({:?}, open={open})", String::from_utf8_lossy(ssid)),
            Want::Forget(s) => write!(f, "Forget({:?})", String::from_utf8_lossy(s)),
            Want::Save(p) => write!(f, "Save({:?})", String::from_utf8_lossy(&p.ssid)),
            Want::Scan => write!(f, "Scan"),
            Want::Profiles => write!(f, "Profiles"),
            Want::Details => write!(f, "Details"),
            Want::Leave => write!(f, "Leave"),
        }
    }
}

/// An answer from atrium-netconf.
pub enum Got {
    /// None: not reachable.
    Status(Option<Status>),
    Nearby(Result<Vec<Nearby>, String>),
    Profiles(Result<Vec<Profile>, String>),
    Details(Result<Vec<Iface>, String>),
    /// A join, leave, forget or save finished.
    Done { what: Want, result: Result<(), String> },
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Notice {
    pub text: String,
    pub error: bool,
}

#[derive(Clone, Debug, Default)]
pub struct NetPage {
    /// None until atrium-netconf answers, and while it is away.
    pub status: Option<Status>,
    pub reachable: bool,
    pub nearby: Vec<Nearby>,
    pub scanned: bool,
    pub profiles: Vec<Profile>,
    pub ifaces: Vec<Iface>,
    pub show_details: bool,
    /// The network whose passphrase row is open.
    pub asking: Option<Vec<u8>>,
    /// The network being joined.
    pub joining: Option<Vec<u8>>,
    /// The network whose Forget waits for a second press.
    pub confirm_forget: Option<Vec<u8>>,
    pub notice: Option<Notice>,
    pub wants: Vec<Want>,
}

pub fn name(s: &[u8]) -> String {
    String::from_utf8_lossy(s).into_owned()
}

/// "Wi-Fi: home", "Ethernet", "Not connected", and what is wrong.
pub fn headline(s: Option<&Status>, reachable: bool) -> (String, Option<String>) {
    let Some(s) = s.filter(|_| reachable) else {
        return ("Network settings are not available".into(), Some("atrium-net is not running".into()));
    };
    let head = match s.link {
        Link::None => "Not connected".to_string(),
        Link::Wired => "Ethernet".to_string(),
        Link::Wifi => format!("Wi-Fi: {}", s.ssid.as_deref().map(name).unwrap_or_default()),
    };
    let mut sub = Vec::new();
    match s.online {
        Online::Yes => sub.push("Online".to_string()),
        Online::No if s.link != Link::None => sub.push("No internet".to_string()),
        Online::Captive => sub.push(match &s.portal {
            Some(p) => format!("Sign-in needed: {p}"),
            None => "Sign-in needed".to_string(),
        }),
        _ => {}
    }
    if s.metered {
        sub.push("Metered".into());
    }
    (head, (!sub.is_empty()).then(|| sub.join(" · ")))
}

pub fn security_label(s: &str) -> &'static str {
    match s {
        "open" => "Open",
        "wpa2" => "WPA2",
        "wpa3" => "WPA3",
        "wpa2/wpa3" => "WPA2/WPA3",
        _ => "Not supported",
    }
}

pub const MAC_POLICIES: [(&str, &str); 3] = [("network", "One address per network"), ("random", "New address each time"), ("device", "The device's own address")];

impl NetPage {
    pub fn new() -> Self {
        NetPage { wants: vec![Want::Scan, Want::Profiles, Want::Details], ..Default::default() }
    }

    pub fn take_wants(&mut self) -> Vec<Want> {
        std::mem::take(&mut self.wants)
    }

    fn connected_to(&self) -> Option<&[u8]> {
        self.status.as_ref().filter(|s| s.link == Link::Wifi).and_then(|s| s.ssid.as_deref())
    }

    /// The networks around, ours first.
    pub fn listed(&self) -> Vec<&Nearby> {
        let cur = self.connected_to();
        let mut v: Vec<&Nearby> = self.nearby.iter().collect();
        v.sort_by_key(|n| (Some(n.ssid.as_slice()) != cur, !n.known));
        v
    }

    pub fn is_connected(&self, ssid: &[u8]) -> bool {
        self.connected_to() == Some(ssid)
    }

    /// A press on a nearby network.
    pub fn pick(&mut self, ssid: &[u8]) {
        if self.joining.is_some() || self.is_connected(ssid) {
            return;
        }
        let Some(n) = self.nearby.iter().find(|n| n.ssid == ssid) else { return };
        self.notice = None;
        if n.security == "unsupported" {
            self.notice = Some(Notice { text: format!("{} uses security Atrium does not support", name(ssid)), error: true });
            return;
        }
        let open = n.security == "open";
        if n.known || open {
            self.asking = None;
            self.joining = Some(ssid.to_vec());
            self.wants.push(Want::Join { ssid: ssid.to_vec(), open });
        } else {
            self.asking = if self.asking.as_deref() == Some(ssid) { None } else { Some(ssid.to_vec()) };
        }
    }

    /// Join pressed (or Enter) on the passphrase row.
    pub fn join_typed(&mut self, passphrase: &str) {
        let Some(ssid) = self.asking.clone() else { return };
        let n = passphrase.chars().count();
        if !(8..=63).contains(&n) || !passphrase.bytes().all(|c| (0x20..0x7f).contains(&c)) {
            self.notice = Some(Notice { text: "A Wi-Fi passphrase is 8 to 63 letters, digits or symbols".into(), error: true });
            return;
        }
        self.notice = None;
        self.joining = Some(ssid.clone());
        self.wants.push(Want::JoinWith { ssid, passphrase: passphrase.to_string() });
    }

    pub fn cancel_ask(&mut self) {
        self.asking = None;
        self.notice = None;
    }

    pub fn leave(&mut self) {
        self.wants.push(Want::Leave);
    }

    /// Forget asks once, then forgets on the second press.
    pub fn forget(&mut self, ssid: &[u8]) {
        if self.confirm_forget.as_deref() == Some(ssid) {
            self.confirm_forget = None;
            self.wants.push(Want::Forget(ssid.to_vec()));
        } else {
            self.confirm_forget = Some(ssid.to_vec());
        }
    }

    pub fn cancel_forget(&mut self) {
        self.confirm_forget = None;
    }

    fn change(&mut self, ssid: &[u8], f: impl FnOnce(&mut Profile)) {
        if let Some(p) = self.profiles.iter_mut().find(|p| p.ssid == ssid) {
            f(p);
            let p = p.clone();
            self.wants.push(Want::Save(p));
        }
    }

    pub fn set_auto(&mut self, ssid: &[u8], on: bool) {
        self.change(ssid, |p| p.auto = on);
    }

    pub fn set_metered(&mut self, ssid: &[u8], on: bool) {
        self.change(ssid, |p| p.metered = on);
    }

    pub fn bump_prio(&mut self, ssid: &[u8], by: i32) {
        self.change(ssid, |p| p.prio = (p.prio + by).clamp(-1000, 1000));
    }

    pub fn set_mac(&mut self, ssid: &[u8], i: usize) {
        if let Some((m, _)) = MAC_POLICIES.get(i) {
            self.change(ssid, |p| p.mac = m.to_string());
        }
    }

    pub fn toggle_details(&mut self) {
        self.show_details = !self.show_details;
        if self.show_details {
            self.wants.push(Want::Details);
        }
    }

    pub fn apply(&mut self, g: Got) {
        match g {
            Got::Status(s) => {
                self.reachable = s.is_some();
                let was = self.status.as_ref().map(|s| (s.link, s.ssid.clone()));
                self.status = s;
                // The network changed: what is saved and the addresses did too.
                if self.status.as_ref().map(|s| (s.link, s.ssid.clone())) != was && self.reachable {
                    self.wants.push(Want::Profiles);
                    if self.show_details {
                        self.wants.push(Want::Details);
                    }
                }
            }
            Got::Nearby(Ok(v)) => {
                self.nearby = v;
                self.scanned = true;
            }
            Got::Nearby(Err(_)) => self.scanned = true,
            Got::Profiles(Ok(v)) => self.profiles = v,
            Got::Profiles(Err(_)) => {}
            Got::Details(Ok(v)) => self.ifaces = v,
            Got::Details(Err(_)) => {}
            Got::Done { what, result } => {
                match (&what, &result) {
                    (Want::Join { ssid, .. } | Want::JoinWith { ssid, .. }, Ok(())) => {
                        self.joining = None;
                        self.asking = None;
                        self.notice = Some(Notice { text: format!("Connected to {}", name(ssid)), error: false });
                    }
                    (Want::Join { ssid, .. } | Want::JoinWith { ssid, .. }, Err(e)) => {
                        self.joining = None;
                        let why = match e.as_str() {
                            "timed out" => "no answer in time: a wrong passphrase, or out of range".to_string(),
                            w => w.replace('-', " "),
                        };
                        self.notice = Some(Notice { text: format!("Could not join {}: {why}", name(ssid)), error: true });
                    }
                    (Want::Forget(ssid), Ok(())) => self.notice = Some(Notice { text: format!("Forgot {}", name(ssid)), error: false }),
                    (_, Err(e)) => self.notice = Some(Notice { text: format!("Did not work: {e}"), error: true }),
                    _ => {}
                }
                self.wants.push(Want::Profiles);
                self.wants.push(Want::Scan);
            }
        }
        // A known network is one in the saved list.
        for n in &mut self.nearby {
            n.known = self.profiles.iter().any(|p| p.ssid == n.ssid);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn near(s: &str, sec: &str, known: bool) -> Nearby {
        Nearby { ssid: s.as_bytes().to_vec(), security: sec.into(), bars: 3, known }
    }

    fn prof(s: &str) -> Profile {
        Profile { ssid: s.as_bytes().to_vec(), open: false, wpa3_only: false, mac: "network".into(), auto: true, prio: 0, metered: false }
    }

    fn page() -> NetPage {
        let mut p = NetPage::new();
        p.take_wants();
        p.apply(Got::Status(Some(Status { link: Link::Wifi, online: Online::Unknown, metered: false, bars: Some(3), ssid: Some(b"home".to_vec()), portal: None })));
        p.apply(Got::Profiles(Ok(vec![prof("home"), prof("office")])));
        p.apply(Got::Nearby(Ok(vec![near("cafe", "open", false), near("neighbour", "wpa2", false), near("office", "wpa3", false), near("home", "wpa2", false)])));
        p.take_wants();
        p
    }

    #[test]
    fn listed_puts_ours_then_saved_first_and_marks_known() {
        let p = page();
        let v: Vec<String> = p.listed().iter().map(|n| name(&n.ssid)).collect();
        assert_eq!(v, ["home", "office", "cafe", "neighbour"]);
        assert!(p.listed()[1].known, "known from the saved list");
    }

    #[test]
    fn pick_joins_known_and_open_asks_for_the_rest() {
        let mut p = page();
        p.pick(b"office");
        assert_eq!(p.take_wants(), vec![Want::Join { ssid: b"office".to_vec(), open: false }]);
        p.apply(Got::Done { what: Want::Join { ssid: b"office".to_vec(), open: false }, result: Err("timed out".into()) });
        assert!(p.notice.as_ref().unwrap().error && p.joining.is_none());
        p.take_wants();
        p.pick(b"neighbour");
        assert!(p.take_wants().is_empty());
        assert_eq!(p.asking.as_deref(), Some(&b"neighbour"[..]));
        p.join_typed("short");
        assert!(p.take_wants().is_empty(), "a short passphrase never leaves the page");
        p.join_typed("correct horse battery staple");
        assert_eq!(p.take_wants(), vec![Want::JoinWith { ssid: b"neighbour".to_vec(), passphrase: "correct horse battery staple".into() }]);
        p.apply(Got::Done { what: Want::JoinWith { ssid: b"neighbour".to_vec(), passphrase: String::new() }, result: Ok(()) });
        assert_eq!((p.asking.clone(), p.notice.clone().unwrap().text), (None, "Connected to neighbour".to_string()));
        p.pick(b"home");
        p.take_wants();
        p.pick(b"home");
        assert!(p.take_wants().iter().all(|w| !matches!(w, Want::Join { .. })), "already on it");
    }

    #[test]
    fn forget_asks_first_and_settings_save_at_once() {
        let mut p = page();
        p.forget(b"office");
        assert!(p.take_wants().is_empty());
        p.forget(b"office");
        assert_eq!(p.take_wants(), vec![Want::Forget(b"office".to_vec())]);
        p.set_auto(b"home", false);
        p.bump_prio(b"home", 5);
        p.set_mac(b"home", 1);
        let w = p.take_wants();
        let Want::Save(last) = w.last().unwrap() else { panic!() };
        assert_eq!((last.auto, last.prio, last.mac.as_str()), (false, 5, "random"));
        assert_eq!(w.len(), 3);
    }

    #[test]
    fn debug_never_shows_a_passphrase() {
        let w = Want::JoinWith { ssid: b"x".to_vec(), passphrase: "hunter2hunter2".into() };
        assert!(!format!("{w:?}").contains("hunter2"));
    }

    #[test]
    fn headline() {
        assert_eq!(super::headline(None, false).0, "Network settings are not available");
        let s = Status { link: Link::Wired, online: Online::Captive, metered: true, bars: None, ssid: None, portal: Some("http://p/".into()) };
        assert_eq!(super::headline(Some(&s), true), ("Ethernet".into(), Some("Sign-in needed: http://p/ · Metered".into())));
    }
}
