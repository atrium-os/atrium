//! S2c's gate: the real daemon on real sockets, with the FIDO driver's real
//! serve loop over a soft key, a scripted trusted UI and a client, all in
//! one process. Peer uids are all this test's uid, so the settings name it
//! for the driver and UI sockets; the tests that need a refused uid name
//! another one.

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use sha2::{Digest, Sha256};
use sigillum_proto::client::{self, Assert, Attestation, Refusal, Register, Reply, Request, Uv};
use sigillum_proto::driver::{self, DeviceClass, FromDriver, Pin};
use sigillum_proto::ui::{self, Choice, FromUi, Sheet, ToUi};
use sigillum_proto::{read_msg, write_msg};
use sigillum_softkey::SoftKey;
use sigillumd::broker::Config;
use sigillumd::daemon::{Daemon, NoPasscode, Settings};
use sigillumd::label::{AppId, Label, Registry};
use zeroize::Zeroizing;

fn me() -> u32 {
    unsafe { libc::getuid() }
}

fn navigator() -> AppId {
    AppId { publisher: [0xAB; 32], app_id: "org.atrium.navigator".into() }
}

/// Every peer is Navigator's broker.
struct AllNavigator;
impl Registry for AllNavigator {
    fn label(&self, _: u32) -> Label {
        Label::App { user: "alice".into(), app: navigator(), rp_ids: vec![] }
    }
}

struct Paths {
    dir: PathBuf,
    settings: Settings,
}

impl Drop for Paths {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn start(dev_uid: u32, ui_uid: u32) -> Paths {
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!("sigillumd-test-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
    std::fs::create_dir_all(&dir).unwrap();
    let settings = Settings {
        client_socket: dir.join("c.sock"),
        driver_socket: dir.join("d.sock"),
        ui_socket: dir.join("u.sock"),
        dev_uid,
        ui_uid,
        broker: Config { web_broker: Some(navigator()), ceremony_ms: 10_000 },
    };
    let d = Daemon::bind(settings.clone(), Arc::new(AllNavigator), Box::new(NoPasscode)).unwrap();
    std::thread::spawn(move || d.run());
    Paths { dir, settings }
}

/// A soft key behind the real driver loop, connected like `--connect`.
fn plug_key(p: &Paths, pin: Option<&'static [u8]>) {
    let path = p.settings.driver_socket.clone();
    std::thread::spawn(move || {
        let sock = UnixStream::connect(path).unwrap();
        let (driver, hello) = sigillum_fido::Driver::open(SoftKey::new(pin), 0x1050, 0x0407, "Soft Key").unwrap();
        let _ = sigillum_fido::serve(driver, hello, sock);
    });
}

/// The trusted UI, scripted: presses the first device on every sheet,
/// types `pin` when asked, and reports everything it was told.
fn ui(p: &Paths, pin: &'static [u8]) -> mpsc::Receiver<ToUi> {
    let mut s = UnixStream::connect(&p.settings.ui_socket).unwrap();
    let mut w = s.try_clone().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        while let Ok(Some(m)) = read_msg::<ToUi>(&mut s, ui::MAX_MESSAGE) {
            match &m {
                ToUi::Show { key, devices, .. } => {
                    if let Some(d) = devices.first() {
                        let _ = write_msg(&mut w, &FromUi::Press { key: *key, choice: Choice::Device(d.id), activation_ok: true }, ui::MAX_MESSAGE);
                    }
                }
                ToUi::AskPin { key, .. } => {
                    let _ = write_msg(&mut w, &FromUi::Pin { key: *key, pin: Pin(Zeroizing::new(pin.to_vec())) }, ui::MAX_MESSAGE);
                }
                _ => {}
            }
            if tx.send(m).is_err() {
                return;
            }
        }
    });
    rx
}

fn client(p: &Paths) -> UnixStream {
    let s = UnixStream::connect(&p.settings.client_socket).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s
}

fn ask(s: &mut UnixStream, req: &Request) -> Reply {
    write_msg(s, req, client::MAX_MESSAGE).unwrap();
    read_msg::<Reply>(s, client::MAX_MESSAGE).unwrap().expect("a reply")
}

fn cdh(s: &str) -> [u8; 32] {
    Sha256::digest(s.as_bytes()).into()
}

fn register(id: u64) -> Request {
    Request::Register(Register {
        ceremony: id,
        timeout_ms: 0,
        rp_id: "example.com".into(),
        rp_name: "Example".into(),
        client_data_hash: cdh("create"),
        user_id: vec![1, 2, 3, 4],
        user_name: "alice@example.com".into(),
        user_display_name: "Alice".into(),
        exclude: vec![],
        resident_key: false,
        uv: Uv::Preferred,
        attestation: Attestation::None,
    })
}

/// Wait until the daemon has the device (its `Hello` arrived): a sheet
/// shown before that would list no devices.
fn settle() {
    std::thread::sleep(Duration::from_millis(300));
}

