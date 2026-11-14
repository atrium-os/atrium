//! atrium-net: the host's network configuration daemon
//! (docs/spec/atrium-net.md). DHCPv4 through a sandboxed atrium-lease per
//! interface, addresses and routes applied after the §5.2 checks, DNS
//! forwarders in local-unbound, and Wi-Fi policy (§4) over aura-mlme and
//! aura-keys.
//!
//! usage: atrium-net [-v] --iface NAME [--iface NAME ...] [--wifi NAME]
//!                   [--lease PATH] [--sock PATH] [--state-dir DIR]
//!                   [--aura-dir DIR] [--keys-dir DIR]
//!                   [--no-unbound] [--unbound-dir DIR] [--capsicum]
//!
//! Control socket (root only), one line per request, answered with lines
//! ending in `ok` or `error`; `subscribe` turns the connection into an event
//! stream as well (aura-mlme's lines, `lease`, `lease-lost`):
//!   status | subscribe | wifi-scan | wifi-status | wifi-list | wifi-leave
//!   wifi-join ssid=HEX [security=open|psk|sae] [mac=network|random|device]
//!   wifi-remember ssid=HEX [secret=HEX] [open=1] [auto=0] [prio=N] [mac=..]
//!   wifi-forget ssid=HEX | country cc=XX

#[cfg(target_os = "freebsd")]
mod imp {
    use atrium_net::plan::{Action, Applied};
    use atrium_net::sys::{self, Bpf, Dir, Ifc, Rtsock, Unbound};
    use atrium_net::wifi::{Act, Profile, Saved, Wifi};
    use atrium_net::wrap;
    use aura_ipc::sock::{self, poll_read, Conn};
    use aura_ipc::{fmt_mac, Got, Line, Reader};
    use net_proto::{check_dns6, check_v4, HostView, LeaseV4};
    use std::io;
    use std::net::{Ipv4Addr, Ipv6Addr};
    use std::os::fd::AsRawFd;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::os::unix::process::CommandExt;
    use std::path::PathBuf;
    use std::process::{Child, Command};
    use std::time::{Duration, Instant};

    const STATE_FILE: &str = "networks";

    struct Opts {
        verbose: bool,
        ifaces: Vec<String>,
        wifi: Option<String>,
        lease: String,
        sock: PathBuf,
        state: String,
        aura: String,
        keys: String,
        unbound: Option<String>,
        capsicum: bool,
    }

    fn die(s: &str) -> ! {
        eprintln!("atrium-net: {s}");
        std::process::exit(1)
    }

    fn opts() -> Opts {
        let mut o = Opts {
            verbose: false,
            ifaces: Vec::new(),
            wifi: None,
            lease: "/usr/local/libexec/atrium-lease".into(),
            sock: "/atrium/sockets/atrium-net/ctl.sock".into(),
            state: "/var/db/atrium-net".into(),
            aura: "/atrium/sockets/aura".into(),
            keys: "/atrium/sockets/aura-keys".into(),
            unbound: Some("/var/run".into()),
            capsicum: false,
        };
        let mut a = std::env::args().skip(1);
        let val = |a: &mut std::iter::Skip<std::env::Args>, f: &str| a.next().unwrap_or_else(|| die(&format!("{f} needs a value")));
        while let Some(x) = a.next() {
            match x.as_str() {
                "-v" => o.verbose = true,
                "--capsicum" => o.capsicum = true,
                "--no-unbound" => o.unbound = None,
                "--iface" => o.ifaces.push(val(&mut a, "--iface")),
                "--wifi" => o.wifi = Some(val(&mut a, "--wifi")),
                "--lease" => o.lease = val(&mut a, "--lease"),
                "--sock" => o.sock = val(&mut a, "--sock").into(),
                "--state-dir" => o.state = val(&mut a, "--state-dir"),
                "--aura-dir" => o.aura = val(&mut a, "--aura-dir"),
                "--keys-dir" => o.keys = val(&mut a, "--keys-dir"),
                "--unbound-dir" => o.unbound = Some(val(&mut a, "--unbound-dir")),
                _ => die("usage: atrium-net [-v] --iface NAME ... [--wifi NAME] [--lease PATH] [--sock PATH] [--state-dir DIR] [--aura-dir DIR] [--keys-dir DIR] [--no-unbound] [--unbound-dir DIR] [--capsicum]"),
            }
        }
        if let Some(w) = &o.wifi {
            if !o.ifaces.contains(w) {
                o.ifaces.push(w.clone());
            }
        }
        if o.ifaces.is_empty() {
            die("no --iface given");
        }
        o
    }

