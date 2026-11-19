//! atrium-vpnctl: CLASS_VPN from a shell, through one capability's socket
//! under --dir (/atrium/sockets) — what an app with that capability may do,
//! no more (vpn.md §7.2).
//!
//!   atrium-vpnctl [--dir D] [--grant read|use|configure] status|watch|list
//!   atrium-vpnctl ... connect NAME | disconnect NAME | details NAME | remove NAME
//!   atrium-vpnctl ... set NAME [block=0|1] [personas=0|1]
//!   atrium-vpnctl ... create NAME PEER ENDPOINT ADDRS ALLOWED [DNS]
//!   atrium-vpnctl ... import NAME < file.conf
//!
//! With no arguments it is a probe for a jailed app: it tries each socket
//! it can see and prints one line, `vpn-probe: ...`, of what each allowed.

use std::io::Read;
use std::path::PathBuf;

use crypta_ctl::client::Client;
use crypta_ctl::{status, CreateWireGuard, Fail, Grant, ImportWireGuard, Net, SetProfile};

fn die(s: &str) -> ! {
    eprintln!("atrium-vpnctl: {s}");
    std::process::exit(2)
}

fn word(f: &Fail) -> String {
    match f.0 {
        status::NOT_ALLOWED => "not-allowed".into(),
        status::MALFORMED => "malformed".into(),
        status::UNAVAILABLE => "unavailable".into(),
        _ => format!("failed({})", f.1.replace(' ', "-")),
    }
}

fn nets(s: &str) -> Vec<Net> {
    s.split(',').filter(|x| !x.is_empty()).map(|x| Net::parse(x).unwrap_or_else(|| die(&format!("{x}: not a network")))).collect()
}

/// What a jailed app can do: each socket in turn.
fn probe(dir: &PathBuf) -> i32 {
    let mut out = Vec::new();
    let try_create = |c: &mut Client| {
        let req = CreateWireGuard {
            name: "probe-made".into(),
            peer: "xTIBA5rboUvnH4htodjb6e697QjLERt1NAB4mZqp8Dg=".into(),
            endpoint: "198.51.100.99:51820".parse().unwrap(),
            addrs: vec![Net::parse("10.98.0.2/32").unwrap()],
            allowed: vec![Net::parse("10.98.0.0/24").unwrap()],
            dns: vec![],
            keepalive: None,
        };
        match c.create(&req) {
            Ok(m) => {
                let _ = c.remove("probe-made");
                format!("made({})", m.public)
            }
            Err(f) => word(&f),
        }
    };
    for g in Grant::ALL {
        let p = Client::path(dir, g);
        let n = g.name().trim_start_matches("vpn-");
        let Ok(mut c) = Client::connect(&p) else {
            out.push(format!("{n}=absent"));
            continue;
        };
        let st = match c.status() {
            Ok(v) => format!("{}tunnels", v.len()),
            Err(f) => word(&f),
        };
        let list = match c.list() {
            Ok(v) => format!("{}profiles", v.len()),
            Err(f) => word(&f),
        };
        out.push(format!("{n}.status={st} {n}.list={list} {n}.create={}", try_create(&mut c)));
    }
    println!("vpn-probe: {}", out.join(" "));
    0
}

fn main() {
    let mut a: Vec<String> = std::env::args().skip(1).collect();
    let mut dir = PathBuf::from("/atrium/sockets");
    let mut grant = Grant::Read;
    loop {
        match a.first().map(String::as_str) {
            Some("--dir") if a.len() > 1 => {
                dir = PathBuf::from(&a[1]);
                a.drain(..2);
            }
            Some("--grant") if a.len() > 1 => {
                grant = match a[1].as_str() {
                    "read" => Grant::Read,
                    "use" => Grant::Use,
                    "configure" => Grant::Configure,
                    g => die(&format!("{g}: read, use or configure")),
                };
                a.drain(..2);
            }
            _ => break,
        }
    }
    if a.is_empty() {
        std::process::exit(probe(&dir));
    }
    let mut c = Client::connect(&Client::path(&dir, grant)).unwrap_or_else(|e| die(&format!("{}: {e}", grant.name())));
    let arg = |i: usize| a.get(i).cloned().unwrap_or_else(|| die("missing argument"));
    let r: Result<String, Fail> = match a[0].as_str() {
        "status" => c.status().map(|v| v.iter().map(|t| format!("tunnel name={} kind={:?} state={:?} scope={:?}\n", t.name, t.kind, t.state, t.scope)).collect()),
        "watch" => {
            let mut ch = c.subscribe().unwrap_or_else(|f| die(&word(&f)));
            let n: usize = a.get(1).and_then(|n| n.parse().ok()).unwrap_or(1);
            for _ in 0..n {
                match ch.next() {
                    Ok(v) => println!("event {}", v.iter().map(|t| format!("{}={:?}", t.name, t.state)).collect::<Vec<_>>().join(" ")),
                    Err(e) => die(&e.to_string()),
                }
            }
            Ok(String::new())
        }
        "list" => c.list().map(|v| v.iter().map(|p| format!("profile name={} state={:?} scope={:?} block={} full={}\n", p.tunnel.name, p.tunnel.state, p.tunnel.scope, p.block as u8, p.full as u8)).collect()),
        "connect" => c.connect_tunnel(&arg(1)).map(|_| "ok\n".into()),
        "disconnect" => c.disconnect_tunnel(&arg(1)).map(|_| "ok\n".into()),
        "remove" => c.remove(&arg(1)).map(|_| "ok\n".into()),
        "details" => c.details(&arg(1)).map(|d| {
            let addrs = d.addrs.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(",");
            format!("details name={} state={:?} public={} endpoint={} addrs={addrs} routes={} rx={} tx={}\n", d.profile.tunnel.name, d.profile.tunnel.state, d.public, d.endpoint, d.routes, d.rx, d.tx)
        }),
        "set" => {
            let mut s = SetProfile { name: arg(1), block: None, personas: None };
            for kv in &a[2..] {
                match kv.split_once('=') {
                    Some(("block", v)) => s.block = Some(v == "1"),
                    Some(("personas", v)) => s.personas = Some(v == "1"),
                    _ => die(&format!("{kv}: block= or personas=")),
                }
            }
            c.set(&s).map(|_| "ok\n".into())
        }
        "create" => {
            let req = CreateWireGuard {
                name: arg(1),
                peer: arg(2),
                endpoint: arg(3).parse().unwrap_or_else(|_| die("ENDPOINT: a.b.c.d:port")),
                addrs: nets(&arg(4)),
                allowed: nets(&arg(5)),
                dns: a.get(6).map(|d| d.split(',').filter_map(|x| x.parse().ok()).collect()).unwrap_or_default(),
                keepalive: Some(25),
            };
            c.create(&req).map(|m| format!("created name={} public={}\n", m.name, m.public))
        }
        "import" => {
            let mut conf = Vec::new();
            std::io::stdin().take(crypta_ctl::MAX_CONF as u64 + 1).read_to_end(&mut conf).unwrap_or_else(|e| die(&e.to_string()));
            c.import(&ImportWireGuard { name: arg(1), conf }).map(|m| format!("imported name={} public={} ignored={}\n", m.name, m.public, m.ignored.join(",")))
        }
        v => die(&format!("{v}: unknown")),
    };
    match r {
        Ok(s) => print!("{s}"),
        Err(f) => {
            println!("refused {} ({})", word(&f), f.1);
            std::process::exit(1);
        }
    }
}
