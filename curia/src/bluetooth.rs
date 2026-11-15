//! The Bluetooth page's model (curia.md §14): the controller, devices found
//! by a scan, pairing (Settings is vinculumd's pairing agent, so its
//! prompts are drawn here and nowhere else, bluetooth.md §7, §13), and the
//! paired devices (connect, disconnect, forget). Over vinculumd's
//! `bluetooth-control` socket. Pure: `main.rs` keeps the connection, hands
//! what arrives to `apply` and sends `wants`.

use std::collections::VecDeque;

use vinculum_control::{Addr, DeviceInfo, Event, Prompt, Request};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub ready: bool,
    pub address: Option<Addr>,
    pub scanning: bool,
    pub pairable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub address: Addr,
    pub name: Option<String>,
    pub rssi: i8,
}

/// What a request's `Ok` or `Error` answers (vinculumd answers each request
/// once, in order).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Asked {
    Pair(Addr),
    Other,
}

/// Found devices kept at most (a busy room advertises a lot).
pub const MAX_FOUND: usize = 40;

#[derive(Clone, Debug, Default)]
pub struct BtPage {
    /// None: not connected yet; Some(Err): why vinculumd can't be reached.
    pub link: Option<Result<(), String>>,
    pub status: Option<Status>,
    pub found: Vec<Found>,
    pub devices: Option<Vec<DeviceInfo>>,
    /// Being paired now.
    pub pairing: Option<Addr>,
    /// What the device being paired asks the person.
    pub prompt: Option<(Addr, Prompt)>,
    /// Forget asks once more.
    pub confirm_forget: Option<Addr>,
    /// The last thing that went wrong, or a pairing that just finished.
    pub note: Option<String>,
    asked: VecDeque<Asked>,
    pub wants: Vec<Request>,
}

/// "AA:BB:…" shortened for a nameless device.
pub fn label(name: Option<&str>, a: &Addr) -> String {
    match name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) => n.to_string(),
        None => format!("Unnamed device ({})", a.to_string().trim_end_matches("/r")),
    }
}

/// "Strong", "Good", "Weak" from an RSSI in dBm.
pub fn signal(rssi: i8) -> &'static str {
    match rssi {
        r if r >= -60 => "Strong signal",
        r if r >= -75 => "Good signal",
        _ => "Weak signal",
    }
}

/// One line on a paired device.
pub fn device_line(d: &DeviceInfo) -> String {
    let mut s = if d.connected { "Connected".to_string() } else { "Not connected".to_string() };
    if !d.authenticated {
        s.push_str(" · paired without a code check");
    }
    if !d.keyboard_allowed {
        s.push_str(" · typing not allowed");
    }
    s
}

/// A typed passkey: six digits at most, 0 to 999999.
pub fn parse_passkey(s: &str) -> Option<u32> {
    let t = s.trim();
    (!t.is_empty() && t.len() <= 6 && t.bytes().all(|b| b.is_ascii_digit())).then(|| t.parse().ok()).flatten()
}

impl BtPage {
    pub fn take_wants(&mut self) -> Vec<Request> {
        std::mem::take(&mut self.wants)
    }

    pub fn ready(&self) -> bool {
        matches!(self.link, Some(Ok(()))) && self.status.as_ref().is_some_and(|s| s.ready)
    }

    fn ask(&mut self, r: Request) {
        if matches!(self.link, Some(Ok(()))) {
            self.asked.push_back(match r {
                Request::Pair { address } => Asked::Pair(address),
                _ => Asked::Other,
            });
            self.wants.push(r);
        }
    }

    /// Connected and said hello (as the pairing agent): ask for everything.
    pub fn connected(&mut self) {
        *self = BtPage { link: Some(Ok(())), ..Default::default() };
        // Hello's own answer.
        self.asked.push_back(Asked::Other);
        self.refresh();
    }

    pub fn lost(&mut self, why: String) {
        *self = BtPage { link: Some(Err(why)), ..Default::default() };
    }

    /// The connection asked for the status itself: its answer is counted.
    pub fn probed(&mut self) {
        self.asked.push_back(Asked::Other);
    }

    /// The controller and the paired devices, again.
    pub fn refresh(&mut self) {
        self.ask(Request::Status);
        self.ask(Request::Devices);
    }

    fn bonded(&self, a: &Addr) -> bool {
        self.devices.as_ref().is_some_and(|v| v.iter().any(|d| d.address == *a || d.identity == Some(*a)))
    }

