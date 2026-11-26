//! `listen` for a running app (background-apps.md §4): which ports it gets
//! now, from where, and keeping that true while the network changes.
//!
//! The network a `local-network` port holds on is named by the person's
//! grant: `wifi-<ssid hex>` for a saved Wi-Fi network with a password, or
//! `wired-<gateway MAC>` for a wired one. atrium-net says which network the
//! egress is on; an open Wi-Fi network has no identity, so nothing holds
//! there. Each change of what atrium-net reports re-renders every running
//! listener's rules through jaild (`SetListen`), which drops a rule before
//! anything else when the network is gone.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use jaild::protocol::{ListenFrom, NetListen};
use portcullis_policy::{Policy, PERSONAL};

pub const NETD_SOCK: &str = "/atrium/sockets/atrium-net/ctl.sock";
/// Bounds on what atrium-net's answer may be.
const MAX_LINES: usize = 64;
const MAX_LINE: usize = 4096;

/// The network the egress is on now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Net {
    pub iface:  String,
    /// The egress address's own network, `a.b.c.d/n`.
    pub subnet: String,
    /// `None`: a network a grant can't name (open Wi-Fi, or a gateway with
    /// no known MAC); `why` says which.
    pub id:     Option<String>,
    pub why:    Option<&'static str>,
}

/// One line of atrium-net's ctl answer: the verb and its `k=v` pairs.
fn fields(line: &str) -> (&str, HashMap<&str, &str>) {
    let mut it = line.split(' ');
    let verb = it.next().unwrap_or("");
    (verb, it.filter_map(|kv| kv.split_once('=')).collect())
}

fn subnet_of(addr: &str) -> Option<String> {
    let (a, p) = addr.split_once('/')?;
    let a: std::net::Ipv4Addr = a.parse().ok()?;
    let p: u32 = p.parse().ok().filter(|p| (8..=32).contains(p))?;
    let mask = if p == 32 { u32::MAX } else { !(u32::MAX >> p) };
    Some(format!("{}/{p}", std::net::Ipv4Addr::from(u32::from(a) & mask)))
}

fn iface_ok(i: &str) -> bool {
    !i.is_empty() && i.len() <= 15 && i.as_bytes()[0].is_ascii_lowercase()
        && i.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// The egress network from atrium-net's `status` lines. `gw_mac` finds a
/// router's MAC (the ARP table).
pub fn parse_status(lines: &[String], mut gw_mac: impl FnMut(&str, &str) -> Option<String>) -> Option<Net> {
    for l in lines {
        let (verb, f) = fields(l);
        if verb != "iface" || f.get("egress") != Some(&"1") || f.get("state") != Some(&"bound") || f.get("link") != Some(&"up") {
            continue;
        }
        let iface = f.get("name").copied().filter(|i| iface_ok(i))?.to_string();
        let subnet = subnet_of(f.get("addr")?)?;
        let (id, why) = if f.get("wifi") == Some(&"1") {
            match (f.get("ssid"), f.get("secured")) {
                (Some(s), Some(&"1")) if !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_hexdigit()) =>
                    (Some(format!("wifi-{}", s.to_ascii_lowercase())), None),
                (Some(_), _) => (None, Some("open Wi-Fi networks can't be allowed")),
                _ => (None, Some("this Wi-Fi network isn't known yet")),
            }
        } else {
            match f.get("router").and_then(|r| gw_mac(&iface, r)) {
                Some(m) => (Some(format!("wired-{m}")), None),
                None => (None, Some("this network's router isn't known yet")),
            }
        };
        return Some(Net { iface, subnet, id, why });
    }
    None
}

/// The router's MAC on `iface`, as 12 lowercase hex, from the ARP table.
fn arp_mac(iface: &str, router: &str) -> Option<String> {
    let r: std::net::Ipv4Addr = router.parse().ok()?;
    // Read to the end rather than `output()`: portcullisd reaps children on
    // other threads, so the exit status can be gone; the table isn't.
    let mut child = std::process::Command::new("/usr/sbin/arp").args(["-n", &r.to_string()])
        .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null())
        .spawn().ok()?;
    let mut text = String::new();
    child.stdout.take()?.take(4096).read_to_string(&mut text).ok()?;
    let _ = child.wait();
    // "? (192.168.1.1) at 52:55:0a:00:02:02 on vtnet0 expires in 1199 seconds [ethernet]"
    let line = text.lines().find(|l| l.contains(&format!(" on {iface} ")) || l.ends_with(&format!(" on {iface}")))?;
    let mut w = line.split_whitespace();
    w.find(|t| *t == "at")?;
    let mac = w.next()?;
    let hex: String = mac.split(':').collect();
    (mac.split(':').count() == 6 && hex.len() == 12 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| hex.to_ascii_lowercase())
}

