//! The courier against a real TLS server on loopback (arca.md §5, A1):
//! secrets only after TLS to the bound host, redirects handed back and
//! never followed, Set-Cookie to Arca, echoes masked, and hostile client
//! bytes that never reach Arca.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use arca_courier::schema::{Encoding, Header, Method, Place, Refusal, Request, Response, Segment, Slot};
use arca_courier::send::{Courier, Endpoint, Vault};
use arca_store::secret::LockedBuf;
use arcad::proto::courier_op::SOURCE_ITEM;
use navigator_fetch::{addr, Connect};

// ── a TLS server ─────────────────────────────────────────────────────

struct Pki {
    ca_pem: String,
    certs: HashMap<String, (Vec<u8>, Vec<u8>)>,
}

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
    Pki { ca_pem: ca_cert.pem(), certs }
}

type Handler = Arc<dyn Fn(&[u8]) -> Vec<u8> + Send + Sync>;

/// Serves TLS as `name` on loopback; records each request it reads.
fn serve(p: &Pki, name: &str, handler: Handler) -> (SocketAddr, Arc<Mutex<Vec<Vec<u8>>>>) {
    use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    let (c, k) = p.certs[name].clone();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = Arc::new(
        rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![CertificateDer::from(c)], PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(k)))
            .unwrap(),
    );
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let at = l.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let (cfg, seen, h) = (cfg.clone(), seen2.clone(), handler.clone());
            std::thread::spawn(move || {
                let Ok(conn) = rustls::ServerConnection::new(cfg) else { return };
                let mut t = rustls::StreamOwned::new(conn, s);
                loop {
                    let Some(req) = read_req(&mut t) else { return };
                    seen.lock().unwrap().push(req.clone());
                    if t.write_all(&h(&req)).and_then(|_| t.flush()).is_err() {
                        return;
                    }
                }
            });
        }
    });
    (at, seen)
}

fn read_req(t: &mut impl Read) -> Option<Vec<u8>> {
    let mut b = Vec::new();
    let mut one = [0u8; 1];
    while !b.ends_with(b"\r\n\r\n") {
        if t.read(&mut one).ok()? == 0 {
            return None;
        }
        b.push(one[0]);
    }
    let head = String::from_utf8_lossy(&b).to_ascii_lowercase();
    let n: usize = head.lines().find_map(|l| l.strip_prefix("content-length: ")).and_then(|v| v.trim().parse().ok()).unwrap_or(0);
    let mut body = vec![0u8; n];
    t.read_exact(&mut body).ok()?;
    b.extend(body);
    Some(b)
}

fn http(status: &str, headers: &[&str], body: &[u8]) -> Vec<u8> {
    let mut r = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\n", body.len());
    for h in headers {
        r.push_str(h);
        r.push_str("\r\n");
    }
    r.push_str("\r\n");
    let mut v = r.into_bytes();
    v.extend_from_slice(body);
    v
}

// ── the courier's world ──────────────────────────────────────────────

/// Hosts → loopback servers; counts every lookup by host.
struct Net {
    map: HashMap<String, SocketAddr>,
    lookups: Mutex<HashMap<String, usize>>,
}

impl Connect for Net {
    fn resolve(&self, host: &str, _: u16) -> std::io::Result<Vec<SocketAddr>> {
        *self.lookups.lock().unwrap().entry(host.into()).or_default() += 1;
        self.map.get(host).map(|a| vec![*a]).ok_or_else(|| std::io::Error::other("nx"))
    }
    fn connect_addr(&self, a: SocketAddr) -> std::io::Result<TcpStream> {
        TcpStream::connect(a)
    }
}

#[derive(Default)]
struct MockVault {
    items: HashMap<[u8; 16], Vec<u8>>,
    fills: Vec<Vec<[u8; 16]>>,
    cookies: Vec<(String, Vec<u8>, Vec<u8>, String)>,
    /// What Arca's matching answers, and the paths it was asked for.
    jar: Option<Vec<u8>>,
    asked: Vec<String>,
}

