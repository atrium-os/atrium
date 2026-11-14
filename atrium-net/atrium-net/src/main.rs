//! atrium-net: the host's network configuration daemon
//! (docs/spec/atrium-net.md). N0: Ethernet-style interfaces with DHCPv4
//! through a sandboxed atrium-lease per interface, addresses and routes
//! applied after the §5.2 checks, DNS forwarders set in local-unbound.
//!
//! usage: atrium-net [-v] --iface NAME [--iface NAME ...]
//!                   [--lease PATH] [--sock PATH] [--no-unbound]
//!                   [--unbound-dir DIR] [--capsicum]

#[cfg(target_os = "freebsd")]
mod imp {
    use atrium_net::plan::{Action, Applied};
    use atrium_net::sys::{self, Bpf, Ifc, Rtsock, Unbound};
    use atrium_net::wrap;
    use aura_ipc::sock::{self, poll_read, Conn};
    use aura_ipc::{fmt_mac, Got, Line};
    use net_proto::{check_v4, HostView, LeaseV4};
    use std::io;
    use std::net::Ipv4Addr;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::os::unix::process::CommandExt;
    use std::path::PathBuf;
    use std::process::{Child, Command};
    use std::time::Instant;

    struct Opts {
        verbose: bool,
        ifaces: Vec<String>,
        lease: String,
        sock: PathBuf,
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
            lease: "/usr/local/libexec/atrium-lease".into(),
            sock: "/atrium/sockets/atrium-net/ctl.sock".into(),
            unbound: Some("/var/run".into()),
            capsicum: false,
        };
        let mut a = std::env::args().skip(1);
        while let Some(x) = a.next() {
            match x.as_str() {
                "-v" => o.verbose = true,
                "--capsicum" => o.capsicum = true,
                "--no-unbound" => o.unbound = None,
                "--iface" => o.ifaces.push(a.next().unwrap_or_else(|| die("--iface NAME"))),
                "--lease" => o.lease = a.next().unwrap_or_else(|| die("--lease PATH")),
                "--sock" => o.sock = a.next().unwrap_or_else(|| die("--sock PATH")).into(),
                "--unbound-dir" => o.unbound = Some(a.next().unwrap_or_else(|| die("--unbound-dir DIR"))),
                _ => die("usage: atrium-net [-v] --iface NAME ... [--lease PATH] [--sock PATH] [--no-unbound] [--unbound-dir DIR] [--capsicum]"),
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
    }

    struct Net {
        v: bool,
        ifc: Ifc,
        rt: Rtsock,
        unbound: Option<Unbound>,
        /// Last known addresses of every interface (getifaddrs may be
        /// unavailable in capability mode).
        host: Vec<(String, Ipv4Addr, u8)>,
    }

    impl Net {
        fn log(&self, s: &str) {
            if self.v {
                eprintln!("atrium-net: {s}");
            }
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
                        let ia = ifaddr.unwrap_or(Ipv4Addr::UNSPECIFIED);
                        // With multipath routing (on by default) RTM_ADD of a
                        // second default adds a path beside the old one; the
                        // default is ours to replace, so change it first.
                        let (first, then) = if r.prefix == 0 { (sys::RTM_CHANGE, sys::RTM_ADD) } else { (sys::RTM_ADD, sys::RTM_CHANGE) };
                        match self.rt.route(first, r.dst, r.prefix, r.gw, ia) {
                            Err(e) if matches!(e.raw_os_error(), Some(libc::EEXIST) | Some(libc::ESRCH)) => self.rt.route(then, r.dst, r.prefix, r.gw, ia),
                            x => x,
                        }
                    }
                    Action::DelRoute(r) => {
                        let ia = ifaddr.unwrap_or(Ipv4Addr::UNSPECIFIED);
                        match self.rt.route(sys::RTM_DELETE, r.dst, r.prefix, r.gw, ia) {
                            Err(e) if e.raw_os_error() == Some(libc::ESRCH) => Ok(()),
                            x => x,
                        }
                    }
                    Action::SetMtu(m) => self.ifc.set_mtu(ifname, *m),
                    Action::SetDns(d) => match &self.unbound {
                        None => Ok(()),
                        Some(u) => {
                            let list: Vec<String> = d.iter().map(|a| a.to_string()).collect();
                            match u.cmd(&format!("forward {}", list.join(" "))) {
                                Ok(s) if s.trim_end() == "ok" => Ok(()),
                                Ok(s) => Err(io::Error::other(format!("unbound said {:?}", s.trim_end()))),
                                Err(e) => Err(e),
                            }
                        }
                    },
                };
                match r {
                    Ok(()) => self.log(&format!("{ifname}: {a:?}")),
                    Err(e) => eprintln!("atrium-net: {ifname}: {a:?} failed: {e}"),
                }
            }
            self.refresh_host();
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

