//! M1 — the Navigator's fetcher (navigator-backend §2, §8).
//!
//! The only component that holds network, and one that cannot name a file:
//! on FreeBSD, `sandbox::enter` puts the process in Capsicum capability mode
//! with a casper `cap_net` channel as its only way out (see `sandbox`). What
//! it can do then is exactly `fetch`: resolve, connect, TLS, one bounded GET,
//! a bounded number of redirects.

pub mod addr;
pub mod http;
#[cfg(target_os = "freebsd")]
pub mod sandbox;

use std::io::{Read, Write};
use std::collections::{BTreeSet, HashMap};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

pub const MAX_REDIRECTS: usize = 5;
pub const TIMEOUT: Duration = Duration::from_secs(20);

/// How a fetch reaches the network. The sandboxed daemon uses casper; tests
/// and the unsandboxed host use `Direct`.
///
/// ★ TWO STEPS, SO THE CHECK CAN SIT BETWEEN THEM. Resolution and connection
/// used to be one call (`connect(host, port)`), which left nowhere to refuse
/// a private answer: whatever the resolver said, the socket went there. Now a
/// caller resolves, `Conversation` checks every answer against the `Policy`
/// and pins it, and only then connects — to an address it has already judged.
pub trait Connect {
    fn resolve(&self, host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>>;
    fn connect_addr(&self, addr: SocketAddr) -> std::io::Result<TcpStream>;
}

pub struct Direct;
impl Connect for Direct {
    fn resolve(&self, host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
        use std::net::ToSocketAddrs;
        Ok((host, port).to_socket_addrs()?.collect())
    }
    fn connect_addr(&self, addr: SocketAddr) -> std::io::Result<TcpStream> { TcpStream::connect_timeout(&addr, TIMEOUT) }
}

#[cfg(target_os = "freebsd")]
impl Connect for sandbox::Net {
    fn resolve(&self, host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> { sandbox::Net::resolve(self, host, port) }
    fn connect_addr(&self, addr: SocketAddr) -> std::io::Result<TcpStream> { sandbox::Net::connect_addr(self, addr) }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Fetched {
    /// The URL the body came from, after redirects.
    pub url: String,
    pub response: http::Response,
    pub redirects: usize,
}

#[derive(Debug, PartialEq)]
pub enum FetchError {
    BadUrl(String),
    /// Only http and https, only ports 80 and 443 (what casper will resolve),
    /// no credentials in the URL.
    Refused(String),
    Connect(String),
    Tls(String),
    Http(http::HttpError),
    TooManyRedirects,
    /// The host resolved (or is written as) an address the policy does not
    /// admit: the LAN, loopback, metadata, … (threat model §2.1).
    NonPublicAddress { host: String, addr: IpAddr, range: addr::Range },
    /// A second resolution of a host already pinned in this conversation
    /// answered with none of the addresses it answered the first time — DNS
    /// rebinding, or a host that changed address mid-conversion. Either way
    /// the first answer is the one this conversion keeps.
    Rebinding { host: String, pinned: Vec<IpAddr>, answered: Vec<IpAddr> },
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::NonPublicAddress { host, addr, range } =>
                write!(f, "Refused(\"{host} is {addr}, a {range} address; the private network is not granted\")"),
            FetchError::Rebinding { host, pinned, answered } =>
                write!(f, "Refused(\"{host} answered {answered:?} after pinning {pinned:?} (rebinding)\")"),
            other => write!(f, "{other:?}"),
        }
    }
}

/// Load the trust store. Must happen BEFORE capability mode: afterwards the
/// file cannot be named.
pub fn tls_config(pem: &[u8]) -> Result<Arc<rustls::ClientConfig>, String> {
    use rustls_pki_types::{pem::PemObject, CertificateDer};
    let mut roots = rustls::RootCertStore::empty();
    let mut n = 0;
    for c in CertificateDer::pem_slice_iter(pem) {
        let c = c.map_err(|e| format!("trust store: {e:?}"))?;
        if roots.add(c).is_ok() { n += 1 }
    }
    if n == 0 { return Err("trust store holds no usable certificates".into()) }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions().map_err(|e| e.to_string())?
        .with_root_certificates(roots).with_no_client_auth();
    Ok(Arc::new(cfg))
}

/// One-shot: a fresh conversation for a single URL. Pins nothing beyond it.
pub fn fetch(net: &dyn Connect, tls: &Arc<rustls::ClientConfig>, url: &str) -> Result<Fetched, FetchError> {
    Conversation::new(net, tls.clone(), addr::Policy::default()).fetch(url)
}

/// Every fetch of one conversion (threat model §2.1).
///
/// ★ THE PIN IS THE REBINDING DEFENCE. The first resolution of a host decides
/// the addresses that host has for the rest of this conversion. A second
/// lookup of the same host and port is not made at all; a lookup on the other
/// port (80 after 443) must answer with at least one already-pinned address,
/// and only those are used. So `evil.example` answering 93.184.x.x first and
/// 127.0.0.1 next gets the second answer refused twice over: by the range
/// check, and — were the private network granted — by the pin.
pub struct Conversation<'a> {
    net: &'a dyn Connect,
    tls: Arc<rustls::ClientConfig>,
    policy: addr::Policy,
    /// (host, port) → the checked addresses to connect to.
    pins: HashMap<(String, u16), Vec<SocketAddr>>,
    /// host → every IP pinned for it on any port.
    hosts: HashMap<String, BTreeSet<IpAddr>>,
    /// Resolver calls made, for the tests that prove a pin is not re-asked.
    pub resolutions: usize,
}

