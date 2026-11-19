//! The mail courier against scripted IMAP and SMTP servers over real TLS on
//! loopback (mail.md §5.2, M1): login only after TLS to the bound host, the
//! client greeted as a logged-in session without login capabilities, and
//! every way back to a login (or out of the filter's sight) refused, logged
//! and never seen by the server.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};

use arca_courier::mail::{encode_hello, session, Ended, MailEndpoint, MailVault, Secret};
use arca_courier::schema::Refusal;
use arca_store::secret::LockedBuf;
use arca_store::Scheme;
use navigator_fetch::{addr, Connect};

// ── PKI and TLS ──────────────────────────────────────────────────────

struct Pki {
    ca_pem: String,
    cert: (Vec<u8>, Vec<u8>),
}

fn pki(name: &str) -> Pki {
    let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let ca_cert = ca.self_signed(&ca_key).unwrap();
    let k = rcgen::KeyPair::generate().unwrap();
    let c = rcgen::CertificateParams::new(vec![name.to_string()]).unwrap().signed_by(&k, &ca_cert, &ca_key).unwrap();
    Pki { ca_pem: ca_cert.pem(), cert: (c.der().to_vec(), k.serialize_der()) }
}

fn server_tls(p: &Pki) -> Arc<rustls::ServerConfig> {
    use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    Arc::new(
        rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![CertificateDer::from(p.cert.0.clone())], PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(p.cert.1.clone())))
            .unwrap(),
    )
}

struct Net(SocketAddr);

impl Connect for Net {
    fn resolve(&self, _: &str, _: u16) -> std::io::Result<Vec<SocketAddr>> {
        Ok(vec![self.0])
    }
    fn connect_addr(&self, a: SocketAddr) -> std::io::Result<TcpStream> {
        TcpStream::connect(a)
    }
}

// ── the vault ────────────────────────────────────────────────────────

#[derive(Default)]
struct Vault {
    logins: usize,
    locked: bool,
    password: &'static str,
    anomalies: Arc<Mutex<Vec<(u8, String)>>>,
}

impl MailVault for Vault {
    fn login(&mut self, _: &[u8; 16]) -> Result<(String, Secret), Refusal> {
        self.logins += 1;
        if self.locked {
            return Err(Refusal::Locked);
        }
        Ok(("priya@example.org".into(), Secret::Password(LockedBuf::from_slice(self.password.as_bytes()).unwrap())))
    }
    fn refreshed(&mut self, _: &[u8; 16], _: &LockedBuf) -> Result<(), Refusal> {
        Err(Refusal::Denied)
    }
    fn anomaly(&mut self, code: u8, detail: &str) {
        self.anomalies.lock().unwrap().push((code, detail.into()));
    }
}

// ── scripted servers ─────────────────────────────────────────────────

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
    let mut acc = 0u32;
    let mut bits = 0;
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

const CREDS: &[u8] = b"\0priya@example.org\0hunter2";