    pub fn apply(&mut self, e: Event) {
        match e {
            Event::Ok => {
                self.asked.pop_front();
            }
            Event::Error { why } => {
                if let Some(Asked::Pair(a)) = self.asked.pop_front() {
                    if self.pairing == Some(a) {
                        self.pairing = None;
                    }
                }
                self.note = Some(why);
            }
            Event::Status { ready, address, scanning, pairable, .. } => {
                self.asked.pop_front();
                self.status = Some(Status { ready, address, scanning, pairable });
                if !scanning {
                    self.found.retain(|f| Some(f.address) == self.pairing);
                }
            }
            Event::Devices { devices } => {
                self.asked.pop_front();
                self.devices = Some(devices);
                let found = std::mem::take(&mut self.found);
                self.found = found.into_iter().filter(|f| !self.bonded(&f.address)).collect();
                if self.confirm_forget.is_some_and(|a| !self.bonded(&a)) {
                    self.confirm_forget = None;
                }
            }
            Event::Found { address, name, rssi } => {
                if self.bonded(&address) {
                    return;
                }
                let n = self.found.len();
                match self.found.iter_mut().find(|f| f.address == address) {
                    Some(f) => {
                        f.rssi = rssi;
                        if name.is_some() {
                            f.name = name;
                        }
                    }
                    None if n < MAX_FOUND => self.found.push(Found { address, name, rssi }),
                    None => {}
                }
            }
            Event::Prompt { address, prompt } => {
                self.prompt = Some((address, prompt));
            }
            Event::Paired { address, .. } => {
                let name = self.found.iter().find(|f| f.address == address).map(|f| label(f.name.as_deref(), &address));
                self.note = Some(format!("Paired with {}", name.unwrap_or_else(|| address.to_string())));
                self.done_pairing(address);
                self.refresh();
            }
            Event::PairingFailed { address, why } => {
                self.note = Some(format!("Pairing didn't finish: {why}"));
                self.done_pairing(address);
            }
            Event::Connected { .. } | Event::Disconnected { .. } => self.ask(Request::Devices),
        }
    }

    fn done_pairing(&mut self, a: Addr) {
        if self.pairing == Some(a) {
            self.pairing = None;
        }
        if self.prompt.is_some_and(|(p, _)| p == a) {
            self.prompt = None;
        }
    }

    pub fn toggle_scan(&mut self) {
        let Some(on) = self.status.as_ref().filter(|s| s.ready).map(|s| !s.scanning) else { return };
        if let Some(s) = &mut self.status {
            s.scanning = on;
        }
        if !on {
            self.found.retain(|f| Some(f.address) == self.pairing);
        }
        self.ask(Request::Scan { on });
        self.ask(Request::Status);
    }

    pub fn pair(&mut self, a: Addr) {
        if !self.ready() || self.pairing.is_some() || self.bonded(&a) {
            return;
        }
        self.pairing = Some(a);
        self.note = None;
        self.ask(Request::Pair { address: a });
    }

    /// The person's answer to a Confirm prompt.
    pub fn confirm(&mut self, yes: bool) {
        let Some((a, Prompt::Confirm { .. })) = self.prompt else { return };
        self.prompt = None;
        self.ask(Request::Confirm { address: a, yes });
    }

    /// A typed passkey for an EnterPasskey prompt; `None` cancels. A
    /// passkey that isn't six digits at most is not sent.
    pub fn passkey(&mut self, typed: Option<&str>) {
        let Some((a, Prompt::EnterPasskey)) = self.prompt else { return };
        let pk = match typed {
            Some(t) => match parse_passkey(t) {
                Some(p) => Some(p),
                None => {
                    self.note = Some("The code is the six digits the device shows".into());
                    return;
                }
            },
            None => None,
        };
        self.prompt = None;
        self.ask(Request::Passkey { address: a, passkey: pk });
    }

    /// Done with a Display prompt (the person typed it on the device).
    pub fn dismiss_display(&mut self) {
        if matches!(self.prompt, Some((_, Prompt::Display { .. }))) {
            self.prompt = None;
        }
    }

    pub fn connect(&mut self, a: Addr) {
        if self.ready() && self.bonded(&a) {
            self.ask(Request::Connect { address: a });
        }
    }

    pub fn disconnect(&mut self, a: Addr) {
        if self.devices.as_ref().is_some_and(|v| v.iter().any(|d| d.address == a && d.connected)) {
            self.ask(Request::Disconnect { address: a });
        }
    }

    /// Forget: the first press asks, the second deletes the bond.
    pub fn forget(&mut self, a: Addr) {
        if !self.bonded(&a) {
            return;
        }
        if self.confirm_forget != Some(a) {
            self.confirm_forget = Some(a);
            return;
        }
        self.confirm_forget = None;
        self.ask(Request::Remove { address: a });
        self.ask(Request::Devices);
    }

