//! VPN tunnels on Aqueduct (`CLASS_VPN`, docs/spec/vpn.md §7.2).
//!
//! cryptad runs as root and takes requests only as lines on its own control
//! socket. Apps reach it through `atrium-crypta-ctl`, an unprivileged front
//! (its own uid, capability mode) that serves one socket per capability and
//! translates, as atrium-netconf does for atrium-net. The socket a
//! connection came in on is the grant: Portcullis mounts a socket's
//! directory only into the jail of an app whose manifest asks for that
//! capability and that the user approved.
//!
//! | capability (socket dir) | ops |
//! |---|---|
//! | `vpn-read` | `STATUS`, `SUBSCRIBE`: each tunnel's name, kind, state and scope; never addresses, gateways or keys |
//! | `vpn-use` | those, `LIST` (adds the block switch and whether it takes all traffic), `CONNECT`, `DISCONNECT` |
//! | `vpn-configure` | those, `DETAILS`, `IMPORT_WIREGUARD`, `CREATE_WIREGUARD`, `SET`, `REMOVE`; for corporate VPNs (V4) `CREATE_ANYCONNECT`, `PROMPT`, `ANSWER`, `SSO_DONE`, `CERT_REQUEST`, `CERTIFICATE`. Restricted to Curia |
//!
//! Which persona uses a tunnel is not here: it is the person's policy, set
//! through portcullisd (`SetPersonaVpn`), so no capability lets an app put
//! itself in or out of a tunnel.
//!
//! Every reply (`IS_RESPONSE`) is `u8 status`, then on `OK` the postcard
//! value, otherwise a postcard `String` saying why. `SUBSCRIBE` answers
//! `OK`, then sends `EV` (`ASYNC_EVENT`) with the [`Tunnel`] list now and on
//! every change.

use serde::{Deserialize, Serialize};
use std::net::{Ipv4Addr, SocketAddrV4};

pub use aqueduct::classes::CLASS_VPN as CLASS;

pub mod op {
    pub const STATUS: u16 = 0x00;
    pub const SUBSCRIBE: u16 = 0x01;
    pub const LIST: u16 = 0x10;
    pub const CONNECT: u16 = 0x11;
    pub const DISCONNECT: u16 = 0x12;
    pub const DETAILS: u16 = 0x20;
    /// [`ImportWireGuard`]: a wg-quick `.conf`; its keys go to Arca and
    /// are never kept anywhere else. Answers [`Created`].
    pub const IMPORT_WIREGUARD: u16 = 0x21;
    /// [`CreateWireGuard`]: a new key made in Arca (never leaves it).
    /// Answers [`Created`], whose public key the user gives the server.
    pub const CREATE_WIREGUARD: u16 = 0x22;
    pub const SET: u16 = 0x23;
    pub const REMOVE: u16 = 0x24;
    /// [`CreateAnyConnect`]: a corporate gateway. Answers the name.
    pub const CREATE_ANYCONNECT: u16 = 0x25;
    /// A tunnel's name: what its gateway asks now ([`Ask`]), if anything.
    pub const PROMPT: u16 = 0x26;
    /// [`Answer`]: the person's answers to the [`Ask`]; passwords and
    /// codes only as handles of Arca's credential field.
    pub const ANSWER: u16 = 0x27;
    /// [`SsoDone`]: the sign-in window finished (or was closed).
    pub const SSO_DONE: u16 = 0x28;
    /// [`CertRequest`]: a client key made in Arca (never leaves it); answers
    /// the PKCS #10 request (PEM) for the IT team to sign.
    pub const CERT_REQUEST: u16 = 0x29;
    /// [`Certificate`]: the signed chain for that key.
    pub const CERTIFICATE: u16 = 0x2A;
    pub const EV: u16 = 0x80;
}

pub mod status {
    pub const OK: u8 = 0;
    pub const MALFORMED: u8 = 1;
    /// This socket's capability does not cover the op.
    pub const NOT_ALLOWED: u8 = 2;
    /// Tried and did not work (no such tunnel, a file that cannot be used).
    pub const FAILED: u8 = 4;
    /// cryptad did not answer.
    pub const UNAVAILABLE: u8 = 6;
}

/// Which capability a socket stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grant {
    Read,
    Use,
    Configure,
}

