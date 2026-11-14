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
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--sock" => sock = it.next().unwrap_or_default(),
            _ => args.push(a),
        }
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
