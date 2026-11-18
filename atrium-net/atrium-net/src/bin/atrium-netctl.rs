//! atrium-netctl: a command-line client of atrium-net's control socket
//! (docs/spec/atrium-net.md §7.3; the settings UI will use CLASS_NETCONF).
//! Its Wi-Fi commands take the place of aura-ctl's in the Aura gates.
//!
//!   atrium-netctl status
//!   atrium-netctl scan
//!   atrium-netctl remember SSID [--open] [--manual] [--metered] [--prio N] [--mac P]
//!                                 (passphrase on stdin unless --open)
//!   atrium-netctl forget SSID | list
//!   atrium-netctl join SSID [--open | --wpa3] [--mac P] [--wait SECS] [--dhcp IFACE]
//!   atrium-netctl leave | wifi-status | country CC | watch SECS
//!
//! Global: --sock PATH (/atrium/sockets/atrium-net/ctl.sock).
//!
//! As an app would (CLASS_NETCONF through atrium-netconf, §7.1), on the
//! socket of one capability (network-read, network-details, network-wifi,
//! wifi-nearby, network-configure) under --dir (/atrium/sockets):
//!
//!   atrium-netctl --app CAP [--dir DIR] status | interfaces | routes | list
//!       | join SSID [--open] (passphrase on stdin unless --open; --known: none)
//!       | leave | forget SSID | nearby | country CC | static | watch SECS
//!
//! A refusal prints `refused: <status> <why>` and exits 2 when the
//! capability does not cover the request, 1 otherwise.

use aura_ipc::{Got, Line, Reader};
use std::io::{BufRead, Write};
use std::os::unix::net::UnixStream;
use std::process::exit;
use std::time::{Duration, Instant};

fn die(s: &str) -> ! {
    eprintln!("atrium-netctl: {s}");
    exit(1)
}

struct Peer {
    s: UnixStream,
    r: Reader,
    pending: Vec<Line>,
}

impl Peer {
    fn connect(path: &str) -> Peer {
        let s = UnixStream::connect(path).unwrap_or_else(|e| die(&format!("{path}: {e}")));
        s.set_read_timeout(Some(Duration::from_millis(500))).ok();
        Peer { s, r: Reader::default(), pending: Vec::new() }
    }
    fn send(&mut self, l: &Line) {
        l.send(&mut self.s).unwrap_or_else(|e| die(&format!("send: {e}")));
    }
    fn next(&mut self, end: Instant) -> Option<Line> {
        loop {
            if !self.pending.is_empty() {
                return Some(self.pending.remove(0));
            }
            match self.r.fill(&mut self.s) {
                Ok(v) => self.pending.extend(v.into_iter().filter_map(|g| if let Got::Line(l) = g { Some(l) } else { None })),
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                Err(_) => die("atrium-net closed the connection"),
            }
            if Instant::now() > end {
                return None;
            }
        }
    }
    fn subscribe(&mut self) {
        self.send(&Line::new("subscribe"));
        self.until(Duration::from_secs(10), |l| l.verb == "ok").unwrap_or_else(|| die("no answer to subscribe"));
    }
    /// Lines until `f` matches (returned) or the time runs out.
    fn until(&mut self, d: Duration, mut f: impl FnMut(&Line) -> bool) -> Option<Line> {
        let end = Instant::now() + d;
        while let Some(l) = self.next(end) {
            if f(&l) {
                return Some(l);
            }
        }
        None
    }
    /// Send a request; print and return its reply lines up to ok/error.
    fn call(&mut self, l: &Line, print: bool) -> Vec<Line> {
        self.send(l);
        let mut v = Vec::new();
        let end = Instant::now() + Duration::from_secs(15);
        while let Some(x) = self.next(end) {
            let last = x.verb == "ok" || x.verb == "error";
            if print {
                println!("{}", show(&x));
            }
            v.push(x);
            if last {
                return v;
            }
        }
        die("no answer");
    }
}

