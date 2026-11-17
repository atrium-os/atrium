//! OAuth mail logins (mail.md §5.3) against a token endpoint and IMAP and
//! SMTP servers over real TLS on loopback: the courier trades the refresh
//! token for an access token, hands a rotated refresh token back to Arca,
//! logs in with XOAUTH2, and a refresh token the provider no longer takes
//! ends with "sign in again" and no login attempt.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};

use arca_courier::mail::{encode_hello, session_with, Ended, MailEndpoint, MailVault, Secret};
use arca_courier::oauth::{self, Provider};
use arca_courier::schema::Refusal;
use arca_store::secret::LockedBuf;
use arca_store::Scheme;
use navigator_fetch::{addr, Connect};

const IMAP_HOST: &str = "imap.contoso.test";
const SMTP_HOST: &str = "smtp.contoso.test";
const TOKEN_HOST: &str = "login.contoso.test";

const TEST_PROVIDER: Provider = Provider {
    name: "testprovider",
    authorize: "https://login.contoso.test/authorize",
    token_host: TOKEN_HOST,
    token_path: "/oauth2/token",
    scopes: "offline_access mail",
    redirect: "https://login.contoso.test/nativeclient",
    imap: (IMAP_HOST, 993),
    smtp: (SMTP_HOST, 587),
};

struct Pki {
    ca_pem: String,
    cfg: Arc<rustls::ServerConfig>,
}

fn pki() -> Pki {
    use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let ca_cert = ca.self_signed(&ca_key).unwrap();
    let k = rcgen::KeyPair::generate().unwrap();
    let c = rcgen::CertificateParams::new(vec![IMAP_HOST.to_string(), SMTP_HOST.to_string(), TOKEN_HOST.to_string()]).unwrap().signed_by(&k, &ca_cert, &ca_key).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![CertificateDer::from(c.der().to_vec())], PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(k.serialize_der())))
        .unwrap();
    Pki { ca_pem: ca_cert.pem(), cfg: Arc::new(cfg) }
}