/// An IMAP server. Everything it receives after login goes in `seen`.
fn imap_session(s: &mut dyn Stream, sasl_ir: bool, seen: &Mutex<Vec<String>>) {
    let mut authed = false;
    while let Some(l) = line(s) {
        if authed {
            seen.lock().unwrap().push(l.clone());
        }
        let mut w = l.splitn(3, ' ');
        let (tag, cmd, rest) = (w.next().unwrap_or(""), w.next().unwrap_or("").to_ascii_uppercase(), w.next().unwrap_or(""));
        match cmd.as_str() {
            "CAPABILITY" => {
                let ir = if sasl_ir { " SASL-IR" } else { "" };
                let caps = if authed { "IMAP4rev1 IDLE UIDPLUS AUTH=PLAIN COMPRESS=DEFLATE".to_string() } else { format!("IMAP4rev1 AUTH=PLAIN{ir} ID") };
                put(s, &format!("* CAPABILITY {caps}\r\n{tag} OK done\r\n"));
            }
            "AUTHENTICATE" => {
                let resp = match rest.split_once(' ') {
                    Some((_, ir)) => ir.to_string(),
                    None => {
                        put(s, "+ \r\n");
                        line(s).unwrap_or_default()
                    }
                };
                if unb64(&resp) == CREDS {
                    authed = true;
                    put(s, &format!("{tag} OK [CAPABILITY IMAP4rev1 IDLE UIDPLUS CONDSTORE AUTH=PLAIN SASL-IR COMPRESS=DEFLATE] Logged in\r\n"));
                } else {
                    put(s, &format!("{tag} NO [AUTHENTICATIONFAILED] no\r\n"));
                }
            }
            "APPEND" => {
                // `{n}` at the end: INBOX gets a go-ahead, anything else is refused.
                let n: usize = rest.rsplit_once('{').and_then(|(_, x)| x.trim_end_matches('}').parse().ok()).unwrap_or(0);
                if rest.starts_with("INBOX") {
                    put(s, "+ go ahead\r\n");
                    let mut lit = vec![0u8; n];
                    if s.read_exact(&mut lit).is_err() { return }
                    seen.lock().unwrap().push(format!("<literal {}>", String::from_utf8_lossy(&lit)));
                    let _ = line(s);
                    put(s, &format!("{tag} OK [APPENDUID 1 9] done\r\n"));
                } else {
                    put(s, &format!("{tag} NO [TRYCREATE] no such mailbox\r\n"));
                }
            }
            "LOGOUT" => {
                put(s, &format!("* BYE bye\r\n{tag} OK bye\r\n"));
                return;
            }
            "" => {}
            _ => put(s, &format!("{tag} OK {cmd} done\r\n")),
        }
    }
}

/// Serves IMAP on loopback: implicit TLS, or STARTTLS on a plain port.
fn imap_server(p: &Pki, starttls: bool, sasl_ir: bool) -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let cfg = server_tls(p);
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let at = l.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    std::thread::spawn(move || {
        for mut tcp in l.incoming().flatten() {
            let (cfg, seen) = (cfg.clone(), seen2.clone());
            std::thread::spawn(move || {
                if starttls {
                    put(&mut tcp, "* OK [CAPABILITY IMAP4rev1 STARTTLS LOGINDISABLED] plain\r\n");
                    let Some(l) = line(&mut tcp) else { return };
                    if !l.to_ascii_uppercase().ends_with("STARTTLS") { return }
                    put(&mut tcp, &format!("{} OK begin TLS\r\n", l.split(' ').next().unwrap()));
                    let mut t = rustls::StreamOwned::new(rustls::ServerConnection::new(cfg).unwrap(), tcp);
                    imap_session(&mut t, sasl_ir, &seen);
                } else {
                    let mut t = rustls::StreamOwned::new(rustls::ServerConnection::new(cfg).unwrap(), tcp);
                    put(&mut t, "* OK ready\r\n");
                    imap_session(&mut t, sasl_ir, &seen);
                }
            });
        }
    });
    (at, seen)
}

fn smtp_server(p: &Pki) -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let cfg = server_tls(p);
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let at = l.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    std::thread::spawn(move || {
        for tcp in l.incoming().flatten() {
            let (cfg, seen) = (cfg.clone(), seen2.clone());
            std::thread::spawn(move || {
                let mut t = rustls::StreamOwned::new(rustls::ServerConnection::new(cfg).unwrap(), tcp);
                let s: &mut dyn Stream = &mut t;
                put(s, "220 smtp.example.org ESMTP\r\n");
                let (mut authed, mut rcpts, mut data) = (false, 0, false);
                while let Some(l) = line(s) {
                    if authed {
                        seen.lock().unwrap().push(l.clone());
                    }
                    if data {
                        if l == "." {
                            data = false;
                            put(s, "250 2.0.0 queued\r\n");
                        }
                        continue;
                    }
                    let verb = l.split(' ').next().unwrap_or("").to_ascii_uppercase();
                    match verb.as_str() {
                        "EHLO" => put(s, "250-smtp.example.org\r\n250-PIPELINING\r\n250-SIZE 35882577\r\n250-AUTH PLAIN LOGIN\r\n250-CHUNKING\r\n250 SMTPUTF8\r\n"),
                        "AUTH" => {
                            if unb64(l.split(' ').nth(2).unwrap_or("")) == CREDS {
                                authed = true;
                                put(s, "235 2.7.0 ok\r\n");
                            } else {
                                put(s, "535 5.7.8 no\r\n");
                            }
                        }
                        "MAIL" => { rcpts = 0; put(s, "250 2.1.0 ok\r\n") }
                        "RCPT" => {
                            if l.contains("nobody@") { put(s, "550 5.1.1 no such user\r\n") } else { rcpts += 1; put(s, "250 2.1.5 ok\r\n") }
                        }
                        "DATA" => {
                            if rcpts == 0 { put(s, "554 5.5.1 no valid recipients\r\n") } else { data = true; put(s, "354 go\r\n") }
                        }
                        "QUIT" => { put(s, "221 bye\r\n"); return }
                        _ => put(s, "250 ok\r\n"),
                    }
                }
            });
        }
    });
    (at, seen)
}

