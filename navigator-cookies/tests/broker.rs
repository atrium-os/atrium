//! The broker's cookie path end to end, in-process: real arcad state
//! (tickets, the jar, SameSite), real couriers (`arca_courier::send`) over
//! TLS to loopback servers, the broker deciding which requests carry what.
//! Only the launcher's jails and the sockets are left out (the VM gate,
//! `scripts/arca-vm-test.sh`, runs those).

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use arca_courier::schema::{Method, Refusal, Request, Response};
use arca_courier::send::{Courier, Endpoint, Vault};
use arca_store::codec::Writer;
use arca_store::secret::LockedBuf;
use arca_store::{Recipient, Scheme};
use arcad::courier::CourierLabel;
use arcad::label::Label;
use arcad::proto::{courier_op as cop, op, status};
use arcad::server::State;
use navigator_cookies::*;
use navigator_fetch::{addr, Connect};

// ── TLS servers on loopback ──────────────────────────────────────────

struct Pki { ca_der: Vec<u8>, certs: HashMap<String, (Vec<u8>, Vec<u8>)> }

fn pki(names: &[&str]) -> Pki {
    let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let ca_cert = ca.self_signed(&ca_key).unwrap();
    let mut certs = HashMap::new();
    for n in names {
        let k = rcgen::KeyPair::generate().unwrap();
        let c = rcgen::CertificateParams::new(vec![n.to_string()]).unwrap().signed_by(&k, &ca_cert, &ca_key).unwrap();
        certs.insert(n.to_string(), (c.der().to_vec(), k.serialize_der()));
    }
    Pki { ca_der: ca_cert.der().to_vec(), certs }
}

type Handler = Arc<dyn Fn(&str, &str) -> Vec<u8> + Send + Sync>;

/// Serves TLS as `name`; the handler gets the request line's target and
/// the Cookie header ("" for none). Every (target, cookie) is logged.
fn serve(p: &Pki, name: &str, handler: Handler) -> (SocketAddr, Arc<Mutex<Vec<(String, String)>>>) {
    use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    let (c, k) = p.certs[name].clone();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = Arc::new(rustls::ServerConfig::builder_with_provider(provider).with_safe_default_protocol_versions().unwrap()
        .with_no_client_auth().with_single_cert(vec![CertificateDer::from(c)], PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(k))).unwrap());
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let at = l.local_addr().unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let log2 = log.clone();
    std::thread::spawn(move || for s in l.incoming().flatten() {
        let (cfg, log, h) = (cfg.clone(), log2.clone(), handler.clone());
        std::thread::spawn(move || {
            let Ok(conn) = rustls::ServerConnection::new(cfg) else { return };
            let mut t = rustls::StreamOwned::new(conn, s);
            loop {
                let mut b = Vec::new();
                let mut one = [0u8; 1];
                while !b.ends_with(b"\r\n\r\n") {
                    match t.read(&mut one) { Ok(1) => b.push(one[0]), _ => return }
                }
                let head = String::from_utf8_lossy(&b).to_string();
                let mut target = head.lines().next().unwrap_or("").split(' ').nth(1).unwrap_or("").to_string();
                // A form's body, if any, after a `#` (the sign-in checks).
                let len: usize = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length: ").map(|v| v.trim().parse().unwrap_or(0))).unwrap_or(0);
                if len > 0 {
                    let mut body = vec![0u8; len];
                    if t.read_exact(&mut body).is_err() { return }
                    target = format!("{target}#{}", String::from_utf8_lossy(&body));
                }
                let cookie = head.lines().find_map(|l| l.strip_prefix("Cookie: ").or_else(|| l.strip_prefix("cookie: "))).unwrap_or("").to_string();
                log.lock().unwrap().push((target.clone(), cookie.clone()));
                if t.write_all(&h(&target, &cookie)).and_then(|_| t.flush()).is_err() { return }
            }
        });
    });
    (at, log)
}

fn http(status: &str, headers: &[String], body: &str) -> Vec<u8> {
    let mut r = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\n", body.len());
    for h in headers { r.push_str(h); r.push_str("\r\n") }
    r.push_str("\r\n");
    r.push_str(body);
    r.into_bytes()
}

/// A site's behaviour: `/set` sets a Lax (by default), a Strict and a None
/// cookie named after the site; `/echo` answers with the Cookie header it
/// got; `/login` sets `sess` and redirects to `/echo` (a cookie check);
/// `/to?<url>` redirects to `<url>`.
fn site(tag: &'static str) -> Handler {
    Arc::new(move |target, cookie| {
        let target = target.split('#').next().unwrap_or(target);
        let (path, q) = target.split_once('?').unwrap_or((target, ""));
        match path {
            "/set" => http("200 OK", &[format!("Set-Cookie: {tag}lax=1"), format!("Set-Cookie: {tag}strict=1; SameSite=Strict"),
                                       format!("Set-Cookie: {tag}none=1; SameSite=None; Secure")], "set"),
            "/echo" => http("200 OK", &[], cookie),
            "/login" => http("302 Found", &["Set-Cookie: sess=1".into(), "Location: /echo".into()], ""),
            "/to" => http("302 Found", &[format!("Location: {q}")], ""),
            _ => http("404 Not Found", &[], "no"),
        }
    })
}

