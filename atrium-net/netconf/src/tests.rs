use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};

use aura_ipc::Line;

use crate::client::Client;
use crate::server::{serve, Backend};
use crate::*;

fn parse(s: &str) -> Line {
    Line::parse(s).unwrap()
}

const STATUS: &[&str] = &[
    "iface name=vtnet1 mac=02:4e:00:00:00:01 state=bound addr=10.88.0.50/24 router=10.88.0.1 dns=10.88.0.53 metered=0 link=up internet=yes refused=0 sends_refused=0",
    "iface name=wlan0 mac=5e:a9:55:f7:22:91 state=bound addr=10.77.0.117/24 router=10.77.0.1 dns=10.77.0.1 metered=1 inet6=fd77::5 wifi=1 link=up internet=captive portal=687474703a2f2f782f egress=1 refused=0 sends_refused=0",
    "wifi state=up mlme=1 profiles=2 ssid=617572612d74657374",
    "ok",
];

fn status_lines() -> Vec<Line> {
    STATUS.iter().map(|s| parse(s)).collect()
}

#[test]
fn status_from_lines() {
    let s = lines::status(&status_lines(), Some(-60));
    assert_eq!(s, Status { link: Link::Wifi, online: Online::Captive, metered: true, bars: Some(3), ssid: Some(b"aura-test".to_vec()), portal: Some("http://x/".into()) });
    let s = s.for_grant(Grant::Read);
    assert_eq!((s.ssid, s.portal), (None, None), "no names for network-read");
    // No egress: offline.
    let v: Vec<Line> = status_lines().into_iter().map(|l| if l.verb == "iface" { parse(&STATUS[0].replace(" egress=1", "")) } else { l }).collect();
    assert_eq!(lines::status(&v, None), Status::offline());
    let r = lines::routes(&status_lines());
    assert_eq!(r.default4, Some(("wlan0".into(), "10.77.0.1".into())));
    let i = lines::iface(&status_lines()[1]).unwrap();
    assert!(i.wifi && i.egress && i.metered);
    assert_eq!(i.addrs6, vec!["fd77::5".to_string()]);
}

#[test]
fn nearby_hides_bssids_and_keeps_the_strongest() {
    let bss = [
        "bss ssid=636166 bssid=02:00:00:00:00:01 freq=2437 rssi=-80 security=psk",
        "bss ssid=636166 bssid=02:00:00:00:00:02 freq=5180 rssi=-50 security=psk",
        "bss ssid= bssid=02:00:00:00:00:03 freq=2412 rssi=-30 security=open",
        "bss ssid=686f6d65 bssid=02:00:00:00:00:04 freq=2412 rssi=-70 security=sae-transition",
        "scan-done status=0 count=4",
    ];
    let v = lines::nearby(&bss.iter().map(|s| parse(s)).collect::<Vec<_>>(), &[b"home".to_vec()]);
    assert_eq!(
        v,
        vec![
            Nearby { ssid: b"caf".to_vec(), security: "wpa2".into(), bars: 4, known: false },
            Nearby { ssid: b"home".to_vec(), security: "wpa2/wpa3".into(), bars: 2, known: true },
        ]
    );
    let enc = postcard::to_stdvec(&v).unwrap();
    assert!(!enc.windows(6).any(|w| w == [2, 0, 0, 0, 0, 2]), "no BSSID on the wire");
}

#[test]
fn profile_round_trip() {
    let l = parse("net ssid=617572612d74657374 open=0 wpa3_only=1 mac=network auto=1 prio=5 metered=0");
    let p = lines::profile(&l).unwrap();
    assert!(p.wpa3_only && p.auto && p.prio == 5);
    let r = lines::remember(&p);
    assert_eq!(r.verb, "wifi-remember");
    assert!(r.get("secret").is_none(), "settings only, never a secret");
}

