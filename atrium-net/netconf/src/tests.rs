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
    let j = Join { ssid: b"x".to_vec(), passphrase: None, open: true };
    assert_eq!(read.join(&j).unwrap_err().0, status::NOT_ALLOWED);
    assert_eq!(read.nearby().unwrap_err().0, status::NOT_ALLOWED);

    let mut wifi = c(Grant::Wifi);
    assert_eq!(wifi.status().unwrap().ssid, Some(b"aura-test".to_vec()));
    wifi.join(&j).unwrap();
    let bad = Join { ssid: b"x".to_vec(), passphrase: Some("short".into()), open: false };
    assert_eq!(wifi.join(&bad).unwrap_err().0, status::MALFORMED);
    let wrong = Join { ssid: b"x".to_vec(), passphrase: Some("wrong-passphrase".into()), open: false };
    assert_eq!(wifi.join(&wrong).unwrap_err(), Fail::new(status::FAILED, "handshake failed"));
    assert_eq!(wifi.nearby().unwrap_err().0, status::NOT_ALLOWED, "joining is not seeing what is around");
    assert_eq!(wifi.set_country("DE").unwrap_err().0, status::NOT_ALLOWED);
    assert_eq!(wifi.forget(&[0u8; 33]).unwrap_err().0, status::MALFORMED);

    assert_eq!(c(Grant::Nearby).nearby().unwrap().len(), 1);
    assert_eq!(c(Grant::Details).interfaces().unwrap().len(), 2);
    let mut conf = c(Grant::Configure);
    conf.set_country("DE").unwrap();
    assert_eq!(conf.set_country("D1").unwrap_err().0, status::MALFORMED);
    assert_eq!(conf.set_static().unwrap_err().0, status::UNSUPPORTED);
    assert_eq!(*fake.log.lock().unwrap(), vec!["join x", "join x", "country DE"]);

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
    let _ = std::fs::remove_dir_all(&dir);
}
