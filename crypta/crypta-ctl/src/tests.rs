use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};

use aura_ipc::Line;

use crate::client::Client;
use crate::server::{serve, Backend};
use crate::*;

const KEY: &str = "xTIBA5rboUvnH4htodjb6e697QjLERt1NAB4mZqp8Dg=";

fn list_lines() -> Vec<Line> {
    [
        format!("tunnel name=acme up=1 iface=tun0 engine=1 handshake=1 starts=2 rx=10 tx=20 dropped=0 public={KEY} endpoint=10.88.0.9:51820 full=1 block=1 scoped=1 addrs=10.7.0.2/32 routes=1 dns=10.7.0.1"),
        format!("tunnel name=home up=0 public={KEY} endpoint=10.88.0.10:51820 full=0 block=0 scoped=0 addrs=10.8.0.2/24 routes=3"),
        "tunnel name=../x up=1".to_string(),
    ]
    .iter()
    .map(|s| Line::parse(s).unwrap())
    .collect()
}

#[test]
fn list_lines_to_profiles() {
    let v: Vec<Profile> = list_lines().iter().filter_map(lines::profile).collect();
    assert_eq!(v.len(), 2, "a bad name is dropped");
    assert_eq!(v[0].tunnel, Tunnel { name: "acme".into(), kind: Kind::WireGuard, state: State::Up, scope: Scope::Personas });
    assert!(v[0].block && v[0].full);
    assert_eq!((v[1].tunnel.state, v[1].tunnel.scope), (State::Down, Scope::Machine));
    let c = Line::parse("tunnel name=x up=1 handshake=0").unwrap();
    assert_eq!(lines::profile(&c).unwrap().tunnel.state, State::Connecting);
    let d = lines::details(&list_lines()[0]).unwrap();
    assert_eq!(d.addrs, vec![Net::parse("10.7.0.2/32").unwrap()]);
    assert_eq!((d.rx, d.tx, d.starts, d.routes), (10, 20, 2, 1));
    assert_eq!(d.dns, vec!["10.7.0.1".parse::<std::net::Ipv4Addr>().unwrap()]);
    assert_eq!(d.public, KEY);
}

fn create() -> CreateWireGuard {
    CreateWireGuard {
        name: "acme".into(),
        peer: KEY.into(),
        endpoint: "10.88.0.9:51820".parse().unwrap(),
        addrs: vec![Net::parse("10.7.0.2/32").unwrap()],
        allowed: vec![Net::parse("0.0.0.0/0").unwrap()],
        dns: vec!["10.7.0.1".parse().unwrap()],
        keepalive: Some(25),
    }
}

#[test]
fn create_checks_and_line() {
    create().check().unwrap();
    let bad = |c: CreateWireGuard| c.check().unwrap_err().0;
    assert_eq!(bad(CreateWireGuard { name: "Acme".into(), ..create() }), status::MALFORMED);
    assert_eq!(bad(CreateWireGuard { peer: "short=".into(), ..create() }), status::MALFORMED);
    assert_eq!(bad(CreateWireGuard { addrs: vec![], ..create() }), status::MALFORMED);
    assert_eq!(bad(CreateWireGuard { allowed: vec![Net::parse("10.0.0.0/8").unwrap(); MAX_ROUTES + 1], ..create() }), status::MALFORMED);
    assert_eq!(bad(CreateWireGuard { endpoint: "127.0.0.1:51820".parse().unwrap(), ..create() }), status::MALFORMED);
    assert_eq!(bad(CreateWireGuard { keepalive: Some(0), ..create() }), status::MALFORMED);
    let l = lines::create(&create());
    assert_eq!(String::from_utf8(l.encode().unwrap()).unwrap(), format!("create name=acme peer={KEY} endpoint=10.88.0.9:51820 addrs=10.7.0.2/32 allowed=0.0.0.0/0 dns=10.7.0.1 keepalive=25\n"));
    // The most routes still fit one line.
    let many = CreateWireGuard { allowed: vec![Net::parse("100.100.100.100/32").unwrap(); MAX_ROUTES], ..create() };
    many.check().unwrap();
    assert!(lines::create(&many).encode().is_some());
    let made = lines::created(&[Line::parse(&format!("created name=acme public={KEY}")).unwrap()]).unwrap();
    assert_eq!((made.name.as_str(), made.public.as_str(), made.ignored.len()), ("acme", KEY, 0));
    let imp = lines::created(&[Line::parse(&format!("imported name=acme public={KEY} ignored=PostUp,Table")).unwrap()]).unwrap();
    assert_eq!(imp.ignored, vec!["PostUp".to_string(), "Table".to_string()]);
}