#[test]
fn grants() {
    use op::*;
    for g in Grant::ALL {
        assert!(g.allows(STATUS) && g.allows(SUBSCRIBE));
    }
    assert!(!Grant::Read.allows(INTERFACES) && Grant::Details.allows(INTERFACES));
    assert!(!Grant::Wifi.allows(WIFI_NEARBY), "network-wifi does not see what is around");
    assert!(Grant::Nearby.allows(WIFI_NEARBY) && !Grant::Nearby.allows(WIFI_JOIN));
    assert!(!Grant::Wifi.allows(SET_COUNTRY) && Grant::Configure.allows(SET_COUNTRY));
    assert!(!Grant::Configure.allows(WIFI_JOIN));
    for o in [WIRED_SIGN_IN, WIRED_SERVER_SEEN, WIRED_TRUST_SERVER, WIRED_CERT_REQUEST, WIRED_FORGET] {
        for g in Grant::ALL {
            assert_eq!(g.allows(o), g == Grant::Configure, "wired sign-in is a network-configure grant: {g:?} {o:#x}");
        }
    }
    for g in Grant::ALL {
        assert_eq!(g.allows(PORTAL_INFO) || g.allows(PORTAL_WATCH), g == Grant::Portal, "{g:?}");
        assert_eq!(g.root_only(), matches!(g, Grant::Portal | Grant::PrintDiscovery));
    }
}

#[test]
fn portal_info_from_lines() {
    let p = lines::portal(&status_lines()).unwrap();
    assert_eq!((p.iface.as_str(), p.ssid.as_deref(), p.online, p.watching), ("wlan0", Some(&b"aura-test"[..]), Online::Captive, false));
    assert_eq!((p.portal.as_deref(), p.router.as_deref(), p.dns.clone()), (Some("http://x/"), Some("10.77.0.1"), vec!["10.77.0.1".to_string()]));
    assert_eq!(p.start_url().as_deref(), Some("http://x/"));
    let p = PortalInfo { portal: None, ..p };
    assert_eq!(p.start_url().as_deref(), Some("http://10.77.0.1/"), "no Location: the router, never a third party");
    // RA servers too, but not link-local ones; the watch flag.
    let v: Vec<Line> = vec![parse("iface name=em0 mac=x state=bound addr=10.0.0.5/24 router=10.0.0.1 dns=10.0.0.1 dns6=fd00::53,fe80::1 link=up internet=captive egress=1 portal_watch=1")];
    let p = lines::portal(&v).unwrap();
    assert_eq!((p.dns, p.ssid, p.watching), (vec!["10.0.0.1".to_string(), "fd00::53".into()], None, true));
    assert!(lines::portal(&[parse(&STATUS[1].replace(" egress=1", ""))]).is_none());
}

#[test]
fn dns_over_tls_stands_in_for_the_probe() {
    let l = |extra: &str| parse(&format!("iface name=em0 mac=x state=bound addr=10.0.0.5/24 router=10.0.0.1 dns=10.0.0.1 link=up egress=1 {extra}").trim_end());
    let on = |extra: &str| lines::portal(&[l(extra)]).unwrap().online;
    assert_eq!(on(""), Online::Unknown);
    assert_eq!(on("dot=failing"), Online::Captive, "TLS server not answering on a new network: offer sign-in");
    assert_eq!(on("dot=ok"), Online::Yes, "answering over TLS: through");
    assert_eq!(on("internet=no dot=failing"), Online::No, "the probe's word wins");
    assert_eq!(on("internet=yes dot=failing"), Online::Yes);
    assert_eq!(lines::status(&[l("dot=failing")], None).online, Online::Captive);
}

#[derive(Default)]
struct Fake {
    log: Mutex<Vec<String>>,
    subs: Mutex<Vec<mpsc::Sender<Status>>>,
}