/// A ManageSieve server (RFC 5804): STARTTLS on its plain port.
fn sieve_server(p: &Pki) -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let cfg = server_tls(p);
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let at = l.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    std::thread::spawn(move || {
        for mut tcp in l.incoming().flatten() {
            let (cfg, seen) = (cfg.clone(), seen2.clone());
            std::thread::spawn(move || {
                put(&mut tcp, "\"IMPLEMENTATION\" \"Test\"\r\n\"SASL\" \"\"\r\n\"STARTTLS\"\r\nOK \"ready\"\r\n");
                if line(&mut tcp).as_deref() != Some("STARTTLS") { return }
                put(&mut tcp, "OK \"begin TLS\"\r\n");
                let mut t = rustls::StreamOwned::new(rustls::ServerConnection::new(cfg).unwrap(), tcp);
                let s: &mut dyn Stream = &mut t;
                put(s, "\"IMPLEMENTATION\" \"Test\"\r\n\"SASL\" \"PLAIN\"\r\n\"SIEVE\" \"fileinto vacation\"\r\nOK\r\n");
                let mut authed = false;
                while let Some(l) = line(s) {
                    if authed {
                        seen.lock().unwrap().push(l.clone());
                    }
                    let verb = l.split(' ').next().unwrap_or("").to_ascii_uppercase();
                    match verb.as_str() {
                        "AUTHENTICATE" => {
                            let b = l.rsplit('"').nth(1).unwrap_or("");
                            if unb64(b) == CREDS { authed = true; put(s, "OK \"Logged in\"\r\n") } else { put(s, "NO \"no\"\r\n") }
                        }
                        "CAPABILITY" => put(s, "\"IMPLEMENTATION\" \"Test\"\r\n\"SASL\" \"PLAIN\"\r\n\"SIEVE\" \"fileinto vacation\"\r\n\"VERSION\" \"1.0\"\r\nOK\r\n"),
                        "PUTSCRIPT" => {
                            let n: usize = l.rsplit_once('{').and_then(|(_, x)| x.trim_end_matches("+}").parse().ok()).unwrap_or(0);
                            let mut lit = vec![0u8; n];
                            if s.read_exact(&mut lit).is_err() { return }
                            seen.lock().unwrap().push(format!("<script {}>", String::from_utf8_lossy(&lit)));
                            let _ = line(s);
                            put(s, "OK\r\n");
                        }
                        "LOGOUT" => { put(s, "OK \"bye\"\r\n"); return }
                        _ => put(s, "OK\r\n"),
                    }
                }
            });
        }
    });
    (at, seen)
}

// ── the client side ──────────────────────────────────────────────────

struct Run {
    c: UnixStream,
    done: std::thread::JoinHandle<(Ended, usize)>,
    anomalies: Arc<Mutex<Vec<(u8, String)>>>,
}