// ── Arca in-process ───────────────────────────────────────────────────

const BROKER: &str = "org.atrium.navigator";

fn broker_label() -> Label { Label::WebBroker { user: "alice".into(), publisher: [9; 32], app_id: BROKER.into() } }

fn web_label(host: &str, port: u16, partition: &str) -> Label {
    Label::Courier(CourierLabel { user: "alice".into(), persona: "web".into(),
        endpoint: Recipient::endpoint(Scheme::Https, host, port, Some(partition)).unwrap() })
}

type Shared = Rc<RefCell<State>>;

/// arcad, asked as the web broker; every ticket it issued, kept for the
/// replay checks.
struct InProcArca { s: Shared, issued: Vec<(TicketAsk, [u8; 16])> }

impl Arca for InProcArca {
    fn ticket(&mut self, ask: &TicketAsk) -> Result<[u8; 16], String> {
        let r = self.s.borrow_mut().handle(&broker_label(), arcad::proto::broker_op::W_TICKET, &ask.encode());
        if r.status != status::OK { return Err(format!("status {}", r.status)) }
        let id: [u8; 16] = r.body.bytes().try_into().unwrap();
        self.issued.push((ask.clone(), id));
        Ok(id)
    }
    fn script_cookies(&mut self, host: &str, port: u16, partition: &str, path: &str) -> Result<String, String> {
        let mut w = Writer::new();
        arcad::proto::write_endpoint(&mut w, host, port);
        w.str(partition);
        w.str(path);
        let r = self.s.borrow_mut().handle(&broker_label(), arcad::proto::broker_op::W_SCRIPT_COOKIES, &w.buf);
        if r.status != status::OK { return Err(format!("status {}", r.status)); }
        String::from_utf8(r.body.bytes().to_vec()).map_err(|e| e.to_string())
    }
    fn forget(&mut self, partition: &str, prefix: bool) -> Result<u32, String> {
        let mut w = Writer::new();
        w.str(partition); w.u8(prefix as u8);
        let r = self.s.borrow_mut().handle(&broker_label(), arcad::proto::broker_op::W_FORGET, &w.buf);
        if r.status != status::OK { return Err(format!("status {}", r.status)) }
        Ok(u32::from_le_bytes(r.body.bytes().try_into().unwrap()))
    }
    fn handle_for(&mut self, h: &[u8; 16], host: &str, port: u16, partition: &str) -> Result<[u8; 16], String> {
        let mut w = Writer::new();
        w.raw(h); arcad::proto::write_endpoint(&mut w, host, port); w.str(partition);
        let r = self.s.borrow_mut().handle(&broker_label(), arcad::proto::broker_op::W_HANDLE_FOR, &w.buf);
        if r.status != status::OK { return Err(format!("status {}", r.status)) }
        Ok(r.body.bytes().try_into().unwrap())
    }
}

/// A courier's vault: arcad under that courier's own label.
struct StateVault { s: Shared, label: Label }

impl Vault for StateVault {
    fn fill(&mut self, refs: &[(u8, [u8; 16])]) -> Result<Vec<LockedBuf>, Refusal> {
        let mut w = Writer::new();
        w.u8(refs.len() as u8);
        for (src, x) in refs { w.u8(*src); w.raw(x) }
        let r = self.s.borrow_mut().handle(&self.label, cop::FILL, &w.buf);
        if r.status != status::OK { return Err(Refusal::Denied) }
        let (mut b, mut out) = (r.body.bytes(), vec![]);
        while b.len() >= 4 {
            let n = u32::from_le_bytes(b[..4].try_into().unwrap()) as usize;
            out.push(LockedBuf::from_slice(&b[4..4 + n]).unwrap());
            b = &b[4 + n..];
        }
        Ok(out)
    }
    fn set_cookie(&mut self, name: &str, value: &[u8], attrs: &[u8], path: &str, ticket: Option<&[u8; 16]>) {
        let mut w = Writer::new();
        w.str(name); w.bytes(value); w.bytes(attrs); w.str(path);
        if let Some(t) = ticket { w.raw(t) }
        self.s.borrow_mut().handle(&self.label, cop::SET_COOKIE, &w.buf);
    }
    fn cookies(&mut self, method: Method, path: &str, query: &str, ticket: Option<&[u8; 16]>) -> Option<LockedBuf> {
        cookies_as(&self.s, &self.label, method as u8, path, query, ticket).1
    }
}

fn cookies_as(s: &Shared, label: &Label, method: u8, path: &str, query: &str, ticket: Option<&[u8; 16]>) -> (u8, Option<LockedBuf>) {
    let mut w = Writer::new();
    w.str(path);
    if let Some(t) = ticket { w.raw(t); w.u8(method); w.str(query) }
    let r = s.borrow_mut().handle(label, cop::COOKIES, &w.buf);
    let v = (r.status == status::OK && !r.body.bytes().is_empty()).then(|| LockedBuf::from_slice(r.body.bytes()).unwrap());
    (r.status, v)
}