impl Vault for MockVault {
    fn fill(&mut self, refs: &[(u8, [u8; 16])]) -> Result<Vec<LockedBuf>, Refusal> {
        self.fills.push(refs.iter().map(|r| r.1).collect());
        refs.iter()
            .map(|(_, r)| self.items.get(r).map(|v| LockedBuf::from_slice(v).unwrap()).ok_or(Refusal::Denied))
            .collect()
    }
    fn set_cookie(&mut self, name: &str, value: &[u8], attrs: &[u8], path: &str, _: Option<&[u8; 16]>) {
        self.cookies.push((name.into(), value.into(), attrs.into(), path.into()));
    }
    fn cookies(&mut self, path: &str, ticket: Option<&[u8; 16]>) -> Option<LockedBuf> {
        self.asked.push(format!("{path}{}", ticket.map_or(String::new(), |t| format!(" ticket {}", t[0]))));
        self.jar.as_ref().map(|j| LockedBuf::from_slice(j).unwrap())
    }
}

const TOK: [u8; 16] = [7; 16];

fn vault() -> MockVault {
    let mut v = MockVault::default();
    v.items.insert(TOK, b"s3cret-token".to_vec());
    v.items.insert([8; 16], b"p&ss w\"rd".to_vec());
    v
}

fn get_with_bearer(path: &str) -> Request {
    Request {
        method: Method::Get,
        path: path.as_bytes().to_vec(),
        query: b"a=1".to_vec(),
        headers: vec![(Header::Accept, b"application/json".to_vec())],
        slots: vec![Slot { source: SOURCE_ITEM, reference: TOK, place: Place::Bearer }],
        body: vec![],
        ticket: None,
    }
}

fn tls(p: &Pki) -> Arc<rustls::ClientConfig> {
    navigator_fetch::tls_config(p.ca_pem.as_bytes()).unwrap()
}

const LOOPBACK: addr::Policy = addr::Policy { grant_private_network: true };

fn ep(h: &str) -> Endpoint {
    Endpoint { host: h.into(), port: 443 }
}

#[test]
fn the_token_goes_only_into_tls_to_the_bound_host_and_cookies_go_to_arca() {
    let p = pki(&["api.example"]);
    let h: Handler = Arc::new(|req: &[u8]| {
        // An API that echoes what it got: the courier must mask it.
        let echoed = String::from_utf8_lossy(req).contains("s3cret-token");
        http("200 OK", &["Set-Cookie: sid=abc123; Path=/; HttpOnly", "X-Echo: s3cret-token"], if echoed { b"you sent s3cret-token" } else { b"no" })
    });
    let (at, seen) = serve(&p, "api.example", h);
    let net = Net { map: HashMap::from([("api.example".into(), at)]), lookups: Default::default() };
    let mut c = Courier::new(ep("api.example"), &net, tls(&p), LOOPBACK);
    let mut v = vault();

    let r = c.handle(&get_with_bearer("/v1/me").encode(), &mut v);
    let Response::Http { status, headers, body } = r else { panic!("{r:?}") };
    assert_eq!(status, 200);
    let sent = String::from_utf8(seen.lock().unwrap()[0].clone()).unwrap();
    assert!(sent.starts_with("GET /v1/me?a=1 HTTP/1.1\r\nHost: api.example\r\n"), "{sent}");
    assert!(sent.contains("\r\nAuthorization: Bearer s3cret-token\r\n"));
    assert!(sent.contains("\r\nAccept: application/json\r\n"));
    // The client sees neither the cookie nor the echo.
    assert!(headers.iter().all(|(k, _)| k != "set-cookie"));
    assert_eq!(headers.iter().find(|(k, _)| k == "x-echo").unwrap().1, "************");
    assert_eq!(body, b"you sent ************");
    assert_eq!(v.cookies, vec![("sid".into(), b"abc123".to_vec(), b"Path=/; HttpOnly".to_vec(), "/v1/me".into())]);
    // The connection is kept for the next request to the same endpoint.
    let _ = c.handle(&get_with_bearer("/v1/again").encode(), &mut v);
    assert_eq!(c.connects, 1);
}