    struct Iface {
        name: String,
        index: u16,
        wifi: bool,
        mac: [u8; 6],
        bpf: Bpf,
        lease: Conn,
        _child: Child,
        applied: Applied,
        ip_id: u16,
        tokens: f64,
        last_fill: Instant,
        refusals: u32,
        last_refusal: String,
        sends_refused: u32,
        /// DNS servers from RAs (checked), and the router solicitations
        /// still to send (three, 4 s apart, after the link comes up).
        dns6: Vec<Ipv6Addr>,
        rs_left: u8,
        rs_at: u64,
    }

    struct Ctl {
        c: Conn,
        subscribed: bool,
    }

    struct Net {
        v: bool,
        t0: Instant,
        ifc: Ifc,
        rt: Rtsock,
        unbound: Option<Unbound>,
        host: Vec<(String, Ipv4Addr, u8)>,
        ifs: Vec<Iface>,
        ctls: Vec<Ctl>,
        state: Dir,
        wifi: Option<Wifi>,
        aura: Option<Dir>,
        keys: Option<Dir>,
        mlme: Option<Conn>,
        mlme_retry: u64,
        /// Wi-Fi went away at this time: the address stays until
        /// `LINK_GRACE_MS` later, so a short drop (a tunnel, a blip in the
        /// signal) does not break every open connection.
        wifi_grace: Option<u64>,
        /// What unbound forwards to now.
        forwarders: Vec<String>,
    }

    /// How long a Wi-Fi address outlives the link.
    const LINK_GRACE_MS: u64 = 60_000;

    impl Net {
        fn now(&self) -> u64 {
            self.t0.elapsed().as_millis() as u64
        }

        fn log(&self, s: &str) {
            if self.v {
                eprintln!("atrium-net: {s}");
            }
        }

        fn event(&mut self, l: &Line) {
            self.ctls.retain_mut(|c| !c.subscribed || c.c.send(l).is_ok());
        }

        fn refresh_host(&mut self) {
            if let Ok(all) = sys::interfaces() {
                self.host = all.iter().flat_map(|i| i.v4.iter().map(move |&(a, p)| (i.name.clone(), a, p))).collect();
            }
        }

        fn host_view(&self, except: &str) -> HostView {
            HostView { others: self.host.iter().filter(|(n, _, _)| n != except && n != "lo0").map(|&(_, a, p)| (a, p)).collect() }
        }

        fn apply(&mut self, ifname: &str, ifaddr: Option<Ipv4Addr>, acts: Vec<Action>) {
            for a in acts {
                let ia = ifaddr.unwrap_or(Ipv4Addr::UNSPECIFIED);
                let r: io::Result<()> = match &a {
                    Action::AddAddr { addr, prefix } => match self.ifc.add_addr(ifname, *addr, *prefix) {
                        Err(e) if e.raw_os_error() == Some(libc::EEXIST) => Ok(()),
                        r => r,
                    },
                    Action::DelAddr { addr } => match self.ifc.del_addr(ifname, *addr) {
                        Err(e) if matches!(e.raw_os_error(), Some(libc::EADDRNOTAVAIL) | Some(libc::ESRCH)) => Ok(()),
                        r => r,
                    },
                    Action::AddRoute(r) => {
                        // With multipath routing (on by default) RTM_ADD of a
                        // second default adds a path beside the old one; the
                        // default is ours to replace, so change it first.
                        let (first, then) = if r.prefix == 0 { (sys::RTM_CHANGE, sys::RTM_ADD) } else { (sys::RTM_ADD, sys::RTM_CHANGE) };
                        match self.rt.route(first, r.dst, r.prefix, r.gw, ia) {
                            Err(e) if matches!(e.raw_os_error(), Some(libc::EEXIST) | Some(libc::ESRCH)) => self.rt.route(then, r.dst, r.prefix, r.gw, ia),
                            x => x,
                        }
                    }
                    Action::DelRoute(r) => match self.rt.route(sys::RTM_DELETE, r.dst, r.prefix, r.gw, ia) {
                        Err(e) if e.raw_os_error() == Some(libc::ESRCH) => Ok(()),
                        x => x,
                    },
                    Action::SetMtu(m) => self.ifc.set_mtu(ifname, *m),
                    // Merged with every interface's servers once the lease is applied.
                    Action::SetDns(_) => Ok(()),
                };
                match r {
                    Ok(()) => self.log(&format!("{ifname}: {a:?}")),
                    Err(e) => eprintln!("atrium-net: {ifname}: {a:?} failed: {e}"),
                }
            }
            self.refresh_host();
            self.forward();
        }