impl<'a> Conversation<'a> {
    pub fn new(net: &'a dyn Connect, tls: Arc<rustls::ClientConfig>, policy: addr::Policy) -> Self {
        Conversation { net, tls, policy, pins: HashMap::new(), hosts: HashMap::new(), resolutions: 0 }
    }

    pub fn policy(&self) -> addr::Policy { self.policy }

    pub fn fetch(&mut self, url: &str) -> Result<Fetched, FetchError> {
        let mut current = url::Url::parse(url).map_err(|e| FetchError::BadUrl(e.to_string()))?;
        for redirects in 0..=MAX_REDIRECTS {
            // ★ Every hop goes through `get_once`, so a redirect target gets
            // exactly the scheme, port, address and pin checks the first URL
            // got — a public page redirecting to http://192.168.1.1/ is
            // refused at the hop, not followed.
            let response = self.get_once(&current)?;
            if matches!(response.status, 301 | 302 | 303 | 307 | 308) {
                let loc = response.header("location").ok_or(FetchError::Http(http::HttpError::Malformed("redirect without Location")))?;
                let next = current.join(loc).map_err(|e| FetchError::BadUrl(e.to_string()))?;
                // ★ Never downgrade: a redirect from https to http would hand a
                // network attacker the rest of the chain.
                if current.scheme() == "https" && next.scheme() != "https" {
                    return Err(FetchError::Refused(format!("redirect downgrades to {next}")));
                }
                current = next;
                continue;
            }
            return Ok(Fetched { url: current.to_string(), response, redirects });
        }
        Err(FetchError::TooManyRedirects)
    }

