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
//!                   [--probe URL | --no-probe] [--probe-expect TEXT]
//!                   [--probe-every SECS] [--probe-bin PATH] [--mdns-bin PATH | --no-mdns] [--netd-dir DIR]
//!
//! With more than one interface, one carries the default route (egress.rs:
//! Ethernet over Wi-Fi, working internet over broken, doubling hold-down);
//! each change is an `egress` event and, with --netd-dir, an
//! `egress-changed iface=..` line to atrium-netd's ctl.sock. Each bound
//! interface is probed through the sandboxed atrium-probe (§4.2): plain HTTP
//! to URL, resolved with that interface's DNS server and pinned to it by a
//! host route; `internet` events report yes / no / captive.
//!
//! Control socket (root only), one line per request, answered with lines
//! ending in `ok` or `error`; `subscribe` turns the connection into an event
//! stream as well (aura-mlme's lines, `lease`, `lease-lost`, `egress`,
//! `internet`):
//!   status | subscribe | wifi-scan | wifi-status | wifi-list | wifi-leave
//!   wifi-join ssid=HEX [security=open|psk|sae] [mac=network|random|device]
//!   wifi-remember ssid=HEX [secret=HEX|handle=HEX [trial=1]] [open=1] [auto=0] [prio=N] [mac=..]
//!     (trial=1: saved, with its passphrase, only once the next join works)
//!   wifi-forget ssid=HEX | country cc=XX
//!   static ssid=HEX|iface=NAME [maddr=A/P [mrouter=R] [mdns=A,B]]  (no maddr: DHCP)
//!   dns [servers=A,B,C]  (the machine's own DNS servers; none: each network's)

#[cfg(target_os = "freebsd")]
mod imp {
    use atrium_net::egress::{Cand, Egress, Inet};
    use atrium_net::plan::{Action, Applied};
    use atrium_net::sys::{self, Bpf, Dir, Ifc, Rtsock, Unbound};
    use atrium_net::wifi::{Act, Profile, Saved, Wifi};
    use atrium_net::spawn::Spawner;
    use atrium_net::wrap;
    use aura_ipc::sock::{self, poll_read, Conn};
    use aura_ipc::{fmt_mac, Got, Line, Reader};
    use net_proto::{check_dns6, check_lease6, check_v4, in_net, ordinary_unicast, HostView, LeaseV4};
    use net_proto::tunnel::{check_tunnel, render_anchor, AnchorTunnel, Net4, Tunnel};
    use std::io;
    use atrium_netconf::{check_dns, lines as nlines, tls_name_ok, Manual};
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    use std::os::fd::AsRawFd;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    const STATE_FILE: &str = "networks";
    /// Fixed settings of wired ports and the machine's own DNS servers.
    const SETTINGS_FILE: &str = "settings";

    /// A fixed setting as a lease that never ends, so it takes the same
    /// checks and the same path as one from DHCP.
    fn fixed_lease(m: &Manual) -> LeaseV4 {
        LeaseV4 {
            addr: m.addr,
            prefix: m.prefix,
            router: m.router,
            routes: Vec::new(),
            dns: m.dns.clone(),
            mtu: None,
            lease_s: u32::MAX,
            server: Ipv4Addr::UNSPECIFIED,
            metered: false,
        }
    }

    /// The settings file: `wired iface=NAME maddr=..` and `dns servers=..`.
    /// Own DNS servers, and the name their TLS certificate must show if
    /// they are asked over TLS.
    type OwnDns = (Vec<IpAddr>, Option<String>);

    /// The `dns` line of the ctl socket and the settings file:
    /// `servers=A,B [tls=NAME]`; no servers: each network's own.
    fn parse_own_dns(l: &Line) -> Option<OwnDns> {
        let v: Vec<IpAddr> = l.get("servers").map(|s| s.split(',').map(|x| x.parse().ok()).collect()).unwrap_or(Some(Vec::new()))?;
        check_dns(&v).ok()?;
        let tls = l.get("tls").map(str::to_string);
        if tls.as_ref().is_some_and(|n| v.is_empty() || !tls_name_ok(n)) {
            return None;
        }
        Some((v, tls))
    }

    fn parse_settings(text: &str) -> (Vec<(String, Manual)>, OwnDns) {
        let (mut wired, mut dns) = (Vec::new(), (Vec::new(), None));
        for l in text.lines().take(64).filter_map(Line::parse) {
            match l.verb.as_str() {
                "wired" => {
                    if let (Some(n), Some(m)) = (l.get("iface"), nlines::manual(&l)) {
                        wired.retain(|(x, _): &(String, Manual)| x != n);
                        wired.push((n.to_string(), m));
                    }
                }
                "dns" => {
                    if let Some(d) = parse_own_dns(&l) {
                        dns = d;
                    }
                }
                _ => {}
            }
        }
        (wired, dns)
    }
    /// A probe that has not answered by then is abandoned (atrium-probe's
    /// own timeouts add up to about 15 s).
    const PROBE_DEADLINE_MS: u64 = 30_000;
    /// After a failed probe, try again this soon (or --probe-every if less).
    const PROBE_RETRY_MS: u64 = 30_000;
    /// A dead probe process is started again after this long.
    const PROBE_AGAIN_MS: u64 = 10_000;
    const UNBOUND_CHECK_MS: u64 = 10_000;
    /// While a portal sign-in window is open (§8.1).
    const PORTAL_PROBE_MS: u64 = 5_000;
    const PORTAL_WATCH_MS: u64 = 10 * 60_000;
    /// DNS over TLS on a new network (§8.1 P3): checked every DOT_CHECK_MS
    /// for DOT_NEW_MS after the egress changes (and while failing); failing
    /// for DOT_FAIL_MS hints at a portal in the way.
    const DOT_CHECK_MS: u64 = 5_000;
    const DOT_NEW_MS: u64 = 60_000;
    const DOT_FAIL_MS: u64 = 10_000;

    /// The DNS-over-TLS check (§8.1 P3): unbound asked, on a socket opened
    /// before capability mode, for the TLS server's own name, flushed from
    /// its cache first so the answer has to come over TLS. Nothing else is
    /// asked of anyone.
    struct DotCheck {
        sock: std::net::UdpSocket,
        id: u16,
        /// A question is out, sent at this time.
        asked: Option<u64>,
        /// When the egress last changed: checks run for DOT_NEW_MS after.
        since_new: u64,
        failing_since: Option<u64>,
        failing: bool,
        /// The server answered over TLS on this network.
        answered: bool,
        next: u64,
    }

    /// The root zone's forwarders in `unbound-control list_forwards` output
    /// (". IN forward A B ..."), without scope suffixes.
    fn root_forwards(s: &str) -> Vec<String> {
        s.lines()
            .find_map(|l| l.strip_prefix(". IN forward "))
            .map(|r| r.split_whitespace().filter(|w| !w.starts_with('+')).map(|w| w.split('%').next().unwrap_or(w).to_string()).collect())
            .unwrap_or_default()
    }
    const PROBE_CONFIRM_MS: u64 = 5_000;

    /// `uptime: N seconds` of `unbound-control status`.
    fn unbound_uptime(s: &str) -> Option<u64> {
        s.lines().find_map(|l| l.strip_prefix("uptime: "))?.split_whitespace().next()?.parse().ok()
    }