fn netd(req: &str) -> Option<Vec<String>> {
    let mut s = UnixStream::connect(NETD_SOCK).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    s.write_all(format!("{req}\n").as_bytes()).ok()?;
    let mut r = BufReader::new(s.take((MAX_LINES * MAX_LINE) as u64));
    let mut v = Vec::new();
    for _ in 0..MAX_LINES {
        let mut l = String::new();
        if r.read_line(&mut l).ok()? == 0 {
            return None;
        }
        let l = l.trim_end().to_string();
        if l == "ok" || l.starts_with("ok ") {
            return Some(v);
        }
        if l == "error" || l.starts_with("error ") {
            return None;
        }
        v.push(l);
    }
    None
}

/// The egress network now; `None` when atrium-net says there is none (or
/// can't be asked): then no `local-network` port holds.
pub fn current() -> Option<Net> {
    // A router named once stays named while atrium-net keeps reporting the
    // same interface and router: one missed ARP lookup (an entry being
    // refreshed) mustn't close the person's ports. Any answer with no
    // network (link down, lease lost) forgets it.
    static KNOWN: Mutex<Option<(String, String, String)>> = Mutex::new(None);
    let mut known = KNOWN.lock().unwrap_or_else(|e| e.into_inner());
    let n = netd("status").and_then(|l| parse_status(&l, |iface, router| {
        match arp_mac(iface, router) {
            Some(m) => {
                *known = Some((iface.into(), router.into(), m.clone()));
                Some(m)
            }
            None => known.as_ref().filter(|(i, r, _)| i == iface && r == router).map(|(_, _, m)| m.clone()),
        }
    }));
    if n.is_none() {
        *known = None;
    }
    drop(known);
    // Said once per change: what every listener's ports now follow.
    static SEEN: Mutex<Option<Option<Net>>> = Mutex::new(None);
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if seen.as_ref() != Some(&n) {
        match &n {
            Some(x) => eprintln!("portcullisd: listen: network now {} on {} ({})", x.id.as_deref().unwrap_or("unnamed"), x.iface, x.subnet),
            None => eprintln!("portcullisd: listen: no network"),
        }
        *seen = Some(n.clone());
    }
    n
}

/// A running instance whose manifest asks for `listen`.
#[derive(Debug, Clone)]
pub struct ListenOf {
    pub user:    String,
    pub app_id:  String,
    pub persona: Option<String>,
    pub hash:    String,
    /// (port, udp, from the local network)
    pub ports:   Vec<(u16, bool, bool)>,
}

impl ListenOf {
    pub fn of(user: &str, m: &portcullis_toml::Manifest, text: &str, persona: Option<&str>) -> Option<ListenOf> {
        let ports: Vec<(u16, bool, bool)> = m.listen().iter().map(|l| (
            l.port,
            l.proto == portcullis_toml::Proto::Udp,
            l.from == portcullis_toml::ListenFrom::LocalNetwork,
        )).collect();
        (!ports.is_empty()).then(|| ListenOf {
            user: user.into(), app_id: m.app.id.clone(), persona: persona.map(String::from),
            hash: portcullis_policy::hash_manifest(text.as_bytes()), ports,
        })
    }
}

/// What `lo` gets on `net` under `policy`: nothing without the person's
/// grant for this manifest and persona; `this-computer` ports with it;
/// `local-network` ports only on a network the grant names.
pub fn compute(lo: &ListenOf, policy: &Policy, owner_uid: u32, net: Option<&Net>) -> Vec<NetListen> {
    let Some(g) = policy.listen_grant(&lo.app_id, &lo.hash) else { return Vec::new() };
    if g.persona != lo.persona.as_deref().unwrap_or(PERSONAL) {
        return Vec::new();
    }
    let lan = net.filter(|n| n.id.as_ref().is_some_and(|id| g.networks.contains(id)));
    lo.ports.iter().filter_map(|&(port, udp, from_lan)| {
        let from = if from_lan {
            let n = lan?;
            ListenFrom::Lan { iface: n.iface.clone(), subnet: n.subnet.clone() }
        } else {
            if udp {
                return None;
            }
            ListenFrom::ThisComputer { owner_uid, owner: lo.user.clone() }
        };
        Some(NetListen { port, udp, from })
    }).collect()
}


