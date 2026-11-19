//! The VPN page (vpn.md §7.3): the tunnels (connect, disconnect, the block
//! switch, the whole computer or chosen personas, remove), which personas
//! use each, and a new tunnel: a WireGuard file through the system picker,
//! or a key made in Arca for a server whose details the person types.
//! cryptad knows Curia by `vpn-configure`; the personas' routes are the
//! person's policy, changed through portcullisd with a press. `main.rs`
//! runs [`work`] for each [`Want`] and hands back what it [`Got`]; the
//! drawing is [`CuriaView::vpn`]. What it does is logged as `curia: vpn: …`
//! (the VM check reads it).
//!
//! Corporate VPNs (V4, vpn.md §9.3): a gateway's name makes a tunnel; when
//! it asks (a form, step by step), the form shows under its card: text and
//! choices here, passwords and codes in Arca's credential field, whose
//! handle is all Curia sends. A client certificate's request is made in
//! Arca and copied for the IT team; the signed certificate comes back
//! through the picker.

use std::path::PathBuf;
use std::sync::mpsc::Sender;

use crypta_ctl::client::Client;
use crypta_ctl::{Answer, Ask, CertRequest, Certificate, CreateAnyConnect, CreateWireGuard, Created, Details, FieldKind, Grant, ImportWireGuard, Kind, Net, Scope, SetProfile, SsoDone, State, Value};
use pergola::geom::Rect;
use pergola::layout::FlexStyle;
use pergola::node::Node;
use pergola::text_edit::TextEdit;
use pergola::theme::{type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::widgets::{phosphor as ph, ScrollView};
use pergola::{Color, Mutable};

use crate::apps::persona_name;
use crate::view::{button, em, icon, rect, text, CuriaView, Look};

/// Where the system picker is drawn (over the whole window).
pub const PICK_SLOT: pergola::slot::SlotKey = 0x5650_4e31;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Want {
    Refresh,
    Connect(String),
    Disconnect(String),
    Set(SetProfile),
    Remove(String),
    Create(CreateWireGuard),
    Import(ImportWireGuard),
    /// Persona `persona` goes through `vpn` (None: direct), with the press.
    PersonaVpn { persona: String, vpn: Option<String>, activation: Option<[u8; 16]> },
    /// The system picker for a `.conf` (main.rs opens it: it holds limend),
    /// or for a certificate when `pick_cert` names a tunnel.
    Pick,
    CreateAc(CreateAnyConnect),
    /// What the tunnel's gateway asks.
    Prompt(String),
    Answer(Answer),
    SsoDone(SsoDone),
    /// The sign-in window for a tunnel's single sign-on, with the press.
    SsoWindow { name: String, login: String, final_url: String, activation: Option<[u8; 16]> },
    CertRequest(String),
    Certificate(Certificate),
}

#[derive(Clone, Debug)]
pub enum Got {
    Tunnels(Result<Vec<Details>, String>),
    /// The person's named personas, and (persona, tunnel) for each routed one.
    Personas(Result<(Vec<String>, Vec<(String, String)>), String>),
    /// A change: what to say, or why it failed.
    Done(Result<String, String>),
    /// A new tunnel, and its public key for the server's admin.
    Created(Result<Created, String>),
    /// The picker gave a file (its name and bytes), or why not.
    Picked(Result<(String, Vec<u8>), String>),
    /// What a tunnel's gateway asks (None: nothing now).
    Prompt(String, Result<Option<Ask>, String>),
    /// A certificate request: (tunnel, PEM).
    CertRequest(Result<(String, String), String>),
}

/// A sign-in on the page: the tunnel, what its gateway asks, and the
/// person's choices so far.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignIn {
    pub name: String,
    pub ask: Ask,
    /// For each choice field (by field index), the chosen one.
    pub chosen: Vec<usize>,
    /// Save the password in Arca for this gateway.
    pub keep: bool,
}

impl SignIn {
    pub fn secrets(&self) -> usize {
        match &self.ask {
            Ask::Form(f) => f.fields.iter().filter(|f| matches!(f.kind, FieldKind::Password | FieldKind::PasswordOnce | FieldKind::Code)).count(),
            Ask::Sso { .. } => 0,
        }
    }

    pub fn has_password(&self) -> bool {
        matches!(&self.ask, Ask::Form(f) if f.fields.iter().any(|f| f.kind == FieldKind::Password))
    }
}

/// The corporate form's fields, in order (after the WireGuard form's six).
pub const AC_FIELDS: [&str; 4] = ["Name, like work", "Gateway, like vpn.example.com", "Your username (optional)", "Gateway certificate SHA-256 (only for a self-signed gateway)"];
/// Where the corporate form's fields start in `vpn_fields`, and the
/// sign-in's text fields after them.
pub const AC_AT: usize = 6;
pub const ANSWER_AT: usize = 10;
pub const MAX_TEXTS: usize = 4;

/// The new-tunnel form's fields, in order.
pub const FIELDS: [&str; 6] = ["Name, like work", "Server, like 203.0.113.5:51820", "The server's public key", "Your address in the tunnel, like 10.7.0.2/32", "Networks it carries (0.0.0.0/0: everything)", "DNS servers in the tunnel (optional)"];

#[derive(Clone, Debug, Default)]
pub struct VpnPage {
    /// None until crypta-ctl answers.
    pub tunnels: Option<Result<Vec<Details>, String>>,
    pub personas: Option<Result<(Vec<String>, Vec<(String, String)>), String>>,
    /// A change on its way (nothing else is offered meanwhile).
    pub busy: bool,
    /// The last change's outcome: (failed, words).
    pub note: Option<(bool, String)>,
    /// Remove pressed once: press again to remove.
    pub confirm_remove: Option<String>,
    /// The new-tunnel form is open.
    pub creating: bool,
    /// A tunnel just made or imported: its key, to give the server.
    pub made: Option<Created>,
    /// The picker is up.
    pub picking: bool,
    /// The corporate form is open.
    pub creating_ac: bool,
    /// The picker is for this tunnel's certificate.
    pub pick_cert: Option<String>,
    /// A certificate request to give the IT team: (tunnel, PEM).
    pub csr: Option<(String, String)>,
    pub signin: Option<SignIn>,
    /// The tunnel whose prompt was asked for.
    asking: Option<String>,
    /// Arca's handle for what was typed in its field.
    pub handle: Option<[u8; 16]>,
    /// Sign in was pressed before the handle came: the text answers.
    pub armed: Option<(std::time::Instant, Vec<String>)>,
    /// Arca's field is on the page.
    pub field_up: bool,
    /// A new form: Arca's field starts empty.
    pub reset_field: bool,
    wants: Vec<Want>,
}

fn list<T>(s: &str, what: &str, f: impl Fn(&str) -> Option<T>) -> Result<Vec<T>, String> {
    s.split([',', ' ']).filter(|x| !x.is_empty()).map(|x| f(x).ok_or_else(|| format!("{x:?} is not {what}"))).collect()
}

/// The new-tunnel form as a request.
pub fn parse_create(f: &[String; 6]) -> Result<CreateWireGuard, String> {
    let name = f[0].trim().to_ascii_lowercase();
    let endpoint = f[1].trim().parse().map_err(|_| "the server is an IPv4 address and port, like 203.0.113.5:51820".to_string())?;
    let nets = |s: &str, what: &str| list(s, what, Net::parse);
    let allowed = if f[4].trim().is_empty() { vec![Net::parse("0.0.0.0/0").unwrap()] } else { nets(&f[4], "a network like 10.0.0.0/8")? };
    let c = CreateWireGuard {
        name,
        peer: f[2].trim().to_string(),
        endpoint,
        addrs: nets(&f[3], "an address like 10.7.0.2/32")?,
        allowed,
        dns: list(&f[5], "an IPv4 address", |a| a.parse().ok())?,
        keepalive: Some(25),
    };
    c.check().map_err(|e| e.1)?;
    Ok(c)
}