impl Backend for Fake {
    fn status(&self) -> Res<Status> {
        Ok(lines::status(&status_lines(), Some(-60)))
    }
    fn subscribe(&self) -> Res<mpsc::Receiver<Status>> {
        let (tx, rx) = mpsc::channel();
        tx.send(self.status()?).unwrap();
        self.subs.lock().unwrap().push(tx);
        Ok(rx)
    }
    fn interfaces(&self) -> Res<Vec<Iface>> {
        Ok(status_lines().iter().filter_map(lines::iface).collect())
    }
    fn routes(&self) -> Res<Routes> {
        Ok(lines::routes(&status_lines()))
    }
    fn profiles(&self) -> Res<Vec<Profile>> {
        Ok(vec![])
    }
    fn join(&self, j: Join) -> Res<()> {
        self.log.lock().unwrap().push(format!("join {}", String::from_utf8_lossy(&j.ssid)));
        if j.passphrase.as_deref() == Some("wrong-passphrase") {
            return Err(Fail::new(status::FAILED, "handshake failed"));
        }
        Ok(())
    }
    fn leave(&self) -> Res<()> {
        Ok(())
    }
    fn forget(&self, _: Vec<u8>) -> Res<()> {
        Ok(())
    }
    fn set_profile(&self, _: Profile) -> Res<()> {
        Ok(())
    }
    fn nearby(&self) -> Res<Vec<Nearby>> {
        Ok(vec![Nearby { ssid: b"caf".to_vec(), security: "wpa2".into(), bars: 3, known: false }])
    }
    fn set_country(&self, cc: String) -> Res<()> {
        self.log.lock().unwrap().push(format!("country {cc}"));
        Ok(())
    }
    fn set_static(&self, s: SetStatic) -> Res<()> {
        self.log.lock().unwrap().push(format!("static {:?} {:?}", s.target, s.manual.map(|m| m.addr)));
        Ok(())
    }
    fn set_dns(&self, v: Vec<std::net::IpAddr>) -> Res<()> {
        self.log.lock().unwrap().push(format!("dns {v:?}"));
        Ok(())
    }
    fn set_dns_tls(&self, t: DnsTls) -> Res<()> {
        self.log.lock().unwrap().push(format!("dns-tls {:?} {}", t.servers, t.name));
        Ok(())
    }
    fn portal_info(&self) -> Res<PortalInfo> {
        lines::portal(&status_lines()).ok_or_else(|| Fail::new(status::FAILED, "no-network"))
    }
    fn portal_watch(&self, on: bool) -> Res<()> {
        self.log.lock().unwrap().push(format!("portal-watch {on}"));
        Ok(())
    }
    fn print_discovery(&self) -> Res<(String, [std::os::fd::OwnedFd; 2])> {
        // Stand-ins for the two sockets: each end of a pair says which it is.
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        let (c, d) = std::os::unix::net::UnixStream::pair().unwrap();
        use std::io::Write;
        (&b).write_all(b"recv").unwrap();
        (&d).write_all(b"send").unwrap();
        std::mem::forget((b, d));
        Ok(("wlan0".into(), [a.into(), c.into()]))
    }
}

fn serve_all(dir: &Path, b: Arc<Fake>) {
    for g in Grant::ALL {
        let p = Client::path(dir, g);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let l = std::os::unix::net::UnixListener::bind(&p).unwrap();
        let b = b.clone();
        std::thread::spawn(move || {
            for s in l.incoming().flatten() {
                let b = b.clone();
                std::thread::spawn(move || {
                    let _ = serve(aqueduct::Connection::wrap(s).unwrap(), g, &*b);
                });
            }
        });
    }
}