struct Net(HashMap<String, SocketAddr>);
impl Connect for Net {
    fn resolve(&self, host: &str, _: u16) -> std::io::Result<Vec<SocketAddr>> { self.0.get(host).map(|a| vec![*a]).ok_or_else(|| std::io::Error::other("nx")) }
    fn connect_addr(&self, a: SocketAddr) -> std::io::Result<TcpStream> { TcpStream::connect(a) }
}

/// The launcher, in-process: one real courier per (host, port, partition).
struct InProcCouriers { s: Shared, net: &'static Net, tls: Arc<rustls::ClientConfig>, open: HashMap<(String, u16, String), Courier<'static>> }

impl Couriers for InProcCouriers {
    fn send(&mut self, host: &str, port: u16, partition: &str, req: &Request) -> Result<Response, String> {
        let (net, tls) = (self.net, self.tls.clone());
        let c = self.open.entry((host.into(), port, partition.into())).or_insert_with(|| {
            Courier::new(Endpoint { host: host.into(), port }, net, tls, addr::Policy { grant_private_network: true })
        });
        let mut v = StateVault { s: self.s.clone(), label: web_label(host, port, partition) };
        Ok(c.handle(&req.encode(), &mut v))
    }
}

struct World {
    b: Broker<InProcArca, InProcCouriers>,
    s: Shared,
    news: String,
    cdn: String,
    news_log: Arc<Mutex<Vec<(String, String)>>>,
    cdn_log: Arc<Mutex<Vec<(String, String)>>>,
    _d: tempfile::TempDir,
}

fn world() -> World {
    let p = pki(&["www.news.example", "cdn.other.example"]);
    let (na, news_log) = serve(&p, "www.news.example", site("news"));
    let (ca, cdn_log) = serve(&p, "cdn.other.example", site("cdn"));
    let net: &'static Net = Box::leak(Box::new(Net(HashMap::from([("www.news.example".into(), na), ("cdn.other.example".into(), ca)]))));
    let mut roots = rustls::RootCertStore::empty();
    roots.add(rustls_pki_types::CertificateDer::from(p.ca_der.clone())).unwrap();
    let tls = Arc::new(rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions().unwrap().with_root_certificates(roots).with_no_client_auth());
    let d = tempfile::tempdir().unwrap();
    let mut st = State::new(d.path().into()).unwrap();
    let mut w = Writer::new();
    w.str("alice"); w.raw(&[1; 32]);
    assert_eq!(st.handle(&Label::Vestibulum, op::V_CREATE, &w.buf).status, status::OK);
    let s: Shared = Rc::new(RefCell::new(st));
    let b = Broker::new(InProcArca { s: s.clone(), issued: vec![] }, InProcCouriers { s: s.clone(), net, tls, open: HashMap::new() });
    World { b, s, news: format!("https://www.news.example:{}", na.port()), cdn: format!("https://cdn.other.example:{}", ca.port()),
            news_log, cdn_log, _d: d }
}

fn names(body: &[u8]) -> String {
    let mut v: Vec<String> = String::from_utf8_lossy(body).split("; ").filter(|p| !p.is_empty()).map(|p| p.split('=').next().unwrap().to_string()).collect();
    v.sort();
    v.join(",")
}

const NEWS_VIEW: ViewId = 1;

// ── the checks ────────────────────────────────────────────────────────

#[test]
fn an_uncookied_view_never_gets_a_ticket_and_sends_no_cookie() {
    let mut w = world();
    w.b.show(NEWS_VIEW, &format!("{}/", w.news)).unwrap();
    let r = w.b.fetch(NEWS_VIEW, &Fetch::navigation(&format!("{}/set", w.news), None, true));
    assert_eq!(r, Err(Refused::NotCookied));
    assert!(r.unwrap_err().plain_instead(), "it goes the plain way");
    assert_eq!(w.b.tickets, 0, "no ticket was asked for");
    assert!(w.b.arca.issued.is_empty());
    assert!(w.news_log.lock().unwrap().is_empty(), "and nothing went through a courier");
    // Even a view the broker has never seen.
    assert_eq!(w.b.fetch(99, &Fetch::navigation(&format!("{}/", w.news), None, true)), Err(Refused::NotCookied));
    assert_eq!(w.b.tickets, 0);
}