/// A SHA-256 typed as hex, with or without colons.
pub fn parse_pin(s: &str) -> Option<[u8; 32]> {
    let h: String = s.chars().filter(|c| !matches!(c, ':' | ' ')).collect();
    if h.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&h[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

/// The corporate form as a request.
pub fn parse_create_ac(f: &[String]) -> Result<CreateAnyConnect, String> {
    let host = f[1].trim().trim_start_matches("https://").trim_end_matches('/').to_ascii_lowercase();
    let pin = match f[3].trim() {
        "" => None,
        p => Some(parse_pin(p).ok_or("the certificate's SHA-256 is 64 hex digits")?),
    };
    let c = CreateAnyConnect {
        name: f[0].trim().to_ascii_lowercase(),
        host,
        path: None,
        user: Some(f[2].trim().to_string()).filter(|u| !u.is_empty()),
        ca: Vec::new(),
        pin,
        personas: true,
    };
    c.check().map_err(|e| e.1)?;
    Ok(c)
}

/// A tunnel name from a picked file's name: `Acme VPN.conf` is `acme-vpn`.
pub fn name_from_file(file: &str) -> String {
    let stem = file.rsplit_once('.').map(|(s, _)| s).unwrap_or(file);
    let mut n: String = stem.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
    while n.contains("--") {
        n = n.replace("--", "-");
    }
    let n: String = n.trim_matches('-').chars().take(32).collect();
    let n = n.trim_end_matches('-').to_string();
    if crypta_ctl::valid_name(&n) {
        n
    } else {
        "vpn".into()
    }
}

/// A tunnel's second line: how it is and what it carries.
pub fn describe(d: &Details) -> String {
    let p = &d.profile;
    let state = match p.tunnel.state {
        State::Up => "Connected",
        State::SignIn => "Waiting for you to sign in",
        State::Connecting if p.block => "Connecting: its traffic waits",
        State::Connecting => "Connecting",
        State::Down => "Not connected",
    };
    let carries = if p.full { "all traffic".to_string() } else { format!("{} {}", d.routes, if d.routes == 1 { "network" } else { "networks" }) };
    let whom = match p.tunnel.scope {
        Scope::Machine => "the whole computer",
        Scope::Personas => "chosen personas",
    };
    format!("{state} \u{b7} {carries} \u{b7} for {whom}")
}

/// Its third line: the server, our address, our key's start.
pub fn detail_line(d: &Details) -> String {
    if let Some(c) = &d.corporate {
        let gateway = if d.profile.tunnel.kind == Kind::GlobalProtect { "GlobalProtect gateway" } else { "Gateway" };
        let mut v = vec![if c.port == 443 { format!("{gateway} {}", c.host) } else { format!("{gateway} {}:{}", c.host, c.port) }];
        if !d.addrs.is_empty() {
            v.push(format!("address {}", d.addrs.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", ")));
        }
        if let Some(u) = &c.user {
            v.push(format!("as {u}"));
        }
        if c.certificate {
            v.push("client certificate".into());
        }
        if c.pinned {
            v.push("pinned certificate".into());
        }
        return v.join(" \u{b7} ");
    }
    let addrs = d.addrs.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", ");
    format!("Server {} \u{b7} address {addrs} \u{b7} key {}…", d.endpoint, &d.public[..8.min(d.public.len())])
}

impl VpnPage {
    pub fn refresh(&mut self) {
        if !self.wants.contains(&Want::Refresh) {
            self.wants.push(Want::Refresh);
        }
    }

    pub fn take_wants(&mut self) -> Vec<Want> {
        std::mem::take(&mut self.wants)
    }