        /// Point unbound at every interface's DNS servers: DHCP's, then the
        /// RAs' (link-local ones with the interface index, which unbound reads
        /// after '%'). Kept as is when nothing is known (an empty list would
        /// leave unbound resolving from the roots).
        fn forward(&mut self) {
            let mut list: Vec<String> = Vec::new();
            for i in &self.ifs {
                if let Some(l) = &i.applied.lease {
                    list.extend(l.dns.iter().map(|a| a.to_string()));
                }
            }
            for i in &self.ifs {
                for a in &i.dns6 {
                    let ll = a.segments()[0] & 0xffc0 == 0xfe80;
                    list.push(if ll { format!("{a}%{}", i.index) } else { a.to_string() });
                }
            }
            let mut seen = Vec::new();
            list.retain(|x| if seen.contains(x) { false } else { seen.push(x.clone()); true });
            if list.is_empty() || list == self.forwarders {
                return;
            }
            let Some(u) = &self.unbound else {
                self.forwarders = list;
                return;
            };
            match u.cmd(&format!("forward {}", list.join(" "))) {
                Ok(s) if s.trim_end() == "ok" => {
                    self.log(&format!("unbound: forward {}", list.join(" ")));
                    self.forwarders = list;
                }
                Ok(s) => eprintln!("atrium-net: unbound said {:?}", s.trim_end()),
                Err(e) => eprintln!("atrium-net: unbound: {e}"),
            }
        }

        /// Router solicitations due, and the interfaces' IPv6 addresses.
        fn tick6(&mut self) {
            let now = self.now();
            for i in &mut self.ifs {
                if i.rs_left > 0 && now >= i.rs_at {
                    i.rs_left -= 1;
                    i.rs_at = now + 4000;
                    if let Err(e) = i.bpf.write(&wrap::rs_frame(&i.mac)) {
                        eprintln!("atrium-net: {}: router solicitation: {e}", i.name);
                    }
                }
            }
        }

        /// Off a network: its SLAAC addresses go too (the kernel would keep
        /// them until their lifetimes end, on a link that no longer has them).
        fn drop_v6(&mut self, k: usize) {
            let name = self.ifs[k].name.clone();
            let _ = self.ifc.accept_rtadv(&name, false);
            if let Ok(all) = sys::interfaces() {
                if let Some(info) = all.iter().find(|x| x.name == name) {
                    for a in &info.v6 {
                        if a.segments()[0] & 0xffc0 != 0xfe80 {
                            match self.ifc.del_addr6(&name, *a) {
                                Ok(()) => self.log(&format!("{name}: removed {a}")),
                                Err(e) => eprintln!("atrium-net: {name}: removing {a}: {e}"),
                            }
                        }
                    }
                }
            }
            let _ = self.ifc.accept_rtadv(&name, true);
        }

        fn wifi_iface(&mut self) -> Option<usize> {
            self.ifs.iter().position(|i| i.wifi)
        }

        fn lose(&mut self, k: usize, why: &str) {
            let ia = self.ifs[k].applied.lease.as_ref().map(|x| x.addr);
            let acts = self.ifs[k].applied.lose();
            if !acts.is_empty() {
                let name = self.ifs[k].name.clone();
                self.log(&format!("{name}: lease dropped ({why})"));
                self.apply(&name, ia, acts);
                let ev = Line::new("lease-lost").with("iface", &name).with("why", why);
                self.event(&ev);
            }
            if self.ifs[k].wifi {
                self.drop_v6(k);
            }
        }

        fn save(&mut self) {
            let Some(w) = &self.wifi else { return };
            if let Err(e) = self.state.write(STATE_FILE, w.saved.render().as_bytes()) {
                eprintln!("atrium-net: saving profiles: {e}");
            }
        }

        /// One request to aura-keys (a fresh connection each time).
        fn keys_call(&self, l: &Line) -> Option<Line> {
            let mut s = self.keys.as_ref()?.connect("keys.sock").ok()?;
            s.set_read_timeout(Some(Duration::from_secs(10))).ok()?;
            l.send(&mut s).ok()?;
            let mut r = Reader::default();
            let end = Instant::now() + Duration::from_secs(10);
            while Instant::now() < end {
                match r.fill(&mut s) {
                    Ok(v) => {
                        if let Some(Got::Line(l)) = v.into_iter().next() {
                            return Some(l);
                        }
                    }
                    Err(_) => return None,
                }
            }
            None
        }

