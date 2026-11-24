//! The Network page's model (curia.md §3): what atrium-netconf reported,
//! what the user is doing (a passphrase row open, a forget to confirm), and
//! the requests the page wants made. `main.rs` makes them on a thread and
//! feeds the answers back with [`NetPage::apply`].

use atrium_netconf::{check_dns, der_to_pem, pem_to_der, tls_name_ok, CertRequest, DnsTls, Enterprise, KeyRequest, Iface, Link, Manual, Nearby, Online, Profile, ServerSeen, SetStatic, Status, Target};
use std::net::{IpAddr, Ipv4Addr};

/// A request to atrium-netconf.
#[derive(Clone, PartialEq, Eq)]
pub enum Want {
    Scan,
    Profiles,
    Details,
    /// Join a remembered or open network.
    Join { ssid: Vec<u8>, open: bool },
    /// Remember with what was typed into Arca's field, then join: the
    /// handle Arca gave us (curia.md §4). Curia never has the passphrase.
    JoinWith { ssid: Vec<u8>, handle: [u8; 16] },
    /// Remember a work or campus (802.1X) network and join it; the
    /// password is Arca's handle, as for a passphrase.
    JoinEnterprise(Enterprise),
    /// The certificate a first 802.1X join stopped at.
    Seen(Vec<u8>),
    /// "Trust this server", then join.
    Trust(Vec<u8>),
    /// A certificate login's key, made in Arca, and its request.
    CertRequest(CertRequest),
    Leave,
    Forget(Vec<u8>),
    Save(Profile),
    /// The default route and the machine's own DNS servers.
    Routes,
    /// A fixed address for a saved network or a wired port, or None: DHCP.
    SetStatic(SetStatic),
    /// The machine's own DNS servers; empty: each network's.
    SetDns(Vec<IpAddr>),
    /// The machine's own DNS servers, asked only over TLS (§6).
    SetDnsTls(DnsTls),
    /// Open the network's sign-in page (atrium-net.md §8.1): portcullisd's
    /// `PortalWindow`, with the press so an open window comes forward.
    SignIn { activation: Option<[u8; 16]> },
    /// The machine's wired 802.1X sign-in is no longer wanted.
    WiredForget,
}

impl std::fmt::Debug for Want {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Want::JoinWith { ssid, .. } => write!(f, "JoinWith({:?}, ..)", String::from_utf8_lossy(ssid)),
            Want::Join { ssid, open } => write!(f, "Join({:?}, open={open})", String::from_utf8_lossy(ssid)),
            Want::JoinEnterprise(e) => write!(f, "JoinEnterprise({:?}, {})", String::from_utf8_lossy(&e.ssid), e.method),
            Want::Seen(s) => write!(f, "Seen({:?})", String::from_utf8_lossy(s)),
            Want::Trust(s) => write!(f, "Trust({:?})", String::from_utf8_lossy(s)),
            Want::CertRequest(r) => write!(f, "CertRequest({:?})", String::from_utf8_lossy(&r.ssid)),
            Want::Forget(s) => write!(f, "Forget({:?})", String::from_utf8_lossy(s)),
            Want::Save(p) => write!(f, "Save({:?})", String::from_utf8_lossy(&p.ssid)),
            Want::Scan => write!(f, "Scan"),
            Want::Profiles => write!(f, "Profiles"),
            Want::Details => write!(f, "Details"),
            Want::Leave => write!(f, "Leave"),
            Want::Routes => write!(f, "Routes"),
            Want::SetStatic(s) => write!(f, "SetStatic({:?})", s.target),
            Want::SetDns(v) => write!(f, "SetDns({v:?})"),
            Want::SetDnsTls(t) => write!(f, "SetDnsTls({:?}, {})", t.servers, t.name),
            Want::SignIn { .. } => write!(f, "SignIn"),
            Want::WiredForget => write!(f, "WiredForget"),
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
    /// The machine's own DNS servers (from the routes).
    /// The own DNS servers and, if they are asked over TLS, their name.
    OwnDns(Result<(Vec<String>, Option<String>), String>),
    /// A join, leave, forget or save finished.
    Done { what: Want, result: Result<(), String> },
    Seen(Vec<u8>, Result<ServerSeen, String>),
    KeyRequest(Vec<u8>, Result<KeyRequest, String>),
    /// A file picked for the sheet (name, bytes); Err("") when cancelled.
    Picked(Result<(String, Vec<u8>), String>),
}

/// What the system picker is open for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetPick {
    /// The network's CA certificate.
    Ca,
    /// Our certificate, signed by the network's IT team.
    Cert,
}

/// How the work network's server is checked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Check {
    /// Its certificate is shown on the first join, for "Trust this server".
    #[default]
    FirstJoin,
    /// The system's root certificates, with its domain.
    System,
    /// A CA certificate from a file, with its domain.
    CaFile,
}

/// The sign-in methods offered for a work or campus network, as named to
/// the network service and to the user.
pub const EAP_METHODS: [(&str, &str); 4] = [("peap", "PEAP"), ("ttls-mschapv2", "TTLS (MSCHAPv2)"), ("ttls-pap", "TTLS (PAP)"), ("tls", "Certificate (EAP-TLS)")];

/// What the work-network sheet holds besides the typed fields.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EntForm {
    /// Index into `EAP_METHODS`.
    pub method: usize,
    pub check: Check,
    /// The CA certificate picked (file name, DER).
    pub ca: Option<(String, Vec<u8>)>,
    /// Username, server domain and anonymous identity, from the sheet's
    /// fields at the press.
    pub typed: Option<(String, String, String)>,
    /// A certificate login: the key Arca made, and its request (PEM) to
    /// give the network's IT team.
    pub request: Option<([u8; 16], String)>,
    /// The certificate they signed (file name, DER).
    pub cert: Option<(String, Vec<u8>)>,
}