    fn change(&mut self, w: Want) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.note = None;
        self.wants.push(w);
    }

    pub fn connect(&mut self, name: &str, on: bool) {
        self.change(if on { Want::Connect(name.into()) } else { Want::Disconnect(name.into()) });
    }

    pub fn set_block(&mut self, name: &str, on: bool) {
        self.change(Want::Set(SetProfile { name: name.into(), block: Some(on), personas: None }));
    }

    pub fn set_scope(&mut self, name: &str, personas: bool) {
        self.change(Want::Set(SetProfile { name: name.into(), block: None, personas: Some(personas) }));
    }

    /// The first press asks; the second removes.
    pub fn remove(&mut self, name: &str) {
        if self.confirm_remove.as_deref() == Some(name) {
            self.confirm_remove = None;
            self.change(Want::Remove(name.into()));
        } else {
            self.confirm_remove = Some(name.into());
        }
    }

    /// The tunnel persona `p` uses, if any.
    pub fn route_of(&self, p: &str) -> Option<&str> {
        match &self.personas {
            Some(Ok((_, r))) => r.iter().find(|(n, _)| n == p).map(|(_, v)| v.as_str()),
            _ => None,
        }
    }

    /// A press on a persona under a tunnel: on it if it was not, off if it was.
    pub fn toggle_persona(&mut self, persona: &str, tunnel: &str, activation: Option<[u8; 16]>) {
        let vpn = if self.route_of(persona) == Some(tunnel) { None } else { Some(tunnel.to_string()) };
        self.change(Want::PersonaVpn { persona: persona.into(), vpn, activation });
    }

    pub fn create(&mut self, fields: &[String; 6]) {
        match parse_create(fields) {
            Ok(c) => self.change(Want::Create(c)),
            Err(e) => self.note = Some((true, e)),
        }
    }

    pub fn create_ac(&mut self, fields: &[String]) {
        match parse_create_ac(fields) {
            Ok(c) => self.change(Want::CreateAc(c)),
            Err(e) => self.note = Some((true, e)),
        }
    }

    pub fn cert_request(&mut self, name: &str) {
        self.change(Want::CertRequest(name.into()));
    }

    /// The picker, for `name`'s signed certificate.
    pub fn pick_certificate(&mut self, name: &str) {
        if !self.busy && !self.picking {
            self.pick_cert = Some(name.into());
            self.pick();
        }
    }

    pub fn choose(&mut self, field: usize, choice: usize) {
        if let Some(s) = self.signin.as_mut() {
            if let Some(c) = s.chosen.get_mut(field) {
                *c = choice;
            }
        }
    }

    pub fn toggle_keep(&mut self) {
        if let Some(s) = self.signin.as_mut() {
            s.keep = !s.keep;
        }
    }

    /// Sign in pressed: the text fields' answers, in the form's order of
    /// text fields; a password or code is Arca's handle.
    pub fn sign_in(&mut self, texts: Vec<String>, now: std::time::Instant) {
        let Some(s) = self.signin.clone() else { return };
        let Ask::Form(f) = &s.ask else { return };
        if self.busy {
            return;
        }
        if s.secrets() > 1 {
            self.note = Some((true, "This gateway asks two secrets at once, which Settings can't take yet".into()));
            return;
        }
        let mut values = Vec::new();
        let mut t = texts.iter();
        for (i, fl) in f.fields.iter().enumerate() {
            let v = match fl.kind {
                FieldKind::Text => {
                    let x = t.next().cloned().unwrap_or_default();
                    if x.trim().is_empty() {
                        self.note = Some((true, format!("{} needs an answer", fl.label.trim_end_matches(':'))));
                        return;
                    }
                    Value::Text(x.trim().to_string())
                }
                FieldKind::Choice | FieldKind::GatewayChoice | FieldKind::HipApproval => match fl.choices.get(s.chosen.get(i).copied().unwrap_or(0)) {
                    Some((v, _)) => Value::Text(v.clone()),
                    None => {
                        self.note = Some((true, "nothing to choose".into()));
                        return;
                    }
                },
                FieldKind::Password | FieldKind::PasswordOnce | FieldKind::Code => match self.handle {
                    Some(h) => Value::Held(h),
                    None => {
                        self.armed = Some((now, texts));
                        return;
                    }
                },
            };
            values.push(v);
        }
        self.handle = None;
        self.armed = None;
        self.signin = None;
        self.asking = None;
        self.reset_field = true;
        self.change(Want::Answer(Answer { name: s.name.clone(), values: Some(values), keep: s.keep }));
    }

    pub fn cancel_signin(&mut self) {
        if let Some(s) = self.signin.take() {
            self.handle = None;
            self.armed = None;
            self.asking = None;
            self.reset_field = true;
            match s.ask {
                Ask::Form(_) => self.change(Want::Answer(Answer { name: s.name, values: None, keep: false })),
                Ask::Sso { .. } => self.change(Want::SsoDone(SsoDone { name: s.name, ok: false })),
            }
        }
    }

    /// Open (or bring forward) the single sign-on's window: the Navigator,
    /// in the tunnel's own jar. The tunnel signs in once its gateway has
    /// handed the window's sign-in to Arca.
    pub fn sso_window(&mut self, activation: Option<[u8; 16]>) {
        if let Some(SignIn { name, ask: Ask::Sso { login, final_url }, .. }) = &self.signin {
            let w = Want::SsoWindow { name: name.clone(), login: login.clone(), final_url: final_url.clone(), activation };
            self.change(w);
        }
    }

    /// Arca's field said something: a handle, or none (emptied).
    pub fn field(&mut self, handle: Option<[u8; 16]>, now: std::time::Instant) {
        self.handle = handle;
        if handle.is_some() {
            if let Some((_, texts)) = self.armed.take() {
                self.sign_in(texts, now);
            }
        }
    }

    /// No handle came after a press on Sign in: the field was empty.
    pub fn tick(&mut self, now: std::time::Instant) -> bool {
        match &self.armed {
            Some((t, _)) if now.duration_since(*t) >= crate::network::ARMED_WAIT => {
                self.armed = None;
                self.note = Some((true, "Type the password or code in the field first".into()));
                true
            }
            _ => false,
        }
    }

    /// Whether Arca's field belongs on the page now.
    pub fn wants_field(&self) -> bool {
        self.signin.as_ref().is_some_and(|s| s.secrets() == 1)
    }

    pub fn pick(&mut self) {
        if !self.busy && !self.picking {
            self.picking = true;
            self.note = None;
            self.wants.push(Want::Pick);
        }
    }

    pub fn apply(&mut self, g: Got) {
        match g {
            Got::Tunnels(r) => {
                // A gateway asking: show what it asks (once per step).
                let asking = match &r {
                    Ok(v) => v.iter().find(|d| d.profile.tunnel.state == State::SignIn).map(|d| d.profile.tunnel.name.clone()),
                    Err(_) => None,
                };
                match asking {
                    Some(n) if self.asking.as_deref() != Some(n.as_str()) && !self.busy => {
                        self.asking = Some(n.clone());
                        self.wants.push(Want::Prompt(n));
                    }
                    None if self.signin.is_some() && !self.busy => {
                        self.signin = None;
                        self.asking = None;
                        self.armed = None;
                        self.reset_field = true;
                    }
                    None => self.asking = None,
                    _ => {}
                }
                self.tunnels = Some(r);
            }
            Got::Prompt(name, r) => match r {
                Ok(Some(ask)) => {
                    let chosen = match &ask {
                        Ask::Form(f) => vec![0; f.fields.len()],
                        Ask::Sso { .. } => Vec::new(),
                    };
                    if let Ask::Sso { login, .. } = &ask {
                        eprintln!("curia: vpn: {name} signs in at {login}");
                    }
                    self.signin = Some(SignIn { name, ask, chosen, keep: false });
                    self.handle = None;
                    self.armed = None;
                    self.reset_field = true;
                }
                Ok(None) => self.asking = None,
                Err(e) => {
                    self.asking = None;
                    self.note = Some((true, e));
                }
            },
            Got::CertRequest(r) => {
                self.busy = false;
                match r {
                    Ok(c) => {
                        self.note = Some((false, format!("A certificate request for {}: give it to your IT team", c.0)));
                        self.csr = Some(c);
                    }
                    Err(e) => self.note = Some((true, e)),
                }
                self.refresh();
            }
            Got::Personas(r) => self.personas = Some(r),
            Got::Done(r) => {
                self.busy = false;
                self.note = Some(match r {
                    Ok(m) => (false, m),
                    Err(m) => (true, m),
                });
                self.refresh();
            }
            Got::Created(r) => {
                self.busy = false;
                match r {
                    Ok(c) => {
                        let ignored = if c.ignored.is_empty() { String::new() } else { format!(" (left out of the file: {})", c.ignored.join(", ")) };
                        self.note = Some((false, format!("{} added{ignored}", c.name)));
                        self.creating = false;
                        self.made = Some(c);
                    }
                    Err(e) => self.note = Some((true, format!("Not added: {e}"))),
                }
                self.refresh();
            }
            Got::Picked(r) => {
                self.picking = false;
                match r {
                    Ok((_, pem)) if self.pick_cert.is_some() => {
                        let name = self.pick_cert.take().unwrap_or_default();
                        self.csr = None;
                        self.change(Want::Certificate(Certificate { name, pem }));
                    }
                    Ok((file, conf)) => {
                        let name = name_from_file(&file);
                        self.change(Want::Import(ImportWireGuard { name, conf }));
                    }
                    Err(e) if e.is_empty() => self.pick_cert = None,
                    Err(e) => {
                        self.pick_cert = None;
                        self.note = Some((true, e));
                    }
                }
            }
        }
    }
}

fn sock_dir() -> PathBuf {
    std::env::var_os("CURIA_SOCKETS").map(PathBuf::from).unwrap_or_else(|| "/atrium/sockets".into())
}

/// portcullisd: the person's personas, and the routes of those with a VPN.
fn personas() -> Result<(Vec<String>, Vec<(String, String)>), String> {
    use portcullis_ipc::{round_trip, Request, Response, PROTO_VERSION};
    let ask = |req: &Request| -> Result<Response, String> {
        let path = portcullis_ipc::resolve_socket_path().ok_or("portcullisd is not running")?;
        let mut s = std::os::unix::net::UnixStream::connect(path).map_err(|e| format!("portcullisd: {e}"))?;
        s.set_read_timeout(Some(std::time::Duration::from_secs(10))).map_err(|e| e.to_string())?;
        match round_trip(&mut s, &Request::Hello { version: PROTO_VERSION }) {
            Ok(Response::Hello { version }) if version == PROTO_VERSION => {}
            Ok(other) => return Err(format!("portcullisd: {other:?}")),
            Err(e) => return Err(format!("portcullisd is not answering ({e})")),
        }
        round_trip(&mut s, req).map_err(|e| format!("portcullisd is not answering ({e})"))
    };
    let named = match ask(&Request::Crossings)? {
        Response::Crossings { personas, .. } => personas.into_iter().map(|(n, _)| n).collect(),
        Response::Error { message } => return Err(message),
        other => return Err(format!("unexpected reply: {other:?}")),
    };
    let routes = match ask(&Request::PersonaVpns)? {
        Response::PersonaVpns { routes } => routes,
        Response::Error { message } => return Err(message),
        other => return Err(format!("unexpected reply: {other:?}")),
    };
    Ok((named, routes))
}

fn set_persona_vpn(persona: &str, vpn: Option<&str>, activation: [u8; 16]) -> Result<(), String> {
    use portcullis_ipc::{round_trip, Request, Response, PROTO_VERSION};
    let path = portcullis_ipc::resolve_socket_path().ok_or("portcullisd is not running")?;
    let mut s = std::os::unix::net::UnixStream::connect(path).map_err(|e| format!("portcullisd: {e}"))?;
    s.set_read_timeout(Some(std::time::Duration::from_secs(10))).map_err(|e| e.to_string())?;
    match round_trip(&mut s, &Request::Hello { version: PROTO_VERSION }) {
        Ok(Response::Hello { version }) if version == PROTO_VERSION => {}
        _ => return Err("portcullisd is not answering".into()),
    }
    match round_trip(&mut s, &Request::SetPersonaVpn { name: persona.into(), vpn: vpn.map(String::from), activation }) {
        Ok(Response::Ok) => Ok(()),
        Ok(Response::Error { message }) => Err(message),
        Ok(other) => Err(format!("unexpected reply: {other:?}")),
        Err(e) => Err(format!("portcullisd is not answering ({e})")),
    }
}