        fn wifi_acts(&mut self, acts: Vec<Act>) {
            let mut queue = acts;
            while !queue.is_empty() {
                let now = self.now();
                let mut more = Vec::new();
                for a in queue.drain(..) {
                    match a {
                        Act::Mlme(l) => {
                            if let Some(c) = &mut self.mlme {
                                if c.send(&l).is_err() {
                                    self.mlme = None;
                                }
                            }
                        }
                        Act::Join { ssid, security, mac } => {
                            if security != "open" {
                                let ok = self.keys_call(&Line::new("prepare").with_hex("ssid", &ssid)).is_some_and(|r| r.verb == "ok");
                                if !ok {
                                    let f = Line::new("state").with("state", "failed").with("why", "no-credential");
                                    if let Some(w) = &mut self.wifi {
                                        more.extend(w.mlme_line(&f, now));
                                    }
                                    continue;
                                }
                            }
                            self.log(&format!("wifi: join {} ({security}, mac {mac})", String::from_utf8_lossy(&ssid)));
                            more.push(Act::Mlme(Line::new("join").with_hex("ssid", &ssid).with("security", security).with("mac", mac)));
                        }
                        Act::LeaseUp | Act::LeaseDown => {
                            if let Some(k) = self.wifi_iface() {
                                let up = a == Act::LeaseUp;
                                let _ = self.ifs[k].lease.send(&Line::new(if up { "up" } else { "down" }));
                                // Off the network: atrium-lease keeps the lease
                                // and confirms it on the next join (INIT-REBOOT).
                                // The address stays for a grace period so a
                                // short drop does not reset every connection;
                                // nothing reaches the dead link meanwhile.
                                if up {
                                    self.wifi_grace = None;
                                    self.ifs[k].rs_left = 3;
                                    self.ifs[k].rs_at = now;
                                } else if self.wifi_grace.is_none() && self.ifs[k].applied.lease.is_some() {
                                    self.wifi_grace = Some(now + LINK_GRACE_MS);
                                    self.log("wifi: link down, keeping the address for 60 s");
                                }
                            }
                        }
                        Act::Drop => {
                            self.wifi_grace = None;
                            if let Some(k) = self.wifi_iface() {
                                self.lose(k, "left");
                            }
                        }
                        Act::LeaseMac(m) => {
                            if let Some(k) = self.wifi_iface() {
                                if self.ifs[k].mac != m {
                                    self.lose(k, "new-mac");
                                    self.ifs[k].mac = m;
                                    let _ = self.ifs[k].lease.send(&Line::new("mac").with("mac", fmt_mac(&m)));
                                }
                            }
                        }
                        Act::Save => self.save(),
                        Act::Event(l) => self.event(&l),
                    }
                }
                queue = more;
            }
        }