#[test]
fn a_cookied_view_keeps_and_sends_its_sites_cookies() {
    let mut w = world();
    w.b.show(NEWS_VIEW, &format!("{}/", w.news)).unwrap();
    w.b.set_cookied(NEWS_VIEW, Why::Reader).unwrap();
    let r = w.b.fetch(NEWS_VIEW, &Fetch::navigation(&format!("{}/set", w.news), None, true)).unwrap();
    assert_eq!((r.status, r.body.as_slice()), (200, &b"set"[..]));
    let r = w.b.fetch(NEWS_VIEW, &Fetch::navigation(&format!("{}/echo", w.news), None, true)).unwrap();
    assert_eq!(names(&r.body), "newslax,newsnone,newsstrict", "the reader's own navigation: every cookie");
    // A same-site subresource (the page's own) gets them all too.
    let r = w.b.fetch(NEWS_VIEW, &Fetch::subresource(&format!("{}/echo", w.news), "www.news.example")).unwrap();
    assert_eq!(names(&r.body), "newslax,newsnone,newsstrict");
    // Turned anonymous again: no ticket, the plain way.
    let n = w.b.tickets;
    w.b.set_anonymous(NEWS_VIEW);
    assert_eq!(w.b.fetch(NEWS_VIEW, &Fetch::navigation(&format!("{}/echo", w.news), None, true)), Err(Refused::NotCookied));
    assert_eq!(w.b.tickets, n);
}

#[test]
fn a_cross_site_subresource_gets_no_lax_or_strict_cookie() {
    let mut w = world();
    w.b.show(NEWS_VIEW, &format!("{}/", w.news)).unwrap();
    w.b.set_cookied(NEWS_VIEW, Why::Reader).unwrap();
    // cdn.other.example sets its three cookies, in news.example's partition.
    w.b.fetch(NEWS_VIEW, &Fetch::subresource(&format!("{}/set", w.cdn), "www.news.example")).unwrap();
    // A subresource of the news page: cross-site, so only SameSite=None.
    let r = w.b.fetch(NEWS_VIEW, &Fetch::subresource(&format!("{}/echo", w.cdn), "www.news.example")).unwrap();
    assert_eq!(names(&r.body), "cdnnone");
    let (_, cookie) = w.cdn_log.lock().unwrap().last().cloned().unwrap();
    assert!(!cookie.contains("cdnlax") && !cookie.contains("cdnstrict"), "what the server got: {cookie:?}");
}

#[test]
fn partitions_are_separate_jars() {
    let mut w = world();
    w.b.show(NEWS_VIEW, &format!("{}/", w.news)).unwrap();
    w.b.set_cookied(NEWS_VIEW, Why::Reader).unwrap();
    w.b.fetch(NEWS_VIEW, &Fetch::subresource(&format!("{}/set", w.cdn), "www.news.example")).unwrap();
    // The same CDN as the top of another view: its own partition, empty.
    let other: ViewId = 2;
    w.b.show(other, &format!("{}/", w.cdn)).unwrap();
    w.b.set_cookied(other, Why::Reader).unwrap();
    assert_eq!(w.b.partition(other), Some("other.example"));
    let r = w.b.fetch(other, &Fetch::navigation(&format!("{}/echo", w.cdn), None, true)).unwrap();
    assert_eq!(names(&r.body), "", "news.example's cdn cookies stay in news.example's partition");
}

#[test]
fn each_unrecorded_trail_is_its_own_jar_and_closing_it_forgets_it() {
    let mut w = world();
    let top = format!("{}/", w.news);
    let (personal, u1, u2): (ViewId, ViewId, ViewId) = (1, 2, 3);
    w.b.show_in(personal, &top, None).unwrap();
    w.b.show_in(u1, &top, Some(0xa1)).unwrap();
    w.b.show_in(u2, &top, Some(0xb2)).unwrap();
    assert_eq!(w.b.partition(u1), Some("u-00000000000000a1.news.example"));
    for v in [personal, u1, u2] {
        w.b.set_cookied(v, Why::Reader).unwrap();
    }
    // Signing in (cookies set) in one Unrecorded trail…
    w.b.fetch(u1, &Fetch::navigation(&format!("{}/set", w.news), None, true)).unwrap();
    let echo = |w: &mut World, v| names(&w.b.fetch(v, &Fetch::navigation(&format!("{}/echo", w.news), None, true)).unwrap().body);
    assert_eq!(echo(&mut w, u1), "newslax,newsnone,newsstrict");
    // …signs in nowhere else.
    assert_eq!(echo(&mut w, u2), "", "another Unrecorded trail");
    assert_eq!(echo(&mut w, personal), "", "the Personal persona");
    w.b.fetch(personal, &Fetch::navigation(&format!("{}/set", w.news), None, true)).unwrap();
    // The trail closes: its jar is forgotten, and only its.
    assert!(w.b.arca.forget(&navigator_cookies::jar_prefix(0xa1), true).unwrap() > 0);
    assert_eq!(echo(&mut w, u1), "");
    assert_eq!(echo(&mut w, personal), "newslax,newsnone,newsstrict");
    // "Forget this site" in Personal.
    assert!(w.b.arca.forget("news.example", false).unwrap() > 0);
    assert_eq!(echo(&mut w, personal), "");
}