fn show(l: &Line) -> String {
    let mut s = l.verb.clone();
    for (k, v) in &l.fields {
        if k == "ssid" {
            let b = aura_ipc::unhex(v).unwrap_or_default();
            if b.iter().all(|c| c.is_ascii_graphic() || *c == b' ') {
                s.push_str(&format!(" ssid=\"{}\"", String::from_utf8_lossy(&b)));
            } else {
                s.push_str(&format!(" ssid=0x{v}"));
            }
        } else {
            s.push_str(&format!(" {k}={v}"));
        }
    }
    s
}

fn ok_or_exit(v: &[Line]) {
    if v.last().is_none_or(|l| l.verb != "ok") {
        exit(1);
    }
}

fn main() {
    let mut sock = "/atrium/sockets/atrium-net/ctl.sock".to_string();
    let mut args: Vec<String> = Vec::new();
    let (mut app, mut dir) = (None, "/atrium/sockets".to_string());
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--sock" => sock = it.next().unwrap_or_default(),
            "--app" => app = Some(it.next().unwrap_or_default()),
            "--dir" => dir = it.next().unwrap_or_default(),
            _ => args.push(a),
        }
    }
    if let Some(cap) = app {
        as_app(&cap, &dir, &args);
    }
    let cmd = args.first().cloned().unwrap_or_default();
    let ssid = || args.get(1).map(|s| s.as_bytes().to_vec()).unwrap_or_else(|| die("SSID missing"));
    let mut p = Peer::connect(&sock);
    match cmd.as_str() {
        "status" => ok_or_exit(&p.call(&Line::new("status"), true)),
        "list" => ok_or_exit(&p.call(&Line::new("wifi-list"), true)),
        "scan" => {
            p.subscribe();
            ok_or_exit(&p.call(&Line::new("wifi-scan"), false));
            let end = Duration::from_secs(60);
            let done = p.until(end, |l| {
                if l.verb == "bss" {
                    println!("{}", show(l));
                }
                l.verb == "scan-done" || l.verb == "error"
            });
            match done {
                Some(l) => {
                    println!("{}", show(&l));
                    exit((l.verb != "scan-done") as i32)
                }
                None => die("scan timed out"),
            }
        }
        "remember" => {
            let s = ssid();
            let mut l = Line::new("wifi-remember").with_hex("ssid", &s);
            let mut open = false;
            let mut a = args[2..].iter();
            while let Some(x) = a.next() {
                match x.as_str() {
                    "--open" => open = true,
                    "--manual" => l = l.with("auto", 0),
                    "--metered" => l = l.with("metered", 1),
                    "--prio" => l = l.with("prio", a.next().cloned().unwrap_or_default()),
                    "--mac" => l = l.with("mac", a.next().cloned().unwrap_or_default()),
                    _ => die("remember SSID [--open] [--manual] [--metered] [--prio N] [--mac P]"),
                }
            }
            if open {
                l = l.with("open", 1);
            } else {
                let mut pass = String::new();
                std::io::stdin().lock().read_line(&mut pass).ok();
                let secret = pass.trim_end_matches(['\n', '\r']).as_bytes().to_vec();
                l = l.with_hex("secret", &secret);
            }
            ok_or_exit(&p.call(&l, true));
        }
        "forget" => ok_or_exit(&p.call(&Line::new("wifi-forget").with_hex("ssid", &ssid()), true)),
        "country" => {
            let cc = args.get(1).cloned().unwrap_or_default().to_ascii_uppercase();
            p.subscribe();
            ok_or_exit(&p.call(&Line::new("country").with("cc", &cc), false));
            // aura-mlme's own answer.
            match p.until(Duration::from_secs(10), |l| (l.verb == "ok" && l.get("country").is_some()) || l.verb == "error") {
                Some(l) => {
                    println!("{}", show(&l));
                    exit((l.verb != "ok") as i32)
                }
                None => die("no answer from aura-mlme"),
            }
        }
        "wifi-status" => {
            p.subscribe();
            ok_or_exit(&p.call(&Line::new("wifi-status"), false));
            match p.until(Duration::from_secs(10), |l| l.verb == "status") {
                Some(l) => println!("{}", show(&l)),
                None => die("no status from aura-mlme"),
            }
        }
        "leave" => {
            p.subscribe();
            ok_or_exit(&p.call(&Line::new("wifi-leave"), false));
            if let Some(l) = p.until(Duration::from_secs(10), |l| l.get("state") == Some("disconnected")) {
                println!("{}", show(&l));
            }
        }
        "watch" => {
            let secs = args.get(1).and_then(|v| v.parse().ok()).unwrap_or(10u64);
            p.subscribe();
            let start = Instant::now();
            let end = start + Duration::from_secs(secs);
            while let Some(l) = p.next(end) {
                println!("{:>7.2}s {}", start.elapsed().as_secs_f64(), show(&l));
                std::io::stdout().flush().ok();
            }
        }
        "join" => {
            let s = ssid();
            let mut l = Line::new("wifi-join").with_hex("ssid", &s);
            let mut wait = 60u64;
            let mut dhcp: Option<String> = None;
            let mut a = args[2..].iter();
            while let Some(x) = a.next() {
                match x.as_str() {
                    "--open" => l = l.with("security", "open"),
                    "--wpa3" => l = l.with("security", "sae"),
                    "--mac" => l = l.with("mac", a.next().cloned().unwrap_or_default()),
                    "--wait" => wait = a.next().and_then(|v| v.parse().ok()).unwrap_or(60),
                    "--dhcp" => dhcp = a.next().cloned(),
                    _ => die("join SSID [--open | --wpa3] [--mac P] [--wait SECS] [--dhcp IFACE]"),
                }
            }
            p.subscribe();
            let start = Instant::now();
            let end = start + Duration::from_secs(wait);
            ok_or_exit(&p.call(&l, false));
            loop {
                let Some(l) = p.next(end) else { die("join timed out") };
                println!("{:>7.2}s {}", start.elapsed().as_secs_f64(), show(&l));
                std::io::stdout().flush().ok();
                match (l.verb.as_str(), l.get("state")) {
                    ("state", Some("authorized")) => break,
                    ("state", Some("failed" | "disconnected")) | ("error", _) => exit(1),
                    _ => {}
                }
            }
            if let Some(ifc) = dhcp {
                // atrium-net runs DHCP itself once the port is open.
                loop {
                    let Some(l) = p.next(end) else { die("no lease in time") };
                    if l.verb == "lease" && l.get("iface") == Some(ifc.as_str()) {
                        let a = l.get("addr").unwrap_or("");
                        let (addr, pfx) = a.split_once('/').unwrap_or((a, "?"));
                        println!("{:>7.2}s address inet {addr} prefix {pfx}", start.elapsed().as_secs_f64());
                        break;
                    }
                }
            }
        }
        _ => {
            eprintln!("usage: atrium-netctl [--sock PATH] status|scan|remember SSID [..]|forget SSID|list|join SSID [..]|leave|wifi-status|country CC|watch S");
            exit(2);
        }
    }
}