    /// A ctl reply line with descriptors attached to its first byte.
    fn send_line_fds(s: &UnixStream, l: &Line, fds: &[std::os::fd::OwnedFd]) -> io::Result<()> {
        use std::io::Write;
        let b = l.encode().ok_or_else(|| io::Error::other("line"))?;
        let raw: Vec<i32> = fds.iter().map(|f| f.as_raw_fd()).collect();
        let n = atrium_net::spawn::send_bytes_fds(s.as_raw_fd(), &b, &raw)?;
        (&*s).write_all(&b[n..])
    }

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
        probe: Option<String>,
        probe_expect: Option<String>,
        probe_every: u64,
        probe_bin: String,
        /// atrium-mdns for `.local` names (§6.1); None: not run.
        mdns_bin: Option<String>,
        netd: Option<String>,
        egress_group: Option<String>,
        /// Exit (status 75) when a port arrives or a managed one goes away,
        /// so the service starts again with the ports there are now.
        follow: bool,
    }

    /// The probe's target, from --probe http://HOST/PATH.
    struct ProbeCfg {
        host: String,
        path: String,
        expect: Option<Vec<u8>>,
        every_ms: u64,
    }

    fn probe_cfg(url: &str, expect: Option<&str>, every_s: u64) -> ProbeCfg {
        let rest = url.strip_prefix("http://").unwrap_or_else(|| die("--probe takes a plain http:// URL"));
        let (host, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        if !net_proto::valid_host_name(host) || !net_proto::valid_http_path(path) {
            die("--probe: bad host or path");
        }
        ProbeCfg { host: host.into(), path: path.into(), expect: expect.map(|e| e.as_bytes().to_vec()), every_ms: every_s.max(5) * 1000 }
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
            // Off unless a URL is given: no third party learns when this
            // machine joins a network (spec §4.2, endpoint decision pending).
            probe: None,
            probe_expect: None,
            probe_every: 300,
            probe_bin: "/usr/local/libexec/atrium-probe".into(),
            mdns_bin: Some("/usr/local/libexec/atrium-mdns".into()),
            netd: None,
            egress_group: Some("atrium_egress".into()),
            follow: false,
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
                "--probe" => o.probe = Some(val(&mut a, "--probe")),
                "--no-probe" => o.probe = None,
                "--mdns-bin" => o.mdns_bin = Some(val(&mut a, "--mdns-bin")),
                "--no-mdns" => o.mdns_bin = None,
                "--probe-expect" => o.probe_expect = Some(val(&mut a, "--probe-expect")),
                "--probe-every" => o.probe_every = val(&mut a, "--probe-every").parse().unwrap_or_else(|_| die("--probe-every SECS")),
                "--probe-bin" => o.probe_bin = val(&mut a, "--probe-bin"),
                "--netd-dir" => o.netd = Some(val(&mut a, "--netd-dir")),
                "--follow-ports" => o.follow = true,
                "--egress-group" => o.egress_group = Some(val(&mut a, "--egress-group")),
                "--no-egress-group" => o.egress_group = None,
                _ => die("usage: atrium-net [-v] --iface NAME ... [--wifi NAME] [--lease PATH] [--sock PATH] [--state-dir DIR] [--aura-dir DIR] [--keys-dir DIR] [--no-unbound] [--unbound-dir DIR] [--capsicum] [--probe URL | --no-probe] [--probe-expect TEXT] [--probe-every SECS] [--probe-bin PATH] [--netd-dir DIR] [--follow-ports] [--egress-group NAME | --no-egress-group]"),
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
        /// Lease-process restarts: their times (the last minute's), and
        /// when a parser that keeps dying may be started again.
        lease_restarts: u32,
        lease_deaths: Vec<u64>,
        lease_hold: Option<u64>,
        lease_gen: u32,
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
        /// The DHCPv6 address applied (checked), if any.
        addr6: Option<Ipv6Addr>,
        rs_left: u8,
        rs_at: u64,
        /// Link up (wired: the kernel's link state; Wi-Fi: port open).
        link: bool,
        /// What the connectivity probe last found, and when it runs next.
        inet: Inet,
        portal: Option<String>,
        next_probe: u64,
        /// Failed probes in a row: one failure is retried after
        /// PROBE_CONFIRM_MS before the interface is reported without internet
        /// (a probe sent the moment a lease binds can lose its first packets).
        probe_fails: u8,
    }

    struct Job {
        k: usize,
        id: u64,
        /// Host routes added for this probe (dst, gw, our address).
        pins: Vec<(Ipv4Addr, Ipv4Addr, Ipv4Addr)>,
        deadline: u64,
    }

    struct Prober {
        c: Conn,
        cfg: ProbeCfg,
        busy: Option<Job>,
        next_id: u64,
    }

    struct Ctl {
        c: Conn,
        subscribed: bool,
    }

    /// A VPN tunnel held for cryptad (vpn.md §5): its tun stays open here
    /// while the tunnel is on, so its routes outlive an engine that dies
    /// and nothing goes around it meanwhile.
    /// A tunnel's id, which is also its route domain's name: cryptad's
    /// profile name.
    fn valid_tunnel_id(i: &str) -> bool {
        !i.is_empty() && i.len() <= 32 && i.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    }

    struct Tun {
        id: String,
        iface: String,
        fd: std::os::fd::OwnedFd,
        /// Includes sent with `tunnel-routes`, for the next `tunnel-apply`.
        staged: Vec<Net4>,
        applied: Option<Tunnel>,
        /// Routes added into the tunnel: (destination, prefix, via).
        routes: Vec<(Ipv4Addr, u8, Ipv4Addr)>,
        /// The gateway's host route: (gateway, router, our address).
        pin: Option<(Ipv4Addr, Ipv4Addr, Ipv4Addr)>,
        /// unbound forward zones set for it.
        zones: Vec<String>,
        /// The routing table its routes are in: 0 (the host's) or its route
        /// domain's.
        fib: u32,
        /// A split persona-scoped tunnel's copy of the egress in its table:
        /// (destination, prefix, gateway, our address).
        egress: Vec<(Ipv4Addr, u8, Ipv4Addr, u16)>,
    }

    const MAX_TUNNELS: usize = 8;
    /// Route domains (vpn.md §5.3): FIBs 1..=MAX_DOMAINS, one per
    /// persona-scoped VPN profile.
    const MAX_DOMAINS: u32 = 8;

    struct Net {
        v: bool,
        t0: Instant,
        ifc: Ifc,
        rt: Rtsock,
        unbound: Option<Unbound>,
        host: Vec<(String, Ipv4Addr, u8)>,
        /// The host's IPv6 addresses, by interface; with `host`, read at
        /// start and then kept from the routing socket's address messages.
        host6: Vec<(String, Ipv6Addr)>,
        /// Interface names by index (start-up, then arrivals).
        names: Vec<(u16, String)>,
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
        /// Wired ports with a fixed setting, and the machine's own DNS
        /// servers (used instead of every network's when set).
        wired_manual: Vec<(String, Manual)>,
        own_dns: Vec<IpAddr>,
        /// Ask `own_dns` only over TLS, checked against this name.
        dns_tls: Option<String>,
        /// unbound's uptime when last checked: less means it restarted
        /// (and lost what it was told, which list_forwards can't show for
        /// TLS).
        unbound_up: u64,
        /// A portal sign-in window is open (§8.1): probe every
        /// PORTAL_PROBE_MS until online, or until this time.
        portal_watch_until: Option<u64>,
        dot: Option<DotCheck>,
        egress: Egress,
        /// The default route we installed (interface, router), and the
        /// interface last announced as the egress.
        def4: Option<(usize, Ipv4Addr)>,
        announced: Option<usize>,
        prober: Option<Prober>,
        /// A probe process that died, started again at this time.
        probe_again: Option<(ProbeCfg, u64)>,
        unbound_check_at: u64,
        spawner: Spawner,
        /// Descriptors for the ctl reply being made (`mdns4`), sent with its
        /// first line.
        reply_fds: Vec<std::os::fd::OwnedFd>,
        /// unbound's `.local` stub listener (127.0.0.1:5354), made once.
        mdns_listener: Option<std::os::fd::OwnedFd>,
        /// The running atrium-mdns: its socket and the network it serves.
        mdns: Option<(UnixStream, String, Ipv4Addr, u8, Vec<Ipv6Addr>)>,
        mdns_again_at: u64,
        netd: Option<Dir>,
        /// The interface group pf's NAT rule names (`nat on atrium_egress`):
        /// the egress interface alone is in it. Every interface there was
        /// at start, to take the others out.
        egress_group: Option<String>,
        ports_seen: Vec<String>,
        group_synced: bool,
        tunnels: Vec<Tun>,
        /// A routing socket per route domain's FIB (index fib - 1), opened
        /// before capability mode.
        rtf: Vec<Rtsock>,
        /// Route domains handed out: (name, fib). A domain outlives its
        /// tunnel: with no tunnel applied its table is empty, so the
        /// persona's apps reach nothing (the kill switch, §5.3).
        domains: Vec<(String, u32)>,
        /// The `atrium/vpn` anchor as last loaded, and the egress it named.
        anchor: Option<(String, Option<String>)>,
        /// unbound restarted: the tunnels' zones are to be set again.
        zones_lost: bool,
    }

    /// How long a Wi-Fi address outlives the link.
    const LINK_GRACE_MS: u64 = 60_000;

    impl Net {
        fn now(&self) -> u64 {
            self.t0.elapsed().as_millis() as u64
        }

        /// Verbose log lines carry the daemon's clock (seconds), so a gate can
        /// measure hold-downs from them.
        fn log(&self, s: &str) {
            if self.v {
                eprintln!("atrium-net: [{:.1}] {s}", self.now() as f64 / 1000.0);
            }
        }

        fn event(&mut self, l: &Line) {
            self.ctls.retain_mut(|c| !c.subscribed || c.c.send(l).is_ok());
        }

        fn refresh_host(&mut self) {
            if let Ok(all) = sys::interfaces() {
                self.host = all.iter().flat_map(|i| i.v4.iter().map(move |&(a, p)| (i.name.clone(), a, p))).collect();
                self.host6 = all.iter().flat_map(|i| i.v6.iter().map(move |&a| (i.name.clone(), a))).collect();
            }
        }

        fn addr_event(&mut self, index: u16, add: bool, addr: IpAddr, prefix: u8) {
            let Some(name) = self.names.iter().find(|(i, _)| *i == index).map(|(_, n)| n.clone()) else { return };
            match addr {
                IpAddr::V4(a) => {
                    self.host.retain(|(n, x, _)| !(n == &name && *x == a));
                    if add {
                        self.host.push((name, a, prefix));
                    }
                }
                IpAddr::V6(a) => {
                    self.host6.retain(|(n, x)| !(n == &name && *x == a));
                    if add {
                        self.host6.push((name, a));
                    }
                }
            }
        }

        fn v6_of(&self, name: &str) -> Vec<Ipv6Addr> {
            self.host6.iter().filter(|(n, _)| n == name).map(|&(_, a)| a).collect()
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
                    Action::AddRoute(r) if r.prefix > 0 && self.full_tunnel().is_some() => {
                        // TunnelVision (vpn.md §6): while a full tunnel is
                        // up, no route from a lease beyond its own subnet.
                        self.log(&format!("{ifname}: route {}/{} from the lease not applied: a full tunnel is up", r.dst, r.prefix));
                        continue;
                    }
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
                    Action::ResetMtu => match self.ifc.mtu(ifname) {
                        Ok(m) if m == atrium_net::plan::DEFAULT_MTU as u32 => continue,
                        _ => self.ifc.set_mtu(ifname, atrium_net::plan::DEFAULT_MTU),
                    },
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
            // A full tunnel's DNS servers take every name (vpn.md §5.5);
            // without any, unbound resolves from the roots, through it.
            if let Some(dns) = self.full_tunnel().map(|t| t.dns.clone()) {
                let list: Vec<String> = if dns.is_empty() { vec!["off".into()] } else { dns.iter().map(|a| a.to_string()).collect() };
                if list == self.forwarders {
                    return;
                }
                let c = format!("forward {}", list.join(" "));
                match self.unbound.as_ref().map(|u| u.cmd(&c)) {
                    None => {
                        self.log(&format!("no unbound: would {c}"));
                        self.forwarders = list;
                    }
                    Some(Ok(s)) if s.trim_end() == "ok" => {
                        self.log(&format!("unbound: {c} (tunnel)"));
                        self.forwarders = list;
                    }
                    Some(Ok(s)) => eprintln!("atrium-net: unbound said {:?}", s.trim_end()),
                    Some(Err(e)) => eprintln!("atrium-net: unbound: {e}"),
                }
                return;
            }
            // Own servers over TLS: port 853 and the name unbound checks the
            // certificate against.
            let tls = self.dns_tls.clone().filter(|_| !self.own_dns.is_empty());
            let mut list: Vec<String> = self
                .own_dns
                .iter()
                .map(|a| match &tls {
                    Some(n) => format!("{a}@853#{n}"),
                    None => a.to_string(),
                })
                .collect();
            let own = !list.is_empty();
            for i in self.ifs.iter().filter(|_| !own) {
                if let Some(l) = &i.applied.lease {
                    list.extend(l.dns.iter().map(|a| a.to_string()));
                }
            }
            for i in self.ifs.iter().filter(|_| !own) {
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
                self.log(&format!("no unbound: would forward {}", list.join(" ")));
                self.forwarders = list;
                return;
            };
            // forward_add replaces the root's forwarders as `forward` does;
            // +t: only over TLS, never plain DNS instead.
            let c = if tls.is_some() { format!("forward_add +t . {}", list.join(" ")) } else { format!("forward {}", list.join(" ")) };
            match u.cmd(&c) {
                Ok(s) if s.trim_end() == "ok" => {
                    self.log(&format!("unbound: {c}"));
                    self.forwarders = list;
                }
                Ok(s) => eprintln!("atrium-net: unbound said {:?}", s.trim_end()),
                Err(e) => eprintln!("atrium-net: unbound: {e}"),
            }
        }

        /// Every 10 s: does unbound still forward to our servers? A restart
        /// of local-unbound (or one that was down when we set them) falls
        /// back to forward.conf; set them again.
        fn unbound_tick(&mut self) {
            let now = self.now();
            if now < self.unbound_check_at || self.forwarders.is_empty() {
                return;
            }
            self.unbound_check_at = now + UNBOUND_CHECK_MS;
            let Some(u) = &self.unbound else { return };
            let have = match u.cmd("list_forwards") {
                Ok(s) => root_forwards(&s),
                Err(_) => return, // down: try again next time
            };
            let up = u.cmd("status").ok().and_then(|s| unbound_uptime(&s));
            let restarted = up.is_some_and(|t| t < self.unbound_up);
            self.unbound_up = up.unwrap_or(0);
            if restarted {
                self.zones_lost = true;
            }
            let mut want: Vec<String> = self.forwarders.iter().filter(|x| *x != "off").map(|x| x.split(['%', '@']).next().unwrap_or(x).to_string()).collect();
            let mut have = have;
            want.sort();
            have.sort();
            if want != have || restarted {
                self.log(&format!("unbound forwards {:?}{}, not ours: setting them again", have, if restarted { " (restarted)" } else { "" }));
                self.forwarders.clear();
                self.forward();
            }
            if std::mem::take(&mut self.zones_lost) {
                for k in 0..self.tunnels.len() {
                    self.tunnels[k].zones.clear();
                    self.tunnel_zones(k);
                }
            }
        }

        fn cands(&self) -> Vec<Cand> {
            self.ifs
                .iter()
                .map(|i| Cand {
                    bound: i.applied.lease.as_ref().is_some_and(|l| l.router.is_some()),
                    link: i.link && !(i.wifi && self.wifi_grace.is_some()),
                    wired: !i.wifi,
                    inet: i.inet,
                })
                .collect()
        }

        /// Put the default route where the egress policy says, and tell
        /// atrium-netd and subscribers when the interface changes.
        fn route_default(&mut self) {
            let now = self.now();
            let c = self.cands();
            let (cur, _) = self.egress.decide(&c, now);
            let want = cur.and_then(|k| self.ifs[k].applied.lease.as_ref().and_then(|l| l.router.map(|g| (k, g, l.addr))));
            if want.map(|(k, g, _)| (k, g)) != self.def4 {
                let any = Ipv4Addr::UNSPECIFIED;
                // Every existing default goes first, all its paths: with
                // multipath routing (on by default) RTM_CHANGE edits only the
                // path whose gateway it names, and RTM_ADD puts a second path
                // beside the old one, so flows would hash onto a dead link.
                let mut r = Ok(());
                for _ in 0..16 {
                    match self.rt.del_default() {
                        Ok(()) => continue,
                        Err(e) if e.raw_os_error() == Some(libc::ESRCH) => break,
                        Err(e) => {
                            r = Err(e);
                            break;
                        }
                    }
                }
                if let (Ok(()), Some((_, gw, ia))) = (&r, want) {
                    r = self.rt.route(sys::RTM_ADD, any, 0, gw, ia);
                }
                match r {
                    Ok(()) => {
                        match want {
                            Some((k, gw, _)) => self.log(&format!("default route via {gw} on {}", self.ifs[k].name)),
                            None => self.log("no default route"),
                        }
                        self.def4 = want.map(|(k, g, _)| (k, g));
                    }
                    Err(e) => eprintln!("atrium-net: default route: {e}"),
                }
            }
            if cur != self.announced || !self.group_synced {
                self.sync_egress_group(cur);
            }
            if !self.tunnels.is_empty() {
                self.tunnel_pins();
                self.tunnel_anchor();
                for k in 0..self.tunnels.len() {
                    if self.tunnels[k].fib != 0 {
                        self.domain_egress(k);
                    }
                }
            }
            if cur != self.announced {
                self.announced = cur;
                let name = cur.map_or("none".to_string(), |k| self.ifs[k].name.clone());
                self.log(&format!("egress {name}{}", cur.map_or(String::new(), |k| format!(" (hold-down {} s)", self.egress.hold_ms(k, now) / 1000))));
                self.egress6(cur);
                self.dot_restart();
                let ev = Line::new("egress").with("iface", &name);
                self.event(&ev);
                if let Some(d) = &self.netd {
                    let l = Line::new("egress-changed").with("iface", &name);
                    match d.connect("ctl.sock") {
                        Ok(mut s) => {
                            let _ = s.set_write_timeout(Some(Duration::from_secs(1)));
                            if let Err(e) = l.send(&mut s) {
                                eprintln!("atrium-net: atrium-netd: {e}");
                            }
                        }
                        Err(e) => self.log(&format!("atrium-netd not reachable: {e}")),
                    }
                }
            }
        }

        /// Only the egress interface in the NAT group, so apps' traffic
        /// (100.64.0.0/16, NAT'd by pf) leaves where the default route does.
        fn sync_egress_group(&mut self, cur: Option<usize>) {
            let Some(g) = self.egress_group.clone() else { return };
            let keep = cur.map(|k| self.ifs[k].name.clone());
            for n in self.ports_seen.iter().filter(|n| Some(*n) != keep.as_ref()) {
                if let Err(e) = self.ifc.group(n, &g, false) {
                    eprintln!("atrium-net: {n}: leaving group {g}: {e}");
                }
            }
            if let Some(n) = &keep {
                match self.ifc.group(n, &g, true) {
                    Ok(()) => self.log(&format!("{n} is the only member of {g} (apps' NAT)")),
                    Err(e) => {
                        eprintln!("atrium-net: {n}: joining group {g}: {e}");
                    }
                }
            } else {
                self.log(&format!("{g} is empty: no egress"));
            }
            // Once at start, then on each change (an error is logged, not
            // retried every second).
            self.group_synced = true;
        }

        /// IPv6 follows: only the egress interface's RAs give default
        /// routers; the others' are flushed, and the egress solicits anew.
        fn egress6(&mut self, cur: Option<usize>) {
            if self.ifs.len() < 2 {
                return;
            }
            for k in 0..self.ifs.len() {
                if let Err(e) = self.ifc.no_radr(&self.ifs[k].name, Some(k) != cur) {
                    eprintln!("atrium-net: {}: IPv6 routers: {e}", self.ifs[k].name);
                }
            }
            if let Err(e) = self.ifc.flush_routers6() {
                eprintln!("atrium-net: IPv6 router flush: {e}");
            }
            if let Some(k) = cur {
                let now = self.now();
                self.ifs[k].rs_left = 3;
                self.ifs[k].rs_at = now;
            }
        }

        /// Start the next probe that is due (one at a time), and give up on
        /// one that has hung.
        /// §8.1 P3: on a new network with DNS over TLS on, a TLS server
        /// that does not answer for DOT_FAIL_MS is shown as `dot_failing`
        /// on the egress line, and the bar and Settings offer sign-in.
        fn dot_tick(&mut self) {
            let now = self.now();
            let active = self.dns_tls.is_some() && !self.own_dns.is_empty() && self.announced.is_some() && self.full_tunnel().is_none();
            let name = self.dns_tls.clone().unwrap_or_default();
            let Some(d) = self.dot.as_mut() else { return };
            // Answers: any rcode from the server (NOERROR, NXDOMAIN) is an
            // answer; SERVFAIL is unbound saying it got none over TLS.
            let mut buf = [0u8; 512];
            let mut answered = None;
            while let Ok(n) = d.sock.recv(&mut buf) {
                if n >= 12 && u16::from_be_bytes([buf[0], buf[1]]) == d.id && buf[2] & 0x80 != 0 {
                    answered = Some(buf[3] & 0x0f != 2);
                    d.asked = None;
                }
            }
            if !active {
                if std::mem::take(&mut d.failing) {
                    self.log("DNS over TLS check: off");
                    self.event(&Line::new("dot").with("failing", 0));
                }
                let d = self.dot.as_mut().unwrap();
                d.failing_since = None;
                d.asked = None;
                d.answered = false;
                return;
            }
            let was = d.failing;
            match answered {
                Some(true) => {
                    d.failing_since = None;
                    d.failing = false;
                    d.answered = true;
                }
                Some(false) => {
                    d.failing_since.get_or_insert(now);
                }
                None => {}
            }
            if now < d.next {
                return self.dot_changed(was);
            }
            // A question still out at the next check went unanswered.
            if d.asked.is_some() {
                d.failing_since.get_or_insert(d.asked.unwrap());
            }
            if d.failing_since.is_some_and(|t| now >= t + DOT_FAIL_MS) {
                d.failing = true;
            }
            if now >= d.since_new + DOT_NEW_MS && !d.failing && d.failing_since.is_none() {
                d.next = u64::MAX; // settled on this network
                return self.dot_changed(was);
            }
            d.next = now + DOT_CHECK_MS;
            d.id = d.id.wrapping_add(0x3b1d);
            let q = atrium_net::dot_query(d.id, &name);
            let id = d.id;
            if let Some(u) = &self.unbound {
                let _ = u.cmd(&format!("flush {name}"));
            }
            let d = self.dot.as_mut().unwrap();
            if !q.is_empty() && d.sock.send(&q).is_ok() {
                d.asked = Some(now);
            } else {
                eprintln!("atrium-net: DNS over TLS check: cannot ask unbound (id {id})");
            }
            self.dot_changed(was);
        }

        /// A new network, or new DNS-over-TLS settings: check afresh.
        fn dot_restart(&mut self) {
            let now = self.now();
            let was = self.dot.as_ref().is_some_and(|d| d.failing);
            if let Some(d) = self.dot.as_mut() {
                d.since_new = now;
                d.failing_since = None;
                d.failing = false;
                d.answered = false;
                d.asked = None;
                d.next = now;
            }
            self.dot_changed(was);
        }

        fn dot_changed(&mut self, was: bool) {
            let now_failing = self.dot.as_ref().is_some_and(|d| d.failing);
            if now_failing != was {
                self.log(&format!("DNS over TLS {}", if now_failing { "not answering on this network: a sign-in page may be in the way" } else { "answering" }));
                self.event(&Line::new("dot").with("failing", now_failing as u8));
                // Signed in, as far as DNS over TLS can tell: a watch for the
                // sign-in window ends (with the probe on, the probe says so).
                if !now_failing && self.prober.is_none() && self.portal_watch_until.take().is_some() {
                    self.log("portal watch: DNS over TLS answers again, off");
                }
            }
        }

        fn probe_tick(&mut self) {
            let now = self.now();
            let Some(p) = &self.prober else { return };
            if let Some(j) = &p.busy {
                if now >= j.deadline {
                    let k = j.k;
                    eprintln!("atrium-net: {}: probe timed out; probing off", self.ifs[k].name);
                    let j = self.prober.as_mut().unwrap().busy.take().unwrap();
                    self.unpin(&j.pins);
                    self.prober = None;
                }
                return;
            }
            // Under a full tunnel the probe's packets would go into it (or
            // be blocked): what it found would say nothing about this network.
            if self.full_tunnel().is_some() {
                return;
            }
            let Some(k) = (0..self.ifs.len()).find(|&k| {
                let i = &self.ifs[k];
                i.link && i.applied.lease.is_some() && now >= i.next_probe
            }) else {
                return;
            };
            let i = &self.ifs[k];
            let l = i.applied.lease.as_ref().unwrap();
            let Some(&dns) = l.dns.first() else {
                self.ifs[k].next_probe = now + PROBE_RETRY_MS;
                return;
            };
            let mut pins = Vec::new();
            self.pin(k, dns, &mut pins);
            let p = self.prober.as_mut().unwrap();
            p.next_id += 1;
            let id = p.next_id;
            let host = p.cfg.host.clone();
            let ok = p.c.send(&Line::new("resolve").with("id", id).with("dns", dns).with("name", &host)).is_ok();
            p.busy = Some(Job { k, id, pins, deadline: now + PROBE_DEADLINE_MS });
            self.log(&format!("{}: probing ({host} via {dns})", self.ifs[k].name));
            if !ok {
                self.probe_gone();
            }
        }

        /// A host route to `a` through interface k, unless it is k's own
        /// subnet or k already carries the default route.
        fn pin(&mut self, k: usize, a: Ipv4Addr, pins: &mut Vec<(Ipv4Addr, Ipv4Addr, Ipv4Addr)>) {
            let Some(l) = &self.ifs[k].applied.lease else { return };
            let (Some(gw), ia) = (l.router, l.addr) else { return };
            if in_net(a, l.addr, l.prefix) || self.def4.map(|(d, _)| d) == Some(k) {
                return;
            }
            match self.rt.route(sys::RTM_ADD, a, 32, gw, ia) {
                Ok(()) => pins.push((a, gw, ia)),
                Err(e) => eprintln!("atrium-net: {}: probe route to {a}: {e}", self.ifs[k].name),
            }
        }

        fn unpin(&mut self, pins: &[(Ipv4Addr, Ipv4Addr, Ipv4Addr)]) {
            for &(a, gw, ia) in pins {
                let _ = self.rt.route(sys::RTM_DELETE, a, 32, gw, ia);
            }
        }

        fn probe_gone(&mut self) {
            eprintln!("atrium-net: atrium-probe went away; probing off");
            if let Some(j) = self.prober.as_mut().and_then(|p| p.busy.take()) {
                self.unpin(&j.pins);
            }
            if let Some(p) = self.prober.take() {
                self.probe_again = Some((p.cfg, self.now() + PROBE_AGAIN_MS));
            }
        }

        fn probe_restart(&mut self) {
            let Some((_, at)) = &self.probe_again else { return };
            if self.now() < *at {
                return;
            }
            let (cfg, _) = self.probe_again.take().unwrap();
            match self.spawner.spawn(b'P').and_then(|s| Conn::new(s).map_err(|_| io::Error::other("conn"))) {
                Ok(c) => {
                    self.log("atrium-probe started again");
                    self.prober = Some(Prober { c, cfg, busy: None, next_id: 0 });
                }
                Err(e) => {
                    eprintln!("atrium-net: starting atrium-probe again: {e}");
                    self.probe_again = Some((cfg, self.now() + PROBE_AGAIN_MS));
                }
            }
        }

        /// The lease process of interface k died or misbehaved: start a new
        /// one (from the spawner; capability mode forbids exec here) and
        /// tell it where the link is. More than five deaths in a minute and
        /// the interface's parser is held off for a minute: a frame that
        /// crashes it every time must not keep the host busy.
        fn lease_died(&mut self, k: usize, why: &str) {
            let now = self.now();
            let i = &mut self.ifs[k];
            i.lease_gen += 1;
            i.lease_deaths.retain(|t| now.saturating_sub(*t) < 60_000);
            i.lease_deaths.push(now);
            eprintln!("atrium-net: {}: atrium-lease {why}", i.name);
            if i.lease_deaths.len() > 5 {
                eprintln!("atrium-net: {}: atrium-lease died {} times in a minute; next try in 60 s", i.name, i.lease_deaths.len());
                i.lease_hold = Some(now + 60_000);
                // A connection nothing writes to, until then.
                let (a, b) = UnixStream::pair().unwrap_or_else(|_| die("socketpair"));
                i.lease = Conn::new(a).unwrap_or_else(|_| die("conn"));
                std::mem::forget(b);
                return;
            }
            self.start_lease(k);
        }

        fn start_lease(&mut self, k: usize) {
            let now = self.now();
            let conn = self.spawner.spawn(b'L').and_then(|s| Conn::new(s).map_err(|_| io::Error::other("conn")));
            let i = &mut self.ifs[k];
            let c = match conn {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("atrium-net: {}: starting atrium-lease: {e}", i.name);
                    i.lease_hold = Some(now + 10_000);
                    return;
                }
            };
            i.lease = c;
            i.lease_hold = None;
            i.lease_restarts += 1;
            let _ = i.lease.send(&Line::new("hello").with("mac", fmt_mac(&i.mac)));
            let m = format!("{}: atrium-lease started again ({})", i.name, i.lease_restarts);
            self.log(&m);
            // The new parser knows nothing: DHCP again if the link is up and
            // DHCP is in use (the applied lease stays until a new one comes).
            if self.ifs[k].link && self.manual_for(k).is_none() {
                let _ = self.ifs[k].lease.send(&Line::new("up"));
            }
        }

        /// atrium-mdns follows the active network (§6.1): one per network,
        /// with that interface's mDNS sockets; none without one. A dead one
        /// is started again after 5 s.
        fn mdns_tick(&mut self) {
            let Some(lis) = &self.mdns_listener else { return };
            let want = self.announced.and_then(|k| self.ifs.get(k)).and_then(|i| i.applied.lease.as_ref().map(|x| (i.name.clone(), x.addr, x.prefix)));
            // The interface's IPv6 /64s (not link-local): AAAA answers must
            // fall in one of them.
            let want = want.map(|(n, a, p)| {
                let mut v6: Vec<Ipv6Addr> = self
                    .v6_of(&n)
                    .into_iter()
                    .filter(|x| x.segments()[0] & 0xffc0 != 0xfe80 && !x.is_loopback())
                    .map(|x| Ipv6Addr::from(u128::from(x) & !((1u128 << 64) - 1)))
                    .collect();
                v6.sort();
                v6.dedup();
                (n, a, p, v6)
            });
            if let Some((s, ..)) = &self.mdns {
                let mut b = [0u8; 16];
                let dead = match std::io::Read::read(&mut &*s, &mut b) {
                    Ok(0) => true,
                    Ok(_) => false,
                    Err(e) => e.kind() != io::ErrorKind::WouldBlock,
                };
                if dead {
                    self.log("atrium-mdns died");
                    self.mdns = None;
                    self.mdns_again_at = self.now() + 5000;
                }
            }
            let have = self.mdns.as_ref().map(|(_, n, a, p, v6)| (n.clone(), *a, *p, v6.clone()));
            if have == want {
                return;
            }
            if self.mdns.take().is_some() {
                self.log("atrium-mdns stopped (network changed)");
            }
            let Some((name, addr, prefix, v6)) = want else { return };
            let now = self.now();
            if now < self.mdns_again_at {
                return;
            }
            let r = (|| -> io::Result<UnixStream> {
                let lis = lis.try_clone()?;
                let child = self.spawner.spawn(b'D')?;
                let (rx, tx) = self.spawner.oneshot4(addr)?;
                let mut l = Line::new("start").with("addr", format!("{addr}/{prefix}"));
                if !v6.is_empty() {
                    l = l.with("v6", v6.iter().map(|x| format!("{x}/64")).collect::<Vec<_>>().join(","));
                }
                send_line_fds(&child, &l, &[lis, rx, tx])?;
                child.set_nonblocking(true)?;
                Ok(child)
            })();
            match r {
                Ok(c) => {
                    let six: String = v6.iter().map(|x| format!(" {x}/64")).collect();
                    self.log(&format!("atrium-mdns: .local on {name} ({addr}/{prefix}{six})"));
                    self.mdns = Some((c, name, addr, prefix, v6));
                }
                Err(e) => {
                    eprintln!("atrium-net: atrium-mdns on {name}: {e}");
                    self.mdns_again_at = now + 10_000;
                }
            }
        }

        fn lease_restart_tick(&mut self) {
            let now = self.now();
            for k in 0..self.ifs.len() {
                if self.ifs[k].lease_hold.is_some_and(|t| now >= t) {
                    self.start_lease(k);
                }
            }
        }

        fn probe_line(&mut self, l: &Line) {
            let Some(p) = self.prober.as_mut() else { return };
            let Some(j) = p.busy.as_ref() else { return };
            if l.num::<u64>("id") != Some(j.id) {
                return;
            }
            let k = j.k;
            match l.verb.as_str() {
                "addr" => {
                    let a = l.get("a").and_then(|x| x.parse::<Ipv4Addr>().ok());
                    let ours = |a: Ipv4Addr| self.host.iter().any(|&(_, h, _)| h == a);
                    match a {
                        Some(a) if ordinary_unicast(a) && !ours(a) => {
                            let mut pins = std::mem::take(&mut self.prober.as_mut().unwrap().busy.as_mut().unwrap().pins);
                            self.pin(k, a, &mut pins);
                            let p = self.prober.as_mut().unwrap();
                            let j = p.busy.as_mut().unwrap();
                            j.pins = pins;
                            let mut x = Line::new("fetch").with("id", j.id).with("a", a).with("host", &p.cfg.host).with("path", &p.cfg.path);
                            if let Some(e) = &p.cfg.expect {
                                x = x.with_hex("expect", e);
                            }
                            if p.c.send(&x).is_err() {
                                self.probe_gone();
                            }
                        }
                        _ => self.probe_done(k, Inet::No, None, "bad address"),
                    }
                }
                "http" => {
                    let status = l.num::<u16>("status").unwrap_or(0);
                    let matched = l.get("match") == Some("1");
                    let expect = p.cfg.expect.is_some();
                    let loc = l.hex("location").filter(|v| v.len() <= 256 && v.iter().all(|&c| (0x21..0x7f).contains(&c))).map(|v| String::from_utf8_lossy(&v).into_owned());
                    let good = if expect { status == 200 && matched } else { status == 204 };
                    if good {
                        self.probe_done(k, Inet::Yes, None, "ok");
                    } else {
                        self.probe_done(k, Inet::Captive, loc, &format!("HTTP {status}"));
                    }
                }
                "fail" => {
                    let why = l.get("why").unwrap_or("?").to_string();
                    self.probe_done(k, Inet::No, None, &why);
                }
                _ => {}
            }
        }

        fn probe_done(&mut self, k: usize, inet: Inet, portal: Option<String>, why: &str) {
            let now = self.now();
            if let Some(j) = self.prober.as_mut().and_then(|p| p.busy.take()) {
                self.unpin(&j.pins);
            }
            let every = self.prober.as_ref().map_or(PROBE_RETRY_MS, |p| p.cfg.every_ms);
            let i = &mut self.ifs[k];
            // Lost the lease meanwhile: the answer is about a network we left.
            if i.applied.lease.is_none() {
                return;
            }
            if inet == Inet::No && i.inet != Inet::No && i.probe_fails == 0 {
                i.probe_fails = 1;
                i.next_probe = now + PROBE_CONFIRM_MS;
                let name = i.name.clone();
                self.log(&format!("{name}: probe failed ({why}), trying again"));
                return;
            }
            i.probe_fails = if inet == Inet::No { i.probe_fails.saturating_add(1) } else { 0 };
            i.next_probe = now + if inet == Inet::Yes { every } else { PROBE_RETRY_MS.min(every) };
            let watching = self.portal_watch_until.is_some_and(|t| now < t);
            let i = &mut self.ifs[k];
            if watching && inet != Inet::Yes {
                i.next_probe = now + PORTAL_PROBE_MS;
            }
            let old = i.inet;
            i.inet = inet;
            if watching && inet == Inet::Yes && self.announced == Some(k) {
                self.portal_watch_until = None;
                self.log(&format!("{}: online: portal watch ends", self.ifs[k].name));
            }
            let i = &mut self.ifs[k];
            if old == inet && i.portal == portal {
                return;
            }
            i.portal = portal.clone();
            let name = i.name.clone();
            self.log(&format!("{name}: internet {} ({why})", inet.word()));
            if old.ok() && !inet.ok() && self.announced == Some(k) {
                self.egress.flap(k, now);
            }
            let mut ev = Line::new("internet").with("iface", &name).with("state", inet.word());
            if let Some(p) = &portal {
                ev = ev.with_hex("portal", p.as_bytes());
            }
            self.event(&ev);
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
            self.ifs[k].addr6 = None;
            let _ = self.ifc.accept_rtadv(&name, false);
            for a in self.v6_of(&name) {
                if a.segments()[0] & 0xffc0 != 0xfe80 {
                    match self.ifc.del_addr6(&name, a) {
                        Ok(()) => self.log(&format!("{name}: removed {a}")),
                        Err(e) => eprintln!("atrium-net: {name}: removing {a}: {e}"),
                    }
                }
            }
            let _ = self.ifc.accept_rtadv(&name, true);
        }

        /// SLAAC addresses made before atrium-net took the port (rc.conf's
        /// `accept_rtadv`, an earlier owner) were made with temporary
        /// addresses off, and the kernel adds a temporary address only when
        /// it makes a new stable one. A prefix with a SLAAC address and no
        /// temporary one loses its SLAAC addresses here; the next RA (the
        /// router solicitations follow) makes them again, with a temporary
        /// address. Prefixes that have one (an atrium-net restart) are left
        /// alone, so restarts do not break connections.
        fn clear_unprivate_slaac(&mut self, name: &str, v6: &[Ipv6Addr]) {
            let p64 = |a: &Ipv6Addr| u64::from_be_bytes(a.octets()[..8].try_into().unwrap());
            let flagged: Vec<(Ipv6Addr, u32)> = v6.iter().filter(|a| a.segments()[0] & 0xffc0 != 0xfe80).filter_map(|&a| self.ifc.addr6_flags(name, a).ok().map(|f| (a, f))).collect();
            let auto = |f: u32| f & sys::IN6_IFF_AUTOCONF != 0;
            let temp = |f: u32| f & sys::IN6_IFF_TEMPORARY != 0;
            let stale: Vec<Ipv6Addr> = flagged.iter().filter(|(a, f)| auto(*f) && !flagged.iter().any(|(b, g)| temp(*g) && p64(a) == p64(b))).map(|(a, _)| *a).collect();
            if stale.is_empty() {
                return;
            }
            // RAs off while deleting (the kernel race, §5.3); enable_inet6 turns them on.
            let _ = self.ifc.accept_rtadv(name, false);
            for a in stale {
                match self.ifc.del_addr6(name, a) {
                    Ok(()) => self.log(&format!("{name}: removed {a} (SLAAC without a temporary address, made before atrium-net)")),
                    Err(e) => eprintln!("atrium-net: {name}: removing {a}: {e}"),
                }
            }
        }

        fn wifi_iface(&mut self) -> Option<usize> {
            self.ifs.iter().position(|i| i.wifi)
        }

        fn lose(&mut self, k: usize, why: &str) {
            self.ifs[k].inet = Inet::Unknown;
            self.ifs[k].portal = None;
            self.ifs[k].next_probe = 0;
            self.ifs[k].probe_fails = 0;
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

        fn save_settings(&mut self) {
            let mut out = String::from("# atrium-net: fixed settings of wired ports, own DNS servers; written by atrium-net\n");
            for (n, m) in &self.wired_manual {
                out.push_str(&String::from_utf8_lossy(&nlines::with_manual(Line::new("wired").with("iface", n), m).encode().unwrap_or_default()));
            }
            if !self.own_dns.is_empty() {
                let tls = self.dns_tls.as_ref().map(|n| format!(" tls={n}")).unwrap_or_default();
                out.push_str(&format!("dns servers={}{tls}\n", self.own_dns.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(",")));
            }
            if let Err(e) = self.state.write(SETTINGS_FILE, out.as_bytes()) {
                eprintln!("atrium-net: saving settings: {e}");
            }
        }

        /// The fixed setting this interface uses now: the wired port's, or
        /// the Wi-Fi network's.
        fn manual_for(&self, k: usize) -> Option<Manual> {
            let i = &self.ifs[k];
            if i.wifi {
                self.wifi.as_ref().and_then(|w| w.manual_now())
            } else {
                self.wired_manual.iter().find(|(n, _)| *n == i.name).map(|(_, m)| m.clone())
            }
        }

        /// The link is up: DHCP, or the fixed setting.
        fn link_up_lease(&mut self, k: usize) {
            match self.manual_for(k) {
                Some(m) => {
                    self.log(&format!("{}: fixed address {}/{}", self.ifs[k].name, m.addr, m.prefix));
                    self.take_lease(k, fixed_lease(&m));
                }
                None => {
                    let _ = self.ifs[k].lease.send(&Line::new("up"));
                }
            }
        }

        /// A setting changed for interface `k`: from DHCP to fixed, fixed
        /// to another, or back to DHCP.
        fn reapply(&mut self, k: usize) {
            if !self.ifs[k].link {
                return;
            }
            let fixed = self.manual_for(k);
            let was_fixed = self.ifs[k].applied.lease.as_ref().is_some_and(|l| l.server.is_unspecified());
            if fixed.is_some() && !was_fixed {
                let _ = self.ifs[k].lease.send(&Line::new("down"));
            }
            if fixed.is_none() && was_fixed {
                self.lose(k, "dhcp-again");
            }
            self.link_up_lease(k);
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
                                if up {
                                    self.ifs[k].link = true;
                                    self.link_up_lease(k);
                                } else {
                                    let _ = self.ifs[k].lease.send(&Line::new("down"));
                                }
                                // Off the network: atrium-lease keeps the lease
                                // and confirms it on the next join (INIT-REBOOT).
                                // The address stays for a grace period so a
                                // short drop does not reset every connection;
                                // nothing reaches the dead link meanwhile.
                                self.ifs[k].link = up;
                                if !up {
                                    self.egress.flap(k, now);
                                }
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
                        Act::Event(l) => {
                            if l.get("what") == Some("network-move") {
                                let to = l.hex("to").map(|t| String::from_utf8_lossy(&t).into_owned()).unwrap_or_default();
                                self.log(&format!("wifi: weak signal, moving to {to} ({} dBm there)", l.get("rssi").unwrap_or("?")));
                            }
                            self.event(&l)
                        }
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
                "tunnel-open" | "tunnel-routes" | "tunnel-apply" | "tunnel-close" => self.tunnel_ctl(l),
                "route-domain" => self.route_domain_ctl(l),
                // Printer discovery's sockets (printing.md §7.1) on the
                // active interface; only atrium-netconf's root-only
                // `print-discovery` socket asks for them.
                "mdns4" => {
                    let Some(i) = self.announced.and_then(|k| self.ifs.get(k)) else { return err("no-network") };
                    let Some(addr) = i.applied.lease.as_ref().map(|x| x.addr) else { return err("no-network") };
                    let name = i.name.clone();
                    match self.spawner.mdns4(addr) {
                        Ok((r, w)) => {
                            self.log(&format!("mdns4: discovery sockets on {name} ({addr})"));
                            self.reply_fds = vec![r, w];
                            vec![Line::new("mdns4").with("iface", name), Line::new("ok")]
                        }
                        Err(e) => {
                            eprintln!("atrium-net: mdns4 on {name}: {e}");
                            err("failed")
                        }
                    }
                }
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
                    let known = self.wifi.as_ref().unwrap().saved.get(&s).is_some_and(|p| p.open == open);
                    let trial = l.get("trial") == Some("1") && (l.get("secret").is_some() || l.get("handle").is_some());
                    // No secret: change the settings of a network already
                    // remembered; its credential stays as it is in aura-keys.
                    if !open && !(known && l.get("secret").is_none() && l.get("handle").is_none()) {
                        // The passphrase, or the handle to what the user typed
                        // into Arca's field (aura-keys redeems it at Arca).
                        let r = match (l.hex("secret"), l.hex("handle")) {
                            (Some(secret), None) => Line::new("remember").with_hex("ssid", &s).with_hex("secret", &secret),
                            (None, Some(h)) if h.len() == 16 => Line::new("remember").with_hex("ssid", &s).with_hex("handle", &h),
                            _ => return err("no-secret"),
                        };
                        let r = self.keys_call(&if trial { r.with("trial", 1) } else { r });
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
                        // Kept from the saved profile (set by `static`).
                        manual: None,
                    };
                    if trial && !open {
                        self.wifi.as_mut().unwrap().remember_on_trial(p);
                        return ok();
                    }
                    let a = self.wifi.as_mut().unwrap().remember(p, now);
                    self.wifi_acts(a);
                    ok()
                }
                "static" => {
                    let m = nlines::manual(l);
                    if l.get("maddr").is_some() && m.is_none() {
                        return err("bad-address");
                    }
                    let k = if let Some(s) = l.hex("ssid") {
                        let on = {
                            let Some(w) = self.wifi.as_mut() else { return err("no-wifi") };
                            if !w.set_manual(&s, m) {
                                return err("not-saved");
                            }
                            w.ssid() == Some(s.as_slice())
                        };
                        self.save();
                        self.ifs.iter().position(|i| i.wifi).filter(|_| on)
                    } else if let Some(n) = l.get("iface") {
                        let Some(k) = self.ifs.iter().position(|i| i.name == n && !i.wifi) else { return err("no-such-port") };
                        self.wired_manual.retain(|(x, _)| x != n);
                        if let Some(m) = m {
                            self.wired_manual.push((n.to_string(), m));
                        }
                        self.save_settings();
                        Some(k)
                    } else {
                        return err("no-target");
                    };
                    if let Some(k) = k {
                        self.reapply(k);
                    }
                    ok()
                }
                "dns" => {
                    let Some((v, tls)) = parse_own_dns(l) else { return err("bad-dns") };
                    // Without certificates to check with, unbound would take
                    // any: refuse rather than pretend.
                    if tls.is_some() {
                        if let Some(u) = &self.unbound {
                            let b = u.cmd("get_option tls-cert-bundle").unwrap_or_default();
                            let b = b.trim();
                            if b.is_empty() || b.starts_with("error") {
                                return err("no-tls-bundle");
                            }
                        }
                    }
                    self.log(&format!("own DNS servers: {v:?}{}", tls.as_ref().map(|n| format!(" over TLS as {n}")).unwrap_or_default()));
                    self.own_dns = v;
                    self.dns_tls = tls;
                    self.save_settings();
                    self.dot_restart();
                    // Back to the networks' own: forward() keeps the list
                    // when it is unchanged, so clear what it compares with.
                    self.forwarders.clear();
                    self.forward();
                    ok()
                }
                // A portal sign-in window opened or closed (§8.1).
                "portal-watch" => {
                    let now = self.now();
                    if l.get("on") == Some("1") {
                        self.portal_watch_until = Some(now + PORTAL_WATCH_MS);
                        if let Some(k) = self.announced {
                            self.ifs[k].next_probe = now;
                        }
                        self.log(&format!("portal watch on{}", if self.prober.is_none() { " (no probe: nothing to watch with)" } else { "" }));
                    } else if self.portal_watch_until.take().is_some() {
                        self.log("portal watch off");
                    }
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
            for i in &self.ifs {
                let mut l = Line::new("iface").with("name", &i.name).with("mac", fmt_mac(&i.mac));
                let v6: Vec<String> = self.v6_of(&i.name).iter().map(|a| a.to_string()).collect();
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
                    }
                    None => l = l.with("state", "none"),
                }
                if !v6.is_empty() {
                    l = l.with("inet6", v6.join(","));
                }
                if !i.dns6.is_empty() {
                    l = l.with("dns6", i.dns6.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(","));
                }
                let k = self.ifs.iter().position(|x| x.name == i.name);
                // A Wi-Fi link gone but its address kept in the grace period: `lost`.
                let lost = i.wifi && i.applied.lease.is_some() && self.wifi_grace.is_some();
                l = l.with("link", if lost { "lost" } else if i.link { "up" } else { "down" }).with("internet", i.inet.word());
                if let Some(p) = &i.portal {
                    l = l.with_hex("portal", p.as_bytes());
                }
                if i.wifi {
                    l = l.with("wifi", 1);
                }
                if let Ok(m) = self.ifc.mtu(&i.name) {
                    l = l.with("mtu", m);
                }
                // An RA's MTU, while it is lower than the port's.
                if let Some(m) = self.ifc.linkmtu6(&i.name).ok().filter(|&m| m > 0) {
                    l = l.with("mtu6", m);
                }
                if let Some(m) = k.and_then(|k| self.manual_for(k)) {
                    l = nlines::with_manual(l, &m);
                }
                if k.is_some() && self.announced == k {
                    l = l.with("egress", 1);
                    if self.portal_watch_until.is_some() {
                        l = l.with("portal_watch", 1);
                    }
                    // What the DNS-over-TLS check found on this network, when
                    // it found anything (§8.1 P3).
                    match self.dot.as_ref().filter(|_| self.dns_tls.is_some() && !self.own_dns.is_empty()) {
                        Some(d) if d.failing => l = l.with("dot", "failing"),
                        Some(d) if d.answered => l = l.with("dot", "ok"),
                        _ => {}
                    }
                }
                if self.mdns.as_ref().is_some_and(|m| m.1 == i.name) {
                    l = l.with("mdns", 1);
                }
                l = l.with("refused", i.refusals).with("sends_refused", i.sends_refused).with("lease_restarts", i.lease_restarts);
                if !i.last_refusal.is_empty() {
                    l = l.with("last_refusal", &i.last_refusal);
                }
                v.push(l);
            }
            if !self.own_dns.is_empty() {
                let mut l = Line::new("dns").with("own", self.own_dns.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(","));
                if let Some(n) = &self.dns_tls {
                    l = l.with("tls", n);
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
            for t in &self.tunnels {
                let mut l = Line::new("tunnel").with("id", &t.id).with("iface", &t.iface).with("applied", t.applied.is_some() as u8);
                if let Some(a) = &t.applied {
                    l = l
                        .with("addr", a.addrs.iter().map(Net4::to_string).collect::<Vec<_>>().join(","))
                        .with("full", a.full as u8)
                        .with("block", a.block as u8)
                        .with("gw", format!("{}:{}", a.gateway, a.port))
                        .with("routes", t.routes.len())
                        .with("pinned", t.pin.is_some() as u8)
                        .with("zones", t.zones.len())
                        .with("fib", t.fib);
                }
                v.push(l);
            }
            for (n, f) in &self.domains {
                v.push(Line::new("domain").with("name", n).with("fib", *f));
            }
            v.push(Line::new("ok"));
            v
        }

        /// `route-domain name=X` (vpn.md §5.3): the routing table for VPN
        /// profile X's persona scope, made on first ask. portcullisd asks
        /// it when it starts an instance in a persona whose `vpn` is X, and
        /// puts the instance's network there through jaild.
        fn route_domain_ctl(&mut self, l: &Line) -> Vec<Line> {
            let err = |why: &str| vec![Line::new("error").with("why", why)];
            let Some(name) = l.get("name").filter(|i| valid_tunnel_id(i)).map(str::to_string) else {
                return err("bad-name");
            };
            match self.domain_fib(&name) {
                Some(f) => vec![Line::new("domain").with("name", &name).with("fib", f), Line::new("ok")],
                None => err("no-fib"),
            }
        }

        /// Domain `name`'s FIB, given out if it has none yet.
        fn domain_fib(&mut self, name: &str) -> Option<u32> {
            if let Some(&(_, f)) = self.domains.iter().find(|(n, _)| n == name) {
                return Some(f);
            }
            let f = (1..=self.rtf.len() as u32).find(|f| !self.domains.iter().any(|(_, g)| g == f))?;
            self.domains.push((name.to_string(), f));
            self.log(&format!("route domain {name}: fib {f}"));
            Some(f)
        }

        /// The routing socket for table `fib`.
        fn rts(&mut self, fib: u32) -> &mut Rtsock {
            if fib == 0 {
                &mut self.rt
            } else {
                &mut self.rtf[fib as usize - 1]
            }
        }

        /// The applied full tunnel, if there is one (there is at most one).
        fn full_tunnel(&self) -> Option<&Tunnel> {
            self.tunnels.iter().filter_map(|t| t.applied.as_ref()).find(|a| a.full && !a.scoped)
        }

        /// `tunnel-open`, `tunnel-routes`, `tunnel-apply`, `tunnel-close`
        /// (vpn.md §5): cryptad's proposals, checked here before anything
        /// changes.
        fn tunnel_ctl(&mut self, l: &Line) -> Vec<Line> {
            let err = |why: &str| vec![Line::new("error").with("why", why)];
            let Some(id) = l.get("id").filter(|i| valid_tunnel_id(i)).map(str::to_string) else {
                return err("bad-id");
            };
            let k = self.tunnels.iter().position(|t| t.id == id);
            match (l.verb.as_str(), k) {
                ("tunnel-open", Some(k)) => match self.tunnels[k].fd.try_clone() {
                    Ok(fd) => {
                        self.reply_fds = vec![fd];
                        vec![Line::new("tunnel").with("id", &id).with("iface", &self.tunnels[k].iface), Line::new("ok")]
                    }
                    Err(_) => err("dup"),
                },
                ("tunnel-open", None) => {
                    if self.tunnels.len() >= MAX_TUNNELS {
                        return err("too-many");
                    }
                    let (fd, iface) = match self.spawner.tun() {
                        Ok(x) => x,
                        Err(e) => {
                            eprintln!("atrium-net: tunnel {id}: {e}");
                            return err("no-tun");
                        }
                    };
                    if let Err(e) = self.ifc.up(&iface) {
                        eprintln!("atrium-net: {iface}: up: {e}");
                    }
                    let dup = match fd.try_clone() {
                        Ok(d) => d,
                        Err(_) => return err("dup"),
                    };
                    self.log(&format!("tunnel {id}: {iface} opened"));
                    self.tunnels.push(Tun { id: id.clone(), iface: iface.clone(), fd, staged: Vec::new(), applied: None, routes: Vec::new(), pin: None, zones: Vec::new(), fib: 0, egress: Vec::new() });
                    self.reply_fds = vec![dup];
                    vec![Line::new("tunnel").with("id", &id).with("iface", &iface), Line::new("ok")]
                }
                (_, None) => err("no-tunnel"),
                ("tunnel-routes", Some(k)) => {
                    let add: Option<Vec<Net4>> = l.get("add").unwrap_or("").split(',').filter(|s| !s.is_empty()).map(Net4::parse).collect();
                    let Some(add) = add else { return err("malformed") };
                    let t = &mut self.tunnels[k];
                    if t.staged.len() + add.len() > net_proto::tunnel::MAX_INCLUDES {
                        t.staged.clear();
                        return err("routes");
                    }
                    t.staged.extend(add);
                    vec![Line::new("ok")]
                }
                ("tunnel-apply", Some(k)) => {
                    let staged = std::mem::take(&mut self.tunnels[k].staged);
                    let Some(mut t) = Tunnel::from_line(l, staged) else { return err("malformed") };
                    let iface = self.tunnels[k].iface.clone();
                    if let Err(r) = check_tunnel(&mut t, &self.host_view(&iface)) {
                        eprintln!("atrium-net: tunnel {id}: refused ({})", r.word());
                        return err(r.word());
                    }
                    // One full tunnel at a time; and no two tunnels with the
                    // same network.
                    let others = self.tunnels.iter().enumerate().filter(|(j, _)| *j != k).filter_map(|(_, o)| o.applied.as_ref());
                    for o in others {
                        if (t.full && o.full && !t.scoped && !o.scoped) || t.includes.iter().chain(&t.addrs).any(|n| o.includes.iter().chain(&o.addrs).any(|m| m.contains(n.net()) || n.contains(m.net()))) {
                            return err("overlap");
                        }
                    }
                    self.tunnel_withdraw(k);
                    self.tunnels[k].applied = Some(t);
                    // pf before routes: with block, nothing goes out the
                    // physical interface from here on but the engine.
                    if !self.tunnel_anchor() {
                        self.tunnels[k].applied = None;
                        self.tunnel_anchor();
                        return err("pf");
                    }
                    self.tunnel_apply(k);
                    vec![Line::new("ok")]
                }
                ("tunnel-close", Some(k)) => {
                    self.tunnel_withdraw(k);
                    let t = self.tunnels.remove(k);
                    self.log(&format!("tunnel {id}: {} closed", t.iface));
                    drop(t);
                    self.tunnel_anchor();
                    self.forward();
                    vec![Line::new("ok")]
                }
                _ => err("bad-request"),
            }
        }

        /// Addresses, routes, the gateway's pin and DNS for tunnel k.
        fn tunnel_apply(&mut self, k: usize) {
            let Some(t) = self.tunnels[k].applied.clone() else { return };
            let iface = self.tunnels[k].iface.clone();
            if let Err(e) = self.ifc.set_mtu(&iface, t.mtu) {
                eprintln!("atrium-net: {iface}: mtu {}: {e}", t.mtu);
            }
            /* A persona's tunnel lives in its route domain (§5.3): the tun
             * goes in that table before its addresses, so their routes and
             * what arrives through it are the domain's, never the host's. */
            if t.scoped {
                let Some(fib) = self.domain_fib(&self.tunnels[k].id.clone()) else {
                    eprintln!("atrium-net: tunnel {}: no routing table left", self.tunnels[k].id);
                    return;
                };
                if let Err(e) = self.ifc.set_fib(&iface, fib) {
                    eprintln!("atrium-net: {iface}: fib {fib}: {e}");
                    return;
                }
                self.tunnels[k].fib = fib;
            }
            for a in &t.addrs {
                match self.ifc.add_addr_p2p(&iface, a.addr, a.prefix, a.addr) {
                    Err(e) if e.raw_os_error() != Some(libc::EEXIST) => eprintln!("atrium-net: {iface}: {a}: {e}"),
                    _ => {}
                }
            }
            // The gateway's host route first, so the engine's packets never
            // follow the tunnel's own routes into it.
            self.tunnel_pins();
            if t.scoped {
                return self.domain_apply(k, &t);
            }
            let via = t.addrs[0].addr;
            let mut want: Vec<(Ipv4Addr, u8)> = t.addrs.iter().filter(|a| a.prefix < 32).map(|a| (a.net(), a.prefix)).collect();
            want.extend(t.includes.iter().map(|n| (n.net(), n.prefix)));
            if t.full {
                // Two halves, not the default: more specific than any
                // default route, which stays the egress policy's.
                want.push((Ipv4Addr::new(0, 0, 0, 0), 1));
                want.push((Ipv4Addr::new(128, 0, 0, 0), 1));
            }
            for (dst, prefix) in want {
                let r = match self.rt.route(sys::RTM_ADD, dst, prefix, via, via) {
                    Err(e) if e.raw_os_error() == Some(libc::EEXIST) => self.rt.route(sys::RTM_CHANGE, dst, prefix, via, via),
                    r => r,
                };
                match r {
                    Ok(()) => self.tunnels[k].routes.push((dst, prefix, via)),
                    Err(e) => eprintln!("atrium-net: {iface}: route {dst}/{prefix}: {e}"),
                }
            }
            if t.full {
                self.lease_routes(false);
            }
            self.refresh_host();
            self.forward();
            self.tunnel_zones(k);
            self.log(&format!(
                "tunnel {}: applied on {iface}: {} route(s){}{}, gateway {}:{}",
                self.tunnels[k].id,
                self.tunnels[k].routes.len(),
                if t.full { ", all traffic" } else { "" },
                if t.block { ", blocking when not connected" } else { "" },
                t.gateway,
                t.port
            ));
        }

        /// A persona-scoped tunnel (§5.3): its routes in its route
        /// domain's table, on-link through the tunnel (the tunnel's address
        /// is the host table's, not the domain's, so no gateway there), and
        /// nothing of the host's own: no DNS, no lease routes withheld.
        /// A split tunnel's table also gets the egress (§5.3: "FIB 0's
        /// routes copied for everything else"), kept by `domain_egress`.
        fn domain_apply(&mut self, k: usize, t: &Tunnel) {
            let iface = self.tunnels[k].iface.clone();
            let fib = self.tunnels[k].fib;
            let ia = t.addrs[0].addr;
            let mut want: Vec<(Ipv4Addr, u8)> = t.addrs.iter().map(|a| (a.net(), a.prefix)).collect();
            want.extend(t.includes.iter().map(|n| (n.net(), n.prefix)));
            if t.full {
                want.push((Ipv4Addr::new(0, 0, 0, 0), 1));
                want.push((Ipv4Addr::new(128, 0, 0, 0), 1));
            }
            for (dst, prefix) in want {
                let r = match self.rts(fib).route(sys::RTM_ADD, dst, prefix, Ipv4Addr::UNSPECIFIED, ia) {
                    Err(e) if e.raw_os_error() == Some(libc::EEXIST) => self.rts(fib).route(sys::RTM_CHANGE, dst, prefix, Ipv4Addr::UNSPECIFIED, ia),
                    r => r,
                };
                match r {
                    Ok(()) => self.tunnels[k].routes.push((dst, prefix, Ipv4Addr::UNSPECIFIED)),
                    Err(e) => eprintln!("atrium-net: {iface}: fib {fib}: route {dst}/{prefix}: {e}"),
                }
            }
            if !t.full {
                self.domain_egress(k);
            }
            self.log(&format!(
                "tunnel {}: applied on {iface} in route domain fib {fib}: {} route(s){}, gateway {}:{}",
                self.tunnels[k].id,
                self.tunnels[k].routes.len(),
                if t.full { ", all traffic" } else { "" },
                t.gateway,
                t.port
            ));
        }

        /// A split persona-scoped tunnel's way to everything else: the
        /// egress's subnet and its default router, copied into the
        /// domain's table, moved when the egress moves.
        fn domain_egress(&mut self, k: usize) {
            let fib = self.tunnels[k].fib;
            self.domain_egress_clear(k);
            if fib == 0 || !self.tunnels[k].applied.as_ref().is_some_and(|a| a.scoped && !a.full) {
                return;
            }
            let Some((e, r)) = self.def4 else { return };
            let Some(l) = self.ifs[e].applied.lease.clone() else { return };
            let net = Ipv4Addr::from(u32::from(l.addr) & net_proto::mask(l.prefix));
            // The subnet straight out the egress (by its index: its
            // addresses are the host's table's), then the router.
            let index = self.ifs[e].index;
            for (dst, prefix, gw, idx) in [(net, l.prefix, Ipv4Addr::UNSPECIFIED, index), (Ipv4Addr::UNSPECIFIED, 0, r, 0)] {
                let rt = self.rts(fib);
                let res = if idx != 0 { rt.route_link(sys::RTM_ADD, dst, prefix, idx) } else { rt.route(sys::RTM_ADD, dst, prefix, gw, l.addr) };
                match res {
                    Ok(()) => self.tunnels[k].egress.push((dst, prefix, gw, idx)),
                    Err(e) => eprintln!("atrium-net: fib {fib}: egress route {dst}/{prefix}: {e}"),
                }
            }
        }

        /// Remove what `domain_egress` put in tunnel k's table.
        fn domain_egress_clear(&mut self, k: usize) {
            let fib = self.tunnels[k].fib;
            for (dst, prefix, gw, idx) in std::mem::take(&mut self.tunnels[k].egress).into_iter().rev() {
                let rt = self.rts(fib);
                let _ = if idx != 0 { rt.route_link(sys::RTM_DELETE, dst, prefix, idx) } else { rt.route(sys::RTM_DELETE, dst, prefix, gw, gw) };
            }
        }

        /// Undo tunnel k's routes, pin and zones (its tun stays open, its
        /// pf rules stay until the anchor is written again).
        fn tunnel_withdraw(&mut self, k: usize) {
            let was_full = self.tunnels[k].applied.as_ref().is_some_and(|a| a.full && !a.scoped);
            let fib = self.tunnels[k].fib;
            for (dst, prefix, via) in std::mem::take(&mut self.tunnels[k].routes) {
                let ia = if fib == 0 { via } else { self.tunnels[k].applied.as_ref().map_or(via, |a| a.addrs[0].addr) };
                let _ = self.rts(fib).route(sys::RTM_DELETE, dst, prefix, via, ia);
            }
            self.domain_egress_clear(k);
            if let Some((gw, r, ia)) = self.tunnels[k].pin.take() {
                let _ = self.rt.route(sys::RTM_DELETE, gw, 32, r, ia);
            }
            let iface = self.tunnels[k].iface.clone();
            if let Some(a) = self.tunnels[k].applied.take() {
                for n in &a.addrs {
                    let _ = self.ifc.del_addr(&iface, n.addr);
                }
            }
            if fib != 0 {
                let _ = self.ifc.set_fib(&iface, 0);
                self.tunnels[k].fib = 0;
            }
            for z in std::mem::take(&mut self.tunnels[k].zones) {
                if let Some(u) = &self.unbound {
                    let _ = u.cmd(&format!("forward_remove {z}"));
                }
            }
            if was_full {
                self.lease_routes(true);
                self.forwarders.clear();
            }
            self.refresh_host();
        }

        /// The leases' own routes (DHCP option 121), taken out while a full
        /// tunnel is up and put back after (vpn.md §6, TunnelVision).
        fn lease_routes(&mut self, add: bool) {
            for k in 0..self.ifs.len() {
                let Some(l) = self.ifs[k].applied.lease.clone() else { continue };
                for r in l.routes.iter().filter(|r| r.prefix > 0) {
                    let res = if add { self.rt.route(sys::RTM_ADD, r.dst, r.prefix, r.gw, l.addr) } else { self.rt.route(sys::RTM_DELETE, r.dst, r.prefix, r.gw, l.addr) };
                    match res {
                        Ok(()) => self.log(&format!("{}: lease route {}/{} {}", self.ifs[k].name, r.dst, r.prefix, if add { "back" } else { "out while the full tunnel is up" })),
                        Err(e) if matches!(e.raw_os_error(), Some(libc::EEXIST) | Some(libc::ESRCH)) => {}
                        Err(e) => eprintln!("atrium-net: {}: lease route {}/{}: {e}", self.ifs[k].name, r.dst, r.prefix),
                    }
                }
            }
        }

        /// Each applied tunnel's gateway pinned through the current egress
        /// router (vpn.md §5.3), moved when the egress moves (§5.6).
        fn tunnel_pins(&mut self) {
            let via = self.def4.and_then(|(k, r)| self.ifs[k].applied.lease.as_ref().map(|l| (r, l.addr, l.prefix)));
            for k in 0..self.tunnels.len() {
                let Some(gw) = self.tunnels[k].applied.as_ref().map(|a| a.gateway) else { continue };
                // On the egress's own subnet: its connected route is already
                // more specific than anything of the tunnel's.
                let want = via.filter(|&(_, ia, p)| !in_net(gw, ia, p)).map(|(r, ia, _)| (gw, r, ia));
                if self.tunnels[k].pin == want {
                    continue;
                }
                if let Some((g, r, ia)) = self.tunnels[k].pin.take() {
                    let _ = self.rt.route(sys::RTM_DELETE, g, 32, r, ia);
                }
                if let Some((g, r, ia)) = want {
                    let res = match self.rt.route(sys::RTM_ADD, g, 32, r, ia) {
                        Err(e) if e.raw_os_error() == Some(libc::EEXIST) => self.rt.route(sys::RTM_CHANGE, g, 32, r, ia),
                        x => x,
                    };
                    match res {
                        Ok(()) => {
                            self.log(&format!("tunnel {}: gateway {g} pinned via {r}", self.tunnels[k].id));
                            self.tunnels[k].pin = want;
                        }
                        Err(e) => eprintln!("atrium-net: tunnel {}: gateway route {g}: {e}", self.tunnels[k].id),
                    }
                }
            }
        }

        /// unbound forward zones for tunnel k's domains (vpn.md §5.5).
        fn tunnel_zones(&mut self, k: usize) {
            let Some(a) = self.tunnels[k].applied.clone() else { return };
            if a.dns.is_empty() {
                return;
            }
            let servers = a.dns.iter().map(Ipv4Addr::to_string).collect::<Vec<_>>().join(" ");
            for d in &a.domains {
                if self.tunnels[k].zones.contains(d) {
                    continue;
                }
                let c = format!("forward_add{} {d} {servers}", if a.insecure { " +i" } else { "" });
                match self.unbound.as_ref().map(|u| u.cmd(&c)) {
                    Some(Ok(s)) if s.trim_end() == "ok" => {
                        self.log(&format!("unbound: {c}"));
                        self.tunnels[k].zones.push(d.clone());
                    }
                    Some(Ok(s)) => eprintln!("atrium-net: unbound said {:?} to {c}", s.trim_end()),
                    Some(Err(e)) => eprintln!("atrium-net: unbound: {e}"),
                    None => {
                        self.log(&format!("no unbound: would {c}"));
                        self.tunnels[k].zones.push(d.clone());
                    }
                }
            }
        }

        /// Write the `atrium/vpn` anchor for the applied tunnels if it
        /// changed (vpn.md §5.4). False if pf refused it.
        fn tunnel_anchor(&mut self) -> bool {
            let phys = self.def4.map(|(k, _)| self.ifs[k].name.clone());
            /* A scoped tunnel's apps ask its own DNS servers (§5.5); a split
             * one with none asks the network's, which its table reaches. A
             * full one with none: no redirect, so no name resolves. */
            let lan_dns = self.def4.and_then(|(e, _)| self.ifs[e].applied.lease.as_ref().and_then(|l| l.dns.first().copied()));
            let ts: Vec<(String, Tunnel, u32)> = self.tunnels.iter().filter_map(|t| Some((t.iface.clone(), t.applied.clone()?, t.fib))).collect();
            let at: Vec<AnchorTunnel> = ts.iter().map(|(i, a, fib)| {
                let dns = a.dns.first().copied().or(if a.full { None } else { lan_dns });
                AnchorTunnel {
                    iface: i, gateway: a.gateway, port: a.port, tcp: a.tcp, full: a.full && !a.scoped, block: a.block && !a.scoped,
                    rdr: if a.scoped && *fib != 0 { dns.map(|d| (*fib, d)) } else { None },
                }
            }).collect();
            let text = render_anchor(&at, phys.as_deref());
            if self.anchor.as_ref().is_some_and(|(t, p)| *t == text && *p == phys) {
                return true;
            }
            let blocking = at.iter().any(|t| t.full && t.block);
            let flush = if blocking { phys.clone().unwrap_or_default() } else { String::new() };
            match self.spawner.pf_anchor(&text, &flush) {
                Ok(()) => {
                    self.log(&format!("pf: atrium/vpn {} ({} tunnel(s){})", if text.is_empty() { "flushed" } else { "loaded" }, at.len(), if blocking { ", blocking when not connected" } else { "" }));
                    self.anchor = Some((text, phys));
                    true
                }
                Err(e) => {
                    eprintln!("atrium-net: pf atrium/vpn: {e}");
                    false
                }
            }
        }

        /// A checked lease (DHCP's, or a fixed setting's) bound to
        /// interface `k`.
        fn take_lease(&mut self, k: usize, lease: LeaseV4) {
            let name = self.ifs[k].name.clone();
            match check_v4(&lease, &self.host_view(&name)) {
                Ok(()) => {
                    self.log(&format!("{name}: lease {}/{} router {:?} dns {:?} for {} s", lease.addr, lease.prefix, lease.router, lease.dns, lease.lease_s));
                    let addr = lease.addr;
                    if self.ifs[k].applied.lease.as_ref().map(|o| (o.addr, o.router)) != Some((lease.addr, lease.router)) {
                        self.ifs[k].inet = Inet::Unknown;
                        self.ifs[k].portal = None;
                        self.ifs[k].next_probe = self.now();
                        self.ifs[k].probe_fails = 0;
                    }
                    let ev = Line::new("lease").with("iface", &name).with("addr", format!("{}/{}", lease.addr, lease.prefix));
                    let acts = self.ifs[k].applied.bind(lease);
                    self.apply(&name, Some(addr), acts);
                    let ev = ev.with("metered", self.metered(&self.ifs[k]) as u8);
                    self.event(&ev);
                }
                Err(e) if lease.server.is_unspecified() => {
                    let i = &mut self.ifs[k];
                    i.refusals += 1;
                    i.last_refusal = format!("fixed: {e:?}");
                    eprintln!("atrium-net: {name}: fixed address refused: {e:?}");
                }
                Err(e) => self.refuse(k, &format!("{e:?}")),
            }
        }

        fn lease_line(&mut self, k: usize, l: &Line) {
            match l.verb.as_str() {
                "send" => self.lease_send(k, l),
                "probe" => self.lease_probe(k, l),
                "lease4" => {
                    let Some(lease) = LeaseV4::from_line(l) else {
                        self.refuse(k, "unparsable");
                        return;
                    };
                    if self.manual_for(k).is_some() {
                        // A fixed setting is in use: DHCP's offer is not.
                        self.log(&format!("{}: DHCP lease ignored (fixed address)", self.ifs[k].name));
                        return;
                    }
                    self.take_lease(k, lease);
                }
                "lost" if self.manual_for(k).is_some() => {}
                "lost" => self.lose(k, l.get("why").unwrap_or("?")),
                "send6" => self.lease_send6(k, l),
                "lease6" => self.lease6(k, l),
                "lost6" => {
                    if let Some(a) = self.ifs[k].addr6.take() {
                        let name = self.ifs[k].name.clone();
                        match self.ifc.del_addr6(&name, a) {
                            Ok(()) => self.log(&format!("{name}: DHCPv6 address {a} removed ({})", l.get("why").unwrap_or("?"))),
                            Err(e) => eprintln!("atrium-net: {name}: removing {a}: {e}"),
                        }
                    }
                }
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
                _ => self.lease_died(k, "sent an unexpected message"),
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

        /// An ARP probe atrium-lease asks for before using a new address:
        /// only an ordinary unicast address, under the DHCP token bucket.
        fn lease_probe(&mut self, k: usize, l: &Line) {
            let i = &mut self.ifs[k];
            let now = Instant::now();
            i.tokens = (i.tokens + now.duration_since(i.last_fill).as_secs_f64() / 2.0).min(10.0);
            i.last_fill = now;
            let a = l.get("addr").and_then(|x| x.parse::<Ipv4Addr>().ok()).filter(|a| net_proto::ordinary_unicast(*a));
            let (Some(a), true) = (a, i.tokens >= 1.0) else {
                i.sends_refused += 1;
                return;
            };
            i.tokens -= 1.0;
            if let Err(e) = i.bpf.write(&wrap::arp_probe(&i.mac, a)) {
                eprintln!("atrium-net: {}: bpf write: {e}", i.name);
            }
        }

        /// A DHCPv6 message from atrium-lease: checked, then sent from the
        /// interface's link-local address to ff02::1:2, under the same
        /// token bucket as DHCPv4.
        fn lease_send6(&mut self, k: usize, l: &Line) {
            let ll = self.v6_of(&self.ifs[k].name).into_iter().find(|a| a.segments()[0] & 0xffc0 == 0xfe80);
            let i = &mut self.ifs[k];
            let now = Instant::now();
            i.tokens = (i.tokens + now.duration_since(i.last_fill).as_secs_f64() / 2.0).min(10.0);
            i.last_fill = now;
            let Some(p) = l.hex("hex") else {
                i.sends_refused += 1;
                return;
            };
            if i.tokens < 1.0 {
                i.sends_refused += 1;
                return;
            }
            if let Err(e) = wrap::check_send6(&p, &i.mac) {
                i.sends_refused += 1;
                let m = format!("{}: DHCPv6 send refused: {e:?}", i.name);
                self.log(&m);
                return;
            }
            let Some(ll) = ll else {
                // No link-local address yet (IPv6 still starting): the
                // client retransmits.
                return;
            };
            i.tokens -= 1.0;
            if let Err(e) = i.bpf.write(&wrap::frame6(&i.mac, ll, &p)) {
                eprintln!("atrium-net: {}: bpf write: {e}", i.name);
            }
        }

        fn lease6(&mut self, k: usize, l: &Line) {
            let name = self.ifs[k].name.clone();
            let a = l.get("addr").and_then(|x| x.parse::<Ipv6Addr>().ok());
            let pref = l.get("pref").and_then(|x| x.parse::<u32>().ok());
            let valid = l.get("valid").and_then(|x| x.parse::<u32>().ok());
            let (Some(a), Some(pref), Some(valid)) = (a, pref, valid) else {
                self.refuse6(k, "lease6: unparsable".into());
                return;
            };
            let others: Vec<Ipv6Addr> = self.host6.iter().filter(|(n, _)| *n != name).map(|&(_, a)| a).collect();
            if let Err(e) = check_lease6(a, pref, valid, &others) {
                self.refuse6(k, format!("lease6: {e:?}"));
                return;
            }
            if let Some(old) = self.ifs[k].addr6.filter(|o| *o != a) {
                let _ = self.ifc.del_addr6(&name, old);
                self.log(&format!("{name}: DHCPv6 address {old} replaced"));
            }
            match self.ifc.add_addr6(&name, a, pref, valid) {
                Ok(()) => {
                    if self.ifs[k].addr6 != Some(a) {
                        self.log(&format!("{name}: DHCPv6 address {a} (valid {valid} s)"));
                    }
                    self.ifs[k].addr6 = Some(a);
                }
                Err(e) => {
                    self.ifs[k].addr6 = None;
                    eprintln!("atrium-net: {name}: adding {a}: {e}");
                }
            }
        }

        /// A refused DHCPv6 address: counted and logged; the DHCPv4 client
        /// is not told (its back-off is for its own leases).
        fn refuse6(&mut self, k: usize, why: String) {
            let i = &mut self.ifs[k];
            i.refusals += 1;
            eprintln!("atrium-net: {}: refused: {why}", i.name);
            i.last_refusal = why;
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

    pub fn main() {
        let o = opts();
        let follow = o.follow;
        // First, while nothing else is open: the spawner that starts the
        // sandboxed helpers now and again later (spawn.rs).
        let vflag: &[&str] = if o.verbose { &["-v"] } else { &[] };
        // Not installed: no .local names, and no retrying every few seconds.
        let mut o = o;
        if o.unbound.is_none() {
            o.mdns_bin = None;
        } else if o.mdns_bin.as_ref().is_some_and(|m| !std::path::Path::new(m).exists()) {
            eprintln!("atrium-net: {} not there: no .local names", o.mdns_bin.take().unwrap());
        }
        let mut mdns_args: Vec<&str> = vec!["--uid", "65049"];
        mdns_args.extend_from_slice(vflag);
        let mut progs: Vec<(u8, &str, &[&str])> = vec![(b'L', o.lease.as_str(), vflag), (b'P', o.probe_bin.as_str(), vflag)];
        if let Some(m) = &o.mdns_bin {
            progs.push((b'D', m.as_str(), &mdns_args));
        }
        let spawner = Spawner::start(&progs).unwrap_or_else(|e| die(&format!("spawner: {e}")));
        // No unbound, no stub zone to answer.
        let mdns_listener = if o.mdns_bin.is_some() && o.unbound.is_some() {
            spawner.stub_listener(5354).map_err(|e| eprintln!("atrium-net: .local listener: {e}")).ok()
        } else {
            None
        };
        let all = sys::interfaces().unwrap_or_else(|e| die(&e.to_string()));
        let _ = std::fs::create_dir_all(&o.state);
        let state = Dir::open(&o.state).unwrap_or_else(|e| die(&format!("{}: {e}", o.state)));
        let (wired_manual, (own_dns, dns_tls)) = parse_settings(&state.read(SETTINGS_FILE, 1 << 16).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default());
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
            host6: Vec::new(),
            names: all.iter().map(|i| (i.index, i.name.clone())).collect(),
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
            wired_manual,
            own_dns,
            dns_tls,
            unbound_up: 0,
            portal_watch_until: None,
            dot: None,
            egress: Egress::new(o.ifaces.len()),
            def4: None,
            announced: None,
            prober: None,
            probe_again: None,
            unbound_check_at: 0,
            spawner,
            reply_fds: Vec::new(),
            mdns_listener,
            mdns: None,
            mdns_again_at: 0,
            netd: o.netd.as_ref().map(|d| Dir::open(d).unwrap_or_else(|e| die(&format!("{d}: {e}")))),
            egress_group: o.egress_group.clone(),
            ports_seen: all.iter().map(|i| i.name.clone()).collect(),
            group_synced: false,
            tunnels: Vec::new(),
            rtf: Vec::new(),
            domains: Vec::new(),
            anchor: None,
            zones_lost: false,
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
            net.clear_unprivate_slaac(name, &info.v6);
            if let Err(e) = net.ifc.enable_inet6(name) {
                eprintln!("atrium-net: {name}: IPv6: {e}");
            }
            let bpf = Bpf::open(name).unwrap_or_else(|e| die(&format!("{name}: {e}")));
            let mut lease = Conn::new(net.spawner.spawn(b'L').unwrap_or_else(|e| die(&format!("atrium-lease: {e}")))).unwrap_or_else(|_| die("lease conn"));
            lease.send(&Line::new("hello").with("mac", fmt_mac(&mac))).unwrap_or_else(|_| die("hello"));
            // Wired links start now (DHCP, or the port's fixed setting just
            // below); Wi-Fi when aura-mlme reports the port open.
            if !is_wifi && !net.wired_manual.iter().any(|(n, _)| n == name) {
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
                lease_restarts: 0,
                lease_deaths: Vec::new(),
                lease_hold: None,
                lease_gen: 0,
                applied: Applied::default(),
                ip_id: 1,
                tokens: 10.0,
                last_fill: Instant::now(),
                refusals: 0,
                last_refusal: String::new(),
                sends_refused: 0,
                dns6: Vec::new(),
                addr6: None,
                rs_left: if is_wifi { 0 } else { 3 },
                rs_at: 0,
                link: !is_wifi,
                inet: Inet::Unknown,
                portal: None,
                next_probe: 0,
                probe_fails: 0,
            });
        }
        for k in 0..net.ifs.len() {
            if !net.ifs[k].wifi && net.manual_for(k).is_some() {
                net.link_up_lease(k);
            }
        }
        net.forward();
        // The probe must exist before capability mode (no exec after it).
        if let Some(url) = &o.probe {
            let cfg = probe_cfg(url, o.probe_expect.as_deref(), o.probe_every);
            let c = Conn::new(net.spawner.spawn(b'P').unwrap_or_else(|e| die(&format!("atrium-probe: {e}")))).unwrap_or_else(|_| die("probe conn"));
            net.log(&format!("probing http://{}{}", cfg.host, cfg.path));
            net.prober = Some(Prober { c, cfg, busy: None, next_id: 0 });
        }
        if let Some(d) = o.sock.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        // Started through atrium-bind (its rc script), the socket is already
        // bound and waits at fd 3, so netconf could connect from the start
        // (fast-boot.md §6.4); run by hand, bind it here.
        let listener: UnixListener = match atrium_listen::listener(&o.sock) {
            Ok((l, atrium_listen::Origin::Inherited)) => l,
            Ok((l, atrium_listen::Origin::Bound)) => {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&o.sock, std::fs::Permissions::from_mode(0o600))
                    .unwrap_or_else(|e| die(&format!("{}: {e}", o.sock.display())));
                l
            }
            Err(e) => die(&format!("{}: {e}", o.sock.display())),
        };
        listener.set_nonblocking(true).ok();
        // The DNS-over-TLS check's socket to unbound, while connect works.
        if net.unbound.is_some() {
            match std::net::UdpSocket::bind("127.0.0.1:0").and_then(|s| s.connect("127.0.0.1:53").map(|_| s)).and_then(|s| s.set_nonblocking(true).map(|_| s)) {
                Ok(sock) => net.dot = Some(DotCheck { sock, id: (std::process::id() as u16) ^ 0x5a5a, asked: None, since_new: 0, failing_since: None, failing: false, answered: false, next: 0 }),
                Err(e) => eprintln!("atrium-net: no DNS over TLS check: {e}"),
            }
        }
        // Route domains (vpn.md §5.3): the tables, and a socket for each,
        // before capability mode. Without them persona-scoped VPNs are
        // refused (`no-fib`); nothing else changes.
        match sys::raise_fibs(MAX_DOMAINS + 1) {
            Ok(()) => {
                for f in 1..=MAX_DOMAINS {
                    match Rtsock::new_in(f) {
                        Ok(r) => net.rtf.push(r),
                        Err(e) => {
                            eprintln!("atrium-net: fib {f}: {e}");
                            break;
                        }
                    }
                }
            }
            Err(e) => eprintln!("atrium-net: no route domains: {e}"),
        }
        if o.capsicum {
            sys::cap_enter().unwrap_or_else(|e| die(&e.to_string()));
            net.log("in capability mode");
        }
        net.mlme_connect();

        loop {
            let mut fds = vec![net.rt.s.as_raw_fd(), listener.as_raw_fd(), net.mlme.as_ref().map_or(-1, |c| c.fd()), net.prober.as_ref().map_or(-1, |p| p.c.fd())];
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
                for ev in net.rt.read_ifinfo() {
                    let (idx, up) = match ev {
                        sys::IfEvent::Link(idx, up) => (idx, up),
                        sys::IfEvent::Arrived(n) if follow && atrium_net::plan::is_port(&n) && !net.ifs.iter().any(|i| i.name == n) => {
                            eprintln!("atrium-net: {n} arrived: exiting to start again with it");
                            std::process::exit(75);
                        }
                        sys::IfEvent::Departed(n) if follow && net.ifs.iter().any(|i| i.name == n) => {
                            eprintln!("atrium-net: {n} went away: exiting to start again without it");
                            std::process::exit(75);
                        }
                        sys::IfEvent::Addr { index, add, addr, prefix } => {
                            net.addr_event(index, add, addr, prefix);
                            continue;
                        }
                        sys::IfEvent::Arrived(n) | sys::IfEvent::Departed(n) => {
                            net.log(&format!("{n}: arrived or went away (not followed)"));
                            continue;
                        }
                    };
                    let now = net.now();
                    if let Some(k) = net.ifs.iter().position(|i| i.index == idx && !i.wifi) {
                        if net.ifs[k].link && !up {
                            net.egress.flap(k, now);
                        }
                        let i = &mut net.ifs[k];
                        i.link = up;
                        let m = format!("{}: link {}", i.name, if up { "up" } else { "down" });
                        if up {
                            i.rs_left = 3;
                            i.rs_at = 0;
                        }
                        net.log(&m);
                        if up {
                            net.link_up_lease(k);
                        } else {
                            let _ = net.ifs[k].lease.send(&Line::new("down"));
                        }
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
            if ready[3] {
                match net.prober.as_mut().map(|p| p.c.lines()) {
                    Some(Ok(v)) => {
                        for g in v {
                            if let Got::Line(l) = g {
                                net.probe_line(&l);
                            }
                        }
                    }
                    Some(Err(_)) => net.probe_gone(),
                    None => {}
                }
            }
            for k in 0..net.ifs.len() {
                if ready[4 + 2 * k] {
                    for f in net.ifs[k].bpf.read() {
                        if f.len() <= 1514 && net.ifs[k].lease_hold.is_none() {
                            let _ = net.ifs[k].lease.send(&Line::new("frame").with_hex("hex", &f));
                        }
                    }
                }
                if ready[5 + 2 * k] {
                    let lines = match net.ifs[k].lease.lines() {
                        Ok(l) => l,
                        Err(_) => {
                            net.lease_died(k, "exited");
                            continue;
                        }
                    };
                    let gen = net.ifs[k].lease_gen;
                    for g in lines {
                        let Got::Line(l) = g else {
                            net.lease_died(k, "sent a bad line");
                            break;
                        };
                        net.lease_line(k, &l);
                        // A new process since: the rest came from the old one.
                        if net.ifs[k].lease_gen != gen {
                            break;
                        }
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
                    let fds = std::mem::take(&mut net.reply_fds);
                    for r in reply {
                        if j >= net.ctls.len() {
                            break;
                        }
                        let sent = if fds.is_empty() || r.verb == "ok" { net.ctls[j].c.send(&r) } else { send_line_fds(&net.ctls[j].c.s, &r, &fds) };
                        if sent.is_err() {
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
            net.route_default();
            if net.portal_watch_until.is_some_and(|t| net.now() >= t) {
                net.portal_watch_until = None;
                net.log("portal watch: 10 minutes, off");
            }
            net.probe_tick();
            net.dot_tick();
            net.probe_restart();
            net.lease_restart_tick();
            net.unbound_tick();
            net.mdns_tick();
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
