//! cryptad's control lines (`list`, `create`) to and from the wire types.

use aura_ipc::Line;

use crate::*;

fn flag(l: &Line, k: &str) -> bool {
    l.get(k) == Some("1")
}

fn nets(l: &Line, k: &str) -> Vec<Net> {
    l.get(k).unwrap_or("").split(',').filter_map(Net::parse).collect()
}

fn join<T: ToString>(v: &[T]) -> String {
    v.iter().map(T::to_string).collect::<Vec<_>>().join(",")
}

/// One `tunnel` line of cryptad's `list` as `vpn-use` sees it.
pub fn profile(l: &Line) -> Option<Profile> {
    if l.verb != "tunnel" {
        return None;
    }
    let name = l.get("name").filter(|n| valid_name(n))?.to_string();
    let asking = matches!(l.get("signin"), Some("prompt" | "sso"));
    let state = match (flag(l, "up"), flag(l, "handshake")) {
        (false, _) => State::Down,
        (true, _) if asking => State::SignIn,
        (true, false) => State::Connecting,
        (true, true) => State::Up,
    };
    let scope = if flag(l, "scoped") { Scope::Personas } else { Scope::Machine };
    let kind = match l.get("kind") {
        Some("ac") => Kind::AnyConnect,
        Some("gp") => Kind::GlobalProtect,
        _ => Kind::WireGuard,
    };
    Some(Profile { tunnel: Tunnel { name, kind, state, scope }, block: flag(l, "block"), full: flag(l, "full") })
}

fn text(l: &Line, k: &str) -> Option<String> {
    l.hex(k).and_then(|b| String::from_utf8(b).ok())
}

/// One `tunnel` line as Curia sees it.
pub fn details(l: &Line) -> Option<Details> {
    let profile = profile(l)?;
    let error = text(l, "error");
    if profile.tunnel.kind != Kind::WireGuard {
        let port = l.num("port").unwrap_or(443);
        return Some(Details {
            profile,
            public: String::new(),
            endpoint: std::net::SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port),
            addrs: nets(l, "addr"),
            routes: l.num("routes").unwrap_or(0),
            dns: Vec::new(),
            rx: l.num("rx").unwrap_or(0),
            tx: l.num("tx").unwrap_or(0),
            starts: l.num("starts").unwrap_or(0),
            corporate: Some(Corporate {
                host: l.get("host")?.to_string(),
                port,
                user: text(l, "user"),
                group: text(l, "group"),
                certificate: flag(l, "cert"),
                pinned: flag(l, "pinned"),
            }),
            error,
            banner: None,
        });
    }
    Some(Details {
        profile,
        public: l.get("public").filter(|k| valid_key(k))?.to_string(),
        endpoint: l.get("endpoint")?.parse().ok()?,
        addrs: nets(l, "addrs"),
        routes: l.num("routes").unwrap_or(0),
        dns: l.get("dns").unwrap_or("").split(',').filter_map(|a| a.parse().ok()).collect(),
        rx: l.num("rx").unwrap_or(0),
        tx: l.num("tx").unwrap_or(0),
        starts: l.num("starts").unwrap_or(0),
        corporate: None,
        error,
        banner: None,
    })
}

/// cryptad's `create-ac` for a checked [`CreateAnyConnect`]; the CA file
/// goes as a descriptor.
pub fn create_ac(c: &CreateAnyConnect) -> Line {
    let mut l = Line::new("create-ac").with("name", &c.name).with("host", &c.host).with("scoped", c.personas as u8);
    if let Some(p) = &c.path {
        l = l.with("path", p);
    }
    if let Some(u) = &c.user {
        l = l.with_hex("user", u.as_bytes());
    }
    if let Some(p) = &c.pin {
        l = l.with_hex("pin", p);
    }
    l
}

/// The `text k=banner` lines of `banner`, joined.
pub fn banner(v: &[Line]) -> Option<String> {
    let t: String = v.iter().filter(|l| l.verb == "text" && l.get("k") == Some("banner")).filter_map(|l| text(l, "v")).collect();
    (!t.is_empty()).then_some(t)
}

/// What `prompt` answered.
pub fn ask(v: &[Line]) -> Option<Ask> {
    if let Some(s) = v.iter().find(|l| l.verb == "sso") {
        return Some(Ask::Sso { login: text(s, "login")?, final_url: text(s, "final")? });
    }
    let mut f = Form { title: String::new(), message: String::new(), error: String::new(), banner: String::new(), fields: Vec::new() };
    for l in v {
        match l.verb.as_str() {
            "text" => {
                let t = text(l, "v").unwrap_or_default();
                match l.get("k") {
                    Some("title") => f.title.push_str(&t),
                    Some("message") => f.message.push_str(&t),
                    Some("error") => f.error.push_str(&t),
                    Some("banner") => f.banner.push_str(&t),
                    _ => {}
                }
            }
            "field" => {
                let kind = match l.num::<u8>("kind")? {
                    0 => FieldKind::Text,
                    1 if l.hex("name").as_deref() == Some(b"gp_password_once") => FieldKind::PasswordOnce,
                    1 => FieldKind::Password,
                    2 => FieldKind::Code,
                    3 if l.hex("name").as_deref() == Some(b"gp_gateway") => FieldKind::GatewayChoice,
                    _ => FieldKind::Choice,
                };
                f.fields.push(Field { label: text(l, "label").unwrap_or_default(), kind, choices: Vec::new() });
            }
            "choice" => {
                let i: usize = l.num("i")?;
                let c = (text(l, "value")?, text(l, "label").unwrap_or_default());
                f.fields.get_mut(i)?.choices.push(c);
            }
            _ => {}
        }
    }
    (!f.fields.is_empty()).then_some(Ask::Form(f))
}

/// cryptad's `answer`.
pub fn answer(a: &Answer) -> Line {
    let mut l = Line::new("answer").with("name", &a.name);
    match &a.values {
        None => l = l.with("cancel", 1),
        Some(v) => {
            for (i, x) in v.iter().enumerate() {
                l = match x {
                    Value::Text(t) => l.with_hex(&format!("t{i}"), t.as_bytes()),
                    Value::Held(h) => l.with_hex(&format!("h{i}"), h),
                };
            }
            if a.keep {
                l = l.with("keep", 1);
            }
        }
    }
    l
}

/// cryptad's `create` for a checked [`CreateWireGuard`].
pub fn create(c: &CreateWireGuard) -> Line {
    let mut l = Line::new("create").with("name", &c.name).with("peer", &c.peer).with("endpoint", c.endpoint).with("addrs", join(&c.addrs)).with("allowed", join(&c.allowed));
    if !c.dns.is_empty() {
        l = l.with("dns", join(&c.dns));
    }
    if let Some(k) = c.keepalive {
        l = l.with("keepalive", k);
    }
    l
}

/// The answer to `import` or `create`.
pub fn created(v: &[Line]) -> Option<Created> {
    let l = v.iter().find(|l| l.verb == "imported" || l.verb == "created")?;
    Some(Created {
        name: l.get("name")?.to_string(),
        public: l.get("public").filter(|k| valid_key(k))?.to_string(),
        ignored: l.get("ignored").unwrap_or("").split(',').filter(|s| !s.is_empty()).map(String::from).collect(),
    })
}