/// The CLASS_NETCONF client, as an app with one capability.
fn as_app(cap: &str, dir: &str, args: &[String]) -> ! {
    use atrium_netconf::client::Client;
    use atrium_netconf::{status as st, Fail, Grant, Join};
    let g = Grant::ALL.into_iter().find(|g| g.name() == cap).unwrap_or_else(|| die(&format!("no capability {cap}")));
    let path = Client::path(std::path::Path::new(dir), g);
    let mut c = Client::connect(&path).unwrap_or_else(|e| die(&format!("{}: {e}", path.display())));
    let ssid = || args.get(1).map(|s| s.as_bytes().to_vec()).unwrap_or_else(|| die("SSID missing"));
    let name = |s: &[u8]| String::from_utf8_lossy(s).into_owned();
    let refused = |f: Fail| -> ! {
        let word = match f.0 {
            st::NOT_ALLOWED => "not-allowed",
            st::MALFORMED => "malformed",
            st::UNSUPPORTED => "unsupported",
            st::FAILED => "failed",
            st::NO_WIFI => "no-wifi",
            _ => "unavailable",
        };
        println!("refused: {word} {}", f.1);
        exit(if f.0 == st::NOT_ALLOWED { 2 } else { 1 })
    };
    let show = |s: &atrium_netconf::Status| {
        let mut o = format!("status link={:?} online={:?} metered={}", s.link, s.online, s.metered as u8).to_lowercase();
        if let Some(b) = s.bars {
            o.push_str(&format!(" bars={b}"));
        }
        if let Some(n) = &s.ssid {
            o.push_str(&format!(" ssid=\"{}\"", name(n)));
        }
        if let Some(p) = &s.portal {
            o.push_str(&format!(" portal={p}"));
        }
        o
    };
    let r: Result<(), Fail> = match args.first().map(String::as_str).unwrap_or("") {
        "status" => c.status().map(|s| println!("{}", show(&s))),
        "interfaces" => c.interfaces().map(|v| {
            for i in v {
                println!(
                    "iface name={} wifi={} link={} addr={} router={} dns={} online={:?} egress={}{}",
                    i.name,
                    i.wifi as u8,
                    i.link,
                    i.addr.unwrap_or_default(),
                    i.router.unwrap_or_default(),
                    i.dns.join(","),
                    i.online,
                    i.egress as u8,
                    if i.manual.is_some() { " manual=1" } else { "" }
                );
            }
        }),
        "routes" => c.routes().map(|r| {
            match r.default4 {
                Some((i, gw)) => println!("default via {gw} on {i}"),
                None => println!("no default route"),
            }
            if !r.own_dns.is_empty() {
                println!("own dns {}{}", r.own_dns.join(","), r.dns_tls.as_ref().map(|n| format!(" over TLS ({n})")).unwrap_or_default());
            }
        }),
        "list" => c.profiles().map(|v| {
            for p in v {
                println!(
                    "net ssid=\"{}\" open={} auto={} prio={} metered={} mac={} wpa3_only={}{}",
                    name(&p.ssid),
                    p.open as u8,
                    p.auto as u8,
                    p.prio,
                    p.metered as u8,
                    p.mac,
                    p.wpa3_only as u8,
                    p.manual.map(|m| format!(" manual={}/{}", m.addr, m.prefix)).unwrap_or_default()
                );
            }
        }),
        "join" => {
            let open = args.iter().any(|a| a == "--open");
            let known = args.iter().any(|a| a == "--known");
            let passphrase = (!open && !known).then(|| {
                let mut l = String::new();
                std::io::stdin().lock().read_line(&mut l).ok();
                l.trim_end_matches(['\r', '\n']).to_string()
            });
            c.join(&Join { ssid: ssid(), passphrase, handle: None, open }).map(|_| println!("joined"))
        }
        "leave" => c.leave().map(|_| println!("left")),
        "forget" => c.forget(&ssid()).map(|_| println!("forgotten")),
        "nearby" => c.nearby().map(|v| {
            for n in v {
                println!("nearby ssid=\"{}\" security={} bars={} known={}", name(&n.ssid), n.security, n.bars, n.known as u8);
            }
        }),
        "country" => c.set_country(args.get(1).map(String::as_str).unwrap_or("")).map(|_| println!("ok")),
        // static wifi SSID|wired IFACE [ADDR/PREFIX [ROUTER [DNS,DNS]]]: no address is DHCP.
        "static" => {
            use atrium_netconf::{Manual, SetStatic, Target};
            let what = args.get(2).cloned().unwrap_or_else(|| die("static wifi SSID|wired IFACE [ADDR/PREFIX [ROUTER [DNS,..]]]"));
            let target = match args.get(1).map(String::as_str) {
                Some("wifi") => Target::Wifi(what.into_bytes()),
                Some("wired") => Target::Wired(what),
                _ => die("static wifi SSID|wired IFACE ..."),
            };
            let manual = args.get(3).map(|a| {
                let (ip, p) = a.split_once('/').unwrap_or_else(|| die("ADDR/PREFIX"));
                Manual {
                    addr: ip.parse().unwrap_or_else(|_| die("bad address")),
                    prefix: p.parse().unwrap_or_else(|_| die("bad prefix")),
                    router: args.get(4).filter(|r| *r != "-").map(|r| r.parse().unwrap_or_else(|_| die("bad router"))),
                    dns: args.get(5).map(|d| d.split(',').map(|x| x.parse().unwrap_or_else(|_| die("bad DNS server"))).collect()).unwrap_or_default(),
                }
            });
            c.set_static(&SetStatic { target, manual }).map(|_| println!("ok"))
        }
        // dns [A,B,C [tls NAME]]: the machine's own DNS servers, over TLS
        // to a certificate for NAME if given; none: each network's.
        "dns" => {
            let v: Vec<std::net::IpAddr> = args.get(1).map(|d| d.split(',').map(|x| x.parse().unwrap_or_else(|_| die("bad DNS server"))).collect()).unwrap_or_default();
            match (args.get(2).map(String::as_str), args.get(3)) {
                (Some("tls"), Some(n)) => c.set_dns_tls(&atrium_netconf::DnsTls { servers: v, name: n.clone() }).map(|_| println!("ok")),
                (None, _) => c.set_dns(&v).map(|_| println!("ok")),
                _ => die("dns [A,B,C [tls NAME]]"),
            }
        }
        // network-portal: what a sign-in window would get, and the watch.
        "portal" => match args.get(1).map(String::as_str) {
            Some("watch") => c.portal_watch(args.get(2).map(String::as_str) != Some("off")).map(|_| println!("ok")),
            _ => c.portal_info().map(|p| {
                let ssid = p.ssid.as_ref().map(|s| format!(" ssid={}", name(s))).unwrap_or_default();
                println!(
                    "portal iface={}{ssid} online={:?} start={} dns={} watching={}",
                    p.iface,
                    p.online,
                    p.start_url().unwrap_or_else(|| "-".into()),
                    p.dns.join(","),
                    p.watching as u8
                )
            }),
        },
        "printers" => {
            let secs: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(5);
            c.print_discovery().map(|(iface, r, w)| printers(&iface, r, w, secs))
        }
        "watch" => {
            let secs: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(10);
            let mut ch = c.subscribe().unwrap_or_else(|f| refused(f));
            let end = Instant::now() + Duration::from_secs(secs);
            while let Some(left) = end.checked_duration_since(Instant::now()) {
                match ch.next_within(left) {
                    Ok(Some(s)) => {
                        println!("{}", show(&s));
                        std::io::stdout().flush().ok();
                    }
                    Ok(None) => break,
                    Err(e) => die(&format!("watch: {e}")),
                }
            }
            Ok(())
        }
        _ => die("--app CAP status|interfaces|routes|list|join SSID [--open|--known]|leave|forget SSID|nearby|country CC|static|watch SECS|printers SECS|portal [watch on|off]"),
    };
    match r {
        Ok(()) => exit(0),
        Err(f) => refused(f),
    }
}