/// Each host name to its loopback server.
struct Net(Vec<(&'static str, SocketAddr)>);

impl Connect for Net {
    fn resolve(&self, host: &str, _: u16) -> std::io::Result<Vec<SocketAddr>> {
        Ok(self.0.iter().filter(|(h, _)| *h == host).map(|(_, a)| *a).collect())
    }
    fn connect_addr(&self, a: SocketAddr) -> std::io::Result<TcpStream> {
        TcpStream::connect(a)
    }
}

trait Stream: Read + Write {}
impl<T: Read + Write> Stream for T {}

fn line(s: &mut dyn Stream) -> Option<String> {
    let mut b = Vec::new();
    let mut one = [0u8; 1];
    while !b.ends_with(b"\r\n") {
        if s.read(&mut one).ok()? == 0 {
            return None;
        }
        b.push(one[0]);
    }
    Some(String::from_utf8_lossy(&b[..b.len() - 2]).into_owned())
}

fn put(s: &mut dyn Stream, t: &str) {
    let _ = s.write_all(t.as_bytes()).and_then(|_| s.flush());
}

fn unb64(s: &str) -> Vec<u8> {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes().filter(|c| *c != b'=') {
        let Some(v) = T.iter().position(|t| *t == c) else { continue };
        acc = acc << 6 | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

/// The token endpoint: answers refresh token `R1` with access token `A1`
/// and a rotated `R2`; anything else with `invalid_grant`. Requests seen
/// go in the log.
fn token_server(p: &Pki, log: Arc<Mutex<Vec<String>>>) -> SocketAddr {
    let cfg = p.cfg.clone();
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let at = l.local_addr().unwrap();
    std::thread::spawn(move || {
        for tcp in l.incoming().flatten() {
            let mut t = rustls::StreamOwned::new(rustls::ServerConnection::new(cfg.clone()).unwrap(), tcp);
            let mut head = Vec::new();
            let mut one = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                if t.read(&mut one).unwrap_or(0) == 0 {
                    break;
                }
                head.push(one[0]);
            }
            let h = String::from_utf8_lossy(&head).to_string();
            let n: usize = h.lines().find_map(|l| l.strip_prefix("Content-Length: ")).and_then(|v| v.trim().parse().ok()).unwrap_or(0);
            let mut body = vec![0u8; n];
            t.read_exact(&mut body).unwrap();
            let body = String::from_utf8(body).unwrap();
            log.lock().unwrap().push(format!("{}\n{}", h.lines().next().unwrap(), body));
            let (status, json) = if body.contains("&refresh_token=R1") && body.starts_with("client_id=cid-1&grant_type=refresh_token&") {
                ("200 OK", r#"{"token_type":"Bearer","expires_in":3600,"access_token":"A1","refresh_token":"R2"}"#)
            } else {
                ("400 Bad Request", r#"{"error":"invalid_grant","error_description":"AADSTS70000"}"#)
            };
            // Chunked, as Microsoft's answers are.
            let reply = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{json}\r\n0\r\n\r\n", json.len());
            let _ = t.write_all(reply.as_bytes());
            t.conn.send_close_notify();
            let _ = t.flush();
        }
    });
    at
}

fn imap_server(p: &Pki, seen: Arc<Mutex<Vec<String>>>) -> SocketAddr {
    let cfg = p.cfg.clone();
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let at = l.local_addr().unwrap();
    std::thread::spawn(move || {
        for tcp in l.incoming().flatten() {
            let (cfg, seen) = (cfg.clone(), seen.clone());
            std::thread::spawn(move || {
                let mut s = rustls::StreamOwned::new(rustls::ServerConnection::new(cfg).unwrap(), tcp);
                put(&mut s, "* OK [CAPABILITY IMAP4rev1 SASL-IR AUTH=PLAIN AUTH=XOAUTH2] ready\r\n");
                while let Some(l) = line(&mut s) {
                    seen.lock().unwrap().push(l.clone());
                    let mut w = l.splitn(4, ' ');
                    let (tag, cmd, mech, ir) = (w.next().unwrap_or(""), w.next().unwrap_or(""), w.next().unwrap_or(""), w.next().unwrap_or(""));
                    match (cmd, mech) {
                        ("AUTHENTICATE", "XOAUTH2") if unb64(ir) == b"user=ada@contoso.test\x01auth=Bearer A1\x01\x01" => {
                            put(&mut s, &format!("{tag} OK [CAPABILITY IMAP4rev1 IDLE] Logged in\r\n"));
                        }
                        ("AUTHENTICATE", _) => {
                            // XOAUTH2's failure: details as a challenge, then NO.
                            put(&mut s, "+ eyJzdGF0dXMiOiI0MDEifQ==\r\n");
                            let empty = line(&mut s).unwrap_or_default();
                            seen.lock().unwrap().push(format!("<answer {empty:?}>"));
                            put(&mut s, &format!("{tag} NO [AUTHENTICATIONFAILED] no\r\n"));
                        }
                        ("LOGOUT", _) => {
                            put(&mut s, &format!("* BYE\r\n{tag} OK bye\r\n"));
                            return;
                        }
                        _ => put(&mut s, &format!("{tag} OK done\r\n")),
                    }
                }
            });
        }
    });
    at
}

fn smtp_server(p: &Pki, seen: Arc<Mutex<Vec<String>>>) -> SocketAddr {
    let cfg = p.cfg.clone();
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let at = l.local_addr().unwrap();
    std::thread::spawn(move || {
        for mut tcp in l.incoming().flatten() {
            let (cfg, seen) = (cfg.clone(), seen.clone());
            std::thread::spawn(move || {
                put(&mut tcp, "220 ready\r\n");
                let _ = line(&mut tcp);
                put(&mut tcp, "250-smtp\r\n250 STARTTLS\r\n");
                let _ = line(&mut tcp);
                put(&mut tcp, "220 go\r\n");
                let mut s = rustls::StreamOwned::new(rustls::ServerConnection::new(cfg).unwrap(), tcp);
                while let Some(l) = line(&mut s) {
                    seen.lock().unwrap().push(l.clone());
                    if l.starts_with("EHLO") {
                        put(&mut s, "250-smtp\r\n250-AUTH LOGIN PLAIN XOAUTH2\r\n250 SIZE 1000\r\n");
                    } else if let Some(ir) = l.strip_prefix("AUTH XOAUTH2 ") {
                        if unb64(ir) == b"user=ada@contoso.test\x01auth=Bearer A1\x01\x01" {
                            put(&mut s, "235 2.7.0 ok\r\n");
                        } else {
                            put(&mut s, "334 eyJzdGF0dXMiOiI0MDEifQ==\r\n");
                            let _ = line(&mut s);
                            put(&mut s, "535 5.7.3 no\r\n");
                        }
                    } else if l == "QUIT" {
                        put(&mut s, "221 bye\r\n");
                        return;
                    } else {
                        put(&mut s, "250 ok\r\n");
                    }
                }
            });
        }
    });
    at
}

/// Arca: one OAuth item; rotations recorded.
struct Vault {
    grant: Arc<Mutex<Vec<u8>>>,
}

impl MailVault for Vault {
    fn login(&mut self, _: &[u8; 16]) -> Result<(String, Secret), Refusal> {
        Ok(("ada@contoso.test".into(), Secret::OAuth(LockedBuf::from_slice(&self.grant.lock().unwrap()).unwrap())))
    }
    fn refreshed(&mut self, _: &[u8; 16], grant: &LockedBuf) -> Result<(), Refusal> {
        *self.grant.lock().unwrap() = grant.as_ref().to_vec();
        Ok(())
    }
    fn anomaly(&mut self, _: u8, _: &str) {}
}

fn run(p: &Pki, net: Net, scheme: Scheme, host: &str, port: u16, grant: Arc<Mutex<Vec<u8>>>) -> (UnixStream, std::thread::JoinHandle<Ended>) {
    let tls = navigator_fetch::tls_config(p.ca_pem.as_bytes()).unwrap();
    let (c, courier_end) = UnixStream::pair().unwrap();
    let ep = MailEndpoint { scheme, host: host.into(), port };
    let done = std::thread::spawn(move || {
        let mut vault = Vault { grant };
        let out = courier_end.try_clone().unwrap();
        session_with(&ep, &net, tls, addr::Policy { grant_private_network: true }, &[TEST_PROVIDER], courier_end, out, &mut vault)
    });
    let mut c2 = c.try_clone().unwrap();
    c2.write_all(&encode_hello(&[7; 16])).unwrap();
    c.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
    (c, done)
}

#[test]
fn imap_and_smtp_over_xoauth2_with_a_rotated_refresh_token() {
    let p = pki();
    let tokens = Arc::new(Mutex::new(Vec::new()));
    let imap_seen = Arc::new(Mutex::new(Vec::new()));
    let smtp_seen = Arc::new(Mutex::new(Vec::new()));
    let tok = token_server(&p, tokens.clone());
    let imap = imap_server(&p, imap_seen.clone());
    let smtp = smtp_server(&p, smtp_seen.clone());
    let net = || Net(vec![(TOKEN_HOST, tok), (IMAP_HOST, imap), (SMTP_HOST, smtp)]);

    let grant = Arc::new(Mutex::new(oauth::grant("testprovider", "cid-1", b"R1").unwrap().as_ref().to_vec()));
    let (mut c, done) = run(&p, net(), Scheme::Imaps, IMAP_HOST, imap.port(), grant.clone());
    let greeting = line(&mut c).unwrap();
    assert!(greeting.starts_with("* PREAUTH [CAPABILITY IMAP4rev1 IDLE]"), "{greeting}");
    put(&mut c, "a1 LOGOUT\r\n");
    while let Some(l) = line(&mut c) {
        if l.starts_with("a1 ") {
            break;
        }
    }
    drop(c);
    assert_eq!(done.join().unwrap(), Ended::Closed);
    // The token endpoint got a refresh grant; Arca got the rotated token.
    let t = tokens.lock().unwrap().clone();
    assert_eq!(t.len(), 1);
    assert!(t[0].starts_with("POST /oauth2/token HTTP/1.1\n"), "{}", t[0]);
    assert!(t[0].contains("&scope=offline_access%20mail&refresh_token=R1"), "{}", t[0]);
    assert_eq!(grant.lock().unwrap().as_slice(), b"oauth1 testprovider cid-1 R2");
    assert!(imap_seen.lock().unwrap()[0].starts_with("A2 AUTHENTICATE XOAUTH2 "));

    // SMTP over STARTTLS, with the grant as Arca now has it: R2 isn't
    // taken by this provider, so: sign in again, and no AUTH sent.
    let (mut c, done) = run(&p, net(), Scheme::Submission, SMTP_HOST, smtp.port(), grant.clone());
    assert_eq!(line(&mut c).unwrap(), "535 5.7.8 Sign in again");
    drop(c);
    assert_eq!(done.join().unwrap(), Ended::NotLoggedIn(Refusal::SignInAgain));
    assert!(!smtp_seen.lock().unwrap().iter().any(|l| l.starts_with("AUTH")));

    // With R1 again, SMTP logs in.
    *grant.lock().unwrap() = oauth::grant("testprovider", "cid-1", b"R1").unwrap().as_ref().to_vec();
    let (mut c, done) = run(&p, net(), Scheme::Submission, SMTP_HOST, smtp.port(), grant.clone());
    let mut reply = Vec::new();
    loop {
        let l = line(&mut c).unwrap();
        let last = l.as_bytes().get(3) == Some(&b' ');
        reply.push(l);
        if last {
            break;
        }
    }
    assert_eq!(reply.first().map(String::as_str), Some("250-smtp"), "{reply:?}");
    assert!(!reply.iter().any(|l| l.contains("AUTH")), "{reply:?}");
    drop(c);
    let _ = done.join();
    assert!(smtp_seen.lock().unwrap().iter().any(|l| l.starts_with("AUTH XOAUTH2 ")));

    // A grant naming a provider not in the list is refused before any request.
    *grant.lock().unwrap() = oauth::grant("elsewhere", "cid-1", b"R1").unwrap().as_ref().to_vec();
    let before = tokens.lock().unwrap().len();
    let (mut c, done) = run(&p, net(), Scheme::Imaps, IMAP_HOST, imap.port(), grant.clone());
    assert!(line(&mut c).unwrap().starts_with("* BYE [AUTHENTICATIONFAILED]"));
    drop(c);
    assert_eq!(done.join().unwrap(), Ended::NotLoggedIn(Refusal::Denied));
    assert_eq!(tokens.lock().unwrap().len(), before);
}