#[test]
fn arcas_cookie_header_goes_after_tls_and_long_values_never_come_back() {
    let p = pki(&["api.example"]);
    let h: Handler = Arc::new(|req: &[u8]| {
        let echoed = String::from_utf8_lossy(req).contains("0123456789abcdef-session");
        http("200 OK", &[], if echoed { b"sid=0123456789abcdef-session; on=true" } else { b"no cookie" })
    });
    let (at, seen) = serve(&p, "api.example", h);
    let net = Net { map: HashMap::from([("api.example".into(), at)]), lookups: Default::default() };
    let mut c = Courier::new(ep("api.example"), &net, tls(&p), LOOPBACK);
    let mut v = vault();
    v.jar = Some(b"sid=0123456789abcdef-session; on=true".to_vec());
    let mut req = get_with_bearer("/inbox");
    req.slots.clear();
    req.ticket = Some([5; 16]);
    let r = c.handle(&req.encode(), &mut v);
    let Response::Http { body, .. } = r else { panic!("{r:?}") };
    let sent = String::from_utf8(seen.lock().unwrap()[0].clone()).unwrap();
    assert!(sent.contains("\r\nCookie: sid=0123456789abcdef-session; on=true\r\n"), "{sent}");
    assert_eq!(v.asked, vec!["/inbox ticket 5".to_string()], "the ticket goes to Arca with the path");
    // The session id is masked; a short value like `true` is left alone.
    assert_eq!(String::from_utf8(body).unwrap(), format!("sid={}; on=true", "*".repeat(24)));
    // A wrong-name certificate: Arca isn't asked for cookies at all.
    let p2 = pki(&["other.example"]);
    let (at2, _) = serve(&p2, "other.example", Arc::new(|_: &[u8]| http("200 OK", &[], b"")));
    let net2 = Net { map: HashMap::from([("api.example".into(), at2)]), lookups: Default::default() };
    let mut c2 = Courier::new(ep("api.example"), &net2, tls(&p2), LOOPBACK);
    let mut v2 = vault();
    v2.jar = Some(b"sid=x".to_vec());
    assert!(matches!(c2.handle(&req.encode(), &mut v2), Response::Refused(Refusal::Tls)));
    assert!(v2.asked.is_empty());
}

#[test]
fn a_redirect_to_another_endpoint_comes_back_and_carries_nothing() {
    let p = pki(&["mail.example", "evil.example"]);
    let (at, _) = serve(&p, "mail.example", Arc::new(|_: &[u8]| http("302 Found", &["Location: https://evil.example/steal"], b"")));
    let (evil, evil_seen) = serve(&p, "evil.example", Arc::new(|_: &[u8]| http("200 OK", &[], b"thanks")));
    let net = Net { map: HashMap::from([("mail.example".into(), at), ("evil.example".into(), evil)]), lookups: Default::default() };
    let mut c = Courier::new(ep("mail.example"), &net, tls(&p), LOOPBACK);
    let mut v = vault();
    let r = c.handle(&get_with_bearer("/login").encode(), &mut v);
    let Response::Http { status, headers, .. } = r else { panic!("{r:?}") };
    assert_eq!(status, 302);
    assert_eq!(headers.iter().find(|(k, _)| k == "location").unwrap().1, "https://evil.example/steal");
    assert!(evil_seen.lock().unwrap().is_empty(), "the courier never went to evil.example");
    assert_eq!(net.lookups.lock().unwrap().get("evil.example"), None, "not even a lookup");
    assert_eq!(v.fills.len(), 1);
}

#[test]
fn arca_is_asked_nothing_unless_tls_to_the_bound_host_succeeds() {
    let p = pki(&["mail.example", "other.example"]);
    // A server at mail.example's address holding only other.example's cert.
    let (at, seen) = serve(&p, "other.example", Arc::new(|_: &[u8]| http("200 OK", &[], b"")));
    let net = Net { map: HashMap::from([("mail.example".into(), at)]), lookups: Default::default() };
    let mut v = vault();
    let r = Courier::new(ep("mail.example"), &net, tls(&p), LOOPBACK).handle(&get_with_bearer("/").encode(), &mut v);
    assert_eq!(r, Response::Refused(Refusal::Tls));
    assert!(v.fills.is_empty() && seen.lock().unwrap().is_empty());
    // An untrusted CA: same.
    let other = pki(&["mail.example"]);
    let (at, _) = serve(&other, "mail.example", Arc::new(|_: &[u8]| http("200 OK", &[], b"")));
    let net = Net { map: HashMap::from([("mail.example".into(), at)]), lookups: Default::default() };
    let r = Courier::new(ep("mail.example"), &net, tls(&p), LOOPBACK).handle(&get_with_bearer("/").encode(), &mut v);
    assert_eq!(r, Response::Refused(Refusal::Tls));
    // Loopback without the grant: refused before connecting.
    let r = Courier::new(ep("mail.example"), &net, tls(&p), addr::Policy::default()).handle(&get_with_bearer("/").encode(), &mut v);
    assert_eq!(r, Response::Refused(Refusal::Connect));
    assert!(v.fills.is_empty());
}