#[test]
fn a_ticket_cannot_be_replayed_or_used_for_another_url_or_partition() {
    let mut w = world();
    w.b.show(NEWS_VIEW, &format!("{}/", w.news)).unwrap();
    w.b.set_cookied(NEWS_VIEW, Why::Reader).unwrap();
    w.b.fetch(NEWS_VIEW, &Fetch::navigation(&format!("{}/set", w.news), None, true)).unwrap();
    w.b.fetch(NEWS_VIEW, &Fetch::navigation(&format!("{}/echo?page=1", w.news), None, true)).unwrap();
    let (ask, used) = w.b.arca.issued.last().cloned().unwrap();
    assert_eq!((ask.path.as_str(), ask.query.as_str(), ask.partition.as_str()), ("/echo", "page=1", "news.example"));
    let port = ask.port;
    let me = web_label("www.news.example", port, "news.example");
    // Replayed by the courier that used it: spent.
    assert_eq!(cookies_as(&w.s, &me, 1, "/echo", "page=1", Some(&used)).0, status::DENIED, "replay");
    // A fresh ticket for /echo?page=1 does not open another URL…
    let t = w.b.arca.ticket(&ask).unwrap();
    assert_eq!(cookies_as(&w.s, &me, 1, "/echo", "page=2", Some(&t)).0, status::DENIED, "another query");
    assert_eq!(cookies_as(&w.s, &me, 1, "/set", "page=1", Some(&t)).0, status::DENIED, "another path");
    assert_eq!(cookies_as(&w.s, &me, 2, "/echo", "page=1", Some(&t)).0, status::DENIED, "another method");
    // …or another partition's courier, or another host's.
    assert_eq!(cookies_as(&w.s, &web_label("www.news.example", port, "other.example"), 1, "/echo", "page=1", Some(&t)).0, status::DENIED, "another partition");
    assert_eq!(cookies_as(&w.s, &web_label("cdn.other.example", port, "news.example"), 1, "/echo", "page=1", Some(&t)).0, status::DENIED, "another host");
    // …and is still good for its own request, once.
    let (st, v) = cookies_as(&w.s, &me, 1, "/echo", "page=1", Some(&t));
    assert_eq!(st, status::OK);
    assert!(v.is_some());
    assert_eq!(cookies_as(&w.s, &me, 1, "/echo", "page=1", Some(&t)).0, status::DENIED);
}

#[test]
fn a_cookie_check_redirect_works_and_a_cross_site_chain_drops_strict() {
    let mut w = world();
    w.b.show(NEWS_VIEW, &format!("{}/", w.news)).unwrap();
    w.b.set_cookied(NEWS_VIEW, Why::SiteNeedsIt).unwrap();
    // The cookie check: /login sets `sess` and redirects to /echo, which
    // must see it.
    let r = w.b.fetch(NEWS_VIEW, &Fetch::navigation(&format!("{}/login", w.news), None, true)).unwrap();
    assert_eq!((r.redirects, names(&r.body)), (1, "sess".into()));
    assert!(r.url.ends_with("/echo"));
    w.b.fetch(NEWS_VIEW, &Fetch::navigation(&format!("{}/set", w.news), None, true)).unwrap();
    // A chain that passes through another site: the news hop is cross-site,
    // so its Strict cookie stays home (a top-level GET still takes Lax).
    let via_cdn = format!("{}/to?{}/echo", w.cdn, w.news);
    let r = w.b.fetch(NEWS_VIEW, &Fetch::navigation(&via_cdn, None, true)).unwrap();
    assert_eq!(r.redirects, 1);
    assert_eq!(names(&r.body), "newslax,newsnone,sess", "no newsstrict after a cross-site hop");
    // The same chain wholly on news.example keeps Strict.
    let r = w.b.fetch(NEWS_VIEW, &Fetch::navigation(&format!("{}/to?/echo", w.news), None, true)).unwrap();
    assert_eq!(names(&r.body), "newslax,newsnone,newsstrict,sess");
}

#[test]
fn single_resource_grant_stops_at_the_first_redirect() {
    let mut w = world();
    w.b.show(NEWS_VIEW, &w.news).unwrap();
    w.b.set_cookied(NEWS_VIEW, Why::Reader).unwrap();
    let ask = Fetch::navigation(&format!("{}/to?{}/echo", w.news, w.cdn), None, true);
    let before = w.b.tickets;
    assert!(matches!(w.b.fetch_once(NEWS_VIEW, &ask), Err(Refused::Redirect(_))));
    assert_eq!(w.b.tickets, before + 1);
}

#[test]
fn only_https_get_and_head_and_never_down_to_http() {
    let mut w = world();
    w.b.show(NEWS_VIEW, &format!("{}/", w.news)).unwrap();
    w.b.set_cookied(NEWS_VIEW, Why::Reader).unwrap();
    let n = w.b.tickets;
    let http_url = w.news.replace("https:", "http:");
    let r = w.b.fetch(NEWS_VIEW, &Fetch::navigation(&format!("{http_url}/echo"), None, true));
    assert_eq!(r, Err(Refused::NotHttps));
    assert!(r.unwrap_err().plain_instead());
    let mut post = Fetch::navigation(&format!("{}/echo", w.news), None, true);
    post.method = Method::Post;
    assert_eq!(w.b.fetch(NEWS_VIEW, &post), Err(Refused::Method), "no logins: a POST is refused");
    assert_eq!(w.b.tickets, n, "neither asked for a ticket");
    let down = format!("{}/to?{http_url}/echo", w.news);
    assert!(matches!(w.b.fetch(NEWS_VIEW, &Fetch::navigation(&down, None, true)), Err(Refused::Redirect(_))));
}