impl Grant {
    pub const ALL: [Grant; 3] = [Grant::Read, Grant::Use, Grant::Configure];

    /// The capability's name, which is also its socket directory and file
    /// (`/atrium/sockets/<name>/<name>.sock`).
    pub fn name(self) -> &'static str {
        match self {
            Grant::Read => "vpn-read",
            Grant::Use => "vpn-use",
            Grant::Configure => "vpn-configure",
        }
    }

    pub fn allows(self, o: u16) -> bool {
        use op::*;
        match o {
            STATUS | SUBSCRIBE => true,
            LIST | CONNECT | DISCONNECT => matches!(self, Grant::Use | Grant::Configure),
            DETAILS | IMPORT_WIREGUARD | CREATE_WIREGUARD | SET | REMOVE | CREATE_ANYCONNECT | PROMPT | ANSWER | SSO_DONE | CERT_REQUEST | CERTIFICATE => self == Grant::Configure,
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    WireGuard,
    /// A Cisco AnyConnect (or compatible) gateway (vpn.md §9).
    AnyConnect,
    GlobalProtect,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum State {
    Down,
    /// Started, no handshake yet (or lost): with the block switch on, the
    /// traffic it carries goes nowhere meanwhile.
    Connecting,
    Up,
    /// The gateway is asking the person (a form, or single sign-on in a
    /// sign-in window): Curia's VPN page shows it.
    SignIn,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scope {
    /// Everything the machine sends that is not in a persona's route domain.
    Machine,
    /// Only the personas whose policy names this tunnel (vpn.md §9.7).
    Personas,
}

/// What `vpn-read` may know of a tunnel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tunnel {
    pub name: String,
    pub kind: Kind,
    pub state: State,
    pub scope: Scope,
}

/// What `vpn-use` adds (`LIST`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub tunnel: Tunnel,
    /// Block when not connected (vpn.md §5.4).
    pub block: bool,
    /// Takes all traffic (`0.0.0.0/0`), not some networks.
    pub full: bool,
}

/// An IPv4 network, `a.b.c.d/prefix`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Net {
    pub addr: Ipv4Addr,
    pub prefix: u8,
}

impl Net {
    pub fn parse(s: &str) -> Option<Net> {
        let (a, p) = s.split_once('/').unwrap_or((s, "32"));
        let prefix: u8 = p.parse().ok().filter(|p| *p <= 32)?;
        Some(Net { addr: a.parse().ok()?, prefix })
    }
}

impl std::fmt::Display for Net {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

/// What Curia shows of one tunnel (`DETAILS`, `vpn-configure`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Details {
    pub profile: Profile,
    /// Our public key (base64): what the server's admin needs.
    pub public: String,
    pub endpoint: SocketAddrV4,
    /// Our addresses in the tunnel.
    pub addrs: Vec<Net>,
    /// How many networks it carries (none shown when `full`).
    pub routes: u32,
    pub dns: Vec<Ipv4Addr>,
    /// Bytes since it was last connected.
    pub rx: u64,
    pub tx: u64,
    /// How many times its engine started since it was connected.
    pub starts: u32,
    /// A corporate gateway's settings.
    pub corporate: Option<Corporate>,
    /// Why it stopped for good the last time (a refusal), in words.
    pub error: Option<String>,
    /// What a corporate gateway said once signed in (plain text, 4 KiB).
    pub banner: Option<String>,
}

/// A corporate VPN's settings, as Curia shows them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Corporate {
    pub host: String,
    pub port: u16,
    /// Remembered after a sign-in.
    pub user: Option<String>,
    pub group: Option<String>,
    /// A client certificate is set up (its key in Arca).
    pub certificate: bool,
    /// The gateway's certificate is pinned instead of checked against CAs.
    pub pinned: bool,
}

/// `CREATE_ANYCONNECT`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateAnyConnect {
    pub name: String,
    /// The gateway: `vpn.example.com` or `vpn.example.com:8443`.
    pub host: String,
    /// A group's path on the gateway (`/staff`), or none.
    pub path: Option<String>,
    pub user: Option<String>,
    /// CA certificates to trust besides the system's, PEM; at most [`MAX_CONF`].
    pub ca: Vec<u8>,
    /// Or the gateway's own certificate, pinned by its SHA-256.
    pub pin: Option<[u8; 32]>,
    /// Persona scope (the default for a work VPN) or the machine.
    pub personas: bool,
}