/// portcullisd's `VpnSignInWindow` (vpn.md §9.3): the person's Navigator
/// in its reserved `vpn` persona, at the gateway's sign-on address.
fn sso_window(name: &str, login: &str, final_url: &str, activation: Option<[u8; 16]>) -> Result<(), String> {
    use portcullis_ipc::{round_trip, Request, Response, PROTO_VERSION};
    let path = portcullis_ipc::resolve_socket_path().ok_or("portcullisd is not running")?;
    let mut s = std::os::unix::net::UnixStream::connect(path).map_err(|e| format!("portcullisd: {e}"))?;
    s.set_read_timeout(Some(std::time::Duration::from_secs(10))).map_err(|e| e.to_string())?;
    match round_trip(&mut s, &Request::Hello { version: PROTO_VERSION }) {
        Ok(Response::Hello { version }) if version == PROTO_VERSION => {}
        _ => return Err("portcullisd is not answering".into()),
    }
    let r = Request::VpnSignInWindow { profile: name.into(), start_url: login.into(), final_url: final_url.into(), activation };
    match round_trip(&mut s, &r) {
        Ok(Response::Ok) => Ok(()),
        Ok(Response::Error { message }) => Err(message),
        Ok(other) => Err(format!("unexpected reply: {other:?}")),
        Err(e) => Err(format!("portcullisd is not answering ({e})")),
    }
}

/// One ask, on a thread of its own.
pub fn work(w: Want, tx: Sender<Got>) {
    std::thread::spawn(move || {
        let connect = || Client::connect(&Client::path(&sock_dir(), Grant::Configure)).map_err(|_| "the VPN service is not running".to_string());
        let say = |r: Result<String, String>| {
            match &r {
                Ok(m) => eprintln!("curia: vpn: {m}"),
                Err(e) => eprintln!("curia: vpn: failed: {e}"),
            }
            let _ = tx.send(Got::Done(r));
        };
        let fail = |what: String| move |e: crypta_ctl::Fail| format!("{what}: {}", e.1);
        match w {
            Want::Refresh => {
                let r = connect().and_then(|mut c| {
                    let names = c.list().map_err(|e| e.1)?;
                    names.iter().map(|p| c.details(&p.tunnel.name).map_err(|e| e.1)).collect::<Result<Vec<_>, _>>()
                });
                let _ = tx.send(Got::Tunnels(r));
                let _ = tx.send(Got::Personas(personas()));
            }
            Want::Connect(n) => say(connect().and_then(|mut c| c.connect_tunnel(&n).map_err(fail(format!("connect {n}")))).map(|_| format!("{n} connecting"))),
            Want::Disconnect(n) => say(connect().and_then(|mut c| c.disconnect_tunnel(&n).map_err(fail(format!("disconnect {n}")))).map(|_| format!("{n} disconnected"))),
            Want::Set(s) => {
                let what = match (s.block, s.personas) {
                    (Some(true), _) => format!("{} blocks traffic when not connected", s.name),
                    (Some(false), _) => format!("{} lets traffic go direct when not connected", s.name),
                    (_, Some(true)) => format!("{} is for chosen personas", s.name),
                    _ => format!("{} is for the whole computer", s.name),
                };
                say(connect().and_then(|mut c| c.set(&s).map_err(fail(s.name.clone()))).map(|_| format!("{what}; from its next connect")))
            }
            Want::Remove(n) => say(connect().and_then(|mut c| c.remove(&n).map_err(fail(format!("remove {n}")))).map(|_| format!("removed {n}"))),
            Want::Create(cw) => {
                let r = connect().and_then(|mut c| c.create(&cw).map_err(|e| e.1));
                match &r {
                    Ok(c) => eprintln!("curia: vpn: created {} (key {})", c.name, c.public),
                    Err(e) => eprintln!("curia: vpn: create {} failed: {e}", cw.name),
                }
                let _ = tx.send(Got::Created(r));
            }
            Want::Import(i) => {
                let name = i.name.clone();
                let r = connect().and_then(|mut c| c.import(&i).map_err(|e| e.1));
                match &r {
                    Ok(c) => eprintln!("curia: vpn: imported {} (key {})", c.name, c.public),
                    Err(e) => eprintln!("curia: vpn: import {name} failed: {e}"),
                }
                let _ = tx.send(Got::Created(r));
            }
            Want::PersonaVpn { persona, vpn, activation } => {
                let who = persona_name(&persona);
                let r = match activation {
                    None => Err("press the button itself".to_string()),
                    Some(a) => set_persona_vpn(&persona, vpn.as_deref(), a),
                };
                say(r.map(|_| match &vpn {
                    Some(v) => format!("{who} goes through {v}"),
                    None => format!("{who} goes direct"),
                }));
            }
            Want::CreateAc(ac) => {
                let n = ac.name.clone();
                let r = connect().and_then(|mut c| c.create_ac(&ac).map_err(|e| e.1));
                match &r {
                    Ok(_) => eprintln!("curia: vpn: created {n} (AnyConnect, {})", ac.host),
                    Err(e) => eprintln!("curia: vpn: create {n} failed: {e}"),
                }
                let _ = tx.send(Got::Created(r.map(|name| Created { name, public: String::new(), ignored: vec![] })));
            }
            Want::Prompt(n) => {
                let r = connect().and_then(|mut c| c.prompt(&n).map_err(|e| e.1));
                if let Ok(Some(Ask::Form(f))) = &r {
                    eprintln!("curia: vpn: {n} asks {} field(s): {}", f.fields.len(), f.fields.iter().map(|f| format!("{:?}", f.kind)).collect::<Vec<_>>().join(","));
                }
                let _ = tx.send(Got::Prompt(n, r));
            }
            Want::Answer(a) => {
                let n = a.name.clone();
                let what = if a.values.is_some() { format!("{n}: signing in") } else { format!("{n}: sign-in cancelled") };
                say(connect().and_then(|mut c| c.answer(&a).map_err(fail(n))).map(|_| what))
            }
            Want::SsoDone(s) => {
                let n = s.name.clone();
                say(connect().and_then(|mut c| c.sso_done(&s).map_err(fail(n.clone()))).map(|_| format!("{n}: single sign-on ended")))
            }
            Want::SsoWindow { name, login, final_url, activation } => {
                say(sso_window(&name, &login, &final_url, activation).map(|_| format!("{name}: the sign-in window is open")))
            }
            Want::CertRequest(n) => {
                let r = connect().and_then(|mut c| c.cert_request(&CertRequest { name: n.clone(), cn: None }).map_err(|e| e.1));
                match &r {
                    Ok(_) => eprintln!("curia: vpn: certificate request for {n}"),
                    Err(e) => eprintln!("curia: vpn: certificate request for {n} failed: {e}"),
                }
                let _ = tx.send(Got::CertRequest(r.map(|p| (n, p))));
            }
            Want::Certificate(c) => {
                let n = c.name.clone();
                say(connect().and_then(|mut cl| cl.certificate(&c).map_err(fail(format!("{n}'s certificate")))).map(|_| format!("{n}: certificate added")))
            }
            Want::Pick => {}
        }
    });
}

