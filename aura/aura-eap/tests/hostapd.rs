//! Interop against a real EAP server: hostapd as a standalone RADIUS
//! server (driver=none), the way a campus or office network authenticates.
//! The test plays the access point: EAP from the peer goes up in RADIUS
//! Access-Requests, and the MSK the server hands the access point
//! (MS-MPPE-Recv-Key | MS-MPPE-Send-Key) must equal the peer's.
//!
//! hostapd is a test tool here (BSD-licensed, installed on the build host);
//! nothing of it ships. Without it the test says so and passes.

use std::net::UdpSocket;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use aura_eap::{Config, Method, Out, Peer, Trust, Why};
use hmac::{Hmac, Mac};
use md5::{Digest, Md5};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::RootCertStore;
use zeroize::Zeroizing;

const SECRET: &[u8] = b"radius-test-secret";
const DOMAIN: &str = "corp.example";

fn openssl(dir: &Path, args: &[&str]) {
    let s = Command::new("openssl").args(args).current_dir(dir).stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
    assert!(s.success(), "openssl {args:?}");
}

/// A CA and a certificate it issues, PEM and DER.
fn pki(dir: &Path, ca: &str, leaf: &str, cn: &str, ext: &str) {
    if !dir.join(format!("{ca}.pem")).exists() {
        openssl(dir, &["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", &format!("{ca}.key"), "-out", &format!("{ca}.pem"), "-days", "30", "-subj", &format!("/CN={ca} Test CA"), "-addext", "basicConstraints=critical,CA:TRUE", "-addext", "keyUsage=critical,keyCertSign,cRLSign"]);
        openssl(dir, &["x509", "-in", &format!("{ca}.pem"), "-outform", "DER", "-out", &format!("{ca}.der")]);
    }
    std::fs::write(dir.join(format!("{leaf}.ext")), ext).unwrap();
    openssl(dir, &["req", "-new", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes", "-keyout", &format!("{leaf}.key"), "-out", &format!("{leaf}.csr"), "-subj", &format!("/CN={cn}")]);
    openssl(dir, &["x509", "-req", "-in", &format!("{leaf}.csr"), "-CA", &format!("{ca}.pem"), "-CAkey", &format!("{ca}.key"), "-CAcreateserial", "-days", "30", "-out", &format!("{leaf}.pem"), "-extfile", &format!("{leaf}.ext")]);
    openssl(dir, &["x509", "-in", &format!("{leaf}.pem"), "-outform", "DER", "-out", &format!("{leaf}.der")]);
    openssl(dir, &["pkcs8", "-topk8", "-nocrypt", "-in", &format!("{leaf}.key"), "-outform", "DER", "-out", &format!("{leaf}.p8")]);
}

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn hostapd(dir: &Path, name: &str, port: u16, ca: &str, cert: &str, extra: &str) -> Server {
    std::fs::write(dir.join("clients"), "127.0.0.1/32 radius-test-secret\n").unwrap();
    std::fs::write(
        dir.join("users"),
        "\"alice\" MSCHAPV2,TTLS-MSCHAPV2 \"s3cret\" [2]\n\"bob\" TTLS-PAP \"pap-pass\" [2]\n\"tls-user\" TLS\n* PEAP,TTLS,TLS\n",
    )
    .unwrap();
    let conf = format!(
        "driver=none\ninterface={name}\nlogger_stdout=-1\nlogger_stdout_level=1\nradius_server_clients=clients\nradius_server_auth_port={port}\neap_server=1\neap_user_file=users\nca_cert={ca}.pem\nserver_cert={cert}.pem\nprivate_key={cert}.key\n{extra}"
    );
    std::fs::write(dir.join(format!("{name}.conf")), conf).unwrap();
    let log = std::fs::File::create(dir.join(format!("{name}.log"))).unwrap();
    let child = Command::new("hostapd").arg("-dd").arg(format!("{name}.conf")).current_dir(dir).stdout(log.try_clone().unwrap()).stderr(log).spawn().unwrap();
    let s = Server(child);
    // Ready once its RADIUS port answers; bounded.
    for _ in 0..50 {
        if std::fs::read_to_string(dir.join(format!("{name}.log"))).unwrap_or_default().contains("RADIUS: Server")
            || std::fs::read_to_string(dir.join(format!("{name}.log"))).unwrap_or_default().contains("radius_server")
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(300));
    s
}

/// The access point's half: EAP up in RADIUS, EAP down from it.
struct Radius {
    sock: UdpSocket,
    id: u8,
    state: Option<Vec<u8>>,
    user: Vec<u8>,
}

#[derive(Debug)]
enum Answer {
    Challenge(Vec<u8>),
    Accept(Vec<u8>, Zeroizing<[u8; 64]>),
    Reject(Vec<u8>),
}

fn hmac_md5(parts: &[&[u8]]) -> [u8; 16] {
    let mut m = <Hmac<Md5> as Mac>::new_from_slice(SECRET).unwrap();
    for p in parts {
        m.update(p);
    }
    m.finalize().into_bytes().into()
}

/// RFC 2548 §2.4.3.
fn mppe_key(v: &[u8], req_auth: &[u8; 16]) -> Vec<u8> {
    let (salt, c) = v.split_at(2);
    let mut plain = Vec::new();
    let mut prev: Vec<u8> = [req_auth.as_slice(), salt].concat();
    for chunk in c.chunks(16) {
        let b = Md5::new().chain_update(SECRET).chain_update(&prev).finalize();
        plain.extend(chunk.iter().zip(b.iter()).map(|(x, y)| x ^ y));
        prev = chunk.to_vec();
    }
    let n = plain[0] as usize;
    plain[1..1 + n].to_vec()
}

impl Radius {
    fn new(port: u16) -> Self {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.connect(("127.0.0.1", port)).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        Radius { sock, id: 0, state: None, user: Vec::new() }
    }

    fn exchange(&mut self, eap: &[u8]) -> Answer {
        if eap[4] == 1 && self.user.is_empty() {
            self.user = eap[5..].to_vec();
        }
        self.id = self.id.wrapping_add(1);
        let mut auth = [0u8; 16];
        auth.iter_mut().enumerate().for_each(|(i, b)| *b = (i as u8) ^ self.id ^ 0x5a);
        let mut attrs = Vec::new();
        let mut put = |t: u8, v: &[u8]| {
            attrs.push(t);
            attrs.push(2 + v.len() as u8);
            attrs.extend_from_slice(v);
        };
        put(1, &self.user.clone());
        for c in eap.chunks(253) {
            put(79, c);
        }
        if let Some(s) = &self.state {
            put(24, s);
        }
        put(80, &[0; 16]);
        let len = 20 + attrs.len();
        let mut pkt = vec![1, self.id];
        pkt.extend_from_slice(&(len as u16).to_be_bytes());
        pkt.extend_from_slice(&auth);
        pkt.extend_from_slice(&attrs);
        let ma = hmac_md5(&[&pkt]);
        pkt[len - 16..].copy_from_slice(&ma);
        self.sock.send(&pkt).unwrap();
        let mut buf = [0u8; 4096];
        let n = self.sock.recv(&mut buf).expect("no RADIUS answer in 5 s");
        let r = &buf[..n];
        assert_eq!(r[1], self.id);
        let mut eap = Vec::new();
        let (mut recv, mut send) = (None, None);
        self.state = None;
        let mut a = &r[20..u16::from_be_bytes([r[2], r[3]]) as usize];
        while a.len() >= 2 {
            let (t, l) = (a[0], a[1] as usize);
            let v = &a[2..l];
            match t {
                79 => eap.extend_from_slice(v),
                24 => self.state = Some(v.to_vec()),
                26 if v.len() > 6 && v[..4] == 311u32.to_be_bytes() => {
                    let (vt, vv) = (v[4], &v[6..]);
                    match vt {
                        17 => recv = Some(mppe_key(vv, &auth)),
                        16 => send = Some(mppe_key(vv, &auth)),
                        _ => {}
                    }
                }
                _ => {}
            }
            a = &a[l..];
        }
        match r[0] {
            11 => Answer::Challenge(eap),
            2 => {
                let mut m = Zeroizing::new([0u8; 64]);
                m[..32].copy_from_slice(&recv.expect("MS-MPPE-Recv-Key")[..32]);
                m[32..].copy_from_slice(&send.expect("MS-MPPE-Send-Key")[..32]);
                Answer::Accept(eap, m)
            }
            3 => Answer::Reject(eap),
            c => panic!("RADIUS code {c}"),
        }
    }
}

/// Runs one join: the outcome, and whether the server's MSK matched.
fn join(port: u16, cfg: Config) -> Out {
    let mut peer = Peer::new(cfg);
    let mut radius = Radius::new(port);
    let mut out = peer.handle(&[1, 0, 0, 5, 1]);
    for _ in 0..60 {
        let Out::Send(resp) = out else { return out };
        match radius.exchange(&resp) {
            Answer::Challenge(eap) => out = peer.handle(&eap),
            Answer::Accept(eap, server_msk) => {
                let o = peer.handle(&eap);
                match &o {
                    Out::Done(msk) => assert_eq!(msk[..], server_msk[..], "the peer and the server agree on the MSK"),
                    other => panic!("accepted by the server but the peer says {other:?}"),
                }
                return o;
            }
            Answer::Reject(eap) => return peer.handle(&eap),
        }
    }
    panic!("no end after 60 round trips");
}

fn der(dir: &Path, name: &str) -> CertificateDer<'static> {
    CertificateDer::from(std::fs::read(dir.join(format!("{name}.der"))).unwrap())
}

fn roots(dir: &Path, ca: &str) -> Arc<RootCertStore> {
    let mut r = RootCertStore::empty();
    r.add(der(dir, ca)).unwrap();
    Arc::new(r)
}

fn cfg(method: Method, outer: &str, id: &str, pw: &str, trust: Trust) -> Config {
    Config { method, outer_identity: outer.into(), identity: id.into(), password: Zeroizing::new(pw.into()), trust, client: None }
}

fn scratch() -> PathBuf {
    let d = std::env::temp_dir().join(format!("aura-eap-hostapd-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn against_hostapd() {
    if Command::new("hostapd").arg("-v").stderr(Stdio::null()).status().is_err() {
        eprintln!("SKIP: hostapd is not installed (apt install hostapd)");
        return;
    }
    let dir = scratch();
    let server_ext = format!("subjectAltName=DNS:radius.{DOMAIN}\nextendedKeyUsage=serverAuth\nbasicConstraints=CA:FALSE\n");
    pki(&dir, "corp", "server", &format!("radius.{DOMAIN}"), &server_ext);
    pki(&dir, "corp", "client", "tls-user", "extendedKeyUsage=clientAuth\nbasicConstraints=CA:FALSE\n");
    // The evil twin: the same name, its own CA.
    pki(&dir, "evil", "twin", &format!("radius.{DOMAIN}"), &server_ext);

    let base = 18120 + (std::process::id() % 500) as u16 * 3;
    let _good = hostapd(&dir, "good", base, "corp", "server", "");
    let _twin = hostapd(&dir, "twin", base + 1, "corp", "twin", "");
    let _old = hostapd(&dir, "old", base + 2, "corp", "server", "tls_flags=[DISABLE-TLSv1.1][DISABLE-TLSv1.2][DISABLE-TLSv1.3]\n");

    let corp = || Trust::Roots { roots: roots(&dir, "corp"), domain: DOMAIN.into() };
    let anon = "anonymous@corp.example";
    let done = |o: &Out| matches!(o, Out::Done(_));

    // The methods, each to an agreed MSK.
    let o = join(base, cfg(Method::Peap, anon, "alice", "s3cret", corp()));
    assert!(done(&o), "PEAP: {o:?}");
    let o = join(base, cfg(Method::TtlsMschapv2, anon, "alice", "s3cret", corp()));
    assert!(done(&o), "TTLS-MSCHAPv2: {o:?}");
    let o = join(base, cfg(Method::TtlsPap, anon, "bob", "pap-pass", corp()));
    assert!(done(&o), "TTLS-PAP: {o:?}");
    let key = rustls::crypto::ring::sign::any_supported_type(&PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(std::fs::read(dir.join("client.p8")).unwrap()))).unwrap();
    let mut c = cfg(Method::Tls, "tls-user", "tls-user", "", corp());
    c.client = Some(Arc::new(rustls::sign::CertifiedKey::new(vec![der(&dir, "client")], key)));
    let o = join(base, c);
    assert!(done(&o), "EAP-TLS: {o:?}");

    // Wrong passwords.
    assert_eq!(join(base, cfg(Method::Peap, anon, "alice", "wrong", corp())), Out::Failed(Why::Rejected));
    assert_eq!(join(base, cfg(Method::TtlsMschapv2, anon, "alice", "wrong", corp())), Out::Failed(Why::Rejected));
    assert_eq!(join(base, cfg(Method::TtlsPap, anon, "bob", "wrong", corp())), Out::Failed(Why::Rejected));

    // The certificate: another CA, another domain, the twin.
    let evil = Trust::Roots { roots: roots(&dir, "evil"), domain: DOMAIN.into() };
    assert!(matches!(join(base, cfg(Method::Peap, anon, "alice", "s3cret", evil)), Out::Failed(Why::Untrusted(_))));
    let other = Trust::Roots { roots: roots(&dir, "corp"), domain: "other.example".into() };
    assert!(matches!(join(base, cfg(Method::Peap, anon, "alice", "s3cret", other)), Out::Failed(Why::Untrusted(_))));
    let o = join(base + 1, cfg(Method::Peap, anon, "alice", "s3cret", corp()));
    assert!(matches!(o, Out::Failed(Why::Untrusted(_))), "the twin: {o:?}");

    // First join with nothing set: stop before the password, show the
    // server; pin what was shown; the twin is refused after that.
    let Out::NeedTrust(seen) = join(base, cfg(Method::Peap, anon, "alice", "s3cret", Trust::Unset)) else { panic!("unset trust must stop") };
    assert_eq!(seen.names, vec![format!("radius.{DOMAIN}")]);
    assert_eq!(seen.chain[0], der(&dir, "server"));
    let pinned = || Trust::Pinned { top: seen.top().clone(), domain: seen.domain().unwrap().into() };
    let o = join(base, cfg(Method::Peap, anon, "alice", "s3cret", pinned()));
    assert!(done(&o), "pinned: {o:?}");
    let o = join(base + 1, cfg(Method::Peap, anon, "alice", "s3cret", pinned()));
    assert!(matches!(o, Out::Failed(Why::Untrusted(_))), "the twin after pinning: {o:?}");
    let Out::NeedTrust(twin) = join(base + 1, cfg(Method::Peap, anon, "alice", "s3cret", Trust::Unset)) else { panic!() };
    assert_ne!(twin.chain[0], seen.chain[0]);

    // A server that only speaks TLS older than 1.2.
    let o = join(base + 2, cfg(Method::Peap, anon, "alice", "s3cret", corp()));
    assert!(matches!(o, Out::Failed(Why::Tls(_)) | Out::Failed(Why::Refused)), "old TLS: {o:?}");

    // The server offers PEAP first; a TTLS peer Naks to TTLS (above), and
    // an EAP-TLS peer with no certificate is refused.
    let o = join(base, cfg(Method::Tls, "tls-user", "tls-user", "", corp()));
    assert!(matches!(o, Out::Failed(_)), "EAP-TLS without a certificate: {o:?}");

    if std::env::var_os("AURA_EAP_KEEP").is_none() {
        let _ = std::fs::remove_dir_all(&dir);
    } else {
        eprintln!("kept {}", dir.display());
    }
}