#[test]
fn moving_a_view_to_another_site_makes_it_anonymous_again() {
    let mut w = world();
    w.b.show(NEWS_VIEW, &format!("{}/", w.news)).unwrap();
    w.b.set_cookied(NEWS_VIEW, Why::Reader).unwrap();
    w.b.show(NEWS_VIEW, &format!("{}/other-page", w.news)).unwrap();
    assert_eq!(w.b.cookied(NEWS_VIEW), Some(Why::Reader), "the same site: still cookied");
    w.b.show(NEWS_VIEW, &format!("{}/", w.cdn)).unwrap();
    assert_eq!(w.b.cookied(NEWS_VIEW), None, "another site: consent is not carried");
    assert_eq!(w.b.partition(NEWS_VIEW), Some("other.example"));
}

/// A VPN gateway's SAML sign-in, as the sign-in window makes it: the
/// gateway's login sends the window to the identity provider; its login
/// form is posted with its session cookie; its answer is posted to the
/// gateway, whose sign-on token Arca keeps for Crypta, never the jar.
#[test]
fn a_sign_in_windows_forms_go_through_couriers_and_the_token_to_crypta() {
    let p = pki(&["gw.corp.example", "sso.idp.example"]);
    let (ia, idp_log) = serve(&p, "sso.idp.example", Arc::new(|target, cookie| {
        let (path, body) = target.split_once('#').unwrap_or((target, ""));
        let path = path.split('?').next().unwrap_or("");
        match path {
            "/auth" => http("200 OK", &["Set-Cookie: KC_SESSION=s1; Secure; SameSite=None".into()], "<form method=post action=/login>"),
            "/login" if cookie.contains("KC_SESSION=s1") && body == "username=alice&password=pw" => http("200 OK", &[], "<form method=post action=gw/acs>"),
            _ => http("403 Forbidden", &[], "no session"),
        }
    }));
    let idp_port = ia.port();
    let (ga, gw_log) = serve(&p, "gw.corp.example", Arc::new(move |target, cookie| {
        let (path, body) = target.split_once('#').unwrap_or((target, ""));
        let path = path.split('?').next().unwrap_or("");
        match path {
            "/login" => http("302 Found", &[format!("Location: https://sso.idp.example:{idp_port}/auth?SAMLRequest=r")], ""),
            "/acs" if body == "SAMLResponse=signed&RelayState=x" =>
                http("303 See Other", &["Set-Cookie: acSamlv2Token=TOKEN123; Secure; HttpOnly".into(), "Set-Cookie: webvpnlogin=1; Secure".into(), "Location: /final".into()], ""),
            "/final" => http("200 OK", &[], "signed on"),
            "/echo" => http("200 OK", &[], cookie),
            _ => http("404 Not Found", &[], "no"),
        }
    }));
    let net: &'static Net = Box::leak(Box::new(Net(HashMap::from([("gw.corp.example".into(), ga), ("sso.idp.example".into(), ia)]))));
    let mut roots = rustls::RootCertStore::empty();
    roots.add(rustls_pki_types::CertificateDer::from(p.ca_der.clone())).unwrap();
    let tls = Arc::new(rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions().unwrap().with_root_certificates(roots).with_no_client_auth());
    let d = tempfile::tempdir().unwrap();
    let mut st = State::new(d.path().into()).unwrap();
    st.open_machine().unwrap();
    let mut w = Writer::new();
    w.str("alice"); w.raw(&[1; 32]);
    assert_eq!(st.handle(&Label::Vestibulum, op::V_CREATE, &w.buf).status, status::OK);
    let s: Shared = Rc::new(RefCell::new(st));
    let mut b = Broker::new(InProcArca { s: s.clone(), issued: vec![] }, InProcCouriers { s: s.clone(), net, tls, open: HashMap::new() });
    let gw = format!("https://gw.corp.example:{}", ga.port());
    let idp = format!("https://sso.idp.example:{}", ia.port());
    const V: ViewId = 7;

    // An ordinary cookied view sends no form.
    b.show(1, &format!("{gw}/")).unwrap();
    b.set_cookied(1, Why::Reader).unwrap();
    assert_eq!(b.fetch(1, &Fetch::form(&format!("{gw}/acs"), "gw.corp.example", b"x=1".to_vec())), Err(Refused::Method));

    b.show_sign_in(V, &format!("{gw}/login"), "corp").unwrap();
    assert_eq!(b.partition(V), Some("vpn-corp.corp.example"));
    assert!(b.sign_in(V) && b.cookied(V).is_some(), "cookied from the start");
    // The gateway sends the window to the identity provider; its pages
    // stay in the same jar (one jar for the whole sign-in).
    let r = b.fetch(V, &Fetch::navigation(&format!("{gw}/login"), None, true)).unwrap();
    assert_eq!((r.status, r.redirects), (200, 1));
    assert!(r.url.starts_with(&idp));
    b.show_sign_in(V, &r.url, "corp").unwrap();
    assert_eq!(b.partition(V), Some("vpn-corp.corp.example"));
    // The password typed into Arca's field: the broker holds only its
    // handle, and the courier puts it in the form (form-encoded).
    let hold = |s: &Shared, v: &[u8]| -> [u8; 16] {
        let mut w = Writer::new();
        let Label::WebBroker { publisher, app_id, .. } = broker_label() else { unreachable!() };
        w.str("alice"); w.raw(&publisher); w.str(&app_id); w.bytes(v);
        s.borrow_mut().handle(&Label::TrustedUi, op::U_HOLD, &w.buf).body.bytes().try_into().unwrap()
    };
    let mut f = Fetch::form(&format!("{idp}/login"), "sso.idp.example", b"username=alice&password=".to_vec());
    f.secret = Some((f.body.len() + 1, hold(&s, b"pw")));
    assert!(matches!(b.fetch(V, &f), Err(Refused::Malformed(_))), "a place past the form");
    f.secret = Some((f.body.len(), hold(&s, b"pw")));
    assert!(matches!(b.fetch(1, &f), Err(Refused::Method)), "not from an ordinary view");
    let r = b.fetch(V, &f).unwrap();
    assert_eq!(r.status, 200, "the idp's own session cookie and the password went with its form");
    assert!(matches!(b.fetch(V, &f), Err(Refused::Ticket(_))), "the handle is spent");
    // Form-encoded: a password with '&' and '=' stays one value.
    let mut f = Fetch::form(&format!("{idp}/login"), "sso.idp.example", b"username=alice&password=".to_vec());
    f.secret = Some((f.body.len(), hold(&s, b"p&w=1")));
    assert_eq!(b.fetch(V, &f).unwrap().status, 403);
    assert!(idp_log.lock().unwrap().iter().any(|(t, _)| t.ends_with("password=p%26w%3D1")), "{:?}", idp_log.lock().unwrap());
    let r = b.fetch(V, &Fetch::form(&format!("{gw}/acs"), "sso.idp.example", b"SAMLResponse=signed&RelayState=x".to_vec())).unwrap();
    assert_eq!((r.status, r.body.as_slice()), (200, &b"signed on"[..]), "after the form, the answer's page by GET");
    assert!(r.url.ends_with("/final"));
    // The token is Crypta's, bound to the gateway's host.
    let lr = s.borrow_mut().handle(&Label::Service(op::CRYPTA_SERVICE.into()), op::LIST, &[]);
    let listed = String::from_utf8_lossy(lr.body.bytes()).into_owned();
    assert!(listed.contains("corp/sso@gw.corp.example"), "{listed:?}");
    // And never the page's: the jar has the gateway's other cookie only.
    b.show_sign_in(V, &format!("{gw}/"), "corp").unwrap();
    let r = b.fetch(V, &Fetch::navigation(&format!("{gw}/echo"), None, true)).unwrap();
    assert_eq!(String::from_utf8_lossy(&r.body), "webvpnlogin=1");
    assert!(gw_log.lock().unwrap().iter().all(|(_, c)| !c.contains("TOKEN123")), "the token never went back out");
}