fn start(p: &Pki, scheme: Scheme, host: &str, at: SocketAddr, vault: Vault) -> Run {
    let tls = navigator_fetch::tls_config(p.ca_pem.as_bytes()).unwrap();
    let (c, courier_end) = UnixStream::pair().unwrap();
    let anomalies = vault.anomalies.clone();
    let ep = MailEndpoint { scheme, host: host.into(), port: at.port() };
    let done = std::thread::spawn(move || {
        let net = Net(at);
        let mut vault = vault;
        let out = courier_end.try_clone().unwrap();
        let e = session(&ep, &net, tls, addr::Policy { grant_private_network: true }, courier_end, out, &mut vault);
        (e, vault.logins)
    });
    let mut c2 = c.try_clone().unwrap();
    c2.write_all(&encode_hello(&[7; 16])).unwrap();
    c.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
    Run { c, done, anomalies }
}

impl Run {
    fn send(&mut self, t: &str) {
        self.c.write_all(t.as_bytes()).unwrap();
    }
    fn line(&mut self) -> String {
        line(&mut self.c).expect("a line from the courier")
    }
    /// Lines until one starts with `prefix`.
    fn until(&mut self, prefix: &str) -> Vec<String> {
        let mut v = Vec::new();
        loop {
            let l = self.line();
            let stop = l.starts_with(prefix);
            v.push(l);
            if stop {
                return v;
            }
        }
    }
    fn end(self) -> (Ended, usize) {
        drop(self.c);
        self.done.join().unwrap()
    }
}

fn vault() -> Vault {
    Vault { password: "hunter2", ..Default::default() }
}

// ── IMAP ─────────────────────────────────────────────────────────────

#[test]
fn imap_session_is_preauthenticated_and_login_after_it_ends_it() {
    let p = pki("imap.example.org");
    let (at, seen) = imap_server(&p, false, true);
    let mut r = start(&p, Scheme::Imaps, "imap.example.org", at, vault());
    let greeting = r.line();
    assert!(greeting.starts_with("* PREAUTH [CAPABILITY IMAP4rev1 IDLE UIDPLUS CONDSTORE]"), "{greeting}");
    assert!(!greeting.contains("AUTH=") && !greeting.contains("COMPRESS") && !greeting.contains("SASL-IR"));
    r.send("t1 NOOP\r\n");
    assert_eq!(r.until("t1 "), vec!["t1 OK NOOP done"]);
    // Lower case and an odd tag change nothing.
    r.send("x.9 login priya@example.org other\r\n");
    let anomalies = r.anomalies.clone();
    let (ended, logins) = r.end();
    assert_eq!(ended, Ended::Refused("LOGIN".into()));
    assert_eq!(logins, 1);
    assert_eq!(*anomalies.lock().unwrap(), vec![(1, "LOGIN".to_string())]);
    let seen = seen.lock().unwrap();
    assert!(seen.iter().any(|l| l == "t1 NOOP"));
    assert!(!seen.iter().any(|l| l.to_ascii_uppercase().contains("LOGIN")), "{seen:?}");
}

#[test]
fn imap_without_sasl_ir_and_over_starttls() {
    let p = pki("imap.example.org");
    let (at, seen) = imap_server(&p, true, false);
    let mut r = start(&p, Scheme::Imap, "imap.example.org", at, vault());
    let greeting = r.line();
    assert!(greeting.starts_with("* PREAUTH [CAPABILITY IMAP4rev1"), "{greeting}");
    assert!(!greeting.contains("STARTTLS") && !greeting.contains("LOGINDISABLED"));
    r.send("a STARTTLS\r\n");
    assert_eq!(r.end().0, Ended::Refused("STARTTLS".into()));
    assert!(seen.lock().unwrap().is_empty());
}

#[test]
fn a_literal_is_data_only_after_the_go_ahead() {
    let p = pki("imap.example.org");
    let (at, seen) = imap_server(&p, false, true);
    let mut r = start(&p, Scheme::Imaps, "imap.example.org", at, vault());
    r.line();
    // With a go-ahead: the literal's bytes are a message, whatever they say.
    let msg = "a1 LOGIN not a command";
    r.send(&format!("a APPEND INBOX {{{}}}\r\n", msg.len()));
    assert_eq!(r.line(), "+ go ahead");
    r.send(&format!("{msg}\r\n"));
    assert_eq!(r.until("a "), vec!["a OK [APPENDUID 1 9] done"]);
    // Non-synchronising: no wait, still data.
    r.send("b NOOP {5+}\r\nLOGIN\r\n");
    r.until("b ");
    // Refused instead of a go-ahead: the "literal" the client sends anyway
    // is commands again, and the LOGIN in it is caught.
    r.send("c APPEND Nope {23}\r\n");
    r.send("d LOGIN priya hunter3\r\n");
    assert_eq!(r.until("c "), vec!["c NO [TRYCREATE] no such mailbox"]);
    assert_eq!(r.end().0, Ended::Refused("LOGIN".into()));
    let seen = seen.lock().unwrap();
    assert!(seen.iter().any(|l| l == "<literal a1 LOGIN not a command>"), "{seen:?}");
    assert!(!seen.iter().any(|l| l.starts_with("d ")), "{seen:?}");
}