        fn ctl(&mut self, j: usize, l: &Line) -> Vec<Line> {
            let now = self.now();
            let ok = || vec![Line::new("ok")];
            let err = |why: &str| vec![Line::new("error").with("why", why)];
            let ssid = || l.hex("ssid").filter(|s| !s.is_empty() && s.len() <= 32);
            let wifi_needed = matches!(l.verb.as_str(), "wifi-scan" | "wifi-status" | "wifi-list" | "wifi-leave" | "wifi-join" | "wifi-remember" | "wifi-forget" | "country");
            if wifi_needed && self.wifi.is_none() {
                return err("no-wifi");
            }
            match l.verb.as_str() {
                "status" => self.status(),
                "subscribe" => {
                    self.ctls[j].subscribed = true;
                    ok()
                }
                "wifi-scan" => {
                    self.wifi_acts(vec![Act::Mlme(Line::new("scan"))]);
                    ok()
                }
                "wifi-status" => {
                    self.wifi_acts(vec![Act::Mlme(Line::new("status"))]);
                    ok()
                }
                "wifi-list" => {
                    let mut v: Vec<Line> = self.wifi.as_ref().unwrap().saved.profiles.iter().map(Profile::to_line).collect();
                    v.push(Line::new("ok"));
                    v
                }
                "wifi-leave" => {
                    let a = self.wifi.as_mut().unwrap().leave();
                    self.wifi_acts(a);
                    ok()
                }
                "wifi-join" => {
                    let Some(s) = ssid() else { return err("bad-ssid") };
                    let mac = l.get("mac").filter(|m| matches!(*m, "network" | "random" | "device"));
                    let a = self.wifi.as_mut().unwrap().join(s, l.get("security"), mac);
                    self.wifi_acts(a);
                    ok()
                }
                "wifi-remember" => {
                    let Some(s) = ssid() else { return err("bad-ssid") };
                    let open = l.get("open") == Some("1");
                    if !open {
                        let Some(secret) = l.hex("secret") else { return err("no-secret") };
                        let r = self.keys_call(&Line::new("remember").with_hex("ssid", &s).with_hex("secret", &secret));
                        match r {
                            Some(r) if r.verb == "ok" => {}
                            Some(r) => return vec![r],
                            None => return err("aura-keys-unreachable"),
                        }
                    }
                    let p = Profile {
                        ssid: s,
                        open,
                        wpa3_only: l.get("wpa3_only") == Some("1"),
                        mac: l.get("mac").filter(|m| matches!(*m, "network" | "random" | "device")).unwrap_or("network").to_string(),
                        // Open networks join automatically only when asked.
                        auto: if open { l.get("auto") == Some("1") } else { l.get("auto") != Some("0") },
                        prio: l.num("prio").unwrap_or(0).clamp(-1000, 1000),
                        metered: l.get("metered") == Some("1"),
                    };
                    let a = self.wifi.as_mut().unwrap().remember(p, now);
                    self.wifi_acts(a);
                    ok()
                }
                "wifi-forget" => {
                    let Some(s) = ssid() else { return err("bad-ssid") };
                    let _ = self.keys_call(&Line::new("forget").with_hex("ssid", &s));
                    let a = self.wifi.as_mut().unwrap().forget(&s);
                    self.wifi_acts(a);
                    ok()
                }
                "country" => {
                    let Some(cc) = l.get("cc").filter(|c| c.len() == 2 && c.bytes().all(|b| b.is_ascii_alphanumeric())) else { return err("bad-country") };
                    let a = self.wifi.as_mut().unwrap().set_country(cc);
                    self.wifi_acts(a);
                    ok()
                }
                _ => err("unknown"),
            }
        }

        /// Metered if the server says so (an Android hotspot) or the user
        /// marked the Wi-Fi network they are on.
        fn metered(&self, i: &Iface) -> bool {
            i.applied.lease.as_ref().is_some_and(|x| x.metered)
                || (i.wifi && self.wifi.as_ref().and_then(|w| w.ssid().and_then(|s| w.saved.get(s))).is_some_and(|p| p.metered))
        }

        fn status(&self) -> Vec<Line> {
            let mut v = Vec::new();
            let all = sys::interfaces().unwrap_or_default();
            for i in &self.ifs {
                let mut l = Line::new("iface").with("name", &i.name).with("mac", fmt_mac(&i.mac));
                let v6: Vec<String> = all.iter().filter(|x| x.name == i.name).flat_map(|x| x.v6.iter()).map(|a| a.to_string()).collect();
                match &i.applied.lease {
                    Some(x) => {
                        l = l.with("state", "bound").with("addr", format!("{}/{}", x.addr, x.prefix));
                        if let Some(r) = x.router {
                            l = l.with("router", r);
                        }
                        if !x.dns.is_empty() {
                            l = l.with("dns", x.dns.iter().map(|d| d.to_string()).collect::<Vec<_>>().join(","));
                        }
                        l = l.with("metered", self.metered(i) as u8);
                        if i.wifi && self.wifi_grace.is_some() {
                            l = l.with("link", "lost");
                        }
                    }
                    None => l = l.with("state", "none"),
                }
                if !v6.is_empty() {
                    l = l.with("inet6", v6.join(","));
                }
                if !i.dns6.is_empty() {
                    l = l.with("dns6", i.dns6.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(","));
                }
                l = l.with("refused", i.refusals).with("sends_refused", i.sends_refused);
                if !i.last_refusal.is_empty() {
                    l = l.with("last_refusal", &i.last_refusal);
                }
                v.push(l);
            }
            if let Some(w) = &self.wifi {
                let mut l = Line::new("wifi").with("state", w.state()).with("mlme", self.mlme.is_some() as u8).with("profiles", w.saved.profiles.len());
                if let Some(s) = w.ssid() {
                    l = l.with_hex("ssid", s);
                }
                v.push(l);
            }
            v.push(Line::new("ok"));
            v
        }