/// A gateway name: letters, digits, `-` and dots, at most 253; or an
/// IPv4 address.
pub fn valid_host(h: &str) -> bool {
    !h.is_empty() && h.len() <= 253 && h.split('.').all(|l| !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && l.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-'))
}

fn plain(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && !s.chars().any(char::is_control)
}

impl CreateAnyConnect {
    pub fn check(&self) -> Res<()> {
        let bad = |w: &str| Err(Fail::new(status::MALFORMED, w));
        if !valid_name(&self.name) {
            return bad("a name is lowercase letters, digits and -, at most 32");
        }
        let (h, port) = match self.host.rsplit_once(':') {
            Some((h, p)) => (h, p.parse::<u16>().ok().filter(|p| *p > 0)),
            None => (self.host.as_str(), Some(443)),
        };
        if !valid_host(h) || port.is_none() {
            return bad("the gateway is a name like vpn.example.com, with :port if not 443");
        }
        if self.path.as_deref().is_some_and(|p| !p.starts_with('/') || p.len() > 128 || !p.bytes().all(|b| b.is_ascii_graphic())) {
            return bad("a group's path starts with /");
        }
        if self.user.as_deref().is_some_and(|u| !plain(u, 128)) {
            return bad("a username of at most 128 characters");
        }
        if self.ca.len() > MAX_CONF {
            return bad("the CA file is larger than 64 KiB");
        }
        Ok(())
    }
}

/// What a gateway asks (`PROMPT`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Ask {
    Form(Form),
    /// Sign in at the identity provider in a Navigator sign-in window
    /// (the profile's own jar); the window ends at `final_url`.
    Sso { login: String, final_url: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Form {
    pub title: String,
    pub message: String,
    /// Why the last answer did not do (`Login failed.`).
    pub error: String,
    pub banner: String,
    pub fields: Vec<Field>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FieldKind {
    Text,
    Password,
    /// A one-time code.
    Code,
    Choice,
    PasswordOnce,
    GatewayChoice,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Field {
    pub label: String,
    pub kind: FieldKind,
    /// `(value, label)` for a choice.
    pub choices: Vec<(String, String)>,
}

/// One field's answer, in the form's order.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Value {
    Text(String),
    /// The handle Arca's credential field gave (`HOLD_FOR_VPN`).
    Held([u8; 16]),
}

impl std::fmt::Debug for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Value::Text(t) => write!(f, "Text({} bytes)", t.len()),
            Value::Held(_) => write!(f, "Held(..)"),
        }
    }
}

/// `ANSWER`; `values: None` cancels the sign-in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    pub name: String,
    pub values: Option<Vec<Value>>,
    /// Save the password in Arca for this gateway once it is accepted.
    pub keep: bool,
}

/// `SSO_DONE`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SsoDone {
    pub name: String,
    pub ok: bool,
}

/// `CERT_REQUEST`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertRequest {
    pub name: String,
    /// The request's common name (the username if none).
    pub cn: Option<String>,
}

/// `CERTIFICATE`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Certificate {
    pub name: String,
    /// The chain, PEM, the client's own first.
    pub pem: Vec<u8>,
}

/// `IMPORT_WIREGUARD`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportWireGuard {
    pub name: String,
    /// The `.conf` file, at most [`MAX_CONF`] bytes. Holds the private key:
    /// it passes through the front into cryptad on a pipe and into Arca.
    pub conf: Vec<u8>,
}

impl std::fmt::Debug for ImportWireGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("ImportWireGuard").field("name", &self.name).field("conf", &format_args!("{} bytes", self.conf.len())).finish()
    }
}

/// The largest `.conf` cryptad reads (crypta-proto's `conf::MAX_FILE`).
pub const MAX_CONF: usize = 64 * 1024;
/// The most networks a new tunnel may carry (one cryptad line holds them).
pub const MAX_ROUTES: usize = 128;

/// `CREATE_WIREGUARD`: what the server's admin gave, with a key made here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateWireGuard {
    pub name: String,
    /// The server's public key, base64 (44 characters).
    pub peer: String,
    pub endpoint: SocketAddrV4,
    /// Our addresses in the tunnel, at least one.
    pub addrs: Vec<Net>,
    /// The networks it carries; `0.0.0.0/0` for all traffic.
    pub allowed: Vec<Net>,
    pub dns: Vec<Ipv4Addr>,
    pub keepalive: Option<u16>,
}