#[test]
fn wrong_password_locked_arca_and_wrong_certificate() {
    let p = pki("imap.example.org");
    let (at, _) = imap_server(&p, false, true);
    let mut r = start(&p, Scheme::Imaps, "imap.example.org", at, Vault { password: "wrong", ..Default::default() });
    assert!(r.line().starts_with("* BYE [AUTHENTICATIONFAILED]"));
    assert_eq!(r.end(), (Ended::LoginFailed, 1));

    let mut r = start(&p, Scheme::Imaps, "imap.example.org", at, Vault { locked: true, ..vault() });
    assert!(r.line().starts_with("* BYE [UNAVAILABLE] Arca is locked"));
    assert_eq!(r.end(), (Ended::NotLoggedIn(Refusal::Locked), 1));

    // The certificate is for another name: TLS fails and Arca is never asked.
    let mut r = start(&p, Scheme::Imaps, "imap.other.example", at, vault());
    assert!(r.line().starts_with("* BYE [UNAVAILABLE]"));
    assert_eq!(r.end(), (Ended::NotLoggedIn(Refusal::Tls), 0));
}

#[test]
fn idle_and_done_pass() {
    let p = pki("imap.example.org");
    let (at, seen) = imap_server(&p, false, true);
    let mut r = start(&p, Scheme::Imaps, "imap.example.org", at, vault());
    r.line();
    r.send("i IDLE\r\n");
    r.until("i ");
    r.send("DONE\r\nz LOGOUT\r\n");
    r.until("z ");
    assert_eq!(r.end().0, Ended::Closed);
    assert!(seen.lock().unwrap().iter().any(|l| l == "DONE"));
}

// ── SMTP ─────────────────────────────────────────────────────────────

#[test]
fn smtp_greets_with_the_post_login_ehlo_and_sends_mail() {
    let p = pki("smtp.example.org");
    let (at, seen) = smtp_server(&p);
    let mut r = start(&p, Scheme::Submissions, "smtp.example.org", at, vault());
    let ehlo = r.until("250 ");
    assert_eq!(ehlo, vec!["250-smtp.example.org", "250-PIPELINING", "250-SIZE 35882577", "250 SMTPUTF8"]);
    // Pipelined envelope, then a message whose text says AUTH.
    r.send("MAIL FROM:<priya@example.org>\r\nRCPT TO:<a@example.net>\r\nDATA\r\n");
    assert_eq!(r.line(), "250 2.1.0 ok");
    assert_eq!(r.line(), "250 2.1.5 ok");
    assert_eq!(r.line(), "354 go");
    r.send("Subject: hi\r\n\r\nAUTH PLAIN AHgAeQ==\r\n.\r\n");
    assert_eq!(r.line(), "250 2.0.0 queued");
    // Back in command mode: AUTH is refused.
    r.send("AUTH PLAIN AHgAeQ==\r\n");
    let anomalies = r.anomalies.clone();
    assert_eq!(r.end().0, Ended::Refused("AUTH".into()));
    assert_eq!(*anomalies.lock().unwrap(), vec![(1, "AUTH".to_string())]);
    let seen = seen.lock().unwrap();
    assert!(seen.iter().any(|l| l == "AUTH PLAIN AHgAeQ=="), "the message text reached the server: {seen:?}");
    assert_eq!(seen.iter().filter(|l| l.starts_with("AUTH")).count(), 1, "{seen:?}");
}