fn tmp() -> PathBuf {
    let d = std::env::temp_dir().join(format!("netconf-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Over real sockets: what each capability's socket allows.
#[test]
fn over_sockets() {
    let dir = tmp();
    let fake = Arc::new(Fake::default());
    serve_all(&dir, fake.clone());
    let c = |g| Client::connect(&Client::path(&dir, g)).unwrap();

    let mut read = c(Grant::Read);
    let s = read.status().unwrap();
    assert_eq!((s.link, s.bars, s.ssid.clone()), (Link::Wifi, Some(3), None));
    assert_eq!(read.interfaces().unwrap_err().0, status::NOT_ALLOWED);
    let j = Join { ssid: b"x".to_vec(), passphrase: None, handle: None, open: true };
    assert_eq!(read.join(&j).unwrap_err().0, status::NOT_ALLOWED);
    assert_eq!(read.nearby().unwrap_err().0, status::NOT_ALLOWED);

    let mut wifi = c(Grant::Wifi);
    assert_eq!(wifi.status().unwrap().ssid, Some(b"aura-test".to_vec()));
    wifi.join(&j).unwrap();
    let bad = Join { ssid: b"x".to_vec(), passphrase: Some("short".into()), handle: None, open: false };
    assert_eq!(wifi.join(&bad).unwrap_err().0, status::MALFORMED);
    let wrong = Join { ssid: b"x".to_vec(), passphrase: Some("wrong-passphrase".into()), handle: None, open: false };
    assert_eq!(wifi.join(&wrong).unwrap_err(), Fail::new(status::FAILED, "handshake failed"));
    assert_eq!(wifi.nearby().unwrap_err().0, status::NOT_ALLOWED, "joining is not seeing what is around");
    assert_eq!(wifi.set_country("DE").unwrap_err().0, status::NOT_ALLOWED);
    assert_eq!(wifi.forget(&[0u8; 33]).unwrap_err().0, status::MALFORMED);

    assert_eq!(c(Grant::Nearby).nearby().unwrap().len(), 1);
    assert_eq!(c(Grant::Details).interfaces().unwrap().len(), 2);
    let mut conf = c(Grant::Configure);
    conf.set_country("DE").unwrap();
    assert_eq!(conf.set_country("D1").unwrap_err().0, status::MALFORMED);
    let m = Manual { addr: "10.0.0.5".parse().unwrap(), prefix: 24, router: Some("10.0.0.1".parse().unwrap()), dns: vec!["1.1.1.1".parse().unwrap()] };
    let fixed = SetStatic { target: Target::Wifi(b"home".to_vec()), manual: Some(m.clone()) };
    assert_eq!(wifi.set_static(&fixed).unwrap_err().0, status::NOT_ALLOWED, "network-configure only");
    conf.set_static(&fixed).unwrap();
    conf.set_static(&SetStatic { target: Target::Wired("vtnet1".into()), manual: None }).unwrap();
    for bad in [
        Manual { prefix: 31, ..m.clone() },
        Manual { addr: "10.0.0.0".parse().unwrap(), ..m.clone() },
        Manual { addr: "127.0.0.5".parse().unwrap(), ..m.clone() },
        Manual { router: Some("10.0.1.1".parse().unwrap()), ..m.clone() },
        Manual { router: Some("10.0.0.5".parse().unwrap()), ..m.clone() },
        Manual { dns: vec!["1.1.1.1".parse().unwrap(); 4], ..m.clone() },
    ] {
        assert_eq!(conf.set_static(&SetStatic { target: Target::Wifi(b"home".to_vec()), manual: Some(bad.clone()) }).unwrap_err().0, status::MALFORMED, "{bad:?}");
    }
    assert_eq!(conf.set_static(&SetStatic { target: Target::Wired("../x".into()), manual: None }).unwrap_err().0, status::MALFORMED);
    conf.set_dns(&["9.9.9.9".parse().unwrap(), "2620:fe::fe".parse().unwrap()]).unwrap();
    conf.set_dns(&[]).unwrap();
    assert_eq!(conf.set_dns(&["127.0.0.1".parse().unwrap()]).unwrap_err().0, status::MALFORMED);
    let q9 = || vec!["9.9.9.9".parse().unwrap()];
    conf.set_dns_tls(&DnsTls { servers: q9(), name: "dns.quad9.net".into() }).unwrap();
    for (servers, name) in [
        (vec![], "dns.quad9.net"),
        (q9(), ""),
        (q9(), "quad9"),
        (q9(), "dns quad9.net"),
        (q9(), "9.9.9.9"),
        (q9(), "-x.example.net"),
        (q9(), "x..net"),
        (vec!["127.0.0.1".parse().unwrap()], "dns.quad9.net"),
    ] {
        assert_eq!(conf.set_dns_tls(&DnsTls { servers, name: name.into() }).unwrap_err().0, status::MALFORMED, "{name}");
    }
    assert!(!tls_name_ok(&format!("{}.net", "a.".repeat(130))));
    assert_eq!(
        *fake.log.lock().unwrap(),
        vec![
            "join x",
            "join x",
            "country DE",
            "static Wifi([104, 111, 109, 101]) Some(10.0.0.5)",
            "static Wired(\"vtnet1\") None",
            "dns [9.9.9.9, 2620:fe::fe]",
            "dns []",
            "dns-tls [9.9.9.9] dns.quad9.net"
        ]
    );
    // Fixed settings and own DNS servers read back from atrium-net's lines.
    let l = lines::with_manual(Line::new("net").with_hex("ssid", b"home"), &m);
    assert_eq!(lines::profile(&l).unwrap().manual, Some(m.clone()));
    assert_eq!(lines::routes(&[parse("dns own=9.9.9.9,1.1.1.1")]).own_dns, vec!["9.9.9.9".to_string(), "1.1.1.1".into()]);
    assert_eq!(lines::routes(&[parse("dns own=9.9.9.9 tls=dns.quad9.net")]).dns_tls.as_deref(), Some("dns.quad9.net"));

    // Subscribe: now, then only changes, names filtered by grant.
    let mut ch = c(Grant::Read).subscribe().unwrap();
    assert_eq!(ch.next().unwrap().link, Link::Wifi);
    let tx = fake.subs.lock().unwrap()[0].clone();
    tx.send(lines::status(&status_lines(), Some(-60))).unwrap(); // the same: no event
    tx.send(Status::offline()).unwrap();
    let s = ch.next_within(std::time::Duration::from_secs(5)).unwrap().unwrap();
    assert_eq!(s, Status::offline());
    let mut ch = c(Grant::Wifi).subscribe().unwrap();
    assert_eq!(ch.next().unwrap().ssid, Some(b"aura-test".to_vec()));

    // Portal sign-in: its own socket only.
    assert_eq!(c(Grant::Details).portal_info().unwrap_err().0, status::NOT_ALLOWED);
    assert_eq!(c(Grant::Portal).portal_info().unwrap().start_url().as_deref(), Some("http://x/"));
    c(Grant::Portal).portal_watch(true).unwrap();
    assert_eq!(fake.log.lock().unwrap().last().map(String::as_str), Some("portal-watch true"));

    // Printer discovery: its own socket only, the two fds in order.
    assert_eq!(c(Grant::Configure).print_discovery().unwrap_err().0, status::NOT_ALLOWED);
    let (name, r, w) = c(Grant::PrintDiscovery).print_discovery().unwrap();
    let said = |f: std::os::fd::OwnedFd| {
        use std::io::Read;
        let mut b = [0u8; 4];
        std::os::unix::net::UnixStream::from(f).read_exact(&mut b).unwrap();
        b
    };
    assert_eq!((name.as_str(), &said(r), &said(w)), ("wlan0", b"recv", b"send"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A service that hangs (socket open, nobody answering) is "not answering"
/// after the op's limit, never a caller stuck for good.
#[test]
fn a_hung_service_times_out() {
    let dir = std::env::temp_dir().join(format!("netconf-hung-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("hung.sock");
    let l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let held = std::thread::spawn(move || {
        let s = l.accept().unwrap();
        std::thread::sleep(std::time::Duration::from_secs(14));
        drop(s);
    });
    let t = std::time::Instant::now();
    let e = Client::connect(&sock).unwrap().status().unwrap_err();
    assert_eq!(e, Fail::new(status::UNAVAILABLE, "not answering"));
    assert!(t.elapsed() >= Client::limit(op::STATUS) && t.elapsed() < Client::limit(op::STATUS) * 2);
    held.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn enterprise_requests_are_checked_and_certificates_convert() {
    use crate::{der_to_pem, pem_to_der, Enterprise};
    let pw = Enterprise { ssid: b"corp".to_vec(), method: "peap".into(), identity: "alice".into(), anonymous: None, domain: String::new(), ca: None, system_roots: false, handle: Some([1; 16]), key: None, cert: None };
    assert!(pw.check().is_ok());
    assert!(Enterprise { handle: None, ..pw.clone() }.check().is_err(), "a password login needs the handle");
    let tls = Enterprise { method: "tls".into(), handle: None, key: Some([2; 16]), cert: Some(vec![0x30, 3, 1, 2, 3]), ..pw.clone() };
    assert!(tls.check().is_ok());
    assert!(Enterprise { cert: None, ..tls.clone() }.check().is_err(), "a certificate login needs its certificate");
    assert!(Enterprise { handle: Some([1; 16]), ..tls.clone() }.check().is_err(), "and no password");
    assert!(Enterprise { cert: Some(vec![0x30; 3001]), ..tls.clone() }.check().is_err(), "bounded");
    assert!(Enterprise { ca: Some(vec![0x30, 0]), ..pw.clone() }.check().is_err(), "a CA needs a domain");
    assert!(Enterprise { method: "md5".into(), ..pw }.check().is_err());
    let der: Vec<u8> = (0..=255u8).cycle().take(301).map(|b| if b == 0 { 0x30 } else { b }).collect();
    let der = [vec![0x30], der].concat();
    let pem = der_to_pem(&der, "CERTIFICATE REQUEST");
    assert!(pem.starts_with("-----BEGIN CERTIFICATE REQUEST-----\n") && pem.lines().all(|l| l.len() <= 64));
    assert_eq!(pem_to_der(pem.as_bytes()), Some(der.clone()));
    assert_eq!(pem_to_der(&der), Some(der), "DER as is");
    assert_eq!(pem_to_der(b"-----BEGIN X-----\n!!!!\n-----END X-----\n"), None);
    assert_eq!(pem_to_der(b"not a certificate"), None);
}