        fn lease_line(&mut self, k: usize, l: &Line) {
            match l.verb.as_str() {
                "send" => self.lease_send(k, l),
                "lease4" => {
                    let name = self.ifs[k].name.clone();
                    let Some(lease) = LeaseV4::from_line(l) else {
                        self.refuse(k, "unparsable");
                        return;
                    };
                    match check_v4(&lease, &self.host_view(&name)) {
                        Ok(()) => {
                            self.log(&format!("{name}: lease {}/{} router {:?} dns {:?} for {} s", lease.addr, lease.prefix, lease.router, lease.dns, lease.lease_s));
                            let addr = lease.addr;
                            let ev = Line::new("lease").with("iface", &name).with("addr", format!("{}/{}", lease.addr, lease.prefix));
                            let acts = self.ifs[k].applied.bind(lease);
                            self.apply(&name, Some(addr), acts);
                            let ev = ev.with("metered", self.metered(&self.ifs[k]) as u8);
                            self.event(&ev);
                        }
                        Err(e) => self.refuse(k, &format!("{e:?}")),
                    }
                }
                "lost" => self.lose(k, l.get("why").unwrap_or("?")),
                "dns6" => {
                    let list: Option<Vec<Ipv6Addr>> = l.get("servers").map(|s| s.split(',').filter(|x| !x.is_empty()).map(|x| x.parse().ok()).collect()).unwrap_or(None);
                    match list {
                        Some(v) if check_dns6(&v).is_ok() => {
                            self.log(&format!("{}: IPv6 DNS {v:?}", self.ifs[k].name));
                            self.ifs[k].dns6 = v;
                            self.forward();
                        }
                        _ => {
                            let i = &mut self.ifs[k];
                            i.refusals += 1;
                            i.last_refusal = "dns6".into();
                            eprintln!("atrium-net: {}: IPv6 DNS refused: {:?}", i.name, l.get("servers"));
                        }
                    }
                }
                _ => die(&format!("{}: unexpected message from atrium-lease", self.ifs[k].name)),
            }
        }

        fn lease_send(&mut self, k: usize, l: &Line) {
            let i = &mut self.ifs[k];
            let now = Instant::now();
            i.tokens = (i.tokens + now.duration_since(i.last_fill).as_secs_f64() / 2.0).min(10.0);
            i.last_fill = now;
            let dest = match l.get("dest") {
                Some("bcast") | None => None,
                Some(a) => a.parse::<Ipv4Addr>().ok(),
            };
            let (Some(p), Some(dmac)) = (l.hex("hex"), l.mac("dmac")) else {
                i.sends_refused += 1;
                return;
            };
            let server = i.applied.lease.as_ref().map(|x| x.server);
            if i.tokens < 1.0 {
                i.sends_refused += 1;
                return;
            }
            if let Err(e) = wrap::check_send(&p, &i.mac, dest, &dmac, server) {
                i.sends_refused += 1;
                let m = format!("{}: send refused: {e:?}", i.name);
                self.log(&m);
                return;
            }
            i.tokens -= 1.0;
            let ours = i.applied.lease.as_ref().map(|x| x.addr);
            let ciaddr = Ipv4Addr::new(p[12], p[13], p[14], p[15]);
            let src = if ours == Some(ciaddr) { ciaddr } else { Ipv4Addr::UNSPECIFIED };
            let f = wrap::frame(&i.mac, &dmac, src, dest.unwrap_or(Ipv4Addr::BROADCAST), &p, i.ip_id);
            i.ip_id = i.ip_id.wrapping_add(1);
            if let Err(e) = i.bpf.write(&f) {
                eprintln!("atrium-net: {}: bpf write: {e}", i.name);
            }
        }

        fn refuse(&mut self, k: usize, why: &str) {
            let i = &mut self.ifs[k];
            i.refusals += 1;
            i.last_refusal = why.to_string();
            eprintln!("atrium-net: {}: lease refused: {why}", i.name);
            let _ = i.lease.send(&Line::new("refused").with("why", why));
        }

        fn mlme_connect(&mut self) {
            let now = self.now();
            if self.mlme.is_some() || self.wifi.is_none() || now < self.mlme_retry {
                return;
            }
            self.mlme_retry = now + 2000;
            let Some(d) = &self.aura else { return };
            let Ok(s) = d.connect("mlme.sock") else { return };
            let Ok(c) = Conn::new(s) else { return };
            self.log("wifi: connected to aura-mlme");
            self.mlme = Some(c);
            let a = self.wifi.as_mut().unwrap().mlme_up(now);
            self.wifi_acts(a);
        }
    }