#[test]
fn form_and_json_bodies_and_values_that_cant_go_in_a_header() {
    let p = pki(&["login.example"]);
    let (at, seen) = serve(&p, "login.example", Arc::new(|_: &[u8]| http("204 No Content", &[], b"")));
    let net = Net { map: HashMap::from([("login.example".into(), at)]), lookups: Default::default() };
    let mut c = Courier::new(ep("login.example"), &net, tls(&p), LOOPBACK);
    let mut v = vault();
    let form = Request {
        method: Method::Post,
        path: b"/session".to_vec(),
        query: vec![],
        headers: vec![(Header::ContentType, b"application/x-www-form-urlencoded".to_vec())],
        slots: vec![Slot { source: SOURCE_ITEM, reference: [8; 16], place: Place::Body }],
        body: vec![
            Segment::Literal(b"user=ana&pass=".to_vec()),
            Segment::Slot { slot: 0, encoding: Encoding::Form },
            Segment::Literal(b"&json={\"p\":\"".to_vec()),
            Segment::Slot { slot: 0, encoding: Encoding::JsonString },
            Segment::Literal(b"\"}".to_vec()),
        ],
        ticket: None,
    };
    assert!(matches!(c.handle(&form.encode(), &mut v), Response::Http { status: 204, .. }));
    let sent = String::from_utf8(seen.lock().unwrap()[0].clone()).unwrap();
    assert!(sent.ends_with("user=ana&pass=p%26ss+w%22rd&json={\"p\":\"p&ss w\\\"rd\"}"), "{sent}");
    assert!(sent.contains("Content-Length: 51\r\n"), "{sent}");
    // That value as a Bearer header: refused, nothing sent.
    let mut bad = get_with_bearer("/");
    bad.slots[0].reference = [8; 16];
    v.items.insert([8; 16], b"x\r\nEvil: 1".to_vec());
    assert_eq!(c.handle(&bad.encode(), &mut v), Response::Refused(Refusal::Unsendable));
    assert_eq!(seen.lock().unwrap().len(), 1);
}

/// The A1 gate: hostile client bytes never read an item. Every malformed
/// frame is refused before Arca or the network is touched; every frame
/// that decodes asks Arca for exactly the refs it names, nothing more.
#[test]
fn hostile_client_bytes_read_no_item() {
    let p = pki(&["api.example"]);
    let (at, _) = serve(&p, "api.example", Arc::new(|_: &[u8]| http("200 OK", &[], b"ok")));
    let net = Net { map: HashMap::from([("api.example".into(), at)]), lookups: Default::default() };
    let mut c = Courier::new(ep("api.example"), &net, tls(&p), LOOPBACK);
    let seeds = [get_with_bearer("/x").encode(), {
        let mut f = get_with_bearer("/y");
        f.method = Method::Post;
        f.slots[0].place = Place::Body;
        f.body = vec![Segment::Literal(b"a".to_vec()), Segment::Slot { slot: 0, encoding: Encoding::Raw }];
        f.encode()
    }];
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut rnd = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let (mut decoded, mut refused) = (0, 0);
    for i in 0..20_000 {
        let mut b = seeds[i % 2].clone();
        match rnd() % 4 {
            0 => {
                let n = (rnd() % 64) as usize;
                b = (0..n).map(|_| rnd() as u8).collect();
            }
            1 => {
                for _ in 0..1 + rnd() % 4 {
                    let j = (rnd() as usize) % b.len();
                    b[j] = rnd() as u8;
                }
            }
            2 => b.truncate((rnd() as usize) % b.len()),
            _ => {
                let j = (rnd() as usize) % b.len();
                b.insert(j, rnd() as u8);
            }
        }
        let mut v = vault();
        match Request::decode(&b) {
            Err(_) => {
                refused += 1;
                assert_eq!(c.handle(&b, &mut v), Response::Refused(Refusal::Malformed));
                assert!(v.fills.is_empty(), "a malformed frame reached Arca");
            }
            Ok(req) => {
                decoded += 1;
                // Round trip: the schema has one encoding per request.
                assert_eq!(req.encode(), b);
                if decoded <= 50 {
                    let _ = c.handle(&b, &mut v);
                    let names: Vec<[u8; 16]> = req.slots.iter().map(|s| s.reference).collect();
                    assert!(v.fills.iter().all(|f| *f == names));
                }
            }
        }
    }
    assert!(refused > 15_000 && decoded > 0, "{refused} refused, {decoded} decoded");
}