    /// The addresses `host:port` may be connected to, checked and pinned.
    pub fn resolve_checked(&mut self, host: &str, port: u16) -> Result<Vec<SocketAddr>, FetchError> {
        let key = (host.to_ascii_lowercase(), port);
        if let Some(p) = self.pins.get(&key) { return Ok(p.clone()) }
        self.resolutions += 1;
        let answers = self.net.resolve(host, port).map_err(|e| FetchError::Connect(e.to_string()))?;
        if answers.is_empty() { return Err(FetchError::Connect(format!("no address for {host}"))) }
        // ★ ANY non-public answer refuses the host, rather than being filtered
        // out while the rest are used. A mixed answer is what a rebinding
        // attacker sends to race the check, and no honest public site needs
        // its users to connect to 10.x.
        for a in &answers {
            if let Err(range) = self.policy.admit(a.ip()) {
                return Err(FetchError::NonPublicAddress { host: host.to_string(), addr: a.ip(), range });
            }
        }
        let chosen: Vec<SocketAddr> = match self.hosts.get(&key.0) {
            None => answers,
            Some(pinned) => {
                let keep: Vec<SocketAddr> = answers.iter().copied().filter(|a| pinned.contains(&a.ip())).collect();
                if keep.is_empty() {
                    return Err(FetchError::Rebinding { host: host.to_string(),
                        pinned: pinned.iter().copied().collect(), answered: answers.iter().map(|a| a.ip()).collect() });
                }
                keep
            }
        };
        self.hosts.entry(key.0.clone()).or_default().extend(chosen.iter().map(|a| a.ip()));
        self.pins.insert(key, chosen.clone());
        Ok(chosen)
    }

    fn get_once(&mut self, u: &url::Url) -> Result<http::Response, FetchError> {
        let https = match u.scheme() { "https" => true, "http" => false, s => return Err(FetchError::Refused(format!("scheme {s}"))) };
        if !u.username().is_empty() || u.password().is_some() {
            return Err(FetchError::Refused("credentials in URL".into()));
        }
        let host = u.host_str().ok_or_else(|| FetchError::BadUrl("no host".into()))?.to_string();
        let port = u.port_or_known_default().unwrap_or(0);
        if port != 80 && port != 443 { return Err(FetchError::Refused(format!("port {port}"))) }
        // ★ A literal address is refused here, before any resolver is asked:
        // the URL parser has already normalised the obfuscated spellings
        // (http://2130706433/, http://0x7f.1/, http://[::ffff:7f00:1]/) to
        // the address they mean, so this sees the real one.
        let literal = match u.host() {
            Some(url::Host::Ipv4(a)) => Some(IpAddr::V4(a)),
            Some(url::Host::Ipv6(a)) => Some(IpAddr::V6(a)),
            _ => None,
        };
        if let Some(ip) = literal {
            self.policy.admit(ip).map_err(|range| FetchError::NonPublicAddress { host: host.clone(), addr: ip, range })?;
        }
        let host_header = match u.port() { Some(p) => format!("{host}:{p}"), None => host.clone() };
        let mut path = u.path().to_string();
        if let Some(q) = u.query() { path.push('?'); path.push_str(q) }

        // The resolver sees the bare address for a bracketed IPv6 literal.
        let lookup = host.trim_start_matches('[').trim_end_matches(']').to_string();
        let addrs = self.resolve_checked(&lookup, port)?;
        let mut last = None;
        let mut tcp = None;
        for a in &addrs {
            match self.net.connect_addr(*a) {
                Ok(s) => { tcp = Some(s); break }
                Err(e) => last = Some(format!("connect {a}: {e}")),
            }
        }
        let tcp = tcp.ok_or_else(|| FetchError::Connect(last.unwrap_or_else(|| format!("no address for {host}"))))?;
        tcp.set_read_timeout(Some(TIMEOUT)).ok();
        tcp.set_write_timeout(Some(TIMEOUT)).ok();
        if https {
            let name = rustls_pki_types::ServerName::try_from(lookup.clone()).map_err(|e| FetchError::Tls(e.to_string()))?;
            let conn = rustls::ClientConnection::new(self.tls.clone(), name).map_err(|e| FetchError::Tls(e.to_string()))?;
            let mut s = rustls::StreamOwned::new(conn, tcp);
            http::write_request(&mut s, &host_header, &path).map_err(|e| FetchError::Tls(e.to_string()))?;
            read(&mut s)
        } else {
            let mut s = tcp;
            http::write_request(&mut s, &host_header, &path).map_err(|e| FetchError::Connect(e.to_string()))?;
            read(&mut s)
        }
    }
}