/// What `IMPORT_WIREGUARD` and `CREATE_WIREGUARD` answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Created {
    pub name: String,
    /// Our public key, base64.
    pub public: String,
    /// Settings in the file that were left out (wg-quick's `PostUp`, ...).
    pub ignored: Vec<String>,
}

/// `SET`: each `Some` changes; it takes effect at the next connect.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetProfile {
    pub name: String,
    pub block: Option<bool>,
    /// Persona scope (true) or machine scope (false).
    pub personas: Option<bool>,
}

/// A tunnel's name: lowercase letters, digits and `-`, 1 to 32, not
/// starting with `-` (cryptad's `profile::valid_name`).
pub fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 32 && !n.starts_with('-') && n.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// A WireGuard public key in base64: 44 characters ending in `=`.
pub fn valid_key(k: &str) -> bool {
    k.len() == 44 && k.ends_with('=') && k[..43].bytes().all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
}

fn usable(a: Ipv4Addr) -> bool {
    !(a.is_unspecified() || a.is_loopback() || a.is_multicast() || a.is_broadcast())
}

impl CreateWireGuard {
    pub fn check(&self) -> Res<()> {
        let bad = |w: &str| Err(Fail::new(status::MALFORMED, w));
        if !valid_name(&self.name) {
            return bad("a name is lowercase letters, digits and -, at most 32");
        }
        if !valid_key(&self.peer) {
            return bad("the server's key is 44 characters of base64");
        }
        if !usable(*self.endpoint.ip()) || self.endpoint.port() == 0 {
            return bad("the server's address and port");
        }
        if self.addrs.is_empty() || self.addrs.len() > 8 || !self.addrs.iter().all(|n| usable(n.addr) && n.prefix >= 8) {
            return bad("one to 8 addresses of ours in the tunnel");
        }
        if self.allowed.is_empty() || self.allowed.len() > MAX_ROUTES || self.allowed.iter().any(|n| n.prefix > 32) {
            return bad("one or more networks it carries (0.0.0.0/0 for all)");
        }
        if self.dns.len() > 3 || !self.dns.iter().all(|d| usable(*d)) {
            return bad("at most 3 DNS servers");
        }
        if self.keepalive.is_some_and(|k| k == 0 || k > 3600) {
            return bad("keepalive is 1 to 3600 seconds");
        }
        Ok(())
    }
}

/// A refusal: a status other than OK and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fail(pub u8, pub String);

impl Fail {
    pub fn new(st: u8, why: &str) -> Fail {
        Fail(st, why.to_string())
    }
}

pub type Res<T> = Result<T, Fail>;

fn enc<T: Serialize>(r: &Res<T>) -> Vec<u8> {
    match r {
        Ok(v) => {
            let mut p = vec![status::OK];
            p.extend(postcard::to_stdvec(v).unwrap_or_default());
            p
        }
        Err(Fail(st, why)) => {
            let mut p = vec![*st];
            p.extend(postcard::to_stdvec(why).unwrap_or_default());
            p
        }
    }
}

fn dec<T: for<'a> Deserialize<'a>>(p: &[u8]) -> Res<T> {
    match p.split_first() {
        Some((&status::OK, rest)) => postcard::from_bytes(rest).map_err(|e| Fail(status::MALFORMED, e.to_string())),
        Some((&st, rest)) => Err(Fail(st, postcard::from_bytes(rest).unwrap_or_default())),
        None => Err(Fail(status::MALFORMED, "empty reply".into())),
    }
}