#[derive(Default)]
struct Fake {
    did: Mutex<Vec<String>>,
}

impl Fake {
    fn did(&self, s: String) {
        self.did.lock().unwrap().push(s);
    }
}

impl Backend for Fake {
    fn status(&self) -> Res<Vec<Tunnel>> {
        Ok(self.list()?.into_iter().map(|p| p.tunnel).collect())
    }
    fn subscribe(&self) -> Res<mpsc::Receiver<Vec<Tunnel>>> {
        let (tx, rx) = mpsc::channel();
        tx.send(self.status()?).unwrap();
        let mut t = self.status()?;
        t[0].state = State::Connecting;
        tx.send(t).unwrap();
        std::mem::forget(tx);
        Ok(rx)
    }
    fn list(&self) -> Res<Vec<Profile>> {
        Ok(list_lines().iter().filter_map(lines::profile).collect())
    }
    fn connect(&self, name: String) -> Res<()> {
        if name == "nope" {
            return Err(Fail(status::FAILED, words("no-profile")));
        }
        self.did(format!("up {name}"));
        Ok(())
    }
    fn disconnect(&self, name: String) -> Res<()> {
        self.did(format!("down {name}"));
        Ok(())
    }
    fn details(&self, name: String) -> Res<Details> {
        list_lines().iter().filter(|l| l.get("name") == Some(&name)).find_map(lines::details).ok_or(Fail::new(status::FAILED, "no such tunnel"))
    }
    fn import(&self, i: ImportWireGuard) -> Res<Created> {
        self.did(format!("import {} {}", i.name, i.conf.len()));
        Ok(Created { name: i.name, public: KEY.into(), ignored: vec![] })
    }
    fn create(&self, c: CreateWireGuard) -> Res<Created> {
        self.did(format!("create {}", c.name));
        Ok(Created { name: c.name, public: KEY.into(), ignored: vec![] })
    }
    fn set(&self, s: SetProfile) -> Res<()> {
        self.did(format!("set {} {:?} {:?}", s.name, s.block, s.personas));
        Ok(())
    }
    fn remove(&self, name: String) -> Res<()> {
        self.did(format!("remove {name}"));
        Ok(())
    }
    fn create_ac(&self, c: CreateAnyConnect) -> Res<String> {
        self.did(format!("create-ac {} {} {}", c.name, c.host, c.ca.len()));
        Ok(c.name)
    }
    fn prompt(&self, name: String) -> Res<Option<Ask>> {
        Ok(lines::ask(&ask_lines()).filter(|_| name == "corp"))
    }
    fn answer(&self, a: Answer) -> Res<()> {
        self.did(lines::answer(&a).encode().map(|b| String::from_utf8(b).unwrap().trim_end().to_string()).unwrap());
        Ok(())
    }
    fn sso_done(&self, s: SsoDone) -> Res<()> {
        self.did(format!("sso-done {} {}", s.name, s.ok));
        Ok(())
    }
    fn cert_request(&self, c: CertRequest) -> Res<String> {
        self.did(format!("csr {} {:?}", c.name, c.cn));
        Ok("-----BEGIN CERTIFICATE REQUEST-----".into())
    }
    fn certificate(&self, c: Certificate) -> Res<()> {
        self.did(format!("certificate {} {}", c.name, c.pem.len()));
        Ok(())
    }
}