/// A gateway's form (or its single sign-on), under the tunnel's card;
/// returns the y below it.
#[allow(clippy::too_many_arguments)]
fn signin_panel(ctx: &mut Ctx, page: &Mutable<VpnPage>, fields: &[Mutable<TextEdit>; 14], vcred: &pergola::widgets::CredentialField, p: &VpnPage, si: &SignIn, pw: f32, y0: f32) -> f32 {
    let t = ctx.theme;
    let mut y = y0;
    let f = match &si.ask {
        Ask::Sso { login, .. } => {
            let h = em(96.0);
            rect(ctx, 0.0, y, pw, h, t.bg_surface(), 8.0);
            text(ctx, em(16.0), y + em(22.0), &format!("{} signs in with your organization's account", si.name), type_size::MD, Weight::Medium, t.text_primary());
            let host = login.split('/').nth(2).unwrap_or(login);
            text(ctx, em(16.0), y + em(44.0), &format!("at {host}, in a sign-in window of its own"), type_size::SM, Weight::Regular, t.text_secondary());
            let pg = page.clone();
            let w = button(ctx, em(16.0), y + em(56.0), "Sign in\u{2026}", Look::Primary, !p.busy, move || {
                let token = pergola::clipboard::current_activation();
                with_vpn(&pg, |a| a.sso_window(token))
            });
            let pg = page.clone();
            button(ctx, em(24.0) + w, y + em(56.0), "Cancel", Look::Secondary, !p.busy, move || with_vpn(&pg, |a| a.cancel_signin()));
            return y + h + em(8.0);
        }
        Ask::Form(f) => f,
    };
    let reviewing_hip = f.fields.iter().any(|x| x.kind == FieldKind::HipApproval);
    let lines = |value: &str, limit| {
        let width = ((pw - em(32.0)) / em(12.0)).max(1.0) as usize;
        let mut lines = Vec::new();
        for line in value.lines().filter(|l| !l.trim().is_empty()).take(limit) {
            if reviewing_hip {
                let chars: Vec<_> = line.chars().collect();
                lines.extend(chars.chunks(width).map(|chunk| chunk.iter().collect::<String>()));
            } else {
                lines.push(line.chars().take(120).collect());
            }
        }
        lines
    };
    let msg = lines(&f.message, 4);
    let banner = lines(&f.banner, if reviewing_hip { 16 } else { 8 });
    let choosing_gateway = f.fields.iter().any(|x| x.kind == FieldKind::GatewayChoice);
    let fh: f32 = f.fields.iter().map(|x| {
        if x.kind == FieldKind::HipApproval {
            0.0
        } else if x.kind == FieldKind::GatewayChoice {
            em(34.0) * x.choices.len().max(1) as f32
        } else if x.kind == FieldKind::Choice {
            em(48.0)
        } else {
            em(44.0)
        }
    }).sum();
    let h = em(36.0) + em(18.0) * (msg.len() + banner.len()) as f32 + if f.error.is_empty() { 0.0 } else { em(22.0) } + fh + em(52.0) + if si.has_password() { em(32.0) } else { 0.0 };
    rect(ctx, 0.0, y, pw, h, t.bg_surface(), 8.0);
    let title = if f.title.trim().is_empty() { format!("Sign in to {}", si.name) } else { format!("{}: {}", si.name, f.title.trim()) };
    text(ctx, em(16.0), y + em(22.0), &title, type_size::MD, Weight::Medium, t.text_primary());
    y += em(36.0);
    for l in banner.iter().chain(msg.iter()) {
        text(ctx, em(16.0), y + em(8.0), l, type_size::SM, Weight::Regular, t.text_secondary());
        y += em(18.0);
    }
    if !f.error.is_empty() {
        text(ctx, em(16.0), y + em(10.0), &f.error.chars().take(120).collect::<String>(), type_size::SM, Weight::Regular, t.danger_fg());
        y += em(22.0);
    }
    let lx = em(16.0);
    let fx = em(160.0);
    let fw = (pw - fx - em(16.0)).clamp(em(160.0), em(320.0));
    let mut ti = 0;
    for (i, fl) in f.fields.iter().enumerate() {
        if fl.kind == FieldKind::HipApproval {
            continue;
        }
        let label = fl.label.trim().trim_end_matches(':');
        text(ctx, lx, y + em(18.0), &label.chars().take(24).collect::<String>(), type_size::SM, Weight::Regular, t.text_secondary());
        match fl.kind {
            FieldKind::Text => {
                if ti < MAX_TEXTS {
                    pergola::widgets::TextField::new(fields[ANSWER_AT + ti].clone()).at(fx, y + em(2.0)).width(fw).render(ctx);
                }
                ti += 1;
                y += em(44.0);
            }
            FieldKind::Password | FieldKind::PasswordOnce | FieldKind::Code => {
                vcred.clone().at(fx, y + em(2.0)).size(fw, em(32.0)).render(ctx);
                if !p.field_up {
                    text(ctx, fx + fw + em(12.0), y + em(18.0), "Arca's field is not here yet", type_size::XS, Weight::Regular, t.text_tertiary());
                }
                y += em(44.0);
            }
            FieldKind::HipApproval => unreachable!("approval uses the form action"),
            FieldKind::GatewayChoice => {
                for (choice, (_, label)) in fl.choices.iter().enumerate() {
                    let on = si.chosen.get(i).copied() == Some(choice);
                    let pg = page.clone();
                    button(ctx, fx, y + em(2.0), label, if on { Look::Primary } else { Look::Secondary },
                        !p.busy, move || with_vpn(&pg, |a| a.choose(i, choice)));
                    y += em(34.0);
                }
                if fl.choices.is_empty() {
                    y += em(34.0);
                }
            }
            FieldKind::Choice => {
                let mut bx = fx;
                for (c, (_, cl)) in fl.choices.iter().enumerate().take(6) {
                    let on = si.chosen.get(i).copied() == Some(c);
                    let pg = page.clone();
                    bx += button(ctx, bx, y + em(2.0), &format!("{}{}", cl, if on { " \u{2713}" } else { "" }), if on { Look::Primary } else { Look::Secondary }, !p.busy, move || with_vpn(&pg, |a| a.choose(i, c))) + em(8.0);
                }
                y += em(48.0);
            }
        }
    }
    if si.has_password() {
        let pg = page.clone();
        button(ctx, fx, y, if si.keep { "Save the password in Arca: yes" } else { "Save the password in Arca: no" }, Look::Ghost, !p.busy, move || with_vpn(&pg, |a| a.toggle_keep()));
        y += em(32.0);
    }
    let ready = !p.busy && p.armed.is_none();
    let (pg, fs, n) = (page.clone(), fields.clone(), f.fields.iter().filter(|x| x.kind == FieldKind::Text).count().min(MAX_TEXTS));
    let w = button(ctx, fx, y + em(8.0), if p.armed.is_some() { "Signing in…" } else if reviewing_hip { "Send device report" } else if choosing_gateway { "Connect" } else { "Sign in" }, Look::Primary, ready, move || {
        let texts: Vec<String> = (0..n).map(|k| fs[ANSWER_AT + k].get_cloned().text().to_string()).collect();
        with_vpn(&pg, |a| a.sign_in(texts, std::time::Instant::now()));
    });
    let pg = page.clone();
    button(ctx, fx + w + em(8.0), y + em(8.0), "Cancel", Look::Secondary, !p.busy, move || with_vpn(&pg, |a| a.cancel_signin()));
    y0 + h + em(8.0)
}

pub fn with_vpn(page: &Mutable<VpnPage>, f: impl FnOnce(&mut VpnPage)) {
    let mut p = page.get_cloned();
    f(&mut p);
    page.set(p);
}

/// The row buttons' slots, left to right from the row's left edge.