/// What a refusal from cryptad means, in words for the person.
pub fn words(why: &str) -> String {
    match why {
        "exists" => "a tunnel with that name exists already".into(),
        "no-profile" => "no such tunnel".into(),
        "bad-file" => "the file is not a WireGuard configuration".into(),
        "many-peers" => "the file has more than one peer; only one is supported".into(),
        "no-endpoint" => "the file names no server (Endpoint)".into(),
        "named-endpoint" => "the server is named, not an address; give its IPv4 address".into(),
        "v6-endpoint" => "the server's address is IPv6; only IPv4 servers for now".into(),
        "no-v4-address" => "the file gives no IPv4 address in the tunnel".into(),
        "no-v4-routes" => "the file carries no IPv4 networks".into(),
        "too-many" => "too many tunnels are connected".into(),
        "unreadable" => "the file could not be read".into(),
        "bad-host" => "the gateway is not a name like vpn.example.com".into(),
        "bad-ca" => "the CA file holds no certificate".into(),
        "no-prompt" => "the gateway is not asking anything now".into(),
        "missing-field" => "every field needs an answer".into(),
        "bad-choice" => "that is not one of the choices".into(),
        "password-through-arca" => "a password goes through Arca's field".into(),
        "not-for-this-key" => "that certificate is not for this tunnel's key; ask for one signed for its request".into(),
        "no-key" => "make a certificate request first".into(),
        "not-up" => "the tunnel is not connected".into(),
        "no-v4-address" => "the gateway's name has no IPv4 address".into(),
        w if w.starts_with("arca-") => format!("Arca refused the key ({})", &w[5..]),
        w => w.replace('-', " "),
    }
}

pub mod lines;

pub mod server {
    use std::io;
    use std::sync::mpsc;
    use std::time::Duration;

    use aqueduct::{flag, Connection};

    use super::*;

    /// What the front does for each op (atrium-crypta-ctl talks to
    /// cryptad; tests use a fake).
    pub trait Backend: Send + Sync {
        fn status(&self) -> Res<Vec<Tunnel>>;
        /// The tunnels now and on every change, until the receiver is dropped.
        fn subscribe(&self) -> Res<mpsc::Receiver<Vec<Tunnel>>>;
        fn list(&self) -> Res<Vec<Profile>>;
        fn connect(&self, name: String) -> Res<()>;
        fn disconnect(&self, name: String) -> Res<()>;
        fn details(&self, name: String) -> Res<Details>;
        fn import(&self, i: ImportWireGuard) -> Res<Created>;
        fn create(&self, c: CreateWireGuard) -> Res<Created>;
        fn set(&self, s: SetProfile) -> Res<()>;
        fn remove(&self, name: String) -> Res<()>;
        fn create_ac(&self, c: CreateAnyConnect) -> Res<String>;
        fn prompt(&self, name: String) -> Res<Option<Ask>>;
        fn answer(&self, a: Answer) -> Res<()>;
        fn sso_done(&self, s: SsoDone) -> Res<()>;
        fn cert_request(&self, c: CertRequest) -> Res<String>;
        fn certificate(&self, c: Certificate) -> Res<()>;
    }

    fn arg<T: for<'a> Deserialize<'a>>(p: &[u8]) -> Res<T> {
        postcard::from_bytes(p).map_err(|_| Fail::new(status::MALFORMED, "request does not parse"))
    }

    fn name(p: &[u8]) -> Res<String> {
        let n: String = arg(p)?;
        if !valid_name(&n) {
            return Err(Fail::new(status::MALFORMED, "not a tunnel name"));
        }
        Ok(n)
    }

