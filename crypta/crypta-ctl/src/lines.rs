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
    let state = match (flag(l, "up"), flag(l, "handshake")) {
        (false, _) => State::Down,
        (true, false) => State::Connecting,
        (true, true) => State::Up,
    };
    let scope = if flag(l, "scoped") { Scope::Personas } else { Scope::Machine };
    Some(Profile { tunnel: Tunnel { name, kind: Kind::WireGuard, state, scope }, block: flag(l, "block"), full: flag(l, "full") })
}

/// One `tunnel` line as Curia sees it.
pub fn details(l: &Line) -> Option<Details> {
    let profile = profile(l)?;
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
    })
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
