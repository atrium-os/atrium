//! S4's gate on the host: an app uses its verify-each-use Arca item; arcad
//! asks sigillumd (`VERIFY_USER`, its real client), the sheet takes the
//! passcode, a passcode checker standing in for ostiarius answers, and only
//! then does Arca sign.

use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arca_store_codec::Writer;
use sigillum_proto::driver::Pin;
use sigillum_proto::ui::{self, Choice, FromUi, Sheet, ToUi};
use sigillum_proto::{read_msg, write_msg};
use sigillumd::broker::Config;
use sigillumd::daemon::{Daemon, Passcode, Settings};
use sigillumd::label::{Label, Registry};
use zeroize::Zeroizing;

mod arca_store_codec {
    /// arcad's wire fields (u32 LE length-prefixed).
    pub struct Writer(pub Vec<u8>);
    impl Writer {
        pub fn raw(&mut self, b: &[u8]) -> &mut Self {
            self.0.extend_from_slice(b);
            self
        }
        pub fn bytes(&mut self, b: &[u8]) -> &mut Self {
            self.0.extend_from_slice(&(b.len() as u32).to_le_bytes());
            self.raw(b)
        }
    }
}

fn me() -> u32 {
    unsafe { libc::getuid() }
}

/// Every peer is arcad.
struct AllArca;
impl Registry for AllArca {
    fn label(&self, _: u32) -> Label {
        Label::Arca
    }
}

/// ostiarius's check, in process: alice's passcode is 2468. Counts asks.
struct Pam(Arc<AtomicU64>);
impl Passcode for Pam {
    fn check(&self, user: &str, passcode: &[u8]) -> bool {
        self.0.fetch_add(1, Ordering::SeqCst);
        user == "alice" && passcode == b"2468"
    }
}

/// sigillumd with a UI that picks the passcode on every verify sheet and
/// types `typed`; returns its client socket, the sheets' descriptions and
/// the checker's count.
fn sigillumd(typed: &'static [u8]) -> (std::path::PathBuf, Arc<Mutex<Vec<String>>>, Arc<AtomicU64>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let settings = Settings {
        client_socket: dir.path().join("c.sock"),
        driver_socket: dir.path().join("d.sock"),
        ui_socket: dir.path().join("u.sock"),
        dev_uid: me(),
        ui_uid: me(),
        broker: Config { web_broker: None, ceremony_ms: 10_000 },
    };
    let checks = Arc::new(AtomicU64::new(0));
    let d = Daemon::bind(settings.clone(), Arc::new(AllArca), Box::new(Pam(checks.clone()))).unwrap();
    std::thread::spawn(move || d.run());
    let shown = Arc::new(Mutex::new(Vec::new()));
    let mut s = UnixStream::connect(&settings.ui_socket).unwrap();
    let mut w = s.try_clone().unwrap();
    let sh = shown.clone();
    std::thread::spawn(move || {
        while let Ok(Some(m)) = read_msg::<ToUi>(&mut s, ui::MAX_MESSAGE) {
            let a = match m {
                ToUi::Show { key, sheet: Sheet::Verify { purpose: sigillum_proto::client::Purpose::ArcaItem { description }, .. }, .. } => {
                    sh.lock().unwrap().push(description);
                    Some(FromUi::Press { key, choice: Choice::Passcode, activation_ok: true })
                }
                ToUi::AskPasscode { key, .. } => Some(FromUi::Passcode { key, passcode: Pin(Zeroizing::new(typed.to_vec())) }),
                _ => None,
            };
            if let Some(a) = a {
                let _ = write_msg(&mut w, &a, ui::MAX_MESSAGE);
            }
        }
    });
    std::thread::sleep(Duration::from_millis(200));
    (settings.client_socket, shown, checks, dir)
}

fn app() -> arcad::label::Label {
    arcad::label::Label::App { user: "alice".into(), publisher: [7; 32], app_id: "org.example.bank".into() }
}

/// arcad's state with alice unlocked, serving one app connection that
/// asks `sigillumd` for verify-each-use.
fn arcad(sigillumd: &std::path::Path) -> (UnixStream, tempfile::TempDir) {
    let d = tempfile::tempdir().unwrap();
    let mut s = arcad::server::State::new(d.path().into()).unwrap();
    let mut b = Writer(Vec::new());
    b.bytes(b"alice").raw(&[1; 32]);
    assert_eq!(s.handle(&arcad::label::Label::Vestibulum, arcad::proto::op::V_CREATE, &b.0).status, 0);
    let st = Arc::new(Mutex::new(s));
    let v = arcad::verify::Sigillum::new(sigillumd);
    let (client, server) = UnixStream::pair().unwrap();
    std::thread::spawn(move || arcad::serve_conn_with(server, &app(), &st, Some(&v)));
    (client, d)
}

fn call(c: &mut UnixStream, op: u16, body: &[u8]) -> (u8, Vec<u8>) {
    arcad::proto::write_request(c, op, body).unwrap();
    let (s, b) = arcad::proto::read_reply(c).unwrap();
    (s, b.as_ref().to_vec())
}

fn mint_and_sign(c: &mut UnixStream) -> u8 {
    let mut b = Writer(Vec::new());
    b.raw(&0x100u16.to_le_bytes()).raw(&[3]).bytes(b"transfer key");
    let (s, out) = call(c, arcad::proto::op::MINT, &b.0);
    assert_eq!(s, 0);
    let mut b = Writer(Vec::new());
    b.raw(&out[..16]).bytes(b"pay 10");
    call(c, arcad::proto::op::SIGN, &b.0).0
}

#[test]
fn a_verify_each_use_item_is_used_after_the_passcode_not_before() {
    let (sock, shown, checks, _d) = sigillumd(b"2468");
    let (mut c, _a) = arcad(&sock);
    assert_eq!(mint_and_sign(&mut c), arcad::proto::status::OK);
    assert_eq!(shown.lock().unwrap().as_slice(), ["\u{201c}transfer key\u{201d} for org.example.bank"]);
    assert_eq!(checks.load(Ordering::SeqCst), 1);
}

#[test]
fn a_wrong_passcode_leaves_the_item_unused() {
    let (sock, _shown, checks, _d) = sigillumd(b"0000");
    let (mut c, _a) = arcad(&sock);
    assert_eq!(mint_and_sign(&mut c), arcad::proto::status::NEEDS_VERIFICATION);
    assert_eq!(checks.load(Ordering::SeqCst), 3, "three tries, then the ceremony ends");
}
