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
                let target = head.lines().next().unwrap_or("").split(' ').nth(1).unwrap_or("").to_string();
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
    fn forget(&mut self, partition: &str, prefix: bool) -> Result<u32, String> {
        let mut w = Writer::new();
        w.str(partition); w.u8(prefix as u8);
        let r = self.s.borrow_mut().handle(&broker_label(), arcad::proto::broker_op::W_FORGET, &w.buf);
        if r.status != status::OK { return Err(format!("status {}", r.status)) }
        Ok(u32::from_le_bytes(r.body.bytes().try_into().unwrap()))
    }
}

/// A courier's vault: arcad under that courier's own label.
struct StateVault { s: Shared, label: Label }

impl Vault for StateVault {
    fn fill(&mut self, _: &[(u8, [u8; 16])]) -> Result<Vec<LockedBuf>, Refusal> { Err(Refusal::Denied) }
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