impl EntForm {
    pub fn tls(&self) -> bool {
        EAP_METHODS[self.method].0 == "tls"
    }
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
    /// When the interfaces were last asked for (kept fresh while a wired
    /// port signs in).
    pub details_at: Option<std::time::Instant>,
    /// The wired sign-in that stopped at the server was already shown.
    pub wired_seen_asked: bool,
    pub show_details: bool,
    /// The network whose passphrase row is open.
    pub asking: Option<Vec<u8>>,
    /// The network being joined.
    pub joining: Option<Vec<u8>>,
    /// Arca's handle for what was typed in its field on the passphrase row.
    pub handle: Option<[u8; 16]>,
    /// Join was pressed before the handle came (the press took the field's
    /// focus, and Arca holds what was typed then): join when it comes.
    pub armed: Option<std::time::Instant>,
    /// The handle was spent on a join that failed: Arca's field starts
    /// afresh (the window asks for a new one).
    pub reset_field: bool,
    /// Arca's field is on the passphrase row (limend and arca-ui answer).
    pub field_up: bool,
    /// The passphrase row is a work-network sheet (802.1X).
    pub ent: Option<EntForm>,
    /// The system picker is wanted (or open) for the sheet.
    pub pick: Option<NetPick>,
    /// A first join stopped at this server's certificate: "Trust this
    /// server" or not.
    pub trusting: Option<(Vec<u8>, ServerSeen)>,
    /// The network whose Forget waits for a second press.
    pub confirm_forget: Option<Vec<u8>>,
    pub notice: Option<Notice>,
    pub wants: Vec<Want>,
    /// The machine's own DNS servers (empty: each network's).
    pub own_dns: Vec<String>,
    /// Set: `own_dns` are asked only over TLS, as this name.
    pub dns_tls: Option<String>,
    /// The IP settings row that is open: a saved network or a wired port.
    pub ip_editing: Option<Target>,
    /// The DNS servers row is open.
    pub dns_editing: bool,
    /// What is wrong with what was typed on the open row.
    pub edit_error: Option<String>,
}

/// What the IP settings row shows for a setting: ("10.0.0.5/24",
/// "10.0.0.1", "1.1.1.1, 9.9.9.9"), empty when automatic.
pub fn manual_fields(m: Option<&Manual>) -> [String; 3] {
    match m {
        None => Default::default(),
        Some(m) => [
            format!("{}/{}", m.addr, m.prefix),
            m.router.map(|r| r.to_string()).unwrap_or_default(),
            m.dns.iter().map(|d| d.to_string()).collect::<Vec<_>>().join(", "),
        ],
    }
}

fn list<T: std::str::FromStr>(s: &str, what: &str) -> Result<Vec<T>, String> {
    s.split([',', ' ']).filter(|x| !x.is_empty()).map(|x| x.parse().map_err(|_| format!("{x:?} is not {what}"))).collect()
}

/// What was typed on the IP settings row, as a fixed setting. The address
/// may leave out the prefix ("/24" is assumed, as most home networks are).
pub fn parse_manual(addr: &str, router: &str, dns: &str) -> Result<Manual, String> {
    let addr = addr.trim();
    if addr.is_empty() {
        return Err("type an address, such as 192.168.1.20/24".into());
    }
    let (a, p) = addr.split_once('/').unwrap_or((addr, "24"));
    let addr: Ipv4Addr = a.trim().parse().map_err(|_| format!("{a:?} is not an IPv4 address"))?;
    let prefix: u8 = p.trim().parse().map_err(|_| format!("/{p} is not a prefix length"))?;
    let router = match router.trim() {
        "" => None,
        r => Some(r.parse().map_err(|_| format!("{r:?} is not an IPv4 address"))?),
    };
    let dns = list(dns, "an IPv4 address")?;
    let m = Manual { addr, prefix, router, dns };
    m.check().map_err(|f| f.1)?;
    Ok(m)
}

/// What was typed on the DNS servers row (IPv4 or IPv6, up to 3).
pub fn parse_dns(s: &str) -> Result<Vec<IpAddr>, String> {
    let v = list(s, "an IP address")?;
    check_dns(&v).map_err(|f| f.1)?;
    Ok(v)
}

/// The DNS servers row: the servers, and the name to ask them as over TLS
/// (empty: plain DNS).
pub fn parse_dns_row(servers: &str, tls: &str) -> Result<Want, String> {
    let v = parse_dns(servers)?;
    let name = tls.trim().trim_end_matches('.').to_ascii_lowercase();
    if name.is_empty() {
        return Ok(Want::SetDns(v));
    }
    if v.is_empty() {
        return Err("Encrypted DNS needs the servers' addresses too".into());
    }
    if !tls_name_ok(&name) {
        return Err(format!("\"{}\" is not a server name like dns.example.net", tls.trim()));
    }
    Ok(Want::SetDnsTls(DnsTls { servers: v, name }))
}

/// How long a press on Join waits for Arca's handle.
pub const ARMED_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

/// The wired 802.1X profile (wifi.md §7.5) where the sheet, the trust
/// prompt and the requests otherwise carry a Wi-Fi network's SSID: no SSID
/// is empty, so an empty one stands for the wired network.
pub const WIRED: &[u8] = &[];
const SIGNING_IN_WIRED: &str = "Signing in on the wired network";

pub fn name(s: &[u8]) -> String {
    if s == WIRED {
        return "the wired network".into();
    }
    String::from_utf8_lossy(s).into_owned()
}