/// `--app print-discovery printers SECS`: the two mDNS sockets checked, a
/// QU query for IPP printers sent, and every answer for SECS seconds shown
/// (a check tool; prelum-browse is the real reader).
fn printers(iface: &str, r: std::os::fd::OwnedFd, w: std::os::fd::OwnedFd, secs: u64) {
    use std::net::UdpSocket;
    let (r, w) = (UdpSocket::from(r), UdpSocket::from(w));
    println!("sockets iface={iface} recv={} send={} peer={}", show_addr(r.local_addr()), show_addr(w.local_addr()), show_addr(w.peer_addr()));
    let refused = |x: std::io::Result<usize>| if x.is_err() { "refused" } else { "SENT" };
    println!("recv-socket-send={}", refused(r.send_to(b"x", "224.0.0.251:5353")));
    println!("send-socket-elsewhere={}", refused(w.send_to(b"x", "10.0.2.2:53")));
    // One message, two questions, the unicast-response bit set.
    let mut q = vec![0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0];
    for svc in [&b"_ipp"[..], b"_ipps"] {
        for l in [svc, b"_tcp", b"local"] {
            q.push(l.len() as u8);
            q.extend_from_slice(l);
        }
        q.extend_from_slice(&[0, 0, 12, 0x80, 1]);
    }
    match w.send(&q) {
        Ok(_) => println!("query sent"),
        Err(e) => die(&format!("query: {e}")),
    }
    let end = Instant::now() + Duration::from_secs(secs);
    let mut buf = [0u8; 9000];
    let mut n = 0;
    while let Some(left) = end.checked_duration_since(Instant::now()) {
        r.set_read_timeout(Some(left.max(Duration::from_millis(1)))).ok();
        let Ok((len, from)) = r.recv_from(&mut buf) else { break };
        n += 1;
        let m = &buf[..len];
        if m.len() >= 12 && m[2] & 0x80 != 0 && n <= 20 {
            println!("answer from={from} len={len}{}", dns_summary(m));
        }
    }
    println!("datagrams={n}");
}

