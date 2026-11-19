//! A TLS server for the jailed courier's VM gate, run on the machine that
//! runs the VM (the guest reaches it as 10.0.2.2 through slirp):
//!
//!   courier-test-server HOSTS CA_OUT PORT_OUT LOG
//!
//! HOSTS (comma separated) are the certificate's names, signed by a
//! throwaway CA written to CA_OUT. The port goes to PORT_OUT; each request
//! head, one line per request (`<request line> token=<bool> cookie=<value|->`),
//! goes to LOG.
//! A fifth argument is the listen address for the Mail sign-in fixture
//! (use 127.0.0.1:0 on the host). In this mode the log contains stage
//! markers only. The fixture checks PKCE request fields and a redirect cookie.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (hosts, ca_out, port_out, log) = (&a[1], &a[2], &a[3], a[4].clone());
    let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca.distinguished_name.push(rcgen::DnType::CommonName, "Atrium courier test CA");
    ca.key_usages.push(rcgen::KeyUsagePurpose::KeyCertSign);
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let ca_cert = ca.self_signed(&ca_key).unwrap();
    std::fs::write(ca_out, ca_cert.pem()).unwrap();
    let k = rcgen::KeyPair::generate().unwrap();
    let names: Vec<String> = hosts.split(',').map(String::from).collect();
    let mut leaf = rcgen::CertificateParams::new(names).unwrap();
    leaf.use_authority_key_identifier_extension = true;
    let c = leaf.signed_by(&k, &ca_cert, &ca_key).unwrap();
    use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    let cfg = Arc::new(
        rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![CertificateDer::from(c.der().to_vec())], PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(k.serialize_der())))
            .unwrap(),
    );
    // Optional guest-local address for the Mail button gate. The only signing
    // authority in that courier jail is this throwaway fixture CA.
    let mail_address = a.get(5);
    let mail = mail_address.is_some();
    let inbox = a.get(6).is_some_and(|mode| mode == "inbox");
    let l = TcpListener::bind(mail_address.map(String::as_str).unwrap_or("127.0.0.1:0")).unwrap();
    std::fs::write(port_out, l.local_addr().unwrap().port().to_string()).unwrap();
    for s in l.incoming().flatten() {
        let (cfg, log) = (cfg.clone(), log.clone());
        std::thread::spawn(move || {
            let Ok(conn) = rustls::ServerConnection::new(cfg) else { return };
            let mut t = rustls::StreamOwned::new(conn, s);
            loop {
                let mut b = Vec::new();
                let mut one = [0u8; 1];
                while !b.ends_with(b"\r\n\r\n") {
                    if t.read(&mut one).unwrap_or(0) == 0 {
                        return;
                    }
                    b.push(one[0]);
                }
                let req = String::from_utf8_lossy(&b).to_string();
                let token = req.contains("Authorization: Bearer hunter2-token\r\n");
                let first = req.lines().next().unwrap_or("").to_string();
                let cookie = req.lines().find_map(|l| l.strip_prefix("Cookie: ")).unwrap_or("-").to_string();
                if inbox {
                    let headers = "Set-Cookie: inbox_session=local; Secure; HttpOnly; SameSite=Strict; Path=/\r\n";
                    let (status, mime, extra, body, marker) = match first.split_whitespace().collect::<Vec<_>>().as_slice() {
                        ["GET", "/inbox", _] => (200, "text/html", headers, include_str!("../../../test-assets/navigator-i0/inbox/index.html"), "document"),
                        ["GET", "/inbox.js", _] => (200, "text/javascript", "", include_str!("../../../test-assets/navigator-i0/inbox/inbox.js"), "script"),
                        ["GET", "/inbox.css", _] => (200, "text/css", "", include_str!("../../../test-assets/navigator-i0/inbox/inbox.css"), "stylesheet"),
                        ["GET", "/api/inbox", _] if cookie.split("; ").any(|s| s == "inbox_session=local") =>
                            (200, "application/json", "", r#"{"subject":"School field trip"}"#, "authenticated inbox"),
                        _ => (403, "text/plain", "", "Forbidden", "refused"),
                    };
                    let mut file = std::fs::OpenOptions::new().append(true).create(true).open(&log).unwrap();
                    writeln!(file, "{marker}").unwrap();
                    let reply = format!("HTTP/1.1 {status} {}\r\nContent-Type: {mime}\r\n{extra}Content-Length: {}\r\n\r\n{body}", if status == 200 { "OK" } else { "Forbidden" }, body.len());
                    if t.write_all(reply.as_bytes()).and_then(|_| t.flush()).is_err() { break; }
                    continue;
                }
                if mail {
                    let target = first.strip_prefix("GET ").and_then(|s| s.split_whitespace().next()).unwrap_or("");
                    let authorization = target.strip_prefix("/common/oauth2/v2.0/authorize?").is_some_and(|q| {
                        let has = |key: &str, value: &str| q.split('&').any(|kv| kv.split_once('=') == Some((key, value)));
                        has("client_id", "11111111-1111-4111-8111-111111111111") && has("code_challenge_method", "S256")
                            && q.split('&').any(|kv| kv.strip_prefix("code_challenge=").is_some_and(|v| v.len() == 43))
                            && q.split('&').any(|kv| kv.strip_prefix("state=").is_some_and(|v| v.len() == 22))
                    });
                    let (marker, reply) = if authorization {
                        ("authorized", "HTTP/1.1 302 Found\r\nSet-Cookie: flow=dev; Secure; HttpOnly; Path=/\r\nLocation: /first\r\nContent-Length: 0\r\n\r\n".to_string())
                    } else if target == "/first" && cookie.split("; ").any(|v| v == "flow=dev") {
                        let html = "<!doctype html><label id=prompt for=account></label><input id=account><script>document.getElementById('prompt').textContent='Enter your account'</script>";
                        ("first-screen-cookie-ok", format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\r\n{html}", html.len()))
                    } else {
                        ("refused", "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n".to_string())
                    };
                    let mut log = std::fs::OpenOptions::new().append(true).create(true).open(&log).unwrap();
                    writeln!(log, "{marker}").unwrap();
                    if t.write_all(reply.as_bytes()).and_then(|_| t.flush()).is_err() { break; }
                    continue;
                }
                if let Ok(mut f) = std::fs::OpenOptions::new().append(true).create(true).open(&log) {
                    let _ = writeln!(f, "{first} token={token} cookie={cookie}");
                }
                let resp = if req.starts_with("GET /redirect") {
                    "HTTP/1.1 302 Found\r\nLocation: https://evil.example/\r\nContent-Length: 0\r\n\r\n".to_string()
                } else {
                    let echo = if token { "echo hunter2-token" } else { "no token" };
                    format!("HTTP/1.1 200 OK\r\nSet-Cookie: sid=vm-cookie; Path=/; Secure; HttpOnly\r\nContent-Length: {}\r\n\r\n{echo}", echo.len())
                };
                if t.write_all(resp.as_bytes()).is_err() {
                    return;
                }
            }
        });
    }
}