#[test]
fn register_then_assert_over_sockets() {
    let p = start(me(), me());
    let ui = ui(&p, b"4711");
    plug_key(&p, Some(b"4711"));
    settle();
    let mut c = client(&p);

    let reply = ask(&mut c, &register(7));
    let Reply::Registered { ceremony: 7, cred_id, auth_data, .. } = reply else { panic!("{reply:?}") };
    assert_eq!(auth_data[..32], cdh("example.com"));

    let reply = ask(&mut c, &Request::Assert(Assert { ceremony: 8, timeout_ms: 0, rp_id: "example.com".into(), client_data_hash: cdh("get"), allow: vec![cred_id.clone()], uv: Uv::Required }));
    let Reply::Asserted { ceremony: 8, cred_id: used, auth_data, .. } = reply else { panic!("{reply:?}") };
    assert_eq!(used, cred_id);
    assert!(auth_data[32] & 0x04 != 0, "UV via the PIN typed on the sheet");

    // The UI saw: two sheets (with the key listed), a PIN ask, touch
    // prompts, and a close for each.
    let seen: Vec<ToUi> = ui.try_iter().collect();
    let shows: Vec<_> = seen.iter().filter_map(|m| if let ToUi::Show { sheet, devices, .. } = m { Some((sheet, devices)) } else { None }).collect();
    assert_eq!(shows.len(), 2);
    assert!(matches!(shows[0].0, Sheet::Register { requester, rp_id, .. } if requester == "Navigator" && rp_id == "example.com"));
    assert_eq!(shows[0].1.len(), 1);
    assert_eq!(shows[0].1[0].name, "Soft Key");
    assert!(seen.iter().any(|m| matches!(m, ToUi::AskPin { .. })));
    assert!(seen.iter().any(|m| matches!(m, ToUi::Touch { .. })), "the keepalive became a touch prompt");
    assert_eq!(seen.iter().filter(|m| matches!(m, ToUi::Close { .. })).count(), 2);
}

#[test]
fn no_trusted_ui_means_no_ceremony() {
    let p = start(me(), me());
    plug_key(&p, None);
    settle();
    let mut c = client(&p);
    assert_eq!(ask(&mut c, &register(1)), Reply::Refused { ceremony: 1, reason: Refusal::NotAllowed });
}

#[test]
fn ui_leaving_ends_the_ceremony() {
    let p = start(me(), me());
    // A UI that never presses, then disconnects.
    let s = UnixStream::connect(&p.settings.ui_socket).unwrap();
    plug_key(&p, None);
    settle();
    let mut c = client(&p);
    write_msg(&mut c, &register(3), client::MAX_MESSAGE).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    drop(s);
    let r = read_msg::<Reply>(&mut c, client::MAX_MESSAGE).unwrap().unwrap();
    assert_eq!(r, Reply::Refused { ceremony: 3, reason: Refusal::NotAllowed });
}

#[test]
fn wrong_uids_are_dropped() {
    let p = start(me().wrapping_add(1), me().wrapping_add(1));
    for path in [&p.settings.driver_socket, &p.settings.ui_socket] {
        let mut s = UnixStream::connect(path).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let r = read_msg::<ToUi>(&mut s, 4096);
        assert!(matches!(r, Ok(None)) || r.is_err(), "{path:?}: {r:?}");
    }
}

#[test]
fn driver_must_say_hello_first_and_once() {
    let p = start(me(), me());
    let _ui = ui(&p, b"");
    let hello = FromDriver::Hello { class: DeviceClass::Fido, vendor: 1, product: 2, name: "x".into(), fido: None };

    // Work before Hello: dropped.
    let mut s = UnixStream::connect(&p.settings.driver_socket).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write_msg(&mut s, &FromDriver::NeedTouch { ceremony: 1 }, driver::MAX_MESSAGE).unwrap();
    assert!(matches!(read_msg::<sigillum_proto::driver::ToDriver>(&mut s, driver::MAX_MESSAGE), Ok(None) | Err(_)));

    // Two Hellos: dropped.
    let mut s = UnixStream::connect(&p.settings.driver_socket).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write_msg(&mut s, &hello, driver::MAX_MESSAGE).unwrap();
    write_msg(&mut s, &hello, driver::MAX_MESSAGE).unwrap();
    assert!(matches!(read_msg::<sigillum_proto::driver::ToDriver>(&mut s, driver::MAX_MESSAGE), Ok(None) | Err(_)));
}

#[test]
fn oversized_client_message_drops_the_client() {
    let p = start(me(), me());
    let mut c = client(&p);
    use std::io::Write;
    c.write_all(&((client::MAX_MESSAGE as u32) + 1).to_le_bytes()).unwrap();
    let r = read_msg::<Reply>(&mut c, client::MAX_MESSAGE);
    assert!(matches!(r, Ok(None) | Err(_)), "{r:?}");
}
