//! The courier's VM gate (scripts/arca-vm-test.sh §4): runs a TLS server
//! in process for `HOST` (its own throwaway CA, written to `CA_OUT`), then
//! runs the courier command `CMD` (under its courier uid, via su) with
//! ARCA_COURIER_CONNECT pointing at that server, and sends it requests.
//!
//!   courier-vm-probe HOST CA_OUT ITEM_HEX CMD
//!
//! `CMD` gets ARCA_COURIER_CONNECT appended as `env` arguments' first word,
//! so pass something like `env ARCA_COURIER_CA=... su -m user -c` and the
//! probe appends `'env ARCA_COURIER_CONNECT=127.0.0.1:N arca-courier'`.
//! Prints one `key=value` line per check.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use arca_courier::schema::{Header, Method, Place, Request, Response, Slot};
use arcad::proto::courier_op::SOURCE_ITEM;

fn unhex(s: &str) -> [u8; 16] {
    let mut o = [0u8; 16];
    for i in 0..16 {
        o[i] = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap();
    }
    o
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (host, ca_out, item, cmd) = (&a[1], &a[2], unhex(&a[3]), &a[4]);

    let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let ca_cert = ca.self_signed(&ca_key).unwrap();
    std::fs::write(ca_out, ca_cert.pem()).unwrap();
    let k = rcgen::KeyPair::generate().unwrap();
    let c = rcgen::CertificateParams::new(vec![host.clone()]).unwrap().signed_by(&k, &ca_cert, &ca_key).unwrap();

    use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    let cfg = Arc::new(
        rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![CertificateDer::from(c.der().to_vec())], PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(k.serialize_der())))
            .unwrap(),
    );
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let seen: Arc<Mutex<Vec<String>>> = Default::default();
    let seen2 = seen.clone();
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let (cfg, seen) = (cfg.clone(), seen2.clone());
            std::thread::spawn(move || {
                let mut t = rustls::StreamOwned::new(rustls::ServerConnection::new(cfg).unwrap(), s);
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
                    seen.lock().unwrap().push(req.clone());
                    let resp = if req.starts_with("GET /redirect") {
                        "HTTP/1.1 302 Found\r\nLocation: https://evil.example/\r\nContent-Length: 0\r\n\r\n".to_string()
                    } else {
                        let echo = if req.contains("hunter2-token") { "echo hunter2-token" } else { "no token" };
                        format!("HTTP/1.1 200 OK\r\nSet-Cookie: sid=vm-cookie; Path=/; Secure; HttpOnly\r\nContent-Length: {}\r\n\r\n{echo}", echo.len())
                    };
                    if t.write_all(resp.as_bytes()).is_err() {
                        return;
                    }
                }
            });
        }
    });

    let full = format!("{cmd} 'env ARCA_COURIER_CONNECT=127.0.0.1:{port} /usr/local/bin/arca-courier'");
    let mut child = Command::new("sh").arg("-c").arg(&full).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let (mut tx, mut rx) = (child.stdin.take().unwrap(), child.stdout.take().unwrap());
    let mut ask = |path: &str| -> Response {
        let r = Request {
            method: Method::Get,
            path: path.as_bytes().to_vec(),
            query: vec![],
            headers: vec![(Header::Accept, b"*/*".to_vec())],
            slots: vec![Slot { source: SOURCE_ITEM, reference: item, place: Place::Bearer }],
            body: vec![],
            ticket: None,
        };
        arca_courier::send::write_frame(&mut tx, &r.encode()).unwrap();
        let f = arca_courier::send::read_frame(&mut rx).unwrap().unwrap_or_default();
        Response::decode(&f).unwrap_or(Response::Refused(arca_courier::schema::Refusal::Internal))
    };

    match ask("/me") {
        Response::Http { status, headers, body } => {
            println!("status={status}");
            println!("cookie_hidden={}", !headers.iter().any(|(k, _)| k == "set-cookie"));
            println!("body={}", String::from_utf8_lossy(&body));
        }
        Response::Refused(r) => println!("refused={r:?}"),
    }
    let got = seen.lock().unwrap().first().cloned().unwrap_or_default();
    println!("server_got_token={}", got.contains("Authorization: Bearer hunter2-token\r\n"));
    match ask("/redirect") {
        Response::Http { status, headers, .. } => {
            let loc = headers.iter().find(|(k, _)| k == "location").map(|h| h.1.clone()).unwrap_or_default();
            println!("redirect={status} {loc}");
        }
        Response::Refused(r) => println!("redirect_refused={r:?}"),
    }
    println!("requests_at_server={}", seen.lock().unwrap().len());
    drop(tx);
    let _ = child.wait();
}