    fn spawn_lease(path: &str, v: bool) -> (Child, Conn) {
        let (ours, theirs) = UnixStream::pair().unwrap_or_else(|_| die("socketpair"));
        let fd = theirs.as_raw_fd();
        let mut cmd = Command::new(path);
        if v {
            cmd.arg("-v");
        }
        unsafe {
            cmd.pre_exec(move || {
                if libc::dup2(fd, 3) < 0 {
                    return Err(io::Error::last_os_error());
                }
                // The lease process gets nothing else of ours.
                libc::closefrom(4);
                Ok(())
            });
        }
        let child = cmd.spawn().unwrap_or_else(|e| die(&format!("spawn {path}: {e}")));
        drop(theirs);
        (child, Conn::new(ours).unwrap_or_else(|_| die("lease conn")))
    }

    pub fn main() {
        let o = opts();
        let all = sys::interfaces().unwrap_or_else(|e| die(&e.to_string()));
        let _ = std::fs::create_dir_all(&o.state);
        let state = Dir::open(&o.state).unwrap_or_else(|e| die(&format!("{}: {e}", o.state)));
        let wifi = o.wifi.as_ref().map(|_| {
            let text = state.read(STATE_FILE, 1 << 20).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
            Wifi::new(Saved::parse(&text))
        });
        let mut net = Net {
            v: o.verbose,
            t0: Instant::now(),
            ifc: Ifc::new().unwrap_or_else(|e| die(&e.to_string())),
            rt: Rtsock::new().unwrap_or_else(|e| die(&e.to_string())),
            unbound: o.unbound.as_ref().map(|d| Unbound::new(d, "local_unbound.ctl").unwrap_or_else(|e| die(&format!("unbound: {e}")))),
            host: Vec::new(),
            ifs: Vec::new(),
            ctls: Vec::new(),
            state,
            aura: wifi.as_ref().map(|_| Dir::open(&o.aura).unwrap_or_else(|e| die(&format!("{}: {e}", o.aura)))),
            keys: wifi.as_ref().map(|_| Dir::open(&o.keys).unwrap_or_else(|e| die(&format!("{}: {e}", o.keys)))),
            wifi,
            mlme: None,
            mlme_retry: 0,
            wifi_grace: None,
            forwarders: Vec::new(),
        };
        // Temporary addresses (RFC 8981) preferred for outgoing connections;
        // stable-private (RFC 7217) for the rest. Before capability mode.
        for (k, v) in [("net.inet6.ip6.use_stableaddr", 1), ("net.inet6.ip6.use_tempaddr", 1), ("net.inet6.ip6.prefer_tempaddr", 1)] {
            if let Err(e) = sys::sysctl_int(k, v) {
                eprintln!("atrium-net: {k}: {e}");
            }
        }
        net.refresh_host();
        for name in &o.ifaces {
            let info = all.iter().find(|i| &i.name == name).unwrap_or_else(|| die(&format!("no interface {name}")));
            let mac = info.mac.unwrap_or_else(|| die(&format!("{name} has no MAC")));
            let is_wifi = o.wifi.as_deref() == Some(name.as_str());
            net.ifc.up(name).unwrap_or_else(|e| die(&format!("{name}: {e}")));
            if let Err(e) = net.ifc.enable_inet6(name) {
                eprintln!("atrium-net: {name}: IPv6: {e}");
            }
            let bpf = Bpf::open(name).unwrap_or_else(|e| die(&format!("{name}: {e}")));
            let (child, mut lease) = spawn_lease(&o.lease, o.verbose);
            lease.send(&Line::new("hello").with("mac", fmt_mac(&mac))).unwrap_or_else(|_| die("hello"));
            // Wired links start now; Wi-Fi when aura-mlme reports the port open.
            if !is_wifi {
                lease.send(&Line::new("up")).unwrap_or_else(|_| die("up"));
            }
            net.log(&format!("{name}: managing{}, mac {}", if is_wifi { " (Wi-Fi)" } else { "" }, fmt_mac(&mac)));
            net.ifs.push(Iface {
                name: name.clone(),
                index: info.index,
                wifi: is_wifi,
                mac,
                bpf,
                lease,
                _child: child,
                applied: Applied::default(),
                ip_id: 1,
                tokens: 10.0,
                last_fill: Instant::now(),
                refusals: 0,
                last_refusal: String::new(),
                sends_refused: 0,
                dns6: Vec::new(),
                rs_left: if is_wifi { 0 } else { 3 },
                rs_at: 0,
            });
        }
        if let Some(d) = o.sock.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let listener: UnixListener = sock::listen(&o.sock, 0o600).unwrap_or_else(|e| die(&format!("{}: {e}", o.sock.display())));
        listener.set_nonblocking(true).ok();
        if o.capsicum {
            sys::cap_enter().unwrap_or_else(|e| die(&e.to_string()));
            net.log("in capability mode");
        }
        net.mlme_connect();

        loop {
            let mut fds = vec![net.rt.s.as_raw_fd(), listener.as_raw_fd(), net.mlme.as_ref().map_or(-1, |c| c.fd())];
            for i in &net.ifs {
                fds.push(i.bpf.fd.as_raw_fd());
                fds.push(i.lease.fd());
            }
            let nfix = fds.len();
            let nctl = net.ctls.len();
            fds.extend(net.ctls.iter().map(|c| c.c.fd()));
            let now = net.now();
            net.tick6();
            if net.wifi_grace.is_some_and(|g| now >= g) {
                net.wifi_grace = None;
                if let Some(k) = net.wifi_iface() {
                    net.lose(k, "link-down");
                }
            }
            let wait = net.wifi.as_ref().and_then(|w| w.deadline()).map_or(1000, |d| d.saturating_sub(now).min(1000)) as i32;
            let ready = poll_read(&fds, wait.max(10));

            if ready[0] {
                for (idx, up) in net.rt.read_ifinfo() {
                    if let Some(i) = net.ifs.iter_mut().find(|i| i.index == idx && !i.wifi) {
                        let m = format!("{}: link {}", i.name, if up { "up" } else { "down" });
                        if up {
                            i.rs_left = 3;
                            i.rs_at = 0;
                        }
                        let _ = i.lease.send(&Line::new(if up { "up" } else { "down" }));
                        net.log(&m);
                    }
                }
            }
            if ready[1] {
                while let Ok((s, _)) = listener.accept() {
                    if sock::peer_uid(&s) == Some(0) {
                        if let Ok(c) = Conn::new(s) {
                            net.ctls.push(Ctl { c, subscribed: false });
                        }
                    }
                }
            }
            if ready[2] {
                let lines = net.mlme.as_mut().map(|c| c.lines());
                // Ok with no lines is a partial line; only Err is the end.
                match lines {
                    Some(Ok(v)) => {
                        for g in v {
                            if let Got::Line(l) = g {
                                let now = net.now();
                                let a = net.wifi.as_mut().unwrap().mlme_line(&l, now);
                                net.wifi_acts(a);
                            }
                        }
                    }
                    _ => {
                        net.log("wifi: aura-mlme went away");
                        net.mlme = None;
                        let a = net.wifi.as_mut().unwrap().mlme_down();
                        net.wifi_acts(a);
                    }
                }
            }
            for k in 0..net.ifs.len() {
                if ready[3 + 2 * k] {
                    for f in net.ifs[k].bpf.read() {
                        if f.len() <= 1514 {
                            let _ = net.ifs[k].lease.send(&Line::new("frame").with_hex("hex", &f));
                        }
                    }
                }
                if ready[4 + 2 * k] {
                    let lines = match net.ifs[k].lease.lines() {
                        Ok(l) => l,
                        Err(_) => die(&format!("{}: atrium-lease exited", net.ifs[k].name)),
                    };
                    for g in lines {
                        let Got::Line(l) = g else { die("bad line from atrium-lease") };
                        net.lease_line(k, &l);
                    }
                }
            }
            // Control connections polled this round (new ones wait a round).
            let mut gone = Vec::new();
            for j in 0..nctl {
                if !ready[nfix + j] {
                    continue;
                }
                let lines = match net.ctls[j].c.lines() {
                    Ok(l) => l,
                    Err(_) => {
                        gone.push(j);
                        continue;
                    }
                };
                for g in lines {
                    let reply = match g {
                        Got::Line(l) => net.ctl(j, &l),
                        _ => vec![Line::new("error").with("why", "bad-line")],
                    };
                    for r in reply {
                        if j < net.ctls.len() && net.ctls[j].c.send(&r).is_err() {
                            gone.push(j);
                            break;
                        }
                    }
                }
            }
            gone.sort_unstable();
            gone.dedup();
            for j in gone.into_iter().rev() {
                if j < net.ctls.len() {
                    net.ctls.remove(j);
                }
            }
            let now = net.now();
            if let Some(w) = net.wifi.as_mut() {
                let a = w.tick(now);
                net.wifi_acts(a);
            }
            net.mlme_connect();
        }
    }

}

#[cfg(target_os = "freebsd")]
fn main() {
    imp::main()
}

#[cfg(not(target_os = "freebsd"))]
fn main() {
    eprintln!("atrium-net runs on FreeBSD only");
    std::process::exit(1);
}