/// What the wired port's 802.1X state says, for its row.
pub fn wired_label(i: &Iface) -> Option<String> {
    let why = |w: &str| match w {
        "eap-trust" => "check its server first".to_string(),
        "eap-rejected" => "the username or password is wrong".to_string(),
        "eap-untrusted" => "its server is not the one expected".to_string(),
        "eap-method" | "eap-refused" => "it does not accept this sign-in method".to_string(),
        w => w.replace('-', " "),
    };
    Some(match (i.dot1x.as_deref()?, i.dot1x_why.as_deref()) {
        ("needed", _) => "This port asks you to sign in".into(),
        ("running", _) => "Signing in…".into(),
        ("authorized", _) => "Signed in".into(),
        ("failed", w) => format!("Could not sign in: {}", why(w.unwrap_or("unknown"))),
        (s, _) => s.to_string(),
    })
}

/// "Wi-Fi: home", "Ethernet", "Not connected", and what is wrong.
pub fn headline(s: Option<&Status>, reachable: bool) -> (String, Option<String>) {
    let Some(s) = s.filter(|_| reachable) else {
        return ("Network settings are not available".into(), Some("Can't reach the network service; trying again".into()));
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
        "wpa2-enterprise" => "WPA2 Enterprise",
        "wpa3-enterprise" => "WPA3 Enterprise",
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
            self.ent = (self.asking.is_some() && n.security.ends_with("-enterprise")).then(EntForm::default);
            self.handle = None;
            self.armed = None;
        }
    }

    /// The work-network sheet's method button: the next method.
    pub fn next_method(&mut self) {
        if let Some(f) = &mut self.ent {
            f.method = (f.method + 1) % EAP_METHODS.len();
        }
    }

    /// The check button: first join, system certificates, a CA file (the
    /// picker opens for it), and round again.
    pub fn next_check(&mut self) {
        let Some(f) = &mut self.ent else { return };
        f.check = match f.check {
            Check::FirstJoin => Check::System,
            Check::System => {
                if f.ca.is_none() {
                    self.pick = Some(NetPick::Ca);
                }
                Check::CaFile
            }
            Check::CaFile => Check::FirstJoin,
        };
    }

    /// A certificate login: make the key in Arca and its request, named
    /// for the username typed.
    pub fn request_certificate(&mut self, user: &str) {
        let (Some(ssid), Some(f)) = (self.asking.clone(), &mut self.ent) else { return };
        let user = user.trim();
        if user.is_empty() || user.len() > 64 {
            self.notice = Some(Notice { text: "Type your username first: the request carries it".into(), error: true });
            return;
        }
        f.request = None;
        f.cert = None;
        self.notice = None;
        self.wants.push(Want::CertRequest(CertRequest { ssid, name: user.to_string() }));
    }

    pub fn import_certificate(&mut self) {
        if self.ent.as_ref().is_some_and(|f| f.request.is_some()) {
            self.pick = Some(NetPick::Cert);
        }
    }

    /// Join pressed on the work-network sheet, with what its fields hold.
    pub fn ent_join_pressed(&mut self, user: &str, domain: &str, anonymous: &str, now: std::time::Instant) {
        let (user, domain, anonymous) = (user.trim(), domain.trim().trim_end_matches('.').to_ascii_lowercase(), anonymous.trim());
        let Some(f) = &mut self.ent else { return };
        let err = if user.is_empty() {
            Some("Type your username first")
        } else if f.check != Check::FirstJoin && domain.is_empty() {
            Some("Checking the server with certificates needs its domain")
        } else if f.check == Check::CaFile && f.ca.is_none() {
            Some("Pick the network's CA certificate first")
        } else if f.tls() && f.cert.is_none() {
            Some("Import the certificate your IT team signed first")
        } else {
            None
        };
        if let Some(e) = err {
            self.notice = Some(Notice { text: e.into(), error: true });
            return;
        }
        f.typed = Some((user.to_string(), domain, anonymous.to_string()));
        if f.tls() {
            // No password: the key in Arca signs.
            let Some(ssid) = self.asking.clone() else { return };
            let e = self.enterprise(ssid, None);
            self.wants.extend(e.map(Want::JoinEnterprise));
            return;
        }
        self.join_pressed(now);
    }

    /// The request for the sheet as it stands.
    fn enterprise(&mut self, ssid: Vec<u8>, handle: Option<[u8; 16]>) -> Option<Enterprise> {
        let f = self.ent.as_ref()?;
        let (identity, domain, anonymous) = f.typed.clone()?;
        self.joining = Some(ssid.clone());
        Some(Enterprise {
            ssid,
            method: EAP_METHODS[f.method].0.into(),
            identity,
            // EAP-TLS has no tunnel: its outer identity is the username the
            // certificate names, so an anonymous one would only be refused.
            anonymous: (!anonymous.is_empty() && !f.tls()).then_some(anonymous),
            domain,
            ca: f.ca.as_ref().filter(|_| f.check == Check::CaFile).map(|c| c.1.clone()),
            system_roots: f.check == Check::System,
            handle,
            key: f.request.as_ref().filter(|_| f.tls()).map(|r| r.0),
            cert: f.cert.as_ref().filter(|_| f.tls()).map(|c| c.1.clone()),
        })
    }

    /// "Trust this server" on the certificate a first join stopped at.
    pub fn trust(&mut self) {
        let Some((ssid, _)) = self.trusting.take() else { return };
        self.notice = None;
        self.joining = Some(ssid.clone());
        self.wants.push(Want::Trust(ssid));
    }

    pub fn dont_trust(&mut self) {
        if let Some((ssid, _)) = self.trusting.take() {
            self.notice = Some(Notice { text: format!("Did not join {}: its server was not trusted", name(&ssid)), error: false });
        }
    }

    /// Join pressed on the passphrase row.
    pub fn join_pressed(&mut self, now: std::time::Instant) {
        let Some(ssid) = self.asking.clone() else { return };
        self.notice = None;
        match self.handle.take() {
            Some(handle) => {
                self.armed = None;
                if self.ent.is_some() {
                    let e = self.enterprise(ssid, Some(handle));
                    self.wants.extend(e.map(Want::JoinEnterprise));
                } else {
                    self.joining = Some(ssid.clone());
                    self.wants.push(Want::JoinWith { ssid, handle });
                }
            }
            None => self.armed = Some(now),
        }
    }

    /// Arca's field said something: a handle (what was typed is held) or
    /// none (emptied).
    pub fn field(&mut self, handle: Option<[u8; 16]>, now: std::time::Instant) {
        self.handle = handle;
        if handle.is_some() && self.armed.take().is_some() {
            self.join_pressed(now);
        }
    }

    /// The Ethernet port whose switch speaks 802.1X, if any.
    pub fn wired_port(&self) -> Option<&Iface> {
        self.ifaces.iter().find(|i| !i.wifi && i.dot1x.is_some())
    }

    /// "Sign in…" on the wired port's row: the work-network sheet for it.
    pub fn sign_in_wired(&mut self) {
        if self.asking.as_deref() == Some(WIRED) {
            return self.cancel_ask();
        }
        self.notice = None;
        self.asking = Some(WIRED.to_vec());
        self.ent = Some(EntForm::default());
        self.handle = None;
        self.armed = None;
    }

    pub fn forget_wired(&mut self) {
        self.wants.push(Want::WiredForget);
    }

    /// Keeps the interfaces fresh: a switch may ask to sign in at any time,
    /// and a wired port signing in changes every few seconds.
    pub fn refresh(&mut self, now: std::time::Instant) {
        let every = match self.wired_port().and_then(|i| i.dot1x.as_deref()) {
            Some("running") => std::time::Duration::from_secs(2),
            _ => std::time::Duration::from_secs(10),
        };
        if self.reachable && self.details_at.is_none_or(|t| now.duration_since(t) >= every) && !self.wants.contains(&Want::Details) {
            self.details_at = Some(now);
            self.wants.push(Want::Details);
        }
    }

    /// No handle came after a press on Join: the field was empty.
    pub fn tick(&mut self, now: std::time::Instant) -> bool {
        match self.armed {
            Some(t) if now.duration_since(t) >= ARMED_WAIT => {
                self.armed = None;
                self.notice = Some(Notice { text: "Type the passphrase in the field first".into(), error: true });
                true
            }
            _ => false,
        }
    }

    pub fn cancel_ask(&mut self) {
        self.asking = None;
        self.ent = None;
        self.pick = None;
        self.armed = None;
        self.handle = None;
        self.notice = None;
    }

    pub fn leave(&mut self) {
        self.wants.push(Want::Leave);
    }

    /// "Sign in to this network": the network's sign-in page in its own
    /// window. Offered whenever a link is up (a portal is only known for
    /// sure when the probe runs, and it is off by default).
    pub fn sign_in(&mut self, activation: Option<[u8; 16]>) {
        self.wants.push(Want::SignIn { activation });
    }

    /// Whether to offer signing in, and whether it is the thing to do now.
    pub fn sign_in_offer(&self) -> Option<bool> {
        let s = self.status.as_ref().filter(|_| self.reachable)?;
        (s.link != Link::None).then_some(s.online == Online::Captive)
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
            self.wants.push(Want::Routes);
        }
    }

    /// The setting a target has now (a saved network's, a wired port's).
    pub fn manual_of(&self, t: &Target) -> Option<&Manual> {
        match t {
            Target::Wifi(s) => self.profiles.iter().find(|p| &p.ssid == s)?.manual.as_ref(),
            Target::Wired(n) => self.ifaces.iter().find(|i| &i.name == n && !i.wifi)?.manual.as_ref(),
        }
    }

    /// Open (or close) the IP settings row for a target.
    pub fn edit_ip(&mut self, t: Target) {
        self.dns_editing = false;
        self.edit_error = None;
        self.ip_editing = if self.ip_editing.as_ref() == Some(&t) { None } else { Some(t) };
    }

    /// Save pressed on the IP settings row: a fixed setting, or with
    /// `automatic`, DHCP again.
    pub fn save_ip(&mut self, automatic: bool, addr: &str, router: &str, dns: &str) {
        let Some(target) = self.ip_editing.clone() else { return };
        let manual = if automatic {
            None
        } else {
            match parse_manual(addr, router, dns) {
                Ok(m) => Some(m),
                Err(e) => {
                    self.edit_error = Some(e);
                    return;
                }
            }
        };
        self.edit_error = None;
        self.ip_editing = None;
        self.wants.push(Want::SetStatic(SetStatic { target, manual }));
    }

    pub fn edit_dns(&mut self) {
        self.ip_editing = None;
        self.edit_error = None;
        self.dns_editing = !self.dns_editing;
    }

    /// Save pressed on the DNS servers row; no servers: each network's
    /// again. A server name: asked only over TLS, as that name.
    pub fn save_dns(&mut self, typed: &str, tls: &str) {
        match parse_dns_row(typed, tls) {
            Ok(w) => {
                self.edit_error = None;
                self.dns_editing = false;
                self.wants.push(w);
            }
            Err(e) => self.edit_error = Some(e),
        }
    }

    pub fn cancel_edit(&mut self) {
        self.ip_editing = None;
        self.dns_editing = false;
        self.edit_error = None;
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
            Got::Details(Ok(v)) => {
                self.ifaces = v;
                // A wired sign-in stopped at the server's certificate: show
                // it once (it stays stopped until trusted or set up again).
                let stopped = self.wired_port().is_some_and(|i| i.dot1x_why.as_deref() == Some("eap-trust"));
                if stopped && !self.wired_seen_asked && self.trusting.is_none() {
                    self.wired_seen_asked = true;
                    self.wants.push(Want::Seen(WIRED.to_vec()));
                }
                if !stopped {
                    self.wired_seen_asked = false;
                }
                // "Signing in" lasts while the port runs; the row says how it ended.
                let running = self.wired_port().is_some_and(|i| i.dot1x.as_deref() == Some("running"));
                if !running && self.notice.as_ref().is_some_and(|n| n.text == SIGNING_IN_WIRED) {
                    self.notice = None;
                }
            }
            Got::Details(Err(_)) => {}
            Got::OwnDns(Ok((v, tls))) => {
                self.own_dns = v;
                self.dns_tls = tls;
            }
            Got::OwnDns(Err(_)) => {}
            Got::KeyRequest(ssid, r) => match (r, &mut self.ent) {
                (Ok(k), Some(f)) if self.asking.as_deref() == Some(ssid.as_slice()) => {
                    f.request = Some((k.key, der_to_pem(&k.csr, "CERTIFICATE REQUEST")));
                    self.notice = Some(Notice { text: "Give your IT team this certificate request, then import what they sign".into(), error: false });
                }
                (Ok(_), _) => {}
                (Err(e), _) => self.notice = Some(Notice { text: format!("No certificate request: {}", e.replace('-', " ")), error: true }),
            },
            Got::Picked(r) => {
                let what = self.pick.take();
                match (r, what, &mut self.ent) {
                    (Ok((file, bytes)), Some(w), Some(f)) => match pem_to_der(&bytes).filter(|d| d.len() <= atrium_netconf::MAX_CERT) {
                        Some(der) if w == NetPick::Ca => f.ca = Some((file, der)),
                        Some(der) => {
                            f.cert = Some((file, der));
                            self.notice = None;
                        }
                        None => self.notice = Some(Notice { text: format!("{file} is not a certificate (PEM or DER)"), error: true }),
                    },
                    (Err(e), _, Some(f)) => {
                        if what == Some(NetPick::Ca) && f.ca.is_none() {
                            f.check = Check::FirstJoin;
                        }
                        if !e.is_empty() {
                            self.notice = Some(Notice { text: e, error: true });
                        }
                    }
                    _ => {}
                }
            }
            Got::Seen(ssid, Ok(seen)) => self.trusting = Some((ssid, seen)),
            Got::Seen(ssid, Err(e)) => self.notice = Some(Notice { text: format!("Could not show the server of {}: {}", name(&ssid), e.replace('-', " ")), error: true }),
            Got::Done { what, result } => {
                match (&what, &result) {
                    // The wired profile is saved at once; the port's row
                    // follows the sign-in itself.
                    (Want::JoinEnterprise(Enterprise { ssid, .. }) | Want::Trust(ssid), Ok(())) if ssid.as_slice() == WIRED => {
                        self.joining = None;
                        self.asking = None;
                        self.ent = None;
                        self.notice = Some(Notice { text: SIGNING_IN_WIRED.into(), error: false });
                        self.wants.push(Want::Details);
                    }
                    (Want::WiredForget, Ok(())) => {
                        self.notice = Some(Notice { text: "The wired network's sign-in is forgotten".into(), error: false });
                        self.wants.push(Want::Details);
                    }
                    (Want::Join { ssid, .. } | Want::JoinWith { ssid, .. } | Want::JoinEnterprise(Enterprise { ssid, .. }) | Want::Trust(ssid), Ok(())) => {
                        self.joining = None;
                        self.asking = None;
                        self.ent = None;
                        self.notice = Some(Notice { text: format!("Connected to {}", name(ssid)), error: false });
                    }
                    // A first join with nothing to check the server with
                    // stopped at its certificate: show it.
                    (Want::Join { ssid, .. } | Want::JoinEnterprise(Enterprise { ssid, .. }), Err(e)) if e == "eap-trust" => {
                        self.joining = None;
                        self.asking = None;
                        self.ent = None;
                        self.handle = None;
                        self.reset_field = true;
                        self.wants.push(Want::Seen(ssid.clone()));
                        self.notice = Some(Notice { text: format!("Check the server of {} before your password goes to it", name(ssid)), error: false });
                    }
                    (Want::Join { ssid, .. } | Want::JoinWith { ssid, .. } | Want::JoinEnterprise(Enterprise { ssid, .. }) | Want::Trust(ssid), Err(e)) => {
                        self.joining = None;
                        if matches!(what, Want::JoinWith { .. } | Want::JoinEnterprise(_)) {
                            self.handle = None;
                            self.reset_field = true;
                        }
                        let why = match e.as_str() {
                            "wrong-passphrase" => "the passphrase is wrong".to_string(),
                            "eap-rejected" => "the username or password is wrong".to_string(),
                            "eap-untrusted" => "its server is not the one expected (a wrong certificate)".to_string(),
                            "eap-no-system-roots" => "the system has no certificates to check the server with".to_string(),
                            "eap-method" | "eap-refused" => "the network does not accept this sign-in method".to_string(),
                            "timed out" => "no answer in time: out of range, or the network is busy".to_string(),
                            "bad-secret" => "a Wi-Fi passphrase is 8 to 63 letters, digits or symbols".to_string(),
                            w => w.replace('-', " "),
                        };
                        self.notice = Some(Notice { text: format!("Could not join {}: {why}", name(ssid)), error: true });
                    }
                    (Want::SignIn { .. }, Ok(())) => self.notice = Some(Notice { text: "Opening the network's sign-in page".into(), error: false }),
                    (Want::SignIn { .. }, Err(e)) => self.notice = Some(Notice { text: format!("Could not open the sign-in page: {e}"), error: true }),
                    (Want::Forget(ssid), Ok(())) => self.notice = Some(Notice { text: format!("Forgot {}", name(ssid)), error: false }),
                    (Want::SetStatic(s), r) => {
                        let what = match &s.target {
                            Target::Wifi(n) => name(n),
                            Target::Wired(n) => n.clone(),
                        };
                        self.notice = Some(match (r, &s.manual) {
                            (Ok(()), Some(m)) => Notice { text: format!("{what} uses {}/{}", m.addr, m.prefix), error: false },
                            (Ok(()), None) => Notice { text: format!("{what} gets its address automatically"), error: false },
                            (Err(e), _) => Notice { text: format!("Could not change {what}: {}", e.replace('-', " ")), error: true },
                        });
                        self.wants.push(Want::Details);
                    }
                    (Want::SetDns(v), Ok(())) => {
                        self.notice = Some(Notice {
                            text: if v.is_empty() { "DNS servers: each network's own".into() } else { format!("DNS servers: {}", v.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", ")) },
                            error: false,
                        });
                        self.wants.push(Want::Routes);
                    }
                    (Want::SetDnsTls(t), Ok(())) => {
                        let a = t.servers.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", ");
                        self.notice = Some(Notice { text: format!("DNS servers: {a}, encrypted ({})", t.name), error: false });
                        self.wants.push(Want::Routes);
                    }
                    (Want::SetDnsTls(_), Err(e)) => {
                        let why = match e.as_str() {
                            "no-tls-bundle" => "the system has no certificates to check the servers with".to_string(),
                            w => w.replace('-', " "),
                        };
                        self.notice = Some(Notice { text: format!("Could not turn on encrypted DNS: {why}"), error: true });
                    }
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
        Profile { ssid: s.as_bytes().to_vec(), open: false, wpa3_only: false, mac: "network".into(), auto: true, prio: 0, metered: false, manual: None, enterprise: false }
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
    fn sign_in_is_offered_while_linked_and_wanted_on_a_portal() {
        let mut p = page();
        assert_eq!(p.sign_in_offer(), Some(false), "offered quietly: the probe is off");
        p.apply(Got::Status(Some(Status { link: Link::Wifi, online: Online::Captive, metered: false, bars: Some(3), ssid: Some(b"cafe".to_vec()), portal: None })));
        p.take_wants();
        assert_eq!(p.sign_in_offer(), Some(true));
        p.sign_in(Some([7; 16]));
        assert_eq!(p.take_wants(), vec![Want::SignIn { activation: Some([7; 16]) }]);
        p.apply(Got::Done { what: Want::SignIn { activation: Some([7; 16]) }, result: Err("no installed app holds network-portal".into()) });
        assert!(p.notice.as_ref().is_some_and(|n| n.error && n.text.contains("network-portal")));
        p.apply(Got::Status(Some(Status::offline())));
        assert_eq!(p.sign_in_offer(), None, "nothing to sign in to");
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
        p.apply(Got::Done { what: Want::Join { ssid: b"office".to_vec(), open: false }, result: Err("wrong-passphrase".into()) });
        assert!(p.notice.as_ref().unwrap().error && p.joining.is_none());
        assert_eq!(p.notice.as_ref().unwrap().text, "Could not join office: the passphrase is wrong");
        p.take_wants();
        p.pick(b"neighbour");
        assert!(p.take_wants().is_empty());
        assert_eq!(p.asking.as_deref(), Some(&b"neighbour"[..]));
        // Join pressed on an empty field: no handle comes, so it says why.
        let t = std::time::Instant::now();
        p.join_pressed(t);
        assert!(p.take_wants().is_empty());
        assert!(!p.tick(t) && p.tick(t + ARMED_WAIT));
        assert!(p.notice.as_ref().unwrap().error);
        // The press takes the field's focus; Arca's handle follows and joins.
        p.join_pressed(t);
        p.field(Some([5; 16]), t);
        assert_eq!(p.take_wants(), vec![Want::JoinWith { ssid: b"neighbour".to_vec(), handle: [5; 16] }]);
        assert_eq!(p.handle, None, "one join per handle");
        p.apply(Got::Done { what: Want::JoinWith { ssid: b"neighbour".to_vec(), handle: [5; 16] }, result: Ok(()) });
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
    fn ip_settings_parse_check_and_send() {
        let m = parse_manual("192.168.1.20", "192.168.1.1", "1.1.1.1, 9.9.9.9").unwrap();
        assert_eq!((m.prefix, m.router, m.dns.len()), (24, Some("192.168.1.1".parse().unwrap()), 2), "/24 assumed");
        assert_eq!(manual_fields(Some(&m)), ["192.168.1.20/24".to_string(), "192.168.1.1".into(), "1.1.1.1, 9.9.9.9".into()]);
        assert!(parse_manual("", "", "").is_err());
        assert!(parse_manual("192.168.1.0/24", "", "").is_err(), "the network's own address");
        assert!(parse_manual("192.168.1.20/24", "10.0.0.1", "").is_err(), "router outside");
        assert!(parse_manual("192.168.1.20/33", "", "").is_err());
        assert!(parse_manual("fe80::1/64", "", "").is_err(), "IPv4 only");
        assert!(parse_dns("").unwrap().is_empty(), "empty: each network's");
        assert_eq!(parse_dns("9.9.9.9 2620:fe::fe").unwrap().len(), 2);
        assert!(parse_dns("127.0.0.1").is_err());
        assert!(parse_dns("1.1.1.1,1.0.0.1,9.9.9.9,8.8.8.8").is_err(), "at most 3");

        let mut p = page();
        p.take_wants();
        p.edit_ip(Target::Wifi(b"office".to_vec()));
        p.save_ip(false, "10.0.0.300", "", "");
        assert!(p.edit_error.is_some() && p.ip_editing.is_some() && p.take_wants().is_empty(), "a typo keeps the row open");
        p.save_ip(false, "10.0.0.5/24", "10.0.0.1", "");
        assert_eq!(p.ip_editing, None);
        let w = p.take_wants();
        assert!(matches!(&w[..], [Want::SetStatic(SetStatic { target: Target::Wifi(s), manual: Some(m) })] if s == b"office" && m.addr == Ipv4Addr::new(10, 0, 0, 5)));
        p.apply(Got::Done { what: w[0].clone(), result: Ok(()) });
        assert_eq!(p.notice.as_ref().unwrap().text, "office uses 10.0.0.5/24");
        assert!(p.take_wants().contains(&Want::Details));
        p.edit_ip(Target::Wired("em0".into()));
        p.save_ip(true, "junk", "", "");
        assert_eq!(p.take_wants(), vec![Want::SetStatic(SetStatic { target: Target::Wired("em0".into()), manual: None })], "automatic ignores the fields");

        p.edit_dns();
        p.save_dns("9.9.9.9", "");
        let w = p.take_wants();
        assert_eq!(w, vec![Want::SetDns(vec!["9.9.9.9".parse().unwrap()])]);
        p.apply(Got::Done { what: w[0].clone(), result: Ok(()) });
        assert!(p.take_wants().contains(&Want::Routes));
        p.apply(Got::OwnDns(Ok((vec!["9.9.9.9".into()], None))));
        assert_eq!(p.own_dns, vec!["9.9.9.9".to_string()]);
        p.apply(Got::Done { what: Want::SetDns(vec![]), result: Err("not-allowed".into()) });
        assert!(p.notice.as_ref().unwrap().error);

        // Encrypted: a server name with the addresses.
        p.take_wants();
        p.edit_dns();
        p.save_dns("9.9.9.9, 149.112.112.112", " DNS.Quad9.net. ");
        let w = p.take_wants();
        let t = DnsTls { servers: vec!["9.9.9.9".parse().unwrap(), "149.112.112.112".parse().unwrap()], name: "dns.quad9.net".into() };
        assert_eq!(w, vec![Want::SetDnsTls(t.clone())]);
        p.apply(Got::Done { what: w[0].clone(), result: Ok(()) });
        assert_eq!(p.notice.as_ref().unwrap().text, "DNS servers: 9.9.9.9, 149.112.112.112, encrypted (dns.quad9.net)");
        assert!(p.take_wants().contains(&Want::Routes));
        p.apply(Got::OwnDns(Ok((vec!["9.9.9.9".into()], Some("dns.quad9.net".into())))));
        assert_eq!(p.dns_tls.as_deref(), Some("dns.quad9.net"));
        p.apply(Got::Done { what: Want::SetDnsTls(t), result: Err("no-tls-bundle".into()) });
        assert_eq!(p.notice.as_ref().unwrap().text, "Could not turn on encrypted DNS: the system has no certificates to check the servers with");
        p.take_wants();
        for (servers, name) in [("", "dns.quad9.net"), ("9.9.9.9", "quad9"), ("9.9.9.9", "9.9.9.9"), ("9.9.9.9", "dns quad9.net")] {
            p.edit_dns();
            p.save_dns(servers, name);
            assert!(p.edit_error.is_some() && p.take_wants().is_empty(), "{servers} {name}");
        }
    }

    #[test]
    fn work_network_sheet_then_trust_this_server() {
        let mut p = page();
        p.apply(Got::Nearby(Ok(vec![near("corp", "wpa3-enterprise", false)])));
        p.take_wants();
        p.pick(b"corp");
        assert!(p.ent.is_some(), "an 802.1X network opens the work-network sheet");
        let now = std::time::Instant::now();
        p.ent_join_pressed("  ", "", "", now);
        assert!(p.notice.as_ref().is_some_and(|n| n.error), "no username: said so");
        assert!(p.take_wants().is_empty());
        p.next_check();
        p.ent_join_pressed("alice@corp.example", "", "", now);
        assert!(p.take_wants().is_empty(), "system certificates need the domain");
        p.next_check();
        assert_eq!(p.pick, Some(NetPick::Ca), "a CA file opens the picker");
        p.apply(Got::Picked(Err(String::new())));
        assert_eq!(p.ent.as_ref().unwrap().check, Check::FirstJoin, "cancelled: back to asking on the first join");
        p.next_method();
        p.ent_join_pressed("alice@corp.example", "Corp.Example.", "", now);
        assert!(p.take_wants().is_empty(), "waits for Arca's handle");
        p.field(Some([3; 16]), now);
        let w = p.take_wants();
        let [Want::JoinEnterprise(e)] = w.as_slice() else { panic!("{w:?}") };
        assert_eq!((e.ssid.as_slice(), e.method.as_str(), e.identity.as_str(), e.domain.as_str(), e.handle, e.system_roots), (&b"corp"[..], "ttls-mschapv2", "alice@corp.example", "corp.example", Some([3; 16]), false));
        assert!(e.check().is_ok());
        assert_eq!(format!("{:?}", w[0]), "JoinEnterprise(\"corp\", ttls-mschapv2)", "no handle in logs");

        // The first join stopped at the server's certificate.
        p.apply(Got::Done { what: w[0].clone(), result: Err("eap-trust".into()) });
        assert!(p.take_wants().contains(&Want::Seen(b"corp".to_vec())));
        assert!(p.reset_field, "the spent handle is dropped");
        let seen = ServerSeen { names: vec!["radius.corp.example".into()], subject: "radius.corp.example".into(), issuer: "corp CA".into(), not_after: "2027-01-01".into(), sha256: vec![0xab; 32] };
        p.apply(Got::Seen(b"corp".to_vec(), Ok(seen)));
        assert!(p.trusting.is_some());
        p.take_wants();
        p.trust();
        assert_eq!(p.take_wants(), vec![Want::Trust(b"corp".to_vec())]);
        assert_eq!(p.joining.as_deref(), Some(&b"corp"[..]));
        p.apply(Got::Done { what: Want::Trust(b"corp".to_vec()), result: Err("eap-rejected".into()) });
        assert!(p.notice.as_ref().unwrap().text.contains("username or password is wrong"));
    }

    #[test]
    fn certificate_login_and_a_ca_file() {
        let mut p = page();
        p.apply(Got::Nearby(Ok(vec![near("corp", "wpa2-enterprise", false)])));
        p.pick(b"corp");
        p.take_wants();
        let now = std::time::Instant::now();
        for _ in 0..3 {
            p.next_method();
        }
        assert!(p.ent.as_ref().unwrap().tls());
        p.request_certificate("");
        assert!(p.take_wants().is_empty(), "the request carries the username");
        p.request_certificate("tls-user@corp.example");
        assert_eq!(p.take_wants(), vec![Want::CertRequest(CertRequest { ssid: b"corp".to_vec(), name: "tls-user@corp.example".into() })]);
        p.ent_join_pressed("tls-user@corp.example", "", "", now);
        assert!(p.take_wants().is_empty(), "no certificate yet");
        p.apply(Got::KeyRequest(b"corp".to_vec(), Ok(KeyRequest { key: [7; 16], csr: vec![0x30, 1, 0] })));
        assert!(p.ent.as_ref().unwrap().request.as_ref().unwrap().1.starts_with("-----BEGIN CERTIFICATE REQUEST-----"));
        p.import_certificate();
        assert_eq!(p.pick, Some(NetPick::Cert));
        p.apply(Got::Picked(Ok(("me.txt".into(), b"hello".to_vec()))));
        assert!(p.notice.as_ref().is_some_and(|n| n.error), "not a certificate: said so");
        p.import_certificate();
        p.apply(Got::Picked(Ok(("me.pem".into(), der_to_pem(&[0x30, 2, 5, 5], "CERTIFICATE").into_bytes()))));
        // A CA from a file, with its domain.
        p.next_check();
        p.next_check();
        assert_eq!(p.pick, Some(NetPick::Ca));
        p.apply(Got::Picked(Ok(("ca.der".into(), vec![0x30, 1, 9]))));
        p.ent_join_pressed("tls-user@corp.example", "corp.example", "anon@corp.example", now);
        let w = p.take_wants();
        let [Want::JoinEnterprise(e)] = w.as_slice() else { panic!("{w:?}") };
        assert_eq!((e.method.as_str(), e.key, e.cert.as_deref(), e.handle, e.ca.as_deref(), e.anonymous.as_deref()), ("tls", Some([7; 16]), Some(&[0x30, 2, 5, 5][..]), None, Some(&[0x30, 1, 9][..]), None));
        assert!(e.check().is_ok(), "{:?}", e.check());
    }

    #[test]
    fn a_wired_port_that_asks_to_sign_in() {
        let mut p = page();
        p.reachable = true;
        let port = |state: &str, why: Option<&str>| Iface { name: "em0".into(), wifi: false, link: "up".into(), addr: None, router: None, dns: vec![], mac: String::new(), addrs6: vec![], dns6: vec![], online: Online::No, metered: false, egress: false, manual: None, dot1x: Some(state.into()), dot1x_why: why.map(String::from) };
        p.apply(Got::Details(Ok(vec![port("needed", None)])));
        assert_eq!(wired_label(p.wired_port().unwrap()).as_deref(), Some("This port asks you to sign in"));
        p.sign_in_wired();
        assert_eq!(p.asking.as_deref(), Some(WIRED));
        p.next_method();
        p.ent_join_pressed("alice@corp.example", "", "", std::time::Instant::now());
        p.field(Some([3; 16]), std::time::Instant::now());
        let w = p.take_wants();
        let Some(Want::JoinEnterprise(e)) = w.iter().find(|w| matches!(w, Want::JoinEnterprise(_))) else { panic!("{w:?}") };
        assert!(e.ssid.is_empty() && e.method == "ttls-mschapv2" && e.handle == Some([3; 16]), "{e:?}");
        assert!(e.check().is_ok());
        p.apply(Got::Done { what: Want::JoinEnterprise(e.clone()), result: Ok(()) });
        assert!(p.asking.is_none() && p.notice.as_ref().is_some_and(|n| n.text == "Signing in on the wired network"));
        p.apply(Got::Details(Ok(vec![port("running", None)])));
        assert!(p.notice.is_some(), "kept while the port signs in");
        // It stops at the server: shown once, trusted through Configure.
        p.take_wants();
        p.apply(Got::Details(Ok(vec![port("failed", Some("eap-trust"))])));
        assert_eq!(p.take_wants(), vec![Want::Seen(vec![])]);
        p.apply(Got::Details(Ok(vec![port("failed", Some("eap-trust"))])));
        assert!(p.take_wants().is_empty(), "asked once");
        assert!(p.notice.is_none(), "the row says how it ended");
        // Kept fresh: every 2 s while signing in.
        let t = std::time::Instant::now();
        p.apply(Got::Details(Ok(vec![port("running", None)])));
        p.refresh(t);
        assert_eq!(p.take_wants(), vec![Want::Details]);
        p.refresh(t + std::time::Duration::from_secs(1));
        assert!(p.take_wants().is_empty());
        p.refresh(t + std::time::Duration::from_secs(2));
        assert_eq!(p.take_wants(), vec![Want::Details]);
        assert_eq!(name(WIRED), "the wired network");
    }

    #[test]
    fn debug_never_shows_a_handle() {
        let w = Want::JoinWith { ssid: b"x".to_vec(), handle: [0xab; 16] };
        assert!(!format!("{w:?}").contains("171"));
    }

    #[test]
    fn headline() {
        assert_eq!(super::headline(None, false).0, "Network settings are not available");
        let s = Status { link: Link::Wired, online: Online::Captive, metered: true, bars: None, ssid: None, portal: Some("http://p/".into()) };
        assert_eq!(super::headline(Some(&s), true), ("Ethernet".into(), Some("Sign-in needed: http://p/ · Metered".into())));
    }
}