fn hx(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02x}")).collect()
}

fn ask_lines() -> Vec<Line> {
    [
        format!("text k=title v={}", hx("Sign in")),
        format!("text k=error v={}", hx("Login failed.")),
        format!("field i=0 kind=0 name={} label={}", hx("username"), hx("Username:")),
        format!("field i=1 kind=3 name={} label={}", hx("group_list"), hx("GROUP:")),
        format!("choice i=1 value={} label={}", hx("Staff"), hx("Staff")),
        format!("choice i=1 value={} label={}", hx("Eng"), hx("Engineering")),
        format!("field i=2 kind=1 name={} label={}", hx("password"), hx("Password:")),
    ]
    .iter()
    .map(|s| Line::parse(s).unwrap())
    .collect()
}

#[test]
fn globalprotect_portal_prompt_kinds() {
    let fields = [
        Line::new("field").with("i", 0).with("kind", 1).with_hex("name", b"gp_password_once"),
        Line::new("field").with("i", 1).with("kind", 3).with_hex("name", b"gp_gateway"),
    ];
    let Some(Ask::Form(f)) = lines::ask(&fields) else { panic!() };
    assert_eq!(f.fields[0].kind, FieldKind::PasswordOnce);
    assert_eq!(f.fields[1].kind, FieldKind::GatewayChoice);
}

/// A corporate tunnel's lines: its kind, its sign-in state, its settings,
/// what its gateway asks and the answer cryptad gets.
#[test]
fn anyconnect_lines() {
    let l = Line::parse(&format!("tunnel name=corp up=1 iface=tun0 engine=1 handshake=0 starts=1 signin=prompt kind=ac host=gw.example.com port=8443 block=0 scoped=1 cert=1 pinned=0 user={}", hx("alice"))).unwrap();
    let p = lines::profile(&l).unwrap();
    assert_eq!((p.tunnel.kind, p.tunnel.state), (Kind::AnyConnect, State::SignIn));
    let d = lines::details(&l).unwrap();
    let c = d.corporate.unwrap();
    assert_eq!((c.host.as_str(), c.port, c.user.as_deref(), c.certificate), ("gw.example.com", 8443, Some("alice"), true));
    let up = Line::parse("tunnel name=corp up=1 handshake=1 signin=in kind=ac host=gw.example.com port=443 addr=10.98.0.50/24 routes=2").unwrap();
    assert_eq!(lines::profile(&up).unwrap().tunnel.state, State::Up);
    assert_eq!(lines::details(&up).unwrap().addrs, vec![Net::parse("10.98.0.50/24").unwrap()]);
    let failed = Line::parse(&format!("tunnel name=corp up=0 kind=ac host=h port=443 error={}", hx("the gateway refused"))).unwrap();
    assert_eq!(lines::details(&failed).unwrap().error.as_deref(), Some("the gateway refused"));

    let Some(Ask::Form(f)) = lines::ask(&ask_lines()) else { panic!() };
    assert_eq!((f.title.as_str(), f.error.as_str(), f.fields.len()), ("Sign in", "Login failed.", 3));
    assert_eq!(f.fields[1].choices, vec![("Staff".into(), "Staff".into()), ("Eng".into(), "Engineering".into())]);
    assert_eq!(f.fields[2].kind, FieldKind::Password);
    let sso = Line::parse(&format!("sso final={} cookie={}", hx("https://gw/done"), hx("acSamlv2Token"))).unwrap();
    let part = |p: &str| Line::parse(&format!("sso-login v={}", hx(p))).unwrap();
    assert_eq!(lines::ask(&[part("https://idp/x"), part("#atrium-saml-post=U0F"), sso.clone()]),
        Some(Ask::Sso { login: "https://idp/x#atrium-saml-post=U0F".into(), final_url: "https://gw/done".into() }));
    assert_eq!(lines::ask(std::slice::from_ref(&sso)), None, "no start");
    let long: Vec<Line> = (0..6).map(|_| part(&"a".repeat(1500))).chain([sso]).collect();
    assert_eq!(lines::ask(&long), None, "longer than a window opens");

    let a = Answer { name: "corp".into(), values: Some(vec![Value::Text("alice".into()), Value::Text("Staff".into()), Value::Held([0xab; 16])]), keep: true };
    assert_eq!(String::from_utf8(lines::answer(&a).encode().unwrap()).unwrap(), format!("answer name=corp t0={} t1={} h2={} keep=1\n", hx("alice"), hx("Staff"), "ab".repeat(16)));
    assert_eq!(lines::answer(&Answer { name: "corp".into(), values: None, keep: false }).get("cancel"), Some("1"));
    let c = CreateAnyConnect { name: "corp".into(), host: "gw.example.com:8443".into(), path: None, user: Some("alice".into()), ca: vec![], pin: None, personas: true };
    c.check().unwrap();
    assert_eq!(String::from_utf8(lines::create_ac(&c).encode().unwrap()).unwrap(), format!("create-ac name=corp host=gw.example.com:8443 scoped=1 user={}\n", hx("alice")));
    for bad in ["", "gw example.com", "gw.example.com:0", "-gw.example.com", "gw.example.com:x"] {
        assert!(CreateAnyConnect { host: bad.into(), ..c.clone() }.check().is_err(), "{bad}");
    }
    assert!(CreateAnyConnect { path: Some("staff".into()), ..c.clone() }.check().is_err());
}