#[test]
fn smtp_data_refused_means_no_message_text() {
    let p = pki("smtp.example.org");
    let (at, seen) = smtp_server(&p);
    let mut r = start(&p, Scheme::Submissions, "smtp.example.org", at, vault());
    r.until("250 ");
    // Every recipient refused, so DATA is refused: what follows is commands.
    r.send("MAIL FROM:<priya@example.org>\r\nRCPT TO:<nobody@example.net>\r\nDATA\r\nAUTH PLAIN AHgAeQ==\r\n");
    assert_eq!(r.line(), "250 2.1.0 ok");
    assert_eq!(r.line(), "550 5.1.1 no such user");
    assert_eq!(r.line(), "554 5.5.1 no valid recipients");
    assert_eq!(r.end().0, Ended::Refused("AUTH".into()));
    assert!(!seen.lock().unwrap().iter().any(|l| l.starts_with("AUTH")));
}

#[test]
fn smtp_chunking_is_refused() {
    let p = pki("smtp.example.org");
    let (at, _) = smtp_server(&p);
    let mut r = start(&p, Scheme::Submissions, "smtp.example.org", at, vault());
    r.until("250 ");
    r.send("BDAT 10 LAST\r\n");
    assert_eq!(r.end().0, Ended::Refused("BDAT".into()));
}

// ── ManageSieve ──────────────────────────────────────────────────────

#[test]
fn sieve_session_is_logged_in_after_starttls_and_scripts_pass() {
    let p = pki("mail.example.org");
    let (at, seen) = sieve_server(&p);
    let mut r = start(&p, Scheme::Sieve, "mail.example.org", at, vault());
    let hello = r.until("OK");
    assert_eq!(hello, vec!["\"IMPLEMENTATION\" \"Test\"", "\"SIEVE\" \"fileinto vacation\"", "\"VERSION\" \"1.0\"", "OK \"Logged in by Arca\""]);
    let script = "require [\"vacation\"];\r\nvacation \"Away.\";\r\n";
    r.send(&format!("PUTSCRIPT \"atrium-mail\" {{{}+}}\r\n{script}\r\n", script.len()));
    assert_eq!(r.line(), "OK");
    r.send("SETACTIVE \"atrium-mail\"\r\n");
    assert_eq!(r.line(), "OK");
    // Signing in again ends it, logged, and the server never sees it.
    r.send("AUTHENTICATE \"PLAIN\" \"AHguAHk=\"\r\n");
    let anomalies = r.anomalies.clone();
    let (e, logins) = r.end();
    assert_eq!(e, Ended::Refused("AUTHENTICATE".into()));
    assert_eq!(logins, 1);
    assert_eq!(anomalies.lock().unwrap().len(), 1);
    let seen = seen.lock().unwrap().clone();
    assert!(seen.contains(&format!("<script {script}>")), "{seen:?}");
    assert!(!seen.iter().any(|l| l.starts_with("AUTHENTICATE")), "{seen:?}");
}

#[test]
fn sieve_wrong_password_and_no_starttls() {
    let p = pki("mail.example.org");
    let (at, _) = sieve_server(&p);
    let mut r = start(&p, Scheme::Sieve, "mail.example.org", at, Vault { password: "wrong", ..Default::default() });
    assert_eq!(r.line(), "BYE \"The server refused the sign-in\"");
    assert_eq!(r.end().0, Ended::LoginFailed);
    // A server that doesn't offer STARTTLS: never signed in, Arca not asked.
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let at = l.local_addr().unwrap();
    std::thread::spawn(move || {
        if let Some(Ok(mut tcp)) = l.incoming().next() {
            put(&mut tcp, "\"IMPLEMENTATION\" \"Plain\"\r\n\"SASL\" \"PLAIN\"\r\nOK\r\n");
            let _ = line(&mut tcp);
        }
    });
    let mut r = start(&p, Scheme::Sieve, "mail.example.org", at, vault());
    assert_eq!(r.line(), "BYE \"Cannot reach the mail server securely\"");
    assert_eq!(r.end(), (Ended::NotLoggedIn(Refusal::Tls), 0));
}