#[test]
fn microsoft_sheet_claims_mail_session_and_fetches_with_its_own_cookies() {
    const HOST: &str = "login.microsoftonline.com";
    const FIRST_SCREEN: &str = "<!doctype html><label id=prompt for=account></label><input id=account><script>document.getElementById('prompt').textContent='Enter your account'</script>";
    let p = pki(&[HOST]);
    let (addr, log) = serve(&p, HOST, Arc::new(|target, cookie| {
        if target.starts_with("/common/oauth2/v2.0/authorize?") {
            http("302 Found", &["Location: /first".into(), "Set-Cookie: flow=dev; Path=/; Secure; HttpOnly".into()], "")
        } else if target == "/first" && cookie.contains("flow=dev") {
            http("200 OK", &["Content-Type: text/html; charset=utf-8".into()], FIRST_SCREEN)
        } else { http("403 Forbidden", &[], "") }
    }));
    let mut world = world();
    let net = Box::leak(Box::new(Net(HashMap::from([(HOST.into(), addr)]))));
    world.b.couriers.net = net;
    let mut roots = rustls::RootCertStore::empty();
    roots.add(p.ca_der.into()).unwrap();
    world.b.couriers.tls = Arc::new(rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions().unwrap().with_root_certificates(roots).with_no_client_auth());
    let mail = Label::App { user: "alice".into(), app_id: "org.atrium.mail".into(), publisher: [9; 32], persona: None };
    let mut arca = world.s.borrow_mut();
    assert_eq!(arca.handle(&mail, op::MAIL_SIGN_IN, &[]).status, status::UNSUPPORTED);
    arca.mail_sign_ins.registration = Some("11111111-1111-4111-8111-111111111111".into());
    arca.mail_sign_ins.mail_publisher = Some([9; 32]);
    let reply = arca.handle(&mail, op::MAIL_SIGN_IN, &[]);
    assert_eq!(reply.status, status::OK);
    let handle: [u8; 16] = reply.body.bytes().try_into().unwrap();
    let claim = arcad::proto::broker_op::W_MAIL_SIGN_IN;
    assert_ne!(arca.handle(&mail, claim, &handle).status, status::OK);
    let url = arca.handle(&broker_label(), claim, &handle);
    assert_eq!(url.status, status::OK);
    assert_eq!(arca.handle(&broker_label(), claim, &handle).status, status::DENIED);
    drop(arca);
    let url = std::str::from_utf8(url.body.bytes()).unwrap();
    assert!(url.contains("code_challenge_method=S256"));
    let page = world.b.mail_first_screen(1, &handle, url).unwrap();
    assert_eq!(page.status, 200);
    assert_eq!(page.body, FIRST_SCREEN.as_bytes());
    assert_eq!(log.lock().unwrap().len(), 2);
    assert_eq!(world.b.script_cookies(1, &page.url).unwrap().as_deref(), Some(""));
    if let Some(path) = std::env::var_os("I0_DEVELOPMENT_DOCUMENT") {
        // Only the public fixture DOM crosses into the engine test, never the authorization URL.
        std::fs::File::create_new(path).unwrap().write_all(&page.body).unwrap();
    }
    let tickets = world.b.tickets;
    for target in ["https://other.example/x", arcad::oauth::MICROSOFT.redirect] {
        assert!(world.b.fetch(1, &Fetch::navigation(target, Some(HOST), false)).is_err());
    }
    assert_eq!(world.b.tickets, tickets);
    world.b.close_mail_sheet(1);
    assert_eq!(world.b.partition(1), None);
}