fn read(s: &mut (impl Read + Write)) -> Result<http::Response, FetchError> {
    // A peer closing without TLS close_notify surfaces as UnexpectedEof from
    // rustls; for a Connection: close response whose framing is already
    // complete that is not an error, so the reader decides by framing.
    http::read_response(s).map_err(FetchError::Http)
}

#[cfg(test)]
mod policy_tests {
    //! Threat model §2.1: the address check, the pin and the redirect hop.
    //!
    //! The resolver is scripted, so a test says exactly what DNS answered;
    //! `connect_addr` records which address it was asked for and connects to
    //! a loopback server that plays the remote host. That split is the point:
    //! the policy judges the ANSWERED address, and the test can watch it do so
    //! without the machine's real DNS or a real LAN.
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::net::TcpListener;

    struct Scripted {
        /// One answer per resolver call, in order.
        answers: RefCell<VecDeque<Vec<IpAddr>>>,
        resolved: RefCell<Vec<(String, u16)>>,
        connected: RefCell<Vec<SocketAddr>>,
        /// Where every connect really goes: a server speaking `responses`.
        server: SocketAddr,
    }

    fn server(responses: Vec<&'static str>) -> SocketAddr {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let a = l.local_addr().unwrap();
        std::thread::spawn(move || {
            for r in responses {
                let Ok((mut s, _)) = l.accept() else { return };
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf);
                let _ = s.write_all(r.as_bytes());
            }
        });
        a
    }

    impl Scripted {
        fn new(answers: Vec<Vec<&str>>, responses: Vec<&'static str>) -> Self {
            Scripted {
                answers: RefCell::new(answers.into_iter().map(|v| v.into_iter().map(|s| s.parse().unwrap()).collect()).collect()),
                resolved: RefCell::default(), connected: RefCell::default(), server: server(responses),
            }
        }
    }

    impl Connect for Scripted {
        fn resolve(&self, host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
            self.resolved.borrow_mut().push((host.to_string(), port));
            let ips = self.answers.borrow_mut().pop_front().ok_or_else(|| std::io::Error::other("unscripted lookup"))?;
            Ok(ips.into_iter().map(|ip| SocketAddr::new(ip, port)).collect())
        }
        fn connect_addr(&self, addr: SocketAddr) -> std::io::Result<TcpStream> {
            self.connected.borrow_mut().push(addr);
            TcpStream::connect(self.server)
        }
    }

    fn tls() -> Arc<rustls::ClientConfig> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        Arc::new(rustls::ClientConfig::builder_with_provider(provider).with_safe_default_protocol_versions().unwrap()
            .with_root_certificates(rustls::RootCertStore::empty()).with_no_client_auth())
    }

    const OK: &str = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
    const PUBLIC: &str = "93.184.215.14";

    #[test]
    fn a_public_name_is_fetched_from_the_address_it_resolved_to() {
        let net = Scripted::new(vec![vec![PUBLIC]], vec![OK]);
        let mut c = Conversation::new(&net, tls(), addr::Policy::default());
        let f = c.fetch("http://example.com/").unwrap();
        assert_eq!(f.response.body, b"ok");
        assert_eq!(*net.connected.borrow(), vec![SocketAddr::new(PUBLIC.parse().unwrap(), 80)]);
    }

    #[test]
    fn every_non_public_literal_is_refused_before_any_lookup_or_connect() {
        for url in [
            "http://127.0.0.1/", "http://127.1.2.3/", "http://0.0.0.0/", "http://10.0.0.5/", "http://172.16.0.1/",
            "http://172.31.255.255/", "http://192.168.1.1/", "http://169.254.169.254/latest/meta-data/",
            "http://169.254.1.1/", "http://100.64.0.1/", "http://224.0.0.1/", "http://255.255.255.255/",
            "http://192.0.2.1/", "http://198.51.100.1/", "http://203.0.113.1/", "http://198.18.0.1/", "http://240.0.0.1/",
            // the URL parser's normalisation of obfuscated IPv4 spellings
            "http://2130706433/", "http://0x7f.0.0.1/", "http://0177.0.0.1/", "http://127.1/", "http://3232235777/",
            "http://[::1]/", "http://[::]/", "http://[fe80::1]/", "http://[fd00::1]/", "http://[fc00::1]/",
            "http://[fd00:ec2::254]/", "http://[ff02::1]/", "http://[2001:db8::1]/",
            "http://[::ffff:127.0.0.1]/", "http://[::ffff:7f00:1]/", "http://[::ffff:10.0.0.1]/",
            "http://[::ffff:169.254.169.254]/", "http://[64:ff9b::7f00:1]/", "http://[2002:c0a8:101::1]/",
            "https://127.0.0.1/", "https://[::1]/",
        ] {
            let net = Scripted::new(vec![], vec![]);
            let mut c = Conversation::new(&net, tls(), addr::Policy::default());
            match c.fetch(url) {
                Err(FetchError::NonPublicAddress { .. }) => {}
                other => panic!("{url}: {other:?}"),
            }
            assert!(net.resolved.borrow().is_empty(), "{url} reached the resolver");
            assert!(net.connected.borrow().is_empty(), "{url} reached connect");
        }
    }

    #[test]
    fn a_name_that_resolves_inward_is_refused_for_every_range_and_family() {
        for ip in ["127.0.0.1", "10.1.2.3", "172.20.0.1", "192.168.0.10", "169.254.169.254", "169.254.7.7",
                   "100.100.100.100", "0.0.0.0", "239.1.1.1", "255.255.255.255",
                   "::1", "fe80::1", "fd12:3456::1", "ff05::1", "::ffff:192.168.1.1", "::ffff:127.0.0.1",
                   "64:ff9b::a00:1", "2002:a00:1::"] {
            let net = Scripted::new(vec![vec![ip]], vec![]);
            let mut c = Conversation::new(&net, tls(), addr::Policy::default());
            match c.fetch("http://intranet.example/") {
                Err(FetchError::NonPublicAddress { addr, .. }) => assert_eq!(addr, ip.parse::<IpAddr>().unwrap()),
                other => panic!("{ip}: {other:?}"),
            }
            assert!(net.connected.borrow().is_empty(), "{ip} reached connect");
        }
    }

    #[test]
    fn a_mixed_answer_is_refused_whole() {
        // A rebinding attacker's race: one public and one private answer.
        let net = Scripted::new(vec![vec![PUBLIC, "127.0.0.1"]], vec![]);
        let mut c = Conversation::new(&net, tls(), addr::Policy::default());
        assert!(matches!(c.fetch("http://evil.example/"), Err(FetchError::NonPublicAddress { .. })));
        assert!(net.connected.borrow().is_empty());
    }

    #[test]
    fn a_redirect_to_a_private_literal_is_refused_at_the_hop() {
        let net = Scripted::new(vec![vec![PUBLIC]],
            vec!["HTTP/1.1 302 Found\r\nLocation: http://192.168.1.1/admin\r\nContent-Length: 0\r\n\r\n"]);
        let mut c = Conversation::new(&net, tls(), addr::Policy::default());
        match c.fetch("http://example.com/") {
            Err(FetchError::NonPublicAddress { addr, .. }) => assert_eq!(addr, "192.168.1.1".parse::<IpAddr>().unwrap()),
            other => panic!("{other:?}"),
        }
        assert_eq!(net.connected.borrow().len(), 1, "only the public hop connected");
    }

    #[test]
    fn a_redirect_to_a_name_that_resolves_privately_is_refused() {
        let net = Scripted::new(vec![vec![PUBLIC], vec!["10.0.0.5"]],
            vec!["HTTP/1.1 301 Moved\r\nLocation: http://router.example/\r\nContent-Length: 0\r\n\r\n"]);
        let mut c = Conversation::new(&net, tls(), addr::Policy::default());
        assert!(matches!(c.fetch("http://example.com/"), Err(FetchError::NonPublicAddress { .. })));
        assert_eq!(net.connected.borrow().len(), 1);
    }

    #[test]
    fn rebinding_on_the_same_port_never_reaches_the_resolver_again() {
        // The second lookup WOULD answer loopback; it is never made.
        let net = Scripted::new(vec![vec![PUBLIC], vec!["127.0.0.1"]], vec![OK, OK]);
        let mut c = Conversation::new(&net, tls(), addr::Policy::default());
        c.fetch("http://evil.example/a").unwrap();
        c.fetch("http://evil.example/b").unwrap();
        assert_eq!(c.resolutions, 1);
        let want = SocketAddr::new(PUBLIC.parse().unwrap(), 80);
        assert_eq!(*net.connected.borrow(), vec![want, want]);
    }

    #[test]
    fn rebinding_on_the_other_port_is_refused_by_range_and_by_pin() {
        // Default policy: the inward answer is refused by its range.
        let net = Scripted::new(vec![vec![PUBLIC], vec!["127.0.0.1"]], vec![]);
        let mut c = Conversation::new(&net, tls(), addr::Policy::default());
        c.resolve_checked("evil.example", 80).unwrap();
        assert!(matches!(c.resolve_checked("evil.example", 443), Err(FetchError::NonPublicAddress { .. })));
        // ★ Under the grant the range check admits loopback — and the PIN
        // still refuses it. That is the case the pin exists for.
        let net = Scripted::new(vec![vec![PUBLIC], vec!["127.0.0.1"]], vec![]);
        let mut c = Conversation::new(&net, tls(), addr::Policy { grant_private_network: true });
        c.resolve_checked("evil.example", 80).unwrap();
        assert!(matches!(c.resolve_checked("evil.example", 443), Err(FetchError::Rebinding { .. })));
        // A different PUBLIC answer is refused too: the first answer is kept.
        let net = Scripted::new(vec![vec![PUBLIC], vec!["8.8.8.8"]], vec![]);
        let mut c = Conversation::new(&net, tls(), addr::Policy::default());
        c.resolve_checked("Evil.Example", 80).unwrap();
        assert!(matches!(c.resolve_checked("evil.example", 443), Err(FetchError::Rebinding { .. })));
        // An overlapping answer keeps only the pinned addresses.
        let net = Scripted::new(vec![vec![PUBLIC], vec!["8.8.8.8", PUBLIC]], vec![]);
        let mut c = Conversation::new(&net, tls(), addr::Policy::default());
        c.resolve_checked("evil.example", 80).unwrap();
        assert_eq!(c.resolve_checked("evil.example", 443).unwrap(), vec![SocketAddr::new(PUBLIC.parse().unwrap(), 443)]);
    }

    #[test]
    fn the_grant_admits_the_lan_but_never_metadata() {
        let net = Scripted::new(vec![vec!["192.168.1.1"]], vec![OK]);
        let mut c = Conversation::new(&net, tls(), addr::Policy { grant_private_network: true });
        assert_eq!(c.fetch("http://router.lan/").unwrap().response.body, b"ok");
        let net = Scripted::new(vec![], vec![]);
        let mut c = Conversation::new(&net, tls(), addr::Policy { grant_private_network: true });
        assert!(matches!(c.fetch("http://169.254.169.254/"), Err(FetchError::NonPublicAddress { range: addr::Range::Metadata, .. })));
        let net = Scripted::new(vec![vec!["169.254.169.254"]], vec![]);
        let mut c = Conversation::new(&net, tls(), addr::Policy { grant_private_network: true });
        assert!(matches!(c.fetch("http://metadata.google.internal/"), Err(FetchError::NonPublicAddress { range: addr::Range::Metadata, .. })));
    }

    #[test]
    fn the_refusal_names_the_host_the_address_and_the_range() {
        let net = Scripted::new(vec![vec!["10.0.0.5"]], vec![]);
        let mut c = Conversation::new(&net, tls(), addr::Policy::default());
        let e = c.fetch("http://router.example/").unwrap_err().to_string();
        assert!(e.contains("router.example") && e.contains("10.0.0.5") && e.contains("RFC 1918"), "{e}");
    }
}
