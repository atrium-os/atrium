//! aura-ctl: what atrium-net does for Wi-Fi (docs/spec/wifi.md §3.5), as a
//! command, until atrium-net exists. Talks to aura-mlme and aura-keys over
//! their sockets (as root, the `--ctl-uid` both daemons accept).
//!
//!   aura-ctl scan
//!   aura-ctl remember SSID        (passphrase on stdin; stored in Arca)
//!   aura-ctl forget SSID
//!   aura-ctl join SSID [--open] [--wait SECS] [--dhcp IFACE]
//!   aura-ctl leave | status
//!
//! Global: --mlme PATH (/atrium/sockets/aura/mlme.sock), --keys PATH
//! (/atrium/sockets/aura-keys/keys.sock). Exit status 0 on success.

use aura_ipc::{Got, Line, Reader};
use std::io::{BufRead, Write};
use std::os::unix::net::UnixStream;
use std::process::{exit, Command};
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

fn die(s: &str) -> ! {
    eprintln!("aura-ctl: {s}");
    exit(1)
}

struct Peer {
    s: UnixStream,
    r: Reader,
}

impl Peer {
    fn connect(path: &str) -> Peer {
        let s = UnixStream::connect(path).unwrap_or_else(|e| die(&format!("{path}: {e}")));
        s.set_read_timeout(Some(Duration::from_millis(500))).ok();
        Peer { s, r: Reader::default() }
    }
    fn send(&mut self, l: &Line) {
        l.send(&mut self.s).unwrap_or_else(|e| die(&format!("send: {e}")));
    }
    /// Next line, waiting until `end`.
    fn next(&mut self, end: Instant) -> Option<Line> {
        loop {
            match self.r.fill(&mut self.s) {
                Ok(v) => {
                    if let Some(Got::Line(l)) = v.into_iter().next() {
                        return Some(l);
                    }
                }
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                Err(_) => die("daemon closed the connection"),
            }
            if Instant::now() > end {
                return None;
            }
        }
    }
    fn call(&mut self, l: &Line) -> Line {
        self.send(l);
        self.next(Instant::now() + Duration::from_secs(10)).unwrap_or_else(|| die("no reply"))
    }
}

fn show(l: &Line) -> String {
    let mut s = l.verb.clone();
    for (k, v) in &l.fields {
        if k == "ssid" {
            let b = aura_ipc::unhex(v).unwrap_or_default();
            let printable = b.iter().all(|c| c.is_ascii_graphic() || *c == b' ');
            if printable {
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

fn main() {
    let mut mlme = "/atrium/sockets/aura/mlme.sock".to_string();
    let mut keys = "/atrium/sockets/aura-keys/keys.sock".to_string();
    let mut args: Vec<String> = Vec::new();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--mlme" => mlme = it.next().unwrap_or_default(),
            "--keys" => keys = it.next().unwrap_or_default(),
            _ => args.push(a),
        }
    }
    let cmd = args.first().map(String::as_str).unwrap_or("");
    let ssid_arg = || args.get(1).map(|s| s.as_bytes().to_vec()).unwrap_or_else(|| die("SSID missing"));
    match cmd {
        "scan" => {
            let mut p = Peer::connect(&mlme);
            p.send(&Line::new("scan"));
            let end = Instant::now() + Duration::from_secs(60);
            while let Some(l) = p.next(end) {
                println!("{}", show(&l));
                if l.verb == "scan-done" || l.verb == "error" {
                    exit(if l.verb == "error" { 1 } else { 0 });
                }
            }
            die("scan timed out");
        }
        "remember" => {
            let ssid = ssid_arg();
            let mut pass = Zeroizing::new(String::new());
            std::io::stdin().lock().read_line(&mut pass).ok();
            let secret = Zeroizing::new(pass.trim_end_matches(['\n', '\r']).as_bytes().to_vec());
            let r = Peer::connect(&keys).call(&Line::new("remember").with_hex("ssid", &ssid).with_hex("secret", &secret));
            println!("{}", show(&r));
            exit((r.verb != "ok") as i32);
        }
        "forget" => {
            let r = Peer::connect(&keys).call(&Line::new("forget").with_hex("ssid", &ssid_arg()));
            println!("{}", show(&r));
            exit((r.verb != "ok") as i32);
        }
        "join" => {
            let ssid = ssid_arg();
            let mut open = false;
            let mut wait = 60u64;
            let mut dhcp: Option<String> = None;
            let mut a = args[2..].iter();
            while let Some(x) = a.next() {
                match x.as_str() {
                    "--open" => open = true,
                    "--wait" => wait = a.next().and_then(|v| v.parse().ok()).unwrap_or(60),
                    "--dhcp" => dhcp = a.next().cloned(),
                    _ => die("join SSID [--open] [--wait SECS] [--dhcp IFACE]"),
                }
            }
            if !open {
                let r = Peer::connect(&keys).call(&Line::new("prepare").with_hex("ssid", &ssid));
                if r.verb != "ok" {
                    die(&format!("no credential for this network ({})", show(&r)));
                }
            }
            let mut p = Peer::connect(&mlme);
            p.send(&Line::new("join").with_hex("ssid", &ssid).with("security", if open { "open" } else { "psk" }));
            let start = Instant::now();
            let end = start + Duration::from_secs(wait);
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
                // atrium-net runs DHCP once the port is open (wifi.md §3.5).
                // Until the jailed client (decision 15), FreeBSD's dhclient.
                let st = Command::new("dhclient").arg(&ifc).status().unwrap_or_else(|e| die(&format!("dhclient: {e}")));
                if !st.success() {
                    die("dhclient failed");
                }
                let out = Command::new("ifconfig").arg(&ifc).arg("inet").output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
                for line in out.lines().filter(|l| l.trim_start().starts_with("inet ")) {
                    println!("{:>7.2}s address {}", start.elapsed().as_secs_f64(), line.trim());
                }
            }
        }
        "leave" | "status" => {
            let mut p = Peer::connect(&mlme);
            p.send(&Line::new(cmd));
            let end = Instant::now() + Duration::from_secs(5);
            while let Some(l) = p.next(end) {
                println!("{}", show(&l));
                if l.verb == "status" || l.get("state") == Some("disconnected") || l.verb == "error" {
                    break;
                }
            }
        }
        _ => {
            eprintln!("usage: aura-ctl [--mlme PATH] [--keys PATH] scan|remember SSID|forget SSID|join SSID [--open] [--wait S] [--dhcp IF]|leave|status");
            exit(2);
        }
    }
}