#[test]
fn a_live_page_signs_in_with_a_form_and_its_scripts_post_in_its_own_jar() {
    let mut w = world();
    // A cookied document-lane view may not POST, from a script or a form.
    w.b.show(NEWS_VIEW, &format!("{}/", w.news)).unwrap();
    w.b.set_cookied(NEWS_VIEW, Why::Reader).unwrap();
    let script = Fetch::script_post(&format!("{}/echo", w.news), "www.news.example", "application/json", b"{}".to_vec());
    assert_eq!(w.b.fetch(NEWS_VIEW, &script), Err(Refused::Method));
    let form = Fetch::live_form(&format!("{}/login", w.news), "www.news.example", b"user=ana".to_vec());
    assert_eq!(w.b.fetch(NEWS_VIEW, &form), Err(Refused::Method));
    // A live page in an Unrecorded jar: cookied from the start, its form
    // signs in (the cookie check's redirect lands on /echo with it).
    const LIVE: ViewId = 7;
    w.b.show_live(LIVE, &format!("{}/app", w.news), Some(42)).unwrap();
    let r = w.b.fetch(LIVE, &form).unwrap();
    assert_eq!((r.status, names(&r.body).as_str()), (200, "sess"));
    assert_eq!(r.url, format!("{}/echo", w.news));
    assert!(w.news_log.lock().unwrap().iter().any(|(t, _)| t == "/login#user=ana"), "the form's body was sent");
    // Its scripts POST with their own type, and carry the session.
    let r = w.b.fetch_once(LIVE, &script).unwrap();
    assert_eq!(names(&r.body), "sess");
    assert!(w.news_log.lock().unwrap().iter().any(|(t, _)| t == "/echo#{}"));
    // A type out of shape, a password slot, or a type on a GET is refused.
    for bad in ["", "text/plain\r\nX-Evil: 1", &"x".repeat(CONTENT_TYPE_MAX + 1)] {
        let f = Fetch::script_post(&format!("{}/echo", w.news), "www.news.example", bad, vec![]);
        assert_eq!(w.b.fetch(LIVE, &f), Err(Refused::Method), "{bad:?}");
    }
    let mut secret = form.clone();
    secret.secret = Some((0, [1; 16]));
    assert!(w.b.fetch(LIVE, &secret).is_err());
    let mut typed = Fetch::subresource(&format!("{}/echo", w.news), "www.news.example");
    typed.content_type = Some("text/plain".into());
    assert!(matches!(w.b.fetch(LIVE, &typed), Err(Refused::Malformed(_))));
    // The same site in another jar (the Personal one) has no session.
    const OTHER: ViewId = 8;
    w.b.show_live(OTHER, &format!("{}/app", w.news), None).unwrap();
    let r = w.b.fetch(OTHER, &Fetch::subresource(&format!("{}/echo", w.news), "www.news.example")).unwrap();
    assert_eq!(names(&r.body), "");
    // A new run of the page in the first jar is still signed in.
    w.b.close(LIVE);
    w.b.show_live(LIVE, &format!("{}/app", w.news), Some(42)).unwrap();
    let r = w.b.fetch(LIVE, &Fetch::navigation(&format!("{}/echo", w.news), None, false)).unwrap();
    assert_eq!(names(&r.body), "sess");
}