fn show_addr(a: std::io::Result<std::net::SocketAddr>) -> String {
    a.map(|a| a.to_string()).unwrap_or_else(|_| "-".into())
}

/// A name at `i` (pointers followed, at most 16 jumps and 255 bytes).
fn dns_name(m: &[u8], mut i: usize) -> Option<(String, usize)> {
    let (mut out, mut end, mut jumps) = (String::new(), None, 0);
    loop {
        let l = *m.get(i)? as usize;
        if l == 0 {
            return Some((out, end.unwrap_or(i + 1)));
        }
        if l & 0xc0 == 0xc0 {
            jumps += 1;
            if jumps > 16 {
                return None;
            }
            end.get_or_insert(i + 2);
            i = ((l & 0x3f) << 8) | *m.get(i + 1)? as usize;
            continue;
        }
        let lab = m.get(i + 1..i + 1 + l)?;
        if !out.is_empty() {
            out.push('.');
        }
        out.push_str(&String::from_utf8_lossy(lab));
        if out.len() > 255 {
            return None;
        }
        i += 1 + l;
    }
}

/// PTR targets, SRV ports and A addresses in an answer.
fn dns_summary(m: &[u8]) -> String {
    let u16at = |i: usize| m.get(i..i + 2).map(|b| u16::from_be_bytes([b[0], b[1]]));
    let (qd, an, ns, ar) = (u16at(4).unwrap_or(0), u16at(6).unwrap_or(0), u16at(8).unwrap_or(0), u16at(10).unwrap_or(0));
    let mut i = 12;
    let mut out = format!(" an={an}");
    for _ in 0..qd {
        let Some((_, e)) = dns_name(m, i) else { return out + " bad" };
        i = e + 4;
    }
    for _ in 0..(an as usize + ns as usize + ar as usize).min(64) {
        let Some((_, e)) = dns_name(m, i) else { return out + " bad" };
        let (Some(t), Some(len)) = (u16at(e), u16at(e + 8)) else { return out + " bad" };
        let rd = e + 10;
        match t {
            12 => {
                if let Some((n, _)) = dns_name(m, rd) {
                    out += &format!(" ptr=\"{n}\"");
                }
            }
            33 => {
                if let Some(p) = u16at(rd + 4) {
                    out += &format!(" srv={p}");
                }
            }
            1 if len == 4 => {
                if let Some(a) = m.get(rd..rd + 4) {
                    out += &format!(" a={}.{}.{}.{}", a[0], a[1], a[2], a[3]);
                }
            }
            _ => {}
        }
        i = rd + len as usize;
    }
    out
}