    /// Serve one connection that came in on `grant`'s socket.
    pub fn serve(mut conn: Connection, grant: Grant, b: &dyn Backend) -> io::Result<()> {
        loop {
            let m = conn.recv_message()?;
            if m.opcode_class != CLASS {
                continue;
            }
            let o = m.op;
            if !grant.allows(o) {
                let r: Res<()> = Err(Fail(status::NOT_ALLOWED, format!("not with {}", grant.name())));
                conn.send_message(CLASS, o, flag::IS_RESPONSE, &enc(&r))?;
                continue;
            }
            let p = &m.payload;
            let reply = match o {
                op::STATUS => enc(&b.status()),
                op::SUBSCRIBE => match b.subscribe() {
                    Err(f) => enc::<()>(&Err(f)),
                    Ok(rx) => {
                        conn.send_message(CLASS, o, flag::IS_RESPONSE, &enc(&Ok(())))?;
                        let mut last: Option<Vec<Tunnel>> = None;
                        loop {
                            match rx.recv_timeout(Duration::from_secs(30)) {
                                Ok(s) => {
                                    if last.as_ref() != Some(&s) {
                                        let ev = postcard::to_stdvec(&s).unwrap_or_default();
                                        conn.send_message(CLASS, op::EV, flag::ASYNC_EVENT, &ev)?;
                                        last = Some(s);
                                    }
                                }
                                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
                            }
                        }
                    }
                },
                op::LIST => enc(&b.list()),
                op::CONNECT => enc(&name(p).and_then(|n| b.connect(n))),
                op::DISCONNECT => enc(&name(p).and_then(|n| b.disconnect(n))),
                op::DETAILS => enc(&name(p).and_then(|n| b.details(n))),
                op::REMOVE => enc(&name(p).and_then(|n| b.remove(n))),
                op::IMPORT_WIREGUARD => enc(&arg::<ImportWireGuard>(p).and_then(|i| {
                    if !valid_name(&i.name) {
                        return Err(Fail::new(status::MALFORMED, "a name is lowercase letters, digits and -, at most 32"));
                    }
                    if i.conf.is_empty() || i.conf.len() > MAX_CONF {
                        return Err(Fail::new(status::MALFORMED, "the file is empty or larger than 64 KiB"));
                    }
                    b.import(i)
                })),
                op::CREATE_WIREGUARD => enc(&arg::<CreateWireGuard>(p).and_then(|c| {
                    c.check()?;
                    b.create(c)
                })),
                op::SET => enc(&arg::<SetProfile>(p).and_then(|s| {
                    if !valid_name(&s.name) || (s.block.is_none() && s.personas.is_none()) {
                        return Err(Fail::new(status::MALFORMED, "a tunnel and something to change"));
                    }
                    b.set(s)
                })),
                op::CREATE_ANYCONNECT => enc(&arg::<CreateAnyConnect>(p).and_then(|c| {
                    c.check()?;
                    b.create_ac(c)
                })),
                op::PROMPT => enc(&name(p).and_then(|n| b.prompt(n))),
                op::ANSWER => enc(&arg::<Answer>(p).and_then(|a| {
                    if !valid_name(&a.name) || a.values.as_ref().is_some_and(|v| v.len() > 16 || v.iter().any(|x| matches!(x, Value::Text(t) if t.len() > 1024))) {
                        return Err(Fail::new(status::MALFORMED, "answers to a tunnel's prompt"));
                    }
                    b.answer(a)
                })),
                op::SSO_DONE => enc(&arg::<SsoDone>(p).and_then(|s| {
                    if !valid_name(&s.name) {
                        return Err(Fail::new(status::MALFORMED, "not a tunnel name"));
                    }
                    b.sso_done(s)
                })),
                op::CERT_REQUEST => enc(&arg::<CertRequest>(p).and_then(|c| {
                    if !valid_name(&c.name) || c.cn.as_deref().is_some_and(|n| !plain(n, 64)) {
                        return Err(Fail::new(status::MALFORMED, "a tunnel and a name of at most 64 characters"));
                    }
                    b.cert_request(c)
                })),
                op::CERTIFICATE => enc(&arg::<Certificate>(p).and_then(|c| {
                    if !valid_name(&c.name) || c.pem.is_empty() || c.pem.len() > MAX_CONF {
                        return Err(Fail::new(status::MALFORMED, "a tunnel and its certificate, at most 64 KiB"));
                    }
                    b.certificate(c)
                })),
                _ => enc::<()>(&Err(Fail::new(status::MALFORMED, "unknown op"))),
            };
            conn.send_message(CLASS, o, flag::IS_RESPONSE, &reply)?;
        }
    }
}

pub mod client {
    use std::io;
    use std::path::Path;

    use aqueduct::{flag, Connection};

    use super::*;

    /// A connection to one of the capability sockets.
    pub struct Client(Connection);

    impl Client {
        pub fn connect(sock: &Path) -> io::Result<Client> {
            Ok(Client(Connection::connect(sock)?))
        }

        /// The socket for `g` under `dir` (`/atrium/sockets` in a jail).
        pub fn path(dir: &Path, g: Grant) -> std::path::PathBuf {
            dir.join(g.name()).join(format!("{}.sock", g.name()))
        }

        /// How long an answer may take: connecting waits for atrium-net
        /// and jaild, a new key for Arca; the rest is quick.
        pub fn limit(o: u16) -> std::time::Duration {
            std::time::Duration::from_secs(match o {
                op::CONNECT | op::IMPORT_WIREGUARD | op::CREATE_WIREGUARD | op::REMOVE | op::CREATE_ANYCONNECT | op::CERT_REQUEST => 30,
                _ => 10,
            })
        }

