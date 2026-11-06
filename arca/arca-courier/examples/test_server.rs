//! A TLS server for the jailed courier's VM gate, run on the machine that
//! runs the VM (the guest reaches it as 10.0.2.2 through slirp):
//!
//!   courier-test-server HOSTS CA_OUT PORT_OUT LOG
//!
//! HOSTS (comma separated) are the certificate's names, signed by a
//! throwaway CA written to CA_OUT. The port goes to PORT_OUT; each request
//! head, one line per request (`<request line> token=<bool>`), goes to LOG.
//! Answers as courier-vm-probe's server does.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (hosts, ca_out, port_out, log) = (&a[1], &a[2], &a[3], a[4].clone());
    let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let ca_cert = ca.self_signed(&ca_key).unwrap();
    std::fs::write(ca_out, ca_cert.pem()).unwrap();
    let k = rcgen::KeyPair::generate().unwrap();
    let names: Vec<String> = hosts.split(',').map(String::from).collect();
    let c = rcgen::CertificateParams::new(names).unwrap().signed_by(&k, &ca_cert, &ca_key).unwrap();
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
                if let Ok(mut f) = std::fs::OpenOptions::new().append(true).create(true).open(&log) {
                    let _ = writeln!(f, "{first} token={token}");
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