/// `lo`'s listen entries now, read fresh from the person's policy.
pub fn now(lo: &ListenOf) -> Vec<NetListen> {
    let Ok(p) = Policy::load(&Policy::user_path(&lo.user)) else { return Vec::new() };
    let Some(uid) = crate::uid_of_user(&lo.user) else { return Vec::new() };
    let net = if lo.ports.iter().any(|p| p.2) { current() } else { None };
    compute(lo, &p, uid, net.as_ref())
}

fn tracked() -> &'static Mutex<HashMap<String, (ListenOf, Vec<NetListen>)>> {
    static T: OnceLock<Mutex<HashMap<String, (ListenOf, Vec<NetListen>)>>> = OnceLock::new();
    T.get_or_init(Default::default)
}

/// `jail` runs with `applied` from `lo`.
pub fn track(jail: &str, lo: ListenOf, applied: Vec<NetListen>) {
    tracked().lock().unwrap_or_else(|e| e.into_inner()).insert(jail.into(), (lo, applied));
}

/// For the bar: how many connections `user`'s `app_id` in `persona` has,
/// and whether its local-network ports are off on the network the
/// computer is on now although the person allowed them on some network.
pub fn bar_status(user: &str, app_id: &str, persona: Option<&str>) -> Option<(Option<u32>, bool)> {
    let (jail, lo, applied) = tracked().lock().unwrap_or_else(|e| e.into_inner()).iter()
        .find(|(_, (lo, _))| lo.user == user && lo.app_id == app_id && lo.persona.as_deref() == persona)
        .map(|(j, (lo, a))| (j.clone(), lo.clone(), a.clone()))?;
    let wants_lan = lo.ports.iter().any(|p| p.2)
        && Policy::load(&Policy::user_path(user)).ok()
            .and_then(|p| p.listen_grant(&lo.app_id, &lo.hash).map(|g| !g.networks.is_empty()))
            .unwrap_or(false);
    let lan_on = applied.iter().any(|l| matches!(l.from, ListenFrom::Lan { .. }));
    let conns = (!applied.is_empty()).then(|| portcullisd::jaild_jail::listen_stats(&jail)).flatten();
    Some((conns, wants_lan && !lan_on))
}

/// The running instance of `user`'s, in `persona`, whose this-computer
/// `port` is applied now: its jail and app id.
pub fn local_target(user: &str, persona: Option<&str>, port: u16) -> Option<(String, String)> {
    tracked().lock().unwrap_or_else(|e| e.into_inner()).iter()
        .find(|(_, (lo, applied))| lo.user == user
            && lo.persona.as_deref().unwrap_or(PERSONAL) == persona.unwrap_or(PERSONAL)
            && applied.iter().any(|l| l.port == port && !l.udp && matches!(l.from, ListenFrom::ThisComputer { .. })))
        .map(|(j, (lo, _))| (j.clone(), lo.app_id.clone()))
}

pub fn untrack(jail: &str) {
    tracked().lock().unwrap_or_else(|e| e.into_inner()).remove(jail);
}

/// Bring every running listener (or `user`'s `app_id`'s) to what holds now.
pub fn refresh(only: Option<(&str, &str)>) {
    let jobs: Vec<(String, ListenOf, Vec<NetListen>)> = tracked().lock().unwrap_or_else(|e| e.into_inner()).iter()
        .filter(|(_, (lo, _))| only.is_none_or(|(u, a)| lo.user == u && lo.app_id == a))
        .map(|(j, (lo, a))| (j.clone(), lo.clone(), a.clone())).collect();
    for (jail, lo, applied) in jobs {
        let want = now(&lo);
        if want == applied {
            continue;
        }
        match portcullisd::jaild_jail::set_listen(&jail, want.clone()) {
            Ok(()) => {
                eprintln!("portcullisd: listen: {} for {}: {} port(s) now", lo.app_id, lo.user, want.len());
                if let Some(e) = tracked().lock().unwrap_or_else(|e| e.into_inner()).get_mut(&jail) {
                    e.1 = want;
                }
            }
            Err(why) => {
                eprintln!("portcullisd: listen: {} for {}: {why}; closing its ports", lo.app_id, lo.user);
                let _ = portcullisd::jaild_jail::set_listen(&jail, Vec::new());
                if let Some(e) = tracked().lock().unwrap_or_else(|e| e.into_inner()).get_mut(&jail) {
                    e.1 = Vec::new();
                }
            }
        }
    }
}