    fn status(ifs: &[Iface]) -> Vec<Line> {
        let mut v = Vec::new();
        for i in ifs {
            let mut l = Line::new("iface").with("name", &i.name).with("mac", fmt_mac(&i.mac));
            match &i.applied.lease {
                Some(x) => {
                    l = l.with("state", "bound").with("addr", format!("{}/{}", x.addr, x.prefix));
                    if let Some(r) = x.router {
                        l = l.with("router", r);
                    }
                    if !x.dns.is_empty() {
                        l = l.with("dns", x.dns.iter().map(|d| d.to_string()).collect::<Vec<_>>().join(","));
                    }
                }
                None => l = l.with("state", "none"),
            }
            l = l.with("refused", i.refusals).with("sends_refused", i.sends_refused);
            if !i.last_refusal.is_empty() {
                l = l.with("last_refusal", &i.last_refusal);
            }
            v.push(l);
        }
        v.push(Line::new("ok"));
        v
    }

    pub fn main() {
        let o = opts();
        let all = sys::interfaces().unwrap_or_else(|e| die(&e.to_string()));
        let mut net = Net {
            v: o.verbose,
            ifc: Ifc::new().unwrap_or_else(|e| die(&e.to_string())),
            rt: Rtsock::new().unwrap_or_else(|e| die(&e.to_string())),
            unbound: o.unbound.as_ref().map(|d| Unbound::new(d, "local_unbound.ctl").unwrap_or_else(|e| die(&e.to_string()))),
            host: Vec::new(),
        };
        net.refresh_host();
        let mut ifs = Vec::new();
        for name in &o.ifaces {
            let info = all.iter().find(|i| &i.name == name).unwrap_or_else(|| die(&format!("no interface {name}")));
            let mac = info.mac.unwrap_or_else(|| die(&format!("{name} has no MAC")));
            net.ifc.up(name).unwrap_or_else(|e| die(&format!("{name}: {e}")));
            let bpf = Bpf::open(name).unwrap_or_else(|e| die(&format!("{name}: {e}")));
            let (child, mut lease) = spawn_lease(&o.lease, o.verbose);
            lease.send(&Line::new("hello").with("mac", fmt_mac(&mac))).unwrap_or_else(|_| die("hello"));
            lease.send(&Line::new("up")).unwrap_or_else(|_| die("up"));
            net.log(&format!("{name}: managing, mac {}", fmt_mac(&mac)));
            ifs.push(Iface {
                name: name.clone(),
                index: info.index,
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
        let mut ctls: Vec<Conn> = Vec::new();

        loop {
            let mut fds = vec![net.rt.s.as_raw_fd(), listener.as_raw_fd()];
            for i in &ifs {
                fds.push(i.bpf.fd.as_raw_fd());
                fds.push(i.lease.fd());
            }
            let nfix = fds.len();
            fds.extend(ctls.iter().map(|c| c.fd()));
            let ready = poll_read(&fds, 1000);

            if ready[0] {
                for (idx, up) in net.rt.read_ifinfo() {
                    if let Some(i) = ifs.iter_mut().find(|i| i.index == idx) {
                        net.log(&format!("{}: link {}", i.name, if up { "up" } else { "down" }));
                        let _ = i.lease.send(&Line::new(if up { "up" } else { "down" }));
                    }
                }
            }
            // New control connections join after this round's polled ones.
            let mut fresh = Vec::new();
            if ready[1] {
                while let Ok((s, _)) = listener.accept() {
                    if sock::peer_uid(&s) == Some(0) {
                        if let Ok(c) = Conn::new(s) {
                            fresh.push(c);
                        }
                    }
                }
            }
            for k in 0..ifs.len() {
                if ready[2 + 2 * k] {
                    for f in ifs[k].bpf.read() {
                        if f.len() <= 1514 {
                            let _ = ifs[k].lease.send(&Line::new("frame").with_hex("hex", &f));
                        }
                    }
                }
                if ready[3 + 2 * k] {
                    let lines = match ifs[k].lease.lines() {
                        Ok(l) => l,
                        Err(_) => die(&format!("{}: atrium-lease went away", ifs[k].name)),
                    };
                    if lines.is_empty() {
                        die(&format!("{}: atrium-lease exited", ifs[k].name));
                    }
                    for g in lines {
                        let Got::Line(l) = g else { die("bad line from atrium-lease") };
                        lease_line(&mut net, &mut ifs[k], &l);
                    }
                }
            }
            let mut keep = Vec::new();
            for (j, mut c) in ctls.drain(..).enumerate() {
                if !ready[nfix + j] {
                    keep.push(c);
                    continue;
                }
                let Ok(lines) = c.lines() else { continue };
                if lines.is_empty() {
                    continue;
                }
                let mut ok = true;
                for g in lines {
                    let reply = match g {
                        Got::Line(l) if l.verb == "status" => status(&ifs),
                        _ => vec![Line::new("error").with("why", "unknown")],
                    };
                    for r in reply {
                        ok &= c.send(&r).is_ok();
                    }
                }
                if ok {
                    keep.push(c);
                }
            }
            keep.extend(fresh);
            ctls = keep;
        }
    }

    fn lease_line(net: &mut Net, i: &mut Iface, l: &Line) {
        match l.verb.as_str() {
            "send" => {
                let now = Instant::now();
                i.tokens = (i.tokens + now.duration_since(i.last_fill).as_secs_f64() / 2.0).min(10.0);
                i.last_fill = now;
                let dest = match l.get("dest") {
                    Some("bcast") => None,
                    Some(a) => a.parse::<Ipv4Addr>().ok(),
                    None => None,
                };
                let (Some(p), Some(dmac)) = (l.hex("hex"), l.mac("dmac")) else {
                    i.sends_refused += 1;
                    return;
                };
                let server = i.applied.lease.as_ref().map(|x| x.server);
                if i.tokens < 1.0 {
                    i.sends_refused += 1;
                    net.log(&format!("{}: send refused: rate", i.name));
                    return;
                }
                if let Err(e) = wrap::check_send(&p, &i.mac, dest, &dmac, server) {
                    i.sends_refused += 1;
                    net.log(&format!("{}: send refused: {e:?}", i.name));
                    return;
                }
                i.tokens -= 1.0;
                let ours = i.applied.lease.as_ref().map(|x| x.addr);
                let ciaddr = Ipv4Addr::new(p[12], p[13], p[14], p[15]);
                let src = if ours == Some(ciaddr) { ciaddr } else { Ipv4Addr::UNSPECIFIED };
                let f = wrap::frame(&i.mac, &dmac, src, dest.unwrap_or(Ipv4Addr::BROADCAST), &p, i.ip_id);
                i.ip_id = i.ip_id.wrapping_add(1);
                if let Err(e) = i.bpf.write(&f) {
                    eprintln!("atrium-net: {}: {e}", i.name);
                }
            }
            "lease4" => {
                let Some(lease) = LeaseV4::from_line(l) else {
                    refuse(net, i, "unparsable");
                    return;
                };
                match check_v4(&lease, &net.host_view(&i.name)) {
                    Ok(()) => {
                        net.log(&format!("{}: lease {}/{} router {:?} dns {:?} for {} s", i.name, lease.addr, lease.prefix, lease.router, lease.dns, lease.lease_s));
                        let addr = lease.addr;
                        let acts = i.applied.bind(lease);
                        net.apply(&i.name, Some(addr), acts);
                    }
                    Err(e) => refuse(net, i, &format!("{e:?}")),
                }
            }
            "lost" => {
                net.log(&format!("{}: lease lost ({})", i.name, l.get("why").unwrap_or("?")));
                let ia = i.applied.lease.as_ref().map(|x| x.addr);
                let acts = i.applied.lose();
                net.apply(&i.name, ia, acts);
            }
            _ => die(&format!("{}: unexpected message from atrium-lease", i.name)),
        }
    }

    fn refuse(net: &mut Net, i: &mut Iface, why: &str) {
        i.refusals += 1;
        i.last_refusal = why.to_string();
        eprintln!("atrium-net: {}: lease refused: {why}", i.name);
        let _ = i.lease.send(&Line::new("refused").with("why", why));
        let _ = net;
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