fn serve_all(dir: &Path, b: Arc<Fake>) {
    for g in Grant::ALL {
        let p = Client::path(dir, g);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let l = std::os::unix::net::UnixListener::bind(&p).unwrap();
        let b = b.clone();
        std::thread::spawn(move || {
            for s in l.incoming().flatten() {
                let b = b.clone();
                std::thread::spawn(move || {
                    let _ = serve(aqueduct::Connection::wrap(s).unwrap(), g, &*b);
                });
            }
        });
    }
}

fn tmp() -> PathBuf {
    let d = std::env::temp_dir().join(format!("crypta-ctl-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Over real sockets: what each capability's socket allows.
#[test]
fn over_sockets() {
    let dir = tmp();
    let fake = Arc::new(Fake::default());
    serve_all(&dir, fake.clone());
    let c = |g| Client::connect(&Client::path(&dir, g)).unwrap();

    let mut read = c(Grant::Read);
    let s = read.status().unwrap();
    assert_eq!(s.len(), 2);
    assert_eq!(read.list().unwrap_err().0, status::NOT_ALLOWED);
    assert_eq!(read.connect_tunnel("acme").unwrap_err().0, status::NOT_ALLOWED);
    assert_eq!(read.details("acme").unwrap_err().0, status::NOT_ALLOWED);

    let mut using = c(Grant::Use);
    assert_eq!(using.list().unwrap().len(), 2);
    using.connect_tunnel("home").unwrap();
    using.disconnect_tunnel("acme").unwrap();
    assert_eq!(using.connect_tunnel("nope").unwrap_err(), Fail(status::FAILED, "no such tunnel".into()));
    assert_eq!(using.connect_tunnel("../x").unwrap_err().0, status::MALFORMED);
    // An app without vpn-configure is refused every change of settings.
    assert_eq!(using.details("acme").unwrap_err().0, status::NOT_ALLOWED);
    assert_eq!(using.create(&create()).unwrap_err().0, status::NOT_ALLOWED);
    assert_eq!(using.import(&ImportWireGuard { name: "x".into(), conf: b"[Interface]".to_vec() }).unwrap_err().0, status::NOT_ALLOWED);
    assert_eq!(using.set(&SetProfile { name: "acme".into(), block: Some(false), personas: None }).unwrap_err().0, status::NOT_ALLOWED);
    assert_eq!(using.remove("acme").unwrap_err().0, status::NOT_ALLOWED);

    let mut conf = c(Grant::Configure);
    assert_eq!(conf.details("acme").unwrap().endpoint, "10.88.0.9:51820".parse().unwrap());
    assert_eq!(conf.details("zzz").unwrap_err().0, status::FAILED);
    assert_eq!(conf.create(&create()).unwrap().public, KEY);
    assert_eq!(conf.create(&CreateWireGuard { addrs: vec![], ..create() }).unwrap_err().0, status::MALFORMED);
    conf.import(&ImportWireGuard { name: "work".into(), conf: vec![b'#'; 100] }).unwrap();
    assert_eq!(conf.import(&ImportWireGuard { name: "work".into(), conf: vec![b'#'; MAX_CONF + 1] }).unwrap_err().0, status::MALFORMED);
    assert_eq!(conf.set(&SetProfile { name: "acme".into(), block: None, personas: None }).unwrap_err().0, status::MALFORMED);
    conf.set(&SetProfile { name: "acme".into(), block: Some(false), personas: Some(true) }).unwrap();
    conf.remove("home").unwrap();
    conf.connect_tunnel("acme").unwrap();

    // Corporate VPNs: only with vpn-configure.
    let ac = CreateAnyConnect { name: "corp".into(), host: "gw.example.com".into(), path: None, user: None, ca: b"-----BEGIN CERTIFICATE-----".to_vec(), pin: None, personas: true };
    assert_eq!(using.create_ac(&ac).unwrap_err().0, status::NOT_ALLOWED);
    assert_eq!(using.prompt("corp").unwrap_err().0, status::NOT_ALLOWED);
    assert_eq!(using.answer(&Answer { name: "corp".into(), values: None, keep: false }).unwrap_err().0, status::NOT_ALLOWED);
    assert_eq!(conf.create_ac(&ac).unwrap(), "corp");
    assert_eq!(conf.create_ac(&CreateAnyConnect { host: "not a host".into(), ..ac.clone() }).unwrap_err().0, status::MALFORMED);
    let Some(Ask::Form(f)) = conf.prompt("corp").unwrap() else { panic!("no form") };
    assert_eq!(f.fields.len(), 3);
    assert_eq!(conf.prompt("home").unwrap(), None);
    conf.answer(&Answer { name: "corp".into(), values: Some(vec![Value::Text("alice".into()), Value::Text("Staff".into()), Value::Held([1; 16])]), keep: false }).unwrap();
    conf.sso_done(&SsoDone { name: "corp".into(), ok: true }).unwrap();
    assert!(conf.cert_request(&CertRequest { name: "corp".into(), cn: Some("alice".into()) }).unwrap().starts_with("-----BEGIN"));
    conf.certificate(&Certificate { name: "corp".into(), pem: vec![b'x'; 10] }).unwrap();
    assert_eq!(conf.certificate(&Certificate { name: "corp".into(), pem: vec![] }).unwrap_err().0, status::MALFORMED);

    let did = fake.did.lock().unwrap().clone();
    assert_eq!(did[..7], ["up home", "down acme", "create acme", "import work 100", "set acme Some(false) Some(true)", "remove home", "up acme"]);
    assert_eq!(did[7..], [
        "create-ac corp gw.example.com 27".to_string(),
        format!("answer name=corp t0={} t1={} h2={}", hx("alice"), hx("Staff"), "01".repeat(16)),
        "sso-done corp true".into(),
        "csr corp Some(\"alice\")".into(),
        "certificate corp 10".into(),
    ]);

    let mut ch = c(Grant::Read).subscribe().unwrap();
    assert_eq!(ch.next().unwrap()[0].state, State::Up);
    assert_eq!(ch.next().unwrap()[0].state, State::Connecting);
    let _ = std::fs::remove_dir_all(&dir);
}