/// Follow atrium-net: every event it sends (a lease, a lost lease, an
/// egress change, a Wi-Fi state) re-checks the listeners at once, and so
/// do 5 quiet seconds, so a missed event costs at most that. A check that
/// changes nothing asks jaild nothing. Runs for portcullisd's life.
pub fn watch() {
    std::thread::spawn(|| loop {
        if let Ok(mut s) = UnixStream::connect(NETD_SOCK) {
            let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
            if s.write_all(b"subscribe\n").is_ok() {
                let mut r = BufReader::new(s);
                loop {
                    let mut l = String::new();
                    match (&mut r).take(MAX_LINE as u64).read_line(&mut l) {
                        Ok(0) => break,
                        Ok(_) => refresh(None),
                        Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => refresh(None),
                        Err(_) => break,
                    }
                }
            }
        }
        // No atrium-net: nothing on the LAN holds; this computer still does.
        refresh(None);
        std::thread::sleep(Duration::from_secs(5));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(extra: &str) -> Vec<String> {
        vec![
            "iface name=vtnet1 mac=x state=none link=down internet=no".into(),
            format!("iface name=vtnet0 mac=x state=bound addr=192.168.7.42/24 router=192.168.7.1 link=up internet=yes egress=1{extra}"),
        ]
    }

    #[test]
    fn the_egress_network_is_named_by_its_router_or_its_saved_wifi() {
        let n = parse_status(&st(""), |_, r| (r == "192.168.7.1").then(|| "52550a000202".into())).unwrap();
        assert_eq!(n, Net { iface: "vtnet0".into(), subnet: "192.168.7.0/24".into(), id: Some("wired-52550a000202".into()), why: None });
        let w = parse_status(&st(" wifi=1 ssid=486f6d65 secured=1"), |_, _| None).unwrap();
        assert_eq!(w.id.as_deref(), Some("wifi-486f6d65"));
        let open = parse_status(&st(" wifi=1 ssid=436166e9 secured=0"), |_, _| None).unwrap();
        assert_eq!(open.id, None, "an open network has no identity");
        assert!(open.why.is_some());
        assert_eq!(parse_status(&st("").into_iter().map(|l| l.replace("link=up", "link=lost")).collect::<Vec<_>>(), |_, _| None), None,
                   "no network while the link is lost");
        assert_eq!(parse_status(&st("").into_iter().map(|l| l.replace("vtnet0", "vt;net0")).collect::<Vec<_>>(), |_, _| None), None);
    }

    #[test]
    fn ports_hold_only_where_the_person_allowed_them() {
        let lo = ListenOf { user: "alice".into(), app_id: "a.srv".into(), persona: None, hash: "h".into(),
                            ports: vec![(8080, false, true), (9000, false, false)] };
        let mut p = Policy::default();
        let home = Net { iface: "vtnet0".into(), subnet: "192.168.7.0/24".into(), id: Some("wired-52550a000202".into()), why: None };
        let cafe = Net { id: Some("wifi-436166e9".into()), ..home.clone() };
        assert!(compute(&lo, &p, 1077, Some(&home)).is_empty(), "nothing without the grant");
        p.set_listen("a.srv", PERSONAL, "h", None, true).unwrap();
        let only_me = compute(&lo, &p, 1077, Some(&home));
        assert_eq!(only_me, vec![NetListen { port: 9000, udp: false,
            from: ListenFrom::ThisComputer { owner_uid: 1077, owner: "alice".into() } }]);
        p.set_listen("a.srv", PERSONAL, "h", Some("wired-52550a000202"), true).unwrap();
        assert_eq!(compute(&lo, &p, 1077, Some(&home)).len(), 2);
        assert_eq!(compute(&lo, &p, 1077, Some(&cafe)), only_me, "not on another network");
        assert_eq!(compute(&lo, &p, 1077, None), only_me, "not with no network");
        let work = ListenOf { persona: Some("work".into()), ..lo.clone() };
        assert!(compute(&work, &p, 1077, Some(&home)).is_empty(), "the grant's persona only");
        let other = ListenOf { hash: "h2".into(), ..lo };
        assert!(compute(&other, &p, 1077, Some(&home)).is_empty(), "another manifest");
    }
}