impl CuriaView {
    /// The VPN page. Each tunnel is a card: its name, how it is, its
    /// server; Connect, the block switch, whom it is for and Remove in fixed
    /// slots; for chosen personas, a press per persona.
    pub(crate) fn vpn(&self, ctx: &mut Ctx, x0: f32, pw: f32, h: f32) {
        let p = self.vpn.get_cloned();
        let t = ctx.theme;
        let y = em(20.0);
        text(ctx, x0, y + em(16.0), "VPN", type_size::XL, Weight::Semibold, t.text_primary());
        let top = y + em(48.0);
        let page = self.vpn.clone();
        let picking = p.picking;
        let fields: [Mutable<TextEdit>; 14] = self.vpn_fields.clone();
        let vcred = self.vpn_cred.clone();
        let content = pergola::view::Draw(move |ctx: &mut Ctx| {
            let t = ctx.theme;
            let g = ctx.push(Node::stack(pergola::geom::Axis::Vertical, Rect::new(0.0, 0.0, pw, 0.0), 0.0));
            ctx.set_flex(g, FlexStyle { free: true, ..FlexStyle::default() });
            let mut y = 0.0;
            match (&p.note, p.busy) {
                (_, true) => {
                    text(ctx, 0.0, y + em(12.0), "Working…", type_size::SM, Weight::Regular, t.text_tertiary());
                }
                (Some((failed, m)), _) => {
                    text(ctx, 0.0, y + em(12.0), m, type_size::SM, Weight::Regular, if *failed { t.danger_fg() } else { t.text_secondary() });
                }
                _ => {}
            }
            y += em(28.0);
            if let Some(c) = &p.made {
                let mh = em(76.0);
                rect(ctx, 0.0, y, pw, mh, t.bg_surface(), 8.0);
                text(ctx, em(16.0), y + em(22.0), &format!("Give the server's admin {}'s public key:", c.name), type_size::SM, Weight::Regular, t.text_secondary());
                text(ctx, em(16.0), y + em(48.0), &c.public, type_size::MD, Weight::Medium, t.text_primary());
                let pg = page.clone();
                button(ctx, pw - em(100.0), y + em(34.0), "Done", Look::Ghost, true, move || with_vpn(&pg, |a| a.made = None));
                y += mh + em(12.0);
            }
            let list = match &p.tunnels {
                None => {
                    text(ctx, 0.0, y + em(10.0), "Asking for your tunnels…", type_size::SM, Weight::Regular, t.text_tertiary());
                    Vec::new()
                }
                Some(Err(why)) => {
                    // curia.md §10: say so, offer nothing, and keep asking.
                    text(ctx, 0.0, y + em(10.0), &format!("VPN settings are not available: {why}"), type_size::SM, Weight::Regular, t.text_secondary());
                    y += em(28.0);
                    if let Some(Node::Stack { rect, .. }) = ctx.tree.get_mut(g) {
                        rect.size.h = y;
                    }
                    ctx.pop();
                    return;
                }
                Some(Ok(v)) if v.is_empty() => {
                    text(ctx, 0.0, y + em(10.0), "No tunnels yet. Add one below.", type_size::SM, Weight::Regular, t.text_secondary());
                    Vec::new()
                }
                Some(Ok(v)) => v.clone(),
            };
            if list.is_empty() {
                y += em(28.0);
            }
            let named: Vec<String> = match &p.personas {
                Some(Ok((n, _))) => n.clone(),
                _ => Vec::new(),
            };
            if let Some((n, pem)) = &p.csr {
                let lines: Vec<&str> = pem.lines().collect();
                let mh = em(64.0) + em(13.0) * lines.len() as f32 + em(44.0);
                rect(ctx, 0.0, y, pw, mh, t.bg_surface(), 8.0);
                text(ctx, em(16.0), y + em(22.0), &format!("{n}: give your IT team this certificate request, then import what they sign."), type_size::SM, Weight::Regular, t.text_secondary());
                text(ctx, em(16.0), y + em(42.0), "The key is in Arca and never leaves it.", type_size::XS, Weight::Regular, t.text_tertiary());
                for (i, l) in lines.iter().enumerate() {
                    text(ctx, em(16.0), y + em(64.0) + em(13.0) * i as f32, l, type_size::XS, Weight::Regular, t.text_primary());
                }
                let by = y + mh - em(40.0);
                let pem2 = pem.clone();
                let mut bx = em(16.0);
                bx += button(ctx, bx, by, "Copy the request", Look::Secondary, true, move || {
                    let _ = pergola::clipboard::copy_text(&pem2, false);
                });
                let (pg, n2) = (page.clone(), n.clone());
                bx += em(8.0);
                bx += button(ctx, bx, by, "Import the signed certificate…", Look::Primary, !p.busy && !p.picking, move || with_vpn(&pg, |a| a.pick_certificate(&n2)));
                let pg = page.clone();
                button(ctx, bx + em(8.0), by, "Later", Look::Ghost, true, move || with_vpn(&pg, |a| a.csr = None));
                y += mh + em(12.0);
            }
            for d in &list {
                let name = d.profile.tunnel.name.clone();
                let chosen = d.profile.tunnel.scope == Scope::Personas;
                let corporate = d.profile.tunnel.kind != Kind::WireGuard;
                let err_h = if d.error.is_some() && d.profile.tunnel.state == State::Down { em(20.0) } else { 0.0 };
                let rh = em(if chosen { 156.0 } else { 116.0 }) + if corporate { em(40.0) } else { 0.0 } + err_h;
                rect(ctx, 0.0, y, pw, rh, t.bg_surface(), 8.0);
                let up = d.profile.tunnel.state != State::Down;
                let colour = match d.profile.tunnel.state {
                    State::Up => t.success_fg(),
                    State::Connecting | State::SignIn => t.warning_fg(),
                    State::Down => t.text_tertiary(),
                };
                icon(ctx, em(16.0), y + em(14.0), ph::LOCK_SIMPLE, em(20.0), colour);
                text(ctx, em(48.0), y + em(24.0), &name, type_size::MD, Weight::Medium, t.text_primary());
                text(ctx, em(48.0), y + em(44.0), &describe(d), type_size::SM, Weight::Regular, t.text_secondary());
                text(ctx, em(48.0), y + em(62.0), &detail_line(d), type_size::XS, Weight::Regular, t.text_tertiary());
                let by = y + em(74.0);
                let enabled = !p.busy;
                let (pg, n) = (page.clone(), name.clone());
                let mut bx = em(16.0);
                bx += button(ctx, bx, by, if up { "Disconnect" } else { "Connect" }, if up { Look::Secondary } else { Look::Primary }, enabled, move || with_vpn(&pg, |a| a.connect(&n, !up)));
                let (pg, n, block) = (page.clone(), name.clone(), d.profile.block);
                bx += em(8.0);
                bx += button(ctx, bx, by, if block { "Block when off: on" } else { "Block when off: off" }, Look::Secondary, enabled, move || with_vpn(&pg, |a| a.set_block(&n, !block)));
                let (pg, n) = (page.clone(), name.clone());
                bx += em(8.0);
                bx += button(ctx, bx, by, if chosen { "For: chosen personas" } else { "For: whole computer" }, Look::Secondary, enabled, move || with_vpn(&pg, |a| a.set_scope(&n, !chosen)));
                let pg = page.clone();
                let confirm = p.confirm_remove.as_deref() == Some(name.as_str());
                let (n2, rx) = (name.clone(), bx + em(8.0));
                button(ctx, rx, by, if confirm { "Remove" } else { "Remove…" }, if confirm { Look::Danger } else { Look::Secondary }, enabled, move || with_vpn(&pg, |a| a.remove(&n2)));
                let mut ey = y + em(116.0);
                if corporate {
                    let cert = d.corporate.as_ref().is_some_and(|c| c.certificate);
                    let (pg, n) = (page.clone(), name.clone());
                    let w = button(ctx, em(16.0), ey, if cert { "New client certificate…" } else { "Client certificate…" }, Look::Secondary, enabled, move || with_vpn(&pg, |a| a.cert_request(&n)));
                    text(ctx, em(28.0) + w, ey + em(15.0), if cert { "Signs in with its certificate (key in Arca)" } else { "For gateways that ask for one" }, type_size::XS, Weight::Regular, t.text_tertiary());
                    ey += em(40.0);
                }
                if chosen {
                    let py = ey;
                    text(ctx, em(16.0), py + em(14.0), "Personas:", type_size::SM, Weight::Regular, t.text_secondary());
                    let mut px = em(96.0);
                    if named.is_empty() {
                        text(ctx, px, py + em(14.0), "none yet: add one on the Personas page", type_size::SM, Weight::Regular, t.text_tertiary());
                    }
                    for persona in &named {
                        let on = p.route_of(persona) == Some(name.as_str());
                        let label = format!("{}{}", persona_name(persona), if on { " \u{2713}" } else { "" });
                        let (pg, pn, n) = (page.clone(), persona.clone(), name.clone());
                        let w = button(ctx, px, py, &label, if on { Look::Primary } else { Look::Secondary }, enabled, move || {
                            let token = pergola::clipboard::current_activation();
                            with_vpn(&pg, |a| a.toggle_persona(&pn, &n, token))
                        });
                        px += w + em(8.0);
                    }
                    ey += em(40.0);
                }
                if err_h > 0.0 {
                    if let Some(e) = &d.error {
                        text(ctx, em(16.0), ey + em(8.0), &format!("Stopped: {}", e.chars().take(140).collect::<String>()), type_size::SM, Weight::Regular, t.danger_fg());
                    }
                }
                y += rh + em(8.0);
                // What the gateway said once signed in (vpn.md §9.4).
                if let (Some(b), State::Up) = (&d.banner, d.profile.tunnel.state) {
                    let lines: Vec<String> = b.lines().filter(|l| !l.trim().is_empty()).take(8).map(|l| l.chars().take(120).collect()).collect();
                    let bh = em(28.0) + em(16.0) * lines.len() as f32;
                    rect(ctx, 0.0, y, pw, bh, t.bg_surface(), 8.0);
                    text(ctx, em(16.0), y + em(16.0), &format!("{name}'s gateway says:"), type_size::XS, Weight::Regular, t.text_tertiary());
                    for (i, l) in lines.iter().enumerate() {
                        text(ctx, em(16.0), y + em(34.0) + em(16.0) * i as f32, l, type_size::SM, Weight::Regular, t.text_secondary());
                    }
                    y += bh + em(8.0);
                }
                // What its gateway asks, here under its card.
                if let Some(si) = p.signin.as_ref().filter(|s| s.name == name) {
                    y = signin_panel(ctx, &page, &fields, &vcred, &p, si, pw, y);
                }
            }

            // A new tunnel: a file, or a key made here.
            y += em(12.0);
            text(ctx, 0.0, y + em(14.0), "Add a tunnel", type_size::LG, Weight::Semibold, t.text_primary());
            y += em(36.0);
            let pg = page.clone();
            let w = button(ctx, 0.0, y, if p.picking { "Choosing a file…" } else { "Import a WireGuard file…" }, Look::Secondary, !p.busy && !p.picking, move || with_vpn(&pg, |a| a.pick()));
            let pg = page.clone();
            let w2 = button(ctx, w + em(12.0), y, "Make a new key…", Look::Secondary, !p.busy, move || with_vpn(&pg, |a| a.creating = !a.creating));
            let pg = page.clone();
            button(ctx, w + w2 + em(24.0), y, "Add a corporate VPN…", Look::Secondary, !p.busy, move || with_vpn(&pg, |a| a.creating_ac = !a.creating_ac));
            y += em(44.0);
            if p.creating_ac {
                text(ctx, 0.0, y + em(10.0), "A Cisco AnyConnect (or compatible) gateway. It signs you in when you connect.", type_size::SM, Weight::Regular, t.text_secondary());
                y += em(28.0);
                for (i, f) in fields[AC_AT..AC_AT + 4].iter().enumerate() {
                    pergola::widgets::TextField::new(f.clone()).placeholder(AC_FIELDS[i]).at(0.0, y).width(em(420.0)).render(ctx);
                    y += em(40.0);
                }
                let (pg, fs) = (page.clone(), fields.clone());
                button(ctx, 0.0, y, "Add corporate VPN", Look::Primary, !p.busy, move || {
                    let v: Vec<String> = (AC_AT..AC_AT + 4).map(|i| fs[i].get_cloned().text().to_string()).collect();
                    with_vpn(&pg, |a| a.create_ac(&v));
                });
                text(ctx, em(150.0), y + em(15.0), "For your work personas; change it on its card.", type_size::XS, Weight::Regular, t.text_tertiary());
                y += em(44.0);
            }
            if p.creating {
                for (i, f) in fields.iter().enumerate() {
                    pergola::widgets::TextField::new(f.clone()).placeholder(FIELDS[i]).at(0.0, y).width(em(420.0)).render(ctx);
                    y += em(40.0);
                }
                let (pg, fs) = (page.clone(), fields.clone());
                button(ctx, 0.0, y, "Add tunnel", Look::Primary, !p.busy, move || {
                    let v: [String; 6] = std::array::from_fn(|i| fs[i].get_cloned().text().to_string());
                    with_vpn(&pg, |a| a.create(&v));
                });
                text(ctx, em(120.0), y + em(15.0), "The key is made in Arca and never leaves it.", type_size::XS, Weight::Regular, t.text_tertiary());
                y += em(44.0);
            }
            y += em(16.0);
            if let Some(Node::Stack { rect, .. }) = ctx.tree.get_mut(g) {
                rect.size.h = y;
            }
            ctx.pop();
        });
        ScrollView::new(self.vpn_scroll.clone(), Rect::new(x0, top, pw, (h - top - em(8.0)).max(em(60.0))), content)
            .key(98)
            .backdrop(t.bg_canvas())
            .emit(ctx);
        if picking {
            let (w, h) = self.size.get();
            let id = ctx.add(Node::Rect { rect: Rect::new(0.0, 0.0, w, h), fill: Color::TRANSPARENT, radius: 0.0 });
            ctx.slot(id, PICK_SLOT);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crypta_ctl::{Kind, Profile, Tunnel};

    const KEY: &str = "xTIBA5rboUvnH4htodjb6e697QjLERt1NAB4mZqp8Dg=";

    fn details(name: &str, state: State, scope: Scope) -> Details {
        Details {
            profile: Profile { tunnel: Tunnel { name: name.into(), kind: Kind::WireGuard, state, scope }, block: true, full: true },
            public: KEY.into(),
            endpoint: "203.0.113.5:51820".parse().unwrap(),
            addrs: vec![Net::parse("10.7.0.2/32").unwrap()],
            routes: 1,
            dns: vec![],
            rx: 0,
            tx: 0,
            starts: 0,
            corporate: None,
            error: None,
            banner: None,
        }
    }

    #[test]
    fn the_form_parses_and_checks() {
        let mut f: [String; 6] = ["Acme".into(), "203.0.113.5:51820".into(), KEY.into(), "10.7.0.2/32".into(), "".into(), "10.7.0.1".into()];
        let c = parse_create(&f).unwrap();
        assert_eq!((c.name.as_str(), c.allowed[0].prefix, c.dns.len()), ("acme", 0, 1), "empty networks: everything");
        f[4] = "10.0.0.0/8, 192.168.50.0/24".into();
        assert_eq!(parse_create(&f).unwrap().allowed.len(), 2);
        f[1] = "vpn.example.com:51820".into();
        assert!(parse_create(&f).unwrap_err().contains("IPv4 address and port"));
        f[1] = "203.0.113.5:51820".into();
        f[2] = "nope".into();
        assert!(parse_create(&f).unwrap_err().contains("key"));
    }

    #[test]
    fn file_names_become_tunnel_names() {
        assert_eq!(name_from_file("Acme VPN.conf"), "acme-vpn");
        assert_eq!(name_from_file("wg0.conf"), "wg0");
        assert_eq!(name_from_file("...conf"), "vpn");
        assert_eq!(name_from_file(&format!("{}.conf", "a".repeat(40))).len(), 32);
    }

    #[test]
    fn changes_one_at_a_time_and_remove_asks() {
        let mut p = VpnPage::default();
        p.remove("acme");
        assert!(p.take_wants().is_empty());
        p.remove("acme");
        assert_eq!(p.take_wants(), vec![Want::Remove("acme".into())]);
        p.connect("acme", true);
        assert!(p.take_wants().is_empty(), "busy");
        p.apply(Got::Done(Ok("removed acme".into())));
        assert_eq!(p.take_wants(), vec![Want::Refresh]);
        p.apply(Got::Personas(Ok((vec!["work".into(), "school".into()], vec![("work".into(), "acme".into())]))));
        p.toggle_persona("work", "acme", Some([1; 16]));
        assert_eq!(p.take_wants(), vec![Want::PersonaVpn { persona: "work".into(), vpn: None, activation: Some([1; 16]) }], "on: off");
        p.apply(Got::Done(Ok("Work goes direct".into())));
        p.take_wants();
        p.toggle_persona("school", "acme", Some([2; 16]));
        assert_eq!(p.take_wants(), vec![Want::PersonaVpn { persona: "school".into(), vpn: Some("acme".into()), activation: Some([2; 16]) }]);
        p.apply(Got::Done(Err("Lyra is open in school: close it first".into())));
        assert_eq!(p.note, Some((true, "Lyra is open in school: close it first".into())));
    }

    #[test]
    fn a_pick_imports_and_a_new_key_is_shown() {
        let mut p = VpnPage::default();
        p.pick();
        assert_eq!(p.take_wants(), vec![Want::Pick]);
        p.apply(Got::Picked(Ok(("Acme.conf".into(), b"[Interface]".to_vec()))));
        assert_eq!(p.take_wants(), vec![Want::Import(ImportWireGuard { name: "acme".into(), conf: b"[Interface]".to_vec() })]);
        p.apply(Got::Created(Ok(Created { name: "acme".into(), public: KEY.into(), ignored: vec!["PostUp".into()] })));
        assert_eq!(p.made.as_ref().unwrap().public, KEY);
        assert_eq!(p.note, Some((false, "acme added (left out of the file: PostUp)".into())));
        p.pick();
        p.take_wants();
        p.apply(Got::Picked(Err(String::new())));
        assert!(!p.picking && p.note.is_none(), "a cancel says nothing");
    }

    fn corp(state: State) -> Details {
        let mut d = details("corp", state, Scope::Personas);
        d.profile.tunnel.kind = Kind::AnyConnect;
        d.corporate = Some(crypta_ctl::Corporate { host: "vpn.example.com".into(), port: 443, user: Some("alice".into()), group: None, certificate: true, pinned: false });
        d
    }

    fn form() -> Ask {
        use crypta_ctl::{Field, Form};
        let f = |label: &str, kind, choices: Vec<(String, String)>| Field { label: label.into(), kind, choices };
        Ask::Form(Form {
            title: "Login".into(),
            message: String::new(),
            error: String::new(),
            banner: String::new(),
            fields: vec![
                f("Username:", FieldKind::Text, vec![]),
                f("GROUP:", FieldKind::Choice, vec![("Staff".into(), "Staff".into()), ("Eng".into(), "Engineering".into())]),
                f("Password:", FieldKind::Password, vec![]),
            ],
        })
    }

    #[test]
    fn a_single_sign_on_opens_its_window_with_the_press() {
        let mut p = VpnPage::default();
        p.apply(Got::Tunnels(Ok(vec![corp(State::SignIn)])));
        assert_eq!(p.take_wants(), vec![Want::Prompt("corp".into())]);
        let sso = Ask::Sso { login: "https://gw.example/+CSCOE+/saml/sp/login".into(), final_url: "https://gw.example/+CSCOE+/saml_ac_login.html".into() };
        p.apply(Got::Prompt("corp".into(), Ok(Some(sso))));
        assert!(!p.wants_field(), "no field: the window asks");
        p.sso_window(Some([3; 16]));
        assert_eq!(p.take_wants(), vec![Want::SsoWindow { name: "corp".into(), login: "https://gw.example/+CSCOE+/saml/sp/login".into(),
            final_url: "https://gw.example/+CSCOE+/saml_ac_login.html".into(), activation: Some([3; 16]) }]);
        p.apply(Got::Done(Ok(String::new())));
        p.take_wants();
        p.cancel_signin();
        assert_eq!(p.take_wants(), vec![Want::SsoDone(SsoDone { name: "corp".into(), ok: false })]);
    }

    #[test]
    fn a_corporate_vpn_is_added_and_signs_in_through_arca() {
        let f: Vec<String> = vec!["Work".into(), "https://VPN.example.com/".into(), "alice".into(), String::new()];
        let c = parse_create_ac(&f).unwrap();
        assert_eq!((c.name.as_str(), c.host.as_str(), c.user.as_deref(), c.personas), ("work", "vpn.example.com", Some("alice"), true));
        let mut bad = f.clone();
        bad[3] = "12:34".into();
        assert!(parse_create_ac(&bad).unwrap_err().contains("SHA-256"));
        assert!(parse_pin(&"ab:".repeat(32)[..95]).is_some());

        let mut p = VpnPage::default();
        p.apply(Got::Tunnels(Ok(vec![corp(State::SignIn)])));
        assert_eq!(p.take_wants(), vec![Want::Prompt("corp".into())]);
        p.apply(Got::Tunnels(Ok(vec![corp(State::SignIn)])));
        assert!(p.take_wants().is_empty(), "asked once per step");
        p.apply(Got::Prompt("corp".into(), Ok(Some(form()))));
        assert!(p.wants_field() && p.reset_field);
        p.choose(1, 1);
        p.toggle_keep();
        // Pressed before Arca's handle came: it waits for it.
        let now = std::time::Instant::now();
        p.sign_in(vec!["alice".into()], now);
        assert!(p.armed.is_some() && p.take_wants().is_empty());
        p.field(Some([7; 16]), now);
        assert_eq!(p.take_wants(), vec![Want::Answer(Answer { name: "corp".into(), values: Some(vec![Value::Text("alice".into()), Value::Text("Eng".into()), Value::Held([7; 16])]), keep: true })]);
        assert!(p.signin.is_none() && p.handle.is_none());
        p.apply(Got::Done(Ok("corp: signing in".into())));
        p.take_wants();
        // The next step (a code), then nothing typed: it says so.
        p.apply(Got::Tunnels(Ok(vec![corp(State::SignIn)])));
        assert_eq!(p.take_wants(), vec![Want::Prompt("corp".into())]);
        p.apply(Got::Prompt("corp".into(), Ok(Some(form()))));
        p.sign_in(vec!["".into()], now);
        assert!(p.note.as_ref().unwrap().1.contains("Username"));
        p.sign_in(vec!["alice".into()], now);
        assert!(p.tick(now + crate::network::ARMED_WAIT));
        assert!(p.note.as_ref().unwrap().1.contains("field first"));
        p.cancel_signin();
        assert_eq!(p.take_wants(), vec![Want::Answer(Answer { name: "corp".into(), values: None, keep: false })]);
        // Signed in: the form goes.
        p.apply(Got::Done(Ok(String::new())));
        p.apply(Got::Tunnels(Ok(vec![corp(State::Up)])));
        assert!(p.signin.is_none());
        assert_eq!(describe(&corp(State::SignIn)).split(" \u{b7} ").next(), Some("Waiting for you to sign in"));
        assert_eq!(detail_line(&corp(State::Up)), "Gateway vpn.example.com \u{b7} address 10.7.0.2/32 \u{b7} as alice \u{b7} client certificate");
    }

    #[test]
    fn a_certificate_request_then_its_certificate() {
        let mut p = VpnPage::default();
        p.cert_request("corp");
        assert_eq!(p.take_wants(), vec![Want::CertRequest("corp".into())]);
        p.apply(Got::CertRequest(Ok(("corp".into(), "-----BEGIN CERTIFICATE REQUEST-----".into()))));
        assert!(p.csr.is_some());
        p.take_wants();
        p.pick_certificate("corp");
        assert_eq!(p.take_wants(), vec![Want::Pick]);
        p.apply(Got::Picked(Ok(("corp.pem".into(), b"-----BEGIN CERTIFICATE-----".to_vec()))));
        assert_eq!(p.take_wants(), vec![Want::Certificate(Certificate { name: "corp".into(), pem: b"-----BEGIN CERTIFICATE-----".to_vec() })]);
        assert!(p.pick_cert.is_none() && p.csr.is_none());
    }

    #[test]
    fn words() {
        let d = details("acme", State::Connecting, Scope::Personas);
        assert_eq!(describe(&d), "Connecting: its traffic waits \u{b7} all traffic \u{b7} for chosen personas");
        assert_eq!(detail_line(&d), "Server 203.0.113.5:51820 \u{b7} address 10.7.0.2/32 \u{b7} key xTIBA5rb…");
        let d = details("home", State::Up, Scope::Machine);
        assert!(describe(&d).starts_with("Connected"));
    }
}