    pub fn cancel_forget(&mut self) {
        self.confirm_forget = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(n: u8) -> Addr {
        Addr { bytes: [0xC0, 0, 0, 0, 0, n], random: true }
    }

    fn dev(n: u8, connected: bool) -> DeviceInfo {
        DeviceInfo { address: a(n), identity: None, name: format!("Dev{n}"), authenticated: true, secure_connections: true, keyboard_allowed: true, connected }
    }

    fn page() -> BtPage {
        let mut p = BtPage::default();
        p.connected();
        assert_eq!(p.take_wants(), vec![Request::Status, Request::Devices]);
        p.apply(Event::Ok); // Hello
        p.apply(Event::Status { ready: true, address: Some(a(0)), scanning: false, pairable: false, agents: 1, connections: 0 });
        p.apply(Event::Devices { devices: vec![dev(1, false)] });
        p
    }

    #[test]
    fn scanning_lists_new_devices_only() {
        let mut p = page();
        p.toggle_scan();
        assert_eq!(p.take_wants(), vec![Request::Scan { on: true }, Request::Status]);
        p.apply(Event::Ok);
        p.apply(Event::Status { ready: true, address: None, scanning: true, pairable: false, agents: 1, connections: 0 });
        p.apply(Event::Found { address: a(1), name: Some("Dev1".into()), rssi: -40 });
        p.apply(Event::Found { address: a(2), name: None, rssi: -80 });
        p.apply(Event::Found { address: a(2), name: Some("Mouse".into()), rssi: -50 });
        assert_eq!(p.found.len(), 1, "a paired device is not offered again");
        assert_eq!((label(p.found[0].name.as_deref(), &p.found[0].address), signal(p.found[0].rssi)), ("Mouse".into(), "Strong signal"));
        assert_eq!(label(None, &a(3)), "Unnamed device (C0:00:00:00:00:03)");
        p.toggle_scan();
        assert!(p.found.is_empty());
    }

    #[test]
    fn pairing_with_a_number_to_compare() {
        let mut p = page();
        p.apply(Event::Found { address: a(2), name: Some("Mouse".into()), rssi: -50 });
        p.pair(a(2));
        p.pair(a(3));
        assert_eq!(p.take_wants(), vec![Request::Pair { address: a(2) }], "one at a time");
        p.apply(Event::Ok);
        p.apply(Event::Prompt { address: a(2), prompt: Prompt::Confirm { number: Some(123456) } });
        p.confirm(true);
        assert_eq!(p.take_wants(), vec![Request::Confirm { address: a(2), yes: true }]);
        p.apply(Event::Ok);
        p.apply(Event::Paired { address: a(2), authenticated: true, keyboard_allowed: true });
        assert_eq!(p.note.as_deref(), Some("Paired with Mouse"));
        assert_eq!(p.pairing, None);
        assert_eq!(p.take_wants(), vec![Request::Status, Request::Devices]);
    }

    #[test]
    fn a_refused_pair_and_a_typed_passkey() {
        let mut p = page();
        p.pair(a(2));
        p.take_wants();
        p.apply(Event::Error { why: "no pairing agent: open the pairing UI".into() });
        assert_eq!(p.pairing, None);
        assert!(p.note.as_ref().unwrap().contains("no pairing agent"));
        p.pair(a(2));
        p.take_wants();
        p.apply(Event::Ok);
        p.apply(Event::Prompt { address: a(2), prompt: Prompt::EnterPasskey });
        p.passkey(Some("12a"));
        assert!(p.take_wants().is_empty() && p.prompt.is_some());
        p.passkey(Some(" 004211 "));
        assert_eq!(p.take_wants(), vec![Request::Passkey { address: a(2), passkey: Some(4211) }]);
        p.apply(Event::Ok);
        p.apply(Event::PairingFailed { address: a(2), why: "the codes didn't match".into() });
        assert_eq!(p.note.as_deref(), Some("Pairing didn't finish: the codes didn't match"));
        assert_eq!(parse_passkey("1234567"), None);
    }

    #[test]
    fn paired_devices_connect_disconnect_and_forget_after_asking() {
        let mut p = page();
        p.connect(a(1));
        p.disconnect(a(1));
        p.connect(a(9));
        assert_eq!(p.take_wants(), vec![Request::Connect { address: a(1) }], "only a paired device; disconnect only a connected one");
        p.apply(Event::Ok);
        p.apply(Event::Connected { address: a(1), encrypted: true });
        assert_eq!(p.take_wants(), vec![Request::Devices]);
        p.apply(Event::Devices { devices: vec![dev(1, true)] });
        assert_eq!(device_line(&p.devices.as_ref().unwrap()[0]), "Connected");
        p.forget(a(1));
        assert!(p.take_wants().is_empty());
        p.forget(a(1));
        assert_eq!(p.take_wants(), vec![Request::Remove { address: a(1) }, Request::Devices]);
        p.apply(Event::Ok);
        p.apply(Event::Devices { devices: vec![] });
        assert_eq!(p.confirm_forget, None);
        p.lost("vinculumd is not running".into());
        p.refresh();
        assert!(p.take_wants().is_empty(), "nothing is sent with no connection");
    }
}