        fn call<A: Serialize, T: for<'a> Deserialize<'a>>(&mut self, o: u16, a: &A) -> Res<T> {
            let p = postcard::to_stdvec(a).map_err(|e| Fail(status::MALFORMED, e.to_string()))?;
            let io = |e: io::Error| match e.kind() {
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => Fail(status::UNAVAILABLE, "not answering".into()),
                _ => Fail(status::UNAVAILABLE, e.to_string()),
            };
            self.0.set_read_timeout(Some(Self::limit(o))).map_err(io)?;
            self.0.send_message(CLASS, o, flag::RESPONSE_EXPECTED, &p).map_err(io)?;
            loop {
                let m = self.0.recv_message().map_err(io)?;
                if m.opcode_class == CLASS && m.op == o && m.flags & flag::IS_RESPONSE != 0 {
                    return dec(&m.payload);
                }
            }
        }

        pub fn status(&mut self) -> Res<Vec<Tunnel>> {
            self.call(op::STATUS, &())
        }
        pub fn list(&mut self) -> Res<Vec<Profile>> {
            self.call(op::LIST, &())
        }
        pub fn connect_tunnel(&mut self, name: &str) -> Res<()> {
            self.call(op::CONNECT, &name.to_string())
        }
        pub fn disconnect_tunnel(&mut self, name: &str) -> Res<()> {
            self.call(op::DISCONNECT, &name.to_string())
        }
        pub fn details(&mut self, name: &str) -> Res<Details> {
            self.call(op::DETAILS, &name.to_string())
        }
        pub fn import(&mut self, i: &ImportWireGuard) -> Res<Created> {
            self.call(op::IMPORT_WIREGUARD, i)
        }
        pub fn create(&mut self, c: &CreateWireGuard) -> Res<Created> {
            self.call(op::CREATE_WIREGUARD, c)
        }
        pub fn set(&mut self, s: &SetProfile) -> Res<()> {
            self.call(op::SET, s)
        }
        pub fn remove(&mut self, name: &str) -> Res<()> {
            self.call(op::REMOVE, &name.to_string())
        }
        pub fn create_ac(&mut self, c: &CreateAnyConnect) -> Res<String> {
            self.call(op::CREATE_ANYCONNECT, c)
        }
        pub fn prompt(&mut self, name: &str) -> Res<Option<Ask>> {
            self.call(op::PROMPT, &name.to_string())
        }
        pub fn answer(&mut self, a: &Answer) -> Res<()> {
            self.call(op::ANSWER, a)
        }
        pub fn sso_done(&mut self, s: &SsoDone) -> Res<()> {
            self.call(op::SSO_DONE, s)
        }
        pub fn cert_request(&mut self, c: &CertRequest) -> Res<String> {
            self.call(op::CERT_REQUEST, c)
        }
        pub fn certificate(&mut self, c: &Certificate) -> Res<()> {
            self.call(op::CERTIFICATE, c)
        }

        /// Subscribe: the connection then carries only tunnel lists.
        pub fn subscribe(mut self) -> Res<Changes> {
            self.call::<(), ()>(op::SUBSCRIBE, &())?;
            let _ = self.0.set_read_timeout(None);
            Ok(Changes(self.0))
        }
    }

    pub struct Changes(Connection);

    impl Changes {
        /// The next list (blocking).
        pub fn next(&mut self) -> io::Result<Vec<Tunnel>> {
            loop {
                let m = self.0.recv_message()?;
                if m.opcode_class == CLASS && m.op == op::EV {
                    return postcard::from_bytes(&m.payload).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()));
                }
            }
        }

        /// Wait at most `t` for the next list.
        pub fn next_within(&mut self, t: std::time::Duration) -> io::Result<Option<Vec<Tunnel>>> {
            self.0.set_read_timeout(Some(t))?;
            let r = match self.next() {
                Ok(s) => Ok(Some(s)),
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => Ok(None),
                Err(e) => Err(e),
            };
            self.0.set_read_timeout(None)?;
            r
        }
    }
}

#[cfg(test)]
mod tests;
